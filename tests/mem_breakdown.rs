//! Diagnostic memory accounting for one live index root.
//!
//! This test is ignored by default. Set `WG_MEM_ROOT` to a corpus directory and
//! run it with `--ignored --nocapture` to print the per-component breakdown.
//! `VmHWM` is the peak resident set of this process; on Linux it is the honest
//! transient peak, not the accounting estimate.

use std::path::PathBuf;
use std::sync::Arc;
use wgrep::ModelChoice;
use wgrep::live::{LiveIndex, ModelPool};

fn status_kib(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix(field)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

#[test]
#[ignore = "diagnostic: set WG_MEM_ROOT and pass --ignored --nocapture"]
fn print_memory_breakdown() {
    let root = std::env::var_os("WG_MEM_ROOT").expect("WG_MEM_ROOT must name a corpus directory");
    let pool = Arc::new(ModelPool::new(None));
    let live = match std::env::var_os("WG_MEM_CACHE") {
        Some(cache) => LiveIndex::open_with_cache(
            &PathBuf::from(root),
            ModelChoice::Code,
            pool,
            Some(PathBuf::from(cache)),
        )
        .unwrap(),
        None => LiveIndex::open(&PathBuf::from(root), ModelChoice::Code, pool).unwrap(),
    };
    let report = live.memory_report();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    println!(
        "model=({} rows x 256 f32) files={} chunks={}",
        63457, report.files, report.chunks
    );
    println!(
        "estimate_mib={:.1} rss_mib={:.1} peak_rss_mib={:.1} anon_mib={:.1} file_mib={:.1} shmem_mib={:.1}",
        report.total_estimate_bytes as f64 / 1048576.0,
        status_kib("VmRSS:").unwrap_or(0) as f64 / 1024.0,
        status_kib("VmHWM:").unwrap_or(0) as f64 / 1024.0,
        status_kib("RssAnon:").unwrap_or(0) as f64 / 1024.0,
        status_kib("RssFile:").unwrap_or(0) as f64 / 1024.0,
        status_kib("RssShmem:").unwrap_or(0) as f64 / 1024.0,
    );
}
