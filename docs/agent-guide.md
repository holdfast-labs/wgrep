# wg guide for coding agents

`wg` is one binary with two routes:

- **Semantic ranking (default).** `wg QUERY [ROOT]` returns a ranked shortlist
  of code chunks.
- **Exact text and regex.** A ripgrep flag such as `-F`, `-e`, `-n`, `-l`, `-g`,
  or `--files` runs the built-in ripgrep 15.1 engine.

The text route needs no `rg` binary. Semantic search uses the shared daemon. The
text route runs in the `wg` process and can ask the daemon for text-index
candidates when a root is attached. The semantic route needs `wg up`. The text
route does not.

## Choose the route

Use the semantic route when you know the intent but not the exact text:

```sh
wg "where do incoming requests get validated" --top 10
wg "decodePacketV17" --top 5
```

Use the text route when you need exact text, a regex, a complete list, or a
count:

> **Always pass a path.** With no path, ripgrep reads stdin when stdin is a
> pipe. That is rg behavior. Agent and CI stdin is often a pipe, so the command
> can wait for EOF. Pass `.` or another path. On Unix, you can also redirect
> stdin from `/dev/null`.

```sh
wg -F "exact text" .            # literal
wg -e "regex pattern" .         # regex
wg -n -e "pattern" .            # line numbers
wg -l "pattern" .               # file names only
wg -c "pattern" .               # match counts by file
wg -g "*.rs" "pattern" .        # glob filter
wg --files .                    # list searchable files
```

The semantic route is ranked. It can miss a correct answer. Do not use it to
prove that a string is absent. Use `-F` or `-e` for that.

## Set up a root for semantic search

```sh
wg up
```

Without a path, `wg up` uses the nearest `.git` ancestor, or the current
directory. It attaches the root to the shared daemon and returns when search is
ready. A later `wg up` in the same root returns fast and reuses the daemon.

`wg up` checks the path first. A missing path, or a path that is not a
directory, fails with `not a directory` and exit 2. No daemon starts. A file is
not a valid root.

`wg up` is safe under captured output. On Windows, the daemon is detached with
an explicit handle list and inherits only the log file. A piped or CI-run
`wg up` returns and releases its pipes. It does not hang the harness.

On Windows, PowerShell may show the `wg up` progress line in red. It is progress
output, not an error. Use `wg up --json` in scripts.

## Detect readiness

`wg up --json` already returns when the root is ready. To check an existing
root, use `wg status --json`.

A root is ready when all of these are true:

- `ready` is `true`.
- `pending` is `0`.
- `text` is an object, or `text_error` is a string.

`wg up` uses the same rule. A root can be ready with `text_error`, because a
failed text index makes text searches fall back to a full scan.

A build in progress appears in `roots` with `ready: false` and
`building: true`. The `stage`, `files_done`, `files_total`, and `encoded_done`
fields show progress. Do not start a second build. A second `wg up` joins the
same build.

### `wg up --json` shape

```text
{
  "ok": true,
  "pid": int,
  "building": false,
  "root": { <root object, same fields as in wg status> },
  "build_ms": int,
  "model_ms": int,
  "total_ms": int
}
```

`pid` is the daemon PID. It is present in every reply: a new build, a joined
build, and an already attached root.

