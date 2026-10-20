//! wgrep-sem: opt-in native semantic ranking over wgrep-style discovery.
//!
//! Separate binary and separate CLI path, so the default `wgrep`/`rg`
//! byte-identical search path is textually untouched by this work:
//!
//! - discovery honors ripgrep's default ignore semantics (hidden files
//!   skipped, gitignore/ignore/rgignore honored) via the same `ignore`
//!   walker defaults `wgrep` builds from;
//! - candidates are NEVER constrained to literal trigram hits: every
//!   indexed chunk is rankable, so semantic-only matches are possible;
//! - one resident daemon per indexed root holds the model weights and all
//!   file vectors in memory and serves many concurrent clients over
//!   loopback, instead of per-agent model/index copies. There is NO
//!   persistent vector index or mmap in this stage: every daemon start
//!   rebuilds (parallel chunk + bounded-batch embed); rebuild cost is
//!   reported at startup and in the completion report. Persistence is an
//!   explicitly staged follow-up, not an implemented feature;
//! - ranking work is admission-controlled (bounded workers + queue, explicit
//!   `busy` errors) so 32 agents cannot oversubscribe the box;
//! - source updates flow through the file watcher with incremental
//!   re-embedding; freshness is keyed on size + mtime + change token, the
//!   same rule the trigram index uses.
//!
//! Stage 1 (this tree): file discovery, line-window chunks, static
//! embeddings (lookup + mean-pool + L2-normalize, no transformer at query
//! time), BM25 + cosine fusion with exact-identifier priority, resident
//! daemon, JSON/text output with source-verifiable ranges and scores.

pub(crate) mod chunks;
pub(crate) mod client;
pub(crate) mod corpus;
pub(crate) mod daemon;
pub(crate) mod iocapped;
pub(crate) mod lex;
pub(crate) mod model;
pub(crate) mod rank;
pub(crate) mod rust_chunks;
pub(crate) mod slots;

/// Embedding width. Both pinned models project to 256 dims.
pub(crate) const DIM: usize = 256;

/// Daemon protocol version, reported by `ping` and `stats`.
pub(crate) const PROTO: u32 = 1;
