#!/usr/bin/env python3
"""wg on Semble's own published benchmark (63 repos, ~1,250 queries, NDCG@10).

Uses Semble's own task loader, annotation files and metric functions so the
queries, relevance spans, target matching and NDCG@10 formula are identical to
`python -m benchmarks.run_benchmark`. wg runs with its CLI defaults
(`--model code --json --top 10`).

Run on the VPS from the Semble repo root (or set SEMBLE_REPO).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

SEMBLE_REPO = Path(os.environ.get('SEMBLE_REPO', '/srv/projects/wispd/wg-semble-20261024/semble-repo'))
BASE = Path('/srv/projects/wispd/wg-semble-20261024')
sys.path.insert(0, str(SEMBLE_REPO))

from benchmarks.data import (  # noqa: E402
    add_filter_args,
    grouped_tasks,
    load_filtered_tasks,
    target_matches_location,
)
from benchmarks.metrics import ndcg_at_k  # noqa: E402


def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def call(argv, *, env=None, timeout=1800):
    started = time.perf_counter()
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=env, timeout=timeout)
    return p.returncode, p.stdout, p.stderr, time.perf_counter() - started


def relevant_ranks(rows, task):
    """1-based rank of the first wg row covering each relevant target."""
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


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    add_filter_args(ap, verbose=True)
    ap.add_argument('--wg', default=str(BASE / 'bin/wg'))
    ap.add_argument('--run', type=Path, default=BASE / 'arm-b/wg')
    ap.add_argument('--top', type=int, default=10)
    args = ap.parse_args()

    wg = str(Path(args.wg).resolve())
    run = args.run.resolve()
    if run.exists():
        raise SystemExit(f'run dir exists: {run}')
    run.mkdir(parents=True)
    services = run / 'services'

    repo_specs, tasks = load_filtered_tasks(args.repo or None, args.language or None)
    by_repo = grouped_tasks(tasks)
    print(f'repos={len(by_repo)} tasks={len(tasks)} '
          f'annotations={sum(1 for _ in (SEMBLE_REPO / "benchmarks/annotations").glob("*.json"))}', flush=True)

    base_env = dict(os.environ)
    base_env['HF_HUB_OFFLINE'] = '1'
    base_env['RAYON_NUM_THREADS'] = '2'
    base_env.pop('RIPGREP_CONFIG_PATH', None)

    per_repo = []
    per_query = []
    started = time.time()
    for n, repo in enumerate(sorted(by_repo), 1):
        spec = repo_specs[repo]
        root = spec.benchmark_dir
        if not root.is_dir():
            raise SystemExit(f'missing repo root: {root}')
        service = services / repo
        env = dict(base_env, WG_SERVICE_DIR=str(service))
        rc, out, err, up_s = call([wg, 'up', str(root), '--model', 'code', '--json'],
                                  env=env, timeout=1800)
        if rc != 0:
            raise SystemExit(f'wg up failed {repo}: {err.decode()[-1200:]}')
        print(f'[{n}/{len(by_repo)}] {repo} up {up_s:.2f}s tasks={len(by_repo[repo])}', flush=True)
        repo_rows = []
        try:
            for task in by_repo[repo]:
                rc, out, err, secs = call(
                    [wg, task.query, str(root), '--model', 'code', '--json',
                     '--top', str(args.top)], env=env, timeout=300)
                error = None if rc == 0 else f'exit {rc}'
                try:
                    parsed = json.loads(out) if rc == 0 and out.strip() else None
                except json.JSONDecodeError as exc:
                    parsed, error = None, f'json: {exc}'
                rows = parsed if isinstance(parsed, list) else []
                ranks = relevant_ranks(rows, task) if not error else []
                n_relevant = len(task.all_relevant)
                q_ndcg10 = ndcg_at_k(ranks, n_relevant, 10) if not error else 0.0
                rec = {
                    'repo': repo,
                    'language': spec.language,
                    'category': task.category,
                    'query': task.query,
                    'n_relevant': n_relevant,
                    'relevant_ranks': ranks,
                    'ndcg@10': q_ndcg10,
                    'rows': len(rows),
                    'seconds': round(secs, 5),
                    'error': error,
                }
                per_query.append(rec)
                repo_rows.append(rec)
                if args.verbose:
                    print(f'  {q_ndcg10:.3f} ranks={ranks} nrel={n_relevant} '
                          f'rows={len(rows)} {task.query!r}', flush=True)
        finally:
            call([wg, 'down'], env=env, timeout=120)
        ndcg10 = sum(r['ndcg@10'] for r in repo_rows) / len(repo_rows)
        per_repo.append({
            'repo': repo,
            'language': spec.language,
            'tasks': len(repo_rows),
            'ndcg@10': ndcg10,
            'up_seconds': round(up_s, 3),
            'errors': sum(1 for r in repo_rows if r['error']),
        })
        print(f'  -> {repo} ndcg@10={ndcg10:.4f}', flush=True)

    nq = len(per_query)
    n_repos = len(per_repo)
    macro = sum(r['ndcg@10'] for r in per_repo) / n_repos
    by_lang = defaultdict(list)
    for r in per_repo:
        by_lang[r['language']].append(r)
    by_category = defaultdict(list)
    for r in per_query:
        by_category[r['category']].append(r['ndcg@10'])

    summary = {
        'arm': 'B-semblem-bench',
        'tool': 'wg',
        'wg': wg,
        'wg_sha256': sha(Path(wg)),
        'wg_commit': '7ffb6242d4151d29d45915803729684de4fd5649',
        'config': f'wg defaults, --model code --json --top {args.top}',
        'repos': n_repos,
        'queries': nq,
        'aggregation': 'per-repo mean NDCG@10, then unweighted mean over repos (same as Semble run_benchmark)',
        'ndcg@10_macro': macro,
        'ndcg@10_query_weighted': sum(r['ndcg@10'] for r in per_query) / nq,
        'errors': sum(1 for r in per_query if r['error']),
        'by_language': {
            lang: {
                'repos': len(rs),
                'ndcg@10': sum(r['ndcg@10'] for r in rs) / len(rs),
            } for lang, rs in sorted(by_lang.items())
        },
        'by_category': {
            cat: {'n': len(v), 'ndcg@10': sum(v) / len(v)}
            for cat, v in sorted(by_category.items())
        },
        'per_repo': per_repo,
        'elapsed_seconds': round(time.time() - started, 2),
    }
    (run / 'per-query.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in per_query), encoding='utf-8')
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in summary.items()
                      if k not in ('per_repo', 'by_language', 'by_category')}, indent=2))
    print(json.dumps({'by_language': summary['by_language'],
                      'by_category': summary['by_category']}, indent=2))


if __name__ == '__main__':
    main()
