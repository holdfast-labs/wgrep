//! Manual-start shared daemon and client for the resident cached sgrep core.
//!
//! This is the M4 immutable-snapshot prototype. One `--serve ROOT` process
//! owns one [`Searcher`] (the pinned model, the corpus passages, and the
//! cached passage vectors) and answers many loopback clients. Clients never
//! load the model, corpus, or vectors. The daemon reuses only the original
//! sgrep query logic ([`Searcher::query`]); there is no fallback to any other
//! engine, and a missing daemon is an explicit error, never a silent rebuild.
//!
//! Wire shape: one `\n`-terminated JSON request and one `\n`-terminated JSON
//! reply per loopback TCP connection. Frames are bounded in both directions
//! with whole-frame deadlines. Admission and ranking use bounded queues with
//! explicit `busy`/drop errors. A lifetime OS advisory lock refuses a
//! duplicate `--serve`; metadata is published atomically and carries a private
//! auth token. The `Searcher` is immutable after construction, so ranking jobs
//! share it lock-free and no mutex guard is held across job processing.
//!
//! This is a prototype, not a production default. There is no watcher and no
//! freshness: the snapshot is fixed at `--serve` time.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use clap::Parser;

use crate::{ModelChoice, RankMode, Result, Searcher};

/// Wire protocol version. Metadata, ping, status, query and shutdown carry it,
/// and the client refuses a daemon that speaks anything else.
pub const PROTO: u32 = 1;
/// Slot layout version. It is part of the slot path, so a future layout change
/// never reuses an old slot.
const SLOT_VERSION: &str = "v1";
/// The only embedding model this prototype serves. The metadata records it and
/// every request must agree with it, so mixed embedding spaces are
/// unrepresentable.
const MODEL: &str = "code";

/// Hard cap on one request line: queries are small text.
const MAX_REQUEST_BYTES: usize = 64 << 10;
/// Hard cap on one reply frame.
const MAX_REPLY_BYTES: usize = 256 << 20;
/// Whole-frame deadline for both the request line and the reply.
const FRAME_SECS: u64 = 120;
/// Connect budget for a loopback client.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Reply write deadline.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded ranking job queue. Overflow is an explicit `busy` error.
const WORKER_QUEUE_CAP: usize = 128;
/// Bounded admission threads and connection queue. Overflow closes the
/// connection and is counted, never an unbounded thread spawn.
const ADMIT_THREADS: usize = 4;
const CONN_QUEUE_CAP: usize = 64;
/// Cap on ranking workers.
const MAX_WORKERS: usize = 8;
/// Buffered fill size and per-fill ceiling inside the whole-frame budget.
const FILL_BUF: usize = 8192;
const PER_FILL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Slot layout, metadata, token
// ---------------------------------------------------------------------------

/// Store root: `WGREP_SEM_SHARED_DIR`, else the platform cache `wgrep/sem-shared`.
fn store_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WGREP_SEM_SHARED_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("wgrep").join("sem-shared"))
}

