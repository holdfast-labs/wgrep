#!/usr/bin/env python3
"""Deterministic repo-level dev/held-out split for the Semble benchmark.

Stratified by language, fixed seed. Writes the list before any tuning.
"""
from __future__ import annotations

import hashlib
import json
import random
from collections import defaultdict
from pathlib import Path

BENCH = Path('/srv/projects/wispd/wg-semble-20261024/semble-bench')
OUT = Path('/srv/projects/wispd/wg-rank2-20261024/split.json')
SEED = 20261024


def main() -> None:
    specs = json.loads((BENCH / 'repos.json').read_text(encoding='utf-8'))
    groups: dict[str, list[str]] = defaultdict(list)
    for spec in specs:
        groups[spec['language']].append(spec['name'])
    ok = {spec['name']: spec for spec in specs}
    dev: dict[str, list[str]] = {}
    held: dict[str, list[str]] = {}
    for i, lang in enumerate(sorted(groups)):
        names = sorted(groups[lang])
        random.Random(SEED + i).shuffle(names)
        n = len(names)
        if n == 1:
            n_dev = 1 if i % 2 == 0 else 0
        else:
            n_dev = (n + 1) // 2 if i % 2 == 0 else n // 2
        dev[lang] = sorted(names[:n_dev])
        held[lang] = sorted(names[n_dev:])
    payload = {
        'seed': SEED,
        'rule': 'per language: sorted names, seeded shuffle, alternate which half gets the extra repo',
        'dev': dev,
        'heldout': held,
    }
    text = json.dumps(payload, indent=2) + '\n'
    OUT.write_text(text, encoding='utf-8')
    dev_names = [n for v in dev.values() for n in v]
    held_names = [n for v in held.values() for n in v]
    assert not (set(dev_names) & set(held_names))
    assert set(dev_names) | set(held_names) == set(ok)
    print(f'dev repos={len(dev_names)} heldout repos={len(held_names)}')
    print(f'sha256={hashlib.sha256(text.encode()).hexdigest()}')
    print(text)


if __name__ == '__main__':
    main()
