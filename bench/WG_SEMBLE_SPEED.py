#!/usr/bin/env python3
"""Speed and memory on the 13.3k-file Rust `src` corpus.

Semble runs two ways: the in-process library (closest to its MCP server, which
holds an index in memory) and the `semble search` CLI. wg runs as its CLI, which
talks to the resident daemon (process start and IPC included). The edit probes
measure time from a returned write to the first CLI response that contains the
new symbol.

No tool setting is tuned. Run on an idle VPS (load average < 1.0).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import resource
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

BASE = Path('/srv/projects/wispd/wg-semble-20261024')
CORPUS = Path('/srv/workspaces/wispd/prof/src')
WG = BASE / 'bin/wg'
SEMBLE = BASE / 'semble-venv/bin/semble'
SEMBLE_PY = BASE / 'semble-venv/bin/python'
QUERIES = [
    'parse Rust expressions',
    'emit compiler error diagnostics',
    'validate command line options',
    'resolve module imports',
    'incremental compilation query dependency tracking',
]
PROBE_REL = 'zz_wg_semble_probe.rs'


def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def percentile(values, q):
    ordered = sorted(values)
    if not ordered:
        return 0.0
    pos = (len(ordered) - 1) * q / 100.0
    lo = int(pos)
    hi = min(lo + 1, len(ordered) - 1)
    frac = pos - lo
    return ordered[lo] * (1 - frac) + ordered[hi] * frac


def require_idle():
    load = os.getloadavg()
    if load[0] >= 1.0:
        raise SystemExit(f'refusing to measure: load average {load[0]:.2f} >= 1.0')
    return load


def env_base(cache: Path | None):
    env = dict(os.environ)
    env['HF_HOME'] = str(BASE / 'hf')
    env['HF_HUB_OFFLINE'] = '1'
    env.pop('RIPGREP_CONFIG_PATH', None)
    if cache is not None:
        env['SEMBLE_CACHE_LOCATION'] = str(cache)
    return env


def rss_tree(pid):
    import psutil
    try:
        p = psutil.Process(pid)
        total = p.memory_info().rss
        for child in p.children(recursive=True):
            try:
                total += child.memory_info().rss
            except psutil.Error:
                pass
        return total
    except psutil.Error:
        return 0


def sample_wg_daemons(stop, samples):
    import psutil
    wanted = str(WG)
    while not stop.is_set():
        total = 0
        for proc in psutil.process_iter(['cmdline']):
            try:
                cmd = proc.info['cmdline'] or []
            except psutil.Error:
                continue
            if len(cmd) >= 2 and cmd[0] == wanted and '--daemon' in cmd:
                try:
                    total += proc.memory_info().rss
                except psutil.Error:
                    pass
        samples.append(total)
        time.sleep(0.05)


def preexisting_daemons():
    import psutil
    out = []
    for proc in psutil.process_iter(['pid', 'cmdline']):
        try:
            cmd = proc.info['cmdline'] or []
        except psutil.Error:
            continue
        if len(cmd) >= 2 and cmd[0] == str(WG) and '--daemon' in cmd:
            out.append(proc.info['pid'])
    return out


def run_semble_inproc(args):
    load = require_idle()
    import numpy as np
    sys.path.insert(0, str(BASE / 'semble-repo'))
    import semble
    from semble import SembleIndex

    cache = args.cache.resolve()
    if cache.exists():
        raise SystemExit(f'cache exists: {cache}')
    cache.parent.mkdir(parents=True, exist_ok=True)
    os.environ['SEMBLE_CACHE_LOCATION'] = str(cache)
    os.environ['HF_HOME'] = str(BASE / 'hf')
    os.environ['HF_HUB_OFFLINE'] = '1'

    t0 = time.perf_counter()
    index = SembleIndex.from_path(args.corpus.resolve())
    index_s = time.perf_counter() - t0
    assert not index.loaded_from_disk
    steady_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
    files = index.stats.indexed_files
    chunks = index.stats.total_chunks

    times = {}
    for q in QUERIES:
        for _ in range(2):
            index.search(q, top_k=10, alpha=None, rerank=True)
        samples = []
        for _ in range(args.reps):
            t0 = time.perf_counter()
            res = index.search(q, top_k=10, alpha=None, rerank=True)
            samples.append((time.perf_counter() - t0) * 1000)
            if not res:
                raise SystemExit(f'no results for {q!r}')
        times[q] = samples
    pooled = [v for s in times.values() for v in s]
    peak_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
    out = {
        'arm': 'C-semble-inproc',
        'tool': 'semble',
        'semble_version': semble.__version__,
        'corpus': str(args.corpus),
        'files': files,
        'chunks': chunks,
        'index_seconds': round(index_s, 3),
        'index_rss_bytes': steady_rss,
        'peak_rss_bytes': peak_rss,
        'rss_after_queries_bytes': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024,
        'reps_per_query': args.reps,
        'p50_ms': percentile(pooled, 50),
        'p95_ms': percentile(pooled, 95),
        'per_query_p50_ms': {q: percentile(v, 50) for q, v in times.items()},
        'raw_ms': times,
        'load_before': load,
        'load_after': os.getloadavg(),
    }
    args.out.write_text(json.dumps(out, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in out.items() if k != 'raw_ms'}, indent=2))


def run_semble_cli(args):
    load = require_idle()
    cache = args.cache.resolve()
    env = env_base(cache)

    def search(q, timed=True):
        argv = [str(SEMBLE), 'search', q, str(args.corpus), '-k', '10', '--format', 'json']
        t0 = time.perf_counter()
        p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        dt = (time.perf_counter() - t0) * 1000
        if p.returncode != 0:
            raise SystemExit(f'semble cli failed: {p.stderr.decode()[-800:]}')
        out = json.loads(p.stdout)
        if not out.get('results'):
            raise SystemExit(f'no results for {q!r}: {out}')
        return dt

    # Warm the cache and OS cache.
    search(QUERIES[0])
    times = {}
    for q in QUERIES:
        for _ in range(2):
            search(q)
        samples = [search(q) for _ in range(args.reps)]
        times[q] = samples
    pooled = [v for s in times.values() for v in s]

    # One more call with RSS sampling for the peak interpreter footprint.
    import psutil
    argv = [str(SEMBLE), 'search', QUERIES[0], str(args.corpus), '-k', '10', '--format', 'json']
    p = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    peak = 0
    while p.poll() is None:
        peak = max(peak, rss_tree(p.pid))
        time.sleep(0.01)
    out = {
        'arm': 'C-semble-cli',
        'tool': 'semble CLI',
        'semble_version': subprocess.check_output(
            [str(SEMBLE), '--version'], text=True, env=env).strip(),
        'corpus': str(args.corpus),
        'reps_per_query': args.reps,
        'p50_ms': percentile(pooled, 50),
        'p95_ms': percentile(pooled, 95),
        'per_query_p50_ms': {q: percentile(v, 50) for q, v in times.items()},
        'cli_peak_rss_bytes': peak,
        'raw_ms': times,
        'load_before': load,
        'load_after': os.getloadavg(),
    }
    args.out.write_text(json.dumps(out, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in out.items() if k != 'raw_ms'}, indent=2))


def run_wg(args):
    load = require_idle()
    assert not preexisting_daemons(), 'a wg daemon is already running'
    service = args.service.resolve()
    if service.exists():
        raise SystemExit(f'service dir exists: {service}')
    service.parent.mkdir(parents=True, exist_ok=True)
    env = env_base(None)
    env['WG_SERVICE_DIR'] = str(service)

    samples = []
    stop = threading.Event()
    thread = threading.Thread(target=sample_wg_daemons, args=(stop, samples), daemon=True)
    thread.start()
    t0 = time.perf_counter()
    up = subprocess.run([str(WG), 'up', str(args.corpus), '--model', 'code', '--json'],
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=1800)
    up_s = time.perf_counter() - t0
    stop.set()
    thread.join()
    if up.returncode != 0:
        raise SystemExit(f'wg up failed: {up.stderr.decode()[-1200:]}')
    up_json = json.loads(up.stdout)
    pid = up_json['pid']
    time.sleep(2.0)
    steady = rss_tree(pid)

    def query(q):
        t0 = time.perf_counter()
        p = subprocess.run([str(WG), q, str(args.corpus), '--model', 'code', '--json',
                            '--top', '10'], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           env=env, timeout=300)
        dt = (time.perf_counter() - t0) * 1000
        if p.returncode != 0:
            raise SystemExit(f'wg query failed: {p.stderr.decode()[-800:]}')
        rows = json.loads(p.stdout)
        if not rows:
            raise SystemExit(f'no results for {q!r}')
        return dt

    try:
        times = {}
        for q in QUERIES:
            for _ in range(2):
                query(q)
            times[q] = [query(q) for _ in range(args.reps)]
        pooled = [v for s in times.values() for v in s]
        out = {
            'arm': 'C-wg',
            'tool': 'wg CLI (daemon resident; process start and IPC included)',
            'wg': str(WG),
            'wg_sha256': sha(WG),
            'wg_commit': '7ffb6242d4151d29d45915803729684de4fd5649',
            'corpus': str(args.corpus),
            'files': up_json['roots'][0]['files'],
            'chunks': up_json['roots'][0]['chunks'],
            'index_seconds': round(up_s, 3),
            'up_json': {k: up_json[k] for k in ('build_ms', 'model_ms', 'total_ms')},
            'peak_rss_bytes': max(samples) if samples else 0,
            'steady_rss_bytes': steady,
            'rss_after_queries_bytes': rss_tree(pid),
            'reps_per_query': args.reps,
            'p50_ms': percentile(pooled, 50),
            'p95_ms': percentile(pooled, 95),
            'per_query_p50_ms': {q: percentile(v, 50) for q, v in times.items()},
            'raw_ms': times,
            'load_before': load,
            'load_after': os.getloadavg(),
        }
    finally:
        subprocess.run([str(WG), 'down'], env=env, timeout=120)
    args.out.write_text(json.dumps(out, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in out.items() if k not in ('raw_ms', 'up_json')}, indent=2))


def _copy_corpus(src: Path, dst: Path):
    if dst.exists():
        shutil.rmtree(dst)
    subprocess.run(['cp', '-a', str(src), str(dst)], check=True)


def run_edit_semble(args):
    load = require_idle()
    corpus = args.corpus.resolve()
    cache = args.cache.resolve()
    if cache.exists():
        shutil.rmtree(cache)
    env = env_base(cache)
    probe = corpus / PROBE_REL
    if probe.exists():
        probe.unlink()

    # Build the initial index.
    t0 = time.perf_counter()
    subprocess.run([str(SEMBLE), 'search', QUERIES[0], str(corpus), '-k', '10', '--format', 'json'],
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env, check=True)
    build_s = time.perf_counter() - t0

    samples = []
    total = args.warmup + args.samples
    for i in range(total):
        symbol = f'wg_semble_edit_probe_{i}'
        probe.write_text(f'pub fn {symbol}() -> u32 {{ {i} }}\n', encoding='utf-8')
        t0 = time.perf_counter()
        found = False
        deadline = t0 + args.timeout
        while time.perf_counter() < deadline:
            p = subprocess.run([str(SEMBLE), 'search', symbol, str(corpus), '-k', '10',
                                '--format', 'json'], stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, env=env)
            if p.returncode == 0:
                out = json.loads(p.stdout)
                if any(r['file_path'] == PROBE_REL for r in out.get('results', [])):
                    found = True
                    break
        dt = (time.perf_counter() - t0) * 1000
        if not found:
            raise SystemExit(f'edit probe {i} never became searchable')
        if i >= args.warmup:
            samples.append(dt)
    out = {
        'arm': 'C-edit-semble',
        'tool': 'semble CLI',
        'method': 'write file, then repeat `semble search <symbol>` until the probe file is returned',
        'watcher': 'none: index refresh happens inside the next search call',
        'initial_index_seconds': round(build_s, 3),
        'samples': len(samples),
        'min_ms': min(samples),
        'p50_ms': statistics.median(samples),
        'p95_ms': percentile(samples, 95),
        'max_ms': max(samples),
        'raw_ms': samples,
        'load_before': load,
        'load_after': os.getloadavg(),
    }
    args.out.write_text(json.dumps(out, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in out.items() if k != 'raw_ms'}, indent=2))


def run_edit_wg(args):
    load = require_idle()
    assert not preexisting_daemons(), 'a wg daemon is already running'
    corpus = args.corpus.resolve()
    service = args.service.resolve()
    if service.exists():
        shutil.rmtree(service)
    env = env_base(None)
    env['WG_SERVICE_DIR'] = str(service)
    probe = corpus / PROBE_REL
    if probe.exists():
        probe.unlink()

    t0 = time.perf_counter()
    up = subprocess.run([str(WG), 'up', str(corpus), '--model', 'code', '--json'],
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=1800)
    build_s = time.perf_counter() - t0
    if up.returncode != 0:
        raise SystemExit(f'wg up failed: {up.stderr.decode()[-1200:]}')
    samples = []
    total = args.warmup + args.samples
    try:
        for i in range(total):
            symbol = f'wg_semble_edit_probe_{i}'
            probe.write_text(f'pub fn {symbol}() -> u32 {{ {i} }}\n', encoding='utf-8')
            t0 = time.perf_counter()
            found = False
            deadline = t0 + args.timeout
            while time.perf_counter() < deadline:
                p = subprocess.run([str(WG), symbol, str(corpus), '--model', 'code',
                                    '--json', '--top', '10'], stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, env=env)
                if p.returncode == 0:
                    rows = json.loads(p.stdout)
                    if any(r.get('path') == PROBE_REL for r in rows):
                        found = True
                        break
            dt = (time.perf_counter() - t0) * 1000
            if not found:
                raise SystemExit(f'edit probe {i} never became searchable')
            if i >= args.warmup:
                samples.append(dt)
    finally:
        subprocess.run([str(WG), 'down'], env=env, timeout=120)
    out = {
        'arm': 'C-edit-wg',
        'tool': 'wg CLI',
        'method': 'write file, then repeat `wg <symbol>` until the probe file is returned',
        'watcher': 'inotify watcher refreshes the index in the daemon',
        'initial_index_seconds': round(build_s, 3),
        'samples': len(samples),
        'min_ms': min(samples),
        'p50_ms': statistics.median(samples),
        'p95_ms': percentile(samples, 95),
        'max_ms': max(samples),
        'raw_ms': samples,
        'load_before': load,
        'load_after': os.getloadavg(),
    }
    args.out.write_text(json.dumps(out, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in out.items() if k != 'raw_ms'}, indent=2))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest='cmd', required=True)
    for name in ('semble-inproc', 'semble-cli'):
        p = sub.add_parser(name)
        p.add_argument('--corpus', type=Path, default=CORPUS)
        p.add_argument('--cache', type=Path, required=True)
        p.add_argument('--out', type=Path, required=True)
        p.add_argument('--reps', type=int, default=24)
    p = sub.add_parser('wg')
    p.add_argument('--corpus', type=Path, default=CORPUS)
    p.add_argument('--service', type=Path, required=True)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--reps', type=int, default=24)
    for name in ('edit-semble', 'edit-wg'):
        p = sub.add_parser(name)
        p.add_argument('--corpus', type=Path, required=True)
        p.add_argument('--cache', type=Path, default=None)
        p.add_argument('--service', type=Path, default=None)
        p.add_argument('--out', type=Path, required=True)
        p.add_argument('--samples', type=int, default=20)
        p.add_argument('--warmup', type=int, default=2)
        p.add_argument('--timeout', type=float, default=120.0)
    args = ap.parse_args()
    if args.cmd == 'semble-inproc':
        run_semble_inproc(args)
    elif args.cmd == 'semble-cli':
        run_semble_cli(args)
    elif args.cmd == 'wg':
        run_wg(args)
    elif args.cmd == 'edit-semble':
        run_edit_semble(args)
    elif args.cmd == 'edit-wg':
        run_edit_wg(args)


if __name__ == '__main__':
    main()
