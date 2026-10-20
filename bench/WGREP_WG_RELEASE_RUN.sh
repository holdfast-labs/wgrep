#!/usr/bin/env bash
# Build the unified wg on the VPS and run the release validation harness.
# Run this in tmux, from a fresh session, only after the search contributor
# reports a compiled wg for the frozen contract. It never mutates the source.
#
# Usage: WGREP_WG_RELEASE_RUN.sh NEW_RUN_DIRECTORY [PROJECT_DIR]
# Env:   WG_SAMPLES, WG_VIS_SAMPLES, WG_BENCH_ROOT, WG_SIXLANG, WG_DONE_CHANNEL
set -euo pipefail

source "$HOME/.cargo/env"
run=${1:?usage: WGREP_WG_RELEASE_RUN.sh NEW_RUN_DIRECTORY [PROJECT_DIR]}
project=${2:-/srv/projects/wispd/sgrep-shared-daemon-v1}
test -d "$(dirname "$run")"
test ! -e "$run"
mkdir "$run"
trap 'rc=$?; printf "%s\n" "$rc" > "$run/exit.status"; if [[ -n "${WG_DONE_CHANNEL:-}" ]]; then tmux wait-for -S "$WG_DONE_CHANNEL"; fi' EXIT

export HF_HUB_OFFLINE=1
export PATH="/srv/workspaces/wispd/tools/rg-15:$PATH"

cd "$project"
cargo build --locked --release -j2 --bin wg > "$run/build.log" 2>&1

wg="$project/target/release/wg"
sha=$(sha256sum "$wg" | cut -d' ' -f1)
rev=$(git -C "$project" rev-parse HEAD 2>/dev/null || printf 'unversioned-copy')

extra=()
[[ -n "${WG_BENCH_ROOT:-}" ]] && extra+=(--bench-root "$WG_BENCH_ROOT")
[[ -n "${WG_VISIBILITY_CORPUS:-}" ]] && extra+=(--visibility-corpus "$WG_VISIBILITY_CORPUS")
[[ -n "${WG_SIXLANG:-}" ]] && extra+=(--sixlang-fixture "$project/bench/WGREP_SGREP_PARITY_FIXTURE")

python3 "$project/bench/WGREP_WG_RELEASE.py" --run \
    --wg "$wg" --wg-sha256 "$sha" --wg-source-revision "$rev" \
    --work "$run/wg-release" \
    --samples "${WG_SAMPLES:-120}" \
    --visibility-samples "${WG_VIS_SAMPLES:-100}" \
    "${extra[@]}" > "$run/release.log" 2>&1
printf 'wg release validation passed: %s\n' "$run"
