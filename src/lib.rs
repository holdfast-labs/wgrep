//! Reusable sgrep core: load-once model, resident corpus, and query API,
//! plus the pinned sgrep command-line entry point.
//!
//! This crate is copied from sgrep revision
//! `0fbe8e5725f71667789be4a3abcea4e02f33afa4`. The copied modules keep the
//! upstream chunking, lexical, BM25, static-embedding, f32 cosine, fusion,
//! dedup and tie logic. The only change is the crate layout: the CLI that
//! lived in `src/main.rs` now lives here, and a thin `src/main.rs` shim calls
//! [`cli_main`]. See `BASELINE_PROVENANCE.md` for the per-file provenance.
//!
//! The resident API is [`LoadedModel`] (load the verified model assets once),
//! [`Corpus`] (build the query-independent passages once) and [`Searcher`]
//! (encode the passage vectors once and answer many queries). [`Searcher::new`]
//! encodes every stored passage once in bounded batches; [`Searcher::query`]
//! then encodes only its own query text and scores it against the immutable
//! stored vectors. It never reloads the model and never mutates or consumes the
//! stored chunks or their vectors.

mod cache;
mod codechunker;
mod index;
mod rg_input;
// M4 immutable-snapshot prototype: a manual-start shared daemon and client
// around the resident core. It is a separate binary (`wgrep-sem-shared`) and
// reuses only the original sgrep query logic. The one-shot `sgrep` CLI is
// unchanged.
pub mod live;
// Unified `wg` front-door support: invocation classification and the bounded
// authoritative `rg` child used by the lexical route. Additive; the one-shot
// `sgrep` CLI is unchanged.
pub mod lexical;
pub mod shared;
mod splitter;

use clap::{Parser, ValueEnum};
use half::f16;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// A path string shared by every chunk of one file. Serializes as a plain
/// string, so the public JSON stays byte-identical to an owned `String`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SharedStr(Arc<str>);

impl SharedStr {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(Arc::from(value.as_ref()))
    }
}

impl std::ops::Deref for SharedStr {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SharedStr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SharedStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Debug for SharedStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for SharedStr {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

impl From<&str> for SharedStr {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl Serialize for SharedStr {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SharedStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(Self(Arc::from(String::deserialize(deserializer)?)))
    }
}

pub struct Model {
    pub repo: &'static str,
    pub revision: &'static str,
    pub rows: usize,
    pub dtype: safetensors::Dtype,
    pub tokenizer: cache::Asset,
    pub weights: cache::Asset,
}
pub const CODE: Model = Model {
    repo: "minishlab/potion-code-16M-v2",
    revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
    rows: 63457,
    dtype: safetensors::Dtype::F16,
    tokenizer: cache::Asset {
        name: "tokenizer.json",
        size: 1024340,
        sha256: "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45",
    },
    weights: cache::Asset {
        name: "model.safetensors",
        size: 32490072,
        sha256: "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c",
    },
};
pub const TEXT: Model = Model {
    repo: "minishlab/potion-base-8M",
    revision: "bf8b056651a2c21b8d2565580b8569da283cab23",
    rows: 29528,
    dtype: safetensors::Dtype::F32,
    tokenizer: cache::Asset {
        name: "tokenizer.json",
        size: 683666,
        sha256: "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
    },
    weights: cache::Asset {
        name: "model.safetensors",
        size: 30236760,
        sha256: "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
    },
};

#[derive(Clone, Copy, PartialEq, ValueEnum)]
pub enum ModelChoice {
    Auto,
    Code,
    Text,
}

static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").unwrap());
static STOP: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    "a an and are as at be been but by can do does for from had has have how i if in into is it its me not of on or our should so that the their them there these they this to use using was we what when where which who will with would you your function description purpose input output".split_whitespace().collect()
});

