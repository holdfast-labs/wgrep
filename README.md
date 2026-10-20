# wg

`wg` is one local front door for code and text search. One command gives you two
search engines:

- **Semantic ranking (the default).** `wg QUERY [ROOT]` ranks source chunks with
  the sgrep-derived hybrid scorer. It finds code by meaning as well as by name.
- **Filesystem text and regex search.** A familiar ripgrep flag such as `-F`,
  `-e`, `-n`, `-l`, `-g` or `--files` selects exact ripgrep behavior.

Both routes go through one shared background daemon. The daemon keeps a live
index for each attached repository and shares loaded models between them.

![wg quickstart](docs/media/wg-quickstart.gif)

*Ask in plain English, or use a ripgrep flag. Both routes use one daemon.*

## Requirements

- Rust 1.90 or newer. The crate uses edition 2024.
- `rg` (ripgrep) on `PATH`. `wg` uses it to select searchable files and to run
  filesystem searches. `WG_RG` can select a specific text-search backend;
  semantic file discovery still requires `rg` on `PATH`.
- A network connection on the first run. `wg` downloads the pinned embedding
  model assets (about 33.5 MB in the recorded run) and any needed language
  parser assets. Later runs use the local cache.

## Install

Linux x64 and Windows x64 are supported. macOS is not qualified yet.

With Rust installed, build and install `wg` from source:

```sh
cargo install --locked --git https://github.com/holdfast-labs/wgrep --bin wg
```

From a checkout, use `cargo install --locked --path . --bin wg`. Either command
installs the `wg` binary into your Cargo binary directory.

Release archives (on the GitHub Releases page) bundle ripgrep, so they do not
need `rg` on `PATH`. Extract one and use the outer `wg`/`wg.cmd` launcher. The launcher finds the bundled `rg`; running
`bin/wg` directly still needs `rg` on `PATH` or `WG_RG`.

## Quick start

Go to the repository you want to search and run setup once:

```sh
cd path/to/repo
wg up
```

`wg up` builds the index, starts or connects to the shared daemon, attaches the
repository, and returns only when search is ready. It prints the root, the file
and chunk counts, the build time, the model setup time, the total time, and the
daemon PID. In an interactive terminal it shows a spinner and elapsed time on
stderr. With `--json` the output is one clean JSON object and no progress text.

Then search:

```sh
# Natural-language or symbol query (semantic, the default):
wg "where do incoming requests get validated"
wg "decodePacketV17"
wg "decodePacketV17" --top 10            # or -k 10
wg "packet framing" --json --explain --top 5
```

- `--top N` / `-k N` sets the semantic result count (1..=100, default 5).
- `--json` alone prints a semantic JSON array.
- `--explain` adds score fields and requires `--json`.
- `--model auto|code|text` chooses the embedding policy. `auto` is the default.

### Exact text and regex search

Use ripgrep flags. These select the filesystem route:

```sh
wg -F "fn main" .                        # literal text
wg -e "fn\s+\w+\(" .                     # regular expression
wg -n -e "TODO" .                        # with line numbers
wg -l "deprecated" .                     # file names only
wg -g "*.rs" "unsafe"                    # filename glob
wg --files                               # list searchable files
wg --json -F "needle" .                  # ripgrep JSON events
```

The daemon runs a real, authoritative `rg` child for these requests. It
preserves your arguments, working directory, stdout bytes, stderr bytes, and
exit status. So ignore rules, binary handling, encodings, and diagnostics are
ripgrep's own. This route reads the filesystem; the shared semantic index does
not accelerate regex scans.

You can pipe input the same way as ripgrep, for example
`cat file.txt | wg -F needle -`.

The preview buffers pipe input before forwarding it. Limits are 8 MiB of
stdin, 8 MiB of stdout, and 1 MiB of stderr per text request. The direct rg child
has a 60-second default time bound. Exceeding a bound returns an error, not a
partial successful result. Live/unbounded streams and external preprocessing
helpers are not qualified in this preview.

Command arguments and the caller's working-directory path must be UTF-8.
File content and captured output can contain arbitrary bytes. Explicit rg
formatting flags are forwarded; automatic color/headings reflect the child's
captured streams. The child inherits the daemon's environment. Restart the
daemon after changing rg-related environment variables.

## A second repository

![two repositories, one daemon](docs/media/wg-multi-repo.gif)

*One daemon, one model, two repositories.*

Run `wg up` again in another directory. It attaches that repository to the same
daemon. One daemon PID serves all attached repositories, with one shared model
pool. Run `wg status` to see every attached root:

```sh
wg status
wg status --json
```

## Stop and clean up

```sh
wg down
```

`wg down` stops the shared daemon. It stops search for every attached
repository, not just the current one. After you stop it, run `wg up` again for
the repositories you want.

## Choose the right route

- Use a plain query for ranked, meaning-based results: "where is this
  handled", "the function that parses the header".
- Use `-F` or `-e` when you need exact text, exact regex, or every match.
- A semantic result list is model-ranked. It is not exhaustive. Do not treat it
  as a complete grep.

## Scope

Semantic search covers one whole attached repository at a time. Without a `ROOT`
argument, `wg` picks the nearest attached ancestor of the current directory. A
semantic query that names a file or a smaller directory returns an explicit
error; it does not fall back to approximate ranking. The filesystem route
supports file and directory paths normally, because it is ripgrep.

## Live updates

![live index update](docs/media/wg-live.gif)

*No restart. The edit is searchable before the next prompt returns.*

The daemon watches every attached root. It updates changed files, reuses
unchanged vectors, and applies edits, creates, renames, and deletes. A query
waits for delivered pending work within a fixed bound. This is not a
transactional filesystem snapshot and does not guarantee instant kernel event
delivery. If the watcher loses trust, `wg` reports an error instead of a stale
answer. To recover, run `wg down` and then `wg up`. `wg status` shows an
unhealthy root and the recovery hint.

## Compatibility binaries

The crate also builds the `sgrep` and `wgrep-sem-shared` binaries. They remain
separate commands. `wg` does not silently substitute a different engine. If its daemon is
unavailable, it reports the problem and asks you to set up the repository.

## More

- Agent setup and exit codes: [docs/agent-guide.md](docs/agent-guide.md)
- Measured setup numbers and pending benchmarks:
  [docs/benchmarks.md](docs/benchmarks.md)

The recordings above are real, short-looping VHS tapes in
[docs/demos](docs/demos/README.md). MP4 versions are in `docs/media`.
