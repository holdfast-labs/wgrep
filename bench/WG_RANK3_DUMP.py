#!/usr/bin/env python3
"""Print RepoQA file and line metrics for one summary.json."""
from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> None:
    d = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
    for k, v in d.items():
        print(f"{k:12s} file={v['hit@1']}/{v['hit@5']}/{v['hit@10']} "
              f"mrr={v['mrr@10']:.5f} line={v['line_hit@1']}/{v['line_hit@5']}/{v['line_hit@10']} "
              f"W/L={v['paired_file_hit_wins_vs_base']}/{v['paired_file_hit_losses_vs_base']}")


if __name__ == '__main__':
    main()
