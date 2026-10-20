//! Resident ranking daemon: one model + one vector store per root, many
//! clients.
//!
//! `serve(root)` installs the file watcher FIRST (so build-time mutations
//! are queued, never lost), loads the pinned code model once, builds the
//! whole corpus (parallel chunk + bounded-batch embed), reconciles once
//! against the queued events, and answers rank requests over loopback TCP
//! with token auth.
//!
//! Resource bounds (single configured budget, no nested pools):
//! - rank workers = `min(available_parallelism, 8)`; admit pool = 4;
//! - rank queue cap 128 with explicit `busy` errors; connection queue cap
//!   64 with explicit drops counted in stats — never unbounded threads,
//!   never unbounded queues, never unbounded request lines (64 KiB cap);
//! - read + write deadlines on every socket: each request/reply frame is
//!   bounded WHOLE-frame by 120 s (recomputed per 8 KiB fill, so a
//!   1-byte trickle cannot stretch a frame), writes by 120 s;
//! - embedding is sequential inside each job: one `encode_fast` per text,
//!   never the tokenizers rayon batch pool (verified: no batch tokenize
//!   API is called anywhere in this tree, so no rayon worker threads
//!   exist); corpus builds use one scoped batch at a time.
//! - singleton: the daemon holds an exclusive OS advisory lock on the
//!   slot's `serve.lock` for life, so a duplicate `--serve` is refused
//!   instead of doubling the resident model+vectors.
//!
//! Freshness: the watcher only wakes a reconcile, which diffs the live
//! tree against stored fingerprints (size + mtime + change token) and
//! re-embeds exactly what changed. Before replying, served passages are
//! re-validated (fingerprint + exact content lines); on mismatch the
//! daemon reconciles and re-ranks ONCE, then drops anything still stale.
//! Every reply carries the corpus generation it ranked from. New files
//! become visible after at most debounce + reconcile + their own embed.
//!
//! Eligibility (ignore rules) is EVENTUAL, not synchronous: a file that
//! leaves discovery (deleted or newly ignored) is dropped by the next
//! reconcile's re-discovery diff — never served after that. Consequences:
//! - in-root rule edits (`.gitignore`/`.rgignore`/`.ignore`) are watcher
//!   events, so the drop lands within debounce + reconcile;
//! - ancestor/global/exclude rules live OUTSIDE the root and are not
//!   watched: those drops wait for the 60 s periodic reconcile;
//! - before-response validation checks fingerprint + content only, so a
//!   freshly-ignored file can still be served until its reconcile lands.
//!   This bound (not silent permanence) is the contract; the alternative
//!   — per-row matcher checks on every reply — is deferred until the
//!   correctness baseline is accepted.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, mpsc};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::chunks;
use super::corpus::{self, Corpus};
use super::model::{self, ModelCtx, CODE};
use super::rank;
use super::slots;
use super::{DIM, PROTO};

const DEBOUNCE_MS: u64 = 500;
const RECONCILE_SECS: u64 = 60;
const MAX_WORKERS: usize = 8;
const QUEUE_CAP: usize = 128;
const ADMIT_THREADS: usize = 4;
const CONN_QUEUE_CAP: usize = 64;
const QUERY_SECS: u64 = 120;
/// Hard cap on one request line: rank queries are small text.
const MAX_REQUEST_BYTES: usize = 64 << 10;
/// Bounded validation retries per rank reply.
const VALIDATE_RETRIES: usize = 1;

struct Shared {
    root: PathBuf,
    slot: PathBuf,
    chunking: chunks::Chunking,
    token: String,
    model: ModelCtx,
    corpus: RwLock<Corpus>,
    generation: AtomicU64,
    queries: AtomicU64,
    rejected_busy: AtomicU64,
    rejected_admit: AtomicU64,
    /// Singleflight for reconcile: only one full diff runs at a time;
    /// concurrent callers set `recon_again` for one follow-up pass.
    recon_mu: std::sync::Mutex<()>,
    recon_again: std::sync::atomic::AtomicBool,
    workers: usize,
    started: Instant,
}

fn random_token() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::process::id().hash(&mut h);
    Instant::now().hash(&mut h);
    format!("{:016x}{:016x}", h.finish(), !h.finish())
}