#[derive(Parser)]
#[command(
    version,
    about = "Search code and text with ripgrep and local static embeddings."
)]
pub struct Args {
    /// Store models, parsers and search indexes under this directory.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Recompute and replace this corpus's search index before searching.
    #[arg(long, conflicts_with = "no_cache")]
    refresh_cache: bool,
    /// Download assets into a disposable cache and compute embeddings without reuse.
    #[arg(long, conflicts_with = "cache_dir")]
    fresh_assets: bool,
    query: String,
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(short = 'n', default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    count: u32,
    #[arg(long)]
    json: bool,
    /// Auto uses text embeddings for prose-only candidates, code embeddings otherwise.
    #[arg(long, value_enum, default_value_t = ModelChoice::Auto)]
    model: ModelChoice,
    /// Add ripgrep --json matches and context from a file, or - for stdin.
    #[arg(long, value_name = "FILE")]
    rg_json: Option<PathBuf>,
    /// Rank only ripgrep --json passages read from standard input.
    #[arg(long, conflicts_with = "rg_json")]
    stdin: bool,
    /// Search without reading or writing the search index.
    #[arg(long)]
    no_cache: bool,
    /// Include lexical, semantic and fused scores in JSON output.
    #[arg(long, requires = "json")]
    explain: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Chunk {
    pub path: SharedStr,
    pub start: usize,
    pub end: usize,
    pub content: Box<str>,
    #[serde(skip)]
    pub counts: HashMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scores: Option<Scores>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Scores {
    pub bm25: f64,
    pub semantic: f64,
    pub bm25_relative: f64,
    pub semantic_relative: f64,
    pub fused: f64,
    pub exact_identifier: bool,
}

pub fn identifier_pattern(query: &str) -> Result<Option<(Regex, Vec<&str>)>> {
    static IDENTIFIER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\p{L}_$][\p{L}\p{M}\p{N}_$]*").unwrap());
    let query = query.trim();
    let literals: Vec<_> = IDENTIFIER
        .find_iter(query)
        .map(|m| m.as_str())
        .filter(|word| {
            *word == query
                || word.contains(['_', '$'])
                || CAMEL.is_match(word)
                || word.chars().any(char::is_numeric)
        })
        .collect();
    if literals.is_empty() {
        return Ok(None);
    }
    let alternatives = literals
        .iter()
        .map(|s| regex::escape(s))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = Regex::new(&format!(
        r"(?:^|[^\p{{L}}\p{{M}}\p{{N}}_$])(?:{alternatives})(?:$|[^\p{{L}}\p{{M}}\p{{N}}_$])"
    ))?;
    Ok(Some((pattern, literals)))
}

fn words(text: &str) -> Vec<String> {
    let text = CAMEL.replace_all(text, "${1} ${2}").to_lowercase();
    WORD.find_iter(&text)
        .map(|m| m.as_str())
        .filter(|w| w.chars().count() > 1 && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

pub fn filename(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).into()
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(bytes).as_ref().into()
    }
}

pub fn source_files(path: &Path) -> Result<(&Path, Vec<PathBuf>)> {
    let root = if path.is_dir() {
        path
    } else {
        path.parent().unwrap()
    };
    let mut command = Command::new("rg");
    command.args(["--no-config", "--files", "-0"]);
    let target = if path.is_dir() {
        Path::new(".")
    } else {
        Path::new(path.file_name().unwrap())
    };
    let result = command
        .arg("--")
        .arg(target)
        .current_dir(root)
        .output()
        .map_err(|e| format!("ripgrep is required: {e}"))?;
    if !matches!(result.status.code(), Some(0 | 1)) {
        return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
    }
    let mut files: Vec<_> = result
        .stdout
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(filename)
        .collect();
    files.sort_unstable_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    Ok((root, files))
}

pub fn candidates(
    path: &Path,
    expected: Option<&HashMap<PathBuf, [u8; 32]>>,
) -> Result<Vec<Chunk>> {
    let (root, files) = source_files(path)?;
    let chunks: Result<Vec<Vec<Chunk>>> = files
        .par_iter()
        .map(|name| {
            let mut chunks = Vec::new();
            let relative = name.strip_prefix(".").unwrap_or(name);
            let shared_path = SharedStr::new(relative.to_string_lossy());
            let file = root.join(relative);
            let bytes = std::fs::read(&file)?;
            if let Some(expected) = expected {
                use sha2::Digest;
                let digest: [u8; 32] = sha2::Sha256::digest(&bytes).into();
                if expected.get(name) != Some(&digest) {
                    return Err("source changed while reading index input; retry search".into());
                }
            }
            if bytes.contains(&0) {
                return Ok(chunks);
            }
            let source = String::from_utf8_lossy(&bytes)
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            let lines: Vec<&str> = source.split_inclusive('\n').collect();
            for (first, last) in splitter::ranges(&source, relative)? {
                chunks.push(make_chunk(
                    shared_path.clone(),
                    first,
                    last,
                    lines[first - 1..last].concat(),
                ));
            }
            Ok(chunks)
        })
        .collect();
    Ok(chunks?.into_iter().flatten().collect())
}

pub fn make_chunk<P: Into<SharedStr>>(
    path: P,
    start: usize,
    end: usize,
    content: String,
) -> Chunk {
    let path = path.into();
    let mut counts = HashMap::new();
    for word in words(&content).into_iter().chain(words(&path)) {
        *counts.entry(word).or_insert(0) += 1;
    }
    Chunk {
        path,
        start,
        end,
        content: content.into(),
        counts,
        scores: None,
    }
}

pub fn bm25(chunks: &[Chunk], terms: &[String], corpus_len: usize) -> Vec<f64> {
    bm25_impl(chunks.iter(), terms, corpus_len)
}

/// Identical BM25 arithmetic and ordering as [`bm25`], over borrowed chunk
/// views. The live index uses it to score `Arc`-shared rows without cloning
/// their content or word maps.
pub fn bm25_borrowed<'a>(
    chunks: impl IntoIterator<Item = &'a Chunk>,
    terms: &[String],
    corpus_len: usize,
) -> Vec<f64> {
    bm25_impl(chunks.into_iter(), terms, corpus_len)
}

fn bm25_impl<'a>(
    chunks: impl Iterator<Item = &'a Chunk>,
    terms: &[String],
    corpus_len: usize,
) -> Vec<f64> {
    let chunks: Vec<&Chunk> = chunks.collect();
    let lengths: Vec<usize> = chunks.iter().map(|c| c.counts.values().sum()).collect();
    let average =
        (lengths[..corpus_len].iter().sum::<usize>() as f64 / corpus_len.max(1) as f64).max(1.0);
    let idfs: Vec<_> = terms
        .iter()
        .map(|t| {
            let df = chunks[..corpus_len]
                .iter()
                .filter(|c| c.counts.contains_key(t))
                .count() as f64;
            (1.0 + (corpus_len as f64 - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    chunks
        .iter()
        .zip(lengths)
        .map(|(c, length)| {
            terms
                .iter()
                .zip(&idfs)
                .filter_map(|(t, idf)| {
                    c.counts.get(t).map(|count| {
                        let count = *count as f64;
                        idf * count * 2.5 / (count + 1.5 * (0.25 + 0.75 * length as f64 / average))
                    })
                })
                .sum()
        })
        .collect()
}

pub fn relative_scores(scores: &[f64]) -> Vec<f64> {
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    scores
        .iter()
        .map(|s| {
            if max > min {
                (s - min) / (max - min)
            } else {
                0.0
            }
        })
        .collect()
}

/// Load the pinned model assets once, then embed many texts from them.
///
/// The embedding math is the copied upstream math: per-text mean of the
/// selected rows, then L2 normalisation. The verified `model.safetensors` bytes
/// are decoded once into a dense f32 row table in [`LoadedModel::load_meta`];
/// the raw bytes are then dropped. [`LoadedModel::embed`] reads only the table.
pub struct LoadedModel {
    rows: usize,
    revision: &'static str,
    tokenizer: tokenizers::Tokenizer,
    table: Vec<f32>,
    encoded_texts: AtomicU64,
}

impl LoadedModel {
    pub fn load(choice: ModelChoice, cache: &cache::Cache) -> Result<Self> {
        let model = if choice == ModelChoice::Text {
            &TEXT
        } else {
            &CODE
        };
        Self::load_meta(model, cache)
    }

    pub fn load_meta(model: &Model, cache: &cache::Cache) -> Result<Self> {
        let mut tokenizer =
            tokenizers::Tokenizer::from_bytes(cache.model_bytes(model, &model.tokenizer)?)?;
        tokenizer.with_truncation(None)?;
        tokenizer.with_padding(None);
        let raw = cache.model_bytes(model, &model.weights)?;
        let tensors = safetensors::SafeTensors::deserialize(&raw)?;
        let tensor = tensors.tensor("embeddings")?;
        if tensor.dtype() != model.dtype || tensor.shape() != [model.rows, 256] {
            return Err("unexpected model tensor".into());
        }
        let width = if model.dtype == safetensors::Dtype::F16 {
            2
        } else {
            4
        };
        let mut table = Vec::with_capacity(model.rows * 256);
        for bytes in tensor.data().chunks_exact(width) {
            table.push(if width == 2 {
                f16::from_le_bytes(bytes.try_into().unwrap()).to_f32()
            } else {
                f32::from_le_bytes(bytes.try_into().unwrap())
            });
        }
        if table.len() != model.rows * 256 {
            return Err("unexpected model tensor".into());
        }
        // Release the raw safetensors allocation now that the f32 table owns the
        // model rows. The tokenizer stays resident for query and passage encodes.
        drop(tensors);
        drop(raw);
        Ok(Self {
            rows: model.rows,
            revision: model.revision,
            tokenizer,
            table,
            encoded_texts: AtomicU64::new(0),
        })
    }

    pub fn revision(&self) -> &'static str {
        self.revision
    }

    /// Count of texts handed to [`LoadedModel::embed`] since load. It lets a
    /// caller prove that passage encoding happens once at construction and
    /// that each query encodes only its own text.
    pub fn encoded_texts(&self) -> u64 {
        self.encoded_texts.load(Ordering::Relaxed)
    }

    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.embed_refs(&texts)
    }

    /// Embed a borrowed slice without cloning its texts. Callers that split a
    /// corpus into batches can run this on several threads; the arithmetic and
    /// order are identical to [`LoadedModel::embed`].
    pub fn embed_refs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.encoded_texts
            .fetch_add(texts.len() as u64, Ordering::Relaxed);
        let unknown = self.tokenizer.token_to_id("[UNK]");
        let encoded = if texts.len() == 1 {
            vec![self.tokenizer.encode_fast(texts[0].as_str(), false)?]
        } else {
            self.tokenizer
                .encode_batch_fast(texts.iter().map(String::as_str).collect::<Vec<_>>(), false)?
        };
        // Validate every selected id before the parallel sum so an out-of-range
        // token reports the same error in the same text order as before.
        for encoding in &encoded {
            for &id in encoding.get_ids() {
                if Some(id) != unknown && id as usize >= self.rows {
                    return Err("token outside model vocabulary".into());
                }
            }
        }
        encoded
            .par_iter()
            .map(|encoding| self.vector_from_ids(encoding.get_ids(), unknown))
            .collect()
    }

    /// Embed many passages with one tokenizer call per text, distributed over
    /// the global worker pool. `encode_batch_fast` parallelizes internally; a
    /// full corpus build is faster when each worker owns a whole text instead
    /// of oversubscribing the same pool with nested batches. The token ids and
    /// the summation arithmetic are identical to [`LoadedModel::embed_refs`].
    pub fn embed_parallel(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.encoded_texts
            .fetch_add(texts.len() as u64, Ordering::Relaxed);
        let unknown = self.tokenizer.token_to_id("[UNK]");
        texts
            .par_iter()
            .map(|text| {
                let encoding = self.tokenizer.encode_fast(text.as_str(), false)?;
                self.vector_from_ids(encoding.get_ids(), unknown)
            })
            .collect()
    }

    /// Batch-encode on a fixed number of worker threads, each with its own
    /// single-thread rayon pool. `encode_batch_fast` parallelizes internally;
    /// isolating each worker prevents nested pools from oversubscribing the
    /// host while keeping the lower per-passage overhead of batch encoding.
    pub fn embed_parallel_batched(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8)
            .min(texts.len().max(1));
        if threads <= 1 || texts.len() < 512 {
            return self.embed_refs(texts);
        }
        self.encoded_texts
            .fetch_add(texts.len() as u64, Ordering::Relaxed);
        let unknown = self.tokenizer.token_to_id("[UNK]");
        let per = texts.len().div_ceil(threads);
        let results: Vec<Result<Vec<Vec<f32>>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = texts
                .chunks(per)
                .map(|slice| {
                    scope.spawn(move || -> Result<Vec<Vec<f32>>> {
                        let pool = rayon::ThreadPoolBuilder::new()
                            .num_threads(1)
                            .build()
                            .map_err(|e| e.to_string())?;
                        pool.install(|| {
                            let mut out = Vec::with_capacity(slice.len());
                            for batch in slice.chunks(256) {
                                let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
                                for encoding in self.tokenizer.encode_batch_fast(refs, false)? {
                                    out.push(self.vector_from_ids(encoding.get_ids(), unknown)?);
                                }
                            }
                            Ok(out)
                        })
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("embed worker panicked".into()))
                })
                .collect()
        });
        let mut out = Vec::with_capacity(texts.len());
        for result in results {
            out.extend(result?);
        }
        Ok(out)
    }

    fn vector_from_ids(&self, ids: &[u32], unknown: Option<u32>) -> Result<Vec<f32>> {
        let mut vector = vec![0.0f32; 256];
        let selected: Vec<_> = ids.iter().filter(|&&id| Some(id) != unknown).collect();
        for &&id in &selected {
            if id as usize >= self.rows {
                return Err("token outside model vocabulary".into());
            }
            let offset = id as usize * 256;
            for (x, weight) in vector.iter_mut().zip(&self.table[offset..offset + 256]) {
                *x += weight;
            }
        }
        if !selected.is_empty() {
            for x in &mut vector {
                *x /= selected.len() as f32;
            }
        }
        let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt() + 1e-32;
        for x in &mut vector {
            *x /= norm;
        }
        if !vector.iter().all(|x| x.is_finite()) {
            return Err("non-finite embedding".into());
        }
        Ok(vector)
    }
}

