"""Stamp a CI-only preview build; never commit, tag, or publish anything."""
import os
from pathlib import Path
import re

manifest = Path("Cargo.toml")
source = manifest.read_text()
match = re.search(r'^version = "((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))"$', source, re.M)
if not match:
    raise SystemExit("Preview builds require a stable source package version")
run = os.environ["GITHUB_RUN_NUMBER"]
if not re.fullmatch(r"[1-9][0-9]*", run):
    raise SystemExit("Invalid CI run number")
version = f"{match[2]}.{match[3]}.{int(match[4]) + 1}-preview.{run}"
lock = Path("Cargo.lock")
old = f'name = "muqun-gateway"\nversion = "{match[1]}"'
if lock.read_text().count(old) != 1:
    raise SystemExit("Expected exactly one matching local package lock entry")
notes = Path("release-notes/unreleased.md").read_text().strip()
if not notes:
    raise SystemExit("Human-written unreleased notes are required")
manifest.write_text(source[:match.start(1)] + version + source[match.end(1):])
lock.write_text(lock.read_text().replace(old, f'name = "muqun-gateway"\nversion = "{version}"'))
if output := os.environ.get("GITHUB_OUTPUT"):
    with open(output, "a") as stream:
        stream.write(f"version={version}\n")
print(version)
