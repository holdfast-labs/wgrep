//! Analysis-only batch scorer for wg ranking experiments.
//!
//! The semantic path matches `rank_impl` as of 0.3: damped one-word file-stem
//! boost and substantial same-file overlap removal. Optional, off-by-default
//! signals: definition-name boost (IDF/coverage/stemming), query gate, narrow
//! definition keywords per language, test/legacy path penalty, and RRF fusion.
//! A config with no semantic reranker knob keeps the classic containment rule.
//! This file is not part of the product.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

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
// Legacy broad keyword set. Kept verbatim for baseline/report comparison.
static DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?(?:extern\s+(?:"[^"]+"\s+)?)?(?:fn|struct|enum|trait|type|mod|impl|class|def|interface|function|func|let|var|const)\s+([A-Za-z_$][A-Za-z0-9_$]*)"#,
    )
    .unwrap()
});

// Narrow definition sites. No `let`, `var`, `const` except top-level JS/TS
// bindings, no `impl`, no `namespace` bodies.
static KW_PY: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
        Regex::new(r"(?m)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_RUST: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r#"(?m)^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+(?:"[^"]*"\s+)?)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"#).unwrap(),
        Regex::new(r"(?m)^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:struct|enum|trait|union|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_GO: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
        Regex::new(r"(?m)^type\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_JS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:export\s+(?:default\s+)?)?(?:declare\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap(),
        Regex::new(r"(?m)^\s*(?:export\s+(?:default\s+)?)?(?:abstract\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap(),
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:interface|enum|type)\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap(),
        // Top-level bindings only: no leading indentation.
        Regex::new(r"(?m)^(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*[=:]").unwrap(),
    ]
});
static KW_JAVA: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:(?:public|private|protected|static|final|abstract|sealed|non-sealed|strictfp)\s+)*(?:class|interface|enum|record)\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap(),
    ]
});
static KW_C: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:template\s*<[^>\n]*>\s*)?(?:class|struct|union|enum(?:\s+class)?)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_RUBY: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*def\s+(?:self\.)?([A-Za-z_][A-Za-z0-9_!?]*)").unwrap(),
        Regex::new(r"(?m)^\s*(?:class|module)\s+([A-Za-z_][A-Za-z0-9_:]*)").unwrap(),
    ]
});
static KW_PHP: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:(?:public|private|protected|static|abstract|final)\s+)*function\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
        Regex::new(r"(?m)^\s*(?:abstract\s+|final\s+)?(?:class|interface|trait|enum)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_SWIFT: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:(?:public|private|internal|open|static|final|override)\s+)*(?:class|struct|enum|protocol|func)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_KOTLIN: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:(?:public|private|internal|open|abstract|sealed|data|inline)\s+)*(?:class|interface|object|fun)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});
static KW_SCALA: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?m)^\s*(?:(?:private|protected|final|sealed|abstract|implicit)\s+)*(?:class|trait|object|def)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap(),
    ]
});

// Test/legacy path rules, in the style of Semble's penalties.
static TEST_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|/)(?:test_[^/]*\.py|[^/]*_test\.py|[^/]*_test\.go|[^/]*Tests?\.java|[^/]*Test\.php|[^/]*_spec\.rb|[^/]*_test\.rb|[^/]*\.test\.[jt]sx?|[^/]*\.spec\.[jt]sx?|[^/]*Tests?\.kt|[^/]*Tests?\.swift|[^/]*Tests?\.cs|test_[^/]*\.cpp|[^/]*_test\.cpp|test_[^/]*\.c|[^/]*_test\.c|[^/]*Spec\.scala|[^/]*Suite\.scala|[^/]*Test\.scala|[^/]*_test\.dart|test_[^/]*\.dart|[^/]*_spec\.lua|[^/]*_test\.lua|test_[^/]*\.lua|test_helpers?[^/]*\.\w+)$",
    )
    .unwrap()
});
static TEST_DIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|/)(?:tests?|__tests__|spec|testing)(?:/|$)").unwrap());
static LEGACY_DIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|/)(?:compat|_compat|legacy|deprecated)(?:/|$)").unwrap());

fn stem(w: &str) -> &str {
    if w.len() >= 4 && w.ends_with('s') && !w.ends_with("ss") {
        &w[..w.len() - 1]
    } else {
        w
    }
}

