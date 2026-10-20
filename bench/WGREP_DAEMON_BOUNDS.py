#!/usr/bin/env python3
"""Live bounds check for the lexical daemon (P0-2).

Starts a real `wgrep --index-serve` daemon and proves the connection/handler
cap and the request/reply bounds with a real process:

  1. baseline: a normal query is served by the daemon and matches rg;
  2. stalled sockets: about 256 clients that connect and never send keep the
     daemon's thread count bounded (handlers are capped and the whole-read
     deadline releases a stalled handler);
  3. recovery: after the stalled clients close, a normal query is served again;
  4. held leases: more broad searches than the cap hold their handlers, so a
     further query is declined and the CLI walks authoritatively, still
     matching rg.

The daemon records each search's outcome in `WGREP_INDEX_EVENTS` (one NDJSON
line: `used:daemon` or `used:walk` + reason). The thread/memory sampling is
Linux-only (`ps -o nlwp=`/`ps -o rss=`).

Not a production test knob: the harness only relies on the real cap by opening
more connections than the documented `MAX_HANDLERS`.
"""
from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

DNULL = subprocess.DEVNULL


def root_key(root: Path) -> str:
    """Forward slashes, no trailing slash, lowercased on Windows (store::root_key)."""
    key = str(root).replace("\\", "/")
    while key.endswith("/"):
        key = key[:-1]
    if os.name == "nt":
        key = key.lower()
    return key


def slot_dir(store: Path, root: Path) -> Path:
    """FNV-1a 64 of the root key (store::slot)."""
    h = 0xCBF29CE484222325
    for b in root_key(root).encode():
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return store / f"{h:016x}"


def read_info(store: Path, root: Path):
    info = slot_dir(store, root) / "daemon.json"
    for _ in range(200):
        if info.is_file():
            try:
                return json.loads(info.read_text())
            except json.JSONDecodeError:
                pass
        time.sleep(0.05)
    raise SystemExit(f"daemon.json never appeared: {info}")


def proc_stats(pid: int):
    """(threads, rss_kb) of `pid`, or None when it is gone."""
    try:
        threads = subprocess.run(["ps", "-o", "nlwp=", "-p", str(pid)], capture_output=True, text=True)
        rss = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True)
    except FileNotFoundError:
        return None
    if threads.returncode != 0 or not threads.stdout.strip():
        return None
    return int(threads.stdout.strip()), int(rss.stdout.strip() or 0)


def run_cli(wgrep, store, pattern, corpus, events):
    """One CLI search with events recorded; returns (rc, stdout, events_text)."""
    ev = Path(events)
    if ev.exists():
        ev.unlink()
    env = dict(os.environ, WGREP_INDEX_DIR=str(store), WGREP_INDEX_EVENTS=str(ev))
    env.pop("WGREP_NO_DAEMON", None)
    env.pop("WGREP_NO_INDEX", None)
    proc = subprocess.run(
        [wgrep, "--no-messages", pattern, str(corpus)],
        env=env, stdin=DNULL, capture_output=True, text=True,
    )
    return proc.returncode, proc.stdout, (ev.read_text() if ev.exists() else "")


def run_rg(rg, pattern, corpus):
    proc = subprocess.run(
        [rg, "--no-messages", pattern, str(corpus)],
        stdin=DNULL, capture_output=True, text=True,
    )
    return proc.returncode, proc.stdout


def index_stats(wgrep, store, corpus):
    env = dict(os.environ, WGREP_INDEX_DIR=str(store))
    proc = subprocess.run(
        [wgrep, "--index-stats", str(corpus)],
        env=env, stdin=DNULL, capture_output=True, text=True,
    )
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout)["stats"]
    except (json.JSONDecodeError, KeyError):
        return None


def event_used(events_text: str):
    """The set of `used` values across this search's events."""
    used = []
    for line in events_text.splitlines():
        try:
            used.append(json.loads(line).get("used"))
        except json.JSONDecodeError:
            pass
    return used


def same_output(a: str, b: str) -> bool:
    return sorted(a.splitlines()) == sorted(b.splitlines())


