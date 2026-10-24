# wg release validation protocol

Status: run it for each release.
See `bench/WGREP_WG_RELEASE.py`,
`bench/WGREP_WG_RELEASE_QUERIES.json` and
`bench/WGREP_WG_RELEASE_RUN.sh`.

## What this validates

The frozen release contract for the unified `wg` front door:

- `wg QUERY [ROOT] --rank classic --json --explain --model code` is the
  sgrep-compatible hybrid semantic search. The default ranker adds boosts and
  is not part of this contract. It must match a fresh pinned sgrep index exactly:
  same ordered rows, same line ranges, same content, and the same finite
  numeric scores (no float tolerance).
- Semantic result count is `--top N` / `-k N`. The pinned sgrep control keeps
  `-n N`. `-n` stays reserved for rg line numbers.
- Familiar rg flags (`-F -e -n -l -g --files ...`) select precise filesystem
  text/regex behavior. The request must preserve argv, caller cwd, stdout
  bytes, stderr bytes and exit status for supported requests.
- `wg up ROOT --model code`, `wg status --json` and `wg down` are the lifecycle
  surface. One daemon PID and one loaded model serve several attached roots.
  `WG_SERVICE_DIR` isolates service state.

The harness does not edit or embed the Rust CLI. It compares an existing
candidate binary with the pinned controls, so it starts from a compiled `wg`.

## Pinned controls

- sgrep: pinned control binary, revision
  `0fbe8e5725f71667789be4a3abcea4e02f33afa4`, SHA-256
  `715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217`.
- rg: `rg` 15.1.0 binary.
- model: `minishlab/potion-code-16M-v2`, revision
  `e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b`, `--model code`.

The harness fails at preflight if a control hash or version differs.

## Phases

`--phase` selects phases (repeatable); the default is all five.

1. `lifecycle` — concurrent `wg up`, repeated `up`, and a second root. Requires
   one PID, two roots, and exactly one loaded model. Raw `up --json` is saved.
2. `isolation` — each root returns only its own files; a query that omits ROOT
   from a subdirectory selects the closest attached root.
3. `semantic` — replays the mutation sequence in the query-set file: baseline,
   same-length edit, create, rename, delete, restored-mtime same-length rewrite,
   and an ignore-file edit. For each step it measures change-to-first-correct
   response, then compares exact ordered explained JSON with a fresh pinned
   sgrep run. It also runs the `--top` / `-k` count contract.
4. `text` — the text matrix: literal, regex, line numbers (`-n`), `-l`,
   `--files`, `-g` glob, default ignore, `--no-ignore`, scoped path, `--json`,
   `--null-data` (NUL), a binary file, and two error cells. Each cell runs the
   same argv, cwd and env against `wg` and the authoritative rg and compares
   stdout bytes, stderr bytes and exit status exactly, except that the JSON
   cell compares ordered parsed records after removing only rg's documented
   volatile elapsed-time fields. Both original byte streams are retained.
5. `bench` — bounded warm benchmarks for semantic and for text, plus an
   edit-visibility distribution. Semantic and text are separate workloads.
   No cross-workload "speedup" is computed. An optional same-workload
   `--bench-control` ratio (candidate p50 over pinned sgrep p50) is labeled.

The text phase also runs six bare-flag/controlled-pipe cases without injected
sort/color flags. These check route selection, implicit-cwd filesystem search
with DEVNULL, and raw literal/JSON pipe forwarding on a one-file root.

`sixlang` is an optional phase: pass `--sixlang-fixture` pointing at the pinned
six-language fixture to add exact semantic parity over its queries. It needs the
tree-sitter parser caches on the host.

The generated roots are `git init`-ed so rg applies `.gitignore` (rg requires a
repository unless `--no-require-git`). Without a `.git` marker the ignore cells
would silently stop testing ignore behavior. `git` must be on PATH.

## Timing method

Change-to-visible latency starts when the filesystem write returns and stops
at the first CLI response that satisfies the
step predicate. The slow fresh-sgrep baseline runs afterwards, outside that
interval. Every attempt is saved, including wrong, not-ready and timed-out
attempts, with its exit and latency. Timeouts are never discarded. A one-sided
timeout is a mismatch.