fn words(text: &str) -> Vec<String> {
    let text = CAMEL.replace_all(text, "${1} ${2}").to_lowercase();
    WORD.find_iter(&text)
        .map(|m| m.as_str())
        .filter(|w| w.chars().count() > 1 && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

fn keywords_for(path: &str) -> Option<&'static Vec<Regex>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "py" => Some(&KW_PY),
        "rs" => Some(&KW_RUST),
        "go" => Some(&KW_GO),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Some(&KW_JS),
        "java" | "cs" => Some(&KW_JAVA),
        "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" => Some(&KW_C),
        "rb" => Some(&KW_RUBY),
        "php" => Some(&KW_PHP),
        "swift" => Some(&KW_SWIFT),
        "kt" | "kts" => Some(&KW_KOTLIN),
        "scala" => Some(&KW_SCALA),
        _ => None,
    }
}

/// One precomputed definition index per (keyword set, stemming) pair.
#[derive(Default)]
struct DefIndex {
    /// chunk index -> name token ids for each distinct defined name.
    names: Vec<Vec<Vec<u32>>>,
    /// chunk index -> normalised full name, parallel to `names`.
    full: Vec<Vec<String>>,
    token_ids: HashMap<String, u32>,
    token_df: Vec<u32>,
    token_idf: Vec<f64>,
    /// Normalised full names, for the identifier-style query gate.
    name_set: HashSet<String>,
    bytes: usize,
}

impl DefIndex {
    fn build(chunks: &[wgrep::Chunk], use_stem: bool, narrow: bool) -> DefIndex {
        let corpus_len = chunks.len();
        let mut raw: Vec<Vec<(String, Vec<String>)>> = Vec::with_capacity(corpus_len);
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut name_set: HashSet<String> = HashSet::new();
        for chunk in chunks {
            let mut per_chunk: Vec<(String, Vec<String>)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut push = |raw_name: &str, per_chunk: &mut Vec<(String, Vec<String>)>| {
                let normalised = raw_name.to_lowercase().replace('_', "");
                let toks: Vec<String> = words(raw_name)
                    .into_iter()
                    .map(|w| if use_stem { stem(&w).to_owned() } else { w })
                    .collect();
                if toks.is_empty() || !seen.insert(normalised.clone()) {
                    return;
                }
                name_set.insert(normalised.clone());
                for t in &toks {
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
                per_chunk.push((normalised, toks));
            };
            if narrow {
                if let Some(pats) = keywords_for(&chunk.path) {
                    for pat in pats {
                        for cap in pat.captures_iter(&chunk.content) {
                            push(&cap[1], &mut per_chunk);
                        }
                    }
                }
            } else {
                for cap in DEF.captures_iter(&chunk.content) {
                    push(&cap[1], &mut per_chunk);
                }
            }
            raw.push(per_chunk);
        }

        let mut token_ids: HashMap<String, u32> = HashMap::with_capacity(df.len());
        let mut token_df: Vec<u32> = Vec::with_capacity(df.len());
        for (tok, d) in &df {
            token_ids.insert(tok.clone(), token_df.len() as u32);
            token_df.push(*d);
        }
        let token_idf: Vec<f64> = token_df
            .iter()
            .map(|d| idf(*d, corpus_len))
            .collect();
        let mut names = Vec::with_capacity(corpus_len);
        let mut full = Vec::with_capacity(corpus_len);
        let mut bytes = 0usize;
        for per_chunk in raw {
            let mut out = Vec::with_capacity(per_chunk.len());
            let mut full_out = Vec::with_capacity(per_chunk.len());
            for (normalised, toks) in per_chunk {
                let ids: Vec<u32> = toks
                    .iter()
                    .map(|t| token_ids.get(t.as_str()).copied().unwrap_or(0))
                    .collect();
                bytes += ids.len() * 4 + 24 + normalised.len();
                out.push(ids);
                full_out.push(normalised);
            }
            names.push(out);
            full.push(full_out);
        }
        bytes += token_ids
            .keys()
            .map(|k| k.len() + 32)
            .sum::<usize>();
        bytes += name_set.iter().map(|s| s.len() + 32).sum::<usize>();
        DefIndex {
            names,
            full,
            token_ids,
            token_df,
            token_idf,
            name_set,
            bytes,
        }
    }

    fn query_ids(&self, terms: &[String], use_stem: bool) -> HashSet<u32> {
        terms
            .iter()
            .filter_map(|t| {
                let key = if use_stem { stem(t) } else { t.as_str() };
                self.token_ids.get(key).copied()
            })
            .collect()
    }
}

fn idf(df: u32, corpus_len: usize) -> f64 {
    (1.0 + (corpus_len as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln()
}

/// IDF/coverage-weighted name match for one chunk.
///
/// `q` holds query tokens, stemmed when the config asks for it. Returns the
/// best name score in [0, 1]. `defn_spec` requires at least one matched token
/// to be rare (df <= spec * corpus_len); `defn_rarity` multiplies by the
/// normalised IDF of the rarest matched token.
fn name_score(
    names: &[Vec<u32>],
    qids: &HashSet<u32>,
    index: &DefIndex,
    corpus_len: usize,
    cfg: &Config,
) -> f64 {
    let idf_hi = idf(1, corpus_len);
    let mut best: f64 = 0.0;
    for name in names {
        let mut matched = 0usize;
        let mut w_match = 0.0;
        let mut w_all = 0.0;
        let mut max_idf = 0.0f64;
        let mut has_specific = false;
        for &tid in name {
            let w = index.token_idf[tid as usize];
            w_all += w;
            if qids.contains(&tid) {
                matched += 1;
                w_match += w;
                if w > max_idf {
                    max_idf = w;
                }
                if cfg.defn_spec > 0.0
                    && (index.token_df[tid as usize] as f64) <= cfg.defn_spec * corpus_len as f64
                {
                    has_specific = true;
                }
            }
        }
        if matched == 0 {
            continue;
        }
        if cfg.defn_spec > 0.0 && !has_specific {
            continue;
        }
        let coverage = matched as f64 / name.len().max(1) as f64;
        let mut s = if cfg.defn_idf {
            if w_all > 0.0 { w_match / w_all } else { 0.0 }
        } else {
            coverage
        };
        if cfg.defn_rarity {
            s *= (max_idf / idf_hi.max(f64::EPSILON)).min(1.0);
        }
        if s > best {
            best = s;
        }
    }
    best
}

fn path_penalty(path: &str, pen: f64) -> f64 {
    if pen == 0.0 {
        return 1.0;
    }
    let p = path.replace('\\', "/");
    if TEST_FILE.is_match(&p) || TEST_DIR.is_match(&p) || LEGACY_DIR.is_match(&p) {
        1.0 - pen
    } else {
        1.0
    }
}

fn query_word_count(query: &str) -> usize {
    WORD.find_iter(query).count()
}

fn query_ident_like(query: &str, def_index: &DefIndex) -> bool {
    // A code-shaped token: snake_case, camelCase, or a letter+digit mix
    // (pure numbers do not count).
    for raw in WORD.find_iter(query).map(|m| m.as_str()) {
        if !raw.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        if raw.contains('_') || CAMEL.is_match(raw) {
            return true;
        }
        if raw.chars().any(|c| c.is_numeric()) {
            return true;
        }
    }
    // A query word that is itself a defined name in this corpus.
    WORD.find_iter(query)
        .map(|m| m.as_str().to_lowercase().replace('_', ""))
        .any(|w| def_index.name_set.contains(&w))
}

// Semble's whole-query symbol test. A symbol query is one token, or a
// namespace-qualified path, a leading underscore, or a token with an
// uppercase letter or an underscore.
static SYMBOL_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+|_[A-Za-z0-9_]*|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*|[A-Z][A-Za-z0-9]*)$",
    )
    .unwrap()
});

