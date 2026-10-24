#!/usr/bin/env python3
"""NDCG@10 runner for the Semble benchmark, using Semble's own metric.

Loads tasks with `benchmarks.data`, scores with `benchmarks.metrics.ndcg_at_k`
and `target_matches_location`, aggregates per repo then macro over repos. This
is the same rule as `bench/WG_SEMBLE_THEIRSET.py`, but ranks with the
WG_QUALITY_TOOL re-scorer instead of the wg CLI.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

SEMBLE_REPO = Path(os.environ.get('SEMBLE_REPO', '/srv/projects/wispd/wg-semble-20261024/semble-repo'))
sys.path.insert(0, str(SEMBLE_REPO))
from benchmarks.data import grouped_tasks, load_filtered_tasks, target_matches_location  # noqa: E402
from benchmarks.metrics import ndcg_at_k  # noqa: E402

TOOL = Path('/srv/projects/wispd/wg-rank2-20261024/target/release/wg-quality-tool')


def relevant_ranks(rows, task):
    """1-based rank of the first row covering each relevant target."""
    ranks = []
    for target in task.all_relevant:
        rank = None
        for i, row in enumerate(rows, 1):
            path = row.get('path')
            start = row.get('start')
            end = row.get('end')
            if path is None or start is None or end is None:
                continue
            if target_matches_location(path, int(start), int(end), target):
                rank = i
                break
        if rank is not None:
            ranks.append(rank)
    return ranks


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default=str(TOOL))
    ap.add_argument('--split', choices=['dev', 'heldout'], required=True)
    ap.add_argument('--split-file', type=Path,
                    default=Path('/srv/projects/wispd/wg-rank2-20261024/split.json'))
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--run', type=Path, required=True)
    ap.add_argument('--top', type=int, default=10)
    ap.add_argument('--profile', action='store_true')
    ap.add_argument('--only-repo', default=None)
    args = ap.parse_args()

    run = args.run.resolve()
    run.mkdir(parents=True, exist_ok=True)
    configs = json.loads(args.configs.read_text(encoding='utf-8'))
    split = json.loads(args.split_file.read_text(encoding='utf-8'))
    wanted = {n for names in split[args.split].values() for n in names}
    if args.only_repo:
        wanted = {n for n in wanted if args.only_repo in n}
    specs, tasks = load_filtered_tasks()
    by_repo = {name: group for name, group in grouped_tasks(tasks).items() if name in wanted}
    if not by_repo:
        raise SystemExit('no repos in split')

    env = dict(os.environ, HF_HUB_OFFLINE='1', RAYON_NUM_THREADS='4')
    env.pop('RIPGREP_CONFIG_PATH', None)
    per_query = []
    per_repo = []
    started = time.time()
    for n, repo in enumerate(sorted(by_repo), 1):
        spec = specs[repo]
        root = spec.benchmark_dir
        if not root.is_dir():
            raise SystemExit(f'missing root {repo}: {root}')
        group = by_repo[repo]
        qpath = run / f'{repo}.queries.json'
        qpath.write_text(json.dumps([
            {'id': f'{repo}-{i}', 'query': t.query, 'terms': []}
            for i, t in enumerate(group)
        ]), encoding='utf-8')
        outp = run / f'{repo}.tool.json'
        argv = [args.tool, '--root', str(root), '--queries', str(qpath),
                '--out', str(outp), '--top', str(args.top), '--rows']
        for cfg in configs:
            argv += ['--config', json.dumps(cfg)]
        if args.profile:
            argv.append('--profile')
        t0 = time.time()
        p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           env=env, timeout=3600)
        if p.returncode != 0:
            raise SystemExit(f'tool failed {repo}: {p.stderr.decode()[-2500:]}')
        if args.profile:
            with (run / 'profile.log').open('a', encoding='utf-8') as f:
                f.write(f'{repo} wall={time.time()-t0:.2f}s\n')
                for line in p.stderr.decode().splitlines():
                    if line.startswith(('def-index', 'profile:', 'defn-stats')):
                        f.write(f'{repo} {line}\n')
        data = json.loads(outp.read_text(encoding='utf-8'))
        rows_by_query = {q['id']: q for q in data}
        repo_scores = defaultdict(list)
        for i, task in enumerate(group):
            qid = f'{repo}-{i}'
            qrow = rows_by_query[qid]
            for cfg in configs:
                name = cfg['name']
                c = next(c for c in qrow['configs'] if c['config']['name'] == name)
                rows = c['rows']
                ranks = relevant_ranks(rows, task)
                n_rel = len(task.all_relevant)
                score = ndcg_at_k(ranks, n_rel, 10)
                repo_scores[name].append(score)
                per_query.append({
                    'repo': repo,
                    'language': spec.language,
                    'category': task.category,
                    'query': task.query,
                    'config': name,
                    'n_relevant': n_rel,
                    'relevant_ranks': ranks,
                    'ndcg@10': score,
                    'rows': len(rows),
                    'top_paths': [r.get('path') for r in rows],
                })
        for name in repo_scores:
            per_repo.append({
                'repo': repo,
                'language': spec.language,
                'tasks': len(repo_scores[name]),
                'config': name,
                'ndcg@10': sum(repo_scores[name]) / len(repo_scores[name]),
            })
        print(f'[{n}/{len(by_repo)}] {repo} {len(group)} tasks', flush=True)

    (run / 'per-query.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in per_query), encoding='utf-8')
    # Macro: per-repo mean, then unweighted mean over repos.
    summary = {}
    for cfg in configs:
        name = cfg['name']
        repo_means = [r['ndcg@10'] for r in per_repo if r['config'] == name]
        q_scores = [r['ndcg@10'] for r in per_query if r['config'] == name]
        by_cat = defaultdict(list)
        by_lang = defaultdict(list)
        for r in per_query:
            if r['config'] == name:
                by_cat[r['category']].append(r['ndcg@10'])
                by_lang[r['language']].append(r['ndcg@10'])
        summary[name] = {
            'repos': len(repo_means),
            'queries': len(q_scores),
            'ndcg@10_macro': sum(repo_means) / max(len(repo_means), 1),
            'ndcg@10_query_weighted': sum(q_scores) / max(len(q_scores), 1),
            'by_category': {k: {'n': len(v), 'ndcg@10': sum(v) / len(v)}
                            for k, v in sorted(by_cat.items())},
            'by_language': {k: {'n': len(v), 'ndcg@10': sum(v) / len(v)}
                            for k, v in sorted(by_lang.items())},
            'config': cfg,
        }
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    (run / 'per-repo.json').write_text(json.dumps(per_repo, indent=2) + '\n', encoding='utf-8')
    for name, s in summary.items():
        print(f"{name:16s} macro={s['ndcg@10_macro']:.4f} qweighted={s['ndcg@10_query_weighted']:.4f} "
              f"repos={s['repos']} queries={s['queries']}")
        for cat, v in s['by_category'].items():
            print(f"   {cat:14s} {v['ndcg@10']:.4f} (n={v['n']})")
    print(f'elapsed {time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
