//! wgrep-sem: opt-in native semantic ranking. Explicitly NOT ripgrep.
//!
//! Usage:
//!   wgrep-sem QUERY [ROOT] [-n COUNT] [--json] [--budget-bytes BYTES]
//!             [--model auto|code|text] [--prefix SUBDIR] [--cache-dir DIR]
//!             [--no-daemon]
//!   wgrep-sem --serve ROOT [--cache-dir DIR]
//!   wgrep-sem --stop [ROOT]
//!   wgrep-sem --status [ROOT]
//!
//! The query path needs a resident daemon (one model + vector store per
//! root, shared by all clients); the first query auto-starts it, mirroring
//! wgrep UX. `--no-daemon` builds the corpus in-process instead (slow,
//! correct, useful for cold checks). Exit codes: 0 results, 1 no results,
//! 2 errors. A broken pipe is a graceful success.

#[path = "../sem/mod.rs"]
mod sem;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use sem::{client, daemon, model, slots};
use sem::chunks::Chunking;

const DEFAULT_TOP_N: usize = 5;
const DEFAULT_BUDGET: u64 = 65536;

fn usage() -> &'static str {
    "usage: wgrep-sem QUERY [ROOT] [--chunking window|rust-fn] [-n COUNT] [--json] [--budget-bytes BYTES]\n       [--model auto|code|text] [--prefix SUBDIR] [--cache-dir DIR] [--no-daemon]\n       wgrep-sem --serve ROOT [--chunking window|rust-fn] [--cache-dir DIR]\n       wgrep-sem --stop [ROOT] [--chunking window|rust-fn] | wgrep-sem --status [ROOT] [--chunking window|rust-fn]"
}

struct QueryArgs {
    query: String,
    root: PathBuf,
    top_n: usize,
    json: bool,
    budget: u64,
    model: String,
    prefix: String,
    cache: PathBuf,
    no_daemon: bool,
    chunking: Chunking,
}

fn parse_query(argv: &[String]) -> anyhow::Result<QueryArgs> {
    let mut query: Option<String> = None;
    let mut root = PathBuf::from(".");
    let mut top_n = DEFAULT_TOP_N;
    let mut json = false;
    let mut budget = DEFAULT_BUDGET;
    let mut model = "auto".to_owned();
    let mut prefix = ".".to_owned();
    let mut cache: Option<PathBuf> = None;
    let mut no_daemon = false;
    let mut chunking = Chunking::Window;
    let mut positional = 0usize;
    let mut i = 0usize;
    let flag_val = |argv: &[String], i: &mut usize, name: &str| -> anyhow::Result<String> {
        *i += 1;
        argv.get(*i).cloned().ok_or_else(|| anyhow::anyhow!("wgrep-sem: {name} needs a value\n{usage}", usage = usage()))
    };
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "-n" => {
                top_n = flag_val(argv, &mut i, "-n")?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("wgrep-sem: bad -n value\n{}", usage()))?;
                if top_n == 0 || top_n > 100 {
                    anyhow::bail!("wgrep-sem: -n must be 1..=100\n{}", usage());
                }
            }
            "--json" => json = true,
            "--budget-bytes" => {
                budget = flag_val(argv, &mut i, "--budget-bytes")?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("wgrep-sem: bad --budget-bytes value\n{}", usage()))?;
            }
            "--model" => {
                model = flag_val(argv, &mut i, "--model")?;
                if !["auto", "code", "text"].contains(&model.as_str()) {
                    anyhow::bail!("wgrep-sem: --model must be auto|code|text\n{}", usage());
                }
                // Code-only MVP (embedding-space safety): corpus, reconcile
                // and queries all use the code model. `--model text` is an
                // explicit error, never a silent fallback or mixed space.
                // (`auto` currently selects code for every candidate set.)
                if model == "text" {
                    anyhow::bail!("wgrep-sem: --model text unsupported in this build (code-model only)");
                }
            }
            "--prefix" => prefix = flag_val(argv, &mut i, "--prefix")?,
            "--cache-dir" => cache = Some(PathBuf::from(flag_val(argv, &mut i, "--cache-dir")?)),
            "--no-daemon" => no_daemon = true,
            "--chunking" => chunking = parse_chunking(&flag_val(argv, &mut i, "--chunking")?)?,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ if a.starts_with('-') => {
                anyhow::bail!("wgrep-sem: unknown flag {a}\n{}", usage());
            }
            _ => {
                positional += 1;
                if positional == 1 {
                    query = Some(argv[i].clone());
                } else if positional == 2 {
                    root = PathBuf::from(&argv[i]);
                } else {
                    anyhow::bail!("wgrep-sem: unexpected argument {}\n{}", argv[i], usage());
                }
            }
        }
        i += 1;
    }
    let Some(query) = query else { anyhow::bail!("wgrep-sem: QUERY is required\n{}", usage()) };
    if query.trim().is_empty() {
        anyhow::bail!("wgrep-sem: query has no searchable words");
    }
    let cache = match cache {
        Some(c) => c,
        None => model::cache_dir(None)?,
    };
    Ok(QueryArgs { query, root, top_n, json, budget, model, prefix, cache, no_daemon, chunking })
}

