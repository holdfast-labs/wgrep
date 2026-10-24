# Rank-3: the RepoQA line@5 blocker is not fixed

Date: 2026-10-24. Analysis only. No product code change. New local files use
the `bench/WG_RANK3_` prefix or live under `bench/WG_QUALITY_TOOL/`. No commit.

## 0. Verdict

**The acceptance target is not met. Do not make r1 the default.**

Saturation is the only cause of the line@5 loss. Two fixes were tried: decay
from the 3rd chunk, and a two-level decay (gentle for the 2nd chunk, strong from
the 3rd). The best second-look result is t90: Semble held-out 0.7835 (passes the
0.7812 target) but RepoQA held-out line@5 **214** (target 222). t95 reaches
line@5 218 and misses the Semble target at 0.7805. Both keep the RepoQA and
CoSQA file metrics.

The two targets are in direct tension: keeping the 2nd chunk of a file at full
score preserves line@5 but gives up about 0.008-0.01 held-out NDCG; decaying the
2nd chunk enough for NDCG costs 9-13 line@5 queries. r1 stays opt-in.
Details and the exact t90 algorithm are below. Section 8 adds a follow-up run:
`no sat` (saturation off) keeps line@5 at 227 and is the candidate default if
score parity is relaxed (Semble held-out 0.7609).

## 1. Diagnosis (RepoQA dev, n = 300)

| Config | File H@1/5/10 | MRR | line@1/5/10 | Top-10 moves |
| --- | --- | ---: | --- | ---: |
| base | 195/268/283 | 0.74903 | 150/228/248 | - |
| r1 (all four) | 196/270/283 | 0.75585 | 149/201/210 | 23/10 |
| sat only | 195/268/283 | 0.74903 | 150/201/211 | 0/0 |
| coh only | 196/269/282 | 0.75543 | 149/228/248 | 21/12 |
| stem only | 194/268/283 | 0.74783 | 149/229/249 | 2/2 |
| sym only | 195/268/283 | 0.74903 | 150/228/248 | 0/0 |
| no sat | 196/270/283 | 0.75585 | 149/228/250 | 23/10 |
| no coh | 194/268/283 | 0.74783 | 149/201/211 | 2/2 |
| no stem | 196/269/282 | 0.75543 | 149/201/209 | 21/12 |
| no sym | 196/270/283 | 0.75585 | 149/201/210 | 23/10 |

Facts:

- `sat only` reproduces the entire failure: line@5 228 -> 201, and file metrics
  are byte-identical to base. Saturation only changes the order inside a file.
- `coh only` raises file H@1 and leaves line@5 at 228.
- `no sat` (sym + stem + coh) keeps line@5 at 228 and the file gain
  (+1 H@1, +0.0068 MRR).
- `sym` does nothing on RepoQA (no query is a whole-query symbol).
- `stem` is the only other mover, and it is slightly negative on RepoQA dev.

The mechanism: a RepoQA answer line often sits in the 2nd chunk of the correct
file. Saturation demotes that chunk relative to other files, which is exactly
what the Semble NDCG needs, and exactly what line@5 punishes.

## 2. Fix attempts (dev only)

Semble dev NDCG (630 queries) and RepoQA dev line@5/file:

| Config | Semble dev | RepoQA line@5 | RepoQA file |
| --- | ---: | ---: | --- |
| base | 0.7787 | 228 | 195/268/283 |
| no sat (sym+stem+coh) | 0.8312 | 228 | 196/270/283 |
| sat 0.5, from 2nd (`f2`) | 0.8412 | 226 | 196/270/283 |
| sat 0.5, from 3rd (`f3`) | 0.8372 | 226 | 196/270/283 |
| sat 0.7, from 2nd | 0.8517 | 210 | 196/270/283 |
| sat 0.8, from 2nd | 0.8491 | 219 | 196/270/283 |
| two-level 0.85 / 0.50 (`t85`) | 0.8490 | 222 | 196/270/283 |
| two-level 0.90 / 0.50 (`t90`) | 0.8469 | 225 | 196/270/283 |
| two-level 0.95 / 0.50 (`t95`) | 0.8440 | 226 | 196/270/283 |
| cap 2 per file | 0.8424 | 225 | 196/270/283 |
| r1 (rank 2) | 0.8524 | 201 | 196/270/283 |

