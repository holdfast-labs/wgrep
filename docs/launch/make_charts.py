#!/usr/bin/env python3
"""Generate the wg launch chart cards from the benchmark results JSON.

The script reads bench/WG_LAUNCH_BENCH_RESULTS.json. It never hard-codes the
measured numbers, so a new campaign can regenerate the charts directly.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from PIL import ImageDraw

sys.path.insert(0, str(Path(__file__).resolve().parent))

from make_cards import (  # noqa: E402
    MOCHA,
    SANS_CANDIDATES,
    MONO_CANDIDATES,
    background,
    find_font,
    font,
    panel,
    text_width,
)

DEFAULT_RESULTS = Path(__file__).resolve().parents[2] / "bench" / "WG_LAUNCH_BENCH_RESULTS.json"
DEFAULT_OUT = Path(__file__).resolve().parent / "charts"

FOOTNOTE = (
    "p50 \u00b7 Rust src, 13.3k files \u00b7 4 vCPU VPS \u00b7 "
    "sgrep CLI rev 0fbe8e5 (upstream project wg is derived from)"
)
FRESHNESS_EXTRA = "sgrep has no watcher; it rebuilds the index"


def assert_clean(text: str) -> str:
    for bad in ("\u00c3", "\u00e2\u20ac", "\ufffd"):
        if bad in text:
            raise SystemExit(f"mojibake in chart label: {text!r}")
    return text


def wrap_text(draw: ImageDraw.ImageDraw, text: str, fnt, max_width: float) -> list[str]:
    lines: list[str] = []
    current = ""
    for word in text.split():
        candidate = f"{current} {word}".strip()
        if current and draw.textlength(candidate, font=fnt) > max_width:
            lines.append(current)
            current = word
        else:
            current = candidate
    if current:
        lines.append(current)
    return lines


def make_chart(
    spec: dict,
    width: int,
    height: int,
    out: Path,
    sans: str | None,
    mono: str | None,
) -> None:
    image = background(width, height)
    draw = ImageDraw.Draw(image)
    margin = int(width * 0.045)
    panel(draw, width, height, margin)

    square = width < 1600
    if square:
        pad_x = int(width * 0.075)
        title_size, number_size, suffix_size = 38, 150, 62
        sub_size, row_label_size, value_size = 30, 34, 32
        bar_x = int(width * 0.235)
        bar_max = int(width - bar_x - int(width * 0.075))
        bar_h, bar_gap = 88, 72
        title_y, number_y, sub_y = int(height * 0.06), int(height * 0.12), int(height * 0.29)
        first_bar_y = int(height * 0.42)
        extra_y, foot_y = int(height * 0.72), int(height * 0.78)
        foot_size, extra_size = 20, 22
    else:
        pad_x = int(width * 0.078)
        title_size, number_size, suffix_size = 46, 170, 70
        sub_size, row_label_size, value_size = 36, 44, 42
        bar_x = int(width * 0.22)
        bar_max = int(width - bar_x - int(width * 0.08))
        bar_h, bar_gap = 72, 52
        title_y, number_y, sub_y = int(height * 0.11), int(height * 0.18), int(height * 0.36)
        first_bar_y = int(height * 0.49)
        extra_y, foot_y = int(height * 0.76), int(height * 0.82)
        foot_size, extra_size = 26, 30

    title_font = font(sans, title_size)
    number_font = font(mono, number_size)
    suffix_font = font(mono, suffix_size)
    sub_font = font(sans, sub_size)
    row_font = font(mono, row_label_size)
    value_font = font(mono, value_size)
    extra_font = font(sans, extra_size)
    foot_font = font(sans, foot_size)

    draw.text((pad_x, title_y), spec["title"], font=title_font, fill=MOCHA["subtext0"])

    number = spec["headline_number"]
    suffix = spec["headline_suffix"]
    number_w = text_width(draw, number, number_font)
    draw.text((pad_x, number_y), number, font=number_font, fill=MOCHA["blue"])
    suffix_y = number_y + int(number_size * 0.54)
    draw.text((pad_x + number_w + int(width * 0.018), suffix_y), suffix, font=suffix_font, fill=MOCHA["text"])

    sub_color = spec.get("sub_color", MOCHA["text"])
    draw.text((pad_x, sub_y), spec["subline"], font=sub_font, fill=sub_color)

    max_value = max(spec["wg"], spec["sgrep"])
    rows = [
        ("wg", spec["wg"], spec["wg_label"], MOCHA["blue"], MOCHA["text"]),
        ("sgrep", spec["sgrep"], spec["sgrep_label"], MOCHA["overlay0"], MOCHA["subtext1"]),
    ]

    zero_top = first_bar_y - int(bar_h * 0.35)
    zero_bottom = first_bar_y + bar_gap + bar_h + int(bar_h * 0.35)
    draw.line((bar_x, zero_top, bar_x, zero_bottom), fill=MOCHA["surface1"], width=2)

    for index, (label, value, value_label, bar_color, label_color) in enumerate(rows):
        y = first_bar_y + index * (bar_h + bar_gap)
        draw.text((pad_x, y + int(bar_h * 0.12)), label, font=row_font, fill=label_color)
        bar_w = max(1, int(bar_max * value / max_value))
        radius = min(12, max(2, bar_w // 2), bar_h // 2)
        draw.rounded_rectangle(
            (bar_x, y, bar_x + bar_w, y + bar_h),
            radius=radius,
            fill=bar_color,
        )
        if bar_w > int(bar_max * 0.22):
            value_color = MOCHA["crust"] if bar_color in (MOCHA["blue"], MOCHA["overlay0"]) else MOCHA["text"]
            value_w = text_width(draw, value_label, value_font)
            draw.text(
                (bar_x + bar_w - value_w - int(width * 0.012), y + int(bar_h * 0.16)),
                value_label,
                font=value_font,
                fill=value_color,
            )
        else:
            draw.text(
                (bar_x + bar_w + int(width * 0.012), y + int(bar_h * 0.16)),
                value_label,
                font=value_font,
                fill=MOCHA["text"],
            )

    direction = spec.get("direction", "")
    if direction:
        draw.text(
            (bar_x, zero_top - int(bar_h * 0.55)),
            direction,
            font=font(sans, max(20, int(foot_size * 1.05))),
            fill=MOCHA["overlay0"],
        )

    y = extra_y
    if spec.get("extra"):
        for line in wrap_text(draw, spec["extra"], extra_font, width - 2 * pad_x):
            draw.text((pad_x, y), line, font=extra_font, fill=MOCHA["peach"])
            y += extra_font.size + 6

    foot_lines = wrap_text(draw, FOOTNOTE, foot_font, width - 2 * pad_x)
    y = max(y, foot_y)
    for line in foot_lines:
        draw.text((pad_x, y), line, font=foot_font, fill=MOCHA["subtext0"])
        y += foot_font.size + 4

    image.save(out)


def build_specs(results: dict) -> list[dict]:
    freshness_wg_s = results["freshness_wg_p50_ms"] / 1000.0
    freshness_sgrep_s = results["freshness_sgrep_p50_ms"] / 1000.0
    freshness_ratio = results["freshness_speedup_sgrep_over_wg_p50_x"]

    warm_wg_ms = results["warm_semantic_wg_p50_ms"]
    warm_sgrep_ms = results["warm_semantic_sgrep_p50_ms"]
    warm_ratio = warm_sgrep_ms / warm_wg_ms
    parity = results["warm_topk_queries_identical"]
    parity_total = results["warm_topk_queries_total"]

    fanout_wg = results["fanout_16_wg_throughput_qps"]
    fanout_sgrep = results["fanout_16_sgrep_throughput_qps"]
    fanout_ratio = fanout_wg / fanout_sgrep
    fanout_errors = results["fanout_16_wg_errors"] + results["fanout_16_wg_timeouts"]

    return [
        {
            "name": "01-edit-searchable",
            "title": "Edit \u2192 searchable",
            "headline_number": f"{freshness_ratio:.0f}x",
            "headline_suffix": "faster",
            "subline": "one-file edit to first correct response",
            "wg": freshness_wg_s,
            "sgrep": freshness_sgrep_s,
            "wg_label": f"{freshness_wg_s:.2f} s",
            "sgrep_label": f"{freshness_sgrep_s:.1f} s",
            "direction": "p50 seconds, lower is better",
            "extra": FRESHNESS_EXTRA,
        },
        {
            "name": "02-warm-query",
            "title": "Warm query",
            "headline_number": f"{warm_ratio:.1f}x",
            "headline_suffix": "faster",
            "subline": f"identical top-10 results ({parity}/{parity_total} queries)",
            "sub_color": MOCHA["green"],
            "wg": warm_wg_ms,
            "sgrep": warm_sgrep_ms,
            "wg_label": f"{warm_wg_ms:.0f} ms",
            "sgrep_label": f"{warm_sgrep_ms:.0f} ms",
            "direction": "p50 latency, lower is better",
        },
        {
            "name": "03-fanout-16",
            "title": "16 agents at once",
            "headline_number": f"{fanout_ratio:.1f}x",
            "headline_suffix": "throughput",
            "subline": f"{fanout_errors} errors",
            "sub_color": MOCHA["green"],
            "wg": fanout_wg,
            "sgrep": fanout_sgrep,
            "wg_label": f"{fanout_wg:.1f} q/s",
            "sgrep_label": f"{fanout_sgrep:.1f} q/s",
            "direction": "queries per second, higher is better",
        },
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", default=str(DEFAULT_RESULTS), help="Benchmark results JSON.")
    parser.add_argument("--out-dir", default=str(DEFAULT_OUT), help="Directory for the chart PNGs.")
    args = parser.parse_args()

    results_path = Path(args.results)
    with results_path.open("r", encoding="utf-8") as handle:
        results = json.load(handle)

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    sans = find_font(SANS_CANDIDATES)
    mono = find_font(MONO_CANDIDATES)

    specs = build_specs(results)
    for spec in specs:
        for width, height, label in ((1920, 1080, "1920x1080"), (1080, 1080, "1080x1080")):
            out = out_dir / f"{spec['name']}-{label}.png"
            make_chart(spec, width, height, out, sans, mono)
            print(f"wrote {out}")

    print(f"source: {results_path}")


if __name__ == "__main__":
    main()
