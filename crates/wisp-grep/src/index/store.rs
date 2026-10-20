//! Where indexes live and how a search path finds its index.
//!
//! Indexes are kept outside the searched tree so `rg --hidden` or
//! `--no-ignore` can never see them. Each indexed root gets a slot directory
//! named by a hash of its normalized absolute path. The slot holds `root`
//! (the path, to rule out hash collisions), `CURRENT` (the file name of the
//! live generation) and immutable `index-<n>.wgi` generations. New
//! generations never overwrite a file that a reader may have mapped, which
//! Windows would refuse anyway.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

pub(crate) fn store_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WGREP_INDEX_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("wgrep").join("indexes"))
}

/// Absolute path with `.` and `..` removed lexically; symlinks are not
/// resolved, so a search through a different spelling of the same directory
/// simply does not use the index.
pub(crate) fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let joined = cwd.join(path);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The normalized form of `root` recorded in a slot's `root` file and in
/// daemon.json.
pub(crate) fn root_key(root: &Path) -> String {
    let key = root.to_string_lossy().replace('\\', "/");
    let key = key.trim_end_matches('/').to_string();
    // Windows paths compare case-insensitively.
    if cfg!(windows) { key.to_lowercase() } else { key }
}

fn slot(store: &Path, root: &Path) -> PathBuf {
    // FNV-1a 64.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in root_key(root).bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    store.join(format!("{hash:016x}"))
}

/// The slot directory for `root`.
pub(crate) fn slot_for(root: &Path) -> Option<PathBuf> {
    Some(slot(&store_dir()?, root))
}

/// The live index file for `root`, if it has one.
pub(crate) fn current(root: &Path) -> Option<PathBuf> {
    let slot = slot(&store_dir()?, root);
    let recorded = fs::read_to_string(slot.join("root")).ok()?;
    if recorded != root_key(root) {
        return None;
    }
    let name = fs::read_to_string(slot.join("CURRENT")).ok()?;
    let name = name.trim();
    // Only ever a generation file name in this slot: a corrupted or hostile
    // `CURRENT` must not point outside it (path traversal).
    let digits = name.strip_prefix("index-")?.strip_suffix(".wgi")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(slot.join(name))
}

/// The nearest ancestor of `abs` (or `abs` itself) that has an index.
pub(crate) fn find(abs: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = Some(abs);
    while let Some(d) = dir {
        if let Some(file) = current(d) {
            return Some((d.to_path_buf(), file));
        }
        dir = d.parent();
    }
    None
}

/// Reserves a path for a new generation of `root`'s index.
///
/// The name is created with `O_EXCL`, so two builds racing on the same root
/// can never reserve the same file: the loser simply tries the next number.
/// The file is left empty for `format::write` to fill in.
pub(crate) fn new_generation(root: &Path) -> io::Result<PathBuf> {
    let store = store_dir().ok_or_else(|| io::Error::other("no index directory; set WGREP_INDEX_DIR"))?;
    let slot = slot(&store, root);
    fs::create_dir_all(&slot)?;
    fs::write(slot.join("root"), root_key(root))?;
    let start = fs::read_dir(&slot)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_prefix("index-")?.strip_suffix(".wgi")?.parse::<u64>().ok())
        .max()
        .map_or(1, |n| n + 1);
    let mut n = start;
    loop {
        let candidate = slot.join(format!("index-{n}.wgi"));
        match fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(err) => return Err(err),
        }
    }
}

/// An exclusive, crash-releasing build lock for one root: one writer per
/// view, and the lock is an OS advisory lock on `build.lock`, so a crashed
/// builder cannot strand it. Acquisition **fails closed** — a live holder is
/// never preempted, no timeout grants ownership, and there is no age-based
/// theft. Readers never take this lock.
pub(crate) struct WriterLock {
    file: fs::File,
}

impl WriterLock {
    pub(crate) fn acquire(root: &Path) -> io::Result<WriterLock> {
        let store = store_dir().ok_or_else(|| io::Error::other("no index directory; set WGREP_INDEX_DIR"))?;
        let slot = slot(&store, root);
        fs::create_dir_all(&slot)?;
        let path = slot.join("build.lock");
        let mut file = fs::OpenOptions::new().create(true).read(true).write(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => {
                // Best effort pid note for diagnostics; the lock, not the file,
                // is the ownership token.
                let _ = file.set_len(0);
                let _ = writeln!(file, "{}", std::process::id());
                Ok(WriterLock { file })
            }
            Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another wgrep build is already running for this root",
            )),
            Err(fs::TryLockError::Error(err)) => Err(err),
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Release explicitly; closing the file would also release it.
        let _ = self.file.unlock();
    }
}

/// An exclusive, crash-releasing lock that a daemon holds for its entire
/// lifetime, so two concurrent auto-starts cannot both serve one root. Like
/// [`WriterLock`], acquisition fails closed. The caller keeps the returned
/// `File` alive; the lock is released when it drops or the process exits.
pub(crate) fn daemon_lock(root: &Path) -> io::Result<fs::File> {
    let store = store_dir().ok_or_else(|| io::Error::other("no index directory; set WGREP_INDEX_DIR"))?;
    let slot = slot(&store, root);
    fs::create_dir_all(&slot)?;
    let file = fs::OpenOptions::new().create(true).read(true).write(true).open(slot.join("daemon.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "a daemon is already serving this root"))
        }
        Err(fs::TryLockError::Error(err)) => Err(err),
    }
}

/// Points `CURRENT` at `generation` and removes older generations that no
/// process still has mapped.
pub(crate) fn publish(generation: &Path) -> io::Result<()> {
    let slot = generation.parent().unwrap();
    let name = generation.file_name().unwrap().to_string_lossy().into_owned();
    // Unique per process: two concurrent publishers must not share the temp.
    let tmp = slot.join(format!("CURRENT.{}.tmp", std::process::id()));
    fs::write(&tmp, &name)?;
    fs::rename(&tmp, slot.join("CURRENT"))?;
    for entry in fs::read_dir(slot)?.flatten() {
        let other = entry.file_name().to_string_lossy().into_owned();
        if other.starts_with("index-") && other.ends_with(".wgi") && other != name {
            // Best effort: a mapped file cannot be deleted on Windows.
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_removes_dot_components() {
        let cwd = if cfg!(windows) { Path::new(r"C:\a\b") } else { Path::new("/a/b") };
        let expect = if cfg!(windows) { PathBuf::from(r"C:\a\c") } else { PathBuf::from("/a/c") };
        assert_eq!(absolute(Path::new("./../c/."), cwd), expect);
    }
}
