#!/usr/bin/env python3
"""Per-phase profiling harness for wgrep's WGREP_PROFILE instrumentation.

For each query (run separately) and each agent count:
  1. stop any daemon serving the corpus;
  2. warm up with 3 runs of the query, with WGREP_PROFILE set, which
     auto-starts a daemon that also writes to the same profile file;
  3. record the file's size, so warm-up lines are ignored;
  4. run N agent threads, each running wgrep as a subprocess
     --per-agent times, timing each process externally and sampling
     whole-machine CPU every 100 ms.

Output: <profile-dir>/summary.json, one <query>-a<N>.ndjson per cell, and a
printed table. Also times `wgrep --version` and `rg --version` once.
"""

import argparse
import json
import math
import os
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

try:
    import psutil
except ImportError:  # machine CPU columns are reported as n/a without it
    psutil = None

# Client marks in the order the code records them.
PHASES = [
    "main", "args_parsed", "plan_done", "connect_done", "header_received",
    "paths_received", "haystacks_built", "daemon_haystacks_done", "search_done", "end",
]
DEFAULT_QUERIES = [["zzqxjwvbnm"], ["kmalloc"], ["-w", "static"], ["-i", "the"]]
DEVNULL = subprocess.DEVNULL


def median(values):
    return statistics.median(values) if values else None


def percentile(values, p):
    if not values:
        return None
    s = sorted(values)
    k = min(len(s) - 1, max(0, math.ceil(p / 100.0 * len(s)) - 1))
    return s[k]


def fmt(value, width=8, digits=1):
    if value is None:
        return "n/a".rjust(width)
    return f"{value:{width}.{digits}f}"


def query_name(qargs):
    return re.sub(r"[^A-Za-z0-9]+", "_", " ".join(qargs)).strip("_") or "query"


def base_env(index_dir, profile=None):
    env = os.environ.copy()
    env["WGREP_INDEX_DIR"] = str(index_dir)
    env.pop("WGREP_PROFILE", None)
    if profile is not None:
        env["WGREP_PROFILE"] = str(profile)
    return env


def stop_daemon(wgrep, corpus, index_dir):
    subprocess.run([wgrep, "--stop", str(corpus)], env=base_env(index_dir),
                   stdout=DEVNULL, stderr=DEVNULL)
    # The daemon removes daemon.json and exits; give it a moment to release
    # its lock so the next auto-start is not refused.
    time.sleep(1.0)


def run_query(wgrep, corpus, qargs, env):
    return subprocess.run([wgrep, "--no-messages", *qargs, str(corpus)], env=env,
                          stdout=DEVNULL, stderr=DEVNULL).returncode


def read_profile_from(path, offset):
    if not path.exists():
        return []
    with open(path, "rb") as f:
        f.seek(offset)
        data = f.read().decode("utf-8", "replace")
    records = []
    for line in data.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            pass  # a torn line would be a bug in the writer; skip it
    return records


def run_agents(wgrep, corpus, qargs, n, per_agent, env):
    """Runs n concurrent agents, each doing per_agent sequential searches.
    Returns (wall_ms list, exit codes list, machine cpu samples)."""
    walls, codes = [], []
    lock = threading.Lock()
    barrier = threading.Barrier(n)
    cmd = [wgrep, "--no-messages", *qargs, str(corpus)]

    def agent():
        barrier.wait()
        for _ in range(per_agent):
            t0 = time.perf_counter()
            rc = subprocess.run(cmd, env=env, stdout=DEVNULL, stderr=DEVNULL).returncode
            dt = (time.perf_counter() - t0) * 1000.0
            with lock:
                walls.append(dt)
                codes.append(rc)

    cpu = []
    stop = threading.Event()

    def sampler():
        if psutil is None:
            return
        psutil.cpu_percent(interval=None)  # prime: the next call covers the last 100 ms
        while not stop.is_set():
            time.sleep(0.1)
            cpu.append(psutil.cpu_percent(interval=None))

    samp = threading.Thread(target=sampler, daemon=True)
    workers = [threading.Thread(target=agent) for _ in range(n)]
    samp.start()
    for w in workers:
        w.start()
    for w in workers:
        w.join()
    stop.set()
    samp.join()
    return walls, codes, cpu


