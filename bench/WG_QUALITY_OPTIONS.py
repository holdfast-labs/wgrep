#!/usr/bin/env python3
"""Dev/held-out option sweep for wg ranking on RepoQA.

Uses WG_QUALITY_TOOL to reproduce the baseline ranker and evaluate candidate
depth, fusion weight, path weighting and symbol-definition boosts. Tuning uses
the dev split only; run the held-out split once with the chosen configs.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from collections import defaultdict
from pathlib import Path

IMPORT = Path('/srv/projects/wispd/sgrep-published-inputs/repoqa-import')
QUERY_PATH = IMPORT / 'repoqa/queries.json'


def split_of(sid, index):
    return 'dev' if index < 5 else 'heldout'


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default='/srv/projects/wispd/wg-quality-20261020/src/target/release/wg-quality-tool')
    ap.add_argument('--run', type=Path, default=Path('/srv/projects/wispd/wg-quality-20261020/options'))
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--split', choices=['dev', 'heldout'], required=True)
    ap.add_argument('--top', type=int, default=100)
    args = ap.parse_args()

    run = args.run.resolve() / args.split
    run.mkdir(parents=True, exist_ok=True)
    queries = json.loads(QUERY_PATH.read_text(encoding='utf-8'))
    configs = json.loads(args.configs.read_text(encoding='utf-8'))

    by_repo = defaultdict(list)
    for q in queries:
        by_repo[q['source_id']].append(q)
    per_language = defaultdict(list)
    for sid in by_repo:
        per_language[by_repo[sid][0]['language']].append(sid)
    repos = []
    for lang, sids in per_language.items():
        for i, sid in enumerate(sorted(sids)):
            if split_of(sid, i) == args.split:
                repos.append(sid)
    repos.sort()

    env = dict(os.environ, HF_HUB_OFFLINE='1')
    env['RAYON_NUM_THREADS'] = '2'
    env.pop('RIPGREP_CONFIG_PATH', None)
    all_rows = []
    started = time.time()
    for n, sid in enumerate(repos, 1):
        group = by_repo[sid]
        root = Path(group[0]['root'])
        qpath = run / f'{sid}.queries.json'
        qpath.write_text(json.dumps([
            {'id': q['id'], 'query': q['query'], 'terms': [],
             'targets': [{'path': q['target_path'], 'line': q['gold']['start_line']}]}
            for q in group]), encoding='utf-8')
        outp = run / f'{sid}.tool.json'
        argv = [args.tool, '--root', str(root), '--queries', str(qpath),
                '--out', str(outp), '--top', str(args.top)]
        for cfg in configs:
            argv += ['--config', json.dumps(cfg)]
        p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           env=env, timeout=3600)
        if p.returncode != 0:
            raise SystemExit(f'tool failed {sid}: {p.stderr.decode()[-1200:]}')
        data = json.loads(outp.read_text(encoding='utf-8'))
        for qrow in data:
            item = {'id': qrow['id'], 'source_id': sid,
                    'language': group[0]['language'], 'configs': {}}
            for c in qrow['configs']:
                t = c['targets'][0]
                item['configs'][c['config']['name']] = {
                    'file_rank': t['file_rank'],
                    'file_rank10': t['file_rank10'],
                    'line_rank': t['line_rank'],
                    'in_union': t['in_union'],
                    'fused_rank': t['fused_rank'],
                    'pool_size': c['pool_size'],
                }
            all_rows.append(item)
        print(f'[{n}/{len(repos)}] {sid} {len(data)} queries', flush=True)

    out = run / 'rows.jsonl'
    out.write_text(''.join(json.dumps(r) + '\n' for r in all_rows), encoding='utf-8')
    summary = {}
    for cfg in configs:
        name = cfg['name']
        ranks = [r['configs'][name]['file_rank10'] for r in all_rows]
        line_ranks = [r['configs'][name]['line_rank'] for r in all_rows]
        ranks = [x if x else 0 for x in ranks]
        line_ranks = [x if x else 0 for x in line_ranks]
        nq = len(ranks)
        def hit(rs, k):
            return sum(1 for x in rs if 0 < x <= k)
        summary[name] = {
            'n': nq,
            'hit@1': hit(ranks, 1),
            'hit@5': hit(ranks, 5),
            'hit@10': hit(ranks, 10),
            'hit@1_rate': hit(ranks, 1) / max(nq, 1),
            'hit@5_rate': hit(ranks, 5) / max(nq, 1),
            'hit@10_rate': hit(ranks, 10) / max(nq, 1),
            'mrr@10': sum(1.0 / x for x in ranks if 0 < x <= 10) / max(nq, 1),
            'line_hit@1': hit(line_ranks, 1),
            'line_hit@5': hit(line_ranks, 5),
            'line_hit@10': hit(line_ranks, 10),
            'union_misses': sum(1 for r in all_rows if not r['configs'][name]['in_union']),
            'config': cfg,
        }
    out_json = run / 'summary.json'
    out_json.write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    for name, s in summary.items():
        print(f"{name:18s} H@1={s['hit@1_rate']*100:5.1f} H@5={s['hit@5_rate']*100:5.1f} "
              f"H@10={s['hit@10_rate']*100:5.1f} MRR={s['mrr@10']:.4f} "
              f"line@5={s['line_hit@5']} unionmiss={s['union_misses']}")
    print(f'elapsed {time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
