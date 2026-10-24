#!/usr/bin/env python3
"""Launch q1-q8 check for the definition-boost configs.

Uses the 63,786-chunk launch corpus. Report-only. Run on the VPS.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from pathlib import Path

TOOL = Path('/srv/projects/wispd/wg-defboost-20261024/target/release/wg-quality-tool')
CORPUS = Path('/srv/projects/wispd/wg-launch-bench-20261020/campaign2/corpus')

QUESTIONS = [
    ('q1', 'How does the parser handle unary expressions in Rust code?',
     'tools/rust-analyzer/crates/parser/src/grammar/expressions.rs', 412),
    ('q2', 'Where does rustfmt decide to lay out a method chain vertically?',
     'tools/rustfmt/src/chains.rs', 709),
    ('q3', 'How does clippy find unnecessary clone calls on Copy types?',
     'tools/clippy/clippy_lints/src/methods/clone_on_copy.rs', 61),
    ('q4', 'How does miri implement the Stacked Borrows aliasing model?',
     'tools/miri/src/borrow_tracker/stacked_borrows/mod.rs', 467),
    ('q5', 'Where does rustdoc build the search index for documentation pages?',
     'librustdoc/html/render/search_index.rs', 1267),
    ('q6', 'How does bootstrap download a prebuilt LLVM from CI?',
     'bootstrap/src/core/download.rs', 356),
    ('q7', 'Where does compiletest compare the compiler output against expected files?',
     'tools/compiletest/src/runtest.rs', 2488),
    ('q8', 'How does rust-analyzer resolve a module import during name resolution?',
     'tools/rust-analyzer/crates/hir-def/src/nameres/path_resolution.rs', 154),
]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--tool', default=str(TOOL))
    ap.add_argument('--run', type=Path, default=Path('/srv/projects/wispd/wg-defboost-20261024/results/launch'))
    ap.add_argument('--corpus', type=Path, default=CORPUS)
    ap.add_argument('--configs', type=Path, required=True)
    ap.add_argument('--top', type=int, default=50)
    ap.add_argument('--profile', action='store_true')
    args = ap.parse_args()

    run = args.run.resolve()
    run.mkdir(parents=True, exist_ok=True)
    configs = json.loads(args.configs.read_text(encoding='utf-8'))
    queries = [{'id': qid, 'query': q, 'terms': [],
                'targets': [{'path': path, 'line': line}]}
               for qid, q, path, line in QUESTIONS]
    qpath = run / 'queries.json'
    qpath.write_text(json.dumps(queries), encoding='utf-8')
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
                       env=env, timeout=7200)
    if p.returncode != 0:
        raise SystemExit(p.stderr.decode()[-3000:])
    (run / 'profile.log').write_text(p.stderr.decode(), encoding='utf-8')
    data = json.loads(outp.read_text(encoding='utf-8'))
    summary = {}
    for cfg in configs:
        name = cfg['name']
        per_q = {}
        for q in data:
            c = next(x for x in q['configs'] if x['config']['name'] == name)
            t = c['targets'][0]
            per_q[q['id']] = {
                'file_rank': t['file_rank'],
                'line_rank': t['line_rank'],
                'in_union': t['in_union'],
                'fused_rank': t['fused_rank'],
                'target_defn': t['chunk'].get('defn_score'),
            }
        summary[name] = per_q
    (run / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n', encoding='utf-8')
    for name in summary:
        print(f'== {name}')
        for qid in summary[name]:
            r = summary[name][qid]
            print(f"   {qid} file_rank={r['file_rank']} line_rank={r['line_rank']} "
                  f"in_union={r['in_union']} defn={r['target_defn']}")
    print(f'elapsed {time.time()-started:.1f}s')


if __name__ == '__main__':
    main()
