#!/usr/bin/env python3
"""Check tool baseline file ranks against the archived wg CLI output (q1-q8)."""
from __future__ import annotations

import json
from pathlib import Path

CLI = Path('/srv/projects/wispd/wg-quality-20261020/launch/cli')
TOOL = Path('/srv/projects/wispd/wg-defboost-20261024/results/launch/tool.json')
QUESTIONS = {
    'q1': 'tools/rust-analyzer/crates/parser/src/grammar/expressions.rs',
    'q2': 'tools/rustfmt/src/chains.rs',
    'q3': 'tools/clippy/clippy_lints/src/methods/clone_on_copy.rs',
    'q4': 'tools/miri/src/borrow_tracker/stacked_borrows/mod.rs',
    'q5': 'librustdoc/html/render/search_index.rs',
    'q6': 'bootstrap/src/core/download.rs',
    'q7': 'tools/compiletest/src/runtest.rs',
    'q8': 'tools/rust-analyzer/crates/hir-def/src/nameres/path_resolution.rs',
}


def unique_rank(rows, target):
    seen = []
    for row in rows:
        if row.get('path') not in seen:
            seen.append(row.get('path'))
    return seen.index(target) + 1 if target in seen else None


def main() -> None:
    tool = {q['id']: q for q in json.loads(TOOL.read_text(encoding='utf-8'))}
    ok = 0
    for qid, target in QUESTIONS.items():
        rows = json.loads((CLI / f'{qid}.json').read_text(encoding='utf-8'))
        cli_rank = unique_rank(rows, target)
        base = next(c for c in tool[qid]['configs'] if c['config']['name'] == 'base')
        tool_rank = base['targets'][0]['file_rank']
        same = cli_rank == tool_rank
        ok += same
        print(f'{qid}: cli={cli_rank} tool={tool_rank} {"OK" if same else "DIFF"}')
    print(f'{ok}/{len(QUESTIONS)} identical')


if __name__ == '__main__':
    main()
