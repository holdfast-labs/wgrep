#!/usr/bin/env python3
"""Measure per-query signal cost and peak RSS for one corpus.

Runs the tool once per arm in separate processes and polls RSS. Prints the
tool's profile lines. Run on the VPS.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from pathlib import Path

import psutil

TOOL = Path('/srv/projects/wispd/wg-defboost-20261024/target/release/wg-quality-tool')
ARMS = {
    'base': [{'name': 'base', 'lex': 200, 'sem': 200, 'w': 0.25}],
    'def10': [{'name': 'def10', 'lex': 200, 'sem': 200, 'w': 0.25, 'def': 0.10}],
    'def10_gate': [{'name': 'def10_gate', 'lex': 200, 'sem': 200, 'w': 0.25, 'def': 0.10,
                    'defn_gate': 'length', 'defn_min_words': 20}],
    'spec10': [{'name': 'spec10', 'lex': 200, 'sem': 200, 'w': 0.25, 'defn': 0.10,
                'defn_idf': True, 'defn_spec': 0.10, 'defn_rarity': True, 'defn_kw': 'broad'}],
}


def run(label: str, root: Path, queries: Path, configs: list[dict], poll: float = 0.02):
    out = Path('/tmp') / f'defboost-cost-{label}.json'
    argv = [str(TOOL), '--root', str(root), '--queries', str(queries),
            '--out', str(out), '--top', '10', '--profile']
    for cfg in configs:
        argv += ['--config', json.dumps(cfg)]
    env = dict(os.environ, HF_HUB_OFFLINE='1', RAYON_NUM_THREADS='2')
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
    ap.add_argument('--arms', default='base,def10,def10_gate,spec10')
    ap.add_argument('--label', default='')
    args = ap.parse_args()
    print(f'corpus={args.root} queries={args.queries}')
    for arm in args.arms.split(','):
        wall, peak, stderr = run(arm, args.root, args.queries, ARMS[arm])
        print(f'== {arm}: wall={wall:.2f}s peak_rss={peak/2**20:.1f} MiB')
        for line in stderr.splitlines():
            if line.startswith('def-index') or line.startswith('profile:') or line.startswith('defn-stats'):
                print('   ' + line)


if __name__ == '__main__':
    main()