fn embed(texts: Vec<String>, model: &Model, cache: &cache::Cache) -> Result<Vec<Vec<f32>>> {
    LoadedModel::load_meta(model, cache)?.embed(texts)
}

pub fn is_prose(path: &str) -> bool {
    let extension = Path::new(path)
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "md" | "mdx" | "txt" | "rst" | "adoc" | "org" | "text"
    )
}

pub fn search(args: &Args) -> Result<Vec<Chunk>> {
    let mut terms = words(&args.query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Err("query has no searchable words".into());
    }
    let path = args.path.canonicalize()?;
    if !path.is_file() && !path.is_dir() {
        return Err("not a file or directory".into());
    }
    let cache = cache::Cache::new(args.cache_dir.as_deref(), args.fresh_assets)?;
    if !args.no_cache
        && !args.fresh_assets
        && !args.stdin
        && args.rg_json.is_none()
        && let Some(results) = index::search(args, &path, &terms, &cache)?
    {
        return Ok(results);
    }
    let mut chunks = if args.stdin {
        rg_input::read(Path::new("-"), &path)?
    } else {
        candidates(&path, None)?
    };
    let mut seen = HashSet::new();
    chunks.retain(|c| seen.insert((c.path.clone(), c.start, c.end)));
    let corpus_len = chunks.len();
    let mut supplied = Vec::new();
    if let Some(input) = &args.rg_json {
        let mut locations: HashMap<_, _> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| ((c.path.clone(), c.start, c.end), i))
            .collect();
        for chunk in rg_input::read(input, &path)? {
            let key = (chunk.path.clone(), chunk.start, chunk.end);
            let i = *locations.entry(key).or_insert_with(|| {
                let i = chunks.len();
                chunks.push(chunk);
                i
            });
            supplied.push(i);
        }
    }
    if chunks.is_empty() {
        return Ok(Vec::new());
    }
    let lexical = bm25(&chunks, &terms, corpus_len);
    let prose_only = chunks.iter().all(|chunk| is_prose(&chunk.path));
    let model =
        if args.model == ModelChoice::Text || (args.model == ModelChoice::Auto && prose_only) {
            &TEXT
        } else {
            &CODE
        };
    let mut texts: Vec<String> = chunks
        .iter()
        .map(|c| format!("{}\n{}", c.path, c.content))
        .collect();
    texts.insert(0, args.query.clone());
    let mut vectors = embed(texts, model, &cache)?;
    let query_vector = vectors.remove(0);
    let similarities: Vec<f64> = vectors
        .iter()
        .map(|v| v.iter().zip(&query_vector).map(|(a, b)| a * b).sum::<f32>() as f64)
        .collect();
    let exact: HashSet<_> = identifier_pattern(&args.query)?
        .into_iter()
        .flat_map(|(pattern, _)| {
            chunks
                .iter()
                .enumerate()
                .filter_map(move |(i, chunk)| pattern.is_match(&chunk.content).then_some(i))
        })
        .collect();
    let mut chunks: Vec<_> = chunks.into_iter().map(Some).collect();
    rank(
        args,
        corpus_len,
        &lexical,
        &similarities,
        supplied,
        &exact,
        |i| Ok(chunks[i].take().unwrap()),
    )
}

