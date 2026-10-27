//! Ranking signals shared by the `no sat` semantic default and the analysis
//! tool `bench/WG_QUALITY_TOOL`.
//!
//! The functions here are a port of the reference implementation:
//! whole-query symbol detection, definition-name normalisation, the narrow
//! per-language definition sites, the natural-language file-stem/path boost,
//! and the multi-chunk file coherence boost. The stem/path boost damps one
//! matched word among several query words, so one common word cannot act as a
//! large file-name bonus. The classic ranker never calls these functions, so
//! its output and scores stay byte-identical.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").unwrap());
static STOP: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    "a an and are as at be been but by can do does for from had has have how i if in into is it its me not of on or our should so that the their them there these they this to use using was we what when where which who will with would you your function description purpose input output"
        .split_whitespace()
        .collect()
});

// Narrow definition sites. No `let`, `var`, `const` except top-level JS/TS
// bindings, no `impl`, no `namespace` bodies. Copied verbatim from the
// reference tool's `no sat` configuration.
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

/// Semble's whole-query symbol test. A symbol query is one token, a
/// namespace-qualified path, a leading underscore, or a token with an
/// uppercase letter or an underscore.
static SYMBOL_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+|_[A-Za-z0-9_]*|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*|[A-Z][A-Za-z0-9]*)$",
    )
    .unwrap()
});

