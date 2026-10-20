# wg launch benchmarks (2026-10-20)

Comparison-style measurements for launch tweets and chart cards. One campaign
on the authorized shared Debian 13 VPS (4 vCPU, 7.8 GiB RAM). Raw per-sample
records are retained on that VPS.

## Scope and identities

| Item | Value |
| --- | --- |
| Machine | `v2202610434069536057`, Debian 13, x86_64, 4 vCPU, 7.8 GiB |
| Corpus | copy of `/srv/workspaces/wispd/prof/src` |
| Corpus files (wg) | 13,332 files, 63,786 semantic chunks |
| Corpus size | 186 MB, 13,448 total files in the copied tree |
| wg | `/srv/projects/wispd/wg-release-20261018/crates/wgrep-sgrep-core/target/release/wg` |
| wg SHA-256 | `b6bf371c7a3be0c7eef6cb5128cacf9f1372126018af7a94cf8759d1223b1fcc` |
| sgrep control | revision `0fbe8e5725f71667789be4a3abcea4e02f33afa4` |
| sgrep SHA-256 | `715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217` |
| rg | 15.1.0, `/srv/workspaces/wispd/tools/rg-15/rg` |
| Model | `minishlab/potion-code-16M-v2`, revision `e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b`, `--model code` |
| Cache mode | `HF_HUB_OFFLINE=1`, cached model and parser assets |
| Daemon state | isolated `WG_SERVICE_DIR` per phase, all task-owned daemons stopped |

The sgrep index cache used a 4 GiB cap (`SGREP_CACHE_MAX_BYTES`) so that one
index per repository stayed on disk. Without that cap sgrep evicts old indexes
and the multi-repo measurement silently turns into repeated index builds.

## Method

- One campaign. Warmups are excluded from every distribution.
- Every subprocess has a timeout and a checked return code.
- Latency includes process start, loopback IPC and daemon work. It is not
  daemon-internal scoring time.
- RSS is `VmRSS` from `/proc/<pid>/status`. wg RSS includes daemon children.
  sgrep RSS is the sum of live client processes; the multi-repo row sums one
  process per repository.
- Freshness starts when the file write returns and stops at the first query
  response that contains the new symbol. wg polls every 10 ms. sgrep runs its
  cached query; sgrep detected the change and rebuilt its index by itself in
  6 of 6 samples.
- Text cells run `wg -F PATTERN .` and `rg -F PATTERN .` in the corpus
  directory, with `RIPGREP_CONFIG_PATH` unset. Outputs are compared as exact
  bytes and as sorted lines. Two identical `rg` runs produced different byte
  order, so exact-byte order is not a stable property of `rg`.
- The eight demo questions were selected first, then each answer location was
  verified by hand with `rg`, then wg and keyword `rg` were run. The reference
  is the cited `file:line`, not a chunk chosen after the result.

## M1. Freshness: one-file edit to first correct response

A probe file was added to a copy of the 13,332-file tree. Each sample rewrote
it with a new symbol, then polled until the response contained that symbol.

| Tool | n | p50 | p95 | min | max | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| wg live daemon | 30 | 222.9 ms | 241.8 ms | 208.5 ms | 245.9 ms | 0 |
| sgrep CLI, cached index | 6 | 20,249.3 ms | 20,826.3 ms | 19,785.0 ms | 20,854.4 ms | 0 |
| Ratio | | **90.9x** | | | | |

sgrep has no watcher. Its next query rebuilds the whole index (6 of 6 samples).
The wg update is incremental. This is a product-level difference, not only an
index-speed difference.

## M2. Warm semantic query latency

120 interleaved samples per tool over five queries:
`parse Rust expressions`, `emit compiler error diagnostics`,
`validate command line options`, `resolve module imports`,
`incremental compilation query dependency tracking`. Both tools used warm
caches and `--model code`, `--top 10` for wg, `-n 10` for sgrep.

| Tool | n | p50 | p95 | min | max | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| wg | 120 | 146.6 ms | 168.6 ms | 123.3 ms | 175.2 ms | 0 |
| sgrep CLI | 120 | 494.6 ms | 535.1 ms | 455.5 ms | 549.1 ms | 0 |
| Ratio wg / sgrep | | **0.296** (wg 3.37x faster) | | | | |

