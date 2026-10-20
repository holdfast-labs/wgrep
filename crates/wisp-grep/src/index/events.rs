//! Structured index-mode observability, off unless `WGREP_INDEX_EVENTS` names a
//! file.
//!
//! One JSON object per line (NDJSON) records what the search actually did:
//! whether the daemon (the default enumeration bypass) was used, or whether the
//! search fell back to authoritative indexed walking, and why. Events go to a
//! file, never stdout/stderr, so parity output is unaffected.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set when the caller requires a one-shot walk with no index or daemon.
static WALK_ONLY: AtomicBool = AtomicBool::new(false);

/// Force (or allow) the one-shot walk-only mode for this process.
pub(crate) fn set_walk_only(yes: bool) {
    WALK_ONLY.store(yes, Ordering::SeqCst);
}

/// Whether the caller forced the one-shot walk-only mode.
pub(crate) fn walk_only() -> bool {
    WALK_ONLY.load(Ordering::SeqCst)
}

/// Whether the daemon is explicitly disabled for this process. The daemon is
/// the default, so this is the only override.
pub(crate) fn daemon_disabled() -> bool {
    walk_only() || std::env::var_os("WGREP_NO_DAEMON").is_some()
}

/// Appends one NDJSON event, if `WGREP_INDEX_EVENTS` names a file.
pub(crate) fn record(fields: serde_json::Value) {
    let Some(path) = std::env::var_os("WGREP_INDEX_EVENTS") else { return };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    let _ = writeln!(file, "{fields}");
}

/// Records the authoritative (non-daemon) mode a search used. Only emitted when
/// the daemon is explicitly disabled, so each search produces exactly one event
/// (the default daemon path records its own outcome, including fallbacks).
pub(crate) fn record_walk(used: &str, reason: &str) {
    if daemon_disabled() {
        record(serde_json::json!({
            "event": "search",
            "requested": "default",
            "used": used,
            "reason": reason,
        }));
    }
}
