# wg clean-install test

Date: 2026-10-22
Commit: `2266600` (`git archive HEAD`)
Scope: README and `docs/agent-guide.md`, clean Linux and clean Windows.

## Result

The main paths work. A new user can build `wg`, package a release archive,
extract it, add `bin/` to `PATH`, run `wg up`, run semantic and text searches,
attach a second repository, and stop the daemon.

There are 2 launch blockers, 8 should-fix items, and 5 nits. The two blockers
are the private repository in the primary install command and the blocking
`-g` example when stdin is an open pipe.

## Environment

### Linux

- VPS: Debian 13.7, kernel 6.12.111, x86_64, 4 vCPU, 8 GB RAM.
- Rust: 1.99.0 for the main build. Rust 1.95.0 for the minimum-version check.
- Fresh Unix user: `wgtester`, home `/home/wgtester`, empty `~/.cache`.
- Test root: `/srv/workspaces/wispd/wg-clean-2266600`.
- No `WG_*` or `WGREP_*` variables. No `rg` on `PATH` for `wg` runs.
- Network on. Other VPS tasks ran during the test, so Linux timings can be
  inflated by CPU and disk contention.

### Windows

- Windows 11 Pro, build 26200.
- Rust 1.98.1.
- Fresh profile: `E:\wg-clean\profile` (empty before the first run).
- Empty `LOCALAPPDATA`, `APPDATA`, `HF_HOME`, `TMP`, and `TEMP` under
  `E:\wg-clean\profile`.
- Build target: `E:\wispd-target\wg-clean`.
- `rg` not on `PATH` for `wg` runs.

## Steps and timings

### 1. Linux archive path

```sh
git -C C:/git/wispd archive --format=tar.gz -o wg-src-2266600.tar.gz HEAD
tar -C src -xzf wg-src-2266600.tar.gz
CARGO_TARGET_DIR=.../target cargo build --locked --release --bin wg -j2
python3 scripts/wg_release.py export --repo .../src --out .../export
python3 scripts/wg_release.py archive --export .../export \
  --wg .../target/release/wg --platform linux-x64 \
  --label PREVIEW --out .../dist/wg-0.1.0-linux-x64-PREVIEW.tar.gz
tar -C install1 -xzf .../dist/wg-0.1.0-linux-x64-PREVIEW.tar.gz
PATH=<archive>/bin:<tool-dir> wg up
```

- Build: 257 s (`-j2`), release binary 17,529,448 bytes.
- Archive: 7,669,921 bytes. Export: 126 files.
- First `wg up`: 4.22 s, model setup 1.74 s, build 2.11 s.
- First-run cache delta: 58,961,538 bytes.
  - Hugging Face model: 33,514,580 bytes.
  - tree-sitter language pack: 23,585,773 bytes.
  - `wgrep` service and index: 1,861,185 bytes.
- First-run stderr (plain `wg up`): one line, `Preparing wg index...`.
- First-run `wg up --json`: stderr 0 bytes, clean JSON object with `pid`.
- Warm `wg up` (same repository): 0.47 s. Second warm call: `Total: 0.00s`.
- Queries: semantic 7 ms; `-n -F` 9 ms; `-l` 7 ms; `--files` 7 ms;
  `status` 6 ms; `down` 4 ms.

### 2. Linux `cargo install` path

```sh
HOME=.../home2 CARGO_HOME=.../cargo-home2 RUSTUP_HOME=/home/wgtester/.rustup \
  CARGO_TARGET_DIR=.../cargo-install-target \
  cargo install --locked --path . --bin wg
```

- Build and install: 300 s, 230 crates.
- `CARGO_HOME`: 207 MiB. Target: 2.7 GiB.
- Installed binary: `.../cargo-home2/bin/wg`, 17,537,704 bytes.
- First `wg up` with this binary: 3.03 s, cache delta 57,286,907 bytes.
- The README `--git` form was not tested because the repository is private.
  `GIT_TERMINAL_PROMPT=0 git ls-remote https://github.com/holdfast-labs/wgrep`
  fails with `fatal: could not read Username for 'https://github.com'`.

### 3. Agent-style use

A Python harness ran `wg` with `capture_output=True` and a 120 s timeout.
All calls had explicit paths or were semantic. No call hung.

