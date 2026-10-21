//! `wgrep --index-serve PATH`: a live index for one root.
//!
//! The daemon keeps the current file set in memory and answers "which files
//! under this prefix may match this trigram query?" so clients skip rg's
//! directory walk. A file watcher keeps the set current:
//!
//! - an event on a known file marks it dirty, and dirty files are always
//!   returned as candidates;
//! - creations, renames and removals of known paths are applied
//!   incrementally with scoped walks rooted at the change (same walker, same
//!   rules, cost of the change not the tree); ignore-file changes and watcher
//!   overflow set `needs_rescan`, and the next query first re-walks with rg's
//!   walker (so ignore rules are always rg's);
//! - once enough files are dirty, a background rebuild publishes a new
//!   generation and the daemon swaps to it.
//!
//! Changes are visible once the OS delivers their notification. Clients
//! authenticate with a random token stored in the (user-private) slot
//! directory, and the daemon listens only on loopback.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::flags::{HiArgs, ParseResult};

use super::format::Index;
use super::{build, ignore_deps, plan, store, watch_budget};

const REBUILD_MIN_DIRTY: usize = 64;
const REBUILD_INTERVAL: Duration = Duration::from_secs(30);

/// A file's metadata and change token, read before taking the state lock.
struct Stat {
    md: std::io::Result<std::fs::Metadata>,
    token: Option<i64>,
}

impl Stat {
    fn of(path: &Path) -> Stat {
        let md = std::fs::metadata(path);
        let token = md.as_ref().ok().and_then(|m| build::change_of_meta(m).or_else(|| build::change_of_path(path)));
        Stat { md, token }
    }
}

/// One candidate: an index id (its path is read from `index` after the lock
/// is released) or a walked file absent from the index.
pub(crate) enum Hit {
    Id(u32),
    Added(String),
}

/// A query's result. Paths are materialized after the lock is released.
pub(crate) struct Answer {
    total: usize,
    index: Arc<Index>,
    hits: Vec<Hit>,
}

/// Per-index-id state: whether the file was seen in the last walk (so it
/// exists) and whether its indexed record still describes it.
#[derive(Clone, Copy)]
struct IdState {
    /// Position in the last walk (rg's walk order).
    seq: u32,
    seen: bool,
    current: bool,
}

/// Why a full walk ran. Counted per walk so the incremental-freshness work
/// can prove ordinary changes stop causing full walks (see
/// `docs/WGREP_INCREMENTAL_FRESHNESS.md`, Testing). When several changes pile
/// up before one walk, the last one wins.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkCause {
    /// The first walk: the daemon starts with a rescan scheduled.
    Startup,
    /// The watcher reported lost events (`need_rescan`) or errored.
    Overflow,
    /// An ignore source outside the watched root changed.
    OutsideDep,
    /// Today's fallback for ordinary changes (create, rename, delete of an
    /// unknown path, ignore-file edits): the cases the incremental phases
    /// convert to scoped walks.
    Scoped,
    /// An event arrived during a reconcile, forcing another full walk.
    PendingRetry,
    /// A background rebuild published a new generation, which the daemon
    /// adopts by re-walking.
    Rebuild,
}

/// Full walks by cause, reported by the `stats` control request.
#[derive(Default)]
struct WalkCounts {
    startup: u64,
    overflow: u64,
    outside_dep: u64,
    scoped: u64,
    pending_retry: u64,
    rebuild: u64,
}

impl WalkCounts {
    fn bump(&mut self, cause: WalkCause) {
        match cause {
            WalkCause::Startup => self.startup += 1,
            WalkCause::Overflow => self.overflow += 1,
            WalkCause::OutsideDep => self.outside_dep += 1,
            WalkCause::Scoped => self.scoped += 1,
            WalkCause::PendingRetry => self.pending_retry += 1,
            WalkCause::Rebuild => self.rebuild += 1,
        }
    }
}

/// A file's index-comparable clock: size, mtime and change token, read as
/// one pre-lock stat so scoped examinations can resolve it against the
/// current index at apply time.
struct FileClock {
    len: u64,
    mtime_ns: Option<i64>,
    token: Option<i64>,
}

impl FileClock {
    fn of(md: &std::fs::Metadata, token: Option<i64>) -> FileClock {
        FileClock { len: md.len(), mtime_ns: build::mtime_ns(md), token }
    }
}

/// What a scoped examination concluded about one watched path. Data only:
/// ids and currency are resolved against the current index under the write
/// lock at apply time, so a generation swap in between cannot corrupt state.
enum ScopedAction {
    /// A file a scoped walk yielded, with its stat.
    Found { rel: String, clock: FileClock },
    /// A path confirmed absent (or never yieldable, like a symlink): forget
    /// it and everything under it.
    Gone { rel: String },
}

/// Outcome of examining one watched path outside the state lock.
enum ExamineOutcome {
    Actions(Vec<ScopedAction>),
    /// This path needs a full walk; the caller flags it and applies any
    /// sibling actions (a later full walk subsumes them).
    Fallback,
}

pub(crate) struct State {
    root_arg: PathBuf,
    /// The absolute root, for re-resolving ignore dependencies.
    root: PathBuf,
    args: HiArgs,
    index: Arc<Index>,
    /// Ignore sources outside the watched root (ancestor `.ignore`/`.gitignore`/
    /// `.rgignore`, Git's `info/exclude`, the global gitignore). Re-checked
    /// before every query; a change forces a reconcile.
    deps: ignore_deps::Deps,
    /// One entry per index id (`0..index.file_count()`). The index's own
    /// sorted path table is the only copy of the paths, so the daemon stores
    /// just a couple of flags and the walk order per file.
    ids: Vec<IdState>,
    /// Files the walk saw that are not in the index (new), by path relative
    /// to `root_arg`, with their walk order. A map (not a `Vec`) so creating
    /// or deleting one file is O(1) instead of a linear scan.
    added: HashMap<String, u32>,
    needs_rescan: bool,
    /// Why the pending rescan runs. Set by `flag_rescan` alongside
    /// `needs_rescan`; only read when a walk actually runs.
    rescan_cause: WalkCause,
    /// Full walks by cause, reported by the `stats` control request.
    walks: WalkCounts,
    /// Next walk-order position for incrementally added files. A full walk
    /// resets it to the walked file count; scoped additions take positions
    /// after everything else (see the order discussion in
    /// `docs/WGREP_INCREMENTAL_FRESHNESS.md`).
    next_seq: u32,
    dirty: usize,
    /// Set while a reconcile holds the state lock. The watcher thread reads
    /// this lock-free and drops events instead of blocking on the lock and
    /// piling up an unbounded channel (a reconcile walks and stats the whole
    /// tree, and its own reads fire events). Any dropped event just escalates
    /// to another reconcile, so nothing is lost.
    reconciling: Arc<AtomicBool>,
    /// Set when an event arrives during a reconcile.
    missed: Arc<AtomicBool>,
    /// A walked path could not be expressed as UTF-8 relative to the root, so
    /// the candidate list cannot name every file the walk would search. The
    /// index then declines to prune rather than risk a dropped match.
    unprunable: bool,
}

impl State {
    /// Build the state an embedded text index uses. The standalone daemon
    /// builds the same value in `serve`.
    pub(crate) fn new(
        args: HiArgs,
        root_arg: PathBuf,
        root: PathBuf,
        index: Arc<Index>,
        reconciling: Arc<AtomicBool>,
        missed: Arc<AtomicBool>,
    ) -> State {
        let deps = ignore_deps::Deps::snapshot(&root);
        State {
            root_arg,
            root,
            args,
            index,
            deps,
            ids: Vec::new(),
            added: HashMap::new(),
            needs_rescan: true,
            rescan_cause: WalkCause::Startup,
            walks: WalkCounts::default(),
            next_seq: 0,
            dirty: 0,
            reconciling,
            missed,
            unprunable: false,
        }
    }

    /// Live counters for `wg status`.
    pub(crate) fn text_stats(&self) -> serde_json::Value {
        serde_json::json!({
            "files": self.index.file_count(),
            "dirty": self.dirty,
            "added": self.added.len(),
            "needs_rescan": self.needs_rescan,
            "unprunable": self.unprunable,
            "walks": {
                "startup": self.walks.startup,
                "overflow": self.walks.overflow,
                "outside_dep": self.walks.outside_dep,
                "scoped": self.walks.scoped,
                "pending_retry": self.walks.pending_retry,
                "rebuild": self.walks.rebuild,
            },
        })
    }

    /// Whether the index's record for `id` matches `md` and a change `token`.
    fn record_matches(index: &Index, id: usize, md: &std::fs::Metadata, token: Option<i64>) -> bool {
        Self::record_matches_clock(index, id, &FileClock::of(md, token))
    }

    /// Whether the index's record for `id` matches a previously read clock.
    /// Lets scoped examinations (which stat outside the state lock) resolve
    /// against the current index at apply time.
    fn record_matches_clock(index: &Index, id: usize, clock: &FileClock) -> bool {
        clock.len == index.size(id)
            && clock.mtime_ns == Some(index.mtime_ns(id))
            && clock.token.is_some_and(|t| t == index.change_ns(id))
    }

    /// Whether absolute `dir` holds a file the daemon knows is visible: a
    /// seen index id or an `added` entry under it. The fast path for
    /// `ancestors_visible`: a directory with a visible known file is itself
    /// visible, which usually ends the climb after one level.
    fn dir_has_known_file(&self, dir: &Path) -> bool {
        if self.added.keys().any(|rel| self.root.join(rel).starts_with(dir)) {
            return true;
        }
        let n = self.index.file_count().min(self.ids.len());
        (0..n).filter(|&id| self.ids[id].seen).any(|id| {
            std::str::from_utf8(self.index.path(id)).is_ok_and(|rel| self.root.join(rel).starts_with(dir))
        })
    }

    /// Schedules a full walk for `cause`. Every `needs_rescan = true` site
    /// goes through here so the `stats` counters see today's fallbacks.
    fn flag_rescan(&mut self, cause: WalkCause) {
        self.needs_rescan = true;
        self.rescan_cause = cause;
    }