fn reply(stream: &TcpStream, v: &serde_json::Value) {
    let mut out = std::io::BufWriter::with_capacity(64 * 1024, stream);
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// Read one `\n`-terminated line, capped at MAX_REQUEST_BYTES, bounded
/// by the whole-frame QUERY_SECS resource contract. Buffered (8 KiB
/// fills, not one syscall per byte); see `iocapped` for the exact
/// preserved semantics.
fn read_capped_line(stream: &TcpStream, cap: usize) -> anyhow::Result<String> {
    let bytes = super::iocapped::read_capped_frame(stream, cap, Duration::from_secs(QUERY_SECS))?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("wgrep-sem: request is not UTF-8"))
}

/// Diff the live tree against stored fingerprints and incrementally
/// re-embed exactly what changed (modified/added) or drop what left
/// (removed). Prepare runs off-lock; publish is atomic and refuses
/// mid-prepare races (the next reconcile heals those).
///
/// Singleflight: concurrent callers (watcher + rank workers) never run
/// duplicate full walks/embeds — losers request one follow-up pass.
/// Removal re-checks absence under the write lock, so a file recreated
/// between the listing and the lock is never deleted (the next diff
/// picks it up as added instead).
fn reconcile(shared: &Shared) {
    match shared.recon_mu.try_lock() {
        Ok(_guard) => {
            reconcile_inner(shared);
            // Drain one coalesced request; anything newer stays flagged
            // for the next caller (or the periodic tick) instead of
            // stacking duplicate passes here.
            if shared.recon_again.swap(false, Ordering::Relaxed) {
                reconcile_inner(shared);
            }
        }
        Err(_) => {
            shared.recon_again.store(true, Ordering::Relaxed);
        }
    }
}

