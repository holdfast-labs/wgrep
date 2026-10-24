# Definition boost for wg semantic search: evaluation

Date: 2026-10-24. Analysis only. No product code, test, docs, script or CI
change. All new local files use the `bench/WG_DEFBOOST_` prefix or live under
`bench/WG_QUALITY_TOOL/`.

## 0. Answer first

**Do not ship a definition boost as the default.** The selective, name-based
design does not improve RepoQA held-out. The old flat bonus does improve RepoQA
held-out, but only behind a query-length gate. That gate also turns the bonus
off for the launch questions and for every query in both confirmation sets. The
gate is therefore a dataset separator, not a general rule.

| Question | Answer |
| --- | --- |
| Does a name-matched definition signal improve RepoQA held-out? | No. -1 H@1, +0.0017 MRR (n = 300). |
| Does it hurt CoSQA held-out? | No. 0 H@1, -0.0027 MRR (n = 250). It is neutral. |
| Does the old flat def 0.10 still improve RepoQA held-out? | Yes. +4 H@1, +0.0111 MRR. It reproduces the earlier result. |
| Does the flat bonus still hurt CoSQA held-out? | Yes. -7 H@1, -0.0263 MRR. |
| Can a gate remove the CoSQA harm? | Yes, a `>= 20` word gate. RepoQA has a minimum of 44 words; CoSQA has a maximum of 16. |
| Does that gated bonus fix the launch misses? | No. q1-q8 have 9-11 words, so the gate turns the bonus off. q5 stays at file rank 10, line rank 26. |
| Is there out-of-sample support for any variant? | Yes, for the narrow keyword set only, on the independent Semble bench: +0.0227 MRR, 1251 queries. It is still neutral on RepoQA and CoSQA held-out. |

Recommendation: **do not ship for the RepoQA goal.** An opt-in symbol-aware mode
is defensible for general code search, but it is not a RepoQA fix. Exact
algorithms are in section 9.

## 1. What Semble does

Source: `github.com/MinishLab/semble`, MIT, commit
`913bdac1d59bc2defc658453a65719d133f1ff5a` (2026-10-24). I read the source and
re-implemented the ideas; no code was copied.

### 1.1 Fusion is RRF, then an alpha blend

`src/semble/search.py`, `search()`:

- Line 12: `_RRF_K = 60`.
- Line 96: fetch `top_k * 5` candidates from each side.
- Line 15 `_rrf_scores()`: replace each raw score with `1 / (60 + rank)`.
- Lines 112-116: `alpha * semantic_rrf + (1 - alpha) * bm25_rrf`.
- Line 140 `resolve_alpha()` in `ranking/boosting.py`: `alpha = 0.3` for a
  symbol query, `alpha = 0.5` for a natural-language query. Alpha is the
  semantic weight; a symbol query leans on BM25.

### 1.2 The query gate is central

`src/semble/ranking/boosting.py`:

- `_SYMBOL_QUERY_RE` (lines 11-18) matches only a whole-query symbol:
  namespace-qualified (`A::b`, `a.b`, `a->b`), a leading underscore, an
  identifier with an uppercase letter or an underscore, or a word that starts
  with an uppercase letter.
- `is_symbol_query()` (line 135) applies the regex to the whole query.
- `apply_query_boost()` (line 91): the symbol-definition boost runs **only** for
  a symbol query. For a natural-language query it runs two other boosts
  (file-stem match and embedded camelCase symbols).

On the two target sets, that gate fires on **0 of 600 RepoQA queries and 0 of
500 CoSQA queries** (measured; see section 3.2). On the Semble bench it fires on
194 of 1251 queries (15.5%). So the Semble definition design cannot help either
target task.

### 1.3 Definition boost

`src/semble/ranking/boosting.py`:

- `_DEFINITION_KEYWORDS` (lines 36-58): `class`, `module`, `defmodule`, `def`,
  `interface`, `struct`, `enum`, `trait`, `type`, `func`, `function`, `object`,
  `abstract class`, `data class`, `fn`, `fun`, `package`, `namespace`,
  `protocol`, `record`, `typedef`. It is case-sensitive. It has no `let`, `var`,
  `const`, or `impl`. SQL DDL gets a separate case-insensitive list.
- `_definition_pattern()` (line 158) builds a regex per symbol name, with an
  optional namespace prefix. `_chunk_defines_symbol()` (line 169) tests one
  chunk.
- `_definition_tier()` (line 191): a defining chunk gets
  `boost_unit`; it gets `1.5 * boost_unit` when the file stem also matches the
  name.
