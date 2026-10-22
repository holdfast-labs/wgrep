# wg launch benchmarks (2026-10-21, Phase D)

This campaign measures the final single-binary `wg`: sgrep-identical semantic
ranking plus the built-in ripgrep 15.1 text route and its trigram index in one
daemon. One final campaign ran on a quiet Debian 13 VPS. Raw per-sample records
are retained on that VPS.

> **This document supersedes the 2026-10-20 campaign.** That run used a
> multi-crate release build and an external `rg` child. Do not mix its numbers
> with these.

## Scope and identities

| Item | Value |
| --- | --- |
| Machine | `v2202610434069536057`, Debian 13, x86_64, 4 vCPU, 7.8 GiB, no swap |
| Corpus | copy of `/srv/workspaces/wispd/prof/src` |
| Corpus files | 13,448 files, 148.7 MB |
| Corpus (wg semantic) | 13,332 files, 63,786 chunks |
| Corpus (wg text index) | 13,289 files, 20.4 MB on disk |
| wg source commit | `ec861dc0666759b3de945deb58949aeb9fbb4aed` |
| wg source archive SHA-256 | `c7ee47ba5fd6b177b64f45c8d5fd48b536226339369f307a577c6457ae2fefb1` |
| wg binary SHA-256 | `412f694f802d8376c3768c1ee90620d18b772e0cd0f414db4baa3f995117e2a8` |
| sgrep control | revision `0fbe8e5725f71667789be4a3abcea4e02f33afa4` |
| sgrep SHA-256 | `715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217` |
| rg | 15.1.0 (rev `af60c2de9d`), `/srv/workspaces/wispd/tools/rg-15/rg` |
| rg SHA-256 | `ebeaf56f8a25e102e9419933423738b3a2a613a444fd749d695e15eba53f71f2` |
| Model | `minishlab/potion-code-16M-v2`, `--model code` |
| Cache mode | `HF_HUB_OFFLINE=1`, cached model and parser assets |
| Isolation | one `WG_SERVICE_DIR` and one `WGREP_INDEX_DIR` per arm |

Build: `git archive HEAD` into a fresh directory, then
`cargo build --locked --release --bin wg -j2`. The machine load average was
0.52 at start and no other measurement job ran.

The sgrep index cache used a 4 GiB cap (`SGREP_CACHE_MAX_BYTES`) so that one
index per repository stayed on disk.

## Method

- One final campaign (`campaign2`). Warmups are excluded from every
  distribution. Run1 is kept only for audit; a harness argument bug aborted its
  text phase, so no run1 number appears here.
- Every subprocess has a timeout and a checked return code.
- Latency includes process start, loopback IPC and daemon work. It is not
  daemon-internal scoring time.
- RSS is `VmRSS` from `/proc/<pid>/status`. wg RSS includes daemon children.
  sgrep RSS is the sum of live client processes.
- Freshness starts when the file write returns and stops at the first query
  response that contains the new symbol. wg polls every 10 ms. sgrep detected
  the change and rebuilt its index by itself in 6 of 6 samples.
- Text cells run `wg PATTERN .` and `rg PATTERN .` in the corpus directory with
  `RIPGREP_CONFIG_PATH` unset. `wg` uses its built-in ripgrep 15.1.
- Text parity uses `--sort path --color never` and compares exact bytes. A
  `WGREP_INDEX_EVENTS` file records the path that each `wg` call used.

## M1. Freshness: one-file edit to first correct response

| Tool | n | p50 | p95 | min | max | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| wg live daemon | 30 | 90.6 ms | 92.9 ms | 84.0 ms | 96.2 ms | 0 |
| sgrep CLI, cached index | 6 | 20,092.1 ms | 21,596.1 ms | 19,181.6 ms | 21,723.4 ms | 0 |
| Ratio sgrep / wg | | **221.6x** | | | | |

sgrep has no watcher. Its next query rebuilt the whole index in 6 of 6 samples.
The wg update is incremental. This is a product-level difference, not only an
index-speed difference.

## M2. Warm semantic query latency

120 interleaved samples per tool over five queries:
`parse Rust expressions`, `emit compiler error diagnostics`,
`validate command line options`, `resolve module imports`,
`incremental compilation query dependency tracking`. Both tools used warm
caches and `--model code`, `--top 10` for wg and `-n 10` for sgrep.

