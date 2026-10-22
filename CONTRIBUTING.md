# Contributing to wg

## Requirements

- Rust 1.95 or newer. The crate uses edition 2024.
- Python 3 for the release script and the benchmark harnesses.
- A real `rg` 15.1.0 binary for the parity tests. `wg` itself does not need
  `rg`.

## Build

```sh
cargo build --locked --release --bin wg
```

The binary is `target/release/wg` (`target\release\wg.exe` on Windows).

## Test

These are the CI commands:

```sh
cargo test --locked --lib --bins
cargo test --locked --test wg_lexical_parity --test wg_source_files_parity

cd crates/wisp-grep
cargo test --locked
cd ../..

cargo build --locked --release --bin wg
python scripts/wg_release.py self-test --work <scratch-dir>
```

CI downloads a SHA-256-pinned `rg` 15.1.0 release for the parity tests. The
text route must stay byte-compatible with `rg` 15.1.0. The parity harness
compares stdout, stderr, and exit status; unsorted traversal order is compared
as a multiset. The semantic route must stay compatible with pinned sgrep.

## Parity and release harnesses

Use the harnesses in `bench/`:

- `bench/WG_TEXT_PARITY.py`: one-shot text parity against `rg` 15.1.0.
- `bench/WG_TEXT_PARITY_DAEMON.py`: text parity through the daemon text index.
- `bench/WGREP_WG_RELEASE.py`: release contract, semantic parity, text parity,
  lifecycle, and benchmarks. See `docs/release-protocol.md`.
- `bench/WGREP_PARITY.py`: vendored ripgrep parity for `wisp-grep`.

Run a harness with `--help` to see its arguments. Use an isolated scratch
directory and a fresh `WG_SERVICE_DIR`. Stop every daemon you start.

## Do not commit build output

Do not commit `target/`, release archives, generated binaries, logs, benchmark
scratch trees, or model caches. Keep them outside the repository or in an
ignored scratch directory.
