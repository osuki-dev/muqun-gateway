#!/bin/sh
# A single, source-free installer for published Desktop packages.
set -eu
case "${1:-}" in
  --help|-h)
    cat <<'USAGE'
Usage: sh install-desktop.sh [--channel stable|preview] [--tag desktop-vVERSION] [--without-gateway]
       sh /path/to/packaged/install-desktop.sh --gateway-only

Install a released Linux x86_64 or macOS arm64 Desktop package. The default
channel is stable. A release download requires Python 3 and curl. Gateway
provisioning alone uses the packaged shell installer and requires Herdr or tmux.
USAGE
    exit 0 ;;
  --gateway-only)
    [ "$#" -eq 1 ] || { echo '--gateway-only takes no other arguments' >&2; exit 2; }
    # An explicitly configured, stopped or externally managed instance keeps its owner.
    existing() {
      [ -n "${MUQUN_LOCAL_GATEWAY:-}${MUQUN_GATEWAY_CONFIG_DIR:-}${MUQUN_GATEWAY_STATE_DIR:-}${HERDR_PLUGIN_CONFIG_DIR:-}${HERDR_PLUGIN_STATE_DIR:-}" ] && return 0
      for binary in muqun-gateway herdr-gateway; do
        command -v "$binary" >/dev/null 2>&1 && return 0
      done
      config=${XDG_CONFIG_HOME:-"$HOME/.config"}
      data=${XDG_DATA_HOME:-"$HOME/.local/share"}
      support="$HOME/Library/Application Support"
      for file in "${MUQUN_GATEWAY_INSTALL_DIR:-$HOME/.local/bin}/muqun-gateway" "${MUQUN_GATEWAY_INSTALL_DIR:-$HOME/.local/bin}/herdr-gateway" \
                  "$config/systemd/user/dev.osuki.muqun-gateway.service" \
                  "$HOME/Library/LaunchAgents/dev.osuki.muqun-gateway.plist"; do
        [ ! -e "$file" ] && [ ! -L "$file" ] || return 0
      done
      for base in "$config" "$data" "$support"; do
        for name in muqun-gateway herdr-gateway herdr/plugins/config/herdr.gateway herdr/plugins/herdr.gateway; do
          for item in config.json local-client.json pairing.json gateway.lock; do
            [ ! -e "$base/$name/$item" ] || return 0
          done
        done
      done
      if command -v pgrep >/dev/null && pgrep -u "$(id -u)" -x "muqun-gateway|herdr-gateway" >/dev/null 2>&1; then return 0; fi
      case "$(uname -s)" in
        Linux)
          if command -v systemctl >/dev/null; then
            state=$(systemctl --user show -p LoadState --value dev.osuki.muqun-gateway.service 2>/dev/null) || state=
            case "$state" in ""|not-found) ;; *) return 0 ;; esac
          fi ;;
        Darwin)
          if command -v launchctl >/dev/null && launchctl print "gui/$(id -u)/dev.osuki.muqun-gateway" >/dev/null 2>&1; then return 0; fi ;;
        *) echo 'Unsupported Gateway platform' >&2; exit 1 ;;
      esac
      return 1
    }
    if existing; then
      echo 'Existing Gateway installation or service detected. Keeping its manager and configuration.'
      exit 0
    fi
    [ -f "$0" ] || { echo "Run --gateway-only from the installed Desktop package." >&2; exit 1; }
    helper=$(dirname "$0")/install.sh
    [ -f "$helper" ] || { echo 'Gateway installer missing; use https://muqun.dev/gateway.sh' >&2; exit 1; }
    echo 'Installing stable Gateway using its official installer.'
    exec sh "$helper" --channel stable ;;
esac
command -v python3 >/dev/null || { echo 'Python 3 is required to read release metadata.' >&2; exit 1; }
exec python3 -I - "$0" "$@" <<'PY'
import argparse, fcntl, hashlib, json, os, plistlib, re, subprocess, sys, tarfile, tempfile, zipfile
from pathlib import Path, PurePosixPath

api = "https://muqun.dev/api/releases/desktop"
pattern = re.compile(
    r"desktop-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-preview\.([1-9][0-9]*))?"
)
parser = argparse.ArgumentParser(prog="install-desktop.sh")
parser.add_argument("--channel", choices=["stable", "preview"], help="Desktop channel (default: stable)")
parser.add_argument("--tag", help="Exact Desktop release tag")
parser.add_argument(
    "--without-gateway", action="store_true", help="Keep this computer local-only or use a remote Gateway"
)
args = parser.parse_args(sys.argv[2:])
home = Path.home()
system = subprocess.check_output(["uname", "-s"], text=True).strip()


def envpath(key, default):
    return Path(os.environ.get(key) or default)


def fetch(url, path, limit):
    subprocess.run(
        [
            "curl",
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "2",
            "--connect-timeout",
            "15",
            "--max-time",
            "300",
            "--max-filesize",
            str(limit),
            "-o",
            str(path),
            url,
        ],
        check=True,
    )
    if path.stat().st_size > limit:
        raise ValueError("Download exceeds its expected size")


