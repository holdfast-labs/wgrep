# wg benchmarks

All numbers here come from retained raw records on a Debian 13 VPS (4 vCPU,
7.8 GiB RAM, no swap). They are single measurement runs, not
percentiles over many sessions or machines. Windows numbers are a separate
corpus copy and are not an apples-to-apples ratio.

Setup and search latency are different measurements. Do not infer one from the
other.

## Comparison with sgrep (2026-10-22)

`wg` is now one binary: a semantic ranker with an exact `--rank classic`
sgrep-parity mode, plus the built-in ripgrep 15.1 text route and trigram index
in one daemon. This campaign compares the final code on the same 13,332-file
Rust `src` corpus, model and machine. Full method, raw-record paths and caveats
are in [bench/WG_LAUNCH_BENCH.md](../bench/WG_LAUNCH_BENCH.md).

| Measure | wg | sgrep CLI | Result |
| --- | ---: | ---: | --- |
| One-file edit to first correct result, p50 | 99.2 ms | 21,806 ms | 220x faster. sgrep has no watcher and rebuilds its index. |
| Warm semantic query, p50 | 25.2 ms | 565.7 ms | 22.4x faster. Top-10 rows and scores identical on 5/5 queries in classic mode. |
| 16 concurrent clients, throughput | 140.9 q/s | 5.5 q/s | 25.4x. 0 errors or timeouts on either side. |
| Five attached repositories, RSS | 902.9 MiB | 1,034.1 MiB | wg uses 12.7% less. At 1 and 3 roots wg still uses more. |
| Text search vs direct `rg 15.1`, p50 | 5.4 / 6.0 / 50.0 / 111.5 / 37.5 ms | 63.2 / 63.8 / 83.7 / 136.1 / 75.7 ms (rg) | Faster on all five cases; byte-identical output. |

One final campaign on one 4 vCPU VPS. sgrep runs one process per query; wg
answers through its resident daemon. The text route is an in-process ripgrep
15.1 engine, not an external `rg` child.

## Setup: Rust `src` and Linux trees

Each tree used a fresh service directory and text-index directory. Peak RSS was
sampled at 50 ms. Steady RSS was sampled 2 s after ready.

| Corpus | Files | Semantic chunks | `wg up` | Text build | Peak RSS | Steady RSS | Text index |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Rust `prof/src` | 13,332 | 63,786 | 18.4 s | 1.61 s | 386.2 MiB | 212.7 MiB | 13,289 files, 20.4 MB |
| Linux 7.2.9 | 94,318 | 1,088,574 | 561.4 s | 27.9 s | 2,774.1 MiB | 2,040.5 MiB | 94,316 files, 247.0 MB |

The Linux tree reaches ready on this 7.8 GiB machine. Streaming chunk text into
the snapshot lowered peak and steady RSS against the 2026-10-21 run. Model and
parser assets were cached, and `HF_HUB_OFFLINE=1` blocked downloads. A repeat
`wg up` on an attached root returns immediately and keeps the same daemon PID.

## Warm semantic latency

120 samples per tool over five queries: `parse Rust expressions`,
`emit compiler error diagnostics`, `validate command line options`,
`resolve module imports`, `incremental compilation query dependency tracking`.
Warm daemon and OS cache, 13,332-file root, `--model code`, top 10.

| Tool | p50 | p95 | Errors |
| --- | ---: | ---: | ---: |
| wg | 25.2 ms | 29.8 ms | 0 |
| sgrep CLI | 565.7 ms | 624.5 ms | 0 |

The row is a declared mixture, not a per-query percentile. Process start,
loopback IPC, and daemon-side work are included. Top-10 rows and scores were
identical to sgrep on 5/5 queries in `--rank classic` mode.

## Edit-to-first-correct-result latency

A probe file in a 13,332-file scratch copy was rewritten with a new symbol.
The clock starts when the write returns and stops at the first CLI response
that returns the new symbol. 30 samples; warmups excluded.

| Samples | min | p50 | p95 | max | Errors |
| --- | ---: | ---: | ---: | ---: | ---: |
| 30 | 84.3 ms | 99.2 ms | 111.9 ms | 115.2 ms | 0 |

The pinned sgrep comparison ran after each sample, outside the interval. It
rebuilt its whole index in 6 of 6 samples and took 21.8 s at p50. In `--rank
classic` mode, final results also matched a freshly built pinned sgrep index
exactly, with no score or ordering differences.

## Semantic ranking quality: Semble and RepoQA (2026-10-24)

The Semble benchmark is Semble's own benchmark, with Semble's task loader,
annotations, target matching and NDCG@10 metric. The split below is 31 held-out
repositories from the 63-repo set; it was fixed before tuning this ranker.
These held-out repositories were also used to check earlier candidates, so the
number may be slightly optimistic. The benchmark is Semble's home field, not a
neutral comparison.

| Ranker | Semble held-out macro NDCG@10 (31 repos) |
| --- | ---: |
| `--rank classic` | 0.6867 |
| default | **0.7609** |
| Semble itself, all 63 repos | 0.854 |

The default ranker uses file coherence 0.05, whole-query symbol
definition boost 1.0 (1.5 when the file stem matches), and file-stem/path boost
0.6. It does not use per-file saturation. `--saturate` is opt-in and was not
used for these numbers.

