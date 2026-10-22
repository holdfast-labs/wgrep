//! `wg up` retry tests: one call retries a failed background build.
//!
//! A test-only marker named by `WG_TEST_FAIL_BUILD` makes a build fail while
//! it exists. The marker text appears in the build error, so a test can prove
//! that a later `wg up` ran a fresh build instead of replaying the old error.
//! Every launch is bounded, and every daemon is stopped on drop.

#![cfg(debug_assertions)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Scratch {
    base: PathBuf,
    root: PathBuf,
    service: PathBuf,
    indexes: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let base = std::env::var_os("WG_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "wg-up-retry-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("root/src")).unwrap();
        for dir in ["service", "indexes"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        let scratch = Scratch {
            root: base.join("root"),
            service: base.join("service"),
            indexes: base.join("indexes"),
            base,
        };
        scratch.write("src/a.rs", "fn alpha() { let needle = 1; }\n");
        scratch.write("src/b.rs", "fn beta() { let other = 2; }\n");
        scratch
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn marker(&self) -> PathBuf {
        self.base.join("fail-build")
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
        .env("WG_TEST_FAIL_BUILD", scratch.marker())
        .env_remove("WGREP_NO_INDEX")
        .env_remove("WGREP_NO_DAEMON");
    command
}

struct Outcome {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Read one child pipe without waiting forever. The daemon must not inherit
/// these pipes; if it does, this bound reports the problem instead of hanging.
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

/// Run one command with a hard bound. Never waits forever on a child pipe.
fn run_bounded(mut command: Command, seconds: u64) -> Outcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let stdout = read_pipe(child.stdout.take().expect("stdout pipe"), 10);
            let stderr = read_pipe(child.stderr.take().expect("stderr pipe"), 10);
            return Outcome {
                status,
                stdout,
                stderr,
            };
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command did not exit within {seconds}s");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct Daemon {
    service: PathBuf,
    indexes: PathBuf,
    cwd: PathBuf,
    alive: bool,
}

impl Daemon {
    fn guard(scratch: &Scratch) -> Daemon {
        Daemon {
            service: scratch.service.clone(),
            indexes: scratch.indexes.clone(),
            cwd: scratch.root.clone(),
            alive: true,
        }
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
            #[cfg(windows)]
            {
                let mut kill = Command::new("taskkill");
                kill.args(["/PID", &pid.to_string(), "/F"]);
                let _ = run_bounded_quiet(kill, 30);
            }
            #[cfg(unix)]
            {
                let mut kill = Command::new("kill");
                kill.args(["-9", &pid.to_string()]);
                let _ = run_bounded_quiet(kill, 30);
            }
        }
    }
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

fn recorded_pid(text: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value["pid"].as_u64().map(|pid| pid as u32)
}

fn up(scratch: &Scratch) -> Outcome {
    let mut command = wg_command(scratch);
    command.args(["up", scratch.root.to_str().unwrap(), "--model", "text"]);
    run_bounded(command, 240)
}

fn root_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn status_error(scratch: &Scratch) -> Option<String> {
    let key = root_key(&scratch.root);
    let mut command = wg_command(scratch);
    command.args(["status", "--json"]);
    let outcome = run_bounded(command, 60);
    assert!(outcome.status.success(), "status failed");
    let value: serde_json::Value = serde_json::from_slice(&outcome.stdout).expect("status json");
    value["roots"]
        .as_array()?
        .iter()
        .find(|root| root["path"].as_str().is_some_and(|p| Path::new(p) == key))
        .and_then(|root| root["error"].as_str())
        .map(str::to_owned)
}

fn text(outcome: &Outcome) -> (String, String) {
    (
        String::from_utf8_lossy(&outcome.stdout).into_owned(),
        String::from_utf8_lossy(&outcome.stderr).into_owned(),
    )
}

#[test]
fn one_up_retries_a_failed_build_after_the_cause_goes_away() {
    let scratch = Scratch::new("retry");
    std::fs::write(scratch.marker(), "first-attempt").unwrap();
    let _daemon = Daemon::guard(&scratch);

    let first = up(&scratch);
    let (stdout, stderr) = text(&first);
    assert!(
        !first.status.success(),
        "a failed build must fail the call\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("test-only build failure: first-attempt"),
        "unexpected failure text: {stderr}"
    );
    let error = status_error(&scratch).expect("status must keep the failed build error");
    assert!(
        error.contains("first-attempt"),
        "status error does not name the failed build: {error}"
    );

    std::fs::remove_file(scratch.marker()).unwrap();
    let second = up(&scratch);
    let (stdout, stderr) = text(&second);
    assert!(
        second.status.success(),
        "one up after the cause is gone must retry and succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("Ready:"),
        "retry did not report a ready root: {stdout}"
    );
}

#[test]
fn persistent_failure_returns_the_new_error_on_each_single_up() {
    let scratch = Scratch::new("persistent");
    let _daemon = Daemon::guard(&scratch);

    for tag in ["attempt-one", "attempt-two"] {
        std::fs::write(scratch.marker(), tag).unwrap();
        let outcome = up(&scratch);
        let (stdout, stderr) = text(&outcome);
        assert!(
            !outcome.status.success(),
            "a persistent failure must fail the call ({tag})\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(
            stderr.contains(&format!("test-only build failure: {tag}")),
            "up did not run a fresh build for {tag}; stderr: {stderr}"
        );
    }
}
