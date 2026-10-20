//! Parity for the unified `wg` lexical (ripgrep) route.
//!
//! Each case runs the same argv through the real `wg` binary (in-process
//! vendored ripgrep 15.1.0) and through a reference `rg` directly, then
//! compares stdout, stderr and exit status. Set `WG_RG` to the reference
//! binary (for example ripgrep 15.1.0); if no `rg` is available the test skips
//! rather than failing.
//!
//! `--sort path` is added to every multi-file case because ripgrep's traversal
//! order is not deterministic between processes. `--json` output is compared
//! with the timing-bearing `begin`/`end`/`summary` events removed.

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

struct Fixture {
    root: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("keep")).unwrap();
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::create_dir_all(root.path().join("skip")).unwrap();
    std::fs::write(
        root.path().join("keep/a.txt"),
        b"alpha needle beta\nsecond line\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("keep/b.txt"),
        b"no match here\nNEEDLE upper\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("keep/bin.dat"),
        b"needle\x00binary\ntail\n",
    )
    .unwrap();
    std::fs::write(root.path().join("sub/c.txt"), b"needle in sub\n").unwrap();
    std::fs::write(root.path().join("skip/d.txt"), b"needle hidden\n").unwrap();
    std::fs::write(root.path().join(".ignore"), b"skip/\n").unwrap();
    std::fs::write(root.path().join("crlf.txt"), b"needle\r\nmore\r\n").unwrap();
    Fixture { root }
}

struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
}

fn spawn_and_capture(mut command: Command, stdin: Option<&[u8]>) -> Run {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn().expect("spawn");
    if let Some(bytes) = stdin {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(bytes)
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait");
    Run {
        stdout: output.stdout,
        stderr: output.stderr,
        code: output.status.code().unwrap_or(-1),
    }
}

fn run_wg(fixture: &Fixture, args: &[&str], stdin: Option<&[u8]>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wg"));
    command.args(args).current_dir(fixture.root.path());
    spawn_and_capture(command, stdin)
}

fn run_rg(rg: &Path, fixture: &Fixture, args: &[&str], stdin: Option<&[u8]>) -> Run {
    let mut command = Command::new(rg);
    command.args(args).current_dir(fixture.root.path());
    spawn_and_capture(command, stdin)
}

/// Keep only `match`/`context` events so ripgrep's per-run timing events do not
/// make two identical searches compare unequal.
fn json_matches(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| line.contains("\"type\":\"match\"") || line.contains("\"type\":\"context\""))
        .map(str::to_owned)
        .collect()
}

fn assert_parity(rg: &Path, fixture: &Fixture, args: &[&str], stdin: Option<&[u8]>) {
    let reference = run_rg(rg, fixture, args, stdin);
    let ours = run_wg(fixture, args, stdin);
    let json = args.contains(&"--json");
    let (reference_out, ours_out) = if json {
        (json_matches(&reference.stdout), json_matches(&ours.stdout))
    } else {
        (
            vec![format!("{:?}", reference.stdout)],
            vec![format!("{:?}", ours.stdout)],
        )
    };
    assert_eq!(
        reference_out, ours_out,
        "stdout mismatch for {args:?}\nreference code {}\nwg code {}",
        reference.code, ours.code
    );
    assert_eq!(
        reference.code,
        ours.code,
        "exit mismatch for {args:?}\nreference stderr {:?}\nwg stderr {:?}",
        String::from_utf8_lossy(&reference.stderr),
        String::from_utf8_lossy(&ours.stderr)
    );
    assert_eq!(
        reference.stderr,
        ours.stderr,
        "stderr mismatch for {args:?}\nreference {:?}\nwg {:?}",
        String::from_utf8_lossy(&reference.stderr),
        String::from_utf8_lossy(&ours.stderr)
    );
}

#[test]
fn lexical_route_matches_reference_rg() {
    let Some(rg) = rg_bin() else {
        eprintln!("skipping: no ripgrep (set WG_RG)");
        return;
    };
    let fixture = fixture();

    let cases: &[(&[&str], Option<&[u8]>)] = &[
        (&["-F", "needle", "--sort", "path", "keep/a.txt"], None),
        (
            &["-e", "n[e]+dle", "--json", "--sort", "path", "keep/a.txt"],
            None,
        ),
        (&["-n", "-c", "needle", "--sort", "path", "."], None),
        (&["--json", "needle", "--sort", "path", "."], None),
        (&["-F", "needle", "--sort", "path", "."], None),
        (&["-F", "--text", "needle", "--sort", "path", "."], None),
        (&["-F", "needle", "--sort", "path", "sub"], None),
        (&["--files", "--sort", "path", "."], None),
        (&["-F", "needle", "does-not-exist/"], None),
        (
            &["-F", "needle", "--sort", "path", ".", "does-not-exist/"],
            None,
        ),
        (&["-F", "needle", "-"], Some(b"a needle line\nplain text\n")),
        (
            &["-F", "needle", "--sort", "path", "."],
            Some(b"ignored stdin\n"),
        ),
    ];
    for (args, stdin) in cases {
        assert_parity(&rg, &fixture, args, *stdin);
    }
}

/// Large output must pass through unchanged. The old daemon route returned an
/// explicit error above 8 MiB; the in-process route has no such cap.
#[test]
fn large_output_matches_reference() {
    let Some(rg) = rg_bin() else {
        eprintln!("skipping: no ripgrep (set WG_RG)");
        return;
    };
    let fixture = fixture();
    let mut line = b"needle".to_vec();
    line.extend(std::iter::repeat(b'a').take(9 * 1024 * 1024));
    line.push(b'\n');
    std::fs::write(fixture.root.path().join("big.txt"), &line).unwrap();

    let args = ["-F", "needle", "--sort", "path", "big.txt"];
    let reference = run_rg(&rg, &fixture, &args, None);
    let ours = run_wg(&fixture, &args, None);
    assert_eq!(ours.code, reference.code, "exit mismatch");
    assert_eq!(ours.stdout, reference.stdout, "stdout mismatch");
    assert_eq!(ours.stderr, reference.stderr, "stderr mismatch");
}
