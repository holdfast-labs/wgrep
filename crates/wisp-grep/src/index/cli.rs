//! Index and daemon management: `wgrep --start [PATH]`, `wgrep --stop [PATH]`,
//! `wgrep --index-build [PATH]...`, `wgrep --index-status [PATH]...`,
//! `wgrep --index-stats [PATH]` and `wgrep --index-serve [PATH]` (the daemon
//! itself, run in the foreground).
//!
//! These are recognized only as the first argument, before rg's parser runs,
//! so every other invocation is parsed exactly as rg would parse it. They are
//! flags rather than words like `start`, because `wgrep start` must keep
//! meaning "search for start", as it does in rg.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::flags::{HiArgs, ParseResult};

use super::{build, client, format, store};

/// Default: store a trigram as dense once it occurs in this share of files.
const DENSE_PERCENT: u64 = 100;

pub(crate) fn run_if_requested() -> Option<ExitCode> {
    let mut argv = std::env::args_os().skip(1);
    let command = argv.next()?;
    let paths: Vec<OsString> = argv.collect();
    let paths = if paths.is_empty() { vec![OsString::from(".")] } else { paths };
    let result = match command.to_str()? {
        "--index-build" => build_all(&paths),
        "--index-status" => status_all(&paths),
        "--index-stats" => stats(&paths[0]),
        "--index-serve" => super::daemon::serve(&paths[0]),
        "--start" => start(&paths[0]),
        "--stop" => stop(&paths[0]),
        _ => return None,
    };
    Some(match result {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            eprintln!("wgrep: {err:#}");
            ExitCode::from(2)
        }
    })
}

pub(crate) fn dense_percent() -> u64 {
    std::env::var("WGREP_DENSE_PERCENT").ok().and_then(|v| v.parse().ok()).unwrap_or(DENSE_PERCENT)
}

/// rg's arguments for walking `path` with default flags, the path as the
/// walker spells it, and its absolute form.
fn resolve(path: &OsString) -> anyhow::Result<(HiArgs, PathBuf, PathBuf)> {
    // `--files PATH` gives exactly the files rg searches under PATH with
    // default flags. Config files are ignored so the build is stable.
    let args = match crate::flags::parse_args(vec![OsString::from("--files"), path.clone()]) {
        ParseResult::Ok(args) => args,
        ParseResult::Err(err) => return Err(err),
        ParseResult::Special(_) => anyhow::bail!("unexpected special mode"),
    };
    let (search_paths, cwd) = args.index_paths();
    let root_arg = search_paths.first().cloned().unwrap_or_else(|| PathBuf::from(path));
    let root = store::absolute(&root_arg, cwd);
    anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());
    Ok((args, root_arg, root))
}

fn build_all(paths: &[OsString]) -> anyhow::Result<()> {
    let dense_percent = dense_percent();
    let debug = std::env::var_os("WGREP_BUILD_DEBUG").is_some();
    for path in paths {
        let invocation_started = Instant::now();
        let (args, root_arg, root) = resolve(path)?;
        let walk_started = Instant::now();
        let files = walk(&args)?;
        // Sampled immediately after the walk, before the build runs.
        let walk_seconds = walk_started.elapsed().as_secs_f64();
        if debug {
            eprintln!("wgrep build: {:<16} {:.3}s ({} files)", "walk", walk_seconds, files.len());
        }
        let s = build::build(&root, &root_arg, files, dense_percent)?;
        if std::env::var_os("WGREP_BUILD_JSON").is_some() {
            // One machine-readable line (stderr) per build for harnesses.
            // `walk_seconds` is the walk alone; `total_seconds` is the whole
            // invocation, which includes the walk and the build.
            eprintln!(
                "{}",
                serde_json::json!({
                    "event": "build",
                    "root": s.root.to_string_lossy(),
                    "walked": s.walked,
                    "indexed": s.indexed,
                    "source_bytes": s.source_bytes,
                    "index_bytes": s.stats.bytes,
                    "trigrams": s.stats.trigrams,
                    "dense_trigrams": s.stats.dense_trigrams,
                    "walk_seconds": walk_seconds,
                    "build_seconds": s.seconds,
                    "total_seconds": invocation_started.elapsed().as_secs_f64(),
                    "index_path": s.index_path.to_string_lossy(),
                })
            );
        }
        println!(
            "indexed {} of {} files ({:.1} MB) under {} in {:.2}s: {} bytes ({:.3}x source), {} trigrams ({} dense) -> {}",
            s.indexed,
            s.walked,
            s.source_bytes as f64 / 1e6,
            s.root.display(),
            s.seconds,
            s.stats.bytes,
            s.stats.bytes as f64 / s.source_bytes.max(1) as f64,
            s.stats.trigrams,
            s.stats.dense_trigrams,
            s.index_path.display()
        );
    }
    Ok(())
}