/// Stable key for a root: the lexical absolute path with redundant trailing
/// separators removed. Case folding and separator conversion are Windows-only;
/// on Unix filenames are case-sensitive and a backslash is an ordinary
/// character, so the key keeps both verbatim.
fn root_key(root: &Path) -> String {
    let key = root.to_string_lossy().into_owned();
    #[cfg(windows)]
    let key = key.replace('\\', "/");
    let key = key.trim_end_matches('/').to_string();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// Absolute path with `.`/`..` removed lexically (no symlink resolution). A
/// different spelling of the same directory simply misses the slot.
fn lexical_absolute(path: &Path, cwd: &Path) -> PathBuf {
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

fn hash_hex(root: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root_key(root).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    for b in SLOT_VERSION.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn slot_path(root: &Path) -> Option<PathBuf> {
    Some(store_dir()?.join(SLOT_VERSION).join(hash_hex(root)))
}

/// Slot dir for `root`, created on demand with the collision guard written. The
/// guard records the root key, model and protocol, so a hash collision with a
/// different root refuses instead of sharing one daemon.
fn slot_for(root: &Path) -> Option<PathBuf> {
    let slot = slot_path(root)?;
    std::fs::create_dir_all(&slot).ok()?;
    let guard = slot.join("root");
    let key = format!("{}\n{}\n{}", root_key(root), MODEL, PROTO);
    match std::fs::read_to_string(&guard) {
        Ok(prev) if prev == key => {}
        Ok(_) => return None,
        Err(_) => {
            std::fs::write(&guard, key).ok()?;
        }
    }
    Some(slot)
}

/// The slot and metadata for a live daemon serving `root`, if the metadata
/// matches this root, model and protocol. Reads only; never creates a slot.
fn slot_read(root: &Path) -> Option<(PathBuf, serde_json::Value)> {
    let slot = slot_path(root)?;
    let info: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(slot.join("daemon.json")).ok()?).ok()?;
    if info.get("root").and_then(|v| v.as_str()) != Some(root_key(root).as_str()) {
        return None;
    }
    if info.get("model").and_then(|v| v.as_str()) != Some(MODEL) {
        return None;
    }
    if info.get("proto").and_then(|v| v.as_u64()) != Some(u64::from(PROTO)) {
        return None;
    }
    if info.get("version").and_then(|v| v.as_str()) != Some(SLOT_VERSION) {
        return None;
    }
    Some((slot, info))
}

/// Publish metadata atomically (write a pid-named temp file, then rename).
fn publish_metadata(slot: &Path, info: &serde_json::Value) -> Result<()> {
    let tmp = slot.join(format!(".daemon.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(info)?)?;
    restrict(&tmp);
    std::fs::rename(&tmp, slot.join("daemon.json"))?;
    Ok(())
}

/// Best-effort private permissions for the token-bearing metadata file.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Remove `daemon.json` only when it still names `pid`; never a successor's.
fn remove_metadata_if_pid(slot: &Path, pid: u64) {
    let path = slot.join("daemon.json");
    let cur: Option<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).ok();
    if cur
        .as_ref()
        .and_then(|v| v.get("pid"))
        .and_then(|v| v.as_u64())
        == Some(pid)
    {
        let _ = std::fs::remove_file(&path);
    }
}

/// A per-serve private auth token. `RandomState` is seeded from the OS, and the
/// token mixes it with the pid and the start time.
fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    let state = RandomState::new();
    let mut h = state.build_hasher();
    std::process::id().hash(&mut h);
    Instant::now().hash(&mut h);
    let a = h.finish();
    let mut h2 = state.build_hasher();
    a.hash(&mut h2);
    (&state as *const RandomState as usize).hash(&mut h2);
    format!("{a:016x}{:016x}", h2.finish())
}

// ---------------------------------------------------------------------------
// Bounded frames
// ---------------------------------------------------------------------------

/// Read one `\n`-terminated frame, capped at `cap` bytes and bounded WHOLE by
/// `whole`. Buffered fills (8 KiB) keep a max-size frame to a few syscalls; the
/// socket timeout is recomputed per fill as `min(PER_FILL, remaining)`, so a
/// 1-byte trickle cannot stretch a frame past the budget.
fn read_frame(stream: &TcpStream, cap: usize, whole: Duration) -> Result<Vec<u8>> {
    let end = Instant::now() + whole;
    let mut reader = BufReader::with_capacity(FILL_BUF, stream);
    let mut out = Vec::with_capacity(1024.min(cap));
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("wgrep-sem-shared: frame deadline exceeded".into());
        }
        stream.set_read_timeout(Some(remaining.min(PER_FILL)))?;
        let n = {
            let chunk = reader
                .fill_buf()
                .map_err(|e| format!("wgrep-sem-shared: read failed: {e}"))?;
            if chunk.is_empty() {
                return Err("wgrep-sem-shared: connection closed mid-frame".into());
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if out.len() + i + 1 > cap + 1 {
                        return Err(format!("wgrep-sem-shared: frame exceeds {cap} bytes").into());
                    }
                    out.extend_from_slice(&chunk[..=i]);
                    let n = i + 1;
                    reader.consume(n);
                    return Ok(out);
                }
                None => {
                    if out.len() + chunk.len() > cap {
                        return Err(format!("wgrep-sem-shared: frame exceeds {cap} bytes").into());
                    }
                    out.extend_from_slice(chunk);
                    chunk.len()
                }
            }
        };
        reader.consume(n);
    }
}

