"""Archive one checked Desktop CI candidate, independently of release channels."""
import argparse
import hashlib
import io
import os
from pathlib import Path
import re

from mirror_release import R2, asset_info, immutable, json_bytes, version_key


def archive(store, directory, run_id, attempt, commit, signed):
    if not all(re.fullmatch(r"[1-9][0-9]*", n) for n in (run_id, attempt)):
        raise ValueError("Invalid GitHub run identity")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Invalid Git commit")
    linux = directory / "muqun-desktop-Linux"
    mac = directory / "muqun-desktop-macOS"
    candidates = list(mac.glob("muqun-desktop-*-macos-arm64.zip"))
    if len(candidates) != 1:
        raise ValueError("Expected one macOS archive to identify the candidate version")
    mac_zip = candidates[0].name
    version = mac_zip.removeprefix("muqun-desktop-").removesuffix("-macos-arm64.zip")
    version_key(version)
    groups = [(linux, "linux-x86_64", "muqun-desktop-linux-x86_64.tar.gz",
               ["muqun-desktop", "muqun-wry-embedded"]),
              (mac, "macos-arm64", mac_zip, ["muqun-desktop"])]
    if signed:
        groups.append((directory / "desktop-preview-signed-macos", "macos-arm64", mac_zip, []))

    prefix = f"desktop/ci/{version}/{run_id}/{attempt}/"
    assets = []
    for group, platform, package, binaries in groups:
        checksum_name = package + ".sha256"
        expected = sorted([package, checksum_name, *binaries])
        if sorted(p.name for p in group.iterdir()) != expected:
            raise ValueError(f"Incomplete or unexpected files in {group.name}")
        package_info = asset_info(group / package, platform, None)
        checksum_file = group / checksum_name
        if checksum_file.stat().st_size > 1024 or not re.fullmatch(
                package_info["sha256"].encode() + rb" [ *]" + re.escape(package.encode()) + rb"\n?",
                checksum_file.read_bytes()):
            raise ValueError(f"Invalid checksum for {group.name}/{package}")
        for name in expected:
            info = package_info if name == package else asset_info(group / name, platform, None)
            info["path"] = f"{group.name}/{name}"
            assets.append(info)

    types = {".gz": "application/gzip", ".zip": "application/zip", ".sha256": "text/plain"}
    for info in assets:
        path = directory / info["path"]
        with path.open("rb") as stream:
            immutable(store, prefix + info["path"], stream, info["sha256"],
                      types.get(path.suffix, "application/octet-stream"))
    manifest = json_bytes(dict(product="desktop", version=version, commit=commit,
                               run_id=run_id, run_attempt=attempt, assets=assets))
    immutable(store, prefix + "manifest.json", io.BytesIO(manifest),
              hashlib.sha256(manifest).hexdigest(), "application/json")
    return prefix + "manifest.json"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--signed", action="store_true")
    args = parser.parse_args()
    print(archive(R2(), args.directory, os.environ["GITHUB_RUN_ID"], os.environ["GITHUB_RUN_ATTEMPT"],
                  os.environ["GITHUB_SHA"], args.signed))
