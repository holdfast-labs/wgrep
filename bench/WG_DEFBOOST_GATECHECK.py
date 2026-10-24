#!/usr/bin/env python3
"""Gate threshold check: word counts for each evaluation query set."""
from __future__ import annotations

import json
import re
from collections import defaultdict
from pathlib import Path

WORD = re.compile(r'[\w$]+')
REPOQA = Path('/srv/projects/wispd/sgrep-published-inputs/repoqa-import/repoqa/queries.json')
COSQA = Path('/srv/projects/wispd/sgrep-published-inputs/CoCLR/data/search/cosqa-retrieval-test-500.json')
LAUNCH = [
    'How does the parser handle unary expressions in Rust code?',
    'Where does rustfmt decide to lay out a method chain vertically?',
    'How does clippy find unnecessary clone calls on Copy types?',
    'How does miri implement the Stacked Borrows aliasing model?',
    'Where does rustdoc build the search index for documentation pages?',
    'How does bootstrap download a prebuilt LLVM from CI?',
    'Where does compiletest compare the compiler output against expected files?',
    'How does rust-analyzer resolve a module import during name resolution?',
]


def main() -> None:
    repo = json.loads(REPOQA.read_text(encoding='utf-8'))
    by_lang = defaultdict(list)
    for q in repo:
        by_lang[q['language']].append(q)
    dev, held = [], []
    for lang, group in by_lang.items():
        sids = sorted({q['source_id'] for q in group})
        for q in group:
            (dev if sids.index(q['source_id']) < 5 else held).append(q)
    cos = json.loads(COSQA.read_text(encoding='utf-8'))
    bench = Path('/srv/projects/wispd/wg-semble-20261024/semble-bench/annotations')
    sem_texts = []
    for f in sorted(bench.glob('*.json')):
        sem_texts.extend(t['query'] for t in json.loads(f.read_text(encoding='utf-8')))
    sets = {
        'RepoQA dev': [q['query'] for q in dev],
        'RepoQA held-out': [q['query'] for q in held],
        'CoSQA dev': [q['doc'] for q in cos[:250]],
        'CoSQA held-out': [q['doc'] for q in cos[250:]],
        'launch q1-q8': LAUNCH,
        'Semble bench': sem_texts,
    }
    for name, texts in sets.items():
        counts = [len(WORD.findall(t)) for t in texts]
        print(f'{name:18s} n={len(counts):3d} min={min(counts):3d} max={max(counts):3d} '
              f'pass>=20={sum(1 for c in counts if c >= 20)}/{len(counts)}')


if __name__ == '__main__':
    main()
