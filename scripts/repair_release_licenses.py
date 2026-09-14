"""Prepare and publish license corrections for existing GitHub releases."""

import argparse
import concurrent.futures
import hashlib
import io
import json
import re
import subprocess
import tarfile
import tempfile
from pathlib import Path

import release_licenses

REPO = "nklmilojevic/sofka"
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def binary_from_archive(path):
    with tarfile.open(path) as archive:
        members = archive.getmembers()
        binaries = [m for m in members if m.name.removeprefix("./") == "sofka"]
        if len(binaries) != 1 or not binaries[0].isfile():
            raise ValueError(f"Expected one regular sofka binary in {path}")
        if binaries[0].size > 200_000_000:
            raise ValueError(f"Unexpected binary size in {path}")
        return archive.extractfile(binaries[0]).read()


def prepare(release, directory, cargo_about):
    tag = release["tag_name"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError(f"Invalid release tag: {tag}")
    destination = directory / tag
    destination.mkdir(parents=True, exist_ok=True)
    record_path = destination / "LICENSE-CORRECTION.json"
    if record_path.exists():
        record = json.loads(record_path.read_text())
        for asset in record["assets"]:
            if (
                digest(destination / asset["corrected_name"])
                != asset["corrected_sha256"]
            ):
                raise ValueError(f"Changed correction archive for {tag}")
        print(f"{tag}: reuse verified correction", flush=True)
        return
    print(f"{tag}: collect notices and original binaries", flush=True)
    source = destination / "source"
    source.mkdir(exist_ok=True)
    revision = subprocess.check_output(
        ["git", "rev-parse", f"{tag}^{{commit}}"], text=True
    ).strip()
    archive = subprocess.check_output(["git", "archive", tag])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
        tree.extractall(source, filter="data")
    notices = destination / "notices"
    release_licenses.generate(
        source / "Cargo.toml", notices, cargo_about, directory / "cache"
    )
    assets = {a["name"]: a for a in release["assets"]}
    record = {"tag": tag, "source_commit": revision, "assets": []}
    original_dir = destination / "original"
    original_dir.mkdir(exist_ok=True)
    for target in TARGETS:
        original_name = f"sofka-{tag}-{target}.tar.gz"
        original = assets[original_name]
        archive = original_dir / original_name
        if not archive.exists():
            gh(
                "release",
                "download",
                tag,
                "--repo",
                REPO,
                "--pattern",
                original_name,
                "--dir",
                str(original_dir),
            )
        original_sha = digest(archive)
        if original.get("digest") != "sha256:" + original_sha:
            raise ValueError(f"GitHub asset checksum mismatch: {original_name}")
        binary = binary_from_archive(archive)
        binary_path = destination / "sofka"
        binary_path.write_bytes(binary)
        corrected_name = f"sofka-{tag}-{target}-licenses.tar.gz"
        corrected = destination / corrected_name
        release_licenses.package(binary_path, notices, corrected)
        if binary_from_archive(corrected) != binary:
            raise ValueError(f"Binary changed: {corrected_name}")
        record["assets"].append(
            {
                "original_name": original_name,
                "original_sha256": original_sha,
                "corrected_name": corrected_name,
                "corrected_sha256": digest(corrected),
                "binary_sha256": hashlib.sha256(binary).hexdigest(),
            }
        )
    (destination / "SHA256SUMS-licenses").write_text(
        "".join(
            f"{a['corrected_sha256']}  {a['corrected_name']}\n"
            for a in record["assets"]
        )
    )
    record_path.write_text(json.dumps(record, indent=2) + "\n")
    print(f"{tag}: prepared and verified four corrections", flush=True)


def publish(directory):
    for record_path in sorted(directory.glob("v*/LICENSE-CORRECTION.json")):
        record = json.loads(record_path.read_text())
        files = [record_path, record_path.with_name("SHA256SUMS-licenses")]
        for asset in record["assets"]:
            path = record_path.with_name(asset["corrected_name"])
            if digest(path) != asset["corrected_sha256"]:
                raise ValueError(f"Changed correction archive: {path}")
            files.append(path)
        existing = json.loads(gh("api", f"repos/{REPO}/releases/tags/{record['tag']}"))
        remote = {a["name"]: a for a in existing["assets"]}
        upload = []
        for path in files:
            if path.name in remote:
                if remote[path.name].get("digest") != "sha256:" + digest(path):
                    raise ValueError(
                        f"An asset already exists with different bytes: {path.name}"
                    )
            else:
                upload.append(str(path))
        if upload:
            gh("release", "upload", record["tag"], "--repo", REPO, *upload)
        verified = json.loads(gh("api", f"repos/{REPO}/releases/tags/{record['tag']}"))
        remote = {a["name"]: a for a in verified["assets"]}
        for path in files:
            if remote[path.name].get("digest") != "sha256:" + digest(path):
                raise ValueError(f"Uploaded asset checksum mismatch: {path.name}")
        marker = "<!-- release-license-correction -->"
        body = verified.get("body") or ""
        if marker not in body:
            body += (
                f"\n\n{marker}\n"
                "License correction: use the `-licenses.tar.gz` downloads. "
                "They contain the original binaries with license notices and "
                "required source packages. Verify them with `SHA256SUMS-licenses`. "
                "`LICENSE-CORRECTION.json` records the original archive hashes, "
                "corrected archive hashes, and unchanged binary hashes.\n"
            )
            with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as notes:
                notes.write(body)
                notes.flush()
                gh(
                    "release",
                    "edit",
                    record["tag"],
                    "--repo",
                    REPO,
                    "--notes-file",
                    notes.name,
                )
        print(f"{record['tag']}: published and verified corrections", flush=True)


def retire(directory):
    for record_path in sorted(directory.glob("v*/LICENSE-CORRECTION.json")):
        record = json.loads(record_path.read_text())
        release = json.loads(gh("api", f"repos/{REPO}/releases/tags/{record['tag']}"))
        remote = {a["name"]: a for a in release["assets"]}
        for asset in record["assets"]:
            if (
                remote.get(asset["corrected_name"], {}).get("digest")
                != "sha256:" + asset["corrected_sha256"]
            ):
                raise ValueError(
                    f"Missing verified replacement for {asset['original_name']}"
                )
            if (
                asset["original_name"] in remote
                and remote[asset["original_name"]].get("digest")
                != "sha256:" + asset["original_sha256"]
            ):
                raise ValueError(f"Original asset changed: {asset['original_name']}")
        if "SHA256SUMS" in remote:
            checksums = gh(
                "release",
                "download",
                record["tag"],
                "--repo",
                REPO,
                "--pattern",
                "SHA256SUMS",
                "--output",
                "-",
            )
            expected = {
                a["original_name"]: a["original_sha256"] for a in record["assets"]
            }
            actual = {}
            for line in checksums.splitlines():
                checksum, name = line.split()
                actual[name.removeprefix("*")] = checksum
            if actual != expected:
                raise ValueError(
                    f"Unexpected original checksum file for {record['tag']}"
                )
        for asset in record["assets"]:
            if asset["original_name"] in remote:
                gh(
                    "release",
                    "delete-asset",
                    record["tag"],
                    asset["original_name"],
                    "--repo",
                    REPO,
                    "--yes",
                )
        if "SHA256SUMS" in remote:
            gh(
                "release",
                "delete-asset",
                record["tag"],
                "SHA256SUMS",
                "--repo",
                REPO,
                "--yes",
            )
        print(f"{record['tag']}: retired incomplete binary archives", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("prepare", "publish", "retire"))
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--tag", default="all")
    parser.add_argument("--cargo-about", default="cargo-about")
    args = parser.parse_args()
    if args.command == "prepare":
        pages = json.loads(gh("api", "--paginate", "--slurp", f"repos/{REPO}/releases"))
        releases = [r for page in pages for r in page if not r["draft"]]
        if args.tag != "all":
            releases = [r for r in releases if r["tag_name"] == args.tag]
        if not releases:
            raise ValueError("No matching releases")
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            results = [
                pool.submit(prepare, r, args.directory, args.cargo_about)
                for r in releases
            ]
            for result in concurrent.futures.as_completed(results):
                result.result()
    elif args.command == "publish":
        publish(args.directory)
    else:
        retire(args.directory)


if __name__ == "__main__":
    main()
