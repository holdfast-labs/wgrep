#!/usr/bin/env bash
# Hidden setup for the wg launch clips.
# Run from the repository root with WG_LAUNCH_WORK, WG_LAUNCH_CORPUS and WG_BIN set.
set -euo pipefail

: "${WG_LAUNCH_WORK:?Set WG_LAUNCH_WORK to a task-owned directory}"
: "${WG_LAUNCH_CORPUS:?Set WG_LAUNCH_CORPUS to the copied Rust src tree}"
: "${WG_BIN:?Set WG_BIN to the wg binary}"

wg_bin_dir=$(cd "$(dirname "$WG_BIN")" && pwd)
export PATH="$wg_bin_dir:$PATH"
if [[ -x "$wg_bin_dir/rg" ]]; then
  export WG_RG="$wg_bin_dir/rg"
fi

export WG_SERVICE_DIR="${WG_SERVICE_DIR:-$WG_LAUNCH_WORK/service}"
export HF_HUB_OFFLINE="${HF_HUB_OFFLINE:-1}"
export PS1='\[\e[1;38;5;117m\]$ \[\e[0m\]'

mkdir -p "$WG_LAUNCH_WORK"

second="$WG_LAUNCH_WORK/api-server"
mkdir -p "$second"
cat >"$second/queue.rs" <<'EOF'
// Choose the next queued job.
pub fn next_job(queue: &mut Vec<String>) -> Option<String> {
    queue.pop()
}
EOF
cat >"$second/metrics.rs" <<'EOF'
// Count completed jobs by status.
pub fn completed_count(statuses: &[&str]) -> usize {
    statuses.iter().filter(|s| **s == "done").count()
}
EOF

# The live clip starts empty and appends a function on screen.
printf '' >"$WG_LAUNCH_CORPUS/live.rs"

cleanup() {
  "$WG_BIN" down >/dev/null 2>&1 || true
}
trap cleanup EXIT

cd "$WG_LAUNCH_CORPUS"

# Keep the interactive shell alive after a no-match command.
set +e
clear
