#!/usr/bin/env python3
"""Print the Semble NDCG dev/held-out summary as a compact table."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    path = Path(sys.argv[1])
    d = json.loads(path.read_text(encoding='utf-8'))
    print(f'{"config":16s} {"macro":>7s} {"qweight":>7s} {"arch":>7s} {"sem":>7s} {"sym":>7s}')
    for name, s in d.items():
        cats = s['by_category']
        print(f'{name:16s} {s["ndcg@10_macro"]:7.4f} {s["ndcg@10_query_weighted"]:7.4f} '
              f'{cats.get("architecture", {}).get("ndcg@10", 0):7.4f} '
              f'{cats.get("semantic", {}).get("ndcg@10", 0):7.4f} '
              f'{cats.get("symbol", {}).get("ndcg@10", 0):7.4f}')


if __name__ == '__main__':
    main()
