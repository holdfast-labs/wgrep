#!/usr/bin/env bash
# Render the wg README demos with VHS. Linux only.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

: "${WG_DEMO_ROOT:?Set WG_DEMO_ROOT to a task-owned directory}"
command -v vhs >/dev/null
command -v ffmpeg >/dev/null
command -v ffprobe >/dev/null
wg_bin=${WG_BIN:-$(command -v wg)}
wg_bin=$(realpath "$wg_bin")
test -x "$wg_bin"
work_root=${WG_DEMO_WORK_ROOT:-/tmp}

run="$WG_DEMO_ROOT/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$run/tmp" "docs/media"
export PATH="$(dirname "$wg_bin"):$PATH"
export HF_HUB_OFFLINE="${HF_HUB_OFFLINE:-1}"
export TMPDIR="$run/tmp"

cleanup() {
  if [[ -n "${WG_SERVICE_DIR:-}" && -f "$WG_SERVICE_DIR/service.json" ]]; then
    "$wg_bin" down >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

for clip in quickstart live multi-repo; do
  work="$work_root/wg-demo-$clip"
  case "$work" in
    "$work_root"/wg-demo-*) ;;
    *) printf 'unsafe work path: %s\n' "$work" >&2; exit 1 ;;
  esac
  rm -rf "$work"
  mkdir -p "$work"
  export WG_DEMO_WORK="$work"
  export WG_SERVICE_DIR="$work/service"
  vhs "docs/demos/$clip.tape" >"$run/$clip.log" 2>&1
  "$wg_bin" down >/dev/null 2>&1 || true
  test ! -f "$WG_SERVICE_DIR/service.json"
  media="docs/media/wg-$clip.mp4"
  ffprobe -v error \
    -show_entries stream=codec_name,width,height,nb_frames,r_frame_rate \
    -show_entries format=duration,size -of json "$media" \
    >"$run/$clip.ffprobe.json"
  ffmpeg -y -v error -sseof -1 -i "$media" -frames:v 1 -update 1 \
    "$run/$clip-end.png"
  ffmpeg -y -v error -i "$media" \
    -vf 'fps=1/2,scale=600:-1,tile=3x2' -frames:v 1 -update 1 \
    "$run/$clip-sheet.png"
  rm -rf "$work"
done

printf 'rendered demos in %s\n' "$run"