    /// Re-walks the root with rg's walker and matches files to the index.
    /// The watcher drops events while this holds the lock; a change during the
    /// walk escalates to another reconcile rather than being lost.
    fn reconcile(&mut self) -> anyhow::Result<()> {
        let cause = self.rescan_cause;
        self.reconciling.store(true, Ordering::SeqCst);
        // Snapshot the outside-root ignore dependencies *before* the walk, so a
        // change made while it runs differs from the snapshot and forces the
        // next query to reconcile again rather than trusting a stale walk.
        self.deps = ignore_deps::Deps::snapshot(&self.root);
        let result = self.reconcile_walk();
        self.reconciling.store(false, Ordering::SeqCst);
        if self.missed.swap(false, Ordering::SeqCst) {
            self.flag_rescan(WalkCause::PendingRetry);
        }
        trim_working_set();
        result?;
        self.walks.bump(cause);
        Ok(())
    }

    fn reconcile_walk(&mut self) -> anyhow::Result<()> {
        let index = self.index.clone();
        let n = index.file_count();
        let mut ids = vec![IdState { seq: 0, seen: false, current: false }; n];
        let mut added: HashMap<String, u32> = HashMap::new();
        let mut dirty = 0usize;
        self.unprunable = false;
        let haystacks = self.args.haystack_builder();
        // Windows: change times come from one directory handle per directory,
        // not one open per file. The walk visits a directory's entries
        // contiguously, so only the current directory's map is kept (one entry
        // per file would otherwise be a whole-tree allocation).
        #[cfg(windows)]
        let mut dir_cache: Option<(PathBuf, std::collections::HashMap<std::ffi::OsString, i64>)> = None;
        let mut seq = 0u32;
        for result in self.args.walk_builder()?.build() {
            let Some(hay) = haystacks.build_from_result(result) else { continue };
            let Some(dent) = hay.dent() else { continue };
            let Some(rel) = build::relative(dent.path(), &self.root_arg) else {
                self.unprunable = true;
                continue;
            };
            match index.find(rel.as_bytes()) {
                Some(id) if (id as usize) < n => {
                    let current = match dent.metadata() {
                        Ok(md) => {
                            #[cfg(unix)]
                            let token = build::change_of_meta(&md);
                            #[cfg(windows)]
                            let token = {
                                let parent = dent.path().parent().unwrap_or(Path::new(""));
                                if dir_cache.as_ref().is_none_or(|(p, _)| p != parent) {
                                    let entries = build::dir_change_times(parent).unwrap_or_default();
                                    dir_cache = Some((parent.to_path_buf(), entries));
                                }
                                dent.path()
                                    .file_name()
                                    .and_then(|name| dir_cache.as_ref().and_then(|(_, m)| m.get(name).copied()))
                            };
                            #[cfg(not(any(unix, windows)))]
                            let token = None;
                            Self::record_matches(&index, id, &md, token)
                        }
                        Err(_) => false,
                    };
                    ids[id as usize] = IdState { seq, seen: true, current };
                    if !current {
                        dirty += 1;
                    }
                }
                _ => {
                    // Not in the index: either genuinely new, or deliberately
                    // excluded by the builder (binary, high-diversity, UTF-16
                    // BOM, racy). Both are searched directly as candidates, so
                    // they are never a *rebuild* signal: counting them would
                    // make a tree with any excluded files rebuild forever.
                    added.insert(rel, seq);
                }
            }
            seq = seq.wrapping_add(1);
        }
        self.ids = ids;
        self.added = added;
        self.dirty = dirty;
        self.needs_rescan = false;
        self.next_seq = seq;
        Ok(())
    }

    /// Next walk-order position for incrementally added files.
    fn alloc_seq(&mut self) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    /// Applies one scoped `Found`: the index knows the path or it is new.
    /// An already-current file keeps its walk order, so re-examining a
    /// pre-existing directory does not churn candidate order.
    fn apply_found(&mut self, rel: String, clock: &FileClock) {
        match self.index.find(rel.as_bytes()) {
            Some(id) if (id as usize) < self.ids.len() => {
                let current = Self::record_matches_clock(&self.index, id as usize, clock);
                let st = self.ids[id as usize];
                if st.seen && st.current && current {
                    return;
                }
                let seq = self.alloc_seq();
                let st = &mut self.ids[id as usize];
                let was = st.seen && !st.current;
                st.seen = true;
                st.current = current;
                st.seq = seq;
                if !was && !current {
                    self.dirty += 1;
                } else if was && current {
                    self.dirty = self.dirty.saturating_sub(1);
                }
            }
            // Either genuinely new or an id beyond a swapped generation's
            // table (the scheduled rescan fixes the layout; meanwhile the
            // file must stay a candidate, which `added` guarantees).
            _ => {
                if !self.added.contains_key(&rel) {
                    let seq = self.alloc_seq();
                    self.added.insert(rel, seq);
                }
            }
        }
    }

    /// Applies one scoped `Gone`: forgets the path and everything under it.
    /// No walk is needed: a deleted directory's files form one id range in
    /// the index (`dir_range`), and `added` entries are dropped by prefix.
    fn apply_gone(&mut self, rel: &str) {
        if let Some(id) = self.index.find(rel.as_bytes()) {
            if (id as usize) < self.ids.len() {
                let st = &mut self.ids[id as usize];
                if st.seen && !st.current {
                    self.dirty = self.dirty.saturating_sub(1);
                }
                st.seen = false;
                st.current = false;
            }
        }
        let mut pfx = String::with_capacity(rel.len() + 1);
        pfx.push_str(rel);
        pfx.push('/');
        let (lo, hi) = self.index.dir_range(pfx.as_bytes());
        for id in lo..hi.min(self.ids.len()) {
            let st = &mut self.ids[id];
            if st.seen && !st.current {
                self.dirty = self.dirty.saturating_sub(1);
            }
            st.seen = false;
            st.current = false;
        }
        self.added.retain(|r, _| r != rel && !r.starts_with(&pfx));
    }

    /// Applies a batch of scoped-examination results, resolved against the
    /// current index. `index` is the generation the examinations ran against:
    /// if a background rebuild swapped generations underneath (its rescan is
    /// already scheduled and subsumes this work), the batch is dropped.
    fn apply_scoped(&mut self, index: &Arc<Index>, actions: Vec<ScopedAction>, fallback: bool) {
        if fallback {
            self.flag_rescan(WalkCause::Scoped);
            return;
        }
        if !Arc::ptr_eq(index, &self.index) {
            if !self.needs_rescan {
                self.flag_rescan(WalkCause::Rebuild);
            }
            return;
        }
        for action in actions {
            match action {
                ScopedAction::Found { rel, clock } => self.apply_found(rel, &clock),
                ScopedAction::Gone { rel } => self.apply_gone(&rel),
            }
        }
    }

    /// Cause for a batch that must fall back to a full walk, or `None` when
    /// every event is scoped-handleable. Pure path syntax; no I/O, no locks.
    /// Anything visibility-related (ignore files, `.git` internals that
    /// change rule applicability, paths outside the root or the root itself)
    /// needs a full walk until the later incremental phases. Paths in `skip`
    /// (directory entry-noise, precomputed by the caller) are ignored.
    fn full_cause(
        event: &notify::Result<notify::Event>,
        root: &Path,
        skip: &HashSet<PathBuf>,
    ) -> Option<WalkCause> {
        let Ok(event) = event else { return Some(WalkCause::Overflow) };
        if event.need_rescan() {
            return Some(WalkCause::Overflow);
        }
        for path in &event.paths {
            if skip.contains(path) {
                continue;
            }
            if path == root {
                return Some(WalkCause::Scoped);
            }
            let Some(rel) = build::relative(path, root) else { return Some(WalkCause::Scoped) };
            let parts: Vec<&str> = rel.split('/').collect();
            if let Some(git) = parts.iter().position(|p| *p == ".git") {
                // Git internals are hidden from the walk; only the exclude
                // file changes which files rg searches.
                if parts[git + 1..] == ["info", "exclude"] {
                    return Some(WalkCause::Scoped);
                }
                continue;
            }
            if matches!(parts.last(), Some(&(".gitignore" | ".ignore" | ".rgignore"))) {
                return Some(WalkCause::Scoped);
            }
        }
        None
    }

    /// Candidates under `prefix` in rg's walk order, and the number of files
    /// under it. Changed files and files absent from the index are always
    /// candidates. The caller has reconciled; this only reads.
    fn candidates(&self, query: &plan::Query, prefix: &str) -> Answer {
        let allowed = self.index.eval(query);
        // The search prefix is a directory path; its files are one contiguous
        // id range in the index's sorted path table.
        let (lo, hi) = if prefix.is_empty() {
            (0, self.ids.len())
        } else {
            let mut pfx = String::with_capacity(prefix.len() + 1);
            pfx.push_str(prefix);
            pfx.push('/');
            self.index.dir_range(pfx.as_bytes())
        };
        let mut total = 0usize;
        let mut hits: Vec<(u32, Hit)> = Vec::new();
        for id in lo..hi {
            let st = self.ids[id];
            if !st.seen {
                continue;
            }
            total += 1;
            let keep = !st.current || allowed.as_ref().is_none_or(|set| set.contains(id));
            if keep {
                hits.push((st.seq, Hit::Id(id as u32)));
            }
        }
        for (rel, seq) in &self.added {
            if let Some(rest) = rest_under(rel, prefix) {
                total += 1;
                hits.push((*seq, Hit::Added(rest.to_owned())));
            }
        }
        // Candidates come back in rg's walk order.
        hits.sort_unstable_by_key(|h| h.0);
        Answer { total, index: self.index.clone(), hits: hits.into_iter().map(|(_, hit)| hit).collect() }
    }
}

/// Whether an event reports a `.git` *pointer file* at or under `root`: a
/// nested linked worktree or submodule appearing, vanishing or being
/// re-pointed. Its directory's `info/exclude` is read by the walk, and a
/// relative pointer target resolves against the process cwd, so a full walk is
/// needed (wrapped by the [`ignore_deps`] snapshot, which then tracks the new
/// target). A `.git` *directory* is an ordinary repository whose `info/exclude`
/// is already watched, so it is not a pointer here. A vanished path counts, so
/// removing a pointer is seen.
fn git_pointer_event(event: &notify::Result<notify::Event>, root: &Path) -> bool {
    let Ok(event) = event else { return false };
    event.paths.iter().any(|path| {
        path.file_name().is_some_and(|name| name == std::ffi::OsStr::new(".git"))
            && build::relative(path, root).is_some()
            && !std::fs::symlink_metadata(path).is_ok_and(|md| md.is_dir())
    })
}

