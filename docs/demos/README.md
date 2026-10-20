# Reproduce the terminal demos

These are real command recordings made with VHS, the Go CLI recorder. The
clips are short and loop cleanly so the README stays visual. Playback is 1.0;
no clip is sped up. They use small generated repositories and cached code-model
assets. They are onboarding examples, not Rust-corpus timing benchmarks.

| clip | shows |
|---|---|
| `quickstart` | `wg up`, semantic ranking, and a ripgrep flag |
| `live` | an edit is searchable before the next prompt |
| `multi-repo` | two repositories behind one daemon and one model |

Requirements: VHS 0.12.1, ttyd 1.7.7, ffmpeg, Chromium, JetBrains Mono, the
current `wg` binary and rg 15.1.0 on `PATH`. Cache the verified code-model
assets once before recording; the tapes default to offline operation.

The checked-in assets are rendered with:

```sh
export WG_DEMO_ROOT=/tmp/wg-demos
export WG_BIN=/path/to/wg
scripts/render_wg_demos.sh
```

Run that from the repository root. The script renders each clip into
`docs/media`, checks the MP4 metadata, and writes end frames and contact sheets
to `WG_DEMO_ROOT` for review. It uses a short task-owned work directory under
`/tmp` (override with `WG_DEMO_WORK_ROOT`) so recorded root paths stay readable,
and it stops the task-owned daemon on exit.

To render one clip by hand, use a fresh scratch directory:

```sh
export WG_DEMO_WORK=/tmp/wg-demo-quickstart
export WG_SERVICE_DIR="$WG_DEMO_WORK/service"
vhs docs/demos/quickstart.tape
```

Set `WG_DEMO_WORK`, `TMPDIR`, model caches, and service state to E: for local
Windows work. The release recordings run on the authorized Linux VPS, then the
reviewed assets are copied to the release workspace.

The helper writes only its generated repositories inside `WG_DEMO_WORK`.
Each tape stops its isolated service. The outer recording runner must also
stop that service if recording fails.

VHS outputs GIF and MP4 under `docs/media`. Inspect the end frames, contact
sheets, and `ffprobe` metadata in `WG_DEMO_ROOT` before linking an asset in the
README. Never substitute echoed results or hard-coded build times.
