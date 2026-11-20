"""Offline installer contract tests. macOS tools are simulated on Linux."""

import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[3]
STABLE = "desktop-v1.0.2"
PREVIEW = "desktop-v1.0.3-preview.12"


class DesktopInstaller(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.bin = self.root / "commands"
        self.bin.mkdir()
        self.home = self.root / "home with spaces"
        self.home.mkdir()
        self.files = self.root / "fixtures"
        self.files.mkdir()
        self.env = {
            "PATH": str(self.bin),
            "HOME": str(self.home),
            "TMPDIR": str(self.root),
            "FIXTURES": str(self.files),
            "TEST_OS": "Linux",
            "TEST_ARCH": "x86_64",
            "PREFIX": str(self.home / "test prefix"),
            "MUQUN_DESKTOP_APPLICATIONS_DIR": str(self.home / "Applications"),
        }
        for name in [
            "bash",
            "sh",
            "id",
            "tar",
            "python3",
            "gzip",
            "cp",
            "mkdir",
            "dirname",
            "basename",
            "chmod",
            "sed",
            "install",
            "mv",
            "pgrep",
            "grep",
            "cat",
        ]:
            self.bin.joinpath(name).symlink_to(shutil.which(name))
        self.bin.joinpath("pgrep").unlink()
        self.command("pgrep", "exit 1")
        self.command("uname", 'case "$1" in -s) echo "$TEST_OS";; -m) echo "$TEST_ARCH";; esac')
        self.command(
            "curl",
            '''while [ "$1" != "-o" ]; do shift; done
out=$2; url=$3
case "$url" in
 https://muqun.dev/api/releases/desktop/*/manifest.json) dir=${url%/*}; name=${dir##*/}.json;;
 https://muqun.dev/api/releases/desktop/*) name=${url##*/};;
 *) exit 1;;
esac
cp "$FIXTURES/$name" "$out"''',
        )
        for channel in ("stable", "preview"):
            (self.files / channel).write_text(json.dumps({"channel": channel, "version": None}))
        self.helper = b'echo "$*" >> "$HOME/gateway-invocations"\nexit "${GATEWAY_EXIT:-0}"\n'

    def command(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    def fixture(self, tag, mac=False):
        v = tag.removeprefix("desktop-v")
        app = "Muqun Desktop.app"
        if mac:
            name = f"muqun-desktop-{v}-macos-arm64.zip"
            with zipfile.ZipFile(self.files / name, "w") as z:
                z.writestr(
                    app + "/Contents/Info.plist", plistlib.dumps({"CFBundleIdentifier": "com.muqun.desktop"})
                )
                z.writestr(app + "/Contents/Resources/installers/install.sh", self.helper)
                z.writestr(app + "/Contents/Resources/installers/VERSION", v + "\n")
                z.write(
                    ROOT / "install-desktop.sh", app + "/Contents/Resources/installers/install-desktop.sh"
                )
                z.writestr(app + "/Contents/MacOS/muqun-desktop", v)
        else:
            name = "muqun-desktop-linux-x86_64.tar.gz"
            build = self.root / ("package-" + v)
            linux = build / "muqun-desktop-linux-x86_64"
            (linux / "bin").mkdir(parents=True)
            for exe in ["muqun-desktop", "muqun-wry-embedded"]:
                file = linux / "bin" / exe
                file.write_text(v)
                file.chmod(0o755)
            for folder in ["desktop/scripts", "desktop/packaging", "desktop/assets/plugins"]:
                (linux / folder).mkdir(parents=True)
            for script in ["install-local.sh", "uninstall-local.sh"]:
                shutil.copy2(ROOT / "desktop/scripts" / script, linux / "desktop/scripts")
            shutil.copytree(ROOT / "desktop/packaging/linux", linux / "desktop/packaging/linux")
            shutil.copytree(
                ROOT / "desktop/assets/plugins/review-center", linux / "desktop/assets/plugins/review-center"
            )
            shutil.copy2(ROOT / "install-desktop.sh", linux)
            (linux / "install.sh").write_bytes(self.helper)
            (linux / "VERSION").write_text(v + "\n")
            with tarfile.open(self.files / name, "w:gz") as tar:
                tar.add(linux, arcname=linux.name)
        sha = hashlib.sha256((self.files / name).read_bytes()).hexdigest()
        (self.files / (name + ".sha256")).write_text(f"{sha}  {name}\n")
        channel = "preview" if "-preview." in tag else "stable"
        assets = []
        for filename in (name, name + ".sha256"):
            data = (self.files / filename).read_bytes()
            assets.append({"name": filename, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()})
        manifest = {"channel": channel, "version": v, "tag": tag, "assets": assets}
        (self.files / (tag + ".json")).write_text(json.dumps(manifest))
        (self.files / channel).write_text(json.dumps({"channel": channel, "version": v}))
        return name

    def run_installer(self, *flags, success=True):
        result = subprocess.run(
            ["/bin/sh", ROOT / "install-desktop.sh", *flags],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=20,
        )
        self.assertEqual(result.returncode == 0, success, result.stderr + result.stdout)
        return result

    def test_linux_stable_install_and_existing_gateway(self):
        self.fixture(STABLE)
        self.run_installer()
        dest = Path(self.env["PREFIX"])
        self.assertEqual((dest / "bin/muqun-desktop").read_text(), "1.0.2")
        self.assertIn(
            str(dest / "bin/muqun-desktop"), (dest / "share/applications/muqun-desktop.desktop").read_text()
        )
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel stable\n")
        self.command("muqun-gateway", "exit 99")
        self.run_installer("--tag", STABLE)
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel stable\n")

    def test_channel_opt_out_and_integrity(self):
        asset = self.fixture(PREVIEW)
        self.run_installer(success=False)
        self.assertFalse((self.home / "gateway-invocations").exists())
        self.run_installer("--channel", "preview", "--without-gateway")
        marker = self.home / ".config/muqun-desktop/gateway-auto-install.disabled"
        self.assertTrue(marker.exists())
        self.run_installer("--tag", PREVIEW)
        self.assertFalse((self.home / "gateway-invocations").exists())
        (self.files / asset).write_bytes(b"damaged")
        self.assertIn("Checksum", self.run_installer("--tag", PREVIEW, success=False).stderr)
        self.assertEqual((Path(self.env["PREFIX"]) / "bin/muqun-desktop").read_text(), "1.0.3-preview.12")

    def test_first_preview_install_provisions_preview_gateway(self):
        self.fixture(PREVIEW)
        self.run_installer("--channel", "preview")
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel preview\n")

    def test_rejects_mismatched_channel_and_release_identity_before_install(self):
        self.fixture(STABLE)
        (self.files / "stable").write_text(json.dumps({"channel": "preview", "version": "1.0.2"}))
        self.run_installer(success=False)
        path = self.files / (STABLE + ".json")
        manifest = json.loads(path.read_bytes())
        manifest["tag"] = PREVIEW
        path.write_text(json.dumps(manifest))
        self.run_installer("--tag", STABLE, success=False)
        self.assertFalse((Path(self.env["PREFIX"]) / "bin/muqun-desktop").exists())

    def test_rejects_sidecar_disagreeing_with_hash_in_version_manifest(self):
        name = self.fixture(STABLE)
        path = self.files / (STABLE + ".json")
        manifest = json.loads(path.read_bytes())
        side = self.files / (name + ".sha256")
        side.write_text("0" * 64 + "  " + name + "\n")
        for asset in manifest["assets"]:
            if asset["name"] == side.name:
                asset["sha256"] = hashlib.sha256(side.read_bytes()).hexdigest()
        path.write_text(json.dumps(manifest))
        self.assertIn("Checksum", self.run_installer("--tag", STABLE, success=False).stderr)
        self.assertFalse((Path(self.env["PREFIX"]) / "bin/muqun-desktop").exists())

    def test_interrupted_replacement_rolls_back_before_next_install_attempt(self):
        self.fixture(STABLE)
        self.run_installer("--without-gateway")
        dest = Path(self.env["PREFIX"])
        txn = dest / ".muqun-desktop-install"
        backup = txn / "old/bin/muqun-desktop"
        backup.parent.mkdir(parents=True)
        executable = dest / "bin/muqun-desktop"
        executable.rename(backup)
        executable.write_text("interrupted candidate")
        (txn / "journal.json").write_text(json.dumps([["bin/muqun-desktop", True]]))
        # A subsequent staging error must still leave the prior version runnable.
        blocker = dest / "share/applications/muqun-desktop.desktop"
        blocker.unlink()
        blocker.mkdir()
        self.run_installer("--without-gateway", success=False)
        self.assertEqual(executable.read_text(), "1.0.2")
        self.assertTrue(blocker.is_dir())

    def test_external_gateway_service_without_path_binary_is_reused(self):
        self.fixture(STABLE)
        unit = self.home / ".config/systemd/user/dev.osuki.muqun-gateway.service"
        unit.parent.mkdir(parents=True)
        unit.write_text("existing external service")
        self.run_installer()
        self.assertFalse((self.home / "gateway-invocations").exists())
        self.assertEqual(unit.read_text(), "existing external service")

    def test_macos_verifies_bundle_before_replacing(self):
        self.env.update(TEST_OS="Darwin", TEST_ARCH="arm64")
        self.fixture(STABLE, mac=True)
        self.command("ditto", 'python3 -m zipfile -e "$3" "$4"')
        self.command("codesign", 'echo verify >> "$HOME/mac-checks"')
        self.command("spctl", 'echo assess >> "$HOME/mac-checks"; exit "${ASSESS_EXIT:-0}"')
        self.run_installer()
        app = self.home / "Applications/Muqun Desktop.app"
        self.assertEqual((self.home / "mac-checks").read_text(), "verify\nassess\n")
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel stable\n")
        self.fixture(PREVIEW, mac=True)
        self.env["ASSESS_EXIT"] = "1"
        self.run_installer("--tag", PREVIEW, success=False)
        self.assertEqual((app / "Contents/MacOS/muqun-desktop").read_text(), "1.0.2")

    def test_gateway_only_runs_without_python(self):
        directory = self.root / "installed helpers"
        directory.mkdir()
        shutil.copy2(ROOT / "install-desktop.sh", directory)
        (directory / "install.sh").write_bytes(self.helper)
        self.bin.joinpath("python3").unlink()
        subprocess.run(["sh", directory / "install-desktop.sh", "--gateway-only"], env=self.env,
                       check=True, capture_output=True)
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel stable\n")

    def test_incomplete_gateway_keeps_desktop_and_allows_manual_retry(self):
        self.fixture(STABLE)
        self.env["GATEWAY_EXIT"] = "1"
        self.assertIn("Desktop is installed", self.run_installer(success=False).stderr)
        self.env["GATEWAY_EXIT"] = "0"
        helper = Path(self.env["PREFIX"]) / "share/muqun-desktop/installers/install-desktop.sh"
        subprocess.run(["sh", helper, "--gateway-only"], env=self.env, check=True, capture_output=True)
        self.assertEqual((self.home / "gateway-invocations").read_text().count("--channel stable"), 2)


if __name__ == "__main__":
    unittest.main()
