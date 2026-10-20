//! Daemon client: locate, ping, rank, start/stop. Any failure returns an
//! error to the caller — the query path never silently degrades.
//!
//! Cold-start safety (review P0): `ensure_serving` serializes concurrent
//! starters on an OS-HELD advisory file lock (`startup.lock`, via
//! `std::fs::File::try_lock` — `flock`/`LockFileEx`, cross-platform).
//! The lock is held, never unlinked, for the whole spawn-and-wait window;
//! readiness is double-checked UNDER the held guard. The OS releases the
//! lock on process death, so there is no stale-lock state at all: no
//! `/proc` probing (broken on macOS), no age/TTL heuristic (which could
//! steal a live-but-slow starter), no lockfile create/delete races, and
//! nothing for a waiter to remove — waiters only wait on `ping`.
//! 16/32 simultaneous cold clients therefore converge on one PID.
//! A second guard, `serve.lock`, is held by the daemon itself for its
//! whole lifetime so even a manual duplicate `--serve` is refused (see
//! `daemon::serve`); the two guards are separate files so the parent
//! (holding `startup.lock` while polling) can never deadlock with the
//! child (holding `serve.lock` while serving).
//!
//! Identity: `ping`/`stats` trust a daemon only when the live reply's
//! `pid` + `proto` match the slot metadata — a stale `daemon.json`
//! (dead pid, recycled pid, half-written publish) never validates.
//! Frames are bounded on both directions (64 KiB request, 256 MiB reply
//! cap) with read + write deadlines, so a hung daemon cannot hang a
//! client forever.

use std::fs::File;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::slots;
use super::chunks::{self, Chunking};
use super::PROTO;

/// Hard cap on one request line: rank queries are small text.
const MAX_REQUEST_BYTES: usize = 64 << 10;
/// Hard cap on one reply frame. Replies hold at most 50 passages; one
/// pathological chunk can approach `MAX_FILE_BYTES` (16 MiB), so the cap
/// is generous but finite — a runaway daemon cannot OOM the client.
const MAX_REPLY_BYTES: usize = 256 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Whole-frame reply deadline: the documented 120 s resource contract.
/// (A 20 KiB reply under the old one-byte loop cost ~20k syscalls of
/// pure latency; the buffered reader in `iocapped` keeps the same cap
/// and the same bound with ~3 fills.)
const FRAME_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Cold-start budget: model download + full corpus build on a slow link.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(900);

fn slot_for_read(root: &Path, mode: Chunking) -> Option<(PathBuf, serde_json::Value)> {
    let store = slots::store_dir()?;
    let slot = store.join(fn_hash(root, mode));
    let info: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(slot.join("daemon.json")).ok()?).ok()?;
    if info.get("root").and_then(|v| v.as_str()) != Some(slots::root_key(root).as_str()) {
        return None;
    }
    if info.get("chunking").and_then(|v| v.as_str()) != Some(chunks::chunking_name(mode)) { return None; }
    Some((slot, info))
}

fn fn_hash(root: &Path, mode: Chunking) -> String {
    // Same FNV-1a64 as slots::slot_for (kept local so this client never
    // depends on slot-creation side effects: no directories are made).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in slots::root_key(root).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    for b in chunks::chunking_name(mode).bytes() { h ^= u64::from(b); h = h.wrapping_mul(0x100000001b3); }
    format!("{h:016x}")
}

fn connect(info: &serde_json::Value) -> Option<(TcpStream, String)> {
    let port = u16::try_from(info.get("port")?.as_u64()?).ok()?;
    let token = info.get("token")?.as_str()?.to_string();
    let stream =
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), CONNECT_TIMEOUT)
            .ok()?;
    let _ = stream.set_nodelay(true);
    Some((stream, token))
}

/// Bounded round-trip: capped request write with a write deadline, capped
/// buffered reply frame with a whole-frame deadline. `None` on any I/O,
/// auth, or framing failure — never an unbounded read.
fn roundtrip(info: &serde_json::Value, msg: serde_json::Value) -> Option<serde_json::Value> {
    let (mut stream, token) = connect(info)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok()?;
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
    let line = super::iocapped::read_capped_frame(&stream, MAX_REPLY_BYTES, FRAME_TIMEOUT).ok()?;
    serde_json::from_slice(&line).ok()
}

