//! Analysis-only batch scorer for wg ranking experiments.
//!
//! It calls the public `wgrep` library (`Searcher`, `bm25`,
//! `identifier_pattern`) and reproduces `rank_impl` exactly for the baseline
//! config. Extra configs change the candidate depth and add optional path or
//! definition bonuses. This file is not part of the product.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wgrep::{bm25, identifier_pattern, relative_scores, ModelChoice, Searcher};

static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").unwrap());
static STOP: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    "a an and are as at be been but by can do does for from had has have how i if in into is it its me not of on or our should so that the their them there these they this to use using was we what when where which who will with would you your function description purpose input output"
        .split_whitespace()
        .collect()
});
static DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?(?:extern\s+(?:"[^"]+"\s+)?)?(?:fn|struct|enum|trait|type|mod|impl|class|def|interface|function|func|let|var|const)\s+([A-Za-z_$][A-Za-z0-9_$]*)"#,
    )
    .unwrap()
});

fn words(text: &str) -> Vec<String> {
    let text = CAMEL.replace_all(text, "${1} ${2}").to_lowercase();
    WORD.find_iter(&text)
        .map(|m| m.as_str())
        .filter(|w| w.chars().count() > 1 && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

#[derive(Deserialize, Serialize, Clone)]
struct Config {
    name: String,
    #[serde(default = "default_pool")]
    lex: usize,
    #[serde(default = "default_pool")]
    sem: usize,
    #[serde(default = "default_weight")]
    w: f64,
    #[serde(default)]
    path: f64,
    #[serde(default)]
    def: f64,
}

fn default_pool() -> usize {
    200
}
fn default_weight() -> f64 {
    0.25
}

#[derive(Deserialize)]
struct Target {
    path: String,
    line: usize,
}

#[derive(Deserialize)]
struct Query {
    id: String,
    query: String,
    terms: Vec<String>,
    #[serde(default)]
    targets: Vec<Target>,
}

struct Options {
    root: PathBuf,
    queries: PathBuf,
    out: PathBuf,
    configs: Vec<Config>,
    top: usize,
    content: bool,
}

fn parse_args() -> Options {
    let args: Vec<String> = std::env::args().collect();
    let mut root = None;
    let mut queries = None;
    let mut out = None;
    let mut configs = Vec::new();
    let mut top = 100usize;
    let mut content = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--queries" => {
                queries = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--config" => {
                configs.push(serde_json::from_str(&args[i + 1]).expect("bad --config JSON"));
                i += 2;
            }
            "--top" => {
                top = args[i + 1].parse().expect("bad --top");
                i += 2;
            }
            "--content" => {
                content = true;
                i += 1;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    if configs.is_empty() {
        configs.push(Config {
            name: "base".into(),
            lex: 200,
            sem: 200,
            w: 0.25,
            path: 0.0,
            def: 0.0,
        });
    }
    Options {
        root: root.expect("--root required"),
        queries: queries.expect("--queries required"),
        out: out.expect("--out required"),
        configs,
        top,
        content,
    }
}

fn contains_line(chunk: &wgrep::Chunk, path: &str, line: usize) -> bool {
    chunk.path == path && chunk.start <= line && line <= chunk.end
}

fn main() {
    let opts = parse_args();
    let raw = std::fs::read_to_string(&opts.queries).expect("read queries");
    let queries: Vec<Query> = serde_json::from_str(&raw).expect("parse queries");

    let searcher = Searcher::open(&opts.root, ModelChoice::Code, None).expect("build searcher");
    let chunks = searcher.corpus().chunks();
    let corpus_len = searcher.corpus().len();
    let vectors = searcher.passage_vectors();

    let mut out = Vec::new();
    for q in &queries {
        let mut terms = if q.terms.is_empty() {
            words(&q.query)
        } else {
            q.terms.clone()
        };
        terms.sort();
        terms.dedup();
        let lexical = bm25(chunks, &terms, corpus_len);
        let query_vector = searcher
            .model()
            .embed(vec![q.query.clone()])
            .expect("embed query")
            .remove(0);
        let similarities: Vec<f64> = vectors
            .iter()
            .map(|v| {
                v.iter()
                    .zip(&query_vector)
                    .map(|(a, b)| a * b)
                    .sum::<f32>() as f64
            })
            .collect();
        let exact: HashSet<usize> = identifier_pattern(&q.query)
            .expect("identifier pattern")
            .into_iter()
            .flat_map(|(pattern, _)| {
                chunks
                    .iter()
                    .enumerate()
                    .filter_map(move |(i, chunk)| pattern.is_match(&chunk.content).then_some(i))
            })
            .collect();

        let mut lex_all: Vec<usize> = (0..corpus_len).filter(|&i| lexical[i] > 0.0).collect();
        lex_all.sort_by(|&a, &b| lexical[b].total_cmp(&lexical[a]));
        let mut sem_all: Vec<usize> = (0..corpus_len).collect();
        sem_all.sort_by(|&a, &b| similarities[b].total_cmp(&similarities[a]));
        let lex_rank: Vec<usize> = {
            let mut rank = vec![0usize; corpus_len];
            for (pos, &i) in lex_all.iter().enumerate() {
                rank[i] = pos + 1;
            }
            rank
        };
        let sem_rank: Vec<usize> = {
            let mut rank = vec![0usize; corpus_len];
            for (pos, &i) in sem_all.iter().enumerate() {
                rank[i] = pos + 1;
            }
            rank
        };

        let target_chunks: Vec<Option<usize>> = q
            .targets
            .iter()
            .map(|t| {
                chunks
                    .iter()
                    .position(|chunk| contains_line(chunk, &t.path, t.line))
            })
            .collect();

        let mut config_results = Vec::new();
        for cfg in &opts.configs {
            let mut order: Vec<usize> = Vec::new();
            let lex_take = lex_all.iter().take(cfg.lex).copied();
            let sem_take = sem_all.iter().take(cfg.sem).copied();
            let mut seen = HashSet::new();
            for i in lex_take.chain(sem_take).collect::<Vec<_>>() {
                if seen.insert(i) {
                    order.push(i);
                }
            }
            let mut exact_order: Vec<usize> = exact.iter().copied().collect();
            exact_order.sort_unstable();
            for i in exact_order {
                if seen.insert(i) {
                    order.push(i);
                }
            }
            let sem_scores: Vec<f64> = order.iter().map(|&i| similarities[i]).collect();
            let bm_scores: Vec<f64> = order.iter().map(|&i| lexical[i]).collect();
            let bm_rel = relative_scores(&bm_scores);
            let sem_rel = relative_scores(&sem_scores);
            let path_scores: Vec<f64> = order
                .iter()
                .map(|&i| {
                    if cfg.path == 0.0 {
                        return 0.0;
                    }
                    let path_words: HashSet<String> = words(&chunks[i].path).into_iter().collect();
                    let hit = terms
                        .iter()
                        .filter(|t| path_words.contains(*t))
                        .count();
                    hit as f64 / terms.len().max(1) as f64
                })
                .collect();
            let def_scores: Vec<f64> = order
                .iter()
                .map(|&i| {
                    if cfg.def == 0.0 {
                        return 0.0;
                    }
                    let mut names = HashSet::new();
                    for cap in DEF.captures_iter(&chunks[i].content) {
                        for w in words(&cap[1]) {
                            names.insert(w);
                        }
                    }
                    if terms.iter().any(|t| names.contains(t)) {
                        1.0
                    } else {
                        0.0
                    }
                })
                .collect();
            let score = |i: usize| {
                cfg.w * bm_rel[i]
                    + (1.0 - cfg.w) * sem_rel[i]
                    + cfg.path * path_scores[i]
                    + cfg.def * def_scores[i]
            };
            let mut fused: Vec<usize> = (0..order.len()).collect();
            fused.sort_by(|&a, &b| {
                exact
                    .contains(&order[b])
                    .cmp(&exact.contains(&order[a]))
                    .then_with(|| score(b).total_cmp(&score(a)))
                    .then_with(|| bm_scores[b].total_cmp(&bm_scores[a]))
            });
            let fused_rank: Vec<usize> = {
                let mut rank = vec![0usize; corpus_len];
                for (pos, &i) in fused.iter().enumerate() {
                    rank[order[i]] = pos + 1;
                }
                rank
            };
            let mut results: Vec<usize> = Vec::new();
            for &fi in &fused {
                let i = order[fi];
                let chunk = &chunks[i];
                if results.iter().any(|&j| {
                    chunks[j].path == chunk.path
                        && chunks[j].start <= chunk.start
                        && chunk.end <= chunks[j].end
                }) {
                    continue;
                }
                results.push(i);
                if results.len() == opts.top {
                    break;
                }
            }
            let mut unique_paths: Vec<&str> = Vec::new();
            for &i in &results {
                if !unique_paths.iter().any(|p| *p == chunks[i].path) {
                    unique_paths.push(&chunks[i].path);
                }
            }
            let mut targets = Vec::new();
            for (target, chunk_idx) in q.targets.iter().zip(&target_chunks) {
                let file_rank = unique_paths
                    .iter()
                    .position(|p| *p == target.path)
                    .map(|p| p + 1);
                let line_rank = results
                    .iter()
                    .position(|&i| contains_line(&chunks[i], &target.path, target.line))
                    .map(|p| p + 1);
                let (chunk_json, in_union, chunk_fused, chunk_bm_rel, chunk_sem_rel) =
                    match chunk_idx {
                        Some(i) => {
                            let pos = order.iter().position(|&x| x == *i);
                            (
                                json!({
                                    "path": chunks[*i].path,
                                    "start": chunks[*i].start,
                                    "end": chunks[*i].end,
                                    "lexical": lexical[*i],
                                    "lexical_rank": lex_rank[*i],
                                    "lexical_pool_pos": lex_all.iter().take(cfg.lex).position(|&x| x == *i).map(|p| p + 1),
                                    "semantic": similarities[*i],
                                    "semantic_rank": sem_rank[*i],
                                    "semantic_pool_pos": sem_all.iter().take(cfg.sem).position(|&x| x == *i).map(|p| p + 1),
                                    "exact_identifier": exact.contains(i),
                                }),
                                pos.is_some(),
                                pos.map(|p| score(p)),
                                pos.map(|p| bm_rel[p]),
                                pos.map(|p| sem_rel[p]),
                            )
                        }
                        None => (
                            json!({"path": target.path, "line": target.line, "missing_chunk": true}),
                            false,
                            None,
                            None,
                            None,
                        ),
                    };
                targets.push(json!({
                    "path": target.path,
                    "line": target.line,
                    "chunk": chunk_json,
                    "in_union": in_union,
                    "fused": chunk_fused,
                    "fused_rank": chunk_idx.map(|i| fused_rank[i]).filter(|&r| r > 0),
                    "bm25_relative": chunk_bm_rel,
                    "semantic_relative": chunk_sem_rel,
                    "file_rank": file_rank,
                    "file_rank10": file_rank.filter(|&r| r <= 10),
                    "line_rank": line_rank,
                }));
            }
            let rows: Vec<Value> = if cfg.name == "base" || cfg.name == "rows" {
                results
                    .iter()
                    .enumerate()
                    .map(|(pos, &i)| {
                        let chunk = &chunks[i];
                        let fi = order
                            .iter()
                            .position(|&x| x == i)
                            .expect("result in union");
                        let mut row = json!({
                            "rank": pos + 1,
                            "path": chunk.path,
                            "start": chunk.start,
                            "end": chunk.end,
                            "fused": score(fi),
                            "bm25": bm_scores[fi],
                            "bm25_relative": bm_rel[fi],
                            "semantic": sem_scores[fi],
                            "semantic_relative": sem_rel[fi],
                            "exact_identifier": exact.contains(&i),
                            "lexical_rank": lex_rank[i],
                            "semantic_rank": sem_rank[i],
                            "path_score": path_scores[fi],
                            "def_score": def_scores[fi],
                        });
                        if opts.content {
                            row["content"] = json!(chunk.content);
                        }
                        row
                    })
                    .collect()
            } else {
                Vec::new()
            };
            config_results.push(json!({
                "config": cfg,
                "pool_size": order.len(),
                "targets": targets,
                "rows": rows,
            }));
        }
        out.push(json!({
            "id": q.id,
            "query": q.query,
            "terms": terms,
            "corpus_len": corpus_len,
            "configs": config_results,
        }));
        eprintln!("done {} ({} chunks)", q.id, corpus_len);
    }

    std::fs::write(&opts.out, serde_json::to_vec(&out).unwrap()).expect("write output");
}
