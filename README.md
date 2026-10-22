# wg

`wg` is one local binary for code and text search. It has two routes:

- **Semantic ranking (the default).** `wg QUERY [ROOT]` ranks code chunks by
  meaning. The ranker is sgrep-compatible. The parity reference is pinned sgrep
  revision `0fbe8e5`.
- **Exact text and regex.** A ripgrep flag such as `-F`, `-e`, `-n`, `-l`, `-g`,
  or `--files` starts the built-in ripgrep 15.1 engine. The output follows `rg`
  15.1.0. You do not need `rg` on `PATH`.

One shared daemon backs both routes. The text route also works without it. For
each attached root, the daemon keeps a live semantic index and a trigram text
index. One watcher per root feeds both indexes. The daemon shares one model pool
between all roots.

![wg quickstart](docs/media/wg-quickstart.gif)

*Ask in plain English, or use a ripgrep flag.*

## Requirements

- Rust 1.95 or newer to build. The crate uses edition 2024.
- A C linker and standard build tools. On Linux, install `build-essential` or
  equivalent. On Windows, install the MSVC build tools.
- Network access on the first semantic run. `wg` downloads about 58 MB of model
  and parser assets. Later runs use the local cache.
- Linux x64 or Windows x64. macOS is not qualified yet.
- No `rg` binary is necessary. `wg` contains ripgrep 15.1.

## Install

Download the archive for your platform from GitHub Releases. Unpack it and put
its `bin/` directory on `PATH`:

```sh
tar -xzf wg-<version>-linux-x64.tar.gz
cd wg-<version>-linux-x64-*/
export PATH="$PWD/bin:$PATH"
wg --version
```

The extracted directory has a build suffix. On Windows, use `Expand-Archive` on
the zip, enter the extracted directory, then add its `bin` directory to `PATH`.
The archive contains `bin/wg` on Linux or `bin/wg.exe` on Windows.

You can also build from source:

```sh
cargo install --locked --path . --bin wg
```

The `--git` form below works once the repository is public. Until then, use the
archive or the `--path` form from a clone.

```sh
cargo install --locked --git https://github.com/holdfast-labs/wgrep --bin wg
```

A source build also produces `sgrep` for parity checks and `wgrep-sem-shared`.

## Quick start

Go to the repository you want to search. Run setup once for the semantic route:

```sh
cd path/to/repo
wg up
```

Without a path, `wg up` uses the nearest `.git` ancestor, or the current
directory. It starts or connects to the shared daemon, attaches the root, and
returns when search is ready. It prints the root, file and chunk counts, build
time, model setup time, total time, daemon PID, and text index size. In a
terminal it shows a spinner and elapsed time on stderr. With `--json` it prints
one JSON object and no progress text.

`wg up` checks the path before it starts the daemon. A missing path, or a path
that is not a directory, fails with `not a directory` and exit 2. No daemon
starts. A file is not a valid root.

The `--json` object includes `pid`, the daemon PID, in every case: a new build,
a joined build, and an already attached root.

The daemon builds asynchronously. The `wg up` client polls status with short
requests. Ctrl-C stops only the client. The daemon keeps building, and a later
`wg up` or `wg status` shows the same build.

On Windows, PowerShell may show the `wg up` progress line in red. It is progress
output, not an error. Use `wg up --json` in scripts.

Then search:

```sh
# Semantic, the default:
wg "where do incoming requests get validated"
wg "decodePacketV17"
wg "decodePacketV17" --top 10            # or -k 10
wg "packet framing" --json --explain --top 5

# Exact text or regex, with ripgrep flags:
wg -F "fn main" .
wg -e "fn\s+\w+\(" .
wg -n -e "TODO" .
wg -l "deprecated" .
wg -g "*.rs" "unsafe" .
wg --files .
wg --json -F "needle" .                  # ripgrep JSON events
```

## The text route

The text route is exact. It uses the ripgrep 15.1 engine that is built into the
`wg` binary. The engine runs in the `wg` process. It is not a child process and
does not read `rg` from `PATH`.

Output bytes, stderr, exit status, ignore rules, binary handling, encodings, and
`RIPGREP_CONFIG_PATH` follow `rg` 15.1.0. The differential test suite compares
`wg` with a real `rg` 15.1.0 binary.

This route does not need `wg up` or a running daemon. `wg up` is optional here.
It enables the trigram index for faster searches.

Always pass a path in scripts and agents. With no path, ripgrep reads stdin when
stdin is a pipe. This is rg behavior. Use `-` to read stdin on purpose:

