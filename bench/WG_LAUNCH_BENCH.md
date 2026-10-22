# wg launch benchmarks (2026-10-22, Phase E)

This campaign measures the final single-binary `wg`: sgrep-identical semantic
ranking plus the built-in ripgrep 15.1 text route and its trigram index in one
daemon. One final campaign ran on a quiet Debian 13 VPS. Raw per-sample records
are retained on that VPS.

> **This document supersedes the 2026-10-21 (Phase D) and 2026-10-20
> campaigns.** Phase E re-ran every section on commit `b245c56`, after chunk
> text moved into the memory-mapped snapshot (`e5450a1`, `102ea1a`). Do not mix
> numbers from different campaigns.

## Scope and identities

| Item | Value |
| --- | --- |
| Machine | `v2202610434069536057`, Debian 13, x86_64, 4 vCPU, 7.8 GiB, no swap |
| Campaign date | `2026-10-22T15:47:59Z` |
| Corpus | copy of `/srv/workspaces/wispd/prof/src` |
| Corpus files | 13,448 files, 148.7 MB |
| Corpus (wg semantic) | 13,332 files, 63,786 chunks |
| Corpus (wg text index) | 13,289 files, 20.4 MB on disk |
| wg source commit | `b245c563025933af3ccc72fc5abdfe1a48f56a55` |
| wg source archive SHA-256 | `c7dfeb2d312862104c9691833c95aca548abd09bdc0ca617e4762ddd2327ad89` |
| wg binary SHA-256 | `3334610bd49fd19d367c20302e3bb9eb634a4a4c62007cb0927163d9d9192db2` |
| sgrep control | revision `0fbe8e5725f71667789be4a3abcea4e02f33afa4` |
| sgrep SHA-256 | `715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217` |
| rg | 15.1.0 (rev `af60c2de9d`), `/srv/workspaces/wispd/tools/rg-15/rg` |
| rg SHA-256 | `ebeaf56f8a25e102e9419933423738b3a2a613a444fd749d695e15eba53f71f2` |
| Model | `minishlab/potion-code-16M-v2`, `--model code` |
| Cache mode | `HF_HUB_OFFLINE=1`, cached model and parser assets |
| Isolation | one `WG_SERVICE_DIR` and one `WGREP_INDEX_DIR` per arm |

Build: `git archive b245c56` into a fresh directory, then
`cargo build --locked --release --bin wg -j2`. The archive SHA-256 above is the
exact file that produced the binary. The machine load average was 0.00 before
the build, and no other heavy job ran during the campaign.

The sgrep index cache used a 4 GiB cap (`SGREP_CACHE_MAX_BYTES`) so that one
index per repository stayed on disk.

## Method

- One campaign (`campaign-e`). Warmups are excluded from every distribution.
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
| wg live daemon | 30 | 99.2 ms | 111.9 ms | 84.3 ms | 115.2 ms | 0 |
| sgrep CLI, cached index | 6 | 21,805.7 ms | 22,539.1 ms | 20,565.5 ms | 22,585.5 ms | 0 |
| Ratio sgrep / wg | | **219.8x** | | | | |

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
| wg | 120 | 25.2 ms | 29.8 ms | 21.5 ms | 32.9 ms | 0 |
| sgrep CLI | 120 | 565.7 ms | 624.5 ms | 503.1 ms | 672.0 ms | 0 |
| Ratio wg / sgrep | | **0.0446** (wg 22.4x faster) | | | | |

Top-10 parity: 5 of 5 queries returned identical ordered rows and identical
scores (exact float equality, not a tolerance). See
`campaign-e/raw/rank_parity.json`.

## M3. Agent fan-out

Fixed 10 s windows, mixed semantic queries, 1, 4 and 16 concurrent client
processes. sgrep runs one CLI process per client; wg clients share one daemon.

