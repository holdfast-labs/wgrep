#!/usr/bin/env python3
"""Per-query comparison of two configs in a WG_DEFBOOST rows.jsonl."""
from __future__ import annotations

import json
import sys
from collections import Counter
from pathlib import Path


def main() -> None:
    path = Path(sys.argv[1])
    a = sys.argv[2]
    b = sys.argv[3]
    rows = [json.loads(l) for l in path.read_text(encoding='utf-8').splitlines() if l.strip()]
    wins = losses = 0
    by_lang = Counter()
    print(f'{"id":52s} {"lang":10s} {"rankA":>5s} {"rankB":>5s} {"defnA":>6s} {"defnB":>6s}')
    for r in rows:
        ca, cb = r['configs'][a], r['configs'][b]
        ra = ca['file_rank10'] or 0
        rb = cb['file_rank10'] or 0
        if ra == rb:
            continue
        better = b if (rb and (not ra or rb < ra)) else a
        if better == b:
            wins += 1
        else:
            losses += 1
        by_lang[r['language']] += 1 if better == b else -1
        print(f'{r["id"]:52s} {r["language"]:10s} {ra:5d} {rb:5d} '
              f'{(ca.get("target_defn") or 0):6.3f} {(cb.get("target_defn") or 0):6.3f}')
    print(f'\n{b} vs {a}: wins={wins} losses={losses}')
    print('language net (positive = B better):', dict(by_lang))


if __name__ == '__main__':
    main()
