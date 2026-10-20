//! wgrep's trigram index. Everything under `src/rg` is vendored ripgrep
//! 15.1.0; this module is the only new search logic, and it can only remove
//! files from rg's search, never change how a file is searched.

pub(crate) mod build;
pub(crate) mod cli;
pub(crate) mod client;
pub(crate) mod daemon;
pub(crate) mod events;
pub(crate) mod fold;
pub(crate) mod format;
pub(crate) mod ignore_deps;
pub(crate) mod plan;
pub(crate) mod profile;
pub(crate) mod prune;
pub(crate) mod spill;
pub(crate) mod store;
pub(crate) mod watch_budget;
