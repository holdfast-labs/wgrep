//! Discovery and chunking: same ignore semantics as the default walk,
//! line-window passages with source-verifiable ranges.
//!
//! Files are listed with the `ignore` walker under ripgrep's default rules
//! (hidden skipped, gitignore/ignore/rgignore honored, no symlink
//! following), sorted by bytes for determinism. Each file becomes one or
//! more whole-line passages targeting ~2048 characters. Binary (NUL)
//! files and non-UTF-8 files are skipped explicitly; CRLF is retained and
//! splitting is on `'\n'` only, so ranges always index the source's own
//! lines and served content is byte-exact source text.
//!
//! Freshness uses the same rule as the trigram index: size + mtime +
//! platform change token (Unix `ctime`; on Windows the handle token is not
//! yet wired here, so Windows freshness is size + mtime — see the report).

use std::path::{Path, PathBuf};

/// Target passage size in characters. Whole lines are always kept.
pub(crate) const TARGET_CHARS: usize = 2048;

/// Files larger than this are skipped (recorded in stats, never ranked).
pub(crate) const MAX_FILE_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Chunking {
    #[default]
    Window,
    RustFn,
}

pub(crate) fn chunking_name(mode: Chunking) -> &'static str {
    match mode { Chunking::Window => "window", Chunking::RustFn => "rust-fn" }
}

/// One rankable passage. `path` is `/`-separated relative to the indexed
/// root; `start`/`end` are 1-based line numbers; `content` is the exact
/// source lines (LF-normalized).
#[derive(Clone, Debug)]
pub(crate) struct Chunk {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub content: String,
}

/// Freshness fingerprint of a file on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub len: u64,
    pub mtime_ns: u128,
    pub token: Option<i64>,
}

pub(crate) fn mtime_ns(md: &std::fs::Metadata) -> Option<u128> {
    md.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

#[cfg(unix)]
fn change_token(md: &std::fs::Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    Some((md.ctime() as i64) * 1_000_000_000 + md.ctime_nsec() as i64)
}

#[cfg(not(unix))]
fn change_token(_md: &std::fs::Metadata) -> Option<i64> {
    // Windows NTFS change time needs the file handle; not wired yet.
    // Freshness there is size + mtime until the handle token lands.
    None
}

pub(crate) fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    Some(Fingerprint { len: md.len(), mtime_ns: mtime_ns(&md)?, token: change_token(&md) })
}

/// List discovered files under `root`, sorted by raw bytes for a
/// deterministic build order.
///
/// Walker settings match ripgrep/wgrep defaults exactly (`hidden`,
/// `parents`, `ignore`, `git_global`, `git_ignore`, `git_exclude` on;
/// `require_git` on so git rules need a repo, exactly like `rg`;
/// symlinks not followed; `.rgignore` honored): the same file set `rg`
/// without flags would search. Explicit `--hidden`/`--no-ignore` style
/// overrides are NOT supported — the default set is fixed. (Covers review:
/// settings verified against `configure_walk_builder` in
/// `src/rg/flags/hiargs.rs`.)
pub(crate) fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .ignore(true)
        .require_git(true)
        .follow_links(false);
    builder.add_custom_ignore_filename(".rgignore");
    let mut files: Vec<PathBuf> = builder
        .build()
        .filter_map(|r| r.ok())
        .filter(|d| d.file_type().is_some_and(|t| t.is_file()))
        .map(|d| d.into_path())
        .collect();
    files.sort_by(|a, b| {
        a.as_os_str().as_encoded_bytes().cmp(b.as_os_str().as_encoded_bytes())
    });
    files
}

/// `/`-separated path of `abs` relative to `root`, or `None` when outside.
/// Separator conversion is Windows-only: on Unix a backslash is a valid
/// filename character and must be preserved verbatim.
pub(crate) fn relative(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut s = rel.to_string_lossy().into_owned();
    if cfg!(windows) {
        s = s.replace('\\', "/");
    }
    if s.is_empty() {
        return None;
    }
    // Guard: never emit absolute or parent-escaping passages.
    if s.starts_with('/') || s.split('/').any(|c| c == "..") {
        return None;
    }
    // Strip a leading `./` if the walker produced one.
    if let Some(rest) = s.strip_prefix("./") {
        s = rest.to_owned();
    }
    Some(s)
}

