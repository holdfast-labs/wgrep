#!/usr/bin/env python3
"""How many queries pass Semble's is_symbol_query gate (whole-query regex)?"""
from __future__ import annotations

import json
import re
from collections import defaultdict
from pathlib import Path

_SYMBOL_QUERY_RE = re.compile(
    r"^(?:"
    r"[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+"
    r"|_[A-Za-z0-9_]*"
    r"|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*"
    r"|[A-Z][A-Za-z0-9]*"
    r")$"
)


def main() -> None:
    repo = json.loads(Path('/srv/projects/wispd/sgrep-published-inputs/repoqa-import/repoqa/queries.json').read_text(encoding='utf-8'))
    cosqa = json.loads(Path('/srv/projects/wispd/sgrep-published-inputs/CoCLR/data/search/cosqa-retrieval-test-500.json').read_text(encoding='utf-8'))
    sets = {
        'RepoQA (all 600)': [q['query'] for q in repo],
        'CoSQA (all 500)': [q['doc'] for q in cosqa],
    }
    bench = Path('/srv/projects/wispd/wg-semble-20261024/semble-bench/annotations')
    sem = []
    for f in sorted(bench.glob('*.json')):
        sem.extend(t['query'] for t in json.loads(f.read_text(encoding='utf-8')))
    sets['Semble bench'] = sem
    for name, texts in sets.items():
        n = sum(1 for t in texts if _SYMBOL_QUERY_RE.match(t.strip()))
        print(f'{name:20s} {n}/{len(texts)} = {n/len(texts):.3f}')


if __name__ == '__main__':
    main()