- `wg up --json`: exit 0, valid JSON, stderr 0 bytes.
- `wg status --json`: valid JSON, stderr 0 bytes.
- `wg "..." --json --top 3`: valid array, stderr 0 bytes.
- `wg "..." --json --explain --top 3`: scores present, stderr 0 bytes.
- `wg --json -F "def " .`: 2,329 newline-delimited rg events, stderr 0 bytes.
- `wg -F WgNoMatchSentinelC903x .`: exit 1, no stderr.
- `wg -e "(" .`: exit 2, clear regex error on stderr.
- `cat file | wg -F needle -`: exit 0.
- `wg down`: exit 0. Later semantic query: exit 2 with the clear
  "no wg service is running" message.

Windows Python harness: same result. Warm `wg up --json` 609 ms; all JSON
outputs clean; no hangs.

### 4. Error paths

| Case | Exit | Message | Clear? |
| --- | ---: | --- | --- |
| Semantic query before `wg up` | 2 | `wg: no wg service is running for this cache; run wg up first` | Yes |
| `wg up` on missing path | 2 | `wg: not a directory: <path>` | Yes, but leaves a daemon |
| `wg up` on a file | 2 | `wg: not a directory: <file>` | Yes, but leaves a daemon |
| Semantic query on a subdirectory | 2 | `wg: directory-scoped semantic queries are not supported by the live index API` | Explicit, not actionable |
| Semantic query on a file | 2 | `wg: file-scoped semantic queries are not supported by the live index API` | Explicit, not actionable |
| `wg down` twice | 2 | `wg: No such file or directory (os error 2)` | No |
| `wg status` with no daemon | 2 | `wg: No such file or directory (os error 2)` | No |
| Query after `wg down` | 2 | `wg: no wg service is running for this cache; run wg up first` | Yes |
| `--explain` without `--json` | 2 | `wg: --explain requires --json` plus help | Yes |
| `--top 0` or `--top 101` | 2 | `wg: --top/-k must be between 1 and 100` plus help | Yes |

Windows uses the same messages, but the OS error text is
`wg: The system cannot find the file specified. (os error 2)`.

### 5. Windows archive path

```powershell
git -C C:\git\wispd archive --format=zip -o E:\wg-clean\src.zip HEAD
cargo build --locked --release --bin wg -j2   # cwd E:\wg-clean\src
python scripts\wg_release.py export --repo E:\wg-clean\src --out E:\wg-clean\export
python scripts\wg_release.py archive --export E:\wg-clean\export `
  --wg E:\wispd-target\wg-clean\release\wg.exe --platform windows-x64 `
  --label PREVIEW --out E:\wg-clean\dist\wg-0.1.0-windows-x64-PREVIEW.zip
```

- Build: 7 min 56 s. Binary: 12,827,648 bytes. Archive: 7,577,135 bytes.
- First `wg up`: 4.38 s, model setup 1.61 s, build 1.93 s.
- First-run profile delta: 57,911,994 bytes.
  - Hugging Face model: 33,514,412 bytes.
  - tree-sitter language pack: 21,719,741 bytes.
  - `wgrep` service and index: about 1.9 MB.
- Text index: 159 files. No cold-start 0-file result on Windows (the clone was
  older than 10 s when `wg up` ran).
- Warm `wg up --json`: 609 ms.
- Queries: semantic 34 ms; `-n -F` 128 ms; `--files` 36 ms.

### 6. Minimum Rust version

`cargo +1.95.0 build --locked --release --bin wg -j2` from the archive source
succeeded in 5 min 34 s. The README minimum of Rust 1.95 holds.

## Findings

Severity: `blocker` stops the public or agent path. `should-fix` causes a wrong,
confusing, or unsafe result. `nit` is small.

### 1. Blocker: the primary install command points to a private repository

README lines 32-34 give this command as the first install method:

```sh
cargo install --locked --git https://github.com/holdfast-labs/wgrep --bin wg
```

Reproduction:

```sh
GIT_TERMINAL_PROMPT=0 git ls-remote https://github.com/holdfast-labs/wgrep
```

Observed:

```
fatal: could not read Username for 'https://github.com': terminal prompts disabled
```

`cargo install --git` has the same result for a user without credentials. The
same repository URL is the source for the release page. A public user cannot
use the first install command or download a release until the repository or a
public mirror is available.

Suggested fix: make the repository public before launch, or use a public URL
and test both `--git` and the release page. Until then, make the release archive
the first install method.

### 2. Blocker for agent/CI use: the `-g` example blocks when stdin is a pipe

README line 81 and agent-guide line 42 use `-g` with no search path:

