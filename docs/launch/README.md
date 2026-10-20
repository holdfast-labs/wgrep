# wg launch media

Launch-grade terminal clips, cards, and chart images for `wg`.
The public URL is `github.com/holdfast-labs/wgrep`. The command is `wg`.
The `wg` crate is at the repository root.

## Status

- Four 16:9 clips and the 16:9 hero are rendered.
- Square terminal clips and the square hero are not produced. The 68-character
  result path is not readable at 1080 square.
- Square charts and square title/end cards remain as standalone PNG images.
- Three chart cards were generated at 1920x1080 and 1080x1080.
- Render run: `/srv/projects/wispd/m/20261020-080210`
- Logs: `/srv/logs/wispd/wg-launch-media-20261020-080210/`
- Final assets are in `media/`, `charts/`, and `cards/`.
- `E:\wg-launch-media\review\` has the six chart PNGs and full-size final
  frames for the four 16:9 clips and the 16:9 hero.

## Corpus

The clips search a copy of the Rust source tree:

- Windows source: `E:\wgrep-code\rust\src`
- VPS source: `/srv/workspaces/wispd/prof/src`
- Windows copy: 13,439 files, 73.4 MB.

The render script copies the corpus to a scratch directory and builds an
isolated index for each clip. The visible part of each clip is warm. The
one-time `wg up` index build is hidden.

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
| Live edit | `media/wg-live-16x9.mp4` | 16:9, 7.8 s, 73 KB |
| rg flags | `media/wg-rg-flags-16x9.mp4` | 16:9, 8.1 s, 91 KB |
| Two repositories | `media/wg-multi-repo-16x9.mp4` | 16:9, 6.1 s, 103 KB |
| Hero | `media/wg-launch-hero-16x9.mp4` | 16:9, 43.6 s, 528 KB |
| Charts | `charts/01-edit-searchable-*.png` | 16:9 and 1:1 PNG |
| Charts | `charts/02-warm-query-*.png` | 16:9 and 1:1 PNG |
| Charts | `charts/03-fanout-16-*.png` | 16:9 and 1:1 PNG |

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
- rg flags: `The rg flags you know. One shared daemon.`
- Two repositories: `One daemon. Two repositories.`

## Re-render

On the VPS, extract the Linux PREVIEW archive and use its `bin/wg`:

```sh
export WG_BIN=/srv/projects/wispd/wg-release-20261018/crates/wgrep-sgrep-core/target/release/wg
export WG_RG=/srv/workspaces/wispd/tools/rg-15/rg
export WG_LAUNCH_CORPUS_SRC=/srv/workspaces/wispd/prof/src
export WG_LAUNCH_ROOT=/srv/projects/wispd/m
python3 docs/launch/make_cards.py
python3 docs/launch/make_charts.py
bash scripts/render_launch_media.sh
```

The final run used the release-tree `wg` build with SHA-256
`b6bf371c7a3be0c7eef6cb5128cacf9f1372126018af7a94cf8759d1223b1fcc`.

The script:

1. Installs `ttyd`, `ffmpeg`, `vhs`, and Chromium when they are missing.
   Set `WG_LAUNCH_SKIP_INSTALL=1` to skip this step. If the Debian package
   index has no `ttyd`, install the `ttyd` release binary first.
2. Copies the corpus to `$WG_LAUNCH_ROOT/<stamp>/w/` for the hook and
   live clips, and to `$HOME/code/rust` for the multi-repo clip.
3. Renders the four 16:9 tapes and the 16:9 hero.
4. Centers each terminal window in a 1920x1000 area and adds the caption bar.
5. Concatenates title, the hook, live-edit, rg-flag, and multi-repo clips,
   the three 1920x1080 charts (3 s each), and end card.
6. Writes `ffprobe` JSON to `probe/` and contact sheets to `sheets/`.
7. Copies final files below 8 MB to `media/`.

Default locations:

- Run directory: `/srv/projects/wispd/m/<stamp>/`
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
so `wg status` paths stay short and fit without `fold`.

The live-edit clip starts with an empty `live.rs`. It runs
`wg -n -F round_secs .` (no match), appends
`pub fn round_secs(t:f64)->u64{t.round() as u64}` in one visible `echo`, then
runs the same search and gets `./live.rs:1:...`. The optional semantic query
was tested and dropped: top-1 was a clippy duration test, not the new function.

All four tapes are 16:9. They use font size 43, padding, margin, rounded
corners, and a colorful window bar. Font 43 is the largest size that keeps the
70-character query line on one line; 48 wraps. Each tape has its own height
(600, 560, 620, and 820) so the window fits the content. The render script
centers each window in the 1920x1000 area above the caption bar.

Cards are generated by `make_cards.py` with Pillow. The script uses the
Catppuccin Mocha palette. The card and chart scripts need Pillow; the render
script uses the checked-in PNGs. The square cards are PNG only. Rebuild the
cards with:

```sh
python3 docs/launch/make_cards.py
```

## Charts

`make_charts.py` reads `bench/WG_LAUNCH_BENCH_RESULTS.json` and writes three
chart cards at 1920x1080 and 1080x1080:

- `01-edit-searchable-*`: one-file edit to first correct response.
- `02-warm-query-*`: warm semantic p50 and top-10 parity.
- `03-fanout-16-*`: 16 concurrent clients.

Values from the results JSON:

- Edit to searchable: `0.22 s` vs `20.2 s`, `91x faster`.
- Warm query: `147 ms` vs `495 ms` p50, `3.4x faster`,
  `identical top-10 results (5/5 queries)`.
- 16 agents: `24.1 q/s` vs `6.2 q/s`, `3.9x throughput`, `0 errors`.

The 16:9 hero uses `charts/*-1920x1080.png` as full-frame 3 s segments.
The 1080x1080 charts are standalone tweet images and are not in the hero.
Rebuild the charts with:

```sh
python3 docs/launch/make_charts.py
```

The script fails if a label contains mojibake characters.

Every chart carries the caveat footnote:
`p50 | Rust src, 13.3k files | 4 vCPU VPS | sgrep CLI rev 0fbe8e5 (upstream project wg is derived from)`.
The edit chart also says `sgrep has no watcher; it rebuilds the index`.

## Honest caveats

- The clips use cached model and parser assets. `HF_HUB_OFFLINE=1` is the
  default. A cold cache can download about 33.5 MB on the first run. The
  corpus contains Python files, so the Python parser must be cached. If it is
  not, run once with `HF_HUB_OFFLINE=0`.
- The `wg up` index build is hidden. The tapes wait for the `wg up` PID line
  before `Show`, so the visible part starts after setup. The visible searches
  are warm. Do not read the clip timing as setup or benchmark numbers.
- Playback speed is 1x. No clip is sped up. If a future edit uses a speed-up,
  the label must appear on screen.
- Semantic results are ranked, not exhaustive. The rg route runs a real
  `rg` child and does not use the semantic index.
- The charts use the single final benchmark campaign. No memory chart is
  included because M4 does not favor wg.
- The live-edit clip uses exact text search for the freshness proof. The
  optional semantic query was tested and dropped because the new function was
  not top-1.
- The hook answer was checked on a copy of the render corpus. The `wg` top-1
  result for M6 q1 was `tools/rust-analyzer/crates/parser/src/grammar/expressions.rs:411-412`.
  Recheck if the corpus or model changes.
- Windows VHS did not produce output on this machine. All videos render on the
  Debian VPS.
- Square video was dropped because the 68-character result path was not
  readable at 1080 square. Square charts and square title/end cards remain
  PNG images.
- The end card uses the real repository URL. Replace it if the URL changes.
