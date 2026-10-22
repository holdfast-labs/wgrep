# Reproduce the terminal demos

These are real command recordings made with VHS, the Go CLI recorder. The GIFs
are short, loop, and stay under 2 MB so the README stays visual. Playback is 1x;
no clip is sped up. They use the 13.3k-file Rust `src` corpus and a small
two-file `api-server` repository. They are not toy fixtures.

| clip | shows | GIF |
|---|---|---:|
| `quickstart` | `wg up` ready line, semantic ranking, and a ripgrep flag | 1200x700, 12.6 s, 354,046 B |
| `live` | an edit is searchable before the next prompt | 1200x560, 6.2 s, 147,422 B |
| `multi-repo` | two roots behind one daemon and one model | 1200x700, 6.0 s, 281,048 B |

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

The checked-in GIFs use the release binary built from `git archive 4e9a8be`:

- Commit: `4e9a8be`
- Binary SHA-256:
  `f2eb5ca4977c49cbdbaa4ac272afeeab01519c244db03edaa598aeb1fb521581`
- Build: `cargo build --locked --release --bin wg -j2`
- The VPS build target was removed after copying the binary to
  `/srv/projects/wispd/m/bin/wg-demos`.

The visible roots are under `$HOME/code`, so no `/srv/`, `wispd`, or run-id
string appears in a recording:

- `$HOME/code/rust`: copy of the Rust `src` corpus.
- `$HOME/code/live`: copy with an empty `live.rs`.
- `$HOME/code/api-server`: two small Rust files created by `prepare.sh`.

The one-time `wg up` index build is hidden. The quickstart shows the ready line
from a visible `wg up` after the hidden build. The multi-repo status shows
`text: ready [current]` for both roots.

## Render

The checked-in assets are rendered with:

```sh
export PATH=/srv/projects/wispd/wg-launch-media-20261020/tools/bin:$PATH
export WG_DEMO_ROOT=/srv/projects/wispd/m/gif-demos
export WG_BIN=/srv/projects/wispd/m/bin/wg-demos
export WG_DEMO_CORPUS_SRC=/srv/workspaces/wispd/prof/src
export WG_DEMO_HOME=$HOME/code
bash scripts/render_wg_demos.sh
```

Run that from the repository root. Set `WG_DEMO_ONLY=quickstart` to render one clip. The script:

1. copies the corpus to `$HOME/code/rust` and `$HOME/code/live`;
2. renders the tapes at 1200 px wide with the Catppuccin frame;
3. converts each MP4 to a looping GIF at 12 fps with a 128-color palette;
4. audits GIF frames with OCR and fails on `/srv/`, `wispd`, or a run id;
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

The final frames and contact sheets are in
`E:\wg-launch-media\review\gifs\`. Never substitute echoed results or hard-coded
build times.
