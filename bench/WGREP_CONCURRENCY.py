#!/usr/bin/env python3
"""Concurrent-agent benchmark for standalone wgrep vs rg.

Each "agent" is an independent process running a fixed sequence of queries;
N agents run at once. Reports, per agent count: per-query p50/p95 latency,
aggregate throughput (completed queries / makespan), and peak aggregate
private memory (summed USS of the live agent processes).

Every invocation is a separate process (wgrep is one-shot), so this measures
the real cost of repeated traversal, freshness checks and per-process threads
under load. Deterministic query mix; warm-up precedes timing.
"""
from __future__ import annotations

import argparse
import os
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

try:
    import psutil
except ImportError:  # memory sampling is best-effort
    psutil = None

DNULL = subprocess.DEVNULL

# A generic code-corpus mix: one absent literal (all files skipped), one
# selective identifier, one medium, one broad. Callers can override.
DEFAULT_QUERIES = [
    ["zzqxjwvbnm"],
    ["kmalloc"],
    ["-w", "static"],
    ["-i", "the"],
]


class MemSampler(threading.Thread):
    """Peak summed USS of this process's descendants, which is the aggregate
    private memory of the concurrent agents."""

    def __init__(self) -> None:
        super().__init__(daemon=True)
        self._done = threading.Event()
        self.peak = 0
        self.available = psutil is not None

    def run(self) -> None:
        if not self.available:
            return
        me = psutil.Process()
        while not self._done.is_set():
            total = 0
            try:
                for child in me.children(recursive=True):
                    try:
                        mi = child.memory_info()
                        # `private` (commit) is the portable private-memory
                        # figure psutil exposes on Windows; fall back to rss.
                        total += getattr(mi, "private", None) or mi.rss
                    except (psutil.NoSuchProcess, psutil.AccessDenied):
                        pass
            except Exception:
                pass
            self.peak = max(self.peak, total)
            time.sleep(0.03)

    def stop(self) -> None:
        self._done.set()


def percentile(xs, p):
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = min(len(xs) - 1, int(round((p / 100.0) * (len(xs) - 1))))
    return xs[k]


def run_arm(exe, base_env, corpus, queries, agents, per_agent):
    """One timed batch: `agents` concurrent processes, each running
    `per_agent` queries sequentially. Returns (latencies_ms, makespan_s,
    peak_private_bytes)."""
    lat: list[float] = []
    lock = threading.Lock()

    def agent(i: int) -> None:
        for j in range(per_agent):
            q = queries[(i + j) % len(queries)]
            t = time.perf_counter()
            subprocess.run(
                [exe, "--no-messages", *q, str(corpus)],
                env=base_env, stdin=DNULL, stdout=DNULL, stderr=DNULL,
            )
            dt = (time.perf_counter() - t) * 1000.0
            with lock:
                lat.append(dt)

    sampler = MemSampler()
    sampler.start()
    start = time.perf_counter()
    threads = [threading.Thread(target=agent, args=(i,)) for i in range(agents)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    makespan = time.perf_counter() - start
    sampler.stop()
    sampler.join(timeout=1.0)
    return lat, makespan, sampler.peak


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wgrep", required=True)
    ap.add_argument("--rg", default="rg")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--index", required=True, help="WGREP_INDEX_DIR for wgrep")
    ap.add_argument("--agents", default="1,4,16,32")
    ap.add_argument("--per-agent", type=int, default=3, help="queries each agent runs per batch")
    ap.add_argument("--reps", type=int, default=2)
    ap.add_argument("--query", action="append", default=None, help="repeatable single-pattern query (overrides default mix)")
    ap.add_argument("--out", default=None, help="write JSON to this path")
    args = ap.parse_args()

    wgrep = str(Path(args.wgrep).resolve())
    rg = args.rg
    corpus = Path(args.corpus).resolve()
    index = Path(args.index).resolve()
    agents_list = [int(x) for x in args.agents.split(",")]
    queries = [[q] for q in args.query] if args.query else DEFAULT_QUERIES

    wgrep_env = dict(os.environ, WGREP_INDEX_DIR=str(index))
    rg_env = dict(os.environ)

    # Warm-up (index mmap / page cache), untimed.
    for q in queries:
        subprocess.run([wgrep, "--no-messages", *q, str(corpus)], env=wgrep_env, stdin=DNULL, stdout=DNULL, stderr=DNULL)
        subprocess.run([rg, "--no-messages", *q, str(corpus)], env=rg_env, stdin=DNULL, stdout=DNULL, stderr=DNULL)

    print(f"corpus={corpus} index={index} per_agent={args.per_agent} reps={args.reps} cores={os.cpu_count()}")
    print(f"{'agents':>6} {'tool':>6} {'p50ms':>8} {'p95ms':>8} {'q/s':>8} {'peak_priv_MB':>12}")
    report = {"corpus": str(corpus), "index": str(index), "per_agent": args.per_agent, "reps": args.reps, "rows": []}
    for n in agents_list:
        for tool, exe, env in (("wgrep", wgrep, wgrep_env), ("rg", rg, rg_env)):
            thrs, p95s, p50s, mems = [], [], [], []
            for _ in range(args.reps):
                lat, makespan, peak = run_arm(exe, env, corpus, queries, n, args.per_agent)
                thrs.append((n * args.per_agent) / makespan if makespan > 0 else float("nan"))
                p50s.append(percentile(lat, 50))
                p95s.append(percentile(lat, 95))
                mems.append(peak)
            row = {
                "agents": n, "tool": tool,
                "p50_ms": statistics.median(p50s),
                "p95_ms": statistics.median(p95s),
                "throughput_qps": statistics.median(thrs),
                "peak_private_bytes": max(mems),
            }
            report["rows"].append(row)
            print(f"{n:>6} {tool:>6} {row['p50_ms']:>8.0f} {row['p95_ms']:>8.0f} "
                  f"{row['throughput_qps']:>8.1f} {row['peak_private_bytes']/1e6:>12.1f}")

    if args.out:
        import json
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        Path(args.out).write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
