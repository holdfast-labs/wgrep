//! Fusion ranking over a scope: every in-scope chunk is a candidate.
//!
//! Candidate union = top-200 lexical (positive BM25 only) + top-200
//! semantic over the SCOPE's chunks + exact-identifier hits. The scope is
//! normally the whole corpus — ranking is never constrained to literal
//! trigram hits, so semantic-only candidates are always possible — but a
//! `--prefix` filter ranks the actual subdirectory, not a global head that
//! is filtered after the fact (which could miss the best in-scope hit).
//! Per-set min-max scaling to 0–1, fused `0.75 * semantic + 0.25 * BM25`,
//! exact case-sensitive identifier hits first. Results fully contained in
//! an earlier result are removed. Scores are relative to the candidate
//! set, not confidence values.

use std::collections::HashSet;

use super::corpus::Corpus;
use super::lex;
use super::model::ModelCtx;
use super::DIM;

#[cfg(test)]
#[derive(Default)]
struct RankTrace {
    lexical_top400: Vec<usize>,
    semantic_top400: Vec<usize>,
    exact: Vec<usize>,
    union: Vec<usize>,
    fused: Vec<TraceRow>,
    deduped: Vec<TraceRow>,
}

#[cfg(test)]
#[derive(Clone)]
struct TraceRow {
    idx: usize,
    bm25: f64,
    semantic: f64,
    bm25_relative: f64,
    semantic_relative: f64,
    fused: f64,
    exact_identifier: bool,
    retained: bool,
}

#[cfg(test)]
thread_local! { static RANK_TRACE: std::cell::RefCell<Option<RankTrace>> = const { std::cell::RefCell::new(None) }; }

#[derive(Clone, Debug)]
pub(crate) struct Scored {
    pub idx: usize,
    pub bm25: f64,
    pub semantic: f64,
    pub bm25_relative: f64,
    pub semantic_relative: f64,
    pub fused: f64,
    pub exact_identifier: bool,
}

/// Resolve a `--prefix` filter to in-scope chunk indices. Shared by the
/// daemon rank path and the `--no-daemon` one-shot path so both rank the
/// identical scope (one-shot/daemon scope parity). `"."`/empty means the
/// whole corpus. A non-matching prefix is an error (exit 2 upstream),
/// never a silent empty ranking — except on an empty corpus, which
/// yields an empty scope.
pub(crate) fn scope_for(corpus: &Corpus, prefix: &str) -> anyhow::Result<Vec<usize>> {
    if prefix.is_empty() || prefix == "." {
        return Ok((0..corpus.chunks.len()).collect());
    }
    let p = prefix.trim_start_matches("./");
    let with_slash = format!("{p}/");
    let scope: Vec<usize> = corpus
        .chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.path == p || c.path.starts_with(&with_slash))
        .map(|(i, _)| i)
        .collect();
    if scope.is_empty() && !corpus.chunks.is_empty() {
        anyhow::bail!("wgrep-sem: prefix {prefix:?} matches no indexed chunks");
    }
    Ok(scope)
}

fn relative(scores: &[f64]) -> Vec<f64> {
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    scores
        .iter()
        .map(|s| if max > min { (s - min) / (max - min) } else { 0.0 })
        .collect()
}

