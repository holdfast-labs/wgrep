# wg launch media

Launch-grade terminal clips, cards, and chart images for `wg`.
The public URL is `github.com/holdfast-labs/wgrep`. The command is `wg`.
The `wg` crate is at the repository root.

## Status

- Four 16:9 clips and the 16:9 hero are rendered with the final single-binary
  `wg`.
- Four chart cards were generated at 1920x1080 and 1080x1080 (8 PNGs).
- Square terminal clips and the square hero are not produced. The 68-character
  result path is not readable at 1080 square.
- Square charts and square title/end cards remain as standalone PNG images.
- Binary commit: `2cee82c`
- Binary SHA-256:
  `0b95823c1d65c89f864f3f271c5ccdfffa11e2833715782846a0667f1d2ffc7e`
- Render run: `/srv/projects/wispd/wg-media-2cee82c/run/20261022-224247`
- Logs: `/srv/logs/wispd/wg-launch-media-20261022-224247/`
- Final assets are in `media/`, `charts/`, and `cards/`.
- The charts are regenerated from the Phase E results JSON. The rg-flags
  clip, the 16:9 hero, the multi-repo clip, and the README multi-repo GIF and
  MP4 are rendered from commit `2cee82c`. The multi-repo status shows
  `text: ready [current], 13333 files (13290 indexed, 43 unindexed)`.
- `E:\wg-launch-media\review\` has the eight chart PNGs and full-size final
  frames for the four 16:9 clips and the 16:9 hero. The Phase E charts are in
  `E:\wg-launch-media\review\phaseE-charts\`. The clip review frames are in
  `E:\wg-launch-media\review\phaseE-clips3\`.

## Corpus

The clips search a copy of the Rust source tree:

- Windows source: `E:\wgrep-code\rust\src`
- VPS source: `/srv/workspaces/wispd/prof/src`
- Windows copy: 13,439 files, 73.4 MB.
- VPS campaign corpus: 13,448 files, 148.7 MB.

The render script copies the corpus to a scratch directory and builds an
isolated index for each clip. The visible semantic searches are warm. The
one-time `wg up` semantic build is hidden. The multi-repo tape waits in the
hidden setup until `wg status --json` reports `"building":false`.

## Hook question

We used benchmark question M6 q1. On a copy of the render corpus
(`/srv/workspaces/wispd/prof/src`), `wg` top-1 was:

```text
tools/rust-analyzer/crates/parser/src/grammar/expressions.rs:411-412
```

The returned chunk contains the cited line 412. The source line is the
`// parse the interior of the unary expression` comment in the unary
expression parser.

Semantic question used in the hook clip:

```text
How does the parser handle unary expressions in Rust code?
```

Realistic keyword search used in the hook clip:

```console
rg -il "unary" . | wc -l
# 184 files on the render corpus copy
```

The count was measured on a copy of `/srv/workspaces/wispd/prof/src`.
The clip runs the same command on the render corpus. The render script also
writes the render-corpus count to `<run>/rg-keyword-count-16x9.txt`.

We chose q1 because it had a correct top-1 path and the cited line was inside
the returned chunk. The earlier rustdoc question remains the fallback:
`find the rustdoc function that parses command line options` returned
`librustdoc/config.rs:381-384`. The broad version
`where does rustdoc parse command line options` returned a documentation page,
so it was not used.

## Assets

All final files are below 8 MB and are in the repository.

| Asset | Path | Format |
| --- | --- | --- |
| Title card | `cards/title-1920x1080.png`, `cards/title-1920x1080.mp4` | 16:9, 2.5 s |
| Title card | `cards/title-1080x1080.png` | 1:1 PNG |
| End card | `cards/end-1920x1080.png`, `cards/end-1920x1080.mp4` | 16:9, 3 s |
| End card | `cards/end-1080x1080.png` | 1:1 PNG |
| Hook | `media/wg-hook-16x9.mp4` | 16:9, 7.0 s, 81 KB |
| Live edit | `media/wg-live-16x9.mp4` | 16:9, 8.0 s, 78 KB |
| rg flags | `media/wg-rg-flags-16x9.mp4` | 16:9, 10.4 s, 187 KB |
| Two repositories | `media/wg-multi-repo-16x9.mp4` | 16:9, 6.2 s, 142 KB |
| Hero | `media/wg-launch-hero-16x9.mp4` | 16:9, 49.1 s, 703 KB |
| Charts | `charts/01-edit-searchable-*.png` | 16:9 and 1:1 PNG |
| Charts | `charts/02-warm-query-*.png` | 16:9 and 1:1 PNG |
| Charts | `charts/03-fanout-16-*.png` | 16:9 and 1:1 PNG |
| Charts | `charts/04-text-vs-ripgrep-*.png` | 16:9 and 1:1 PNG |

