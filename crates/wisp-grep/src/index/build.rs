//! Builds an index from the file list that rg's own walker produces.
//!
//! Trigram postings are accumulated in a [`spill::SpillBuffer`] instead of a
//! single in-memory map, so peak memory during a build is bounded by
//! `WGREP_BUILD_MEMORY_MB` (default 512) plus the small, corpus-size-
//! independent sections `format::write` keeps in memory (see its doc
//! comment), not by the size of the corpus itself.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::{fold, format, spill, store};

/// A file modified within this window of being read might change again
/// without its mtime changing, so it is left out of the index (and therefore
/// always searched).
pub(crate) const RACY_NS: i64 = 2_000_000_000;
const CHUNK: usize = 64;

/// Default budget, in bytes, for the postings held in memory before they are
/// spilled to a run file. `WGREP_BUILD_MEMORY_MB` overrides it.
const DEFAULT_MEMORY_BUDGET_MB: u64 = 512;

pub(crate) struct Summary {
    pub(crate) root: PathBuf,
    pub(crate) index_path: PathBuf,
    pub(crate) walked: usize,
    pub(crate) indexed: usize,
    pub(crate) source_bytes: u64,
    pub(crate) stats: format::BuildStats,
    pub(crate) seconds: f64,
}

pub(crate) fn mtime_ns(md: &fs::Metadata) -> Option<i64> {
    let d = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

// A change token that survives an mtime-forging, same-length rewrite, where
// the platform exposes one. Unix: the inode change time (`ctime`), which
// userspace cannot set. Windows: the NTFS change time, which requires the
// file handle (`GetFileInformationByHandleEx`), not `Metadata`. When no token
// can be read, the caller must search the file rather than trust size+mtime.

#[cfg(unix)]
pub(crate) fn change_of_meta(md: &fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    let secs = md.ctime().checked_mul(1_000_000_000)?;
    i64::try_from(secs.checked_add(md.ctime_nsec())?).ok()
}

#[cfg(not(unix))]
pub(crate) fn change_of_meta(md: &fs::Metadata) -> Option<i64> {
    let _ = md;
    None
}

#[cfg(unix)]
pub(crate) fn change_of_file(file: &fs::File) -> Option<i64> {
    change_of_meta(&file.metadata().ok()?)
}

#[cfg(unix)]
pub(crate) fn change_of_path(path: &Path) -> Option<i64> {
    change_of_meta(&fs::metadata(path).ok()?)
}

#[cfg(windows)]
pub(crate) fn change_of_file(file: &fs::File) -> Option<i64> {
    use std::os::windows::io::AsRawHandle;
    win::change_time(file.as_raw_handle() as win::Handle)
}

#[cfg(windows)]
thread_local! {
    /// Reused UTF-16 path buffer so a reconcile does not allocate one per file.
    static WIDE_PATH: std::cell::RefCell<Vec<u16>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(windows)]
pub(crate) fn change_of_path(path: &Path) -> Option<i64> {
    use std::os::windows::ffi::OsStrExt;
    WIDE_PATH.with(|buf| {
        let mut wide = buf.borrow_mut();
        wide.clear();
        wide.extend(path.as_os_str().encode_wide());
        wide.push(0);
        // Attribute-only open: no file data, just the metadata the change time
        // lives in.
        let handle = unsafe {
            win::CreateFileW(
                wide.as_ptr(),
                win::FILE_READ_ATTRIBUTES,
                win::FILE_SHARE_READ | win::FILE_SHARE_WRITE | win::FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                win::OPEN_EXISTING,
                win::FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == win::INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        let token = win::change_time(handle);
        unsafe {
            win::CloseHandle(handle);
        }
        token
    })
}

/// Change times for every entry in `dir`, in one directory-handle enumeration
/// (`GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`), so a reconcile
/// does not open each file on Windows. `None` falls back to a per-file read.
#[cfg(windows)]
pub(crate) fn dir_change_times(dir: &Path) -> Option<std::collections::HashMap<std::ffi::OsString, i64>> {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        win::CreateFileW(
            wide.as_ptr(),
            win::FILE_LIST_DIRECTORY,
            win::FILE_SHARE_READ | win::FILE_SHARE_WRITE | win::FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            win::OPEN_EXISTING,
            win::FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == win::INVALID_HANDLE_VALUE || handle.is_null() {
        return None;
    }
    let mut map = HashMap::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let ok = unsafe {
            win::GetFileInformationByHandleEx(
                handle,
                win::FILE_ID_BOTH_DIR_INFO_CLASS,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                buf.len() as u32,
            )
        };
        if ok == 0 {
            // A real error (not end-of-directory) means the map is incomplete;
            // do not cache a partial view, let the caller fall back per file.
            if unsafe { win::GetLastError() } != win::ERROR_NO_MORE_FILES {
                unsafe {
                    win::CloseHandle(handle);
                }
                return None;
            }
            break;
        }
        let mut off = 0usize;
        // FILE_ID_BOTH_DIR_INFO: NextEntryOffset@0, ChangeTime@32,
        // FileNameLength@60, FileName@104. Every read is bounds-checked before
        // it happens: the buffer is untrusted at this unsafe FFI boundary.
        const RECORD: usize = 104;
        while off + RECORD <= buf.len() {
            // SAFETY: the entry header was bounds-checked; reads are unaligned
            // so buffer alignment does not matter.
            let base = unsafe { buf.as_ptr().add(off) };
            let next = unsafe { std::ptr::read_unaligned(base as *const u32) } as usize;
            let change = unsafe { std::ptr::read_unaligned(base.add(32) as *const i64) };
            let name_len = unsafe { std::ptr::read_unaligned(base.add(60) as *const u32) } as usize;
            let name_end = match off.checked_add(RECORD).and_then(|v| v.checked_add(name_len)) {
                Some(end) if name_len % 2 == 0 && end <= buf.len() => end,
                _ => break,
            };
            // Read the UTF-16 name from its bytes so no 2-byte alignment is
            // assumed of the buffer, and no out-of-bounds read is possible.
            let name: Vec<u16> = buf[off + RECORD..name_end]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            map.insert(OsString::from_wide(&name), change);
            if next == 0 {
                break;
            }
            if next < RECORD || off.checked_add(next).is_none_or(|end| end > buf.len()) {
                break;
            }
            off += next;
        }
    }
    unsafe {
        win::CloseHandle(handle);
    }
    Some(map)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn change_of_file(file: &fs::File) -> Option<i64> {
    let _ = file;
    None
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn change_of_path(path: &Path) -> Option<i64> {
    let _ = path;
    None
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub(crate) type Handle = *mut c_void;

    #[repr(C)]
    pub(crate) struct FileBasicInfo {
        creation_time: i64,
        last_access_time: i64,
        last_write_time: i64,
        change_time: i64,
        file_attributes: u32,
    }

    pub(crate) const FILE_BASIC_INFO_CLASS: i32 = 0; // FileBasicInfo
    pub(crate) const FILE_ID_BOTH_DIR_INFO_CLASS: i32 = 10; // FileIdBothDirectoryInfo
    pub(crate) const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    pub(crate) const FILE_LIST_DIRECTORY: u32 = 0x0001;
    pub(crate) const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    pub(crate) const ERROR_NO_MORE_FILES: u32 = 18;
    pub(crate) const FILE_SHARE_READ: u32 = 0x0001;
    pub(crate) const FILE_SHARE_WRITE: u32 = 0x0002;
    pub(crate) const FILE_SHARE_DELETE: u32 = 0x0004;
    pub(crate) const OPEN_EXISTING: u32 = 3;
    pub(crate) const FILE_ATTRIBUTE_NORMAL: u32 = 0x0080;
    pub(crate) const INVALID_HANDLE_VALUE: Handle = usize::MAX as Handle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(crate) fn GetFileInformationByHandleEx(
            handle: Handle,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
        pub(crate) fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        pub(crate) fn CloseHandle(handle: Handle) -> i32;
        pub(crate) fn GetLastError() -> u32;
    }

    /// NTFS change time (100 ns ticks since 1601), or `None` on failure.
    pub(crate) fn change_time(handle: Handle) -> Option<i64> {
        let mut info =
            FileBasicInfo { creation_time: 0, last_access_time: 0, last_write_time: 0, change_time: 0, file_attributes: 0 };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FILE_BASIC_INFO_CLASS,
                &mut info as *mut FileBasicInfo as *mut c_void,
                std::mem::size_of::<FileBasicInfo>() as u32,
            )
        };
        (ok != 0).then_some(info.change_time)
    }
}

/// Minimum distinct trigrams before a file's trigram/length ratio is judged.
/// Below this, even a ratio of 1 is too small to matter.
const DIVERSITY_MIN: usize = 4_096;

/// Whether a file is so high-diversity (binary, compressed, random, minified)
/// that indexing it would add ~one posting per trigram for almost no pruning
/// benefit. Such files are left out and searched normally.
fn too_diverse(distinct: usize, folded_len: usize) -> bool {
    distinct >= DIVERSITY_MIN && distinct.saturating_mul(2) > folded_len.saturating_sub(2)
}

/// `path` relative to `base` as a `/`-joined string, when every component
/// is plain UTF-8.
pub(crate) fn relative(path: &Path, base: &Path) -> Option<String> {
    let rest = path.strip_prefix(base).ok()?;
    let mut out = String::new();
    for component in rest.components() {
        let Component::Normal(part) = component else { return None };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part.to_str()?);
    }
    Some(out)
}

/// Appends `path` relative to `base` to `out` as `/`-joined UTF-8, inserting a
/// separator before the first component when `out` is already non-empty. Writes
/// nothing and returns `false` when a component is not plain UTF-8 or `path` is
/// not under `base`. This is the allocation-free form of [`relative`] for the
/// per-file prune path, where a fresh `String` per walked file is measurable.
pub(crate) fn append_relative(path: &Path, base: &Path, out: &mut Vec<u8>) -> bool {
    let Ok(rest) = path.strip_prefix(base) else { return false };
    let mut first = true;
    for component in rest.components() {
        let Component::Normal(part) = component else { return false };
        let Some(part) = part.to_str() else { return false };
        if first {
            if !out.is_empty() && !out.ends_with(b"/") {
                out.push(b'/');
            }
            first = false;
        } else {
            out.push(b'/');
        }
        out.extend_from_slice(part.as_bytes());
    }
    true
}

pub(crate) fn memory_budget_bytes() -> u64 {
    std::env::var("WGREP_BUILD_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MEMORY_BUDGET_MB)
        .saturating_mul(1024 * 1024)
}

struct Indexed {
    entry: format::FileEntry,
    /// Trigram keys with their next-byte masks.
    trigrams: Vec<(u32, u8)>,
}

struct PhaseTimes {
    /// When false, no clock reads or atomic updates happen per file.
    on: bool,
    io: AtomicU64,
    fold: AtomicU64,
    trigram: AtomicU64,
}

fn index_one(
    path: &Path,
    rel: &str,
    scratch: &mut Option<fold::MaskScratch>,
    content: &mut Vec<u8>,
    folded: &mut Vec<u8>,
    phases: &PhaseTimes,
) -> Option<Indexed> {
    let t = phases.on.then(Instant::now);
    // One open, then stats on the handle. `File::metadata` is a stat on the
    // open handle, which cannot be redirected to another file. The captured
    // read must match the handle both before and after it (size and mtime
    // unchanged across the read), so a file written while being read is not
    // recorded with mixed content; the racy-window check below is a second
    // guard for a writer that restores mtime.
    let mut file = fs::File::open(path).ok()?;
    let before = file.metadata().ok()?;
    let before_change = change_of_file(&file);
    content.clear();
    content.reserve(before.len() as usize);
    std::io::Read::read_to_end(&mut file, content).ok()?;
    let after = file.metadata().ok()?;
    let after_change = change_of_file(&file);
    let (size, mtime) = (after.len(), mtime_ns(&after)?);
    let change = after_change.unwrap_or(0);
    // Size, mtime and the change token must all be unchanged across the read,
    // so a same-size write that restores mtime cannot leave mixed content
    // recorded under the new token.
    if before.len() != size
        || mtime_ns(&before)? != mtime
        || content.len() as u64 != size
        || before_change != after_change
    {
        return None;
    }
    let now = mtime_ns_now()?;
    if mtime >= now - RACY_NS {
        return None;
    }
    // rg transcodes files that start with a UTF-16 byte order mark, so their
    // raw bytes say nothing about what it matches.
    if content.starts_with(&[0xFF, 0xFE]) || content.starts_with(&[0xFE, 0xFF]) {
        return None;
    }
    if let Some(t) = t {
        phases.io.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    let t = phases.on.then(Instant::now);
    fold::fold_into(content, folded);
    if let Some(t) = t {
        phases.fold.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    let mut trigrams = Vec::new();
    let t = phases.on.then(Instant::now);
    fold::trigram_masks(folded, scratch, &mut trigrams);
    if let Some(t) = t {
        phases.trigram.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    // A high-diversity file would add roughly one posting per trigram while
    // pruning almost nothing; skip it so it is searched normally.
    if too_diverse(trigrams.len(), folded.len()) {
        return None;
    }
    Some(Indexed {
        entry: format::FileEntry { rel: rel.to_owned(), size, mtime_ns: mtime, change_ns: change },
        trigrams,
    })
}

pub(crate) fn mtime_ns_now() -> Option<i64> {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()
}

/// Indexes `files`, the walker's output for the search path `root_arg`, and
/// publishes the result for the absolute directory `root`.
pub(crate) fn build(root: &Path, root_arg: &Path, files: Vec<PathBuf>, dense_percent: u64) -> io::Result<Summary> {
    let started = Instant::now();
    let debug = std::env::var_os("WGREP_BUILD_DEBUG").is_some();
    let mut lap = Instant::now();
    let report = |name: &str, lap: &mut Instant| {
        if debug {
            eprintln!("wgrep build: {name:<16} {:.3}s", lap.elapsed().as_secs_f64());
            *lap = Instant::now();
        }
    };
    let walked = files.len();
    let mut named: Vec<(String, PathBuf)> =
        files.into_iter().filter_map(|p| Some((relative(&p, root_arg)?, p))).collect();
    named.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    named.dedup_by(|a, b| a.0 == b.0);
    report("scan+sort", &mut lap);

    // One writer per root: concurrent builds serialize here (readers never
    // take this lock). The generation name is then reserved exclusively.
    let _lock = store::WriterLock::acquire(root)?;
    // Reserved up front so the spill and intermediate-merge runs below have
    // somewhere to live: a directory next to this generation's index file,
    // inside the index slot (never inside the searched tree). Removed once
    // the build finishes, including on error.
    let index_path = store::new_generation(root)?;
    let tmp_dir = index_path.with_extension("build.tmp");
    fs::create_dir_all(&tmp_dir)?;
    let _tmp_guard = spill::TmpDirGuard(tmp_dir.clone());
    report("setup", &mut lap);

    let chunks: Vec<&[(String, PathBuf)]> = named.chunks(CHUNK).collect();
    let next = AtomicUsize::new(0);
    // Chunks merged so far. Workers stay within `window` chunks of it, so a
    // slow chunk (for example one full of huge generated files) cannot let
    // finished results pile up without bound while the merge waits for it.
    let merged = AtomicUsize::new(0);
    let threads = std::env::var("WGREP_BUILD_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let window = threads * 2;
    let mut entries = Vec::with_capacity(named.len());
    let mut source_bytes = 0u64;
    let mut spill_buf = spill::SpillBuffer::new(tmp_dir.clone(), memory_budget_bytes());
    let mut spill_err: Option<io::Error> = None;
    let worker_ns = AtomicU64::new(0);
    let phases = PhaseTimes { on: debug, io: AtomicU64::new(0), fold: AtomicU64::new(0), trigram: AtomicU64::new(0) };
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::sync_channel::<(usize, Vec<Option<Indexed>>)>(threads * 4);
        for _ in 0..threads {
            let tx = tx.clone();
            let (chunks, next, merged, worker_ns, phases) = (&chunks, &next, &merged, &worker_ns, &phases);
            scope.spawn(move || {
                let mut scratch = None;
                // Reused across every file this worker handles, so a full read
                // and a folded copy are not reallocated per file.
                let mut content = Vec::new();
                let mut folded = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(chunk) = chunks.get(i) else { break };
                    while i >= merged.load(Ordering::Acquire) + window {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    let t = Instant::now();
                    let done = chunk
                        .iter()
                        .map(|(rel, path)| {
                            index_one(path, rel.as_str(), &mut scratch, &mut content, &mut folded, phases)
                        })
                        .collect();
                    worker_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    if tx.send((i, done)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // Merge chunks in order so ids are assigned by sorted path, and each
        // key's postings are pushed to the spill buffer in increasing id
        // order (spill.rs relies on this to merge runs by concatenation).
        let mut pending = BTreeMap::new();
        let mut want = 0;
        for (i, done) in rx {
            pending.insert(i, done);
            while let Some(done) = pending.remove(&want) {
                for indexed in done.into_iter().flatten() {
                    let id = entries.len() as u32;
                    source_bytes += indexed.entry.size;
                    if spill_err.is_none() {
                        for (key, mask) in indexed.trigrams {
                            if let Err(e) = spill_buf.push(key, id, mask) {
                                spill_err = Some(e);
                                break;
                            }
                        }
                    }
                    entries.push(indexed.entry);
                }
                want += 1;
                merged.store(want, Ordering::Release);
            }
        }
    });
    if debug {
        eprintln!("wgrep build: worker_cpu       {:.1}s", worker_ns.load(Ordering::Relaxed) as f64 / 1e9);
        eprintln!(
            "wgrep build: io/fold/tri      {:.1}s / {:.1}s / {:.1}s (summed over threads)",
            phases.io.load(Ordering::Relaxed) as f64 / 1e9,
            phases.fold.load(Ordering::Relaxed) as f64 / 1e9,
            phases.trigram.load(Ordering::Relaxed) as f64 / 1e9,
        );
    }
    report("workers+merge", &mut lap);
    if let Some(e) = spill_err {
        return Err(e);
    }

    let runs = spill_buf.finish()?;
    report("spill finish", &mut lap);
    let runs = spill::reduce_to_fanin(runs, &tmp_dir, spill::FAN_IN)?;
    report("reduce", &mut lap);
    let merged = spill::MergeIter::open(&runs)?;
    report("merge open", &mut lap);

    let header = serde_json::json!({
        "version": format::VERSION,
        "root": root.to_string_lossy(),
        "files": entries.len(),
        "source_bytes": source_bytes,
        "dense_percent": dense_percent,
        "built_unix_ns": mtime_ns_now(),
    });
    let stats = format::write(&index_path, &header, &entries, merged, dense_percent)?;
    store::publish(&index_path)?;
    report("format+publish", &mut lap);
    if debug {
        eprintln!("wgrep build: {:<16} {:.3}s", "total", started.elapsed().as_secs_f64());
    }
    Ok(Summary {
        root: root.to_path_buf(),
        index_path,
        walked,
        indexed: entries.len(),
        source_bytes,
        stats,
        seconds: started.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A tiny deterministic PRNG so the test below can span many distinct
    /// trigram keys without touching a real filesystem.
    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            self.0
        }
    }

    /// Fake per-file trigrams: `file_count` files, each with up to
    /// `keys_per_file` distinct keys drawn from a small key space so keys
    /// repeat across files (as real trigrams do).
    fn fake_trigrams(file_count: usize, keys_per_file: usize) -> Vec<Vec<(u32, u8)>> {
        let mut rng = Lcg(0x1234_5678);
        let mut out = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            let mut keys: Vec<(u32, u8)> = Vec::with_capacity(keys_per_file);
            for _ in 0..keys_per_file {
                keys.push((rng.next() % 500, (rng.next() % 256) as u8));
            }
            keys.sort_unstable();
            keys.dedup_by_key(|p| p.0);
            out.push(keys);
        }
        out
    }

    /// Runs the spill/merge/format pipeline directly (no filesystem walk)
    /// for `trigrams` (file id = index) under `budget_bytes`, with a fan-in
    /// small enough to force multiple merge levels once there are more than
    /// a handful of runs, and returns the raw index file bytes.
    fn build_with_budget(dir: &Path, name: &str, trigrams: &[Vec<(u32, u8)>], budget_bytes: u64) -> Vec<u8> {
        let tmp_dir = dir.join(format!("{name}.tmp"));
        fs::create_dir_all(&tmp_dir).unwrap();
        let mut spill_buf = spill::SpillBuffer::new(tmp_dir.clone(), budget_bytes);
        for (id, keys) in trigrams.iter().enumerate() {
            for &(key, mask) in keys {
                spill_buf.push(key, id as u32, mask).unwrap();
            }
        }
        let runs = spill_buf.finish().unwrap();
        let runs = spill::reduce_to_fanin(runs, &tmp_dir, 4).unwrap();
        let merged = spill::MergeIter::open(&runs).unwrap();

        let entries: Vec<format::FileEntry> = (0..trigrams.len())
            .map(|i| format::FileEntry { rel: format!("file{i:04}.txt"), size: 100, mtime_ns: i as i64, change_ns: 0 })
            .collect();
        let header = serde_json::json!({"version": format::VERSION, "files": entries.len()});
        let index_path = dir.join(format!("{name}.wgi"));
        format::write(&index_path, &header, &entries, merged, 100).unwrap();
        fs::remove_dir_all(&tmp_dir).ok();

        let mut bytes = Vec::new();
        fs::File::open(&index_path).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn spill_budget_does_not_change_index_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let trigrams = fake_trigrams(50, 10);

        // A huge budget never spills mid-build, so the buffer's single
        // final flush is the only run (one code path, per the module doc).
        // A tiny budget flushes after only a few postings, and with a
        // fan-in of 4 (set in `build_with_budget`) that is enough runs to
        // need more than one merge level. Both must produce byte-identical
        // output, including the header, since the header passed here is
        // the same `serde_json::Value` both times.
        let huge = build_with_budget(dir.path(), "huge", &trigrams, 1 << 30);
        let tiny = build_with_budget(dir.path(), "tiny", &trigrams, 40);
        assert_eq!(huge, tiny);
        assert!(!huge.is_empty());
    }
}
