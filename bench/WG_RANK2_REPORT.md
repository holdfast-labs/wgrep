# Rank-2: closing the Semble NDCG gap without losing RepoQA or CoSQA

Date: 2026-10-24. Analysis only. No product code change. New local files use
the `bench/WG_RANK2_` prefix or live under `bench/WG_QUALITY_TOOL/`. No commit.

## 0. Answer first

The gap closes from **0.7334 to 0.8222** macro NDCG@10 on Semble's 63-repo
benchmark (Semble: 0.8539). That is **74% of the gap**, with **no loss** on
RepoQA or CoSQA held-out file/snippet metrics. One config does it:

```
sat 0.5, sym 1.0, stem 0.6, coh 0.05, sym_stem 1.5, narrow definitions
```

| Set | baseline | r1 | Semble | Note |
| --- | ---: | ---: | ---: | --- |
| Semble 63 repos, macro NDCG@10 | 0.7334 | **0.8222** | 0.8539 | +0.0888; 30 of 31 held-out repos improve |
| Semble symbol category | 0.7701 | **0.9384** | 0.947 | +0.168 |
| Semble architecture category | 0.6997 | **0.7565** | 0.808 | +0.057 |
| Semble semantic category | 0.7384 | **0.8210** | 0.847 | +0.083 |
| RepoQA held-out file H@1/5/10 | 202/270/283 | **202/270/286** | - | +0 H@1, +3 H@10, MRR +0.0013 |
| CoSQA held-out H@1/5/10 | 74/146/177 | **74/146/177** | - | exact identity |
| Launch q5 file/line | 10 / 26 | **3 / 21** | - | q4 2 -> 1; q6 4 -> 3 |
| Added cost, top 10, 63.8k chunks | - | **+0.85 ms/query** | - | index 5.9 MB, build 474 ms |

**Recommendation: ship as an opt-in ranking mode, not as the default.** It
breaks exact score parity, and it has one real secondary cost: RepoQA line@5
falls from 227 to 199 because the diversity rule reorders chunks inside the
correct file (the file itself stays in the top 5). The default should keep
parity. The exact algorithm is in section 8.

## 1. Baseline reproduction

The tool's base config must reproduce wg's 0.7331 macro NDCG@10. It does, in
two independent passes.

| Run | Repos | Queries | Macro NDCG@10 | Query-weighted |
| --- | ---: | ---: | ---: | ---: |
| Semble dev split | 32 | 630 | 0.7787 | 0.7784 |
| Semble held-out split | 31 | 621 | 0.6867 | 0.6868 |
| Combined, 63 repos | 63 | 1,251 | **0.7334** | 0.7329 |
| `WG_SEMBLE_THEIRSET.py` (wg CLI) | 63 | 1,251 | 0.7331 | 0.7326 |

The held-out quality run was executed twice: once with the first selection
implementation and once with the optimized tail. The two per-query JSON files
are byte-identical, so the optimization changed cost only, not ranking. The
table uses the final binary.

The 0.0003 difference is the `g` (flask) query: wg's CLI exits with "query has
no searchable words", and the tool ranks that query instead. This matches the
other report's note that the query is worth about 0.0006 of the macro.

Identity checks after every tool change:

| Check | Result |
| --- | --- |
| RepoQA dev base vs archived tool rows | 300 of 300 identical |
| RepoQA held-out base vs archived tool rows | 300 of 300 identical |
| Launch q1-q8 base file rank vs archived CLI JSON | 8 of 8 identical |
| Semble zod base NDCG@10 | 0.2420 (other report: 0.242) |
| Re-run after the selection optimization | per-query JSON byte-identical on dev and held-out |

The metric is Semble's own: `benchmarks.metrics.ndcg_at_k` over the 1-based
ranks of the first chunk that covers each `task.all_relevant` target, using
`benchmarks.data.target_matches_location` (suffix path match and span overlap).
No file dedup. Aggregation is per-repo mean, then unweighted mean over repos,
exactly as `run_benchmark`.

## 2. Split

63 repos, stratified by language, seed 20261024. For each language: sorted
names, a seeded shuffle, then alternate which half gets the extra repo.
Python (9 repos) splits 5/4; every other language (3 repos) splits 2/1 or 1/2
so the totals stay balanced.

