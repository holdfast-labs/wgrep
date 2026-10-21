# wg Phase B2 text route: final daemon parity, freshness and speed

Final run, 2026-10-21, against B1's finished code.

## Identity

| Item | Value |
| --- | --- |
| Source tree sha256 | `7808e65148e1d2e05a4ddadc2444f11b86eca4729968559da7c26e8e1d52e0f2` |
| `wg` sha256 | `b4e5655aa7aa8b12b2ebfd2424b9a0a9fcb35b573a8520cca496b69b54233817` |
| rg reference | `/srv/workspaces/wispd/tools/rg-15/rg`, ripgrep 15.1.0 (rev af60c2de9d) |
| Model | `minishlab/potion-code-16M-v2`, `--model code` |
| Machine | Debian 13, 4 vCPU, 7.8 GiB RAM, no swap, shared VPS |
| Repo copy | `/srv/projects/wispd/wg-b2-20261021-final` (no `.git`) |
| Corpus | copy of `/srv/workspaces/wispd/prof/src`, 13,448 files, 148,775,301 B |

The source hash is sha256 over sorted file hashes of `src/`,
`crates/wisp-grep/src/` and `Cargo.lock`. Every result JSON carries it.

## Method

- `WG_TEXT_PARITY_DAEMON.py` imports `WG_TEXT_PARITY.py` and
  `WGREP_PARITY.py`, builds the edge-case fixture, attaches it with `wg up`,
  and runs every text-route matrix case and freshness mutation through `wg`
  and rg 15.1.0.
- Output is compared byte-exactly. Sorted cases use exact bytes; unsorted
  cases compare line records as a multiset. `--json` and `--stats` timing
  fields are normalized.
- `WGREP_INDEX_EVENTS` writes one decision event per `wg` invocation:
  `used="daemon"` means candidate pruning was used; `used="walk"` means a full
  scan with the real `reason`. The harness also asserts exactly one event per
  invocation.
- `WG_TEXT_FRESHNESS.py` compares `wg` and rg on every first query after a
  change. A disagreeing first query is retried for 3 s; a converged retry is
  `stale_first`, a retry that never converges is a hard mismatch.
- `WG_TEXT_SPEED.py` alternates rg and `wg` samples after warmups. Literal
  queries use `-F`; the broad query is `-n '\w+\('`. Rare and common
  identifiers are picked from a sample, then verified by document frequency
  over the whole corpus.
- Every subprocess has a timeout and a checked exit code. Each arm has an
  isolated `WG_SERVICE_DIR` and `WGREP_INDEX_DIR`. Every daemon is stopped by
  `wg down` and killed by PID if it survives.

## 1. Daemon parity

1,822 applicable cases: 1,714 matrix cases and 108 freshness cases.

| Result | Count |
| --- | ---: |
| Applicable | 1,822 |
| Daemon candidate path (`used="daemon"`) | 1,169 |
| Fallback full scan (`used="walk"`) | 653 |
| Mismatched | 0 |
| Inconclusive (both timed out) | 0 |
| Stale on first query | 0 |
| Missing event | 0 |
| More than one search event | 0 |
| Errors | 0 |

Fallback reasons:

| Reason | Count | Why |
| --- | ---: | --- |
| `not_walk_default` | 369 | flags change the file set: `--hidden`, `--no-ignore`, `-uu`, globs, types, max depth |
| `plan_disabled` | 252 | non-matching or transforming modes: `-v`, `-L`, `--json`, `--stats`, `--passthru`, `-P`, `-E`, `--include-zero` |
| `not_a_directory` | 32 | an explicit file or a list of files replaces the directory walk |

The 653 fallbacks are by design. Each records its real reason. Fallback per
flag group:

| Group | Daemon | Walk |
| --- | ---: | ---: |
| `default`, `-i`, `-S`, `-s`, `-w`, `-x`, `-F`, `-c`, `--count-matches`, `-l`, `-o`, `-n`, `-N`, `--column`, `-A2`, `-B1`, `-C1`, `-m3`, `-U`, `-U-dotall`, `--no-unicode`, `--null-data`, `-a`, `--binary`, `--crlf` | 41 each | 0 |
| `-e_multi` | 3 | 0 |
| `-f_patternfile` | 1 | 0 |
| `path_implicit_*`, `path_subdir_*` | 4 each | 0 |
| `--hidden`, `--no-ignore`, `-uu`, `-g_md`, `-g_not_rs`, `-t_rust`, `-t_md`, `--max-depth2`, `-E_utf16le`, `-L`, `-P`, `--json`, `--stats`, `--passthru`, `--include-zero-c` | 0 | 41 each |
| `-v` | 0 | 6 |
| `path_explicit_*`, `path_multi_*` | 0 | 4 each |

