# wg vs Semble: retrieval quality and speed

Date: 2026-10-24. Comparison only. No product code, test, docs, script or CI
change. All new files use the `bench/WG_SEMBLE_*` prefix. No commit.

## 0. Verdict

| Question | Winner | Size |
| --- | --- | --- |
| RepoQA 600, distinct-file quality | wg | H@10 94.3% vs 86.2%; H@1 66.2% vs 44.3%; MRR@10 0.757 vs 0.590 |
| CoSQA 500, unique-snippet quality | wg | H@10 70.2% vs 64.8%; H@1 29.2% vs 25.8%; MRR@10 0.417 vs 0.383 |
| Semble's 1,251-query benchmark, NDCG@10 | Semble | 0.854 vs 0.733 (+0.121; Semble wins 58 of 63 repos) |
| Warm queries, CLI against CLI | wg | 22.8 ms vs 2,121 ms p50 (93x) |
| Warm queries, Semble in-process against wg CLI | Semble | 6.7 ms vs 22.8 ms p50 (3.4x); modes differ, see 5.2 |
| Cold index, 13.3k-file Rust tree | wg | 17.4 s vs 40.3 s, but wg indexes more files |
| Resident RSS | wg | 208 MiB steady vs 940 MiB peak (4.5x less) |
| Edit to first correct result | wg | 77 ms vs 4,095 ms p50 (53x); Semble has no watcher |

Short form: wg is better on file and snippet retrieval for the RepoQA and CoSQA
sets, on resident memory, and on edit freshness. Semble is better on its own
natural-language benchmark, mostly on symbol and architecture queries, and it
has lower warm query latency when it runs in-process.

Do not generalise one result to the other task. The two quality sets measure
different things. Semble's set rewards natural-language descriptions of code.
RepoQA and CoSQA reward finding one known file or snippet for a longer
description.

## 1. Identity and environment

| Item | Value |
| --- | --- |
| Machine | Debian 13, x86_64, 4 vCPU (QEMU), 7.8 GiB RAM, shared VPS |
| wg source | commit `7ffb6242d4151d29d45915803729684de4fd5649` (local `main`) |
| wg binary | `/srv/projects/wispd/wg-semble-20261024/bin/wg`, SHA-256 `c430b973773d83361824603f75cbc18e2b97e045a4b40716bbb986b8240a66dd` |
| wg build | `cargo build --locked --release --bin wg` |
| Semble source | commit `913bdac1d59bc2defc658453a65719d133f1ff5a`, 2026-10-24 11:31:24 +0200, version `0.7.0` |
| Semble deps | `model2vec 0.10.0`, `vicinity 0.4.6`, `numpy 2.5.4`, `semble-grammars 0.1.2`, `orjson 3.13.0`, `pathspec 1.1.1` |
| Python | 3.13.5 in `semble-venv` (uv 0.13.5) |
| Model | `minishlab/potion-code-16M-v2`, revision `e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b` (same model for both tools) |
| RepoQA queries | SHA-256 `a3b5fe78601fe590ff293e3c190ab8bd7f63f3407af46702c8b4dbfe6811774c` |
| CoSQA queries | SHA-256 `27d3552b2e7dd86cf9f9a0b3c135e6777d4faafa9c29fb1b9b01ece945974ad6` |
| CoSQA snippet map | SHA-256 `be8a910ca03cb9f1b38a0eba6dc8109eeb8040a40aaba1a467a7b8426d261f3f` |
| RepoQA corpus | `/srv/projects/wispd/sgrep-published-inputs/repoqa-import/` (60 repos) |
| CoSQA corpus | `/srv/projects/wispd/sgrep-published-inputs/CoCLR/` at `14ebcacf9e9bc3e7109102632bc63047876f27d2` |
| Rust speed corpus | `/srv/workspaces/wispd/prof/src`, 13,443 files on disk |
| Disk | never fell below 12 GiB free; 16 GiB free after cleanup (90% used) |

No tool setting was tuned. Semble ran with its defaults: `content=code`,
`rerank=True`, `alpha=auto`. wg ran with `--model code`. The only non-default
environment knob for quality runs was `RAYON_NUM_THREADS=2`, the same setting
as `bench/WG_QUALITY_REPORT.md`; thread count does not change ranking.