fn is_symbol_query(query: &str) -> bool {
    SYMBOL_QUERY.is_match(query.trim())
}

fn symbol_name(query: &str) -> String {
    let q = query.trim();
    for sep in ["::", "\\", "->", "."] {
        if let Some((_, tail)) = q.rsplit_once(sep) {
            return tail.to_string();
        }
    }
    q.to_string()
}

fn normalise_name(name: &str) -> String {
    name.to_lowercase().replace('_', "")
}

/// Semble's file-stem match: exact, snake-normalised, or plural-tolerant.
fn file_stem_matches(path: &str, symbol: &str) -> bool {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let stem_norm = stem.replace('_', "");
    let name = symbol.to_lowercase();
    let name_norm = name.replace('_', "");
    let trim_s = |s: &str| s.strip_suffix('s').unwrap_or(s).to_string();
    stem == name
        || stem_norm == name_norm
        || trim_s(&stem) == name
        || trim_s(&stem_norm) == name_norm
}

/// Semble's `_count_keyword_matches`: exact, or a 3-character prefix overlap.
fn keyword_matches(keywords: &HashSet<String>, parts: &HashSet<String>) -> usize {
    let mut n = 0usize;
    for keyword in keywords {
        if parts.contains(keyword) {
            n += 1;
            continue;
        }
        for part in parts {
            let (shorter, longer) = if keyword.len() <= part.len() {
                (keyword, part)
            } else {
                (part, keyword)
            };
            if shorter.chars().count() >= 3 && longer.starts_with(shorter) {
                n += 1;
                break;
            }
        }
    }
    n
}