- `_boost_symbol_definitions()` (line 217): `boost_unit = max_score * 3.0`
  (`_DEFINITION_BOOST_MULTIPLIER`, line 73). It boosts candidate chunks and also
  scans all chunks whose file stem matches the symbol, so a defining chunk that
  was not a candidate can enter the list.
- `_boost_embedded_symbols()` (line 239): CamelCase/camelCase names inside a NL
  query get a half-strength boost (`_EMBEDDED_SYMBOL_BOOST_SCALE = 0.5`, line 32).

The boost is additive on the fused score, scaled by the best score in the
candidate set. It is not IDF-weighted and not name-coverage-weighted.

### 1.4 Stem and file boosts

- `_boost_stem_matches()` (line 283): for NL queries, match query words to the
  file stem and the parent directory. Boost is `max_score * 1.0 *
  match_ratio`, and it applies only when `match_ratio >= 0.10` (line 313).
  Matching allows a 3-character prefix (`_count_keyword_matches`, line 270).
- `boost_multi_chunk_files()` (line 112): the best chunk of each file gets
  `0.2 * max_score * file_sum / max_file_sum` (line 79). This promotes files
  with several relevant chunks.

### 1.5 Tokenization and BM25

- `src/semble/tokens.py`, `split_identifier()` (line 12): a compound identifier
  contributes both the whole lowercase token and its camel/snake sub-tokens.
  `tokenize()` (line 35) applies this to all text.
- `src/semble/index/sparse.py`, `enrich_for_bm25()` (line 18): the BM25 document
  is `content + stem + stem + last three directory names`. The file stem is
  weighted twice.
- `src/semble/index/bm25.py`: `k1 = 1.5`, `b = 0.75`.

### 1.6 Path penalties and file saturation

`src/semble/ranking/penalties.py`, `rerank_topk()` (line 82) and
`_file_path_penalty()` (line 140):

- Test file or test directory: multiply by `0.3` (`_STRONG_PENALTY`).
- `compat`, `_compat`, `legacy`: multiply by `0.3`.
- `examples`, `_examples`, `docs_src`: multiply by `0.3`.
- `__init__.py`, `package-info.java`: multiply by `0.5`.
- `.d.ts`: multiply by `0.7`.
- The second and later chunks from one file are decayed by `0.5` per extra
  chunk (`_FILE_SATURATION_DECAY`).

The penalties are applied after the boosts, at rerank time. They are on by
default unless alpha is 1.0 (pure semantic).

### 1.7 Summary of the Semble design

Semble is careful about *when* a symbol boost applies. It requires a whole-query
symbol and uses a narrow definition keyword set. It does not weight names by
IDF or coverage. Its other main levers are RRF, file-stem matching, file
coherence, and test/legacy penalties. For RepoQA, the symbol gate is the
blocker: RepoQA questions are prose.

## 2. Method

### 2.1 Tool and baseline identity

The tool is `bench/WG_QUALITY_TOOL/`. I extended it with the signals below. All
new options default to off, and the baseline path is unchanged.

Checks before any experiment:

| Check | Result |
| --- | --- |
| RepoQA dev baseline vs archived tool rows | 300 of 300 identical |
| RepoQA held-out baseline vs archived tool rows | 300 of 300 identical |
| Launch q1-q8 baseline file rank vs archived `wg` CLI JSON | 8 of 8 identical |
| RepoQA dev base H@1/5/10, MRR | 195/268/283, 0.74903 (archive: same) |
| RepoQA held-out base | 202/270/283, 0.76569 (archive: same) |
| CoSQA dev base | 72/141/174, 0.41394 |
| CoSQA held-out base | 74/146/177, 0.42088 |

`src/lib.rs`, `src/codechunker.rs` and `src/splitter.rs` have no ranking or
chunking arithmetic change between `87b0b51` and HEAD. The `lib.rs` changes are
storage types (`SharedStr`, `ChunkText`); the chunker and splitter files are
unchanged.

### 2.2 Splits and protocol

- RepoQA: repo-level split. First five `source_id` values per language are dev
  (30 repos, 300 queries); the last five are held-out (30 repos, 300 queries).
- CoSQA: query-order split, first 250 dev, last 250 held-out.
- Parameter selection used RepoQA dev only. The configuration list was then
  fixed in `bench/WG_DEFBOOST_HELDOUT_CONFIGS.json` (SHA-256
  `b2f708b4ab78bab3bebb432059ae737957da6abe823e80ef1569f30865e4fd21`,
  taken before the held-out run) and run once on RepoQA held-out, CoSQA dev and
  CoSQA held-out.