The fixture has 75 files; `wg up` reported a 75-file text index.

## 2. Freshness

26 checks, all served by the daemon, all byte-identical to rg.

| Change | Checks | Stale first query | Mismatch |
| --- | ---: | ---: | ---: |
| create, append, same-length overwrite, delete, rename | 5 | 0 | 0 |
| same-length rewrite with the original mtime restored | 2 | 0 | 0 |
| racy append, queried immediately | 2 | 0 | 0 |
| root `.gitignore` add and remove | 3 | 0 | 0 |
| ancestor `.ignore` add and remove | 3 | 0 | 0 |
| ancestor `.gitignore` in a parent Git repository | 3 | 0 | 0 |
| Git `info/exclude` add and remove | 3 | 0 | 0 |
| nested `.git` pointer and its external `info/exclude` | 5 | 0 | 0 |

The same-length rewrite with restored mtime is the important case: an index
that trusted size and mtime alone would miss the new token. The daemon found
it on the first query. Fallback count: 0. Missing or duplicate events: 0.

## 3. Speed, one query at a time

Warm cache, 30 samples per tool per query, alternating order, output to
`/dev/null`. Times are milliseconds. `wg` used the daemon candidate path for
every sample (one search event each).

| Query | Pattern | rg p50 | rg p95 | wg p50 | wg p95 | wg / rg p50 |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| absent literal | `zzqxjwvbnm_wg_text_absent_zzz` | 64.5 | 115.3 | 7.8 | 8.2 | 0.12 |
| rare identifier | `testable` (10 files) | 64.4 | 66.1 | 9.6 | 16.0 | 0.15 |
| common identifier | `not` (6,414 files) | 114.8 | 116.6 | 65.8 | 68.8 | 0.57 |
| broad regex | `\w+\(` | 164.8 | 168.9 | 117.6 | 165.6 | 0.71 |
| case-insensitive | `-i -F error` | 114.7 | 117.3 | 64.1 | 68.1 | 0.56 |

Exact output parity with `--sort path` held for all five queries (bytes and
exit code). Sizes: absent 0 B; rare 2,395 B; common 5,409,230 B; broad
45,695,272 B; case-insensitive 5,321,683 B.

## 4. Speed, concurrent clients

Fixed mix of four queries (absent, rare, common, broad). Each client ran four
queries per rep; two reps. Latency is per query, including process start and
IPC. Throughput is completed queries divided by makespan.

| Clients | wg q/s | wg p50 | wg p95 | rg q/s | rg p50 | rg p95 | Errors |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 17.5 | 65 ms | 142 ms | 9.7 | 116 ms | 165 ms | 0 |
| 4 | 31.4 | 93 ms | 328 ms | 16.6 | 181 ms | 384 ms | 0 |
| 16 | 31.3 | 332 ms | 1,462 ms | 17.2 | 723 ms | 1,736 ms | 0 |
| 32 | 31.3 | 481 ms | 2,940 ms | 17.0 | 1,446 ms | 3,529 ms | 0 |

`wg` saturates near 31 queries/s on 4 vCPU, about 1.8x rg. All clients share
one daemon and one index.

## 5. Daemon RSS

`VmRSS` and `smaps_rollup` Pss of the daemon, after warm queries and a 2 s
settle. The no-text arm pointed `WGREP_INDEX_DIR` at a regular file, so
`TextIndex::open` failed with `Not a directory (os error 20)` and `wg up` still
returned success for the semantic route.

| Arm | Idle RSS | Idle Pss | Loaded RSS | Text files | Text index on disk | Text mapping resident |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| semantic + text | 500.8 MiB | 499.1 MiB | 500.8 MiB | 13,294 | 20.4 MB | 3.3 MB |
| semantic only | 483.3 MiB | 481.7 MiB | 483.4 MiB | none | none | none |
| Difference | +17.5 MiB | +17.4 MiB | +17.5 MiB | | | |

Startup: `wg up` took 21.9 s total; the text index build was 1.4 s; the
semantic index and model setup were the rest. Semantic files 13,337; chunks
63,801; text-indexed files 13,294. The 43 absent files are searched directly.

## 6. Inherited-fd check (Linux)