On Windows, human-readable `wg up` and `wg status` output removes the `\\?\`
prefix from paths. JSON keeps the canonical path, which can keep the prefix.
Compare paths after you normalize them.

### `wg status --json` shape

```text
{
  "ok": true,
  "proto": 1,
  "pid": int,
  "model_loads": int,
  "loaded_models": int,
  "roots": [
    {
      "path": string,
      "ready": boolean,
      "pending": int,
      "generation": int,
      "files": int,
      "chunks": int,
      "error": string|null,
      "text": {
        "files": int,
        "dirty": int,
        "added": int,
        "needs_rescan": boolean,
        "unprunable": boolean,
        "walks": {
          "startup": int, "overflow": int, "outside_dep": int,
          "scoped": int, "pending_retry": int, "rebuild": int
        },
        "ready": boolean,
        "untrusted": boolean,
        "build_ms": int,
        "built": boolean,
        "disk_bytes": int,
        "resident_bytes": int,
        "resident_estimated": boolean,
        "index_path": string,
        "root": string
      } | null,
      "text_error": string|null,
      "text_build_ms": int,
      "build_ms": int,
      "model_ms": int
    }
  ],
  "uptime_ms": int
}
```

A root that is still building has this shape instead. A build in progress has
`building: true`. A finished failed build also appears here, with
`building: false` and `error` set:

```text
{
  "path": string,
  "ready": false,
  "building": boolean,
  "stage": string,
  "stage_code": int,
  "files_done": int,
  "files_total": int,
  "chunks": int,
  "encoded_done": int,
  "encoded_total": int,
  "pending": 0,
  "generation": 0,
  "files": 0,
  "elapsed_ms": int,
  "error": string|null
}
```

## Semantic output

`--json` alone prints a JSON array:

```text
[
  {
    "path": string,
    "start": int,
    "end": int,
    "content": string
  }
]
```

`--json --explain` adds one field to each row:

```text
"scores": {
  "bm25": number,
  "semantic": number,
  "bm25_relative": number,
  "semantic_relative": number,
  "fused": number,
  "exact_identifier": boolean
}
```

`--explain` requires `--json`.

## Text output

With ripgrep flags, `--json` prints ripgrep JSON. It is newline-delimited JSON
events such as `begin`, `match`, `end`, and `summary`. The fields are ripgrep
15.1's fields. Do not parse this as the semantic array.

For stable machine output, use explicit formatting such as `--color never` or
`--json`. Without `--sort path`, ripgrep traversal order is not stable. Use
`--sort path` when you compare output bytes.

The text route runs in the `wg` process. There is no `rg` child and no output
cap in `wg`. The engine is ripgrep 15.1, so its own limits and behavior apply.

### Text index and fallbacks

The index is used for a default directory walk under an attached root. In that
case, `wg` asks the daemon for candidate files, then runs the full ripgrep
engine on them. The index only removes files that cannot match. A flag that
changes the file set, such as `-g`, uses a full scan.

`wg` falls back to a full scan when any of these are true:

- The root is not attached, or the daemon does not answer.
- The invocation is not a default walk. Examples: `--hidden`, `-g`, an explicit
  file, `-v`, `--json`, or a transform.
- The pattern is not supported by the index plan, or `WGREP_NO_INDEX` is set.
- The search path is not a directory, or its spelling is not UTF-8.
- The watcher is untrusted, lost events, or overflowed.
- A walked path cannot be represented in the index.
- The daemon reply is missing, malformed, or exceeds the 8-second bound.

A fallback changes speed, not results. To force a full scan for a diagnostic
run, set `WGREP_NO_INDEX=1`.

## Exit codes

| Code | Meaning |
| ---: | --- |
| 0 | Matches found, or the management command succeeded. |
| 1 | No matches. |
| 2 | Error. The message is on stderr. |

These apply to both search routes. On the semantic route, an empty result array
gives exit 1. On the text route, the exit code is ripgrep's. A bad flag, a
missing daemon, a failed build, or a query against an unhealthy root gives exit
2. Management commands return 0 or 2. They do not use exit 1.

## Freshness and scope

The semantic route waits for delivered pending work within a fixed bound. It
also checks the index generation before it returns. This is not a transactional
filesystem snapshot. It does not guarantee instant kernel event delivery. If
the watcher loses trust, `wg` returns an error instead of a stale answer.

The text route reads the live bytes of each candidate file. Changes become
visible when the OS delivers their watcher event. If the index cannot prove
freshness, `wg` scans the full tree. A full scan is always correct.

Semantic scope is one whole attached root. Without `ROOT`, `wg` picks the
nearest attached ancestor of the current directory. A semantic query that names
a file or a smaller directory fails with an explicit error. The text route
accepts files and directories normally.

## Recovery

- **No daemon:** `wg status` exits 2 and prints
  `wg: no wg service is running for this cache; run wg up first`. Run `wg up`.
- **Unhealthy semantic root:** run `wg down`, then `wg up`.
- **Untrusted text index:** text searches fall back to a full scan.
  `wg status --json` shows `text.untrusted: true`. Run `wg down`, then `wg up` to
  restore index pruning.
- **Failed build:** `wg status --json` shows `error`. The next single `wg up`
  retries the build in the same call. If the new build fails, that call returns
  the new error. Check the cause, then run `wg up` again.
- **Ctrl-C during `wg up`:** the daemon keeps building. Run `wg status --json`
  or `wg up` again to join the same build.
- **Model mismatch:** the model is fixed at `wg up`. A query that names `code`
  or `text` must match it. The error names the attached model and gives the fix:
  run `wg down`, then `wg up --model <model>`.
- **Stop the daemon:** `wg down` is idempotent. With no daemon it exits 0 and
  prints `No wg service is running.` The JSON reply is
  `{"ok":true,"running":false}`.
- **Many roots:** `wg down` stops the daemon for every attached root.

## Do not

- Do not use `-n` to set a semantic count. A bare `-n` selects ripgrep line
  numbers and the text route. Use `--top` or `-k` for the semantic count.
- Do not mix the semantic options `--top`, `-k`, `--model`, or `--explain` with
  ripgrep flags. `wg` rejects that combination. Run the two searches as separate
  commands.
- Do not treat a model-ranked result list as an exhaustive grep.
- Do not expect the same JSON shape from the two routes.
- Do not assume the text index is used. A full-scan fallback is normal and
  correct.
