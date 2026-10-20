# wg retrieval quality: launch misses, scale, and ranking options

Date: 2026-10-20. Analysis only. No product code, test, docs, script or CI
change. All new files are under `bench/` with the `WG_QUALITY_` prefix.

## 0. Identity and environment

| Item | Value |
| --- | --- |
| Benchmarked tree | commit `87b0b51` |
| `src/lib.rs` SHA-256 on the VPS | `db7dec11742d7a6908c150567c35af0cface387965b07bb688f33bc8046110c1` (same blob as `87b0b51`) |
| `src/live.rs` SHA-256 on the VPS | `5d0e887c33e2915357520471f1107abb574fc2a6193aba0a46556a2a5ac9a0c3` (same blob as `87b0b51`) |
| Current `wg` binary | `/srv/projects/wispd/wg-quality-20261020/src/target/release/wg` |
| `wg` SHA-256 | `3408641531e1d1bdf0d2ec4e3dddd29e0ee4d1bcbbd43e157f3436db5c80b802` |
| Analysis tool | `/srv/projects/wispd/wg-quality-20261020/src/target/release/wg-quality-tool` |
| Analysis tool SHA-256 | `1efe0e52dc803c15e1a98f768d309b81fe89ab408ab7b0bc1cfacee9b3a03f66` |
| Model | `minishlab/potion-code-16M-v2`, revision `e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b` |
| Weights SHA-256 | `75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c` |
| Machine | Debian 13, x86_64, 4 vCPU, 7.8 GiB, shared VPS |
| Threads | `RAYON_NUM_THREADS=2` for all runs |
| Launch corpus | `/srv/projects/wispd/wg-launch-bench-20261020/campaign2/corpus` |
| Launch chunks | 63,786 |

The local working tree is changed by another agent after the sync. The VPS copy
matches commit `87b0b51` exactly, checked by SHA-256 for `src/lib.rs` and
`src/live.rs`. The benchmark did not include the concurrent changes.

The analysis tool is a separate Cargo package at
`bench/WG_QUALITY_TOOL/`. It calls the public `wgrep` library and reproduces
`rank_impl` for the baseline. It adds optional candidate depth, fusion weight,
path bonus and definition bonus. The baseline reproduces the `wg` CLI exactly:

| Check | Result |
| --- | --- |
| Launch q1-q8 file rank, tool baseline vs CLI | 8 of 8 identical |
| Launch q1-q8 line rank, tool baseline vs CLI | identical where present |
| RepoQA dev, tool baseline vs CLI | 195/268/283, MRR 0.74903 in both |
| RepoQA held-out, tool baseline vs CLI | 202/270/283, MRR 0.76569 in both |
| CoSQA dev, tool baseline vs CLI | 72/141/174, MRR 0.41394 in both |
| CoSQA held-out, tool baseline vs CLI | 74/146/177, MRR 0.42088 in both |

## 1. Root cause of the launch misses

Commands: `wg "<question>" <corpus> --model code --json --explain --top 50`,
plus the analysis tool with the same corpus. The tool reports the answer chunk,
its lexical rank over all chunks, its semantic rank over all chunks, the position
in each top-200 pool, and the fused rank in the union.

### 1.1 Answer-chunk facts

| Q | Answer chunk | In lexical top 200 | Lexical rank | In semantic top 200 | Semantic rank | In union | BM25 | Semantic | Fused | Fused rank | File rank | Line rank |
| --- | --- | :---: | ---: | :---: | ---: | :---: | ---: | ---: | ---: | ---: | ---: | ---: |
| q3 | `tools/clippy/clippy_lints/src/methods/clone_on_copy.rs:61-118` | no | 308 | no | 3,121 | no | 11.791 | 0.4664 | - | - | absent | absent |
| q5 | `librustdoc/html/render/search_index.rs:1267-1274` | yes | 70 | yes | 51 | yes | 16.613 | 0.5696 | 0.73995 | 26 of 343 | 10 | 26 |