| Split | Repos | Queries |
| --- | ---: | ---: |
| dev | 32 | 630 |
| held-out | 31 | 621 |

The list was written to `/srv/projects/wispd/wg-rank2-20261024/split.json`
before any tuning; the local copy is `bench/WG_RANK2_SPLIT.json` (SHA-256
`d21ba7d4da0f4e40a101aa4ad140aeee90058b571125a4b1e935c4bbe5bbcdef`). The
problem repo zod, and the other large-gap repos, are in held-out.

## 3. Signals

Five signals were added to the analysis tool. Each is off by default. The
baseline path is unchanged.

1. **Per-file saturation (`sat`)**: during the greedy selection of the top
   `sel_top` positions (default 10), a chunk's effective score is
   `score * sat^count`, where `count` is the number of chunks already selected
   from that file. Semble uses 0.5.
2. **Per-file cap (`cap`)**: skip a chunk once its file has `cap` chunks in the
   top `sel_top` positions. Off by default.
3. **Whole-query symbol mode (`sym`, `sym_w`, `sym_stem`, `sym_soft`)**: apply
   Semble's `is_symbol_query` regex to the whole query. When it matches, extract
   the last namespace segment, boost chunks whose definition-name set contains
   that symbol by `sym * max_score`, and multiply by `sym_stem` when the file
   stem matches the symbol. `sym_w` can override the BM25 weight; `sym_soft`
   replaces the hard exact tier with an additive boost.
4. **NL file-stem/path boost (`stem`)**: Semble's `_boost_stem_matches`. Query
   words (3+ characters, no stopwords) are matched against the file stem and
   parent directory name. Exact or 3-character prefix; `ratio = matches /
   keywords`; fire only when `ratio >= 0.10`; add `stem * max_score * ratio`.
   Skipped for symbol queries.
5. **Multi-chunk file coherence (`coh`)**: Semble's `boost_multi_chunk_files`.
   Add `coh * max_score * file_sum / max_file_sum` to each file's best-scoring
   chunk, where `file_sum` is over the union.

Definition names come from the narrow per-language regex set from round 1.
The index is precomputed once per corpus: token ids, document frequency, IDF,
and the normalised full name per chunk. The symbol boost uses the full-name
exact match, not the IDF score.

Selection: the exact-identifier tier remains a hard tier. Within it, the top
`sel_top` positions are chosen greedily with saturation and cap. After that
window the effective score is the raw score, so the remaining positions use a
sorted tail. This is equivalent to a continuous greedy pass and much cheaper
for `--top 100`.

## 4. Semble dev sweeps

n = 630 queries, 32 repos. Macro NDCG@10, then per category.

### 4.1 Single signals

| Config | Macro | Arch | Sem | Sym |
| --- | ---: | ---: | ---: | ---: |
| base | 0.7787 | 0.7411 | 0.7874 | 0.8116 |
| sat 0.5 | 0.7992 | 0.7725 | 0.8079 | 0.8137 |
| sat 0.7 | 0.7992 | 0.7718 | 0.8078 | 0.8157 |
| sat 0.85 | 0.7961 | 0.7666 | 0.8049 | 0.8162 |
| cap 1 | 0.7988 | 0.7696 | 0.8071 | 0.8192 |
| cap 2 | 0.7864 | 0.7488 | 0.7977 | 0.8112 |
| cap 3 | 0.7834 | 0.7442 | 0.7944 | 0.8116 |
| sat 0.7 + cap 2 | 0.7998 | 0.7718 | 0.8088 | 0.8157 |
| sat 0.5 + cap 2 | 0.8003 | 0.7725 | 0.8090 | 0.8166 |
| sym 0.5 / 1.0 / 2.0 | 0.7973 | 0.7411 | 0.7874 | **0.9310** |
| sym_w 0.5 | 0.7791 | 0.7411 | 0.7874 | 0.8142 |
| sym_w 0.7 | 0.7735 | 0.7411 | 0.7874 | 0.7821 |
| sym_soft 0.5 / 1.0 | 0.7792 / 0.7787 | 0.7411 | 0.7874 | 0.8150 / 0.8116 |
| stem 0.1 | 0.7891 | 0.7413 | 0.8052 | 0.8144 |
| stem 0.3 | 0.7995 | 0.7481 | 0.8190 | 0.8201 |
| stem 0.5 | 0.8052 | 0.7390 | 0.8328 | 0.8243 |
| coh 0.10 | 0.7936 | 0.7624 | 0.7964 | 0.8392 |
| coh 0.20 | 0.7984 | 0.7755 | 0.8000 | 0.8338 |
| coh 0.40 | 0.7840 | 0.7719 | 0.7796 | 0.8239 |
| narrow10 (round 1) | 0.7842 | 0.7421 | 0.7920 | 0.8274 |

