#!/usr/bin/env python3
"""Query-shape statistics for the definition-boost gate study.

Reads query text only (no ranks, no labels) from the published RepoQA and
CoSQA inputs. Prints length and identifier-lexeme distributions. Run on the
VPS.
"""
from __future__ import annotations

import json
import re
import statistics
from collections import Counter, defaultdict
from pathlib import Path

REPOQA = Path('/srv/projects/wispd/sgrep-published-inputs/repoqa-import/repoqa/queries.json')
COSQA = Path('/srv/projects/wispd/sgrep-published-inputs/CoCLR/data/search/cosqa-retrieval-test-500.json')

IDENT = re.compile(r'[\w$]+')
CAMEL = re.compile(r'[a-z][A-Z]|[A-Z][a-z]+[A-Z]')
STOP = set('a an and are as at be by for from how in is it its of on or that the this to was what when where which who why with does do'.split())


def ident_like(text: str) -> list[str]:
    out = []
    for w in IDENT.findall(text):
        if '_' in w or CAMEL.search(w) or any(ch.isdigit() for ch in w):
            out.append(w)
    return out


def describe(name: str, queries: list[dict], key: str) -> None:
    lengths = []
    content = []
    n_ident = 0
    ident_examples = Counter()
    for q in queries:
        text = q[key]
        words = IDENT.findall(text)
        content_words = [w for w in words if w.lower() not in STOP and len(w) > 1]
        lengths.append(len(words))
        content.append(len(content_words))
        ids = ident_like(text)
        if ids:
            n_ident += 1
            ident_examples.update(ids)
    n = len(queries)
    print(f'== {name}: n={n}')
    for label, arr in (('words', lengths), ('content', content)):
        arr_s = sorted(arr)
        p = lambda f: arr_s[min(len(arr_s) - 1, int(f * (len(arr_s) - 1)))]
        print(f'  {label}: min={arr_s[0]} p10={p(.1)} p25={p(.25)} median={statistics.median(arr)} '
              f'p75={p(.75)} p90={p(.9)} max={arr_s[-1]} mean={statistics.mean(arr):.2f}')
    print(f'  with identifier-like token: {n_ident}/{n} = {n_ident / n:.3f}')
    print(f'  top identifier-like: {ident_examples.most_common(12)}')
    for k in (8, 10, 12, 14, 16, 18, 20):
        print(f'  content>={k}: {sum(1 for c in content if c >= k) / n:.3f}')


def main() -> None:
    repo = json.loads(REPOQA.read_text(encoding='utf-8'))
    cosqa = json.loads(COSQA.read_text(encoding='utf-8'))
    by_lang = defaultdict(list)
    for q in repo:
        by_lang[q['language']].append(q)
    # repo-level dev split, same rule as WG_QUALITY_REPOQA.py
    dev = []
    for lang, group in by_lang.items():
        sids = sorted({q['source_id'] for q in group})
        keep = set(sids[:5])
        dev.extend(q for q in group if q['source_id'] in keep)
    describe('RepoQA dev', dev, 'query')
    describe('RepoQA all', repo, 'query')
    describe('CoSQA all', cosqa, 'doc')
    print('\nRepoQA dev samples:')
    for q in dev[:12]:
        print(' ', q['query'])
    print('\nCoSQA samples:')
    for q in cosqa[:12]:
        print(' ', q['doc'])


if __name__ == '__main__':
    main()