fn reconcile_inner(shared: &Shared) {
    let live = chunks::list_files(&shared.root);
    let live_set: HashSet<&PathBuf> = live.iter().collect();
    let known: HashMap<PathBuf, super::chunks::Fingerprint> = shared
        .corpus
        .read()
        .unwrap()
        .files
        .iter()
        .map(|(p, f)| (p.clone(), f.fp))
        .collect();
    let mut changed = 0usize;
    for abs in &live {
        if known.get(abs) != chunks::fingerprint(abs).as_ref() {
            match corpus::prepare_path(&shared.model, &shared.root, abs, shared.chunking) {
                Ok(Some(prep)) => {
                    let mut c = shared.corpus.write().unwrap();
                    if c.publish(prep) {
                        changed += 1;
                        shared.generation.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("wgrep-sem: prepare failed for {}: {e:#}", abs.display()),
            }
        }
    }
    let gone: Vec<PathBuf> = known.keys().filter(|p| !live_set.contains(*p)).cloned().collect();
    if !gone.is_empty() {
        // Removal eligibility by RE-DISCOVERY, not by an existence
        // shortcut: a file missing from discovery is dropped whether it
        // was deleted or newly ignored (.gitignore/.rgignore/global
        // rules). The old `metadata().is_ok() → skip` check kept
        // newly-ignored-but-existing files indexed forever, and
        // validation (fingerprint + content) cannot see eligibility, so
        // those rows would have been served indefinitely.
        //
        // Recreate race: a file absent from the first listing but present
        // again now is KEPT (never deleted on a stale gone list) and
        // refreshed when its fingerprint moved — publish's own
        // re-fingerprint guard covers races inside the prepare. Removal
        // itself runs under the write lock that serializes all publishes.
        // Residual corner (documented, vanishingly narrow): delete +
        // byte-identical recreate within one change-token tick AND a
        // removal landing exactly between the fresh listing and the lock
        // could drop rows the next tick re-adds as new; serve-time
        // validation never serves the deleted bytes regardless.
        let fresh = chunks::list_files(&shared.root);
        let fresh_set: HashSet<&PathBuf> = fresh.iter().collect();
        let mut removed = 0usize;
        let mut dropped = 0usize;
        for g in &gone {
            if fresh_set.contains(g) {
                // Recreated (or transiently missed): refresh, don't drop.
                let known_fp = shared.corpus.read().unwrap().files.get(g).map(|f| f.fp);
                if known_fp != chunks::fingerprint(g) {
                    match corpus::prepare_path(&shared.model, &shared.root, g, shared.chunking) {
                        Ok(Some(prep)) => {
                            let mut c = shared.corpus.write().unwrap();
                            if c.publish(prep) {
                                changed += 1;
                                shared.generation.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("wgrep-sem: prepare failed for {}: {e:#}", g.display())
                        }
                    }
                }
                continue;
            }
            {
                let mut c = shared.corpus.write().unwrap();
                // Under the lock that serializes all publishes: `g` missed
                // the fresh listing and no publish path installs rows for
                // unlisted files, so the drop is exact. No existence
                // shortcut: ignored files stay dropped until un-ignored
                // and re-discovered (then the changed-loop re-adds them).
                if !fresh_set.contains(g) {
                    let had = c.files.contains_key(g);
                    let before = c.chunk_count();
                    c.remove_prefix(g);
                    removed += before - c.chunk_count();
                    if had {
                        dropped += 1;
                    }
                }
            }
        }
        shared.generation.fetch_add(1, Ordering::Relaxed);
        if removed > 0 || changed > 0 {
            eprintln!("wgrep-sem: reconcile: {changed} updated, {dropped} dropped");
        }
    } else if changed > 0 {
        eprintln!("wgrep-sem: reconcile: {changed} updated, 0 removed");
    }
}

/// Content-relevant event kinds. Access/open/close carry no freshness
/// signal and are ignored; delivery ERRORS count as relevant (events may
/// have been lost, so a diff is mandatory).
fn event_relevant(res: &Result<notify::Event, notify::Error>) -> bool {
    match res {
        Err(_) => true,
        Ok(ev) => matches!(
            ev.kind,
            notify::EventKind::Create(_)
                | notify::EventKind::Modify(_)
                | notify::EventKind::Remove(_)
                | notify::EventKind::Other
        ),
    }
}

fn watch_loop(rx: mpsc::Receiver<Result<notify::Event, notify::Error>>, shared: Arc<Shared>) {
    loop {
        match rx.recv_timeout(Duration::from_secs(RECONCILE_SECS)) {
            Ok(first) => {
                // Aggregate relevance over the FIRST event plus the whole
                // drained burst: an Access followed by a Modify (or an
                // overflow error anywhere in the batch) must still wake a
                // reconcile. Checking only the first event loses changes.
                let mut relevant = event_relevant(&first);
                let mut disconnected = false;
                // Drain the burst within the debounce window (bounded: the
                // channel itself is capped, so this always terminates).
                let deadline = Instant::now() + Duration::from_millis(DEBOUNCE_MS);
                while Instant::now() < deadline {
                    match rx.try_recv() {
                        Ok(ev) => {
                            relevant |= event_relevant(&ev);
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            // Watcher died mid-burst: assume loss, reconcile.
                            relevant = true;
                            disconnected = true;
                            break;
                        }
                    }
                }
                if relevant {
                    reconcile(&shared);
                }
                if disconnected {
                    // Fall through to the reconcile-only tail below.
                    eprintln!("wgrep-sem: watcher channel lost; reconcile-only mode");
                    loop {
                        std::thread::sleep(Duration::from_secs(RECONCILE_SECS));
                        reconcile(&shared);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => reconcile(&shared),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Watcher is dead: bounded periodic reconcile instead of a
                // tight loop — correctness degrades to the interval, loudly.
                eprintln!("wgrep-sem: watcher channel lost; reconcile-only mode");
                loop {
                    std::thread::sleep(Duration::from_secs(RECONCILE_SECS));
                    reconcile(&shared);
                }
            }
        }
    }
}

struct Job {
    stream: TcpStream,
    query: String,
    prefix: String,
    top_n: usize,
}

/// Path join honoring the storage convention: passage paths are stored
/// `/`-separated; on Windows they map back to `\`. On Unix the stored
/// path joins verbatim (a backslash there is a real filename character).
fn abs_for(root: &Path, rel: &str) -> PathBuf {
    #[cfg(windows)]
    let rel = rel.replace('/', "\\");
    root.join(rel)
}

/// One served passage, FULLY OWNED: snapshotted under the same read lock
/// that ranked it. Nothing here is an index into the corpus, so a
/// concurrent watcher publish (swap_remove) between rank and validation
/// can neither panic out-of-bounds nor silently swap in a different
/// passage — the failure mode the index-carrying design had. Validation
/// and serialization touch only these owned bytes plus live source.
struct ServedRow {
    path: String,
    start: usize,
    end: usize,
    content: String,
    fp: Option<super::chunks::Fingerprint>,
    bm25: f64,
    semantic: f64,
    bm25_relative: f64,
    semantic_relative: f64,
    fused: f64,
    exact_identifier: bool,
}

/// Re-validate owned rows against live source: fingerprint plus the exact
/// content lines. Takes NO corpus reference — by construction it cannot
/// observe (or be corrupted by) a concurrent corpus mutation. Returns
/// per-row freshness.
fn validate_rows(root: &Path, rows: &[ServedRow]) -> Vec<bool> {
    rows.iter()
        .map(|r| {
            let abs = abs_for(root, &r.path);
            // Fingerprint check closes same-tick rewrite gaps only in
            // combination with the content check below (Linux ctime
            // granularity): both must agree.
            if chunks::fingerprint(&abs) != r.fp || r.fp.is_none() {
                return false;
            }
            let Ok(raw) = std::fs::read(&abs) else { return false };
            if raw.contains(&0) {
                return false;
            }
            // Same transform as chunk_file: exact UTF-8, split on '\n'.
            let Ok(text) = std::str::from_utf8(&raw) else { return false };
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            if r.start == 0 || r.end > lines.len() || r.start > r.end {
                return false;
            }
            lines[r.start - 1..r.end].concat() == r.content
        })
        .collect()
}

/// Rank and snapshot under ONE read lock: the returned rows are owned
/// copies (path/lines/content/fingerprint/scores) plus the generation
/// they were ranked from. No corpus index ever leaves the lock, so the
/// validate → reconcile → re-rank cycle in `serve_job` cannot observe a
/// torn corpus.
fn rank_once(shared: &Shared, query: &str, prefix: &str, top_n: usize) -> anyhow::Result<(Vec<ServedRow>, u64, usize, usize)> {
    let corpus = shared.corpus.read().unwrap();
    let scope: Vec<usize> = rank::scope_for(&corpus, prefix)?;
    let ranked = rank::rank_scoped(&corpus, &shared.model, query, &scope, top_n)?;
    let generation = shared.generation.load(Ordering::Relaxed);
    let files = corpus.file_count();
    let chunks_n = corpus.chunk_count();
    let rows: Vec<ServedRow> = ranked
        .iter()
        .map(|r| {
            let c = &corpus.chunks[r.idx];
            let abs = abs_for(&shared.root, &c.path);
            ServedRow {
                path: c.path.clone(),
                start: c.start,
                end: c.end,
                content: c.content.clone(),
                fp: corpus.files.get(&abs).map(|f| f.fp),
                bm25: r.bm25,
                semantic: r.semantic,
                bm25_relative: r.bm25_relative,
                semantic_relative: r.semantic_relative,
                fused: r.fused,
                exact_identifier: r.exact_identifier,
            }
        })
        .collect();
    Ok((rows, generation, files, chunks_n))
}

fn serve_job(shared: &Arc<Shared>, job: Job) {
    let answer: anyhow::Result<serde_json::Value> = (|| {
        let (mut ranked, mut generation, mut files, mut chunks_n) =
            rank_once(shared, &job.query, &job.prefix, job.top_n)?;
        // Bounded validate → reconcile → re-rank → drop-stale cycle.
        // Validation reads owned rows + live source only; a concurrent
        // publish between rank and validation can at most mark rows stale.
        for _ in 0..=VALIDATE_RETRIES {
            let fresh = validate_rows(&shared.root, &ranked);
            if fresh.iter().all(|&f| f) {
                break;
            }
            reconcile(shared);
            let retry = rank_once(shared, &job.query, &job.prefix, job.top_n)?;
            ranked = retry.0;
            generation = retry.1;
            files = retry.2;
            chunks_n = retry.3;
            let fresh2 = validate_rows(&shared.root, &ranked);
            ranked = ranked
                .into_iter()
                .zip(fresh2)
                .filter(|(_, f)| *f)
                .map(|(r, _)| r)
                .collect();
            break;
        }
        let model_id = format!("{}@{}", shared.model.pin.repo, &shared.model.pin.revision[..12]);
        // Serialization reads the owned snapshot — no corpus lock, no
        // re-indexing, so a racing publish cannot corrupt the reply.
        let results: Vec<serde_json::Value> = ranked
            .iter()
            .map(|r| {
                serde_json::json!({
                    "path": r.path,
                    "start": r.start,
                    "end": r.end,
                    "content": r.content,
                    "scores": {
                        "bm25": r.bm25,
                        "semantic": r.semantic,
                        "bm25_relative": r.bm25_relative,
                        "semantic_relative": r.semantic_relative,
                        "fused": r.fused,
                        "exact_identifier": r.exact_identifier,
                    },
                })
            })
            .collect();
        shared.queries.fetch_add(1, Ordering::Relaxed);
        Ok(serde_json::json!({
            "ok": true,
            "proto": PROTO,
            "model": model_id,
            "dim": DIM,
            "files": files,
            "chunks": chunks_n,
            "generation": generation,
            "results": results,
        }))
    })();
    match answer {
        Ok(v) => reply(&job.stream, &v),
        Err(e) => reply(&job.stream, &serde_json::json!({ "ok": false, "reason": format!("{e:#}") })),
    }
}

/// Parse + route one connection. Cheap control requests answer inline;
/// rank jobs go to the bounded pool, with explicit `busy` backpressure.
fn admit(shared: &Arc<Shared>, queue: &mpsc::SyncSender<Job>, stream: TcpStream) {
    let line = match read_capped_line(&stream, MAX_REQUEST_BYTES) {
        Ok(l) => l,
        Err(e) => {
            reply(&stream, &serde_json::json!({ "ok": false, "reason": format!("{e:#}") }));
            return;
        }
    };
    let req: serde_json::Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(_) => {
            reply(&stream, &serde_json::json!({ "ok": false, "reason": "bad json" }));
            return;
        }
    };
    if req.get("token").and_then(|v| v.as_str()) != Some(shared.token.as_str()) {
        reply(&stream, &serde_json::json!({ "ok": false, "reason": "bad token" }));
        return;
    }
    if req.get("ping").is_some() {
        reply(&stream, &serde_json::json!({ "ok": true, "proto": PROTO, "pid": std::process::id(), "chunking": super::chunks::chunking_name(shared.chunking) }));
        return;
    }
    if req.get("shutdown").is_some() {
        // Daemon-owned metadata removal: the daemon deletes its own
        // `daemon.json` (only if it still names this pid) BEFORE replying,
        // so clients never need a racy read-then-delete — the single
        // writer is the daemon itself. The client's owner-safe removal
        // stays as a backup for SIGKILLed daemons (whose stale file names
        // a dead pid and therefore never validates).
        let path = shared.slot.join("daemon.json");
        let cur: Option<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).ok();
        if cur.as_ref().and_then(|v| v.get("pid")).and_then(|v| v.as_u64())
            == Some(u64::from(std::process::id()))
        {
            let _ = std::fs::remove_file(&path);
        }
        reply(&stream, &serde_json::json!({ "ok": true, "proto": PROTO, "pid": std::process::id() }));
        eprintln!("wgrep-sem: shutting down on request");
        std::process::exit(0);
    }
    if req.get("stats").is_some() {
        let c = shared.corpus.read().unwrap();
        reply(
            &stream,
            &serde_json::json!({
                "ok": true, "proto": PROTO,
                "chunking": super::chunks::chunking_name(shared.chunking),
                "pid": std::process::id(),
                "files": c.file_count(), "chunks": c.chunk_count(),
                "skipped_large": c.skipped_large, "skipped_binary": c.skipped_binary,
                "generation": shared.generation.load(Ordering::Relaxed),
                "queries": shared.queries.load(Ordering::Relaxed),
                "rejected_busy": shared.rejected_busy.load(Ordering::Relaxed),
                "rejected_admit": shared.rejected_admit.load(Ordering::Relaxed),
                "uptime_s": shared.started.elapsed().as_secs(),
                "workers": shared.workers, "queue_cap": QUEUE_CAP,
                "admit_threads": ADMIT_THREADS, "conn_queue_cap": CONN_QUEUE_CAP,
                "model": format!("{}@{}", shared.model.pin.repo, &shared.model.pin.revision[..12]),
            }),
        );
        return;
    }
    let Some(r) = req.get("rank") else {
        reply(&stream, &serde_json::json!({ "ok": false, "reason": "bad request" }));
        return;
    };
    let query = r.get("query").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    if query.trim().is_empty() {
        reply(&stream, &serde_json::json!({ "ok": false, "reason": "empty query" }));
        return;
    }
    // MVP is code-model only (blocker 1): no per-request model switch, so
    // mixed embedding spaces are unrepresentable. `model` must be "auto"
    // or "code"; anything else is an explicit error, never a silent map.
    let model_req = r.get("model").and_then(|v| v.as_str()).unwrap_or("auto");
    if model_req != "auto" && model_req != "code" {
        reply(
            &stream,
            &serde_json::json!({ "ok": false, "reason": "MVP is code-model only; --model text unsupported" }),
        );
        return;
    }
    let job = Job {
        stream,
        query,
        prefix: r.get("prefix").and_then(|v| v.as_str()).unwrap_or(".").to_owned(),
        top_n: r.get("top_n").and_then(|v| v.as_u64()).unwrap_or(5).clamp(1, 100) as usize,
    };
    match queue.try_send(job) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(job)) => {
            shared.rejected_busy.fetch_add(1, Ordering::Relaxed);
            reply(&job.stream, &serde_json::json!({ "ok": false, "reason": "busy" }));
        }
        Err(mpsc::TrySendError::Disconnected(job)) => {
            reply(&job.stream, &serde_json::json!({ "ok": false, "reason": "shutting down" }));
        }
    }
}

/// Serve `root` in the foreground. Returns only on fatal error.
pub(crate) fn serve(root: &Path, cache: &Path, chunking: chunks::Chunking) -> anyhow::Result<()> {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(MAX_WORKERS).max(1);
    let slot = slots::slot_for(root, chunking)
        .ok_or_else(|| anyhow::anyhow!("wgrep-sem: no slot directory; set WSEM_DIR"))?;

    // Serve singleton: hold an exclusive OS advisory lock on `serve.lock`
    // for the daemon's WHOLE lifetime. A second `--serve` for the same
    // root (manual or racing auto-start) finds the lock held and refuses
    // instead of building a duplicate model+corpus and orphaning one.
    // Separate file from the client's `startup.lock`, so a starter polling
    // under its startup guard can never deadlock with the serving child.
    // The file is never unlinked; the OS releases the lock on death.
    let _serve_guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(slot.join("serve.lock"))?;
    match _serve_guard.try_lock() {
        Ok(()) => {}
        // Any lock error refuses the duplicate: a held lock (the common
        // case) and a broken lock file both fail safe toward one daemon.
        Err(_) => {
            anyhow::bail!("wgrep-sem: another daemon is already serving {}", root.display());
        }
    }

    // Watcher FIRST: events during the build queue up and the post-build
    // reconcile applies them, so build-time mutations are never lost.
    let (wtx, wrx) = mpsc::sync_channel::<Result<notify::Event, notify::Error>>(1024);
    let watcher = (|| -> anyhow::Result<notify::RecommendedWatcher> {
        use notify::{Config, RecursiveMode, Watcher};
        let mut w = notify::RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                // Bounded + coalesced: a burst drops (never blocks the
                // notify thread); the reconcile diffs fingerprints anyway.
                let _ = wtx.try_send(res);
            },
            Config::default(),
        )?;
        w.watch(root, RecursiveMode::Recursive)?;
        Ok(w)
    })();
    let _watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("wgrep-sem: watcher unavailable ({e:#}); reconcile-only mode");
            None
        }
    };

    eprintln!("wgrep-sem: loading code model...");
    let t0 = Instant::now();
    let cache_dir = model::cache_dir(Some(cache))?;
    let code = ModelCtx::load(&CODE, &cache_dir)?;
    eprintln!("wgrep-sem: code model loaded in {:.1}s", t0.elapsed().as_secs_f64());

    let shared = Arc::new(Shared {
        root: root.to_path_buf(),
        slot: slot.clone(),
        chunking,
        token: random_token(),
        model: code,
        corpus: RwLock::new(Corpus::empty(root.to_path_buf())),
        generation: AtomicU64::new(0),
        queries: AtomicU64::new(0),
        rejected_busy: AtomicU64::new(0),
        rejected_admit: AtomicU64::new(0),
        recon_mu: std::sync::Mutex::new(()),
        recon_again: std::sync::atomic::AtomicBool::new(false),
        workers,
        started: Instant::now(),
    });
    eprintln!("wgrep-sem: building corpus under {}...", root.display());
    let t1 = Instant::now();
    let n = shared.corpus.write().unwrap().build(&shared.model, chunking)?;
    shared.generation.fetch_add(1, Ordering::Relaxed);
    // Apply anything that changed during the build (watcher was already up).
    while wrx.try_recv().is_ok() {}
    reconcile(&shared);
    {
        let c = shared.corpus.read().unwrap();
        eprintln!(
            "wgrep-sem: indexed {} files, {} chunks in {:.1}s ({} large skipped, {} binary skipped)",
            c.file_count(),
            n,
            t1.elapsed().as_secs_f64(),
            c.skipped_large,
            c.skipped_binary
        );
    }

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let port = listener.local_addr()?.port();
    // Atomic metadata publish: a racing cold-start loser must never read
    // a torn daemon.json. Write tmp + rename under the slot lifecycle
    // lock held by our spawner (see client::ensure_serving).
    let meta = serde_json::json!({
        "port": port, "token": shared.token,
        "pid": std::process::id(),
        "root": slots::root_key(root),
        "chunking": chunks::chunking_name(chunking),
        "proto": PROTO,
    })
    .to_string();
    let tmp = slot.join(format!(".daemon.{}.tmp", std::process::id()));
    std::fs::write(&tmp, meta)?;
    std::fs::rename(&tmp, slot.join("daemon.json"))?;
    eprintln!(
        "wgrep-sem: serving {} on 127.0.0.1:{port} (pid {}, chunking {}, {workers} rank workers)",
        root.display(),
        std::process::id(), chunks::chunking_name(chunking)
    );

    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("wsem-watch".into())
            .spawn(move || watch_loop(wrx, shared))
            .map_err(|e| anyhow::anyhow!("wgrep-sem: watcher spawn failed: {e}"))?;
    }

    let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE_CAP);
    let rx = Arc::new(std::sync::Mutex::new(rx));
    for i in 0..workers {
        let shared = shared.clone();
        let rx = rx.clone();
        std::thread::Builder::new()
            .name(format!("wsem-rank-{i}"))
            .spawn(move || loop {
                let job = rx.lock().unwrap().recv();
                match job {
                    Ok(j) => serve_job(&shared, j),
                    Err(_) => break,
                }
            })
            .map_err(|e| anyhow::anyhow!("wgrep-sem: worker spawn failed: {e}"))?;
    }

    // Bounded admission: fixed admit threads drain a capped connection
    // queue inline (parse + route). Overflow closes the connection, counted
    // in stats — no unbounded thread spawn, no silent queue growth.
    // NOTE (correctness): the recv is bound in its own block so the queue
    // mutex releases BEFORE admit runs. `while let ... = lock().recv()`
    // would retain the guard through the admit body (match-scrutinee
    // temporaries live for the whole match), serializing all 4 threads
    // behind one slow connection.
    let (ctx, crx) = mpsc::sync_channel::<TcpStream>(CONN_QUEUE_CAP);
    let crx = Arc::new(std::sync::Mutex::new(crx));
    for _ in 0..ADMIT_THREADS {
        let shared = shared.clone();
        let tx = tx.clone();
        let crx = crx.clone();
        std::thread::Builder::new()
            .name("wsem-admit".into())
            .spawn(move || loop {
                let next = {
                    crx.lock().unwrap().recv()
                };
                match next {
                    Ok(s) => admit(&shared, &tx, s),
                    Err(_) => break,
                }
            })
            .map_err(|e| anyhow::anyhow!("wgrep-sem: admit spawn failed: {e}"))?;
    }
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(QUERY_SECS)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(QUERY_SECS)));
                let _ = s.set_nodelay(true);
                if ctx.try_send(s).is_err() {
                    // Admission saturated: close (client sees EOF → error),
                    // counted. TCP backlog absorbs only brief bursts.
                    shared.rejected_admit.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(e) => eprintln!("wgrep-sem: accept failed: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served_row(path: &str, start: usize, end: usize, content: &str, fp: Option<chunks::Fingerprint>) -> ServedRow {
        ServedRow {
            path: path.to_owned(),
            start,
            end,
            content: content.to_owned(),
            fp,
            bm25: 0.0,
            semantic: 0.0,
            bm25_relative: 0.0,
            semantic_relative: 0.0,
            fused: 0.0,
            exact_identifier: false,
        }
    }

    #[test]
    fn validate_rows_needs_no_corpus_lock() {
        // Structural regression: validation takes owned rows + root only.
        // A concurrent watcher publish (swap_remove) between rank and
        // validation therefore cannot panic OOB or swap passages — there
        // is no index to go stale. This exercises the owned-row path
        // against live source, including a racing mutation.
        let dir = std::env::temp_dir().join("wsem-validate-rows");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("f.txt");
        std::fs::write(&abs, "alpha\nbeta\ngamma\n").unwrap();
        let fp = chunks::fingerprint(&abs);
        let row = served_row("f.txt", 2, 3, "beta\ngamma\n", fp);
        assert_eq!(validate_rows(&dir, std::slice::from_ref(&row)), vec![true]);
        // Racing mutation between rank and validation marks stale (never
        // panics, never serves the old bytes as current).
        std::fs::write(&abs, "alpha\nBETA\ngamma\n").unwrap();
        assert_eq!(validate_rows(&dir, std::slice::from_ref(&row)), vec![false]);
        // Deleted source is stale, not an error.
        std::fs::remove_file(&abs).unwrap();
        assert_eq!(validate_rows(&dir, std::slice::from_ref(&row)), vec![false]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rank_then_mutate_snapshot_is_stable() {
        // Rank-then-mutate regression: two owned rows are snapshotted
        // (as rank_once would), then one source mutates before
        // validation. Exactly the mutated row must go stale — no panic,
        // no index reuse, and the surviving row keeps its own bytes.
        let dir = std::env::temp_dir().join("wsem-rank-mutate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "alpha\nbeta\n").unwrap();
        std::fs::write(&b, "gamma\ndelta\n").unwrap();
        let rows = vec![
            served_row("a.txt", 1, 2, "alpha\nbeta\n", chunks::fingerprint(&a)),
            served_row("b.txt", 1, 2, "gamma\ndelta\n", chunks::fingerprint(&b)),
        ];
        assert_eq!(validate_rows(&dir, &rows), vec![true, true]);
        // Mutation lands between rank and validation.
        std::fs::write(&b, "gamma\nDELTA\n").unwrap();
        let fresh = validate_rows(&dir, &rows);
        assert_eq!(fresh, vec![true, false]);
        // Serializing the survivors (what serve_job replies) yields only
        // the fresh row, with its own snapshotted content — never the
        // mutated bytes, never a swapped passage.
        let kept: Vec<&ServedRow> =
            rows.iter().zip(&fresh).filter(|(_, f)| **f).map(|(r, _)| r).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "a.txt");
        assert_eq!(kept[0].content, "alpha\nbeta\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rows_crlf_exact() {
        // CRLF-retained content validates against the same source bytes.
        let dir = std::env::temp_dir().join("wsem-validate-crlf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("f.txt");
        std::fs::write(&abs, b"a\r\nb\r\nc\r\n").unwrap();
        let fp = chunks::fingerprint(&abs);
        let row = served_row("f.txt", 1, 2, "a\r\nb\r\n", fp);
        assert_eq!(validate_rows(&dir, std::slice::from_ref(&row)), vec![true]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_batch_aggregates_relevance() {
        // An Access burst ending in a Modify must still wake a reconcile:
        // relevance is OR-ed over the whole drained batch, not just the
        // first event.
        let access: Result<notify::Event, notify::Error> =
            Ok(notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Any)));
        let modify: Result<notify::Event, notify::Error> =
            Ok(notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any)));
        assert!(!event_relevant(&access));
        assert!(event_relevant(&modify));
        let batch = [access, modify];
        assert!(batch.iter().fold(false, |acc, ev| acc | event_relevant(ev)));
        // Delivery errors force a reconcile (events may have been lost).
        let err: Result<notify::Event, notify::Error> =
            Err(notify::Error::generic("overflow"));
        assert!(event_relevant(&err));
    }
}