/// Answers a query: waits for in-flight scoped work, reconciles first if the
/// file set may be stale, then reads under a shared lock.
/// Returns the answer, whether it rescanned, and the milliseconds spent
/// waiting for the state lock (read and, on a rescan, write).
pub(crate) fn answer(
    state: &RwLock<State>,
    pending: &PendingSet,
    health: &watch_budget::Health,
    query: &plan::Query,
    prefix: &str,
) -> anyhow::Result<Answered> {
    if health.stats().untrusted { return Ok(Answered::PendingTimeout); }
    // In-flight scoped work first: an answer must reflect every event
    // delivered before it started waiting. A timeout means the cache may be
    // stale, and a stale candidate set is worse than none: decline it so the
    // client falls back to the authoritative walk instead. Do not reconcile
    // here: that would race the still-unapplied work.
    if pending.wait_empty() == WaitOutcome::TimedOut {
        return Ok(Answered::PendingTimeout);
    }
    let waited = Instant::now();
    if health.stats().untrusted { return Ok(Answered::PendingTimeout); }
    let s = state.read().unwrap();
    let mut lock_ms = waited.elapsed().as_secs_f64() * 1000.0;
    // An ancestor, Git-repository or global ignore file may have changed
    // outside the watched root; the tokens are cheap to re-read and a
    // change re-walks with rg's current ignore rules.
    if !s.needs_rescan && !s.deps.changed() {
        if health.stats().untrusted || s.unprunable { return Ok(Answered::PendingTimeout); }
        return Ok(Answered::Candidates(s.candidates(query, prefix), false, lock_ms));
    }
    drop(s);
    let waited = Instant::now();
    let mut s = state.write().unwrap();
    lock_ms += waited.elapsed().as_secs_f64() * 1000.0;
    // Re-check under the write lock: another query may have reconciled already.
    if s.deps.changed() {
        s.flag_rescan(WalkCause::OutsideDep);
    }
    let rescanned = s.needs_rescan;
    if s.needs_rescan {
        s.reconcile()?;
    }
    if health.stats().untrusted { return Ok(Answered::PendingTimeout); }
    if s.unprunable { return Ok(Answered::PendingTimeout); }
    Ok(Answered::Candidates(s.candidates(query, prefix), rescanned, lock_ms))
}

/// Candidates materialized for an embedded query. Paths are relative to the
/// requested prefix, `/`-separated, in rg's walk order.
pub(crate) struct TextAnswer {
    pub(crate) files: usize,
    pub(crate) paths: Vec<String>,
}

/// [`answer`], with paths read from the index after the lock is released.
/// `None` means the cache cannot be trusted; the caller must walk.
pub(crate) fn answer_text(
    state: &RwLock<State>,
    pending: &PendingSet,
    health: &watch_budget::Health,
    query: &plan::Query,
    prefix: &str,
) -> anyhow::Result<Option<TextAnswer>> {
    let ans = match answer(state, pending, health, query, prefix)? {
        Answered::Candidates(ans, _, _) => ans,
        Answered::PendingTimeout => return Ok(None),
    };
    let mut paths = Vec::with_capacity(ans.hits.len());
    for hit in &ans.hits {
        match hit {
            Hit::Id(id) => {
                let rel = std::str::from_utf8(ans.index.path(*id as usize)).unwrap_or("");
                paths.push(rest_under(rel, prefix).unwrap_or(rel).to_owned());
            }
            Hit::Added(rest) => paths.push(rest.clone()),
        }
    }
    Ok(Some(TextAnswer { files: ans.total, paths }))
}

/// `rel` with the directory `prefix` and its single `/` removed, or `None` when
/// `rel` is not under `prefix`. Prefix `""` matches every file.
fn rest_under<'a>(rel: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(rel);
    }
    rel.strip_prefix(prefix)?.strip_prefix('/')
}

/// Whether `dir` is yielded when rg's walker lists `dir`'s parent with this
/// walk's settings: i.e. no ignore rule filters it out. `dir` must be an
/// absolute path at or under `root`. The walk root itself is always yielded,
/// so the check is done one level up with a depth-1 listing of the parent
/// (see `docs/WGREP_INCREMENTAL_FRESHNESS.md`).
fn dir_listed(args: &HiArgs, root: &Path, dir: &Path) -> bool {
    if dir == root {
        return true;
    }
    let Some(parent) = dir.parent() else { return false };
    let Some(mut builder) = args.walk_builder_at(root, parent) else { return false };
    // Only the parent's immediate children matter; the explicit root is
    // always yielded, so finding `dir` among them means rg keeps it.
    builder.max_depth(Some(1));
    builder.build().filter_map(|r| r.ok()).any(|entry| entry.path() == dir)
}

/// Whether every directory between `root` and `dir` (inclusive) is visible
/// to rg's walker. `dir` must be an absolute path at or under `root`.
/// `known_visible` answers whether a directory already holds a visible known
/// file; such a directory's ancestors up to the root are visible too (the
/// full walk yielded that file through them), so it ends the climb early.
fn ancestors_visible(
    args: &HiArgs,
    root: &Path,
    dir: &Path,
    known_visible: &dyn Fn(&Path) -> bool,
) -> bool {
    if dir.strip_prefix(root).is_err() {
        return false;
    }
    let mut current = Some(dir);
    while let Some(d) = current {
        if known_visible(d) {
            return true;
        }
        if !dir_listed(args, root, d) {
            return false;
        }
        if d == root {
            return true;
        }
        current = d.parent();
    }
    false
}

/// Re-examines one watched path with rg's own walker, outside the state
/// lock. `stat` was read before locking (like the watcher's pre-stats);
/// `known_visible` answers whether a directory already holds a visible known
/// file (read under a brief shared lock per level by the caller).
///
/// The dispatch is stat-driven, so superseded or coalesced events converge:
/// a path that exists is verified as a create, a missing path is forgotten,
/// and anything the full walk could never yield (symlinks, sockets, ignored
/// paths that resolve against current rules) is either forgotten or falls
/// back to a full walk. When in doubt the answer is `Fallback`, which is
/// today's behavior.
fn examine(
    args: &HiArgs,
    root: &Path,
    path: &Path,
    stat: &Stat,
    known_visible: &dyn Fn(&Path) -> bool,
) -> ExamineOutcome {
    use ExamineOutcome::*;
    let Some(rel) = build::relative(path, root) else { return Fallback };
    // Classify without following symlinks: the full walk (link-following
    // disabled) never yields them, so they can never be in the file set.
    match std::fs::symlink_metadata(path).map(|m| m.file_type()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Actions(vec![ScopedAction::Gone { rel }]),
        Err(_) => return Fallback,
        Ok(ft) if ft.is_dir() => examine_dir(args, root, path, &rel, known_visible),
        Ok(ft) if ft.is_file() => examine_file(args, root, path, &rel, stat, known_visible),
        // Symlinks, sockets and friends are never yielded by the full walk.
        Ok(_) => Actions(vec![ScopedAction::Gone { rel }]),
    }
}

/// Examines one file: its parent must be visible, and the file itself must
/// survive the walker's filters (a depth-1 listing of the parent, which
/// applies file-level patterns like `*.tmp`).
fn examine_file(
    args: &HiArgs,
    root: &Path,
    path: &Path,
    rel: &str,
    stat: &Stat,
    known_visible: &dyn Fn(&Path) -> bool,
) -> ExamineOutcome {
    use ExamineOutcome::*;
    let Some(parent) = path.parent() else { return Fallback };
    // No rescan is pending and no ignore rule changed in this batch (those
    // take the full-walk path), so invisibility verdicts below are current:
    // an invisible path holds no searchable files and is forgotten.
    if !ancestors_visible(args, root, parent, known_visible) {
        return Actions(vec![ScopedAction::Gone { rel: rel.to_owned() }]);
    }
    let Some(mut builder) = args.walk_builder_at(root, parent) else { return Fallback };
    builder.max_depth(Some(1));
    // Regular files only: this is the same filter the full walk applies
    // (symlinks and directories never become candidates).
    let listed = builder.build().filter_map(|r| r.ok()).any(|entry| {
        entry.path() == path && entry.file_type().is_some_and(|t| t.is_file())
    });
    if !listed {
        // Filtered by current rules (or vanished mid-walk): either way it
        // holds no searchable file.
        return Actions(vec![ScopedAction::Gone { rel: rel.to_owned() }]);
    }
    match &stat.md {
        Ok(md) => Actions(vec![ScopedAction::Found { rel: rel.to_owned(), clock: FileClock::of(md, stat.token) }]),
        // Vanished between classification and the pre-lock stat: the delete
        // will arrive as its own event, but re-walking is the safe answer.
        Err(_) => Fallback,
    }
}

/// Examines one directory: the directory itself must be visible (the walk
/// root is always yielded, so an ignored `dir` would otherwise leak its
/// whole subtree), then a subtree walk collects its files with rg's rules.
fn examine_dir(
    args: &HiArgs,
    root: &Path,
    dir: &Path,
    rel: &str,
    known_visible: &dyn Fn(&Path) -> bool,
) -> ExamineOutcome {
    use ExamineOutcome::*;
    // The walk root itself is always yielded, so confirm the directory
    // itself is visible first: an ignored `dir` hides its whole subtree
    // from the full walk, and is forgotten (rules are current, see above).
    if !ancestors_visible(args, root, dir, known_visible) {
        return Actions(vec![ScopedAction::Gone { rel: rel.to_owned() }]);
    }
    let Some(builder) = args.walk_builder_at(root, dir) else { return Fallback };
    let mut actions = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            // A walk error (permission, vanishing tree) is what a full walk
            // tolerates by skipping; here it means fall back to one.
            Err(_) => return Fallback,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(rel) = build::relative(entry.path(), root) else { return Fallback };
        let stat = Stat::of(entry.path());
        match &stat.md {
            Ok(md) => actions.push(ScopedAction::Found { rel, clock: FileClock::of(md, stat.token) }),
            // Vanished mid-walk: forget it. A concurrent re-create arrives
            // as its own event and re-adds it, so this converges.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                actions.push(ScopedAction::Gone { rel });
            }
            Err(_) => return Fallback,
        }
    }
    Actions(actions)
}