fn rank(
    args: &Args,
    corpus_len: usize,
    lexical: &[f64],
    similarities: &[f64],
    supplied: Vec<usize>,
    exact: &HashSet<usize>,
    get_chunk: impl FnMut(usize) -> Result<Chunk>,
) -> Result<Vec<Chunk>> {
    rank_impl(
        corpus_len,
        lexical,
        similarities,
        supplied,
        exact,
        args.stdin,
        args.explain,
        args.count as usize,
        get_chunk,
    )
}

#[allow(clippy::too_many_arguments)]
fn rank_impl(
    corpus_len: usize,
    lexical: &[f64],
    similarities: &[f64],
    supplied: Vec<usize>,
    exact: &HashSet<usize>,
    stdin: bool,
    explain: bool,
    top_n: usize,
    mut get_chunk: impl FnMut(usize) -> Result<Chunk>,
) -> Result<Vec<Chunk>> {
    let mut order: Vec<_> = if stdin {
        (0..lexical.len()).collect()
    } else {
        let mut lexical_order: Vec<_> = (0..corpus_len).filter(|&i| lexical[i] > 0.0).collect();
        lexical_order.sort_by(|&a, &b| lexical[b].total_cmp(&lexical[a]));
        lexical_order.truncate(200);
        let mut semantic_order: Vec<_> = (0..corpus_len).collect();
        semantic_order.sort_by(|&a, &b| similarities[b].total_cmp(&similarities[a]));
        semantic_order.truncate(200);
        lexical_order.extend(semantic_order);
        lexical_order.extend(supplied);
        let mut exact_order: Vec<_> = exact.iter().copied().collect();
        exact_order.sort_unstable();
        lexical_order.extend(exact_order);
        lexical_order
    };
    let mut selected = HashSet::new();
    order.retain(|&i| selected.insert(i));
    let semantic: Vec<_> = order.iter().map(|&i| similarities[i]).collect();
    let bm25_scores: Vec<_> = order.iter().map(|&i| lexical[i]).collect();
    let bm25_relative = relative_scores(&bm25_scores);
    let semantic_relative = relative_scores(&semantic);
    let score = |i: usize| 0.25 * bm25_relative[i] + 0.75 * semantic_relative[i];
    let mut fused: Vec<_> = (0..order.len()).collect();
    fused.sort_by(|&a, &b| {
        exact
            .contains(&order[b])
            .cmp(&exact.contains(&order[a]))
            .then_with(|| score(b).total_cmp(&score(a)))
            .then_with(|| bm25_scores[b].total_cmp(&bm25_scores[a]))
    });
    let mut results: Vec<Chunk> = Vec::new();
    for i in fused {
        let mut chunk = get_chunk(order[i])?;
        if explain {
            chunk.scores = Some(Scores {
                bm25: bm25_scores[i],
                semantic: semantic[i],
                bm25_relative: bm25_relative[i],
                semantic_relative: semantic_relative[i],
                fused: score(i),
                exact_identifier: exact.contains(&order[i]),
            });
        }
        if results
            .iter()
            .any(|c| c.path == chunk.path && c.start <= chunk.start && chunk.end <= c.end)
        {
            continue;
        }
        results.push(chunk);
        if results.len() == top_n {
            break;
        }
    }
    Ok(results)
}

