//! Unified `wg` front-door support: classify one invocation and, for the
//! filesystem text/regex route, run one bounded, authoritative `rg` child on
//! behalf of the local service.
//!
//! `wg QUERY [ROOT]` stays sgrep-compatible semantic ranking by default. A
//! familiar ripgrep flag (or any flag `wg` does not own) selects the lexical
//! route instead: the original argv is handed to an `rg` child so matching,
//! ignore rules, formatting, JSON output and diagnostics stay ripgrep's, never
//! a semantic approximation. The child runs inside the same authenticated
//! service process, with explicit admission, output and time bounds, and its
//! raw stdout/stderr bytes cross the IPC as base64 so newlines and non-UTF-8
//! bytes survive intact.
//!
//! Mode is decided by flags only. Natural language versus regex is never
//! guessed from punctuation, and `--` ends option parsing for both routes.

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Raw child-stdout cap. Base64 (x4/3) must stay below the service reply cap.
pub const MAX_STDOUT: usize = 8 * 1024 * 1024;
/// Raw child-stderr cap.
pub const MAX_STDERR: usize = 1024 * 1024;
/// Cap on stdin forwarded to a child.
pub const MAX_STDIN: usize = 8 * 1024 * 1024;
/// Default wall-clock bound for one child; must stay under the frame deadline.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Hard ceiling for the tunable time bound.
pub const MAX_TIMEOUT_MS: u64 = 75_000;
/// Concurrent lexical children one service process admits.
pub const MAX_CONCURRENT: usize = 4;

/// Flags `wg` itself accepts with no value.
const WG_BOOL: &[&str] = &["--json", "--explain", "-h", "--help", "-V", "--version"];
/// Flags `wg` itself accepts with a value.
const WG_VALUE: &[&str] = &["-k", "--top", "--model", "--cache-dir"];
/// `wg`-owned value flags that have no meaning in the ripgrep route.
const WG_SEMANTIC_ONLY: &[&str] = &["-k", "--top", "--model", "--explain"];

/// One parsed `wg` invocation. The program name is already removed.
pub enum Invocation {
    /// `--daemon`: run the foreground service.
    Daemon,
    /// `up`/`status`/`down`, plus the remaining arguments.
    Service {
        name: String,
        rest: Vec<String>,
    },
    /// Default sgrep-compatible semantic search.
    Semantic(Semantic),
    /// Ripgrep filesystem search: the exact argv for the child.
    Lexical {
        argv: Vec<String>,
    },
    Help,
    Version,
}

/// Parsed semantic-search arguments.
pub struct Semantic {
    pub query: String,
    pub root: Option<String>,
    pub top: usize,
    pub json: bool,
    pub explain: bool,
    pub model: String,
}

/// Classify `args` and lift the global `--cache-dir` out of every route.
pub fn classify(args: &[String]) -> Result<(Invocation, Option<String>), String> {
    // A global prefix must be removed before recognizing a subcommand. Never
    // search an rg argv for these tokens: `-e --cache-dir` is a valid pattern.
    let mut leading = 0;
    let mut prefix_cache = None;
    while let Some(arg) = args.get(leading) {
        if arg == "--cache-dir" {
            let value = args.get(leading + 1).ok_or("--cache-dir needs a value")?;
            if value.is_empty() || value.starts_with('-') {
                return Err("--cache-dir needs a directory value".into());
            }
            prefix_cache = Some(value.clone());
            leading += 2;
        } else if let Some(value) = arg.strip_prefix("--cache-dir=") {
            if value.is_empty() {
                return Err("--cache-dir needs a directory value".into());
            }
            prefix_cache = Some(value.to_owned());
            leading += 1;
        } else {
            break;
        }
    }
    let (invocation, cache) = classify_inner(&args[leading..])?;
    if prefix_cache.is_some() && cache.is_some() {
        return Err("--cache-dir must be supplied only once".into());
    }
    Ok((invocation, prefix_cache.or(cache)))
}