/// Outcome of waiting for in-flight watcher work to drain.
///
/// Explicit so the caller cannot ignore a timeout by accident: a timed-out
/// cache may be stale, and serving it would return candidates that miss the
/// just-delivered change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    /// No event is in flight: the cached file set reflects every delivery.
    Drained,
    /// Work was still in flight at the deadline: the cache may be stale.
    TimedOut,
}

/// Total time a query waits for in-flight watcher work before declining to
/// answer. The deadline is fixed for the whole wait: a wakeup that leaves
/// work pending does not start a fresh timeout.
const PENDING_WAIT_BOUND: Duration = Duration::from_secs(30);

/// Paths with delivered-but-unapplied watcher events. Queries wait while it
/// is non-empty, so an answer reflects every event delivered before it
/// started waiting (see `docs/WGREP_INCREMENTAL_FRESHNESS.md`, Concurrency).
/// The set lives beside the state lock (a `Condvar` needs a `Mutex`): the
/// watcher inserts on delivery and removes after applying, and queries only
/// ever wait on it, so neither side holds it while taking the state lock.
pub(crate) struct PendingSet {
    mutex: Mutex<HashSet<PathBuf>>,
    cv: Arc<Condvar>,
    gate: Arc<Mutex<()>>,
    /// Whole-wait bound, from the first `wait_empty` call. Kept as a field so
    /// tests can use a short bound instead of a real 30 s delay.
    bound: Duration,
    health: Option<watch_budget::Health>,
}

impl PendingSet {
    pub(crate) fn with_health(health: &watch_budget::Health) -> PendingSet {
        PendingSet { mutex: Mutex::new(HashSet::new()), cv: health.condvar(), gate: health.gate(), bound: PENDING_WAIT_BOUND, health: Some(health.clone()) }
    }

    /// Test-only constructor with a short bound. Not a public timing knob: it
    /// is compiled only for the crate's own tests.
    #[cfg(test)]
    fn new_with_bound(bound: Duration) -> PendingSet {
        PendingSet { mutex: Mutex::new(HashSet::new()), cv: Arc::new(Condvar::new()), gate: Arc::new(Mutex::new(())), bound, health: None }
    }

    fn insert(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let _gate = self.gate.lock().unwrap();
        self.mutex.lock().unwrap().extend(paths);
    }

    fn remove(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let _gate = self.gate.lock().unwrap();
        let mut set = self.mutex.lock().unwrap();
        for path in paths {
            set.remove(&path);
        }
        if set.is_empty() {
            self.cv.notify_all();
        }
    }

    /// Waits until no event is in flight, bounded by one fixed whole-wait
    /// deadline (`self.bound` from the first call). A wedged watcher thread
    /// must not wedge queries forever, so a timeout reports `TimedOut`; the
    /// caller must then decline the cached answer and let the client walk
    /// authoritatively, rather than serve possibly-stale candidates.
    fn wait_empty(&self) -> WaitOutcome {
        let deadline = Instant::now() + self.bound;
        let mut gate = self.gate.lock().unwrap();
        loop {
            let empty = self.mutex.lock().unwrap().is_empty();
            if empty && !self.health.as_ref().is_some_and(|h| h.stats().events != 0) { break; }
            let now = Instant::now();
            if now >= deadline {
                return WaitOutcome::TimedOut;
            }
            // Wait only the time left on the whole-wait deadline, so a wakeup
            // that leaves work pending cannot extend the total wait.
            let (guard, _) = self.cv.wait_timeout(gate, deadline - now).unwrap();
            gate = guard;
        }
        if self.health.as_ref().is_some_and(|h| h.stats().untrusted) { WaitOutcome::TimedOut } else { WaitOutcome::Drained }
    }

    fn len(&self) -> usize {
        self.mutex.lock().unwrap().len()
    }
}

/// Result of a daemon query.
pub(crate) enum Answered {
    /// Authoritative candidates (whether the index was re-walked or not),
    /// whether a rescan ran, and the milliseconds spent waiting for the state
    /// lock.
    Candidates(Answer, bool, f64),
    /// In-flight watcher work did not drain within the bound: the cached
    /// candidate set may be stale, so it is declined. The client must walk.
    PendingTimeout,
}

/// Reused worker threads. A short query does not pay for a thread spawn.
const WORKERS: usize = 4;

/// Hard cap on concurrent connections/handlers: the reused workers plus any
/// threads spawned for connections that arrive while every worker is busy. A
/// broad search's lease can hold its handler for `LEASE_HOLD`, so without a
/// cap a burst of such connections would spawn unbounded threads. Past the cap
/// the daemon declines the connection and the client walks authoritatively.
const MAX_HANDLERS: usize = 64;

/// Largest request line accepted, in bytes. A longer line is refused, never
/// truncated: a cut query could select a different file set.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Whole-read deadline for one request line. A per-read timeout alone lets a
/// client that trickles bytes hold a handler without bound.
const REQUEST_READ_BOUND: Duration = Duration::from_secs(5);

/// Whole-reply deadline, including the header, candidate paths, and flush.
/// Keep this generous: a broad reply can carry many candidate paths.
const REPLY_WRITE_BOUND: Duration = Duration::from_secs(300);

/// How long a broad search's lease outlives its reply (an open connection
/// keeps the thread share).
const LEASE_HOLD: Duration = Duration::from_secs(300);

/// A finite budget of concurrent handlers. A [`HandlerGuard`] releases its
/// slot on drop, so an error, panic, disconnect or failed spawn returns it.
struct Capacity {
    max: usize,
    active: AtomicUsize,
    rejected: AtomicUsize,
}

/// One claimed handler slot. Dropping it releases the slot.
struct HandlerGuard {
    cap: Arc<Capacity>,
}

impl Capacity {
    fn new(max: usize) -> Arc<Capacity> {
        Arc::new(Capacity { max, active: AtomicUsize::new(0), rejected: AtomicUsize::new(0) })
    }

    /// Claims a slot, or `None` when the cap is reached (counting the
    /// rejection).
    fn try_acquire(self: &Arc<Self>) -> Option<HandlerGuard> {
        let mut active = self.active.load(Ordering::SeqCst);
        loop {
            if active >= self.max {
                self.rejected.fetch_add(1, Ordering::SeqCst);
                return None;
            }
            match self.active.compare_exchange_weak(active, active + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(HandlerGuard { cap: self.clone() }),
                Err(current) => active = current,
            }
        }
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    fn rejected(&self) -> usize {
        self.rejected.load(Ordering::SeqCst)
    }
}

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        self.cap.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One accepted connection queued to a reused worker, with the handler slot it
/// already holds.
struct Job {
    stream: TcpStream,
    guard: HandlerGuard,
}

/// Serves one connection on its own thread while holding its capacity slot.
/// If the spawn fails, the closure (and its guard) is dropped, releasing the
/// slot; the client then sees a closed connection and walks authoritatively.
fn spawn_handler(
    stream: TcpStream,
    guard: HandlerGuard,
    state: Arc<RwLock<State>>,
    pending: Arc<PendingSet>,
    token: String,
    capacity: Arc<Capacity>,
) {
    let _ = std::thread::Builder::new().name("wgrep-conn".to_string()).spawn(move || {
        let _guard = guard;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle(stream, &state, &pending, &token, &capacity)
        }));
    });
}

/// Refuses an over-capacity connection with one explicit answer, then closes.
/// The client treats the failure as "no daemon answer" and walks
/// authoritatively; it is never an empty success. The reply is tiny, so this
/// does not block the acceptor on a peer that is not reading.
fn decline_overloaded(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = writeln!(stream, "{}", serde_json::json!({ "ok": false, "reason": "overloaded" }));
    let _ = stream.flush();
}

/// Outcome of reading one request line.
enum RequestRead {
    /// A complete newline-terminated line (newline removed).
    Line(String),
    /// No newline within the byte cap: refuse the whole request.
    TooLarge,
    /// The peer closed, stalled past the whole-read deadline, or sent a
    /// non-UTF-8 line.
    Ended,
}

/// Reads one newline-terminated request line, bounded in bytes (`max_bytes`)
/// and time (`bound`, a whole-read deadline). The socket read timeout is
/// re-armed with the time left before each read, so a client that trickles
/// bytes cannot extend the read past `bound`.
fn read_request(stream: &TcpStream, max_bytes: usize, bound: Duration) -> std::io::Result<RequestRead> {
    let deadline = Instant::now() + bound;
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    let mut src = stream;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(RequestRead::Ended);
        }
        stream.set_read_timeout(Some(deadline - now))?;
        let n = match src.read(&mut chunk) {
            Ok(0) => return Ok(RequestRead::Ended),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Ok(RequestRead::Ended);
            }
            Err(e) => return Err(e),
        };
        if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..pos]);
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > max_bytes {
            return Ok(RequestRead::TooLarge);
        }
    }
    if buf.len() > max_bytes {
        return Ok(RequestRead::TooLarge);
    }
    Ok(match String::from_utf8(buf) {
        Ok(line) => RequestRead::Line(line),
        Err(_) => RequestRead::Ended,
    })
}

/// Writer that applies one absolute deadline to a complete reply. `write_all`
/// may call `write` many times; each call receives only the time left, so
/// partial progress cannot restart the budget.
struct ReplyWriter {
    stream: TcpStream,
    deadline: Option<Instant>,
}

fn reply_remaining(deadline: Instant, now: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "reply deadline elapsed"))
}

impl ReplyWriter {
    fn new(stream: TcpStream) -> ReplyWriter {
        ReplyWriter { stream, deadline: None }
    }

    fn start(&mut self) {
        self.deadline.get_or_insert_with(|| Instant::now() + REPLY_WRITE_BOUND);
    }

    fn remaining(&self) -> std::io::Result<Duration> {
        reply_remaining(self.deadline.unwrap(), Instant::now())
    }

    fn get_ref(&self) -> &TcpStream {
        &self.stream
    }
}

impl Write for ReplyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.start();
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.start();
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.flush()
    }
}

