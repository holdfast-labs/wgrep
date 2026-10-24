# wg v0.1.0

`wg` is one local binary for code and text search. It has two routes:

- **Semantic ranking.** `wg QUERY [ROOT]` ranks code chunks with a
  sgrep-compatible hybrid scorer.
- **Exact text and regex.** A ripgrep flag such as `-F`, `-e`, `-n`, `-l`, `-g`,
  or `--files` runs the built-in ripgrep 15.1 engine.

One shared daemon keeps a live semantic index and a trigram text index for each
attached root. One watcher per root feeds both indexes. `wg` does not need `rg`
on `PATH`.

## Highlights

- **Warm semantic answers:** 25.2 ms p50, against 565.7 ms for the sgrep CLI
  (22.4x). Top-10 rows and scores matched pinned sgrep on 5/5 queries.
- **Fresh code:** one-file edit to first correct result, 99.2 ms p50, against
  21.8 s for sgrep (220x). sgrep has no watcher and rebuilds its index.
- **Many agents:** 16 concurrent clients reached 140.9 queries/s, against 5.5
  for sgrep (25.4x), with zero errors.
- **Exact text, faster on selective terms:** absent terms 5.4 ms vs 63.2 ms for
  `rg` 15.1 (11.7x); rare terms 6.0 ms vs 63.8 ms (10.6x). Common 50.0 vs
  83.7 ms; broad 111.5 vs 136.1 ms; case-insensitive 37.5 vs 75.7 ms. Output
  was byte-identical under `--sort path`.
- **Large trees:** 13,332 files build in 18.4 s (386.2 MiB peak, 212.7 MiB
  steady). A 94,318-file Linux tree builds in 561.4 s (9m 21s), with 2,774.1
  MiB peak and 2,040.5 MiB steady RSS.

These values come from one final campaign on one Debian 13 VPS with 4 vCPU,
7.8 GiB RAM, and no swap. They are single runs, not percentiles over many
machines. Warm semantic latency is a five-query mixture, not a per-query
percentile. The sgrep freshness row has 6 samples; the `wg` row has 30. The text
index only removes files that cannot match. The sgrep CLI pays process start and
rebuilds after changes. Do not read these numbers as a specification for your
machine.

## Install

Download the archive for your platform from GitHub Releases:

- Linux x64: `wg-0.1.0-linux-x64.tar.gz`
- Windows x64: `wg-0.1.0-windows-x64.zip`
- macOS arm64: `wg-0.1.0-macos-arm64.tar.gz`
- macOS x64: `wg-0.1.0-macos-x64.tar.gz`

Unpack it, enter the extracted directory, then run `bin/wg` on Linux or macOS,
or `bin\wg.exe` on Windows, or add `bin/` to `PATH`. The macOS x64 archive is
built by cross-compilation on `macos-14` and smoke-tested under Rosetta 2.

To build from source, use Rust 1.95 or newer and a C linker: `cc` on Linux and
macOS, or the MSVC build tools on Windows.

```sh
cargo install --locked --path . --bin wg
```

```sh
cargo install --locked --git https://github.com/holdfast-labs/wgrep wgrep --bin wg
```

The first `wg up` downloads about 56 MB of model and parser assets (about 65 MB
on disk). Later runs use the cache. The text route does not need the model or
`rg`.

On Windows, PowerShell may show the `wg up` progress line in red. It is progress
output, not an error.

## Service commands

- `wg up --json` includes `pid` in every reply: a new build, a joined build, and
  an already attached root.
- `wg up` checks the path before it starts the daemon. A missing path or a file
  fails with `not a directory`.
- `wg down` is idempotent. With no daemon it exits 0 and prints
  `No wg service is running.` The JSON reply is `{"ok":true,"running":false}`.
- `wg status` with no daemon exits 2 and prints
  `wg: no wg service is running for this cache; run wg up first`.
- The model is fixed at `wg up`. A mismatch error names the attached model and
  says: run `wg down`, then `wg up --model <m>`.
- On Windows, human-readable output removes the `\\?\` prefix from paths. JSON
  keeps the canonical path.
- A failed build is retried by the next single `wg up`.

## Supported platforms

- Linux x64
- Windows x64
- macOS arm64 (Apple Silicon). Tested on `macos-14` in CI.
- macOS x64 (Intel). Built by cross-compilation on `macos-14` and smoke-tested
  under Rosetta 2 in CI.

macOS has no benchmark numbers; all benchmarks are Linux. On macOS the cache
lives in `~/.cache/wgrep`, not `~/Library/Caches`.

## Known limits

- Semantic results are ranked, not exhaustive.
- The text index only removes impossible files. Common terms still scan much of
  the tree.
- Text-route commands in scripts need an explicit path. With no path, `rg` reads
  piped stdin and can wait for EOF.
- Memory favors `wg` only at five roots, and the margin is small. Five attached
  13.3k-file roots used 902.9 MiB RSS, against 1,034.1 MiB for five warm sgrep
  processes: 12.7% less. This is not a general multi-repo win. At one root `wg`
  used 282.6 MiB against 249.2 MiB (1.13x more); at three roots 606.9 MiB
  against 499.8 MiB (1.21x more). `wg` stays below the sgrep per-process peak
  sum at three and five roots. The sgrep number is the sum of live processes,
  not a resident server. Mapped vector and chunk-text pages count in RSS after a
  query touches them, until the kernel reclaims them.
- A kernel-size tree takes 561.4 s (9m 21s) to attach on a 4-vCPU VPS with
  7.8 GiB RAM, with 2,774.1 MiB peak and 2,040.5 MiB steady RSS.
- One daemon holds up to 32 attached roots.
- Semantic paths and arguments must be UTF-8. The text route passes non-UTF-8
  arguments to the engine; index pruning falls back to a full scan.
- Current Windows performance was not measured with the launch harness.

## Checksums

The release workflow generates `SHA256SUMS` for the Linux, Windows, and macOS
archives and attaches it to the release.

<!-- SHA256SUMS -->

## Credits

- sgrep: semantic ranking. MIT.
- ripgrep 15.1: text engine and walker. MIT or Unlicense.
- notify: filesystem watcher. CC0.
- `minishlab/potion-code-16M-v2`: embedding model. MIT. Downloaded at runtime.

See [NOTICE](NOTICE) for details and [LICENSE](LICENSE) for the main license.
