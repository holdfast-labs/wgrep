#!/usr/bin/env bash
# Render the wg launch clips, cards, and hero videos on Linux.
#
# This script expects the final Linux wg binary and a copy of the Rust src
# corpus. It creates an isolated service directory and index for each clip,
# and it stops every daemon that it starts.
#
# Typical run on the authorized VPS:
#   WG_BIN=/srv/workspaces/wispd/tools/wg-linux/bin/wg \
#   WG_LAUNCH_CORPUS_SRC=/srv/workspaces/wispd/prof/src \
#   bash scripts/render_launch_media.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
LAUNCH="$ROOT/docs/launch"
STAMP=$(date +%Y%m%d-%H%M%S)
RUN_ROOT=${WG_LAUNCH_ROOT:-/srv/wg-lm}
RUN="$RUN_ROOT/$STAMP"
LOG_ROOT=${WG_LAUNCH_LOG_ROOT:-/srv/logs/wispd}
LOG_DIR="$LOG_ROOT/wg-launch-media-$STAMP"
CORPUS_SRC=${WG_LAUNCH_CORPUS_SRC:-/srv/workspaces/wispd/prof/src}
WG_BIN=${WG_BIN:-/srv/workspaces/wispd/tools/wg-linux/bin/wg}
RG_BIN=${WG_RG:-}
HF_HUB_OFFLINE=${HF_HUB_OFFLINE:-1}
HERO_LIMIT_BYTES=$((8 * 1024 * 1024))
SMALL_LIMIT_BYTES=$((8 * 1024 * 1024))
MOCHA_BASE=0x1e1e2e
MOCHA_CRUST=0x11111b
MOCHA_SURFACE=0x313244
MOCHA_TEXT=0xcdd6f4
MOCHA_BLUE=0x89b4fa

mkdir -p "$RUN"/{raw,w,final,probe,sheets,tapes,tmp} "$LOG_DIR" "$LAUNCH/media"
exec > >(tee -a "$LOG_DIR/render.log") 2>&1

log() {
  printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_file() {
  [[ -f "$1" ]] || die "missing file: $1"
}

ensure_tools() {
  if [[ "${WG_LAUNCH_SKIP_INSTALL:-0}" == 1 ]]; then
    return
  fi
  export DEBIAN_FRONTEND=noninteractive

  if ! command -v ttyd >/dev/null 2>&1; then
    log "installing ttyd"
    sudo apt-get update -qq || true
    if ! sudo apt-get install -y ttyd; then
      log "ttyd is not in apt; downloading the release binary"
      local ttyd_arch ttyd_url
      case "$(uname -m)" in
        x86_64) ttyd_arch='x86_64' ;;
        aarch64|arm64) ttyd_arch='aarch64' ;;
        *) die "unsupported architecture for ttyd: $(uname -m)" ;;
      esac
      ttyd_url=$(curl -fsSL https://api.github.com/repos/tsl0922/ttyd/releases/latest \
        | grep -o "https://[^\"]*ttyd\.${ttyd_arch}" \
        | head -n1)
      [[ -n "$ttyd_url" ]] || die "could not find a ttyd release binary"
      mkdir -p "$RUN_ROOT/tools/bin"
      curl -fsSL "$ttyd_url" -o "$RUN_ROOT/tools/bin/ttyd"
      chmod 0755 "$RUN_ROOT/tools/bin/ttyd"
      export PATH="$RUN_ROOT/tools/bin:$PATH"
    fi
  fi

  if ! command -v ffmpeg >/dev/null 2>&1 || ! command -v ffprobe >/dev/null 2>&1; then
    log "installing ffmpeg"
    sudo apt-get update -qq
    sudo apt-get install -y ffmpeg
  fi

  if ! command -v vhs >/dev/null 2>&1; then
    log "installing vhs"
    local arch asset url tmpdir
    case "$(uname -m)" in
      x86_64) asset='Linux_x86_64' ;;
      aarch64|arm64) asset='Linux_arm64' ;;
      *) die "unsupported architecture for vhs: $(uname -m)" ;;
    esac
    url=$(curl -fsSL https://api.github.com/repos/charmbracelet/vhs/releases/latest \
      | grep -v 'sbom' \
      | grep -o "https://[^\"]*${asset}\.tar\.gz" \
      | head -n1)
    [[ -n "$url" ]] || die "could not find a vhs Linux release asset"
    tmpdir=$(mktemp -d "$RUN/tmp/vhs.XXXXXX")
    curl -fsSL "$url" -o "$tmpdir/vhs.tar.gz"
    tar -xzf "$tmpdir/vhs.tar.gz" -C "$tmpdir"
    local vhs_bin="$tmpdir/vhs"
    if [[ ! -f "$vhs_bin" ]]; then
      vhs_bin=$(find "$tmpdir" -type f -name vhs | head -n1)
    fi
    [[ -n "$vhs_bin" ]] || die "could not find the vhs binary in the release archive"
    mkdir -p "$RUN_ROOT/tools/bin"
    install -m 0755 "$vhs_bin" "$RUN_ROOT/tools/bin/vhs"
    export PATH="$RUN_ROOT/tools/bin:$PATH"
    rm -rf "$tmpdir"
  fi

  # VHS uses a Chromium-compatible browser for terminal capture.
  if ! command -v chromium >/dev/null 2>&1 \
    && ! command -v chromium-browser >/dev/null 2>&1 \
    && ! command -v google-chrome >/dev/null 2>&1; then
    log "installing chromium for vhs"
    sudo apt-get update -qq
    sudo apt-get install -y chromium
  fi

  command -v vhs >/dev/null || die "vhs is not available"
  command -v ttyd >/dev/null || die "ttyd is not available"
  command -v ffmpeg >/dev/null || die "ffmpeg is not available"
  command -v ffprobe >/dev/null || die "ffprobe is not available"
}