| Case | Result | Evidence |
| --- | --- | --- |
| Inherited pipe write end passed with `pass_fds` | **FAIL** | `wg up` exited 0 and the daemon was alive, but the read end did not reach EOF in 5 s. `/proc/<daemon>/fd` showed the pipe: `fd 4 -> pipe:[4140112]`. |
| Plain captured stdout/stderr | PASS | `communicate()` returned, both pipes reached EOF, daemon alive. |

This is the Unix version of the Windows hang that B1 fixed. `detached.rs` on
Unix redirects stdio but does not close an inherited descriptor >= 3 that the
caller marked inheritable. See `WG_TEXT_BUGS.md` for the reproduction.

## 7. Large repository `wg up`

`--model code`, one corpus at a time, each with isolated service and index
directories. The client waits up to 300 s; readiness is polled up to 1,800 s.

| Corpus | Files | Size | `wg up` | Client time | Ready | Peak daemon RSS | Result |
| --- | ---: | ---: | --- | ---: | --- | ---: | --- |
| Linux 7.2.9 | 94,862 | 1.62 GB | exit 2, `service frame deadline exceeded` | 90.1 s | no | 7,047.9 MiB | daemon OOM-killed at ~14 min |
| Rust `prof/src` | 13,448 | 148.8 MB | exit 0 | 22.5 s | yes | 681.7 MiB | text index: 13,289 files, 20.4 MB |

The Linux text index itself was written (236 MB on disk) before the semantic
phase. The client hit the fixed 90 s service frame deadline while the daemon
continued. The kernel then killed the daemon:

```text
Out of memory: Killed process 356500 (wg) total-vm:8161748kB,
anon-rss:7270048kB, file-rss:1056kB, shmem-rss:0kB, UID:1000
```

Peak sampled RSS was 7,047.9 MiB on a 7.8 GiB machine with no swap. The root
never attached, so a text search falls back to a full scan. This is a real
failure on a large tree, not just a slow success.

## Caveats

- One shared 4-vCPU VPS and one campaign. There is no swap. Page cache was
  warm for the Rust corpus.
- `wg up` builds the semantic index too. Startup time and RSS include the
  semantic route, which the text route does not need by itself.
- The text index is a snapshot plus a watcher. Files it does not index are
  searched as added candidates, so a small tree can have zero indexed files
  and still be correct.
- Concurrency uses a fixed four-query mixture. The p50 and p95 pool two reps
  of `clients x 4` queries. They are not per-query percentiles.
- RSS is daemon resident memory. Text-index resident bytes are only the
  touched pages of the mapping.
- Sorted output is byte-exact. Unsorted ripgrep traversal order is not stable,
  so unsorted checks compare line multisets.
- The broad regex prints about 46 MB. It measures search and printing.
- The large-tree check used `--model code`. Other models can use different
  memory.

## Receipts

VPS, under `/srv/workspaces/wispd/wg-b2-final/`:

- `parity2/summary.json`
- `freshness/summary.json`
- `speed/results.json`
- `inherit-fd/summary.json`
- `large-up/summary.json`

Logs:

- `/srv/logs/wispd/wg-b2-build-final.log`
- `/srv/logs/wispd/wg-b2-parity-final2.log`
- `/srv/logs/wispd/wg-b2-freshness-final.log`
- `/srv/logs/wispd/wg-b2-speed-final.log`
- `/srv/logs/wispd/wg-b2-large-up.log`

Source copy: `/srv/projects/wispd/wg-b2-20261021-final`.

Local harnesses and results:

- `bench/WG_TEXT_COMMON.py`
- `bench/WG_TEXT_PARITY_DAEMON.py`
- `bench/WG_TEXT_FRESHNESS.py`
- `bench/WG_TEXT_SPEED.py`
- `bench/WG_TEXT_INHERIT_FD.py`
- `bench/WG_TEXT_LARGE_UP.py`
- `bench/WG_TEXT_RESULTS.json`
- `bench/WG_TEXT_BUGS.md`

## Bug summary

- **B2-FD-1** (open): Unix daemon keeps an inherited descriptor >= 3, so a
  piped caller can still wait forever.
- **B2-UP-1** (open): `wg up` on a 1.62 GB Linux tree hits the 90 s client
  deadline and the daemon is OOM-killed at 7,047.9 MiB RSS.
- **B2-OBS-1** (fixed): the extra `reason:"daemon_disabled"` event is gone.
  The final run got exactly one search event per invocation.
- No parity or freshness correctness bug was found in this run.