The answer line is inside one chunk in both cases. Neither miss is a chunking
failure: the q3 chunk holds the full `check` function, and the q5 chunk holds
the full `build_index` function.

### 1.2 q3: what beats the answer

The answer chunk is not in the candidate union. The top five rows are all test
files, generated lint tables, or a sibling lint:

| Rank | Path | BM25 | Semantic | Fused |
| ---: | --- | ---: | ---: | ---: |
| 1 | `tools/clippy/tests/ui/clone_on_copy.rs:1-59` | 19.380 | 0.8101 | 0.9288 |
| 2 | `tools/clippy/tests/ui/search_is_some_fixable_some.rs:89` | 13.127 | 0.8447 | 0.8821 |
| 3 | `tools/clippy/tests/ui/search_is_some_fixable_none.rs:90` | 13.127 | 0.8412 | 0.8786 |
| 4 | `tools/clippy/tests/ui/map_clone.rs:1-10` | 13.092 | 0.8279 | 0.8655 |
| 5 | `tools/clippy/tests/ui/clone_on_copy.fixed:1-102` | 18.473 | 0.7412 | 0.8512 |

The answer chunk has BM25 11.791 against 19.380 for the top test file. It has
semantic 0.4664 against 0.8101. Both signals put it far down. The filename
`clone_on_copy` supplies `clone` and `copy` to BM25 for many test files, so the
path terms do not separate the lint source from its tests.

Depth 400 includes the answer chunk, but only at fused rank 491 of 755. Depth
alone does not fix the miss within the top 50. A path bonus alone recovers the
file at unique-file rank 11 (`path20`) and 16 (`path10`), but not the answer
line and not the top 10.

**Class: candidate-recall failure.** Root cause is a model failure: the static
embedding ranks the answer chunk 3,121st, and BM25 gives it too little weight to
enter the top 200.

### 1.3 q5: what beats the answer

The answer chunk is in both pools, at lexical position 70 and semantic position
51. Prose pages beat it on both signals:

| Rank | Path | BM25 | Semantic | Fused |
| ---: | --- | ---: | ---: | ---: |
| 1 | `doc/rustdoc.md:1-3` | 20.305 | 0.6904 | 0.9386 |
| 2 | `doc/rustc-dev-guide/src/building/compiler-documenting.md:1-67` | 21.304 | 0.6507 | 0.8938 |
| 3 | `doc/index.md:78-141` | 26.920 | 0.6049 | 0.8837 |
| 4 | `doc/rustdoc/README.md:1-5` | 19.497 | 0.6487 | 0.8744 |
| 5 | `doc/rustdoc/src/unstable-features.md:410-456` | 24.928 | 0.5966 | 0.8539 |
| 26 | `librustdoc/html/render/search_index.rs:1267-1274` (answer) | 16.613 | 0.5696 | 0.7400 |

A fusion weight change does not fix this. At `w=0.50` the answer moves to fused
rank 20, worse than 26. Both base signals prefer the prose set. A
symbol-definition bonus does fix it: the answer chunk defines `build_index`,
and the query has `build` and `index`. With `def=0.10` the answer moves to
fused rank 6; with `def=0.20` it moves to rank 1.

**Class: fusion/weighting failure.** The answer is in the candidate pool and the
chunk is correct. The ranker has no definition signal, and the static model
prefers documentation prose.

### 1.4 Summary

| Q | Primary class | Evidence |
| --- | --- | --- |
| q3 | Candidate-recall failure (model root cause) | Lexical rank 308, semantic rank 3,121, absent from the union; depth 400 still rank 491 |
| q5 | Fusion/weighting failure | In union (lexical 70, semantic 51), fused rank 26, fixed to rank 1 by a definition bonus |

q3 is not fixable by weighting or depth alone. q5 is fixable by a definition
signal, but that signal is not free across tasks (section 3.4).

## 2. Scale: RepoQA 600 and CoSQA 500

