//! One authenticated, multi-root semantic service for the `wg` front door.
//! The daemon owns one shared model pool; each root has one live index.
use base64::{Engine, engine::general_purpose::STANDARD};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use wgrep::lexical;
use wgrep::live::{LiveIndex, ModelPool};
use wgrep::{ModelChoice, Result};
use std::collections::HashMap;
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
}
struct Root {
    index: Arc<LiveIndex>,
    pending: Arc<AtomicUsize>,
    lost: Arc<AtomicBool>,
    model: ModelChoice,
    dirty: bool,
    error: Option<String>,
}
struct State {
    roots: HashMap<PathBuf, Root>,
}
struct Service {
    state: Mutex<State>,
    admission: Mutex<()>,
    models: Arc<ModelPool>,
    token: String,
    started: Instant,
    shutdown: AtomicBool,
    /// In-flight lexical (rg child) requests, for the explicit admission bound.
    lexical_active: AtomicUsize,
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
fn read_frame(stream: &TcpStream, cap: usize) -> Result<Vec<u8>> {
    let mut r = BufReader::new(stream);
    let mut out = Vec::new();
    let end = Instant::now() + FRAME_TIMEOUT;
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
    write_frame(&s, &b, Duration::from_secs(30))?;
    let line = read_frame(&s, MAX_REPLY)?;
    let r: serde_json::Value = serde_json::from_slice(&line)?;
    if r["ok"] != true {
        return Err(r["error"]
            .as_str()
            .unwrap_or("service request failed")
            .into());
    }
    Ok(r)
}

/// RAII admission slot for one lexical child; releases on drop.
struct LexAdmission<'a>(&'a AtomicUsize);

impl Drop for LexAdmission<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn admit_lexical(service: &Service) -> Result<LexAdmission<'_>> {
    if service.lexical_active.fetch_add(1, Ordering::AcqRel) >= lexical::MAX_CONCURRENT {
        service.lexical_active.fetch_sub(1, Ordering::AcqRel);
        return Err("lexical search admission limit reached; retry shortly".into());
    }
    Ok(LexAdmission(&service.lexical_active))
}

