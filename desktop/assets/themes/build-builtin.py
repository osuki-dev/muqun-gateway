#!/usr/bin/env python3
"""Rebuild the built-in theme packs from a muqun-themes checkout.

    python3 desktop/assets/themes/build-builtin.py /path/to/muqun/themes

Each pack is a regular `.muqun-theme` (stored ZIP: `theme.json` + `assets/*`,
the layout `muqun-theme pack` writes), trimmed for the desktop binary:

* only the images the desktop draws are kept -- the gallery `preview`, the
  Home `home.artwork` / `home.wallpaper` slots, and `launch.artwork` --
  downscaled to at most `MAX_EDGE` px and re-encoded as WebP (`magick`,
  quality `QUALITY`);
* every other slot, the icon set and a custom `homeIdentity.logo` are dropped,
  and each kept asset's `sha256` is rewritten to the new bytes.

The colours, terminal palette, metadata and everything else in the manifest
are the source's, unchanged. Only CC0-1.0 sources are accepted: these bytes
ship inside the application. Output is deterministic (fixed ZIP timestamps).
"""

import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile
import zipfile

THEMES = [
    "cover-courier",
    "akiba-overdrive",
    "jade-tempest",
    "eclipse-archivist",
    "vanguard-drifter",
    "scene-voyager",
]
KEPT_SLOTS = ("home.artwork", "home.wallpaper", "launch.artwork")
MAX_EDGE = 1280
PREVIEW_SIZE = "768x480"
QUALITY = 78
OUT = pathlib.Path(__file__).resolve().parent / "builtin"


def webp(source: pathlib.Path, resize: str) -> bytes:
    with tempfile.NamedTemporaryFile(suffix=".webp") as out:
        subprocess.run(
            [
                "magick",
                str(source),
                "-resize",
                resize,
                "-strip",
                "-quality",
                str(QUALITY),
                "-define",
                "webp:method=6",
                out.name,
            ],
            check=True,
        )
        return pathlib.Path(out.name).read_bytes()


def kept_slots(slots):
    return {k: v for k, v in (slots or {}).items() if k in KEPT_SLOTS and v}


def trim_slot(slot):
    slot = dict(slot)
    slot.pop("compact", None)
    slot.pop("regular", None)
    return slot


def build(src_root: pathlib.Path, theme_id: str) -> pathlib.Path:
    src = src_root / theme_id
    manifest = json.loads((src / "theme.json").read_text())
    if manifest.get("license") != "CC0-1.0":
        sys.exit(f"{theme_id}: license is {manifest.get('license')!r}, not CC0-1.0")

    decoration = {k: trim_slot(v) for k, v in kept_slots(manifest.get("decoration")).items()}
    variant_decorations = {
        scheme: {k: trim_slot(v) for k, v in kept_slots(slots).items()}
        for scheme, slots in (manifest.get("variantDecorations") or {}).items()
    }
    used = {manifest["preview"]}
    for slots in [decoration, *variant_decorations.values()]:
        used.update(slot["asset"] for slot in slots.values())

    files = {}
    assets = {}
    for asset_id in sorted(used):
        source = src / manifest["assets"][asset_id]["path"]
        resize = PREVIEW_SIZE if asset_id == manifest["preview"] else f"{MAX_EDGE}x{MAX_EDGE}>"
        data = webp(source, resize)
        path = f"assets/{asset_id}.webp"
        files[path] = data
        assets[asset_id] = {"path": path, "sha256": hashlib.sha256(data).hexdigest()}

    manifest["assets"] = assets
    if decoration:
        manifest["decoration"] = decoration
    else:
        manifest.pop("decoration", None)
    if any(variant_decorations.values()):
        manifest["variantDecorations"] = {k: v for k, v in variant_decorations.items() if v}
    else:
        manifest.pop("variantDecorations", None)
    for key in ("icons", "iconDirections"):
        manifest.pop(key, None)
    identity = manifest.get("homeIdentity") or {}
    if (identity.get("logo") or {}).get("mode") == "custom":
        identity["logo"] = {"mode": "hidden"}

    OUT.mkdir(parents=True, exist_ok=True)
    target = OUT / f"{theme_id}.muqun-theme"
    with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_STORED) as archive:
        entries = [("theme.json", json.dumps(manifest, indent=2).encode() + b"\n")]
        entries += sorted(files.items())
        for name, data in entries:
            info = zipfile.ZipInfo(name, date_time=(2026, 11, 2, 0, 0, 0))
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)
    return target


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    src_root = pathlib.Path(sys.argv[1]) / "src"
    total = 0
    for theme_id in THEMES:
        target = build(src_root, theme_id)
        size = target.stat().st_size
        total += size
        print(f"{target.name:40} {size / 1024:8.1f} KiB")
    print(f"{'total':40} {total / 1024:8.1f} KiB")


if __name__ == "__main__":
    main()