```sh
wg -g "*.rs" "unsafe"
```

In an interactive terminal this works. When stdin is an open pipe, as in most
agent harnesses and CI shells, it blocks and waits for EOF.

Reproduction:

```sh
cd <repo>
timeout 20 sh -c 'sleep 300 | wg -g "*.py" "def"'
# exit 124, no output

wg -g "*.py" "def" < /dev/null   # works
wg -g "*.py" "def" .             # works
```

The same applies to `wg -F "needle"` with no path. This matches ripgrep's rule:
when stdin is not a terminal and no path is given, ripgrep reads stdin. The
result is correct for ripgrep, but the docs do not state the rule and the agent
guide gives a command that hangs in the agent context. In the docs run, the
exact README command took 891 s and returned only when the SSH stdin closed.

Suggested fix: add `.` to the examples, and add this rule to the agent guide:

> Always give an explicit path. If stdin can be a pipe, use `.` or redirect
> stdin from `/dev/null`. A command with no path reads stdin, as ripgrep does.

### 3. Should-fix: a failed `wg up` leaves a daemon running

Reproduction:

```sh
wg up /srv/workspaces/wispd/wg-clean-2266600/does-not-exist; echo $?
wg status; echo $?
wg down
```

Observed:

```
Preparing wg index...
wg: not a directory: /srv/workspaces/wispd/wg-clean-2266600/does-not-exist
2
pid=511501 models=0 roots=0
0
Stopped wg service (pid 511501).
```

Windows has the same behavior: `wg up E:\wg-clean\does-not-exist` exited 2
and left daemon PID 85136. A later `wg down` stopped it.

Impact: a typo starts a background process with no roots. `wg status` reports
it, and a later `wg up` can reuse it. A new user can also think the failed
command succeeded.

Suggested fix: start the service only after the path check, or stop a service
that this `wg up` started when the build fails.

### 4. Should-fix: the second `wg up` omits the daemon PID

README line 53 says `wg up` prints the daemon PID. The first call does. The
second call in the same repository does not.

Reproduction:

```sh
wg up
wg up
```

Observed on the second call:

```
Ready: 161 files, 1191 chunks
Root: /srv/workspaces/wispd/wg-clean-2266600/repos/click
Build: 0.13s | Model setup: 0.28s | Total: 0.00s | PID: null
```

`wg up --json` on the second call omits `pid`, `proto`, `loaded_models`,
`model_loads`, `roots`, and `uptime_ms`:

```json
{"build_ms":139,"building":false,"model_ms":278,"ok":true,"root":{...},"total_ms":5}
```

Agent code that reads `pid` from the `wg up --json` result breaks on the second
call. The agent guide says a later `wg up` reuses the daemon, so this path is
normal.

Suggested fix: include `pid` and the other service fields in the already-ready
response, or state in the README that `pid` is only present on first attach.

### 5. Should-fix: `wg down` twice and `wg status` with no daemon return a raw OS error

Reproduction:

```sh
wg down
wg down
wg status
```

Observed:

```
Stopped wg service (pid 477731).
wg: No such file or directory (os error 2)
wg: No such file or directory (os error 2)
```

Windows:

```
wg: The system cannot find the file specified. (os error 2)
```

This message is not actionable and does not match the clear semantic-query
message. README does not define `wg down` on an already stopped service.

Suggested fix: use `no wg service is running for this cache; run wg up first`.
Make `wg down` idempotent (exit 0 with a short note) or keep exit 2 with the
clear message.

### 6. Should-fix: a fresh clone can produce a zero-file text index

Reproduction on Linux:

```sh
git clone --depth 1 https://github.com/pallets/click.git click
cd click
wg up
wg status
```

Observed:

```
Text index: 0.01s | 0 files | 0.0 MB on disk | 0.0 MB resident
text: ready [reconciling], 0 files, 0.0 MB disk, 0.0 MB resident
```

The index file is 283 bytes and `needs_rescan` is true. `wg -F`, `wg -l`,
`--files`, and semantic search still work. The count is wrong.

Control tests:

- Sleep 10 s after `git clone`, then `wg up`: text index 159 files.
- Delete `~/.cache/wgrep/indexes/<hash>` and run `wg up`: text index 159 files.
- A second repository (`pallets/itsdangerous`) showed 36 files in `wg up` but
  0 files in the text index when `wg up` ran immediately after clone.

