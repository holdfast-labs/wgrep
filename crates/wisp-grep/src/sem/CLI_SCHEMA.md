# wgrep-sem CLI + daemon contract (for the evaluation harness owner)

Binary: `wgrep-sem` (dedicated binary; no `wgrep`/`rg` flags are shared).
This file is the implementation-owned coordination record. The harness
template's provisional `--query`/`--root` flags are NOT implemented: the
final schema below uses **positional `QUERY [ROOT]`** (ripgrep-style).
Harness must use this schema.

## Build (VPS)

Reproducible locked build (max 2 cargo jobs on the shared host):

```
cargo build --locked --release --features semantic --bin wgrep-sem -j2
```

The embedding/tokenizer/download stack (`half`, `safetensors`,
`tokenizers`, `ureq`, `sha2`) is optional behind the `semantic` feature
(`required-features` on the `wgrep-sem` target): a plain
`cargo build --bin wgrep` never compiles it, and the default dependency
graph is unchanged. `Cargo.lock` carries the resolved semantic closure
(additive: 81→174 packages, no version/checksum changes to existing
entries). Current VPS paths:

- binary: `/srv/projects/wispd/sgrep-integration-20261013/native/build/target/release/wgrep-sem`
- frozen baseline: `native/frozen/wgrep-sem-baseline-<sha>` (copied
  before any rebuild; the semAB2 baseline sha256 is
  `cd2d9d3ef374e95d301df214070d50b9fca6b071260ca7a6781d28776afa6236`)
- source: `/srv/projects/wispd/sgrep-integration-20261013/native/build/src/sem*`
- model cache: `WSEM_CACHE_DIR`, else `~/.cache/wgrep-sem` (holds the two
  pinned assets, ~63 MB total; shared by all roots on the host)
- daemon stderr: `<slot>/daemon-stderr.log` (spawned daemons never inherit
  caller pipes)

Built in the current binary: admit-guard release, buffered whole-frame
socket IO on both ends (120 s whole-frame), `iocapped` unit tests,
stalled-connection regression (`native/wsem-stall.py`, post-freeze runs
only). New binary sha256
`6ceb6645fcf1cd8982af4a6217cf75493fc13e8d5c3fb6f9da27575097539d14`
(same path, in-place rebuild). Old-vs-new A/B parity on the frozen
tree + 1/4/16/32 rebench pending the evaluator's frozen-tree
path+manifest.

## Query

```
wgrep-sem QUERY [ROOT] [-n COUNT] [--json] [--budget-bytes BYTES]
          [--model auto|code|text] [--prefix SUBDIR] [--cache-dir DIR]
          [--no-daemon]
```

| Arg | Default | Meaning |
|---|---|---|
| `QUERY` (positional, required) | — | natural-language intent; error if no searchable words |
| `ROOT` (positional) | `.` | indexed root; must be a directory |
| `-n COUNT` | `5` | results kept, 1..=50 |
| `--json` | text | machine output (single-line JSON array, see below) |
| `--budget-bytes BYTES` | `65536` | **hard cap on summed `content` UTF-8 bytes** (headers/scores not counted); `0` disables; over-cap top passage yields zero results, never an over-budget answer |
| `--model` | `auto` | `auto`/`code` use the pinned code model for corpus and query (single embedding space); `--model text` is an explicit error in this build (code-model only). A future dual-corpus change needs its own gate review — spaces are never mixed or silently remapped |
| `--prefix SUBDIR` | `.` | rank only chunks under this root-relative subpath; matches nothing → error (exit 2), not empty |
| `--cache-dir DIR` | platform cache `wgrep-sem/` | model/tokenizer asset cache (also `WSEM_CACHE_DIR`); index slots live separately (see below) |
| `--no-daemon` | daemon | one-shot: build + rank in-process (slow, correct; no daemon touched) |

Exit codes: `0` results returned, `1` no results (`[]` with `--json`,
empty output as text), `2` errors. Broken pipe → `0`.
`--budget-bytes` trimming to zero results reports exit `1`.

## Daemon management