pub(crate) fn serve(path: &OsString) -> anyhow::Result<()> {
    // Bound the daemon's own (re)builds so they leave CPU and memory for
    // queries; an explicit user setting wins. SAFETY: this runs before the
    // process spawns any helper thread.
    unsafe {
        if std::env::var_os("WGREP_BUILD_THREADS").is_none() {
            let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
            std::env::set_var("WGREP_BUILD_THREADS", (cpus / 2).max(1).to_string());
        }
        if std::env::var_os("WGREP_BUILD_MEMORY_MB").is_none() {
            std::env::set_var("WGREP_BUILD_MEMORY_MB", "128");
        }
    }
    let args = parse_walk_args(path)?;
    let (search_paths, cwd) = args.index_paths();
    let root_arg = search_paths.first().cloned().unwrap_or_else(|| PathBuf::from(path));
    let root = store::absolute(&root_arg, cwd);
    anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
    let slot = store::slot_for(&root).ok_or_else(|| anyhow::anyhow!("no index directory; set WGREP_INDEX_DIR"))?;
    // One daemon per root, enforced by an advisory lock held for the process
    // lifetime; the ping check below is a nicety but races two auto-starts.
    let _daemon_lock = match store::daemon_lock(&root) {
        Ok(lock) => lock,
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            anyhow::bail!("another daemon is already serving {}", root.display());
        }
        Err(err) => return Err(err.into()),
    };
    if let Some(pid) = super::client::control(&root, "ping") {
        anyhow::bail!("a daemon (pid {pid}) is already serving {}", root.display());
    }

    // Watch before the first walk so no change falls between them.
    let (ingress, rx, health) = watch_budget::bounded();
    let callback = ingress;
    let mut watcher = notify::recommended_watcher(move |event| callback.try_ingest(event))?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let usable = store::current(&root).is_some_and(|f| Index::open(&f).is_ok());
    if !usable {
        eprintln!("wgrep: no current index for {}, building one", root.display());
        rebuild(&args, &root, &root_arg)?;
    }
    let index_file = store::current(&root).ok_or_else(|| anyhow::anyhow!("index missing after build"))?;
    let index = Arc::new(Index::open(&index_file)?);
    let rebuild_args = parse_walk_args(path)?;
    let reconciling = Arc::new(AtomicBool::new(false));
    let missed = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(PendingSet::with_health(&health));
    let state = State {
        root_arg: root_arg.clone(),
        root: root.clone(),
        args,
        index,
        deps: ignore_deps::Deps::snapshot(&root),
        ids: Vec::new(),
        added: HashMap::new(),
        needs_rescan: true,
        rescan_cause: WalkCause::Startup,
        walks: WalkCounts::default(),
        next_seq: 0,
        dirty: 0,
        reconciling: reconciling.clone(),
        missed: missed.clone(),
        unprunable: false,
    };
    // The file set is reconciled lazily on the first query (`needs_rescan`),
    // so startup is fast and a slow first walk does not delay the listener.
    eprintln!("wgrep: serving {}", root.display());
    let state = Arc::new(RwLock::new(state));

    {
        let state = state.clone();
        let pending = pending.clone();
        let root = root.clone();
        let health = health.clone();
        std::thread::spawn(move || event_loop(state, pending, health, root, rx));
    }
    {
        let (state, root, root_arg) = (state.clone(), root.clone(), root_arg.clone());
        std::thread::spawn(move || rebuild_loop(state, rebuild_args, root, root_arg));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let token = random_token();
    let port = listener.local_addr()?.port();
    // `root` lets clients tell this daemon's slot from a hash collision, so
    // they can find the daemon without reading the slot's `root` and `CURRENT`.
    let daemon_cwd = std::env::current_dir()?;
    let info = serde_json::json!({ "port": port, "token": token, "pid": std::process::id(), "root": store::root_key(&root), "cwd": daemon_cwd });
    let tmp = slot.join(format!("daemon.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, info.to_string())?;
    std::fs::rename(&tmp, slot.join("daemon.json"))?;
    let _ = DAEMON_FILE.set(slot.join("daemon.json"));
    eprintln!("wgrep: listening on 127.0.0.1:{port}");

    // Connections are served by a few reused workers, so a short query does
    // not pay for a thread spawn. A broad search's lease can hold its worker
    // for up to `LEASE_HOLD`, so a connection that finds no idle worker gets a
    // thread of its own — but only up to `MAX_HANDLERS` total. Past that cap
    // the connection is declined so the client walks authoritatively instead of
    // piling up an unbounded number of threads.
    let capacity = Capacity::new(MAX_HANDLERS);
    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));
    let idle = Arc::new(AtomicUsize::new(WORKERS));
    for _ in 0..WORKERS {
        let (jobs_rx, idle, state, pending, token) =
            (jobs_rx.clone(), idle.clone(), state.clone(), pending.clone(), token.clone());
        let capacity = capacity.clone();
        std::thread::spawn(move || {
            loop {
                let job = {
                    let rx = jobs_rx.lock().unwrap();
                    rx.recv()
                };
                let Ok(job) = job else { break };
                let Job { stream, guard } = job;
                // A panic must not kill the worker and leak its idle slot, and
                // `guard` drops on unwind so the capacity slot is returned too.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle(stream, &state, &pending, &token, &capacity)
                }));
                drop(guard);
                idle.fetch_add(1, Ordering::SeqCst);
            }
        });
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // A finite total budget of in-flight handlers: the reused workers plus
        // one thread per over-worker connection. The guard returns its slot on
        // every exit path (drop): normal end, error, panic, disconnect, or a
        // failed thread spawn. Past the cap, decline so the client walks.
        let Some(guard) = capacity.try_acquire() else {
            decline_overloaded(stream);
            continue;
        };
        // The acceptor claims the idle worker itself, so two quick connections
        // cannot both queue behind one worker.
        if idle.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
            match jobs_tx.send(Job { stream, guard }) {
                Ok(()) => continue,
                // Every worker has exited: serve it directly instead.
                Err(mpsc::SendError(job)) => {
                    let Job { stream, guard } = job;
                    spawn_handler(stream, guard, state.clone(), pending.clone(), token.clone(), capacity.clone());
                    continue;
                }
            }
        }
        spawn_handler(stream, guard, state.clone(), pending.clone(), token.clone(), capacity.clone());
    }
    drop(watcher);
    Ok(())
}