```sh
cat file.txt | wg -F needle -
```

### The trigram index

`wg up` also builds a trigram index for the root. For a default directory walk
under an attached root, the daemon returns the files that can contain the
pattern. `wg` then runs the full ripgrep engine on those files. A flag that
changes the file set, such as `-g`, uses a full scan instead. The index skips
files that cannot match; it does not replace the search.

The gain is largest for rare or absent terms. On the 13,332-file Rust corpus:

| Case | `wg` p50 | `rg` 15.1 p50 | Speed |
| --- | ---: | ---: | ---: |
| Absent literal | 5.4 ms | 63.2 ms | 11.7x |
| Rare identifier | 6.0 ms | 63.8 ms | 10.6x |
| Common identifier | 50.0 ms | 83.7 ms | 1.7x |
| Broad regex | 111.5 ms | 136.1 ms | 1.2x |
| Case-insensitive | 37.5 ms | 75.7 ms | 2.0x |

All five cases were byte-identical to `rg` under `--sort path`. Common, broad,
and case-insensitive cases still scan much of the tree. The index removes only
impossible files. `rg` also pays process start. These are p50 values from one
campaign on a 4-vCPU Debian 13 VPS.

### When the text route falls back

`wg` uses the index only when the search is a default walk over a directory
inside an attached root and the index is trusted. It falls back to a full scan
in these cases:

- `wg up` did not attach the root, or the daemon does not answer.
- The invocation is not a default walk. Examples: `--hidden`, `-g`, an explicit
  file, `-v`, `--json`, or a transform.
- The pattern is not supported by the index plan, or `WGREP_NO_INDEX` is set.
- The search path is not a directory, or the path is not UTF-8.
- The watcher is untrusted, lost events, or overflowed.
- A walked path cannot be represented in the index, for example a non-UTF-8
  relative path.
- The daemon reply is missing, malformed, or exceeds the 8-second client bound.

A fallback changes speed, not results. The full scan is still the built-in
ripgrep engine.

## The semantic route

The semantic route ranks code chunks. It uses a small local model. The pinned
code model is `minishlab/potion-code-16M-v2`. The first run downloads the needed
model and parser assets. Later runs use the cache. This route needs an attached
root. Run `wg up` first.

```sh
wg "where do we validate incoming requests" --top 10
wg "decodePacketV17" --top 5
wg "packet framing" --json --top 20
wg "packet framing" --json --explain --top 5
```

- `--top N` / `-k N` sets the count. The range is 1 to 100. The default is 5.
- `--json` alone prints a semantic JSON array.
- `--explain` adds score fields and requires `--json`.
- `--model auto|code|text` selects the embedding policy. `auto` is the default.
  A query that names `code` or `text` must match the model used by `wg up`.
  `auto` does not enforce a model. The model is fixed at `wg up`. A mismatch
  fails, names the attached model, and gives the fix: run `wg down`, then
  `wg up --model <model>`.

Semantic search is ranked, not exhaustive. `wg` fuses a lexical candidate set
and a semantic candidate set, then returns a ranked shortlist. A correct answer
can fall outside the shortlist. Use `-F` or `-e` when you need every match.

Semantic scope is one whole attached root. Without `ROOT`, `wg` uses the nearest
attached ancestor of the current directory. A file or subdirectory `ROOT`
returns an explicit error. The text route accepts files and directories
normally.

The ranker is sgrep-compatible. On the launch corpus, warm top-10 rows and
scores were identical to pinned sgrep on 5 of 5 queries. Warm semantic latency
was 25.2 ms p50 for `wg` and 565.7 ms p50 for the sgrep CLI, 22.4x faster. This
was a five-query mixture on one machine. The sgrep CLI pays process start and
asset load on each call.

With 16 concurrent clients, `wg` reached 140.9 queries/s against 5.5 for sgrep,
with zero errors. The test used 10-second windows on 4 shared vCPU.

## One daemon, many roots

![two repositories, one daemon](docs/media/wg-multi-repo.gif)

*One daemon, one model, two repositories.*

Run `wg up` in another directory. It attaches that root to the same daemon. One
daemon PID serves all attached roots, with one shared model pool. Run
`wg status` to see every attached root:

```sh
wg status
wg status --json
```

## Large repositories

`wg up` builds both indexes. The semantic build is the long part. These values
come from one campaign on a Debian 13 VPS with 4 vCPU, 7.8 GiB RAM, and no swap:

