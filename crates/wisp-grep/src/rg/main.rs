/*!
The `wgrep` binary entry point. The implementation lives in the crate library
so `wg` can run the same vendored ripgrep in-process.
*/

fn main() -> std::process::ExitCode {
    wisp_grep::run_cli()
}