/// Chunk one file's text into whole-line passages.
pub(crate) fn chunk_text(rel: &str, text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut chunks = Vec::new();
    let mut first = 0usize;
    let mut chars = 0usize;
    for (i, line) in lines.iter().enumerate() {
        chars += line.chars().count();
        if chars >= TARGET_CHARS {
            chunks.push(Chunk {
                path: rel.to_owned(),
                start: first + 1,
                end: i + 1,
                content: lines[first..=i].concat(),
            });
            first = i + 1;
            chars = 0;
        }
    }
    if first < lines.len() {
        chunks.push(Chunk {
            path: rel.to_owned(),
            start: first + 1,
            end: lines.len(),
            content: lines[first..].concat(),
        });
    }
    chunks
}

pub(crate) fn chunk_text_mode(rel: &str, text: &str, mode: Chunking) -> anyhow::Result<Vec<Chunk>> {
    match mode {
        Chunking::Window => Ok(chunk_text(rel, text)),
        Chunking::RustFn if rel.ends_with(".rs") => super::rust_chunks::chunk_rust(rel, text),
        Chunking::RustFn => Ok(chunk_text(rel, text)),
    }
}

/// Read + chunk one file. Returns `(fingerprint, chunks)`; too-large files
/// yield an empty chunk list (counted as skipped by the caller).
/// Content contract (review: source-verifiable ranges):
/// - exact UTF-8 only: files that are not valid UTF-8 are skipped
///   explicitly (empty chunks, counted as binary-skipped) — never lossy
///   text, which would serve passages that match no source bytes;
/// - CRLF is RETAINED and splitting is on `'\n'` only: a lone `'\r'` never
///   creates a line break, so `start`/`end` always index the source's own
///   newline-separated lines. `content` is the exact source line bytes.
pub(crate) fn chunk_file(root: &Path, abs: &Path) -> Option<(Fingerprint, Vec<Chunk>)> {
    chunk_file_mode(root, abs, Chunking::Window)
}

pub(crate) fn chunk_file_mode(root: &Path, abs: &Path, mode: Chunking) -> Option<(Fingerprint, Vec<Chunk>)> {
    let fp = fingerprint(abs)?;
    if fp.len > MAX_FILE_BYTES {
        return Some((fp, Vec::new()));
    }
    let rel = relative(root, abs)?;
    let raw = std::fs::read(abs).ok()?;
    if raw.contains(&0) {
        return Some((fp, Vec::new()));
    }
    let Ok(text) = std::str::from_utf8(&raw) else {
        // Unsupported encoding: skip explicitly, never serve lossy text.
        return Some((fp, Vec::new()));
    };
    Some((fp, chunk_text_mode(&rel, text, mode).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn crlf_retained_and_ranges_verifiable() {
        let dir = std::env::temp_dir().join("wsem-chunks-crlf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = tmpfile(&dir, "a.txt", b"line1\r\nline2\r\nline3\r\n");
        let (fp, chunks) = chunk_file(&dir, &abs).unwrap();
        assert!(!chunks.is_empty());
        assert!(fp.len > 0);
        // Content keeps CRLF; re-splitting the live source on '\n' must
        // reproduce the served lines exactly.
        let raw = std::fs::read(&abs).unwrap();
        let text = std::str::from_utf8(&raw).unwrap();
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        for c in &chunks {
            assert_eq!(lines[c.start - 1..c.end].concat(), c.content);
            assert!(c.content.contains("\r\n"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lone_cr_is_not_a_line_break() {
        let dir = std::env::temp_dir().join("wsem-chunks-lonecr");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = tmpfile(&dir, "a.txt", b"one\rtwo\nthree\n");
        let (_, chunks) = chunk_file(&dir, &abs).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start, chunks[0].end), (1, 2));
        assert_eq!(chunks[0].content, "one\rtwo\nthree\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_utf8_skipped_explicitly() {
        let dir = std::env::temp_dir().join("wsem-chunks-badutf8");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = tmpfile(&dir, "a.txt", b"valid start \xff\xfe invalid\n");
        let (_, chunks) = chunk_file(&dir, &abs).unwrap();
        assert!(chunks.is_empty(), "non-UTF8 must skip, never serve lossy text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn backslash_filename_preserved_on_unix() {
        // On Unix a backslash is an ordinary filename character: it must
        // survive into the passage path verbatim, not become a separator.
        let dir = std::env::temp_dir().join("wsem-chunks-backslash");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = tmpfile(&dir, "a\\b.txt", b"hello\n");
        let rel = relative(&dir, &abs).unwrap();
        assert_eq!(rel, "a\\b.txt");
        let (_, chunks) = chunk_file(&dir, &abs).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].path, "a\\b.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
