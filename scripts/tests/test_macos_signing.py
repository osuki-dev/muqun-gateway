import hashlib
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("signing", Path(__file__).parents[1] / "sign-macos-artifacts.py")
signing = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(signing)
SHA = "a" * 40


class SigningBoundaryTests(unittest.TestCase):
    def test_only_canonical_main_preview_or_stable_tag_can_sign(self):
        environment = {"GITHUB_REPOSITORY": signing.REPOSITORY, "GITHUB_SHA": SHA,
                       "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/main",
                       "GITHUB_RUN_NUMBER": "14"}
        signing.validate_context(environment, SHA, "0.14.1-preview.14")
        for changed in ({"GITHUB_REPOSITORY": "attacker/fork"}, {"GITHUB_EVENT_NAME": "pull_request"},
                        {"GITHUB_REF": "refs/heads/feature"}, {"GITHUB_SHA": "b" * 40}):
            with self.subTest(changed=changed), self.assertRaises(ValueError):
                signing.validate_context({**environment, **changed}, SHA, "0.14.1-preview.14")
        with self.assertRaises(ValueError):
            signing.validate_context(environment, SHA, "0.14.1-preview.13")
        stable = {**environment, "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/tags/v0.15.0"}
        signing.validate_context(stable, SHA, "0.15.0")
        with self.assertRaises(ValueError):
            signing.validate_context(stable, SHA, "0.15.1")

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.raw = self.root / "raw"
        self.raw.mkdir()
        for target, cpu in signing.TARGETS.items():
            path = self.raw / ("muqun-gateway-" + target)
            path.write_bytes(b"\xcf\xfa\xed\xfe" + struct.pack("<I", cpu) + bytes(24))
            self.checksum(path)

    def checksum(self, path):
        (self.raw / (path.name + ".sha256")).write_text(
            hashlib.sha256(path.read_bytes()).hexdigest() + "  " + path.name + "\n")

    def test_input_matrix_rejects_tampering_architecture_and_extra_files(self):
        self.assertEqual(len(signing.verified_inputs(self.raw)), 2)
        path = self.raw / "muqun-gateway-aarch64-apple-darwin"
        original = path.read_bytes()
        path.write_bytes(original + b"tampered")
        with self.assertRaises(ValueError):
            signing.verified_inputs(self.raw)
        path.write_bytes(original[:4] + struct.pack("<I", 0x01000007) + original[8:])
        self.checksum(path)
        with self.assertRaises(ValueError):
            signing.verified_inputs(self.raw)
        path.write_bytes(original)
        self.checksum(path)
        (self.raw / "unexpected").write_text("not an artifact")
        with self.assertRaises(ValueError):
            signing.verified_inputs(self.raw)

    def test_checksum_symlinks_and_path_substitution_are_rejected(self):
        path = self.raw / "muqun-gateway-aarch64-apple-darwin.sha256"
        outside = self.root / "outside"
        path.rename(outside)
        path.symlink_to(outside)
        with self.assertRaises(ValueError):
            signing.verified_inputs(self.raw)
        path.unlink()
        path.write_text("0" * 64 + "  ../outside\n")
        with self.assertRaises(ValueError):
            signing.verified_inputs(self.raw)

    def test_wrong_team_never_produces_a_publishable_audit(self):
        output = self.root / "signed"
        audit = self.root / "audit.json"
        metadata = ("Identifier=" + signing.IDENTIFIER + "\nTeamIdentifier=WRONGTEAM0\n"
                    "Authority=Developer ID Application: Wrong team\nTimestamp=now\n")
        with patch.object(signing.platform, "system", return_value="Darwin"), \
                patch.object(signing, "command", return_value=metadata), self.assertRaises(ValueError):
            signing.sign(self.raw, output, audit, "A" * 40, "EXPECTED00", SHA, "0.15.0")
        self.assertFalse(audit.exists())

    def test_copy_drift_is_rejected_before_accessing_the_signing_identity(self):
        def changed_copy(_source, destination):
            destination.write_bytes(b"different bytes")
        with patch.object(signing.platform, "system", return_value="Darwin"), \
                patch.object(signing.shutil, "copyfile", side_effect=changed_copy), \
                patch.object(signing, "command") as codesign, self.assertRaises(ValueError):
            signing.sign(self.raw, self.root / "signed", self.root / "audit.json",
                         "A" * 40, "EXPECTED00", SHA, "0.15.0")
        codesign.assert_not_called()


if __name__ == "__main__":
    unittest.main()