def summarize_cell(qname, qargs, n, walls, codes, cpu, records, profile_file):
    clients = [r for r in records if r.get("proc") == "client"]
    daemons = [r for r in records if r.get("proc") == "daemon"]

    deltas = {}
    totals = []
    for rec in clients:
        marks = rec.get("marks", {})
        prev = None
        for name in PHASES:
            if name not in marks:
                continue
            if prev is not None:
                deltas.setdefault(f"{prev}>{name}", []).append(marks[name] - marks[prev])
            prev = name
        if "end" in marks and "main" in marks:
            totals.append(marks["end"] - marks["main"])

    cpu_costs = [r["cpu_user_ms"] + r["cpu_kernel_ms"]
                 for r in clients if "cpu_user_ms" in r and "cpu_kernel_ms" in r]

    def dmed(key):
        return median([d[key] for d in daemons if key in d])

    ext_p50 = median(walls)
    client_total_p50 = median(totals)
    return {
        "query": qname,
        "args": qargs,
        "agents": n,
        "runs": len(walls),
        "exit_codes": {str(c): codes.count(c) for c in sorted(set(codes))},
        "ext_p50_ms": ext_p50,
        "ext_p95_ms": percentile(walls, 95),
        "client_records": len(clients),
        "daemon_records": len(daemons),
        "client_total_p50_ms": client_total_p50,
        "process_overhead_p50_ms": (ext_p50 - client_total_p50)
        if ext_p50 is not None and client_total_p50 is not None else None,
        "phase_p50_ms": {k: median(v) for k, v in deltas.items()},
        "daemon_p50": {
            "read_ms": dmed("read_ms"),
            "lock_ms": dmed("lock_ms"),
            "answer_ms": dmed("answer_ms"),
            "write_ms": dmed("write_ms"),
            "candidates": dmed("candidates"),
            "rescans": sum(1 for d in daemons if d.get("rescan")),
        },
        "client_cpu_ms_p50": median(cpu_costs),
        "machine_cpu_pct": {
            "mean": statistics.fmean(cpu) if cpu else None,
            "max": max(cpu) if cpu else None,
            "samples": len(cpu),
        },
        "profile_file": str(profile_file),
    }


