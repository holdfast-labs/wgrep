#!/usr/bin/env python3
"""Option check on CoSQA as a second, independent held-out task.

The config set is the one selected on RepoQA dev. CoSQA is not used to choose
parameters. Run on the VPS after WG_QUALITY_COSQA.py has materialized the
corpus.
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


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default='/srv/projects/wispd/wg-quality-20261020/src/target/release/wg-quality-tool')
    ap.add_argument('--run', type=Path,
                    default=Path('/srv/projects/wispd/wg-quality-20261020/cosqa-options'))
    ap.add_argument('--corpus', type=Path,
                    default=Path('/srv/projects/wispd/wg-quality-20261020/cosqa/corpus'))
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--split', choices=['dev', 'heldout'], required=True)
    ap.add_argument('--top', type=int, default=100)
    args = ap.parse_args()

    run = args.run.resolve() / args.split
    run.mkdir(parents=True, exist_ok=True)
    configs = json.loads(args.configs.read_text(encoding='utf-8'))
    queries = json.loads(QPATH.read_text(encoding='utf-8'))
    if args.split == 'dev':
        queries = queries[:250]
    else:
        queries = queries[250:]
    payload = []
    for q in queries:
        payload.append({
            'id': q['idx'],
            'query': q['doc'],
            'terms': [],
            'targets': [{'path': f"{q['retrieval_idx']:04d}.py", 'line': 1}],
        })
    qpath = run / 'queries.json'
    qpath.write_text(json.dumps(payload), encoding='utf-8')
    outp = run / 'tool.json'
    argv = [args.tool, '--root', str(args.corpus), '--queries', str(qpath),
            '--out', str(outp), '--top', str(args.top)]
    for cfg in configs:
        argv += ['--config', json.dumps(cfg)]
    env = dict(os.environ, HF_HUB_OFFLINE='1', RAYON_NUM_THREADS='2')
    env.pop('RIPGREP_CONFIG_PATH', None)
    started = time.time()
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=env, timeout=3600)
    if p.returncode != 0:
        raise SystemExit(p.stderr.decode()[-2000:])
    data = json.loads(outp.read_text(encoding='utf-8'))
    summary = {}
    for cfg in configs:
        name = cfg['name']
        ranks = []
        for q in data:
            c = next(x for x in q['configs'] if x['config']['name'] == name)
            ranks.append(c['targets'][0]['file_rank10'] or 0)
        n = len(ranks)
        h = lambda k: sum(1 for x in ranks if 0 < x <= k)
        summary[name] = {
            'n': n,
            'hit@1': h(1), 'hit@5': h(5), 'hit@10': h(10),
            'hit@1_rate': h(1) / max(n, 1),
            'hit@5_rate': h(5) / max(n, 1),
            'hit@10_rate': h(10) / max(n, 1),
            'mrr@10': sum(1.0 / x for x in ranks if 0 < x <= 10) / max(n, 1),
            'config': cfg,
        }
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    for name, s in summary.items():
        print(f"{name:16s} H@1={s['hit@1_rate']*100:5.1f} H@5={s['hit@5_rate']*100:5.1f} "
              f"H@10={s['hit@10_rate']*100:5.1f} MRR={s['mrr@10']:.5f}")
    print(f'elapsed {time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
