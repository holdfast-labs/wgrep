//! One authenticated, multi-root semantic service for the `wg` front door.
//! The daemon owns one shared model pool; each root has one live index.
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use wgrep::live::{BuildProgress, LiveIndex, ModelPool};
use wgrep::{ModelChoice, RankMode, Result};
use wisp_grep::text::{TextGroup, TextIndex, TextPlan};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const PROTO: u32 = 1;
const MAX_REQUEST: usize = 16 * 1024 * 1024;
const MAX_REPLY: usize = 16 * 1024 * 1024;
const MAX_ROOTS: usize = 32;
const QUEUE: usize = 64;
const CONNS: usize = 32;
const FRAME_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_EVENT_PATHS: usize = 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
struct RootStatus {
    path: PathBuf,
    ready: bool,
    pending: usize,
    generation: u64,
    files: usize,
    chunks: usize,
    error: Option<String>,
    text: Option<serde_json::Value>,
    text_error: Option<String>,
    text_build_ms: u64,
    build_ms: u64,
    model_ms: u64,
}
struct Root {
    index: Arc<LiveIndex>,
    pending: Arc<AtomicUsize>,
    lost: Arc<AtomicBool>,
    model: ModelChoice,
    dirty: bool,
    error: Option<String>,
    text: Option<Arc<TextIndex>>,
    text_error: Option<String>,
    text_build_ms: u64,
    build_ms: u64,
    model_ms: u64,
}
struct State {
    roots: HashMap<PathBuf, Root>,
}

/// One in-flight root build. `up` starts it and returns at once; the client
/// polls `status`, so no single request must outlive the frame deadline.
struct BuildState {
    path: PathBuf,
    started: Instant,
    progress: Arc<BuildProgress>,
    done: AtomicBool,
    error: Mutex<Option<String>>,
}

impl BuildState {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            started: Instant::now(),
            progress: Arc::new(BuildProgress::new()),
            done: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn status(&self) -> serde_json::Value {
        let mut value = self.progress.to_json();
        let error = self.error.lock().unwrap().clone();
        value["path"] = serde_json::json!(self.path);
        value["ready"] = serde_json::json!(false);
        value["building"] = serde_json::json!(!self.done.load(Ordering::Acquire) && error.is_none());
        value["pending"] = serde_json::json!(0);
        value["generation"] = serde_json::json!(0);
        value["files"] = serde_json::json!(0);
        value["elapsed_ms"] = serde_json::json!(self.started.elapsed().as_millis() as u64);
        value["error"] = serde_json::json!(error);
        value
    }
}

struct Service {
    state: Mutex<State>,
    building: Mutex<HashMap<PathBuf, Arc<BuildState>>>,
    admission: Mutex<()>,
    models: Arc<ModelPool>,
    token: String,
    started: Instant,
    shutdown: AtomicBool,
}

fn cache_root() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("wgrep")
        .join("wg-service")
}
fn store_dir() -> PathBuf {
    std::env::var_os("WG_SERVICE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(cache_root)
}
fn metadata() -> PathBuf {
    store_dir().join("service.json")
}
fn lock_path() -> PathBuf {
    store_dir().join("service.lock")
}
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let s = std::collections::hash_map::RandomState::new();
    format!(
        "{:016x}{:016x}",
        s.build_hasher().finish(),
        std::process::id()
    )
}
fn private(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600));
    }
}
fn root_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Stable per-root snapshot name. The header also checks the real model
/// revision, so a changed model can never read another model's vectors.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn snapshot_cache_path(root: &Path, choice: ModelChoice) -> PathBuf {
    let directory = store_dir().join("roots");
    let _ = std::fs::create_dir_all(&directory);
    let model = match choice {
        ModelChoice::Auto => "auto",
        ModelChoice::Code => "code",
        ModelChoice::Text => "text",
    };
    directory.join(format!(
        "{:016x}-{model}.wgsnap",
        fnv1a64(root.as_os_str().as_encoded_bytes())
    ))
}
fn read_only(event: &notify::Event) -> bool {
    !event.need_rescan()
        && matches!(event.kind, notify::EventKind::Access(_))
        && !matches!(
            event.kind,
            notify::EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write
            ))
        )
}
fn refresh_batch(
    index: &LiveIndex,
    paths: &[PathBuf],
    mut reconcile: bool,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match index.refresh(paths, reconcile) {
            Ok(_) => return Ok(()),
            Err(error) => {
                let message = error.to_string();
                let raced = message.starts_with("source changed while")
                    || message.starts_with("source membership changed while")
                    || message.contains("requires reconcile=true");
                let temporarily_locked = error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                    )
                });
                if (!raced && !temporarily_locked) || Instant::now() >= deadline {
                    return Err(message);
                }
                reconcile = true;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
