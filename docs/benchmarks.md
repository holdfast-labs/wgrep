# wg benchmarks

All numbers here come from retained raw records on the authorized Debian 13
VPS (4 vCPU, 7.8 GiB RAM). They are single measurement runs, not percentiles
over many sessions or machines. Windows numbers are a separate corpus copy and
are not an apples-to-apples ratio.

Setup and search latency are different measurements. Do not infer one from the
other.

## Setup: Rust `src` corpus

| Measure | Earlier isolated run | Final copied-corpus run |
| --- | ---: | ---: |
| Files | 13,332 | 13,333 |
| Semantic chunks | 63,786 | 63,787 |
| Index build | 29,280 ms | 25,259 ms |
| Model setup | 129 ms | 0 ms (already loaded) |
| Total reported | 29,560 ms | 25,314 ms |
| End-to-end wall | 29,563 ms | 25,314 ms |

The final run used a scratch copy of the corpus, so its files were freshly in
the page cache. The earlier run walked the isolated source tree directly.
Model and parser assets were cached in both cases; `HF_HUB_OFFLINE=1` blocked
downloads. A repeat `wg up` on an attached root took 5.9 ms wall and kept the
same daemon PID. A second one-file repository attached to the same PID in
54.5 ms wall, with one loaded model.

## Warm search latency (final binary)

120 samples per workload, interleaved over four queries/cells, warm daemon and
OS cache, 13,333-file root.

| Workload | p50 | p95 | p99 | Errors |
| --- | ---: | ---: | ---: | ---: |
| Semantic (`parse Rust expressions`, `emit compiler error diagnostics`, `validate command line options`, `resolve module imports`) | 154.2 ms | 170.8 ms | 173.7 ms | 0 |
| rg text/regex (`-F` absent, `-F` identifier, broad `fn`, `\bpub\s+fn\b`) | 143.4 ms | 196.0 ms | 209.1 ms | 0 |

Each row is a declared mixture, not a per-query percentile. Process start,
loopback IPC, and daemon-side work are included. The text route runs a real
`rg` child; the semantic index does not accelerate regex scans.

## Edit-to-first-correct-result latency (final binary)

A 100-iteration alternating same-size edit of one probe file in a 13,333-file
scratch copy, measured from a completed write to the first CLI response that
returned the new symbol. The fresh pinned-sgrep comparison ran after each
measurement, outside the interval.

| Samples | min | p50 | p95 | max | Misses |
| --- | ---: | ---: | ---: | ---: | ---: |
| 100 | 188.1 ms | 208.7 ms | 229.5 ms | 246.0 ms | 0 |

Final results also matched a freshly built pinned sgrep index exactly, with no
score or ordering differences.

## Update-design trade-off

The first live implementation copied and re-encoded the whole snapshot on each
update. The final implementation shares unchanged file, chunk, and vector
records and copies only index metadata. Measured A/B on the same corpus, 40
samples each:

| Workload | Whole-snapshot copy | Shared snapshots |
| --- | ---: | ---: |
| Semantic p50 | 122.3 ms | 161.6 ms |
| rg text p50 | 142.5 ms | 145.2 ms |
| Edit visibility p50 (100 samples, whole-snapshot run) | 1,879.7 ms | 208.7 ms |

The update path is about 9x faster. Warm semantic scoring is about 32% slower
because unchanged chunks now sit behind individual reference-counted
allocations, which costs cache locality. This is a known preview trade-off.
A follow-up should store per-file chunks contiguously behind one shared
allocation while keeping the same zero-copy update behavior.

## Windows

Windows 11 x64, separate corpus copy: 13,328 files, 63,767 chunks, 32,908 ms
index build, 361 ms model setup, 33,396 ms total, 34,487 ms wall. The run
created the Rust parser asset, so it was not a fully cached-parser run. Repeat
attach took 164 ms wall. Windows warm-query and edit-visibility numbers have
not been measured with this harness.

## Reproduce

Protocol: `docs/release-protocol.md`. Harness:
`bench/WGREP_WG_RELEASE.py`. Pinned controls are sgrep revision
`0fbe8e5725f71667789be4a3abcea4e02f33afa4` and ripgrep 15.1.0.

Raw receipts (commands, stdout/stderr, argv, per-sample timings) are retained
under `E:\wgrep-worktrees\wg-release-20261018\validation` and on the VPS under
`/srv/logs/wispd/wg-release-*-20261018`.

## Media

Reproducible VHS tapes are in [docs/demos](demos/README.md). Rendered GIF
and MP4 files, plus the exact recorded terminal text, are in `docs/media`:

- [Quick start GIF](media/wg-quickstart.gif) / [MP4](media/wg-quickstart.mp4)
- [Two-repository GIF](media/wg-multi-repo.gif) / [MP4](media/wg-multi-repo.mp4)

The recordings use small generated repositories and cached assets. They
demonstrate onboarding, not these latency numbers.