def print_tables(cells):
    print()
    print("Per query and agent count (external wall and client totals in ms)")
    head = (f"{'query':<14}{'agents':>7}{'runs':>6}{'ext p50':>9}{'ext p95':>9}"
            f"{'client':>9}{'overhead':>10}{'d.read':>8}{'d.lock':>8}{'d.answer':>10}"
            f"{'d.write':>9}{'cands':>8}{'cpu ms':>8}{'mach%':>8}{'max%':>7}")
    print(head)
    print("-" * len(head))
    for c in cells:
        d = c["daemon_p50"]
        m = c["machine_cpu_pct"]
        print(f"{c['query']:<14}{c['agents']:>7}{c['runs']:>6}"
              f"{fmt(c['ext_p50_ms'], 9)}{fmt(c['ext_p95_ms'], 9)}"
              f"{fmt(c['client_total_p50_ms'], 9)}{fmt(c['process_overhead_p50_ms'], 10)}"
              f"{fmt(d['read_ms'], 8, 2)}{fmt(d['lock_ms'], 8, 2)}{fmt(d['answer_ms'], 10, 2)}"
              f"{fmt(d['write_ms'], 9, 2)}{fmt(d['candidates'], 8, 0)}"
              f"{fmt(c['client_cpu_ms_p50'], 8)}{fmt(m['mean'], 8)}{fmt(m['max'], 7)}")

    print()
    print("Client phase medians (ms, delta between consecutive marks)")
    labels = []
    for c in cells:
        for k in c["phase_p50_ms"]:
            if k not in labels:
                labels.append(k)
    print(f"{'phase':<40}" + "".join(f"{c['query'][:10] + '/a' + str(c['agents']):>16}" for c in cells))
    for label in labels:
        row = "".join(fmt(c["phase_p50_ms"].get(label), 16, 2) for c in cells)
        print(f"{label:<40}{row}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wgrep", required=True, help="path to the wgrep executable")
    ap.add_argument("--corpus", required=True, help="directory to search")
    ap.add_argument("--index", required=True, help="WGREP_INDEX_DIR for the runs")
    ap.add_argument("--profile-dir", required=True, help="output directory")
    ap.add_argument("--agents", default="1,4,16", help="comma-separated concurrent agent counts")
    ap.add_argument("--per-agent", type=int, default=8, help="searches per agent")
    ap.add_argument("--query", action="append", default=None,
                    help="one query as a shell-style string, e.g. 'kmalloc' or '-w static' (repeatable)")
    ap.add_argument("--rg", default="rg", help="rg executable for the startup baseline")
    args = ap.parse_args()

    queries = [shlex.split(q) for q in args.query] if args.query else DEFAULT_QUERIES
    agent_counts = [int(x) for x in args.agents.split(",") if x.strip()]
    profile_dir = Path(args.profile_dir)
    profile_dir.mkdir(parents=True, exist_ok=True)
    corpus = Path(args.corpus)

    if psutil is None:
        print("note: psutil not installed; machine CPU columns will be n/a", file=sys.stderr)

    cells = []
    for qargs in queries:
        qname = query_name(qargs)
        for n in agent_counts:
            profile_file = profile_dir / f"{qname}-a{n}.ndjson"
            print(f"[{qname} a{n}] stop, warm up, run", file=sys.stderr)
            stop_daemon(args.wgrep, corpus, args.index)
            env = base_env(args.index, profile_file)
            for _ in range(3):
                run_query(args.wgrep, corpus, qargs, env)
            offset = profile_file.stat().st_size if profile_file.exists() else 0
            walls, codes, cpu = run_agents(args.wgrep, corpus, qargs, n, args.per_agent, env)
            time.sleep(0.2)  # the daemon's line can land just after its client exits
            records = read_profile_from(profile_file, offset)
            cells.append(summarize_cell(qname, qargs, n, walls, codes, cpu, records, profile_file))
    stop_daemon(args.wgrep, corpus, args.index)

    def startup(cmd):
        times = []
        for _ in range(20):
            t0 = time.perf_counter()
            subprocess.run(cmd, stdout=DEVNULL, stderr=DEVNULL)
            times.append((time.perf_counter() - t0) * 1000.0)
        return median(times)

    rg_path = shutil.which(args.rg) or args.rg
    startup_ms = {
        "wgrep_version_p50_ms": startup([args.wgrep, "--version"]),
        "rg_version_p50_ms": startup([rg_path, "--version"]) if shutil.which(args.rg) else None,
    }

    summary = {
        "generated": datetime.now().isoformat(timespec="seconds"),
        "wgrep": args.wgrep,
        "corpus": str(corpus),
        "index": args.index,
        "per_agent": args.per_agent,
        "queries": [" ".join(q) for q in queries],
        "agents": agent_counts,
        "startup": startup_ms,
        "cells": cells,
    }
    (profile_dir / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")

    print_tables(cells)
    print()
    print(f"Startup, median of 20 (ms): wgrep --version {fmt(startup_ms['wgrep_version_p50_ms'], 1, 1).strip()}"
          f", rg --version {fmt(startup_ms['rg_version_p50_ms'], 1, 1).strip()}")
    print(f"Summary: {profile_dir / 'summary.json'}")


if __name__ == "__main__":
    main()
