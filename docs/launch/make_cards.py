#!/usr/bin/env python3
"""Generate the wg launch title and end cards.

The cards use the Catppuccin Mocha palette. The script writes PNG files and
2.5 s / 3 s H.264 MP4 files for the 16:9 and 1:1 formats.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

MOCHA = {
    "base": "#1e1e2e",
    "mantle": "#181825",
    "crust": "#11111b",
    "surface0": "#313244",
    "surface1": "#45475a",
    "text": "#cdd6f4",
    "subtext1": "#bac2de",
    "subtext0": "#a6adc8",
    "overlay0": "#6c7086",
    "blue": "#89b4fa",
    "lavender": "#b4befe",
    "mauve": "#cba6f7",
    "green": "#a6e3a1",
    "peach": "#fab387",
    "teal": "#94e2d5",
}

SANS_CANDIDATES = [
    r"C:\Windows\Fonts\segoeui.ttf",
    r"C:\Windows\Fonts\arial.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
]

MONO_CANDIDATES = [
    r"C:\Windows\Fonts\CascadiaMono.ttf",
    r"C:\Windows\Fonts\CascadiaCode.ttf",
    r"C:\Windows\Fonts\consola.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
]

REPO_URL = "github.com/holdfast-labs/wgrep"
INSTALL_LINES = [
    "cargo install --locked --git \\",
    "  https://github.com/holdfast-labs/wgrep wgrep --bin wg",
]


def find_font(candidates: list[str]) -> str | None:
    for candidate in candidates:
        if Path(candidate).exists():
            return candidate
    return None


def font(path: str | None, size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    if path:
        return ImageFont.truetype(path, size)
    return ImageFont.load_default()


def text_width(draw: ImageDraw.ImageDraw, text: str, fnt) -> float:
    return draw.textlength(text, font=fnt)


def draw_centered(
    draw: ImageDraw.ImageDraw,
    text: str,
    fnt,
    y: int,
    fill: str,
    width: int,
) -> None:
    w = text_width(draw, text, fnt)
    draw.text(((width - w) / 2, y), text, font=fnt, fill=fill)


def background(width: int, height: int) -> Image.Image:
    image = Image.new("RGB", (width, height), MOCHA["base"])
    draw = ImageDraw.Draw(image)

    # Soft glow behind the panel.
    glow = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    gdraw = ImageDraw.Draw(glow)
    size = int(min(width, height) * 0.75)
    gdraw.ellipse(
        (
            (width - size) // 2,
            (height - size) // 2 - height // 10,
            (width + size) // 2,
            (height + size) // 2 - height // 10,
        ),
        fill=(137, 180, 250, 34),
    )
    glow = glow.filter(ImageFilter.GaussianBlur(radius=90))
    image = Image.alpha_composite(image.convert("RGBA"), glow).convert("RGB")
    draw = ImageDraw.Draw(image)

    # Faint grid.
    step = 80
    grid = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    gdraw = ImageDraw.Draw(grid)
    for x in range(0, width, step):
        gdraw.line((x, 0, x, height), fill=(69, 71, 90, 22), width=1)
    for y in range(0, height, step):
        gdraw.line((0, y, width, y), fill=(69, 71, 90, 22), width=1)
    image = Image.alpha_composite(image.convert("RGBA"), grid).convert("RGB")
    return image


def panel(draw: ImageDraw.ImageDraw, width: int, height: int, margin: int) -> None:
    draw.rounded_rectangle(
        (margin, margin, width - margin, height - margin),
        radius=28,
        fill=MOCHA["mantle"],
        outline=MOCHA["surface0"],
        width=2,
    )


def render_title(width: int, height: int, out: Path) -> None:
    image = background(width, height)
    draw = ImageDraw.Draw(image)
    margin = int(width * 0.045)
    panel(draw, width, height, margin)

    sans = find_font(SANS_CANDIDATES)
    mono = find_font(MONO_CANDIDATES)

    if width >= 1600:
        label_size, mark_size, tag_size, note_size = 26, 300, 54, 28
    else:
        label_size, mark_size, tag_size, note_size = 22, 220, 40, 22
    label_font = font(mono, label_size)
    mark_font = font(mono, mark_size)
    tag_font = font(sans, tag_size)
    note_font = font(mono, note_size)

    draw_centered(draw, "ONE COMMAND", label_font, int(height * 0.20), MOCHA["subtext0"], width)
    draw_centered(draw, "wg", mark_font, int(height * 0.27), MOCHA["blue"], width)
    draw_centered(
        draw,
        "Ask in English. Or use rg flags.",
        tag_font,
        int(height * 0.62),
        MOCHA["text"],
        width,
    )

    line_width = int(width * 0.094)
    line_y = int(height * 0.72)
    draw.rounded_rectangle(
        ((width - line_width) // 2, line_y, (width + line_width) // 2, line_y + max(2, width // 480)),
        radius=2,
        fill=MOCHA["mauve"],
    )

    draw_centered(
        draw,
        "semantic ranking + ripgrep, one daemon",
        note_font,
        int(height * 0.78),
        MOCHA["subtext0"],
        width,
    )
    image.save(out)


def render_end(width: int, height: int, out: Path) -> None:
    image = background(width, height)
    draw = ImageDraw.Draw(image)
    margin = int(width * 0.045)
    panel(draw, width, height, margin)

    sans = find_font(SANS_CANDIDATES)
    mono = find_font(MONO_CANDIDATES)

    if width >= 1600:
        mark_size, url_size, install_size, note_size = 96, 62, 34, 28
    else:
        mark_size, url_size, install_size, note_size = 120, 44, 26, 24
    mark_font = font(mono, mark_size)
    url_font = font(mono, url_size)
    install_font = font(mono, install_size)
    note_font = font(sans, note_size)

    if width >= 1600:
        mark_y, url_y, line_y = 0.18, 0.34, 0.46
        install_y1, install_y2, note_y = 0.54, 0.60, 0.74
    else:
        mark_y, url_y, line_y = 0.20, 0.36, 0.48
        install_y1, install_y2, note_y = 0.56, 0.62, 0.78

    draw_centered(draw, "wg", mark_font, int(height * mark_y), MOCHA["blue"], width)
    draw_centered(draw, REPO_URL, url_font, int(height * url_y), MOCHA["lavender"], width)

    line_width = int(width * 0.125)
    line_y_px = int(height * line_y)
    draw.rounded_rectangle(
        ((width - line_width) // 2, line_y_px, (width + line_width) // 2, line_y_px + max(2, width // 640)),
        radius=2,
        fill=MOCHA["mauve"],
    )

    draw_centered(draw, INSTALL_LINES[0], install_font, int(height * install_y1), MOCHA["text"], width)
    draw_centered(draw, INSTALL_LINES[1], install_font, int(height * install_y2), MOCHA["text"], width)

    draw_centered(
        draw,
        "Linux, Windows and macOS",
        note_font,
        int(height * note_y),
        MOCHA["green"],
        width,
    )
    image.save(out)


def make_mp4(png: Path, out: Path, duration: float) -> None:
    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        raise SystemExit("ffmpeg is required to write card MP4 files")
    subprocess.run(
        [
            ffmpeg,
            "-y",
            "-v",
            "error",
            "-loop",
            "1",
            "-i",
            str(png),
            "-t",
            str(duration),
            "-vf",
            "fps=30,format=yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "slow",
            "-crf",
            "18",
            "-an",
            "-movflags",
            "+faststart",
            str(out),
        ],
        check=True,
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out-dir",
        default=str(Path(__file__).resolve().parent / "cards"),
        help="Directory for the generated cards.",
    )
    parser.add_argument("--no-mp4", action="store_true", help="Write PNG files only.")
    args = parser.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    sizes = [(1920, 1080, "1920x1080"), (1080, 1080, "1080x1080")]
    for width, height, label in sizes:
        title_png = out_dir / f"title-{label}.png"
        end_png = out_dir / f"end-{label}.png"
        render_title(width, height, title_png)
        render_end(width, height, end_png)
        print(f"wrote {title_png}")
        print(f"wrote {end_png}")
        if not args.no_mp4 and label == "1920x1080":
            make_mp4(title_png, out_dir / f"title-{label}.mp4", 2.5)
            make_mp4(end_png, out_dir / f"end-{label}.mp4", 3.0)
            print(f"wrote title-{label}.mp4 and end-{label}.mp4")


if __name__ == "__main__":
    main()