// ---------------------------------------------------------------------------
// Reply helpers
// ---------------------------------------------------------------------------

fn bad(reason: &str) -> serde_json::Value {
    serde_json::json!({ "ok": false, "reason": reason })
}

fn ok_with(extra: serde_json::Value) -> serde_json::Value {
    let mut base = serde_json::json!({
        "ok": true, "proto": PROTO, "model": MODEL, "pid": std::process::id(),
    });
    if let (Some(dst), Some(src)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    base
}

/// Write one bounded JSON reply frame.
fn reply(stream: &TcpStream, value: &serde_json::Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_REPLY_BYTES {
        bytes = serde_json::to_vec(&bad("reply too large"))?;
    }
    bytes.push(b'\n');
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut writer = stream;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

struct Server {
    searcher: Arc<Searcher>,
    slot: PathBuf,
    token: String,
    port: u16,
    workers: usize,
    /// Digest of the immutable passage vectors, fixed at serve time. The status
    /// reply exposes it so a client can prove the vectors did not change while
    /// queries ran.
    snapshot: String,
    queries: AtomicU64,
    rejected_busy: AtomicU64,
    rejected_admit: AtomicU64,
    started: Instant,
}

/// SHA-256 over the length-prefixed, little-endian bytes of every stored
/// passage vector, in chunk order.
fn vectors_sha256(searcher: &Searcher) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update((searcher.passage_vectors().len() as u64).to_le_bytes());
    for vector in searcher.passage_vectors() {
        hasher.update((vector.len() as u64).to_le_bytes());
        for value in vector {
            hasher.update(value.to_le_bytes());
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn status_reply(server: &Arc<Server>) -> serde_json::Value {
    ok_with(serde_json::json!({
        "chunks": server.searcher.corpus().len(),
        "vectors": server.searcher.passage_vectors().len(),
        "encoded_texts": server.searcher.model().encoded_texts(),
        "vectors_sha256": server.snapshot,
        "queries": server.queries.load(Ordering::Relaxed),
        "rejected_busy": server.rejected_busy.load(Ordering::Relaxed),
        "rejected_admit": server.rejected_admit.load(Ordering::Relaxed),
        "port": server.port,
        "uptime_s": server.started.elapsed().as_secs(),
        "workers": server.workers,
        "queue_cap": WORKER_QUEUE_CAP,
        "admit_threads": ADMIT_THREADS,
        "conn_queue_cap": CONN_QUEUE_CAP,
    }))
}

struct Job {
    stream: TcpStream,
    query: String,
    top_n: usize,
    explain: bool,
}

/// Parse and route one connection. Cheap control requests answer inline;
/// ranking jobs go to the bounded worker pool with an explicit `busy` error.
fn admit(server: &Arc<Server>, queue: &mpsc::SyncSender<Job>, stream: TcpStream) {
    let line = match read_frame(&stream, MAX_REQUEST_BYTES, Duration::from_secs(FRAME_SECS)) {
        Ok(line) => line,
        Err(e) => {
            let _ = reply(&stream, &bad(&format!("{e:#}")));
            return;
        }
    };
    let req: serde_json::Value = match serde_json::from_slice(&line) {
        Ok(v) => v,
        Err(_) => {
            let _ = reply(&stream, &bad("bad json"));
            return;
        }
    };
    if req.get("token").and_then(|v| v.as_str()) != Some(server.token.as_str()) {
        let _ = reply(&stream, &bad("bad token"));
        return;
    }
    match req.get("op").and_then(|v| v.as_str()) {
        Some("ping") => {
            let _ = reply(&stream, &ok_with(serde_json::json!({})));
        }
        Some("status") => {
            let _ = reply(&stream, &status_reply(server));
        }
        Some("shutdown") => {
            // The daemon owns its metadata: remove it before replying, so a
            // client never has to read-then-delete under a race.
            remove_metadata_if_pid(&server.slot, u64::from(std::process::id()));
            let _ = reply(&stream, &ok_with(serde_json::json!({})));
            eprintln!("wgrep-sem-shared: shutting down on request");
            std::process::exit(0);
        }
        Some("query") => {
            let query = req
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if query.trim().is_empty() {
                let _ = reply(&stream, &bad("empty query"));
                return;
            }
            let model = req.get("model").and_then(|v| v.as_str()).unwrap_or(MODEL);
            if model != MODEL {
                let _ = reply(&stream, &bad("shared daemon is code-model only"));
                return;
            }
            let top_n = req
                .get("top_n")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .clamp(1, 100) as usize;
            let explain = req
                .get("explain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let job = Job {
                stream,
                query,
                top_n,
                explain,
            };
            match queue.try_send(job) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(job)) => {
                    server.rejected_busy.fetch_add(1, Ordering::Relaxed);
                    let _ = reply(&job.stream, &bad("busy"));
                }
                Err(mpsc::TrySendError::Disconnected(job)) => {
                    let _ = reply(&job.stream, &bad("shutting down"));
                }
            }
        }
        _ => {
            let _ = reply(&stream, &bad("bad request"));
        }
    }
}

/// Rank one job with the original query logic and write the reply. The
/// `Searcher` is owned by the `Arc`; no lock is held here.
fn serve_job(server: &Arc<Server>, job: Job) {
    let answer: Result<serde_json::Value> = (|| {
        // This prototype keeps the original sgrep ranking.
        let results = server
            .searcher
            .query_with_mode(&job.query, job.top_n, job.explain, RankMode::Classic, false)?;
        server.queries.fetch_add(1, Ordering::Relaxed);
        Ok(ok_with(
            serde_json::json!({ "results": serde_json::to_value(&results)? }),
        ))
    })();
    match answer {
        Ok(value) => {
            let _ = reply(&job.stream, &value);
        }
        Err(e) => {
            let _ = reply(&job.stream, &bad(&format!("{e:#}")));
        }
    }
}

/// Serve `root` in the foreground with one resident searcher. Returns only on a
/// fatal bind error. Manual start only: this never spawns itself.
pub fn serve(root: &Path, cache_dir: Option<&Path>) -> Result<()> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()).into());
    }
    let slot = slot_for(root).ok_or(
        "wgrep-sem-shared: slot hash collision, or no store directory (set WGREP_SEM_SHARED_DIR)",
    )?;

    // Lifetime serve singleton: hold a try-lock for the daemon's whole life, so
    // a duplicate `--serve` is refused instead of doubling the resident model
    // and vectors. The OS releases the lock on death; the file is never unlinked.
    let _serve_guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(slot.join("serve.lock"))?;
    if File::try_lock(&_serve_guard).is_err() {
        return Err(format!(
            "wgrep-sem-shared: another daemon is already serving {}",
            root.display()
        )
        .into());
    }

    eprintln!(
        "wgrep-sem-shared: loading the code model and building {} (this can take a while)...",
        root.display()
    );
    let t0 = Instant::now();
    let searcher = Arc::new(Searcher::open(root, ModelChoice::Code, cache_dir)?);
    eprintln!(
        "wgrep-sem-shared: ready: {} chunks, {} vectors, {} encoded texts in {:.1}s",
        searcher.corpus().len(),
        searcher.passage_vectors().len(),
        searcher.model().encoded_texts(),
        t0.elapsed().as_secs_f64()
    );

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(MAX_WORKERS)
        .max(1);
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let port = listener.local_addr()?.port();

    let snapshot = vectors_sha256(&searcher);
    let server = Arc::new(Server {
        searcher,
        slot: slot.clone(),
        token: random_token(),
        port,
        workers,
        snapshot,
        queries: AtomicU64::new(0),
        rejected_busy: AtomicU64::new(0),
        rejected_admit: AtomicU64::new(0),
        started: Instant::now(),
    });

    // Atomic metadata publish: a racing reader never sees a torn file.
    let info = serde_json::json!({
        "port": port,
        "token": server.token,
        "pid": std::process::id(),
        "root": root_key(root),
        "model": MODEL,
        "proto": PROTO,
        "version": SLOT_VERSION,
    });
    publish_metadata(&slot, &info)?;

    // Bounded ranking workers. The receiver mutex is released before the job
    // runs: the `recv` result is bound in its own block, so no guard is held
    // across processing and workers do not serialize behind a slow job.
    let (tx, rx) = mpsc::sync_channel::<Job>(WORKER_QUEUE_CAP);
    let rx = Arc::new(std::sync::Mutex::new(rx));
    for i in 0..workers {
        let server = server.clone();
        let rx = rx.clone();
        std::thread::Builder::new()
            .name(format!("wsem-shared-rank-{i}"))
            .spawn(move || {
                loop {
                    let next = { rx.lock().unwrap().recv() };
                    match next {
                        Ok(job) => serve_job(&server, job),
                        Err(_) => break,
                    }
                }
            })?;
    }

    // Bounded admission: fixed threads drain a capped connection queue inline.
    // Overflow closes the connection and is counted.
    let (ctx, crx) = mpsc::sync_channel::<TcpStream>(CONN_QUEUE_CAP);
    let crx = Arc::new(std::sync::Mutex::new(crx));
    for _ in 0..ADMIT_THREADS {
        let server = server.clone();
        let tx = tx.clone();
        let crx = crx.clone();
        std::thread::Builder::new()
            .name("wsem-shared-admit".into())
            .spawn(move || {
                loop {
                    let next = { crx.lock().unwrap().recv() };
                    match next {
                        Ok(stream) => admit(&server, &tx, stream),
                        Err(_) => break,
                    }
                }
            })?;
    }

    eprintln!(
        "wgrep-sem-shared: serving {} on 127.0.0.1:{port} (pid {}, {workers} rank workers)",
        root.display(),
        std::process::id()
    );

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(FRAME_SECS)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(FRAME_SECS)));
                let _ = s.set_nodelay(true);
                if ctx.try_send(s).is_err() {
                    server.rejected_admit.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(e) => eprintln!("wgrep-sem-shared: accept failed: {e:#}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

fn connect(info: &serde_json::Value) -> Option<(TcpStream, String)> {
    let port = u16::try_from(info.get("port")?.as_u64()?).ok()?;
    let token = info.get("token")?.as_str()?.to_string();
    let stream =
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), CONNECT_TIMEOUT)
            .ok()?;
    let _ = stream.set_nodelay(true);
    Some((stream, token))
}

