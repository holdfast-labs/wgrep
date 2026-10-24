#!/usr/bin/env python3
"""Semble on wg's own evaluation sets: RepoQA 600 and CoSQA 500.

Same corpora, same scoring as bench/WG_QUALITY_REPOQA.py and
bench/WG_QUALITY_COSQA.py: distinct-file Hit@1/5/10 and MRR@10, and for
CoSQA the unit is the unique snippet. Semble runs with its default config,
default model and default content type (code).

Run on the VPS inside the Semble venv.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from WG_QUALITY_REPOQA import (  # noqa: E402
    DEFAULT_RUN as REPOQA_DEFAULT_RUN,
    QUERY_PATH as REPOQA_QUERY_PATH,
    QUERY_SHA as REPOQA_QUERY_SHA,
    sha,
    unique_rank,
)
from WG_QUALITY_COSQA import (  # noqa: E402
    DEFAULT_RUN as COSQA_DEFAULT_RUN,
    M_SHA as COSQA_M_SHA,
    PUB as COSQA_PUB,
    Q_SHA as COSQA_Q_SHA,
    QREV as COSQA_QREV,
)


def file_rank_full(paths, target):
    try:
        return paths.index(target) + 1
    except ValueError:
        return 0


def index_files(index):
    return {c.file_path for c in index.chunks}


def run_repoqa(args):
    from semble import SembleIndex
    import semble

    assert REPOQA_QUERY_PATH.is_file(), REPOQA_QUERY_PATH
    assert sha(REPOQA_QUERY_PATH) == REPOQA_QUERY_SHA, 'RepoQA query hash mismatch'
    queries = json.loads(REPOQA_QUERY_PATH.read_text(encoding='utf-8'))
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
    if args.limit_repos:
        repos = repos[:args.limit_repos]

    run = args.run.resolve()
    if run.exists():
        raise SystemExit(f'run dir exists: {run}')
    run.mkdir(parents=True)

    rows_out = []
    repo_rows = []
    started = time.time()
    for n, sid in enumerate(repos, 1):
        group = by_repo[sid]
        root = Path(group[0]['root'])
        t0 = time.perf_counter()
        index = SembleIndex.from_path(root)
        index_s = time.perf_counter() - t0
        chunk_files = index_files(index)
        info = {
            'source_id': sid,
            'language': group[0]['language'],
            'split': split[sid],
            'root': str(root),
            'indexed_files': index.stats.indexed_files,
            'chunks': index.stats.total_chunks,
            'index_seconds': round(index_s, 3),
            'loaded_from_disk': index.loaded_from_disk,
        }
        repo_rows.append(info)
        print(f'[{n}/{len(repos)}] {sid} files={index.stats.indexed_files} '
              f'chunks={index.stats.total_chunks} index={index_s:.2f}s', flush=True)
        for q in group:
            qid = q['id']
            t0 = time.perf_counter()
            try:
                res = index.search(q['query'], top_k=args.top)
                error = None
            except Exception as exc:  # noqa: BLE001 - record per-query failures
                res, error = [], f'{type(exc).__name__}: {exc}'
            secs = time.perf_counter() - t0
            rows = [r.chunk for r in res]
            paths = []
            for chunk in rows:
                if chunk.file_path not in paths:
                    paths.append(chunk.file_path)
            rank = unique_rank([{'path': p} for p in paths], q['target_path']) if not error else 0
            full = file_rank_full(paths, q['target_path'])
            rec = {
                'id': qid,
                'source_id': sid,
                'language': q['language'],
                'split': split[sid],
                'target_path': q['target_path'],
                'target_indexed': q['target_path'] in chunk_files,
                'rank': rank,
                'full_file_rank': full,
                'rows': len(rows),
                'seconds': round(secs, 5),
                'error': error,
            }
            rows_out.append(rec)
            if rank == 0:
                print(f'  MISS {qid} target={q["target_path"]} '
                      f'indexed={rec["target_indexed"]} full={full} rows={len(rows)}', flush=True)

    nq = len(rows_out)

    def hit(k):
        return sum(1 for r in rows_out if 0 < r['rank'] <= k)

    mrr = sum(1.0 / r['rank'] for r in rows_out if 0 < r['rank'] <= 10) / max(nq, 1)
    by_lang = defaultdict(list)
    for r in rows_out:
        by_lang[r['language']].append(r)
    per_language_out = {}
    for lang, rs in sorted(by_lang.items()):
        m = len(rs)
        per_language_out[lang] = {
            'n': m,
            'hit@1': sum(1 for r in rs if 0 < r['rank'] <= 1),
            'hit@5': sum(1 for r in rs if 0 < r['rank'] <= 5),
            'hit@10': sum(1 for r in rs if 0 < r['rank'] <= 10),
            'mrr@10': sum(1.0 / r['rank'] for r in rs if 0 < r['rank'] <= 10) / m,
        }
    summary = {
        'arm': 'A-repoqa',
        'tool': 'semble',
        'semble_version': semble.__version__,
        'semble_commit': '913bdac1d59bc2defc658453a65719d133f1ff5a',
        'model': 'minishlab/potion-code-16M-v2',
        'model_revision': 'e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b',
        'config': 'defaults: content=code, rerank=True, alpha=auto, top_k=%d' % args.top,
        'queries': nq,
        'repos': len(repos),
        'query_sha256': REPOQA_QUERY_SHA,
        'split': 'repo-level: first 5 repos per language dev, last 5 heldout',
        'hit@1': hit(1), 'hit@5': hit(5), 'hit@10': hit(10),
        'hit@1_rate': hit(1) / max(nq, 1),
        'hit@5_rate': hit(5) / max(nq, 1),
        'hit@10_rate': hit(10) / max(nq, 1),
        'mrr@10': mrr,
        'errors': sum(1 for r in rows_out if r['error']),
        'target_not_indexed': sum(1 for r in rows_out if not r['target_indexed']),
        'per_language': per_language_out,
        'per_repo': repo_rows,
        'misses': [r for r in rows_out if not r['rank']],
        'elapsed_seconds': round(time.time() - started, 2),
    }
    (run / 'per-query.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in rows_out), encoding='utf-8')
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in summary.items() if k != 'misses'}, indent=2))


def run_cosqa(args):
    from semble import SembleIndex
    import semble
    import subprocess

    run = args.run.resolve()
    if run.exists():
        raise SystemExit(f'run dir exists: {run}')
    run.mkdir(parents=True)
    corpus = run / 'corpus'
    corpus.mkdir()

    rev = subprocess.check_output(['git', '-C', str(COSQA_PUB), 'rev-parse', 'HEAD'], text=True).strip()
    assert rev == COSQA_QREV, rev
    qpath = COSQA_PUB / 'data/search/cosqa-retrieval-test-500.json'
    mpath = COSQA_PUB / 'data/search/code_idx_map.txt'
    assert sha(qpath) == COSQA_Q_SHA, 'CoSQA query hash mismatch'
    assert sha(mpath) == COSQA_M_SHA, 'CoSQA snippet map hash mismatch'
    queries = json.loads(qpath.read_text(encoding='utf-8'))
    code_map = json.loads(mpath.read_text(encoding='utf-8'))
    assert len(queries) == 500 and len(code_map) == 6267

    by_index = {}
    for code, idx in code_map.items():
        assert type(idx) is int and 0 <= idx < 6267 and idx not in by_index
        by_index[idx] = code
    for idx in range(6267):
        (corpus / f'{idx:04d}.py').write_bytes(by_index[idx].encode('utf-8'))

    t0 = time.perf_counter()
    index = SembleIndex.from_path(corpus)
    index_s = time.perf_counter() - t0
    chunk_files = index_files(index)
    print(f'cosqa index {index_s:.2f}s files={index.stats.indexed_files} '
          f'chunks={index.stats.total_chunks}', flush=True)

    rows_out = []
    started = time.time()
    for n, q in enumerate(queries, 1):
        qid = q['idx']
        target = f"{q['retrieval_idx']:04d}.py"
        t0 = time.perf_counter()
        try:
            res = index.search(q['doc'], top_k=args.top)
            error = None
        except Exception as exc:  # noqa: BLE001
            res, error = [], f'{type(exc).__name__}: {exc}'
        secs = time.perf_counter() - t0
        paths = []
        for r in res:
            if r.chunk.file_path not in paths:
                paths.append(r.chunk.file_path)
        rank = unique_rank([{'path': p} for p in paths], target) if not error else 0
        rows_out.append({
            'id': qid,
            'target_path': target,
            'split': 'dev' if n <= 250 else 'heldout',
            'target_indexed': target in chunk_files,
            'rank': rank,
            'full_file_rank': file_rank_full(paths, target),
            'rows': len(res),
            'seconds': round(secs, 5),
            'error': error,
        })
        if n % 50 == 0:
            print(f'{n}/500', flush=True)

    nq = len(rows_out)

    def hit(k):
        return sum(1 for r in rows_out if 0 < r['rank'] <= k)

    mrr = sum(1.0 / r['rank'] for r in rows_out if 0 < r['rank'] <= 10) / max(nq, 1)
    summary = {
        'arm': 'A-cosqa',
        'tool': 'semble',
        'semble_version': semble.__version__,
        'semble_commit': '913bdac1d59bc2defc658453a65719d133f1ff5a',
        'model': 'minishlab/potion-code-16M-v2',
        'model_revision': 'e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b',
        'config': 'defaults: content=code, rerank=True, alpha=auto, top_k=%d' % args.top,
        'queries': nq,
        'query_sha256': COSQA_Q_SHA,
        'snippet_map_sha256': COSQA_M_SHA,
        'split': 'query order: first 250 dev, last 250 heldout',
        'index_seconds': round(index_s, 3),
        'indexed_files': index.stats.indexed_files,
        'chunks': index.stats.total_chunks,
        'hit@1': hit(1), 'hit@5': hit(5), 'hit@10': hit(10),
        'hit@1_rate': hit(1) / max(nq, 1),
        'hit@5_rate': hit(5) / max(nq, 1),
        'hit@10_rate': hit(10) / max(nq, 1),
        'mrr@10': mrr,
        'errors': sum(1 for r in rows_out if r['error']),
        'target_not_indexed': sum(1 for r in rows_out if not r['target_indexed']),
        'misses': [r for r in rows_out if not r['rank']],
        'elapsed_seconds': round(time.time() - started, 2),
    }
    (run / 'per-query.jsonl').write_text(
        ''.join(json.dumps(r) + '\n' for r in rows_out), encoding='utf-8')
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({k: v for k, v in summary.items() if k != 'misses'}, indent=2))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest='task', required=True)
    r = sub.add_parser('repoqa')
    r.add_argument('--run', type=Path,
                   default=Path('/srv/projects/wispd/wg-semble-20261024/arm-a/repoqa'))
    r.add_argument('--top', type=int, default=100)
    r.add_argument('--only-language', default=None)
    r.add_argument('--only-split', choices=['dev', 'heldout'], default=None)
    r.add_argument('--limit-repos', type=int, default=0)
    c = sub.add_parser('cosqa')
    c.add_argument('--run', type=Path,
                   default=Path('/srv/projects/wispd/wg-semble-20261024/arm-a/cosqa'))
    c.add_argument('--top', type=int, default=100)
    args = ap.parse_args()
    os.environ.setdefault('HF_HUB_OFFLINE', '1')
    if args.task == 'repoqa':
        run_repoqa(args)
    else:
        run_cosqa(args)


if __name__ == '__main__':
    main()
