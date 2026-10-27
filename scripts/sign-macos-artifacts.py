"""Sign verified CI artifacts using an existing local Developer ID identity."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import struct
import subprocess

REPOSITORY = "osuki-dev/muqun-gateway"
IDENTIFIER = "dev.osuki.muqun-gateway"
TARGETS = {"aarch64-apple-darwin": 0x0100000C, "x86_64-apple-darwin": 0x01000007}
MAX_BINARY_BYTES = 128 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


def validate_context(environment, source_sha, version):
    require(environment.get("GITHUB_REPOSITORY") == REPOSITORY, "Unexpected repository")
    require(re.fullmatch(r"[a-f0-9]{40}", source_sha), "Invalid source SHA")
    require(environment.get("GITHUB_SHA") == source_sha, "Source SHA differs from caller")
    event = environment.get("GITHUB_EVENT_NAME")
    ref = environment.get("GITHUB_REF")
    if event == "workflow_dispatch" and ref == "refs/heads/main":
        require(re.fullmatch(r"\d+\.\d+\.\d+-preview\.[1-9]\d*", version), "Invalid preview version")
        require(version.endswith("-preview." + environment.get("GITHUB_RUN_NUMBER", "")),
                "Preview version differs from this run")
    else:
        require(event == "push" and ref == "refs/tags/v" + version
                and re.fullmatch(r"\d+\.\d+\.\d+", version), "Untrusted signing event or ref")


def sha256(path):
    with path.open("rb") as stream:
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
        return digest.hexdigest()


def verified_inputs(directory):
    expected = {"muqun-gateway-" + target + suffix for target in TARGETS for suffix in ("", ".sha256")}
    require({path.name for path in directory.iterdir()} == expected, "Unexpected or incomplete input matrix")
    inputs = []
    for target, cpu_type in TARGETS.items():
        binary = directory / ("muqun-gateway-" + target)
        checksum = directory / (binary.name + ".sha256")
        for path in (binary, checksum):
            require(not path.is_symlink() and stat.S_ISREG(path.stat().st_mode), "Unsafe artifact path")
        require(32 <= binary.stat().st_size <= MAX_BINARY_BYTES, "Unexpected binary size")
        require(checksum.stat().st_size <= 1024, "Oversized checksum")
        match = re.fullmatch(r"([a-f0-9]{64})  " + re.escape(binary.name) + r"\n", checksum.read_text())
        require(match is not None and sha256(binary) == match[1], "Artifact checksum mismatch")
        with binary.open("rb") as stream:
            header = stream.read(8)
        require(header[:4] == b"\xcf\xfa\xed\xfe" and struct.unpack("<I", header[4:])[0] == cpu_type,
                "Artifact has the wrong Mach-O platform")
        inputs.append((target, binary, match[1]))
    return inputs


def command(arguments):
    result = subprocess.run(arguments, capture_output=True, text=True, timeout=120)
    require(result.returncode == 0, "codesign failed; inspect the signer locally")
    return result.stderr


def sign(directory, output, audit_path, identity, team, source_sha, version):
    require(platform.system() == "Darwin", "Signing requires the trusted macOS signer")
    require(re.fullmatch(r"[A-Fa-f0-9]{40}", identity), "Expected an existing certificate SHA-1 identity")
    require(re.fullmatch(r"[A-Z0-9]{10}", team), "Invalid expected Team ID")
    inputs = verified_inputs(directory)
    require(not output.exists() and not audit_path.exists(), "Refusing to overwrite signing output")
    output.mkdir(mode=0o700, parents=True)
    requirement = ('anchor apple generic and identifier "' + IDENTIFIER + '" '
                   'and certificate leaf[subject.OU] = "' + team + '" '
                   'and certificate leaf[field.1.2.840.113635.100.6.1.13] exists')
    records = []
    for target, binary, before in inputs:
        destination = output / binary.name
        shutil.copyfile(binary, destination)
        require(sha256(destination) == before, "Artifact changed while copying to signing output")
        destination.chmod(0o755)
        command(["/usr/bin/codesign", "--force", "--sign", identity,
                 "--identifier", IDENTIFIER, "--timestamp", str(destination)])
        command(["/usr/bin/codesign", "--verify", "--strict", "--verbose=2",
                 "-R=" + requirement, str(destination)])
        metadata = command(["/usr/bin/codesign", "-dvvv", str(destination)]).splitlines()
        public = [line for line in metadata if line.startswith(
            ("Identifier=", "Authority=", "TeamIdentifier=", "Timestamp=", "CDHash="))]
        require("Identifier=" + IDENTIFIER in public and "TeamIdentifier=" + team in public
                and any(line.startswith("Authority=Developer ID Application:") for line in public)
                and any(line.startswith("Timestamp=") for line in public), "Unexpected signing identity or timestamp")
        after = sha256(destination)
        (output / (binary.name + ".sha256")).write_text(after + "  " + binary.name + "\n")
        records.append({"target": target, "input_sha256": before,
                        "signed_sha256": after, "signature": public})
    with audit_path.open("x") as stream:
        json.dump({"source_sha": source_sha, "version": version, "identifier": IDENTIFIER,
                   "team_id": team, "identity_sha1": identity, "artifacts": records}, stream, indent=2)
        stream.write("\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--check-context", action="store_true")
    parser.add_argument("--input", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--audit", type=Path)
    args = parser.parse_args()
    validate_context(os.environ, args.source_sha, args.version)
    if args.check_context:
        for name in ("MACOS_SIGNING_RUNNER_GROUP", "MACOS_SIGNING_IDENTITY_SHA1", "MACOS_SIGNING_TEAM_ID"):
            require(bool(os.environ.get(name)), "Missing repository variable: " + name)
        return
    require(args.input is not None and args.output is not None and args.audit is not None,
            "Input, output and audit paths are required")
    sign(args.input, args.output, args.audit, os.environ.get("MACOS_SIGNING_IDENTITY_SHA1", ""),
         os.environ.get("MACOS_SIGNING_TEAM_ID", ""), args.source_sha, args.version)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error)) from None
