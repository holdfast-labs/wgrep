//! Embedded text index for a host daemon (the `wg` service).
//!
//! This exposes the same trigram index and freshness contract as the standalone
//! `wgrep` daemon, but without its listener or watcher: the host owns one
//! watcher per root and forwards the same events here. Candidate paths are
//! relative to a search prefix, `/`-separated, and already in rg's walk order.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::flags::{HiArgs, ParseResult};
use crate::index::{build, cli, daemon, format::Index, plan, store, watch_budget};

/// One directory search path and its encoded trigram query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextPlanItem {
    /// Position in rg's parsed search-path list.
    pub arg_index: usize,
    /// Absolute, canonical directory for this search path.
    pub absolute: PathBuf,
    /// Encoded [`crate::index::plan::Query`].
    pub query: String,
}

/// What an embedded service can prune for one invocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextPlan {
    pub prunable: bool,
    pub items: Vec<TextPlanItem>,
    pub reason: Option<String>,
}

/// Candidate paths for one search path, relative to that path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextGroup {
    pub arg_index: usize,
    /// Files the index knows under this prefix, whether or not they may match.
    pub files: usize,
    pub paths: Vec<String>,
}

/// Verified candidates under one search path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextCandidates {
    /// Files under the prefix, whether or not they may match.
    pub files: usize,
    /// Candidate paths relative to the prefix, `/`-separated, in walk order.
    pub paths: Vec<String>,
}

/// A text index for one root, fed by the host's watcher.
pub struct TextIndex {
    /// Canonical spelling, for comparing an attached search path with roots.
    root: PathBuf,
    /// The spelling the walker and index store use. On Windows this is not a
    /// verbatim (`\\?\`) path, so walked files stay under it.
    walk_root: PathBuf,
    state: Arc<RwLock<daemon::State>>,
    pending: Arc<daemon::PendingSet>,
    health: watch_budget::Health,
    ingress: watch_budget::Ingress,
    index_file: PathBuf,
    build_ms: u64,
    built: bool,
}