Observations:

- Saturation and cap are the diversity levers. They lift architecture by about
  0.03 and semantic by 0.02, and symbol by only 0.002-0.008.
- The symbol definition boost is the symbol lever. It lifts the symbol category
  from 0.8116 to 0.9310 (+0.119) with no change to the other categories. The
  weight does not matter (0.5, 1.0, 2.0 give the same result) because every
  defining chunk gets the same boost; only the move above non-defining chunks
  matters. This reproduces Semble's `_boost_symbol_definitions` idea without
  the 3.0 multiplier.
- Leaning on BM25 for symbol queries (`sym_w`) hurts. `sym_soft` (removing the
  hard exact tier) does nothing. Both were dropped.
- The NL stem boost is the strongest single signal for the semantic category
  (+0.045 at 0.5). It also helps symbol slightly. It is neutral for
  architecture.
- Coherence helps architecture most, but a large value hurts the rest and, as
  section 5 shows, hurts RepoQA.

### 4.2 Combinations

| Config | Macro | Arch | Sem | Sym |
| --- | ---: | ---: | ---: | ---: |
| sat .5 + stem .5 | 0.8263 | 0.7720 | 0.8542 | 0.8252 |
| sat .5 + sym 1 | 0.8182 | 0.7725 | 0.8079 | 0.9354 |
| sat .5 + coh .2 | 0.8099 | 0.7918 | 0.8122 | 0.8346 |
| sym 1 + stem .5 | 0.8238 | 0.7390 | 0.8328 | 0.9437 |
| stem .5 + coh .2 | 0.8200 | 0.7828 | 0.8314 | 0.8481 |
| sat .5 + sym 1 + stem .5 | 0.8453 | 0.7720 | 0.8542 | 0.9468 |
| sat .5 + sym 1 + coh .2 | 0.8254 | 0.7918 | 0.8122 | 0.9316 |
| sym 1 + stem .5 + coh .2 | 0.8349 | 0.7828 | 0.8314 | 0.9422 |
| sat .5 + sym 1 + stem .5 + coh .2 | **0.8512** | 0.8058 | 0.8483 | 0.9453 |
| same + cap 1 | 0.8496 | 0.8030 | 0.8471 | 0.9444 |

The four signals are close to additive. The full four-signal config reaches
0.8512 on dev, within 0.003 of Semble's published 0.854 (which is over all 63
repos, so this is not a like-for-like comparison).

### 4.3 Refinement and the RepoQA guardrail

Coherence is the one signal that hurts RepoQA. Tuning used Semble dev only.
As a guardrail, the candidates were also checked on RepoQA **dev** (not
held-out) for file retrieval, and on CoSQA dev for inertness. This is
disclosed: RepoQA dev influenced the choice among Semble-dev winners. RepoQA
held-out was still run only once, after predeclaration.

RepoQA dev (n = 300), file metrics:

| Config | H@1 | H@5 | H@10 | MRR | W/L | line@5 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| base | 195 | 268 | 283 | 0.74903 | - | 228 |
| sym + stem (no sat/coh) | 195 | 268 | 283 | 0.74949 | 2/1 | 228 |
| sat + sym + stem (no coh) | 195 | 268 | 283 | 0.74949 | 2/1 | 201 |
| sat + sym + stem + coh 0.05 | 196 | 270 | 283 | 0.75585 | 23/10 | 201 |
| ... + coh 0.10 | 193 | 271 | 283 | 0.75052 | 27/21 | 201 |
| ... + coh 0.15 | 189 | 271 | 284 | 0.74533 | 34/30 | 201 |
| ... + coh 0.20 | 180 | 268 | 285 | 0.72974 | 36/48 | 199 |

