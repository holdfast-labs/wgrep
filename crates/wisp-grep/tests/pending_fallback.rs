//! P0 regression: when the daemon declines a query (in-flight watcher work did
//! not drain), the client must fall back to rg's authoritative walk and still
//! find the match, recording the fallback in `WGREP_INDEX_EVENTS`.
//!
//! A stub "daemon" answers `ping` but declines every query, so the test does
//! not need to wedge a real watcher thread. It also proves the decline is not
//! treated as "no candidates": the walk must actually run.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The daemon's normalized root key: forward slashes, no trailing slash,
/// lowercased on Windows. Mirrors `index::store::root_key`.
fn root_key(root: &Path) -> String {
    let mut key = root.to_string_lossy().replace('\\', "/");
    while key.ends_with('/') {
        key.pop();
    }
    if cfg!(windows) { key.to_lowercase() } else { key }
}

/// The slot directory for `root` under `store`. Mirrors `index::store::slot`
/// (FNV-1a 64), so the test can place a `daemon.json` without the crate's
/// internals.
fn slot_dir(store: &Path, root: &Path) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root_key(root).bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    store.join(format!("{hash:016x}"))
}

/// A stub daemon: `ping` succeeds, queries are declined. Returns the port, a
/// stop flag, and the server thread handle.
fn spawn_declining_daemon() -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    spawn_stub_daemon(None)
}

fn spawn_stub_daemon(query_reply: Option<&'static str>) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_one(stream, query_reply),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        })
    };
    (port, stop, handle)
}

/// One request/response. `ping` gets `ok:true`; anything else is declined with
/// the same reason the daemon uses on a pending-work timeout.
fn serve_one(mut stream: TcpStream, query_reply: Option<&'static str>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let reply = if line.contains("\"ping\"") {
        "{\"ok\":true,\"pid\":424242}\n"
    } else if let Some(reply) = query_reply {
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
        return;
    } else {
        "{\"ok\":false,\"reason\":\"pending_timeout\"}\n"
    };
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

#[test]
fn partial_replies_fall_back_and_match_rg() {
    for (name, reply) in [
        ("partial-header", "{\"ok\":true,\"files\":1,\"candidates\":1}"),
        ("partial-body", "{\"ok\":true,\"files\":1,\"candidates\":1}\na.txt"),
    ] {
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "hello needle world\n").unwrap();
        let store = work.path().join("indexes");
        std::fs::create_dir_all(&store).unwrap();
        let (port, stop, server) = spawn_stub_daemon(Some(reply));
        let slot = slot_dir(&store, &root);
        std::fs::create_dir_all(&slot).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let info = format!(
            "{{\"port\":{port},\"token\":\"testtoken\",\"pid\":424242,\"root\":{:?},\"cwd\":{:?}}}",
            root_key(&root), cwd.to_string_lossy()
        );
        std::fs::write(slot.join("daemon.json"), info).unwrap();
        let events = work.path().join("events.ndjson");
        let actual = Command::new(env!("CARGO_BIN_EXE_wgrep"))
            .arg("needle").arg(&root)
            .env("WGREP_INDEX_DIR", &store).env("WGREP_INDEX_EVENTS", &events)
            .env_remove("WGREP_NO_DAEMON").env_remove("WGREP_NO_INDEX")
            .env_remove("RIPGREP_CONFIG_PATH").output().unwrap();
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let expected = Command::new("rg").arg("needle").arg(&root)
            .env_remove("RIPGREP_CONFIG_PATH").output().unwrap();
        assert_eq!(actual.status.code(), expected.status.code(), "{name}: status differs");
        assert_eq!(actual.stdout, expected.stdout, "{name}: output differs from rg");
        let events = std::fs::read_to_string(events).unwrap_or_default();
        assert!(events.lines().any(|line| line.contains("\"used\":\"walk\"") && line.contains("\"reason\":\"query_failed\"")),
            "{name}: fallback event missing: {events}");
    }
}

#[test]
fn daemon_decline_falls_back_to_walk() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "hello needle world\n").unwrap();

    let store = work.path().join("indexes");
    std::fs::create_dir_all(&store).unwrap();

    let (port, stop, server) = spawn_declining_daemon();
    let slot = slot_dir(&store, &root);
    std::fs::create_dir_all(&slot).unwrap();
    // Match the real daemon's recorded cwd so the client passes the cwd guard
    // and reaches the stub's RPC decline path under test.
    let cwd = std::env::current_dir().unwrap();
    let info = format!(
        "{{\"port\":{port},\"token\":\"testtoken\",\"pid\":424242,\"root\":{:?},\"cwd\":{:?}}}",
        root_key(&root),
        cwd.to_string_lossy()
    );
    std::fs::write(slot.join("daemon.json"), info).unwrap();

    let events = work.path().join("events.ndjson");
    let output = Command::new(env!("CARGO_BIN_EXE_wgrep"))
        .arg("needle")
        .arg(&root)
        .env("WGREP_INDEX_DIR", &store)
        .env("WGREP_INDEX_EVENTS", &events)
        .env_remove("WGREP_NO_DAEMON")
        .env_remove("WGREP_NO_INDEX")
        .env_remove("RIPGREP_CONFIG_PATH")
        .output()
        .unwrap();

    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();

    assert!(
        output.status.success(),
        "wgrep failed: {:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The fallback walk actually searched and found the match.
    assert!(stdout.contains("needle"), "authoritative walk did not run: {stdout:?}");

    let events_text = std::fs::read_to_string(&events).unwrap_or_default();
    assert!(
        events_text
            .lines()
            .any(|l| l.contains("\"used\":\"walk\"") && l.contains("\"reason\":\"query_failed\"")),
        "expected a query_failed walk fallback event, got:\n{events_text}"
    );
}