- The third set (Semble bench) was read-only and was not used for selection.
- Query *style* was inspected to design gates (length, identifier shape). CoSQA
  outcomes were not used for selection.

### 2.3 Signals implemented

1. `def`: the legacy flat bonus (broad keyword regex, any name token matches a
   query term), kept for reproduction.
2. `defn`: a per-chunk definition-name score.
   - Name tokens: camelCase and snake_case split, lowercase, stopwords and
     1-character tokens removed.
   - Optional light stemming (strip a trailing `s`, not `ss`).
   - `defn_idf`: IDF-weighted coverage, `sum(idf matched) / sum(idf name)`.
   - `defn_spec`: require at least one matched token with
     `df <= spec * corpus_size` (specificity).
   - `defn_rarity`: multiply by `max(idf matched) / idf(df = 1)`.
   - `defn_kw`: `narrow` (real per-language definition sites) or `broad` (the
     legacy keyword regex).
   - `defn_gate`: `none`, `length` (`>= 20` words), `ident` (snake_case,
     camelCase, letter+digit, or a query word that is a full defined name),
     `length_or_ident`.
3. `pen`: multiplicative test/legacy path penalty (Semble-style).
4. `rrf`: reciprocal-rank fusion, k = 60, replacing min-max relative fusion.
5. A precomputed definition index: token ids, document frequency, IDF, and a
   full-name set. Built once per corpus.

Narrow keyword set by language:

| Language | Definition sites |
| --- | --- |
| Python | `def`, `class` |
| Rust | `fn`, `struct`, `enum`, `trait`, `union`, `type`, `mod` |
| Go | `func` (with or without a receiver), `type` |
| JS/TS | `function`, `class`, `interface`, `enum`, `type`, and top-level `const`/`let`/`var` |
| Java/C# | `class`, `interface`, `enum`, `record` |
| C/C++ | `class`, `struct`, `union`, `enum` (and `enum class`) |
| Ruby | `def`, `class`, `module` |
| PHP | `function`, `class`, `interface`, `trait`, `enum` |
| Swift | `class`, `struct`, `enum`, `protocol`, `func` |
| Kotlin | `class`, `interface`, `object`, `fun` |
| Scala | `class`, `trait`, `object`, `def` |

## 3. Dev sweeps (RepoQA dev, n = 300)

### 3.1 Signal ablation and weight sweep

| Config | H@1 | H@5 | H@10 | MRR |
| --- | ---: | ---: | ---: | ---: |
| baseline | 195 | 268 | 283 | 0.74903 |
| legacy def 0.10 (broad, flat) | 199 | 269 | 283 | 0.75707 |
| name coverage only | 194 | 266 | 285 | 0.75005 |
| + IDF | 191 | 268 | 285 | 0.74572 |
| + specificity 5% | 192 | 268 | 286 | 0.74955 |
| + specificity 5% + rarity, w 0.05 | 193 | 268 | 283 | 0.74851 |
| + specificity 5% + rarity, w 0.10 | 195 | 268 | 283 | 0.75276 |
| + specificity 5% + rarity, w 0.20 | 193 | 268 | 286 | 0.74954 |
| + specificity 5% + rarity, w 0.40 | 183 | 267 | 287 | 0.72802 |
| narrow keywords, weight 0.10 | 195 | 268 | 283 | 0.75276 |
| broad keywords, weight 0.10 | 200 | 268 | 283 | 0.76029 |
| broad, specificity 10%, weight 0.10 | 201 | 268 | 283 | 0.76112 |
| broad, specificity 2%, weight 0.10 | 199 | 268 | 282 | 0.75699 |
| broad, specificity 5%, weight 0.05 | 194 | 267 | 283 | 0.74913 |
| broad, specificity 5%, weight 0.20 | 196 | 267 | 285 | 0.75409 |
| broad, specificity 5%, rarity off | 199 | 267 | 284 | 0.75963 |
| broad, specificity 5%, IDF off | 199 | 268 | 283 | 0.75853 |
| broad, specificity 5%, + stemming | 195 | 268 | 283 | 0.75108 |
| broad, specificity 10%, + length gate | 201 | 268 | 283 | 0.76112 |
| RRF, no signal | 182 | 264 | 281 | 0.72527 |
| RRF + signal | 185 | 267 | 283 | 0.73723 |
| test/legacy penalty 0.30 | 189 | 265 | 281 | 0.73336 |
| penalty + signal | 188 | 265 | 281 | 0.73550 |
| legacy def 0.10 + signal | 200 | 270 | 285 | 0.75784 |

