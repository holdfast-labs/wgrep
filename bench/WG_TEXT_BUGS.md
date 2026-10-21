# wg Phase B2 text-route bug reports

Final run. Source tree sha256
`7808e65148e1d2e05a4ddadc2444f11b86eca4729968559da7c26e8e1d52e0f2`,
`wg` sha256
`b4e5655aa7aa8b12b2ebfd2424b9a0a9fcb35b573a8520cca496b69b54233817`.

## Correctness

No mismatch, stale result or wrong exit code occurred in this run:

- daemon parity: 1,822 applicable, 0 mismatched, 0 stale, one event each;
- freshness: 26 checks, 0 stale, 0 mismatched;
- speed: 0 errors in 300 single queries and 512 concurrent queries.

## B2-FD-1: a Unix daemon inherits a caller descriptor >= 3

Severity: high. This is the Unix form of the Windows pipe hang that B1 fixed.
`wg up` exits, but the daemon keeps the caller's pipe write end open, so a
caller that waits for EOF can wait forever.

### Reproduction

Run on Linux:

```python
import json, os, select, subprocess, time
from pathlib import Path

wg = "/srv/projects/wispd/wg-b2-20261021-final/target/release/wg"
work = Path("/tmp/wg-fd-repro")
tree = work / "tree"
tree.mkdir(parents=True, exist_ok=True)
(tree / "a.txt").write_text("needle\n")
os.environ["WG_SERVICE_DIR"] = str(work / "svc")
os.environ["WGREP_INDEX_DIR"] = str(work / "idx")

r, w = os.pipe()
os.set_inheritable(w, True)
proc = subprocess.Popen(
    [wg, "up", str(tree), "--model", "text", "--json"],
    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    pass_fds=(w,),
)
out, err = proc.communicate(timeout=120)      # wg up exits
pid = json.loads((work / "svc" / "service.json").read_text())["pid"]
os.close(w)
ready, _, _ = select.select([r], [], [], 5)   # <- fails: no EOF
print("eof", bool(ready) and os.read(r, 4096) == b"")
print(open(f"/proc/{pid}/fd/4").read())       # pipe:[...]
subprocess.run([wg, "down"], check=False)
```

### Expected

The read end reaches EOF within 5 s while the daemon is still running.

### Actual

`wg up` exits 0 and the daemon is alive, but the read end has no EOF. The
daemon holds the pipe:

```text
lr-x------ 0 -> /dev/null
l-wx------ 1 -> .../svc/daemon.log
l-wx------ 2 -> .../svc/daemon.log
lrwx------ 3 -> .../svc/service.lock
l-wx------ 4 -> pipe:[4140112]
```

Plain captured stdout/stderr does reach EOF, so the standard handles are
correct. Only an inherited descriptor >= 3 leaks.

### Cause

`crates/wisp-grep/src/detached.rs` (Unix branch) redirects stdin, stdout and
stderr, but Rust's `Command` does not close an arbitrary inherited descriptor
that the caller marked inheritable. The Windows branch uses an explicit handle
list; the Unix branch has no equivalent.

### Suggested direction

In a `pre_exec` hook, close every descriptor >= 3 (`close_range(3, ~0U, 0)` or
iterate `/proc/self/fd`) before `exec`.

## B2-UP-1: a large `wg up` hits the frame deadline and is OOM-killed

Severity: high for large repositories. The client reports failure at 90 s, and
the daemon later dies, so the root never attaches.

### Reproduction

Corpus: `/srv/workspaces/wispd/corpora/linux-7.2.9` (94,862 files, 1.62 GB),
Debian 13, 4 vCPU, 7.8 GiB RAM, no swap.

```sh
cd /srv/projects/wispd/wg-b2-20261021-final
python3 bench/WG_TEXT_LARGE_UP.py --run \
  --wg target/release/wg \
  --work /srv/workspaces/wispd/wg-b2-final/large-up \
  --timeout 300 --ready-timeout 1800
```

Or directly:

```sh
export WG_SERVICE_DIR=/tmp/wg-linux/svc
export WGREP_INDEX_DIR=/tmp/wg-linux/idx
target/release/wg up /srv/workspaces/wispd/corpora/linux-7.2.9 \
  --model code --json
```

### Expected

`wg up` succeeds or reports a clear, recoverable timeout; the daemon finishes
the build and the root becomes ready.

### Actual

`wg up` exits 2 after 90.1 s:

```text
wg: service frame deadline exceeded
```

The text index is written (236 MB on disk) before the semantic phase. The
daemon then grows to 7,047.9 MiB RSS (PSS 7,047.8 MiB) and is OOM-killed at
about 14 minutes:

```text
Out of memory: Killed process 356500 (wg) total-vm:8161748kB,
anon-rss:7270048kB, file-rss:1056kB, shmem-rss:0kB, UID:1000
```

`wg status` never reports the root, so text queries fall back to a full scan.
The 90 s client timeout is a known open issue in the handoff; the OOM kill is
new evidence and makes the run unusable on this machine.

The same harness on the 13.3k-file Rust corpus succeeds in 22.5 s with a peak
of 681.7 MiB.

## B2-OBS-1: extra search event hid the fallback reason (fixed)

The final tree sets `events::set_external(true)` in `run_rg_text`, so
`record_walk` adds no second event. The final run observed exactly one
`search` event per invocation in 1,822 parity cases, 26 freshness checks and
five speed parity checks. No action needed.
