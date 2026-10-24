#!/usr/bin/env python3
"""Inspect definition-boost scores in a WG_DEFBOOST tool output."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    path = Path(sys.argv[1])
    limit = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    data = json.loads(path.read_text(encoding='utf-8'))
    for q in data:
        base = next(c for c in q['configs'] if c['config']['name'] == 'base')
        rows = next(c for c in q['configs'] if c['config']['name'] == 'rows')
        target = base['targets'][0]
        print(f"== {q['id']}  base_file_rank={target['file_rank']} base_fused_rank={target['fused_rank']}")
        print(f"   target_path={target['path']}")
        print(f"   query={q['query'][:110]!r}")
        for r in rows['rows'][:limit]:
            print(f"   {r['rank']:3d} f={r['fused']:.4f} bm={r['bm25_relative']:.3f} "
                  f"se={r['semantic_relative']:.3f} defn={r['defn_score']:.3f} {r['path']}:{r['start']}")
        print()


if __name__ == '__main__':
    main()