Suggested fix: complete the text-index reconcile before `wg up` reports ready,
or do not print a zero count as ready. Add a regression test that clones a
repository and runs `wg up` with no delay.

### 7. Should-fix: the agent guide contradicts the observed text route

`docs/agent-guide.md` line 4 says both routes use one shared daemon. Line 46
says the text route runs `rg` inside the daemon. Line 85 says to restart the
daemon after changing rg environment variables.

Observed:

- After `wg down`, `wg -F "def " .`, `wg -l "def " .`, and `wg --files` return
  correct results without a daemon. README line 90 is correct.
- With no daemon, `RIPGREP_CONFIG_PATH=<file with --hidden>` changed
  `wg --files` from 161 files to 207 files. The setting is read for each call,
  not by the daemon.

Suggested fix: change the agent guide to say that semantic search uses the
daemon and the filesystem route runs in-process, and that rg environment
variables apply to each text call. Keep README line 90 as the source of truth.

### 8. Should-fix: the public README does not explain how to use a release archive

README lines 39-40 say release archives exist and that `wg` is a single binary.
The archive contains `bin/wg`, `README.md`, `docs/`, `licenses/`, `PROVENANCE/`,
and `source/`. It does not contain `wg` at the archive root.

A new user who downloads the archive gets no instruction to run `bin/wg` or to
add the archive `bin/` directory to `PATH`. `docs/release.md` line 59 already
tells maintainers to give that instruction, but the public README does not.

Suggested fix: add a short "From a release archive" section:

```sh
tar -xzf wg-<version>-linux-x64.tar.gz
export PATH="$PWD/wg-<version>-linux-x64/bin:$PATH"
wg --version
```

Use the equivalent `Expand-Archive` steps on Windows.

### 9. Should-fix: `--model text` fails on a root attached with `auto`

README lines 67-70 present `--model auto|code|text` as a query option. A user
who runs `wg up` and then `--model text` gets an error.

Reproduction:

```sh
wg up
wg "where are command line options parsed" --model text
```

Observed:

```
wg: query model differs from attached root model; use the model selected by wg up
```

Control: `wg up --model text` then the same query with `--model text` works.
`--model code` works on an `auto` root.

Suggested fix: document that the model is fixed at `wg up`; a query `--model`
must match the attached root and cannot switch it.

### 10. Should-fix: `wg_release.py archive` can ship no dependency licenses

The Linux archive was built without `--cargo-registry` and without
`CARGO_HOME` in the environment. `BUILD_INFO.json` reports:

```json
"dependency_licenses": {
  "registry_src": null,
  "packages_copied": 0,
  "packages_unavailable": 301
}
```

`licenses/deps/` is absent. `licenses/DEPENDENCY_LICENSES.txt` lists all 301
packages as unavailable. The Windows archive copied 195 packages and listed 106
as unavailable because it found a partial registry candidate at
`E:\wgrep-worktrees\...`.

Cause: `resolve_registry` in `scripts/wg_release.py` checks the explicit value
and `CARGO_HOME`, but it does not fall back to `~/.cargo/registry/src`. The
release workflow does not pass `--cargo-registry`.

Suggested fix: fall back to `$HOME/.cargo/registry/src`, or pass
`--cargo-registry` from the workflow. Consider failing a `RELEASE` archive when
`packages_unavailable` is greater than 0.

### 11. Nit: the first-run download is about 58 MB, not 33.5 MB

README line 22-24 says the model assets are "about 33.5 MB in the recorded run"
and mentions parser assets separately. Measured:

- Linux total: 58,961,538 bytes. Model 33,514,580; tree-sitter 23,585,773.
- Windows total: 57,911,994 bytes. Model 33,514,412; tree-sitter 21,719,741.

The model number matches. A new user sees about 58 MB on a cold cache.

Suggested fix: state the total, for example "about 33.5 MB for the embedding
model plus about 22-24 MB for parser assets (about 58 MB total)".

### 12. Nit: Requirements omit the C linker and build tools

README lines 19-21 list only Rust 1.95 or newer. On a rustup-only `PATH`,
`cargo install --locked --path . --bin wg` failed with:

```
error: linker `cc` not found
```

After adding `/usr/bin` to `PATH`, the same command succeeded in 300 s.

Suggested fix: add "A C linker and standard build tools (for example,
`build-essential` on Debian/Ubuntu) are required to build from source."

### 13. Nit: semantic scope errors are explicit but not actionable

Observed:

```
wg: directory-scoped semantic queries are not supported by the live index API
wg: file-scoped semantic queries are not supported by the live index API
```