fn classify_inner(args: &[String]) -> Result<(Invocation, Option<String>), String> {
    if args.is_empty() {
        return Ok((Invocation::Help, None));
    }
    if args[0] == "--daemon" {
        return Ok((Invocation::Daemon, cache_dir_of(args)));
    }
    if matches!(args[0].as_str(), "up" | "status" | "down") {
        let (rest, cache) = management_args(&args[1..])?;
        if rest
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "--help" || a == "-h")
        {
            return Ok((Invocation::Help, cache));
        }
        return Ok((
            Invocation::Service {
                name: args[0].clone(),
                rest,
            },
            cache,
        ));
    }

    let mut trigger = false;
    let mut semantic_only = false;
    let mut i = 0usize;
    let mut dd = false;
    while i < args.len() {
        let a = args[i].as_str();
        if dd {
            i += 1;
            continue;
        }
        if a == "--" {
            dd = true;
            i += 1;
            continue;
        }
        if a == "-" || !a.starts_with('-') {
            i += 1;
            continue;
        }
        let (name, inline) = split_eq(a);
        if WG_BOOL.contains(&name) || name == "--daemon" {
            i += 1;
            continue;
        }
        if WG_VALUE.contains(&name) || is_short_top(a) {
            if WG_SEMANTIC_ONLY.contains(&name) || is_short_top(a) {
                semantic_only = true;
            }
            i += if inline.is_some() || is_short_top(a) {
                1
            } else {
                2
            };
            continue;
        }
        trigger = true;
        break;
    }

    let cache = cache_dir_of(args);
    if trigger {
        if semantic_only {
            return Err(
                "cannot combine semantic options (--top/-k/--model/--explain) with ripgrep \
                 options; run them as separate commands"
                    .into(),
            );
        }
        return Ok((
            Invocation::Lexical {
                argv: args.to_vec(),
            },
            None,
        ));
    }
    let semantic = parse_semantic(args)?;
    match semantic {
        SemanticParse::Help => Ok((Invocation::Help, cache)),
        SemanticParse::Version => Ok((Invocation::Version, cache)),
        SemanticParse::Args(s) => Ok((Invocation::Semantic(s), cache)),
    }
}

enum SemanticParse {
    Args(Semantic),
    Help,
    Version,
}

fn split_eq(a: &str) -> (&str, Option<&str>) {
    match a.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (a, None),
    }
}

fn is_short_top(a: &str) -> bool {
    // `-k5` and `-k=5` are value-taking `-k`; `-k` alone is handled by split_eq.
    let bytes = a.as_bytes();
    bytes.len() > 2
        && bytes[0] == b'-'
        && bytes[1] == b'k'
        && (bytes[2] == b'=' || bytes[2].is_ascii_digit())
}

fn cache_dir_of(args: &[String]) -> Option<String> {
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            return None;
        }
        if a == "--cache-dir" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = a.strip_prefix("--cache-dir=") {
            return Some(value.to_owned());
        }
        i += 1;
    }
    None
}