/// A readable reply is trusted only when it echoes the slot's pid and the
/// protocol version we speak. Anything else (stale metadata, recycled
/// pid answering on the old port, half-written publish) is rejection.
fn trusted(info: &serde_json::Value, reply: &serde_json::Value, mode: Chunking) -> Option<u64> {
    if reply.get("ok")?.as_bool()? {
        let pid = reply.get("pid")?.as_u64()?;
        let proto = reply.get("proto")?.as_u64()? as u32;
        if Some(pid) == info.get("pid")?.as_u64() && proto == PROTO
            && info.get("chunking")?.as_str()? == chunks::chunking_name(mode)
            && reply.get("chunking")?.as_str()? == chunks::chunking_name(mode) {
            return Some(pid);
        }
    }
    None
}

/// Daemon pid if a live, identity-validated daemon answers for `root`.
pub(crate) fn ping(root: &Path, mode: Chunking) -> Option<u64> {
    let (_, info) = slot_for_read(root, mode)?;
    let reply = roundtrip(&info, serde_json::json!({ "ping": true }))?;
    trusted(&info, &reply, mode)
}

pub(crate) fn stats(root: &Path, mode: Chunking) -> Option<serde_json::Value> {
    let (_, info) = slot_for_read(root, mode)?;
    let reply = roundtrip(&info, serde_json::json!({ "stats": true }))?;
    trusted(&info, &reply, mode)?;
    Some(reply)
}

pub(crate) fn shutdown(root: &Path, mode: Chunking) -> Option<u64> {
    let (slot, info) = slot_for_read(root, mode)?;
    let reply = roundtrip(&info, serde_json::json!({ "shutdown": true }))?;
    let pid = trusted(&info, &reply, mode)?;
    // Backup metadata cleanup: the daemon removes its own daemon.json
    // before replying (single-writer protocol), so this only matters for
    // daemons that died without shutting down — and then only if the file
    // still names the pid we just stopped (a racing cold start may have
    // published its successor already; that metadata is never touched).
    remove_daemon_json_if_pid(&slot, pid);
    Some(pid)
}

/// Remove `daemon.json` only when it still names `pid`. Never removes a
/// successor daemon's metadata.
fn remove_daemon_json_if_pid(slot: &Path, pid: u64) {
    let path = slot.join("daemon.json");
    let cur: Option<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).ok();
    if cur.as_ref().and_then(|v| v.get("pid")).and_then(|v| v.as_u64()) == Some(pid) {
        let _ = std::fs::remove_file(&path);
    }
}

pub(crate) struct Ranked {
    pub reply: serde_json::Value,
}

/// Rank `query` on the daemon serving `root`. Returns the raw reply object
/// (`ok` + `results` with path/start/end/content/scores, or `ok:false`).
pub(crate) fn rank(
    root: &Path,
    query: &str,
    prefix: &str,
    top_n: usize,
    model: &str,
    mode: Chunking,
) -> anyhow::Result<Ranked> {
    let (_, info) =
        slot_for_read(root, mode).ok_or_else(|| anyhow::anyhow!("wgrep-sem: no daemon serving {}", root.display()))?;
    let reply = roundtrip(
        &info,
        serde_json::json!({ "rank": { "query": query, "prefix": prefix, "top_n": top_n, "model": model } }),
    )
    .ok_or_else(|| anyhow::anyhow!("wgrep-sem: daemon for {} did not answer", root.display()))?;
    if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let reason = reply.get("reason").and_then(|v| v.as_str()).unwrap_or("unknown");
        anyhow::bail!("wgrep-sem: daemon error: {reason}");
    }
    Ok(Ranked { reply })
}

/// Per-slot startup guard file. Persistent: created once, never unlinked
/// (unlinking under waiters is an inode race — a waiter could hold the
/// deleted inode while a successor locks a new file, defeating mutual
/// exclusion). Exclusion comes from the OS-held advisory lock on the open
/// handle (`File::try_lock`: `flock` on Unix, `LockFileEx` on Windows),
/// which the kernel releases on process death — so a dead starter can
/// never leave a stale lock behind, and no takeover heuristic exists to
/// steal a live one.
fn startup_lock_file(slot: &Path) -> anyhow::Result<File> {
    std::fs::create_dir_all(slot)?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(slot.join("startup.lock"))
        .map_err(|e| e.into())
}