```
wgrep-sem --serve ROOT [--cache-dir DIR]   # foreground serve (detached by client, not by hand)
wgrep-sem --stop [ROOT]                    # shutdown daemon for ROOT (default ".")
wgrep-sem --status [ROOT]                  # print stats JSON, or "no daemon is serving ..."
```

First query auto-starts the daemon (spawn `--serve`, poll `ping` up to
900 s — includes model download + full corpus build) and reuses it after.
Concurrent cold starters serialize on the slot's `startup.lock` — an
OS-held advisory lock (`flock`/`LockFileEx`), held for the whole
spawn-and-wait window, auto-released by the kernel on process death.
There is no takeover heuristic and the file is never unlinked, so a
slow-but-live starter can never be duplicated and no stale-lock state
exists: 1/4/16/32 simultaneous cold clients converge on ONE pid
(harness asserts this). The daemon itself holds a second guard,
`serve.lock`, for its whole lifetime: a duplicate `--serve` for the same
root is refused outright (exit 2), even when started by hand. The two
guards are separate files, so the polling parent and the serving child
cannot deadlock. `--stop` targets ONLY the daemon whose `daemon.json`
root matches; the daemon deletes its own metadata before confirming
shutdown (single-writer protocol; the client's same-pid removal is only
a backup for SIGKILLed daemons). Shared trigram `wgrep` daemons are
never touched (separate slot namespace). `--stop`/`--status` resolve
the slot without requiring the root to still exist, so a deleted repo's
daemon is still cleanable.

## Readiness protocol (what the harness must do)

1. Resolve `ROOT` the same way (absolute, no symlink resolution).
2. Run query or `--status`; if no daemon answers, the client spawns
   `--serve` itself — the harness does not need to pre-start anything,
   but SHOULD record cold-start latency separately (first start includes
   ~63 MB model download + full parallel embed build).
3. Poll `--status` until `"ok": true`, or run one warmup query and
   discard its timing. Steady-state readings start after that.
4. `WSEM_OFFLINE=1` forbids model downloads (missing asset → exit 2).

## Status / PID fields

`--status` prints one JSON line (all present when `"ok": true`):

```
{"ok":true,"proto":1,"pid":PID,"files":N,"chunks":M,
 "skipped_large":L,"skipped_binary":B,"generation":G,"queries":Q,
 "rejected_busy":RB,"rejected_admit":RA,
 "uptime_s":S,"workers":W,"queue_cap":128,
 "admit_threads":4,"conn_queue_cap":64,
 "model":"minishlab/potion-code-16M-v2@e9d2a44ca6a0"}
```

- `pid`: daemon process id (harness: assert single PID across 1/4/16/32
  clients to prove one shared resident store).
- `workers`: `min(available_parallelism, 8)` on the daemon host, logged at
  startup; the single configured compute budget (no nested pools;
  embedding inside a rank job is sequential).
- `generation`: increments on every applied change batch; results carry
  the generation they ranked from — harness can detect races.
- `queries`: served rank count; `rejected_busy` (rank queue full) and
  `rejected_admit` (connection queue full) are explicit, countable
  backpressure — never silent drops.
- Served passages are re-validated (fingerprint + exact content lines)
  before reply; on mismatch the daemon reconciles and re-ranks once,
  then drops anything still stale.
- No persistent vector store in this build: daemon start rebuilds the
  corpus (parallel chunk + bounded-batch embed); the harness must record
  cold-start (download + build) vs warm timings separately.

Rank reply object: `{"ok":true,"proto":1,"model":"...","dim":256,
"files":N,"chunks":M,"generation":G,"results":[...]}`.

## Discovery + content contract

File discovery uses ripgrep/wgrep default walk settings (hidden skipped;
`parents`, `ignore`, `git_global`, `git_ignore`, `git_exclude` on;
`require_git` on; symlinks not followed; `.rgignore` honored) — the same
file set plain `rg` would search. Explicit `--hidden`/`--no-ignore`
overrides are NOT supported (fixed default set). Candidates are NEVER
constrained to literal trigram hits: every indexed chunk is rankable.

