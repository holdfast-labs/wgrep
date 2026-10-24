#!/usr/bin/env python3
"""Final paired tables for the definition-boost report."""
from __future__ import annotations

import json
import sys
from collections import Counter
from pathlib import Path


def load(path: Path):
    return [json.loads(l) for l in path.read_text(encoding='utf-8').splitlines() if l.strip()]


def h1_transitions(rows, name):
    gain = loss = 0
    gains, losses = [], []
    for r in rows:
        a = r['configs']['base']['file_rank10'] or 0
        b = r['configs'][name]['file_rank10'] or 0
        b1 = a == 1
        c1 = b == 1
        if c1 and not b1:
            gain += 1
            gains.append(r['id'])
        elif b1 and not c1:
            loss += 1
            losses.append(r['id'])
    return gain, loss, gains, losses


def show(label: str, rows_path: Path) -> None:
    if not rows_path.is_file():
        print(f'{label}: missing {rows_path}')
        return
    rows = load(rows_path)
    names = list(rows[0]['configs'])
    print(f'\n== {label} (n={len(rows)})')
    print(f'{"config":18s} {"H@1":>4s} {"H@5":>4s} {"H@10":>4s} {"MRR":>7s} {"H1+":>4s} {"H1-":>4s} {"top10+":>7s} {"top10-":>7s}')
    base_ranks = [r['configs']['base']['file_rank10'] or 0 for r in rows]
    bmrr = sum(1.0 / x for x in base_ranks if 0 < x <= 10) / len(rows)
    for name in names:
        ranks = [r['configs'][name]['file_rank10'] or 0 for r in rows]
        h = lambda k: sum(1 for x in ranks if 0 < x <= k)
        mrr = sum(1.0 / x for x in ranks if 0 < x <= 10) / len(rows)
        g, l, _, _ = h1_transitions(rows, name)
        w = loss = 0
        for r in rows:
            a = r['configs']['base']['file_rank10'] or 0
            b = r['configs'][name]['file_rank10'] or 0
            if b and (not a or b < a):
                w += 1
            elif a and (not b or a < b):
                loss += 1
        print(f'{name:18s} {h(1):4d} {h(5):4d} {h(10):4d} {mrr:7.5f} {g:+4d} {l:-4d} {w:+7d} {loss:-7d} '
              f'(mrr {mrr-bmrr:+.4f})')


def main() -> None:
    base = Path('/srv/projects/wispd/wg-defboost-20261024/results')
    for label, rel in (('RepoQA dev', 'dev3/dev/rows.jsonl'),
                       ('RepoQA held-out', 'heldout/heldout/rows.jsonl'),
                       ('CoSQA dev', 'cosqa/dev/rows.jsonl'),
                       ('CoSQA held-out', 'cosqa/heldout/rows.jsonl'),
                       ('Semble bench', 'semble-bench/rows.jsonl')):
        show(label, base / rel)
    sem = base / 'semble-bench/rows.jsonl'
    if sem.is_file():
        rows = load(sem)
        cats = Counter(r['category'] for r in rows)
        print('\nSemble categories:', dict(cats))


if __name__ == '__main__':
    main()