/// Management options have a known grammar. Do not apply this extraction to
/// arbitrary ripgrep arguments, whose pattern values can look like flags.
fn management_args(args: &[String]) -> Result<(Vec<String>, Option<String>), String> {
    let mut out = Vec::with_capacity(args.len());
    let mut cache = None;
    let mut i = 0usize;
    let mut dd = false;
    while i < args.len() {
        let a = args[i].as_str();
        if dd {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "--" {
            dd = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "--cache-dir" {
            let value = args
                .get(i + 1)
                .ok_or("--cache-dir needs a directory value")?;
            if value.is_empty() || value.starts_with('-') {
                return Err("--cache-dir needs a directory value".into());
            }
            if cache.replace(value.clone()).is_some() {
                return Err("--cache-dir must be supplied only once".into());
            }
            i += 2;
            continue;
        }
        if let Some(value) = a.strip_prefix("--cache-dir=") {
            if value.is_empty() {
                return Err("--cache-dir needs a directory value".into());
            }
            if cache.replace(value.to_owned()).is_some() {
                return Err("--cache-dir must be supplied only once".into());
            }
            i += 1;
            continue;
        }
        if a == "--model" {
            out.push(args[i].clone());
            if let Some(value) = args.get(i + 1) {
                out.push(value.clone());
            }
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    Ok((out, cache))
}

fn parse_semantic(args: &[String]) -> Result<SemanticParse, String> {
    let mut query: Option<String> = None;
    let mut root: Option<String> = None;
    let mut top = 5usize;
    let mut json = false;
    let mut explain = false;
    let mut model = "auto".to_owned();
    let mut i = 0usize;
    let mut dd = false;
    while i < args.len() {
        let a = args[i].as_str();
        if dd {
            push_positional(a, &mut query, &mut root)?;
            i += 1;
            continue;
        }
        if a == "--" {
            dd = true;
            i += 1;
            continue;
        }
        if a == "-h" || a == "--help" {
            return Ok(SemanticParse::Help);
        }
        if a == "-V" || a == "--version" {
            return Ok(SemanticParse::Version);
        }
        if a == "--json" {
            json = true;
            i += 1;
            continue;
        }
        if a == "--explain" {
            explain = true;
            i += 1;
            continue;
        }
        if a == "--cache-dir" {
            i += 2;
            continue;
        }
        if a.starts_with("--cache-dir=") {
            i += 1;
            continue;
        }
        let (name, inline) = split_eq(a);
        if name == "-k" || name == "--top" {
            let value = match inline {
                Some(v) => v.to_owned(),
                None => value_at(args, i + 1, "-k/--top")?,
            };
            top = value
                .parse::<usize>()
                .map_err(|_| format!("--top/-k needs a number, got {value:?}"))?;
            if top == 0 || top > 100 {
                return Err("--top/-k must be between 1 and 100".into());
            }
            i += if inline.is_some() { 1 } else { 2 };
            continue;
        }
        if is_short_top_value(a) {
            top = a[2..]
                .parse::<usize>()
                .map_err(|_| format!("--top/-k needs a number, got {a:?}"))?;
            if top == 0 || top > 100 {
                return Err("--top/-k must be between 1 and 100".into());
            }
            i += 1;
            continue;
        }
        if name == "--model" {
            let value = match inline {
                Some(v) => v.to_owned(),
                None => value_at(args, i + 1, "--model")?,
            };
            if !["auto", "code", "text"].contains(&value.as_str()) {
                return Err("--model must be auto, code, or text".into());
            }
            model = value;
            i += if inline.is_some() { 1 } else { 2 };
            continue;
        }
        if a.starts_with('-') && a != "-" {
            // Classification only routes unknown flags to ripgrep, so this is
            // defensive: a wg-owned flag shape that skipped the scanner.
            return Err(format!("unrecognized option {a}"));
        }
        push_positional(a, &mut query, &mut root)?;
        i += 1;
    }
    let query = query.ok_or_else(|| "QUERY is required".to_string())?;
    if query.trim().is_empty() {
        return Err("query has no searchable words".into());
    }
    if explain && !json {
        return Err("--explain requires --json".into());
    }
    Ok(SemanticParse::Args(Semantic {
        query,
        root,
        top,
        json,
        explain,
        model,
    }))
}

fn is_short_top_value(a: &str) -> bool {
    let bytes = a.as_bytes();
    bytes.len() > 2 && bytes[0] == b'-' && bytes[1] == b'k' && bytes[2].is_ascii_digit()
}

fn value_at(args: &[String], i: usize, flag: &str) -> Result<String, String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn push_positional(
    a: &str,
    query: &mut Option<String>,
    root: &mut Option<String>,
) -> Result<(), String> {
    if query.is_none() {
        *query = Some(a.to_owned());
    } else if root.is_none() {
        *root = Some(a.to_owned());
    } else {
        return Err(format!(
            "unexpected argument {a:?}; semantic search takes QUERY and one ROOT. \
             Add a ripgrep flag (-F, -e, -n, -g, ...) for filesystem search."
        ));
    }
    Ok(())
}

/// The ripgrep executable for this host: `WG_RG`, else `rg` from PATH.
pub fn rg_program() -> OsString {
    std::env::var_os("WG_RG").unwrap_or_else(|| OsString::from("rg"))
}

/// The child time bound: `WG_LEX_TIMEOUT_MS` clamped to a safe ceiling.
pub fn timeout() -> Duration {
    let ms = std::env::var("WG_LEX_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms.min(MAX_TIMEOUT_MS))
}

/// Raw result of one bounded child run.
pub struct LexOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run one `rg` child with explicit output, time and admission bounds.
///
/// Both output pipes are drained on their own threads, so a child that fills
/// one pipe while writing the other cannot deadlock. Exceeding a cap, or the
/// time bound, kills the child and returns an explicit error; output is never
/// silently truncated into a successful reply.
pub fn run(
    argv: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<LexOutput, String> {
    let program = rg_program();
    let mut child = Command::new(&program)
        .args(argv)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", program.to_string_lossy()))?;

    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().ok_or("missing child stdout pipe")?;
    let stderr = child.stderr.take().ok_or("missing child stderr pipe")?;
    let out_overflow = overflow.clone();
    let out_thread = std::thread::spawn(move || read_capped(stdout, MAX_STDOUT, out_overflow));
    let err_overflow = overflow.clone();
    let err_thread = std::thread::spawn(move || read_capped(stderr, MAX_STDERR, err_overflow));
    let in_thread = stdin.map(|bytes| {
        let bytes = bytes.to_vec();
        let mut pipe = child.stdin.take();
        std::thread::spawn(move || {
            if let Some(ref mut pipe) = pipe {
                let _ = pipe.write_all(&bytes);
            }
            drop(pipe);
        })
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("waiting for rg failed: {e}"));
            }
        }
        if overflow.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let out_result = out_thread.join();
    let err_result = err_thread.join();
    if let Some(thread) = in_thread {
        let _ = thread.join();
    }
    let (stdout, out_over) =
        out_result.map_err(|_| "stdout capture worker failed".to_owned())??;
    let (stderr, err_over) =
        err_result.map_err(|_| "stderr capture worker failed".to_owned())??;

    if out_over || err_over {
        return Err(format!(
            "rg output exceeded the service bound (stdout max {} MiB, stderr max {} MiB)",
            MAX_STDOUT / (1024 * 1024),
            MAX_STDERR / (1024 * 1024)
        ));
    }
    if timed_out {
        return Err(format!(
            "rg exceeded the {} ms time bound",
            timeout.as_millis()
        ));
    }
    let status = status.ok_or("rg stopped without an exit status")?;
    Ok(LexOutput {
        exit_code: status.code().unwrap_or(2),
        stdout,
        stderr,
    })
}

fn read_capped(
    mut reader: impl Read,
    cap: usize,
    overflow: Arc<AtomicBool>,
) -> Result<(Vec<u8>, bool), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok((buf, false)),
            Ok(n) => {
                if buf.len() + n > cap {
                    overflow.store(true, Ordering::Release);
                    return Ok((buf, true));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                overflow.store(true, Ordering::Release);
                return Err(format!("capturing rg output failed: {error}"));
            }
        }
    }
}

/// Read bounded client stdin for forwarding. A terminal stdin forwards
/// nothing (matching ripgrep, which only reads a non-terminal stdin).
pub fn read_client_stdin() -> Result<Option<Vec<u8>>, String> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    if !stdin_is_data()? {
        return Ok(None);
    }
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match stdin.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > MAX_STDIN {
                    return Err(format!(
                        "stdin exceeds the {} MiB forwarding bound",
                        MAX_STDIN / (1024 * 1024)
                    ));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("reading stdin: {e}")),
        }
    }
    Ok(Some(buf))
}

