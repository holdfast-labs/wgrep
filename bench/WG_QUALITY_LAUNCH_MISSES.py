#!/usr/bin/env python3
"""Launch-bench q1-q8 analysis on the VPS. Analysis only; no product edits."""
from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

RUN = Path('/srv/projects/wispd/wg-quality-20261020/launch')
SRC = Path('/srv/projects/wispd/wg-quality-20261020/src')
WG = SRC / 'target/release/wg'
TOOL = SRC / 'target/release/wg-quality-tool'
CORPUS = Path('/srv/projects/wispd/wg-launch-bench-20261020/campaign2/corpus')
SERVICE = RUN / 'service'

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

CONFIGS = [
    {'name': 'base', 'lex': 200, 'sem': 200, 'w': 0.25, 'path': 0.0, 'def': 0.0},
    {'name': 'w50', 'lex': 200, 'sem': 200, 'w': 0.50, 'path': 0.0, 'def': 0.0},
    {'name': 'pool400', 'lex': 400, 'sem': 400, 'w': 0.25, 'path': 0.0, 'def': 0.0},
    {'name': 'def10', 'lex': 200, 'sem': 200, 'w': 0.25, 'path': 0.0, 'def': 0.10},
    {'name': 'def20', 'lex': 200, 'sem': 200, 'w': 0.25, 'path': 0.0, 'def': 0.20},
    {'name': 'path10', 'lex': 200, 'sem': 200, 'w': 0.25, 'path': 0.10, 'def': 0.0},
    {'name': 'path20', 'lex': 200, 'sem': 200, 'w': 0.25, 'path': 0.20, 'def': 0.0},
    {'name': 'path10_def10', 'lex': 200, 'sem': 200, 'w': 0.25, 'path': 0.10, 'def': 0.10},
]


def call(argv, *, env=None, timeout=900):
    started = time.perf_counter()
    p = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=env, timeout=timeout)
    return p.returncode, p.stdout, p.stderr, time.perf_counter() - started


def main():
    RUN.mkdir(parents=True, exist_ok=True)
    (RUN / 'cli').mkdir(exist_ok=True)
    queries = []
    for qid, question, path, line in QUESTIONS:
        queries.append({'id': qid, 'query': question, 'terms': [],
                        'targets': [{'path': path, 'line': line}]})
    (RUN / 'queries.json').write_text(json.dumps(queries), encoding='utf-8')

    env = dict(os.environ, HF_HUB_OFFLINE='1')
    env['RAYON_NUM_THREADS'] = '2'
    env['WG_SERVICE_DIR'] = str(SERVICE)
    rc, out, err, secs = call([str(WG), 'up', str(CORPUS), '--model', 'code', '--json'],
                              env=env, timeout=900)
    print('up', rc, round(secs, 2), err.decode()[-500:])
    if rc != 0:
        raise SystemExit('wg up failed')
    try:
        for qid, question, path, line in QUESTIONS:
            rc, out, err, secs = call(
                [str(WG), question, str(CORPUS), '--model', 'code', '--json',
                 '--explain', '--top', '50'], env=env, timeout=300)
            if rc != 0:
                raise SystemExit(f'wg {qid} failed: {err.decode()[-500:]}')
            (RUN / 'cli' / f'{qid}.json').write_bytes(out)
            print('cli', qid, round(secs, 3), len(json.loads(out)))
    finally:
        call([str(WG), 'down'], env=env, timeout=60)

    argv = [str(TOOL), '--root', str(CORPUS), '--queries', str(RUN / 'queries.json'),
            '--out', str(RUN / 'tool.json'), '--top', '50', '--content']
    for cfg in CONFIGS:
        argv += ['--config', json.dumps(cfg)]
    rc, out, err, secs = call(argv, env=env, timeout=3600)
    print('tool', rc, round(secs, 2))
    if rc != 0:
        raise SystemExit(f'tool failed: {err.decode()[-2000:]}')
    print(err.decode()[-2000:])


if __name__ == '__main__':
    main()
