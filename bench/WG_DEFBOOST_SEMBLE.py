#!/usr/bin/env python3
"""Third confirmation set: Semble's public benchmark annotations (read-only).

Reads the annotations and checkouts that the Semble-bench agent imported under
/srv/projects/wispd/wg-semble-20261024/semble-bench/. Writes only to this
study's results directory. Scores distinct-file Hit@1/5/10 and MRR@10 against
the first primary relevant path, overall and by category.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from collections import defaultdict
from pathlib import Path

BENCH = Path('/srv/projects/wispd/wg-semble-20261024/semble-bench')
TOOL = Path('/srv/projects/wispd/wg-defboost-20261024/target/release/wg-quality-tool')


def target_path(entry) -> str | None:
    if isinstance(entry, str):
        return entry
    if isinstance(entry, dict):
        return entry.get('path')
    return None


def target_line(entry) -> int:
    if isinstance(entry, dict) and entry.get('start_line'):
        return int(entry['start_line'])
    return 1


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default=str(TOOL))
    ap.add_argument('--run', type=Path, default=Path('/srv/projects/wispd/wg-defboost-20261024/results/semble-bench'))
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--top', type=int, default=50)
    ap.add_argument('--only-repo', default=None)
    ap.add_argument('--online', action='store_true',
                    help='allow the tsx parser to be fetched once; models stay cached')
    args = ap.parse_args()

    run = args.run.resolve()
    run.mkdir(parents=True, exist_ok=True)
    configs = json.loads(args.configs.read_text(encoding='utf-8'))
    specs = json.loads((BENCH / 'repos.json').read_text(encoding='utf-8'))
    env = dict(os.environ, HF_HUB_OFFLINE='1', RAYON_NUM_THREADS='2')
    if args.online:
        env.pop('HF_HUB_OFFLINE', None)
    env.pop('RIPGREP_CONFIG_PATH', None)
    all_rows = []
    missing_roots = 0
    started = time.time()
    for n, spec in enumerate(specs, 1):
        name = spec['name']
        if args.only_repo and args.only_repo not in name:
            continue
        ann = BENCH / 'annotations' / f'{name}.json'
        if not ann.is_file():
            continue
        tasks = json.loads(ann.read_text(encoding='utf-8'))
        base = BENCH / 'repos' / name
        candidates = [base]
        if spec.get('benchmark_root'):
            candidates.insert(0, base / spec['benchmark_root'])
        candidates = [c for c in candidates if c.is_dir()]
        if not candidates:
            missing_roots += 1
            print(f'skip {name}: no checkout', flush=True)
            continue
        paths = [target_path(t['relevant'][0]) if t.get('relevant') else None for t in tasks]

        def matches(root: Path) -> int:
            return sum(1 for p in paths if p and (root / p).is_file())

        root = max(candidates, key=matches)
        payload = []
        for i, (t, path) in enumerate(zip(tasks, paths)):
            if not path or not (root / path).is_file():
                missing_roots += 1
                continue
            first = t['relevant'][0]
            payload.append({
                'id': f'{name}-{i}',
                'query': t['query'],
                'terms': [],
                'targets': [{'path': path, 'line': target_line(first)}],
                'category': t.get('category', 'unknown'),
            })
        if not payload:
            continue
        qpath = run / f'{name}.queries.json'
        qpath.write_text(json.dumps([{k: v for k, v in p.items() if k != 'category'} for p in payload]),
                         encoding='utf-8')
        outp = run / f'{name}.tool.json'
        argv = [args.tool, '--root', str(root), '--queries', str(qpath),
                '--out', str(outp), '--top', str(args.top)]
        for cfg in configs:
            argv += ['--config', json.dumps(cfg)]
        p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           env=env, timeout=3600)
        if p.returncode != 0:
            raise SystemExit(f'tool failed {name}: {p.stderr.decode()[-2000:]}')
        data = json.loads(outp.read_text(encoding='utf-8'))
        for qrow, meta in zip(data, payload):
            item = {'id': qrow['id'], 'repo': name, 'category': meta['category'],
                    'language': spec['language'], 'configs': {}}
            for c in qrow['configs']:
                t = c['targets'][0]
                item['configs'][c['config']['name']] = {
                    'file_rank': t['file_rank'],
                    'file_rank10': t['file_rank10'],
                    'line_rank': t['line_rank'],
                    'in_union': t['in_union'],
                    'target_defn': t['chunk'].get('defn_score'),
                }
            all_rows.append(item)
        print(f'[{n}/{len(specs)}] {name} {len(data)} queries', flush=True)
    (run / 'rows.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in all_rows), encoding='utf-8')

    def report(subset, label):
        base_rank = {r['id']: r['configs']['base']['file_rank10'] or 0 for r in subset}
        for cfg in configs:
            name = cfg['name']
            ranks = [r['configs'][name]['file_rank10'] or 0 for r in subset]
            nq = len(ranks)
            h = lambda k: sum(1 for x in ranks if 0 < x <= k)
            mrr = sum(1.0 / x for x in ranks if 0 < x <= 10) / max(nq, 1)
            bmrr = sum(1.0 / x for x in base_rank.values() if 0 < x <= 10) / max(nq, 1)
            print(f'{label:12s} {name:16s} n={nq:4d} H@1={h(1)/max(nq,1):.3f} '
                  f'H@5={h(5)/max(nq,1):.3f} H@10={h(10)/max(nq,1):.3f} '
                  f'MRR={mrr:.4f} dMRR={mrr-bmrr:+.4f}')
    print()
    report(all_rows, 'ALL')
    by_cat = defaultdict(list)
    for r in all_rows:
        by_cat[r['category']].append(r)
    for cat in sorted(by_cat):
        print()
        report(by_cat[cat], cat)
    print(f'\nrows={len(all_rows)} missing_roots={missing_roots} elapsed={time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
