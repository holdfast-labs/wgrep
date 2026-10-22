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

- **Warm semantic answers:** 23.2 ms p50, against 502.3 ms for the sgrep CLI
  (21.7x). Top-10 rows and scores matched pinned sgrep on 5/5 queries.
- **Fresh code:** one-file edit to first correct result, 90.6 ms p50, against
  20.1 s for sgrep (222x). sgrep has no watcher and rebuilds its index.
- **Many agents:** 16 concurrent clients reached 149.0 queries/s, against 5.7
  for sgrep (26.2x), with zero errors.
- **Exact text, faster on selective terms:** absent terms 4.9 ms vs 52.4 ms for
  `rg` 15.1 (10.7x); rare terms 5.2 ms vs 52.5 ms (10.1x). Common 41.8 vs
  67.9 ms; broad 96.6 vs 110.8 ms; case-insensitive 31.7 vs 62.4 ms. Output was
  byte-identical under `--sort path`.
- **Large trees:** 13,332 files build in 18.1 s (634.9 MiB peak, 359.4 MiB
  steady). A 94,318-file Linux tree builds in 592.3 s (9m 52s), with 3,905.7
  MiB peak and 3,387.7 MiB steady RSS.

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

Unpack it, enter the extracted directory, then run `bin/wg` on Linux or
`bin\wg.exe` on Windows, or add `bin/` to `PATH`.

To build from source, use Rust 1.95 or newer and a C linker: `cc` on Linux, or
the MSVC build tools on Windows.

```sh
cargo install --locked --path . --bin wg
```

`cargo install --locked --git https://github.com/holdfast-labs/wgrep --bin wg`
works once the repository is public. Until then, use the archive or the `--path`
form from a clone.

The first semantic run downloads about 58 MB of model and parser assets. Later
runs use the cache. The text route does not need the model or `rg`.

On Windows, PowerShell may show the `wg up` progress line in red. It is progress
output, not an error.

## Supported platforms

- Linux x64
- Windows x64
- macOS: not qualified yet

## Known limits

- Semantic results are ranked, not exhaustive.
- The text index only removes impossible files. Common terms still scan much of
  the tree.
- Text-route commands in scripts need an explicit path. With no path, `rg` reads
  piped stdin and can wait for EOF.
- Memory does not favor `wg` for many roots. Five attached 13.3k-file roots used
  1,447.5 MiB RSS, against 1,192.0 MiB for five warm sgrep processes (1.21x
  more for `wg`). The sgrep number is a sum of live processes, not a resident
  server.
- A kernel-size tree takes about 10 minutes to attach on a 4-vCPU VPS with
  7.8 GiB RAM.
- One daemon holds up to 32 attached roots.
- Semantic paths and arguments must be UTF-8. The text route passes non-UTF-8
  arguments to the engine; index pruning falls back to a full scan.
- Current Windows performance was not measured with the launch harness.

## Checksums

The release workflow generates `SHA256SUMS` for the Linux and Windows archives
and attaches it to the release.

<!-- SHA256SUMS -->

## Credits

- sgrep: semantic ranking. MIT.
- ripgrep 15.1: text engine and walker. MIT or Unlicense.
- notify: filesystem watcher. CC0.
- `minishlab/potion-code-16M-v2`: embedding model. MIT. Downloaded at runtime.

See [NOTICE](NOTICE) for details and [LICENSE](LICENSE) for the main license.