### 1.1 wg baseline confirmation

`bench/WG_QUALITY_REPOQA.py` and `bench/WG_QUALITY_COSQA.py` at this wg commit
reproduce the published baseline exactly:

| Set | H@1 | H@5 | H@10 | MRR@10 |
| --- | ---: | ---: | ---: | ---: |
| RepoQA (wg) | 397 | 538 | 566 | 0.75736 |
| CoSQA (wg) | 146 | 287 | 351 | 0.417412 |

These are the same counts as `WG_QUALITY_REPORT.md` section 2. Arm A therefore
compares Semble against a confirmed current wg number, not an archived one.

## 2. Method

### 2.1 Arm A: Semble on our sets

Runner: `bench/WG_SEMBLE_OURSETS.py`. It imports `unique_rank` and `sha` from
`bench/WG_QUALITY_REPOQA.py` and the CoSQA constants from
`bench/WG_QUALITY_COSQA.py`, so the scoring rule is the same code.

- RepoQA: one Semble index per repository, built with `SembleIndex.from_path`.
  Each of the 600 queries calls `index.search(query, top_k=100)`. Results are
  deduplicated by file in rank order. Hit@1/5/10 and MRR@10 use the distinct
  file rank of `target_path`.
- CoSQA: the 6,267 published snippets are written as `NNNN.py` files in one
  corpus. The same top-k and distinct-file rule applies; each snippet is one
  file.
- Split: RepoQA uses the report's repo-level split (first five `source_id`
  values per language are dev, last five are held-out). CoSQA uses query order
  (first 250 dev, last 250 held-out).
- `top_k=100` chunks matches the wg runner's `--top 100` rows, so both tools
  may fill the top 10 distinct files after deduplication.
- Semble writes its own index cache under a task-local
  `SEMBLE_CACHE_LOCATION`; quality does not depend on cache state. All 60 RepoQA
  target files and all 6,267 CoSQA target files were in the index
  (`target_not_indexed=0`).

Command:

```sh
HF_HOME=/srv/projects/wispd/wg-semble-20261024/hf HF_HUB_OFFLINE=1 \
  ../semble-venv/bin/python bench/WG_SEMBLE_OURSETS.py repoqa
HF_HOME=/srv/projects/wispd/wg-semble-20261024/hf HF_HUB_OFFLINE=1 \
  ../semble-venv/bin/python bench/WG_SEMBLE_OURSETS.py cosqa
```

### 2.2 Arm B: wg on Semble's benchmark

First, Semble's own runner reproduced the published number:

```sh
python -m benchmarks.run_benchmark
```

Result: `benchmarks/results/semble-hybrid-913bdac1d59b.json`, summary NDCG@10
**0.8539** over 63 repos and 1,251 queries. The shipped result at commit
`5fbcd035d594` records **0.8546**; the task's published figure is **0.854**,
and the README rounds to **0.855**. The difference is 0.0007 against the shipped
result, and every per-language value matches the README table to three decimals
(e.g. bash 0.840, go 0.910, rust 0.826, zig 0.899). The reproduction is a match
within one part in a thousand.

Second, `bench/WG_SEMBLE_THEIRSET.py` loads the same tasks with Semble's own
`benchmarks.data` loader and scores with Semble's own `ndcg_at_k` and
`target_matches_location`. For each repo it runs:

```sh
wg up <benchmark_root> --model code --json
wg "<query>" <benchmark_root> --model code --json --top 10
```

The 63 index roots are `repos/<name>/<benchmark_root>`; `<query>` is the
annotation text; ranking uses the 10 returned chunks; a chunk is relevant when
its line span overlaps an annotated target span. Aggregation is per-repo mean
NDCG@10, then unweighted mean over 63 repos, exactly as `_save_results` does.
The query-weighted mean is also reported.

### 2.3 Arm C: speed and memory

Runner: `bench/WG_SEMBLE_SPEED.py`. Corpus: the Rust `src` copy (13,443 files
on disk; wg indexes 13,332 of them).

