//! Bounded Rust function-aware chunking prototype (opt-in, never the default).
//!
//! # Scope and ownership
//! This module is owned by the rust-chunks prototype agent. It is the ONLY
//! `src/` file that agent may touch. Integration (Cargo deps, `mod`
//! wiring, CLI flag, daemon slots) belongs to the core owner, who approved
//! `tree-sitter = "0.26"` + `tree-sitter-rust = "0.24"` (caret, lockfile
//! records exact; default features only, no wasm; static C compile) as
//! optional dependencies under the existing `semantic` feature.
//!
//! # Fixed configuration (documented BEFORE any scoring, never tuned)
//! - Base window: `super::chunks::TARGET_CHARS` (2048 chars) is REUSED, not
//!   redefined. Every window emitted here (gap windows, over-cap windows,
//!   full-file fallback) follows the same whole-line `>= TARGET_CHARS`
//!   rule as `chunk_text`, so the window arm of the A/B is identical code.
//! - Function cap: `RUST_FN_CAP_BYTES = 8192` bytes (4x the char window).
//!   Rationale: a fixed 8 KiB ceiling bounds per-passage embedding dilution
//!   and memory while allowing modest functions to remain atomic. It is an
//!   engineering limit, not selected from any evaluation fixture. Functions
//!   beyond the cap are NOT truncated and NOT emitted unbounded: they fall
//!   back to bounded whole-line windows over their own line range. A single
//!   source line can exceed the target (including a line up to the existing
//!   16 MiB file limit); whole-line/source-verifiable ranges take precedence
//!   over a strict byte cap for such a window.
//! - No query-dependent sizing, no weight changes, no other languages.
//!
//! # Algorithm (query-independent, reference prototype)
//! 1. Parse with the tree-sitter Rust grammar (real parser, never brace
//!    regex: raw strings, macros, and nested blocks cannot confuse it).
//!    One parser per call, single-threaded, no statics, no rayon.
//! 2. Collect outermost named `function_item` nodes only, by direct node
//!    traversal (no `.scm` query files). Free functions AND `impl` methods
//!    are both `function_item`; closures are a different node kind and are
//!    never collected. Nested functions are skipped (the outer span already
//!    contains them), so no nested/duplicated passages.
//! 3. Extend each span backwards over contiguous `attribute_item` /
//!    `line_comment` / `block_comment` siblings (whitespace-only gap), so
//!    `///` docs and `#[...]` attributes stay with their function where
//!    the source is contiguous. Best-effort: anything not absorbed stays
//!    covered as gap windows below.
//! 4. Snap spans to whole lines (start back to line start, end forward to
//!    line end including `\n`). Content is always an exact source slice:
//!    byte-exact UTF-8, CRLF retained, lone `\r` never a break.
//! 5. Cover the gaps: every top-level region NOT inside a function
//!    (`use`/`mod`, constants, types, trait signatures without bodies,
//!    `impl` headers, blank-code separators) is windowed with the same
//!    `TARGET_CHARS` rule and absolute line numbers — meaningful code is
//!    never dropped. Gaps with no alphanumeric character (pure
//!    whitespace/punctuation such as a lone `}`) are skipped, not emitted
//!    as junk passages.
//! 6. Fall back to `super::chunks::chunk_text` (byte-identical baseline)
//!    when: the file is empty, the parse fails outright, or no named
//!    function exists (with or without a parse error). A partial parse
//!    error WITH functions still emits function mode — tree-sitter is
//!    error-tolerant by design.
//! 7. Emitted passages are sorted by `(start, end)` for determinism.
//!
//! # Coverage invariants (checked by tests, relied on by eval)
//! - Every chunk `content` is a contiguous slice of the input `text`
//!   (`text.contains(content)`); nothing is synthesized or reordered.
//! - `lines[start-1..end].concat() == content` under `split_inclusive('\n')`
//!   on the live source (same verifiability contract as `chunks.rs`).
//! - Chunks are sorted and non-overlapping.
//! - Every source line containing an alphanumeric character is covered by
//!   some chunk (no dropped meaningful code).
//!
//! # Slots (core-side, recorded here so eval can rely on it)
//! The chunk mode MUST be part of the daemon/index slot key (mode-suffixed
//! slots, mode in daemon status), so a corpus indexed under `window` is
//! never served as `rust-fn` and vice versa. Eval additionally isolates
//! `--work` roots per mode plus a `--run-label`.

use super::chunks::Chunk;

/// Maximum byte length of one atomic function passage. Fixed BEFORE
/// scoring (see module docs); deliberately a byte (not char) bound so the
/// embedding/memory budget is independent of multibyte content.
pub(crate) const RUST_FN_CAP_BYTES: usize = 8192;