pub(crate) fn event_loop(
    state: Arc<RwLock<State>>,
    pending: Arc<PendingSet>,
    health: watch_budget::Health,
    root: PathBuf,
    rx: watch_budget::Receiver,
) {
    let (reconciling, missed) = {
        let s = state.read().unwrap();
        (s.reconciling.clone(), s.missed.clone())
    };
    // Batches that arrive while a reconcile holds the state lock are
    // re-queued here when they are scoped-class, for examination
    // after the walk (which reads strictly newer filesystem state,
    // so it covers whatever the walk missed). Full-class batches
    // still escalate via `missed`, which the reconciler turns into
    // one follow-up walk covering them. Re-queuing instead of
    // escalating scoped work is what keeps ordinary changes from
    // causing full walks under laggy event delivery.
    let mut deferred: Vec<watch_budget::Work> = Vec::new();
    loop {
        // Drain re-queued batches before blocking for new events, but
        // never block with work outstanding: queries wait on
        // `pending`, so wedging here would wedge them. While a walk
        // runs, wait briefly for new events instead of spinning, then
        // re-check (the walk may have ended).
        let mut batch: Vec<watch_budget::Work>;
        let fresh: bool;
        if !deferred.is_empty() && !reconciling.load(Ordering::SeqCst) {
            batch = std::mem::take(&mut deferred);
            fresh = false;
        } else {
            let first = if deferred.is_empty() {
                match rx.recv() {
                    Ok(event) => event,
                    Err(_) => { health.mark_untrusted(); break },
                }
            } else {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => event,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => { health.mark_untrusted(); break },
                }
            };
            // Drain everything pending into one batch so a burst takes the
            // state lock once instead of once per event.
            batch = vec![first];
            while let Ok(e) = rx.try_recv() {
                batch.push(e);
            }
            fresh = true;
        }
        if std::env::var_os("WGREP_INDEX_DEBUG").is_some() {
            for event in batch.iter().filter_map(|w| w.event().as_ref().ok()) {
                eprintln!("wgrep: event {:?} {:?}", event.kind, event.paths);
            }
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        for event in batch.iter().filter_map(|w| w.event().as_ref().ok()) {
            paths.extend(event.paths.iter().cloned());
        }
        // Record fresh batches before doing any I/O, so a query never
        // answers between delivery and application. Re-queued batches
        // never left `pending`.
        if fresh {
            pending.insert(paths.iter().cloned());
        }
        // A reconcile is walking: scoped-class batches re-queue for
        // examination afterwards, full-class batches escalate via
        // `missed` as before. The pre-stat classification is
        // conservative (no skip set yet); the precise one runs below
        // if the walk already ended. Re-check after storing: the
        // reconciler may have finished between our load and store, in
        // which case this batch is processed inline.
        if reconciling.load(Ordering::SeqCst) {
            let mut full = false;
            for work in &batch { let event = work.event();
                if State::full_cause(event, &root, &HashSet::new()).is_some()
                    || git_pointer_event(event, &root)
                {
                    full = true;
                    break;
                }
            }
            if full {
                missed.store(true, Ordering::SeqCst);
            }
            if reconciling.load(Ordering::SeqCst) {
                if full {
                    pending.remove(paths);
                } else {
                    deferred.extend(batch);
                }
                continue;
            }
        }
        // Stat before walking: on Windows the change token may open
        // the file, and that I/O must not block queries.
        let mut stats: HashMap<PathBuf, Stat> = HashMap::new();
        for event in batch.iter().filter_map(|w| w.event().as_ref().ok()) {
            for path in &event.paths {
                if !stats.contains_key(path) {
                    let mut stat = Stat::of(path);
                    // Fresh files can be briefly locked by antivirus
                    // scanners; short backoff beats a full walk for a
                    // transient lock (observed well past 25 ms under
                    // Defender). A missing file stays missing.
                    for nap in [25, 50, 100] {
                        if matches!(&stat.md, Err(e) if e.kind() != std::io::ErrorKind::NotFound) {
                            std::thread::sleep(Duration::from_millis(nap));
                            stat = Stat::of(path);
                        } else {
                            break;
                        }
                    }
                    stats.insert(path.clone(), stat);
                }
            }
        }
        // A modify on a directory means its entries changed; the
        // entries' own events carry the changes, so the directory
        // itself needs no state change. This matters on Windows,
        // where every entry add/remove also modifies the parent (up
        // to the root, which would otherwise read as a full-walk
        // trigger) and where notify lumps all modifications into
        // Modify(Any). Skipping is sound: without watcher overflow (a
        // full-walk trigger on its own) no event is ever dropped, so
        // the entries are always examined separately. Renames
        // (Modify(Name)) and unclassified modifications
        // (Modify(Other)) are never skipped. Directory
        // attribute-only changes are astronomically rare and benign
        // here: at worst a file stays a candidate until the next
        // event or walk touches it.
        //
        // A modify on a file whose stat still matches its index
        // record is likewise noise (typically the reconcile's own
        // reads on Windows, where last-access updates surface as
        // Modify(Any)): dropping it is exactly what today's
        // dirty-marking does when the same comparison succeeds, so
        // the candidate computation is unchanged. Only already-seen,
        // currently-current files qualify; anything else (new,
        // dirty, forgotten, or unknown) is examined normally, which
        // preserves the old resurrect-on-modify semantics exactly.
        // The racy same-tick hole this shares with today's logic
        // (a rewrite within the change-token tick still matching)
        // is Phase 3's subject, not Phase 1's.
        let is_noise = |s: &State, event: &notify::Event, path: &Path| -> bool {
            let stat = match stats.get(path) {
                Some(stat) => stat,
                None => return false,
            };
            let md = match stat.md.as_ref() {
                Ok(md) => md,
                Err(_) => return false,
            };
            match &event.kind {
                EventKind::Modify(
                    notify::event::ModifyKind::Data(_)
                    | notify::event::ModifyKind::Metadata(_)
                    | notify::event::ModifyKind::Any,
                ) if md.is_dir() => true,
                EventKind::Modify(
                    notify::event::ModifyKind::Data(_)
                    | notify::event::ModifyKind::Metadata(_)
                    | notify::event::ModifyKind::Any,
                ) => {
                    let Some(rel) = build::relative(path, &s.root) else { return false };
                    let Some(id) = s.index.find(rel.as_bytes()) else { return false };
                    (id as usize) < s.ids.len() && {
                        let st = s.ids[id as usize];
                        st.seen
                            && st.current
                            && State::record_matches_clock(
                                &s.index,
                                id as usize,
                                &FileClock::of(md, stat.token),
                            )
                    }
                }
                _ => false,
            }
        };
        // Directory entry-noise and read-noise (see above) never wait
        // in `pending`: neither needs a state change, so queries
        // must not wait on them.
        let s = state.read().unwrap();
        let mut skipped: HashSet<PathBuf> = HashSet::new();
        for event in batch.iter().filter_map(|w| w.event().as_ref().ok()) {
            for path in &event.paths {
                if is_noise(&s, event, path) {
                    skipped.insert(path.clone());
                }
            }
        }
        drop(s);
        pending.remove(skipped.iter().cloned());
        paths.retain(|path| !skipped.contains(path));
        // Visibility-related events still need a full walk (later
        // incremental phases); flagging now subsumes any scoped work
        // for the rest of the batch.
        let mut full: Option<WalkCause> = None;
        for work in &batch { let event = work.event();
            if let Some(cause) = State::full_cause(event, &root, &skipped) {
                full = Some(cause);
                break;
            }
            if git_pointer_event(event, &root) {
                full = Some(WalkCause::Scoped);
                break;
            }
        }
        if let Some(cause) = full {
            state.write().unwrap().flag_rescan(cause);
            pending.remove(paths);
            continue;
        }
        // A rescan is already scheduled: scoped results would be
        // discarded by it, so skip the walks.
        if state.read().unwrap().needs_rescan {
            pending.remove(paths);
            continue;
        }
        // Scoped walks (I/O, no state lock held across them beyond
        // one shared read guard, which blocks writers but not
        // queries; the known-file closure borrows that guard instead
        // of re-locking).
        let index = state.read().unwrap().index.clone();
        let mut actions = Vec::new();
        let mut fallback_paths: Vec<PathBuf> = Vec::new();
        for event in batch.iter().filter_map(|w| w.event().as_ref().ok()) {
            for path in &event.paths {
                if skipped.contains(path) {
                    continue;
                }
                let s = state.read().unwrap();
                let outcome = match stats.get(path) {
                    Some(stat) => examine(&s.args, &s.root, path, stat, &|dir| {
                        s.dir_has_known_file(dir)
                    }),
                    None => ExamineOutcome::Fallback,
                };
                drop(s);
                match outcome {
                    ExamineOutcome::Actions(mut found) => actions.append(&mut found),
                    ExamineOutcome::Fallback => fallback_paths.push(path.clone()),
                }
            }
        }
        // Re-check before taking the write lock: a reconcile may
        // have started during the walks. Same rule as above:
        // full-class escalates, scoped-class re-queues.
        if reconciling.load(Ordering::SeqCst) {
            let mut full = false;
            for work in &batch { let event = work.event();
                if State::full_cause(event, &root, &skipped).is_some() {
                    full = true;
                    break;
                }
            }
            if full {
                missed.store(true, Ordering::SeqCst);
            }
            if reconciling.load(Ordering::SeqCst) {
                if full {
                    pending.remove(paths);
                } else {
                    deferred.extend(batch);
                }
                continue;
            }
        }
        state.write().unwrap().apply_scoped(&index, actions, !fallback_paths.is_empty());
        if !fallback_paths.is_empty() && std::env::var_os("WGREP_INDEX_DEBUG").is_some() {
            for path in &fallback_paths {
                eprintln!("wgrep: scoped fallback for {}", path.display());
            }
        }
        pending.remove(paths);
    }
}

fn parse_walk_args(path: &OsString) -> anyhow::Result<HiArgs> {
    match crate::flags::parse_args(vec![OsString::from("--files"), path.clone()]) {
        ParseResult::Ok(args) => Ok(args),
        ParseResult::Err(err) => Err(err),
        ParseResult::Special(_) => anyhow::bail!("unexpected special mode"),
    }
}

fn rebuild(args: &HiArgs, root: &Path, root_arg: &Path) -> anyhow::Result<()> {
    let files = super::cli::walk(args)?;
    build::build(root, root_arg, files, super::cli::dense_percent())?;
    Ok(())
}

pub(crate) fn rebuild_loop(state: Arc<RwLock<State>>, args: HiArgs, root: PathBuf, root_arg: PathBuf) {
    let mut last = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let dirty = state.read().unwrap().dirty;
        if dirty < REBUILD_MIN_DIRTY || last.elapsed() < REBUILD_INTERVAL {
            continue;
        }
        last = Instant::now();
        if let Err(err) = rebuild(&args, &root, &root_arg) {
            eprintln!("wgrep: rebuild failed: {err:#}");
            continue;
        }
        let Some(file) = store::current(&root) else { continue };
        match Index::open(&file) {
            Ok(index) => {
                let mut s = state.write().unwrap();
                s.index = Arc::new(index);
                // Files that changed while the rebuild read them no longer
                // match its metadata, so they stay dirty.
                s.flag_rescan(WalkCause::Rebuild);
                eprintln!("wgrep: swapped to {}", file.display());
            }
            Err(err) => eprintln!("wgrep: cannot open rebuilt index: {err}"),
        }
        trim_working_set();
    }
}

