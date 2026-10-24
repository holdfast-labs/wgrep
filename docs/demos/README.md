# Reproduce the terminal demos

These are real command recordings made with VHS, the Go CLI recorder. The GIFs
are short, loop, and stay under 2 MB so the README stays visual. Playback is 1x;
no clip is sped up. They use the 13.3k-file Rust `src` corpus and a small
two-file `api-server` repository. They are not toy fixtures.

| clip | shows | GIF |
|---|---|---:|
| `quickstart` | `wg up` ready line, semantic ranking, and a ripgrep flag | 1200x314, 10.8 s, 188,334 B |
| `live` | an edit is searchable before the next prompt | 1200x178, 6.1 s, 138,275 B |
| `multi-repo` | two roots behind one daemon and one model | 1200x700, 6.0 s, 285,290 B |

The README captions are:

- `quickstart`: *Ask in plain English, or use a ripgrep flag.*
- `live`: *No restart. The edit is searchable before the next prompt returns.*
- `multi-repo`: *One daemon, one model, two repositories.*

A `wg-text-speed` clip was tested and dropped. The trial's one-shot `time wg`
result was around 58 ms, against around 80 ms for `time rg`. A follow-up with
a warmup query measured 6 ms, and `WGREP_INDEX_EVENTS` reported
`used=daemon`, `reason=null`. The trial was not an index fallback. The sequence
was not stable enough for a README clip, so the timed comparison remains in the
launch rg-flags clip.

## Binary and corpus

The quickstart and live GIFs and MP4s use the release binary built from
`git archive v0.1.0`:

- Tag: `v0.1.0`
- Commit: `422eefe`
- Binary SHA-256:
  `88df12947478fc32539d8bfc9a537aee6642ae60a27c6e63408cd444ab8a5aa0`
- Build: `cargo build --locked --release --bin wg -j2`
- The build target was removed after the binary was copied to a stable path.

The multi-repo GIF and MP4 use the release binary built from `git archive
2cee82c`:

- Commit: `2cee82c`
- Binary SHA-256:
  `0b95823c1d65c89f864f3f271c5ccdfffa11e2833715782846a0667f1d2ffc7e`
- Build: `cargo build --locked --release --bin wg -j2`
- The build target was removed after the binary was copied to a stable path.

The visible roots are under `$HOME/code`, so no build path or run-id string
appears in a recording:

- `$HOME/code/rust`: copy of the Rust `src` corpus.
- `$HOME/code/live`: copy with an empty `live.rs`.
- `$HOME/code/api-server`: two small Rust files created by `prepare.sh`.

The one-time `wg up` index build is hidden. The quickstart shows the ready line
from a visible `wg up` after the hidden build. That visible `wg up` still shows
its own short `Preparing wg index` step. The multi-repo status shows
`text: ready [current]` for both roots.

## Render

The checked-in assets are rendered with:

```sh
export PATH=/path/to/vhs/tools/bin:$PATH
export WG_DEMO_ROOT=/tmp/wg-demo-v010
export WG_BIN=/path/to/wg
export WG_DEMO_CORPUS_SRC=/path/to/rust/src
export WG_DEMO_HOME=$HOME/code
bash scripts/render_wg_demos.sh
```

Run that from the repository root. Set `WG_DEMO_ONLY=quickstart` to render one
clip. Set `WG_DEMO_ROOT` to a task-owned directory on local disk; `/tmp` is
known to work. The script:

1. copies the corpus to `$HOME/code/rust` and `$HOME/code/live`;
2. renders the tapes at 1200 px wide with the Catppuccin frame; the quickstart
   and live tapes use tight canvases (1200x314 and 1200x178) and no window bar;
3. converts each MP4 to a looping GIF at 12 fps with a 128-color palette;
4. audits GIF frames with OCR and fails on a build path or a run id;
5. writes the GIFs and MP4s to `docs/media`;
6. writes final frames and contact sheets to `<run>/review`;
7. stops every daemon and removes `$HOME/code`.

Requirements: VHS 0.12.1, ttyd 1.7.7, ffmpeg, Chromium, DejaVu Sans Mono, and
tesseract. Cache the model and parser assets once before recording; the tapes
run offline.

The tapes are in this directory:

- `quickstart.tape`
- `live.tape`
- `multi-repo.tape`
- `prepare.sh`

`prepare.sh` expects `WG_DEMO_WORK`, `WG_DEMO_CORPUS`, and `WG_BIN`. It creates
the second repository when `WG_DEMO_SECOND` is set, and empties `live.rs` when
`WG_DEMO_EMPTY_LIVE=1`.

The final frames and contact sheets are in `<run>/review`. The README key
frames (quickstart first, middle, and last; live last) are selected from those
files. Never substitute echoed results or hard-coded build times.
