#!/usr/bin/env python3
"""RepoQA 600-query evaluation for the current wg binary.

Reuses the archived imported query set and corpus. Scores distinct-file
Hit@1/5/10 and MRR@10 exactly as the archived comparison does. Raw stdout,
stderr and per-query rows are retained. Run on the VPS.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import time
from collections import defaultdict
from pathlib import Path

IMPORT = Path('/srv/projects/wispd/sgrep-published-inputs/repoqa-import')
QUERY_PATH = IMPORT / 'repoqa/queries.json'
QUERY_SHA = 'a3b5fe78601fe590ff293e3c190ab8bd7f63f3407af46702c8b4dbfe6811774c'
DEFAULT_RUN = Path('/srv/projects/wispd/wg-quality-20261020/repoqa')


def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def call(argv, *, env=None, timeout=900):
    started = time.perf_counter()
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=env, timeout=timeout)
    return p.returncode, p.stdout, p.stderr, time.perf_counter() - started


def unique_rank(rows, target, limit=10):
    seen = []
    for row in rows:
        path = row.get('path')
        if path not in seen:
            seen.append(path)
    try:
        rank = seen.index(target) + 1
    except ValueError:
        return 0
    return rank if rank <= limit else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--wg', default='/srv/projects/wispd/wg-quality-20261020/src/target/release/wg')
    ap.add_argument('--run', type=Path, default=DEFAULT_RUN)
    ap.add_argument('--top', type=int, default=100)
    ap.add_argument('--limit', type=int, default=0, help='debug: first N queries only')
    ap.add_argument('--only-language', default=None)
    ap.add_argument('--only-split', choices=['dev', 'heldout'], default=None,
                    help='repo-level split: first 5 repos per language are dev')
    args = ap.parse_args()

    wg = str(Path(args.wg).resolve())
    run = args.run.resolve()
    if run.exists():
        raise SystemExit(f'run dir exists: {run}')
    raw = run / 'raw'
    raw.mkdir(parents=True)

    assert QUERY_PATH.is_file(), QUERY_PATH
    assert sha(QUERY_PATH) == QUERY_SHA, 'RepoQA query hash mismatch'
    queries = json.loads(QUERY_PATH.read_text(encoding='utf-8'))
    assert len(queries) == 600, len(queries)

    by_repo = defaultdict(list)
    for q in queries:
        by_repo[q['source_id']].append(q)
    repos = sorted(by_repo)
    per_language = defaultdict(list)
    for sid in repos:
        per_language[by_repo[sid][0]['language']].append(sid)
    split = {}
    for lang, sids in per_language.items():
        for i, sid in enumerate(sorted(sids)):
            split[sid] = 'dev' if i < 5 else 'heldout'
    if args.only_split:
        repos = [sid for sid in repos if split[sid] == args.only_split]
    if args.only_language:
        repos = [sid for sid in repos if by_repo[sid][0]['language'] == args.only_language]
    if args.limit:
        repos = repos[:args.limit]

    base_env = dict(os.environ)
    base_env['HF_HUB_OFFLINE'] = '1'
    base_env['RAYON_NUM_THREADS'] = '2'
    base_env.pop('RIPGREP_CONFIG_PATH', None)
    rows_out = []
    started = time.time()
    for n, sid in enumerate(repos, 1):
        group = by_repo[sid]
        root = Path(group[0]['root'])
        if not root.is_dir():
            raise SystemExit(f'missing repo root: {root}')
        service = run / 'service' / sid
        env = dict(base_env, WG_SERVICE_DIR=str(service))
        rc, out, err, secs = call([wg, 'up', str(root), '--model', 'code', '--json'],
                                  env=env, timeout=900)
        if rc != 0:
            raise SystemExit(f'up failed {sid}: {err.decode()[-800:]}')
        print(f'[{n}/{len(repos)}] {sid} up {secs:.2f}s', flush=True)
        try:
            for q in group:
                qid = q['id']
                rc, out, err, secs = call(
                    [wg, q['query'], str(root), '--model', 'code', '--json',
                     '--explain', '--top', str(args.top)], env=env, timeout=300)
                (raw / f'{qid}.stdout').write_bytes(out)
                (raw / f'{qid}.stderr').write_bytes(err)
                parsed = None
                error = None
                try:
                    parsed = json.loads(out) if rc == 0 and out.strip() else None
                except json.JSONDecodeError as exc:
                    error = f'json: {exc}'
                if rc != 0:
                    error = f'exit {rc}'
                rows = parsed if isinstance(parsed, list) else []
                rank = unique_rank(rows, q['target_path']) if not error else 0
                if rows:
                    first = rows[0]
                    src = root / first['path']
                    if not src.is_file() or first['start'] < 1 or first['end'] < first['start']:
                        error = 'row validation failed'
                rec = {
                    'id': qid,
                    'source_id': sid,
                    'language': q['language'],
                    'split': split[sid],
                    'target_path': q['target_path'],
                    'rank': rank,
                    'rows': len(rows),
                    'seconds': round(secs, 4),
                    'error': error,
                }
                rows_out.append(rec)
                print(f'  {qid} rank={rank} rows={len(rows)} {secs:.3f}s'
                      + (f' ERR={error}' if error else ''), flush=True)
        finally:
            call([wg, 'down'], env=env, timeout=60)

    nq = len(rows_out)
    def hit(k):
        return sum(1 for r in rows_out if 0 < r['rank'] <= k)

    mrr = sum(1.0 / r['rank'] for r in rows_out if 0 < r['rank'] <= 10) / max(nq, 1)
    summary = {
        'queries': nq,
        'repos': len(repos),
        'wg': wg,
        'wg_sha256': sha(Path(wg)),
        'query_sha256': QUERY_SHA,
        'split': 'repo-level: first 5 repos per language dev, last 5 heldout',
        'hit@1': hit(1),
        'hit@5': hit(5),
        'hit@10': hit(10),
        'hit@1_rate': hit(1) / max(nq, 1),
        'hit@5_rate': hit(5) / max(nq, 1),
        'hit@10_rate': hit(10) / max(nq, 1),
        'mrr@10': mrr,
        'errors': sum(1 for r in rows_out if r['error']),
        'misses': [r for r in rows_out if not r['rank']],
        'elapsed_seconds': round(time.time() - started, 2),
    }
    (run / 'per-query.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in rows_out), encoding='utf-8')
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in summary.items() if k != 'misses'}, indent=2))


if __name__ == '__main__':
    main()