fn parse_chunking(s: &str) -> anyhow::Result<Chunking> {
    match s { "window" => Ok(Chunking::Window), "rust-fn" => Ok(Chunking::RustFn), _ => anyhow::bail!("wgrep-sem: --chunking must be window|rust-fn") }
}

/// Hard source-byte budget contract:
/// - counts the summed UTF-8 bytes (`str::len`, so multibyte characters
///   count all their bytes) of returned `content` fields ONLY. Headers
///   (`path`/`start`/`end`) and `scores` are metadata, never counted;
/// - `budget == 0` disables the cap (unlimited);
/// - results are kept in rank order while they fit; the first result that
///   would exceed the cap — including the top result itself — is dropped
///   along with everything below it. The budget therefore NEVER exceeds:
///   a too-large top passage yields zero results (exit 1, `[]` under
///   `--json`), never an over-budget answer.
fn apply_budget(results: &mut Vec<serde_json::Value>, budget: u64) {
    if budget == 0 {
        return;
    }
    let mut kept = 0usize;
    let mut bytes = 0u64;
    for r in results.iter() {
        let n = r
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.len() as u64)
            .unwrap_or(0);
        if bytes + n > budget {
            break;
        }
        bytes += n;
        kept += 1;
    }
    results.truncate(kept);
}

fn print_results(
    reply: &serde_json::Value,
    json: bool,
    budget: u64,
) -> anyhow::Result<bool> {
    let mut results: Vec<serde_json::Value> = reply
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    apply_budget(&mut results, budget);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    if json {
        writeln!(out, "{}", serde_json::Value::Array(results.clone()))?;
    } else {
        for r in &results {
            let path = r.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let start = r.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
            let end = r.get("end").and_then(|v| v.as_u64()).unwrap_or(0);
            let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
            writeln!(out, "{path}:{start}-{end}\n{}\n", content.trim_end())?;
        }
    }
    out.flush()?;
    Ok(!results.is_empty())
}

fn resolve_root(arg: &PathBuf) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let abs = slots::absolute(arg, &cwd);
    if !abs.is_dir() {
        anyhow::bail!("wgrep-sem: not a directory: {}", abs.display());
    }
    Ok(abs)
}

/// Management-path normalization (`--stop`/`--status`): resolves the
/// slot WITHOUT requiring the root to still exist, so a deleted repo's
/// daemon can still be found and cleaned up. Never used for serving.
fn resolve_mgmt(arg: &PathBuf) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(slots::absolute(arg, &cwd))
}

fn parse_mgmt(argv: &[String]) -> anyhow::Result<(PathBuf, Chunking)> {
    let mut root = PathBuf::from(".");
    let mut mode = Chunking::Window;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--chunking" => {
                i += 1;
                mode = parse_chunking(argv.get(i).ok_or_else(|| anyhow::anyhow!("wgrep-sem: --chunking needs a value"))?)?;
            }
            arg if arg.starts_with('-') => anyhow::bail!("wgrep-sem: unknown management argument {arg}"),
            _ if root == PathBuf::from(".") => root = PathBuf::from(&argv[i]),
            _ => anyhow::bail!("wgrep-sem: unexpected management argument {}", argv[i]),
        }
        i += 1;
    }
    Ok((root, mode))
}

