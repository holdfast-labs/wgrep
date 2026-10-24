#!/usr/bin/env python3
"""Dump per-query ranks for several configs plus target definition scores."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    path = Path(sys.argv[1])
    names = sys.argv[2:]
    rows = [json.loads(l) for l in path.read_text(encoding='utf-8').splitlines() if l.strip()]
    print(f'{"id":50s} ' + ' '.join(f'{n:>10s}' for n in names))
    for r in rows:
        vals = []
        for n in names:
            c = r['configs'][n]
            vals.append(f'{(c["file_rank10"] or 0):>4d}/{c.get("target_defn") or 0:.2f}')
        base = r['configs'][names[0]]['file_rank10'] or 0
        if any((r['configs'][n]['file_rank10'] or 0) != base for n in names[1:]):
            print(f'{r["id"]:50s} ' + ' '.join(f'{v:>10s}' for v in vals))


if __name__ == '__main__':
    main()