Coherence 0.05 is the largest value that is safe on RepoQA dev; it is also
positive there (+1 H@1, +0.0068 MRR). Coherence 0.10 is marginal, and 0.15-0.20
are clear losses. The Dev-Semble gain from 0.05 to 0.20 is only about 0.003
macro, so 0.05 is the right trade.

Stem 0.6 and saturation 0.5 were selected the same way: stem 0.6 is at or above
stem 0.5 on every Semble category, and saturation 0.5 and 0.6 are within 0.0002
of each other. The final dev table for the predeclared list:

| Config | Macro | Arch | Sem | Sym |
| --- | ---: | ---: | ---: | ---: |
| base | 0.7787 | 0.7411 | 0.7874 | 0.8116 |
| r1 = sat .5, sym 1, stem .6, coh .05 | 0.8524 | 0.7871 | 0.8582 | 0.9518 |
| r2 = r1 with coh .08 | 0.8526 | 0.7857 | 0.8594 | 0.9518 |
| r3 = sat .5, cap 2, sym 1, stem .5, coh .05 | 0.8519 | 0.7876 | 0.8580 | 0.9481 |
| r4 = sat .5, sym 1, stem .6, no coh | 0.8467 | 0.7724 | 0.8556 | 0.9505 |
| r5 = sym 1, stem .6, no sat | 0.8235 | 0.7368 | 0.8324 | 0.9474 |

CoSQA dev is exactly unchanged for every config (0 of 250 differences): a
CoSQA corpus has one chunk per file, so saturation, cap and coherence cannot
fire, the file names are numeric so the stem boost cannot fire, and the queries
are not whole-query symbols.

## 5. Predeclared held-out

The list was fixed in `bench/WG_RANK2_HELDOUT_CONFIGS.json` (SHA-256
`43be6f2d80a9b00f10d1ef91c2bba933bbff89d0c1de8ea62b50c6b4b85e44c4`) and run
once on each set.

### 5.1 Semble held-out (31 repos, 621 queries)

| Config | Macro | Query-weighted | Arch | Sem | Sym | Repos W/L | Query W/L/T |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| base | 0.6867 | 0.6868 | 0.6569 | 0.6884 | 0.7303 | - | - |
| **r1** | **0.7912** | 0.7910 | 0.7248 | 0.7830 | 0.9256 | 30/1 | 424/72/753 |
| r2 | 0.7978 | 0.7977 | 0.7350 | 0.7902 | 0.9250 | 31/0 | 435/65/749 |
| r3 | 0.7879 | 0.7878 | 0.7259 | 0.7766 | 0.9256 | 31/0 | 426/63/760 |
| r4 | 0.7837 | 0.7835 | 0.7102 | 0.7746 | 0.9327 | 30/1 | 402/78/769 |
| r5 | 0.7479 | 0.7477 | 0.6763 | 0.7272 | 0.9327 | 27/4 | 282/119/848 |

Combined 63-repo macro: base 0.7335, r1 0.8222, r2 0.8257, r3 0.8204,
r4 0.8157, r5 0.7863. Semble's own score is 0.8539.

The single r1 regression is abseil-cpp, by 0.0003. The largest
gains are the repos that the other report named as wg's worst: zod 0.2420 ->
0.4249 (+0.183), newtonsoft-json 0.4892 -> 0.7532 (+0.264), laravel-framework
0.4488 -> 0.6556 (+0.207), gson 0.6090 -> 0.7751 (+0.166), rails 0.5899 ->
0.7653 (+0.175). The zod failure mode (many chunks of `v3/types.ts`) is mostly fixed
by saturation: without it zod is 0.3001; with it 0.4249.

r2 scores 0.0035 higher on Semble but loses 2 H@1 on RepoQA held-out (see
5.2). r1 is the recommended config because it passes the hard rule with no
ambiguity.

### 5.2 RepoQA held-out (30 repos, 300 queries)

