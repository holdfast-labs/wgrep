//! A detached child must start with only descriptors 0, 1 and 2.
//!
//! The regression: a caller made a pipe write end inheritable, `wg up` passed
//! it to the daemon, and the caller's read end never reached EOF. This test
//! spawns a long-lived helper through `detached::spawn` while a pipe write end
//! is inheritable, then requires EOF while the helper still runs.

#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use wisp_grep::detached;

#[test]
fn detached_child_does_not_keep_an_inherited_pipe_open() {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` has room for two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // `libc::pipe` does not set close-on-exec, but state it explicitly so the
    // regression also holds if that default ever changes.
    // SAFETY: valid descriptor and scalar flags.
    assert_eq!(unsafe { libc::fcntl(write_fd, libc::F_SETFD, 0) }, 0);

    let log =
        std::env::temp_dir().join(format!("wisp-grep-detached-fd-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);

    let mut child = detached::spawn(
        Path::new("/bin/sh"),
        &[OsString::from("-c"), OsString::from("sleep 30")],
        &log,
    )
    .expect("spawn detached helper");

    // The parent's copy must go, or EOF is impossible even without a leak.
    // SAFETY: this process owns the write end and closes it once.
    assert_eq!(unsafe { libc::close(write_fd) }, 0);

    let mut poll = libc::pollfd {
        fd: read_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` reads and writes one `pollfd` owned by this frame.
    let ready = unsafe { libc::poll(&mut poll, 1, 5_000) };
    let error = std::io::Error::last_os_error();

    let mut byte = 0u8;
    // SAFETY: read into one owned byte. A zero return means EOF.
    let read = if ready > 0 {
        unsafe { libc::read(read_fd, &mut byte as *mut u8 as *mut libc::c_void, 1) }
    } else {
        -1
    };
    let child_alive = child.try_wait().expect("try_wait").is_none();

    // SAFETY: this process owns the read end and closes it once.
    unsafe {
        libc::close(read_fd);
    }
    // SAFETY: the pid comes from the child this test spawned.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("try_wait").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&log);

    assert!(ready > 0, "read end did not reach EOF within 5s: {error}");
    assert_eq!(read, 0, "read end carried data instead of EOF");
    assert!(
        child_alive,
        "helper exited before the EOF check; the test proves nothing"
    );
}

/// The pre-exec hook must not break the standard library's exec-error pipe:
/// a program that cannot be executed is an error from `spawn`, not a pid.
#[test]
fn detached_spawn_reports_exec_failure() {
    let log = std::env::temp_dir().join(format!(
        "wisp-grep-detached-exec-{}.log",
        std::process::id()
    ));
    let result = detached::spawn(Path::new("/nonexistent/wisp-grep-daemon"), &[], &log);
    let _ = std::fs::remove_file(&log);
    assert!(result.is_err(), "exec failure was reported as a successful spawn");
}
