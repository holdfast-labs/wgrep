#!/usr/bin/env python3
"""Print a summary table and profile aggregates for a WG_DEFBOOST run."""
from __future__ import annotations

import argparse
import json
import re
from collections import defaultdict
from pathlib import Path


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument('--run', type=Path, required=True)
    ap.add_argument('--split', default='dev')
    args = ap.parse_args()
    run = args.run / args.split
    summary = json.loads((run / 'summary.json').read_text(encoding='utf-8'))
    print(f'{"config":22s} {"H@1":>4s} {"H@5":>4s} {"H@10":>4s} {"MRR":>7s} '
          f'{"dH1":>4s} {"dMRR":>7s} {"W/L":>7s} {"line@5":>6s} {"umiss":>5s}')
    for name, s in summary.items():
        print(f'{name:22s} {s["hit@1"]:4d} {s["hit@5"]:4d} {s["hit@10"]:4d} '
              f'{s["mrr@10"]:7.5f} {s["delta_hit@1"]:+4d} {s["delta_mrr"]:+7.4f} '
              f'{s["paired_file_hit_wins_vs_base"]:3d}/{s["paired_file_hit_losses_vs_base"]:<3d} '
              f'{s.get("line_hit@5", 0):6d} {s.get("union_misses", 0):5d}')
    prof = run / 'profile.log'
    if prof.is_file():
        idx_build = []
        per_query = defaultdict(list)
        stats = defaultdict(lambda: defaultdict(list))
        for line in prof.read_text(encoding='utf-8').splitlines():
            if 'def-index build total:' in line:
                idx_build.append(float(line.rsplit(':', 1)[1].split()[0]))
            m = re.search(r'profile: queries=\d+ defn=([\d.]+) us/q legacy_def=([\d.]+) us/q pen=([\d.]+) us/q rrf=([\d.]+) us/q index=([\d.]+) ms', line)
            if m:
                per_query['defn'].append(float(m.group(1)))
                per_query['legacy_def'].append(float(m.group(2)))
                per_query['pen'].append(float(m.group(3)))
                per_query['rrf'].append(float(m.group(4)))
            m = re.search(r'defn-stats (\S+): union_rows=(\d+) fire=([\d.]+) full=([\d.]+) queries_firing=([\d.]+)', line)
            if m:
                stats[m.group(1)]['union_rows'].append(int(m.group(2)))
                stats[m.group(1)]['fire'].append(float(m.group(3)))
                stats[m.group(1)]['full'].append(float(m.group(4)))
                stats[m.group(1)]['qfiring'].append(float(m.group(5)))
        if idx_build:
            print(f'\ndef-index build per repo: mean={sum(idx_build)/len(idx_build):.1f} ms '
                  f'max={max(idx_build):.1f} ms n={len(idx_build)}')
        if per_query['defn']:
            print(f'per-query signal cost: defn mean={sum(per_query["defn"])/len(per_query["defn"]):.1f} us, '
                  f'pen mean={sum(per_query["pen"])/len(per_query["pen"]):.1f} us, '
                  f'rrf mean={sum(per_query["rrf"])/len(per_query["rrf"]):.1f} us')
        for name in sorted(stats):
            s = stats[name]
            n = len(s['fire'])
            print(f'defn-stats {name:20s} fire={sum(s["fire"])/n:.3f} full={sum(s["full"])/n:.3f} '
                  f'queries_firing={sum(s["qfiring"])/n:.3f} union_rows={sum(s["union_rows"])/n:.0f}')


if __name__ == '__main__':
    main()