- Semble in-process: build `SembleIndex.from_path`, then 120 warm samples
  (24 per query) of `index.search(q, top_k=10)`. This is the closest analogue
  to the Semble MCP server, which holds an index in memory.
- Semble CLI: `semble search q <corpus> -k 10 --format json`, 120 warm samples.
  Each call starts Python, loads the model and about 157 MiB of index cache,
  searches, and exits.
- wg: `wg up <corpus> --model code --json`, then 120 warm samples of
  `wg q <corpus> --model code --json --top 10`. Each sample includes process
  start and loopback IPC to the resident daemon. This is the public wg mode.
- Index time: both measured cold after `echo 3 > /proc/sys/vm/drop_caches`.
  Semble's library build does not write its disk cache; the first CLI call does.
  wg's `up` includes its index persistence.
- RSS: Semble from `ru_maxrss` (peak). wg daemon from 50 ms sampling during
  `up`, then a steady reading 2 s after ready. Semble CLI peak sampled at 10 ms.
- Edit: write a new probe file with a unique symbol, then repeat the CLI
  search until the probe file is returned (10 measured samples, 2 warmups,
  after an untimed build). Measured on separate copies of the corpus.

The five queries are the ones in `docs/benchmarks.md`:
`parse Rust expressions`, `emit compiler error diagnostics`,
`validate command line options`, `resolve module imports`,
`incremental compilation query dependency tracking`.

All measurement phases started with a 1-minute load average below 1.0. Another
agent ran `wg-defboost` jobs on the same VPS between 23:09 and 23:21. One
Semble CLI pass overlapped that window and was discarded; the reported Semble
CLI and edit numbers come from a clean pass at 23:24-23:32 with no `defboost`
process and `load1` 0.45-0.99 at phase start. Index and warm-query phases ran
at `load1` 0.44-0.48. The load averages after each phase (1.2-1.3) are the
measurement's own CPU use.

## 3. Arm A results

### 3.1 RepoQA 600

| Measure | Semble | wg | Delta (wg - Semble) |
| --- | ---: | ---: | ---: |
| Hit@1 distinct file | 266 / 600 (44.3%) | 397 / 600 (66.2%) | +21.8 pp |
| Hit@5 distinct files | 475 / 600 (79.2%) | 538 / 600 (89.7%) | +10.5 pp |
| Hit@10 distinct files | 517 / 600 (86.2%) | 566 / 600 (94.3%) | +8.2 pp |
| MRR@10 | 0.59002 | 0.75736 | +0.1673 |
| Errors | 0 | 0 | - |

Split:

| Split | Semble H@1/5/10 | Semble MRR | wg H@1/5/10 | wg MRR |
| --- | --- | ---: | --- | ---: |
| Dev (300) | 126 / 244 / 260 | 0.58184 | 195 / 268 / 283 | 0.74903 |
| Held-out (300) | 140 / 231 / 257 | 0.59821 | 202 / 270 / 283 | 0.76569 |

Per language (Hit@1/5/10 and MRR@10):

| Language | Semble | wg |
| --- | --- | --- |
| C++ | 41 / 76 / 88, 0.5502 | 58 / 87 / 93, 0.6942 |
| Go | 35 / 71 / 76, 0.4985 | 72 / 93 / 95, 0.8013 |
| Java | 48 / 82 / 88, 0.6079 | 62 / 90 / 95, 0.7286 |
| Python | 43 / 76 / 86, 0.5857 | 63 / 94 / 97, 0.7564 |
| Rust | 52 / 88 / 93, 0.6677 | 74 / 93 / 96, 0.8168 |
| TypeScript | 47 / 82 / 86, 0.6301 | 68 / 81 / 90, 0.7468 |

The wg TypeScript Hit@5 is one query below Semble (81 vs 82); wg wins every
other cell. Semble indexed every gold file, so the gap is chunking and ranking,
not file coverage.

### 3.2 CoSQA 500

| Measure | Semble | wg | Delta (wg - Semble) |
| --- | ---: | ---: | ---: |
| Hit@1 unique snippet | 129 / 500 (25.8%) | 146 / 500 (29.2%) | +3.4 pp |
| Hit@5 unique snippets | 267 / 500 (53.4%) | 287 / 500 (57.4%) | +4.0 pp |
| Hit@10 unique snippets | 324 / 500 (64.8%) | 351 / 500 (70.2%) | +5.4 pp |
| MRR@10 | 0.38255 | 0.41741 | +0.0349 |
| Errors | 0 | 0 | - |