/// Outcome counters for one `chunk_rust` call, for daemon stats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RustChunkStats {
    /// Outermost named `function_item`s found (free fns + methods).
    pub fn_count: usize,
    /// Window chunks emitted (gap windows + over-cap windows, or the full
    /// fallback count when the whole file fell back to line windows).
    pub fallback_windows: usize,
    /// Why the whole file fell back to line windows, if it did:
    /// `"empty"`, `"parse-failed"`, `"no-functions"`, or
    /// `"parse-error-no-functions"`. `None` means function mode engaged
    /// (even if the tree had a partial, error-tolerant parse error).
    pub fallback_reason: Option<&'static str>,
}

/// Chunk one Rust file's text into function-atomic passages with gap
/// coverage. See module docs for the contract. Never panics on slicing:
/// all byte ranges are line-aligned (hence char boundaries).
pub(crate) fn chunk_rust(rel: &str, text: &str) -> anyhow::Result<Vec<Chunk>> {
    Ok(chunk_rust_with_stats(rel, text)?.0)
}

/// `chunk_rust` plus daemon stats. `fn_count` is outermost named functions
/// found (including over-cap ones, which are windowed instead of atomic);
/// `fallback_windows` counts every window chunk emitted.
pub(crate) fn chunk_rust_with_stats(
    rel: &str,
    text: &str,
) -> anyhow::Result<(Vec<Chunk>, RustChunkStats)> {
    if text.is_empty() {
        let chunks = super::chunks::chunk_text(rel, text);
        let n = chunks.len();
        return Ok((
            chunks,
            RustChunkStats { fn_count: 0, fallback_windows: n, fallback_reason: Some("empty") },
        ));
    }
    let mut parser = tree_sitter::Parser::new();
    // Verified API (docs.rs tree-sitter-rust 0.24.2): the grammar exports a
    // `LANGUAGE: LanguageFn` constant converted with `.into()`.
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    parser
        .set_language(&language)
        .map_err(|e| anyhow::anyhow!("load Rust grammar: {e:?}"))?;
    let Some(tree) = parser.parse(text, None) else {
        let chunks = super::chunks::chunk_text(rel, text);
        let n = chunks.len();
        return Ok((
            chunks,
            RustChunkStats {
                fn_count: 0,
                fallback_windows: n,
                fallback_reason: Some("parse-failed"),
            },
        ));
    };
    let root = tree.root_node();
    let had_error = root.has_error();

    // Phase 1: outermost `function_item` spans, with doc/attribute extension.
    // Iterative stack (no recursion depth risk on generated files). Each
    // entry expands its own named children, so preceding siblings are in
    // hand for the contiguity walk — no parent() lookup needed.
    let mut fns: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<(tree_sitter::Node<'_>, bool)> = vec![(root, false)];
    while let Some((node, in_fn)) = stack.pop() {
        // NOTE: tree-sitter counts/indices are u32 (C API): cast at the boundary.
        let count = node.named_child_count() as usize;
        if count == 0 {
            continue;
        }
        let mut kids: Vec<tree_sitter::Node<'_>> = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(c) = node.named_child(i as u32) {
                kids.push(c);
            }
        }
        for (idx, kid) in kids.iter().enumerate() {
            let is_fn = kid.kind() == "function_item";
            if is_fn && !in_fn {
                let mut s = kid.start_byte();
                // Absorb contiguous docs/comments/attributes above the fn.
                let mut j = idx;
                while j > 0 {
                    j -= 1;
                    let sib = kids[j];
                    if !matches!(
                        sib.kind(),
                        "attribute_item" | "line_comment" | "block_comment"
                    ) {
                        break;
                    }
                    let gap = &text[sib.end_byte()..s];
                    if !gap.chars().all(|c| c.is_whitespace()) {
                        break;
                    }
                    s = sib.start_byte();
                }
                fns.push((s, kid.end_byte()));
            }
            stack.push((*kid, in_fn || is_fn));
        }
    }
    // Determinism: sorted by (start, end) regardless of traversal order.
    fns.sort();

    if fns.is_empty() {
        let chunks = super::chunks::chunk_text(rel, text);
        let n = chunks.len();
        return Ok((
            chunks,
            RustChunkStats {
                fn_count: 0,
                fallback_windows: n,
                fallback_reason: Some(if had_error {
                    "parse-error-no-functions"
                } else {
                    "no-functions"
                }),
            },
        ));
    }

    // Line table on '\n' only (CRLF retained, lone '\r' never a break).
    // Exactly one entry per split_inclusive('\n') line: a trailing '\n'
    // does NOT open a phantom line.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' && i + 1 < text.len() {
            line_starts.push(i + 1);
        }
    }
    // 1-based line number of the line containing `byte`.
    let line_of = |byte: usize| -> usize { line_starts.partition_point(|&s| s <= byte) };
    // Exclusive byte end of the line containing `byte` (includes its '\n').
    let snap_end = |byte: usize| -> usize {
        match line_starts.binary_search(&(byte + 1)) {
            Ok(i) => line_starts[i],
            Err(i) => line_starts.get(i).copied().unwrap_or(text.len()),
        }
    };

    // Phase 2: snap spans to whole lines.
    let fn_total = fns.len();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(fns.len());
    for (s, e) in fns {
        debug_assert!(s <= e && e <= text.len());
        if s >= e || e > text.len() {
            continue;
        }
        let idx = line_of(s) - 1;
        let ss = line_starts[idx];
        let se = snap_end(e.saturating_sub(1).max(ss));
        if ss < se {
            spans.push((ss, se));
        }
    }
    spans.sort();

    // Phase 3: emit atomic-or-windowed function passages plus gap windows.
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut stats = RustChunkStats { fn_count: fn_total, ..Default::default() };
    let mut cursor = 0usize;
    let mut emit_gap = |gs: usize, ge: usize, chunks: &mut Vec<Chunk>, stats: &mut RustChunkStats| {
        if gs >= ge {
            return;
        }
        // Never drop meaningful top-level code; skip pure punctuation gaps.
        if !text[gs..ge].chars().any(|c| c.is_alphanumeric()) {
            return;
        }
        let first = line_of(gs);
        let last = line_of(ge - 1);
        let before = chunks.len();
        chunks.extend(window_lines(rel, text, &line_starts, first, last));
        stats.fallback_windows += chunks.len() - before;
    };
    for (ss, se) in spans {
        if se <= cursor {
            continue; // defensive: spans are disjoint by construction
        }
        let ss = ss.max(cursor);
        emit_gap(cursor, ss, &mut chunks, &mut stats);
        if se - ss > RUST_FN_CAP_BYTES {
            // Over cap: bounded whole-line windows over the fn's own
            // lines — never truncated, never unbounded.
            let first = line_of(ss);
            let last = line_of(se - 1);
            let before = chunks.len();
            chunks.extend(window_lines(rel, text, &line_starts, first, last));
            stats.fallback_windows += chunks.len() - before;
        } else {
            chunks.push(Chunk {
                path: rel.to_owned(),
                start: line_of(ss),
                end: line_of(se - 1),
                content: text[ss..se].to_owned(),
            });
        }
        cursor = cursor.max(se);
    }
    emit_gap(cursor, text.len(), &mut chunks, &mut stats);

    // Sorted, non-overlapping by construction; belt-and-braces order.
    chunks.sort_by(|a, b| (a.start, a.end).cmp(&(b.start, b.end)));
    Ok((chunks, stats))
}