/// Test-only fault injection: a build fails while the marker file named by
/// `WG_TEST_FAIL_BUILD` exists. Tests use it to prove that one `up` retries a
/// failed build. Normal use never sets the variable.
#[cfg(debug_assertions)]
fn injected_build_failure() -> Result<()> {
    let Some(marker) = std::env::var_os("WG_TEST_FAIL_BUILD") else {
        return Ok(());
    };
    if Path::new(&marker).exists() {
        let tag = std::fs::read_to_string(&marker).unwrap_or_default();
        return Err(format!("test-only build failure: {}", tag.trim()).into());
    }
    Ok(())
}
#[cfg(not(debug_assertions))]
fn injected_build_failure() -> Result<()> {
    Ok(())
}
/// Display form of a path. On Windows it drops the verbatim prefix. Never use
/// the result to open a file.
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    #[cfg(windows)]
    {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return rest.to_owned();
        }
    }
    text
}
fn model_name(choice: ModelChoice) -> &'static str {
    match choice {
        ModelChoice::Auto => "auto",
        ModelChoice::Code => "code",
        ModelChoice::Text => "text",
    }
}
fn attached_model_name(revision: &str) -> &'static str {
    if revision == wgrep::CODE.revision {
        "code"
    } else if revision == wgrep::TEXT.revision {
        "text"
    } else {
        "unknown"
    }
}
const NO_SERVICE: &str = "no wg service is running for this cache; run wg up first";
/// True when the error means that no daemon owns this cache: the metadata file
/// is absent or the recorded listener refuses the connection.
fn daemon_unavailable(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
        )
    })
}
fn read_frame(stream: &TcpStream, cap: usize) -> Result<Vec<u8>> {
    read_frame_timeout(stream, cap, FRAME_TIMEOUT)
}
fn read_frame_timeout(stream: &TcpStream, cap: usize, whole: Duration) -> Result<Vec<u8>> {
    let mut r = BufReader::new(stream);
    let mut out = Vec::new();
    let end = Instant::now() + whole;
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("service frame deadline exceeded".into());
        }
        stream.set_read_timeout(Some(remaining.min(Duration::from_secs(2))))?;
        let chunk = match r.fill_buf() {
            Ok(chunk) => chunk,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if chunk.is_empty() {
            return Err("connection closed mid-frame".into());
        }
        let count = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if out.len() + i + 1 > cap {
                    return Err("oversized service frame".into());
                }
                out.extend_from_slice(&chunk[..=i]);
                let count = i + 1;
                r.consume(count);
                return Ok(out);
            }
            None => {
                if out.len() + chunk.len() > cap {
                    return Err("oversized service frame".into());
                }
                out.extend_from_slice(chunk);
                chunk.len()
            }
        };
        r.consume(count);
    }
}
fn send(stream: &TcpStream, value: &serde_json::Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_REPLY {
        bytes = br#"{"ok":false,"error":"reply too large"}"#.to_vec();
    }
    bytes.push(b'\n');
    write_frame(stream, &bytes, Duration::from_secs(30))
}
fn write_frame(stream: &TcpStream, bytes: &[u8], whole: Duration) -> Result<()> {
    let end = Instant::now() + whole;
    let mut sent = 0;
    let mut writer = stream;
    while sent < bytes.len() {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("service frame write deadline exceeded".into());
        }
        stream.set_write_timeout(Some(remaining.min(Duration::from_secs(2))))?;
        match writer.write(&bytes[sent..]) {
            Ok(0) => return Err("service connection closed during frame write".into()),
            Ok(n) => sent += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn publish(port: u16, token: &str) -> Result<()> {
    let path = metadata();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::json!({"proto":PROTO,"pid":std::process::id(),"port":port,"token":token})
            .to_string(),
    )?;
    private(&tmp);
    std::fs::rename(tmp, path)?;
    Ok(())
}
fn load_metadata() -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(metadata())?)?;
    if v["proto"].as_u64() != Some(PROTO as u64) {
        return Err("wg service protocol mismatch".into());
    }
    Ok(v)
}
fn request(v: serde_json::Value) -> Result<serde_json::Value> {
    request_timeout(v, FRAME_TIMEOUT)
}
fn request_timeout(v: serde_json::Value, whole: Duration) -> Result<serde_json::Value> {
    let info = load_metadata()?;
    let port = info["port"].as_u64().ok_or("bad service metadata")? as u16;
    let s = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )?;
    s.set_nodelay(true)?;
    let mut q = v.as_object().cloned().ok_or("bad request")?;
    q.insert("token".into(), info["token"].clone());
    let mut b = serde_json::to_vec(&q)?;
    if b.len() > MAX_REQUEST {
        return Err("request too large".into());
    }
    b.push(b'\n');
    write_frame(&s, &b, whole)?;
    let line = read_frame_timeout(&s, MAX_REPLY, whole)?;
    let r: serde_json::Value = serde_json::from_slice(&line)?;
    if r["ok"] != true {
        return Err(r["error"]
            .as_str()
            .or_else(|| r["fallback"].as_str())
            .unwrap_or("service request failed")
            .into());
    }
    Ok(r)
}

/// Status for one text index. The engine reports `files` as the indexed
/// postings only, and `added` as walked files searched directly. `pending`
/// means a rebuild will still try those files; `unindexed` means the builder
/// cannot index them (UTF-16 BOM, high-diversity, unreadable) and they stay
/// candidates. Report the total the text route knows.
fn text_status(text: &Arc<TextIndex>) -> serde_json::Value {
    decorate_text_status(text.status())
}

/// Add the totals `wg status` shows. `building` means a rebuild is due, so
/// `pending > 0`. A lazily cleared `needs_rescan` is not a rebuild; `wg status`
/// shows it as the `reconciling` phase instead.
fn decorate_text_status(mut value: serde_json::Value) -> serde_json::Value {
    let indexed = value["files"].as_u64().unwrap_or(0);
    let added = value["added"].as_u64().unwrap_or(0);
    let pending = value["pending"].as_u64().unwrap_or(0);
    value["indexed"] = serde_json::json!(indexed);
    value["files"] = serde_json::json!(indexed + added);
    value["building"] = serde_json::json!(pending > 0);
    value
}

