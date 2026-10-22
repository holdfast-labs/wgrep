# Release guide

This page is for maintainers. It describes the CI and release workflows for the
`wg` crate at the repository root.

## Toolchain

CI and release build with Rust `1.95`. This is the `rust-version` in
`Cargo.toml`. Do not use a different toolchain without a test run.

## Continuous integration

`.github/workflows/ci.yml` runs on each push and pull request to `main`:

- `cargo test --locked --lib --bins`
- `cargo test --locked --test wg_lexical_parity --test wg_source_files_parity`
- `cargo test --locked` in `crates/wisp-grep`
- `cargo build --locked --release --bin wg`
- `python scripts/wg_release.py self-test --work <scratch-dir>`

Jobs run on `ubuntu-latest` and `windows-latest`. Cargo caches are restored by
`Swatinem/rust-cache`.

## Release steps

1. Set the new version in `Cargo.toml`. Update `Cargo.lock` if needed.
2. Commit the version change and push it to `main`.
3. Create an annotated tag `v<version>`. The tag must match the root
   `Cargo.toml` version.
4. Push the tag:

   ```sh
   git push origin v0.1.0
   ```

5. The `Release` workflow starts. It also supports manual runs with
   `workflow_dispatch` and an existing tag as input.
6. Wait for the `preflight`, `linux-x86_64`, and `windows-x86_64` jobs. Each
   build job builds `wg`, exports the source, and builds the archive with
   `scripts/wg_release.py`.
   The export contains the root crate files, `src/`, `tests/`,
   `crates/wisp-grep/`, `vendor/notify/`, `third_party/`, and `docs/` without
   `docs/launch/`. This source builds with
   `cargo build --locked --release --bin wg`.
7. The workflow creates a **draft** GitHub Release. It uploads both archives and
   a `SHA256SUMS` file. The draft body comes from `RELEASE_NOTES.md`; the
   `publish` job writes `dist/RELEASE_BODY.md` with the `SHA256SUMS` content
   filled into the Checksums placeholder. It does not publish the release.
8. Review the draft:
   - confirm the draft body and the filled Checksums block;
   - confirm the asset names and checksums;
   - confirm both smoke jobs passed;
   - confirm the source commit and tag;
   - publish the draft manually.

The workflow uses `--label RELEASE` and `--binary-status final`. The script
default label stays `PREVIEW` for local test archives.

## Draft release body

`RELEASE_NOTES.md` is the source of the draft release body. The `publish` job
checks out the release tag, writes `dist/RELEASE_BODY.md`, and passes it to the
release action with `body_path`.

`RELEASE_NOTES.md` must contain this marker on its own line in the Checksums
section:

```text
<!-- SHA256SUMS -->
```

The job replaces the marker with a fenced `text` block that contains
`dist/SHA256SUMS`. The body is not generated from commit history.

`generate_release_notes` is `false`. The curated body is the whole body.
GitHub's generated notes would append commit-derived text and make the draft
less controlled. If maintainers want the auto notes, set it to `true` and
accept the appended section.

`dist/RELEASE_BODY.md` is not uploaded as an asset. The release asset list is
explicit: `dist/*.zip`, `dist/*.tar.gz`, `dist/*.sha256`, and
`dist/SHA256SUMS`.

## Archive contents

The archive ships one binary: `bin/wg` on Linux or `bin/wg.exe` on Windows.
There is no launcher script and no bundled `rg` binary. ripgrep 15.1.0 is
compiled into `wg` through `crates/wisp-grep`, and `wg` does not read `WG_RG`.
Tell users to run `bin/wg` directly or add `bin/` to `PATH`.

The archive also contains `README.md`, `docs/`, `licenses/`, `PROVENANCE/`, and
`source/`. The `licenses/` tree keeps the ripgrep notices from
`third_party/ripgrep/` and `crates/wisp-grep/`, because the ripgrep source is
part of the `wg` build.

## Dependency licenses

`scripts/wg_release.py archive` runs `cargo metadata --locked` on the exported
source with the archive target platform. That gives the exact compiled package
set and each package's `manifest_path`, including path crates such as
`crates/wisp-grep` and `vendor/notify`.

For every compiled crate, the script copies the license files from the package
manifest directory into `licenses/deps/<name>-<version>/`. It writes
`licenses/DEPENDENCY_LICENSES.txt` with the crate, version, license expression,
and copied file names. The archive step fails if a compiled crate has no local
license file. The only exceptions are a verified upstream fallback or an entry
in the reviewed `LICENSE_ALLOWLIST` in the script.

Two published crates ship no license file: `number_prefix` 0.4.0 and
`tree-sitter-language-pack` 1.21.0. For those, the script embeds the upstream
MIT text and checks its pinned SHA-256 before writing it. The index marks these
entries as a verified upstream fallback.

The registry path is resolved automatically from `CARGO_HOME` or
`~/.cargo/registry/src`. `--cargo-registry` remains available as an explicit
override but the release workflow does not need it.

## Pinned ripgrep parity reference

The release workflow downloads the official ripgrep 15.1.0 asset and pins its
SHA-256 value. This binary is **not** packaged. The smoke test uses it only as
an optional parity reference. The reference runs by absolute path and never
enters `PATH`.

- `ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz`:
  `1c9297be4a084eea7ecaedf93eb03d058d6faae29bbc57ecdaf5063921491599`
- `ripgrep-15.1.0-x86_64-pc-windows-msvc.zip`:
  `124510b94b6baa3380d051fdf4650eaa80a302c876d611e9dba0b2e18d87493a`

The Linux job uses the musl build for the `x86_64-unknown-linux-gnu` target.
The binary is static and runs on glibc systems.

## What the smoke checks prove

The extracted archive smoke runs `scripts/wg_release.py smoke` with a small
fixture, isolated service, cache, and temp directories. It removes every
`PATH` directory that contains `rg` and unsets `WG_RG`.

It proves that the archive:

- starts the local service with `wg up --json`;
- reports one ready root with `wg status --json`;
- answers a semantic query;
- answers `wg -n -F`, `wg -l`, and `wg --files`;
- keeps ignore rules;
- returns the `0`/`1`/`2` exit-code contract;
- shuts down cleanly with `wg down`.

When `--rg` is given, the smoke also compares sorted text output with ripgrep
15.1.0 for `-n -F`, `-l`, and `--files`. This is an optional parity check. It is
not a dependency. A parity failure fails the smoke.

The checksum step proves that each file matches `PROVENANCE/SHA256SUMS`. The
smoke does not prove model quality, cold-cache download behavior, or behavior on
a clean machine. Run a second-person smoke before the first public release.

## Notes

- macOS jobs are not enabled. A commented stub stays in `release.yml`.
- The workflows were syntax-checked locally. The release body step has not run
  in this environment. The first real run must be watched.
- The release workflow does not commit, tag, or publish. Those actions stay
  explicit maintainer steps.