Top-k parity: 5 of 5 queries returned identical ordered rows and identical
scores (exact float equality, not a tolerance). See
`raw/rank_parity.json`.

## M3. Agent fan-out

Fixed 10 s windows, mixed semantic queries, 1, 4 and 16 concurrent client
processes. sgrep runs one CLI process per client; wg clients share one daemon.

| Clients | wg q/s | wg p50 | wg p95 | wg errors / timeouts | wg daemon RSS peak | sgrep q/s | sgrep p50 | sgrep p95 | sgrep errors / timeouts | sgrep peak concurrent RSS (sum) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 6.79 | 149.3 ms | 164.9 ms | 0 / 0 | 803.4 MiB | 2.02 | 494.6 ms | 519.9 ms | 0 / 0 | 269.2 MiB |
| 4 | 25.28 | 156.5 ms | 181.5 ms | 0 / 0 | 803.4 MiB | 6.21 | 635.1 ms | 691.5 ms | 0 / 0 | 998.8 MiB |
| 16 | 24.11 | 659.2 ms | 717.0 ms | 0 / 0 | 803.4 MiB | 6.22 | 2,464.7 ms | 3,006.7 ms | 0 / 0 | 2,147.0 MiB |

wg throughput saturates at about 24 queries/s on 4 vCPU. At 16 clients wg has
3.88x the throughput and less than one third of the p50 latency. The wg daemon
keeps the same RSS at every client count. The sgrep column is the observed peak
of the sum of live processes. If all 16 processes peaked at once at the
single-process warm peak (269 MiB), the upper bound would be about 4.3 GiB; the
observed peak of the live sum was 2.1 GiB.

## M4. Multi-repo memory

Copies of the same corpus were attached to one wg daemon, or served by one
sgrep process each. sgrep indexes were warm and retained (4 GiB cap).

| Repos | wg RSS idle | wg RSS peak during queries | sgrep sum peak | sgrep per-process peak sum | sgrep errors |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 803.9 MiB | 803.9 MiB | 238.4 MiB | 238.4 MiB | 0 |
| 3 | 1,719.4 MiB | 1,719.4 MiB | 714.2 MiB | 723.0 MiB | 0 |
| 5 | 2,609.3 MiB | 2,609.3 MiB | 1,188.8 MiB | 1,287.1 MiB | 0 |

This result does **not** favor wg. One daemon loads one model, but its
per-repository index RSS is larger than a warm sgrep process. At five repos wg
uses about 2.2x the RSS. Do not claim a multi-repo memory win from this run.
sgrep processes are transient and map their index; wg holds a live index and
watcher state in anonymous memory.

## M5. Text-route overhead

| Pattern | wg n | wg p50 | wg p95 | rg p50 | rg p95 | wg / rg p50 | Output |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| absent literal | 30 | 45.6 ms | 58.1 ms | 54.5 ms | 62.2 ms | 0.84 | identical (0 bytes) |
| common identifier `parse` | 30 | 60.7 ms | 68.1 ms | 61.1 ms | 68.8 ms | 0.99 | same 5,835 lines / 1,789,315 B; exact order differs; sorted lines identical |

The text route is not slower than direct `rg` in this run. `wg` runs a real
`rg` child, so this is consistency, not acceleration. The exact-byte mismatch
is traversal order: two direct `rg` runs also differ in order.

## M6. Natural-language questions: wg vs keyword `rg`

`top1` and `top5` below mean the answer **file** appears in the top-1 or top-5
wg rows. `line` means the cited line is inside a returned chunk.