fn root_status(path: &Path, root: &Root) -> RootStatus {
    let stats = root.index.stats();
    RootStatus {
        path: path.to_path_buf(),
        ready: root.error.is_none()
            && !root.lost.load(Ordering::Acquire)
            && root.pending.load(Ordering::Acquire) == 0,
        pending: root.pending.load(Ordering::Acquire),
        generation: stats.generation,
        files: stats.files,
        chunks: stats.chunks,
        error: root.error.clone().or_else(|| {
            root.lost
                .load(Ordering::Acquire)
                .then(|| "watcher lost trust; run wg down, then wg up".into())
        }),
        text: root.text.as_ref().map(text_status),
        text_error: root.text_error.clone(),
        text_build_ms: root.text_build_ms,
        build_ms: root.build_ms,
        model_ms: root.model_ms,
    }
}

fn status(service: &Service) -> serde_json::Value {
    let (mut roots, attached): (Vec<serde_json::Value>, HashSet<PathBuf>) = {
        let state = service.state.lock().unwrap();
        (
            state
                .roots
                .iter()
                .map(|(p, r)| serde_json::to_value(root_status(p, r)).unwrap())
                .collect(),
            state.roots.keys().cloned().collect(),
        )
    };
    // A build that has not published its root yet is still visible. `ready`
    // stays false, so `wg up` keeps polling and `wg status` shows progress.
    for (path, build) in service.building.lock().unwrap().iter() {
        if !attached.contains(path) {
            roots.push(build.status());
        }
    }
    serde_json::json!({"ok":true,"proto":PROTO,"pid":std::process::id(),"model_loads":service.models.loaded_count(),"loaded_models":service.models.loaded_count(),"roots":roots,"uptime_ms":service.started.elapsed().as_millis()})
}
fn wait_root(
    service: &Service,
    path: &Path,
    pending: &AtomicUsize,
    lost: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    loop {
        if lost.load(Ordering::Acquire) {
            return Err("root watcher lost events; run wg down; wg up to recover".into());
        }
        if let Some(error) = service
            .state
            .lock()
            .unwrap()
            .roots
            .get(path)
            .and_then(|r| r.error.clone())
        {
            return Err(format!(
                "root watcher/index is unhealthy: {error}; run wg down; wg up to recover"
            )
            .into());
        }
        if pending.load(Ordering::Acquire) == 0 && !lost.load(Ordering::Acquire) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("root updates did not settle before the readiness deadline".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn attach(
    service: &Arc<Service>,
    path: &Path,
    choice: ModelChoice,
    progress: Option<Arc<BuildProgress>>,
) -> Result<(RootStatus, u64, u64)> {
    let _admission = service.admission.lock().unwrap();
    injected_build_failure()?;
    let path = root_key(path);
    if !path.is_dir() {
        return Err(format!("not a directory: {}", display_path(&path)).into());
    }
    let existing = {
        service
            .state
            .lock()
            .unwrap()
            .roots
            .get(&path)
            .map(|r| r.index.clone())
    };
    if let Some(index) = existing {
        let (current_choice, pending, error, lost, text, text_error, text_build_ms) = {
            let state = service.state.lock().unwrap();
            let r = &state.roots[&path];
            (
                r.model,
                r.pending.clone(),
                r.error.clone(),
                r.lost.clone(),
                r.text.clone(),
                r.text_error.clone(),
                r.text_build_ms,
            )
        };
        if current_choice != choice {
            return Err(
                "root is already attached with a different model; restart service to change it"
                    .into(),
            );
        }
        wait_root(
            service,
            &path,
            &pending,
            &lost,
            Instant::now() + FRAME_TIMEOUT,
        )?;
        if let Some(error) = error {
            return Err(format!(
                "root watcher/index is unhealthy: {error}; run wg down; wg up to recover"
            )
            .into());
        }
        let stats = index.stats();
        return Ok((
            RootStatus {
                path,
                ready: !lost.load(Ordering::Acquire),
                pending: pending.load(Ordering::Acquire),
                generation: stats.generation,
                files: stats.files,
                chunks: stats.chunks,
                error: None,
                text: text.as_ref().map(text_status),
                text_error,
                text_build_ms,
                build_ms: 0,
                model_ms: 0,
            },
            0,
            0,
        ));
    }
    {
        let s = service.state.lock().unwrap();
        if s.roots.len() >= MAX_ROOTS {
            return Err("service root limit reached".into());
        }
    }
    // Build the text index before subscribing. Its file set is reconciled
    // with rg's walker only after the watcher below is running, so nothing
    // between the build and the watcher can make a pruned answer stale.
    if let Some(progress) = progress.as_ref() {
        progress.set_stage(wgrep::live::STAGE_TEXT);
    }
    let (text, text_error, text_build_ms) = match TextIndex::open(&path) {
        Ok(text) => {
            let build_ms = text.build_ms();
            (Some(Arc::new(text)), None, build_ms)
        }
        Err(error) => {
            eprintln!(
                "wg: text index for {} is unavailable: {error:#}",
                path.display()
            );
            (None, Some(format!("{error:#}")), 0)
        }
    };
    // Subscribe before the initial scan/build. Loss is sticky: a partial
    // event stream cannot authorise a successful cached answer.
    let (tx, rx) = mpsc::sync_channel::<notify::Result<notify::Event>>(QUEUE);
    let lost = Arc::new(AtomicBool::new(false));
    let lost_cb = lost.clone();
    let pending = Arc::new(AtomicUsize::new(0));
    let pending_cb = pending.clone();
    let lost_root = lost.clone();
    let text_cb = text.clone();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
        // One watcher, two indexes: the text index sees the same events.
        if let Some(text) = text_cb.as_ref() {
            match &e {
                Ok(event) => text.ingest(Ok(event.clone())),
                Err(_) => text.mark_untrusted(),
            }
        }
        match &e {
            Ok(event) if read_only(event) => return,
            Err(_) => {
                lost_cb.store(true, Ordering::Release);
            }
            Ok(event) if event.need_rescan() => {
                lost_cb.store(true, Ordering::Release);
            }
            _ => {}
        }
        if let Ok(event) = &e {
            if event.paths.len() > MAX_EVENT_PATHS
                || event
                    .paths
                    .iter()
                    .map(|p| p.as_os_str().as_encoded_bytes().len())
                    .try_fold(0usize, usize::checked_add)
                    .is_none_or(|n| n > MAX_EVENT_BYTES)
            {
                lost_cb.store(true, Ordering::Release);
                return;
            }
        }
        // Reserve before publication: the worker cannot finish an event before
        // its reservation is visible to query admission.
        pending_cb.fetch_add(1, Ordering::AcqRel);
        if tx.try_send(e).is_err() {
            lost_cb.store(true, Ordering::Release);
            pending_cb.fetch_sub(1, Ordering::AcqRel);
        }
    })?;
    watcher.watch(&path, RecursiveMode::Recursive)?;
    // The watcher is live, so reconcile now: `wg up` returns a ready root and
    // the first text query does not pay for the walk. On error the first query
    // retries the reconcile.
    if let Some(text) = text.as_ref() {
        if let Err(error) = text.reconcile_now() {
            eprintln!(
                "wg: text index reconcile for {} deferred to first query: {error:#}",
                path.display()
            );
        }
    }
    let snapshot_path = snapshot_cache_path(&path, choice);
    let index = Arc::new(LiveIndex::open_with_cache_progress(
        &path,
        choice,
        service.models.clone(),
        Some(snapshot_path),
        progress.clone(),
    )?);
    let build = index.stats().build_ms;
    let model_ms = index.stats().model_ms;
    // Keep watcher ownership with a bounded worker. A loss requires a service
    // restart, which recreates the backend subscription as well as the index.
    let idx = index.clone();
    let weak = Arc::downgrade(service);
    let rp = path.clone();
    let pend_worker = pending.clone();
    service.state.lock().unwrap().roots.insert(
        path.clone(),
        Root {
            index: index.clone(),
            pending: pending.clone(),
            lost: lost_root,
            model: choice,
            dirty: false,
            error: None,
            text,
            text_error: text_error.clone(),
            text_build_ms,
            build_ms: build,
            model_ms,
        },
    );
    std::thread::Builder::new()
        .name("wg-root-watch".into())
        .spawn(move || {
            let _watcher = watcher;
            loop {
                let (first, mut reconcile) = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(e) => (Some(e), false),
                    Err(mpsc::RecvTimeoutError::Timeout) => (None, false),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if first.is_none() && !reconcile {
                    continue;
                }
                let Some(svc) = weak.upgrade() else { break };
                // A failed earlier preparation may have left several paths
                // unapplied. A later successful single-file update alone must
                // not authorise that old snapshot again.
                reconcile |= svc
                    .state
                    .lock()
                    .unwrap()
                    .roots
                    .get(&rp)
                    .is_some_and(|r| r.error.is_some());
                let mut batch = Vec::new();
                if let Some(e) = first {
                    batch.push(e);
                }
                for _ in batch.len()..QUEUE {
                    match rx.try_recv() {
                        Ok(e) => batch.push(e),
                        Err(_) => break,
                    }
                }
                let batch_count = batch.len();
                let mut paths = Vec::new();
                let mut failure = if lost.load(Ordering::Acquire) {
                    Some(
                        "watcher queue overflow; root trust lost, restart service to recover"
                            .to_string(),
                    )
                } else {
                    None
                };
                for event in batch {
                    match event {
                        Err(e) => {
                            failure = Some(e.to_string());
                        }
                        Ok(e) => {
                            if e.need_rescan() {
                                lost.store(true, Ordering::Release);
                                reconcile = true;
                            }
                            let event_paths = e.paths;
                            if event_paths.len() > MAX_EVENT_PATHS {
                                lost.store(true, Ordering::Release);
                                failure =
                                    Some("watch event path payload exceeded limit".to_string());
                                continue;
                            }
                            if event_paths.is_empty() {
                                reconcile = true;
                            }
                            for p in event_paths {
                                if p.is_dir()
                                    || matches!(
                                        p.file_name().and_then(|n| n.to_str()),
                                        Some(
                                            ".ignore"
                                                | ".rgignore"
                                                | ".gitignore"
                                                | ".git"
                                                | "exclude"
                                        )
                                    )
                                    || matches!(
                                        e.kind,
                                        notify::EventKind::Create(_)
                                            | notify::EventKind::Remove(_)
                                            | notify::EventKind::Any
                                            | notify::EventKind::Modify(
                                                notify::event::ModifyKind::Name(_)
                                            )
                                    )
                                {
                                    reconcile = true;
                                } else if let Ok(rel) = p.strip_prefix(&rp) {
                                    paths.push(rel.to_path_buf());
                                }
                            }
                        }
                    }
                }
                let result = if let Some(e) = failure {
                    Err(e)
                } else {
                    refresh_batch(&idx, &paths, reconcile)
                };
                let mut st = svc.state.lock().unwrap();
                if let Some(r) = st.roots.get_mut(&rp) {
                    pend_worker.fetch_sub(batch_count, Ordering::AcqRel);
                    match result {
                        Ok(()) => {
                            r.dirty = false;
                            if !r.lost.load(Ordering::Acquire) {
                                r.error = None;
                            }
                        }
                        Err(e) => {
                            r.dirty = true;
                            r.error = Some(e)
                        }
                    }
                }
            }
        })
        .map_err(|e| {
            service.state.lock().unwrap().roots.remove(&path);
            format!("start watcher worker: {e}")
        })?;
    let lost = service.state.lock().unwrap().roots[&path].lost.clone();
    wait_root(
        service,
        &path,
        &pending,
        &lost,
        Instant::now() + FRAME_TIMEOUT,
    )?;
    let stats = service
        .state
        .lock()
        .unwrap()
        .roots
        .get(&path)
        .unwrap()
        .index
        .stats();
    let (text_status, text_error, text_build_ms) = {
        let state = service.state.lock().unwrap();
        let root = state.roots.get(&path).unwrap();
        (
            root.text.as_ref().map(text_status),
            root.text_error.clone(),
            root.text_build_ms,
        )
    };
    Ok((
        RootStatus {
            path,
            ready: true,
            pending: pending.load(Ordering::Acquire),
            generation: stats.generation,
            files: stats.files,
            chunks: stats.chunks,
            error: None,
            text: text_status,
            text_error,
            text_build_ms,
            build_ms: build,
            model_ms,
        },
        build,
        model_ms,
    ))
}
/// Start a root build and return at once. The build owns its own thread, so
/// the `up` frame stays far below the frame deadline however large the tree
/// is. `status` reports the same [`BuildState`] until the root is published.
fn start_up(
    service: &Arc<Service>,
    path: &Path,
    choice: ModelChoice,
) -> Result<serde_json::Value> {
    let path = root_key(path);
    if !path.is_dir() {
        return Err(format!("not a directory: {}", display_path(&path)).into());
    }
    {
        let state = service.state.lock().unwrap();
        if let Some(root) = state.roots.get(&path) {
            if root.model != choice {
                return Err(format!(
                    "root is already attached with model {}; run wg down, then wg up --model {} to change it",
                    model_name(root.model),
                    model_name(choice)
                )
                .into());
            }
            return Ok(serde_json::json!({
                "ok": true,
                "pid": std::process::id(),
                "building": false,
                "root": serde_json::to_value(root_status(&path, root))?,
            }));
        }
    }
    let mut building = service.building.lock().unwrap();
    if let Some(existing) = building.get(&path).cloned() {
        if existing.error.lock().unwrap().is_some() {
            // A finished failed build: drop it and start a fresh build in this
            // same call, so one `up` retries instead of only clearing the
            // error. The new build reports its own error if it fails too.
            building.remove(&path);
        } else {
            return Ok(serde_json::json!({
                "ok": true,
                "pid": std::process::id(),
                "building": true,
                "root": existing.status(),
            }));
        }
    }
    if building.len() >= MAX_ROOTS {
        return Err("service root limit reached".into());
    }
    let build = Arc::new(BuildState::new(path.clone()));
    building.insert(path.clone(), build.clone());
    drop(building);
    let svc = service.clone();
    let p = path.clone();
    let worker = build.clone();
    std::thread::Builder::new()
        .name("wg-root-build".into())
        .spawn(move || {
            let result = attach(&svc, &p, choice, Some(worker.progress.clone()));
            if let Err(error) = result {
                *worker.error.lock().unwrap() = Some(format!("{error:#}"));
            }
            worker.done.store(true, Ordering::Release);
            // A successful build publishes its root; the build entry is no
            // longer needed. A failed one stays visible so polling clients and
            // `wg status` can read the error.
            if worker.error.lock().unwrap().is_none() {
                svc.building.lock().unwrap().remove(&p);
            }
        })
        .map_err(|error| {
            service.building.lock().unwrap().remove(&path);
            format!("start root build: {error}")
        })?;
    Ok(serde_json::json!({
        "ok": true,
        "pid": std::process::id(),
        "building": true,
        "root": build.status(),
    }))
}

fn process(service: &Arc<Service>, req: serde_json::Value) -> serde_json::Value {
    if req["token"].as_str() != Some(&service.token) {
        return serde_json::json!({"ok":false,"error":"authentication failed"});
    }
    let result: Result<serde_json::Value> = (|| match req["op"].as_str().unwrap_or("") {
        "ping" => Ok(serde_json::json!({"ok":true,"pid":std::process::id(),"proto":PROTO})),
        "status" => Ok(status(service)),
        "down" => {
            service.shutdown.store(true, Ordering::SeqCst);
            Ok(serde_json::json!({"ok":true,"pid":std::process::id()}))
        }
        "up" => {
            let p = PathBuf::from(req["path"].as_str().ok_or("path required")?);
            let c = match req["model"].as_str().unwrap_or("auto") {
                "auto" => ModelChoice::Auto,
                "code" => ModelChoice::Code,
                "text" => ModelChoice::Text,
                _ => return Err("model must be auto, code, or text".into()),
            };
            start_up(service, &p, c)
        }
        "text" => {
            let plan: TextPlan = serde_json::from_value(req["plan"].clone())
                .map_err(|e| format!("bad text plan: {e}"))?;
            if !plan.prunable {
                return Ok(serde_json::json!({
                    "ok": false,
                    "fallback": "plan_not_prunable",
                    "reason": plan.reason,
                }));
            }
            let mut groups = Vec::with_capacity(plan.items.len());
            for item in &plan.items {
                // The longest attached root that holds this search path and
                // has a text index. A nested root wins, matching semantic
                // queries.
                let found = {
                    let state = service.state.lock().unwrap();
                    state
                        .roots
                        .iter()
                        .filter_map(|(root, r)| {
                            if item.absolute.starts_with(root) {
                                r.text.clone().map(|text| (root.clone(), text))
                            } else {
                                None
                            }
                        })
                        .max_by_key(|(root, _)| root.components().count())
                };
                let Some((root, text)) = found else {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "fallback": "no_attached_text_root",
                    }));
                };
                let Some(prefix) = wisp_grep::text::prefix_of(&item.absolute, &root) else {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "fallback": "prefix_not_utf8",
                    }));
                };
                match text.candidates(&item.query, &prefix)? {
                    Some(candidates) => groups.push(TextGroup {
                        arg_index: item.arg_index,
                        files: candidates.files,
                        paths: candidates.paths,
                    }),
                    None => {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "fallback": "text_index_untrusted",
                        }))
                    }
                }
            }
            Ok(serde_json::json!({"ok":true,"groups":groups}))
        }
        "query" => {
            let p = root_key(Path::new(req["path"].as_str().unwrap_or(".")));
            let whole_repo = req["whole_repo"] == true;
            let query = req["query"].as_str().ok_or("query required")?;
            let requested_model = match req["model"].as_str().unwrap_or("auto") {
                "auto" => ModelChoice::Auto,
                "code" => ModelChoice::Code,
                "text" => ModelChoice::Text,
                _ => return Err("model must be auto, code, or text".into()),
            };
            let n = req["top_n"].as_u64().unwrap_or(5);
            if !(1..=100).contains(&n) {
                return Err("result count must be between 1 and 100".into());
            }
            let n = n as usize;
            let rank = match req["rank"].as_str().unwrap_or("semantic") {
                "semantic" => RankMode::Semantic,
                "classic" => RankMode::Classic,
                _ => return Err("rank must be semantic or classic".into()),
            };
            let saturate = req["saturate"] == true;
            if saturate && rank == RankMode::Classic {
                return Err("saturation requires the semantic ranking mode".into());
            }
            let (root, index, pending, lost) = {
                let state = service.state.lock().unwrap();
                let root = state
                    .roots
                    .keys()
                    .filter(|r| p.starts_with(r))
                    .max_by_key(|r| r.components().count())
                    .cloned()
                    .ok_or("no attached root contains this path; run wg up")?;
                let r = &state.roots[&root];
                if let Some(e) = &r.error {
                    return Err(format!(
                        "root watcher/index is untrusted: {e}; run wg down; wg up to recover"
                    )
                    .into());
                }
                let revision = r.index.stats().model_revision;
                if (requested_model == ModelChoice::Code && revision != wgrep::CODE.revision)
                    || (requested_model == ModelChoice::Text && revision != wgrep::TEXT.revision)
                {
                    let requested = model_name(requested_model);
                    return Err(format!(
                        "query model {requested} does not match the attached root model {} (root attached with {}); run wg down, then wg up --model {requested} to change it",
                        attached_model_name(&revision),
                        model_name(r.model)
                    )
                    .into());
                }
                (root, r.index.clone(), r.pending.clone(), r.lost.clone())
            };
            let scope = if whole_repo {
                &root
            } else if p.is_file() {
                return Err(
                    "file-scoped semantic queries are not supported by the live index API".into(),
                );
            } else {
                &p
            };
            if scope != root.as_path() {
                return Err(
                    "directory-scoped semantic queries are not supported by the live index API"
                        .into(),
                );
            }
            let deadline = Instant::now() + FRAME_TIMEOUT;
            loop {
                wait_root(service, &root, &pending, &lost, deadline)?;
                let generation = index.stats().generation;
                let rows = index.query_with_mode(query, n, req["explain"] == true, rank, saturate)?;
                wait_root(service, &root, &pending, &lost, deadline)?;
                if generation == index.stats().generation {
                    return Ok(serde_json::json!({"ok":true,"results":rows}));
                }
                if Instant::now() >= deadline {
                    return Err("root kept changing throughout the query deadline".into());
                }
            }
        }
        _ => Err("unknown service operation".into()),
    })();
    result.unwrap_or_else(|e| serde_json::json!({"ok":false,"error":format!("{e:#}")}))
}