| Config | File H@1/5/10 | MRR | H@1 W/L | Top-10 rank moves | Line H@1/5/10 |
| --- | --- | ---: | ---: | ---: | --- |
| base | 202/270/283 | 0.76569 | - | - | 156/227/245 |
| **r1** | **202/270/286** | **0.76699** | 4/4 | 17/17 | 155/199/214 |
| r2 | 200/270/285 | 0.76502 | 7/9 | 26/25 | 154/199/213 |
| r3 | 202/270/286 | 0.76699 | 4/4 | 17/17 | 155/199/212 |
| r4 | 203/270/283 | 0.76791 | 1/0 | 1/0 | 157/200/213 |
| r5 | 203/270/283 | 0.76791 | 1/0 | 1/0 | 157/227/245 |

r1 passes the hard rule: +0 H@1, +3 H@10, +0.0013 MRR. Its 4 H@1
gains and 4 H@1 losses cancel. The `top-10 rank moves = 17/17` means 17 queries
moved up and 17 moved down inside the top 10; the headline counts do not move.

The disclosed cost: line-level ranking degrades (line@5 227 -> 199, line@10
245 -> 214). Saturation moves a file's second and later chunks down, and a
RepoQA answer line often sits in a later chunk of the correct file. The file
itself stays in the top 5 for the same 270 queries. r5, without saturation,
keeps line@5 at 227 and even gains 1 file H@1, but its Semble macro is 0.7863
against 0.8222 for r1. The 0.036 NDCG difference is the price of line order.

### 5.3 CoSQA held-out (250 queries)

Every r1/r3/r4/r5 result is byte-identical to the baseline: H@1/5/10
74/146/177, MRR 0.42088. r2 loses 0.0001 MRR on one query. This is expected:
one chunk per file means the new selection rules are inert, the file names are
numeric, and the queries are not whole-query symbols.

### 5.4 Launch q1-q8

| Q | base file/line | r1 file/line |
| --- | --- | --- |
| q1 | 1 / 1 | 1 / 1 |
| q2 | 1 / - | 1 / - |
| q3 | absent / absent | **6 / absent** |
| q4 | 2 / - | **1 / -** |
| q5 | 10 / 26 | **3 / 21** |
| q6 | 4 / 5 | **3 / 3** |
| q7 | 1 / 17 | 1 / 35 |
| q8 | 1 / 8 | 1 / 12 |

- q3's file enters the top 10 (rank 6) even though the answer chunk is still
  not in the candidate union, so the line remains absent. The file improvement
  comes from a different chunk of the same file.
- q5 improves from file 10 / line 26 to file 3 / line 21.
- q4 improves to file rank 1.
- q7 and q8 keep file rank 1 but their line ranks get worse (17 -> 35, 8 -> 12).
  This is the same within-file reordering cost as RepoQA line@5.

## 6. Cost and memory

Measured with the tool's `Instant` timer around the score and selection block
(`sel`) and peak RSS polled from a separate process. The warm wg query is about
25 ms.

| Corpus | Chunks | Top | base `sel` | r1 `sel` | Added | Index | Build |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| tokio (Semble) | 4,611 | 10 | 125 us | 616 us | +0.49 ms | 681 KB | 52 ms |
| pingora (RepoQA) | 500 | 100 | 95 us | 547 us | +0.45 ms | 95 KB | 7 ms |
| launch | 63,786 | 10 | 122 us | 970 us | +0.85 ms | 5.87 MB | 474 ms |

Peak RSS: launch base 741 MiB, r1 737 MiB (noise; the index is 5.87 MB);
tokio base 149 MiB, r1 145 MiB. The reliable memory figure is the index:
5.87 MB at 63,786 chunks (26,544 chunks have definition names), 42% of the
chunks. There is no per-query allocation growth.

The cost is dominated by the stem/symbol scans and the greedy window, not by
the index lookup. A production implementation would precompute the definition
names at chunk build (as this tool does) and reuse the existing `counts` word
map where possible.

## 7. Findings

1. **Per-file saturation is the main NDCG lever.** It alone adds +0.020 macro
   and fixes the zod failure mode. A hard cap works too but is not better.
2. **The whole-query symbol definition boost fixes the symbol category.** It
   adds +0.119 on dev and +0.195 on held-out with no effect on other
   categories. wg's existing hard exact tier orders usage sites but not the
   definition; this boost fixes that.
3. **The NL stem/path boost is the main semantic lever.** +0.045 on the
   semantic category alone.
4. **Coherence helps architecture but hurts RepoQA above 0.05.** 0.05 is the
   largest safe value.