Split:

| Split | Semble H@1/5/10 | Semble MRR | wg H@1/5/10 | wg MRR |
| --- | --- | ---: | --- | ---: |
| Dev (250) | 62 / 133 / 160 | 0.37618 | 72 / 141 / 174 | 0.41394 |
| Held-out (250) | 67 / 134 / 164 | 0.38892 | 74 / 146 / 177 | 0.42088 |

Semble indexed all 6,267 snippets as 6,267 files and 6,501 chunks. Both tools
show the same dev/held-out direction.

## 4. Arm B results

### 4.1 Reproduction of Semble's published number

| Item | Value |
| --- | ---: |
| 63 repos, 1,251 queries | - |
| Reproduced NDCG@10 (mean over repos) | 0.8539 |
| Shipped result `5fbcd035d594` | 0.8546 |
| Task's published value | 0.854 |
| README rounded value | 0.855 |

The per-language table from the reproduction matches the README to three
decimals on all 19 languages. The 0.0007 gap is the same order as the spread
between the five shipped hybrid result files (0.8519-0.8546), which were made
at different commits.

### 4.2 wg on the same queries and repos

| Measure | Semble | wg |
| --- | ---: | ---: |
| NDCG@10, mean over 63 repos | 0.8539 | 0.7331 |
| NDCG@10, query-weighted | - | 0.7326 |
| Repos where the tool is ahead | 58 | 5 |
| Queries | 1,251 | 1,251 |
| Queries with no relevant chunk in top 10 | - | 98 |
| Failed queries | 0 | 1 |

The one wg failure is the query `g` (flask): wg exits 2 with
`wg: query has no searchable words`. It scores 0. Semble answers it. Removing
that query would change the macro by about 0.0006.

Per language:

| Language | Semble | wg | Delta |
| --- | ---: | ---: | ---: |
| bash | 0.840 | 0.731 | +0.109 |
| c | 0.750 | 0.641 | +0.109 |
| cpp | 0.881 | 0.798 | +0.083 |
| csharp | 0.872 | 0.612 | +0.260 |
| elixir | 0.906 | 0.858 | +0.048 |
| go | 0.910 | 0.736 | +0.174 |
| haskell | 0.775 | 0.747 | +0.028 |
| java | 0.828 | 0.663 | +0.165 |
| javascript | 0.911 | 0.887 | +0.025 |
| kotlin | 0.803 | 0.687 | +0.116 |
| lua | 0.835 | 0.812 | +0.023 |
| php | 0.864 | 0.631 | +0.233 |
| python | 0.876 | 0.774 | +0.102 |
| ruby | 0.909 | 0.780 | +0.129 |
| rust | 0.826 | 0.667 | +0.159 |
| scala | 0.921 | 0.805 | +0.116 |
| swift | 0.848 | 0.756 | +0.091 |
| typescript | 0.725 | 0.511 | +0.214 |
| zig | 0.899 | 0.753 | +0.146 |

Per category:

| Category | Semble | wg | Delta |
| --- | ---: | ---: | ---: |
| architecture (344) | 0.808 | 0.700 | +0.108 |
| semantic (703) | 0.847 | 0.738 | +0.109 |
| symbol (204) | 0.947 | 0.770 | +0.177 |

Largest per-repo gaps for wg (Semble minus wg):

| Repo | Language | Semble | wg | Delta |
| --- | --- | ---: | ---: | ---: |
| newtonsoft-json | csharp | 0.892 | 0.489 | +0.402 |
| zod | typescript | 0.643 | 0.242 | +0.401 |
| nvm | bash | 1.000 | 0.606 | +0.394 |
| laravel-framework | php | 0.788 | 0.449 | +0.339 |
| cobra | go | 0.971 | 0.679 | +0.292 |
| gson | java | 0.892 | 0.609 | +0.283 |
| axum | rust | 0.815 | 0.545 | +0.270 |
| rails | ruby | 0.854 | 0.590 | +0.264 |
| zig-clap | zig | 0.938 | 0.682 | +0.256 |
| trpc | typescript | 0.849 | 0.608 | +0.241 |

