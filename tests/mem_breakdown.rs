//! Diagnostic memory accounting for one live index root.
//!
//! This test is ignored by default. Set `WG_MEM_ROOT` to a corpus directory and
//! run it with `--ignored --nocapture` to print the per-component breakdown.

use std::path::PathBuf;
use std::sync::Arc;
use wgrep::ModelChoice;
use wgrep::live::{LiveIndex, ModelPool};

#[test]
#[ignore = "diagnostic: set WG_MEM_ROOT and pass --ignored --nocapture"]
fn print_memory_breakdown() {
    let root = std::env::var_os("WG_MEM_ROOT").expect("WG_MEM_ROOT must name a corpus directory");
    let pool = Arc::new(ModelPool::new(None));
    let live = LiveIndex::open(&PathBuf::from(root), ModelChoice::Code, pool).unwrap();
    let report = live.memory_report();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    println!(
        "model=({} rows x 256 f32) files={} chunks={}",
        63457, report.files, report.chunks
    );
}