/// Run the private foreground service process.
pub fn daemon(cache: Option<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(store_dir())?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path())?;
    File::try_lock(&lock).map_err(|_| "another wg service already owns the global lock")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let token = random_token();
    let service = Arc::new(Service {
        state: Mutex::new(State {
            roots: HashMap::new(),
        }),
        building: Mutex::new(HashMap::new()),
        admission: Mutex::new(()),
        models: Arc::new(ModelPool::new(cache)),
        token: token.clone(),
        started: Instant::now(),
        shutdown: AtomicBool::new(false),
    });
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(CONNS);
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..4 {
        let rx = rx.clone();
        let svc = service.clone();
        std::thread::spawn(move || {
            loop {
                let stream = { rx.lock().unwrap().recv() };
                if let Ok(s) = stream {
                    let _ = s.set_read_timeout(Some(FRAME_TIMEOUT));
                    let reply = read_frame(&s, MAX_REQUEST)
                        .and_then(|b| {
                            let v = serde_json::from_slice(&b)?;
                            Ok(process(&svc, v))
                        })
                        .unwrap_or_else(|e| serde_json::json!({"ok":false,"error":e.to_string()}));
                    let _ = send(&s, &reply);
                } else {
                    break;
                }
            }
        });
    }
    publish(listener.local_addr()?.port(), &token)?;
    while !service.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, _)) => {
                if tx.try_send(s).is_err() { /* explicit busy via connection close */ }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(e) => return Err(e.into()),
        }
    }
    let m = metadata();
    let _ = std::fs::remove_file(m);
    Ok(())
}

