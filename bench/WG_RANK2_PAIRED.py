#!/usr/bin/env python3
"""Paired statistics for the rank2 study.

Reads the Semble dev+held-out runs and reports combined macro, per-repo wins
and losses, and per-query NDCG wins/losses for each config against base.
"""
from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path


def load_jsonl(path: Path):
    return [json.loads(l) for l in path.read_text(encoding='utf-8').splitlines() if l.strip()]


def main() -> None:
    base = Path('/srv/projects/wispd/wg-rank2-20261024/results')
    dev_repo = json.loads((base / 'devpre2/per-repo.json').read_text(encoding='utf-8'))
    held_repo = json.loads((base / 'heldout2/per-repo.json').read_text(encoding='utf-8'))
    dev_q = load_jsonl(base / 'devpre2/per-query.jsonl')
    held_q = load_jsonl(base / 'heldout2/per-query.jsonl')
    configs = sorted({r['config'] for r in dev_repo})
    print(f'{"config":12s} {"dev":>7s} {"held":>7s} {"comb63":>7s} '
          f'{"repoW":>5s} {"repoL":>5s} {"qW":>5s} {"qL":>5s} {"qTie":>5s}')
    for cfg in configs:
        dv = {r['repo']: r['ndcg@10'] for r in dev_repo if r['config'] == cfg}
        hv = {r['repo']: r['ndcg@10'] for r in held_repo if r['config'] == cfg}
        bv = {r['repo']: r['ndcg@10'] for r in dev_repo if r['config'] == 'base'}
        bh = {r['repo']: r['ndcg@10'] for r in held_repo if r['config'] == 'base'}
        dev_m = sum(dv.values()) / len(dv)
        held_m = sum(hv.values()) / len(hv)
        comb = (sum(dv.values()) + sum(hv.values())) / (len(dv) + len(hv))
        repo_w = sum(1 for k in hv if hv[k] > bh[k] + 1e-12)
        repo_l = sum(1 for k in hv if hv[k] < bh[k] - 1e-12)
        qd = {r['repo'] + '|' + r['query']: r['ndcg@10'] for r in dev_q if r['config'] == cfg}
        qh = {r['repo'] + '|' + r['query']: r['ndcg@10'] for r in held_q if r['config'] == cfg}
        bqd = {r['repo'] + '|' + r['query']: r['ndcg@10'] for r in dev_q if r['config'] == 'base'}
        bqh = {r['repo'] + '|' + r['query']: r['ndcg@10'] for r in held_q if r['config'] == 'base'}
        q = {**qd, **qh}
        bq = {**bqd, **bqh}
        qw = sum(1 for k in q if q[k] > bq[k] + 1e-12)
        ql = sum(1 for k in q if q[k] < bq[k] - 1e-12)
        qt = len(q) - qw - ql
        print(f'{cfg:12s} {dev_m:7.4f} {held_m:7.4f} {comb:7.4f} '
              f'{repo_w:5d} {repo_l:5d} {qw:5d} {ql:5d} {qt:5d}')
    # Largest held-out repo changes for r1.
    for cfg in [c for c in configs if c != 'base']:
        deltas = []
        for r in held_repo:
            if r['config'] == cfg:
                b = next(x['ndcg@10'] for x in held_repo if x['repo'] == r['repo'] and x['config'] == 'base')
                deltas.append((r['ndcg@10'] - b, r['repo']))
        deltas.sort()
        print(f'\n{cfg} held-out largest regressions:', deltas[:5])
        print(f'{cfg} held-out largest gains:', deltas[-5:][::-1])


if __name__ == '__main__':
    main()