5. **RRF, BM25-leaning symbol weights, and soft exact tiers do not help.** RRF
   was rejected in round 1. `sym_w` and `sym_soft` were neutral or negative
   here.
6. **The combined config generalizes.** Held-out is 30/31 repos improved and
   424 query wins against 72 losses. The gain is not concentrated in one
   language or repo.
7. **The cost is line order inside the correct file.** File retrieval is
   unchanged or better on RepoQA; the line rank within the file gets worse
   because of saturation. This is the one real trade-off.

## 8. Recommendation and exact algorithm

**Recommendation: ship as an opt-in ranking mode.** Do not make it the default.
Reasons:

- It changes the top-k order for every query, so it breaks exact score parity.
- It lowers RepoQA line@5 by 28 queries. File retrieval is safe, but an agent
  that needs an exact line is affected.
- The gain is on Semble's own benchmark; RepoQA and CoSQA are neutral. The
  default should remain the parity mode that wins on RepoQA/CoSQA.

If the team accepts the line-order cost, r1 is a candidate for the default of
a future major version after a larger validation. The exact algorithm:

### 8.1 Index build (once per corpus)

1. For each chunk, take the language from the path extension. Match the narrow
   per-language definition regexes from `bench/WG_DEFBOOST_REPORT.md` section
   2.3. Do not include `let`/`var`/`const` except top-level JS/TS.
2. For each defined name, store the normalised full name (lowercase, remove
   `_`). Optionally also store its word tokens (camelCase and snake_case split,
   stopwords and 1-character tokens removed) with token ids, document frequency
   and `idf(t) = ln(1 + (N - df + 0.5) / (df + 0.5))`. The recommended config
   uses only the normalised full name; token ids and IDF are needed only for
   the optional round-1 name-score mode.
3. Keep the normalised full-name set for the symbol boost.

The measured index (5.87 MB at 63,786 chunks) includes the token ids and IDF.
An r1-only implementation needs only the full-name strings and is smaller.

### 8.2 Query time (`rank_impl`)

Let `B[i]` be the current fused score `0.25 * bm25_relative[i] +
0.75 * semantic_relative[i]` for each union row. Let `M = max(B)`.

1. **Symbol query test.** Apply Semble's regex to the trimmed query:
   `^(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+|_[A-Za-z0-9_]*|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*|[A-Z][A-Za-z0-9]*)$`.
   If it matches, extract the last segment after `::`, `\`, `->`, or `.` and
   normalise it.
2. **Coherence (0.05).** For each file in the union, sum `max(B, 0)` and find
   the chunk with the largest `B`. Add `0.05 * M * file_sum / max_file_sum` to
   that chunk.
3. **Symbol boost (1.0, stem 1.5).** If the query is a symbol, add
   `1.0 * M` to every union chunk whose definition-name set contains the
   normalised symbol, multiplied by 1.5 when the file stem matches the symbol
   (exact, underscore-normalised, or plural-tolerant).
4. **Stem boost (0.6).** If the query is not a symbol, take query words of 3+
   characters that are not stopwords. For each chunk, match them against the
   file stem and parent directory name (exact or 3-character prefix). Let
   `ratio = matches / keywords`; if `ratio >= 0.10`, add `0.6 * M * ratio`.
5. **Score.** `score = B + coherence + symbol + stem` (plus the existing path,
   definition and penalty terms if those modes are on).
6. **Selection (top 10).** Keep the hard exact tier. For positions 1 to 10,
   greedily pick the best remaining chunk by
   `(exact, score * 0.5^file_count, bm25)`, skipping a chunk that is contained
   in an already selected chunk of the same file. Increment the file count.
   After position 10, append the remaining chunks by `(exact, score, bm25)`
   with the same containment rule.
7. **`--top` above 10.** The saturation window stays at 10. Positions 11+ use
   the raw score. This is what keeps RepoQA/CoSQA file metrics safe.

The exact config is:

```json
{"sat": 0.5, "sel_top": 10, "cap": 0, "coh": 0.05,
 "stem": 0.6, "sym": 1.0, "sym_stem": 1.5, "sym_w": null, "sym_soft": 0}
