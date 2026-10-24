#!/bin/bash
# Driver for the definition-boost study. All actions run on the VPS.
set -euo pipefail
ROOT=/srv/projects/wispd/wg-defboost-20261024
LOG=/srv/logs/wispd/wg-defboost-20261024
export RAYON_NUM_THREADS=2
export HF_HUB_OFFLINE=1
source $HOME/.cargo/env
cd "$ROOT"
TOOL="$ROOT/target/release/wg-quality-tool"

case "${1:-}" in
  diag)
    for r in rust-alacritty__alacritty python-locustio__locust typescript-axios__axios java-google__gson; do
      python3 -u src/bench/WG_DEFBOOST_OPTIONS.py --split dev --configs diag.json \
        --run "results/diag/$r" --only-repo "$r" --profile > "$LOG/diag-$r.log" 2>&1
    done
    ;;
  dev)
    python3 -u src/bench/WG_DEFBOOST_OPTIONS.py --split dev --configs dev.json \
      --run results/dev --profile > "$LOG/dev.log" 2>&1
    ;;
  dev2)
    python3 -u src/bench/WG_DEFBOOST_OPTIONS.py --split dev --configs dev2.json \
      --run results/dev2 --profile > "$LOG/dev2.log" 2>&1
    ;;
  dev3)
    python3 -u src/bench/WG_DEFBOOST_OPTIONS.py --split dev --configs predeclared.json \
      --run results/dev3 --profile > "$LOG/dev3.log" 2>&1
    ;;
  heldout)
    python3 -u src/bench/WG_DEFBOOST_OPTIONS.py --split heldout --configs predeclared.json \
      --run results/heldout --profile > "$LOG/heldout.log" 2>&1
    ;;
  cosqa-dev)
    python3 -u src/bench/WG_DEFBOOST_COSQA.py --split dev --configs predeclared.json \
      --run results/cosqa --profile > "$LOG/cosqa-dev.log" 2>&1
    ;;
  cosqa-heldout)
    python3 -u src/bench/WG_DEFBOOST_COSQA.py --split heldout --configs predeclared.json \
      --run results/cosqa --profile > "$LOG/cosqa-heldout.log" 2>&1
    ;;
  launch)
    python3 -u src/bench/WG_DEFBOOST_LAUNCH.py --configs predeclared.json \
      --run results/launch --profile > "$LOG/launch.log" 2>&1
    ;;
  semble)
    python3 -u src/bench/WG_DEFBOOST_SEMBLE.py --configs predeclared.json --online \
      --run results/semble-bench > "$LOG/semble-bench.log" 2>&1
    ;;
  *)
    echo "usage: $0 {diag|dev|heldout}" >&2
    exit 2
    ;;
esac
echo "OK $1"
