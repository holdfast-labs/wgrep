# wg benchmarks

All numbers here come from retained raw records on the authorized Debian 13
VPS (4 vCPU, 7.8 GiB RAM, no swap). They are single measurement runs, not
percentiles over many sessions or machines. Windows numbers are a separate
corpus copy and are not an apples-to-apples ratio.

Setup and search latency are different measurements. Do not infer one from the
other.

## Comparison with sgrep (2026-10-21)

`wg` is now one binary: sgrep-identical semantic ranking plus the built-in
ripgrep 15.1 text route and trigram index in one daemon. This campaign compares
the final code on the same 13,332-file Rust `src` corpus, model and machine.
Full method, raw-record paths and caveats are in
[bench/WG_LAUNCH_BENCH.md](../bench/WG_LAUNCH_BENCH.md).

| Measure | wg | sgrep CLI | Result |
| --- | ---: | ---: | --- |
| One-file edit to first correct result, p50 | 90.6 ms | 20,092 ms | 222x faster. sgrep has no watcher and rebuilds its index. |
| Warm semantic query, p50 | 23.2 ms | 502.3 ms | 21.7x faster. Top-10 rows and scores identical on 5/5 queries. |
| 16 concurrent clients, throughput | 149.0 q/s | 5.7 q/s | 26.2x. 0 errors or timeouts on either side. |
| Five attached repositories, RSS | 1,447.5 MiB | 1,192.0 MiB | wg uses 1.21x the memory. Honest negative. |
| Text search vs direct `rg 15.1`, p50 | 4.9 / 5.2 / 41.8 / 96.6 / 31.7 ms | 52.4 / 52.5 / 67.9 / 110.8 / 62.4 ms (rg) | Faster on all five cases; byte-identical output. |

One final campaign on one 4 vCPU VPS. sgrep runs one process per query; wg
answers through its resident daemon. The text route is an in-process ripgrep
15.1 engine, not an external `rg` child.

## Setup: Rust `src` and Linux trees

Each tree used a fresh service directory and text-index directory. Peak RSS was
sampled at 50 ms. Steady RSS was sampled 2 s after ready.

| Corpus | Files | Semantic chunks | `wg up` | Text build | Peak RSS | Steady RSS | Text index |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Rust `prof/src` | 13,332 | 63,786 | 18.1 s | 1.48 s | 634.9 MiB | 359.4 MiB | 13,289 files, 20.4 MB |
| Linux 7.2.9 | 94,318 | 1,088,574 | 592.3 s | 28.2 s | 3,905.7 MiB | 3,387.7 MiB | 94,316 files, 247.0 MB |

The Linux tree now reaches ready on this 7.8 GiB machine; the earlier campaign
was OOM-killed on the same tree. Model and parser assets were cached, and
`HF_HUB_OFFLINE=1` blocked downloads. A repeat `wg up` on an attached root
returns immediately and keeps the same daemon PID.

## Warm semantic latency

120 samples per tool over five queries: `parse Rust expressions`,
`emit compiler error diagnostics`, `validate command line options`,
`resolve module imports`, `incremental compilation query dependency tracking`.
Warm daemon and OS cache, 13,332-file root, `--model code`, top 10.

| Tool | p50 | p95 | Errors |
| --- | ---: | ---: | ---: |
| wg | 23.2 ms | 26.6 ms | 0 |
| sgrep CLI | 502.3 ms | 529.8 ms | 0 |

The row is a declared mixture, not a per-query percentile. Process start,
loopback IPC, and daemon-side work are included. Top-10 rows and scores were
identical on 5/5 queries.

## Edit-to-first-correct-result latency

A probe file in a 13,332-file scratch copy was rewritten with a new symbol.
The clock starts when the write returns and stops at the first CLI response
that returns the new symbol. 30 samples; warmups excluded.

| Samples | min | p50 | p95 | max | Errors |
| --- | ---: | ---: | ---: | ---: | ---: |
| 30 | 84.0 ms | 90.6 ms | 92.9 ms | 96.2 ms | 0 |

The pinned sgrep comparison ran after each sample, outside the interval. It
rebuilt its whole index in 6 of 6 samples and took 20.1 s at p50. Final results
also matched a freshly built pinned sgrep index exactly, with no score or
ordering differences.

## Text route vs direct rg 15.1

Five cases on the Rust corpus. Literal cases use `-F`; the broad case uses `-n`
to stay on the text route; case-insensitive uses `-i -F`. n = 30 per tool per
case. All five cases used the daemon index and were byte-identical to rg under
`--sort path`.

| Case | Pattern | wg p50 | wg p95 | rg p50 | rg p95 | wg / rg p50 | Output bytes / lines |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| Absent literal | `zzqxjwvbnm_wg_text_absent_zzz` | 4.9 ms | 5.8 ms | 52.4 ms | 65.4 ms | 0.09 | 0 / 0 |
| Rare identifier | `genericargs` | 5.2 ms | 6.4 ms | 52.5 ms | 61.0 ms | 0.10 | 107 / 1 |
| Common identifier | `let` | 41.8 ms | 53.6 ms | 67.9 ms | 86.3 ms | 0.62 | 16,197,218 / 136,359 |
| Broad regex | `\w+\(` | 96.6 ms | 128.2 ms | 110.8 ms | 126.3 ms | 0.87 | 45,694,788 / 414,206 |
| Case-insensitive | `-i -F error` | 31.7 ms | 37.5 ms | 62.4 ms | 75.8 ms | 0.51 | 5,321,683 / 34,567 |

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
| 1 | 27.8 | 22.7 ms | 86.4 ms | 4 | 14.6 | 57.1 ms | 104.7 ms | 4 | 0 |
| 4 | 29.9 | 102.5 ms | 335.0 ms | 16 | 15.1 | 240.4 ms | 447.3 ms | 16 | 0 |
| 16 | 29.9 | 293.5 ms | 1,423.9 ms | 64 | 16.0 | 811.4 ms | 1,738.3 ms | 64 | 0 |
| 32 | 31.5 | 835.3 ms | 2,732.8 ms | 128 | 16.1 | 1,528.0 ms | 3,505.2 ms | 128 | 0 |

`wg` saturates near 31 text queries/s on 4 vCPU, about 1.9-2.0x rg. All clients
share one daemon and one index. The 1-client row has only 4 requests, so its
p95 is weak.

## Update-design trade-off (earlier preview)

The first live implementation copied and re-encoded the whole snapshot on each
update. The current implementation shares unchanged file, chunk, and vector
records and maps the base vectors. The measured edit visibility changed from
1,879.7 ms p50 (whole-snapshot copy) to 90.6 ms p50 in this campaign. The
earlier preview trade-off is superseded; this campaign measured only the final
code.

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

Raw receipts (commands, stdout/stderr, argv, per-sample timings) are retained
under `E:\wgrep-worktrees\wg-release-20261018\validation` and on the VPS under
`/srv/logs/wispd/`.

## Media

Reproducible VHS tapes are in [docs/demos](demos/README.md). Rendered GIF
and MP4 files, plus the exact recorded terminal text, are in `docs/media`:

- [Quick start GIF](media/wg-quickstart.gif) / [MP4](media/wg-quickstart.mp4)
- [Two-repository GIF](media/wg-multi-repo.gif) / [MP4](media/wg-multi-repo.mp4)

The recordings use small generated repositories and cached assets. They
demonstrate onboarding, not these latency numbers.
