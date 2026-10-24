#!/usr/bin/env python3
"""List per-query Hit@1 transitions (appendix)."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    rows = [json.loads(l) for l in Path(sys.argv[1]).read_text(encoding='utf-8').splitlines() if l.strip()]
    for name in sys.argv[2:]:
        gains, losses = [], []
        for r in rows:
            a = r['configs']['base']['file_rank10'] or 0
            b = r['configs'][name]['file_rank10'] or 0
            if b == 1 and a != 1:
                gains.append(r['id'])
            elif a == 1 and b != 1:
                losses.append(r['id'])
        print(f'== {name}: H@1 gains={len(gains)} losses={len(losses)}')
        print('  gains :', ', '.join(gains) or '-')
        print('  losses:', ', '.join(losses) or '-')


if __name__ == '__main__':
    main()