Runners: `bench/WG_QUALITY_REPOQA.py` and `bench/WG_QUALITY_COSQA.py`. They use
the archived imported queries and corpora. They call the current `wg` CLI with
`--model code --json --explain --top 100`, deduplicate by file, and score
distinct-file Hit@1/5/10 and MRR@10. The archived source-token budget does not
apply to file-hit metrics, so no `--budget-bytes` step is used.

### 2.1 RepoQA 600

| Measure | Current `wg` | Archived sgrep | Delta |
| --- | ---: | ---: | ---: |
| Hit@1 distinct file | 397 / 600 (66.2%) | 397 / 600 (66.2%) | 0 |
| Hit@5 distinct files | 538 / 600 (89.7%) | 538 / 600 (89.7%) | 0 |
| Hit@10 distinct files | 566 / 600 (94.3%) | 566 / 600 (94.3%) | 0 |
| MRR@10 | 0.75736 | not published | - |
| Errors | 0 | 0 | 0 |

Per language, all counts match the archived control table exactly:

| Language | Hit@1 | Hit@5 | Hit@10 | MRR@10 |
| --- | ---: | ---: | ---: | ---: |
| C++ | 58 | 87 | 93 | 0.6942 |
| Go | 72 | 93 | 95 | 0.8013 |
| Java | 62 | 90 | 95 | 0.7286 |
| Python | 63 | 94 | 97 | 0.7564 |
| Rust | 74 | 93 | 96 | 0.8168 |
| TypeScript | 68 | 81 | 90 | 0.7468 |

The 34 RepoQA misses (not in the top 10) span these full unique-file ranks:

| Full rank | Queries |
| ---: | --- |
| absent from top 100 (5) | `cpp-ml-explore__mlx-02`, `go-iawia002__lux-00`, `java-karatelabs__karate-03`, `rust-helix-editor__helix-01`, `typescript-openai__openai-node-03` |
| 11-20 (21) | `cpp-WasmEdge__WasmEdge-05` (11), `go-iawia002__lux-01` (11), `typescript-mrdoob__three.js-03` (11), `typescript-mrdoob__three.js-08` (11), `cpp-sass__node-sass-04` (12), `cpp-sass__node-sass-08` (12), `python-psf__black-02` (12), `cpp-ClickHouse__clickhouse-cpp-06` (13), `rust-huggingface__candle-04` (13), `typescript-umami-software__umami-01` (13), `go-lima-vm__lima-07` (14), `python-ethereum__web3.py-08` (14), `typescript-cheeriojs__cheerio-07` (15), `cpp-WasmEdge__WasmEdge-08` (16), `rust-rust-bakery__nom-00` (16), `java-Password4j__password4j-00` (19), `java-karatelabs__karate-00` (19), `typescript-mrdoob__three.js-01` (19), `typescript-openai__openai-node-05` (19), `cpp-ml-explore__mlx-07` (20), `java-palantir__palantir-java-format-06` (20) |
| 21-50 (5) | `python-pyg-team__pytorch_geometric-03` (22), `typescript-firebase__firebase-admin-node-08` (27), `java-google__conscrypt-03` (31), `rust-rust-lang__cargo-04` (35), `typescript-langchain-ai__langchainjs-03` (35) |
| 51-100 (3) | `go-iawia002__lux-06` (67), `go-iawia002__lux-05` (71), `typescript-date-fns__date-fns-09` (72) |

Target paths are in `repoqa/full-ranks.jsonl` on the VPS. Only 5 of 34 misses
are absent from the returned top 100. The rest are ranking misses.

### 2.2 CoSQA 500

| Measure | Current `wg` | Archived sgrep | Delta |
| --- | ---: | ---: | ---: |
| Hit@1 unique snippet | 146 / 500 (29.2%) | 146 / 500 (29.2%) | 0 |
| Hit@5 unique snippets | 287 / 500 (57.4%) | 287 / 500 (57.4%) | 0 |
| Hit@10 unique snippets | 351 / 500 (70.2%) | 351 / 500 (70.2%) | 0 |
| MRR@10 | 0.417412 | 0.41741 | 0 |
| Errors | 0 | 0 | 0 |