| ID | Question | Verified answer | wg top-1 path | wg top-5 path | wg top-5 line | rg matches | rg files | rg has answer file |
| --- | --- | --- | :---: | :---: | :---: | ---: | ---: | :---: |
| q1 | How does the parser handle unary expressions in Rust code? | `tools/rust-analyzer/crates/parser/src/grammar/expressions.rs:412` | yes | yes | yes | 77,105 | 6,315 | yes |
| q2 | Where does rustfmt decide to lay out a method chain vertically? | `tools/rustfmt/src/chains.rs:709` | yes | yes | no | 58,486 | 5,472 | yes |
| q3 | How does clippy find unnecessary clone calls on Copy types? | `tools/clippy/clippy_lints/src/methods/clone_on_copy.rs:61` | no | no | no | 64,241 | 6,667 | yes |
| q4 | How does miri implement the Stacked Borrows aliasing model? | `tools/miri/src/borrow_tracker/stacked_borrows/mod.rs:467` | no | yes | no | 16,050 | 3,019 | yes |
| q5 | Where does rustdoc build the search index for documentation pages? | `librustdoc/html/render/search_index.rs:1267` | no | no | no | 44,707 | 2,864 | yes |
| q6 | How does bootstrap download a prebuilt LLVM from CI? | `bootstrap/src/core/download.rs:356` | no | yes | yes | 11,064 | 768 | yes |
| q7 | Where does compiletest compare the compiler output against expected files? | `tools/compiletest/src/runtest.rs:2488` | yes | yes | no | 28,457 | 2,667 | yes |
| q8 | How does rust-analyzer resolve a module import during name resolution? | `tools/rust-analyzer/crates/hir-def/src/nameres/path_resolution.rs:154` | yes | yes | no | 92,184 | 4,416 | yes |

Totals:

| Metric | Result |
| --- | ---: |
| wg top-1 path hits | 4 / 8 |
| wg top-5 path hits | 6 / 8 |
| wg top-5 exact-line hits | 2 / 8 |
| rg searches that include the answer file | 8 / 8 |
| rg total matches over 8 questions | 392,294 |
| rg match range per question | 11,064 to 92,184 |

The keyword search includes the answer file every time, but the answer is one
file among hundreds to thousands. wg returns the file in the top five for 6 of
8. It misses q3 (the lint source) and q5 (the code that builds the search
index). Those misses are real; do not show them as wins.

Exact commands:

```sh
wg "<question>" <corpus> --model code --json --explain --top 10
rg -i -F -e <word1> -e <word2> ... --count-matches .
```

Keywords are the question words after removing stopwords and words shorter than
three characters. The patterns are ORed with one `-e` per word. Exact argv, wg
JSON and rg output are retained per question.

## Tweetable headlines

Each headline cites its table and its caveat. Use the caveat, not only the
number.

1. **"Edit to answer: wg 0.22 s vs sgrep 20.2 s (91x)."** [M1] Caveat: sgrep is
   a CLI with no watcher and rebuilds the whole index; one 4-vCPU VPS; wg n=30,
   sgrep n=6.
2. **"Warm semantic query: wg 147 ms vs sgrep 495 ms p50 (3.4x)."** [M2]
   Caveat: same corpus and model, five-query mixture, one machine; sgrep pays
   process start and model/index load each call.
3. **"Warm top-10 results are identical: 5/5 queries, exact scores."** [M2]
   Caveat: this proves ranking parity on this corpus, not large-corpus quality.
4. **"16 agents: wg 24.1 queries/s vs 6.2 (3.9x); p50 0.66 s vs 2.46 s; zero
   errors."** [M3] Caveat: 10 s windows on 4 shared vCPU; wg saturates near
   24 queries/s.
5. **"One daemon serves 16 agents at a constant 0.80 GiB RSS."** [M3] Caveat:
   the clients and the corpus are on the same machine; this is resident RSS,
   not peak virtual memory.
6. **"Text route is not slower: 46 ms vs 54 ms absent, 61 ms vs 61 ms common;
   same lines."** [M5] Caveat: wg runs a real rg child; exact line order differs
   because rg traversal order is not stable.
7. **"Natural-language demo: right file in top-5 for 6/8 hand-checked
   questions, while keyword rg returns 11k-92k matches."** [M6] Caveat: exact
   line in top-5 only 2/8; wg misses q3 and q5; rg included the answer file for
   8/8 among thousands of matches.