Two-level means: 1st chunk x1.0, 2nd x`sat2`, k-th (k >= 3) x
`sat2 * sat^(k-2)`. In all cases the RepoQA file metrics are unchanged.

The dev table already shows the trap. `f2`/`f3` keep line@5 but lose 0.011-0.015
Semble dev. `t90` keeps most of the Semble gain but still loses 3 line@5 on dev.
Any config that reaches line@5 228 is `no sat`, which loses about 0.021 Semble
dev.

Predeclared candidates: **t90** and **t95**. `t85` was rejected on dev because
its line@5 was exactly 222 and the dev-to-held drop was expected to put it
below. `f2` was rejected because its Semble dev margin was too small.

## 3. Second held-out look (bias stated)

This is a **second look** at the Semble held-out repos, RepoQA held-out and
CoSQA held-out. r1 already used them once in rank 2. t90 and t95 were selected
after that first look. The held-out numbers below are therefore optimistically
biased; treat them as an upper bound, not a clean estimate.

List fixed in `bench/WG_RANK3_HELDOUT_CONFIGS.json` (SHA-256
`8ebb8da5e4fab9f4fe477eb14c95ed30e5d8602e0fda30518712b390cba240da`).

| Set | base | t90 | t95 | Target |
| --- | ---: | ---: | ---: | --- |
| Semble held-out macro NDCG@10 (31 repos) | 0.6867 | **0.7835** | 0.7805 | >= 0.7812 |
| Semble combined 63 repos | 0.7334 | 0.8157 | 0.8128 | - |
| RepoQA held-out file H@1/5/10 | 202/270/283 | **202/270/286** | **202/270/286** | no loss |
| RepoQA held-out MRR | 0.76569 | **0.76699** | **0.76699** | no loss |
| RepoQA held-out line@1/5/10 | 156/227/245 | 155/**214**/234 | 155/**218**/233 | line@5 >= 222 |
| CoSQA held-out H@1/5/10 | 74/146/177 | **74/146/177** | **74/146/177** | no loss |

Acceptance:

| Criterion | t90 | t95 |
| --- | --- | --- |
| Semble within 0.01 of r1 (0.7912) | PASS (0.7835) | FAIL (0.7805) |
| RepoQA line@5 >= 222 | **FAIL (214)** | **FAIL (218)** |
| No RepoQA file loss | PASS | PASS |
| No CoSQA loss | PASS | PASS |

Launch q1-q8 for t90 is the same as r1: q3 file 6 / line absent, q4 file 1,
q5 file 3 / line 21, q6 file 3 / line 3, q7 line 34, q8 line 11.

The held-out line@5 drop is larger than the dev drop for every saturation
variant (t90: 225 dev -> 214 held; t95: 226 -> 218). The held-out repos have
more multi-chunk gold files. This is why the dev-tuned line margin did not
survive.

## 4. Why this is structurally hard

For a RepoQA query, the gold line is in the 1st or 2nd chunk of the gold file.
For Semble NDCG, the largest gain comes from demoting the 2nd and later chunks
of distractor files. The 2nd chunk cannot be both fully ranked for line@5 and
demoted for diversity.

Measured trade-off, dev:

- Keep the 2nd chunk at 1.0: line@5 228, Semble 0.8312 (held ~0.77).
- 2nd at 0.95: line@5 226, Semble 0.8440 (held 0.7805).
- 2nd at 0.90: line@5 225, Semble 0.8469 (held 0.7835).
- 2nd at 0.80: line@5 219, Semble 0.8491.
- 2nd at 0.50: line@5 201, Semble 0.8524 (held 0.7912).

A pure line target of 222 needs the 2nd chunk at about 0.97 or higher, which
puts Semble held-out at or below 0.781. A clean Semble target of 0.7812 needs
the 2nd chunk at about 0.90, which puts line@5 at 214. The two targets do not
overlap in this signal family.

