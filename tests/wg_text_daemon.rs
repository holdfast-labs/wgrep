//! Phase B text-route tests: daemon candidates, freshness and fallback.
//!
//! These tests start a real `wg` service with an isolated `WG_SERVICE_DIR` and
//! `WGREP_INDEX_DIR`. Every launch is bounded, and every daemon is stopped on
//! drop. On Windows, one test launches `wg up` with inheritable stdout/stderr
//! pipes and proves that the daemon does not keep them open.

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
    events: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let base = std::env::var_os("WG_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "wg-text-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("root")).unwrap();
        for dir in ["service", "indexes"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        Scratch {
            root: base.join("root"),
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

    /// A tiny tree. `.gitignore` applies only inside a git repository, so the
    /// fixture initializes one when `git` is available.
    fn fixture(&self) {
        self.write("src/a.rs", "fn alpha() { let needle = 1; }\n");
        self.write("src/b.rs", "fn beta() { let other = 2; }\n");
        self.write("docs/c.md", "# doc\nno match here\n");
        let _ = run_bounded(
            {
                let mut c = Command::new("git");
                c.arg("init").arg("-q").current_dir(&self.root);
                c
            },
            30,
        );
        self.write(".gitignore", "ignored.rs\n");
        // The index skips files changed in the last two seconds (a racy write
        // may not match the captured content). Let the fixture settle so the
        // build records real index entries, not only reconcile-added files.
        std::thread::sleep(Duration::from_millis(2300));
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
            let stdout = read_pipe(child.stdout.take().unwrap(), 10);
            let stderr = read_pipe(child.stderr.take().unwrap(), 10);
            return Outcome { status, stdout, stderr };
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command did not exit within {seconds}s");
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
    fn start(scratch: &Scratch, extra: &[(&str, &str)]) -> Daemon {
        let mut guard = Daemon {
            service: scratch.service.clone(),
            indexes: scratch.indexes.clone(),
            cwd: scratch.root.clone(),
            alive: false,
        };
        let mut command = wg_command(scratch);
        command.args([
            "up",
            scratch.root.to_str().unwrap(),
            "--model",
            "text",
        ]);
        for (key, value) in extra {
            command.env(key, value);
        }
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
        // The service removes service.json on a clean exit. If it is still
        // there, force the recorded pid.
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

fn text_query(scratch: &Scratch, args: &[&str]) -> Vec<u8> {
    let mut command = wg_command(scratch);
    command.args(args);
    let outcome = run_bounded(command, 60);
    assert!(
        outcome.status.success() || outcome.status.code() == Some(1),
        "query failed: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    outcome.stdout
}

fn full_scan(scratch: &Scratch) -> Vec<u8> {
    let mut command = wg_command(scratch);
    command
        .env("WGREP_NO_INDEX", "1")
        .env("WGREP_INDEX_EVENTS", scratch.base.join("scan-events.ndjson"))
        .args([
            "--sort",
            "path",
            "-n",
            "needle",
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

fn daemon_query(scratch: &Scratch) -> Vec<u8> {
    text_query(
        scratch,
        &[
            "--sort",
            "path",
            "-n",
            "needle",
            scratch.root.to_str().unwrap(),
        ],
    )
}

fn query_until(
    scratch: &Scratch,
    predicate: impl Fn(&[u8]) -> bool,
    seconds: u64,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let output = daemon_query(scratch);
        if predicate(&output) {
            return output;
        }
        if Instant::now() > deadline {
            panic!(
                "output never reached the expected state:\n{}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn search_events(scratch: &Scratch) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(&scratch.events).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "search")
        .collect()
}

fn has(output: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(output)
        .replace('\\', "/")
        .contains(needle)
}

#[test]
fn daemon_candidates_match_the_full_scan_and_are_used() {
    let scratch = Scratch::new("parity");
    scratch.fixture();
    let _daemon = Daemon::start(&scratch, &[]);
    let _ = std::fs::remove_file(&scratch.events);

    let daemon = daemon_query(&scratch);
    let events = search_events(&scratch);
    assert_eq!(events.len(), 1, "expected exactly one search event: {events:?}");
    assert_eq!(events[0]["used"], "daemon", "event: {}", events[0]);
    assert!(
        events[0]["reason"].is_null(),
        "a served query must have no fallback reason: {}",
        events[0]
    );
    assert!(has(&daemon, "src/a.rs"));

    // The build must have recorded real index entries, so the daemon case is
    // not merely a candidate list of reconcile-added files.
    let mut status = wg_command(&scratch);
    status.args(["status", "--json"]);
    let status = run_bounded(status, 60);
    assert!(status.status.success());
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert!(
        value["roots"][0]["text"]["files"].as_u64().unwrap_or(0) >= 3,
        "text index has no entries: {}",
        value["roots"][0]["text"]
    );

    let scan = full_scan(&scratch);
    assert_eq!(
        String::from_utf8_lossy(&daemon),
        String::from_utf8_lossy(&scan),
        "daemon candidates changed the output"
    );
}

#[test]
fn text_index_sees_edits_creates_deletes_renames_and_gitignore_changes() {
    let scratch = Scratch::new("fresh");
    scratch.fixture();
    let _daemon = Daemon::start(&scratch, &[]);
    assert!(has(&daemon_query(&scratch), "src/a.rs"));

    // Create.
    scratch.write("src/new.rs", "pub fn created() { let needle = 2; }\n");
    std::thread::sleep(Duration::from_millis(400));
    let output = query_until(&scratch, |out| has(out, "src/new.rs"), 10);
    assert!(has(&output, "src/new.rs"));

    // Edit.
    scratch.write("src/b.rs", "fn beta() { let needle = 3; }\n");
    std::thread::sleep(Duration::from_millis(400));
    assert!(has(
        &query_until(&scratch, |out| has(out, "src/b.rs"), 10),
        "src/b.rs"
    ));

    // Delete.
    std::fs::remove_file(scratch.root.join("src/new.rs")).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let output = query_until(&scratch, |out| !has(out, "src/new.rs"), 10);
    assert!(!has(&output, "src/new.rs"));

    // Rename.
    std::fs::rename(scratch.root.join("src/a.rs"), scratch.root.join("src/renamed.rs")).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let output = query_until(&scratch, |out| has(out, "src/renamed.rs"), 10);
    assert!(has(&output, "src/renamed.rs"));
    assert!(!has(&output, "src/a.rs"));

    // Ignored create: invisible while the rule is active.
    scratch.write("ignored.rs", "let needle = 4;\n");
    std::thread::sleep(Duration::from_millis(400));
    let output = query_until(&scratch, |out| has(out, "src/renamed.rs"), 10);
    assert!(!has(&output, "ignored.rs"), "an ignored file must not match");

    // .gitignore change: the file becomes visible to the next query.
    scratch.write(".gitignore", "# no rules\n");
    std::thread::sleep(Duration::from_millis(500));
    assert!(has(
        &query_until(&scratch, |out| has(out, "ignored.rs"), 15),
        "ignored.rs"
    ));
}

#[test]
fn untrusted_watcher_falls_back_to_the_full_scan() {
    let scratch = Scratch::new("fallback");
    scratch.fixture();
    let _daemon = Daemon::start(&scratch, &[("WG_TEXT_FORCE_UNTRUSTED", "1")]);
    let _ = std::fs::remove_file(&scratch.events);

    let output = daemon_query(&scratch);
    let events = search_events(&scratch);
    assert_eq!(events.len(), 1, "expected exactly one search event: {events:?}");
    assert_eq!(events[0]["used"], "walk", "event: {}", events[0]);
    assert!(
        events[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("text_index_untrusted")),
        "event: {}",
        events[0]
    );

    let scan = full_scan(&scratch);
    assert_eq!(
        String::from_utf8_lossy(&output),
        String::from_utf8_lossy(&scan),
        "the fallback must equal the full scan"
    );

    let mut status = wg_command(&scratch);
    status.args(["status", "--json"]);
    let status = run_bounded(status, 60);
    assert!(status.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(value["roots"][0]["text"]["untrusted"], true);
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr;

    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;

    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_chars: u32,
        y_chars: u32,
        fill: u32,
        flags: u32,
        show: u16,
        reserved2: u16,
        reserved_ptr: *mut u8,
        std_input: isize,
        std_output: isize,
        std_error: isize,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: isize,
        thread: isize,
        pid: u32,
        tid: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(read: *mut isize, write: *mut isize, attributes: *const SecurityAttributes, size: u32) -> i32;
        fn CreateProcessW(
            application: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            flags: u32,
            environment: *mut c_void,
            cwd: *const u16,
            startup: *const StartupInfoW,
            process: *mut ProcessInformation,
        ) -> i32;
        fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
        fn GetExitCodeProcess(handle: isize, code: *mut u32) -> i32;
        fn TerminateProcess(handle: isize, code: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn quote(command_line: &mut Vec<u16>, arg: &OsStr) {
        let text: Vec<u16> = arg.encode_wide().collect();
        let needs = text.is_empty()
            || text
                .iter()
                .any(|c| matches!(*c, 0x20 | 0x09 | 0x0a | 0x0b | 0x22));
        if !needs {
            command_line.extend(text);
            return;
        }
        command_line.push(b'"' as u16);
        let mut slashes = 0usize;
        for c in text {
            if c == b'\\' as u16 {
                slashes += 1;
            } else if c == b'"' as u16 {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
                command_line.push(c);
                slashes = 0;
            } else {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, slashes));
                command_line.push(c);
                slashes = 0;
            }
        }
        command_line.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        command_line.push(b'"' as u16);
    }

    fn environment(overrides: &[(&str, &OsStr)]) -> Vec<u16> {
        let mut pairs: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(key, _)| {
                let name = key.to_string_lossy();
                !name.is_empty() && !name.contains('=')
            })
            .collect();
        for (key, value) in overrides {
            let key = OsString::from(key);
            pairs.retain(|(name, _)| {
                !name.to_string_lossy().eq_ignore_ascii_case(&key.to_string_lossy())
            });
            pairs.push((key, value.to_os_string()));
        }
        pairs.sort_by(|a, b| {
            a.0.to_string_lossy()
                .to_uppercase()
                .cmp(&b.0.to_string_lossy().to_uppercase())
        });
        let mut block = Vec::new();
        for (key, value) in pairs {
            block.extend(key.encode_wide());
            block.push(b'=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    /// Launch `wg up` the way a shell or agent harness does: `CreateProcessW`
    /// with `bInheritHandles = TRUE` and no handle list. The stdout/stderr
    /// pipes are inheritable, so an unpatched daemon spawn would keep them
    /// open and this read would never see EOF.
    #[test]
    fn piped_wg_up_returns_and_releases_its_pipes() {
        let scratch = Scratch::new("stdio");
        scratch.fixture();

        let mut attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: ptr::null_mut(),
            inherit: 1,
        };
        let (mut out_read, mut out_write) = (0isize, 0isize);
        let (mut err_read, mut err_write) = (0isize, 0isize);
        // SAFETY: valid out pointers and an inherit-enabled attributes value.
        assert_ne!(
            unsafe { CreatePipe(&mut out_read, &mut out_write, &mut attributes, 0) },
            0
        );
        assert_ne!(
            unsafe { CreatePipe(&mut err_read, &mut err_write, &mut attributes, 0) },
            0
        );
        let nul = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")
            .unwrap();
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
        }
        // SAFETY: the NUL handle belongs to `nul`.
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    nul.as_raw_handle() as *mut c_void,
                    1,
                    1,
                )
            },
            0
        );

        let root = scratch.root.to_str().unwrap();
        let mut command_line = Vec::new();
        quote(&mut command_line, OsStr::new(wg_bin()));
        for arg in ["up", root, "--model", "text"] {
            command_line.push(b' ' as u16);
            quote(&mut command_line, OsStr::new(arg));
        }
        command_line.push(0);

        let environment = environment(&[
            ("WG_SERVICE_DIR", scratch.service.as_os_str()),
            ("WGREP_INDEX_DIR", scratch.indexes.as_os_str()),
        ]);
        let cwd = wide(scratch.root.as_os_str());
        let application = wide(OsStr::new(wg_bin()));
        let mut info = StartupInfoW {
            cb: std::mem::size_of::<StartupInfoW>() as u32,
            reserved: ptr::null_mut(),
            desktop: ptr::null_mut(),
            title: ptr::null_mut(),
            x: 0,
            y: 0,
            x_size: 0,
            y_size: 0,
            x_chars: 0,
            y_chars: 0,
            fill: 0,
            flags: STARTF_USESTDHANDLES,
            show: 0,
            reserved2: 0,
            reserved_ptr: ptr::null_mut(),
            std_input: nul.as_raw_handle() as isize,
            std_output: out_write,
            std_error: err_write,
        };
        let mut process = ProcessInformation { process: 0, thread: 0, pid: 0, tid: 0 };
        // SAFETY: all pointers refer to live values; bInheritHandles is TRUE.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                1,
                0x0800_0000 | 0x0000_0400,
                environment.as_ptr() as *mut c_void,
                cwd.as_ptr(),
                &mut info,
                &mut process,
            )
        };
        assert_ne!(created, 0, "CreateProcessW: {}", std::io::Error::last_os_error());
        // SAFETY: the handles above are owned by this test.
        unsafe {
            CloseHandle(process.thread as *mut c_void);
        }
        drop(nul);
        // The child owns the write ends now; close ours so EOF is reachable.
        unsafe {
            CloseHandle(out_write as *mut c_void);
            CloseHandle(err_write as *mut c_void);
        }

        let mut guard = Daemon {
            service: scratch.service.clone(),
            indexes: scratch.indexes.clone(),
            cwd: scratch.root.clone(),
            alive: true,
        };
        // Bound `wg up` itself: 240 s covers a cold model setup.
        // SAFETY: `process.process` is a valid process handle.
        let waited = unsafe { WaitForSingleObject(process.process, 240_000) };
        if waited == WAIT_TIMEOUT {
            unsafe {
                TerminateProcess(process.process, 1);
                CloseHandle(process.process as *mut c_void);
            }
            guard.alive = false;
            panic!("piped wg up did not return within 240s");
        }
        assert_eq!(waited, WAIT_OBJECT_0);
        let mut code = 0u32;
        // SAFETY: same handle.
        unsafe {
            GetExitCodeProcess(process.process, &mut code);
            CloseHandle(process.process as *mut c_void);
        }
        assert_eq!(code, 0, "wg up exited nonzero");

        // The daemon is running (service.json exists). Reading the pipes must
        // reach EOF within seconds; otherwise the daemon inherited them.
        let metadata = std::fs::read_to_string(scratch.service.join("service.json"))
            .expect("daemon metadata");
        assert!(recorded_pid(&metadata).is_some());

        // SAFETY: this test owns both read handles and closes them once.
        let out = unsafe { std::fs::File::from_raw_handle(out_read as *mut c_void) };
        let err = unsafe { std::fs::File::from_raw_handle(err_read as *mut c_void) };
        let out = read_pipe::<std::fs::File>(out, 5);
        let err = read_pipe::<std::fs::File>(err, 5);
        assert!(
            String::from_utf8_lossy(&out).contains("Ready:"),
            "unexpected wg up stdout: {}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            String::from_utf8_lossy(&out).contains("Text index:"),
            "wg up did not report the text index: {}",
            String::from_utf8_lossy(&out)
        );
        let _ = err;
        guard.alive = true;
    }
}