All videos use H.264, `yuv420p`, 30 fps, and `+faststart`.

Card text:

- Title: `wg`, then `Ask in English. Or use rg flags.`
- End: `github.com/holdfast-labs/wgrep`, then:

  ```sh
  cargo install --locked --git \
    https://github.com/holdfast-labs/wgrep --bin wg
  ```

Burned-in captions:

- Hook: `rg: <count> files mention it. wg: the exact spot.` `<count>` is the
  real `rg` count on the render corpus (184 in this run).
- Live edit: `New function. Next query finds it. No restart.`
- rg flags: `Built-in rg 15.1 + trigram index. 10.6x faster on rare terms.
  Same bytes.`
- Two repositories: `One daemon. Two repositories.`

## Build

Build the launch binary from a source export on the VPS:

```sh
git -C /c/git/wispd archive 2cee82c \
  | ssh vps 'mkdir -p /srv/projects/wispd/wg-media-2cee82c/src && \
             tar -C /srv/projects/wispd/wg-media-2cee82c/src -xf -'
ssh vps
source ~/.cargo/env
cd /srv/projects/wispd/wg-media-2cee82c/src
CARGO_TARGET_DIR=/srv/projects/wispd/wg-media-2cee82c/target \
  cargo build --locked --release --bin wg -j2
```

The recorded run used `2cee82c`. Binary SHA-256:
`0b95823c1d65c89f864f3f271c5ccdfffa11e2833715782846a0667f1d2ffc7e`.
The binary was copied to `/srv/projects/wispd/wg-media-2cee82c/bin/wg`, and the
build target was removed after the render.

## Re-render

```sh
export WG_BIN=/srv/projects/wispd/wg-media-2cee82c/bin/wg
export WG_RG=/srv/workspaces/wispd/tools/rg-15/rg
export WG_LAUNCH_CORPUS_SRC=/srv/workspaces/wispd/prof/src
export WG_LAUNCH_ROOT=/srv/projects/wispd/wg-media-2cee82c/run
python3 docs/launch/make_cards.py
python3 docs/launch/make_charts.py
bash scripts/render_launch_media.sh
```

The script:

1. Installs `ttyd`, `ffmpeg`, `vhs`, and Chromium when they are missing.
   Set `WG_LAUNCH_SKIP_INSTALL=1` to skip this step. If the Debian package
   index has no `ttyd`, install the `ttyd` release binary first.
2. Copies the corpus to `$WG_LAUNCH_ROOT/<stamp>/w/` for the hook and
   live clips, and to `$HOME/code/rust` for the multi-repo clip.
3. Renders the four 16:9 tapes and the 16:9 hero.
4. Centers each terminal window in a 1920x1000 area and adds the caption bar.
5. Concatenates title, the hook, live-edit, rg-flag, and multi-repo clips,
   the four 1920x1080 charts (3 s each), and end card.
6. Writes `ffprobe` JSON to `probe/` and contact sheets to `sheets/`.
7. Copies final files below 8 MB to `media/`.

Default locations:

- Run directory: `/srv/projects/wispd/wg-media-2cee82c/run/<stamp>/`
- Logs: `/srv/logs/wispd/wg-launch-media-<stamp>/`
- Probe JSON: `<run>/probe/`
- Contact sheets: `<run>/sheets/`
- Manifest: `<run>/manifest.txt`

The script stops every daemon it starts. Each clip uses its own
`WG_SERVICE_DIR`.

## Tapes

The tapes are in `demos/`:

- `hook.tape`
- `live.tape`
- `rg-flags.tape`
- `multi-repo.tape`
- `prepare.sh`

They use the big Rust corpus for the hook, live edit, and rg-flag clips.
The two-repository clip attaches a small generated second repository to the
same daemon. It uses `/home/agent/code/rust` and `/home/agent/code/api-server`
so `wg status` paths stay short and fit without `fold`. The multi-repo
status shows `text: ready [current]` for both roots.

The live-edit clip starts with an empty `live.rs`. It runs
`wg --no-heading -n -F round_secs .` (no match), appends
`pub fn round_secs(t:f64)->u64{t.round() as u64}` in one visible `echo`, then
runs the same search and gets `./live.rs:1:...`. The optional semantic query
was tested and dropped: top-1 was a clippy duration test, not the new function.

The rg-flags clip uses `export P=zzqxjwvbnm_wg_text_absent_zzz`, one visible
`wg -F $P .` warmup, then `time rg -F $P .` and `time wg -F $P .`. It ends with
`wg --no-heading -n -F "struct Flags" .` and real output. The caption uses the
benchmark p50 ratio from the JSON (10.6x on the rare-identifier case). The
visible one-shot times vary; the warmup makes the timed `wg` query warm.