| Tree | Files | `wg up` | Peak RSS | Steady RSS |
| --- | ---: | ---: | ---: | ---: |
| Rust `src` | 13,332 | 18.4 s | 386.2 MiB | 212.7 MiB |
| Linux 7.2.9 | 94,318 | 561.4 s (9m 21s) | 2,774.1 MiB | 2,040.5 MiB |

The Linux tree now reaches ready on this 7.8 GiB machine. An earlier campaign
was OOM-killed on the same tree. The build is CPU bound. The text index build is
separate: 1.61 s for the Rust tree and 27.9 s for the Linux tree. The text index
uses 20.4 MB on disk for the Rust tree and 247.0 MB for the Linux tree.

These are single runs, not percentiles over many machines. Do not read them as a
guarantee. A warm semantic query can raise RSS until the kernel reclaims mapped
vector and chunk-text pages.

## Status and stop

```sh
wg status
wg status --json
wg down
```

`wg status` shows every attached root, its counts, and its health.
`wg status --json` is machine-readable. With no daemon, `wg status` exits 2 and
prints `wg: no wg service is running for this cache; run wg up first`.

`wg down` stops the shared daemon. It stops search for every attached root, not
only the current one. After you stop it, run `wg up` again for the roots you
want. `wg down` is idempotent. With no daemon it exits 0 and prints
`No wg service is running.` The JSON reply is `{"ok":true,"running":false}`.

On Windows, human-readable `wg up` and `wg status` output removes the `\\?\`
prefix from paths. JSON keeps the canonical path, which can keep the prefix.

## Freshness

One watcher per root feeds both indexes. The daemon updates changed files,
reuses unchanged vectors, and applies edits, creates, renames, and deletes.

A semantic query waits for delivered pending work within a fixed bound. It also
checks the index generation before it returns. This is not a transactional
filesystem snapshot. It does not guarantee instant kernel event delivery. If the
watcher loses trust, `wg` reports an error instead of a stale answer.

The text route reads the live bytes of each candidate file. The file set follows
the watcher and the index. Changes become visible when the OS delivers their
watcher event. If the index cannot prove freshness, `wg` scans the full tree. A
fallback changes speed, not results.

![live index update](docs/media/wg-live.gif)

*No restart. The edit is searchable before the next prompt returns.*

To recover an unhealthy root, run `wg down` and then `wg up`. `wg status` shows
the error and the recovery hint. If a build fails, the next single `wg up`
retries it in the same call. That reply reports the new result or the new error.

## Platform support

- Linux x64: supported.
- Windows x64: supported.
- macOS: not qualified yet.

Current Windows performance has not been measured with the launch harness.

## Limits

- Semantic results are ranked, not exhaustive.
- The text index only removes impossible files. Common terms still scan much of
  the tree.
- Memory favors `wg` only at five roots, and the margin is small. Five attached
  13.3k-file roots used 902.9 MiB RSS, against 1,034.1 MiB for five warm sgrep
  processes: 12.7% less. This is not a general multi-repo win. At one root `wg`
  used 282.6 MiB against 249.2 MiB (1.13x more); at three roots 606.9 MiB
  against 499.8 MiB (1.21x more). `wg` stays below the sgrep per-process peak
  sum at three and five roots. The sgrep number is the sum of live processes,
  not a resident server. Mapped vector and chunk-text pages count in RSS after a
  query touches them, until the kernel reclaims them.
- A kernel-size tree takes about 9.5 minutes to attach on a 4-vCPU VPS with
  7.8 GiB RAM.
- One daemon holds up to 32 attached roots.
- The semantic route needs the model assets. The text route does not need the
  model or `wg up`.
- Semantic paths and arguments must be UTF-8. The text route passes non-UTF-8
  arguments to the engine. Index pruning falls back to a full scan.
- The launch numbers are from one campaign on one machine. Treat them as an
  order of magnitude, not a specification.

## Credits

- sgrep: semantic ranking. MIT license. Pinned revision `0fbe8e5`.
- ripgrep 15.1: text engine and walker. MIT or Unlicense.
- notify: filesystem watcher. CC0.

`wg` is MIT licensed.

## More

- Architecture and internals: [docs/how-it-works.md](docs/how-it-works.md)
- Agent setup, JSON shapes, and exit codes:
  [docs/agent-guide.md](docs/agent-guide.md)
- Measured setup numbers and method:
  [docs/benchmarks.md](docs/benchmarks.md)
- Launch media and charts:
  [docs/launch/README.md](docs/launch/README.md)

The recordings above are real, short-looping VHS tapes in
[docs/demos](docs/demos/README.md). MP4 versions are in `docs/media`.
