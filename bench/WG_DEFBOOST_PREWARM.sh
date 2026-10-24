#!/bin/bash
# One-off: populate the tree-sitter tsx parser in the shared user cache.
set -euo pipefail
cd /srv/projects/wispd/wg-defboost-20261024
CFG='{"name":"base","lex":200,"sem":200,"w":0.25}'
env -u HF_HUB_OFFLINE RAYON_NUM_THREADS=2 ./target/release/wg-quality-tool \
  --root /srv/projects/wispd/wg-semble-20261024/semble-bench/repos/trpc/packages/server/src \
  --queries results/semble-bench/trpc.queries.json \
  --out /tmp/trpc.json --top 5 --config "$CFG"
echo PREWARM_OK