Content is exact UTF-8 source bytes (CRLF retained, split on `'\n'`
only); binary (NUL) and non-UTF-8 files are skipped explicitly, never
served lossy. `path`/`start`/`end` index the source's own lines.

Eligibility (ignore rules) is eventual: a file that leaves discovery
(deleted or newly ignored) is dropped by the next reconcile's
re-discovery diff and never served after that. In-root rule edits apply
within watcher debounce + reconcile; ancestor/global/exclude rules
(outside the root, unwatched) apply at the 60 s periodic reconcile.
Before-response validation checks fingerprint + content only, so a
freshly-ignored file may be served until its reconcile lands — a bounded
lag, not silent permanence.

## Result schema (`--json`)

Array of, in rank order:

```
{"path":"src/f.rs","start":12,"end":40,"content":"...\n",
 "scores":{"bm25":1.23,"semantic":0.87,"bm25_relative":1.0,
           "semantic_relative":0.93,"fused":0.9475,
           "exact_identifier":true}}
```

- `path`: `/`-separated, root-relative. `start`/`end`: 1-based line
  numbers; `content` is the exact source bytes for those lines (CRLF
  retained where the source has it — never LF-normalized).
- `scores` are relative to the candidate set (min-max scaled, fused
  `0.75*semantic + 0.25*bm25`, exact identifiers first) — ranking
  signals, not confidences.
- Text mode prints `path:start-end`, blank line, content per result.

## Fidelity hook (test-only, no CLI)

`eval/bench/WGREP_STATIC_FIDELITY.py --verify --rust <rust.json>`
compares Rust embeddings against independent goldens (L2 ≤ 1e-5, token
ids exact). The export is a `#[ignore]`d unit test in `sem/model.rs`
(`fidelity_export`): it embeds the fixture's `cases[].text` in order via
the real `ModelCtx` and writes `{"vectors","token_ids"}`. Normal
`cargo test` never runs it (no assets/network/env needed). Run:

```
WSEM_CACHE_DIR=<verified cache> WSEM_OFFLINE=1
WGREP_FIDELITY_FIXTURE=<WGREP_STATIC_FIDELITY_FIXTURE.json>
WGREP_FIDELITY_OUT=<rust.json>
cargo test --locked --release --features semantic --bin wgrep-sem \
  -- --ignored sem::model::tests::fidelity_export
```

Receipt: `rust_max_l2=3.03e-07`, 11/11 ids exact, harness PASS.

## Baseline receipts (semAB2)

First real baseline (four dev descriptive queries, warm): worst-query
p95 **sgrep 631.4 ms / native 74.4 ms**; recall@10 **.25** and MRR
**.125** on both arms (hybrid equally low — quality work, not tuning,
is the follow-up; held-out stays gated). Weighted table + 32-run owned
by the benchmark agent. `native/evidence/` holds the smoke/fidelity/
parity receipts from the impl side.

## Slots, env, errors, bounds

- Slots: `<WSEM_DIR || platform-cache/wgrep/sem>/<fnv1a64>/` with `root`
  guard + `daemon.json` (`port`,`token`,`pid`,`root`,`proto`), plus
  `startup.lock` / `serve.lock` guard files (never unlinked) and
  `daemon-stderr.log`.
- Env: `WSEM_DIR` (slots), `WSEM_CACHE_DIR` (+ `--cache-dir` wins),
  `WSEM_OFFLINE=1` (no downloads).
- Socket bounds: every request/reply frame is `\n`-terminated, capped
  (64 KiB request, 256 MiB reply), and bounded WHOLE-frame by 120 s
  (recomputed per fill, so trickles cannot stretch a frame); writes
  bounded by 120 s (daemon) / 30 s (client request write). (The frozen
  baseline `cd2d9d3e` still enforces per-`recv` 120 s (daemon) / 150 s
  (client read) — the A/B parity run covers exactly this delta.)
- Daemon errors surface as `{"ok":false,"reason":"..."}` → client exit 2
  with `wgrep-sem: daemon error: ...`. Producer/consumer statuses are
  never merged: there is no pipe stage in this binary.
