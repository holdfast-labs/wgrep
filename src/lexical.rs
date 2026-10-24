//! Unified `wg` front-door support: classify one invocation into the semantic
//! or filesystem text/regex route.
//!
//! `wg QUERY [ROOT]` stays sgrep-compatible semantic ranking by default. A
//! familiar ripgrep flag (or any flag `wg` does not own) selects the lexical
//! route instead: the original argv is handed to the vendored ripgrep 15.1.0
//! engine in the `wg` process, so matching, ignore rules, formatting, JSON
//! output and diagnostics stay ripgrep's, never a semantic approximation.
//!
//! Mode is decided by flags only. Natural language versus regex is never
//! guessed from punctuation, and `--` ends option parsing for both routes.

/// Flags `wg` itself accepts with no value.
const WG_BOOL: &[&str] = &["--json", "--explain", "--saturate", "-h", "--help", "-V", "--version"];
/// Flags `wg` itself accepts with a value.
const WG_VALUE: &[&str] = &["-k", "--top", "--model", "--cache-dir", "--rank"];
/// `wg`-owned value flags that have no meaning in the ripgrep route.
const WG_SEMANTIC_ONLY: &[&str] = &["-k", "--top", "--model", "--explain", "--rank"];

use std::ffi::OsString;
use std::path::PathBuf;

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
    /// Ripgrep filesystem search: the exact argv for the in-process engine.
    /// Kept as `OsString` so non-UTF-8 arguments reach ripgrep unchanged.
    Lexical {
        argv: Vec<OsString>,
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
    /// `semantic` (default) or `classic`.
    pub rank: String,
    /// Opt-in per-file saturation within the top 10.
    pub saturate: bool,
}