fn bm25_scoped(corpus: &Corpus, scope: &[usize], terms: &[u32]) -> Vec<f64> {
    let n = scope.len();
    if n == 0 || terms.is_empty() {
        return vec![0.0; n];
    }
    let lengths: Vec<usize> = scope
        .iter()
        .map(|&i| corpus.counts[i].iter().map(|(_, k)| *k as usize).sum())
        .collect();
    let avg = (lengths.iter().sum::<usize>() as f64 / n as f64).max(1.0);
    // Document frequency is corpus-global (stable IDF); scoring is scoped.
    let total = corpus.chunks.len() as f64;
    let idfs: Vec<f64> = terms
        .iter()
        .map(|t| {
            let df = corpus.counts.iter().filter(|c| c.binary_search_by(|(id, _)| id.cmp(t)).is_ok()).count()
                as f64;
            (1.0 + (total - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    scope
        .iter()
        .zip(lengths)
        .map(|(&i, len)| {
            terms
                .iter()
                .zip(&idfs)
                .filter_map(|(t, idf)| {
                    corpus.counts[i].binary_search_by(|(id, _)| id.cmp(t)).ok().map(|k| {
                        let count = corpus.counts[i][k].1 as f64;
                        idf * count * 2.5 / (count + 1.5 * (0.25 + 0.75 * len as f64 / avg))
                    })
                })
                .sum()
        })
        .collect()
}

/// Rank `scope` (corpus chunk indices) for `query`. Returns at most
/// `top_n` scored chunk indices in final order. Errors only when the
/// query has no searchable words or embeddings fail; an empty scope
/// yields an empty vec.
pub(crate) fn rank_scoped(
    corpus: &Corpus,
    model: &ModelCtx,
    query: &str,
    scope: &[usize],
    top_n: usize,
) -> anyhow::Result<Vec<Scored>> {
    let mut terms = lex::words(query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        anyhow::bail!("wgrep-sem: query has no searchable words");
    }
    if scope.is_empty() {
        return Ok(Vec::new());
    }
    let term_ids: Vec<u32> = terms
        .iter()
        .filter_map(|t| corpus.vocab.get(t).copied())
        .collect();
    let lexical = bm25_scoped(corpus, scope, &term_ids);

    // Query embedding + cosine against in-scope chunk vectors only.
    let qv = model.embed(&[query.to_owned()])?.remove(0);
    let similarities: Vec<f64> = scope
        .iter()
        .map(|&i| {
            corpus.vecs[i * DIM..(i + 1) * DIM]
                .iter()
                .zip(&qv)
                .map(|(a, b)| f64::from(*a * *b))
                .sum()
        })
        .collect();

    let literals = lex::identifier_literals(query);
    let exact: HashSet<usize> = if literals.is_empty() {
        HashSet::new()
    } else {
        scope
            .iter()
            .copied()
            .filter(|&i| {
                literals.iter().any(|lit| lex::contains_identifier(&corpus.chunks[i].content, lit))
            })
            .collect()
    };

    // Candidate union within the scope.
    let mut lex_order: Vec<usize> =
        (0..scope.len()).filter(|&k| lexical[k] > 0.0).collect();
    lex_order.sort_by(|&a, &b| lexical[b].total_cmp(&lexical[a]));
    #[cfg(test)]
    let lexical_top400: Vec<usize> = lex_order.iter().take(400).map(|&k| scope[k]).collect();
    lex_order.truncate(200);
    let mut sem_order: Vec<usize> = (0..scope.len()).collect();
    sem_order.sort_by(|&a, &b| similarities[b].total_cmp(&similarities[a]));
    #[cfg(test)]
    let semantic_top400: Vec<usize> = sem_order.iter().take(400).map(|&k| scope[k]).collect();
    sem_order.truncate(200);
    let mut exact_order: Vec<usize> = exact
        .iter()
        .filter_map(|i| scope.iter().position(|&s| s == *i))
        .collect();
    exact_order.sort_unstable();

    let mut order = Vec::with_capacity(lex_order.len() + sem_order.len() + exact_order.len());
    order.extend(lex_order);
    order.extend(sem_order);
    order.extend(exact_order);
    let mut seen = HashSet::new();
    order.retain(|k| seen.insert(*k));

    #[cfg(test)]
    let traced_union = order.iter().map(|&k| scope[k]).collect::<Vec<_>>();

    let b: Vec<f64> = order.iter().map(|&k| lexical[k]).collect();
    let s: Vec<f64> = order.iter().map(|&k| similarities[k]).collect();
    let b_rel = relative(&b);
    let s_rel = relative(&s);
    let fused = |k: usize| 0.25 * b_rel[k] + 0.75 * s_rel[k];
    let mut fused_pos: Vec<usize> = (0..order.len()).collect();
    fused_pos.sort_by(|&a, &b2| {
        exact
            .contains(&scope[order[b2]])
            .cmp(&exact.contains(&scope[order[a]]))
            .then_with(|| fused(b2).total_cmp(&fused(a)))
            .then_with(|| b[b2].total_cmp(&b[a]))
            // Deterministic final tie key: identical scores must order
            // identically on every run and every worker. (P1: path, then
            // line range — stable across swap_remove rebuilds, unlike
            // corpus indices.)
            .then_with(|| {
                let ca = &corpus.chunks[scope[order[a]]];
                let cb = &corpus.chunks[scope[order[b2]]];
                ca.path.cmp(&cb.path).then(ca.start.cmp(&cb.start)).then(ca.end.cmp(&cb.end))
            })
    });

    #[cfg(test)]
    let fused_trace: Vec<TraceRow> = fused_pos.iter().map(|&k| {
        let ci = scope[order[k]];
        TraceRow { idx: ci, bm25: b[k], semantic: s[k], bm25_relative: b_rel[k], semantic_relative: s_rel[k], fused: fused(k), exact_identifier: exact.contains(&ci), retained: false }
    }).collect();

    let mut results = Vec::new();
    #[cfg(test)]
    let mut dedup_trace = Vec::new();
    for &k in &fused_pos {
        let ci = scope[order[k]];
        let c = &corpus.chunks[ci];
        let duplicate = results.iter().any(|r: &Scored| {
            let o = &corpus.chunks[r.idx];
            o.path == c.path && o.start <= c.start && c.end <= o.end
        });
        #[cfg(test)]
        dedup_trace.push(TraceRow { idx: ci, bm25: b[k], semantic: s[k], bm25_relative: b_rel[k], semantic_relative: s_rel[k], fused: fused(k), exact_identifier: exact.contains(&ci), retained: !duplicate });
        if duplicate {
            continue;
        }
        results.push(Scored {
            idx: ci,
            bm25: b[k],
            semantic: s[k],
            bm25_relative: b_rel[k],
            semantic_relative: s_rel[k],
            fused: fused(k),
            exact_identifier: exact.contains(&ci),
        });
        if results.len() >= top_n.max(1) {
            break;
        }
    }
    #[cfg(test)]
    RANK_TRACE.with(|slot| {
        if let Some(trace) = slot.borrow_mut().as_mut() {
            *trace = RankTrace { lexical_top400, semantic_top400, exact: exact.iter().copied().collect(), union: traced_union, fused: fused_trace, deduped: dedup_trace };
        }
    });
    Ok(results)
}

/// Rank the whole corpus (the common case): every chunk is a candidate.
pub(crate) fn rank(
    corpus: &Corpus,
    model: &ModelCtx,
    query: &str,
    top_n: usize,
) -> anyhow::Result<Vec<Scored>> {
    let scope: Vec<usize> = (0..corpus.chunks.len()).collect();
    rank_scoped(corpus, model, query, &scope, top_n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn coverage(corpus: &Corpus, ids: &[usize], file: &str, start: usize, end: usize) -> bool {
        let mut spans: Vec<_> = ids.iter().filter_map(|&i| {
            let c = &corpus.chunks[i];
            (c.path == file && c.end >= start && c.start <= end)
                .then_some((c.start.max(start), c.end.min(end)))
        }).collect();
        spans.sort_unstable();
        let mut cursor = start;
        for (a, b) in spans {
            if a > cursor { return false; }
            cursor = cursor.max(b.saturating_add(1));
            if cursor > end { return true; }
        }
        cursor > end
    }

    fn sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn output_coverage(intervals: &[(String, usize, usize)], file: &str, start: usize, end: usize) -> bool {
        let mut spans: Vec<_> = intervals.iter().filter_map(|(path, a, b)| {
            (path == file && *b >= start && *a <= end).then_some(((*a).max(start), (*b).min(end)))
        }).collect();
        spans.sort_unstable();
        let mut cursor = start;
        for (a, b) in spans {
            if a > cursor { return false; }
            cursor = cursor.max(b.saturating_add(1));
            if cursor > end { return true; }
        }
        cursor > end
    }

    fn corpus_with(paths: &[&str]) -> Corpus {
        let mut c = Corpus::empty(PathBuf::from("/root"));
        for (i, p) in paths.iter().enumerate() {
            c.chunks.push(super::super::chunks::Chunk {
                path: p.to_string(),
                start: 1,
                end: 1,
                content: format!("content {i}\n"),
            });
            c.counts.push(Vec::new());
            c.vecs.extend_from_slice(&[0.0f32; super::super::DIM]);
        }
        c
    }

    #[test]
    fn scope_for_parity_cases() {
        // The SAME resolver serves daemon and one-shot paths: one
        // implementation, one behavior. These cases pin it.
        let c = corpus_with(&["a/f.rs", "a/g.rs", "b/h.rs"]);
        assert_eq!(scope_for(&c, ".").unwrap(), vec![0, 1, 2]);
        assert_eq!(scope_for(&c, "").unwrap(), vec![0, 1, 2]);
        assert_eq!(scope_for(&c, "a").unwrap(), vec![0, 1]);
        assert_eq!(scope_for(&c, "./a").unwrap(), vec![0, 1]);
        // A file path as prefix selects exactly that file.
        assert_eq!(scope_for(&c, "b/h.rs").unwrap(), vec![2]);
        // No match is an error, never a silent empty ranking...
        assert!(scope_for(&c, "zzz").is_err());
        // ...except on an empty corpus, which yields empty.
        let empty = corpus_with(&[]);
        assert!(scope_for(&empty, "zzz").unwrap().is_empty());
    }

    #[test]
    fn file_presence_does_not_imply_function_coverage() {
        let mut c = corpus_with(&["src/lib.rs"]);
        c.chunks[0].start = 1;
        c.chunks[0].end = 20;
        assert!(c.chunks.iter().any(|chunk| chunk.path == "src/lib.rs"));
        assert!(!coverage(&c, &[0], "src/lib.rs", 40, 45));
        assert!(coverage(&c, &[0], "src/lib.rs", 10, 15));
    }

    /// DEV-only stage audit. Requires the frozen source corpus and verified
    /// offline model cache; deliberately ignored by ordinary test runs.
    #[test]
    #[ignore]
    fn dev11_stage_export() {
        use super::super::{chunks::Chunking, model::{self, ModelCtx}};
        use std::path::PathBuf;

        let fixture_path = std::env::var("WGREP_CANDIDATE_FIXTURE").expect("DEV fixture env");
        let root = PathBuf::from(std::env::var("WGREP_CANDIDATE_ROOT").expect("frozen corpus env"));
        let out_path = std::env::var("WGREP_CANDIDATE_OUT").expect("audit output env");
        let report_path = std::env::var("WGREP_CANDIDATE_REPORT").expect("classification report env");
        let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(&fixture_path).unwrap()).unwrap();
        let cases = fixture["dev"].as_array().expect("dev cases");
        assert_eq!(cases.len(), 11, "must use exactly DEV11");
        unsafe { std::env::set_var("WSEM_OFFLINE", "1"); }
        let cache = PathBuf::from("/srv/projects/wispd/sgrep-integration-20261013/native/sem-mode-smoke-cache-20261014");
        let model_dir = cache.join("models/minishlab--potion-code-16M-v2--e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b.");
        for (name, size, hash) in [(model::CODE.tok_name, model::CODE.tok_size, model::CODE.tok_sha), (model::CODE.w_name, model::CODE.w_size, model::CODE.w_sha)] {
            let bytes = std::fs::read(model_dir.join(name)).expect("verified cache asset must exist before ModelCtx::load");
            assert_eq!(bytes.len() as u64, size, "cache asset size: {name}");
            assert_eq!(sha256(&bytes), hash, "cache asset SHA-256: {name}");
        }
        let model = ModelCtx::load(&model::CODE, &cache).expect("load cached model");
        let mut corpus = Corpus::empty(root.clone());
        corpus.build(&model, Chunking::Window).expect("build frozen DEV window corpus");
        let scope: Vec<usize> = (0..corpus.chunks.len()).collect();
        let mut output = Vec::new();

        for case in cases {
            let id = case["id"].as_str().unwrap();
            let query = case["query"].as_str().unwrap();
            let gold: Vec<String> = case["gold"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_owned()).collect();
            let spans = case["gold_spans"].as_object().unwrap();
            RANK_TRACE.with(|t| *t.borrow_mut() = Some(RankTrace::default()));
            let normal = rank_scoped(&corpus, &model, query, &scope, 10).expect("rank top10");
            let all = rank_scoped(&corpus, &model, query, &scope, usize::MAX).expect("rank all");
            let trace = RANK_TRACE.with(|t| t.borrow_mut().take().unwrap());
            let same = |a: &Scored, b: &Scored| a.idx == b.idx && a.bm25.to_bits() == b.bm25.to_bits()
                && a.semantic.to_bits() == b.semantic.to_bits() && a.bm25_relative.to_bits() == b.bm25_relative.to_bits()
                && a.semantic_relative.to_bits() == b.semantic_relative.to_bits() && a.fused.to_bits() == b.fused.to_bits()
                && a.exact_identifier == b.exact_identifier;
            assert_eq!(normal.len(), 10, "{id}: expected ten normal rows");
            assert!(all.len() >= normal.len(), "{id}: all-result ranking shorter than top-10");
            assert!(normal.iter().zip(all.iter()).all(|(a,b)| same(a,b)), "{id}: top10 parity mismatch");

            let row_json = |r: &TraceRow| {
                let c = &corpus.chunks[r.idx];
                serde_json::json!({"path":c.path,"start":c.start,"end":c.end,"bm25":r.bm25,"semantic":r.semantic,"bm25_relative":r.bm25_relative,"semantic_relative":r.semantic_relative,"fused":r.fused,"exact_identifier":r.exact_identifier,"retained":r.retained})
            };
            let describe_ids = |ids: &[usize]| ids.iter().map(|&idx| { let c=&corpus.chunks[idx]; serde_json::json!({"path":c.path,"start":c.start,"end":c.end}) }).collect::<Vec<_>>();
            let gold_file_hits = |ids: &[usize]| gold.iter().any(|g| ids.iter().any(|&idx| corpus.chunks[idx].path == *g));
            let mut span_results = serde_json::Map::new();
            let corpus_ids: Vec<usize> = (0..corpus.chunks.len()).collect();
            let fused_ids: Vec<usize> = trace.fused.iter().map(|r| r.idx).collect();
            let dedup_ids: Vec<usize> = trace.deduped.iter().filter(|r| r.retained).map(|r| r.idx).collect();
            let mut budget = 64 * 1024usize;
            let mut output_ids = Vec::new();
            let mut output_intervals = Vec::new();
            for r in &normal {
                if budget == 0 { break; }
                output_ids.push(r.idx);
                let c = &corpus.chunks[r.idx];
                let mut take = c.content.len().min(budget);
                while !c.content.is_char_boundary(take) { take -= 1; }
                let prefix = &c.content[..take];
                let complete_lines = prefix.bytes().filter(|b| *b == b'\n').count();
                let line_count = if take == c.content.len() && !prefix.is_empty() && !prefix.ends_with('\n') { complete_lines + 1 } else { complete_lines };
                if line_count > 0 { output_intervals.push((c.path.clone(), c.start, (c.start + line_count - 1).min(c.end))); }
                budget -= take;
            }
            let mut corpus_complete = true;
            let mut union_complete = true;
            let mut fused_complete = true;
            let mut dedup_complete = true;
            let mut output_complete = true;
            let mut requires_multiple_corpus_chunks = false;
            for (file, span) in spans {
                let start = span[0].as_u64().unwrap() as usize;
                let end = span[1].as_u64().unwrap() as usize;
                let cc = coverage(&corpus, &corpus_ids, file, start, end);
                let one_chunk_cc = corpus_ids.iter().any(|&i| {
                    let c = &corpus.chunks[i];
                    c.path == *file && c.start <= start && c.end >= end
                });
                let uc = coverage(&corpus, &trace.union, file, start, end);
                let fc = coverage(&corpus, &fused_ids, file, start, end);
                let dc = coverage(&corpus, &dedup_ids, file, start, end);
                let oc = output_coverage(&output_intervals, file, start, end);
                corpus_complete &= cc;
                requires_multiple_corpus_chunks |= cc && !one_chunk_cc;
                union_complete &= uc;
                fused_complete &= fc;
                dedup_complete &= dc;
                output_complete &= oc;
                span_results.insert(file.clone(), serde_json::json!({"gold_start":start,"gold_end":end,"corpus_complete":cc,"corpus_single_chunk_complete":one_chunk_cc,"actual_union_complete":uc,"fused_complete":fc,"dedup_complete":dc,"budgeted_top10_complete":oc}));
            }
            let gold_top10 = normal.iter().any(|r| gold.contains(&corpus.chunks[r.idx].path));
            let gold_file_in_union = gold_file_hits(&trace.union);
            let gold_overlap_in_union = spans.iter().any(|(file, span)| trace.union.iter().any(|&i| {
                let c = &corpus.chunks[i];
                c.path == *file && c.end >= span[0].as_u64().unwrap() as usize && c.start <= span[1].as_u64().unwrap() as usize
            }));
            let classification = if !corpus_complete { "representation_gap" }
                else if output_complete { "retrieved" }
                else if !gold_overlap_in_union { "candidate-span omission" }
                else if !union_complete { "candidate-span incomplete" }
                else if fused_complete && !dedup_complete { "dedup-loss (trace-proven)" }
                else { "ranking/output-loss" };
            let membership = |ids: &[usize]| serde_json::json!({"any_gold_file":gold_file_hits(ids),"members":describe_ids(ids)});
            let component_cov = |ids: &[usize]| spans.iter().map(|(file, span)| {
                let start=span[0].as_u64().unwrap() as usize; let end=span[1].as_u64().unwrap() as usize;
                (file.clone(), serde_json::json!({"overlap":ids.iter().any(|&i| {let c=&corpus.chunks[i]; c.path==*file && c.end>=start && c.start<=end}),"complete":coverage(&corpus,ids,file,start,end)}))
            }).collect::<serde_json::Map<String,serde_json::Value>>();
            let best_overlap_rank = |ranked: &[usize]| ranked.iter().position(|&i| spans.iter().any(|(file,span)| {
                let c=&corpus.chunks[i]; c.path==*file && c.end>=span[0].as_u64().unwrap() as usize && c.start<=span[1].as_u64().unwrap() as usize
            })).map(|i| i+1);
            output.push(serde_json::json!({
                "id":id,"query":query,"gold":gold,"classification":classification,"gold_file_in_actual_union":gold_file_in_union,"gold_file_in_top10":gold_top10,
                "gold_function_complete_corpus_coverage":corpus_complete,"gold_function_complete_union_coverage":union_complete,"gold_function_complete_fused_coverage":fused_complete,"gold_function_complete_dedup_coverage":dedup_complete,"gold_function_complete_budgeted_top10_coverage":output_complete,"multichunk_gold_span":requires_multiple_corpus_chunks,"gold_spans":span_results,
                "best_gold_overlap_component_rank":{"lexical_top400":best_overlap_rank(&trace.lexical_top400),"semantic_top400":best_overlap_rank(&trace.semantic_top400)},
                "component_top200_400":{
                    "lexical_top200":{"membership":membership(&trace.lexical_top400.iter().take(200).copied().collect::<Vec<_>>()),"gold_span":component_cov(&trace.lexical_top400.iter().take(200).copied().collect::<Vec<_>>())},
                    "lexical_top400":{"membership":membership(&trace.lexical_top400),"gold_span":component_cov(&trace.lexical_top400)},
                    "semantic_top200":{"membership":membership(&trace.semantic_top400.iter().take(200).copied().collect::<Vec<_>>()),"gold_span":component_cov(&trace.semantic_top400.iter().take(200).copied().collect::<Vec<_>>())},
                    "semantic_top400":{"membership":membership(&trace.semantic_top400),"gold_span":component_cov(&trace.semantic_top400)}},
                "exact_identifier":membership(&trace.exact),"actual_union":membership(&trace.union),
                "fused_order":trace.fused.iter().map(row_json).collect::<Vec<_>>(),
                "dedup_final_rows":trace.deduped.iter().map(row_json).collect::<Vec<_>>(),
                "budget_bytes":65536,"normal_top10":normal.iter().map(|r| { let c=&corpus.chunks[r.idx]; serde_json::json!({"path":c.path,"start":c.start,"end":c.end,"bm25":r.bm25,"semantic":r.semantic,"bm25_relative":r.bm25_relative,"semantic_relative":r.semantic_relative,"fused":r.fused,"exact_identifier":r.exact_identifier}) }).collect::<Vec<_>>(),"budgeted_top10":output_ids.iter().map(|&i| {let c=&corpus.chunks[i]; serde_json::json!({"path":c.path,"start":c.start,"end":c.end})}).collect::<Vec<_>>()
            }));
        }
        let fixture_bytes = std::fs::read(&fixture_path).unwrap();
        let gold_paths: std::collections::BTreeSet<String> = cases.iter().flat_map(|c| c["gold"].as_array().unwrap().iter().map(|g| g.as_str().unwrap().to_owned())).collect();
        assert_eq!(gold_paths.len(), 7, "DEV11 gold manifest should contain exactly seven distinct source files");
        let gold_hashes: serde_json::Map<String, serde_json::Value> = gold_paths.into_iter().map(|p| { let b=std::fs::read(root.join(&p)).expect("gold source file"); (p,serde_json::json!({"sha256":sha256(&b),"bytes":b.len()})) }).collect();
        let result = serde_json::json!({"fixture_sha256": sha256(&fixture_bytes),"gold_file_hashes":gold_hashes,"model":{"repo":model.pin.repo,"revision":model.pin.revision,"tokenizer_sha256":model.pin.tok_sha,"weights_sha256":model.pin.w_sha,"cache":cache}, "root": root, "chunks": corpus.chunks.len(), "queries": output});
        let mut report = String::from("# DEV11 corrected candidate stage audit\n\n| Query | Classification | Gold file top-10 | Full gold span in actual union | Full span in budgeted top-10 |\n|---|---|---:|---:|---:|\n");
        for q in result["queries"].as_array().unwrap() {
            report.push_str(&format!("| {} | {} | {} | {} | {} |\n", q["id"].as_str().unwrap(), q["classification"].as_str().unwrap(), q["gold_file_in_top10"], q["gold_function_complete_union_coverage"], q["gold_function_complete_budgeted_top10_coverage"]));
        }
        report.push_str("\nCoverage uses complete 1-based inclusive line intervals. `retrieved` means the full gold span is present in actual top-10 rows within a 64 KiB cumulative content-prefix budget. Component ranks are diagnostic only; no counterfactual fusion or causal diagnosis is claimed. Multichunk spans are flagged. Top-10 score/order parity was asserted against an independent `rank_scoped(..., usize::MAX)` result for every query.\n");
        std::fs::write(&out_path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        std::fs::write(report_path, report).unwrap();
        eprintln!("DEV11 audit wrote {} query rows", result["queries"].as_array().unwrap().len());
    }
}