The 149 CoSQA misses are in `bench/WG_QUALITY_COSQA_MISSES.csv` with their full
unique-file rank. Distribution: 26 absent from top 100, 47 at 11-20, 56 at
21-50, 20 at 51-100.

**Conclusion: confirmed.** Current `wg` equals the archived pinned-sgrep
numbers exactly on both tasks, including every per-language RepoQA count.

## 3. Ranking options

### 3.1 Split

- RepoQA: repo-level split. For each language, the first five `source_id`
  values in sorted order are dev (30 repos, 300 queries); the last five are
  held-out (30 repos, 300 queries). No repo appears in both halves.
- CoSQA: query-order split, first 250 dev and last 250 held-out. CoSQA is used
  only to confirm the config selected on RepoQA dev. It is not used to choose
  parameters.
- Parameter scan and selection use RepoQA dev only. Held-out is reported once
  with a predeclared config list.

### 3.2 Dev sweep (n = 300, Hit@1/5/10 and MRR@10)

| Config | H@1 | H@5 | H@10 | MRR@10 | Line@5 | Union misses |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline (0.25/0.75, 200+200) | 195 | 268 | 283 | 0.74903 | 228 | 2 |
| pool 400+400 | 196 | 268 | 284 | 0.75112 | 227 | 0 |
| pool 600+600 | 196 | 268 | 284 | 0.75057 | 227 | 0 |
| w=0.10 | 178 | 266 | 280 | 0.71152 | 228 | 2 |
| w=0.15 | 185 | 267 | 280 | 0.72474 | 231 | 2 |
| w=0.35 | 194 | 265 | 283 | 0.74792 | 233 | 2 |
| w=0.50 | 195 | 266 | 281 | 0.74816 | 233 | 2 |
| path 0.05 | 196 | 268 | 283 | 0.75103 | 228 | 2 |
| path 0.10 | 196 | 268 | 283 | 0.75107 | 228 | 2 |
| path 0.20 | 197 | 268 | 283 | 0.75274 | 228 | 2 |
| def 0.05 | 197 | 265 | 283 | 0.75417 | 229 | 2 |
| def 0.10 | 199 | 269 | 283 | 0.75707 | 228 | 2 |
| def 0.20 | 192 | 269 | 286 | 0.74227 | 228 | 2 |
| pool 400+400, w=0.15 | 186 | 266 | 280 | 0.72716 | 230 | 0 |
| path 0.10 + def 0.10 | 200 | 270 | 283 | 0.75891 | 228 | 2 |

Dev best was `path 0.10 + def 0.10` at 200/270/283. The definition bonus alone
was 199/269/283. On held-out the two were identical (206/270/283), so the path
component added no value. The parsimonious dev-selected candidate is the
definition bonus at 0.10.

### 3.3 Held-out measurement (n = 300)

The config list was fixed before this run: baseline, def 0.10, path 0.20,
pool 400+400, w=0.50, and path 0.10 + def 0.10.

| Config | H@1 | H@5 | H@10 | MRR@10 | Line@5 | Delta H@1 | Delta MRR |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | 202 | 270 | 283 | 0.76569 | 227 | 0 | 0 |
| def 0.10 | 206 | 270 | 283 | 0.77682 | 233 | +4 | +0.0111 |
| path 0.20 | 201 | 270 | 283 | 0.76406 | 227 | -1 | -0.0016 |
| pool 400+400 | 201 | 270 | 283 | 0.76762 | 228 | -1 | +0.0019 |
| w=0.50 | 202 | 264 | 286 | 0.76745 | 226 | 0 | +0.0018 |
| path 0.10 + def 0.10 | 206 | 270 | 283 | 0.77682 | 233 | +4 | +0.0111 |

