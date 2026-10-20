//! wgrep-sem-shared: manual-start shared daemon and client for the resident
//! cached sgrep core (M4 immutable-snapshot prototype).
//!
//! The reusable shared daemon/client lives in `src/shared.rs` of the `sgrep`
//! library. The original `sgrep` binary and CLI are unchanged; this is a
//! separate binary.
use std::process::ExitCode;

fn main() -> ExitCode {
    sgrep::shared::main()
}