/// One bounded request/reply round trip. `None` on any I/O, framing, or parse
/// failure, never an unbounded read.
fn roundtrip(info: &serde_json::Value, msg: serde_json::Value) -> Option<serde_json::Value> {
    let (mut stream, token) = connect(info)?;
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut envelope = serde_json::Map::new();
    envelope.insert("token".to_owned(), serde_json::Value::String(token));
    if let Some(obj) = msg.as_object() {
        for (k, v) in obj {
            envelope.insert(k.clone(), v.clone());
        }
    }
    let mut text = serde_json::Value::Object(envelope).to_string();
    text.push('\n');
    if text.len() > MAX_REQUEST_BYTES {
        return None;
    }
    stream.write_all(text.as_bytes()).ok()?;
    let line = read_frame(&stream, MAX_REPLY_BYTES, Duration::from_secs(FRAME_SECS)).ok()?;
    serde_json::from_slice(&line).ok()
}

/// A reply is trusted only when it echoes the slot's pid and the model we
/// speak. A stale metadata file, a recycled pid on the old port, or a
/// half-written publish never validates.
fn trusted(info: &serde_json::Value, reply: &serde_json::Value) -> Option<u64> {
    if reply.get("ok")?.as_bool()? {
        let pid = reply.get("pid")?.as_u64()?;
        let proto = reply.get("proto")?.as_u64()? as u32;
        if Some(pid) == info.get("pid")?.as_u64()
            && proto == PROTO
            && reply.get("model")?.as_str()? == MODEL
            && info.get("model")?.as_str()? == MODEL
        {
            return Some(pid);
        }
    }
    None
}

