#![cfg(target_os = "macos")]

//! macOS FSEvents qualification for the `wg` text route.
//!
//! These tests start a real `wg` service with an isolated `WG_SERVICE_DIR` and
//! `WGREP_INDEX_DIR`. Each launch is bounded, and each daemon is stopped on
//! drop. They cover the two FSEvents gaps that block qualification: unpaired
//! and replacing renames, and a non-UTF-8 file name that must not panic the
//! watcher thread.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Scratch {
    base: PathBuf,
    root: PathBuf,
    service: PathBuf,
    indexes: PathBuf,
    events: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        Scratch::create(name, None)
    }

    fn at(name: &str, root: PathBuf) -> Scratch {
        Scratch::create(name, Some(root))
    }

    fn create(name: &str, root: Option<PathBuf>) -> Scratch {
        let base = std::env::var_os("WG_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "wg-macos-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&base);
        let root = root.unwrap_or_else(|| base.join("root"));
        std::fs::create_dir_all(&root).unwrap();
        for dir in ["service", "indexes"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        Scratch {
            root,
            service: base.join("service"),
            indexes: base.join("indexes"),
            events: base.join("events.ndjson"),
            base,
        }
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn wg_bin() -> &'static str {
    env!("CARGO_BIN_EXE_wg")
}

fn wg_command(scratch: &Scratch) -> Command {
    let mut command = Command::new(wg_bin());
    command
        .current_dir(&scratch.root)
        .env("WG_SERVICE_DIR", &scratch.service)
        .env("WGREP_INDEX_DIR", &scratch.indexes)
        .env("WGREP_INDEX_EVENTS", &scratch.events)
        .env_remove("WGREP_NO_INDEX")
        .env_remove("WGREP_NO_DAEMON")
        .env_remove("WG_TEXT_FORCE_UNTRUSTED");
    command
}

struct Outcome {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn read_pipe<R: Read + Send + 'static>(mut reader: R, seconds: u64) -> Vec<u8> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    rx.recv_timeout(Duration::from_secs(seconds))
        .unwrap_or_else(|_| panic!("pipe did not reach EOF within {seconds}s"))
}

fn run_optional(mut command: Command, seconds: u64) -> Option<Outcome> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait().ok()? {
            let stdout = read_pipe(child.stdout.take()?, 10);
            let stderr = read_pipe(child.stderr.take()?, 10);
            return Some(Outcome {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn run_bounded(command: Command, seconds: u64) -> Outcome {
    run_optional(command, seconds)
        .unwrap_or_else(|| panic!("command did not exit within {seconds}s"))
}

fn run_bounded_quiet(mut command: Command, seconds: u64) -> std::io::Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Stops the service on drop, then kills any pid still recorded.
struct Daemon {
    service: PathBuf,
    indexes: PathBuf,
    cwd: PathBuf,
    alive: bool,
}

impl Daemon {
    fn start(scratch: &Scratch) -> Daemon {
        let mut guard = Daemon {
            service: scratch.service.clone(),
            indexes: scratch.indexes.clone(),
            cwd: scratch.root.clone(),
            alive: false,
        };
        let mut command = wg_command(scratch);
        command.args(["up", scratch.root.to_str().unwrap(), "--model", "text"]);
        let outcome = run_bounded(command, 240);
        assert!(
            outcome.status.success(),
            "wg up failed: {}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        guard.alive = true;
        guard
    }

    /// Runs `wg down` with a hard limit. A timeout means the FSEvents stop
    /// path did not finish, so the test reports that instead of hanging.
    fn stop(&mut self, seconds: u64, label: &str) -> Outcome {
        let mut command = Command::new(wg_bin());
        command
            .current_dir(&self.cwd)
            .env("WG_SERVICE_DIR", &self.service)
            .env("WGREP_INDEX_DIR", &self.indexes)
            .arg("down");
        let outcome = run_optional(command, seconds).unwrap_or_else(|| {
            panic!(
                "{label}: wg down did not exit within {seconds}s; the FSEvents stop path is spinning under load"
            )
        });
        if outcome.status.success() {
            self.alive = false;
        }
        outcome
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if !self.alive {
            return;
        }
        let mut command = Command::new(wg_bin());
        command
            .current_dir(&self.cwd)
            .env("WG_SERVICE_DIR", &self.service)
            .env("WGREP_INDEX_DIR", &self.indexes)
            .arg("down");
        let _ = run_bounded_quiet(command, 30);
        let text = std::fs::read_to_string(self.service.join("service.json")).unwrap_or_default();
        if let Some(pid) = recorded_pid(&text) {
            let mut kill = Command::new("kill");
            kill.args(["-9", &pid.to_string()]);
            let _ = run_bounded_quiet(kill, 30);
        }
    }
}

fn recorded_pid(text: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value["pid"].as_u64().map(|pid| pid as u32)
}

fn query(scratch: &Scratch, args: &[&str]) -> Outcome {
    let mut command = wg_command(scratch);
    command.args(args);
    run_bounded(command, 60)
}

fn search(scratch: &Scratch, pattern: &str) -> Vec<u8> {
    let outcome = query(
        scratch,
        &[
            "--sort",
            "path",
            "-n",
            pattern,
            scratch.root.to_str().unwrap(),
        ],
    );
    assert!(
        outcome.status.success() || outcome.status.code() == Some(1),
        "search failed: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    outcome.stdout
}

fn files(scratch: &Scratch) -> Vec<u8> {
    let outcome = query(
        scratch,
        &["--files", "--sort", "path", scratch.root.to_str().unwrap()],
    );
    assert!(
        outcome.status.success(),
        "--files failed: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    outcome.stdout
}

fn full_scan(scratch: &Scratch, pattern: &str) -> Vec<u8> {
    let mut command = wg_command(scratch);
    command
        .env("WGREP_NO_INDEX", "1")
        .env(
            "WGREP_INDEX_EVENTS",
            scratch.base.join("scan-events.ndjson"),
        )
        .args([
            "--sort",
            "path",
            "-n",
            pattern,
            scratch.root.to_str().unwrap(),
        ]);
    let outcome = run_bounded(command, 60);
    assert!(
        outcome.status.success() || outcome.status.code() == Some(1),
        "full scan failed: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    outcome.stdout
}

fn query_until(
    scratch: &Scratch,
    pattern: &str,
    predicate: impl Fn(&[u8]) -> bool,
    seconds: u64,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let output = search(scratch, pattern);
        if predicate(&output) {
            return output;
        }
        if Instant::now() > deadline {
            panic!(
                "search output never reached the expected state:\n{}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn files_until(scratch: &Scratch, predicate: impl Fn(&[u8]) -> bool, seconds: u64) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let output = files(scratch);
        if predicate(&output) {
            return output;
        }
        if Instant::now() > deadline {
            panic!(
                "--files output never reached the expected state:\n{}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn has(output: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(output).contains(needle)
}

fn daemon_is_alive(scratch: &Scratch) -> bool {
    let mut command = wg_command(scratch);
    command.args(["status", "--json"]);
    let outcome = run_bounded(command, 60);
    outcome.status.success() && scratch.service.join("service.json").is_file()
}

#[test]
fn unpaired_rename_reaches_search_and_files() {
    let scratch = Scratch::new("rename");
    scratch.write("a.rs", "fn wg_macos_rename_symbol() { let needle = 1; }\n");
    // The index skips files changed in the last two seconds.
    std::thread::sleep(Duration::from_millis(2300));
    let _daemon = Daemon::start(&scratch);
    assert!(has(&search(&scratch, "wg_macos_rename_symbol"), "a.rs"));

    std::fs::rename(scratch.root.join("a.rs"), scratch.root.join("b.rs")).unwrap();

    let output = query_until(
        &scratch,
        "wg_macos_rename_symbol",
        |out| has(out, "b.rs") && !has(out, "a.rs"),
        10,
    );
    assert!(has(&output, "b.rs"));
    assert!(!has(&output, "a.rs"));

    let listed = files_until(&scratch, |out| has(out, "b.rs") && !has(out, "a.rs"), 10);
    assert!(has(&listed, "b.rs"));
    assert!(!has(&listed, "a.rs"));
}

#[test]
fn rename_over_existing_path_replaces_the_file() {
    let scratch = Scratch::new("replace");
    scratch.write("a.rs", "fn wg_macos_alpha_symbol() { let needle = 1; }\n");
    scratch.write("c.rs", "fn wg_macos_charlie_symbol() { let needle = 1; }\n");
    std::thread::sleep(Duration::from_millis(2300));
    let _daemon = Daemon::start(&scratch);
    assert!(has(&search(&scratch, "wg_macos_alpha_symbol"), "a.rs"));
    assert!(has(&search(&scratch, "wg_macos_charlie_symbol"), "c.rs"));

    std::fs::rename(scratch.root.join("a.rs"), scratch.root.join("c.rs")).unwrap();

    let output = query_until(
        &scratch,
        "wg_macos_alpha_symbol",
        |out| has(out, "c.rs") && !has(out, "a.rs"),
        10,
    );
    assert!(has(&output, "c.rs"));
    assert!(!has(&output, "a.rs"));

    // The replaced file's old content is gone.
    let replaced = query_until(
        &scratch,
        "wg_macos_charlie_symbol",
        |out| !has(out, "c.rs"),
        10,
    );
    assert!(!has(&replaced, "c.rs"));

    let listed = files_until(&scratch, |out| has(out, "c.rs") && !has(out, "a.rs"), 10);
    assert!(has(&listed, "c.rs"));
    assert!(!has(&listed, "a.rs"));
}

/// Runs the non-UTF-8 case on `scratch`. Returns false when the filesystem
/// rejects the file name.
fn non_utf8_case(scratch: &Scratch) -> bool {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let path = scratch
        .root
        .join(OsString::from_vec(b"bad-\xff.rs".to_vec()));
    if let Err(error) = std::fs::write(&path, "fn wg_macos_non_utf8_symbol() { let needle = 1; }\n")
    {
        eprintln!("non-UTF-8 file name rejected: {error}");
        return false;
    }
    std::thread::sleep(Duration::from_millis(2300));
    let _daemon = Daemon::start(scratch);

    assert!(daemon_is_alive(scratch), "the service stopped after wg up");
    assert_eq!(
        search(scratch, "wg_macos_non_utf8_symbol"),
        full_scan(scratch, "wg_macos_non_utf8_symbol"),
        "the daemon result must equal the full scan"
    );

    std::fs::write(&path, "fn wg_macos_non_utf8_symbol() { let needle = 2; }\n").unwrap();
    let expected = full_scan(scratch, "wg_macos_non_utf8_symbol");
    let output = query_until(
        scratch,
        "wg_macos_non_utf8_symbol",
        |out| out == expected.as_slice(),
        10,
    );
    assert_eq!(output, expected);
    assert!(
        daemon_is_alive(scratch),
        "the watcher panicked on a non-UTF-8 event path"
    );
    true
}

/// A small HFS+ image mounted at a known path. Some runner filesystems reject
/// non-UTF-8 names, so the test uses this image when it runs in CI.
struct HfsVolume {
    dir: PathBuf,
    mount: PathBuf,
}

impl HfsVolume {
    fn new() -> Option<HfsVolume> {
        let dir = std::env::temp_dir().join(format!(
            "wg-macos-hfs-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let image = dir.join("volume.dmg");
        let mount = dir.join("mnt");
        std::fs::create_dir_all(&mount).ok()?;
        let ok = |command: Command| run_optional(command, 120).map(|out| out.status.success());

        let mut create = Command::new("hdiutil");
        create
            .args([
                "create", "-quiet", "-size", "16m", "-fs", "HFS+", "-volname", "wgmacos",
            ])
            .arg(&image);
        if ok(create) != Some(true) {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }

        let mut attach = Command::new("hdiutil");
        attach
            .args([
                "attach",
                "-quiet",
                "-nobrowse",
                "-noverify",
                "-readwrite",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(&image);
        if ok(attach) != Some(true) {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        Some(HfsVolume { dir, mount })
    }

    fn root(&self) -> PathBuf {
        self.mount.join("root")
    }
}

impl Drop for HfsVolume {
    fn drop(&mut self) {
        let mut detach = Command::new("hdiutil");
        detach.args(["detach", "-quiet", "-force"]).arg(&self.mount);
        let _ = run_optional(detach, 120);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn non_utf8_file_name_does_not_panic_the_watcher() {
    let direct = Scratch::new("non-utf8");
    if non_utf8_case(&direct) {
        eprintln!("non-UTF-8 file name accepted by the runner filesystem");
        return;
    }
    drop(direct);

    if std::env::var_os("CI").is_none() {
        eprintln!(
            "skipping: filesystem rejects non-UTF-8 file names (set CI=true for an HFS+ image)"
        );
        return;
    }
    let Some(volume) = HfsVolume::new() else {
        eprintln!("skipping: HFS+ fallback image is unavailable");
        return;
    };
    let fallback = Scratch::at("non-utf8-hfs", volume.root());
    if non_utf8_case(&fallback) {
        eprintln!("non-UTF-8 file name accepted by the HFS+ fallback");
        return;
    }
    eprintln!("skipping: HFS+ fallback also rejects non-UTF-8 file names");
}

/// The FSEvents stop path waits for the stream run loop to become idle. Run
/// that path 20 times while several threads write files, and fail with a clear
/// message if `wg down` does not exit within 30 seconds.
#[test]
fn repeated_down_under_write_load_does_not_spin() {
    const ROUNDS: usize = 20;
    const WRITERS: usize = 4;
    const FILES_PER_WRITER: usize = 32;

    let started = Instant::now();
    for round in 0..ROUNDS {
        let scratch = Scratch::new(&format!("stop-spin-{round}"));
        scratch.write(
            "seed.rs",
            "fn wg_macos_stop_spin_seed() { let needle = 1; }\n",
        );
        let mut daemon = Daemon::start(&scratch);

        let barrier = Arc::new(Barrier::new(WRITERS + 1));
        let mut writers = Vec::with_capacity(WRITERS);
        for writer in 0..WRITERS {
            let root = scratch.root.clone();
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                for file in 0..FILES_PER_WRITER {
                    let path = root.join(format!("burst-{writer}-{file}.rs"));
                    std::fs::write(
                        &path,
                        format!("fn wg_macos_stop_spin_{writer}_{file}() {{ let needle = 1; }}\n"),
                    )
                    .unwrap();
                }
            }));
        }
        // Start the writes first, then stop the daemon while they are active.
        barrier.wait();
        let outcome = daemon.stop(30, &format!("round {round}"));
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(
            outcome.status.success(),
            "wg down failed on round {round}: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
    }
    eprintln!(
        "{ROUNDS} rounds of write load plus wg down took {:?}",
        started.elapsed()
    );
}
