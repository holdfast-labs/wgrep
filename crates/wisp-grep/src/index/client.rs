//! Asks a running `wgrep --index-serve` for candidate files. Any failure
//! returns `None`, and the caller falls back to rg's walk.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{plan, profile, store};

pub(crate) struct Candidates {
    /// Files under the prefix, whether or not they may match.
    pub(crate) files: usize,
    /// Candidate paths relative to the requested prefix, `/`-separated, in
    /// rg's walk order.
    pub(crate) paths: Vec<String>,
    /// For a broad search, the thread share the daemon granted. The share is
    /// released when `lease` is dropped.
    pub(crate) threads: Option<usize>,
    pub(crate) lease: Option<TcpStream>,
}

/// Haystacks from daemons for one search, plus the thread leases that must
/// stay alive until the search finishes.
pub(crate) struct DaemonSearch<H> {
    pub(crate) haystacks: Vec<H>,
    /// Whether any file exists under the search paths.
    pub(crate) any_files: bool,
    /// The smallest thread share any daemon granted, if one was granted.
    pub(crate) threads: Option<usize>,
    pub(crate) leases: Vec<TcpStream>,
}

/// The parsed daemon.json of the daemon serving `root`, if there is one.
pub(crate) fn read_info(root: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(store::slot_for(root)?.join("daemon.json")).ok()?).ok()
}

/// Whether the daemon described by `info` runs with the same process working
/// directory as `cwd`, so its walk is proven equivalent to this client's.
///
/// The bundled `ignore` crate resolves a relative `gitdir:` against the process
/// cwd, so a daemon whose cwd differs can select a different file set. A
/// daemon.json with no `cwd` (an older daemon, or a hand-made one) is not a
/// proof and returns false. Both sides are normalized the same way as a root
/// key, so a trailing separator or a case difference on Windows does not split
/// an otherwise equal pair.
pub(crate) fn cwd_matches(info: &serde_json::Value, cwd: &Path) -> bool {
    info.get("cwd")
        .and_then(|v| v.as_str())
        .is_some_and(|recorded| store::root_key(Path::new(recorded)) == store::root_key(cwd))
}

#[cfg(test)]
mod cwd_tests {
    use std::path::Path;

    use super::cwd_matches;

    #[test]
    fn cwd_match_requires_recorded_equal_directory() {
        let info = serde_json::json!({ "cwd": "C:/work/tree/" });
        assert!(cwd_matches(&info, Path::new("C:/work/tree")));
        assert!(!cwd_matches(&info, Path::new("C:/other")));
        assert!(!cwd_matches(&serde_json::json!({}), Path::new("C:/work/tree")));
        assert!(!cwd_matches(&serde_json::json!({ "cwd": 7 }), Path::new("C:/work/tree")));
    }
}

/// Connects to the daemon described by `info`. Returns the stream and the
/// token.
fn connect(info: &serde_json::Value) -> Option<(TcpStream, String)> {
    let port = u16::try_from(info.get("port")?.as_u64()?).ok()?;
    let token = info.get("token")?.as_str()?.to_string();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    // Requests are small and written in one piece, so don't wait for Nagle.
    let _ = stream.set_nodelay(true);
    Some((stream, token))
}

/// Where a search path's index lives, found from daemon.json files.
pub(crate) struct Located {
    /// The indexed root that holds the path.
    pub(crate) root: PathBuf,
    /// The daemon.json of a daemon serving `root`, or `None` when `root` has
    /// an index but no daemon.json (the caller starts one).
    pub(crate) info: Option<serde_json::Value>,
}

/// The nearest ancestor of `abs` (or `abs` itself) with a daemon or an index.
///
/// A daemon writes daemon.json only into a slot whose index exists, and
/// records the root in it, so checking each ancestor's daemon.json first gives
/// the same answer as `store::find` with fewer file reads on the fast path.
pub(crate) fn locate(abs: &Path) -> Option<Located> {
    for d in abs.ancestors() {
        if let Some(info) = read_info(d) {
            if info.get("root").and_then(|v| v.as_str()) == Some(store::root_key(d).as_str()) {
                return Some(Located { root: d.to_path_buf(), info: Some(info) });
            }
        }
        if store::current(d).is_some() {
            return Some(Located { root: d.to_path_buf(), info: None });
        }
    }
    None
}

/// Sends one management request (`ping` or `shutdown`) to the daemon serving
/// `root`. Returns the daemon's pid if it answered.
pub(crate) fn control(root: &Path, command: &str) -> Option<u64> {
    let info = read_info(root)?;
    let (mut stream, token) = connect(&info)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut msg = serde_json::json!({ "token": token, command: true }).to_string();
    msg.push('\n');
    stream.write_all(msg.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
    if reply.get("ok")?.as_bool()? { info.get("pid")?.as_u64() } else { None }
}

/// Asks the daemon serving `root` for its `stats`: full-walk counters by
/// cause plus queue depths. Used to prove incremental freshness (see
/// `docs/WGREP_INCREMENTAL_FRESHNESS.md`, Testing). Returns the raw reply.
pub(crate) fn stats(root: &Path) -> Option<serde_json::Value> {
    let info = read_info(root)?;
    let (mut stream, token) = connect(&info)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut msg = serde_json::json!({ "token": token, "stats": true }).to_string();
    msg.push('\n');
    stream.write_all(msg.as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
    if reply.get("ok")?.as_bool()? { Some(reply) } else { None }
}

/// Sends a candidate `query` to the daemon described by `info`, over one
/// connection. `prefix` is a `/`-separated path relative to the daemon's root.
pub(crate) fn query_with(info: &serde_json::Value, query: &plan::Query, prefix: &str) -> Option<Candidates> {
    let (mut stream, token) = connect(info)?;
    profile::mark("connect_done");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let request = serde_json::json!({ "token": token, "query": plan::encode(query), "prefix": prefix });
    let mut msg = request.to_string();
    msg.push('\n');
    stream.write_all(msg.as_bytes()).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // A valid JSON prefix without its terminating newline is still a partial
    // reply. Decline it so the caller performs the normal filesystem walk.
    if !line.ends_with('\n') {
        return None;
    }
    let header: serde_json::Value = serde_json::from_str(&line).ok()?;
    profile::mark("header_received");
    if header.get("ok")?.as_bool()? != true {
        return None;
    }
    let files = usize::try_from(header.get("files")?.as_u64()?).ok()?;
    let count = usize::try_from(header.get("candidates")?.as_u64()?).ok()?;
    let threads = header.get("threads").and_then(|v| v.as_u64()).and_then(|t| usize::try_from(t).ok());
    let mut paths = Vec::with_capacity(count);
    let mut raw = Vec::new();
    for _ in 0..count {
        raw.clear();
        if reader.read_until(b'\n', &mut raw).ok()? == 0 {
            return None;
        }
        // A reply cut short (no trailing newline) must fail, not yield a
        // truncated path: the caller then walks authoritatively.
        if raw.last() != Some(&b'\n') {
            return None;
        }
        raw.pop();
        paths.push(String::from_utf8(raw.clone()).ok()?);
    }
    profile::mark("paths_received");
    let lease = threads.is_some().then_some(stream);
    Some(Candidates { files, paths, threads, lease })
}