fn status(service: &Service) -> serde_json::Value {
    let state = service.state.lock().unwrap();
    let roots: Vec<_> = state
        .roots
        .iter()
        .map(|(p, r)| {
            let s = r.index.stats();
            RootStatus {
                path: p.clone(),
                ready: r.error.is_none()
                    && !r.lost.load(Ordering::Acquire)
                    && r.pending.load(Ordering::Acquire) == 0,
                pending: r.pending.load(Ordering::Acquire),
                generation: s.generation,
                files: s.files,
                chunks: s.chunks,
                error: r.error.clone().or_else(|| {
                    r.lost
                        .load(Ordering::Acquire)
                        .then(|| "watcher lost trust; run wg down, then wg up".into())
                }),
            }
        })
        .collect();
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
) -> Result<(RootStatus, u64, u64)> {
    let _admission = service.admission.lock().unwrap();
    let path = root_key(path);
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()).into());
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
        let (current_choice, pending, error, lost) = {
            let state = service.state.lock().unwrap();
            let r = &state.roots[&path];
            (r.model, r.pending.clone(), r.error.clone(), r.lost.clone())
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
    // Subscribe before the initial scan/build. Loss is sticky: a partial
    // event stream cannot authorise a successful cached answer.
    let (tx, rx) = mpsc::sync_channel::<notify::Result<notify::Event>>(QUEUE);
    let lost = Arc::new(AtomicBool::new(false));
    let lost_cb = lost.clone();
    let pending = Arc::new(AtomicUsize::new(0));
    let pending_cb = pending.clone();
    let lost_root = lost.clone();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |e| {
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
    let snapshot_path = snapshot_cache_path(&path, choice);
    let index = Arc::new(LiveIndex::open_with_cache(
        &path,
        choice,
        service.models.clone(),
        Some(snapshot_path),
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
    Ok((
        RootStatus {
            path,
            ready: true,
            pending: pending.load(Ordering::Acquire),
            generation: stats.generation,
            files: stats.files,
            chunks: stats.chunks,
            error: None,
        },
        build,
        model_ms,
    ))
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
            let (r, build, model_ms) = attach(service, &p, c)?;
            Ok(
                serde_json::json!({"ok":true,"pid":std::process::id(),"root":r,"build_ms":build,"model_ms":model_ms,"total_ms":req["total_ms"]}),
            )
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
                    return Err("query model differs from attached root model; use the model selected by wg up".into());
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
                let rows = index.query(query, n, req["explain"] == true)?;
                wait_root(service, &root, &pending, &lost, deadline)?;
                if generation == index.stats().generation {
                    return Ok(serde_json::json!({"ok":true,"results":rows}));
                }
                if Instant::now() >= deadline {
                    return Err("root kept changing throughout the query deadline".into());
                }
            }
        }
        "lexical" => {
            let raw = req["args"].as_array().ok_or("lexical args required")?;
            let mut args = Vec::with_capacity(raw.len());
            for value in raw {
                args.push(
                    value
                        .as_str()
                        .ok_or("lexical args must be strings")?
                        .to_owned(),
                );
            }
            if args.is_empty() {
                return Err("lexical args required".into());
            }
            let cwd = PathBuf::from(req["cwd"].as_str().ok_or("lexical cwd required")?);
            if !cwd.is_dir() {
                return Err(format!("search directory does not exist: {}", cwd.display()).into());
            }
            let stdin = match req["stdin"].as_str() {
                Some(encoded) => Some(STANDARD.decode(encoded)?),
                _ => None,
            };
            let _admission = admit_lexical(service)?;
            let out = lexical::run(&args, &cwd, stdin.as_deref(), lexical::timeout())?;
            Ok(serde_json::json!({
                "ok": true,
                "exit_code": out.exit_code,
                "stdout": STANDARD.encode(&out.stdout),
                "stderr": STANDARD.encode(&out.stderr),
            }))
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
        admission: Mutex::new(()),
        models: Arc::new(ModelPool::new(cache)),
        token: token.clone(),
        started: Instant::now(),
        shutdown: AtomicBool::new(false),
        lexical_active: AtomicUsize::new(0),
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
/// Used by both `wg up` and the lexical route so every request reaches the
/// same process (one daemon, never a per-root or per-query daemon).
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

pub fn up(path: &Path, model: &str, cache: Option<PathBuf>) -> Result<serde_json::Value> {
    let start = Instant::now();
    ensure_running(cache)?;
    let mut v = request(
        serde_json::json!({"op":"up","path":path,"model":model,"total_ms":start.elapsed().as_millis()}),
    )?;
    v["total_ms"] = serde_json::json!(start.elapsed().as_millis());
    Ok(v)
}
#[cfg(windows)]
struct StdioInheritanceGuard(Vec<(isize, u32)>);

#[cfg(windows)]
impl StdioInheritanceGuard {
    /// Mark the caller's std handles non-inheritable for the duration of one
    /// `CreateProcessW`. Without this, a detached daemon inherits the client's
    /// stdout/stderr pipe and keeps it open, so `wg up | tee`, CI capture, and
    /// any parent waiting for EOF hang after `wg up` exits.
    fn disable() -> Self {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        }
        const STD_INPUT_HANDLE: u32 = -10i32 as u32;
        const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
        const STD_ERROR_HANDLE: u32 = -12i32 as u32;
        let handles: Vec<isize> = unsafe {
            [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
                .into_iter()
                .map(|which| GetStdHandle(which) as isize)
                .filter(|handle| *handle != 0 && *handle != -1)
                .collect()
        };
        Self(disable_handle_inheritance(&handles))
    }
}

#[cfg(windows)]
fn disable_handle_inheritance(handles: &[isize]) -> Vec<(isize, u32)> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetHandleInformation(handle: *mut std::ffi::c_void, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    let mut saved = Vec::new();
    for &handle in handles {
        let mut flags = 0u32;
        unsafe {
            if GetHandleInformation(handle as *mut std::ffi::c_void, &mut flags) != 0
                && flags & HANDLE_FLAG_INHERIT != 0
                && SetHandleInformation(handle as *mut std::ffi::c_void, HANDLE_FLAG_INHERIT, 0)
                    != 0
            {
                saved.push((handle, flags));
            }
        }
    }
    saved
}

#[cfg(windows)]
fn restore_handle_inheritance(saved: &[(isize, u32)]) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for &(handle, flags) in saved {
        unsafe {
            let _ = SetHandleInformation(
                handle as *mut std::ffi::c_void,
                HANDLE_FLAG_INHERIT,
                flags & HANDLE_FLAG_INHERIT,
            );
        }
    }
}

#[cfg(windows)]
impl Drop for StdioInheritanceGuard {
    fn drop(&mut self) {
        restore_handle_inheritance(&std::mem::take(&mut self.0));
    }
}

fn spawn(cache: Option<PathBuf>) -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut c = std::process::Command::new(exe);
    std::fs::create_dir_all(store_dir())?;
    let log_path = store_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    private(&log_path);
    c.arg("--daemon");
    if let Some(p) = cache {
        c.arg("--cache-dir").arg(p);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    #[cfg(windows)]
    let _stdio_guard = StdioInheritanceGuard::disable();
    Ok(c.stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?)
}
pub fn query(
    query: &str,
    path: &Path,
    whole_repo: bool,
    n: usize,
    explain: bool,
    model: &str,
) -> Result<serde_json::Value> {
    load_metadata().map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
        "no wg service is running for this cache; run `wg up` first".into()
    })?;
    request(
        serde_json::json!({"op":"query","query":query,"path":path,"whole_repo":whole_repo,"top_n":n,"explain":explain,"model":model}),
    )
}