def release(temp):
    channel = args.channel or "stable"
    if args.tag:
        m = pattern.fullmatch(args.tag)
        if not m:
            raise ValueError("Expected a desktop-vX.Y.Z or desktop-vX.Y.Z-preview.N tag")
        channel = "preview" if m[4] else "stable"
        if args.channel and args.channel != channel:
            raise ValueError("Tag and channel disagree")
    meta = temp / "metadata"
    tag = args.tag
    if not tag:
        fetch(f"{api}/{channel}", meta, 64 * 1024)
        latest = json.loads(meta.read_bytes())
        if latest.get("channel") != channel or "version" not in latest:
            raise ValueError("Unexpected Desktop channel metadata")
        if latest["version"] is None:
            raise ValueError(f"No published {channel} Desktop release")
        tag = "desktop-v" + latest["version"]
    version = pattern.fullmatch(tag)
    if not version or bool(version[4]) != (channel == "preview"):
        raise ValueError("Desktop version and channel disagree")
    fetch(f"{api}/{tag}/manifest.json", meta, 64 * 1024)
    manifest = json.loads(meta.read_bytes())
    if (manifest.get("tag"), manifest.get("channel"), manifest.get("version")) != (tag, channel, tag[9:]):
        raise ValueError("Desktop release manifest does not match requested version")
    name = (
        "muqun-desktop-linux-x86_64.tar.gz"
        if system == "Linux"
        else f"muqun-desktop-{tag[9:]}-macos-arm64.zip"
    )
    for filename in (name, name + ".sha256"):
        entries = [a for a in manifest["assets"] if a.get("name") == filename]
        if len(entries) != 1:
            raise ValueError(f"{tag} must list exactly one {filename}")
        asset = entries[0]
        size = asset.get("size")
        sha = asset.get("sha256")
        limit = 512 * 1024**2 if filename == name else 1024
        if type(size) is not int or not 0 < size <= limit or not isinstance(sha, str) or not re.fullmatch(r"[a-f0-9]{64}", sha):
            raise ValueError(f"Invalid size or SHA-256 for {filename}")
        path = temp / filename
        fetch(f"{api}/{tag}/{filename}", path, size)
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        actual = digest.hexdigest()
        if path.stat().st_size != size or actual != sha:
            raise ValueError(f"Checksum or size verification failed for {filename}")
        if filename == name:
            archive_hash = actual
    expected = re.fullmatch(
        r"([0-9a-fA-F]{64}) [ *]" + re.escape(name) + r"\n?", (temp / (name + ".sha256")).read_text("ascii")
    )
    if not expected or expected[1].lower() != archive_hash:
        raise ValueError(f"Checksum verification failed for {name}")
    print(f"{tag}  {name}  SHA-256 verified: {archive_hash}", flush=True)
    return tag, temp / name


def validate(names, roots):
    for name in names:
        path = PurePosixPath(name)
        if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] not in roots:
            raise ValueError(f"Unexpected package path: {name!r}")


def stage(temp, archive, payload, dest):
    if system == "Linux":
        package = "muqun-desktop-linux-x86_64"
        with tarfile.open(archive, "r:gz") as stream:
            members = stream.getmembers()
            validate((m.name for m in members), [package])
            if any(not (m.isfile() or m.isdir()) for m in members):
                raise ValueError("Unexpected Linux archive entry type")
        subprocess.run(["tar", "-xzf", str(archive), "-C", str(temp)], check=True)
        subprocess.run(
            ["bash", str(temp / package / "desktop/scripts/install-local.sh"), "--no-build"],
            check=True,
            env={**os.environ, "PREFIX": str(payload), "MUQUN_DESKTOP_LAUNCH_PREFIX": str(dest)},
        )
        if not (payload / "bin/muqun-desktop").is_file():
            raise ValueError("Packaged installer did not install Desktop")
        launcher = payload / "share/applications/muqun-desktop.desktop"
        if str(payload) in launcher.read_text():
            raise ValueError(
                "This Linux package predates staged installation. Use its included install-local.sh --no-build."
            )
        return [
            p.relative_to(payload)
            for p in payload.rglob("*")
            if p.is_file() and p.name not in ("mimeinfo.cache", "icon-theme.cache")
        ]
    app = Path("Muqun Desktop.app")
    with zipfile.ZipFile(archive) as z:
        validate(z.namelist(), [str(app), "__MACOSX"])
        if any((m.external_attr >> 16) & 0o170000 == 0o120000 for m in z.infolist()):
            raise ValueError("Unexpected symbolic link in app ZIP")
    subprocess.run(["ditto", "-x", "-k", str(archive), str(payload)], check=True)
    for bundle in (payload / app, dest / app):
        if bundle == dest / app and not os.path.lexists(bundle):
            continue
        with (bundle / "Contents/Info.plist").open("rb") as f:
            if bundle.is_symlink() or plistlib.load(f).get("CFBundleIdentifier") != "com.muqun.desktop":
                raise ValueError(f"Unexpected app identity at {bundle}")
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(payload / app)], check=True)
    subprocess.run(["spctl", "--assess", "--type", "execute", str(payload / app)], check=True)
    return [app]