/// Daemon pid if a live, identity-validated daemon answers for `root`.
pub fn ping(root: &Path) -> Option<u64> {
    let (_, info) = slot_read(root)?;
    let reply = roundtrip(&info, serde_json::json!({ "op": "ping" }))?;
    trusted(&info, &reply)
}

/// Public status of the daemon serving `root`, if a trusted one answers.
pub fn status(root: &Path) -> Option<serde_json::Value> {
    let (_, info) = slot_read(root)?;
    let reply = roundtrip(&info, serde_json::json!({ "op": "status" }))?;
    trusted(&info, &reply)?;
    Some(reply)
}

/// Stop the daemon serving `root`. `None` when no trusted daemon answers.
pub fn shutdown(root: &Path) -> Option<u64> {
    let (slot, info) = slot_read(root)?;
    let reply = roundtrip(&info, serde_json::json!({ "op": "shutdown" }))?;
    let pid = trusted(&info, &reply)?;
    // Backup cleanup for a daemon that died without removing its own metadata.
    remove_metadata_if_pid(&slot, pid);
    Some(pid)
}

/// Rank `query` on the daemon serving `root`. Never falls back: a missing or
/// untrusted daemon is an explicit error.
pub fn query(root: &Path, query: &str, top_n: usize, explain: bool) -> Result<serde_json::Value> {
    let (_, info) = slot_read(root).ok_or(
        "no daemon is serving this root; start one with --serve ROOT (this prototype has no auto-start)",
    )?;
    let reply = roundtrip(
        &info,
        serde_json::json!({
            "op": "query", "query": query, "model": MODEL,
            "top_n": top_n, "explain": explain,
        }),
    )
    .ok_or("the daemon did not answer")?;
    trusted(&info, &reply).ok_or("the daemon identity did not match the slot metadata")?;
    if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let reason = reply
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(format!("daemon error: {reason}").into());
    }
    Ok(reply)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "wgrep-sem-shared",
    version,
    about = "Manual-start shared daemon and client for the resident cached sgrep core."
)]
struct Cli {
    /// Start a shared daemon for ROOT in the foreground.
    #[arg(long, value_name = "ROOT")]
    serve: Option<PathBuf>,
    /// Print the public status of the daemon serving ROOT.
    #[arg(long, value_name = "ROOT")]
    status: Option<PathBuf>,
    /// Stop the daemon serving ROOT.
    #[arg(long, value_name = "ROOT")]
    stop: Option<PathBuf>,
    /// Read model and parser assets from DIR (default: platform caches).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Embedding model. Only `code` is supported.
    #[arg(long, value_enum, default_value_t = ModelChoice::Code)]
    model: ModelChoice,
    /// Emit the exact JSON array the sgrep CLI emits.
    #[arg(long)]
    json: bool,
    /// Include lexical, semantic and fused scores in JSON output.
    #[arg(long, requires = "json")]
    explain: bool,
    #[arg(short = 'n', default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    count: u32,
    /// Query text.
    query: Option<String>,
    /// Corpus root served by the daemon.
    root: Option<PathBuf>,
}

