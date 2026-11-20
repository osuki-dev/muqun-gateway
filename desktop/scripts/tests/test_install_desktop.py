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
    @classmethod
    def setUpClass(cls):
        directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(directory.cleanup)
        cls.native = Path(directory.name) / "muqun-desktop-install"
        subprocess.run(["go", "build", "-trimpath", "-ldflags=-s -w", "-o", cls.native,
                        *sorted((ROOT / "desktop/installer").glob("*.go"))], check=True,
                       env={**os.environ, "CGO_ENABLED": "0"})

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
            "awk", "wc", "sleep", "mktemp", "rm", "sha256sum", "unzip", "touch",
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
        self.command("tmux", "exit 0")
        self.command("uname", 'case "$1" in -s) echo "$TEST_OS";; -m) echo "$TEST_ARCH";; esac')
        self.command(
            "curl",
            '''while [ "$1" != "-o" ]; do shift; done
out=$2; url=$3
case "$url" in
 https://muqun.dev/api/releases/desktop/*/version) dir=${url%/*}; name=${dir##*/};;
 https://muqun.dev/api/releases/desktop/*) name=${url##*/};;
 *) exit 1;;
esac
cp "$FIXTURES/$name" "$out"''',
        )
        for channel in ("stable", "preview"):
            (self.files / channel).write_text("")
        self.helper = b'echo "$*" >> "$HOME/gateway-invocations"\nexit "${GATEWAY_EXIT:-0}"\n'

    def command(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + body + "\n")
        path.chmod(0o755)

    def fixture(self, tag, mac=False, desktop_executable=None, packaged_version=None):
        v = tag.removeprefix("desktop-v")
        app = "Muqun Desktop.app"
        if mac:
            name = f"muqun-desktop-{v}-macos-arm64.zip"
            with zipfile.ZipFile(self.files / name, "w") as z:
                z.writestr(
                    app + "/Contents/Info.plist", plistlib.dumps({"CFBundleIdentifier": "com.muqun.desktop"})
                )
                z.writestr(app + "/Contents/Resources/installers/install.sh", self.helper)
                z.write(self.native, app + "/Contents/Helpers/muqun-desktop-install")
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
            shutil.copy2(self.native, linux / "bin/muqun-desktop-install")
            for exe in ["muqun-desktop", "muqun-wry-embedded"]:
                file = linux / "bin" / exe
                file.write_text(desktop_executable if exe == "muqun-desktop" and desktop_executable else v)
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
            (linux / "VERSION").write_text((packaged_version or v) + "\n")
            with tarfile.open(self.files / name, "w:gz") as tar:
                tar.add(linux, arcname=linux.name)
        sha = hashlib.sha256((self.files / name).read_bytes()).hexdigest()
        (self.files / (name + ".sha256")).write_text(f"{sha}  {name}\n")
        channel = "preview" if "-preview." in tag else "stable"
        self.write_metadata(name)
        (self.files / channel).write_text(v + "\n")
        return name

    def write_metadata(self, name):
        fields = []
        for filename in (name, name + ".sha256"):
            data = (self.files / filename).read_bytes()
            fields.extend([str(len(data)), hashlib.sha256(data).hexdigest()])
        (self.files / (name + ".metadata")).write_text(" ".join(fields) + "\n")

    def run_installer(self, *flags, success=True):
        result = subprocess.run(
            ["/bin/sh", "-s", "--", *flags],
            input=(ROOT / "install-desktop.sh").read_text(),
            env=self.env,
            capture_output=True,
            text=True,
            timeout=20,
        )
        self.assertEqual(result.returncode == 0, success, result.stderr + result.stdout)
        return result

    def confirm_gateway_setup(self, success=True):
        helper = Path(self.env["PREFIX"]) / "share/muqun-desktop/installers/install-desktop.sh"
        result = subprocess.run(["sh", helper, "--gateway-only"], env=self.env, capture_output=True)
        self.assertEqual(result.returncode == 0, success, result.stderr)

    def test_linux_stable_install_and_existing_gateway(self):
        self.fixture(STABLE)
        self.run_installer()
        dest = Path(self.env["PREFIX"])
        self.assertEqual((dest / "bin/muqun-desktop").read_text(), "1.0.2")
        self.assertIn(
            str(dest / "bin/muqun-desktop"), (dest / "share/applications/muqun-desktop.desktop").read_text()
        )
        self.assertFalse((self.home / "gateway-invocations").exists())
        self.command("muqun-gateway", "exit 99")
        self.run_installer("--tag", STABLE)
        self.confirm_gateway_setup()
        self.assertFalse((self.home / "gateway-invocations").exists())

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

    def test_fresh_preview_waits_for_separate_gateway_setup(self):
        (self.bin / "tmux").unlink()
        self.fixture(PREVIEW, desktop_executable='#!/bin/sh\necho "$1" >> "$HOME/desktop-starts"\n')
        self.run_installer("--channel", "preview")
        self.assertFalse((self.home / "desktop-starts").exists())
        self.assertFalse((self.home / "gateway-invocations").exists())
        self.confirm_gateway_setup()
        self.assertEqual((self.home / "desktop-starts").read_text(), "--ensure-term-daemon\n")
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel preview --desktop-terminal\n")
        self.command("muqun-gateway", "exit 99")
        self.run_installer("--tag", PREVIEW)
        self.confirm_gateway_setup()
        self.assertEqual((self.home / "desktop-starts").read_text(), "--ensure-term-daemon\n")

    def test_tagged_gui_update_reports_measured_download_and_activation(self):
        asset = self.fixture(PREVIEW)
        total = (self.files / asset).stat().st_size
        self.env["MUQUN_INSTALL_PROGRESS"] = "1"
        def events(result):
            return [line.removeprefix("MUQUN_PROGRESS ") for line in result.stderr.splitlines()
                    if line.startswith("MUQUN_PROGRESS ")]
        records = events(self.run_installer("--tag", PREVIEW))
        self.assertEqual(records[0], f"download 0 {total}")
        self.assertEqual(records[-3:], [f"download {total} {total}", "verify", "install"])
        self.assertFalse((self.home / "gateway-invocations").exists())
        (self.files / asset).write_bytes(b"broken")
        records = events(self.run_installer("--tag", PREVIEW, success=False))
        self.assertEqual(records[-1], "verify")
        self.assertNotIn("install", records)

    def test_mislabeled_verified_update_preserves_installed_version(self):
        self.fixture(STABLE)
        self.run_installer("--tag", STABLE)
        self.fixture(PREVIEW, packaged_version="1.0.4")
        error = self.run_installer("--tag", PREVIEW, success=False)
        self.assertIn("staged Desktop version differs", error.stderr)
        self.assertEqual((Path(self.env["PREFIX"]) / "bin/muqun-desktop").read_text(), "1.0.2")

    def test_archive_with_external_link_fails_before_install(self):
        name = self.fixture(STABLE)
        root = self.root / "package-1.0.2" / "muqun-desktop-linux-x86_64"
        (root / "outside").symlink_to(self.home)
        archive = self.files / name
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(root, arcname=root.name)
        (self.files / (name + ".sha256")).write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {name}\n")
        self.write_metadata(name)
        self.assertIn("unexpected Linux archive entry", self.run_installer(success=False).stderr)
        self.assertFalse((Path(self.env["PREFIX"]) / "bin/muqun-desktop").exists())

    def test_rejects_mismatched_channel_and_invalid_metadata_before_install(self):
        name = self.fixture(STABLE)
        (self.files / "stable").write_text("1.0.3-preview.12\n")
        self.run_installer(success=False)
        (self.files / (name + ".metadata")).write_text("missing digest\n")
        self.run_installer("--tag", STABLE, success=False)
        self.assertFalse((Path(self.env["PREFIX"]) / "bin/muqun-desktop").exists())

    def test_rejects_sidecar_disagreeing_with_hash_in_version_manifest(self):
        name = self.fixture(STABLE)
        side = self.files / (name + ".sha256")
        side.write_text("0" * 64 + "  " + name + "\n")
        self.write_metadata(name)
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
        self.confirm_gateway_setup()
        self.assertFalse((self.home / "gateway-invocations").exists())
        self.assertEqual(unit.read_text(), "existing external service")

    def test_macos_verifies_bundle_before_replacing(self):
        self.env.update(TEST_OS="Darwin", TEST_ARCH="arm64")
        self.fixture(STABLE, mac=True)
        self.command("ditto", 'unzip -q "$3" -d "$4"')
        self.command("plutil", "echo com.muqun.desktop")
        self.command("codesign", 'echo verify >> "$HOME/mac-checks"')
        self.command("spctl", 'echo assess >> "$HOME/mac-checks"; exit "${ASSESS_EXIT:-0}"')
        self.run_installer()
        app = self.home / "Applications/Muqun Desktop.app"
        self.assertEqual((self.home / "mac-checks").read_text(), "verify\nassess\n")
        self.assertFalse((self.home / "gateway-invocations").exists())
        self.fixture(PREVIEW, mac=True)
        self.env["ASSESS_EXIT"] = "1"
        self.run_installer("--tag", PREVIEW, success=False)
        self.assertEqual((app / "Contents/MacOS/muqun-desktop").read_text(), "1.0.2")

    def test_gateway_only_runs_without_python(self):
        directory = self.root / "installed helpers"
        directory.mkdir()
        shutil.copy2(ROOT / "install-desktop.sh", directory)
        (directory / "install.sh").write_bytes(self.helper)
        subprocess.run(["sh", directory / "install-desktop.sh", "--gateway-only"], env=self.env,
                       check=True, capture_output=True)
        self.assertEqual((self.home / "gateway-invocations").read_text(), "--channel stable --desktop-terminal\n")

    def test_incomplete_gateway_keeps_desktop_and_allows_manual_retry(self):
        self.fixture(STABLE)
        self.env["GATEWAY_EXIT"] = "1"
        self.run_installer()
        self.confirm_gateway_setup(success=False)
        self.env["GATEWAY_EXIT"] = "0"
        self.confirm_gateway_setup()
        self.assertEqual((self.home / "gateway-invocations").read_text().count("--channel stable"), 2)


if __name__ == "__main__":
    unittest.main()