/// Semble's NL file-stem/path boost for one path, in [0, 1]. One matched word
/// among several query words gets half the ratio, matching `rank_signals`.
fn stem_path_score(path: &str, keywords: &HashSet<String>, cache: &mut HashMap<String, f64>) -> f64 {
    if keywords.is_empty() {
        return 0.0;
    }
    if let Some(v) = cache.get(path) {
        return *v;
    }
    let p = Path::new(path);
    let mut parts: HashSet<String> = words(p.file_stem().and_then(|s| s.to_str()).unwrap_or(""))
        .into_iter()
        .filter(|w| w.chars().count() >= 3)
        .collect();
    if let Some(parent) = p.parent().and_then(|q| q.file_name()).and_then(|s| s.to_str()) {
        if parent != "." && parent != "/" {
            parts.extend(
                words(parent)
                    .into_iter()
                    .filter(|w| w.chars().count() >= 3),
            );
        }
    }
    let n = keyword_matches(keywords, &parts);
    let score = if n > 0 {
        let mut ratio = n as f64 / keywords.len() as f64;
        if n == 1 && keywords.len() > 1 {
            ratio *= 0.5;
        }
        if ratio >= 0.10 { ratio } else { 0.0 }
    } else {
        0.0
    };
    cache.insert(path.to_string(), score);
    score
}

/// True when two 1-based inclusive line spans share at least half of the
/// shorter span. Matches `rank_signals::spans_substantially_overlap` for the
/// semantic ranker.
fn spans_substantially_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    let lo = a.0.max(b.0);
    let hi = a.1.min(b.1);
    if hi < lo {
        return false;
    }
    let overlap = hi - lo + 1;
    let shorter = (a.1 - a.0 + 1).min(b.1 - b.0 + 1);
    overlap.saturating_mul(2) >= shorter
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
    /// Legacy flat definition bonus (broad keyword set, any-token match).
    #[serde(default)]
    def: f64,
    /// Name-match definition boost weight.
    #[serde(default)]
    defn: f64,
    #[serde(default)]
    defn_idf: bool,
    #[serde(default)]
    defn_stem: bool,
    /// "narrow" selects the per-language real-definition-site set.
    #[serde(default)]
    defn_kw: Option<String>,
    /// "none" (default), "length", "ident", "length_or_ident".
    #[serde(default)]
    defn_gate: Option<String>,
    #[serde(default = "default_min_words")]
    defn_min_words: usize,
    /// Test/legacy multiplicative penalty strength in [0,1). 0 = off.
    #[serde(default)]
    pen: f64,
    /// Reciprocal-rank fusion instead of min-max relative fusion.
    #[serde(default)]
    rrf: bool,
    #[serde(default = "default_rrf_k")]
    rrf_k: f64,
    /// "max" (default) or "sum" over names.
    #[serde(default)]
    defn_mode: Option<String>,
    /// Require a matched name token with df <= spec * corpus_len (0 = off).
    #[serde(default)]
    defn_spec: f64,
    /// Multiply by normalised IDF of the rarest matched name token.
    #[serde(default)]
    defn_rarity: bool,
    /// Per-file saturation: decay factor for the 2nd+ chunk from one file.
    #[serde(default)]
    sat: f64,
    /// Number of chunks per file kept at full score before decay starts.
    #[serde(default = "default_sat_from")]
    sat_from: usize,
    /// Optional gentler decay for the 2nd chunk of a file (0 = use `sat`).
    #[serde(default)]
    sat2: f64,
    /// Hard cap of chunks per file in the selected top-N (0 = off).
    #[serde(default)]
    cap: usize,
    /// Position window for saturation and cap (default 10).
    #[serde(default = "default_sel_top")]
    sel_top: usize,
    /// Multi-chunk file coherence boost, as a fraction of the max score.
    #[serde(default)]
    coh: f64,
    /// Natural-language file-stem/path boost, as a fraction of the max score.
    #[serde(default)]
    stem: f64,
    /// Whole-query symbol: definition boost, as a fraction of the max score.
    #[serde(default)]
    sym: f64,
    /// Whole-query symbol: BM25 weight override.
    #[serde(default)]
    sym_w: Option<f64>,
    /// Whole-query symbol: multiplier when the file stem matches the symbol.
    #[serde(default = "default_sym_stem")]
    sym_stem: f64,
    /// Whole-query symbol: additive exact-match boost instead of a hard tier.
    #[serde(default)]
    sym_soft: f64,
}

fn default_sym_stem() -> f64 {
    1.5
}

fn default_sel_top() -> usize {
    10
}

fn default_sat_from() -> usize {
    1
}

fn default_pool() -> usize {
    200
}
fn default_weight() -> f64 {
    0.25
}
fn default_min_words() -> usize {
    20
}
fn default_rrf_k() -> f64 {
    60.0
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
    profile: bool,
    rows: bool,
}

