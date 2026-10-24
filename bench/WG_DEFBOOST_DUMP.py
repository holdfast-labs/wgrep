#!/usr/bin/env python3
"""Dump exact counts from one or more result summaries."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    for path in sys.argv[1:]:
        p = Path(path)
        if not p.is_file():
            print(f'{p}: missing')
            continue
        d = json.loads(p.read_text(encoding='utf-8'))
        print(f'== {p.parent}')
        for k, v in d.items():
            print(f'{k:18s} {v["hit@1"]:4d} {v["hit@5"]:4d} {v["hit@10"]:4d} {v["mrr@10"]:.5f}')


if __name__ == '__main__':
    main()
