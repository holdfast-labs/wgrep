## Summary

## Tests run

- [ ] `cargo test --locked --lib --bins`
- [ ] `cargo test --locked --test wg_lexical_parity --test wg_source_files_parity`
- [ ] `(cd crates/wisp-grep && cargo test --locked)`
- [ ] `cargo build --locked --release --bin wg`
- [ ] `python scripts/wg_release.py self-test --work <scratch-dir>`
- [ ] Not run. Explain:

## Parity impact

- [ ] Text route output is still byte-identical to `rg` 15.1.0 for applicable cases.
- [ ] Semantic results are still identical to pinned sgrep.
- [ ] Not applicable. Explain:

## Notes

Do not commit `target/`, release archives, generated binaries, logs, benchmark
scratch trees, or model caches.