A different mechanism would be needed to satisfy both. The only one that looks
promising is relevance-aware diversification (demote a repeat chunk only when
it does not cover a distinct target), which requires relevance at query time.
The product does not have it.

## 5. Exact final algorithm (t90)

This is the best-balanced candidate. It is **not recommended for the default**
because it misses line@5. It is written here so it can be implemented directly
if the line target is relaxed.

Index build: same as rank 2 (narrow per-language definition sites, normalised
full names for the symbol boost).

Query time, after the 200+200 candidate union and the relative scores:

1. `B[i] = 0.25 * bm25_relative[i] + 0.75 * semantic_relative[i]`.
   `M = max(B)`.
2. Coherence: for each file, sum `max(B,0)`; add
   `0.05 * M * file_sum / max_file_sum` to the file's best-B chunk.
3. Symbol boost: if the whole query matches Semble's symbol regex, extract the
   last segment, and add `1.0 * M` to chunks defining it (`1.5` when the file
   stem matches). Skip otherwise.
4. Stem boost: if the query is not a symbol, match query words (3+ characters,
   not stopwords) to the file stem and parent directory (exact or 3-character
   prefix). If `ratio = matches/keywords >= 0.10`, add `0.6 * M * ratio`.
5. `score = B + coherence + symbol + stem`.
6. Selection for the first 10 positions:
   - keep the hard exact-identifier tier;
   - maintain `count[file]`, the number of chunks already selected from the
     file, and the selected spans per file;
   - skip a chunk contained in an already selected chunk of the same file;
   - effective score:
     - `count = 0`: `score`
     - `count = 1`: `score * 0.9`
     - `count >= 2`: `score * 0.9 * 0.5^(count-1)`
   - pick the maximum by `(exact desc, effective desc, bm25 desc)`;
   - increment `count[file]`.
7. Positions 11 and later: sort the remaining chunks by
   `(exact desc, score desc, bm25 desc)` and append, skipping contained chunks.
8. `--top` above 10 keeps the saturation window at 10.

Constants: `sat = 0.5`, `sat2 = 0.9`, `sel_top = 10`, `coh = 0.05`,
`stem = 0.6`, `sym = 1.0`, `sym_stem = 1.5`, narrow definitions.

Measured effect: Semble held-out 0.7835 (second look), RepoQA held-out
202/270/286 with MRR 0.76699, line@5 214; CoSQA unchanged; launch q5 3/21.

## 6. Cost

The selection path is the same as r1. Measured with the tool timer:

| Corpus | Top | base `sel` | t90 `sel` | Added |
| --- | ---: | ---: | ---: | ---: |
| tokio (4,611 chunks) | 10 | 122 us | 577 us | +0.45 ms |
| launch (63,786 chunks) | 10 | 149 us | 1026 us | +0.88 ms |

Definition index: 5.87 MB at 63,786 chunks, 495-537 ms build. Peak RSS within
run-to-run noise (launch 739 vs 729 MiB). The first launch sample read 2.3 ms
for t90; a repeat gave 1.03 ms, so that first sample was an outlier.

## 7. Recommendation

1. **Keep r1 opt-in.** The blocker is not fixed; the default should stay at
   parity.
2. **Do not ship t90 or t95 as the default.** t90 passes Semble and the file
   metrics but misses line@5 by 8. t95 misses both.
3. If the team accepts line@5 214, t90 is the best-balanced option and the
   algorithm above is ready.
4. The clean fix is relevance-aware saturation: demote a repeat chunk only when
   it does not cover a target that no earlier chunk covers. That needs relevance
   at query time, which the current ranker does not have.

## 8. `no sat`: third held-out look

### 8.1 Bias statement

This is the **third look** at the held-out sets. Rank 2 used them once for r1;
rank 3 used them once for t90/t95. The config here was fixed **before** this
run: it is the `no sat` row from sections 1 and 2, a sub-config of the
predeclared r1 with saturation off (`sym 1.0, stem 0.6, coh 0.05, sym_stem 1.5,
narrow definitions, sat/cap off`). It was not tuned on these held-out numbers.
Even so, repeated use of the same held-out sets means the numbers are
optimistic; treat them as an upper bound.