wg wins five repos: `bats-core` 0.975 vs 0.866 (delta -0.109), `aeson` 0.875 vs
0.768 (-0.107), `ecto` 0.926 vs 0.903 (-0.023), `telescope.nvim` 0.769 vs 0.761
(-0.008), `redux` 0.923 vs 0.918 (-0.006).

Two spot checks confirmed the adapter is not losing paths:

- zod: wg returns `v4/core/schemas.ts` for the query `ZodStringDef` (rank 3
  distinct file), so the file is indexed. It does not return that file in the
  top 100 chunks for `ZodType` or for the architecture query about `ZodType`,
  because wg ranks `v3/types.ts` first and fills most rows with one file. This
  is a ranking and diversity difference, not a path-mapping error.
- Annotation targets may be relative to the repository root (`packages/zod/src/...`)
  while wg paths are relative to the indexed root (`v4/...`). Semble's own
  `path_matches` suffix rule handles this, and both tools use that same rule.

### 4.3 Reusable Semble benchmark set

Another agent can reuse this set. Layout:

```
/srv/projects/wispd/wg-semble-20261024/semble-bench/
  repos.json                 # 63 pinned repos (copied from Semble benchmarks/)
  annotations/<repo>.json    # 63 query sets with paths, spans, categories
  repos/<repo>/              # git checkouts at the revision in repos.json
```

- `repos.json` SHA-256: `fd178d28fdb98ef774d9d2b8169b34812917bde4ca57c5f7c73a1e34dfc70b1b`
- Annotations tree SHA-256 (tar of the directory): `6491f11eeb3b6dae63cc6912566bc70223bb73769ec65391202b60a34bcf7600`
- The index root for a repo is `repos/<name>/<benchmark_root>`.
- `benchmarks.sync_repos --check` reports `Verified 63 pinned repo(s)`.
- `~/.cache/semble-bench` is a symlink to `semble-bench/repos`, so Semble's
  own scripts find the checkouts at their default path.
- Imported set size: 4.7 GiB.

## 5. Arm C results

### 5.1 Raw numbers

| Measure | Semble library (in-process) | Semble CLI | wg CLI |
| --- | ---: | ---: | ---: |
| Files indexed | 7,875 | 7,875 | 13,332 |
| Chunks | 73,896 | 73,896 | 63,786 |
| Cold index time | 40.31 s | not separately timed | 17.36 s |
| Warm query p50 | 6.69 ms | 2,120.9 ms | 22.82 ms |
| Warm query p95 | 14.83 ms | 2,295.4 ms | 24.74 ms |
| Peak RSS | 939.5 MiB | 358.2 MiB | 479.9 MiB |
| Steady RSS | not sampled | n/a (process exits) | 208.2 MiB |
| RSS after queries | not sampled | n/a | 278.6 MiB |
| Edit-to-searchable p50 (n=10) | - | 4,094.9 ms | 76.6 ms |
| Edit-to-searchable p95 (n=10) | - | 4,391.6 ms | 88.8 ms |
| Edit-to-searchable min / max | - | 3,949 / 4,562 ms | 72.9 / 94.9 ms |

Index time composition: wg `up` reports `build_ms=16115`, `model_ms=275`,
`total_ms=17343`. Semble's 40.31 s includes model load and all chunking and
embedding.

### 5.2 How to read the speed table

These are three different execution modes, not one:

1. **Semble library against wg CLI** (the closest match to "agent tool" use):
   6.69 ms vs 22.82 ms p50. Semble is 3.4x faster. This comparison is not
   perfectly symmetric: the Semble number excludes process start, while the wg
   number includes process start and loopback IPC. wg has no documented
   in-process search client for this benchmark; its resident daemon is the
   in-process part, and the CLI is the public client.
2. **Semble CLI against wg CLI** (same interface class): 2,120.9 ms vs
   22.82 ms p50. wg is 92.9x faster. Semble's CLI reloads the model and the
   157 MiB index on every call; it is built for MCP, not for per-call CLI use.