fn parse_args() -> Options {
    let args: Vec<String> = std::env::args().collect();
    let mut root = None;
    let mut queries = None;
    let mut out = None;
    let mut configs = Vec::new();
    let mut top = 100usize;
    let mut content = false;
    let mut profile = false;
    let mut rows = false;
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
            "--profile" => {
                profile = true;
                i += 1;
            }
            "--rows" => {
                rows = true;
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
            defn: 0.0,
            defn_idf: false,
            defn_stem: false,
            defn_kw: None,
            defn_gate: None,
            defn_min_words: 20,
            pen: 0.0,
            rrf: false,
            rrf_k: 60.0,
            defn_mode: None,
            defn_spec: 0.0,
            defn_rarity: false,
            sat: 0.0,
            sat_from: 1,
            sat2: 0.0,
            cap: 0,
            sel_top: 10,
            coh: 0.0,
            stem: 0.0,
            sym: 0.0,
            sym_w: None,
            sym_stem: 1.5,
            sym_soft: 0.0,
        });
    }
    Options {
        root: root.expect("--root required"),
        queries: queries.expect("--queries required"),
        out: out.expect("--out required"),
        configs,
        top,
        content,
        profile,
        rows,
    }
}

fn contains_line(chunk: &wgrep::Chunk, path: &str, line: usize) -> bool {
    &*chunk.path == path && chunk.start <= line && line <= chunk.end
}

fn uses_defn(cfg: &Config) -> bool {
    cfg.defn != 0.0
}