| Clients | wg q/s | wg p50 | wg p95 | wg errors / timeouts | wg daemon RSS peak | sgrep q/s | sgrep p50 | sgrep p95 | sgrep errors / timeouts | sgrep peak concurrent RSS (sum) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 39.96 | 24.6 ms | 26.7 ms | 0 / 0 | 289.4 MiB | 1.76 | 559.5 ms | 611.6 ms | 0 / 0 | 269.2 MiB |
| 4 | 141.69 | 27.1 ms | 34.0 ms | 0 / 0 | 294.4 MiB | 5.58 | 688.2 ms | 778.1 ms | 0 / 0 | 975.0 MiB |
| 16 | 140.94 | 111.4 ms | 128.7 ms | 0 / 0 | 295.2 MiB | 5.55 | 2,767.7 ms | 3,530.5 ms | 0 / 0 | 2,119.0 MiB |

wg saturates near 141 queries/s on 4 vCPU. At 16 clients wg has 25.4x the
throughput and 24.9x lower p50 latency, with zero errors. The wg daemon keeps
about 295 MiB RSS at every client count. The sgrep column is the observed peak
of the sum of live processes.

## M4. Multi-repo memory

Copies of the same corpus were attached to one wg daemon, or served by one
sgrep process each. sgrep indexes were warm and retained (4 GiB cap).

| Repos | wg RSS idle | wg RSS query peak | sgrep sum peak | sgrep per-process peak sum | errors |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 282.6 MiB | 282.6 MiB | 249.2 MiB | 249.2 MiB | 0 |
| 3 | 606.9 MiB | 606.9 MiB | 499.8 MiB | 745.3 MiB | 0 |
| 5 | 902.9 MiB | 902.9 MiB | 1,034.1 MiB | 1,308.1 MiB | 0 |

Memory changed materially since Phase D. At five roots one wg daemon now uses
902.9 MiB, against 1,034.1 MiB for the sgrep sum peak (wg uses 0.87x, or 12.7%
less). The wg daemon also stays below the sgrep per-process peak sum at five
roots (0.69x) and at three roots (0.81x).

This is not a general multi-repo win. At one root wg uses 1.13x the sgrep sum
peak. At three roots wg uses 1.21x the sgrep sum peak, though it is below the
sgrep per-process peak sum. Claim the five-root crossing only, and state the
repo count with it. Chunk text now lives in the memory-mapped snapshot, so
mapped file pages grow RSS after a query touches them, and the kernel can
reclaim those pages.

## T1. Text route: one query at a time

Five cases on the Rust corpus. Literal cases use `-F`; the broad case uses `-n`
to stay on the text route; case-insensitive uses `-i -F`. n = 30 per tool per
case. All five cases used the daemon index (`used="daemon"`), had one search
event each, and were byte-identical to rg under `--sort path`.

| Case | Pattern | wg p50 | wg p95 | rg p50 | rg p95 | wg / rg p50 | Output bytes / lines | Index |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| Absent literal | `zzqxjwvbnm_wg_text_absent_zzz` | 5.4 ms | 6.5 ms | 63.2 ms | 78.5 ms | 0.09 | 0 / 0 | daemon |
| Rare identifier | `genericargs` | 6.0 ms | 8.6 ms | 63.8 ms | 72.4 ms | 0.09 | 107 / 1 | daemon |
| Common identifier | `let` | 50.0 ms | 55.7 ms | 83.7 ms | 97.3 ms | 0.60 | 16,197,218 / 136,359 | daemon |
| Broad regex | `\w+\(` | 111.5 ms | 132.0 ms | 136.1 ms | 151.9 ms | 0.82 | 45,694,788 / 414,206 | daemon |
| Case-insensitive | `-i -F error` | 37.5 ms | 44.2 ms | 75.7 ms | 85.3 ms | 0.50 | 5,321,683 / 34,567 | daemon |

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
| 1 | 21.9 | 29.1 ms | 109.4 ms | 4 | 0 | 11.8 | 76.9 ms | 118.7 ms | 4 | 0 |
| 4 | 28.1 | 70.7 ms | 410.7 ms | 16 | 0 | 14.1 | 248.7 ms | 501.7 ms | 16 | 0 |
| 16 | 26.0 | 340.1 ms | 1,727.1 ms | 64 | 0 | 12.6 | 1,017.5 ms | 2,258.2 ms | 64 | 0 |
| 32 | 26.1 | 728.4 ms | 3,331.5 ms | 128 | 0 | 14.5 | 1,761.6 ms | 3,798.9 ms | 128 | 0 |

