# wgrep: indexed ripgrep with identical output

`wgrep` (`crates/wisp-grep`) is ripgrep 15.1.0 plus a trigram index that
removes files which provably cannot match. Its output is meant to be
byte-identical to `rg` 15.1.0. There is one mode: rg walks as usual, and a
walked file whose size and mtime match the index is skipped when its trigrams
rule out the query.

Build the index once per tree with `wgrep --index-build PATH`. There is no
daemon, no watcher and no background process: every `wgrep` invocation is a
plain `rg` invocation plus a per-file index lookup, and falls back to exactly
`rg` when there is no index.

All measurements are from one Windows 11 machine (16 logical CPUs) with warm
caches, on 2026-09-28 and 2026-10-05. Cache state dominates: a first read of a
large tree off disk (plus per-file on-access scanning) can take minutes, so
every table below is a warm-cache measurement (an untimed warm-up pass precedes
timing).

## Correctness

`WGREP_PARITY.py` compares `wgrep` with `rg` on exit code, stderr and stdout
bytes. Stdout is compared exactly under `--sort path`, or as a sorted-line
multiset for about 10% of unsorted runs; only elapsed-time fields in
`--json`/`--stats` are normalized. Patterns are sampled gold-blind with a
fixed seed, plus a regex battery, across roughly 39 flag groups. The corpora
are:

- xAFS dp013: 9,988 files, 484 MB;
- a git-initialized copy of this repo's source;
- an edge-case fixture with NUL binaries, UTF-16 files with BOMs, a UTF-8
  BOM, Kelvin sign and long s, non-ASCII case variants, CRLF, a line over
  1 MB, hidden and ignored files, nested ignore files, non-ASCII file names
  and invalid UTF-8;
- shallow clones of torvalds/linux and rust-lang/rust.

A freshness phase applies seven mutations without rebuilding the index:
append, same-length overwrite, create, delete, rename, un-ignore, and an
immediate query after a write. Every mutated or new file must still be
searched, and the index is only trusted after the size and mtime match.

| Corpus | Invocations | Mismatches | Both timed out |
| --- | ---: | ---: | ---: |
| crate source (current build) | 627 | 0 | 0 |
| built-in corpora | 4,095 | 0 | 2 |
| Linux + Rust checkouts | 690 | 0 | 7 |

The two built-in timeouts are `-U '^'` and `-U --multiline-dotall '^'` on
dp013: both tools exceeded 60 s printing every line. The seven code-corpus
inconclusives are single-threaded (`--sort path`) scans of ~96,000 files where
both tools exceeded the timeout.

The harness also confirms that pruning is active. For ten sampled rare words
per corpus, wgrep skipped 181–9,959 of 9,988 dp013 files and 118–184 of 185
repo files. On the Rust checkout, `kmalloc` left 1 candidate of 63,059 and
`wgrep` skipped all 13,328 walked files under `src`. Selectivity follows the
pattern's trigrams, not word rarity: `accolades` left 1,223 dp013 candidates,
while `insertion` left 9,346.

Raw output: `E:\wgrep-parity\*.json`.

## Speed, one query at a time

Median of 5–9 runs per query, output to a null device, warm cache.

**Linux kernel, 95,929 files / 1.685 GB:**

| Query | `rg` | wgrep | vs `rg` |
| --- | ---: | ---: | ---: |
| absent literal | 1,096 ms | 11.3 ms | 95× |
| `-w unsafe fn` | 1,062 ms | 14.0 ms | 76× |
| `kmalloc` | 1,098 ms | 53.6 ms | 20.7× |
| `EXPORT_SYMBOL_GPL` | 1,074 ms | 53.6 ms | 19.5× |
| `-i 'TODO\|FIXME'` | 1,096 ms | 71.4 ms | 12.6× |
| `-w static` | 1,158 ms | 522 ms | 2.1× |
| `-i the` | 1,221 ms | 839 ms | 1.5× |
| `\w+\(` | 1,350 ms | 1,265 ms | 1.0× |

**Rust checkout, 63,059 files / 225 MB:**

| Query | `rg` | wgrep | vs `rg` |
| --- | ---: | ---: | ---: |
| absent literal | 700 ms | 11.6 ms | 61× |
| `EXPORT_SYMBOL_GPL` | 713 ms | 11.4 ms | 63× |
| `kmalloc` | 671 ms | 11.8 ms | 58× |
| `-w unsafe fn` | 700 ms | 29.8 ms | 24× |
| `-i 'TODO\|FIXME'` | 734 ms | 50.8 ms | 14× |
| `-w static` | 722 ms | 94.8 ms | 7.7× |
| `-i the` | 735 ms | 350 ms | 2.1× |
| `\w+\(` | 746 ms | 742 ms | 1.1× |

