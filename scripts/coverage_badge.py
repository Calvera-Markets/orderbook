#!/usr/bin/env python3
"""Write badges/coverage.svg from an llvm-cov JSON summary."""

import json
import sys
from pathlib import Path


def line_percent(summary_path: Path) -> float:
    report = json.loads(summary_path.read_text())
    return float(report["data"][0]["totals"]["lines"]["percent"])


def badge_color(percent: float) -> str:
    if percent >= 90:
        return "#4c1"
    if percent >= 75:
        return "#dfb317"
    return "#e05d44"


def badge_svg(percent: float) -> str:
    label = "coverage"
    value = f"{percent:.2f}%"
    left, right = 62, 58
    width = left + right
    color = badge_color(percent)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="20" role="img" aria-label="{label}: {value}">
  <title>{label}: {value}</title>
  <clipPath id="r"><rect width="{width}" height="20" rx="3" fill="#fff"/></clipPath>
  <g clip-path="url(#r)">
    <rect width="{left}" height="20" fill="#555"/>
    <rect x="{left}" width="{right}" height="20" fill="{color}"/>
  </g>
  <g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
    <text x="{left // 2}" y="14">{label}</text>
    <text x="{left + right // 2}" y="14">{value}</text>
  </g>
</svg>
"""


def main() -> None:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} SUMMARY.json BADGE.svg", file=sys.stderr)
        sys.exit(2)
    summary = Path(sys.argv[1])
    dest = Path(sys.argv[2])
    percent = line_percent(summary)
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(badge_svg(percent))
    print(f"wrote {dest} ({percent:.2f}%)")


if __name__ == "__main__":
    main()