### 8.2 Results

| Set | base | `no sat` | r1 (rank 2) |
| --- | ---: | ---: | ---: |
| Semble held-out macro NDCG@10 (31 repos) | 0.6867 | **0.7609** | 0.7912 |
| Semble combined 63 repos | 0.7334 | **0.7966** | 0.8222 |
| Semble architecture | 0.6569 | **0.6965** | 0.7248 |
| Semble semantic | 0.6884 | **0.7426** | 0.7830 |
| Semble symbol | 0.7303 | **0.9256** | 0.9256 |
| RepoQA held-out file H@1/5/10 | 202/270/283 | **202/270/286** | 202/270/286 |
| RepoQA held-out MRR | 0.76569 | **0.76699** | 0.76699 |
| RepoQA held-out line@1/5/10 | 156/227/245 | **155/227/247** | 155/214/234 |
| CoSQA held-out H@1/5/10 | 74/146/177 | **74/146/177** | 74/146/177 |

Semble held-out: 29 repos improved, 2 worse (libuv -0.0085, abseil-cpp
-0.0059), 0 flat. Per-query NDCG: 194 wins, 62 losses, 365 ties (621 queries).
Largest gains: newtonsoft-json +0.232, laravel-framework +0.188, gson +0.155,
rails +0.153, guzzle +0.149.

RepoQA held-out paired: H@1 4 gains / 4 losses (net 0), 17 top-10 rank moves up
and 17 down. The file metrics do not move beyond noise. Line@5 is **exactly
227**, the baseline value; line@10 is +2. This is the key result: with
saturation off, the line cost disappears.

CoSQA held-out is byte-identical to the baseline.

Launch q1-q8 for `no sat`:

| Q | base file/line | `no sat` file/line | r1 file/line |
| --- | --- | --- | --- |
| q1 | 1 / 1 | 1 / 1 | 1 / 1 |
| q2 | 1 / - | 1 / - | 1 / - |
| q3 | absent / absent | 6 / absent | 6 / absent |
| q4 | 2 / - | 1 / - | 1 / - |
| q5 | 10 / 26 | **3 / 17** | 3 / 21 |
| q6 | 4 / 5 | 3 / 3 | 3 / 3 |
| q7 | 1 / 17 | 1 / 34 | 1 / 35 |
| q8 | 1 / 8 | **1 / 5** | 1 / 12 |

`no sat` matches r1 on every file rank and is better on line rank for q5, q7
and q8. It does not improve q3's line, which remains outside the candidate
union.

Cost: no saturation means no greedy loop. The selection is the existing sort.
On tokio (4,611 chunks, top 10), the added score/boost work is +0.36 ms per
query (base `sel` 136 us, `no sat` 492 us); the index is 681 KB with a 63 ms
build. The launch index is the same 5.87 MB as r1.

### 8.3 Verdict

`no sat` is the **candidate default ranking**. It gives:

- Semble held-out +0.0742 macro (combined +0.0632, 52% of the gap);
- RepoQA file metrics unchanged or better (+3 H@10, +0.0013 MRR);
- RepoQA line@5 exactly equal to baseline (227);
- CoSQA unchanged;
- launch file ranks equal to r1 and better line ranks.

It does **not** meet the rank-3 target "Semble within 0.01 of r1": 0.7609
against 0.7812. The missing 0.030 is the explicit price of line@5: r1 gains
+0.030 over `no sat` by demoting the 2nd chunk of a file, and loses 28 line@5
queries (199 against 227). t90 gains +0.023 over `no sat` and loses 13 line@5
(214). The choice is now clean:

- zero line cost, +0.074 Semble: `no sat` (this section);
- +0.097 Semble, line@5 214: t90 (section 5);
- +0.105 Semble, line@5 199: r1 (rank 2).

Recommendation: if the team accepts that the ranking score changes (parity
break) but wants no line regression, **make `no sat` the default**. If exact
score parity is still required, the default stays as it is and `no sat` is an
opt-in mode.