```

Measured effect: Semble macro NDCG@10 0.8222 (from 0.7334; Semble 0.8539);
RepoQA held-out 202/270/286, MRR 0.76699 (+0 H@1, +3 H@10, +0.0013);
CoSQA held-out identical; launch q5 file 3 / line 21; q4 file 1; q3 file 6
but its answer line still absent.

## Appendix A. Held-out repos (31)

| Language | Repos |
| --- | --- |
| bash | nvm |
| c | libuv, redis |
| cpp | abseil-cpp |
| csharp | dapper, newtonsoft-json |
| elixir | plug |
| go | cobra, gin |
| haskell | xmonad |
| java | commons-lang, gson |
| javascript | redux |
| kotlin | kotlinx-coroutines, ktor |
| lua | lazy.nvim |
| php | guzzle, laravel-framework |
| python | aiohttp, fastapi, httpx, pydantic |
| ruby | rack, rails |
| rust | serde |
| scala | cats, http4s |
| swift | vapor |
| typescript | trpc, zod |
| zig | zls |

The dev split is the complement in `bench/WG_RANK2_SPLIT.json`.

## 9. Limits

- The Semble benchmark was written and labelled by Semble. It is a home-field
  test, even though the metric is theirs and the split is ours.
- The split is one seed. The held-out result (30/31 repos, 424/72 queries) is
  strong, but a second seed would be better.
- The line@5/10 regression on RepoQA is real and is not measured by the
  NDCG metric. It is the main reason for the opt-in recommendation.
- The coherence window applies to all candidates, not only the top 10. A
  windowed variant was not tested; coh 0.05 is small enough to be safe.
- A gentler saturation that keeps the first two chunks of a file at full score
  and decays from the third may reduce the line@5 cost while keeping most of
  the NDCG gain. It was not tested; it is the first thing to try if the line
  cost blocks adoption.
- The Semble metric uses the first chunk that covers each target and counts
  all primary and secondary targets. A chunk that covers two targets at the
  same rank cannot reach NDCG 1.0; both tools share this rule.
- The index cost is measured on the launch corpus and one Semble repo. Very
  large repos scale linearly with the number of chunks that have definitions
  (42% here).
- The benchmark ran on a shared VPS with `RAYON_NUM_THREADS=4`; latency was
  measured separately from the quality runs.

## Receipts

Local files:

- `bench/WG_RANK2_REPORT.md` (this file)
- `bench/WG_QUALITY_TOOL/src/main.rs` (extended, baseline unchanged)
- `bench/WG_RANK2_SPLIT.py`, `bench/WG_RANK2_SPLIT.json`
- `bench/WG_RANK2_NDCG.py`, `WG_RANK2_COST.py`, `WG_RANK2_DEVDUMP.py`,
  `WG_RANK2_PAIRED.py`, `WG_RANK2_REPOS.py`
- `bench/WG_RANK2_DRIVER.sh`
- Config files: `WG_RANK2_BASE_CONFIGS.json`, `WG_RANK2_DEV_CONFIGS.json`,
  `WG_RANK2_DEV2_CONFIGS.json`, `WG_RANK2_DEV3_CONFIGS.json`,
  `WG_RANK2_DEV4_CONFIGS.json`, `WG_RANK2_DEV5_CONFIGS.json`,
  `WG_RANK2_GUARD_CONFIGS.json`, `WG_RANK2_GUARD2_CONFIGS.json`,
  `WG_RANK2_GUARD3_CONFIGS.json`, `WG_RANK2_HELDOUT_CONFIGS.json` (the
  predeclared list)

VPS paths (`/srv/projects/wispd/wg-rank2-20261024/`):

- `src/` (synced tree), `target/` (deleted at the end)
- `split.json`
- `results/base-dev`, `base-heldout`, `dev`, `dev2`, `dev3`, `dev4`, `dev5`,
  `devpre`, `devpre2`, `heldout`, `heldout2`, `repoqa-guard*`,
  `repoqa-heldout2`, `cosqa-guard2`, `cosqa-heldout2`, `launch2`
- Logs: `/srv/logs/wispd/wg-rank2-20261024/`
- The Semble benchmark was read only at
  `/srv/projects/wispd/wg-semble-20261024/semble-bench/`; no file there was
  changed.
- All task-owned daemons were stopped. The tool does not start a daemon.

No commit was made.
