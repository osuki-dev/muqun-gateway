#!/usr/bin/env python3
"""Normalize brand SVGs into rune Icon bodies (0..24 square)."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GLYPHS = ROOT / "assets" / "glyphs"
OUT = ROOT / "src" / "ui" / "agent_marks.rs"

NAMES = [
    ("CODEX", "brand-codex"),
    ("OPENCODE", "brand-opencode"),
    ("DEEPSEEK", "brand-deepseek"),
    ("T3", "brand-t3"),
    ("FX", "brand-fx"),
    ("QODER", "brand-qoder"),
    ("PI", "brand-pi"),
    ("CLAUDE", "brand-claude"),
    ("ANTIGRAVITY", "brand-antigravity"),
    ("CURSOR", "brand-cursor"),
    ("AGENT", "agent"),
]

ARITY = {
    "M": 2,
    "L": 2,
    "H": 1,
    "V": 1,
    "C": 6,
    "S": 4,
    "Q": 4,
    "T": 2,
    "A": 7,
    "Z": 0,
}


def fmt(v: float) -> str:
    s = f"{v:.4f}".rstrip("0").rstrip(".")
    return s if s else "0"


def rewrite_path_d(d: str, minx: float, miny: float, scale: float) -> str:
    tokens = re.findall(
        r"[MmLlHhVvCcSsQqTtAaZz]|[-+]?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?", d
    )
    out: list[str] = []
    i = 0
    cmd = "M"
    while i < len(tokens):
        t = tokens[i]
        if re.match(r"[A-Za-z]", t):
            cmd = t
            out.append(t)
            i += 1
            if cmd in "Zz":
                continue
            n = ARITY[cmd.upper()]
            while i < len(tokens) and not re.match(r"[A-Za-z]", tokens[i]):
                args: list[float] = []
                for _ in range(n):
                    if i >= len(tokens) or re.match(r"[A-Za-z]", tokens[i]):
                        break
                    args.append(float(tokens[i]))
                    i += 1
                if len(args) < n:
                    break
                cu = cmd.upper()
                is_abs = cmd.isupper()
                if cu == "H":
                    v = args[0]
                    v = (v - minx) * scale if is_abs else v * scale
                    out.append(fmt(v))
                elif cu == "V":
                    v = args[0]
                    v = (v - miny) * scale if is_abs else v * scale
                    out.append(fmt(v))
                elif cu == "A":
                    rx, ry, rot, la, sw, x, y = args
                    rx *= scale
                    ry *= scale
                    if is_abs:
                        x = (x - minx) * scale
                        y = (y - miny) * scale
                    else:
                        x *= scale
                        y *= scale
                    out.extend(fmt(v) for v in (rx, ry, rot, la, sw, x, y))
                else:
                    vals: list[float] = []
                    for j in range(0, n, 2):
                        x, y = args[j], args[j + 1]
                        if is_abs:
                            x = (x - minx) * scale
                            y = (y - miny) * scale
                        else:
                            x *= scale
                            y *= scale
                        vals.extend([x, y])
                    out.extend(fmt(v) for v in vals)
                if cu == "M":
                    cmd = "L" if is_abs else "l"
        else:
            i += 1
    return "".join(
        tok if re.match(r"[A-Za-z]", tok) else (" " + tok) for tok in out
    ).strip()


def rewrite_element(el: str, minx: float, miny: float, scale: float) -> str:
    def d_repl(m: re.Match[str]) -> str:
        return 'd="' + rewrite_path_d(m.group(1), minx, miny, scale) + '"'

    el = re.sub(r'd="([^"]*)"', d_repl, el)

    def num_attr(name: str) -> None:
        nonlocal el

        def repl(m: re.Match[str]) -> str:
            v = float(m.group(1))
            if name in ("x", "cx", "x1", "x2"):
                v = (v - minx) * scale
            elif name in ("y", "cy", "y1", "y2"):
                v = (v - miny) * scale
            elif name in ("width", "height", "r", "rx", "ry"):
                v = v * scale
            return f'{name}="{fmt(v)}"'

        el = re.sub(rf'{name}="([^"]*)"', repl, el)

    for a in ("x", "y", "cx", "cy", "width", "height", "r", "rx", "ry", "x1", "y1", "x2", "y2"):
        num_attr(a)
    return el


def body_for(file_stem: str) -> str:
    t = (GLYPHS / f"{file_stem}.svg").read_text()
    vb = re.search(r'viewBox="([^"]+)"', t)
    assert vb, file_stem
    minx, miny, w, h = [float(x) for x in vb.group(1).replace(",", " ").split()]
    scale = 24.0 / max(w, h)
    pad_x = (24.0 - w * scale) / 2.0
    pad_y = (24.0 - h * scale) / 2.0
    minx2 = minx - pad_x / scale
    miny2 = miny - pad_y / scale

    inner = re.sub(r"^[\s\S]*?<svg[^>]*>", "", t)
    inner = re.sub(r"</svg>\s*$", "", inner).strip()
    inner = re.sub(r"<title>[\s\S]*?</title>", "", inner)
    parts = re.findall(r"<[^>]+/?>", inner)
    out_parts: list[str] = []
    for p in parts:
        tag = p[1:].split()[0].rstrip(">/")
        if tag not in ("path", "rect", "circle", "ellipse", "line"):
            continue
        p2 = re.sub(r'\sfill="currentColor"', "", p)
        p2 = rewrite_element(p2, minx2, miny2, scale)
        out_parts.append(p2)
    return "".join(out_parts)


def main() -> None:
    lines = [
        "//! Generated brand-mark Icon bodies (0..24). Run `scripts/gen-agent-marks.py`.",
        "",
        "use rune::Icon;",
        "",
    ]
    for const, stem in NAMES:
        body = body_for(stem)
        # Escape for raw string: avoid "# in body
        if '"' in body and 'r#"' in body:
            raise SystemExit(f"awkward quotes in {stem}")
        lines.append(f"pub static {const}: Icon = Icon {{")
        lines.append(f'    name: "{stem}",')
        lines.append("    view: 24.0,")
        lines.append(f'    body: r#"{body}"#,')
        lines.append("};")
        lines.append("")
    OUT.write_text("\n".join(lines))
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
