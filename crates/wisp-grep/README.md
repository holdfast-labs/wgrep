# wgrep

**ripgrep output, without the scan.** `wgrep` is `rg` with an index: it takes
every ripgrep flag, prints byte-identical output, and skips files the index
proves cannot match. A resident daemon keeps the file set and index in memory
and answers queries over loopback, so searches skip the directory walk
entirely — an order of magnitude less work for the queries agents and
developers actually run, and a large win when many agents search at once.

Drop-in: same flags, same output, same exit codes. The index can only remove
files from the search; it never changes how a file is searched. With no index
or daemon, `wgrep` is `rg`.

## Install

```sh
cargo install --path crates/wisp-grep
```

The crate also exposes a library target (`wisp_grep`) that runs the same
vendored ripgrep in-process. `run_rg(argv)` behaves like the `wgrep` binary in
one-shot mode, and `list_files(cwd, path)` uses the same walker for file
discovery. Neither function runs an external `rg`.

Then, once per repository:

```sh
wgrep --start .                # index the tree and serve it in the background
wgrep -n "some pattern" .      # any rg invocation, identical output, faster
```

The daemon is the default: the first search of an indexed tree starts one
automatically (reused afterwards), so after `wgrep --index-build .` you can
just search. `wgrep --start` builds the index (if needed), keeps it current
with a file watcher, and keeps the daemon running.

For agents, paste this into the instruction file:

> Search with `wgrep` instead of `rg`: it takes the same flags and prints the
> same output, only faster. Run `wgrep --start` once in the repo root; it
> indexes the tree, keeps the index current as files change, and keeps running
> in the background.

## Why

Single query, selective lookup (Rust checkout, `rust/src`, 13k files):

| query | `rg` | `wgrep` (daemon) |
| --- | ---: | ---: |
| absent literal | 243 ms | **13 ms** |
| `kmalloc` | 244 ms | 17 ms |

Concurrent agents (same tree; throughput, p50 latency):

| agents | `rg` q/s | wgrep q/s | p50 `rg` → wgrep | peak private MB `rg` → wgrep |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 4.0 | **72.4** | 243 → 13 ms | 6.9 → 0 |
| 4 | 6.0 | **31.7** | 571 → 103 ms | 26.5 → 9.5 |
| 16 | 6.0 | **24.0** | 2,307 → 334 ms | 104.9 → 34.0 |
| 32 | 6.1 | **32.9** | 3,108 → 542 ms | 170.6 → 72.7 |

Without the daemon (one-shot pruning) `wgrep` is bounded by rg's walk: it still
beats `rg` where file reads dominate (large files, cold cache), but on a small
cached tree it is close to rg. The daemon is what removes the walk for every
query. Broad queries (`-i the`) still read most files and gain less.

Index cost is small: **0.099×** the source on prose (xAFS dp013, 484 MB →
48 MB), **0.148×** on the Linux kernel (1.685 GB → 249 MB), **0.288×** on a
Rust checkout (225 MB → 65 MB). Builds are parallel and bounded-memory (spill
+ merge, `WGREP_BUILD_MEMORY_MB`, default 512).

## Correctness

`wgrep`'s walker *is* ripgrep's, so ignore rules, globs, types and every walk
flag behave exactly as in `rg`. A file is skipped only when it is in the index,
its size, mtime and platform change token (Unix `ctime`, Windows NTFS change
time) equal the indexed values, and its trigrams rule out the query. Changed,
new, recently-written and UTF-16-BOM files are always searched; the daemon's
watcher marks changes dirty and rescans on creations, renames, removals and
ignore-file changes. Non-matching or content-transforming modes (`-v`, `-L`,
`--json`, `--stats`, `-z`, explicit `-E`, `-P`, …) disable pruning.

`WGREP_PARITY.py` compares `wgrep` to `rg` on exit code, stderr and stdout
bytes across ~39 flag groups and several corpora, one-shot and daemon. The
index is a necessary-condition filter, so a mismatch would mean a dropped
match; the harness exists to prove there are none.

## How it works

- `wgrep` parses patterns with `regex-syntax` into a trigram query (Russ Cox's
  codesearch literal-set method); anything not analyzable becomes "every file".
- One-shot: `wgrep` walks exactly as `rg` does and, for each walked file,
  consults the trigram index; a present, unchanged, provably-non-matching file
  is skipped.
- Daemon (default): `wgrep --start` (or the auto-start on first search) runs a
  process that watches the tree, keeps the file set current, and returns the
  candidate files under a search path. The client then searches exactly those
  and skips the walk. Broad searches share the machine's threads through a
  lease so many agents do not oversubscribe the CPUs.

Full measurements, corpora and caveats: `eval/bench/WGREP.md`.

## Configuration

| Variable | Effect |
| --- | --- |
| `WGREP_INDEX_DIR` | where indexes live (default `%LOCALAPPDATA%\wgrep\indexes`, or `$XDG_CACHE_HOME/wgrep/indexes`) |
| `WGREP_NO_DAEMON=1` | disable the daemon; search one-shot |
| `WGREP_NO_INDEX=1` | disable pruning |
| `WGREP_INDEX_DEBUG=1` | print the query plan, candidates and how many files were skipped |
| `WGREP_INDEX_EVENTS=<file>` | append one NDJSON line per search recording the mode actually used (`daemon`/`prune`/`walk`), the fallback reason, and candidate counts; used by the parity harness to prove the daemon was exercised |
| `WGREP_BUILD_MEMORY_MB` | posting buffer before a build spills (default 512) |
| `WGREP_BUILD_THREADS` | build worker threads (default: CPU count) |
| `WGREP_THREADS` | default query threads (default: min(CPUs, 12)); `-j` overrides |
| `WGREP_DENSE_PERCENT` | store a trigram densely once it occurs in this share of files (default 100) |

Flags: `--start [PATH]`, `--stop [PATH]`, `--index-build [PATH]...`,
`--index-status [PATH]...`, `--index-serve [PATH]` (foreground daemon). They are
flags, not words, because `wgrep start` must keep meaning "search for `start`".

## Provenance

`src/rg/` is ripgrep 15.1.0's `crates/core`, adapted (local changes marked
`wgrep:`), under ripgrep's `Unlicense OR MIT` (see `COPYING`, `LICENSE-MIT`,
`UNLICENSE`). `src/index/` is the index: case folding, the regex-to-trigram
planner, the on-disk format, the builder, the per-file prune check and the
daemon. The crate is its own workspace so its `Cargo.lock` can track ripgrep's
exactly and its output stays byte-identical to the `rg` 15.1.0 release build.

## Build

```sh
cd crates/wisp-grep
cargo build --release
```
