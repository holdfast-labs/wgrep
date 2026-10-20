//! Decides, per walked file, whether the index proves it cannot match.
//!
//! rg's walker still produces the file set, so ignore rules, globs, types and
//! every other walk flag behave exactly as in rg. A file is skipped only if
//! it is in the index, its size and mtime equal the indexed ones, and its
//! indexed trigrams rule out the query. Everything else is searched.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::format::{Bitset, Index};
use super::plan::{self, PlanOptions};
use super::{build, store};

thread_local! {
    /// Reused across files on one walker thread, so the per-file prune builds
    /// the relative path with no heap allocation per walked file.
    static REL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// The last directory seen on this walker thread and the index id range of
    /// its files. The walker visits a directory's entries back-to-back, so
    /// this turns the per-file full binary search into a search of one small
    /// range. It only narrows the search: a miss still means "not in the
    /// index", so a stale range can only cause a file to be searched.
    static DIR: RefCell<DirCursor> = const {
        RefCell::new(DirCursor { root: usize::MAX, parent: Vec::new(), lo: 0, hi: 0 })
    };
}

struct DirCursor {
    root: usize,
    parent: Vec<u8>,
    lo: usize,
    hi: usize,
}

struct Root {
    /// A search path exactly as rg's walker spells it.
    arg: PathBuf,
    /// That path relative to the index root.
    prefix: String,
    index: Arc<Index>,
    candidates: Arc<Bitset>,
}

pub(crate) struct Pruner {
    roots: Vec<Root>,
    considered: AtomicUsize,
    skipped: AtomicUsize,
    debug: bool,
    started: std::time::Instant,
}

impl Pruner {
    /// Returns `None` when the index cannot help: disabled, no index covers
    /// the search paths, or the patterns rule out no file.
    pub(crate) fn new(patterns: &[String], opts: &PlanOptions, paths: &[PathBuf], cwd: &Path) -> Option<Pruner> {
        if std::env::var_os("WGREP_NO_INDEX").is_some() {
            return None;
        }
        let debug = std::env::var_os("WGREP_INDEX_DEBUG").is_some();
        // Resolve the index for each path first; parse the pattern into a
        // trigram query only if some path actually has an index, so a
        // non-indexed search never pays for the plan.
        let mut resolved = Vec::new();
        for arg in paths {
            let abs = store::absolute(arg, cwd);
            if let Some((root, file)) = store::find(&abs) {
                resolved.push((arg.clone(), abs, root, file));
            }
        }
        if resolved.is_empty() {
            return None;
        }
        let query = plan::plan(patterns, opts);
        if debug {
            eprintln!("wgrep index: query {query:?}");
        }
        let mut opened: Vec<(PathBuf, Arc<Index>, Arc<Bitset>)> = Vec::new();
        let mut roots = Vec::new();
        for (arg, abs, root, file) in resolved {
            let (index, candidates) = match opened.iter().find(|(f, _, _)| *f == file) {
                Some((_, index, candidates)) => (index.clone(), candidates.clone()),
                None => {
                    let Ok(index) = Index::open(&file) else { continue };
                    // `None` means the query keeps every file.
                    let Some(candidates) = index.eval(&query) else { continue };
                    if debug {
                        eprintln!(
                            "wgrep index: {} candidates of {} indexed files in {}",
                            candidates.count(),
                            index.file_count(),
                            root.display()
                        );
                    }
                    let entry = (file.clone(), Arc::new(index), Arc::new(candidates));
                    opened.push(entry.clone());
                    (entry.1, entry.2)
                }
            };
            let Some(prefix) = build::relative(&abs, &root) else { continue };
            roots.push(Root { arg: arg.clone(), prefix, index, candidates });
        }
        if roots.is_empty() {
            return None;
        }
        Some(Pruner {
            roots,
            considered: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            debug,
            started: std::time::Instant::now(),
        })
    }

    pub(crate) fn skip(&self, dent: &ignore::DirEntry) -> bool {
        let skip = self.provably_no_match(dent);
        if self.debug {
            self.considered.fetch_add(1, Ordering::Relaxed);
            if skip {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
        skip
    }

    fn provably_no_match(&self, dent: &ignore::DirEntry) -> bool {
        if dent.is_stdin() {
            return false;
        }
        let path = dent.path();
        let Some((ri, root)) = self.roots.iter().enumerate().find(|(_, r)| path.starts_with(&r.arg)) else {
            return false;
        };
        let id = REL.with(|buf| {
            let mut rel = buf.borrow_mut();
            rel.clear();
            rel.extend_from_slice(root.prefix.as_bytes());
            if !build::append_relative(path, &root.arg, &mut rel) {
                return None;
            }
            find_cached(root, ri, rel.as_slice())
        });
        let Some(id) = id else { return false };
        // A candidate is searched regardless of freshness, so test it before
        // paying for metadata (a real syscall on platforms where the walker
        // does not cache it) — only a non-candidate needs the freshness check.
        if root.candidates.contains(id) {
            return false;
        }
        let Ok(md) = dent.metadata() else { return false };
        if !md.is_file()
            || md.len() != root.index.size(id)
            || build::mtime_ns(&md) != Some(root.index.mtime_ns(id))
        {
            return false;
        }
        // Freshness must be established by a change token. On Unix that is
        // `ctime` from the metadata; on Windows it needs the file handle. If
        // no token can be read, search the file — never skip on size+mtime
        // alone, so an equal-length rewrite with a restored mtime is caught.
        match build::change_of_meta(&md).or_else(|| build::change_of_path(path)) {
            Some(token) if token == root.index.change_ns(id) => true,
            _ => false,
        }
    }
}

/// `Index::find`, narrowed to the current directory's id range via the
/// thread-local cursor. Correctness does not depend on the cursor: a miss
/// within a wrong range is indistinguishable from "not indexed", and both
/// cause the file to be searched.
fn find_cached(root: &Root, ri: usize, rel: &[u8]) -> Option<usize> {
    let parent_end = rel.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    DIR.with(|cell| {
        let mut cur = cell.borrow_mut();
        if cur.root != ri || cur.parent.as_slice() != &rel[..parent_end] {
            cur.root = ri;
            cur.parent.clear();
            cur.parent.extend_from_slice(&rel[..parent_end]);
            let (lo, hi) = root.index.dir_range(&rel[..parent_end]);
            cur.lo = lo;
            cur.hi = hi;
        }
        root.index.find_in(cur.lo, cur.hi, rel)
    })
}

impl Drop for Pruner {
    fn drop(&mut self) {
        if self.debug {
            eprintln!(
                "wgrep index: skipped {} of {} walked files in {:.1} ms",
                self.skipped.load(Ordering::Relaxed),
                self.considered.load(Ordering::Relaxed),
                self.started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
