#!/usr/bin/env python3
"""CoSQA 500-query evaluation for the current wg binary.

Materializes the published 6,267 snippets as one corpus and scores distinct
snippet Hit@1/5/10 and MRR@10. Run on the VPS.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import time
from pathlib import Path

PUB = Path('/srv/projects/wispd/sgrep-published-inputs/CoCLR')
QREV = '14ebcacf9e9bc3e7109102632bc63047876f27d2'
Q_SHA = '27d3552b2e7dd86cf9f9a0b3c135e6777d4faafa9c29fb1b9b01ece945974ad6'
M_SHA = 'be8a910ca03cb9f1b38a0eba6dc8109eeb8040a40aaba1a467a7b8426d261f3f'
DEFAULT_RUN = Path('/srv/projects/wispd/wg-quality-20261020/cosqa')


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
    args = ap.parse_args()

    wg = str(Path(args.wg).resolve())
    run = args.run.resolve()
    if run.exists():
        raise SystemExit(f'run dir exists: {run}')
    raw = run / 'raw'
    raw.mkdir(parents=True)
    corpus = run / 'corpus'
    corpus.mkdir()

    rev = subprocess.check_output(['git', '-C', str(PUB), 'rev-parse', 'HEAD'], text=True).strip()
    assert rev == QREV, rev
    qpath = PUB / 'data/search/cosqa-retrieval-test-500.json'
    mpath = PUB / 'data/search/code_idx_map.txt'
    assert sha(qpath) == Q_SHA, 'CoSQA query hash mismatch'
    assert sha(mpath) == M_SHA, 'CoSQA snippet map hash mismatch'
    queries = json.loads(qpath.read_text(encoding='utf-8'))
    code_map = json.loads(mpath.read_text(encoding='utf-8'))
    assert len(queries) == 500 and len(code_map) == 6267

    by_index = {}
    for code, idx in code_map.items():
        assert type(idx) is int and 0 <= idx < 6267 and idx not in by_index
        by_index[idx] = code
    assert set(by_index) == set(range(6267))
    for q in queries:
        assert by_index[q['retrieval_idx']] == q['code']
    for idx in range(6267):
        (corpus / f'{idx:04d}.py').write_bytes(by_index[idx].encode('utf-8'))

    base_env = dict(os.environ)
    base_env['HF_HUB_OFFLINE'] = '1'
    base_env['RAYON_NUM_THREADS'] = '2'
    base_env.pop('RIPGREP_CONFIG_PATH', None)
    service = run / 'service'
    env = dict(base_env, WG_SERVICE_DIR=str(service))
    rc, out, err, secs = call([wg, 'up', str(corpus), '--model', 'code', '--json'],
                              env=env, timeout=1800)
    if rc != 0:
        raise SystemExit(f'up failed: {err.decode()[-800:]}')
    print(f'up {secs:.2f}s', flush=True)
    rows_out = []
    started = time.time()
    try:
        for n, q in enumerate(queries, 1):
            qid = q['idx']
            target = f"{q['retrieval_idx']:04d}.py"
            rc, out, err, secs = call(
                [wg, q['doc'], str(corpus), '--model', 'code', '--json',
                 '--explain', '--top', str(args.top)], env=env, timeout=300)
            (raw / f'{qid}.stdout').write_bytes(out)
            (raw / f'{qid}.stderr').write_bytes(err)
            error = None if rc == 0 else f'exit {rc}'
            try:
                parsed = json.loads(out) if rc == 0 and out.strip() else None
            except json.JSONDecodeError as exc:
                parsed, error = None, f'json: {exc}'
            rows = parsed if isinstance(parsed, list) else []
            rank = unique_rank(rows, target) if not error else 0
            rows_out.append({
                'id': qid,
                'target_path': target,
                'split': 'dev' if n <= 250 else 'heldout',
                'rank': rank,
                'rows': len(rows),
                'seconds': round(secs, 4),
                'error': error,
            })
            if n % 50 == 0:
                print(f'{n}/500', flush=True)
    finally:
        call([wg, 'down'], env=env, timeout=60)

    nq = len(rows_out)
    def hit(k):
        return sum(1 for r in rows_out if 0 < r['rank'] <= k)

    mrr = sum(1.0 / r['rank'] for r in rows_out if 0 < r['rank'] <= 10) / max(nq, 1)
    summary = {
        'queries': nq,
        'wg': wg,
        'wg_sha256': sha(Path(wg)),
        'query_sha256': Q_SHA,
        'snippet_map_sha256': M_SHA,
        'split': 'query order: first 250 dev, last 250 heldout',
        'hit@1': hit(1), 'hit@5': hit(5), 'hit@10': hit(10),
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