fn resolve_root(arg: &Path) -> Result<PathBuf> {
    let abs = lexical_absolute(arg, &std::env::current_dir()?);
    if !abs.is_dir() {
        return Err(format!("not a directory: {}", abs.display()).into());
    }
    Ok(abs)
}

/// Management-path normalization: resolve the slot WITHOUT requiring the root
/// to still exist, so a deleted tree's daemon can still be found and stopped.
fn resolve_mgmt(arg: &Path) -> Result<PathBuf> {
    Ok(lexical_absolute(arg, &std::env::current_dir()?))
}

fn render_status(value: &serde_json::Value) -> String {
    let field = |key: &str| match value.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "?".to_string(),
    };
    format!(
        "pid={} model={} proto={} chunks={} vectors={} encoded_texts={} vectors_sha256={} queries={} \
rejected_busy={} rejected_admit={} port={} uptime_s={}",
        field("pid"),
        field("model"),
        field("proto"),
        field("chunks"),
        field("vectors"),
        field("encoded_texts"),
        field("vectors_sha256"),
        field("queries"),
        field("rejected_busy"),
        field("rejected_admit"),
        field("port"),
        field("uptime_s"),
    )
}

fn print_results(reply: &serde_json::Value, json: bool) -> Result<bool> {
    let results: Vec<serde_json::Value> = reply
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    if json {
        writeln!(out, "{}", serde_json::Value::Array(results.clone()))?;
    } else {
        for row in &results {
            let path = row.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let start = row.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
            let end = row.get("end").and_then(|v| v.as_u64()).unwrap_or(0);
            let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
            writeln!(out, "{path}:{start}-{end}\n{}\n", content.trim_end())?;
        }
    }
    out.flush()?;
    Ok(!results.is_empty())
}

