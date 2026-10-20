//! Daemon slot layout for wgrep-sem, separate from trigram index slots.
//!
//! Each indexed root gets `.../wgrep-sem/<fnv1a64>` holding `root` (collision
//! guard) and `daemon.json` (`port`, `token`, `pid`, `root`). Nothing here
//! shares files with the trigram index: stopping/starting one daemon can
//! never disturb the other.

use std::path::{Path, PathBuf};
use super::chunks::{self, Chunking};

/// Store root: `WSEM_DIR`, else platform cache `wgrep/sem`.
pub(crate) fn store_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WSEM_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("wgrep").join("sem"))
}

/// Stable key for a root: the absolute path with redundant separators
/// removed. Case folding and separator conversion are Windows-only —
/// on Unix filenames are case-sensitive and a backslash is an ordinary
/// filename character, so the key preserves both verbatim.
pub(crate) fn root_key(root: &Path) -> String {
    let key = root.to_string_lossy().into_owned();
    #[cfg(windows)]
    let key = key.replace('\\', "/");
    let key = key.trim_end_matches('/').to_string();
    if cfg!(windows) { key.to_lowercase() } else { key }
}

/// Absolute path with `.`/`..` removed lexically (no symlink resolution,
/// mirroring the trigram store: a different spelling simply misses).
pub(crate) fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let joined = cwd.join(path);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn hash_hex(root: &Path, mode: Chunking) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root_key(root).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    for b in chunks::chunking_name(mode).bytes() { h ^= u64::from(b); h = h.wrapping_mul(0x100000001b3); }
    format!("{h:016x}")
}

/// Slot dir for `root`, created on demand with the collision guard written.
pub(crate) fn slot_for(root: &Path, mode: Chunking) -> Option<PathBuf> {
    let slot = store_dir()?.join(hash_hex(root, mode));
    std::fs::create_dir_all(&slot).ok()?;
    let guard = slot.join("root");
    let key = format!("{}\n{}", root_key(root), chunks::chunking_name(mode));
    match std::fs::read_to_string(&guard) {
        Ok(prev) if prev == key => {}
        Ok(_) => return None, // hash collision with a different root: refuse.
        Err(_) => {
            std::fs::write(&guard, key).ok()?;
        }
    }
    Some(slot)
}
