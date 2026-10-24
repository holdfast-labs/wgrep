#!/usr/bin/env python3
"""Per-query cost and peak RSS for the rank2 signals.

Runs the tool once per arm in a separate process, polls RSS, and prints the
tool's profile lines. Run on the VPS with ~/venv/bin/python.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from pathlib import Path

import psutil

TOOL = Path('/srv/projects/wispd/wg-rank2-20261024/target/release/wg-quality-tool')
ARMS = {
    'base': [{'name': 'base', 'lex': 200, 'sem': 200, 'w': 0.25}],
    'r1': [{'name': 'r1', 'lex': 200, 'sem': 200, 'w': 0.25,
            'sat': 0.5, 'sym': 1.0, 'stem': 0.6, 'coh': 0.05}],
    'r5': [{'name': 'r5', 'lex': 200, 'sem': 200, 'w': 0.25,
            'sym': 1.0, 'stem': 0.6}],
}


def run(label: str, root: Path, queries: Path, top: int, poll: float = 0.02):
    out = Path('/tmp') / f'rank2-cost-{label}.json'
    argv = [str(TOOL), '--root', str(root), '--queries', str(queries),
            '--out', str(out), '--top', str(top), '--profile']
    for cfg in ARMS[label]:
        argv += ['--config', json.dumps(cfg)]
    env = dict(os.environ, HF_HUB_OFFLINE='1', RAYON_NUM_THREADS='4')
    env.pop('RIPGREP_CONFIG_PATH', None)
    started = time.perf_counter()
    p = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env)
    proc = psutil.Process(p.pid)
    peak = 0
    while p.poll() is None:
        try:
            peak = max(peak, proc.memory_info().rss)
        except psutil.Error:
            pass
        time.sleep(poll)
    stderr = p.stderr.read().decode(errors='replace')
    wall = time.perf_counter() - started
    if p.returncode != 0:
        raise SystemExit(f'{label} failed: {stderr[-2000:]}')
    return wall, peak, stderr


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument('--root', type=Path, required=True)
    ap.add_argument('--queries', type=Path, required=True)
    ap.add_argument('--arms', default='base,r1,r5')
    ap.add_argument('--top', type=int, default=10)
    args = ap.parse_args()
    print(f'corpus={args.root} queries={args.queries} top={args.top}')
    for arm in args.arms.split(','):
        wall, peak, stderr = run(arm, args.root, args.queries, args.top)
        print(f'== {arm}: wall={wall:.2f}s peak_rss={peak/2**20:.1f} MiB')
        for line in stderr.splitlines():
            if line.startswith('def-index') or line.startswith('profile:'):
                print('   ' + line)


if __name__ == '__main__':
    main()