impl TextIndex {
    /// Open a root's index, building one when it is missing or unreadable.
    ///
    /// The cached file set is reconciled with rg's walker by
    /// [`TextIndex::reconcile_now`] or, failing that, by the first query, so a
    /// caller must forward watcher events from before either runs.
    pub fn open(root: &Path) -> anyhow::Result<TextIndex> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let started = std::time::Instant::now();
        let args = walk_args(&walk_spelling(&root))?;
        let (search_paths, cwd) = args.index_paths();
        let root_arg = search_paths
            .first()
            .cloned()
            .unwrap_or_else(|| walk_spelling(&root));
        let walk_root = store::absolute(&root_arg, cwd);
        let mut built = false;
        // Taken before the walk: a file modified after this point can be
        // skipped as racy, so it must stay pending for the next rebuild.
        let build_started_ns = build::mtime_ns_now();
        let index_file = match store::current(&walk_root) {
            Some(file) if Index::open(&file).is_ok() => file,
            _ => {
                let files = cli::walk(&args)?;
                build::build(&walk_root, &root_arg, files, cli::dense_percent())?;
                built = true;
                store::current(&walk_root).ok_or_else(|| anyhow::anyhow!("index missing after build"))?
            }
        };
        let index = Arc::new(Index::open(&index_file)?);
        if std::env::var_os("WGREP_INDEX_DEBUG").is_some() {
            eprintln!(
                "wg-text: root={} root_arg={} walk_root={} index={:?} files={}",
                root.display(),
                root_arg.display(),
                walk_root.display(),
                index_file,
                index.file_count()
            );
        }
        let build_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (ingress, rx, health) = watch_budget::bounded();
        // Diagnostic hook: force the sticky untrusted state so a test can
        // prove that an unhealthy watcher falls back to a full scan.
        if std::env::var_os("WG_TEXT_FORCE_UNTRUSTED").is_some() {
            ingress.mark_untrusted();
        }
        let pending = Arc::new(daemon::PendingSet::with_health(&health));
        let reconciling = Arc::new(AtomicBool::new(false));
        let missed = Arc::new(AtomicBool::new(false));
        let state = Arc::new(RwLock::new(daemon::State::new(
            args,
            root_arg.clone(),
            walk_root.clone(),
            index,
            reconciling,
            missed,
        )));
        if built {
            state.write().unwrap().set_build_started_ns(build_started_ns);
        }
        {
            let (state, pending, health, root, rx) =
                (state.clone(), pending.clone(), health.clone(), walk_root.clone(), rx);
            std::thread::Builder::new()
                .name("wg-text-events".into())
                .spawn(move || daemon::event_loop(state, pending, health, root, rx))?;
        }
        let rebuild_args = walk_args(&walk_spelling(&root))?;
        {
            let (state, root) = (state.clone(), walk_root.clone());
            std::thread::Builder::new()
                .name("wg-text-rebuild".into())
                .spawn(move || daemon::rebuild_loop(state, rebuild_args, root.clone(), root))?;
        }
        Ok(TextIndex {
            root,
            walk_root,
            state,
            pending,
            health,
            ingress,
            index_file,
            build_ms,
            built,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reconcile the file set with rg's walker now, so the first query does
    /// not pay for it. Call only after the host's watcher is running: a change
    /// made before that would otherwise be missed. On error the rescan stays
    /// pending and the first query retries it.
    pub fn reconcile_now(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("text index state lock poisoned"))?;
        state.reconcile_if_needed()
    }

    /// Milliseconds spent opening or building the index for `wg up`.
    pub fn build_ms(&self) -> u64 {
        self.build_ms
    }

    /// Whether this process built the index (rather than reusing one).
    pub fn built(&self) -> bool {
        self.built
    }

    /// Forward one watcher event. Bounded: overflow makes the index untrusted.
    pub fn ingest(&self, event: notify::Result<notify::Event>) {
        self.ingress.try_ingest(event);
    }

    /// The host lost events. Every later query declines until a restart.
    pub fn mark_untrusted(&self) {
        self.ingress.mark_untrusted();
    }

    /// Verified candidates for an encoded query under `prefix` (relative to
    /// the root, `/`-separated, empty for the whole root). `None` means the
    /// caller must walk authoritatively.
    pub fn candidates(&self, query: &str, prefix: &str) -> anyhow::Result<Option<TextCandidates>> {
        if !self.root_contains_prefix(prefix) {
            return Ok(None);
        }
        let Some(query) = plan::decode(query) else { return Ok(None) };
        Ok(daemon::answer_text(&self.state, &self.pending, &self.health, &query, prefix)?
            .map(|answer| TextCandidates { files: answer.files, paths: answer.paths }))
    }

    /// Live state for `wg status` plus on-disk and resident size estimates.
    pub fn status(&self) -> serde_json::Value {
        let state = self.state.read().unwrap();
        let mut value = state.text_stats();
        let current = store::current(&self.walk_root).unwrap_or_else(|| self.index_file.clone());
        let disk_bytes = std::fs::metadata(&current).map(|m| m.len()).unwrap_or(0);
        let resident = resident_bytes(&current).unwrap_or(disk_bytes);
        value["ready"] = serde_json::json!(!self.health.stats().untrusted);
        value["untrusted"] = serde_json::json!(self.health.stats().untrusted);
        value["build_ms"] = serde_json::json!(self.build_ms);
        value["built"] = serde_json::json!(self.built);
        value["disk_bytes"] = serde_json::json!(disk_bytes);
        value["resident_bytes"] = serde_json::json!(resident);
        value["resident_estimated"] = serde_json::json!(cfg!(not(target_os = "linux")));
        value["index_path"] = serde_json::json!(current);
        value["root"] = serde_json::json!(self.root);
        value
    }

    /// The prefix is index data, so it never contains `.` or `..` components.
    fn root_contains_prefix(&self, prefix: &str) -> bool {
        prefix.is_empty()
            || prefix.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
    }
}

/// `path` relative to `root`, `/`-separated, when every component is UTF-8.
/// `None` means the caller must walk authoritatively.
pub fn prefix_of(path: &Path, root: &Path) -> Option<String> {
    crate::index::build::relative(path, root)
}

/// The spelling to give rg's walker and the index store.
///
/// On Windows, `canonicalize` returns a verbatim path (`\\?\E:\...`). rg's
/// walker reports entries without that prefix, so a verbatim root would make
/// every walked file fail `relative(path, root)`. Strip it here; the canonical
/// spelling stays available for matching an attached search path.
#[cfg(windows)]
fn walk_spelling(root: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let mut units: Vec<u16> = root.as_os_str().encode_wide().collect();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    if units.starts_with(&verbatim) {
        units.drain(0..verbatim.len());
        let unc: Vec<u16> = r"UNC\".encode_utf16().collect();
        if units.starts_with(&unc) {
            units.splice(0..unc.len(), [b'\\' as u16, b'\\' as u16]);
        }
        return PathBuf::from(std::ffi::OsString::from_wide(&units));
    }
    root.to_path_buf()
}

#[cfg(not(windows))]
fn walk_spelling(root: &Path) -> PathBuf {
    root.to_path_buf()
}

fn walk_args(root: &Path) -> anyhow::Result<HiArgs> {
    match crate::flags::parse_args(vec![
        OsString::from("--files"),
        root.as_os_str().to_os_string(),
    ]) {
        ParseResult::Ok(args) => Ok(args),
        ParseResult::Err(err) => Err(err),
        ParseResult::Special(_) => anyhow::bail!("unexpected special mode"),
    }
}

/// Resident bytes of the index mapping, from `/proc/self/smaps` on Linux.
/// Other platforms return `None`; the caller then reports the mapped size.
fn resident_bytes(path: &Path) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let wanted = path.to_string_lossy();
        let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
        let mut in_mapping = false;
        let mut total = 0u64;
        for line in smaps.lines() {
            if line.contains(" r") && line.contains('/') && !line.trim_start().starts_with("Rss") {
                in_mapping = line.ends_with(wanted.as_ref());
                continue;
            }
            if in_mapping {
                if let Some(rest) = line.strip_prefix("Rss:") {
                    total += rest
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0)
                        * 1024;
                }
                if line.starts_with("VmFlags:") {
                    in_mapping = false;
                }
            }
        }
        Some(total)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}
