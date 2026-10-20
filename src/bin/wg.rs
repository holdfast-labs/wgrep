//! `wg`: one local front door for semantic code search and ripgrep-compatible
//! filesystem search, backed by one shared service process.
#[path = "../progress.rs"]
mod progress;
#[path = "../service.rs"]
mod service;

use base64::{Engine, engine::general_purpose::STANDARD};
use sgrep::Result;
use sgrep::lexical::{self, Invocation, Semantic};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
wg: semantic code search with ripgrep-compatible filesystem search.

usage:
  wg QUERY [ROOT] [--top N|-k N] [--json] [--explain] [--model auto|code|text]
  wg [RIPGREP-FLAGS] PATTERN [PATH...]
  wg up [PATH] [--model auto|code|text] [--json]
  wg status [--json]
  wg down

Semantic sgrep-compatible fused ranking is the default. A familiar ripgrep
flag such as -F, -e, -n, -l, -g or --files selects ripgrep filesystem search:
wg then runs an authoritative rg child inside its local service and returns
rg's raw stdout/stderr and exit status (0 match, 1 no match, 2 error). The
filesystem route is a real walk, not the semantic index.

  --top N, -k N     semantic result count, 1..=100 (default 5)
  -n                ripgrep line numbers, not the semantic count
  --json            semantic JSON array by default; ripgrep JSON with rg flags
  --explain         add score detail to semantic JSON (requires --json)
  --model M         auto|code|text (semantic route)
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
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| {
            a.into_string()
                .map_err(|_| "wg requires UTF-8 command arguments")
        })
        .collect::<std::result::Result<_, _>>()?;
    let (invocation, cache) = match lexical::classify(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("wg: {error}");
            eprintln!("{HELP}");
            return Ok(ExitCode::from(2));
        }
    };
    let cache = cache.map(PathBuf::from);
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
        Invocation::Lexical { argv } => run_lexical(&argv, cache),
    }
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
                println!("Root: {}", root["path"].as_str().unwrap_or("?"));
                println!(
                    "Build: {:.2}s | Model setup: {:.2}s | Total: {:.2}s | PID: {}",
                    r["build_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    r["model_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    r["total_ms"].as_u64().unwrap_or(0) as f64 / 1000.0,
                    r["pid"]
                );
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
                        root["path"].as_str().unwrap_or("?"),
                        state,
                        root["files"],
                        root["chunks"],
                        root["generation"]
                    );
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
            println!("Stopped wg service (pid {}).", r["pid"]);
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

fn run_lexical(argv: &[String], cache: Option<PathBuf>) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    cwd.to_str()
        .ok_or("wg requires a UTF-8 working-directory path")?;
    let stdin = lexical::read_client_stdin()?;
    let r = service::lexical(argv, &cwd, stdin.as_deref(), cache)?;
    let status = r["exit_code"]
        .as_i64()
        .ok_or("lexical response is missing an exit code")?;
    if !(0..=255).contains(&status) {
        return Err("lexical response has an invalid exit code".into());
    }
    let stdout = decode(&r["stdout"])?;
    let stderr = decode(&r["stderr"])?;
    {
        let mut out = std::io::stdout().lock();
        out.write_all(&stdout)?;
        out.flush()?;
    }
    {
        let mut err = std::io::stderr().lock();
        err.write_all(&stderr)?;
        err.flush()?;
    }
    Ok(ExitCode::from(if (0..=255).contains(&status) {
        status as u8
    } else {
        2
    }))
}

fn decode(value: &serde_json::Value) -> Result<Vec<u8>> {
    Ok(STANDARD.decode(
        value
            .as_str()
            .ok_or("lexical response is missing a stream payload")?,
    )?)
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