/// Classify `args` and lift the global `--cache-dir` out of every route.
///
/// The text route keeps the caller's exact `OsString` argv, so a pattern or
/// path that is not valid UTF-8 reaches ripgrep unchanged.
pub fn classify_os(args: &[OsString]) -> Result<(Invocation, Option<PathBuf>), String> {
    // A global prefix must be removed before recognizing a subcommand. Never
    // search an rg argv for these tokens: `-e --cache-dir` is a valid pattern.
    let mut leading = 0;
    let mut prefix_cache: Option<PathBuf> = None;
    while let Some(arg) = args.get(leading) {
        if arg == "--cache-dir" {
            let value = args.get(leading + 1).ok_or("--cache-dir needs a value")?;
            if value.is_empty() || value.to_string_lossy().starts_with('-') {
                return Err("--cache-dir needs a directory value".into());
            }
            prefix_cache = Some(PathBuf::from(value));
            leading += 2;
        } else if let Some(value) = arg.to_str().and_then(|a| a.strip_prefix("--cache-dir=")) {
            if value.is_empty() {
                return Err("--cache-dir needs a directory value".into());
            }
            prefix_cache = Some(PathBuf::from(value));
            leading += 1;
        } else {
            break;
        }
    }
    let tail = &args[leading..];
    let utf8: Option<Vec<String>> = tail.iter().map(|a| a.to_str().map(str::to_owned)).collect();
    // A non-UTF-8 value cannot be a wg-owned flag, so a lossy view is enough
    // to choose the route. It is never used to run the search.
    let view: Vec<String> = match &utf8 {
        Some(args) => args.clone(),
        None => tail.iter().map(|a| a.to_string_lossy().into_owned()).collect(),
    };
    let (invocation, cache) = classify_inner(&view)?;
    let invocation = match invocation {
        Invocation::Lexical { .. } => Invocation::Lexical { argv: tail.to_vec() },
        other => other,
    };
    if utf8.is_none() && !matches!(invocation, Invocation::Lexical { .. }) {
        return Err("wg requires UTF-8 command arguments".into());
    }
    let cache = cache.map(PathBuf::from);
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
        if name == "--explain" || name == "--saturate" {
            semantic_only = true;
            i += 1;
            continue;
        }
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
                "cannot combine semantic options (--top/-k/--model/--explain/--rank) with ripgrep \
                 options; run them as separate commands"
                    .into(),
            );
        }
        return Ok((
            Invocation::Lexical {
                argv: args.iter().map(OsString::from).collect(),
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
    let mut rank = "semantic".to_owned();
    let mut saturate = false;
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
        if a == "--saturate" {
            saturate = true;
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
        if name == "--rank" {
            let value = match inline {
                Some(v) => v.to_owned(),
                None => value_at(args, i + 1, "--rank")?,
            };
            if !["semantic", "classic"].contains(&value.as_str()) {
                return Err("--rank must be semantic or classic".into());
            }
            rank = value;
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
    if saturate && rank == "classic" {
        return Err("--saturate requires --rank semantic".into());
    }
    Ok(SemanticParse::Args(Semantic {
        query,
        root,
        top,
        json,
        explain,
        model,
        rank,
        saturate,
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


#[cfg(test)]
mod tests {
    use super::*;

    fn c(args: &[&str]) -> (Invocation, Option<String>) {
        let owned: Vec<OsString> = args.iter().map(OsString::from).collect();
        let (invocation, cache) = classify_os(&owned).expect("classification should succeed");
        (invocation, cache.map(|p| p.to_string_lossy().into_owned()))
    }

    fn semantic(args: &[&str]) -> Semantic {
        match c(args).0 {
            Invocation::Semantic(s) => s,
            _ => panic!("expected semantic route for {args:?}"),
        }
    }

    fn lexical(args: &[&str]) -> Vec<String> {
        match c(args).0 {
            Invocation::Lexical { argv } => argv
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
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
        assert_eq!(s.rank, "semantic");
        assert!(!s.saturate);
    }

    #[test]
    fn rank_mode_and_saturation_are_parsed() {
        let s = semantic(&["q", "--rank", "classic"]);
        assert_eq!(s.rank, "classic");
        assert!(!s.saturate);
        let s = semantic(&["q", "--rank=semantic", "--saturate"]);
        assert_eq!(s.rank, "semantic");
        assert!(s.saturate);
        let owned: Vec<OsString> = ["q", "--rank", "other"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(classify_os(&owned).is_err());
        let owned: Vec<OsString> = ["q", "--rank", "classic", "--saturate"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(classify_os(&owned).is_err());
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
        let owned: Vec<OsString> = ["-k", "3", "-F", "foo"]
            .iter()
            .map(OsString::from)
            .collect();
        assert!(classify_os(&owned).is_err());
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
            let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
            assert!(classify_os(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn needless_json_only_query_is_an_error_not_empty_success() {
        let owned: Vec<OsString> = ["--json"].iter().map(OsString::from).collect();
        assert!(classify_os(&owned).is_err());
    }

    #[test]
    fn short_dash_is_positional_stdin() {
        let s = semantic(&["q", "-"]);
        assert_eq!(s.root.as_deref(), Some("-"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_arguments_reach_the_text_route_unchanged() {
        use std::os::unix::ffi::OsStringExt;
        let pattern = OsString::from_vec(vec![b'f', 0xff, b'o']);
        let args = vec![OsString::from("-F"), pattern.clone(), OsString::from(".")];
        match classify_os(&args).expect("text route").0 {
            Invocation::Lexical { argv } => {
                assert_eq!(argv, args);
                assert_eq!(argv[1], pattern);
            }
            _ => panic!("expected lexical route"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn non_utf8_arguments_reach_the_text_route_unchanged() {
        use std::os::windows::ffi::OsStringExt;
        let pattern = OsString::from_wide(&[0xD800, 0x0061]);
        let args = vec![OsString::from("-F"), pattern.clone(), OsString::from(".")];
        match classify_os(&args).expect("text route").0 {
            Invocation::Lexical { argv } => {
                assert_eq!(argv, args);
                assert_eq!(argv[1], pattern);
            }
            _ => panic!("expected lexical route"),
        }
    }
}
