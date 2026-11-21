import hashlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from archive_desktop_ci import archive
from mirror_release import publish


class Store:
    def __init__(self):
        self.objects = {}
        self.race = None
        self.corrupt = None
        self.writes = []

    def get(self, key):
        data = self.objects.get(key)
        return None if data is None else (io.BytesIO(data), hashlib.sha256(data).hexdigest())

    def put(self, key, stream, content_type, etag=None):
        if self.race and "/channels/" in key:
            self.objects[key] = self.race
            self.race = None
        current = self.get(key)
        if etag != (current[1] if current else None):
            return False
        self.objects[key] = b"corrupted" if key == self.corrupt else stream.read()
        self.writes.append(key)
        return True


class MirrorTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        self.store = Store()

    def release(self, version, *, bad_checksum=False):
        tag = "desktop-v" + version
        assets = []
        for package in ["muqun-desktop-linux-x86_64.tar.gz", f"muqun-desktop-{version}-macos-arm64.zip"]:
            content = (version + " QA " + package).encode()
            digest = hashlib.sha256(content).hexdigest()
            checksum = (("0" * 64 if bad_checksum else digest) + "  " + package + "\n").encode()
            for name, data in [(package, content), (package + ".sha256", checksum)]:
                (self.directory / name).write_bytes(data)
                assets.append(dict(name=name, size=len(data), digest="sha256:" + hashlib.sha256(data).hexdigest()))
        return dict(tagName=tag, isPrerelease="-preview." in version, isDraft=False, assets=assets)

    def send(self, release):
        publish(self.store, release, release["tagName"], self.directory)

    def test_separate_channels_and_replaying_old_release(self):
        for version in ["1.2.1", "1.2.10", "1.2.10-preview.12"]:
            self.send(self.release(version))
        writes = self.store.writes.copy()
        self.send(self.release("1.2.1"))
        self.assertEqual(writes, self.store.writes)
        self.assertEqual("1.2.10", json.loads(self.store.objects["desktop/channels/stable.json"])["version"])
        preview = json.loads(self.store.objects["desktop/channels/preview.json"])
        self.assertEqual("1.2.10-preview.12", preview["version"])
        manifest = json.loads(self.store.objects["desktop/releases/desktop-v1.2.10/manifest.json"])
        for asset in manifest["assets"]:
            remote = self.store.objects["desktop/releases/desktop-v1.2.10/" + asset["name"]]
            self.assertEqual(asset["sha256"], hashlib.sha256(remote).hexdigest())
            self.assertEqual(asset["size"], len(remote))

    def test_gateway_backfill_isolated_and_verified_against_github(self):
        self.send(self.release("2.0.0"))
        release = dict(tagName="v1.0.0", isDraft=False, isPrerelease=False, assets=[])
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin",
                       "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"]:
            name = "muqun-gateway-" + target
            data = target.encode()
            (self.directory / name).write_bytes(data)
            release["assets"].append(dict(name=name, size=len(data),
                                          digest="sha256:" + hashlib.sha256(data).hexdigest()))
        upstream = release["assets"][0]
        digest = upstream.pop("digest")
        with self.assertRaises(ValueError):
            self.send(release)
        upstream["digest"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            self.send(release)
        self.assertFalse(any(key.startswith("gateway/") for key in self.store.objects))
        upstream["digest"] = digest
        self.send(release)
        self.assertEqual("2.0.0", json.loads(self.store.objects["desktop/channels/stable.json"])["version"])
        self.assertEqual("1.0.0", json.loads(self.store.objects["gateway/channels/stable.json"])["version"])
        for asset in release["assets"]:
            name = asset["name"]
            checksum = self.store.objects["gateway/releases/v1.0.0/" + name + ".sha256"].decode()
            self.assertEqual(asset["digest"][7:] + "  " + name + "\n", checksum)

    def test_ci_archive_requires_complete_outputs_and_preserves_platform_and_signing(self):
        version = "1.2.3-preview.17"
        mac_package = f"muqun-desktop-{version}-macos-arm64.zip"
        for group, package, extras in [
                ("muqun-desktop-Linux", "muqun-desktop-linux-x86_64.tar.gz", ["muqun-desktop", "muqun-wry-embedded"]),
                ("muqun-desktop-macOS", mac_package, ["muqun-desktop"]),
                ("desktop-preview-signed-macos", mac_package, [])]:
            folder = self.directory / group
            folder.mkdir()
            for name in [package, *extras]:
                (folder / name).write_bytes((group + name).encode())
            digest = hashlib.sha256((folder / package).read_bytes()).hexdigest()
            (folder / (package + ".sha256")).write_text(f"{digest}  {package}\n")
        mac_binary = self.directory / "muqun-desktop-macOS" / "muqun-desktop"
        mac_binary.unlink()
        with self.assertRaisesRegex(ValueError, "Incomplete"):
            archive(self.store, self.directory, "193", "2", "a" * 40, signed=True)
        self.assertFalse(self.store.objects)
        mac_binary.write_bytes(b"mac binary")
        key = archive(self.store, self.directory, "193", "2", "a" * 40, signed=True)
        self.assertEqual(key, f"desktop/ci/{version}/193/2/manifest.json")
        prefix = key.removesuffix("manifest.json")
        manifest = json.loads(self.store.objects[key])
        self.assertEqual(len(manifest["assets"]), 9)
        for info in manifest["assets"]:
            data = self.store.objects[prefix + info["path"]]
            self.assertEqual(info["size"], len(data))
            self.assertEqual(info["sha256"], hashlib.sha256(data).hexdigest())
        self.assertTrue(all(name.startswith(prefix) for name in self.store.objects))
        self.assertNotEqual(self.store.objects[prefix + "muqun-desktop-macOS/" + mac_package],
                            self.store.objects[prefix + "desktop-preview-signed-macos/" + mac_package])

    def test_checks_upstream_before_any_upload(self):
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.send(self.release("1.2.1", bad_checksum=True))
        missing = self.release("1.2.1")
        missing["assets"].pop()
        with self.assertRaisesRegex(ValueError, "Incomplete or unexpected"):
            self.send(missing)
        self.assertFalse(self.store.objects)

    def test_corrupted_upload_does_not_advance_and_cannot_be_overwritten(self):
        release = self.release("1.2.1")
        self.store.corrupt = "desktop/releases/desktop-v1.2.1/muqun-desktop-linux-x86_64.tar.gz"
        with self.assertRaisesRegex(ValueError, "Stored bytes differ"):
            self.send(release)
        self.store.corrupt = None
        with self.assertRaisesRegex(ValueError, "Stored bytes differ"):
            self.send(release)
        self.assertNotIn("desktop/channels/stable.json", self.store.objects)
        self.assertEqual(len(self.store.writes), 1)

    def test_backfill_loses_race_to_newer_publication(self):
        self.send(self.release("0.3.0-preview.2"))
        self.store.race = b'{"channel":"preview","version":"0.3.0-preview.11"}'
        self.send(self.release("0.3.0-preview.10"))
        pointer = json.loads(self.store.objects["desktop/channels/preview.json"])
        self.assertEqual("0.3.0-preview.11", pointer["version"])
