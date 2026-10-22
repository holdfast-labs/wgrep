#!/usr/bin/env bash
# Render the wg README GIFs with VHS, then optimize them for GitHub.
#
# The clips use the 13.3k-file Rust corpus, not toy fixtures. The visible roots
# are under $HOME/code so no /srv/ paths appear in the recordings.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

: "${WG_DEMO_ROOT:?Set WG_DEMO_ROOT to a task-owned directory}"

wg_bin=${WG_BIN:-$(command -v wg)}
wg_bin=$(realpath "$wg_bin")
test -x "$wg_bin"

rg_bin=${WG_RG:-}
if [[ -z "$rg_bin" && -x /srv/workspaces/wispd/tools/rg-15/rg ]]; then
  rg_bin=/srv/workspaces/wispd/tools/rg-15/rg
fi
if [[ -z "$rg_bin" ]]; then
  rg_bin=$(command -v rg)
fi
test -x "$rg_bin"

corpus_src=${WG_DEMO_CORPUS_SRC:-/srv/workspaces/wispd/prof/src}
demo_home=${WG_DEMO_HOME:-$HOME/code}
run="$WG_DEMO_ROOT/run-$(date +%Y%m%d-%H%M%S)"

mkdir -p "$run/tmp" "$run/review" "$run/audit" docs/media

export PATH="$(dirname "$wg_bin"):$(dirname "$rg_bin"):$PATH"
export HF_HUB_OFFLINE="${HF_HUB_OFFLINE:-1}"
export TMPDIR="$run/tmp"
export WG_BIN="$wg_bin"

log() {
  printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

cleanup_service() {
  local work=${1:-${WG_DEMO_WORK:-}}
  if [[ -n "$work" && -f "$work/service/service.json" ]]; then
    WG_SERVICE_DIR="$work/service" "$wg_bin" down >/dev/null 2>&1 || true
  fi
}

trap 'cleanup_service' EXIT

command -v vhs >/dev/null || die "vhs is not available"
command -v ttyd >/dev/null || die "ttyd is not available"
command -v ffmpeg >/dev/null || die "ffmpeg is not available"
command -v ffprobe >/dev/null || die "ffprobe is not available"
command -v tesseract >/dev/null || die "tesseract is not available"

[[ -d "$corpus_src" ]] || die "corpus source not found: $corpus_src"
log "wg: $wg_bin"
log "rg: $rg_bin"
log "corpus: $corpus_src"
log "demo home: $demo_home"
log "run: $run"

log "prepare corpus copies"
rm -rf "$demo_home/rust" "$demo_home/live" "$demo_home/api-server"
mkdir -p "$demo_home"
mkdir -p "$demo_home/rust" "$demo_home/live"
cp -a "$corpus_src/." "$demo_home/rust/"
cp -a "$corpus_src/." "$demo_home/live/"
printf '' >"$demo_home/live/live.rs"

make_gif() {
  local mp4=$1 gif=$2 fps colors size
  for fps in 12 10 8; do
    for colors in 128 96 64; do
      ffmpeg -y -v error -i "$mp4" \
        -vf "fps=${fps},scale=1200:-1:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=${colors}:stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=3" \
        -loop 0 "$gif"
      size=$(stat -c%s "$gif")
      if (( size <= 2 * 1024 * 1024 )); then
        log "GIF $(basename "$gif"): ${size} bytes, ${fps} fps, ${colors} colors"
        return 0
      fi
    done
  done
  die "GIF exceeds 2 MB: $gif"
}

audit_gif() {
  local gif=$1 clip=$2 dir="$run/audit/$clip"
  local text="$run/audit-$clip.txt"
  mkdir -p "$dir"
  ffmpeg -y -v error -i "$gif" -vsync 0 "$dir/f%04d.png"
  : >"$text"
  local frame
  for frame in "$dir"/f*.png; do
    tesseract "$frame" stdout --psm 6 2>/dev/null >>"$text" || true
  done
  if grep -Eiq '/srv/|wispd|[0-9]{8}-[0-9]{6}' "$text"; then
    die "internal path or run id found in $clip (see $text)"
  fi
  log "audit $clip: no internal paths"
}

render_clip() {
  local clip=$1 corpus=$2 second=${3:-} empty_live=${4:-0}
  local work="$run/$clip"
  local mp4="docs/media/wg-$clip.mp4"
  local gif="docs/media/wg-$clip.gif"

  mkdir -p "$work"
  log "render $clip"
  (
    WG_DEMO_WORK="$work" \
    WG_DEMO_CORPUS="$corpus" \
    WG_DEMO_SECOND="$second" \
    WG_DEMO_EMPTY_LIVE="$empty_live" \
    WG_SERVICE_DIR="$work/service" \
    HF_HUB_OFFLINE="$HF_HUB_OFFLINE" \
      vhs "docs/demos/$clip.tape" >"$run/$clip.log" 2>&1
  ) || {
    cleanup_service "$work"
    die "vhs failed for $clip (see $run/$clip.log)"
  }
  cleanup_service "$work"
  [[ -f "$mp4" ]] || die "missing MP4: $mp4"

  make_gif "$mp4" "$gif"
  audit_gif "$gif" "$clip"

  ffprobe -v error \
    -show_entries stream=codec_name,width,height,nb_frames,r_frame_rate \
    -show_entries format=duration,size -of json "$mp4" \
    >"$run/$clip.mp4.ffprobe.json"
  ffprobe -v error \
    -show_entries stream=codec_name,width,height,nb_frames,r_frame_rate \
    -show_entries format=duration,size -of json "$gif" \
    >"$run/$clip.gif.ffprobe.json"

  ffmpeg -y -v error -sseof -0.2 -i "$mp4" -frames:v 1 \
    "$run/review/$clip-final.png"
  ffmpeg -y -v error -i "$gif" \
    -vf 'fps=1,scale=480:-1,tile=4x3' -frames:v 1 \
    "$run/review/$clip-sheet.png"

  local gif_size gif_dims gif_duration
  gif_size=$(stat -c%s "$gif")
  gif_dims=$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=width,height -of csv=p=0 "$gif")
  gif_duration=$(ffprobe -v error -show_entries format=duration \
    -of csv=p=0 "$gif")
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$clip" "$gif_size" "$gif_dims" "$gif_duration" "$(stat -c%s "$mp4")" \
    >>"$run/manifest.txt"

  rm -rf "$work"
}

render_clip quickstart "$demo_home/rust"
render_clip live "$demo_home/live" "" 1
render_clip multi-repo "$demo_home/rust" "$demo_home/api-server"

rm -rf "$demo_home/rust" "$demo_home/live" "$demo_home/api-server"
rmdir "$demo_home" 2>/dev/null || true

log "done"
printf 'run directory: %s\n' "$run"
printf 'review frames: %s\n' "$run/review"
printf 'audit logs: %s\n' "$run"/audit-*.txt
printf 'manifest: %s\n' "$run/manifest.txt"
