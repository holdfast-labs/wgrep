#!/usr/bin/env python3
"""Three-way indexed-grep speed benchmark: rg vs wgrep vs Microsoft tgrep.

wgrep and tgrep are both trigram-indexed, ripgrep-compatible greps, so the
interesting comparison is not "wgrep vs rg" (index vs no index) but
"wgrep vs tgrep" (two indexes, same class of query). This harness runs a
fixed, ripgrep-compatible query mix against all three on the same corpora,
on the same machine, in the same session, so the columns are directly
comparable.

Two modes:

  oneshot  every invocation is a fresh process. rg walks with no index;
           wgrep uses its on-disk index (WGREP_NO_DAEMON=1) but still walks;
           tgrep uses its local `.tgrep` index. This isolates the index
           lookup/verification cost, not the daemon.

  daemon   rg is unchanged; wgrep runs its resident daemon (default mode);
           tgrep runs `tgrep serve` and clients connect over loopback. This
           is the designed steady-state mode for both indexed tools.

Output is written to NUL/DEVNULL (never measured through a pipe), warm cache,
median of --reps timed runs after an untimed warm-up. Prints a markdown table
and, with --out, a JSON report with the exact argv/env used.
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

DNULL = subprocess.DEVNULL

# Query mix mirrors the one-shot speed tables in WGREP.md: one absent literal
# (every file pruned), a word-boundaried phrase, two selective identifiers, a
# case-insensitive alternation, a broad word, and a broad regex. All are
# ripgrep-compatible; tgrep accepts the same flags.
QUERY_MIX = {
    "linux": [
        ("absent literal", ["zzqxjwvbnm"]),
        ("-w unsafe fn", ["-w", "unsafe fn"]),
        ("kmalloc", ["kmalloc"]),
        ("EXPORT_SYMBOL_GPL", ["EXPORT_SYMBOL_GPL"]),
        ("-i 'TODO|FIXME'", ["-i", "TODO|FIXME"]),
        ("-w static", ["-w", "static"]),
        ("-i the", ["-i", "the"]),
        (r"\w+\(", [r"\w+\("]),
    ],
    "rust": [
        ("absent literal", ["zzqxjwvbnm"]),
        ("EXPORT_SYMBOL_GPL", ["EXPORT_SYMBOL_GPL"]),
        ("kmalloc", ["kmalloc"]),
        ("-w unsafe fn", ["-w", "unsafe fn"]),
        ("-i 'TODO|FIXME'", ["-i", "TODO|FIXME"]),
        ("-w static", ["-w", "static"]),
        ("-i the", ["-i", "the"]),
        (r"\w+\(", [r"\w+\("]),
    ],
}


MAX_CALL_SECONDS = 180.0


def run_query(exe: str, query_flags: list[str], pattern: str, corpus: Path, env: dict) -> float | None:
    """One timed invocation; output to the null device. Returns elapsed ms, or
    None if it exceeded MAX_CALL_SECONDS (the process is killed)."""
    cmd = [exe, "--no-messages", *query_flags, "--", pattern, str(corpus)]
    t = time.perf_counter()
    try:
        subprocess.run(cmd, env=env, stdin=DNULL, stdout=DNULL, stderr=DNULL,
                       timeout=MAX_CALL_SECONDS)
    except subprocess.TimeoutExpired:
        return None
    return (time.perf_counter() - t) * 1000.0


def warm(exe: str, queries, corpus: Path, env: dict) -> None:
    for _, flags in queries:
        run_query(exe, flags[:-1], flags[-1], corpus, env)


def measure(exe: str, queries, corpus: Path, env: dict, reps: int) -> list[dict]:
    rows = []
    for label, flags in queries:
        pattern = flags[-1]
        samples = [run_query(exe, flags[:-1], pattern, corpus, env) for _ in range(reps)]
        ok = [s for s in samples if s is not None]
        rows.append({
            "query": label,
            "median_ms": statistics.median(ok) if ok else None,
            "min_ms": min(ok) if ok else None,
            "max_ms": max(ok) if ok else None,
            "timeouts": sum(1 for s in samples if s is None),
            "samples": [s for s in samples],
        })
    return rows


def tgrep_start_server(tgrep: str, corpus: Path, ready_seconds: float = 180.0,
                       max_seconds: float = 900.0) -> subprocess.Popen:
    proc = subprocess.Popen(
        [tgrep, "serve", str(corpus)],
        stdin=DNULL, stdout=DNULL, stderr=DNULL,
    )

    def watchdog():
        deadline = time.time() + max_seconds
        while time.time() < deadline:
            if proc.poll() is not None:
                return
            time.sleep(1.0)
        proc.kill()

    threading.Thread(target=watchdog, daemon=True).start()
    deadline = time.time() + ready_seconds
    while time.time() < deadline:
        try:
            out = subprocess.run(
                [tgrep, "status", str(corpus)],
                stdin=DNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                timeout=10,
            ).stdout.decode("utf-8", "ignore")
        except subprocess.TimeoutExpired:
            out = ""
        if "Server status for" in out and "Indexing:" in out and "complete" in out:
            return proc
        if proc.poll() is not None:
            raise RuntimeError("tgrep serve exited early")
        time.sleep(0.5)
    proc.kill()
    raise RuntimeError("tgrep server did not become ready")


def wgrep_daemon_start(wgrep: str, corpus: Path, env: dict) -> None:
    subprocess.run([wgrep, "--start", str(corpus)], env=env, stdin=DNULL,
                   stdout=DNULL, stderr=DNULL, timeout=600)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rg", default="rg")
    ap.add_argument("--wgrep", required=True)
    ap.add_argument("--tgrep", required=True)
    ap.add_argument("--corpus", required=True, type=Path)
    ap.add_argument("--corpus-kind", required=True, choices=sorted(QUERY_MIX))
    ap.add_argument("--wgrep-index", required=True, type=Path, help="WGREP_INDEX_DIR")
    ap.add_argument("--mode", default="oneshot", choices=["oneshot", "daemon", "both"])
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--max-call-seconds", type=float, default=180.0,
                    help="hard per-invocation cap; a slower query is killed and recorded as a timeout")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    global MAX_CALL_SECONDS
    MAX_CALL_SECONDS = args.max_call_seconds

    corpus = args.corpus.resolve()
    wgrep = str(Path(args.wgrep).resolve())
    tgrep = str(Path(args.tgrep).resolve())
    rg = args.rg
    queries = QUERY_MIX[args.corpus_kind]

    base = dict(os.environ)
    wgrep_env = dict(base, WGREP_INDEX_DIR=str(args.wgrep_index.resolve()))
    tgrep_env = dict(base)

    report = {"corpus": str(corpus), "kind": args.corpus_kind, "reps": args.reps,
              "wgrep_index": str(args.wgrep_index.resolve()),
              "modes": {}}

    def do_mode(mode: str) -> dict:
        arms = [("rg", rg, dict(base)), ("wgrep", wgrep, dict(wgrep_env)), ("tgrep", tgrep, dict(tgrep_env))]
        if mode == "oneshot":
            arms = [("rg", rg, dict(base)),
                    ("wgrep", wgrep, dict(wgrep_env, WGREP_NO_DAEMON="1")),
                    ("tgrep", tgrep, dict(tgrep_env))]
        server = None
        if mode == "daemon":
            wgrep_daemon_start(wgrep, corpus, wgrep_env)
            server = tgrep_start_server(tgrep, corpus)
        try:
            print(f"\n=== {mode} | {corpus} ===", flush=True)
            print(f"{'query':>20} {'rg_ms':>9} {'wgrep_ms':>9} {'tgrep_ms':>9} {'tgrep/rg':>9} {'wgrep/rg':>9}")
            header = []
            result = {"arms": {}, "table": []}
            for tool, exe, env in arms:
                warm(exe, queries, corpus, env)
                rows = measure(exe, queries, corpus, env, args.reps)
                result["arms"][tool] = {"exe": exe, "rows": rows}
                header.append((tool, {r["query"]: r["median_ms"] for r in rows}))
            merged = {}
            for tool, by_q in header:
                for q, ms in by_q.items():
                    merged.setdefault(q, {})[tool] = ms
            def fmt(v):
                return "  TIMEOUT" if v is None else f"{v:>9.1f}"

            for label, _ in queries:
                m = merged[label]
                r = m["rg"]
                ratio = lambda x: ("   n/a" if (x is None or r is None or r == 0) else f"{x/r:>8.2f}x")
                print(f"{label:>20} {fmt(r)} {fmt(m['wgrep'])} {fmt(m['tgrep'])} "
                      f"{ratio(m['tgrep'])} {ratio(m['wgrep'])}")
                result["table"].append({"query": label, **{k: m[k] for k in ("rg", "wgrep", "tgrep")}})
            return result
        finally:
            if server is not None:
                server.terminate()
                try:
                    server.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    server.kill()
            if mode == "daemon":
                subprocess.run([wgrep, "--stop", str(corpus)], env=wgrep_env,
                               stdin=DNULL, stdout=DNULL, stderr=DNULL)

    modes = ["oneshot", "daemon"] if args.mode == "both" else [args.mode]
    for mode in modes:
        report["modes"][mode] = do_mode(mode)

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        Path(args.out).write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