| Tool | n | p50 | p95 | min | max | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| wg | 120 | 23.2 ms | 26.6 ms | 21.3 ms | 29.7 ms | 0 |
| sgrep CLI | 120 | 502.3 ms | 529.8 ms | 472.8 ms | 533.3 ms | 0 |
| Ratio wg / sgrep | | **0.046** (wg 21.7x faster) | | | | |

Top-10 parity: 5 of 5 queries returned identical ordered rows and identical
scores (exact float equality, not a tolerance). See
`campaign2/raw/rank_parity.json`.

## M3. Agent fan-out

Fixed 10 s windows, mixed semantic queries, 1, 4 and 16 concurrent client
processes. sgrep runs one CLI process per client; wg clients share one daemon.

| Clients | wg q/s | wg p50 | wg p95 | wg errors / timeouts | wg daemon RSS peak | sgrep q/s | sgrep p50 | sgrep p95 | sgrep errors / timeouts | sgrep peak concurrent RSS (sum) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 43.04 | 22.8 ms | 25.0 ms | 0 / 0 | 425.7 MiB | 1.94 | 510.7 ms | 542.7 ms | 0 / 0 | 269.0 MiB |
| 4 | 151.50 | 25.3 ms | 32.0 ms | 0 / 0 | 427.9 MiB | 5.68 | 677.3 ms | 789.4 ms | 0 / 0 | 996.4 MiB |
| 16 | 148.97 | 105.3 ms | 122.0 ms | 0 / 0 | 431.5 MiB | 5.68 | 2,622.9 ms | 3,683.2 ms | 0 / 0 | 2,282.8 MiB |

wg saturates near 149 queries/s on 4 vCPU. At 16 clients wg has 26.2x the
throughput and 24.9x lower p50 latency, with zero errors. The wg daemon keeps
about 430 MiB RSS at every client count. The sgrep column is the observed peak
of the sum of live processes.

## M4. Multi-repo memory

Copies of the same corpus were attached to one wg daemon, or served by one
sgrep process each. sgrep indexes were warm and retained (4 GiB cap).

| Repos | wg RSS idle | wg RSS query peak | sgrep sum peak | sgrep per-process peak sum | errors |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 395.7 MiB | 395.7 MiB | 237.9 MiB | 237.9 MiB | 0 |
| 3 | 923.8 MiB | 923.8 MiB | 547.3 MiB | 807.0 MiB | 0 |
| 5 | 1,447.5 MiB | 1,447.5 MiB | 1,192.0 MiB | 1,269.0 MiB | 0 |

This result still does not favor wg, but the gap is smaller than in the
2026-10-20 run. At five roots wg uses 1.21x the sgrep sum peak and 1.14x the
per-process peak sum. Do not claim a multi-repo memory win.

## T1. Text route: one query at a time

Five cases on the Rust corpus. Literal cases use `-F`; the broad case uses `-n`
to stay on the text route; case-insensitive uses `-i -F`. n = 30 per tool per
case. All five cases used the daemon index (`used="daemon"`), had one search
event each, and were byte-identical to rg under `--sort path`.

| Case | Pattern | wg p50 | wg p95 | rg p50 | rg p95 | wg / rg p50 | Output bytes / lines | Index |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| Absent literal | `zzqxjwvbnm_wg_text_absent_zzz` | 4.9 ms | 5.8 ms | 52.4 ms | 65.4 ms | 0.09 | 0 / 0 | daemon |
| Rare identifier | `genericargs` | 5.2 ms | 6.4 ms | 52.5 ms | 61.0 ms | 0.10 | 107 / 1 | daemon |
| Common identifier | `let` | 41.8 ms | 53.6 ms | 67.9 ms | 86.3 ms | 0.62 | 16,197,218 / 136,359 | daemon |
| Broad regex | `\w+\(` | 96.6 ms | 128.2 ms | 110.8 ms | 126.3 ms | 0.87 | 45,694,788 / 414,206 | daemon |
| Case-insensitive | `-i -F error` | 31.7 ms | 37.5 ms | 62.4 ms | 75.8 ms | 0.51 | 5,321,683 / 34,567 | daemon |

The absent and rare cases show the index prune. Common, broad and
case-insensitive cases still scan much of the tree; the index removes only
impossible candidates. The text route runs an in-process ripgrep 15.1 engine,
so this is not a separate `rg` child.

## T2. Text route: concurrent clients