check_inputs() {
  require_file "$WG_BIN"
  [[ -x "$WG_BIN" ]] || die "WG_BIN is not executable: $WG_BIN"
  WG_BIN=$(cd "$(dirname "$WG_BIN")" && pwd)/$(basename "$WG_BIN")
  if [[ -n "$WG_RG" && -x "$WG_RG" ]]; then
    RG_BIN="$WG_RG"
  elif [[ -x "$(dirname "$WG_BIN")/rg" ]]; then
    RG_BIN="$(dirname "$WG_BIN")/rg"
  else
    RG_BIN=$(command -v rg || true)
  fi
  [[ -n "$RG_BIN" ]] || die "rg not found"
  [[ -d "$CORPUS_SRC" ]] || die "corpus directory not found: $CORPUS_SRC"
  local count
  count=$(find "$CORPUS_SRC" -type f | wc -l)
  (( count > 1000 )) || die "corpus looks too small: $count files in $CORPUS_SRC"
  log "corpus: $CORPUS_SRC ($count files)"
  log "wg: $WG_BIN"
  command -v python3 >/dev/null || die "python3 is required to read the benchmark JSON"
  TEXT_RATIO=$(python3 -c "import json; d=json.load(open('$ROOT/bench/WG_LAUNCH_BENCH_RESULTS.json')); print(f\"{d['text_absent_literal_rg_p50_ms']/d['text_absent_literal_wg_p50_ms']:.1f}\")")
  log "text absent-literal ratio: ${TEXT_RATIO}x"
  log "run: $RUN"
  log "logs: $LOG_DIR"
}

caption_font() {
  local candidate
  candidate=$(fc-match -f '%{file}' 'DejaVu Sans' 2>/dev/null | head -n1 || true)
  if [[ -n "$candidate" && -f "$candidate" ]]; then
    printf '%s' "$candidate"
    return
  fi
  for candidate in \
    /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
    /usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf; do
    if [[ -f "$candidate" ]]; then
      printf '%s' "$candidate"
      return
    fi
  done
  return 1
}

copy_corpus() {
  local dest=$1
  mkdir -p "$dest"
  if command -v rsync >/dev/null 2>&1; then
    rsync -a --delete "$CORPUS_SRC/" "$dest/"
  else
    cp -a "$CORPUS_SRC/." "$dest/"
  fi
}