/// Ensure a daemon serves `root`, spawning `wgrep-sem --serve` detached
/// when no live daemon answers. Concurrent cold starters converge on one
/// daemon: exactly one holds the exclusive `startup.lock`; the rest poll
/// `ping` until the winner's daemon answers (or the budget expires).
/// The guard is held across the readiness double-check, the spawn, and the
/// whole readiness poll, then released on return. A dead starter releases
/// the lock via the OS itself — waiters never take over, only the next
/// acquirer proceeds, so a slow-but-live starter can never be duplicated.
/// Never builds anything implicitly beyond the daemon's own startup build.
pub(crate) fn ensure_serving(root: &Path, cache: &Path, mode: Chunking) -> anyhow::Result<()> {
    if ping(root, mode).is_some() {
        return Ok(());
    }
    let Some(store) = slots::store_dir() else {
        anyhow::bail!("wgrep-sem: no slot directory; set WSEM_DIR");
    };
    let slot = store.join(fn_hash(root, mode));
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    // Serialize starters on the OS-held guard. No takeover, no removal:
    // while the guard is held elsewhere we only wait for its daemon.
    let guard = startup_lock_file(&slot)?;
    loop {
        // `TryLockError` carries no kind; ANY error means "not acquired".
        // Treating all errors as contended is the safe direction: waiters
        // poll `ping` and never spawn a duplicate. A permanently broken
        // lock surfaces as the bounded wait timing out, never as two
        // daemons.
        match guard.try_lock() {
            Ok(()) => break,
            Err(_) => {
                if ping(root, mode).is_some() {
                    guard.unlock().ok();
                    return Ok(()); // winner's daemon answers: reuse it.
                }
                if Instant::now() > deadline {
                    guard.unlock().ok();
                    anyhow::bail!("wgrep-sem: timed out waiting for a concurrent daemon start");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    // Guard HELD from here to return: re-check readiness under it (this
    // closes the ping-then-spawn race — without it, 32 cold clients spawn
    // 32 daemons and orphan 31), then spawn and poll.
    if ping(root, mode).is_some() {
        guard.unlock().ok();
        return Ok(());
    }
    // Claim the slot early: a root guard written before spawn lets a
    // racing client distinguish "starting" from "absent" via the lock.
    if let Err(e) = claim_slot(&slot, root, mode) {
        anyhow::bail!("wgrep-sem: slot for {} is claimed by another root: {e:#}", root.display());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--serve").arg(root).arg("--chunking").arg(chunks::chunking_name(mode)).arg("--cache-dir").arg(cache);
    // Detached stdio: stdin + stdout to null, stderr to the slot-owned
    // log. Inheriting the caller's pipes would wedge CLI callers whose
    // `capture_output` waits for EOF that a long-lived daemon holds open.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    let log_path = slot.join("daemon-stderr.log");
    match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => {
            cmd.stderr(f);
        }
        Err(_) => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd.spawn()?;
    // The startup guard stays HELD for this whole poll (released on
    // return): no second starter can spawn while we wait, and the OS
    // releases it for us if we die.
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            guard.unlock().ok();
            anyhow::bail!("wgrep-sem: daemon exited during startup ({status})");
        }
        if ping(root, mode).is_some() {
            // Harness-visible readiness: the guard releases on return and
            // waiters converge on this PID.
            std::mem::forget(child);
            guard.unlock().ok();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    // Startup failure cleans up ONLY what we own: our child. Never touches
    // a successor daemon (its ping would have succeeded above).
    let _ = child.kill();
    let _ = child.wait();
    guard.unlock().ok();
    anyhow::bail!("wgrep-sem: daemon for {} did not become ready", root.display())
}

/// Write the slot `root` guard, refusing a hash collision with another
/// root. Same rule as `slots::slot_for`; called with the startup guard
/// held, which already serializes starters for this slot.
fn claim_slot(slot: &Path, root: &Path, mode: Chunking) -> anyhow::Result<()> {
    std::fs::create_dir_all(slot)?;
    let guard = slot.join("root");
    let key = format!("{}\n{}", slots::root_key(root), chunks::chunking_name(mode));
    match std::fs::read_to_string(&guard) {
        Ok(prev) if prev == key => Ok(()),
        Ok(_) => anyhow::bail!("slot hash collision"),
        Err(_) => {
            std::fs::write(&guard, key)?;
            Ok(())
        }
    }
}