Selective queries are the whole point: wgrep reads only candidate files, so
the win scales with how few files the trigrams rule out. Broad queries
(`-i the`, `\w+\(`) still read most of the tree and gain little — the index can
only remove files that cannot match.

The walk itself is cheap once warm: `rg --files` takes about 185 ms on Linux
and a warm `wgrep` run on a selective query is ~11 ms, so the per-file index
lookup is not the bottleneck.

## Native Linux and cold cache (WSL2 Ubuntu)

The same machine through WSL2 Ubuntu (16 vCPUs, 7.9 GB), corpora on the VM's
ext4 disk, wgrep built natively (Rust 1.98.1, zig for PCRE2), with rg 15.1.0's
official musl release as the reference. All tests pass on Linux.

Index builds: the kernel (95,942 files, 1.64 GB) took 12.5 s with an 834 MB
peak and produced 0.154× the source; dp013 took 3.0 s with a 484 MB peak and
produced 0.099×.

Parity on Linux with the kernel tree: 350 invocations, **0 mismatches**. Rare-
word prune samples walked all 95,942 files; for example, `insertion` skipped
94,921. This required a harness change: rg and wgrep output streams to
temporary files and is compared in chunks (with an external sort for large
unsorted outputs), because several Linux invocations print gigabytes. A
`--selftest` checks that path on two 300 MB outputs differing at one line.

Warm (median of 7) and cold (median of 5, `echo 3 > /proc/sys/vm/drop_caches`
before each run) timings, in ms:

| Query | `rg` warm | `rg` cold | wgrep warm | wgrep cold |
| --- | ---: | ---: | ---: | ---: |
| Linux `kmalloc` | 372 | 2,331 | 177 | 569 |
| Linux `EXPORT_SYMBOL_GPL` | 320 | 3,279 | 126 | 568 |
| Linux absent literal | 372 | 2,273 | 166 | 418 |
| Linux `-i 'TODO\|FIXME'` | 721 | 3,579 | 170 | 669 |
| Linux `-w inline` | 1,973 | 3,527 | 217 | 1,071 |
| Linux `-w static` | 1,621 | 2,826 | 374 | 1,672 |
| dp013 `-i hummingbird` | 72 | 517 | 16 | 66 |
| dp013 `insertion` | 68 | 467 | 16 | 116 |

With a cold cache, rg must read the whole tree from disk while wgrep reads only
candidate files. The advantage is largest exactly when I/O is expensive.

## Index format

The current index is format 3.

- **Trigram postings, with next-byte masks.** Each trigram posting carries a
  one-byte mask of the characters that follow that trigram in the file
  (`fold::next_bit`). A literal's consecutive trigrams must then also agree on
  the next character, which removes files where every trigram of a word occurs
  but never in sequence. This can only remove files that cannot match.

  Candidate files before → after, against files that actually match (debug
  counts):

  | Query | Before | After | Matching |
  | --- | ---: | ---: | ---: |
  | dp013 `insertion` | 9,346 | 271 | 42 |
  | dp013 `accolades` | 1,223 | 46 | 44 |
  | dp013 `-i "term sheet"` | 4,679 | 1,604 | 820 |
  | Linux `insertion` | 9,067 | 1,037 | 603 |
  | Linux `-i "interrupt handler"` | 4,917 | 1,831 | 1,127 |
  | Linux `kmalloc` | 4,143 | 3,751 | 3,696 |
  | Linux `-w static` | 50,683 | 44,541 | 43,010 |

  No query gained candidates.

- **Posting offsets are `u32`, not `u64`.** The per-trigram offset table is
  monotonic and the postings blob is capped at 4 GiB, so the table is half the
  size; on trigram-heavy trees this removes roughly 4 bytes per distinct
  trigram from the mmap. A build whose blob would exceed 4 GiB fails with
  `postings exceed 4 GiB`. `VERSION` is 3; older indexes are ignored until
  `wgrep --index-build` is run.

- **The prune builds the relative path into a reused thread-local buffer**
  instead of allocating a `String` per walked file.

The build also folds and extracts trigrams adaptively: files up to 64 KB use a
sort-based path with no scratch table, so a tree of small files never
allocates the 16 MB per-thread `MaskScratch`; larger files use the table, which
bounds their memory by distinct trigrams.

## Build profile: it is I/O, not CPU

`WGREP_BUILD_DEBUG=1` prints a phase breakdown. On the Rust checkout (63,059
files, 225 MB) the phases are unambiguous:

| phase | cold first build | warm build |
| --- | ---: | ---: |
| walk | ~1–3 s | ~1 s |
| **file open + read** | **~150–250 s** | **~10 s (summed over 16 threads)** |
| fold | 0.3 s | 0.3 s |
| trigram masks | ~5 s | ~5 s |
| spill flush / format write | ~4 s | ~4 s |
| total | 160–250 s | **~6.6 s** |

A first read of 63k small files off disk (plus per-file on-access scanning)
dominates; every phase that does real work is seconds. Warm, the whole build
is ~6.6 s.