def open_stub(info, request: bytes, drain_paths: bool):
    """Connects and sends `request`; reads the header, and (if asked) the body.

    Returns (sock, header, holders). A held lease keeps the socket open: the
    daemon blocks reading one byte, holding the thread share.
    """
    s = socket.create_connection(("127.0.0.1", info["port"]), timeout=10)
    try:
        s.sendall(request)
        f = s.makefile("rb")
        line = f.readline()
        header = json.loads(line) if line else {}
    except (OSError, json.JSONDecodeError):
        s.close()
        return None, {}, False
    if not header.get("ok"):
        s.close()
        return None, header, False
    n = int(header.get("candidates", 0))
    if drain_paths:
        for _ in range(n):
            if not f.readline():
                break
    return s, header, True


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wgrep", required=True)
    ap.add_argument("--rg", default="rg")
    ap.add_argument("--index", required=True, help="WGREP_INDEX_DIR for wgrep")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--pattern", default="fn", help="a selective pattern whose files are compared to rg")
    ap.add_argument("--stalled", type=int, default=256)
    ap.add_argument("--leases", type=int, default=70, help="held broad searches; must exceed MAX_HANDLERS (64)")
    ap.add_argument("--thread-bound", type=int, default=96, help="max resident threads tolerated")
    args = ap.parse_args()

    wgrep = str(Path(args.wgrep).resolve())
    store = Path(args.index).resolve()
    corpus = Path(args.corpus).resolve()
    work = Path(os.environ.get("WGREP_BOUNDS_WORK", "/tmp/wgrep-bounds")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    events = work / "events.ndjson"

    failures = []

    def check(name, ok, detail=""):
        print(f"[{'ok' if ok else 'FAIL'}] {name}" + (f" :: {detail}" if detail else ""))
        if not ok:
            failures.append(name)

    # Clean start.
    subprocess.run([wgrep, "--stop", str(corpus)],
                   env=dict(os.environ, WGREP_INDEX_DIR=str(store)), capture_output=True)
    start = subprocess.run([wgrep, "--start", str(corpus)],
                           env=dict(os.environ, WGREP_INDEX_DIR=str(store)),
                           capture_output=True, text=True, stdin=DNULL)
    if start.returncode != 0:
        print(start.stdout)
        print(start.stderr, file=sys.stderr)
        raise SystemExit("daemon did not start")
    info = read_info(store, corpus)
    pid = int(info["pid"])
    baseline = proc_stats(pid)
    print(f"daemon pid={pid} port={info['port']} baseline threads/rss={baseline}")

    rg_rc, rg_out = run_rg(args.rg, args.pattern, corpus)
    check("rg reference ran", rg_rc in (0, 1), f"rc={rg_rc}, files={len(rg_out.split())}")

    # Phase 1: baseline daemon query.
    rc, out, ev = run_cli(wgrep, store, args.pattern, corpus, events)
    check("baseline query exits 0/1", rc in (0, 1), f"rc={rc}")
    check("baseline used the daemon", "daemon" in event_used(ev), f"used={event_used(ev)}")
    check("baseline files match rg", same_output(out, rg_out),
          f"wgrep={len(out.splitlines())} rg={len(rg_out.splitlines())}")

    # Phase 2: stalled sockets must not spawn unbounded threads.
    stalled = []
    for _ in range(args.stalled):
        try:
            s = socket.create_connection(("127.0.0.1", info["port"]), timeout=5)
            stalled.append(s)
        except OSError:
            break
    time.sleep(1.0)
    peak = proc_stats(pid)
    check("stalled clients kept threads bounded",
          peak is not None and peak[0] <= args.thread_bound,
          f"opened={len(stalled)} peak={peak} bound={args.thread_bound}")
    for s in stalled:
        s.close()
    time.sleep(6.5)  # let the whole-read deadline (5 s) release every handler

    # Phase 3: recovery after release.
    recovered = proc_stats(pid)
    check("threads recovered after release",
          recovered is not None and recovered[0] <= max(16, baseline[0] + 8),
          f"recovered={recovered} baseline={baseline}")
    rc, out, ev = run_cli(wgrep, store, args.pattern, corpus, events)
    check("recovery query used the daemon", "daemon" in event_used(ev), f"used={event_used(ev)}")
    check("recovery files match rg", same_output(out, rg_out))

    # Phase 4: held broad leases exhaust the cap, so a further query is
    # declined and the CLI walks authoritatively.
    request = (json.dumps({"token": info["token"], "query": "*", "prefix": ""}) + "\n").encode()
    held = []
    holders = 0
    for _ in range(args.leases):
        sock, header, ok = open_stub(info, request, drain_paths=True)
        if ok:
            held.append(sock)
            holders += 1
    check("lease count reaches the cap", holders >= 64, f"holders={holders} of {args.leases}")
    held_threads = proc_stats(pid)
    check("held leases kept threads bounded",
          held_threads is not None and held_threads[0] <= args.thread_bound,
          f"held threads={held_threads} bound={args.thread_bound}")
    rc, out, ev = run_cli(wgrep, store, args.pattern, corpus, events)
    used = event_used(ev)
    check("overloaded CLI falls back to the walk", "walk" in used and "daemon" not in used, f"used={used}")
    check("overloaded CLI result matches rg", same_output(out, rg_out),
          f"wgrep={len(out.splitlines())} rg={len(rg_out.splitlines())}")

    for s in held:
        s.close()
    time.sleep(1.0)
    rc, out, ev = run_cli(wgrep, store, args.pattern, corpus, events)
    check("query served again after leases close", "daemon" in event_used(ev), f"used={event_used(ev)}")

    stats = index_stats(wgrep, store, corpus)
    if stats is None:
        check("daemon reports handler stats", False, "no stats reply")
    else:
        handlers = stats.get("handlers", {})
        check("handler cap is reported", handlers.get("max") == 64, f"handlers={handlers}")
        check("overloads were counted", handlers.get("rejected", 0) >= 1, f"handlers={handlers}")

    subprocess.run([wgrep, "--stop", str(corpus)],
                   env=dict(os.environ, WGREP_INDEX_DIR=str(store)), capture_output=True)

    print(f"\n{len(failures)} failure(s)")
    for name in failures:
        print(f"  - {name}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
