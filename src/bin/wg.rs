//! `wg`: one local front door for semantic code search and ripgrep-compatible
//! filesystem search, backed by one shared service process.
#[path = "../progress.rs"]
mod progress;
#[path = "../service.rs"]
mod service;

use wgrep::Result;
use wgrep::lexical::{self, Invocation, Semantic};
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
wg: semantic code search with ripgrep-compatible filesystem search.

usage:
  wg QUERY [SCOPE] [--top N|-k N] [--json] [--explain] [--model auto|code|text]
           [--rank semantic|classic] [--saturate] [--semantic-glob GLOB]...
           [--min-score SCORE]
  wg [RIPGREP-FLAGS] PATTERN [PATH...]
  wg up [PATH] [--model auto|code|text] [--json]
  wg status [--json]
  wg down

Semantic fused ranking is the default. The default ranker boosts file
coherence, whole-query symbol definitions and natural-language file-stem
matches. --rank classic selects the pinned sgrep ranker (exact parity).
--saturate adds opt-in per-file diversity in the first ten positions.
SCOPE limits semantic search to one file or directory under an attached root.
--semantic-glob adds a root-relative include glob; prefix it with ! to exclude.
Includes are OR-ed and any exclude vetoes a match. A glob without / matches a
file name at any depth; a glob with / is root-relative. -g stays ripgrep-only.
--min-score keeps only results whose raw cosine similarity is at least SCORE.
SCORE must be finite and in [-1, 1]. The cutoff applies before the shortlist
fills, and there is no default cutoff. Relative and fused scores are ranking
values, not confidence.
A familiar ripgrep flag such as -F, -e, -n, -l, -g or --files selects the
in-process ripgrep 15.1.0 engine. wg then returns rg's stdout/stderr and exit
status (0 match, 1 no match, 2 error). The filesystem route is a real walk,
not the semantic index. It does not need an rg binary on PATH.

  --top N, -k N     semantic result count, 1..=100 (default 5)
  -n                ripgrep line numbers, not the semantic count
  --json            semantic JSON array by default; ripgrep JSON with rg flags
  --explain         add score detail to semantic JSON (requires --json)
  --model M         auto|code|text (semantic route)
  --rank M          semantic (default) or classic (sgrep parity)
  --saturate        decay repeated chunks of one file in the top 10
  --semantic-glob G repeatable include glob; !G excludes (semantic route)
  --min-score S     raw cosine cutoff in [-1, 1]; no default (semantic route)
  --cache-dir DIR   model and parser asset cache (global prefix)
  -h, --help        this help
  -V, --version     version";

fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wg: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let (invocation, cache) = match lexical::classify_os(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("wg: {error}");
            eprintln!("{HELP}");
            return Ok(ExitCode::from(2));
        }
    };
    match invocation {
        Invocation::Help => {
            println!("{HELP}");
            Ok(ExitCode::SUCCESS)
        }
        Invocation::Version => {
            println!("wg {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Invocation::Daemon => {
            service::daemon(cache)?;
            Ok(ExitCode::SUCCESS)
        }
        Invocation::Service { name, rest } => run_service(&name, &rest, cache),
        Invocation::Semantic(semantic) => run_semantic(semantic),
        Invocation::Lexical { argv } => run_lexical(&argv),
    }
}

fn text_files_text(text: &serde_json::Value) -> String {
    let indexed = text["indexed"].as_u64().unwrap_or(0);
    let pending = text["pending"].as_u64().unwrap_or(0);
    let unindexed = text["unindexed"].as_u64().unwrap_or(0);
    let total = text["files"].as_u64().unwrap_or(0);
    if pending == 0 && unindexed == 0 {
        return format!("{total} files");
    }
    let mut detail = vec![format!("{indexed} indexed")];
    if pending > 0 {
        detail.push(format!("{pending} pending"));
    }
    if unindexed > 0 {
        detail.push(format!("{unindexed} unindexed"));
    }
    format!("{total} files ({})", detail.join(", "))
}

