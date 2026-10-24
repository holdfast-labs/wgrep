#!/usr/bin/env python3
"""Compare two configs, possibly in the same rows.jsonl.

Usage: WG_DEFBOOST_COMPARE.py <fileA> <fileB> <cfgA> [cfgB]
When cfgB is omitted, cfgA is used in both files (baseline identity check).
"""
from __future__ import annotations

import json
import sys
from pathlib import Path


def load(path: Path):
    return {json.loads(line)['id']: json.loads(line)
            for line in path.read_text(encoding='utf-8').splitlines() if line.strip()}


def main() -> None:
    a = load(Path(sys.argv[1]))
    b = load(Path(sys.argv[2]))
    ca = sys.argv[3]
    cb = sys.argv[4] if len(sys.argv) > 4 else ca
    assert a.keys() == b.keys(), (len(a), len(b))
    diff = 0
    for qid in sorted(a):
        ra = a[qid]['configs'][ca]
        rb = b[qid]['configs'][cb]
        keys = ('file_rank', 'file_rank10', 'line_rank', 'in_union', 'fused_rank', 'pool_size')
        da = [ra[k] for k in keys]
        db = [rb[k] for k in keys]
        if da != db:
            diff += 1
            print(f'{qid}: {da} != {db}')
    print(f'compared {len(a)} queries, {diff} differ')


if __name__ == '__main__':
    main()
