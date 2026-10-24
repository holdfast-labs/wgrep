#!/bin/bash
# Launch-corpus cost measurement.
set -euo pipefail
source $HOME/venv/bin/activate
cd /srv/projects/wispd/wg-defboost-20261024
python3 -u src/bench/WG_DEFBOOST_COST.py \
  --root /srv/projects/wispd/wg-launch-bench-20261020/campaign2/corpus \
  --queries results/launch/queries.json