def retire(path):
    # Move an installer-owned object aside before deleting its temporary container.
    with tempfile.TemporaryDirectory(prefix=".muqun-clean-", dir=path.parent) as garbage:
        path.rename(Path(garbage) / "discard")


def recover(dest, txn):
    journal = txn / "journal.json"
    if not txn.exists():
        return
    if txn.is_symlink() or not txn.is_dir():
        raise ValueError(f"Unexpected staging path: {txn}")
    if journal.exists():
        for rel, existed in reversed(json.loads(journal.read_bytes())):
            relative = Path(rel)
            if relative.is_absolute() or ".." in relative.parts or not relative.parts:
                raise ValueError("Invalid recovery journal")
            old, current = txn / "old" / rel, dest / rel
            if os.path.lexists(old):
                if os.path.lexists(current):
                    retire(current)
                old.rename(current)
            elif not existed and os.path.lexists(current):
                retire(current)
    if txn.exists():
        retire(txn)


def install(temp, archive, dest):
    dest.mkdir(parents=True, exist_ok=True)
    txn = dest / ".muqun-desktop-install"
    with (dest / ".muqun-desktop-install.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        recover(dest, txn)
        txn.mkdir(mode=0o700)
        payload = txn / "payload"
        payload.mkdir()
        try:
            entries = stage(temp, archive, payload, dest)
            for rel in entries:
                target = dest / rel
                if os.path.commonpath((dest.resolve(), target.parent.resolve())) != str(dest.resolve()):
                    raise ValueError(f"Package destination points outside the installation: {target}")
                if target.is_dir() and not (system == "Darwin" and rel == Path("Muqun Desktop.app")):
                    raise ValueError(f"Expected a file at {target}")
            plan = [(str(rel), os.path.lexists(dest / rel)) for rel in entries]
            pending = txn / "journal.tmp"
            with pending.open("w") as file:
                json.dump(plan, file)
                file.flush()
                os.fsync(file.fileno())
            pending.rename(txn / "journal.json")
            for rel, existed in plan:
                current = dest / rel
                current.parent.mkdir(parents=True, exist_ok=True)
                if existed:
                    old = txn / "old" / rel
                    old.parent.mkdir(parents=True, exist_ok=True)
                    current.rename(old)
                (payload / rel).rename(current)
            (txn / "journal.json").unlink()
        finally:
            recover(dest, txn)


def main():
    if system not in ("Linux", "Darwin"):
        raise ValueError(f"Unsupported OS: {system}")
    arch = subprocess.check_output(["uname", "-m"], text=True).strip()
    if (system, arch) not in [("Linux", "x86_64"), ("Darwin", "arm64")]:
        raise ValueError(f"No Desktop release target for {system} {arch}")
    if system == "Linux":
        dest = envpath("PREFIX", home / ".local").absolute()
        helper = dest / "share/muqun-desktop/installers/install-desktop.sh"
        config = envpath("XDG_CONFIG_HOME", home / ".config")
    else:
        existing = Path("/Applications/Muqun Desktop.app")
        dest = envpath(
            "MUQUN_DESKTOP_APPLICATIONS_DIR", existing.parent if existing.exists() else home / "Applications"
        ).absolute()
        helper = dest / "Muqun Desktop.app/Contents/Resources/installers/install-desktop.sh"
        config = home / "Library/Application Support"
    with tempfile.TemporaryDirectory(prefix="muqun-desktop-") as tmp:
        temp = Path(tmp)
        tag, archive = release(temp)
        install(temp, archive, dest)
        print(f"Installed Muqun Desktop {tag} under {dest}.", flush=True)
        if args.without_gateway:
            marker = config / "muqun-desktop/gateway-auto-install.disabled"
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.touch(mode=0o600)
        elif (config / "muqun-desktop/gateway-auto-install.disabled").exists():
            print("Keeping your local-only Gateway preference.")
        else:
            try:
                if not helper.is_file():
                    raise ValueError("Packaged Gateway installer missing; use https://muqun.dev/gateway.sh")
                subprocess.run(["sh", str(helper), "--gateway-only"], check=True)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                raise ValueError(
                    "Desktop is installed. Fix the reported Gateway prerequisite, then retry --gateway-only. "
                    "If setup already installed the Gateway binary, finish with muqun-gateway setup and "
                    f"muqun-gateway start. {error}"
                ) from error


try:
    main()
except (
    OSError,
    KeyError,
    ValueError,
    subprocess.SubprocessError,
    tarfile.TarError,
    zipfile.BadZipFile,
) as error:
    sys.exit(f"Muqun installer: {error}")
PY