fn main() {
    let opts = parse_args();
    let raw = std::fs::read_to_string(&opts.queries).expect("read queries");
    let queries: Vec<Query> = serde_json::from_str(&raw).expect("parse queries");

    let searcher = Searcher::open(&opts.root, ModelChoice::Code, None).expect("build searcher");
    let chunks = searcher.corpus().chunks();
    let corpus_len = searcher.corpus().len();
    let vectors = searcher.passage_vectors();

    // Definition indexes: built once per corpus, like a product index-time pass.
    let mut def_indexes: HashMap<(bool, bool), DefIndex> = HashMap::new();
    let index_ms;
    {
        let mut needed: HashSet<(bool, bool)> = HashSet::new();
        for c in &opts.configs {
            if uses_defn(c) {
                needed.insert((c.defn_kw.as_deref() != Some("broad"), c.defn_stem));
            }
            if c.sym > 0.0 {
                needed.insert((true, false));
            }
        }
        let t_index = Instant::now();
        for (narrow, stem) in needed {
            let di = DefIndex::build(chunks, stem, narrow);
            eprintln!(
                "def-index narrow={} stem={}: {} chunks, {} with names, {} distinct tokens, {} bytes",
                narrow,
                stem,
                corpus_len,
                di.names.iter().filter(|n| !n.is_empty()).count(),
                di.token_ids.len(),
                di.bytes
            );
            def_indexes.insert((narrow, stem), di);
        }
        index_ms = t_index.elapsed().as_secs_f64() * 1e3;
    }
    if !def_indexes.is_empty() {
        eprintln!("def-index build total: {:.1} ms", index_ms);
    }

    let mut defn_ns: u128 = 0;
    let mut legacy_def_ns: u128 = 0;
    let mut sel_ns: u128 = 0;
    let mut pen_ns: u128 = 0;
    let mut rrf_ns: u128 = 0;
    let mut timed_queries: u64 = 0;
    // per config: union rows, defn>0 rows, defn>=1 rows, queries, firing queries
    let mut stats: HashMap<String, [f64; 5]> = HashMap::new();

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

        let sym_query = is_symbol_query(&q.query);
        let sym_name = if sym_query {
            symbol_name(&q.query)
        } else {
            String::new()
        };
        let sym_norm = normalise_name(&sym_name);
        let stem_keywords: HashSet<String> = words(&q.query)
            .into_iter()
            .filter(|w| w.chars().count() >= 3)
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
            let length_gate_ok = query_word_count(&q.query) >= cfg.defn_min_words;
            let t_legacy = Instant::now();
            let def_scores: Vec<f64> = order
                .iter()
                .map(|&i| {
                    if cfg.def == 0.0 {
                        return 0.0;
                    }
                    if cfg.defn_gate.as_deref() == Some("length") && !length_gate_ok {
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
            legacy_def_ns += t_legacy.elapsed().as_nanos();

            let t_defn = Instant::now();
            let defn_scores: Vec<f64> = if uses_defn(cfg) {
                let key = (cfg.defn_kw.as_deref() != Some("broad"), cfg.defn_stem);
                let idx = def_indexes.get(&key).expect("definition index missing");
                let qids = idx.query_ids(&terms, cfg.defn_stem);
                let gate_ok = match cfg.defn_gate.as_deref().unwrap_or("none") {
                    "none" => true,
                    "length" => length_gate_ok,
                    "ident" => query_ident_like(&q.query, idx),
                    "length_or_ident" => {
                        length_gate_ok || query_ident_like(&q.query, idx)
                    }
                    other => panic!("unknown gate {other}"),
                };
                if !gate_ok {
                    vec![0.0; order.len()]
                } else {
                    order
                        .iter()
                        .map(|&i| name_score(&idx.names[i], &qids, idx, corpus_len, cfg))
                        .collect()
                }
            } else {
                vec![0.0; order.len()]
            };
            defn_ns += t_defn.elapsed().as_nanos();

            {
                let e = stats.entry(cfg.name.clone()).or_default();
                e[0] += order.len() as f64;
                e[1] += defn_scores.iter().filter(|s| **s > 0.0).count() as f64;
                e[2] += defn_scores.iter().filter(|s| **s >= 0.999).count() as f64;
                e[3] += 1.0;
                if defn_scores.iter().any(|s| *s > 0.0) {
                    e[4] += 1.0;
                }
            }

            let t_pen = Instant::now();
            let penalties: Vec<f64> = if cfg.pen == 0.0 {
                vec![1.0; order.len()]
            } else {
                order
                    .iter()
                    .map(|&i| path_penalty(&chunks[i].path, cfg.pen))
                    .collect()
            };
            pen_ns += t_pen.elapsed().as_nanos();

            let t_rrf = Instant::now();
            let rrf_lex: Vec<f64> = if cfg.rrf {
                order
                    .iter()
                    .map(|&i| {
                        let r = lex_rank[i];
                        if r == 0 { 0.0 } else { 1.0 / (cfg.rrf_k + r as f64) }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let rrf_sem: Vec<f64> = if cfg.rrf {
                order
                    .iter()
                    .map(|&i| 1.0 / (cfg.rrf_k + sem_rank[i] as f64))
                    .collect()
            } else {
                Vec::new()
            };
            let rrf_norm = if cfg.rrf {
                (1.0 / (cfg.rrf_k + 1.0)).max(f64::EPSILON)
            } else {
                1.0
            };
            rrf_ns += t_rrf.elapsed().as_nanos();

            let t_sel = Instant::now();
            let w_eff = if sym_query {
                cfg.sym_w.unwrap_or(cfg.w)
            } else {
                cfg.w
            };
            let base_scores: Vec<f64> = (0..order.len())
                .map(|i| {
                    if cfg.rrf {
                        let l = rrf_lex[i] / rrf_norm;
                        let s = rrf_sem[i] / rrf_norm;
                        w_eff * l + (1.0 - w_eff) * s
                    } else {
                        w_eff * bm_rel[i] + (1.0 - w_eff) * sem_rel[i]
                    }
                })
                .collect();
            let max_base = base_scores
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max)
                .max(0.0);

            // Multi-chunk file coherence: add a share of the file's total score
            // to its best chunk.
            let mut coh_scores = vec![0.0; order.len()];
            if cfg.coh > 0.0 {
                let mut file_sum: HashMap<&str, f64> = HashMap::new();
                let mut best_chunk: HashMap<&str, usize> = HashMap::new();
                for (j, &i) in order.iter().enumerate() {
                    let path = &*chunks[i].path;
                    *file_sum.entry(path).or_insert(0.0) += base_scores[j].max(0.0);
                    let slot = best_chunk.entry(path).or_insert(j);
                    if base_scores[j] > base_scores[*slot] {
                        *slot = j;
                    }
                }
                let max_sum = file_sum
                    .values()
                    .copied()
                    .fold(0.0f64, f64::max)
                    .max(f64::EPSILON);
                for (path, &j) in &best_chunk {
                    coh_scores[j] = cfg.coh * max_base * file_sum[path] / max_sum;
                }
            }

            // Whole-query symbol: boost chunks that define the symbol, and
            // more when the file stem matches it.
            let sym_scores: Vec<f64> = if cfg.sym > 0.0 && sym_query {
                let idx = def_indexes.get(&(true, false)).expect("symbol index missing");
                order
                    .iter()
                    .map(|&i| {
                        if idx.full[i].iter().any(|n| n == &sym_norm) {
                            if file_stem_matches(&chunks[i].path, &sym_name) {
                                cfg.sym_stem
                            } else {
                                1.0
                            }
                        } else {
                            0.0
                        }
                    })
                    .collect()
            } else {
                vec![0.0; order.len()]
            };

            // Natural-language file-stem/path boost.
            let mut stem_cache: HashMap<String, f64> = HashMap::new();
            let stem_scores: Vec<f64> = if cfg.stem > 0.0 && !sym_query {
                order
                    .iter()
                    .map(|&i| stem_path_score(&chunks[i].path, &stem_keywords, &mut stem_cache))
                    .collect()
            } else {
                vec![0.0; order.len()]
            };

            let use_hard_tier = !(cfg.sym_soft > 0.0 && sym_query);
            let exact_bonus: Vec<f64> = if !use_hard_tier {
                order
                    .iter()
                    .map(|&i| if exact.contains(&i) { cfg.sym_soft * max_base } else { 0.0 })
                    .collect()
            } else {
                vec![0.0; order.len()]
            };

            let score = |i: usize| {
                (base_scores[i]
                    + coh_scores[i]
                    + cfg.path * path_scores[i]
                    + cfg.def * def_scores[i]
                    + cfg.defn * defn_scores[i]
                    + cfg.stem * stem_scores[i]
                    + cfg.sym * sym_scores[i]
                    + exact_bonus[i])
                    * penalties[i]
            };

            // The plain baseline keeps the classic containment rule. A config
            // that adds a semantic reranker knob follows the 0.3 semantic rule.
            let new_rank = cfg.path > 0.0
                || cfg.def > 0.0
                || cfg.defn != 0.0
                || cfg.pen > 0.0
                || cfg.rrf
                || cfg.sat > 0.0
                || cfg.cap > 0
                || cfg.coh > 0.0
                || cfg.stem > 0.0
                || cfg.sym > 0.0
                || cfg.sym_soft > 0.0;
            let mut results: Vec<usize> = Vec::new();
            let mut fused: Vec<usize> = Vec::new();
            if cfg.sat == 0.0 && cfg.cap == 0 {
                fused = (0..order.len()).collect();
                fused.sort_by(|&a, &b| {
                    let ea = use_hard_tier && exact.contains(&order[a]);
                    let eb = use_hard_tier && exact.contains(&order[b]);
                    eb.cmp(&ea)
                        .then_with(|| score(b).total_cmp(&score(a)))
                        .then_with(|| bm_scores[b].total_cmp(&bm_scores[a]))
                });
                for &fi in &fused {
                    let i = order[fi];
                    let chunk = &chunks[i];
                    if results.iter().any(|&j| {
                        chunks[j].path == chunk.path
                            && if new_rank {
                                spans_substantially_overlap(
                                    (chunks[j].start, chunks[j].end),
                                    (chunk.start, chunk.end),
                                )
                            } else {
                                chunks[j].start <= chunk.start && chunk.end <= chunks[j].end
                            }
                    }) {
                        continue;
                    }
                    results.push(i);
                    if results.len() == opts.top {
                        break;
                    }
                }
            } else {
                let mut taken = vec![false; order.len()];
                let mut file_count: HashMap<&str, usize> = HashMap::new();
                let mut by_path: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
                while results.len() < opts.top {
                    if results.len() >= cfg.sel_top {
                        // After the diversification window the effective score is
                        // the raw score, so a sorted tail is equivalent to more
                        // greedy scans and much cheaper for large --top.
                        let mut rest: Vec<usize> =
                            (0..order.len()).filter(|&j| !taken[j]).collect();
                        rest.sort_by(|&a, &b| {
                            let ea = use_hard_tier && exact.contains(&order[a]);
                            let eb = use_hard_tier && exact.contains(&order[b]);
                            eb.cmp(&ea)
                                .then_with(|| score(b).total_cmp(&score(a)))
                                .then_with(|| bm_scores[b].total_cmp(&bm_scores[a]))
                        });
                        for j in rest {
                            let i = order[j];
                            let path = &*chunks[i].path;
                            if let Some(spans) = by_path.get(path) {
                                let span = (chunks[i].start, chunks[i].end);
                                if spans.iter().any(|&(s, e)| {
                                    if new_rank {
                                        spans_substantially_overlap((s, e), span)
                                    } else {
                                        s <= span.0 && span.1 <= e
                                    }
                                }) {
                                    continue;
                                }
                            }
                            by_path
                                .entry(path)
                                .or_default()
                                .push((chunks[i].start, chunks[i].end));
                            results.push(i);
                            fused.push(j);
                            if results.len() == opts.top {
                                break;
                            }
                        }
                        break;
                    }
                    let mut best: Option<usize> = None;
                    let mut best_eff = 0.0f64;
                    let mut best_exact = false;
                    let mut best_bm = 0.0f64;
                    for j in 0..order.len() {
                        if taken[j] {
                            continue;
                        }
                        let i = order[j];
                        let path = &*chunks[i].path;
                        if let Some(spans) = by_path.get(path) {
                            let span = (chunks[i].start, chunks[i].end);
                            if spans.iter().any(|&(s, e)| {
                                if new_rank {
                                    spans_substantially_overlap((s, e), span)
                                } else {
                                    s <= span.0 && span.1 <= e
                                }
                            }) {
                                continue;
                            }
                        }
                        let c = file_count.get(path).copied().unwrap_or(0);
                        let in_window = results.len() < cfg.sel_top;
                        if in_window && cfg.cap > 0 && c >= cfg.cap {
                            continue;
                        }
                        let eff = score(j)
                            * if in_window && cfg.sat > 0.0 {
                                if cfg.sat2 > 0.0 {
                                    if c == 0 {
                                        1.0
                                    } else if c == 1 {
                                        cfg.sat2
                                    } else {
                                        cfg.sat2 * cfg.sat.powi((c - 1) as i32)
                                    }
                                } else {
                                    let exp = (c + 1).saturating_sub(cfg.sat_from);
                                    cfg.sat.powi(exp as i32)
                                }
                            } else {
                                1.0
                            };
                        let ex = use_hard_tier && exact.contains(&i);
                        let better = match best {
                            None => true,
                            Some(bj) => {
                                ex.cmp(&best_exact)
                                    .then_with(|| eff.total_cmp(&best_eff))
                                    .then_with(|| bm_scores[j].total_cmp(&best_bm))
                                    == std::cmp::Ordering::Greater
                            }
                        };
                        if better {
                            best = Some(j);
                            best_eff = eff;
                            best_exact = ex;
                            best_bm = bm_scores[j];
                        }
                    }
                    let Some(j) = best else { break };
                    taken[j] = true;
                    let i = order[j];
                    let path = &*chunks[i].path;
                    *file_count.entry(path).or_insert(0) += 1;
                    by_path
                        .entry(path)
                        .or_default()
                        .push((chunks[i].start, chunks[i].end));
                    results.push(i);
                    fused.push(j);
                }
            }
            let fused_rank: Vec<usize> = {
                let mut rank = vec![0usize; corpus_len];
                for (pos, &i) in fused.iter().enumerate() {
                    rank[order[i]] = pos + 1;
                }
                rank
            };
            sel_ns += t_sel.elapsed().as_nanos();
            let mut unique_paths: Vec<&str> = Vec::new();
            for &i in &results {
                if !unique_paths.iter().any(|p| *p == &*chunks[i].path) {
                    unique_paths.push(&*chunks[i].path);
                }
            }
            let mut targets = Vec::new();
            for (target, chunk_idx) in q.targets.iter().zip(&target_chunks) {
                let file_rank = unique_paths
                    .iter()
                    .position(|p| *p == target.path.as_str())
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
                                    "defn_score": pos.map(|p| defn_scores[p]),
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
            let rows: Vec<Value> = if opts.rows || cfg.name == "base" || cfg.name == "rows" {
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
                            "defn_score": defn_scores[fi],
                            "penalty": penalties[fi],
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
        timed_queries += 1;
        out.push(json!({
            "id": q.id,
            "query": q.query,
            "terms": terms,
            "corpus_len": corpus_len,
            "configs": config_results,
        }));
        eprintln!("done {} ({} chunks)", q.id, corpus_len);
    }

    if opts.profile && timed_queries > 0 {
        let n = timed_queries as f64;
        eprintln!(
            "profile: queries={} defn={:.1} us/q legacy_def={:.1} us/q sel={:.1} us/q pen={:.1} us/q rrf={:.1} us/q index={:.1} ms",
            timed_queries,
            defn_ns as f64 / 1e3 / n,
            legacy_def_ns as f64 / 1e3 / n,
            sel_ns as f64 / 1e3 / n,
            pen_ns as f64 / 1e3 / n,
            rrf_ns as f64 / 1e3 / n,
            index_ms,
        );
        let mut names: Vec<&String> = stats.keys().collect();
        names.sort();
        for name in names {
            let s = stats[name];
            eprintln!(
                "defn-stats {}: union_rows={} fire={:.3} full={:.3} queries_firing={:.3}",
                name,
                s[0],
                s[1] / s[0].max(1.0),
                s[2] / s[0].max(1.0),
                s[4] / s[3].max(1.0),
            );
        }
    }

    std::fs::write(&opts.out, serde_json::to_vec(&out).unwrap()).expect("write output");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_matches_production_stem_damping_and_overlap() {
        let many: HashSet<String> = ["parser", "zzz", "yyy", "xxx"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut cache = HashMap::new();
        assert!((stem_path_score("src/parser.rs", &many, &mut cache) - 0.125).abs() < 1e-12);

        let full: HashSet<String> = ["parser", "read"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut cache = HashMap::new();
        assert_eq!(stem_path_score("src/parser/reader.rs", &full, &mut cache), 1.0);

        assert!(spans_substantially_overlap((1, 60), (20, 80)));
        assert!(spans_substantially_overlap((1, 10), (5, 10)));
        assert!(!spans_substantially_overlap((1, 60), (50, 120)));
        assert!(!spans_substantially_overlap((1, 10), (11, 20)));
    }
}