/// Whole-line windows over 1-based lines `first..=last` with absolute
/// numbering, using the same `TARGET_CHARS` rule as `chunk_text`.
/// Caller guarantees the range lies on line boundaries of `text`.
fn window_lines(
    rel: &str,
    text: &str,
    line_starts: &[usize],
    first: usize,
    last: usize,
) -> Vec<Chunk> {
    let mut out = Vec::new();
    if first > last || last > line_starts.len() {
        return out;
    }
    let target = super::chunks::TARGET_CHARS;
    let line_end = |no: usize| -> usize {
        if no < line_starts.len() { line_starts[no] } else { text.len() }
    };
    let mut start = first;
    let mut chars = 0usize;
    for no in first..=last {
        let s = line_starts[no - 1];
        let e = line_end(no);
        chars += text[s..e].chars().count();
        if chars >= target {
            out.push(Chunk {
                path: rel.to_owned(),
                start,
                end: no,
                content: text[line_starts[start - 1]..e].to_owned(),
            });
            start = no + 1;
            chars = 0;
        }
    }
    if start <= last {
        out.push(Chunk {
            path: rel.to_owned(),
            start,
            end: last,
            content: text[line_starts[start - 1]..line_end(last)].to_owned(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REL: &str = "src/sample.rs";

    /// Shared invariants: byte-exact verifiable ranges, sorted,
    /// non-overlapping, no synthesized text.
    fn assert_invariants(text: &str, chunks: &[Chunk]) {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut prev_end = 0usize;
        for c in chunks {
            assert_eq!(c.path, REL, "path must be the rel path");
            assert!(c.start >= 1 && c.end >= c.start && c.end <= lines.len(), "range in bounds");
            assert!(c.start > prev_end, "sorted and non-overlapping");
            prev_end = c.end;
            assert_eq!(
                lines[c.start - 1..c.end].concat(),
                c.content,
                "content must be the exact source lines"
            );
            assert!(
                c.content.is_empty() || text.contains(&c.content),
                "no synthesized text"
            );
        }
    }

    /// No meaningful (alphanumeric-bearing) source line may be uncovered.
    fn assert_meaningful_covered(text: &str, chunks: &[Chunk]) {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if line.chars().any(|c| c.is_alphanumeric()) {
                let no = i + 1;
                assert!(
                    chunks.iter().any(|c| c.start <= no && no <= c.end),
                    "meaningful line {no} dropped: {line:?}"
                );
            }
        }
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// `Chunk` has no `PartialEq` (core owner's struct): compare field-wise.
    fn assert_chunks_eq(a: &[Chunk], b: &[Chunk]) {
        assert_eq!(a.len(), b.len(), "chunk counts must match");
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!((&x.path, x.start, x.end, &x.content), (&y.path, y.start, y.end, &y.content));
        }
    }

    #[test]
    fn cap_is_frozen_at_8192_bytes() {
        assert_eq!(RUST_FN_CAP_BYTES, 8192);
    }

    #[test]
    fn free_fn_atomic_with_docs() {
        let text = "/// Adds one.\n#[inline]\nfn alpha(x: i32) -> i32 {\n    x + 1\n}\n\n/// Doubles.\nfn beta(x: i32) -> i32 {\n    x * 2\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(stats.fallback_reason, None);
        assert_eq!(stats.fn_count, 2);
        // Blank-line gap between the fns carries no code: exactly 2 chunks.
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].content.starts_with("/// Adds one."));
        assert!(chunks[0].content.contains("fn alpha"));
        assert!(chunks[1].content.starts_with("/// Doubles."));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn impl_methods_emitted_and_struct_gap_kept() {
        let text = "pub struct Foo {\n    pub x: i32,\n}\n\nimpl Foo {\n    /// Gets x.\n    pub fn get(&self) -> i32 {\n        self.x\n    }\n\n    pub fn set(&mut self, v: i32) {\n        self.x = v;\n    }\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(stats.fallback_reason, None);
        assert_eq!(stats.fn_count, 2);
        // Struct-decl gap (meaningful) + 2 method passages.
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].content.contains("pub struct Foo"));
        assert!(chunks[1].content.contains("fn get"));
        assert!(chunks[1].content.starts_with("    /// Gets x."), "chunks: {chunks:#?}");
        assert!(chunks[2].content.contains("fn set"));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn nested_fn_never_duplicated() {
        let text = "fn outer() -> i32 {\n    fn inner() -> i32 {\n        41\n    }\n    inner() + 1\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(stats.fn_count, 1);
        assert_eq!(chunks.len(), 1);
        let all: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert_eq!(count_occurrences(&all, "fn inner"), 1, "nested fn must appear once");
        assert_eq!(count_occurrences(&all, "fn outer"), 1);
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn same_line_functions_and_deep_nesting_preserve_exact_ranges() {
        let text = "fn first() {} fn second() {\n    if true { if true { if true { let _x = 1; } } }\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(stats.fn_count, 2);
        assert_eq!(chunks.iter().map(|c| c.content.as_str()).collect::<String>(), text);
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn comment_attributed_to_following_fn() {
        let text = "fn a() {}\n// about b\nfn b() {}\n";
        let (chunks, _) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].content.contains("// about b"), "comment must not leak to prior fn");
        assert!(chunks[1].content.starts_with("// about b"));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn raw_string_braces_and_macros_do_not_split() {
        let text = "macro_rules! m {\n    ($x:expr) => { { $x } };\n}\n\nfn f() -> &'static str {\n    let s = r#\"{\"key\": 1}\"#;\n    let _ = m!(s);\n    s\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(stats.fallback_reason, None);
        assert_eq!(stats.fn_count, 1);
        // macro_rules! block is a meaningful gap; fn is atomic.
        assert_eq!(chunks.len(), 2);
        let f = chunks.iter().find(|c| c.content.contains("fn f")).unwrap();
        assert!(f.content.contains("r#\""));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn crlf_retained_and_ranges_verifiable() {
        let text = "/// Doc.\r\nfn a() {}\r\n\r\nfn b() {\r\n    1\r\n}\r\n";
        let (chunks, _) = chunk_rust_with_stats(REL, text).unwrap();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(c.content.contains("\r\n"), "CRLF must be retained");
        }
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn lone_cr_is_not_a_line_break() {
        let text = "// a\rb note\nfn f() -> i32 {\n    1\n}\n";
        let (chunks, _) = chunk_rust_with_stats(REL, text).unwrap();
        assert!(chunks.iter().any(|c| c.content.contains("fn f")));
        // Lone CR kept verbatim inside its line; ranges still verify.
        assert!(chunks.iter().any(|c| c.content.contains("a\rb")));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn utf8_byte_exact() {
        let text = "/// 計算する 🦀\nfn たしざん(あ: i32, い: i32) -> i32 {\n    あ + い // 合計 🧮\n}\n";
        let (chunks, _) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("たしざん"));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn syntax_error_partial_still_emits_functions() {
        let text = "fn broken( {\n    oops\n}\n\nfn good() -> i32 {\n    7\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        // Error-tolerant: the valid fn is still atomic; mode stays function.
        assert_eq!(stats.fallback_reason, None);
        assert!(chunks.iter().any(|c| c.content.contains("fn good")));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn garbage_with_no_functions_falls_back_to_baseline() {
        let text = "@@@ {{{ ]]] not rust at all (((\nmore ((( noise\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert!(stats.fallback_reason.is_some());
        assert_chunks_eq(&chunks, &super::super::chunks::chunk_text(REL, text));
    }

    #[test]
    fn plain_text_with_no_functions_falls_back_to_baseline() {
        let text = "hello\nworld\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        // Exact reason ("no-functions" vs "parse-error-no-functions") is
        // grammar-version-dependent; the contract is full baseline fallback.
        assert!(stats.fallback_reason.is_some());
        assert_chunks_eq(&chunks, &super::super::chunks::chunk_text(REL, text));
    }

    #[test]
    fn huge_function_windowed_not_unbounded() {
        let mut text = String::from("/// Big.\nfn huge() {\n");
        for i in 0..400 {
            text.push_str(&format!("    let v{i}: u64 = {i};\n"));
        }
        text.push_str("}\n");
        assert!(text.len() > RUST_FN_CAP_BYTES, "fixture must exceed the cap");
        let (chunks, stats) = chunk_rust_with_stats(REL, &text).unwrap();
        assert_eq!(stats.fallback_reason, None);
        assert_eq!(stats.fn_count, 1);
        assert!(chunks.len() > 1, "over-cap fn must be windowed, got {}", chunks.len());
        // Contiguous absolute coverage of the whole file, no truncation.
        assert_eq!(chunks[0].start, 1);
        for w in chunks.windows(2) {
            assert_eq!(w[0].end + 1, w[1].start, "windows must tile without gaps");
        }
        let last = chunks.last().unwrap();
        let total_lines = text.split_inclusive('\n').count();
        assert_eq!(last.end, total_lines);
        let all: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert_eq!(all, text, "tiling must reproduce the source exactly");
        assert_invariants(&text, &chunks);
        assert_meaningful_covered(&text, &chunks);
    }

    #[test]
    fn empty_file_matches_baseline() {
        let (chunks, stats) = chunk_rust_with_stats(REL, "").unwrap();
        assert_eq!(stats.fallback_reason, Some("empty"));
        assert_chunks_eq(&chunks, &super::super::chunks::chunk_text(REL, ""));
        assert_invariants("", &chunks);
    }

    #[test]
    fn trait_and_const_context_preserved_via_gaps() {
        let text = "pub const LIMIT: usize = 8;\n\npub trait T {\n    fn sig(&self) -> i32;\n}\n\nfn real() -> i32 {\n    LIMIT as i32\n}\n";
        let (chunks, stats) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(stats.fallback_reason, None);
        // Bodiless trait signatures are not atomic fns, but their text
        // must survive via gap windows alongside the const and the fn.
        let all: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(all.contains("pub const LIMIT"));
        assert!(all.contains("fn sig"));
        assert!(all.contains("fn real"));
        assert_invariants(text, &chunks);
        assert_meaningful_covered(text, &chunks);
    }

    #[test]
    fn chunk_rust_agrees_with_stats_vector() {
        let text = "fn a() {}\nfn b() {}\n";
        let v = chunk_rust(REL, text).unwrap();
        let (v2, _) = chunk_rust_with_stats(REL, text).unwrap();
        assert_eq!(v.len(), v2.len());
        for (a, b) in v.iter().zip(v2.iter()) {
            assert_eq!((a.start, a.end, &a.content), (b.start, b.end, &b.content));
        }
    }
}
