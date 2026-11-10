#!/usr/bin/env python3
"""Synchronize desktop localization catalogs.

Uses OpenCC (s2twp) to derive Traditional Chinese (zh-TW) from Simplified Chinese (zh-CN).
Ensures all 12 locales have identical key sets.
"""
import json
import os
import subprocess
from pathlib import Path

LOCALES_DIR = Path(__file__).resolve().parents[1] / "locales" / "desktop"
LANGUAGES = ["en", "zh-CN", "zh-TW", "ja", "ko", "de", "fr", "es", "pt", "ru", "vi", "th"]

def opencc_s2twp(text: str) -> str:
    res = subprocess.run(
        ["opencc", "-c", "s2twp.json"],
        input=text,
        text=True,
        capture_output=True,
        check=True
    )
    return res.stdout.strip()

def main():
    catalogs = {}
    for lang in LANGUAGES:
        p = LOCALES_DIR / f"{lang}.json"
        with open(p, "r", encoding="utf-8") as f:
            catalogs[lang] = json.load(f)

    zh_cn = catalogs["zh-CN"]
    zh_tw = catalogs["zh-TW"]
    en = catalogs["en"]

    # Sync zh-TW from zh-CN using opencc s2twp
    for k, v in zh_cn.items():
        if k not in zh_tw or zh_tw[k] == "":
            zh_tw[k] = opencc_s2twp(v)

    # Ensure all languages have all keys present in zh-CN
    all_keys = list(zh_cn.keys())
    for lang in LANGUAGES:
        cat = catalogs[lang]
        for k in all_keys:
            if k not in cat:
                if lang == "en":
                    cat[k] = k
                elif lang == "zh-TW":
                    cat[k] = opencc_s2twp(zh_cn[k])
                else:
                    cat[k] = k # fallback to English key

    for lang in LANGUAGES:
        p = LOCALES_DIR / f"{lang}.json"
        with open(p, "w", encoding="utf-8") as f:
            json.dump(catalogs[lang], f, ensure_ascii=False, indent=2)
            f.write("\n")

    print(f"Synchronized {len(all_keys)} keys across all {len(LANGUAGES)} locales.")

if __name__ == "__main__":
    main()
