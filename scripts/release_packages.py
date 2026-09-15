"""Build and check release packages with GoReleaser OSS."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    "x86_64-unknown-linux-gnu": "ubuntu-22.04",
    "aarch64-unknown-linux-gnu": "ubuntu-22.04-arm",
    "x86_64-unknown-linux-musl": "ubuntu-22.04",
    "aarch64-unknown-linux-musl": "ubuntu-22.04-arm",
    "x86_64-apple-darwin": "macos-15-intel",
    "aarch64-apple-darwin": "macos-latest",
    "x86_64-pc-windows-msvc": "windows-2025",
    "aarch64-pc-windows-msvc": "windows-11-arm",
}
REQUIRED = ("LICENSE-MIT", "LICENSE-APACHE", "THIRD-PARTY-LICENSES.txt")


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output(*args):
    return run(*args, capture_output=True, text=True).stdout.strip()


def regular_files(directory):
    result = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"Release inputs cannot be symlinks: {path}")
        if path.is_file():
            result[path.relative_to(directory).as_posix()] = path.read_bytes()
    return result


def stage_notices(notices, destination, rust_notices, target):
    files = regular_files(notices)
    for name in REQUIRED:
        if not files.get(name):
            raise ValueError(f"Missing or empty release notice: {name}")
    if any(name in files for name in ("sofka", "sofka.exe", "RUST-LICENSES.html")):
        raise ValueError("Release notices contain a reserved filename")
    if rust_notices.is_symlink() or not rust_notices.is_file():
        raise ValueError("The Rust library notice must be a regular file")
    files["RUST-LICENSES.html"] = rust_notices.read_bytes()
    if not files["RUST-LICENSES.html"]:
        raise ValueError("The Rust library notice is empty")
    if target.endswith("musl"):
        files["MUSL-LICENSE.txt"] = (ROOT / "scripts/licenses/musl-COPYRIGHT").read_bytes()
    destination.mkdir(parents=True, exist_ok=False)
    for name, data in files.items():
        path = destination / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(0o644)


def configure(target, stage, dist, dependencies=None):
    import yaml

    config = yaml.safe_load((ROOT / ".goreleaser.yaml").read_text())
    if target not in TARGETS or target not in config["builds"][0]["targets"]:
        raise ValueError(f"Unsupported release target: {target}")
    config["dist"] = dist.as_posix()
    build = config["builds"][0]
    build["targets"] = [target]
    build["hooks"] = {"post": [{"cmd": f'uv run --locked python scripts/release_packages.py check-binary --target {target} --binary "{{{{ .Path }}}}"'}]}
    if target.endswith("musl"):
        build["env"] = ["CC=musl-gcc", "RUSTFLAGS=-C link-self-contained=yes"]
    elif target.endswith("msvc"):
        build["env"] = ["RUSTFLAGS=-C target-feature=+crt-static"]
    if "linux" not in target:
        config.pop("nfpms")
    else:
        package = config["nfpms"][0]
        # nFPM 2.47.0 tree entries write invalid APK directory modes (nfpm#1112).
        package["contents"].extend(
            {
                "src": "{{ .Env.SOFKA_RELEASE_NOTICES }}/" + name,
                "dst": "/usr/share/doc/sofka/" + name,
                "file_info": {"mode": 0o644},
            }
            for name in regular_files(stage / "notices")
        )
        if target.endswith("musl"):
            package["formats"] = ["apk"]
            package["dependencies"] = ["ca-certificates"]
        else:
            package["overrides"] = dependencies or {}
    return config


def glibc_version(text):
    versions = re.findall(r"\bGLIBC_(\d+\.\d+(?:\.\d+)?)\b", text)
    if not versions:
        raise ValueError("No GLIBC symbol versions found")
    newest = max(versions, key=lambda value: tuple(map(int, value.split("."))))
    if tuple(map(int, newest.split("."))) > (2, 35):
        raise ValueError(f"The binary requires GLIBC_{newest}; the limit is GLIBC_2.35")
    return newest


def check_binary(target, binary):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if output(binary.resolve(), "--version") != f"sofka {version}":
        raise ValueError("The binary version differs from Cargo.toml")
    if "linux" not in target:
        return
    dynamic = output("readelf", "--dynamic", binary)
    needed = re.findall(r"Shared library: \[(.*?)\]", dynamic)
    if target.endswith("musl"):
        if needed or "INTERP" in output("readelf", "--program-headers", binary):
            raise ValueError("The Alpine binary must be statically linked")
        return
    allowed = {"libgcc_s.so.1", "libc.so.6", "libm.so.6", "libpthread.so.0", "libdl.so.2", "librt.so.1", "ld-linux-aarch64.so.1", "ld-linux-x86-64.so.2"}
    if set(needed) - allowed:
        raise ValueError(f"Unmapped shared libraries: {set(needed) - allowed}")
    glibc = glibc_version(output("readelf", "--version-info", binary))
    with tempfile.TemporaryDirectory() as temporary:
        control = Path(temporary) / "debian/control"
        control.parent.mkdir()
        control.write_text("Source: sofka\n\nPackage: sofka\nArchitecture: any\n")
        depends = run("dpkg-shlibdeps", "-O", "-e" + str(binary.resolve()), cwd=temporary, capture_output=True, text=True).stdout.strip()
    prefix = "shlibs:Depends="
    if not depends.startswith(prefix) or not depends.removeprefix(prefix):
        raise ValueError("dpkg-shlibdeps returned no runtime dependencies")
    return {
        "deb": {"dependencies": [depends.removeprefix(prefix), "ca-certificates"]},
        "rpm": {"dependencies": [f"glibc >= {glibc}", "libgcc", "ca-certificates"]},
        "archlinux": {"dependencies": [f"glibc>={glibc}", "gcc-libs", "ca-certificates"]},
    }


def verify_archive(path, binary, notices):
    expected = regular_files(notices)
    expected[binary.name] = binary.read_bytes()
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            names = [item.filename for item in archive.infolist() if not item.is_dir()]
            actual = {name: archive.read(name) for name in names}
    else:
        with tarfile.open(path) as archive:
            members = [member for member in archive if member.isfile()]
            names = [member.name for member in members]
            actual = {member.name: archive.extractfile(member).read() for member in members}
    if len(names) != len(set(names)) or actual != expected:
        raise ValueError(f"The archive does not match its binary and notices: {path}")


def verify_packages(directory, target, binary, notices):
    if "linux" not in target:
        return
    expected = regular_files(notices)
    expected["copyright"] = (notices.parent / "copyright").read_bytes()
    packages = [path for path in directory.iterdir() if path.name.endswith((".deb", ".rpm", ".apk", ".pkg.tar.zst"))]
    if len(packages) != (1 if target.endswith("musl") else 3):
        raise ValueError("The expected Linux packages were not created")
    for package in sorted(packages):
        with tempfile.TemporaryDirectory() as temporary:
            if package.suffix == ".deb":
                run("dpkg-deb", "--extract", package, temporary)
            else:
                run("bsdtar", "--no-same-owner", "--options", "read_concatenated_archives", "-xf", package, "-C", temporary)
            root = Path(temporary)
            if (root / "usr/bin/sofka").read_bytes() != binary.read_bytes():
                raise ValueError(f"The package binary differs from the release binary: {package}")
            if (root / "usr/bin/sofka").stat().st_mode & 0o777 != 0o755:
                raise ValueError(f"The package binary has wrong permissions: {package}")
            if regular_files(root / "usr/share/doc/sofka") != expected:
                raise ValueError(f"The package license files differ from the release notices: {package}")
        if package.suffix == ".deb":
            if output("dpkg-deb", "--field", package, "Architecture") != ("amd64" if target.startswith("x86_64") else "arm64"):
                raise ValueError("Wrong Debian architecture")
            if not output("dpkg-deb", "--field", package, "Depends"):
                raise ValueError("The Debian package has no dependencies")


def install_test(target):
    directory = ROOT / "target/release-assets" / target
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    alpine_install = '''
repo="/tmp/sofka-repo/$(apk --print-arch)"
mkdir -p "$repo"
for package in /packages/*.apk; do
    version=$(tar -xzOf "$package" .PKGINFO | sed -n 's/^pkgver = //p')
    test -n "$version"
    cp "$package" "$repo/sofka-$version.apk"
done
apk index --allow-untrusted -o "$repo/APKINDEX.tar.gz" "$repo"/*.apk
apk add --allow-untrusted --repository /tmp/sofka-repo sofka
'''.strip()
    tests = [
        (".deb", "ubuntu:22.04", "apt-get update -qq; apt-get install -y /packages/*.deb; dpkg --verify sofka", "apt-get install --reinstall -y /packages/*.deb", "apt-get remove -y sofka"),
        (".rpm", "fedora:latest", "dnf install -y /packages/*.rpm; rpm -V sofka", "dnf reinstall -y /packages/*.rpm", "dnf remove -y sofka"),
        (".apk", "alpine:3.23", alpine_install, "apk fix --allow-untrusted --repository /tmp/sofka-repo sofka", "apk del sofka"),
    ]
    if target.startswith("x86_64"):
        tests.append((".pkg.tar.zst", "archlinux:base", "pacman -Syu --noconfirm; pacman -U --noconfirm /packages/*.pkg.tar.zst; pacman -Qk sofka", "pacman -U --noconfirm /packages/*.pkg.tar.zst", "pacman -R --noconfirm sofka"))
    for suffix, image, install, reinstall, remove in tests:
        if not any(directory.glob("*" + suffix)):
            continue
        script = f'{install}; test "$(sofka --version)" = "sofka {version}"; {reinstall}; test "$(sofka --version)" = "sofka {version}"; {remove}; test ! -e /usr/bin/sofka'
        run("docker", "run", "--rm", "--volume", f"{directory}:/packages:ro", image, "sh", "-eu", "-c", script)
    if target.startswith("aarch64") and target.endswith("gnu"):
        print("Arch Linux ARM: payload verified; no official container is available for installation tests")


def build_packages(args):
    import yaml

    target = args.target
    stage = ROOT / "target/release-stage" / target
    if stage.exists():
        shutil.rmtree(stage)
    stage.mkdir(parents=True)
    sysroot = Path(output("rustc", "--print", "sysroot"))
    notices = stage / "notices"
    stage_notices(args.notices.resolve(), notices, sysroot / "share/doc/rust/COPYRIGHT-library.html", target)
    (stage / "copyright").write_bytes(
        b"Upstream: https://github.com/nklmilojevic/sofka\nLicense: MIT OR Apache-2.0\n\n"
        + (notices / "LICENSE-MIT").read_bytes()
        + (notices / "LICENSE-APACHE").read_bytes()
    )
    dist = ROOT / "target/release-dist" / target
    config = configure(target, stage, dist)
    if target.endswith("linux-gnu"):
        build = config["builds"][0]
        run(build["tool"], build["command"], *build["flags"], "--target", target, cwd=ROOT)
        binary = ROOT / "target" / target / "release" / build["binary"]
        config = configure(target, stage, dist, check_binary(target, binary))
    config_file = stage / "goreleaser.yaml"
    config_file.write_text(yaml.safe_dump(config, sort_keys=False))
    environment = os.environ.copy()
    environment["SOFKA_RELEASE_NOTICES"] = notices.relative_to(ROOT).as_posix()
    environment["SOFKA_RELEASE_COPYRIGHT"] = (stage / "copyright").relative_to(ROOT).as_posix()
    command = [args.goreleaser, "release", "--clean", "--config", str(config_file), "--skip=publish,announce"]
    if args.snapshot:
        command.append("--snapshot")
    run(*command, cwd=ROOT, env=environment)
    artifacts = json.loads((dist / "artifacts.json").read_text())
    binaries = [Path(artifact["path"]) for artifact in artifacts if artifact["type"] == "Binary"]
    archives = [Path(artifact["path"]) for artifact in artifacts if artifact["type"] == "Archive"]
    if len(binaries) != 1 or len(archives) != 1:
        raise ValueError("Expected one binary and one archive for the target")
    verify_archive(archives[0], binaries[0], notices)
    verify_packages(dist, target, binaries[0], notices)
    destination = ROOT / "target/release-assets" / target
    destination.mkdir(parents=True, exist_ok=True)
    if any(destination.iterdir()):
        raise ValueError(f"The asset output directory must be empty: {destination}")
    for artifact in artifacts:
        if artifact["type"] in ("Archive", "Linux Package"):
            path = Path(artifact["path"])
            shutil.copyfile(path, destination / path.name)
    print(f"Verified release assets: {destination}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("matrix")
    build = commands.add_parser("build")
    build.add_argument("--target", choices=TARGETS, required=True)
    build.add_argument("--notices", type=Path, required=True)
    build.add_argument("--goreleaser", default="goreleaser")
    build.add_argument("--snapshot", action="store_true")
    check = commands.add_parser("check-binary")
    check.add_argument("--target", choices=TARGETS, required=True)
    check.add_argument("--binary", type=Path, required=True)
    install = commands.add_parser("install-test")
    install.add_argument("--target", choices=TARGETS, required=True)
    args = parser.parse_args()
    if args.command == "matrix":
        print(json.dumps({"include": [{"target": target, "os": runner} for target, runner in TARGETS.items()]}))
    elif args.command == "check-binary":
        check_binary(args.target, args.binary)
    elif args.command == "install-test":
        install_test(args.target)
    else:
        build_packages(args)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        sys.exit(str(error))
    except (ValueError, OSError) as error:
        sys.exit(str(error))