The README says an explicit error is returned, so this is technically correct.
It does not tell the user what to do.

Suggested fix: `semantic search covers the whole attached repository; query
the repository root instead of a subdirectory or file`.

### 14. Nit: PowerShell shows a successful `wg up` as an error record

On Windows, plain `wg up` writes `Preparing wg index...` to stderr. PowerShell
5.1 reports that line as `NativeCommandError` in the console even when the exit
code is 0. `--json` avoids the line.

Suggested fix: in the README, recommend `wg up --json` for scripts, or write
the status line only when stderr is a terminal.

### 15. Nit: Windows prints the verbatim path prefix

`wg up` prints `Root: \\?\E:\wg-clean\repo\click`. `status --json` contains
`"path":"\\\\?\\E:\\wg-clean\\repo\\click"`. The paths are valid, but the
prefix is unfamiliar. A user may think the path is wrong.

Suggested fix: strip the `\\?\` prefix for display, or mention it in the
Windows notes.

## Docs check

| README or guide item | Result |
| --- | --- |
| `cargo install --locked --git ...` | Blocked: repository is private (finding 1) |
| `cargo install --locked --path . --bin wg` | Works with a C linker (finding 12) |
| Release archives, single binary, no `rg` | Archive works; README lacks `bin/` steps (finding 8) |
| `wg up` prints root, files, chunks, build, model, total, PID | First call yes; second call `PID: null` (finding 4) |
| `wg up --json` clean, no progress | Yes, first and second call; second call lacks `pid` |
| Spinner on interactive stderr | Code path exists; not tested with a pseudo-TTY for `up` |
| `--top N` / `-k N`, 1..=100, default 5 | 10 rows for `--top 10` and `-k 10`; 5 by default; invalid values rejected |
| `--json` semantic array | Yes |
| `--explain` needs `--json` | Yes; error is clear |
| `--model auto|code|text` | `auto` and `code` work; `text` needs `wg up --model text` (finding 9) |
| Text flags `-F`, `-e`, `-n`, `-l`, `-g`, `--files` | Work with an explicit path or a TTY; `-g` without a path blocks on a pipe (finding 2) |
| `--json -F` is newline-delimited rg JSON | Yes, first event type `begin` |
| `cat file | wg -F needle -` | Yes |
| Text route does not need `wg up` or the daemon | Yes; contradicts agent guide (finding 7) |
| `RIPGREP_CONFIG_PATH` behaves as rg | Yes, per call |
| Second repository attaches to one daemon | Yes: one PID, two roots, one loaded model |
| `wg status` and `status --json` | Yes with a daemon; raw OS error without one (finding 5) |
| `wg down` stops every root | Yes |
| Nearest attached ancestor is used with no ROOT | Yes, from `src/click` |
| Subdirectory or file semantic query gives an explicit error | Yes, but not actionable (finding 13) |
| Text route supports file and directory paths | Yes |
| Live create is searchable from the next query | Yes, first query after create found the new file |
| Live edit, rename, delete | Not tested |
| Unhealthy watcher reports an error and a recovery hint | Not tested |
| Exit codes 0/1/2 | Text: 0, 1, 2 verified. Semantic error: 2 verified. Semantic no-match: not tested |
| `sgrep` and `wgrep-sem-shared` also build | Not tested; the archive ships only `wg` |
| README media and docs links | Files are present in the archive |
| macOS supported | Not tested; README says not qualified |
| GitHub Releases assets and checksums | Not tested; repository is private |

## Test gaps

- `cargo install --git` (private repository).
- GitHub Releases page and published checksums.
- macOS.
- Live edit, rename, and delete; unhealthy watcher and recovery hint.
- Binary files, non-UTF-8 encodings, and the 8 MiB pipe/reply limits.
- `sgrep` and `wgrep-sem-shared` binaries.
- Interactive spinner with a real terminal for `wg up`.
- Windows `cargo install --path`.
- Rust 1.95 on Windows (Linux 1.95 build passed).

## Notes

- The release archive smoke test in `scripts/wg_release.py` was not run. The
  manual checks above cover the same commands and add cold-cache, docs, and
  error-path checks.
- The test user, service directories, and build targets were removed after the
  run. Build targets were 2.7 GiB (`target`), 2.7 GiB (`cargo-install-target`),
  and 2.6 GiB (`target-195`) on the VPS. The Windows target was removed at the
  end.