wg saturates near 26 text queries/s on 4 vCPU, about 1.8-2.1x rg. All clients
share one daemon and one index. The 1-client row has only 4 requests, so its
p95 is weak; treat it as a throughput point.

## S1. Setup: `wg up`

Each tree used a fresh service directory and index directory. Peak RSS was
sampled at 50 ms. Steady RSS was sampled 2 s after the ready reply. The method
matches `bench/WG_TEXT_LARGE_UP.py`, folded into the launch harness.

| Corpus | Files | Semantic chunks | `wg up` | Text build | Peak RSS | Steady RSS | Text index |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Rust `prof/src` | 13,332 | 63,786 | 18.4 s | 1.61 s | 386.2 MiB | 212.7 MiB | 13,289 files, 20.4 MB |
| Linux 7.2.9 | 94,318 | 1,088,574 | 561.4 s (9m 21s) | 27.9 s | 2,774.1 MiB | 2,040.5 MiB | 94,316 files, 247.0 MB |

The Linux tree reaches ready on this 7.8 GiB machine. Streaming chunk text into
the snapshot lowered both peak and steady RSS against Phase D. Steady RSS is
about 2 GiB after ready. A warm semantic query touches mapped vector and chunk
pages, so later RSS can be higher until the kernel reclaims them.

## Tweetable headlines

Each headline cites its table row and its caveat. Use the caveat, not only the
number.

1. **"Edit to answer: wg 99 ms vs sgrep 21.8 s p50 (220x)."** [M1] Caveat:
   sgrep is a CLI with no watcher and rebuilds the whole index; one 4-vCPU VPS;
   wg n=30, sgrep n=6.
2. **"Warm semantic query: wg 25 ms vs sgrep 566 ms p50 (22.4x)."** [M2]
   Caveat: same corpus and model, five-query mixture, one machine; sgrep pays
   process start and model/index load each call.
3. **"Warm top-10 results are identical: 5/5 queries, exact scores."** [M2]
   Caveat: this proves ranking parity on this corpus, not large-corpus quality.
4. **"16 agents: wg 141 queries/s vs 5.5 (25.4x); p50 111 ms vs 2.77 s; zero
   errors."** [M3] Caveat: 10 s windows on 4 shared vCPU; wg saturates near
   141 queries/s.
5. **"One daemon serves 16 agents at 295.2 MiB RSS."** [M3] Caveat: the clients
   and the corpus are on the same machine; this is resident RSS, not virtual
   memory.
6. **"Text search with the index: absent 5.4 ms vs 63.2 ms (11.7x); rare
   6.0 ms vs 63.8 ms (10.6x); common 50.0 ms vs 83.7 ms; broad 111.5 ms vs
   136.1 ms; case-insensitive 37.5 ms vs 75.7 ms."** [T1] Caveat: wg runs an
   in-process rg 15.1 engine; direct rg pays process start; all five measured
   cases were byte-identical; the index only prunes impossible candidates.
7. **"32 text clients: wg 26.1 queries/s vs rg 14.5 (1.8x), zero errors."**
   [T2] Caveat: fixed four-query mixture, one rep, 4 vCPU; wg saturates near
   26 queries/s.
8. **"Five roots: one wg daemon uses 902.9 MiB vs 1,034.1 MiB for five warm
   sgrep processes (12.7% less)."** [M4] Caveat: this crossing is not general.
   At one root wg uses 282.6 MiB vs 249.2 MiB (1.13x more); at three roots
   606.9 MiB vs 499.8 MiB (1.21x more) on the sum peak. wg stays below the
   sgrep per-process peak sum at three and five roots. Mapped snapshot pages
   count in RSS after a query touches them and are reclaimable.
9. **"The Linux tree attaches: `wg up` 9m 21s, 2,774.1 MiB peak,
   2,040.5 MiB steady."** [S1] Caveat: one 4-vCPU VPS with no swap; the time is
   CPU bound.

## Limits

- One campaign, one shared VPS. Load and page cache can move the numbers.
- sgrep is a CLI, not a server. Every sgrep call includes process start and
  asset/index load. This is the real end-to-end CLI cost, but it is not an
  architecture-to-architecture comparison.