All four tapes are 16:9. They use font size 43, except the multi-repo clip at
41 for the longer text-index status line. The heights are 600 (hook), 560
(live), 900 (rg flags), and 820 (multi). The render script centers each window
in the 1920x1000 area above the caption bar.

Cards are generated by `make_cards.py` with Pillow. The card and chart scripts
need Pillow; the render script uses the checked-in PNGs. The square cards are
PNG only. Rebuild the cards with:

```sh
python3 docs/launch/make_cards.py
```

## Charts

`make_charts.py` reads `bench/WG_LAUNCH_BENCH_RESULTS.json` and writes four
chart cards at 1920x1080 and 1080x1080:

- `01-edit-searchable-*`: one-file edit to first correct response.
- `02-warm-query-*`: warm semantic p50 and top-10 parity.
- `03-fanout-16-*`: 16 concurrent clients.
- `04-text-vs-ripgrep-*`: grouped text timings for five cases.

Values from the results JSON:

- Edit to searchable: `99.2 ms` vs `21,806 ms` (21.8 s), `220x faster`.
- Warm query: `25.2 ms` vs `565.7 ms` p50, `22.4x faster`,
  `identical top-10 results (5/5 queries)`.
- 16 agents: `140.9 q/s` vs `5.5 q/s`, `25x throughput`, `0 errors`.
- Text search: absent `5.4 ms` vs `63.2 ms`, rare `6.0` vs `63.8`,
  common `50.0` vs `83.7`, broad `111.5` vs `136.1`,
  case-insensitive `37.5` vs `75.7`; `10.6x faster on rare terms`,
  `byte-identical output`.

The 16:9 hero uses `charts/*-1920x1080.png` as full-frame 3 s segments.
The 1080x1080 charts are standalone tweet images and are not in the hero.
Rebuild the charts with:

```sh
python3 docs/launch/make_charts.py
```

The script fails if a label contains mojibake characters.

The p50 charts (01 and 02) carry the caveat:
`p50 · Rust src, 13.3k files · 4 vCPU VPS · sgrep CLI rev 0fbe8e5 (upstream project wg is derived from)`.
The fan-out chart (03) carries:
`10 s window, 16 concurrent clients · Rust src, 13.3k files · 4 vCPU VPS · sgrep CLI rev 0fbe8e5 (upstream project wg is derived from)`.
The edit chart also says `sgrep has no watcher; it rebuilds the index`.

The text chart carries:
`rg 15.1 with default threads; wg uses its built-in ripgrep 15.1 plus a trigram index; 13.3k-file Rust src; 4 vCPU VPS`.
The 16:9 text chart uses the same title and legend top padding as the other
16:9 charts. The square text chart uses the same top padding as the other
square charts.

## Honest caveats

- The clips use cached model and parser assets. `HF_HUB_OFFLINE=1` is the
  default. A cold cache can download about 58 MB in total on the first run. The
  corpus contains Python files, so the Python parser must be cached. If it is
  not, run once with `HF_HUB_OFFLINE=0`.
- The `wg up` index build is hidden. The tapes wait for the `wg up` PID line
  before `Show`, so the visible part starts after setup. The visible searches
  are warm. Do not read the clip timing as setup or benchmark numbers.
- Playback speed is 1x. No clip is sped up.
- Semantic results are ranked, not exhaustive. The text route uses the
  built-in ripgrep 15.1 and trigram index. External rg 15.1 is used only for
  the comparison and the keyword count.
- The charts use the single final benchmark campaign. No memory chart is
  included. At five attached 13.3k-file roots, one `wg` daemon uses 902.9 MiB
  against 1,034.1 MiB for five warm sgrep processes (12.7% less). This is not
  general: at one root `wg` uses 1.13x more, and at three roots 1.21x more. The
  sgrep value is the sum of live processes, not a resident server.
- The rg-flags clip uses one-shot shell `time`; the caption uses the benchmark
  p50 ratio from the JSON. The visible times can differ from the p50.
- The live-edit clip uses exact text search for the freshness proof. The
  optional semantic query was tested and dropped because the new function was
  not top-1.
- The hook answer was checked on a copy of the render corpus. The `wg` top-1
  result for M6 q1 was
  `tools/rust-analyzer/crates/parser/src/grammar/expressions.rs:411-412`.
  Recheck if the corpus or model changes.
- Windows VHS did not produce output on this machine. All videos render on the
  Debian VPS.
- Square video was dropped because the 68-character result path was not
  readable at 1080 square. Square charts and square title/end cards remain
  PNG images.
- The end card uses the real repository URL. Replace it if the URL changes.