#[cfg(unix)]
fn stdin_is_data() -> Result<bool, String> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("inspecting stdin: {e}"))?;
    let metadata = std::fs::File::from(fd)
        .metadata()
        .map_err(|e| format!("inspecting stdin: {e}"))?;
    let null = std::fs::metadata("/dev/null").map_err(|e| format!("inspecting /dev/null: {e}"))?;
    Ok(!(metadata.file_type().is_char_device() && metadata.rdev() == null.rdev()))
}

#[cfg(windows)]
fn stdin_is_data() -> Result<bool, String> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileType(handle: *mut std::ffi::c_void) -> u32;
    }
    // Windows NUL is a character device, not an empty pipe. Retain Some(empty)
    // for a genuine empty pipe/file so rg does not search the cwd instead.
    let kind = unsafe { GetFileType(std::io::stdin().as_raw_handle()) } & !0x8000;
    match kind {
        1 | 3 => Ok(true),
        2 => Ok(false),
        _ => Err("cannot identify redirected stdin".into()),
    }
}

#[cfg(not(any(unix, windows)))]
fn stdin_is_data() -> Result<bool, String> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(args: &[&str]) -> (Invocation, Option<String>) {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        classify(&owned).expect("classification should succeed")
    }

    fn semantic(args: &[&str]) -> Semantic {
        match c(args).0 {
            Invocation::Semantic(s) => s,
            _ => panic!("expected semantic route for {args:?}"),
        }
    }

    fn lexical(args: &[&str]) -> Vec<String> {
        match c(args).0 {
            Invocation::Lexical { argv } => argv,
            _ => panic!("expected lexical route for {args:?}"),
        }
    }

    #[test]
    fn plain_query_is_semantic_by_default() {
        let s = semantic(&["how does the watcher recover"]);
        assert_eq!(s.query, "how does the watcher recover");
        assert!(s.root.is_none());
        assert_eq!(s.top, 5);
        assert!(!s.json);
    }

    #[test]
    fn top_k_sets_count_and_does_not_select_rg() {
        assert_eq!(semantic(&["-k", "3", "q"]).top, 3);
        assert_eq!(semantic(&["-k5", "q"]).top, 5);
        assert_eq!(semantic(&["--top", "9", "q"]).top, 9);
        assert_eq!(semantic(&["--top=7", "q"]).top, 7);
        let s = semantic(&["q", "src", "--json"]);
        assert_eq!(s.root.as_deref(), Some("src"));
        assert!(s.json);
    }

    #[test]
    fn json_alone_stays_semantic() {
        let s = semantic(&["q", "--json"]);
        assert!(s.json);
        assert!(matches!(c(&["--json", "q"]).0, Invocation::Semantic(_)));
    }

    #[test]
    fn rg_flags_select_lexical_and_keep_argv() {
        assert_eq!(lexical(&["-F", "foo", "."]), vec!["-F", "foo", "."]);
        assert_eq!(lexical(&["-e", "a.*b", "."]), vec!["-e", "a.*b", "."]);
        assert_eq!(lexical(&["-n", "foo"]), vec!["-n", "foo"]);
        assert_eq!(lexical(&["-l", "foo"]), vec!["-l", "foo"]);
        assert_eq!(lexical(&["-g", "*.rs", "foo"]), vec!["-g", "*.rs", "foo"]);
        assert_eq!(lexical(&["--files"]), vec!["--files"]);
        assert_eq!(lexical(&["--frobnicate", "x"]), vec!["--frobnicate", "x"]);
    }

    #[test]
    fn rg_flags_with_json_yield_rg_json() {
        assert_eq!(
            lexical(&["-F", "--json", "foo"]),
            vec!["-F", "--json", "foo"]
        );
        assert_eq!(
            lexical(&["-e", "x", "--json", "."]),
            vec!["-e", "x", "--json", "."]
        );
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        let s = semantic(&["--", "-F"]);
        assert_eq!(s.query, "-F");
        assert!(s.root.is_none());
        let s = semantic(&["q", "--", "-n"]);
        assert_eq!(s.root.as_deref(), Some("-n"));
    }

    #[test]
    fn cache_dir_is_global_and_stripped_from_rg_argv() {
        let (inv, cache) = c(&["--cache-dir", "E:/c", "-F", "foo"]);
        assert_eq!(cache.as_deref(), Some("E:/c"));
        match inv {
            Invocation::Lexical { argv } => assert_eq!(argv, vec!["-F", "foo"]),
            _ => panic!("expected lexical"),
        }
        let (_, cache) = c(&["--cache-dir=E:/c", "q"]);
        assert_eq!(cache.as_deref(), Some("E:/c"));
    }

    #[test]
    fn mixing_semantic_count_with_rg_options_is_rejected() {
        let owned: Vec<String> = ["-k", "3", "-F", "foo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(classify(&owned).is_err());
    }

    #[test]
    fn service_subcommands_and_flags() {
        assert!(matches!(c(&["up"]).0, Invocation::Service { .. }));
        assert!(matches!(
            c(&["status", "--json"]).0,
            Invocation::Service { .. }
        ));
        assert!(matches!(c(&["down"]).0, Invocation::Service { .. }));
        assert!(matches!(c(&["--daemon"]).0, Invocation::Daemon));
        assert!(matches!(c(&["--help"]).0, Invocation::Help));
        assert!(matches!(c(&["-V"]).0, Invocation::Version));
    }

    #[test]
    fn cache_prefix_and_management_suffix_preserve_the_command() {
        for args in [
            vec!["--cache-dir", "cache", "up", "repo", "--model", "code"],
            vec!["up", "repo", "--cache-dir", "cache", "--model", "code"],
        ] {
            let (inv, cache) = c(&args);
            assert_eq!(cache.as_deref(), Some("cache"));
            match inv {
                Invocation::Service { name, rest } => {
                    assert_eq!(name, "up");
                    assert_eq!(rest, vec!["repo", "--model", "code"]);
                }
                _ => panic!("cache option must not turn up into a semantic query"),
            }
        }
    }

    #[test]
    fn rg_pattern_named_like_a_global_option_is_not_removed() {
        let (inv, cache) = c(&["-F", "-e", "--cache-dir", "."]);
        assert!(cache.is_none());
        match inv {
            Invocation::Lexical { argv } => assert_eq!(argv, vec!["-F", "-e", "--cache-dir", "."]),
            _ => panic!("expected rg route"),
        }
    }

    #[test]
    fn missing_or_duplicate_cache_directory_is_an_error() {
        for args in [
            vec!["--cache-dir"],
            vec!["up", "--cache-dir"],
            vec!["up", "--cache-dir="],
            vec!["--cache-dir", "one", "up", "--cache-dir", "two"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            assert!(classify(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn output_read_failure_cannot_become_a_partial_success() {
        struct BrokenReader(bool);
        impl Read for BrokenReader {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    out[0] = b'x';
                    Ok(1)
                } else {
                    Err(std::io::Error::other("injected capture failure"))
                }
            }
        }
        let overflow = Arc::new(AtomicBool::new(false));
        assert!(read_capped(BrokenReader(false), 1024, overflow.clone()).is_err());
        assert!(overflow.load(Ordering::Acquire));
    }

    #[test]
    fn needless_json_only_query_is_an_error_not_empty_success() {
        let owned: Vec<String> = ["--json"].iter().map(|s| s.to_string()).collect();
        assert!(classify(&owned).is_err());
    }

    #[test]
    fn short_dash_is_positional_stdin() {
        let s = semantic(&["q", "-"]);
        assert_eq!(s.root.as_deref(), Some("-"));
    }
}