/// The query-independent passages of one corpus, built once.
///
/// [`Corpus::build`] runs the pinned file listing and splitter exactly as the
/// one-shot CLI does (with `--no-cache`). The stored chunks are the ranking
/// input for every later query. All fields are read-only after construction.
pub struct Corpus {
    path: PathBuf,
    chunks: Vec<Chunk>,
    corpus_len: usize,
}

impl Corpus {
    pub fn build(path: &Path) -> Result<Corpus> {
        let path = path.canonicalize()?;
        if !path.is_file() && !path.is_dir() {
            return Err("not a file or directory".into());
        }
        let mut chunks = candidates(&path, None)?;
        let mut seen = HashSet::new();
        chunks.retain(|c| seen.insert((c.path.clone(), c.start, c.end)));
        let corpus_len = chunks.len();
        Ok(Corpus {
            path,
            chunks,
            corpus_len,
        })
    }

    /// The canonical corpus root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored passages, read-only. Callers cannot change the chunks that
    /// the resident passage vectors were built from.
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Number of stored passages.
    pub fn len(&self) -> usize {
        self.corpus_len
    }

    pub fn is_empty(&self) -> bool {
        self.corpus_len == 0
    }
}

/// Passage texts per embedding batch. Bounded so construction memory stays
/// small. Batching cannot change a vector: a passage vector depends only on its
/// own tokens and the fixed model rows, never on batch neighbours.
const PASSAGE_BATCH: usize = 256;

