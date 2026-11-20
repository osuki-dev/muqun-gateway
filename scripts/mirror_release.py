"""Mirror one published Muqun release into the shared themes bucket."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re

BUCKET = "muqun-themes"
NUMBER = r"(0|[1-9][0-9]*)"
VERSION = re.compile(rf"{NUMBER}\.{NUMBER}\.{NUMBER}(?:-preview\.([1-9][0-9]*))?")


def version_key(version):
    match = VERSION.fullmatch(version)
    if not match:
        raise ValueError(f"Invalid release version: {version}")
    return tuple(int(part) for part in match.groups() if part is not None)


def release_assets(release, tag):
    if tag.startswith("desktop-v"):
        product = "desktop"
        version = tag[len("desktop-v"):]
        packages = {
            "muqun-desktop-linux-x86_64.tar.gz": "linux-x86_64",
            f"muqun-desktop-{version}-macos-arm64.zip": "macos-arm64",
        }
    elif tag.startswith("v"):
        product = "gateway"
        version = tag[1:]
        targets = ("x86_64-apple-darwin", "aarch64-apple-darwin",
                   "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl")
        packages = {f"muqun-gateway-{target}": target for target in targets}
    else:
        raise ValueError(f"Unrecognized release tag: {tag}")
    preview = len(version_key(version)) == 4
    if (release["tagName"] != tag or release["isDraft"]
            or release["isPrerelease"] != preview):
        raise ValueError("Expected a published release with matching tag and channel")

    names = set(packages) | {name + ".sha256" for name in packages}
    assets = {asset["name"]: asset for asset in release["assets"]}
    # Older Gateway releases predate companion files. Only accept their complete
    # binary matrix when GitHub has recorded a SHA-256 for every binary.
    legacy = product == "gateway" and set(assets) == set(packages) and all(
        re.fullmatch(r"sha256:[0-9a-f]{64}", asset.get("digest") or "") for asset in assets.values())
    if len(assets) != len(release["assets"]) or (set(assets) != names and not legacy):
        raise ValueError(f"Incomplete or unexpected {product} release assets for {tag}")
    return product, version, "preview" if preview else "stable", packages, assets


def sha256(stream):
    digest = hashlib.sha256()
    while chunk := stream.read(1024 * 1024):
        digest.update(chunk)
    return digest.hexdigest()


def json_bytes(value):
    return (json.dumps(value, separators=(",", ":")) + "\n").encode()


class R2:
    def __init__(self):
        import boto3
        from botocore.config import Config
        from botocore.exceptions import ClientError
        self.client_error = ClientError
        account = os.environ["CLOUDFLARE_ACCOUNT_ID"]
        if not re.fullmatch("[0-9a-fA-F]{32}", account):
            raise ValueError("Invalid Cloudflare account ID")
        self.s3 = boto3.client("s3", endpoint_url=f"https://{account}.r2.cloudflarestorage.com",
                               region_name="auto", config=Config(request_checksum_calculation="when_required",
                                                                 response_checksum_validation="when_required"))

    def get(self, key):
        try:
            response = self.s3.get_object(Bucket=BUCKET, Key=key)
        except self.client_error as error:
            if error.response["Error"]["Code"] == "NoSuchKey":
                return None
            raise
        return response["Body"], response["ETag"]

    def put(self, key, stream, content_type, etag=None):
        condition = {"IfMatch": etag} if etag else {"IfNoneMatch": "*"}
        try:
            self.s3.put_object(Bucket=BUCKET, Key=key, Body=stream, ContentType=content_type,
                               CacheControl="no-cache" if "/channels/" in key else "public,max-age=31536000,immutable",
                               **condition)
        except self.client_error as error:
            if error.response["ResponseMetadata"]["HTTPStatusCode"] in (409, 412):
                return False
            raise
        return True


def immutable(store, key, stream, digest, content_type):
    existing = store.get(key)
    if existing is None:
        store.put(key, stream, content_type)
        existing = store.get(key)
    if existing is None:
        raise ValueError(f"Missing object after upload: {key}")
    with existing[0] as remote:
        if sha256(remote) != digest:
            raise ValueError(f"Stored bytes differ at immutable key: {key}")


def asset_info(path, platform, upstream):
    with path.open("rb") as stream:
        digest = sha256(stream)
    size = path.stat().st_size
    if size == 0:
        raise ValueError(f"Empty release asset: {path.name}")
    if upstream is not None:
        if size != upstream["size"] or upstream.get("digest") not in (None, "", "sha256:" + digest):
            raise ValueError(f"Release asset size or digest mismatch: {path.name}")
    return dict(name=path.name, platform=platform, size=size, sha256=digest)


def publish(store, release, tag, directory):
    product, version, channel, packages, github_assets = release_assets(release, tag)
    prefix = f"{product}/releases/{tag}/"
    assets = []
    for package, platform in packages.items():
        binary = asset_info(directory / package, platform, github_assets[package])
        checksum_path = directory / (package + ".sha256")
        upstream = github_assets.get(checksum_path.name)
        if upstream is None:
            # A legacy binary's digest was required and checked against GitHub.
            checksum_path.write_text(f"{binary['sha256']}  {package}\n", encoding="ascii")
        if checksum_path.stat().st_size > 1024:
            raise ValueError(f"Oversized companion checksum for {package}")
        checksum = checksum_path.read_bytes().decode("ascii")
        separator = "  " if product == "gateway" else r" [ *]"
        if not re.fullmatch(binary["sha256"] + separator + re.escape(package) + r"\n?", checksum):
            raise ValueError(f"Invalid companion checksum for {package}")
        assets.extend((binary, asset_info(checksum_path, platform, upstream)))

    types = {".sha256": "text/plain", ".zip": "application/zip", ".gz": "application/gzip"}
    for asset in assets:
        name = asset["name"]
        content_type = types.get(Path(name).suffix, "application/octet-stream")
        with (directory / name).open("rb") as stream:
            immutable(store, prefix + name, stream, asset["sha256"], content_type)

    manifest = json_bytes(dict(channel=channel, version=version, tag=tag, assets=assets))
    immutable(store, prefix + "manifest.json", io.BytesIO(manifest),
              hashlib.sha256(manifest).hexdigest(), "application/json")
    key = f"{product}/channels/{channel}.json"
    for _ in range(8):
        current = store.get(key)
        etag = None
        if current:
            stream, etag = current
            with stream:
                pointer = json.load(stream)
            order = version_key(pointer["version"])
            if pointer["channel"] != channel or len(order) != len(version_key(version)):
                raise ValueError(f"Invalid channel pointer at {key}")
            if order >= version_key(version):
                return
        if store.put(key, io.BytesIO(json_bytes(dict(channel=channel, version=version))), "application/json", etag):
            return
    raise RuntimeError(f"Concurrent writes at {key}; retry publication")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("release", type=Path)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    publish(R2(), json.loads(args.release.read_text()), args.tag, args.directory)