/// Start the shared service if it is not already answering, then return.
/// Used by `wg up` so every semantic request reaches the same process
/// (one daemon, never a per-root or per-query daemon).
pub fn ensure_running(cache: Option<PathBuf>) -> Result<()> {
    if request(serde_json::json!({"op":"ping"})).is_ok() {
        return Ok(());
    }
    let mut child = spawn(cache)?;
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        if request(serde_json::json!({"op":"ping"})).is_ok() {
            return Ok(());
        }
        if let Some(exit) = child.try_wait()? {
            // A simultaneous first request may have won the lifetime lock but
            // not published its listener yet. Wait for that owner, not for
            // this redundant child process.
            let another_owner = OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path())
                .ok()
                .is_some_and(|file| File::try_lock(&file).is_err());
            if !another_owner {
                return Err(format!(
                    "wg service exited ({exit}); see {}",
                    store_dir().join("daemon.log").display()
                )
                .into());
            }
        }
        if Instant::now() > until {
            return Err(format!(
                "wg service did not become ready; see {}",
                store_dir().join("daemon.log").display()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Attach a root without holding one long request frame open.
///
/// `up` starts the build and returns at once. This function then polls
/// `status` with short frames until the root is ready. Every individual frame
/// keeps the bounded-IPC rule; the total wall time is unbounded, so a large
/// tree can build for minutes. A Ctrl-C here only stops this client: the
/// daemon keeps building, and a later `up` or `status` sees the same build.
pub fn up(path: &Path, model: &str, cache: Option<PathBuf>) -> Result<serde_json::Value> {
    let start = Instant::now();
    // Check the root before the daemon starts. A typo must not leave an empty
    // daemon behind, and no build can succeed for a missing path.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !canonical.is_dir() {
        return Err(format!("not a directory: {}", display_path(&canonical)).into());
    }
    ensure_running(cache)?;
    let mut reply = request_timeout(
        serde_json::json!({"op":"up","path":path,"model":model}),
        Duration::from_secs(30),
    )?;
    loop {
        let root = reply
            .get("root")
            .filter(|root| root.is_object())
            .cloned()
            .or_else(|| {
                reply["roots"]
                    .as_array()?
                    .iter()
                    .find(|root| {
                        root["path"]
                            .as_str()
                            .is_some_and(|p| Path::new(p) == canonical)
                    })
                    .cloned()
            });
        if let Some(root) = root {
            if root["ready"] == true
                && (!root["text"].is_null() || root["text_error"].is_string())
            {
                let mut value = reply;
                value["ok"] = serde_json::json!(true);
                value["building"] = serde_json::json!(false);
                value["root"] = root.clone();
                value["build_ms"] = root["build_ms"].clone();
                value["model_ms"] = root["model_ms"].clone();
                value["total_ms"] = serde_json::json!(start.elapsed().as_millis() as u64);
                return Ok(value);
            }
            if root["building"] != true
                && let Some(error) = root["error"].as_str()
            {
                return Err(error.to_owned().into());
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        reply = request_timeout(
            serde_json::json!({"op":"status"}),
            Duration::from_secs(30),
        )?;
    }
}

fn spawn(cache: Option<PathBuf>) -> Result<wisp_grep::detached::DetachedChild> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(store_dir())?;
    let log_path = store_dir().join("daemon.log");
    private(&log_path);
    let mut args = vec![std::ffi::OsString::from("--daemon")];
    if let Some(p) = cache {
        args.push(std::ffi::OsString::from("--cache-dir"));
        args.push(p.into_os_string());
    }
    // The daemon inherits only NUL and the log. In particular it must not
    // inherit the caller's stdout pipe, or a piped `wg up` never sees EOF.
    Ok(wisp_grep::detached::spawn(&exe, &args, &log_path)?)
}
#[allow(clippy::too_many_arguments)]
pub fn query(
    query: &str,
    path: &Path,
    whole_repo: bool,
    n: usize,
    explain: bool,
    model: &str,
    rank: &str,
    saturate: bool,
) -> Result<serde_json::Value> {
    load_metadata().map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
        "no wg service is running for this cache; run `wg up` first".into()
    })?;
    request(
        serde_json::json!({"op":"query","query":query,"path":path,"whole_repo":whole_repo,"top_n":n,"explain":explain,"model":model,"rank":rank,"saturate":saturate}),
    )
}

/// Ask a reachable `wg` service for verified text candidates.
///
/// `None` on any doubt: no service, plan not prunable, an untrusted index, an
/// unexpected reply, or the bounded timeout. The caller then runs rg's walk.
pub fn text_groups(plan: &TextPlan) -> std::result::Result<Vec<TextGroup>, String> {
    if !plan.prunable {
        return Err(plan
            .reason
            .clone()
            .unwrap_or_else(|| "plan_not_prunable".to_owned()));
    }
    let reply = request_timeout(
        serde_json::json!({"op":"text","plan":plan}),
        Duration::from_secs(8),
    )
    .map_err(|error| format!("service_unavailable: {error}"))?;
    serde_json::from_value(reply["groups"].clone())
        .map_err(|error| format!("bad_text_reply: {error}"))
}

pub fn status_json() -> Result<serde_json::Value> {
    request(serde_json::json!({"op":"status"})).map_err(|error| {
        if daemon_unavailable(error.as_ref()) {
            NO_SERVICE.into()
        } else {
            error
        }
    })
}
pub fn down() -> Result<serde_json::Value> {
    match request(serde_json::json!({"op":"down"})) {
        Ok(value) => Ok(value),
        // Stopping an already stopped daemon is not an error.
        Err(error) if daemon_unavailable(error.as_ref()) => {
            Ok(serde_json::json!({"ok":true,"running":false}))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_service() -> Service {
        Service {
            state: Mutex::new(State {
                roots: HashMap::new(),
            }),
            building: Mutex::new(HashMap::new()),
            admission: Mutex::new(()),
            models: Arc::new(ModelPool::new(None)),
            token: "t".into(),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
        }
    }
    #[test]
    fn text_status_building_means_a_rebuild_is_due() {
        // An idle daemon with a lazily pending rescan is not building: the
        // human status shows it as `reconciling`, and `building` stays false.
        let rescan = decorate_text_status(serde_json::json!({
            "files": 10,
            "added": 1,
            "pending": 0,
            "needs_rescan": true,
        }));
        assert_eq!(rescan["building"], false, "{rescan}");
        assert_eq!(rescan["needs_rescan"], true, "{rescan}");

        let pending = decorate_text_status(serde_json::json!({
            "files": 10,
            "added": 1,
            "pending": 1,
            "needs_rescan": true,
        }));
        assert_eq!(pending["building"], true, "{pending}");
    }

    #[test]
    fn readiness_wait_drains_work_but_never_accepts_lost_trust() {
        let svc = empty_service();
        let pending = Arc::new(AtomicUsize::new(1));
        let lost = AtomicBool::new(false);
        let worker_pending = pending.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            worker_pending.fetch_sub(1, Ordering::AcqRel);
        });
        wait_root(
            &svc,
            Path::new("."),
            &pending,
            &lost,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        worker.join().unwrap();
        lost.store(true, Ordering::Release);
        assert!(
            wait_root(
                &svc,
                Path::new("."),
                &pending,
                &lost,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }
    #[test]
    fn readiness_wait_has_one_deadline_with_outstanding_work() {
        let svc = empty_service();
        let pending = AtomicUsize::new(1);
        let lost = AtomicBool::new(false);
        let start = Instant::now();
        assert!(
            wait_root(
                &svc,
                Path::new("."),
                &pending,
                &lost,
                start + Duration::from_millis(15)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(pending.load(Ordering::Acquire), 1);
    }
    #[test]
    fn root_key_is_absolute_when_existing() {
        let p = std::env::current_dir().unwrap();
        assert_eq!(root_key(&p), p.canonicalize().unwrap());
    }
    #[test]
    fn frame_reader_requires_newline_and_honors_cap() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"{\"ok\":true}\n").unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        assert_eq!(read_frame(&stream, 32).unwrap(), b"{\"ok\":true}\n");
        writer.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(b"12345\n").unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(read_frame(&stream, 4).is_err());
        writer.join().unwrap();
    }
    #[test]
    fn rejects_unknown_model() {
        let svc = Arc::new(Service {
            state: Mutex::new(State {
                roots: HashMap::new(),
            }),
            building: Mutex::new(HashMap::new()),
            admission: Mutex::new(()),
            models: Arc::new(ModelPool::new(None)),
            token: "t".into(),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
        });
        let r = process(
            &svc,
            serde_json::json!({"token":"t","op":"up","path":".","model":"not-a-model"}),
        );
        assert!(!r["ok"].as_bool().unwrap());
    }
    #[test]
    fn status_schema_is_global() {
        let svc = Service {
            state: Mutex::new(State {
                roots: HashMap::new(),
            }),
            building: Mutex::new(HashMap::new()),
            admission: Mutex::new(()),
            models: Arc::new(ModelPool::new(None)),
            token: "t".into(),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
        };
        let s = status(&svc);
        assert!(s["pid"].is_number());
        assert!(s["roots"].as_array().unwrap().is_empty());
    }
    #[test]
    fn snapshot_cache_path_is_per_root_and_model() {
        let root = std::env::current_dir().unwrap();
        let canonical = root_key(&root);
        let code = snapshot_cache_path(&canonical, ModelChoice::Code);
        assert_eq!(code, snapshot_cache_path(&canonical, ModelChoice::Code));
        assert_ne!(code, snapshot_cache_path(&canonical, ModelChoice::Text));
        assert_ne!(code, snapshot_cache_path(&canonical, ModelChoice::Auto));
        assert_eq!(code.parent(), Some(store_dir().join("roots").as_path()));
        assert!(
            code.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-code.wgsnap")
        );
    }
}