fn run(cli: &Cli) -> Result<std::process::ExitCode> {
    use std::process::ExitCode;

    let management = [
        cli.serve.is_some(),
        cli.status.is_some(),
        cli.stop.is_some(),
    ]
    .iter()
    .filter(|present| **present)
    .count();
    if management > 1 {
        return Err("use only one of --serve, --status, --stop".into());
    }

    if let Some(root) = &cli.serve {
        if cli.query.is_some() || cli.root.is_some() {
            return Err("--serve takes ROOT; do not also pass a query".into());
        }
        let root = resolve_root(root)?;
        serve(&root, cli.cache_dir.as_deref())?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(root) = &cli.status {
        let root = resolve_mgmt(root)?;
        return match status(&root) {
            Some(value) => {
                println!("{}", render_status(&value));
                Ok(ExitCode::SUCCESS)
            }
            None => Err(format!("no daemon is serving {}", root.display()).into()),
        };
    }
    if let Some(root) = &cli.stop {
        let root = resolve_mgmt(root)?;
        return match shutdown(&root) {
            Some(pid) => {
                println!(
                    "wgrep-sem-shared: stopped the daemon for {} (pid {pid})",
                    root.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            None => Err(format!("no daemon is serving {}", root.display()).into()),
        };
    }

    let text = cli.query.clone().ok_or("QUERY is required")?;
    if text.trim().is_empty() {
        return Err("query has no searchable words".into());
    }
    if cli.model != ModelChoice::Code {
        return Err("the shared daemon is code-model only".into());
    }
    let root = cli.root.clone().ok_or("ROOT is required")?;
    let root = resolve_root(&root)?;
    let reply = query(&root, &text, cli.count as usize, cli.explain)?;
    let nonempty = print_results(&reply, cli.json)?;
    Ok(ExitCode::from(if nonempty { 0 } else { 1 }))
}

pub fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("wgrep-sem-shared: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        (client, server)
    }

    #[test]
    fn root_key_trims_trailing_separators() {
        let key = root_key(Path::new("/tmp/project/"));
        assert!(key.ends_with("project"), "{key}");
    }

    #[test]
    fn lexical_absolute_removes_dot_and_dotdot() {
        let cwd = Path::new("/tmp/work");
        assert_eq!(
            lexical_absolute(Path::new("./a/../b"), cwd),
            PathBuf::from("/tmp/work/b")
        );
    }

    #[test]
    fn frame_round_trips_exact_bytes() {
        let (mut client, server) = loopback();
        client.write_all(b"{\"op\":\"ping\"}\n").unwrap();
        let got = read_frame(&server, MAX_REQUEST_BYTES, Duration::from_secs(10)).unwrap();
        assert_eq!(got, b"{\"op\":\"ping\"}\n");
    }

    #[test]
    fn frame_rejects_oversize_and_short_close() {
        let (mut client, server) = loopback();
        client.write_all(&vec![b'x'; 100]).unwrap();
        client.write_all(b"\n").unwrap();
        assert!(read_frame(&server, 10, Duration::from_secs(10)).is_err());

        let (client, server) = loopback();
        drop(client);
        assert!(read_frame(&server, MAX_REQUEST_BYTES, Duration::from_secs(10)).is_err());
    }

    #[test]
    fn token_is_hex_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn status_render_carries_the_public_fields() {
        let value = ok_with(serde_json::json!({
            "chunks": 15, "vectors": 15, "encoded_texts": 27, "queries": 12,
            "rejected_busy": 0, "rejected_admit": 0, "port": 5000, "uptime_s": 3,
        }));
        let line = render_status(&value);
        for needle in [
            "chunks=15",
            "vectors=15",
            "encoded_texts=27",
            "queries=12",
            "pid=",
            "model=code",
        ] {
            assert!(line.contains(needle), "missing {needle} in {line}");
        }
    }
}