The one code cost worth removing was the read path. It used `fs::read` (which
opens the file *and* stats the path to size its buffer) plus two more
`fs::metadata` path lookups before and after. That is three path-based file
system round trips per file; the build now does one `File::open` and two stats
on the open handle (`File::metadata`), sized from the first. Same race check
(size and mtime unchanged, content length equals size), fewer path lookups —
which matters most precisely on the cold, scanning first build.

## Bounded-memory builds

Builds stream postings through sorted spill runs once a buffer budget
(`WGREP_BUILD_MEMORY_MB`, default 512) fills, then k-way merge them into the
index. The output is byte-identical to an in-memory build: checked on Linux and
dp013 at both the default and a 128 MB budget. Trigram masks are collected in a
reusable per-thread table instead of a per-position vector, which for a 24 MB
generated header was about 190 MB per thread. Workers may run at most two
chunks per thread ahead of the in-order merge, so a slow chunk cannot let
finished results pile up.

| Corpus | Before: build / peak | After: build / peak (default) | Peak at 128 MB budget |
| --- | ---: | ---: | ---: |
| Linux | 57.5 s / 5.4 GB | 12.7 s / 0.93 GB | — |
| dp013 | 8.0 s / 1.8 GB | 3.3 s / 0.80 GB | 0.31 GB |

## Index size and build time

| Corpus | Files | Source | Index | Ratio | Build | Peak build memory |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| xAFS dp013 prose | 9,988 | 483.6 MB | 48 MB | 0.099× | 3.0 s | 484 MB |
| wispd source copy | 185 | 1.8 MB | 0.73 MB | 0.395× | 0.05 s | — |
| Linux kernel | 95,929 | 1.685 GB | 248.8 MB | 0.148× | 12.7 s | 0.93 GB |
| Rust compiler | 63,059 | 225.4 MB | 108.0 MB | 0.479× | ~6.6 s (warm) | 0.84 GB |

The ratio depends on file size more than on content type: many small files each
contribute their own trigram postings, as in Rust's test suite. Format 3's u32
offset table reduced the Rust index from 0.538× to **0.479×**; it is a larger
share on trigram-heavy small-file trees than on the kernel.

## Daemon (default mode)

The daemon (`wgrep --start PATH`, auto-started on the first search of an
indexed tree) keeps the file set and index resident and returns candidate
files, so a search skips rg's walk. A watcher keeps the set current and a
thread lease bounds broad searches under load. Raw:
`E:\wgrep-worktrees\conc-daemon.json` (`WGREP_CONCURRENCY.py`, `rust\src`,
13,328 files, median of 2).

Single query: absent literal 13 ms vs rg 243 ms; `kmalloc` 17 ms vs 244 ms.

| agents | `rg` q/s | wgrep q/s | p50 ms `rg` → wgrep | peak private MB `rg` → wgrep |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 4.0 | 72.4 | 243 → 13 | 6.9 → 0 |
| 4 | 6.0 | 31.7 | 571 → 103 | 26.5 → 9.5 |
| 16 | 6.0 | 24.0 | 2,307 → 334 | 104.9 → 34.0 |
| 32 | 6.1 | 32.9 | 3,108 → 542 | 170.6 → 72.7 |

This is the win the standalone one-shot mode gives up: one-shot is bounded by
rg's walk, so on a small cached tree it is close to rg. The daemon removes the
walk for every query. Daemon parity (fixtures + mutations + source corpus,
`WGREP_PARITY.py --daemon`) is 627/0 on the source corpus and 0 mismatches on
the built-in corpora.

**Daemon memory and reconcile.** The daemon stores only per-id flags (the
index's own sorted path table holds the paths), keeps change times current,
and trims its working set after a reconcile. On the Rust checkout (63k files,
65 MB index) it settles at ~2 MB RSS / ~3 MB private once warm, and the first
query (which reconciles) is ~2 s: the reconcile reads change times with **one
directory handle per directory** (`GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`)
rather than opening every file, and read-only events are dropped in the
watcher (and a reconcile's own reads never trigger another reconcile).
Measured before these fixes on the same tree: 394 MB RSS and a 237 s first
query, with every query re-reconciling.

## Limits

- One machine, Windows, warm cache; cold-cache and Linux numbers above come
  from a WSL2 run on the same host.
- The index is a snapshot. A changed, new, recently-written or UTF-16-BOM file
  is always searched; the index only helps while entries are unchanged. Run
  `wgrep --index-build` to refresh.
- Non-matching or content-transforming modes (`-v`, `-L`, `--json`, `--stats`,
  `-z`, explicit `-E`, `-P`, …) disable pruning entirely and search every file.
- Broad and medium queries gain 1.0–3× on code and 1.1–1.6× on prose. The
  large gains are for selective queries, which dominate agent lookups but not
  every workload.
