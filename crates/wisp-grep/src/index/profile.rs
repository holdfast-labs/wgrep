//! Opt-in per-phase timing. Active only when `WGREP_PROFILE` names a file;
//! otherwise every call returns after one cached environment check. Each
//! process appends NDJSON lines to that file: the client calls `flush` once
//! (marks and counts from its global list), and the daemon calls `flush_with`
//! once per request with that request's own values.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::{Map, Value};

static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();
/// Named instants in the order they were recorded (client process only).
static MARKS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());
/// Extra fields recorded with `set` (client process only).
static FIELDS: Mutex<Vec<(&'static str, Value)>> = Mutex::new(Vec::new());

fn target() -> Option<&'static Path> {
    TARGET
        .get_or_init(|| std::env::var_os("WGREP_PROFILE").filter(|v| !v.is_empty()).map(PathBuf::from))
        .as_deref()
}

/// Records `name` at the current instant. No-op when profiling is inactive.
pub(crate) fn mark(name: &'static str) {
    if target().is_none() {
        return;
    }
    if let Ok(mut marks) = MARKS.lock() {
        marks.push((name, Instant::now()));
    }
}

/// Records an extra field for this process. No-op when profiling is inactive.
pub(crate) fn set(key: &'static str, value: Value) {
    if target().is_none() {
        return;
    }
    if let Ok(mut fields) = FIELDS.lock() {
        fields.push((key, value));
    }
}

/// Appends one line for this process: its marks (ms since the first mark),
/// the recorded fields, and CPU time where the platform reports it.
pub(crate) fn flush(proc: &str) {
    let Some(path) = target() else { return };
    let now = Instant::now();
    let marks: Vec<(&'static str, Instant)> = MARKS.lock().map(|m| m.clone()).unwrap_or_default();
    let fields: Vec<(&'static str, Value)> = FIELDS.lock().map(|f| f.clone()).unwrap_or_default();
    let origin = marks.first().map_or(now, |m| m.1);
    let mut marks_obj = Map::new();
    for (name, at) in &marks {
        marks_obj.insert((*name).to_string(), Value::from((*at - origin).as_secs_f64() * 1000.0));
    }
    marks_obj.insert("end".to_string(), Value::from((now - origin).as_secs_f64() * 1000.0));
    let mut obj = Map::new();
    obj.insert("proc".to_string(), Value::from(proc));
    obj.insert("pid".to_string(), Value::from(std::process::id()));
    obj.insert("marks".to_string(), Value::Object(marks_obj));
    for (key, value) in fields {
        obj.insert(key.to_string(), value);
    }
    if let Some((user_ms, kernel_ms)) = cpu_ms() {
        obj.insert("cpu_user_ms".to_string(), Value::from(user_ms));
        obj.insert("cpu_kernel_ms".to_string(), Value::from(kernel_ms));
    }
    write_line(path, &Value::Object(obj));
}

/// Appends one line built from `fields` (an object), tagged with `proc` and
/// this process's pid. Used by the daemon, which is multi-threaded and so
/// cannot use the global marks.
pub(crate) fn flush_with(proc: &str, fields: Value) {
    let Some(path) = target() else { return };
    let mut obj = Map::new();
    obj.insert("proc".to_string(), Value::from(proc));
    obj.insert("pid".to_string(), Value::from(std::process::id()));
    if let Value::Object(extra) = fields {
        for (key, value) in extra {
            obj.insert(key, value);
        }
    }
    write_line(path, &Value::Object(obj));
}

fn write_line(path: &Path, obj: &Value) {
    let mut line = obj.to_string();
    line.push('\n');
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

/// User and kernel CPU time of this process, in ms.
#[cfg(windows)]
fn cpu_ms() -> Option<(f64, f64)> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let (mut creation, mut exit, mut kernel, mut user) =
        (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    // SAFETY: the out-pointers are valid for the duration of the call, and
    // the pseudo-handle from GetCurrentProcess needs no closing.
    let ok = unsafe {
        GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user)
    };
    if ok == 0 {
        return None;
    }
    // FILETIME counts 100 ns units.
    let to_ms = |t: &FileTime| (((t.high as u64) << 32) | t.low as u64) as f64 / 10_000.0;
    Some((to_ms(&user), to_ms(&kernel)))
}

#[cfg(target_os = "linux")]
fn cpu_ms() -> Option<(f64, f64)> {
    // /proc/self/stat fields 14 (utime) and 15 (stime), in clock ticks. The
    // comm field (2) may contain spaces, so parse after the last ')'. The
    // first token after ')' is field 3, so field N is at index N - 3.
    // Assumes USER_HZ = 100 (ticks * 10 = ms).
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(14 - 3)?.parse().ok()?;
    let stime: u64 = fields.get(15 - 3)?.parse().ok()?;
    Some((utime as f64 * 10.0, stime as f64 * 10.0))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_ms() -> Option<(f64, f64)> {
    None
}