fn passage_text(chunk: &Chunk) -> String {
    format!("{}\n{}", chunk.path, chunk.content)
}

fn embed_passages(model: &LoadedModel, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>> {
    let mut vectors = Vec::with_capacity(chunks.len());
    for batch in chunks.chunks(PASSAGE_BATCH) {
        let texts: Vec<String> = batch.iter().map(passage_text).collect();
        vectors.extend(model.embed(texts)?);
    }
    Ok(vectors)
}

/// One resident model, one resident corpus, and the corpus passages encoded
/// once.
///
/// Build it once with [`Searcher::new`]. It loads the verified model assets
/// once, builds the chunks once, and encodes the passage vectors once in
/// bounded batches. Every field is read-only after construction, so the stored
/// chunks and their passage vectors cannot drift apart. [`Searcher::query`]
/// encodes only the query text and scores it against the immutable stored
/// vectors with the same pinned BM25, f32 dot-product, identifier, fusion,
/// dedup and tie code as the one-shot CLI.
pub struct Searcher {
    model: std::sync::Arc<LoadedModel>,
    corpus: Corpus,
    passage_vectors: Vec<Vec<f32>>,
}

impl Searcher {
    pub fn new(path: &Path, choice: ModelChoice, cache: &cache::Cache) -> Result<Searcher> {
        let corpus = Corpus::build(path)?;
        let prose_only = corpus.chunks.iter().all(|c| is_prose(&c.path));
        let effective =
            if choice == ModelChoice::Text || (choice == ModelChoice::Auto && prose_only) {
                ModelChoice::Text
            } else {
                ModelChoice::Code
            };
        let model = LoadedModel::load(effective, cache)?;
        let passage_vectors = embed_passages(&model, &corpus.chunks)?;
        Ok(Searcher {
            model: std::sync::Arc::new(model),
            corpus,
            passage_vectors,
        })
    }

    /// Build a resident searcher, reading the pinned model assets from
    /// `cache_dir` (or the platform caches when `None`). This is the public
    /// constructor: it makes the asset cache an explicit boundary option and
    /// lets callers build a [`Searcher`] without naming the private asset
    /// cache type. `refresh` is never requested here.
    pub fn open(path: &Path, choice: ModelChoice, cache_dir: Option<&Path>) -> Result<Searcher> {
        let cache = cache::Cache::new(cache_dir, false)?;
        Searcher::new(path, choice, &cache)
    }

    /// The resident model.
    pub fn model(&self) -> &LoadedModel {
        &self.model
    }

    /// The resident corpus chunks, read-only.
    pub fn corpus(&self) -> &Corpus {
        &self.corpus
    }

    /// The immutable passage vectors, one per stored chunk, in chunk order.
    pub fn passage_vectors(&self) -> &[Vec<f32>] {
        &self.passage_vectors
    }

    pub fn query(&self, query: &str, top_n: usize, explain: bool) -> Result<Vec<Chunk>> {
        let mut terms = words(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Err("query has no searchable words".into());
        }
        let chunks = &self.corpus.chunks;
        let corpus_len = self.corpus.corpus_len;
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let lexical = bm25(chunks, &terms, corpus_len);
        let query_vector = self.model.embed(vec![query.to_string()])?.remove(0);
        let similarities: Vec<f64> = self
            .passage_vectors
            .iter()
            .map(|v| v.iter().zip(&query_vector).map(|(a, b)| a * b).sum::<f32>() as f64)
            .collect();
        let exact: HashSet<_> = identifier_pattern(query)?
            .into_iter()
            .flat_map(|(pattern, _)| {
                chunks
                    .iter()
                    .enumerate()
                    .filter_map(move |(i, chunk)| pattern.is_match(&chunk.content).then_some(i))
            })
            .collect();
        rank_impl(
            corpus_len,
            &lexical,
            &similarities,
            Vec::new(),
            &exact,
            false,
            explain,
            top_n,
            |i| Ok(chunks[i].clone()),
        )
    }
}

pub fn run() -> Result<ExitCode> {
    let args = Args::parse();
    let results = search(&args)?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if args.json {
        writeln!(out, "{}", serde_json::to_string(&results)?)?;
    } else {
        for c in &results {
            writeln!(
                out,
                "{}:{}-{}\n{}\n",
                c.path,
                c.start,
                c.end,
                c.content.trim_end()
            )?;
        }
    }
    out.flush()?;
    Ok(ExitCode::from(if results.is_empty() { 1 } else { 0 }))
}

pub fn cli_main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("sgrep: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod resident_parity {
    use super::*;

    fn fixture_dir() -> PathBuf {
        std::env::var_os("SGREP_FIXTURE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("bench/WGREP_SGREP_PARITY_FIXTURE")
            })
    }

    fn control_binary() -> PathBuf {
        std::env::var_os("SGREP_CONTROL")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from("/srv/projects/wispd/sgrep-control-0fbe8e5/target/release/sgrep")
            })
    }

    /// Run all twelve fixture queries through one resident model and corpus and
    /// compare the exact ordered source, ranges, content and scores to the
    /// pinned control binary. Ignored by default because it needs the pinned
    /// control, the fixture, and the verified offline model and parser caches.
    #[test]
    #[ignore = "needs pinned control binary, fixture, and verified offline model/parser caches"]
    fn resident_object_answers_six_language_fixture() {
        use sha2::{Digest, Sha256};

        let fixture = fixture_dir();
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.join("queries.json")).unwrap()).unwrap();
        let queries = meta["queries"].as_array().unwrap().clone();
        assert_eq!(queries.len(), 12, "fixture must hold twelve queries");
        let root = fixture.join("corpus");

        for q in &queries {
            let rel = q["path"].as_str().unwrap();
            let want = q["sha256"].as_str().unwrap();
            let bytes = std::fs::read(root.join(rel)).unwrap();
            let got: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(got, want, "fixture source hash mismatch: {rel}");
        }

        let cache = cache::Cache::new(None, false).unwrap();
        let searcher = Searcher::new(&root, ModelChoice::Code, &cache).unwrap();
        let model_revision = searcher.model().revision();
        assert_eq!(
            model_revision, CODE.revision,
            "resident model must be the pinned code model"
        );
        let before = serde_json::to_vec(searcher.corpus().chunks()).unwrap();
        let before_len = searcher.corpus().chunks().len();
        let vectors_before = serde_json::to_vec(searcher.passage_vectors()).unwrap();
        assert_eq!(
            searcher.passage_vectors().len(),
            before_len,
            "one passage vector per stored chunk"
        );
        assert_eq!(
            searcher.model().encoded_texts(),
            before_len as u64,
            "only the stored passages were encoded at construction"
        );

        let control = control_binary();
        let mut mismatches = 0usize;
        for (i, q) in queries.iter().enumerate() {
            let id = q["id"].as_str().unwrap();
            let query = q["query"].as_str().unwrap();
            let before_query = searcher.model().encoded_texts();
            let resident = searcher.query(query, 10, true).unwrap();
            assert_eq!(
                searcher.model().encoded_texts(),
                before_query + 1,
                "query {id} encoded more than its own text"
            );
            let resident_json = serde_json::to_string(&resident).unwrap();
            let out = Command::new(&control)
                .arg(query)
                .arg(&root)
                .args([
                    "--model",
                    "code",
                    "--json",
                    "--explain",
                    "--no-cache",
                    "-n",
                    "10",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "control failed for {id}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let control_json = String::from_utf8(out.stdout).unwrap();
            let a: serde_json::Value = serde_json::from_str(&control_json).unwrap();
            let b: serde_json::Value = serde_json::from_str(&resident_json).unwrap();
            let rows = a.as_array().map(|r| r.len()).unwrap_or(0);
            if a != b {
                mismatches += 1;
                eprintln!("MISMATCH {i:02} {id}");
                eprintln!("  control : {control_json}");
                eprintln!("  resident: {resident_json}");
            } else {
                println!("PASS {i:02} {id} rows={rows} score_max_abs_diff=0.0");
            }
        }

        let after = serde_json::to_vec(searcher.corpus().chunks()).unwrap();
        assert_eq!(
            before, after,
            "resident corpus chunks changed while answering queries"
        );
        assert_eq!(
            before_len,
            searcher.corpus().chunks().len(),
            "resident corpus chunk count changed"
        );
        let vectors_after = serde_json::to_vec(searcher.passage_vectors()).unwrap();
        assert_eq!(
            vectors_before, vectors_after,
            "resident passage vectors changed while answering queries"
        );
        let encoded = searcher.model().encoded_texts();
        assert_eq!(
            encoded,
            before_len as u64 + queries.len() as u64,
            "passages must be encoded once at construction; each query encodes only its own text"
        );
        assert_eq!(
            mismatches,
            0,
            "{mismatches}/{} resident vs control mismatches",
            queries.len()
        );
        println!(
            "resident parity: {}/{} exact pairs; one model ({model_revision}) and one corpus ({before_len} chunks) answered all queries; stored chunks and {before_len} passage vectors unchanged; encoded texts={encoded} ({before_len} at construction + {} queries)",
            queries.len(),
            queries.len(),
            queries.len()
        );
    }
}