Observations:

- At weight 0.10, IDF, specificity and rarity each moved dev H@1 by at most 3.
  That is within noise on 300 queries. Weight 0.40 was clearly too large
  (-12 H@1). The flat legacy bonus was still the strongest single signal on dev.
- The narrow keyword set removed the gain (195 vs 201). The broad set also
  includes `impl` and `let`/`var`/`const` bindings; one or more of those add
  coverage in this corpus. The narrow set is more precise but loses answers.
- RRF was clearly worse (-13 H@1). Min-max relative fusion fits the fixed
  candidate union better.
- The test/legacy penalty was clearly worse (-6 H@1). Some RepoQA gold files
  are test files (`nom/src/bytes/tests.rs`, `lux/.../reddit_test.go`), so a
  blanket penalty removes valid answers.
- Stemming was neutral.

### 3.2 Gate candidates

| Gate | RepoQA dev fire | CoSQA dev fire | Verdict |
| --- | ---: | ---: | --- |
| Semble whole-query symbol | 0/300 | 0/250 | Does nothing for either task |
| identifier-style (snake/camel/digit or a word that is a defined name) | 74% of queries | 100% of queries | Does not separate; CoSQA words match defined names |
| length `>= 20` words | 300/300 | 0/250 | Separates, but only because the two sets have disjoint lengths |

Query length is the only cheap gate that separates the sets. The Semble gate is
too strict and the identifier gate is too loose.

## 4. Predeclared held-out (RepoQA, n = 300)

Run once with the list fixed above. `spec10_gate` is the length-gated signal.

| Config | H@1 | H@5 | H@10 | MRR | H@1 wins | H@1 losses | top-10 wins | top-10 losses |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | 202 | 270 | 283 | 0.76569 | - | - | - | - |
| legacy def 0.10 | 206 | 270 | 283 | 0.77682 | 8 | 4 | 27 | 10 |
| name spec 5%, w 0.10 | 201 | 270 | 283 | 0.76694 | 4 | 5 | 18 | 14 |
| name spec 10%, w 0.10 | 201 | 269 | 282 | 0.76739 | 4 | 5 | 19 | 14 |
| same, length gate | 201 | 269 | 282 | 0.76739 | 4 | 5 | 19 | 14 |
| same, identifier gate | 202 | 269 | 282 | 0.76933 | 4 | 4 | 19 | 12 |
| name, narrow keywords | 202 | 271 | 283 | 0.76647 | 7 | 7 | 20 | 15 |
| legacy def + name signal | 199 | 268 | 282 | 0.76555 | 8 | 11 | 28 | 20 |

The name signal does not replicate its dev gain. Its best MRR change is +0.0036
(the identifier gate) with an H@1 record of 4 wins and 4 losses. The plain
configs are 4 wins and 5 losses. The legacy flat bonus replicates the archived
result exactly (+4 H@1, +0.0111 MRR, 8 wins against 4 losses).

The length gate is a no-op on RepoQA (all 300 dev and all 300 held-out queries
pass; verified row-for-row on dev: 0 of 300 differences). So `legacy def 0.10 +
length gate` is byte-identical to `legacy def 0.10` on RepoQA held-out. Its
measured held-out result is therefore the def10 row: **+4 H@1, +0.0111 MRR**.

## 5. CoSQA confirmation (n = 250 + 250)

| Config | Dev H@1/5/10 | Dev MRR | Held-out H@1/5/10 | Held-out MRR | Held-out H@1 wins/losses |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline | 72/141/174 | 0.41394 | 74/146/177 | 0.42088 | - |
| legacy def 0.10 | 67/141/168 | 0.40110 | 67/141/172 | 0.39455 | 3 / 10 |
| name spec 5% | 79/143/173 | 0.43063 | 74/145/176 | 0.41815 | 5 / 5 |
| name spec 10% | 79/143/173 | 0.43063 | 74/145/176 | 0.41815 | 5 / 5 |
| name spec 10%, length gate | 72/141/174 | 0.41394 | 74/146/177 | 0.42088 | 0 / 0 |
| name spec 10%, identifier gate | 79/143/173 | 0.43063 | 74/145/176 | 0.41815 | 5 / 5 |
| name, narrow keywords | 79/143/173 | 0.43063 | 74/145/176 | 0.41815 | 5 / 5 |
| legacy def + name | 67/142/169 | 0.39740 | 66/139/168 | 0.38502 | 7 / 15 |

