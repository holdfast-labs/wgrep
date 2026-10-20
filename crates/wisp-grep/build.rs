// Adapted from ripgrep 15.1.0's build.rs (Unlicense OR MIT).
fn main() {
    set_git_revision_hash();
    set_windows_exe_options();
}

/// Embed ripgrep's Windows manifest (long path support) into `wgrep`.
fn set_windows_exe_options() {
    static MANIFEST: &str = "pkg/windows/Manifest.xml";

    let Ok(target_os) = std::env::var("CARGO_CFG_TARGET_OS") else { return };
    let Ok(target_env) = std::env::var("CARGO_CFG_TARGET_ENV") else { return };
    if !(target_os == "windows" && target_env == "msvc") {
        return;
    }
    let Ok(mut manifest) = std::env::current_dir() else { return };
    manifest.push(MANIFEST);
    let Some(manifest) = manifest.to_str() else { return };

    println!("cargo:rerun-if-changed={MANIFEST}");
    println!("cargo:rustc-link-arg-bin=wgrep=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bin=wgrep=/MANIFESTINPUT:{manifest}");
    println!("cargo:rustc-link-arg-bin=wgrep=/WX");
}

/// Expose the short git hash as `RIPGREP_BUILD_GIT_HASH` for `--version`.
fn set_git_revision_hash() {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output();
    if let Ok(output) = output {
        let rev = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !rev.is_empty() {
            println!("cargo:rustc-env=RIPGREP_BUILD_GIT_HASH={rev}");
        }
    }
}
