#!/usr/bin/env python3
"""Per-query rank changes for one config (appendix tables)."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    rows = [json.loads(l) for l in Path(sys.argv[1]).read_text(encoding='utf-8').splitlines() if l.strip()]
    name = sys.argv[2]
    print(f'{"query":50s} {"base":>5s} {"cfg":>5s}')
    n = 0
    for r in rows:
        a = r['configs']['base']['file_rank10'] or 0
        b = r['configs'][name]['file_rank10'] or 0
        if a != b:
            n += 1
            print(f'{r["id"]:50s} {a:5d} {b:5d}')
    print(f'changed: {n}/{len(rows)}')


if __name__ == '__main__':
    main()
