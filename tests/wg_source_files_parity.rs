//! File discovery parity: `source_files` must return exactly the files that
//! `rg 15.1 --files` returns, with the same relative spelling. The comparison
//! is order-insensitive because `source_files` sorts its result.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn rg_bin() -> Option<PathBuf> {
    let candidate = std::env::var_os("WG_RG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rg"));
    let ok = Command::new(&candidate)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    ok.then_some(candidate)
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("keep")).unwrap();
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::create_dir_all(root.path().join("skip")).unwrap();
    std::fs::create_dir_all(root.path().join(".hidden")).unwrap();
    std::fs::write(root.path().join("keep/a.txt"), b"alpha\n").unwrap();
    std::fs::write(root.path().join("keep/b.rs"), b"beta\n").unwrap();
    std::fs::write(root.path().join("sub/c.txt"), b"gamma\n").unwrap();
    std::fs::write(root.path().join("skip/d.txt"), b"hidden\n").unwrap();
    std::fs::write(root.path().join(".hidden/e.txt"), b"hidden\n").unwrap();
    std::fs::write(root.path().join(".ignore"), b"skip/\n").unwrap();
    std::fs::write(root.path().join(".rgignore"), b"ignored.txt\n").unwrap();
    std::fs::write(root.path().join("ignored.txt"), b"ignored\n").unwrap();
    std::fs::write(root.path().join("kept.txt"), b"kept\n").unwrap();
    root
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).into()
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    String::from_utf8_lossy(bytes).as_ref().into()
}

fn rg_files(rg: &Path, root: &Path, target: &str) -> Vec<PathBuf> {
    let output = Command::new(rg)
        .args(["--no-config", "--files", "-0", "--", target])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .expect("run reference rg");
    assert!(output.status.success(), "rg failed: {output:?}");
    output
        .stdout
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(path_from_bytes)
        .collect()
}

fn sorted(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    files.sort_unstable_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    files
}

#[test]
fn discovery_matches_reference_rg_for_a_directory() {
    let Some(rg) = rg_bin() else {
        eprintln!("skipping: no ripgrep (set WG_RG)");
        return;
    };
    let fixture = fixture();
    let (root, ours) = wgrep::source_files(fixture.path()).expect("source_files");
    assert_eq!(root, fixture.path());
    let reference = rg_files(&rg, fixture.path(), ".");
    assert_eq!(sorted(ours), sorted(reference));
}

#[test]
fn discovery_matches_reference_rg_for_a_file() {
    let Some(rg) = rg_bin() else {
        eprintln!("skipping: no ripgrep (set WG_RG)");
        return;
    };
    let fixture = fixture();
    let file = fixture.path().join("keep/a.txt");
    let (root, ours) = wgrep::source_files(&file).expect("source_files");
    assert_eq!(root, fixture.path().join("keep"));
    let reference = rg_files(&rg, root, "a.txt");
    assert_eq!(sorted(ours), sorted(reference));
}