/// Run a bounded, authoritative `rg` child inside the shared service.
pub fn lexical(
    args: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    cache: Option<PathBuf>,
) -> Result<serde_json::Value> {
    ensure_running(cache)?;
    request(serde_json::json!({
        "op": "lexical",
        "args": args,
        "cwd": cwd.to_string_lossy(),
        "stdin": stdin.map(|bytes| STANDARD.encode(bytes)),
    }))
}
pub fn status_json() -> Result<serde_json::Value> {
    request(serde_json::json!({"op":"status"}))
}
pub fn down() -> Result<serde_json::Value> {
    request(serde_json::json!({"op":"down"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn inheritance_guard_clears_and_restores_a_real_handle() {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetHandleInformation(handle: *mut std::ffi::c_void, flags: *mut u32) -> i32;
            fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 1;
        let path = std::env::temp_dir().join(format!("wg-inherit-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let handle = file.as_raw_handle() as isize;
        let flag = |h: isize| {
            let mut flags = 0u32;
            assert_ne!(unsafe { GetHandleInformation(h as *mut _, &mut flags) }, 0);
            flags & HANDLE_FLAG_INHERIT
        };
        assert_ne!(
            unsafe {
                SetHandleInformation(handle as *mut _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
            },
            0
        );
        assert_eq!(flag(handle), HANDLE_FLAG_INHERIT);
        let saved = disable_handle_inheritance(&[handle]);
        assert_eq!(saved.len(), 1);
        assert_eq!(flag(handle), 0);
        restore_handle_inheritance(&saved);
        assert_eq!(flag(handle), HANDLE_FLAG_INHERIT);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    fn empty_service() -> Service {
        Service {
            state: Mutex::new(State {
                roots: HashMap::new(),
            }),
            admission: Mutex::new(()),
            models: Arc::new(ModelPool::new(None)),
            token: "t".into(),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
            lexical_active: AtomicUsize::new(0),
        }
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
            admission: Mutex::new(()),
            models: Arc::new(ModelPool::new(None)),
            token: "t".into(),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
            lexical_active: AtomicUsize::new(0),
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
            admission: Mutex::new(()),
            models: Arc::new(ModelPool::new(None)),
            token: "t".into(),
            started: Instant::now(),
            shutdown: AtomicBool::new(false),
            lexical_active: AtomicUsize::new(0),
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