`--settle-ms` defaults to zero. If set, that deliberate wait is included in
the measured interval. Semantic deletion checks for disappearance of the old
path/content, not an empty result list: ranked semantic search still returns
other passages for an absent symbol. Cleanup has a separate bounded budget
even if the overall run deadline has expired.

Warm-cell distributions report count, min, max, mean, p50, p95 and p99, plus an
error count. `--samples` and `--visibility-samples` default to 120 and 100 so a
p95 cell has at least 100 samples. `--bench-root` points the warm benchmarks at
a read-only larger corpus such as the Rust `src` tree; the mutation and
visibility fixtures always stay in the generated scratch tree.

For realistic edit timing, `--visibility-corpus PATH` copies only that corpus's
rg-visible regular files to a new scratch root, adds one new probe file, and
indexes the copy. The benchmark edits only the probe in that copy. It records
the copied file count and actual index/chunk counts. The source corpus remains
read-only. With no option, edit timing uses the small generated fixture.

Larger-corpus cells use the separate `corpus_semantic` and `corpus_text` query
sets. They include natural-language Rust tasks and absent/selective/broad/regex
file-list searches. A failed request fails the run; its error latency cannot be
published as successful query latency. The published distribution is a declared
mixture over the listed queries, not a per-query percentile from 120 samples.

## Evidence and identity rules

- All comparisons are exact, including finite JSON numbers parsed with
  NaN/Infinity rejected. There is no score tolerance.
- Raw stdout, stderr, argv, cwd, exit and elapsed time are saved for every
  command under `<work>/raw`. The service events file, when the daemon exposes
  one through `--events-env` (`WG_SERVICE_EVENTS` by default), is copied after
  each mutation step.
- Recorded identities: candidate `wg` path/SHA-256/source revision/version,
  sgrep control path/SHA-256/revision, rg path/SHA-256/version, model choice
  and revision, and the effective config (store env, sort, color, top, extra
  args).
- Every ordinary subprocess uses `stdin=DEVNULL`, a per-command timeout and an
  overall `--deadline`. The two explicit pipe-parity cases use a small recorded
  byte string on stdin for both tools; their payloads are retained as `.stdin`.
- Cleanup runs in `finally`: `wg down` on the task-owned `WG_SERVICE_DIR`, then
  a status check that the daemon is gone. A failed cleanup can never yield PASS.
- Only the generated scratch fixture is mutated. A `--bench-root` corpus is
  read-only. Do not point `--work` at a source checkout.

## Honesty boundaries

- rg-shaped requests may use an authoritative rg subprocess inside the daemon
  for this first version. Shared semantic indexing does not imply accelerated
  regex scans. A text-matrix mismatch is a real finding, not an approximation.
- The semantic comparison proves candidate/control equality on settled small
  fixtures. It is not a large-corpus quality claim.
- Setup measurements are not search latency. Warm and visibility numbers come
  only from the saved raw records of a real run.

## How to run

Self-check (no service binary needed):

```sh
python bench/WGREP_WG_RELEASE.py --self-test
python bench/WGREP_WG_RELEASE.py --verify-queries
python -m py_compile bench/WGREP_WG_RELEASE.py
```

Full run against a compiled `wg`:

```sh
candidate=/path/to/wg
work=$(mktemp -d)
python3 bench/WGREP_WG_RELEASE.py --run \
  --wg "$candidate" --wg-sha256 "$(sha256sum "$candidate" | cut -d' ' -f1)" \
  --wg-source-revision "<git-rev-or-tree-hash>" \
  --work "$work" --samples 120 --visibility-samples 100
```

Optional six-language parity and a read-only large-corpus timing arm:

```sh
python3 bench/WGREP_WG_RELEASE.py --run ... \
  --sixlang-fixture bench/WGREP_SGREP_PARITY_FIXTURE \
  --bench-root /path/to/rust/src
```

Keep generated output outside the repository.

## Run status

Run the self-check before each release. Run the full protocol against the
candidate binary for that release. The harness must not run against a stale
CLI.