8. **Do not tweet a multi-repo memory win.** [M4] Five roots on one wg daemon
   use 2.6 GiB RSS; five warm sgrep processes use 1.2 GiB. One shared model is
   true, but per-root index RSS dominates on this corpus.

## Limits

- One final campaign, one shared VPS. Load and page cache can move the numbers.
  A first full run is retained as run1 but is not used: its sgrep cache used the
  default size, so per-repo indexes were evicted and the multi-repo sgrep arm
  rebuilt indexes. The final run set a 4 GiB cap. Only the final run's numbers
  appear here and in the results JSON.
- sgrep is a CLI, not a server. Every sgrep call includes process start and
  asset/index load. This is the real end-to-end CLI cost, but it is not an
  architecture-to-architecture comparison.
- The freshness comparison is the most product-shaped result and the least
  symmetric: sgrep rebuilds its index after a change; wg updates incrementally.
- Warm semantic p50/p95 are declared mixtures over five queries, not per-query
  percentiles. Per-query samples are in `raw/warm_semantic.jsonl`.
- Freshness p95 for sgrep rests on 6 samples. The wg row has 30 samples.
- RSS is resident memory sampled at 50-100 ms. sgrep maps its index; wg holds
  its index and watcher state in anonymous memory. These are not identical
  accounting models.
- The multi-repo sgrep number is the sum of one transient process per repo, not
  a resident server. The multi-repo result is unfavorable to wg.
- The corpus is 7,773 `.rs` files inside a 13,332-file rg-visible tree. The
  semantic index has 63,786 chunks. Other corpora and the `auto` model can rank
  differently.
- The text route uses an authoritative `rg` child. Do not attach the lexical
  `wgrep` trigram speedups to wg.
- No commit was made. The wg binary is the release-tree build at the SHA above.

## Receipts

VPS (final campaign, `campaign2`):

- Work and copies: `/srv/projects/wispd/wg-launch-bench-20261020/campaign2/`
- Log: `/srv/logs/wispd/wg-launch-bench-20261020/campaign2.log`
- Flat results: `/srv/projects/wispd/wg-launch-bench-20261020/WG_LAUNCH_BENCH_RESULTS.json`
- Raw per-sample records: `campaign2/raw/*.jsonl`
  - `warm_semantic.jsonl` (240 records)
  - `freshness.jsonl` (36 records)
  - `fanout.jsonl` and `fanout_requests.jsonl` (736 request records)
  - `multirepo.jsonl` (6 records)
  - `text_route.jsonl` (120 records)
  - `demo.jsonl` (8 records)
  - `commands.jsonl` (1,287 commands with argv, exit, timeout, elapsed)
- Rank parity: `campaign2/raw/rank_parity.json`
- Text parity: `campaign2/raw/text_parity.json`
- Demo outputs: `campaign2/raw/demo/q1-wg.json` ... `q8-wg.json`,
  `q1-rg.txt` ... `q8-rg.txt`, and `q1-rg.argv.json` ... `q8-rg.argv.json`
- Superseded first run: `WG_LAUNCH_BENCH_RESULTS.run1.json` and `campaign/`.
  It is kept only for audit; the numbers in this document come from run 2.

Local repository:

- Harness: `bench/WG_LAUNCH_BENCH.py`
- Flat machine-readable numbers: `bench/WG_LAUNCH_BENCH_RESULTS.json`
- This document: `bench/WG_LAUNCH_BENCH.md`

## Reproduce

```sh
ssh vps
cd /srv/projects/wispd/wg-launch-bench-20261020
python3 WG_LAUNCH_BENCH.py --self-test
python3 WG_LAUNCH_BENCH.py \
  --work /srv/projects/wispd/wg-launch-bench-20261020/campaign3 \
  --out /srv/projects/wispd/wg-launch-bench-20261020/WG_LAUNCH_BENCH_RESULTS.run3.json
```

The harness copies the corpus, prepares the cached sgrep assets, sets
`HF_HUB_OFFLINE=1` and `SGREP_CACHE_MAX_BYTES`, isolates every `WG_SERVICE_DIR`,
and stops only the daemons it started.