Fixed mix of four queries (absent, rare, common, broad). Each client ran four
queries; one rep. Latency is per query, including process start and IPC.
Throughput is completed queries divided by makespan. n is the number of
requests in the arm.

| Clients | wg q/s | wg p50 | wg p95 | wg n | wg errors | rg q/s | rg p50 | rg p95 | rg n | rg errors |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 27.8 | 22.7 ms | 86.4 ms | 4 | 0 | 14.6 | 57.1 ms | 104.7 ms | 4 | 0 |
| 4 | 29.9 | 102.5 ms | 335.0 ms | 16 | 0 | 15.1 | 240.4 ms | 447.3 ms | 16 | 0 |
| 16 | 29.9 | 293.5 ms | 1,423.9 ms | 64 | 0 | 16.0 | 811.4 ms | 1,738.3 ms | 64 | 0 |
| 32 | 31.5 | 835.3 ms | 2,732.8 ms | 128 | 0 | 16.1 | 1,528.0 ms | 3,505.2 ms | 128 | 0 |

wg saturates near 31 text queries/s on 4 vCPU, about 1.9-2.0x rg. All clients
share one daemon and one index. The 1-client row has only 4 requests, so its
p95 is weak; treat it as a throughput point.

## S1. Setup: `wg up`

Each tree used a fresh service directory and index directory. Peak RSS was
sampled at 50 ms. Steady RSS was sampled 2 s after the ready reply. The method
matches `bench/WG_TEXT_LARGE_UP.py`, folded into the launch harness.

| Corpus | Files | Semantic chunks | `wg up` | Text build | Peak RSS | Steady RSS | Text index |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Rust `prof/src` | 13,332 | 63,786 | 18.1 s | 1.48 s | 634.9 MiB | 359.4 MiB | 13,289 files, 20.4 MB |
| Linux 7.2.9 | 94,318 | 1,088,574 | 592.3 s (9m 52s) | 28.2 s | 3,905.7 MiB | 3,387.7 MiB | 94,316 files, 247.0 MB |

The Linux tree now reaches ready on this 7.8 GiB machine. The earlier campaign
was OOM-killed on the same tree. Steady RSS is below 4 GiB after ready.
A warm semantic query touches mapped vector pages, so later RSS can be higher
until the kernel reclaims them.

## Tweetable headlines

Each headline cites its table row and its caveat. Use the caveat, not only the
number.

1. **"Edit to answer: wg 91 ms vs sgrep 20.1 s p50 (222x)."** [M1] Caveat:
   sgrep is a CLI with no watcher and rebuilds the whole index; one 4-vCPU VPS;
   wg n=30, sgrep n=6.
2. **"Warm semantic query: wg 23 ms vs sgrep 502 ms p50 (21.7x)."** [M2]
   Caveat: same corpus and model, five-query mixture, one machine; sgrep pays
   process start and model/index load each call.
3. **"Warm top-10 results are identical: 5/5 queries, exact scores."** [M2]
   Caveat: this proves ranking parity on this corpus, not large-corpus quality.
4. **"16 agents: wg 149 queries/s vs 5.7 (26.2x); p50 105 ms vs 2.62 s; zero
   errors."** [M3] Caveat: 10 s windows on 4 shared vCPU; wg saturates near
   149 queries/s.
5. **"One daemon serves 16 agents at 431.5 MiB RSS."** [M3] Caveat: the clients
   and the corpus are on the same machine; this is resident RSS, not virtual
   memory.
6. **"Text search with the index: absent 4.9 ms vs 52.4 ms (10.7x); rare
   5.2 ms vs 52.5 ms (10.1x); common 41.8 ms vs 67.9 ms; broad 96.6 ms vs
   110.8 ms; case-insensitive 31.7 ms vs 62.4 ms."** [T1] Caveat: wg runs an
   in-process rg 15.1 engine; direct rg pays process start; all five measured
   cases were byte-identical; the index only prunes impossible candidates.
7. **"32 text clients: wg 31.5 queries/s vs rg 16.1 (2.0x), zero errors."**
   [T2] Caveat: fixed four-query mixture, one rep, 4 vCPU; wg saturates near
   31 queries/s.
8. **Do not tweet a multi-repo memory win.** [M4] Five roots on one wg daemon
   use 1,447.5 MiB RSS; five warm sgrep processes use 1,192.0 MiB (1.21x more
   for wg). One shared model is true, but per-root index RSS still dominates.
