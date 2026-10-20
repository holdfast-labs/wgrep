# wg guide for coding agents

`wg` is one command with two routes. The default is semantic ranking. A ripgrep
flag selects exact filesystem search. Both routes use one shared daemon.

## Set up once per repository

```sh
wg up
```

Run it once in the repository root. It builds the index and attaches the
repository to the shared daemon. Wait for it to finish before searching. A later
`wg up` in the same repository returns fast and reuses the daemon. Run `wg up`
in a second repository to attach it to the same daemon.

## Semantic queries (default)

Use these for meaning-based or symbol lookups:

```sh
wg "where do we validate incoming requests" --top 10
wg "decodePacketV17" --top 5
wg "packet framing" --json --top 20
```

- `--top N` / `-k N` sets the count (1..=100, default 5).
- `--json` prints a JSON array for parsing.
- `--json --explain` adds score fields.
- Results are ranked by a small local model. They are not exhaustive. For a
  complete list, use the filesystem route.

## Exact text and regex

Use a ripgrep flag. This selects the filesystem route:

```sh
wg -F "exact text" .            # literal
wg -e "regex pattern" .         # regex
wg -n -e "pattern" .            # with line numbers
wg -l "pattern" .               # file names only
wg -g "*.rs" "pattern"          # glob filter
wg --files                      # list searchable files
```

The route runs `rg` inside the daemon. It preserves arguments, working
directory, output bytes, and exit status.

For deterministic machine output, use explicit formatting such as
`--color never` or `--json`. JSON with rg flags is newline-delimited rg events,
not the semantic result array. Buffered pipe forwarding is limited to 8 MiB;
text replies are limited to 8 MiB stdout and 1 MiB stderr. Over-limit requests
fail explicitly. Use `rg` directly for unbounded streams or external helpers.

## Exit codes

- `0`: matches found.
- `1`: no matches.
- `2`: error. The message is on stderr.

These apply to both routes.

## Do not

- Do not pass `-n` to a semantic search. `-n` means ripgrep line numbers. Use
  `--top` or `-k` for the semantic count.
- Do not mix the semantic options `--top`, `-k`, or `--model` with ripgrep
  flags. `wg` rejects that combination. Run the two searches as separate
  commands.
- `--explain` is a semantic option and needs `--json`.
- Do not treat a model-ranked result list as an exhaustive grep.

## Lifecycle and recovery

- `wg status` shows attached roots, counts, and health. `wg status --json` is
  machine-readable.
- `wg down` stops the shared daemon for every attached repository.
- If a root is unhealthy, run `wg down` and then `wg up`.
- A query waits for delivered pending work within a fixed bound. This is not a
  transactional filesystem snapshot and does not guarantee instant kernel event
  delivery.
- Exact text/regex checks read the live filesystem. Use that route when you
  need to verify a just-written literal; semantic visibility follows the
  delivered update events.
- Set rg-related environment variables before setup, or restart the daemon
  after changing them. The preview requires UTF-8 arguments and cwd paths.

## Scope

Semantic search covers one whole attached repository. Without a `ROOT`, `wg`
picks the nearest attached ancestor of the current directory. A semantic query
that names a file or a smaller directory fails with an explicit error. The
filesystem route supports file and directory paths normally.