The name signal is neutral on CoSQA held-out (0 H@1, -0.0027 MRR, 5 wins and 5
losses). Its +7 H@1 on CoSQA dev did not replicate. The legacy flat bonus
reproduces the archived CoSQA loss exactly (-7 H@1, -0.0263 MRR).

The length gate makes the signal exactly inert on CoSQA: 0 of 500 queries reach
20 words, so the output is identical to the baseline (verified: 0 of 250
differences in both splits; the tool's firing count is 0 for `spec10_gate`).

## 6. Third confirmation set: the Semble bench

### 6.1 Setup

The other agent had imported Semble's public benchmark at
`/srv/projects/wispd/wg-semble-20261024/semble-bench/` before this point. I read
it only and wrote no file there. It has 63 repositories and 1251 annotated
queries with categories `architecture` (344), `semantic` (703), and `symbol`
(204). I used the first primary `relevant` path per query and scored
distinct-file Hit@1/5/10 and MRR@10, the same family of metrics as RepoQA and
CoSQA. This is not Semble's nDCG metric; it is a file-hit proxy. It was used
only as confirmation.

One operational note: the first pass aborted because the `tsx` tree-sitter
parser was not in the user cache. I ran one repository once with
`HF_HUB_OFFLINE` unset to fetch that parser into the shared user cache. No
model download occurred. The benchmark then ran fully offline.

### 6.2 Results (n = 1251)

| Config | H@1 | H@5 | H@10 | MRR | H@1 wins | H@1 losses |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline | 441 | 924 | 1045 | 0.51363 | - | - |
| legacy def 0.10 | 441 | 922 | 1055 | 0.51413 | 50 | 50 |
| name spec 5% | 447 | 934 | 1058 | 0.52110 | 34 | 28 |
| name spec 10% | 446 | 935 | 1059 | 0.52108 | 34 | 29 |
| name spec 10%, length gate | 441 | 924 | 1045 | 0.51363 | 0 | 0 |
| name spec 10%, identifier gate | 439 | 935 | 1057 | 0.51739 | 26 | 28 |
| name, narrow keywords | 472 | 953 | 1064 | 0.53629 | 40 | 9 |
| legacy def + name | 442 | 934 | 1056 | 0.51545 | 74 | 73 |

By category (MRR):

| Config | architecture (344) | semantic (703) | symbol (204) |
| --- | ---: | ---: | ---: |
| baseline | 0.4677 | 0.5321 | 0.5276 |
| legacy def 0.10 | 0.4571 | 0.5403 | 0.5202 |
| name spec 10% | 0.4748 | 0.5430 | 0.5234 |
| name, narrow keywords | 0.4801 | 0.5590 | 0.5527 |

The narrow name signal is the only variant with clear out-of-sample support:
+0.0227 MRR and 40 H@1 wins against 9 losses over 1251 queries. It helps all
three categories, including symbol queries. The broad variant gives +0.0075.
The flat legacy bonus is a wash (+0.0005) with 50 wins and 50 losses.

All 1251 Semble queries have 1-17 words (maximum 17), so the length gate is off
for the whole set. `spec10_gate` is byte-identical to the baseline there.

### 6.3 Why this does not rescue RepoQA

The third set shows that the name signal is useful for short, symbol-oriented
queries on real repositories. It does not show a RepoQA gain: RepoQA questions
are long descriptions of behaviour, and the function name is often not part of
the description. The third set also confirms that the length gate is a
dataset separator: it never fires outside RepoQA.

## 7. Launch q1-q8

The launch corpus is 63,786 chunks. File rank and line rank per question:

| Q | base file/line | def 0.10 | name spec 10% | name + length gate | def + name |
| --- | --- | --- | --- | --- | --- |
| q1 | 1 / 1 | 1 / 1 | 1 / 1 | 1 / 1 | 1 / 1 |
| q2 | 1 / - | 1 / - | 1 / - | 1 / - | 1 / - |
| q3 | absent / absent | absent / absent | absent / absent | absent / absent | absent / absent |
| q4 | 2 / - | 2 / - | 2 / - | 2 / - | 2 / - |
| q5 | 10 / 26 | 6 / 6 | 8 / 11 | 10 / 26 | 3 / 3 |
| q6 | 4 / 5 | 3 / 4 | 3 / 3 | 4 / 5 | 3 / 3 |
| q7 | 1 / 17 | 1 / 21 | 1 / 17 | 1 / 17 | 1 / 29 |
| q8 | 1 / 8 | 2 / 5 | 1 / 6 | 1 / 8 | 2 / 5 |

- **q3 does not change.** The answer chunk is absent from the candidate union in
  every arm. This is the known candidate-recall failure; weighting cannot fix
  it.
- **q5 improves.** The base is file 10 / line 26. The legacy bonus gives 6 / 6;
  the name signal gives 8 / 11; the combination gives 3 / 3.
- The length gate turns the signal off on q1-q8, because these questions have
  9-11 words. A gated bonus therefore leaves q5 at 10 / 26. This is the main
  practical weakness of the gate.

## 8. Cost and memory

Per-query signal cost, measured with `Instant` around the signal code and peak
RSS polled from a separate process. The warm query latency for the launch
corpus is about 25 ms (launch bench), so these are additions to that.

| Corpus | Chunks | Signal | Added per query | Index build | Resident index |
| --- | ---: | --- | ---: | ---: | ---: |
| pingora (RepoQA) | 500 | legacy def 0.10 | 3.08 ms | 0 | 0 |
| pingora | 500 | name signal | 0.09 ms | 7.6 ms | 140 KB |
| CoSQA | 6,319 | name signal | 0.19 ms | 24 ms | 617 KB |
| launch | 63,786 | legacy def 0.10 | 2.31 ms | 0 | 0 |
| launch | 63,786 | name signal | 0.20 ms | 647 ms | 9.0 MB |
| launch | 63,786 | legacy def + length gate (gate off) | 0.002 ms | 0 | 0 |

Peak RSS was 730 MiB for the baseline launch run and 747 MiB with the name
signal. The name index is 9.0 MB, so a peak rise of about 17 MiB is consistent
with build-time temporary allocations. The pingora and CoSQA runs showed
differences within noise (125 vs 115 MiB; 130 vs 128 MiB).

The legacy bonus is expensive because it runs the broad regex over every
candidate chunk on every query. The name signal is an order of magnitude
cheaper because its index is built once. If the legacy bonus were shipped, the
same precompute would remove its 2-3 ms cost: store the definition name tokens
at chunk build, as the name signal does, and the per-query cost falls to about
0.2 ms and the index to about 9 MB at 63.8k chunks.

## 9. Findings and recommendation

### 9.1 Findings

1. **A selective name-match signal does not improve RepoQA.** The dev gains
   (+6 H@1) did not replicate on held-out (-1 H@1, +0.0017 MRR). The paired
   H@1 record is 4 wins and 5 losses. This is noise, not a gain.
2. **The name signal is safe on CoSQA.** Held-out is 0 H@1, -0.0027 MRR, 5 wins
   and 5 losses. This is also noise, but it is not the -7 H@1 harm of the flat
   bonus. The old CoSQA regression came from the broad flat bonus, not from the
   idea of definition matching.
3. **The name signal works on an independent, larger set.** The narrow variant
   gives +0.0227 MRR and 40 wins against 9 losses on 1251 Semble queries. This
   is real support for a general symbol-aware feature, not for a RepoQA fix.
4. **The old flat bonus replicates on RepoQA.** +4 H@1 and +0.0111 MRR on
   held-out, 8 wins against 4 losses, and it reproduces the earlier report.
5. **A query-length gate removes the CoSQA harm but also removes the launch
   benefit.** RepoQA has 44+ words, CoSQA has 16 or fewer, and the launch
   questions have 9-11. The gate is a separator between the benchmark formats,
   not between "useful" and "harmful" queries.
6. **RRF and test/legacy penalties do not transfer to wg.** RRF lost 13 H@1 on
   dev. The test penalty lost 6 H@1 because some gold files are tests. Both are
   rejected.
7. **q3 is not a ranking problem.** No definition signal reaches it; it is
   absent from the candidate union.

### 9.2 Recommendation

**Do not ship a definition boost as the default.** It does not meet the bar on
RepoQA held-out, and the only configuration that does meet it (flat bonus plus
length gate) does not fire on the launch questions and is tuned to the gap
between two benchmark formats.

**Optional opt-in: symbol-aware name mode.** This is the only variant with
out-of-sample support. It is neutral on RepoQA and CoSQA held-out and positive
on the 63-repo Semble bench. Ship it only behind an explicit flag, and describe
it as symbol awareness, not as a RepoQA improvement.

**Do not ship the flat bonus plus length gate.** It produces the RepoQA number
(+4 H@1, +0.0111 MRR) and leaves CoSQA unchanged, but it is off for the launch
questions and for every query in both confirmation sets. It is a benchmark
artifact.

### 9.3 Exact algorithm for the opt-in name mode

If the team decides to ship an opt-in mode, use this algorithm. It is the
`narrow10` configuration from this study (`+0.0227` MRR on the third set).

Index build, once per corpus (in `Corpus::build` or the live index rebuild):

1. For each chunk, take the language from the path extension and match the
   narrow definition regexes in section 2.3.
2. For each defined name, produce word tokens: split camelCase and snake_case,
   lowercase, drop stopwords and 1-character tokens. Light stemming is optional
   and was neutral; leave it off.
3. Map each token to a `u32` id. Store `Vec<Vec<Vec<u32>>>` (chunk, name,
   tokens). Count document frequency per token id.
4. Store `idf(t) = ln(1 + (N - df + 0.5) / (df + 0.5))` and
   `idf_hi = idf(df = 1)`.

Query time, after the union and fused scores are built:

1. Map the query terms to token ids (the terms are already tokenized).
2. For each candidate chunk, set `best = 0`. For each name:
   - Let `matched` be the name tokens that occur in the query. Skip if empty.
   - Skip if no matched token has `df <= 0.05 * N` (specificity).
   - `coverage = sum(idf(matched)) / sum(idf(all name tokens))`.
   - `rarity = max(idf(matched)) / idf_hi`, capped at 1.
   - `best = max(best, coverage * rarity)`.
3. Add `0.10 * best` to the fused score.

Everything else stays the same: the 200+200 candidate union, the exact-identifier
tier, min-max relative fusion with `w = 0.25`, and the tie-break on BM25.

Measured cost: 0.2 ms per query at 63.8k chunks; 9.0 MB index; 0.65 s one-time
build. This breaks exact score parity.

Measured effect of this exact config: RepoQA held-out 202/271/283, MRR 0.76647
(+0.0008 against the baseline; 7 H@1 wins and 7 losses). CoSQA held-out
74/145/176, MRR 0.41815 (-0.0027; 5 wins and 5 losses). Semble bench
472/953/1064, MRR 0.53629 (+0.0227; 40 wins and 9 losses). It is a general
symbol-aware feature, not a RepoQA gain.

### 9.4 If the team still wants the RepoQA number

This combination is **not recommended**, but it is the only measured one that
meets the RepoQA requirement on the benchmarks:

1. Keep the current broad definition regex.
2. Precompute the defined name tokens per chunk at chunk build (do not run the
   regex per query; that costs 2-3 ms).
3. At query time, if the query has fewer than 20 words, add nothing.
4. Otherwise, add `0.10` to a chunk's fused score when any defined name token
   equals a query term.

Expected on the measured sets: RepoQA held-out +4 H@1 and +0.0111 MRR; CoSQA
held-out unchanged; q3 unchanged; q5 unchanged on the 9-11 word launch
questions. Do not describe this as a general improvement.

### 9.5 Per-query Hit@1 transitions

These are the queries that enter or leave file rank 1 against the baseline.

RepoQA held-out (n = 300):

- `def10` (8 gains, 4 losses)
  - gains: `cpp-oatpp__oatpp-04`, `go-projectdiscovery__nuclei-07`,
    `go-schollz__croc-07`, `java-microsoft__mssql-jdbc-05`,
    `python-psf__black-09`, `python-pyg-team__pytorch_geometric-08`,
    `python-python-poetry__poetry-01`, `rust-serde-rs__serde-02`
  - losses: `cpp-oatpp__oatpp-03`, `go-projectdiscovery__nuclei-05`,
    `rust-seanmonstar__warp-03`, `typescript-xenova__transformers.js-04`
- `spec10` (4 gains, 5 losses)
  - gains: `go-schollz__croc-07`, `python-pyg-team__pytorch_geometric-08`,
    `python-python-poetry__poetry-01`, `rust-seanmonstar__warp-07`
  - losses: `cpp-oatpp__oatpp-03`, `cpp-oatpp__oatpp-08`, `go-nsqio__nsq-06`,
    `rust-seanmonstar__warp-03`, `typescript-xenova__transformers.js-04`
- `narrow10` (7 gains, 7 losses)
  - gains: `go-projectdiscovery__nuclei-03`, `go-schollz__croc-07`,
    `python-pyg-team__pytorch_geometric-08`, `python-python-poetry__poetry-01`,
    `rust-seanmonstar__warp-07`, `rust-serde-rs__serde-02`,
    `typescript-langchain-ai__langchainjs-04`
  - losses: `cpp-oatpp__oatpp-03`, `cpp-oatpp__oatpp-08`,
    `go-projectdiscovery__nuclei-00`, `go-projectdiscovery__nuclei-08`,
    `rust-seanmonstar__warp-03`, `typescript-langchain-ai__langchainjs-05`,
    `typescript-mrdoob__three.js-00`

CoSQA held-out (n = 250):

- `def10` (3 gains, 10 losses)
  - gains: `cosqa-train-4764`, `cosqa-train-5794`, `cosqa-train-13226`
  - losses: `cosqa-train-13151`, `cosqa-train-4543`, `cosqa-train-11636`,
    `cosqa-train-9880`, `cosqa-train-9187`, `cosqa-train-12023`,
    `cosqa-train-14131`, `cosqa-train-10773`, `cosqa-train-12970`,
    `cosqa-train-10587`
- `spec10` and `narrow10` (5 gains, 5 losses)
  - gains: `cosqa-train-18559`, `cosqa-train-4764`, `cosqa-train-12362`,
    `cosqa-train-13226`, `cosqa-train-14671`
  - losses: `cosqa-train-13151`, `cosqa-train-10854`, `cosqa-train-19204`,
    `cosqa-train-14131`, `cosqa-train-10773`

The full per-query rank for every config and every query is in
`results/*/*/rows.jsonl` on the VPS.

## 10. Limits

- One model (`potion-code-16M-v2`) and one machine. The file-hit metric is not
  full answer coverage.
- RepoQA held-out is 300 queries. A paired H@1 change of 8 wins against 4
  losses is small. The name signal's 4 against 5 is clearly not a gain.
- The CoSQA regression and the RepoQA gain of the flat bonus are each about
  1-2 points of H@1. Both replicate the earlier report.
- The length gate threshold sits in a gap (16 vs 44 words). A future corpus with
  18-word queries would get the harm without the benefit.
- The third set uses file-hit metrics on the first primary relevant path, not
  Semble's nDCG over all relevant paths. It is a proxy, not a reproduction of
  Semble's score.
- The third set is dominated by short queries (1-17 words), so it does not test
  the long-description regime at all.
- The Semble source was read at one commit. Its behaviour may change.
- The development machine is shared with another agent. All runs used
  `RAYON_NUM_THREADS=2`; latency was not the focus.

## Receipts

VPS paths:

- Source and builds: `/srv/projects/wispd/wg-defboost-20261024/` (`src/`,
  `semble-src/`). The build `target/` directory was deleted at the end, per the
  VPS disk rule.
- Results: `/srv/projects/wispd/wg-defboost-20261024/results/` (`verify/`,
  `verify2/`, `dev/`, `dev2/`, `dev3/`, `heldout/`, `cosqa/`, `launch/`,
  `semble-bench/`, `defgate/`).
- Logs: `/srv/logs/wispd/wg-defboost-20261024/`.
- The Semble bench was read at
  `/srv/projects/wispd/wg-semble-20261024/semble-bench/` only. No file there
  was changed.
- All task-owned daemons were stopped. The tool does not start a daemon.

Local files:

- `bench/WG_DEFBOOST_REPORT.md` (this file)
- `bench/WG_QUALITY_TOOL/src/main.rs` (extended, baseline unchanged)
- `bench/WG_DEFBOOST_OPTIONS.py`, `WG_DEFBOOST_COSQA.py`,
  `WG_DEFBOOST_LAUNCH.py`, `WG_DEFBOOST_SEMBLE.py`, `WG_DEFBOOST_COST.py`
- `bench/WG_DEFBOOST_SUMMARY.py`, `WG_DEFBOOST_FINAL.py`,
  `WG_DEFBOOST_PAIRED.py`, `WG_DEFBOOST_RANKS.py`, `WG_DEFBOOST_APPENDIX.py`,
  `WG_DEFBOOST_COMPARE.py`, `WG_DEFBOOST_SHAPE.py`, `WG_DEFBOOST_DUMP.py`,
  `WG_DEFBOOST_GATECHECK.py`, `WG_DEFBOOST_SYMBOLGATE.py`,
  `WG_DEFBOOST_LAUNCH_CHECK.py`
- `bench/WG_DEFBOOST_DRIVER.sh`, `WG_DEFBOOST_COST_RUN.sh`,
  `WG_DEFBOOST_PREWARM.sh`
- Config files: `WG_DEFBOOST_BASE_CONFIGS.json`,
  `WG_DEFBOOST_DIAG_CONFIGS.json`, `WG_DEFBOOST_DEV_CONFIGS.json`,
  `WG_DEFBOOST_DEV2_CONFIGS.json`, `WG_DEFBOOST_DEFGATE_CONFIGS.json`,
  `WG_DEFBOOST_HELDOUT_CONFIGS.json` (the predeclared list)

No commit was made.
