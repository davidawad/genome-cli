#!/usr/bin/env python3
"""Render an ANSI terminal transcript as a terminal-window SVG (stdlib only).

    ansi2svg.py [--title T] [--cols N] IN.ansi OUT.svg

Understands the SGR codes `genome` emits (bold, dim, 31-37 foreground,
resets). Lines longer than --cols soft-wrap like a terminal. Every run of
non-space text is placed at its exact column with a fixed advance, so table
columns line up whatever monospace font the viewer has.
"""

import argparse
import re
from xml.sax.saxutils import escape

FG = "#c9d1d9"
PALETTE = {
    "31": "#ff7b72",
    "32": "#7ee787",
    "33": "#e3b341",
    "34": "#79c0ff",
    "35": "#d2a8ff",
    "36": "#56d4dd",
    "37": "#f0f6fc",
}
DIM = "#8b949e"
PROMPT = "#7ee787"

FONT = 14
CELL_W = 8.4
CELL_H = 19
PAD_X = 18
PAD_TOP = 46
PAD_BOTTOM = 18
GAP = 9  # extra space above each command after the first

SGR = re.compile(r"\x1b\[([0-9;]*)m")


def parse(line):
    """Split one line into (text, style) cells; style = (color, bold, dim)."""
    cells, style = [], (None, False, False)
    pos = 0
    for m in SGR.finditer(line):
        cells.extend((ch, style) for ch in line[pos : m.start()])
        color, bold, dim = style
        for code in (m.group(1) or "0").split(";"):
            if code in ("", "0"):
                color, bold, dim = None, False, False
            elif code == "1":
                bold = True
            elif code == "2":
                dim = True
            elif code == "22":
                bold = dim = False
            elif code == "39":
                color = None
            elif code in PALETTE:
                color = code
        style = (color, bold, dim)
        pos = m.end()
    cells.extend((ch, style) for ch in line[pos:])
    return cells


def prompt_cells(line):
    """Style a `$ cmd` prompt line written by readme-samples.sh."""
    return [(ch, ("prompt", True, False) if i < 2 else ("37", True, False)) for i, ch in enumerate(line)]


def runs(cells):
    """Yield (column, text, style) for each run of same-style non-space chars."""
    col = 0
    while col < len(cells):
        ch, style = cells[col]
        if ch == " ":
            col += 1
            continue
        end = col
        while end < len(cells) and cells[end][0] != " " and cells[end][1] == style:
            end += 1
        yield col, "".join(c for c, _ in cells[col:end]), style
        col = end


def fill(style):
    color, _, dim = style
    if color == "prompt":
        return PROMPT
    if color:
        return PALETTE[color]
    return DIM if dim else FG


def render(text, title, cols):
    rows, y = [], PAD_TOP + FONT
    for n, line in enumerate(text.rstrip("\n").split("\n")):
        prompt = line.startswith("$ ")
        cells = prompt_cells(line) if prompt else parse(line)
        if prompt and n:
            y += GAP
        for i in range(0, max(len(cells), 1), cols):
            rows.append((y, cells[i : i + cols]))
            y += CELL_H
    width = round(PAD_X * 2 + cols * CELL_W)
    height = y - FONT + PAD_BOTTOM
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" aria-label="{escape(title)}">',
        "<style>text{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,'DejaVu Sans Mono',"
        f"'Liberation Mono',monospace;font-size:{FONT}px;white-space:pre}}.b{{font-weight:bold}}</style>",
        f'<rect width="{width}" height="{height}" rx="10" fill="#0d1117"/>',
        f'<rect x="0.5" y="0.5" width="{width - 1}" height="{height - 1}" rx="10" fill="none" stroke="#30363d"/>',
        '<circle cx="20" cy="18" r="6" fill="#ff5f57"/>',
        '<circle cx="40" cy="18" r="6" fill="#febc2e"/>',
        '<circle cx="60" cy="18" r="6" fill="#28c840"/>',
        f'<text x="{width / 2:.1f}" y="23" text-anchor="middle" fill="{DIM}">{escape(title)}</text>',
    ]
    for y, cells in rows:
        for col, chunk, style in runs(cells):
            cls = ' class="b"' if style[1] else ""
            out.append(
                f'<text x="{PAD_X + col * CELL_W:.1f}" y="{y}" fill="{fill(style)}"{cls} '
                f'textLength="{len(chunk) * CELL_W:.1f}" lengthAdjust="spacingAndGlyphs">{escape(chunk)}</text>'
            )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--title", default="")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("input")
    ap.add_argument("output")
    a = ap.parse_args()
    with open(a.input, encoding="utf-8") as f:
        svg = render(f.read(), a.title, a.cols)
    with open(a.output, "w", encoding="utf-8") as f:
        f.write(svg)


if __name__ == "__main__":
    main()