fn run_service(name: &str, rest: &[String], cache: Option<PathBuf>) -> Result<ExitCode> {
    match name {
        "up" => {
            let mut json = false;
            let mut model = "auto".to_owned();
            let mut path: Option<PathBuf> = None;
            let mut i = 0usize;
            let mut positional_only = false;
            while i < rest.len() {
                let a = rest[i].as_str();
                if !positional_only && a == "--" {
                    positional_only = true;
                    i += 1;
                    continue;
                }
                if !positional_only && a == "--json" {
                    json = true;
                } else if !positional_only && a == "--model" {
                    i += 1;
                    model = rest.get(i).ok_or("--model needs a value")?.clone();
                } else if !positional_only && a.starts_with("--model=") {
                    let value = a.strip_prefix("--model=").unwrap();
                    model = value.to_owned();
                } else if !positional_only && a.starts_with('-') {
                    return Err(format!("unrecognized option {a} for wg up").into());
                } else if path.is_none() {
                    path = Some(PathBuf::from(a));
                } else {
                    return Err(format!("unexpected argument {a}").into());
                }
                i += 1;
            }
            if !["auto", "code", "text"].contains(&model.as_str()) {
                return Err("--model must be auto, code, or text".into());
            }
            let path = match path {
                Some(path) => absolute(path)?,
                None => repository_root()?,
            };
            let r = if json {
                service::up(&path, &model, cache)?
            } else {
                progress::during("Preparing wg index", || service::up(&path, &model, cache))?
            };
            if json {
                println!("{r}")
            } else {
                let root = &r["root"];
                println!("Ready: {} files, {} chunks", root["files"], root["chunks"]);
                println!(
                    "Root: {}",
                    display_path(root["path"].as_str().unwrap_or("?"))
                );
                println!(
                    "Build: {:.2}s | Model setup: {:.2}s | Total: {:.2}s | PID: {}",
                    r["build_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    r["model_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    r["total_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    r["pid"]
                );
                if let Some(text) = root.get("text") {
                    println!(
                        "Text index: {:.2}s | {} | {:.1} MB on disk | {:.1} MB resident",
                        text["build_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                        text_files_text(text),
                        text["disk_bytes"].as_u64().unwrap_or(0) as f64 / 1_000_000.0,
                        text["resident_bytes"].as_u64().unwrap_or(0) as f64 / 1_000_000.0
                    );
                } else if let Some(error) = r["text_error"].as_str() {
                    println!("Text index unavailable: {error}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "status" => {
            if rest.iter().any(|a| a != "--json") {
                return Err("wg status only accepts --json".into());
            }
            let json = rest.iter().any(|a| a == "--json");
            let r = service::status_json()?;
            if json {
                println!("{r}")
            } else {
                println!(
                    "pid={} models={} roots={}",
                    r["pid"],
                    r["loaded_models"],
                    r["roots"].as_array().map_or(0, Vec::len)
                );
                for root in r["roots"].as_array().into_iter().flatten() {
                    let state = if root["ready"] == true {
                        "ready"
                    } else if root["pending"].as_u64().unwrap_or(0) > 0 {
                        "updating"
                    } else {
                        "unhealthy"
                    };
                    println!(
                        "{} [{}]: {} files, {} chunks, generation {}",
                        display_path(root["path"].as_str().unwrap_or("?")),
                        state,
                        root["files"],
                        root["chunks"],
                        root["generation"]
                    );
                    if let Some(text) = root.get("text") {
                        let phase = if text["rescanning"] == true {
                            "rescanning"
                        } else if text["rebuilding"] == true {
                            "rebuilding"
                        } else if text["needs_rescan"] == true {
                            "rescan pending"
                        } else {
                            "current"
                        };
                        println!(
                            "  text: {} [{}], {}, {:.1} MB disk, {:.1} MB resident",
                            if text["untrusted"] == true {
                                "untrusted"
                            } else {
                                "ready"
                            },
                            phase,
                            text_files_text(text),
                            text["disk_bytes"].as_u64().unwrap_or(0) as f64 / 1_000_000.0,
                            text["resident_bytes"].as_u64().unwrap_or(0) as f64 / 1_000_000.0
                        );
                    } else if let Some(error) = root["text_error"].as_str() {
                        println!("  text index unavailable: {error}");
                    }
                    if let Some(error) = root["error"].as_str() {
                        println!("  {error}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "down" => {
            if !rest.is_empty() {
                return Err("wg down takes no arguments".into());
            }
            let r = service::down()?;
            if r["running"] == false {
                println!("No wg service is running.");
            } else {
                println!("Stopped wg service (pid {}).", r["pid"]);
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(format!("unknown subcommand {name}").into()),
    }
}

fn run_semantic(semantic: Semantic) -> Result<ExitCode> {
    let whole_repo = semantic.root.is_none();
    let path = absolute(PathBuf::from(
        semantic.root.unwrap_or_else(|| ".".to_owned()),
    ))?;
    let r = service::query(
        &semantic.query,
        &path,
        whole_repo,
        semantic.top,
        semantic.explain,
        &semantic.model,
        &semantic.rank,
        semantic.saturate,
        &semantic.globs,
        semantic.min_score,
    )?;
    let results = r["results"].as_array().ok_or("invalid service response")?;
    if semantic.json {
        println!("{}", serde_json::Value::Array(results.clone()))
    } else {
        for row in results {
            println!(
                "{}:{}-{}\n{}\n",
                row["path"].as_str().unwrap_or(""),
                row["start"],
                row["end"],
                row["content"].as_str().unwrap_or("").trim_end()
            );
        }
    }
    Ok(ExitCode::from(if results.is_empty() { 1 } else { 0 }))
}

fn run_lexical(argv: &[std::ffi::OsString]) -> Result<ExitCode> {
    let code = wisp_grep::run_rg_text(argv.to_vec(), service::text_groups);
    Ok(ExitCode::from(code as u8))
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    path.to_str().ok_or("wg requires UTF-8 paths")?;
    Ok(path)
}

fn repository_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let root = cwd
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(&cwd)
        .to_path_buf();
    root.to_str().ok_or("wg requires UTF-8 paths")?;
    Ok(root)
}

/// Human-readable path. On Windows it drops the verbatim prefix. This is for
/// display only; file access always uses the original path.
#[cfg(windows)]
fn display_path(value: &str) -> String {
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    value.strip_prefix(r"\\?\").unwrap_or(value).to_owned()
}
#[cfg(not(windows))]
fn display_path(value: &str) -> String {
    value.to_owned()
}
