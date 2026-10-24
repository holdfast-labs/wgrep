#!/bin/bash
# Driver for the rank2 study. Run on the VPS.
set -euo pipefail
ROOT=/srv/projects/wispd/wg-rank2-20261024
LOG=/srv/logs/wispd/wg-rank2-20261024
PY=/srv/projects/wispd/wg-semble-20261024/semble-venv/bin/python
export RAYON_NUM_THREADS=4
cd "$ROOT"

case "${1:-}" in
  base)
    "$PY" -u WG_RANK2_NDCG.py --split dev --configs WG_RANK2_BASE_CONFIGS.json \
      --run results/base-dev > "$LOG/base-dev.log" 2>&1
    "$PY" -u WG_RANK2_NDCG.py --split heldout --configs WG_RANK2_BASE_CONFIGS.json \
      --run results/base-heldout > "$LOG/base-heldout.log" 2>&1
    ;;
  dev)
    "$PY" -u WG_RANK2_NDCG.py --split dev --configs dev.json \
      --run results/dev --profile > "$LOG/dev.log" 2>&1
    ;;
  dev5)
    "$PY" -u WG_RANK2_NDCG.py --split dev --configs dev5.json \
      --run results/dev5 --profile > "$LOG/dev5.log" 2>&1
    ;;
  dev4)
    "$PY" -u WG_RANK2_NDCG.py --split dev --configs dev4.json \
      --run results/dev4 --profile > "$LOG/dev4.log" 2>&1
    ;;
  dev3)
    "$PY" -u WG_RANK2_NDCG.py --split dev --configs dev3.json \
      --run results/dev3 --profile > "$LOG/dev3.log" 2>&1
    ;;
  dev2)
    "$PY" -u WG_RANK2_NDCG.py --split dev --configs dev2.json \
      --run results/dev2 --profile > "$LOG/dev2.log" 2>&1
    ;;
  heldout)
    "$PY" -u WG_RANK2_NDCG.py --split heldout --configs predeclared.json \
      --run results/heldout --profile > "$LOG/heldout.log" 2>&1
    ;;
  *)
    echo "usage: $0 {base|dev|heldout}" >&2
    exit 2
    ;;
esac
echo "OK $1"