pub(crate) fn walk(args: &HiArgs) -> anyhow::Result<Vec<PathBuf>> {
    let haystacks = args.haystack_builder();
    let mut files = Vec::new();
    for result in args.walk_builder()?.build() {
        if let Some(dent) = haystacks.build_from_result(result).as_ref().and_then(|h| h.dent()) {
            if !dent.is_stdin() {
                files.push(dent.path().to_path_buf());
            }
        }
    }
    Ok(files)
}

/// Default mode: ensures a daemon is serving `root`, starting one in the
/// background when an index already exists. Returns true when a daemon is
/// serving. It never builds an index (a surprise build is avoided); a root
/// without an index falls back to the plain walk.
pub(crate) fn ensure_serving(root: &Path) -> bool {
    if client::control(root, "ping").is_some() {
        return true;
    }
    if store::current(root).is_none() {
        return false;
    }
    spawn_serve(root).is_ok()
}

/// Spawns `--index-serve root` detached and waits (bounded) for it to answer.
fn spawn_serve(root: &Path) -> anyhow::Result<()> {
    let slot = store::slot_for(root).ok_or_else(|| anyhow::anyhow!("no index directory; set WGREP_INDEX_DIR"))?;
    std::fs::create_dir_all(&slot)?;
    let log = std::fs::File::create(slot.join("daemon.log"))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--index-serve")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut command);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("daemon exited ({status})");
        }
        if client::control(root, "ping").is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("daemon did not become ready")
}

/// Starts a daemon for `path` in the background (building its index first
/// if needed) and returns once it answers.
fn start(path: &OsString) -> anyhow::Result<()> {
    let (_, _, root) = resolve(path)?;
    if let Some(pid) = client::control(&root, "ping") {
        println!("wgrep: already serving {} (pid {pid})", root.display());
        return Ok(());
    }
    let slot = store::slot_for(&root).ok_or_else(|| anyhow::anyhow!("no index directory; set WGREP_INDEX_DIR"))?;
    std::fs::create_dir_all(&slot)?;
    let log_path = slot.join("daemon.log");
    let log = std::fs::File::create(&log_path)?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--index-serve")
        .arg(&root)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut command);
    let mut child = command.spawn()?;
    // The first start of a large tree includes its index build.
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            anyhow::bail!("daemon exited ({status}):\n{}", tail.trim_end());
        }
        if client::control(&root, "ping") == Some(u64::from(child.id())) {
            println!(
                "wgrep: serving {} (pid {}); searches under it now skip the walk. Log: {}",
                root.display(),
                child.id(),
                log_path.display()
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("daemon did not become ready; see {}", log_path.display())
}

/// Lets the daemon outlive the terminal or agent session that started it.
fn detach(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);

        // Windows children inherit every inheritable handle, not only their
        // stdio. If `wgrep --start` runs inside a pipeline, the daemon would
        // hold the pipe open forever and the reader would never see EOF. The
        // launcher exits right after the spawn, so it can stop sharing its
        // own standard handles.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
            fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE.
        for which in [-10i32, -11, -12] {
            // SAFETY: GetStdHandle has no preconditions; SetHandleInformation
            // only changes the inherit flag and fails harmlessly on an invalid
            // or absent handle.
            unsafe {
                let handle = GetStdHandle(which as u32);
                if !handle.is_null() && handle as isize != -1 {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

/// Prints the serving daemon's `stats` (full-walk counters by cause, queue
/// depths) as JSON. Used to prove incremental freshness (see
/// `docs/WGREP_INCREMENTAL_FRESHNESS.md`, Testing).
fn stats(path: &OsString) -> anyhow::Result<()> {
    let (_, _, root) = resolve(path)?;
    match client::stats(&root) {
        Some(v) => {
            println!("{}", serde_json::json!({ "root": root, "stats": v }));
            Ok(())
        }
        None => anyhow::bail!("no daemon is serving {}", root.display()),
    }
}

fn stop(path: &OsString) -> anyhow::Result<()> {
    let (_, _, root) = resolve(path)?;
    match client::control(&root, "shutdown") {
        Some(pid) => println!("wgrep: stopped the daemon for {} (pid {pid})", root.display()),
        None => println!("wgrep: no daemon is serving {}", root.display()),
    }
    Ok(())
}

fn status_all(paths: &[OsString]) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    for path in paths {
        let abs = store::absolute(&PathBuf::from(path), &cwd);
        match store::find(&abs) {
            None => println!("{}: no index", abs.display()),
            Some((root, file)) => {
                let index = format::Index::open(&file)?;
                println!(
                    "{}: indexed at {} ({} files, {} source bytes, {} index bytes) -> {}",
                    abs.display(),
                    root.display(),
                    index.file_count(),
                    index.header.get("source_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                    std::fs::metadata(&file)?.len(),
                    file.display()
                );
                match client::control(&root, "ping") {
                    Some(pid) => println!("  daemon: running (pid {pid})"),
                    None => println!("  daemon: not running (start it with `wgrep --start {}`)", root.display()),
                }
            }
        }
    }
    Ok(())
}