CURRENT_WORK=""
stop_service() {
  local work=$1
  if [[ -f "$work/service/service.json" ]]; then
    WG_SERVICE_DIR="$work/service" "$WG_BIN" down >/dev/null 2>&1 || true
  fi
}

cleanup_current() {
  if [[ -n "$CURRENT_WORK" ]]; then
    stop_service "$CURRENT_WORK"
  fi
}
trap cleanup_current EXIT

render_tape() {
  local name=$1 tape=$2 work=$3 corpus=$4 variant=$5
  local raw="$LAUNCH/renders/raw/${name}-${variant}.mp4"
  local tmp_tape=$tape
  local tmp_final="$RUN/tmp/${name}-${variant}-raw.mp4"

  mkdir -p "$LAUNCH/renders/raw"
  rm -f "$raw"


  CURRENT_WORK="$work"
  log "render $name $variant"
  (
    cd "$ROOT"
    WG_BIN="$WG_BIN" \
    WG_LAUNCH_WORK="$work" \
    WG_LAUNCH_CORPUS="$corpus" \
    WG_SERVICE_DIR="$work/service" \
    HF_HUB_OFFLINE="$HF_HUB_OFFLINE" \
    PATH="$(dirname "$RG_BIN"):$(dirname "$WG_BIN"):$PATH" \
      vhs "$tmp_tape" >"$LOG_DIR/vhs-${name}-${variant}.log" 2>&1
  ) || {
    stop_service "$work"
    CURRENT_WORK=""
    die "vhs failed for $name $variant (see $LOG_DIR/vhs-${name}-${variant}.log)"
  }

  stop_service "$work"
  CURRENT_WORK=""

  [[ -f "$raw" ]] || die "vhs produced no output for $name $variant"
  mv -f "$raw" "$tmp_final"
  printf '%s\n' "$tmp_final" >"$RUN/tmp/.last-raw"
}

postprocess() {
  local name=$1 input=$2 output=$3 caption=$4 format=$5
  local font vf caption_escaped caption_size
  caption_escaped="${caption//:/\\:}"
  caption_size=40
  if [[ "$name" == wg-rg-flags ]]; then
    caption_size=36
  fi
  font=$(caption_font || true)

  if [[ -n "$font" && -n "$caption" ]]; then
    vf="scale=1920:1000:force_original_aspect_ratio=decrease,pad=1920:1000:(ow-iw)/2:(oh-ih)/2:color=${MOCHA_BASE},pad=1920:1080:0:0:color=${MOCHA_CRUST},drawbox=x=0:y=1000:w=1920:h=2:color=${MOCHA_BLUE}@0.55:t=fill,drawtext=fontfile=${font}:text='${caption_escaped}':fontcolor=${MOCHA_TEXT}:fontsize=${caption_size}:x=(w-text_w)/2:y=1014"
  else
    vf="scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=${MOCHA_BASE}"
  fi

  ffmpeg -y -v error -i "$input" \
    -vf "$vf" -an -c:v libx264 -preset slow -crf 20 \
    -pix_fmt yuv420p -r 30 -movflags +faststart "$output"
}

probe_output() {
  local file=$1 base
  base=$(basename "$file")
  ffprobe -v error -show_streams -show_format -of json "$file" \
    >"$RUN/probe/$base.ffprobe.json"

  if [[ "$file" == *.png ]]; then
    ffmpeg -y -v error -i "$file" -vf 'scale=480:-1' -frames:v 1 \
      "$RUN/sheets/$base-sheet.png"
    return
  fi

  local duration
  duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$file")
  if awk "BEGIN {exit !($duration > 20)}"; then
    ffmpeg -y -v error -i "$file" \
      -vf 'fps=1,scale=320:-1,tile=6x6' -frames:v 1 \
      "$RUN/sheets/$base-sheet.png"
  else
    ffmpeg -y -v error -i "$file" \
      -vf 'fps=1,scale=480:-1,tile=5x4' -frames:v 1 \
      "$RUN/sheets/$base-sheet.png"
  fi
}

