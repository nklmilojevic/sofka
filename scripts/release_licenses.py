"""Collect release notices and package a binary without changing its bytes."""

import argparse
import gzip
import hashlib
import io
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.error
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
LICENSE_NAMES = ("LICENSE", "LICENCE", "COPYING", "COPYRIGHT", "NOTICE")
REQUIRED = ("LICENSE-MIT", "LICENSE-APACHE", "THIRD-PARTY-LICENSES.txt")
# Kube 4.0.0 records an unavailable commit. All 86 Rust source files in its
# five crates match this published release tag. Use that tag's license.
SOURCE_REVISIONS = {
    (
        "https://github.com/kube-rs/kube",
        "7b4e520b3ef9d6aae8214577fe5f8f4f4ddb4fab",
    ): "b4f0cc4d7b4ce00ac7fa2d85c0120bbd38fa6210",
}


def download(url, cache):
    path = cache / hashlib.sha256(url.encode()).hexdigest()
    missing = path.with_suffix(".missing")
    if missing.exists():
        raise urllib.error.HTTPError(url, 404, "Not found", None, None)
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=cache) as temporary:
            response = subprocess.run(
                [
                    "curl",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--retry",
                    "2",
                    "--max-time",
                    "60",
                    "--output",
                    temporary.name,
                    "--write-out",
                    "%{http_code}",
                    url,
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            if response.stdout != "200":
                if response.stdout == "404":
                    missing.touch()
                raise urllib.error.HTTPError(
                    url, int(response.stdout), "Download failed", None, None
                )
            with tempfile.NamedTemporaryFile(dir=cache, delete=False) as saved:
                saved.write(Path(temporary.name).read_bytes())
            Path(saved.name).replace(path)
    return path.read_bytes()


def license_files(package):
    root = Path(package["manifest_path"]).parent
    files = {
        p
        for p in root.rglob("*")
        if p.is_file()
        and (
            p.name.upper().startswith(LICENSE_NAMES)
            or any(
                part.lstrip(".").upper() in ("LICENSES", "LICENCES")
                for part in p.relative_to(root).parts[:-1]
            )
        )
    }
    if package.get("license_file"):
        files.add(root / package["license_file"])
    return [(str(p.relative_to(root)), p.read_text()) for p in sorted(files)]


def repository_notices(package, cache):
    """Read omitted notices from the exact source commit recorded by Cargo."""
    root = Path(package["manifest_path"]).parent
    vcs = json.loads((root / ".cargo_vcs_info.json").read_text())
    commit = vcs["git"]["sha1"]
    repository = (package.get("repository") or "").removesuffix(".git")
    commit = SOURCE_REVISIONS.get((repository, commit), commit)
    if not re.fullmatch(r"https://github.com/[\w.-]+/[\w.-]+", repository):
        raise ValueError(f"Review the missing notices for {package['name']}")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Invalid source commit")
    base = repository.replace(
        "https://github.com/", "https://raw.githubusercontent.com/"
    )
    result = []
    for name in ("LICENSE", "LICENSE-MIT", "LICENSE-APACHE", "NOTICE", "COPYRIGHT"):
        url = f"{base}/{commit}/{name}"
        try:
            result.append((url, download(url, cache).decode()))
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
    if not result:
        raise ValueError(f"No source notices found for {package['name']}")
    return result


def generate(manifest, output, cargo_about, cache):
    manifest = manifest.resolve()
    output.mkdir(parents=True, exist_ok=True)
    lock = manifest.with_name("Cargo.lock")
    before = lock.read_bytes()
    subprocess.run(
        ["cargo", "fetch", "--locked", "--manifest-path", str(manifest)], check=True
    )
    with tempfile.TemporaryDirectory() as temporary:
        report = Path(temporary) / "licenses.json"
        subprocess.run(
            [
                cargo_about,
                "generate",
                "--locked",
                "--offline",
                "--fail",
                "--format",
                "json",
                "--manifest-path",
                str(manifest),
                "--config",
                str(ROOT / "about.toml"),
                "--output-file",
                str(report),
            ],
            check=True,
        )
        data = json.loads(report.read_text())
    if lock.read_bytes() != before:
        raise ValueError("License collection changed Cargo.lock")
    packages = sorted(
        (
            entry["package"]
            for entry in data["crates"]
            if Path(entry["package"]["manifest_path"]).resolve() != manifest
        ),
        key=lambda p: (p["name"], p["version"]),
    )
    if not packages or not data["licenses"]:
        raise ValueError("The dependency license report is empty")
    selected = {}
    for license in data["licenses"]:
        if not license["text"].strip():
            raise ValueError("An empty license was returned")
        for user in license["used_by"]:
            selected.setdefault(user["crate"]["id"], set()).add(license["id"])
    checksums = {
        (p["name"], p["version"]): p.get("checksum")
        for p in tomllib.loads(before.decode())["package"]
    }
    lines = [
        "Third-party licenses for Sofka",
        "",
        "This report covers the default release features on the four supported targets.",
        "It includes dependency license texts and notices from bundled source trees.",
        "Some notices can apply to code that is not included in a particular binary.",
        "The source links identify the exact published dependency versions.",
        "MPL 2.0 source packages are also included in THIRD-PARTY-SOURCES/.",
        "These source packages retain their original licenses.",
        "",
    ]
    for package in packages:
        name, version = package["name"], package["version"]
        if not re.fullmatch(r"[A-Za-z0-9_.+-]+", name + version):
            raise ValueError("Invalid package name or version")
        licenses = selected.get(package["id"])
        if not licenses:
            raise ValueError(f"No selected license for {name} {version}")
        if package["source"] != "registry+https://github.com/rust-lang/crates.io-index":
            raise ValueError(f"Review source distribution for {name} {version}")
        source = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
        lines.extend(
            [
                f"{name} {version}",
                f"Declared license: {package['license']}",
                f"Selected licenses: {', '.join(sorted(licenses))}",
                f"Source: {source}",
                "",
            ]
        )
        if "MPL-2.0" in licenses:
            archive = download(source, cache)
            if hashlib.sha256(archive).hexdigest() != checksums[(name, version)]:
                raise ValueError(f"Source checksum mismatch for {name} {version}")
            sources = output / "THIRD-PARTY-SOURCES"
            sources.mkdir(exist_ok=True)
            (sources / f"{name}-{version}.crate").write_bytes(archive)
    for license in data["licenses"]:
        users = sorted(
            {
                f"{u['crate']['name']} {u['crate']['version']}"
                for u in license["used_by"]
            }
        )
        lines.extend(
            [
                "=" * 72,
                license["name"],
                "Used by: " + ", ".join(users),
                "",
                license["text"],
                "",
            ]
        )
    for package in packages:
        notices = license_files(package)
        if not notices:
            notices = repository_notices(package, cache)
        for path, text in notices:
            if not text.strip():
                raise ValueError(f"Empty notice: {package['name']}/{path}")
            lines.extend(
                [
                    "=" * 72,
                    f"{package['name']} {package['version']}: {path}",
                    "",
                    text,
                    "",
                ]
            )
    (output / "THIRD-PARTY-LICENSES.txt").write_text("\n".join(lines))
    for name in REQUIRED[:2]:
        shutil.copyfile(manifest.parent / name, output / name)
    validate_notices(output)


def validate_notices(directory):
    for name in REQUIRED:
        if not (directory / name).is_file() or not (directory / name).stat().st_size:
            raise ValueError(f"Missing or empty release notice: {name}")


def package(binary, notices, output):
    validate_notices(notices)
    original = binary.read_bytes()
    if not original:
        raise ValueError("The release binary is empty")
    output.parent.mkdir(parents=True, exist_ok=True)
    with (
        output.open("wb") as raw,
        gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed,
        tarfile.open(fileobj=compressed, mode="w") as archive,
    ):
        entry = tarfile.TarInfo("sofka")
        entry.size = len(original)
        entry.mode = 0o755
        archive.addfile(entry, io.BytesIO(original))
        for path in sorted(notices.rglob("*")):
            if path.is_symlink():
                raise ValueError(f"A notice must not be a symlink: {path}")
            if path.is_file():
                entry = tarfile.TarInfo(str(path.relative_to(notices)))
                content = path.read_bytes()
                entry.size = len(content)
                entry.mode = 0o644
                archive.addfile(entry, io.BytesIO(content))
    with tarfile.open(output) as archive:
        if archive.extractfile("sofka").read() != original:
            raise ValueError("Packaging changed the release binary")
        for name in REQUIRED:
            if archive.extractfile(name).read() != (notices / name).read_bytes():
                raise ValueError(f"Archive notice mismatch: {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    collect = commands.add_parser("generate")
    collect.add_argument("--manifest", type=Path, default=ROOT / "Cargo.toml")
    collect.add_argument("--output", type=Path, required=True)
    collect.add_argument("--cargo-about", default="cargo-about")
    collect.add_argument("--cache", type=Path, default=ROOT / "target/license-cache")
    pack = commands.add_parser("package")
    pack.add_argument("--binary", type=Path, required=True)
    pack.add_argument("--notices", type=Path, required=True)
    pack.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "generate":
        generate(args.manifest, args.output, args.cargo_about, args.cache)
    else:
        package(args.binary, args.notices, args.output)


if __name__ == "__main__":
    main()