/// Ask the OS to return pages a large reconcile or rebuild accumulated. On
/// Windows this trims the whole working set; the index mmap re-faults from the
/// system cache on the next query, which is cheap.
#[cfg(windows)]
fn trim_working_set() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetProcessWorkingSetSize(process: *mut std::ffi::c_void, min: usize, max: usize) -> i32;
    }
    // (HANDLE)-1 is the current process; (-1, -1) trims as much as possible.
    unsafe {
        SetProcessWorkingSetSize(usize::MAX as *mut std::ffi::c_void, usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
fn trim_working_set() {}

/// This daemon's `daemon.json`, removed on a shutdown request.
static DAEMON_FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn handle(
    stream: TcpStream,
    state: &RwLock<State>,
    pending: &PendingSet,
    token: &str,
    capacity: &Capacity,
) -> anyhow::Result<()> {
    let handle_started = Instant::now();
    let _ = stream.set_nodelay(true);
    let reply = |out: &mut ReplyWriter, v: serde_json::Value| -> std::io::Result<()> {
        writeln!(out, "{v}")
    };
    let mut out = ReplyWriter::new(stream);
    // The request line is bounded in bytes and in time: an unbounded or
    // trickled header must not pin this handler. An oversized request is
    // refused whole (never truncated) so a cut query cannot select a
    // different file set.
    let request = match read_request(out.get_ref(), MAX_REQUEST_BYTES, REQUEST_READ_BOUND)? {
        RequestRead::Line(line) => match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(request) => request,
            Err(_) => {
                reply(&mut out, serde_json::json!({ "ok": false, "reason": "bad request" }))?;
                return Ok(());
            }
        },
        RequestRead::TooLarge => {
            reply(&mut out, serde_json::json!({ "ok": false, "reason": "request_too_large" }))?;
            return Ok(());
        }
        RequestRead::Ended => return Ok(()),
    };
    let read_ms = handle_started.elapsed().as_secs_f64() * 1000.0;
    if request.get("token").and_then(|v| v.as_str()) != Some(token) {
        reply(&mut out, serde_json::json!({ "ok": false, "reason": "bad token" }))?;
        return Ok(());
    }
    if request.get("ping").is_some() {
        reply(&mut out, serde_json::json!({ "ok": true }))?;
        out.flush()?;
        return Ok(());
    }
    if request.get("shutdown").is_some() {
        if let Some(file) = DAEMON_FILE.get() {
            let _ = std::fs::remove_file(file);
        }
        // Cleanup and shutdown must complete even if the peer cannot receive
        // its acknowledgement.
        let _ = reply(&mut out, serde_json::json!({ "ok": true }));
        let _ = out.flush();
        eprintln!("wgrep: shutting down on request");
        std::process::exit(0);
    }
    // Full-walk counters by cause, for the incremental-freshness work (see
    // `docs/WGREP_INCREMENTAL_FRESHNESS.md`, Testing). Read under the shared
    // lock, so it can wait behind a running reconcile.
    if request.get("stats").is_some() {
        let s = state.read().unwrap();
        let watcher = pending.health.as_ref().map(watch_budget::Health::stats).unwrap_or_default();
        reply(
            &mut out,
            serde_json::json!({
                "ok": true,
                "walks": {
                    "startup": s.walks.startup,
                    "overflow": s.walks.overflow,
                    "outside_dep": s.walks.outside_dep,
                    "scoped": s.walks.scoped,
                    "pending_retry": s.walks.pending_retry,
                    "rebuild": s.walks.rebuild,
                },
                "dirty": s.dirty,
                "added": s.added.len(),
                "pending": pending.len(),
                "watcher": {
                    "events": watcher.events,
                    "paths": watcher.paths,
                    "payload_bytes": watcher.payload_bytes,
                    "untrusted": watcher.untrusted,
                },
                "needs_rescan": s.needs_rescan,
                "handlers": {
                    "active": capacity.active(),
                    "max": capacity.max,
                    "rejected": capacity.rejected(),
                },
            }),
        )?;
        out.flush()?;
        return Ok(());
    }
    let query = request.get("query").and_then(|v| v.as_str()).and_then(plan::decode);
    let prefix = request.get("prefix").and_then(|v| v.as_str());
    let (Some(query), Some(prefix)) = (query, prefix) else {
        reply(&mut out, serde_json::json!({ "ok": false, "reason": "bad request" }))?;
        return Ok(());
    };
    let started = Instant::now();
    let result = answer(state, pending, &pending.health.as_ref().expect("daemon health").clone(), &query, prefix);
    let answer_ms = started.elapsed().as_secs_f64() * 1000.0;
    if std::env::var_os("WGREP_INDEX_DEBUG").is_some() {
        eprintln!("wgrep: answer {answer_ms:.1} ms (rescan {})", matches!(result, Ok(Answered::Candidates(_, true, _))));
    }
    let (ans, rescanned, lock_ms) = match result {
        Ok(Answered::Candidates(ans, rescanned, lock_ms)) => (ans, rescanned, lock_ms),
        Ok(Answered::PendingTimeout) => {
            // In-flight watcher work did not drain within the bound: the cached
            // candidate set may be stale. Decline it (never answer an empty
            // list as success) so the client walks authoritatively.
            eprintln!("wgrep: pending work did not drain; declining cached answer");
            reply(&mut out, serde_json::json!({ "ok": false, "reason": "pending_timeout" }))?;
            out.flush()?;
            return Ok(());
        }
        Err(err) => {
            reply(&mut out, serde_json::json!({ "ok": false, "reason": format!("{err:#}") }))?;
            out.flush()?;
            return Ok(());
        }
    };
    if pending.health.as_ref().is_some_and(|health| health.stats().untrusted) {
        reply(&mut out, serde_json::json!({ "ok": false, "reason": "watcher_untrusted" }))?;
        out.flush()?;
        return Ok(());
    }
    // Broad searches share the machine's threads instead of each client
    // starting one per CPU, which oversubscribes the CPUs when many agents
    // search at once. The share is held until the client closes the
    // connection (or its process exits).
    let lease = (ans.hits.len() >= LEASE_MIN_CANDIDATES).then(Lease::acquire);
    let threads = lease.as_ref().map(|l| l.threads);
    let write_started = Instant::now();
    reply(
        &mut out,
        serde_json::json!({ "ok": true, "files": ans.total, "candidates": ans.hits.len(), "threads": threads }),
    )?;
    // Paths are read from the index here, outside the state lock.
    for hit in &ans.hits {
        match hit {
            Hit::Id(id) => {
                let rel = std::str::from_utf8(ans.index.path(*id as usize)).unwrap_or("");
                out.write_all(rest_under(rel, prefix).unwrap_or(rel).as_bytes())?;
            }
            Hit::Added(rest) => out.write_all(rest.as_bytes())?,
        }
        out.write_all(b"\n")?;
    }
    out.flush()?;
    let write_ms = write_started.elapsed().as_secs_f64() * 1000.0;
    crate::index::profile::flush_with(
        "daemon",
        serde_json::json!({
            "read_ms": read_ms,
            "lock_ms": lock_ms,
            "answer_ms": answer_ms,
            "write_ms": write_ms,
            "candidates": ans.hits.len(),
            "files": ans.total,
            "rescan": rescanned,
            "lease_threads": threads,
        }),
    );
    if lease.is_some() {
        let stream = out.stream;
        // Any byte or EOF ends the lease; a hung client must not pin the
        // thread (and its thread share) forever, so bound the wait too.
        stream.set_read_timeout(Some(LEASE_HOLD))?;
        let _ = std::io::Read::read(&mut &stream, &mut [0u8; 1]);
    }
    Ok(())
}

/// Searches with at least this many candidate files take a thread lease.
const LEASE_MIN_CANDIDATES: usize = 256;

static ACTIVE_LEASES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One broad search's share of the machine's threads.
struct Lease {
    threads: usize,
}

impl Lease {
    fn acquire() -> Lease {
        use std::sync::atomic::Ordering;
        let active = ACTIVE_LEASES.fetch_add(1, Ordering::SeqCst) + 1;
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        Lease { threads: (cpus / active).max(1) }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        ACTIVE_LEASES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hiargs_for(argv: &[&str]) -> HiArgs {
        let args: Vec<OsString> = argv.iter().map(OsString::from).collect();
        match crate::flags::parse_args(args) {
            ParseResult::Ok(args) => args,
            _ => panic!("parse failed for {argv:?}"),
        }
    }

    fn test_health() -> watch_budget::Health { watch_budget::bounded().2 }

    /// Fixture root:
    ///   .git/                  (marks a git repo, so .gitignore applies)
    ///   visible.txt
    ///   sub/keep.txt
    ///   sub/.gitignore         ("ignored/": applies to sub's subtree, not sub)
    ///   sub/ignored/skip.txt
    ///   sub/ignored/deep/nest.txt
    ///   .hdir/.h.txt          (hidden dir, filtered with default flags)
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("sub/ignored/deep")).unwrap();
        std::fs::create_dir_all(root.join(".hdir")).unwrap();
        for f in ["visible.txt", "sub/keep.txt", "sub/ignored/skip.txt", "sub/ignored/deep/nest.txt", ".hdir/.h.txt"] {
            std::fs::write(root.join(f), "hello\n").unwrap();
        }
        std::fs::write(root.join("sub/.gitignore"), "ignored/\n").unwrap();
        Fixture { _dir: dir, root }
    }

    /// Files in the walk as root-relative `/`-joined strings, sorted.
    fn walked_files(builder: &ignore::WalkBuilder, root: &Path) -> Vec<String> {
        let mut out: Vec<String> = builder
            .build()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .filter_map(|e| build::relative(e.path(), root))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn scoped_walk_at_root_matches_full_walk() {
        let f = fixture();
        let args = hiargs_for(&["--files", f.root.to_str().unwrap()]);
        let full = walked_files(&args.walk_builder().unwrap(), &f.root);
        let scoped = walked_files(&args.walk_builder_at(&f.root, &f.root).unwrap(), &f.root);
        assert_eq!(full, scoped);
        assert!(full.contains(&"sub/keep.txt".to_string()));
        // Ignore files drive the walk but are never yielded by it.
        assert!(!full.contains(&"sub/.gitignore".to_string()));
        assert!(!full.contains(&"sub/ignored/skip.txt".to_string()));
        assert!(!full.contains(&".hdir/.h.txt".to_string()));
    }

    #[test]
    fn scoped_walk_of_subtree_matches_full_walk_subset() {
        let f = fixture();
        let args = hiargs_for(&["--files", f.root.to_str().unwrap()]);
        let sub = f.root.join("sub");
        let scoped = walked_files(&args.walk_builder_at(&f.root, &sub).unwrap(), &f.root);
        assert_eq!(scoped, vec!["sub/keep.txt".to_string()]);
    }

    #[test]
    fn scoped_walk_offsets_max_depth() {
        let f = fixture();
        let root = f.root.to_str().unwrap();
        let args = hiargs_for(&["--max-depth", "2", "--files", root]);
        // Full walk: depth-2 files only.
        let full = walked_files(&args.walk_builder().unwrap(), &f.root);
        assert_eq!(full, vec!["sub/keep.txt".to_string(), "visible.txt".to_string()]);
        // `sub` is at depth 1, so its scoped walk gets max_depth 1.
        let sub = f.root.join("sub");
        let scoped = walked_files(&args.walk_builder_at(&f.root, &sub).unwrap(), &f.root);
        assert_eq!(scoped, vec!["sub/keep.txt".to_string()]);
        // At exactly max_depth the scoped walk is empty; below it there is
        // no scoped walk.
        let at_max = walked_files(&args.walk_builder_at(&f.root, &sub.join("ignored")).unwrap(), &f.root);
        assert!(at_max.is_empty());
        assert!(args.walk_builder_at(&f.root, &sub.join("ignored/deep")).is_none());
    }

    #[test]
    fn scoped_walk_falls_back_with_override_globs() {
        let f = fixture();
        let root = f.root.to_str().unwrap();
        let args = hiargs_for(&["-g", "*.txt", "--files", root]);
        assert!(args.walk_builder_at(&f.root, &f.root).is_none());
    }

    #[test]
    fn ancestor_visibility_follows_ignore_rules() {
        let f = fixture();
        let args = hiargs_for(&["--files", f.root.to_str().unwrap()]);
        let none = |_: &Path| false;
        assert!(ancestors_visible(&args, &f.root, &f.root, &none));
        assert!(ancestors_visible(&args, &f.root, &f.root.join("sub"), &none));
        assert!(ancestors_visible(&args, &f.root, &f.root.join("visible.txt"), &none));
        // Ignored and hidden subtrees are not visible...
        assert!(!ancestors_visible(&args, &f.root, &f.root.join("sub/ignored"), &none));
        assert!(!ancestors_visible(&args, &f.root, &f.root.join("sub/ignored/deep"), &none));
        assert!(!ancestors_visible(&args, &f.root, &f.root.join(".hdir"), &none));
        // ...unless already known to hold a visible file.
        assert!(ancestors_visible(&args, &f.root, &f.root.join("sub/ignored"), &|_| true));
        // Outside the root is never visible.
        assert!(!ancestors_visible(&args, &f.root, f.root.parent().unwrap(), &none));
    }

    /// A one-file index for `rel`, written to `dir` so the query tests need
    /// neither `WGREP_INDEX_DIR` nor a real build. No postings are needed:
    /// `Query::All` returns every seen file.
    fn one_file_index(dir: &Path, rel: &str) -> Arc<Index> {
        let path = dir.join("unit-index.wgi");
        let header = serde_json::json!({ "version": crate::index::format::VERSION });
        let files = vec![crate::index::format::FileEntry {
            rel: rel.to_string(),
            size: 0,
            mtime_ns: 0,
            change_ns: 0,
        }];
        let postings: Vec<std::io::Result<(u32, Vec<u32>, Vec<u8>)>> = Vec::new();
        crate::index::format::write(&path, &header, &files, postings.into_iter(), 100).unwrap();
        Arc::new(Index::open(&path).unwrap())
    }

    /// A `State` whose file set is already reconciled (`needs_rescan` false)
    /// and holds one current file, so `candidates` returns it with no walk.
    fn state_for(root: &Path, index: Arc<Index>) -> State {
        let args = hiargs_for(&["--files", root.to_str().unwrap()]);
        let n = index.file_count();
        State {
            root_arg: root.to_path_buf(),
            root: root.to_path_buf(),
            args,
            index,
            deps: ignore_deps::Deps::snapshot(root),
            ids: vec![IdState { seq: 0, seen: true, current: true }; n],
            added: HashMap::new(),
            needs_rescan: false,
            rescan_cause: WalkCause::Startup,
            walks: WalkCounts::default(),
            next_seq: n as u32,
            dirty: 0,
            reconciling: Arc::new(AtomicBool::new(false)),
            missed: Arc::new(AtomicBool::new(false)),
            unprunable: false,
        }
    }

    #[test]
    fn pending_wait_empty_drains_when_empty() {
        let pending = PendingSet::new_with_bound(Duration::from_millis(50));
        assert_eq!(pending.wait_empty(), WaitOutcome::Drained);
        let path = PathBuf::from("/tmp/wgrep-pending");
        pending.insert([path.clone()]);
        assert_eq!(pending.len(), 1);
        pending.remove([path]);
        assert_eq!(pending.wait_empty(), WaitOutcome::Drained);
    }

    #[test]
    fn pending_wait_empty_times_out_with_work_in_flight() {
        let pending = PendingSet::new_with_bound(Duration::from_millis(50));
        pending.insert([PathBuf::from("/tmp/wgrep-stuck")]);
        let started = Instant::now();
        assert_eq!(pending.wait_empty(), WaitOutcome::TimedOut);
        // The short test bound, not the production 30 s.
        assert!(started.elapsed() < Duration::from_secs(5), "wait ignored the short test bound");
        // The path is still pending: the caller must decline, not read a
        // stale cache.
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn pending_wait_uses_one_fixed_deadline_across_wakeups() {
        // A 200 ms bound. If each wakeup restarted the timeout, the signaller
        // below would keep the wait alive for its whole ~1 s run.
        let pending = Arc::new(PendingSet::new_with_bound(Duration::from_millis(200)));
        pending.insert([PathBuf::from("/tmp/wgrep-stuck")]);
        let stop = Arc::new(AtomicBool::new(false));
        let signaller = {
            let (pending, stop) = (pending.clone(), stop.clone());
            std::thread::spawn(move || {
                for _ in 0..100 {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    // A spurious wakeup with work still pending: exactly what
                    // a reset-per-wakeup deadline would never outlast.
                    drop(pending.mutex.lock().unwrap());
                    pending.cv.notify_all();
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let started = Instant::now();
        let outcome = pending.wait_empty();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        signaller.join().unwrap();
        assert_eq!(outcome, WaitOutcome::TimedOut);
        // Fixed deadline: ~200 ms, not the signaller's ~1 s.
        assert!(elapsed < Duration::from_secs(1), "fixed deadline not honored: {elapsed:?}");
    }

    #[test]
    fn answer_returns_cached_candidates_when_pending_drained() {
        let f = fixture();
        let index = one_file_index(&f.root, "visible.txt");
        let state = RwLock::new(state_for(&f.root, index));
        let pending = PendingSet::new_with_bound(Duration::from_millis(50));
        let got = answer(&state, &pending, &test_health(), &plan::Query::All, "").unwrap();
        match got {
            Answered::Candidates(ans, _, _) => assert_eq!(ans.hits.len(), 1),
            Answered::PendingTimeout => panic!("drained pending must not decline"),
        }
    }

    #[test]
    fn answer_declines_cached_candidates_when_pending_times_out() {
        // The same state as the drained test would return one candidate. A
        // stuck pending path must make the daemon decline it, never serve it.
        let f = fixture();
        let index = one_file_index(&f.root, "visible.txt");
        let state = RwLock::new(state_for(&f.root, index));
        let pending = PendingSet::new_with_bound(Duration::from_millis(50));
        pending.insert([f.root.join("visible.txt")]);
        let got = answer(&state, &pending, &test_health(), &plan::Query::All, "").unwrap();
        assert!(
            matches!(got, Answered::PendingTimeout),
            "timeout must decline the cached candidate set"
        );
    }

    #[test]
    fn watcher_overflow_without_paths_declines_cached_answer() {
        let f = fixture();
        let state = RwLock::new(state_for(&f.root, one_file_index(&f.root, "visible.txt")));
        let (ingress, _rx, health) = watch_budget::bounded_with_limits(1, 4, 128);
        let event = || Ok(notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any)));
        ingress.try_ingest(event());
        ingress.try_ingest(event());
        let pending = PendingSet {
            mutex: Mutex::new(HashSet::new()),
            cv: health.condvar(),
            gate: health.gate(),
            bound: Duration::ZERO,
            health: Some(health.clone()),
        };
        assert!(health.stats().untrusted);
        let got = answer(&state, &pending, &health, &plan::Query::All, "").unwrap();
        assert!(matches!(got, Answered::PendingTimeout));
    }

    #[test]
    fn queued_pathless_work_counts_at_query_gate() {
        let f = fixture();
        let state = RwLock::new(state_for(&f.root, one_file_index(&f.root, "visible.txt")));
        let (ingress, _rx, health) = watch_budget::bounded_with_limits(2, 4, 128);
        ingress.try_ingest(Ok(notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))));
        assert!(!health.stats().untrusted);
        let pending = PendingSet {
            mutex: Mutex::new(HashSet::new()),
            cv: health.condvar(),
            gate: health.gate(),
            bound: Duration::ZERO,
            health: Some(health.clone()),
        };
        assert!(matches!(answer(&state, &pending, &health, &plan::Query::All, "").unwrap(), Answered::PendingTimeout));
    }

    /// A connected server/client socket pair on loopback.
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn handler_cap_limits_and_releases() {
        let cap = Capacity::new(2);
        assert_eq!(cap.max, 2);
        let a = cap.try_acquire().expect("first slot");
        let b = cap.try_acquire().expect("second slot");
        assert_eq!(cap.active(), 2);
        assert!(cap.try_acquire().is_none(), "cap must refuse past the limit");
        assert_eq!(cap.rejected(), 1);
        drop(a);
        assert_eq!(cap.active(), 1);
        let c = cap.try_acquire().expect("a released slot is reusable");
        assert_eq!(cap.active(), 2);
        drop((b, c));
        assert_eq!(cap.active(), 0);
        assert!(cap.try_acquire().is_some(), "an emptied cap is reusable");
    }

    #[test]
    fn handler_slot_releases_on_panic() {
        let cap = Capacity::new(1);
        let guard = cap.try_acquire().expect("slot");
        let cap2 = cap.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("handler blew up");
        }));
        assert!(result.is_err());
        assert_eq!(cap2.active(), 0, "a panicking handler must release its slot");
        assert!(cap2.try_acquire().is_some(), "the slot must be reusable");
    }

    #[test]
    fn read_request_accepts_one_line() {
        let (mut client, server) = socket_pair();
        client.write_all(b"{\"token\":\"t\"}\n").unwrap();
        match read_request(&server, 1024, Duration::from_secs(2)).unwrap() {
            RequestRead::Line(line) => assert_eq!(line, "{\"token\":\"t\"}"),
            _ => panic!("a normal request line must be accepted"),
        }
    }

    #[test]
    fn reply_deadline_does_not_restart_after_partial_progress() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(300);
        // Simulated successful partial writes and trickling progress. Each
        // write receives less time, and progress beyond the absolute deadline
        // fails instead of receiving a fresh 300 seconds.
        assert_eq!(reply_remaining(deadline, start).unwrap(), Duration::from_secs(300));
        assert_eq!(reply_remaining(deadline, start + Duration::from_secs(120)).unwrap(), Duration::from_secs(180));
        assert_eq!(reply_remaining(deadline, start + Duration::from_secs(299)).unwrap(), Duration::from_secs(1));
        assert_eq!(reply_remaining(deadline, deadline).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn reply_local_send_shutdown_returns_handler_capacity() {
        let cap = Capacity::new(1);
        let (client, server) = socket_pair();
        // Shut down the server's own send direction. This makes its next send
        // fail locally; it does not model a remote reset or peer-close error.
        server.shutdown(std::net::Shutdown::Write).unwrap();
        let mut writer = ReplyWriter::new(server);
        let result = (|| {
            let _guard = cap.try_acquire().unwrap();
            writer.write_all(b"reply\n")?;
            writer.flush()?;
            Ok::<(), std::io::Error>(())
        })();
        drop(client);
        assert!(result.is_err());
        assert_eq!(cap.active(), 0);
        assert!(cap.try_acquire().is_some());
    }

    #[test]
    fn read_request_rejects_oversized_without_newline() {
        let (mut client, server) = socket_pair();
        client.write_all(&vec![b'x'; 4096]).unwrap();
        match read_request(&server, 1024, Duration::from_secs(2)).unwrap() {
            RequestRead::TooLarge => {}
            _ => panic!("an oversized request must be refused, never truncated"),
        }
    }

    #[test]
    fn read_request_rejects_over_cap_line_even_with_newline() {
        let (mut client, server) = socket_pair();
        let mut big = vec![b'x'; 4096];
        big.push(b'\n');
        client.write_all(&big).unwrap();
        match read_request(&server, 1024, Duration::from_secs(2)).unwrap() {
            RequestRead::TooLarge => {}
            _ => panic!("a line over the cap must be refused whole"),
        }
    }

    #[test]
    fn read_request_times_out_on_trickle() {
        let (mut client, server) = socket_pair();
        client.write_all(b"{").unwrap();
        let started = Instant::now();
        let out = read_request(&server, 1024, Duration::from_millis(150)).unwrap();
        assert!(matches!(out, RequestRead::Ended), "a trickled header must end, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the whole-read deadline must be honored, not the per-read timeout"
        );
    }
}