make_card_mp4() {
  local png=$1 out=$2 duration=$3
  ffmpeg -y -v error -loop 1 -i "$png" -t "$duration" \
    -vf 'fps=30,format=yuv420p' -c:v libx264 -preset slow -crf 18 \
    -an -movflags +faststart "$out"
}

ensure_cards() {
  local cards="$LAUNCH/cards"
  mkdir -p "$cards"
  if [[ ! -f "$cards/title-1920x1080.png" || ! -f "$cards/end-1080x1080.png" ]]; then
    if python3 -c 'import PIL' >/dev/null 2>&1; then
      log "generating cards with Pillow"
      python3 "$LAUNCH/make_cards.py" --out-dir "$cards"
    else
      die "card PNGs are missing and Pillow is not installed"
    fi
  fi
  make_card_mp4 "$cards/title-1920x1080.png" "$cards/title-1920x1080.mp4" 2.5
  make_card_mp4 "$cards/end-1920x1080.png" "$cards/end-1920x1080.mp4" 3.0
}

make_chart_clip() {
  local png=$1 out=$2
  local vf="scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:color=${MOCHA_BASE}"
  ffmpeg -y -v error -loop 1 -i "$png" -t 3 \
    -vf "$vf" -an -c:v libx264 -preset slow -crf 20 \
    -pix_fmt yuv420p -r 30 -movflags +faststart "$out"
}