9. **"The 1.62 GB Linux tree now attaches: `wg up` 9m 52s, 3,905.7 MiB peak,
   3,387.7 MiB steady."** [S1] Caveat: one 4-vCPU VPS with no swap; the earlier
   build was OOM-killed; the time is CPU bound.

## Limits

- One final campaign, one shared VPS. Load and page cache can move the numbers.
  Run1 is retained only for audit and is not used.
- sgrep is a CLI, not a server. Every sgrep call includes process start and
  asset/index load. This is the real end-to-end CLI cost, but it is not an
  architecture-to-architecture comparison.
- The freshness comparison is the most product-shaped result and the least
  symmetric: sgrep rebuilds its index after a change; wg updates incrementally.
- Warm semantic p50/p95 are declared mixtures over five queries, not per-query
  percentiles.
- Freshness p95 for sgrep rests on 6 samples. The wg row has 30 samples.
- RSS is resident memory. sgrep maps its index; wg holds its index and watcher
  state in anonymous memory and maps the base vectors. These are not identical
  accounting models. Mapped vector pages count in RSS after a query touches
  them, but they are reclaimable.
- The multi-repo sgrep number is the sum of one transient process per repo, not
  a resident server. The multi-repo result remains unfavorable to wg.
- Text parity is exact for the five measured cases with `--sort path`. Unsorted
  ripgrep traversal order is not stable, so do not require byte order without
  `--sort path`.
- The 1-client text concurrency row has 4 requests. Its p95 is not a stable
  percentile.
- The corpus is 7,773 `.rs` files inside a 13,332-file rg-visible tree. Other
  corpora and the `auto` model can rank differently.
- No commit was made.

## Receipts

VPS (final campaign, `campaign2`):

- Work and copies: `/srv/projects/wispd/wg-d-20261021/campaign2/`
- Log: `/srv/logs/wispd/wg-d-20261021/campaign2.log`
- Build log: `/srv/logs/wispd/wg-d-20261021/build.log`
- Flat results: `/srv/projects/wispd/wg-d-20261021/campaign2/results.json`
- Raw per-sample records: `campaign2/raw/`
  - `warm_semantic.jsonl` (240 records)
  - `freshness.jsonl` (36 records)
  - `fanout.jsonl` and `fanout_requests.jsonl`
  - `multirepo.jsonl` (6 records)
  - `text_route.jsonl` (300 records)
  - `text_concurrency.jsonl` and `text_concurrency_requests.jsonl`
  - `setup.jsonl` (2 records)
  - `commands.jsonl` (every subprocess with argv, exit, timeout, elapsed)
- Rank parity: `campaign2/raw/rank_parity.json`
- Text parity and index mode: `campaign2/raw/text_parity.json`,
  `campaign2/raw/events/`
- Superseded run1: `/srv/projects/wispd/wg-d-20261021/campaign/` (audit only)

Local repository:

- Harness: `bench/WG_LAUNCH_BENCH.py`
- Flat machine-readable numbers: `bench/WG_LAUNCH_BENCH_RESULTS.json`
- This document: `bench/WG_LAUNCH_BENCH.md`

## Reproduce

```sh
ssh vps
cd /srv/projects/wispd/wg-d-20261021
source ~/.cargo/env
cargo build --locked --release --bin wg -j2
python3 bench/WG_LAUNCH_BENCH.py --self-test
python3 bench/WG_LAUNCH_BENCH.py \
  --wg /srv/projects/wispd/wg-d-20261021/target/release/wg \
  --sgrep /srv/projects/wispd/sgrep-control-0fbe8e5/target/release/sgrep \
  --rg /srv/workspaces/wispd/tools/rg-15/rg \
  --commit ec861dc0666759b3de945deb58949aeb9fbb4aed \
  --source-sha256 c7ee47ba5fd6b177b64f45c8d5fd48b536226339369f307a577c6457ae2fefb1 \
  --work /srv/projects/wispd/wg-d-20261021/campaign3 \
  --out /srv/projects/wispd/wg-d-20261021/campaign3/results.json
```

The harness copies the corpus, prepares the cached sgrep assets, sets
`HF_HUB_OFFLINE=1` and `SGREP_CACHE_MAX_BYTES`, isolates every `WG_SERVICE_DIR`
and `WGREP_INDEX_DIR`, and stops only the daemons it started. Use
`--skip-linux-setup` to omit the kernel tree.
