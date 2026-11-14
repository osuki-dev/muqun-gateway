#!/usr/bin/env python3
"""Regenerate desktop/assets/i18n/s2tw-chars.txt from OpenCC's data.

Every CJK ideograph (and the common punctuation block) is run through
OpenCC's `s2tw` configuration (Simplified -> Traditional, Taiwan standard);
characters that change are written as `from\tto` lines. The table is data
derived from OpenCC (Apache-2.0); see the NOTICE beside it. Needs the
`opencc` Python package (python-opencc) at generation time only.
"""
import sys
from pathlib import Path

import opencc

out = Path(__file__).resolve().parents[1] / "assets" / "i18n" / "s2tw-chars.txt"
cc = opencc.OpenCC("s2tw")
pairs = []
for lo, hi in [(0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xF900, 0xFAFF), (0x3000, 0x303F), (0xFF00, 0xFFEF)]:
    for cp in range(lo, hi + 1):
        ch = chr(cp)
        to = cc.convert(ch)
        if to != ch and len(to) == 1:
            pairs.append((ch, to))
out.write_text("".join(f"{a}\t{b}\n" for a, b in pairs), encoding="utf-8")
print(f"{len(pairs)} characters -> {out}", file=sys.stderr)