Paired H@1 changes for def 0.10 against baseline: dev gained 9 and lost 5;
held-out gained 8 and lost 4. The direction is consistent, but the counts are
small.

### 3.4 CoSQA confirmation (selected on RepoQA dev)

CoSQA dev (n = 250) and held-out (n = 250), Hit@1/5/10 and MRR@10:

| Config | Dev H@1/5/10 | Dev MRR | Held-out H@1/5/10 | Held-out MRR |
| --- | ---: | ---: | ---: | ---: |
| baseline | 72/141/174 | 0.41394 | 74/146/177 | 0.42088 |
| def 0.10 | 67/141/168 | 0.40110 | 67/141/172 | 0.39455 |
| path 0.20 | 72/141/174 | 0.41394 | 74/146/177 | 0.42088 |
| pool 400+400 | 73/141/174 | 0.41602 | 75/146/176 | 0.42172 |
| w=0.50 | 70/130/159 | 0.39245 | 72/137/166 | 0.39719 |
| path 0.10 + def 0.10 | 67/141/168 | 0.40110 | 67/141/172 | 0.39455 |

CoSQA held-out H@1 for def 0.10 against baseline: gained 4, lost 9. The
definition bonus is task-specific. It helps long function-level descriptions
and hurts short snippet queries.

### 3.5 Ranked options and cost

Prototype definition signal: for each candidate chunk, match definition-leading
keywords (`fn`, `struct`, `enum`, `trait`, `type`, `mod`, `impl`, `class`, `def`,
`interface`, `function`, `func`, `let`, `var`, `const`), split the defined name
into the same word tokens as BM25, and add a flat bonus when any token equals a
query term. There is no stemming, IDF or name-position weighting. The broad
keyword set is why the signal fires often on short Python snippets.

Order is by held-out benefit on the primary task (RepoQA), with the CoSQA
confirmation and the cost.

| Rank | Option | RepoQA held-out gain | CoSQA held-out gain | Cost | Verdict |
| ---: | --- | --- | --- | --- | --- |
| 1 | Definition bonus 0.10 | +4 H@1, +0.0111 MRR, +6 line@5, no H@5/H@10 loss | -7 H@1, -0.0263 MRR | Regex over at most 400 candidate chunks; about 1 ms or less per query; no index or resident-memory growth | Task-specific; do not enable by default |
| 2 | Candidate pool 400+400 | -1 H@1, +0.0019 MRR; zero union misses | +1 H@1, +0.0008 MRR | At most 400 extra union rows. BM25 and cosine are already computed for all chunks. Under 10 KiB extra per query | Noise; not worth a parity break |
| 3 | Path/filename bonus 0.20 | -1 H@1, -0.0016 MRR | 0 | Path tokenization for at most 400 rows; about 1 ms or less; no memory growth | Does not generalise |
| 4 | Fusion weight w=0.50 | 0 H@1, -6 H@5, +3 H@10, +0.0018 MRR | -2 H@1, -0.024 MRR | Zero | Reject |
| 5 | Reranker over top-K (LateOn-Code-edge) | Not measured; archived DEV was negative | Not measured | +13.9 ms query encode and +54.2 ms MaxSim per query; 287.9 MB document-vector cache; 1.16 GB peak RSS; 375.6 s one-time document precompute | Reject |

The LateOn evidence is the archived `WGREP_LATEON_DEV_RERANK.md` result on 11 DEV
queries: file hits 3 of 11 for both arms, summed MRR 1.350 baseline versus 1.222
for LateOn, and one full gold span in the budgeted top 10 versus none. A
held-out run was not done because the DEV result did not justify the cost.

### 3.6 Public claim on semantic search

Use this wording:

> Semantic search is ranked, not exhaustive. `wg` fuses a lexical candidate set
> and a semantic candidate set, then returns a ranked shortlist. A correct
> answer can fall outside the shortlist.

