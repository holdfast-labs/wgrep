#!/usr/bin/env python3
"""Acceptance check: wg CLI vs the analysis tool's `no sat` rankings.

The reference rows are the recorded tool outputs under
`/srv/projects/wispd/wg-rank3-20261024/results/nosat-repoqa` and
`.../nosat-semble`. This script runs the wg CLI, compares the ordered rows and
target ranks, and reports identical/differing query counts and held-out metrics.

Run on the VPS; each run needs disk under the run directory and starts one wg
service per repo (all stopped with `wg down` in a finally block).
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

REPOQA_QUERIES = Path(
    '/srv/projects/wispd/sgrep-published-inputs/repoqa-import/repoqa/queries.json'
)
REPOQA_RUN = Path('/srv/projects/wispd/wg-rank3-20261024/results/nosat-repoqa/heldout')
SEMBLE_RUN = Path('/srv/projects/wispd/wg-rank3-20261024/results/nosat-semble')
SEMBLE_REPO = Path(
    os.environ.get('SEMBLE_REPO', '/srv/projects/wispd/wg-semble-20261024/semble-repo')
)
sys.path.insert(0, str(SEMBLE_REPO))
from benchmarks.data import grouped_tasks, load_filtered_tasks, target_matches_location  # noqa: E402
from benchmarks.metrics import ndcg_at_k  # noqa: E402


def call(argv, *, env, timeout=900):
    return subprocess.run(
        argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=timeout
    )


def rows_of(out: bytes):
    parsed = json.loads(out) if out.strip() else []
    return parsed if isinstance(parsed, list) else []


def unique_file_rank(rows, path):
    seen = []
    for row in rows:
        p = row.get('path')
        if p not in seen:
            seen.append(p)
    try:
        return seen.index(path) + 1
    except ValueError:
        return None


def line_rank(rows, path, line):
    for i, row in enumerate(rows, 1):
        if (
            row.get('path') == path
            and int(row.get('start', 0)) <= line <= int(row.get('end', -1))
        ):
            return i
    return None


def relevant_ranks(rows, task):
    ranks = []
    for target in task.all_relevant:
        rank = None
        for i, row in enumerate(rows, 1):
            if target_matches_location(
                row.get('path'), int(row.get('start', -1)), int(row.get('end', -2)), target
            ):
                rank = i
                break
        if rank is not None:
            ranks.append(rank)
    return ranks


class Wg:
    def __init__(self, binary: Path, run: Path, mode: str, top: int):
        self.binary = str(binary)
        self.run = run
        self.mode = mode
        self.top = top
        self.base_env = dict(os.environ)
        self.base_env['HF_HUB_OFFLINE'] = '1'
        self.base_env['RAYON_NUM_THREADS'] = '4'
        self.base_env.pop('RIPGREP_CONFIG_PATH', None)

    def query_repo(self, repo: str, root: Path, queries: list[dict]):
        service = self.run / 'services' / repo
        env = dict(self.base_env, WG_SERVICE_DIR=str(service))
        up = call(
            [self.binary, 'up', str(root), '--model', 'code', '--json'],
            env=env,
            timeout=1800,
        )
        if up.returncode != 0:
            raise SystemExit(f'wg up failed {repo}: {up.stderr.decode()[-1200:]}')
        out_rows = []
        try:
            for q in queries:
                argv = [
                    self.binary,
                    q['query'],
                    str(root),
                    '--model',
                    'code',
                    '--json',
                    '--top',
                    str(self.top),
                ]
                if self.mode == 'classic':
                    argv += ['--rank', 'classic']
                p = call(argv, env=env, timeout=300)
                if p.returncode != 0:
                    out_rows.append({'error': f'exit {p.returncode}', 'stderr': p.stderr.decode()[-400:]})
                else:
                    out_rows.append({'rows': rows_of(p.stdout)})
        finally:
            call([self.binary, 'down'], env=env, timeout=120)
        return out_rows


def repoqa(ap, wg: Wg):
    original = json.loads(REPOQA_QUERIES.read_text(encoding='utf-8'))
    roots = {q['source_id']: q['root'] for q in original}
    files = sorted(REPOQA_RUN.glob('*.queries.json'))
    if ap.only_repo:
        files = [p for p in files if ap.only_repo in p.name]
    if ap.limit_repos:
        files = files[: ap.limit_repos]
    per_query = []
    for n, qpath in enumerate(files, 1):
        sid = qpath.name[: -len('.queries.json')]
        queries = json.loads(qpath.read_text(encoding='utf-8'))
        tool = json.loads((REPOQA_RUN / f'{sid}.tool.json').read_text(encoding='utf-8'))
        cfg_name = 'base' if wg.mode == 'classic' else 'nosat'
        recorded = {}
        for qrow in tool:
            cfg = next(c for c in qrow['configs'] if c['config']['name'] == cfg_name)
            recorded[qrow['id']] = cfg['targets']
        got = wg.query_repo(sid, Path(roots[sid]), queries)
        differing = []
        for q, result in zip(queries, got):
            if 'error' in result:
                differing.append((q['id'], result['error']))
                per_query.append({'id': q['id'], 'source_id': sid, 'error': result['error']})
                continue
            rows = result['rows']
            targets = recorded[q['id']]
            ok = True
            detail = []
            for target, rec in zip(q['targets'], targets):
                fr = unique_file_rank(rows, target['path'])
                lr = line_rank(rows, target['path'], target['line'])
                if fr != rec.get('file_rank') or lr != rec.get('line_rank'):
                    ok = False
                    detail.append(
                        {
                            'path': target['path'],
                            'line': target['line'],
                            'wg_file': fr,
                            'ref_file': rec.get('file_rank'),
                            'wg_line': lr,
                            'ref_line': rec.get('line_rank'),
                        }
                    )
            if not ok:
                differing.append((q['id'], detail))
            per_query.append(
                {
                    'id': q['id'],
                    'source_id': sid,
                    'file_rank': unique_file_rank(rows, q['targets'][0]['path']),
                    'line_rank': line_rank(rows, q['targets'][0]['path'], q['targets'][0]['line']),
                    'same_as_ref': ok,
                }
            )
        print(
            f'[{n}/{len(files)}] repoqa {sid} queries={len(queries)} differing={len(differing)}',
            flush=True,
        )
        for item in differing:
            print(f'  DIFF {item}', flush=True)

    ranks = [r['file_rank'] or 0 for r in per_query if 'error' not in r]
    line_ranks = [r['line_rank'] or 0 for r in per_query if 'error' not in r]
    nq = len(ranks)

    def hit(rs, k):
        return sum(1 for x in rs if 0 < x <= k)

    summary = {
        'mode': wg.mode,
        'queries': nq,
        'differing_queries': sum(1 for r in per_query if not r.get('same_as_ref', False)),
        'errors': sum(1 for r in per_query if 'error' in r),
        'file_hit@1': hit(ranks, 1),
        'file_hit@5': hit(ranks, 5),
        'file_hit@10': hit(ranks, 10),
        'mrr@10': sum(1.0 / x for x in ranks if 0 < x <= 10) / max(nq, 1),
        'line_hit@1': hit(line_ranks, 1),
        'line_hit@5': hit(line_ranks, 5),
        'line_hit@10': hit(line_ranks, 10),
    }
    print('repoqa', json.dumps(summary, indent=2), flush=True)
    return summary


def semble(ap, wg: Wg):
    files = sorted(SEMBLE_RUN.glob('*.tool.json'))
    if ap.only_repo:
        files = [p for p in files if ap.only_repo in p.name]
    if ap.limit_repos:
        files = files[: ap.limit_repos]
    specs, tasks = load_filtered_tasks()
    by_repo = grouped_tasks(tasks)
    per_query = []
    reps = []
    for n, tpath in enumerate(files, 1):
        repo = tpath.name[: -len('.tool.json')]
        if repo not in by_repo:
            print(f'  skip {repo}: not in Semble loader', flush=True)
            continue
        task_list = by_repo[repo]
        queries = json.loads((SEMBLE_RUN / f'{repo}.queries.json').read_text(encoding='utf-8'))
        tool = json.loads(tpath.read_text(encoding='utf-8'))
        cfg_name = 'base' if wg.mode == 'classic' else 'nosat'
        got = wg.query_repo(repo, specs[repo].benchmark_dir, queries)
        same = 0
        repo_scores = []
        ref_scores = []
        for task, q, result in zip(task_list, queries, got):
            if 'error' in result:
                print(f'  ERROR {repo} {q["id"]}: {result["error"]}', flush=True)
                continue
            rows = result['rows']
            qrow = next(r for r in tool if r['id'] == q['id'])
            cfg = next(c for c in qrow['configs'] if c['config']['name'] == cfg_name)
            ref_rows = cfg.get('rows') or []
            ref_key = [(r.get('path'), r.get('start'), r.get('end')) for r in ref_rows]
            got_key = [(r.get('path'), r.get('start'), r.get('end')) for r in rows]
            identical = ref_key == got_key
            same += identical
            ranks = relevant_ranks(rows, task)
            ref_ranks = relevant_ranks(ref_rows, task)
            score = ndcg_at_k(ranks, len(task.all_relevant), 10)
            ref_score = ndcg_at_k(ref_ranks, len(task.all_relevant), 10)
            repo_scores.append(score)
            ref_scores.append(ref_score)
            per_query.append(
                {
                    'repo': repo,
                    'id': q['id'],
                    'query': q['query'],
                    'category': task.category,
                    'ndcg': score,
                    'ref_ndcg': ref_score,
                    'identical_rows': identical,
                    'wg_ranks': ranks,
                    'ref_ranks': ref_ranks,
                    'first_diff': None
                    if identical
                    else next(
                        (i, a, b) for i, (a, b) in enumerate(zip(got_key, ref_key)) if a != b
                    ),
                }
            )
        rep = sum(repo_scores) / len(repo_scores) if repo_scores else 0.0
        ref_rep = sum(ref_scores) / len(ref_scores) if ref_scores else 0.0
        reps.append({'repo': repo, 'ndcg': rep, 'ref_ndcg': ref_rep, 'tasks': len(repo_scores)})
        print(
            f'[{n}/{len(files)}] semble {repo} tasks={len(repo_scores)} '
            f'rows_identical={same}/{len(repo_scores)} ndcg={rep:.4f} ref={ref_rep:.4f}',
            flush=True,
        )
        for item in per_query[-len(repo_scores) :]:
            if not item['identical_rows']:
                print(
                    f'  DIFF {item["id"]} first={item["first_diff"]} '
                    f'ndcg={item["ndcg"]:.4f} ref={item["ref_ndcg"]:.4f}',
                    flush=True,
                )
    macro = sum(r['ndcg'] for r in reps) / max(len(reps), 1)
    ref_macro = sum(r['ref_ndcg'] for r in reps) / max(len(reps), 1)
    summary = {
        'mode': wg.mode,
        'repos': len(reps),
        'queries': len(per_query),
        'identical_row_queries': sum(1 for r in per_query if r['identical_rows']),
        'differing_row_queries': sum(1 for r in per_query if not r['identical_rows']),
        'macro_ndcg@10': macro,
        'ref_macro_ndcg@10': ref_macro,
        'by_category': {
            cat: {
                'n': len(v),
                'ndcg': sum(x['ndcg'] for x in v) / len(v),
                'ref_ndcg': sum(x['ref_ndcg'] for x in v) / len(v),
            }
            for cat, v in sorted(
                (c, [r for r in per_query if r['category'] == c])
                for c in {r['category'] for r in per_query}
            )
        },
    }
    print('semble', json.dumps(summary, indent=2), flush=True)
    return summary


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--wg', default='/srv/projects/wispd/wg-rankimpl-20261024/target/release/wg')
    ap.add_argument('--run', type=Path, default=Path('/srv/projects/wispd/wg-rankimpl-20261024/equiv'))
    ap.add_argument('--which', choices=['repoqa', 'semble', 'both'], default='both')
    ap.add_argument('--mode', choices=['semantic', 'classic'], default='semantic')
    ap.add_argument('--top', type=int, default=None)
    ap.add_argument('--only-repo', default=None)
    ap.add_argument('--limit-repos', type=int, default=0)
    ap.add_argument('--fresh', action='store_true', help='delete the run directory first')
    args = ap.parse_args()

    if args.run.exists():
        if args.fresh:
            shutil.rmtree(args.run)
        else:
            raise SystemExit(f'run dir exists: {args.run} (pass --fresh)')
    args.run.mkdir(parents=True)
    wg = Wg(
        Path(args.wg),
        args.run,
        args.mode,
        args.top or 10,
    )
    started = time.time()
    out = {}
    if args.which in ('repoqa', 'both'):
        wg.top = args.top or 100
        out['repoqa'] = repoqa(args, wg)
    if args.which in ('semble', 'both'):
        wg.top = args.top or 10
        out['semble'] = semble(args, wg)
    out['elapsed_seconds'] = round(time.time() - started, 2)
    (args.run / f'summary-{args.mode}.json').write_text(
        json.dumps(out, indent=2) + '\n', encoding='utf-8'
    )
    print(json.dumps(out, indent=2))


if __name__ == '__main__':
    main()