On the same held-out RepoQA queries, the default ranker keeps the classic file
metrics and the line@5 count:

| Ranker | File H@1/5/10 | MRR | Line H@1/5/10 |
| --- | --- | ---: | --- |
| `--rank classic` | 202/270/283 | 0.76569 | 156/227/245 |
| default | 202/270/286 | 0.76699 | 155/227/247 |

The acceptance harness is `bench/WG_RANK_IMPL_EQUIV.py`. It compares the wg CLI
default with the analysis-tool rows; the method is in
[bench/WG_RANK3_REPORT.md](../bench/WG_RANK3_REPORT.md).

## Text route vs direct rg 15.1

Five cases on the Rust corpus. Literal cases use `-F`; the broad case uses `-n`
to stay on the text route; case-insensitive uses `-i -F`. n = 30 per tool per
case. All five cases used the daemon index and were byte-identical to rg under
`--sort path`.

| Case | Pattern | wg p50 | wg p95 | rg p50 | rg p95 | wg / rg p50 | Output bytes / lines |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| Absent literal | `zzqxjwvbnm_wg_text_absent_zzz` | 5.4 ms | 6.5 ms | 63.2 ms | 78.5 ms | 0.09 | 0 / 0 |
| Rare identifier | `genericargs` | 6.0 ms | 8.6 ms | 63.8 ms | 72.4 ms | 0.09 | 107 / 1 |
| Common identifier | `let` | 50.0 ms | 55.7 ms | 83.7 ms | 97.3 ms | 0.60 | 16,197,218 / 136,359 |
| Broad regex | `\w+\(` | 111.5 ms | 132.0 ms | 136.1 ms | 151.9 ms | 0.82 | 45,694,788 / 414,206 |
| Case-insensitive | `-i -F error` | 37.5 ms | 44.2 ms | 75.7 ms | 85.3 ms | 0.50 | 5,321,683 / 34,567 |

The absent and rare cases show the index prune. Common, broad and
case-insensitive cases still scan much of the tree; the index removes only
impossible candidates. The text route runs an in-process ripgrep 15.1 engine,
so it does not pay an `rg` process start.

## Text route: concurrent clients

Fixed mix of four queries (absent, rare, common, broad). Each client ran four
queries; one rep. Latency is per query, including process start and IPC.
Throughput is completed queries divided by makespan.

| Clients | wg q/s | wg p50 | wg p95 | wg n | rg q/s | rg p50 | rg p95 | rg n | Errors |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 21.9 | 29.1 ms | 109.4 ms | 4 | 11.8 | 76.9 ms | 118.7 ms | 4 | 0 |
| 4 | 28.1 | 70.7 ms | 410.7 ms | 16 | 14.1 | 248.7 ms | 501.7 ms | 16 | 0 |
| 16 | 26.0 | 340.1 ms | 1,727.1 ms | 64 | 12.6 | 1,017.5 ms | 2,258.2 ms | 64 | 0 |
| 32 | 26.1 | 728.4 ms | 3,331.5 ms | 128 | 14.5 | 1,761.6 ms | 3,798.9 ms | 128 | 0 |

`wg` saturates near 26 text queries/s on 4 vCPU, about 1.8-2.1x rg. All clients
share one daemon and one index. The 1-client row has only 4 requests, so its
p95 is weak.

## Update-design trade-off (earlier preview)

The first live implementation copied and re-encoded the whole snapshot on each
update. The current implementation shares unchanged file, chunk, and vector
records, maps the base vectors, and streams chunk text into a memory-mapped
snapshot. The measured edit visibility changed from 1,879.7 ms p50
(whole-snapshot copy) to 99.2 ms p50 in this campaign. The earlier preview
trade-off is superseded; this campaign measured only the final code.

## Windows

The only Windows numbers are from an old 2026-10-18 preview build, before the
memory, async-up and text-index work. They were not re-measured on the final
code. Do not use them as current numbers.

Preview build, Windows 11 x64, separate corpus copy: 13,328 files, 63,767
chunks, 32,908 ms index build, 361 ms model setup, 33,396 ms total, 34,487 ms
wall. The run created the Rust parser asset, so it was not a fully
cached-parser run. Repeat attach took 164 ms wall. Windows warm-query and
edit-visibility numbers have not been measured with this harness.

## Reproduce

Protocol: `docs/release-protocol.md`. Harnesses:
`bench/WG_LAUNCH_BENCH.py` and `bench/WGREP_WG_RELEASE.py`. Pinned controls are
sgrep revision `0fbe8e5725f71667789be4a3abcea4e02f33afa4` and ripgrep 15.1.0.
The launch campaign is documented in
[bench/WG_LAUNCH_BENCH.md](../bench/WG_LAUNCH_BENCH.md).

Raw receipts (commands, stdout/stderr, argv, per-sample timings) are kept
outside this repository.

## Media

Reproducible VHS tapes are in [docs/demos](demos/README.md). Rendered GIF
and MP4 files, plus the exact recorded terminal text, are in `docs/media`:

- [Quick start GIF](media/wg-quickstart.gif) / [MP4](media/wg-quickstart.mp4)
- [Two-repository GIF](media/wg-multi-repo.gif) / [MP4](media/wg-multi-repo.mp4)

The recordings use small generated repositories and cached assets. They
demonstrate onboarding, not these latency numbers.
