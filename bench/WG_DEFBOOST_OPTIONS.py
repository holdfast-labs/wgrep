#!/usr/bin/env python3
"""RepoQA dev/held-out option sweep for the definition-boost study.

Mirrors WG_QUALITY_OPTIONS.py but points at the WG_DEFBOOST tool and writes
per-query paired statistics against the baseline config. Tuning uses the dev
split only. Run on the VPS.
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
TOOL = Path('/srv/projects/wispd/wg-defboost-20261024/target/release/wg-quality-tool')


def split_of(index: int) -> str:
    return 'dev' if index < 5 else 'heldout'


def summarize(all_rows, configs, base='base'):
    summary = {}
    base_rank = {r['id']: r['configs'][base]['file_rank10'] or 0 for r in all_rows}
    for cfg in configs:
        name = cfg['name']
        ranks = [r['configs'][name]['file_rank10'] or 0 for r in all_rows]
        line_ranks = [r['configs'][name]['line_rank'] or 0 for r in all_rows]
        nq = len(ranks)
        def hit(rs, k):
            return sum(1 for x in rs if 0 < x <= k)
        wins = losses = 0
        for r in all_rows:
            a = r['configs'][name]['file_rank10'] or 0
            b = r['configs'][base]['file_rank10'] or 0
            if a and (not b or a < b):
                wins += 1
            elif b and (not a or b < a):
                losses += 1
        mrr = sum(1.0 / x for x in ranks if 0 < x <= 10) / max(nq, 1)
        base_mrr = sum(1.0 / x for x in base_rank.values() if 0 < x <= 10) / max(nq, 1)
        summary[name] = {
            'n': nq,
            'hit@1': hit(ranks, 1),
            'hit@5': hit(ranks, 5),
            'hit@10': hit(ranks, 10),
            'hit@1_rate': hit(ranks, 1) / max(nq, 1),
            'hit@5_rate': hit(ranks, 5) / max(nq, 1),
            'hit@10_rate': hit(ranks, 10) / max(nq, 1),
            'mrr@10': mrr,
            'delta_hit@1': hit(ranks, 1) - hit([base_rank[r['id']] for r in all_rows], 1),
            'delta_mrr': mrr - base_mrr,
            'line_hit@1': hit(line_ranks, 1),
            'line_hit@5': hit(line_ranks, 5),
            'line_hit@10': hit(line_ranks, 10),
            'union_misses': sum(1 for r in all_rows if not r['configs'][name]['in_union']),
            'paired_file_hit_wins_vs_base': wins,
            'paired_file_hit_losses_vs_base': losses,
            'config': cfg,
        }
    return summary


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default=str(TOOL))
    ap.add_argument('--run', type=Path, default=Path('/srv/projects/wispd/wg-defboost-20261024/results'))
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--split', choices=['dev', 'heldout'], required=True)
    ap.add_argument('--top', type=int, default=100)
    ap.add_argument('--only-repo', default=None)
    ap.add_argument('--profile', action='store_true')
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
            if split_of(i) == args.split:
                repos.append(sid)
    repos.sort()
    if args.only_repo:
        repos = [r for r in repos if args.only_repo in r]

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
        if args.profile:
            argv.append('--profile')
        p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           env=env, timeout=3600)
        if p.returncode != 0:
            raise SystemExit(f'tool failed {sid}: {p.stderr.decode()[-2000:]}')
        if args.profile:
            lines = [ln for ln in p.stderr.decode().splitlines()
                     if ln.startswith('def-index') or ln.startswith('profile:')
                     or ln.startswith('defn-stats')]
            with (run / 'profile.log').open('a', encoding='utf-8') as f:
                for ln in lines:
                    f.write(f'{sid} {ln}\n')
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
                    'target_defn': t['chunk'].get('defn_score'),
                }
            all_rows.append(item)
        print(f'[{n}/{len(repos)}] {sid} {len(data)} queries', flush=True)

    out = run / 'rows.jsonl'
    out.write_text(''.join(json.dumps(r) + '\n' for r in all_rows), encoding='utf-8')
    summary = summarize(all_rows, configs)
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    for name, s in summary.items():
        print(f"{name:22s} H@1={s['hit@1_rate']*100:5.1f} H@5={s['hit@5_rate']*100:5.1f} "
              f"H@10={s['hit@10_rate']*100:5.1f} MRR={s['mrr@10']:.5f} "
              f"dH1={s['delta_hit@1']:+d} dMRR={s['delta_mrr']:+.4f} "
              f"W/L={s['paired_file_hit_wins_vs_base']}/{s['paired_file_hit_losses_vs_base']} "
              f"line@5={s['line_hit@5']} unionmiss={s['union_misses']}")
    print(f'elapsed {time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