assemble_hero() {
  local list="$RUN/tmp/hero.txt"
  local out="$RUN/final/wg-launch-hero-16x9.mp4"
  local -a segments=()
  local title end chart chart_clip

  title="$LAUNCH/cards/title-1920x1080.mp4"
  end="$LAUNCH/cards/end-1920x1080.mp4"
  segments+=("$title")
  segments+=("$RUN/final/wg-hook-16x9.mp4")
  segments+=("$RUN/final/wg-live-16x9.mp4")
  segments+=("$RUN/final/wg-rg-flags-16x9.mp4")
  segments+=("$RUN/final/wg-multi-repo-16x9.mp4")

  local -a chart_pngs=()
  shopt -s nullglob
  chart_pngs=("$LAUNCH"/charts/*-1920x1080.png)
  if (( ${#chart_pngs[@]} == 0 )); then
    chart_pngs=("$LAUNCH"/charts/*.png)
  fi
  shopt -u nullglob
  for chart in "${chart_pngs[@]}"; do
    chart_clip="$RUN/tmp/chart-$(basename "${chart%.png}").mp4"
    make_chart_clip "$chart" "$chart_clip"
    segments+=("$chart_clip")
  done

  segments+=("$end")

  : >"$list"
  for chart in "${segments[@]}"; do
    printf "file '%s'\n" "$chart" >>"$list"
  done

  local crf size duration
  for crf in 22 24 26 28; do
    log "assemble hero at crf $crf"
    ffmpeg -y -v error -f concat -safe 0 -i "$list" \
      -an -c:v libx264 -preset slow -crf "$crf" \
      -pix_fmt yuv420p -r 30 -movflags +faststart "$out"
    size=$(stat -c%s "$out")
    if (( size <= HERO_LIMIT_BYTES )); then
      break
    fi
  done
  size=$(stat -c%s "$out")
  duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$out")
  log "hero: ${size} bytes, ${duration}s"
  if awk "BEGIN {exit !($duration < 30 || $duration > 50)}"; then
    log "WARN: hero duration is outside the 30-50 s target"
  fi
  if (( size > HERO_LIMIT_BYTES )); then
    log "WARN: hero still exceeds the 8 MB copy limit"
  fi
}

render_variant() {
  local variant=$1
  local base="$RUN/w/$variant"

  log "prepare $variant corpus copies"
  copy_corpus "$base/hook/corpus/rust-src"
  copy_corpus "$base/live/corpus/rust-src"
  rm -rf "$HOME/code/rust" "$HOME/code/api-server" "$HOME/code/service"
  mkdir -p "$HOME/code"
  copy_corpus "$HOME/code/rust"

  local keyword_count
  keyword_count=$(cd "$base/hook/corpus/rust-src" && "$RG_BIN" -il "unary" . | wc -l)
  printf "%s\n" "$keyword_count" >"$RUN/rg-keyword-count-$variant.txt"
  log "rg keyword count ($variant): $keyword_count files"

  render_tape wg-hook "$LAUNCH/demos/hook.tape" \
    "$base/hook" "$base/hook/corpus/rust-src" "$variant"
  postprocess wg-hook "$(cat "$RUN/tmp/.last-raw")" \
    "$RUN/final/wg-hook-$variant.mp4" \
    "rg: ${keyword_count} files mention it. wg: the exact spot." "$variant"

  render_tape wg-live "$LAUNCH/demos/live.tape" \
    "$base/live" "$base/live/corpus/rust-src" "$variant"
  postprocess wg-live "$(cat "$RUN/tmp/.last-raw")" \
    "$RUN/final/wg-live-$variant.mp4" \
    'New function. Next query finds it. No restart.' "$variant"

  render_tape wg-rg-flags "$LAUNCH/demos/rg-flags.tape" \
    "$base/hook" "$base/hook/corpus/rust-src" "$variant"
  postprocess wg-rg-flags "$(cat "$RUN/tmp/.last-raw")" \
    "$RUN/final/wg-rg-flags-$variant.mp4" \
    "Built-in rg 15.1 + trigram index. ${TEXT_RATIO}x faster on rare terms. Same bytes." "$variant"

  render_tape wg-multi-repo "$LAUNCH/demos/multi-repo.tape" \
    "$HOME/code" "$HOME/code/rust" "$variant"
  postprocess wg-multi-repo "$(cat "$RUN/tmp/.last-raw")" \
    "$RUN/final/wg-multi-repo-$variant.mp4" \
    'One daemon. Two repositories.' "$variant"

  rm -rf "$HOME/code/rust" "$HOME/code/api-server" "$HOME/code/service"
  rmdir "$HOME/code" 2>/dev/null || true
}

copy_small_outputs() {
  local file size
  for file in "$RUN/final"/*.mp4; do
    [[ -f "$file" ]] || continue
    size=$(stat -c%s "$file")
    if (( size <= SMALL_LIMIT_BYTES )); then
      cp -f "$file" "$LAUNCH/media/"
      log "copied $(basename "$file") to docs/launch/media"
    else
      log "kept $(basename "$file") on the run disk (${size} bytes)"
    fi
  done
}

write_manifest() {
  local manifest="$RUN/manifest.txt" file size duration streams
  : >"$manifest"
  for file in "$RUN/final"/*.mp4 "$LAUNCH"/cards/*.mp4 "$LAUNCH"/cards/*.png; do
    [[ -f "$file" ]] || continue
    size=$(stat -c%s "$file")
    if [[ "$file" == *.png ]]; then
      duration='-'
      streams=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 "$file")
    else
      duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$file")
      streams=$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height,r_frame_rate,codec_name -of csv=p=0 "$file")
    fi
    printf '%s\t%s\t%s\t%s\n' "$file" "$size" "$duration" "$streams" >>"$manifest"
  done
}

main() {
  check_inputs
  ensure_tools
  ensure_cards

  render_variant 16x9

  assemble_hero

  local file
  for file in "$RUN"/final/*.mp4 "$LAUNCH"/cards/*.mp4; do
    [[ -f "$file" ]] || continue
    probe_output "$file"
  done
  for file in "$LAUNCH"/cards/*.png; do
    [[ -f "$file" ]] || continue
    probe_output "$file"
  done

  copy_small_outputs
  write_manifest
  log "done"
  printf 'run directory: %s\n' "$RUN"
  printf 'logs: %s\n' "$LOG_DIR"
  printf 'small assets copied to: %s\n' "$LAUNCH/media"
  printf 'probe JSON: %s\n' "$RUN/probe"
  printf 'contact sheets: %s\n' "$RUN/sheets"
}

main "$@"
