#!/usr/bin/env python3
"""CoSQA confirmation for the definition-boost study.

Uses the corpus materialized by WG_QUALITY_COSQA.py. Report-only: the config
list is the one predeclared after RepoQA-dev selection. Run on the VPS.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from pathlib import Path

PUB = Path('/srv/projects/wispd/sgrep-published-inputs/CoCLR')
QPATH = PUB / 'data/search/cosqa-retrieval-test-500.json'
TOOL = Path('/srv/projects/wispd/wg-defboost-20261024/target/release/wg-quality-tool')
CORPUS = Path('/srv/projects/wispd/wg-quality-20261020/cosqa/corpus')


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default=str(TOOL))
    ap.add_argument('--run', type=Path, default=Path('/srv/projects/wispd/wg-defboost-20261024/results/cosqa'))
    ap.add_argument('--corpus', type=Path, default=CORPUS)
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--split', choices=['dev', 'heldout'], required=True)
    ap.add_argument('--top', type=int, default=100)
    ap.add_argument('--profile', action='store_true')
    args = ap.parse_args()

    run = args.run.resolve() / args.split
    run.mkdir(parents=True, exist_ok=True)
    configs = json.loads(args.configs.read_text(encoding='utf-8'))
    queries = json.loads(QPATH.read_text(encoding='utf-8'))
    queries = queries[:250] if args.split == 'dev' else queries[250:]
    payload = [{'id': q['idx'], 'query': q['doc'], 'terms': [],
                'targets': [{'path': f"{q['retrieval_idx']:04d}.py", 'line': 1}]}
               for q in queries]
    qpath = run / 'queries.json'
    qpath.write_text(json.dumps(payload), encoding='utf-8')
    outp = run / 'tool.json'
    argv = [args.tool, '--root', str(args.corpus), '--queries', str(qpath),
            '--out', str(outp), '--top', str(args.top)]
    for cfg in configs:
        argv += ['--config', json.dumps(cfg)]
    if args.profile:
        argv.append('--profile')
    env = dict(os.environ, HF_HUB_OFFLINE='1', RAYON_NUM_THREADS='2')
    env.pop('RIPGREP_CONFIG_PATH', None)
    started = time.time()
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=env, timeout=3600)
    if p.returncode != 0:
        raise SystemExit(p.stderr.decode()[-3000:])
    if args.profile:
        lines = [ln for ln in p.stderr.decode().splitlines()
                 if ln.startswith('def-index') or ln.startswith('profile:')
                 or ln.startswith('defn-stats')]
        (run / 'profile.log').write_text('\n'.join(lines) + '\n', encoding='utf-8')
    data = json.loads(outp.read_text(encoding='utf-8'))
    all_rows = []
    for q in data:
        item = {'id': q['id'], 'query': q['query'], 'configs': {}}
        for c in q['configs']:
            t = c['targets'][0]
            item['configs'][c['config']['name']] = {
                'file_rank': t['file_rank'],
                'file_rank10': t['file_rank10'],
                'line_rank': t['line_rank'],
                'in_union': t['in_union'],
                'fused_rank': t['fused_rank'],
                'pool_size': c['pool_size'],
                'target_defn': t['chunk'].get('defn_score'),
            }
        all_rows.append(item)
    (run / 'rows.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in all_rows), encoding='utf-8')

    base_rank = {r['id']: r['configs']['base']['file_rank10'] or 0 for r in all_rows}
    summary = {}
    for cfg in configs:
        name = cfg['name']
        ranks = [r['configs'][name]['file_rank10'] or 0 for r in all_rows]
        n = len(ranks)
        h = lambda k: sum(1 for x in ranks if 0 < x <= k)
        wins = losses = 0
        for r in all_rows:
            a = r['configs'][name]['file_rank10'] or 0
            b = r['configs']['base']['file_rank10'] or 0
            if a and (not b or a < b):
                wins += 1
            elif b and (not a or b < a):
                losses += 1
        mrr = sum(1.0 / x for x in ranks if 0 < x <= 10) / max(n, 1)
        base_mrr = sum(1.0 / x for x in base_rank.values() if 0 < x <= 10) / max(n, 1)
        summary[name] = {
            'n': n,
            'hit@1': h(1), 'hit@5': h(5), 'hit@10': h(10),
            'hit@1_rate': h(1) / max(n, 1),
            'hit@5_rate': h(5) / max(n, 1),
            'hit@10_rate': h(10) / max(n, 1),
            'mrr@10': mrr,
            'delta_hit@1': h(1) - sum(1 for v in base_rank.values() if 0 < v <= 1),
            'delta_mrr': mrr - base_mrr,
            'paired_file_hit_wins_vs_base': wins,
            'paired_file_hit_losses_vs_base': losses,
            'config': cfg,
        }
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    for name, s in summary.items():
        print(f"{name:20s} H@1={s['hit@1_rate']*100:5.1f} H@5={s['hit@5_rate']*100:5.1f} "
              f"H@10={s['hit@10_rate']*100:5.1f} MRR={s['mrr@10']:.5f} "
              f"dH1={s['delta_hit@1']:+d} dMRR={s['delta_mrr']:+.4f} "
              f"W/L={s['paired_file_hit_wins_vs_base']}/{s['paired_file_hit_losses_vs_base']}")
    print(f'elapsed {time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
