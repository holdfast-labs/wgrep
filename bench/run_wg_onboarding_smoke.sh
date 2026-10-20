#!/usr/bin/env bash
set -euo pipefail

source "$HOME/.cargo/env"
project=/srv/projects/wispd/sgrep-shared-daemon-v1
run=${1:?usage: run_wg_onboarding_smoke.sh NEW_RUN_DIRECTORY}
test -d "$(dirname "$run")"
test ! -e "$run"
mkdir "$run"
trap 'rc=$?; printf "%s\n" "$rc" > "$run/exit.status"; if [[ -n "${WG_DONE_CHANNEL:-}" ]]; then tmux wait-for -S "$WG_DONE_CHANNEL"; fi' EXIT
export HF_HUB_OFFLINE=1
export PATH="/srv/workspaces/wispd/tools/rg-15:$PATH"

cd "$project"
cargo build --locked --release -j2 --bin wg --bin sgrep > "$run/build.log" 2>&1
cargo test --locked --release -j2 --bin wg service::tests -- --nocapture > "$run/service-tests.log" 2>&1
cargo test --locked --release -j2 --lib live::tests -- --nocapture > "$run/live-tests.log" 2>&1
python3 "$project/bench/WGREP_WG_ONBOARDING.py" \
    --wg "$project/target/release/wg" \
    --sgrep /srv/projects/wispd/sgrep-control-0fbe8e5/target/release/sgrep \
    --work "$run/onboarding" > "$run/onboarding.log" 2>&1
printf 'wg onboarding smoke passed: %s\n' "$run"