- The freshness comparison is the most product-shaped result and the least
  symmetric: sgrep rebuilds its index after a change; wg updates incrementally.
- Warm semantic p50/p95 are declared mixtures over five queries, not per-query
  percentiles.
- Freshness p95 for sgrep rests on 6 samples. The wg row has 30 samples.
- RSS is resident memory. sgrep maps its index; wg holds its index and watcher
  state in anonymous memory and maps the base vectors and the chunk text
  snapshot. These are not identical accounting models. Mapped pages count in
  RSS after a query touches them, but they are reclaimable.
- The multi-repo sgrep number is the sum of one transient process per repo, not
  a resident server. The five-root result favors wg, but the one-root and
  three-root results do not. Do not describe this as a general multi-repo win.
- Text parity is exact for the five measured cases with `--sort path`. Unsorted
  ripgrep traversal order is not stable, so do not require byte order without
  `--sort path`.
- The 1-client text concurrency row has 4 requests. Its p95 is not a stable
  percentile.
- The corpus is 7,773 `.rs` files inside a 13,332-file rg-visible tree. Other
  corpora and the `auto` model can rank differently.
- No commit was made.

## Receipts

VPS (campaign `campaign-e`):

- Work and copies: `/srv/projects/wispd/wg-e-20261022/campaign/`
- Log: `/srv/logs/wispd/wg-e-20261022/campaign.log`
- Build log: `/srv/logs/wispd/wg-e-20261022/build.log`
- Self-test log: `/srv/logs/wispd/wg-e-20261022/selftest.log`
- Binary hash receipt: `/srv/logs/wispd/wg-e-20261022/wg-binary.sha256`
- Flat results copy: `/srv/logs/wispd/wg-e-20261022/results.json`
- Flat results: `/srv/projects/wispd/wg-e-20261022/campaign/results.json`
- Raw per-sample records: `campaign/raw/`
  - `warm_semantic.jsonl` (240 records)
  - `freshness.jsonl` (36 records)
  - `fanout.jsonl` (6) and `fanout_requests.jsonl` (3,386)
  - `multirepo.jsonl` (6 records)
  - `text_route.jsonl` (300 records)
  - `text_concurrency.jsonl` (8) and `text_concurrency_requests.jsonl` (424)
  - `setup.jsonl` (2 records)
  - `commands.jsonl` (3,815; every subprocess with argv, exit, timeout, elapsed)
- Rank parity: `campaign/raw/rank_parity.json`
- Text parity and index mode: `campaign/raw/text_parity.json`,
  `campaign/raw/events/`

Local repository:

- Harness: `bench/WG_LAUNCH_BENCH.py`
- Flat machine-readable numbers: `bench/WG_LAUNCH_BENCH_RESULTS.json`
- This document: `bench/WG_LAUNCH_BENCH.md`

## Reproduce

```sh
ssh vps
cd /srv/projects/wispd/wg-e-20261022
source ~/.cargo/env
cargo build --locked --release --bin wg -j2
python3 bench/WG_LAUNCH_BENCH.py --self-test
python3 bench/WG_LAUNCH_BENCH.py \
  --wg /srv/projects/wispd/wg-e-20261022/target/release/wg \
  --sgrep /srv/projects/wispd/sgrep-control-0fbe8e5/target/release/sgrep \
  --rg /srv/workspaces/wispd/tools/rg-15/rg \
  --commit b245c563025933af3ccc72fc5abdfe1a48f56a55 \
  --source-sha256 c7dfeb2d312862104c9691833c95aca548abd09bdc0ca617e4762ddd2327ad89 \
  --work /srv/projects/wispd/wg-e-20261022/campaign2 \
  --out /srv/projects/wispd/wg-e-20261022/campaign2/results.json
```

The harness copies the corpus, prepares the cached sgrep assets, sets
`HF_HUB_OFFLINE=1` and `SGREP_CACHE_MAX_BYTES`, isolates every `WG_SERVICE_DIR`
and `WGREP_INDEX_DIR`, and stops only the daemons it started. Use
`--skip-linux-setup` to omit the kernel tree.
