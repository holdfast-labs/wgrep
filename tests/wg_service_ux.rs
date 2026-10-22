//! Clean-install service UX tests: early path checks, daemon PID, clear
//! no-daemon errors, actionable model errors, and display-only path prefixes.
//!
//! Every launch is bounded, and every daemon is stopped on drop.

use std::io::Read;
use std::path::PathBuf;
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
                "wg-service-ux-{}-{}-{name}",
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
        scratch
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
        .env_remove("WGREP_NO_INDEX")
        .env_remove("WGREP_NO_DAEMON")
        .env_remove("WG_TEST_FAIL_BUILD");
    command
}

struct Outcome {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Outcome {
    fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
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

fn run(scratch: &Scratch, args: &[&str], seconds: u64) -> Outcome {
    let mut command = wg_command(scratch);
    command.args(args);
    run_bounded(command, seconds)
}

fn up_json(scratch: &Scratch, model: Option<&str>) -> Outcome {
    let root = scratch.root.to_str().unwrap();
    let mut args = vec!["up", root, "--json"];
    if let Some(model) = model {
        args.push("--model");
        args.push(model);
    }
    run(scratch, &args, 240)
}

#[test]
fn missing_path_does_not_start_a_daemon_and_no_daemon_errors_are_clear() {
    let scratch = Scratch::new("missing");
    let missing = scratch.base.join("does-not-exist");

    let failed = run(
        &scratch,
        &["up", missing.to_str().unwrap(), "--model", "text"],
        120,
    );
    assert!(
        !failed.status.success(),
        "up on a missing path must fail: {}",
        failed.out()
    );
    assert!(
        failed.err().contains("not a directory"),
        "unexpected error: {}",
        failed.err()
    );
    assert!(
        !scratch.service.join("service.json").exists(),
        "a failed up started a daemon"
    );

    let status = run(&scratch, &["status", "--json"], 60);
    assert_eq!(
        status.status.code(),
        Some(2),
        "status without a daemon must exit 2: {}",
        status.err()
    );
    assert!(
        status.err().contains("no wg service is running"),
        "status error is not clear: {}",
        status.err()
    );
    assert!(
        !scratch.service.join("service.json").exists(),
        "status started a daemon"
    );

    let first = run(&scratch, &["down"], 60);
    assert_eq!(first.status.code(), Some(0), "down must be idempotent");
    assert!(
        first.out().contains("No wg service is running."),
        "down output is not clear: {}",
        first.out()
    );
    let second = run(&scratch, &["down"], 60);
    assert_eq!(
        second.status.code(),
        Some(0),
        "a second down must succeed: {}",
        second.err()
    );
}

#[test]
fn a_second_up_reports_the_real_daemon_pid() {
    let scratch = Scratch::new("pid");
    let _daemon = Daemon::guard(&scratch);

    let first = up_json(&scratch, Some("text"));
    assert!(first.status.success(), "first up failed: {}", first.err());
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).expect("first json");
    let pid = first_json["pid"].as_u64().expect("first up has no pid");
    assert!(pid > 0);

    let second = up_json(&scratch, Some("text"));
    assert!(
        second.status.success(),
        "second up failed: {}",
        second.err()
    );
    let second_json: serde_json::Value =
        serde_json::from_slice(&second.stdout).expect("second json");
    assert_eq!(
        second_json["pid"].as_u64(),
        Some(pid),
        "second up --json has the wrong pid: {second_json}"
    );

    let plain = run(
        &scratch,
        &["up", scratch.root.to_str().unwrap(), "--model", "text"],
        120,
    );
    assert!(plain.status.success(), "plain up failed: {}", plain.err());
    assert!(
        plain.out().contains(&format!("PID: {pid}")),
        "plain up did not print the daemon PID: {}",
        plain.out()
    );
    #[cfg(windows)]
    {
        assert!(
            !plain.out().contains(r"\\?\"),
            "plain up printed a verbatim path: {}",
            plain.out()
        );
        let status = run(&scratch, &["status"], 60);
        assert!(status.status.success());
        assert!(
            !status.out().contains(r"\\?\"),
            "plain status printed a verbatim path: {}",
            status.out()
        );
        assert!(
            plain
                .out()
                .contains(&format!("Root: {}", scratch.root.display())),
            "plain up did not show the root path: {}",
            plain.out()
        );
    }
}

#[test]
fn model_mismatch_says_how_to_change_the_root_model() {
    let scratch = Scratch::new("model");
    let _daemon = Daemon::guard(&scratch);

    let up = up_json(&scratch, None);
    assert!(up.status.success(), "auto up failed: {}", up.err());

    let query = run(
        &scratch,
        &[
            "where is alpha",
            scratch.root.to_str().unwrap(),
            "--model",
            "text",
            "--json",
        ],
        120,
    );
    assert_eq!(
        query.status.code(),
        Some(2),
        "a text query on a code root must fail: {}",
        query.out()
    );
    let error = query.err();
    assert!(
        error.contains("does not match the attached root model code"),
        "model error is not specific: {error}"
    );
    assert!(
        error.contains("wg up --model text"),
        "model error is not actionable: {error}"
    );

    let change = run(
        &scratch,
        &["up", scratch.root.to_str().unwrap(), "--model", "text"],
        120,
    );
    assert_eq!(
        change.status.code(),
        Some(2),
        "changing the root model without a restart must fail"
    );
    let error = change.err();
    assert!(
        error.contains("already attached with model auto"),
        "up model error is not specific: {error}"
    );
    assert!(
        error.contains("wg down, then wg up --model text"),
        "up model error is not actionable: {error}"
    );
}
