//! Pinned sgrep command-line entry point.
//!
//! The reusable library lives in `lib.rs`. This shim keeps the copied `sgrep`
//! binary and its command-line behavior unchanged.
use std::process::ExitCode;

fn main() -> ExitCode {
    wgrep::cli_main()
}
