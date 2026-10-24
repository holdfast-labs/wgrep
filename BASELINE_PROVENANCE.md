# Pinned sgrep baseline provenance

This standalone crate is copied from sgrep revision
`0fbe8e5725f71667789be4a3abcea4e02f33afa4`. The pinned upstream `LICENSE` and
`Cargo.lock` are retained verbatim. Ranking, chunking, lexical processing,
model math, source eligibility, newline handling, and the CLI contract are
unchanged. The resident-library change is described below.

## Layout change (resident core)

The copied upstream CLI lived in `src/main.rs`. It now lives in `src/lib.rs`,
and `src/main.rs` is a two-line shim that calls `sgrep::cli_main()`. The moved
code keeps the upstream behavior. The added public API is `LoadedModel`
(load the verified model assets once), `Corpus` (build the passages once), and
`Searcher` (answer many queries from one resident model and corpus). The CLI
call sites for `embed`, `rank`, `candidates`, and `identifier_pattern` are
unchanged; `rank` now delegates to `rank_impl`, which the resident API also
calls. Upstream `Cargo.toml`, `Cargo.lock`, and `LICENSE` are unchanged.
`Searcher::new` now encodes each stored passage once in bounded batches, and
`Searcher` keeps the passage vectors in read-only fields. `Searcher::query`
encodes only its own query text. The CLI path (`search`) is unchanged.

## Per-file SHA-256 (current working tree)

| File | SHA-256 | Note |
| --- | --- | --- |
| `LICENSE` | `9b37abb88e6469c265d1c524be4a8b64f274adba365648c9cab0bf114df884c4` | upstream, verbatim |
| `Cargo.lock` | `38a4a2ce02dff7a3faa89403df953150f714e33b6c10b15554475ef115f282f5` | upstream, verbatim |
| `Cargo.toml` | `642dfa9d562b11ac48fd16d51c027f630e7017aac29f0f57230a0fe13a2da672` | upstream + empty `[workspace]` |
| `src/cache.rs` | `878e0a9b72757379131539db63d032b34118cc8863248c280b9bdc1ea00c903b` | upstream, verbatim |
| `src/codechunker.rs` | `dd1a62d844a8850ff580e5f6ec629f7c780365ceb4cb921ec455805e83a06722` | upstream, verbatim |
| `src/index.rs` | `0902ef830697931649d66b17e365783becde8f23fc30170006cd62a0d674b244` | upstream, verbatim |
| `src/rg_input.rs` | `ac22e812c1b6bcce285aa9193d92e7254606e39a20d36e893e29c98db74dcc98` | upstream, verbatim |
| `src/splitter.rs` | `58739805b5295171cad7f615a8e7948edfbf24681b857c19daa18245c5d0f1ca` | upstream, verbatim |
| `src/lib.rs` | `c8e68a3691c748e255732c21fe8b1c6a9bf3b0a7a64138551cc246a5b810e02b` | moved upstream `main.rs` + resident API + cached passage vectors + ignored probe |
| `src/main.rs` | `c060e64fb1eda54b81367deaff1c492299357cf9c15a5a8be33b3e4327441c18` | new two-line CLI shim |

Upstream `src/main.rs` SHA-256 was
`3e71aa16e4a35c291c0c566b4ea9db1181e8cd197107be1aff3f6bd5a80475a2`; its code
moved to `src/lib.rs`. Upstream `Cargo.toml` SHA-256 was
`0640bd8ae08b4585e77b5560835e25f32920ef24e4798652ffc43b181e8e0f35`.

## Build identities

- M1 frozen binary (built from upstream `src/main.rs`, not rebuilt here):
  `7baf3c66113d44dafdf993c79382e6c6f785c2a9968e630ba29109e35fbec842`,
  built on the Linux benchmark host.
- Resident-core binary (this working tree, `cargo build --release --locked -j2`):
  `eaa0501c4fb9ed3b4b503faed4afe263d69ee6fe3fece46e750095eb9b8d5024`,
  size `12459368`, built on the Linux benchmark host.
- Resident-core source-tree SHA-256 (sorted `sha256sum` of the seven
  `src/*.rs`, `Cargo.toml`, `Cargo.lock`, `LICENSE`, then SHA-256):
  `7f917afa872dfa105609febb9fa7e0413d1554756232069ce95fd4c34de2c926`.
- Resident-vectors binary (this working tree, `cargo build --release --locked -j2`):
  `e988ea828702f69bd3132b8e7a5981cc28a804dfbec2a77f9bcb7ae50b545875`,
  size `12476608`, built on the Linux benchmark host.
- Resident-vectors source-tree SHA-256:
  `65e6c7054fda67ca2264dca31032d460ca511ce7710b6face1ff86c3437e1cf9`.

Both binaries keep the upstream `rg --files` discovery and accept
`QUERY ROOT --model code --json --explain --no-cache -n N`. `--no-cache` skips
index reads and writes and recomputes passages and vectors.