Supporting numbers, with scope: RepoQA 600 puts the target file in the top 10
for 94.3% of queries; CoSQA 500 puts the gold snippet in the top 10 for 70.2%.
These are file and snippet hit rates, not full answer coverage.

## 4. Recommendation

1. **Ship the current baseline for wide release.** It reproduces pinned sgrep
   exactly on RepoQA 600 and CoSQA 500. The default `0.25/0.75` fusion over
   200+200 candidates is the only tested configuration that keeps exact parity.
2. **Do not ship the fusion weight change, the path bonus, the larger pool, or
   the reranker.** The measured gains are zero or negative, or within noise, and
   each one breaks exact parity.
3. **Do not make the definition bonus the default.** It is the only option with
   a consistent RepoQA gain (`+4` H@1 on dev and held-out), and it fixes q5 from
   rank 26 to rank 6. It also loses 7 H@1 and 0.0263 MRR on CoSQA held-out.
   A future release can add it as an explicit, non-parity, symbol-aware mode
   for function-level questions. Do that only after a fresh larger evaluation.
4. **q3 needs a representation or symbol-index change, not a rank change.**
   The answer chunk is 308th lexically and 3,121st semantically. A definition
   bonus does not reach it; depth 400 puts it at fused rank 491.
5. **State the parity break for each option.** Definition bonus, path bonus,
   candidate depth, fusion weight and reranker all change top-k order or scores.
   The definition and path bonuses change the fused float even when the order
   stays the same. A release that claims exact sgrep score parity must not
   include them.

## Appendix A. RepoQA miss list (full unique-file rank)

These are the 34 queries whose target file is outside the top 10.

| Query | Rank | Target file |
| --- | ---: | --- |
| `cpp-WasmEdge__WasmEdge-05` | 11 | `lib/api/wasmedge.cpp` |
| `go-iawia002__lux-01` | 11 | `extractors/tangdou/tangdou.go` |
| `typescript-mrdoob__three.js-03` | 11 | `src/core/BufferGeometry.js` |
| `typescript-mrdoob__three.js-08` | 11 | `src/renderers/webgl/WebGLExtensions.js` |
| `cpp-sass__node-sass-04` | 12 | `src/libsass/src/debugger.hpp` |
| `cpp-sass__node-sass-08` | 12 | `src/libsass/src/debugger.hpp` |
| `python-psf__black-02` | 12 | `src/black/nodes.py` |
| `cpp-ClickHouse__clickhouse-cpp-06` | 13 | `clickhouse/columns/column.h` |
| `rust-huggingface__candle-04` | 13 | `candle-core/src/quantized/metal.rs` |
| `typescript-umami-software__umami-01` | 13 | `src/pages/api/reports/insights.ts` |
| `go-lima-vm__lima-07` | 14 | `cmd/limactl/stop.go` |
| `python-ethereum__web3.py-08` | 14 | `web3/beacon/beacon.py` |
| `typescript-cheeriojs__cheerio-07` | 15 | `src/api/attributes.spec.ts` |
| `cpp-WasmEdge__WasmEdge-08` | 16 | `lib/host/wasi/win.h` |
| `rust-rust-bakery__nom-00` | 16 | `src/bytes/tests.rs` |
| `java-Password4j__password4j-00` | 19 | `src/main/java/com/password4j/types/Bcrypt.java` |
| `java-karatelabs__karate-00` | 19 | `karate-core/src/main/java/com/intuit/karate/MatchStep.java` |
| `typescript-mrdoob__three.js-01` | 19 | `src/objects/BatchedMesh.js` |
| `typescript-openai__openai-node-05` | 19 | `src/lib/chatCompletionUtils.ts` |
| `cpp-ml-explore__mlx-07` | 20 | `mlx/primitives.h` |
| `java-palantir__palantir-java-format-06` | 20 | `palantir-java-format/src/main/java/com/palantir/javaformat/java/ModifierOrderer.java` |
| `python-pyg-team__pytorch_geometric-03` | 22 | `torch_geometric/datasets/tu_dataset.py` |
| `typescript-firebase__firebase-admin-node-08` | 27 | `src/project-management/project-management-api-request-internal.ts` |
| `java-google__conscrypt-03` | 31 | `common/src/main/java/org/conscrypt/HpkeContextRecipient.java` |
| `rust-rust-lang__cargo-04` | 35 | `src/cargo/core/profiles.rs` |
| `typescript-langchain-ai__langchainjs-03` | 35 | `langchain/src/retrievers/self_query/vectara.ts` |
| `go-iawia002__lux-06` | 67 | `extractors/reddit/reddit_test.go` |
| `go-iawia002__lux-05` | 71 | `extractors/rumble/rumble_test.go` |
| `typescript-date-fns__date-fns-09` | 72 | `src/locale/bn/_lib/localize/index.ts` |
| `cpp-ml-explore__mlx-02` | absent | `mlx/array.cpp` |
| `go-iawia002__lux-00` | absent | `extractors/huya/huya_test.go` |
| `java-karatelabs__karate-03` | absent | `karate-core/src/main/java/com/intuit/karate/http/WebSocketClient.java` |
| `rust-helix-editor__helix-01` | absent | `helix-core/src/diff.rs` |
| `typescript-openai__openai-node-03` | absent | `src/_shims/index-deno.ts` |