3. **Memory**: wg holds 208.2 MiB steady after warm-up, against 939.5 MiB peak
   for the Semble library. wg uses 4.5x less resident memory. wg's peak during
   build (479.9 MiB) is above Semble CLI's per-process peak (358.2 MiB), but
   Semble's library peak is higher.

Index time is not fully like-for-like. Semble indexes code files only (7,875
files here, almost all `.rs`); wg reports 13,332 files for its combined text and
semantic index. On chunks the comparison is closer (73,896 vs 63,786), but the
chunkers differ. Semble's library build also does not write its disk cache,
while wg's `up` persists its index. Treat 40.3 s vs 17.4 s as "on this corpus,
with these defaults", not as a pure per-file ratio.

### 5.3 Edit-to-searchable

wg has an inotify watcher. Its measured time is write return to first CLI
response that contains the new symbol: 76.6 ms p50. This is consistent with the
99.2 ms p50 in `docs/benchmarks.md` (different probe and sample count).

Semble has no watcher. Its CLI revalidates the file manifest on each search and
rebuilds incrementally when a file changed. The measured 4,094.9 ms p50 is the
time to the first CLI response that contains the new symbol, and it includes
Python start, model load, the 157 MiB index load, the incremental rebuild, and
the index save. A resident Semble MCP server does not watch either: it
revalidates a cached index at most once per 3x the last build time
(`_MIN_REVALIDATE_FACTOR = 3`), so on this corpus the first possible refresh is
about 120 s after a build. Do not read 4.1 s as "Semble refreshes in 4.1 s in
all modes". Read it as "the next CLI search that refreshes the index costs
about 4.1 s".

## 6. Where each tool is better

**Semble is better at:**

- Natural-language and architecture queries on its own benchmark: NDCG@10 0.854
  against 0.733 for wg, a 0.121 absolute and 16.5% relative gap. It wins 58 of
  63 repos.
- Symbol queries on that benchmark: 0.947 against 0.770. This is the largest
  category gap and the most surprising one, because wg is strong on exact file
  lookup in RepoQA.
- Warm query latency when the index is in process: 6.7 ms against 22.8 ms.
- Per-language consistency on its benchmark: it is ahead in all 19 languages.

**wg is better at:**

- RepoQA 600: H@10 94.3% vs 86.2%, H@1 66.2% vs 44.3%, MRR 0.757 vs 0.590.
  This holds on dev and held-out splits and in all six languages at H@10
  (TypeScript H@5 is one query lower for wg).
- CoSQA 500: H@10 70.2% vs 64.8%, H@1 29.2% vs 25.8%, MRR 0.417 vs 0.383.
- CLI latency: 22.8 ms vs 2,121 ms p50 (93x), because it keeps a resident
  daemon and Semble's CLI reloads its index per call.
- Resident memory: 208 MiB steady for wg vs 940 MiB peak for Semble.
- Edit freshness: 77 ms vs 4,095 ms p50, because it has a watcher and Semble
  refreshes lazily.

**Mixed:**

- Index time and coverage. wg is faster on this corpus (17.4 s vs 40.3 s) and
  indexes more files, but the tools index different file sets and chunks, so
  this is not a clean per-file win.

## 7. Caveats and limits

- One machine, one corpus for speed, one run per arm. Query latency uses 120
  samples; edit latency uses 10 samples after 2 warmups.
- Arm B uses the benchmark that Semble wrote and labelled (Claude Sonnet 4.6
  generated the queries and judged the labels). It is a fair test of that
  benchmark, but it is a home-field test.
- Arm A uses our file and snippet metrics, not NDCG. Semble returns chunks; we
  deduplicate by file, which is the rule our report uses. Semble's own published
  metric does not deduplicate by file.
- Arm B's wg numbers include one query that wg rejects (`g`) and 98 queries
  where no returned chunk overlaps the target span. Both are real behaviour; no
  retry or repair was applied.
- The two tools do not index the same file set. Semble uses `content=code`;
  wg's reported file count includes its text index. Index time and RSS are
  therefore not pure per-file tools.
- The Semble CLI numbers are a worst-case interface for Semble, because each
  call loads the index. They are included because they are the only same-shape
  comparison (CLI against CLI). The in-process number is the fair stand-in for
  its MCP mode.