### 8.4 Exact algorithm for `no sat`

Index build (once per corpus): the rank-2 narrow definition index. For each
chunk, match the narrow per-language definition regexes. Store each defined
name as a normalised full name (lowercase, remove `_`). No token IDF is needed
for this mode.

Query time (`rank_impl`), after the 200+200 candidate union and the existing
relative scores:

1. For each union row `i`, `B[i] = 0.25 * bm25_relative[i] +
   0.75 * semantic_relative[i]`. Let `M = max(B)`.
2. **File coherence** (Semble `boost_multi_chunk_files`): for each file, sum
   `max(B, 0)` over its union chunks; find the chunk with the largest `B`. Add
   `0.05 * M * file_sum / max_file_sum` to that chunk.
3. **Symbol boost** (Semble `_boost_symbol_definitions`, no 3.0 multiplier):
   - Test the trimmed query against Semble's whole-query symbol regex:
     ```
     ^(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+|_[A-Za-z0-9_]*|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*|[A-Z][A-Za-z0-9]*)$
     ```
   - If it matches, take the last segment after `::`, `\`, `->`, or `.`,
     normalise it, and add `1.0 * M` to every chunk whose normalised
     definition-name set contains it.
   - Multiply that boost by `1.5` when the file stem matches the symbol
     (exact, underscore-normalised, or plural-tolerant).
4. **NL stem boost** (Semble `_boost_stem_matches`): skip for symbol queries.
   Take query words of 3+ characters that are not stopwords. For each chunk,
   match them against the file stem and parent directory name (exact or a
   3-character prefix). Let `ratio = matches / keywords`; if `ratio >= 0.10`,
   add `0.6 * M * ratio`.
5. `score[i] = B[i] + coherence[i] + symbol[i] + stem[i]`.
6. **Selection is unchanged from `rank_impl`**: sort by
   `(exact identifier first, score descending, BM25 descending)`, skip a chunk
   contained in an already selected chunk of the same file, and take the top N.
   There is no saturation, no per-file cap, and no other change to tie-breaks
   or deduplication.

Constants: `coh = 0.05`, `stem = 0.6`, `sym = 1.0`, `sym_stem = 1.5`, narrow
definitions, `sat = 0`, `cap = 0`, no `defn`, no `pen`, no `rrf`. The run
config carries `defn_kw: narrow` for documentation, but `defn = 0`, so the
name-score path is off; the symbol index always uses the narrow set.

Measured effect on the second/third-look held-out sets: Semble 0.7609 (31
repos), RepoQA file 202/270/286 with MRR 0.76699, line@5 227, CoSQA unchanged,
launch q5 3/17.

## Receipts

Local files:

- `bench/WG_RANK3_REPORT.md` (this file)
- `bench/WG_QUALITY_TOOL/src/main.rs` (`sat_from`, `sat2`; baseline unchanged)
- `bench/WG_RANK3_DIAG_CONFIGS.json`, `WG_RANK3_FIX_CONFIGS.json`,
  `WG_RANK3_NOSAT_CONFIGS.json`,
  `WG_RANK3_FIX2_CONFIGS.json`, `WG_RANK3_FIX3_CONFIGS.json`,
  `WG_RANK3_HELDOUT_CONFIGS.json` (predeclared)
- `bench/WG_RANK3_COST.py`, `WG_RANK3_DUMP.py`, `WG_RANK3_NOSAT_STATS.py`

VPS (`/srv/projects/wispd/wg-rank3-20261024/`):

- `results/diag`, `nosat-semble`, `nosat-repoqa`, `nosat-cosqa`, `nosat-launch`,
  `repoqa-dev-fix`, `repoqa-dev-fix3`, `semble-dev-fix`,
  `semble-dev-fix2`, `semble-dev-fix3`, `semble-heldout`, `repoqa-heldout`,
  `cosqa-heldout`, `launch`
- Logs: `/srv/logs/wispd/wg-rank3-20261024/`
- The split is the rank-2 split, read-only
  (`/srv/projects/wispd/wg-rank2-20261024/split.json`).
- `target/` deleted at the end. No daemons started. No commit.