/// One-shot fallback: build + rank in this process. Slow (model load +
/// full embed per invocation) but independent of any daemon — the cold
/// correctness check and the no-daemon escape hatch.
fn oneshot(args: &QueryArgs, root: &std::path::Path) -> anyhow::Result<ExitCode> {
    use sem::corpus::Corpus;
    use sem::model::{CODE, ModelCtx};
    let cache = model::cache_dir(Some(&args.cache))?;
    let code = ModelCtx::load(&CODE, &cache)?;
    let mut corpus = Corpus::empty(root.to_path_buf());
    corpus.build(&code, args.chunking)?;
    if corpus.chunks.is_empty() {
        if args.json {
            println!("[]");
        }
        return Ok(ExitCode::from(1));
    }
    let ranked = {
        // Same scope resolver as the daemon rank path: one-shot and
        // daemon rank the identical chunk set for the same --prefix.
        let scope = sem::rank::scope_for(&corpus, &args.prefix)?;
        sem::rank::rank_scoped(&corpus, &code, &args.query, &scope, args.top_n)?
    };
    let reply = serde_json::json!({
        "ok": true, "proto": sem::PROTO, "model": format!("{}@{}", code.pin.repo, &code.pin.revision[..12]),
        "dim": sem::DIM, "files": corpus.file_count(), "chunks": corpus.chunk_count(),
        "generation": 0u64,
        "results": ranked.iter().map(|r| {
            let c = &corpus.chunks[r.idx];
            serde_json::json!({
                "path": c.path, "start": c.start, "end": c.end, "content": c.content,
                "scores": { "bm25": r.bm25, "semantic": r.semantic,
                    "bm25_relative": r.bm25_relative, "semantic_relative": r.semantic_relative,
                    "fused": r.fused, "exact_identifier": r.exact_identifier },
            })
        }).collect::<Vec<_>>(),
    });
    let nonempty = print_results(&reply, args.json, args.budget)?;
    Ok(ExitCode::from(if nonempty { 0 } else { 1 }))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = run(argv);
    match code {
        Ok(c) => c,
        Err(e) => {
            let broken = e
                .chain()
                .any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe));
            if broken {
                return ExitCode::from(0);
            }
            eprintln!("{e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(argv: Vec<String>) -> anyhow::Result<ExitCode> {
    if argv.first().is_some_and(|a| a == "--serve") {
        if argv.len() < 2 {
            anyhow::bail!("wgrep-sem: --serve needs ROOT\n{}", usage());
        }
        let root = resolve_root(&PathBuf::from(&argv[1]))?;
        let mut cache: Option<PathBuf> = None;
        let mut chunking = Chunking::Window;
        let mut i = 2usize;
        while i < argv.len() {
            if argv[i] == "--cache-dir" {
                i += 1;
                cache = Some(PathBuf::from(
                    argv.get(i).ok_or_else(|| anyhow::anyhow!("wgrep-sem: --cache-dir needs a value"))?,
                ));
            } else if argv[i] == "--chunking" {
                i += 1;
                chunking = parse_chunking(argv.get(i).ok_or_else(|| anyhow::anyhow!("wgrep-sem: --chunking needs a value"))?)?;
            } else {
                anyhow::bail!("wgrep-sem: unknown --serve argument {}\n{}", argv[i], usage());
            }
            i += 1;
        }
        let cache = match cache {
            Some(c) => c,
            None => model::cache_dir(None)?,
        };
        daemon::serve(&root, &cache, chunking)?;
        return Ok(ExitCode::from(0));
    }
    if argv.first().is_some_and(|a| a == "--stop") {
        let (root, mode) = parse_mgmt(&argv[1..])?;
        let root = resolve_mgmt(&root)?;
        match client::shutdown(&root, mode) {
            Some(pid) => println!("wgrep-sem: stopped the daemon for {} (pid {pid})", root.display()),
            None => println!("wgrep-sem: no daemon is serving {}", root.display()),
        }
        return Ok(ExitCode::from(0));
    }
    if argv.first().is_some_and(|a| a == "--status") {
        let (root, mode) = parse_mgmt(&argv[1..])?;
        let root = resolve_mgmt(&root)?;
        match client::stats(&root, mode) {
            Some(v) => println!("{v}"),
            None => println!("wgrep-sem: no daemon is serving {}", root.display()),
        }
        return Ok(ExitCode::from(0));
    }
    let args = parse_query(&argv)?;
    let root = resolve_root(&args.root)?;
    if args.no_daemon {
        return oneshot(&args, &root);
    }
    client::ensure_serving(&root, &args.cache, args.chunking)?;
    let ranked = client::rank(&root, &args.query, &args.prefix, args.top_n, &args.model, args.chunking)?;
    let nonempty = print_results(&ranked.reply, args.json, args.budget)?;
    Ok(ExitCode::from(if nonempty { 0 } else { 1 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str) -> serde_json::Value {
        serde_json::json!({
            "path": "src/f.rs", "start": 1, "end": 40, "content": content,
            "scores": {"bm25": 1.0, "semantic": 0.9, "bm25_relative": 1.0,
                       "semantic_relative": 1.0, "fused": 0.9,
                       "exact_identifier": false},
        })
    }

    #[test]
    fn first_passage_over_cap_yields_empty_not_over_budget() {
        // Top passage alone (100 bytes) exceeds the 10-byte budget: the
        // honest answer is zero results, never an over-cap answer.
        let mut v = vec![item(&"x".repeat(100)), item("tiny")];
        apply_budget(&mut v, 10);
        assert!(v.is_empty());
    }

    #[test]
    fn multibyte_counts_utf8_bytes_not_chars() {
        // "é" is 2 UTF-8 bytes: 10 of them are 20 bytes, over a 15 budget.
        let mut v = vec![item(&"é".repeat(10))];
        apply_budget(&mut v, 15);
        assert!(v.is_empty());
        // Exactly at the byte cap still fits.
        let mut v = vec![item(&"é".repeat(10))];
        apply_budget(&mut v, 20);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn zero_disables_and_prefix_fit_keeps_order() {
        let mut v = vec![item(&"x".repeat(1000)), item(&"y".repeat(1000))];
        apply_budget(&mut v, 0);
        assert_eq!(v.len(), 2);
        // 1000 + 4 fits in 1004; the third (1000 more) does not.
        let mut v = vec![item(&"x".repeat(1000)), item("tiny"), item(&"z".repeat(1000))];
        apply_budget(&mut v, 1004);
        assert_eq!(v.len(), 2);
    }
}