## Limits

- One machine and one model. The launch corpus is a Rust tree. The option
  gains rest on 300 held-out RepoQA queries per arm.
- The def 0.10 gain is small. A paired H@1 gain of 8 against 4 losses is not
  statistically strong at this sample size.
- RepoQA and CoSQA score file and snippet hits. They do not measure full answer
  span coverage.
- CoSQA is an independent confirmation task. It was not used for tuning. It is
  also a different query style, so its regression is a real scope limit, not a
  reason to retune.
- The archived LateOn comparison used 11 consumed DEV queries, not this split.
- The archived RepoQA document records a scorer provenance limit for the control
  arm. This run uses the same distinct-file rank rule and reproduces the control
  counts exactly. It does not re-validate the original scorer.
- Another agent changed the local working tree during this work. The benchmarked
  source is commit `87b0b51`, checked by blob SHA-256 on the VPS.

## Receipts

VPS work and raw data:

- `/srv/projects/wispd/wg-quality-20261020/src` (source copy and builds)
- `/srv/projects/wispd/wg-quality-20261020/launch/` (`tool.json`, `cli/q*.json`)
- `/srv/projects/wispd/wg-quality-20261020/repoqa/` (`raw/`, `per-query.jsonl`, `summary.json`, `full-ranks.jsonl`)
- `/srv/projects/wispd/wg-quality-20261020/cosqa/` (`raw/`, `per-query.jsonl`, `summary.json`, `full-ranks.jsonl`)
- `/srv/projects/wispd/wg-quality-20261020/options/dev/` and `options/heldout/`
- `/srv/projects/wispd/wg-quality-20261020/cosqa-options/dev/` and `heldout/`
- Logs: `/srv/logs/wispd/wg-quality-*.log`
- All task-owned daemons were stopped with `wg down` after each repo and run.

Local files:

- `bench/WG_QUALITY_REPORT.md` (this file)
- `bench/WG_QUALITY_TOOL/` (analysis-only Rust re-scorer, with `Cargo.lock`)
- `bench/WG_QUALITY_LAUNCH_MISSES.py`
- `bench/WG_QUALITY_REPOQA.py`
- `bench/WG_QUALITY_COSQA.py`
- `bench/WG_QUALITY_OPTIONS.py`
- `bench/WG_QUALITY_COSQA_OPTIONS.py`
- `bench/WG_QUALITY_CALIBRATION_CONFIGS.json`
- `bench/WG_QUALITY_HELDOUT_CONFIGS.json`
- `bench/WG_QUALITY_COSQA_MISSES.csv`

No commit was made.
