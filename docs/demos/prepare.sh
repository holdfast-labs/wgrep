#!/usr/bin/env bash
# Source this file from a VHS shell in the repository root.
set -euo pipefail

: "${WG_DEMO_WORK:?Set WG_DEMO_WORK to a task-owned directory}"
: "${WG_DEMO_CORPUS:?Set WG_DEMO_CORPUS to a task-owned corpus copy}"
: "${WG_BIN:?Set WG_BIN to the wg binary}"

wg_bin_dir=$(cd "$(dirname "$WG_BIN")" && pwd)
export PATH="$wg_bin_dir:$PATH"
export WG_SERVICE_DIR="${WG_SERVICE_DIR:-$WG_DEMO_WORK/service}"
export HF_HUB_OFFLINE="${HF_HUB_OFFLINE:-1}"
export PS1='\[\e[1;38;5;117m\]$ \[\e[0m\]'

mkdir -p "$WG_DEMO_WORK"

if [[ -n "${WG_DEMO_SECOND:-}" ]]; then
  mkdir -p "$WG_DEMO_SECOND"
  cat >"$WG_DEMO_SECOND/queue.rs" <<'EOF'
// Choose the next queued job.
pub fn next_job(queue: &mut Vec<String>) -> Option<String> {
    queue.pop()
}
EOF
  cat >"$WG_DEMO_SECOND/metrics.rs" <<'EOF'
// Count completed jobs by status.
pub fn completed_count(statuses: &[&str]) -> usize {
    statuses.iter().filter(|s| **s == "done").count()
}
EOF
fi

if [[ "${WG_DEMO_EMPTY_LIVE:-0}" == 1 ]]; then
  printf '' >"$WG_DEMO_CORPUS/live.rs"
fi

cleanup() {
  "$WG_BIN" down >/dev/null 2>&1 || true
}
trap cleanup EXIT

cd "$WG_DEMO_CORPUS"

# Keep the interactive shell alive after a no-match command.
set +e
clear
