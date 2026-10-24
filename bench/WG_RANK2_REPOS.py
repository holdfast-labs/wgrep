#!/usr/bin/env python3
"""Per-repo held-out NDCG table for selected configs."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    path = Path(sys.argv[1])
    configs = sys.argv[2:]
    rows = json.loads(path.read_text(encoding='utf-8'))
    repos = sorted({r['repo'] for r in rows})
    print(f'{"repo":22s} ' + ' '.join(f'{c:>8s}' for c in configs))
    for repo in repos:
        vals = []
        for cfg in configs:
            v = next((r['ndcg@10'] for r in rows if r['repo'] == repo and r['config'] == cfg), 0.0)
            vals.append(f'{v:8.4f}')
        print(f'{repo:22s} ' + ' '.join(vals))


if __name__ == '__main__':
    main()