- Semble has no watcher. The edit comparison measures a watcher against lazy
  per-search refresh, and the Semble figure includes CLI process start and index
  persistence.
- Another agent ran `wg-defboost` jobs on the same VPS during part of the work.
  One Semble CLI pass overlapped and was discarded and re-run. The reported
  phases started with `load1` below 1.0 and no `defboost` process. The VPS is
  shared, so small drift remains possible.
- Disk never fell below 12 GiB free. The task said to stop if it fell below
  10 GiB; it did not.

## 8. Receipts

Local files:

- `bench/WG_SEMBLE_OURSETS.py` (Arm A runner)
- `bench/WG_SEMBLE_THEIRSET.py` (Arm B adapter)
- `bench/WG_SEMBLE_SPEED.py` (Arm C runner)
- `bench/WG_SEMBLE_REPORT.md` (this file)

VPS results (`/srv/projects/wispd/wg-semble-20261024/`):

- `arm-a/repoqa/summary.json`, `arm-a/repoqa/per-query.jsonl`
- `arm-a/cosqa/summary.json`, `arm-a/cosqa/per-query.jsonl`
- `arm-a/wg-repoqa/summary.json`, `arm-a/wg-cosqa/summary.json`
- `arm-b/wg/summary.json`, `arm-b/wg/per-query.jsonl`
- `arm-b` reproduction: `semble-repo/benchmarks/results/semble-hybrid-913bdac1d59b.json`
- `arm-c/semble-inproc.json`, `arm-c/semble-cli.json`, `arm-c/wg.json`,
  `arm-c/edit-wg.json`, `arm-c/edit-semble.json`
- `bin/wg` (SHA-256 above), `wg/` (synced tree), `semble-repo/` (pinned clone),
  `semble-venv/`, `semble-bench/` (reusable set)
- Logs: `/srv/logs/wispd/wg-semble-20261024/`

Exact commands:

```sh
# build
source ~/.cargo/env && cd /srv/projects/wispd/wg-semble-20261024/wg
cargo build --locked --release --bin wg

# Arm A
cd wg
HF_HOME=../hf HF_HUB_OFFLINE=1 ../semble-venv/bin/python bench/WG_SEMBLE_OURSETS.py repoqa
HF_HOME=../hf HF_HUB_OFFLINE=1 ../semble-venv/bin/python bench/WG_SEMBLE_OURSETS.py cosqa
python3 bench/WG_QUALITY_REPOQA.py --wg ../bin/wg --run ../arm-a/wg-repoqa
python3 bench/WG_QUALITY_COSQA.py --wg ../bin/wg --run ../arm-a/wg-cosqa

# Arm B
cd ../semble-repo
HF_HOME=../hf HF_HUB_OFFLINE=1 ../semble-venv/bin/python -m benchmarks.sync_repos --check
HF_HOME=../hf HF_HUB_OFFLINE=1 ../semble-venv/bin/python -m benchmarks.run_benchmark
cd ../wg
../semble-venv/bin/python bench/WG_SEMBLE_THEIRSET.py

# Arm C (load1 < 1.0)
python bench/WG_SEMBLE_SPEED.py semble-inproc --cache ../arm-c/cache-inproc --out ../arm-c/semble-inproc.json
python bench/WG_SEMBLE_SPEED.py wg --service ../arm-c/svc-wg --out ../arm-c/wg.json
python bench/WG_SEMBLE_SPEED.py semble-cli --cache ../arm-c/cache-cli --out ../arm-c/semble-cli.json
python bench/WG_SEMBLE_SPEED.py edit-wg --corpus ../arm-c/corpus-wg --service ../arm-c/svc-edit-wg --out ../arm-c/edit-wg.json
python bench/WG_SEMBLE_SPEED.py edit-semble --corpus ../arm-c/corpus-semble --cache ../arm-c/cache-edit-semble --out ../arm-c/edit-semble.json
```

Cleanup at end: all wg daemons stopped with `wg down`; the wg `target/`
directory was deleted after copying the binary; arm-c corpus copies and index
caches were deleted. Logs, JSON results and the `semble-bench/` set were kept.
No commit was made.