/// Split camelCase and lower-case a text, then keep the words that are longer
/// than one character and are not stopwords. This is the same `words` helper
/// the ranker uses for BM25, so path and name scoring share one tokenisation.
pub(crate) fn words(text: &str) -> Vec<String> {
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

/// The narrow definition-name set of one chunk, normalised. Names whose
/// tokenised form is empty are dropped, exactly as the reference index does.
pub(crate) fn definition_names(path: &str, content: &str) -> Vec<String> {
    let Some(pats) = keywords_for(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for pat in pats {
        for cap in pat.captures_iter(content) {
            let raw = &cap[1];
            let normalised = normalise_name(raw);
            if words(raw).is_empty() || !seen.insert(normalised.clone()) {
                continue;
            }
            names.push(normalised);
        }
    }
    names
}

pub(crate) fn is_symbol_query(query: &str) -> bool {
    SYMBOL_QUERY.is_match(query.trim())
}

/// The last namespace segment of a whole-query symbol.
pub(crate) fn symbol_name(query: &str) -> String {
    let q = query.trim();
    for sep in ["::", "\\", "->", "."] {
        if let Some((_, tail)) = q.rsplit_once(sep) {
            return tail.to_string();
        }
    }
    q.to_string()
}

pub(crate) fn normalise_name(name: &str) -> String {
    name.to_lowercase().replace('_', "")
}

/// Semble's file-stem match: exact, snake-normalised, or plural-tolerant.
pub(crate) fn file_stem_matches(path: &str, symbol: &str) -> bool {
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
/// among several query words gets half the ratio.
pub(crate) fn stem_path_score(
    path: &str,
    keywords: &HashSet<String>,
    cache: &mut HashMap<String, f64>,
) -> f64 {
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
    if let Some(parent) = p
        .parent()
        .and_then(|q| q.file_name())
        .and_then(|s| s.to_str())
    {
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
        // One matched word among several is a weak signal. Common words such
        // as "surface" then act as a file-name bonus that is too large. Damp
        // only that case; a single-keyword query and a multi-word match keep
        // the full ratio.
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
/// shorter span. The ranker uses this to drop substantial same-file overlap
/// before it fills the shortlist, and to keep separate regions.
pub(crate) fn spans_substantially_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    let lo = a.0.max(b.0);
    let hi = a.1.min(b.1);
    if hi < lo {
        return false;
    }
    let overlap = hi - lo + 1;
    let shorter = (a.1 - a.0 + 1).min(b.1 - b.0 + 1);
    overlap.saturating_mul(2) >= shorter
}

/// Query words of 3+ characters that are not stopwords, for the stem boost.
pub(crate) fn stem_keywords(query: &str) -> HashSet<String> {
    words(query)
        .into_iter()
        .filter(|w| w.chars().count() >= 3)
        .collect()
}

/// Multi-chunk file coherence: add a share of a file's total score to its
/// best-scoring chunk. `paths` and `base` are parallel, in union order.
pub(crate) fn coherence_boosts(paths: &[&str], base: &[f64], coh: f64, max_base: f64) -> Vec<f64> {
    let mut boosts = vec![0.0; base.len()];
    if coh == 0.0 {
        return boosts;
    }
    let mut file_sum: HashMap<&str, f64> = HashMap::new();
    let mut best_chunk: HashMap<&str, usize> = HashMap::new();
    for (j, path) in paths.iter().enumerate() {
        *file_sum.entry(*path).or_insert(0.0) += base[j].max(0.0);
        let slot = best_chunk.entry(*path).or_insert(j);
        if base[j] > base[*slot] {
            *slot = j;
        }
    }
    let max_sum = file_sum
        .values()
        .copied()
        .fold(0.0f64, f64::max)
        .max(f64::EPSILON);
    for (path, &j) in &best_chunk {
        boosts[j] = coh * max_base * file_sum[path] / max_sum;
    }
    boosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_query_regex_matches_whole_query_symbols_only() {
        for q in [
            "foo_bar",
            "_private",
            "FooBar",
            "camelCase",
            "std::vector",
            "a::b::c",
            "ns\\name",
            "foo->bar",
            "mod.func",
            "x86_64",
        ] {
            assert!(is_symbol_query(q), "{q} must be a symbol query");
        }
        for q in [
            "foo",
            "find the parser",
            "how does it work",
            "absl::flat_hash_map and flat_hash_set",
            "foo bar",
        ] {
            assert!(!is_symbol_query(q), "{q} must not be a symbol query");
        }
        assert!(is_symbol_query("  foo_bar \n"));
    }

    #[test]
    fn symbol_name_takes_the_last_segment() {
        assert_eq!(symbol_name("std::vector"), "vector");
        assert_eq!(symbol_name("a::b::c"), "c");
        assert_eq!(symbol_name("ns\\name"), "name");
        assert_eq!(symbol_name("foo->bar"), "bar");
        assert_eq!(symbol_name("mod.func"), "func");
        assert_eq!(symbol_name("plain_name"), "plain_name");
        assert_eq!(symbol_name("  Foo::Bar  "), "Bar");
    }

    #[test]
    fn name_normalisation_lowercases_and_drops_underscores() {
        assert_eq!(normalise_name("Foo_Bar"), "foobar");
        assert_eq!(normalise_name("__init__"), "init");
        assert_eq!(normalise_name("HTTPServer"), "httpserver");
    }

    #[test]
    fn file_stem_matching_is_exact_normalised_or_plural_tolerant() {
        for (path, symbol) in [
            ("src/parser.rs", "parser"),
            ("src/my_parser.rs", "myparser"),
            ("src/my_parser.rs", "MyParser"),
            ("src/things.rs", "thing"),
            ("src/Thing.rs", "thing"),
        ] {
            assert!(file_stem_matches(path, symbol), "{path} vs {symbol}");
        }
        assert!(!file_stem_matches("src/parser.rs", "parse"));
        assert!(!file_stem_matches("src/parser.rs", "lexer"));
    }

    #[test]
    fn stem_path_score_matches_exact_and_prefix_words() {
        let kw: HashSet<String> = ["parser", "read"].into_iter().map(str::to_owned).collect();
        let mut cache = HashMap::new();
        assert_eq!(stem_path_score("src/parser/reader.rs", &kw, &mut cache), 1.0);
        let kw2: HashSet<String> = ["parse", "reader"].into_iter().map(str::to_owned).collect();
        let mut cache = HashMap::new();
        assert_eq!(stem_path_score("src/parser/reader.rs", &kw2, &mut cache), 1.0);
        let kw3: HashSet<String> = ["zzz"].into_iter().map(str::to_owned).collect();
        let mut cache = HashMap::new();
        assert_eq!(stem_path_score("src/parser/reader.rs", &kw3, &mut cache), 0.0);
    }

    #[test]
    fn stem_path_damps_one_matched_word_among_many() {
        let kw: HashSet<String> = ["parser", "zzz", "yyy", "xxx"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut cache = HashMap::new();
        assert!((stem_path_score("src/parser.rs", &kw, &mut cache) - 0.125).abs() < 1e-12);

        let full: HashSet<String> = ["parser", "read"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut cache = HashMap::new();
        assert_eq!(stem_path_score("src/parser/reader.rs", &full, &mut cache), 1.0);

        let single: HashSet<String> = ["parser"].into_iter().map(str::to_owned).collect();
        let mut cache = HashMap::new();
        assert_eq!(stem_path_score("src/parser.rs", &single, &mut cache), 1.0);
    }

    #[test]
    fn substantial_overlap_uses_half_the_shorter_span() {
        assert!(spans_substantially_overlap((1, 60), (20, 80)));
        assert!(spans_substantially_overlap((1, 10), (5, 10)));
        assert!(spans_substantially_overlap((1, 1), (1, 5)));
        assert!(!spans_substantially_overlap((1, 60), (50, 120)));
        assert!(!spans_substantially_overlap((1, 10), (11, 20)));
    }

    #[test]
    fn coherence_adds_to_each_files_best_chunk() {
        let paths = ["a.rs", "a.rs", "b.rs"];
        let base = [0.4, 0.8, -0.2];
        let boosts = coherence_boosts(&paths, &base, 0.05, 0.8);
        // a.rs sums 1.2, b.rs sums 0.0, max sum 1.2.
        assert!((boosts[1] - 0.05 * 0.8).abs() < 1e-12);
        assert_eq!(boosts[0], 0.0);
        assert_eq!(boosts[2], 0.0);
        let boosts = coherence_boosts(&paths, &base, 0.05, 0.8);
        assert!(!boosts.iter().any(|b| b.is_nan()));
    }

    #[test]
    fn narrow_definitions_use_language_sites() {
        let rust = definition_names(
            "src/lib.rs",
            "pub fn build_index() {}\nstruct DefIndex;\nlet nope = 1;\n",
        );
        assert_eq!(rust, vec!["buildindex", "defindex"]);
        let python = definition_names("a.py", "def build_index():\n    pass\nclass Thing:\n    pass\n");
        assert_eq!(python, vec!["buildindex", "thing"]);
        let unknown = definition_names("a.txt", "fn build_index() {}\n");
        assert!(unknown.is_empty());
    }
}
