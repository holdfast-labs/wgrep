#!/usr/bin/env python3
"""Paired statistics for the `no sat` held-out run."""
from __future__ import annotations

import json
from collections import defaultdict
from pathlib import Path

RUN = Path('/srv/projects/wispd/wg-rank3-20261024/results/nosat-semble')


def main() -> None:
    repo = json.loads((RUN / 'per-repo.json').read_text(encoding='utf-8'))
    query = [json.loads(l) for l in (RUN / 'per-query.jsonl').read_text(encoding='utf-8').splitlines() if l.strip()]
    base_r = {r['repo']: r['ndcg@10'] for r in repo if r['config'] == 'base'}
    nos_r = {r['repo']: r['ndcg@10'] for r in repo if r['config'] == 'nosat'}

    def keyed(rows, cfg):
        seen = defaultdict(int)
        out = {}
        for r in rows:
            if r['config'] != cfg:
                continue
            k = (r['repo'], r['query'], seen[(r['repo'], r['query'])])
            seen[(r['repo'], r['query'])] += 1
            out[k] = r['ndcg@10']
        return out

    base_q = keyed(query, 'base')
    nos_q = keyed(query, 'nosat')
    repos = sorted(base_r)
    print(f'macro base={sum(base_r.values())/len(repos):.4f} nosat={sum(nos_r.values())/len(repos):.4f}')
    rw = sum(1 for r in repos if nos_r[r] > base_r[r] + 1e-12)
    rl = sum(1 for r in repos if nos_r[r] < base_r[r] - 1e-12)
    print(f'repos improved={rw} worse={rl} flat={len(repos)-rw-rl} of {len(repos)}')
    qw = sum(1 for k in nos_q if nos_q[k] > base_q[k] + 1e-12)
    ql = sum(1 for k in nos_q if nos_q[k] < base_q[k] - 1e-12)
    print(f'queries wins={qw} losses={ql} ties={len(nos_q)-qw-ql} of {len(nos_q)}')
    cats = defaultdict(lambda: [0, 0, 0, []])
    seen_q = defaultdict(int)
    for r in query:
        if r['config'] != 'nosat':
            continue
        k = (r['repo'], r['query'], seen_q[(r['repo'], r['query'])])
        seen_q[(r['repo'], r['query'])] += 1
        c = cats[r['category']]
        c[0] += 1
        c[1] += nos_q[k]
        c[2] += base_q[k]
        c[3].append(nos_q[k] - base_q[k])
    print('category n  base  nosat  delta')
    for cat in sorted(cats):
        n, s, b, _ = cats[cat]
        print(f'{cat:13s} {n:4d} {b/n:6.4f} {s/n:6.4f} {(s-b)/n:+.4f}')
    deltas = sorted(((nos_r[r] - base_r[r], r) for r in repos))
    print('worst repos:', [(round(d, 4), r) for d, r in deltas[:5]])
    print('best repos :', [(round(d, 4), r) for d, r in deltas[-5:][::-1]])


if __name__ == '__main__':
    main()
