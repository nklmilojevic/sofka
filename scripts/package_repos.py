"""Build, verify, and publish the signed Linux package repositories."""

import argparse
import functools
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
REPOSITORY = "nklmilojevic/sofka"
BASE_URL = "https://pkg.sofka.rs"
BUCKET = "r2:sofka-packages"
KEY_UID = "Sofka packages <packages@sofka.rs>"
RELEASES = 5
PATTERNS = ("*.deb", "*.rpm", "*.apk", "*.pkg.tar.zst")
ARCHITECTURES = {"deb": ("amd64", "arm64"), "rpm": ("x86_64", "aarch64"),
                 "arch": ("x86_64", "aarch64"), "alpine": ("x86_64", "aarch64")}
NAMES = {
    "deb": re.compile(r"sofka_(?P<version>[0-9.]+)_(?P<arch>amd64|arm64)\.deb"),
    "rpm": re.compile(r"sofka-(?P<version>[0-9.]+)-\d+\.(?P<arch>x86_64|aarch64)\.rpm"),
    "arch": re.compile(r"sofka-(?P<version>[0-9.]+)-\d+-(?P<arch>x86_64|aarch64)\.pkg\.tar\.zst"),
    "alpine": re.compile(r"sofka_(?P<version>[0-9.]+)_(?P<arch>x86_64|aarch64)\.apk"),
}
PACKAGE_FILES = ("*.deb", "*.rpm", "*.apk", "*.pkg.tar.zst", "*.pkg.tar.zst.sig")
# Files clients fetch first; they reference everything else, so they upload last.
ENTRY_POINTS = ("/deb/dists/*/InRelease",
                "/rpm/*/repodata/repomd.xml", "/rpm/*/repodata/repomd.xml.asc",
                "/arch/*/sofka.*", "/alpine/*/APKINDEX.tar.gz")
MANIFEST = ".publish-manifest.json"
# How long a file stays after it leaves the indexes, so clients holding older
# indexes can still fetch what they reference.
RETENTION = 7 * 24 * 3600
PACKAGE_CACHE = "Cache-Control: public, max-age=86400"
# Uncached, so the CDN never pairs a new index with an old signature.
METADATA_CACHE = "Cache-Control: no-cache"

RPM_REPO = """[sofka]
name=sofka
baseurl={base}/rpm/$basearch
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey={base}/sofka.asc
"""

# The documentation shows these snippets verbatim; `verify` runs them.
SETUP = {
    "apt": """sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL {base}/sofka.asc | sudo tee /etc/apt/keyrings/sofka.asc > /dev/null
echo "deb [signed-by=/etc/apt/keyrings/sofka.asc] {base}/deb stable main" | sudo tee /etc/apt/sources.list.d/sofka.list
sudo apt update
sudo apt install sofka""",
    "dnf": """curl -fsSL {base}/rpm/sofka.repo | sudo tee /etc/yum.repos.d/sofka.repo
sudo dnf install sofka""",
    "zypper": """sudo zypper addrepo {base}/rpm/sofka.repo
sudo zypper install sofka""",
    "pacman": """curl -fsSL {base}/sofka.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key packages@sofka.rs
printf '[sofka]\\nSigLevel = Required\\nServer = {base}/arch/$arch\\n' | sudo tee -a /etc/pacman.conf
sudo pacman -Syu sofka""",
    "apk": """wget -qO /etc/apk/keys/sofka.rsa.pub {base}/alpine/sofka.rsa.pub
echo {base}/alpine >> /etc/apk/repositories
apk update
apk add sofka""",
}

# image, snippet, preparation, install an older version, upgrade, remove
VERIFY = (
    ("ubuntu:22.04", "apt",
     "apt-get update -qq; apt-get install -y -qq curl ca-certificates > /dev/null; "
     "echo 'APT::Get::Assume-Yes \"true\";' > /etc/apt/apt.conf.d/90sofka",
     "apt-get install -y sofka={old}", "apt-get install -y --only-upgrade sofka", "apt-get remove -y sofka"),
    ("debian:stable", "apt",
     "apt-get update -qq; apt-get install -y -qq curl ca-certificates > /dev/null; "
     "echo 'APT::Get::Assume-Yes \"true\";' > /etc/apt/apt.conf.d/90sofka",
     "apt-get install -y sofka={old}", "apt-get install -y --only-upgrade sofka", "apt-get remove -y sofka"),
    ("fedora:latest", "dnf", "echo assumeyes=True >> /etc/dnf/dnf.conf",
     "dnf install -y sofka-{old}", "dnf upgrade -y sofka", "dnf remove -y sofka"),
    ("opensuse/tumbleweed", "zypper",
     'zypper() { command zypper --non-interactive --gpg-auto-import-keys "$@"; }',
     None, None, "zypper remove sofka"),
    ("archlinux:base", "pacman",
     'pacman-key --init > /dev/null 2>&1; pacman-key --populate archlinux > /dev/null 2>&1; '
     'pacman() { command pacman --noconfirm --disable-sandbox "$@"; }',
     None, None, "pacman -R sofka"),
    ("alpine:3.23", "apk", "",
     "apk add sofka={old}", "apk add sofka; apk upgrade sofka", "apk del sofka"),
)

GPG_IMPORT = """
export GNUPGHOME="$(mktemp -d)"
gpg --batch --quiet --import /keys/gpg.asc
fpr="$(gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr:/ { print $10; exit }')"
created="$(gpg --batch --with-colons --list-secret-keys | awk -F: '/^sec:/ { print $6; exit }')"
test -n "$fpr" && test -n "$created"
"""

CONTAINERS = {
    "deb": ("debian:stable", """
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq apt-utils gnupg > /dev/null
""" + GPG_IMPORT + """
cd /repo/deb
for arch in amd64 arm64; do
    mkdir -p "dists/stable/main/binary-$arch"
    apt-ftparchive --arch "$arch" packages pool > "dists/stable/main/binary-$arch/Packages"
    gzip -9nkf "dists/stable/main/binary-$arch/Packages"
done
apt-ftparchive \\
    -o APT::FTPArchive::Release::Origin=sofka \\
    -o APT::FTPArchive::Release::Label=sofka \\
    -o APT::FTPArchive::Release::Suite=stable \\
    -o APT::FTPArchive::Release::Codename=stable \\
    -o "APT::FTPArchive::Release::Architectures=amd64 arm64" \\
    -o APT::FTPArchive::Release::Components=main \\
    release dists/stable > /tmp/Release
# Only the inline-signed InRelease: a separate Release and Release.gpg could be
# fetched from different publishes.
gpg --batch --yes --local-user "$fpr" --clearsign -o dists/stable/InRelease /tmp/Release
gpg --batch --armor --export "$fpr" > /repo/sofka.asc
"""),
    # A fixed signature time keeps re-signed packages byte-identical between runs,
    # so cached copies always match the checksums in the new metadata. A signature
    # cannot predate its key, so packages built earlier use the key creation time.
    "rpm": ("fedora:latest", """
dnf install -y -q createrepo_c rpm-sign gnupg2 > /dev/null
""" + GPG_IMPORT + """
for arch in x86_64 aarch64; do
    dir="/repo/rpm/$arch"
    test -d "$dir" || continue
    for package in "$dir"/*.rpm; do
        time="$(rpm -qp --qf '%{BUILDTIME}' "$package")"
        if [ "$time" -lt "$created" ]; then time="$created"; fi
        cp "$package" /tmp/check.rpm
        for file in "$package" /tmp/check.rpm; do
            rpmsign --addsign --define "_gpg_name $fpr" \\
                --define "_gpg_sign_cmd_extra_args --faked-system-time ${time}!" "$file" > /dev/null
        done
        test "$(sha256sum < "$package")" = "$(sha256sum < /tmp/check.rpm)"
    done
    createrepo_c --quiet --general-compress-type=gz "$dir"
    gpg --batch --yes --local-user "$fpr" --armor --detach-sign "$dir/repodata/repomd.xml"
done
"""),
    "arch": ("archlinux:base", GPG_IMPORT + """
for arch in x86_64 aarch64; do
    dir="/repo/arch/$arch"
    test -d "$dir" || continue
    cd "$dir"
    for package in *.pkg.tar.zst; do
        gpg --batch --yes --local-user "$fpr" --detach-sign "$package"
    done
    repo-add --quiet --sign --key "$fpr" --include-sigs sofka.db.tar.zst *.pkg.tar.zst
done
"""),
    "alpine": ("alpine:3.23", """
apk add -q abuild openssl
openssl rsa -in /keys/sofka.rsa -pubout -out /repo/alpine/sofka.rsa.pub 2> /dev/null
for arch in x86_64 aarch64; do
    dir="/repo/alpine/$arch"
    test -d "$dir" || continue
    cd "$dir"
    apk index --quiet --allow-untrusted -o APKINDEX.tar.gz *.apk
    abuild-sign -q -k /keys/sofka.rsa -p sofka.rsa.pub APKINDEX.tar.gz
done
"""),
}

KEYGEN = """
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq gnupg openssl > /dev/null
export GNUPGHOME="$(mktemp -d)"
gpg --batch --pinentry-mode loopback --passphrase '' --quick-gen-key "$KEY_UID" rsa4096 sign never
gpg --batch --armor --export-secret-keys "$KEY_UID" > /keys/gpg.asc
openssl genrsa -out /keys/sofka.rsa 4096 2> /dev/null
chown "$HOST_UID:$HOST_GID" /keys/gpg.asc /keys/sofka.rsa
"""


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output(*args):
    return run(*args, capture_output=True, text=True).stdout.strip()


def version_key(version):
    return tuple(int(part) for part in version.split("."))


def classify(name):
    for family, pattern in NAMES.items():
        if match := pattern.fullmatch(name):
            return family, match["arch"], match["version"]
    raise ValueError(f"Unknown package file: {name}")


def container(image, script, *volumes, network=False):
    command = ["docker", "run", "--rm", "--env", f"HOST_UID={os.getuid()}", "--env", f"HOST_GID={os.getgid()}"]
    for source, target, mode in volumes:
        command += ["--volume", f"{Path(source).resolve()}:{target}:{mode}"]
    if network:
        command += ["--network", "host"]
    if image.startswith("archlinux"):
        # Arch Linux publishes no ARM image; arm64 hosts run it emulated.
        command += ["--platform", "linux/amd64"]
    run(*command, image, "sh", "-eu", "-c", script)


def verify_checksums(directory):
    sums = {}
    for line in (directory / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split(maxsplit=1)
        sums[name.lstrip("*")] = digest
    for path in sorted(directory.iterdir()):
        if path.name == "SHA256SUMS":
            continue
        if sums.get(path.name) != hashlib.sha256(path.read_bytes()).hexdigest():
            raise ValueError(f"Release asset does not match SHA256SUMS: {path}")


def download(out, releases):
    tags = output("gh", "release", "list", "--repo", REPOSITORY, "--exclude-drafts",
                  "--exclude-pre-releases", "--limit", releases, "--json", "tagName", "--jq", ".[].tagName").split()
    if not tags:
        raise ValueError("No published releases found")
    out.mkdir(parents=True, exist_ok=False)
    for tag in tags:
        directory = out / tag
        directory.mkdir()
        patterns = [argument for pattern in PATTERNS + ("SHA256SUMS",) for argument in ("--pattern", pattern)]
        run("gh", "release", "download", tag, "--repo", REPOSITORY, "--dir", directory, *patterns)
        verify_checksums(directory)
    print(f"Downloaded Linux packages from {', '.join(tags)}")


def release_packages(release):
    """Classify one release's packages; every format and architecture must be present once."""
    packages = {}
    for path in sorted(release.iterdir()):
        if path.is_file() and path.name != "SHA256SUMS":
            family, arch, version = classify(path.name)
            if (family, arch) in packages:
                raise ValueError(f"{release.name} has more than one {family} package for {arch}")
            packages[family, arch] = version, path
    expected = {(family, arch) for family, arches in ARCHITECTURES.items() for arch in arches}
    if missing := sorted(expected - packages.keys()):
        raise ValueError(f"{release.name} is missing packages: {missing}")
    if len({version for version, _ in packages.values()}) != 1:
        raise ValueError(f"{release.name} contains packages of different versions")
    return packages


def layout(packages, tree):
    """Copy packages into the repository tree; return the versions per family."""
    found = {}
    releases = sorted({path.parent for path in packages.rglob("*") if path.is_file()})
    if not releases:
        raise ValueError(f"No packages found in {packages}")
    for release in releases:
        for (family, arch), (version, path) in release_packages(release).items():
            if version in found.setdefault((family, arch), {}):
                raise ValueError(f"Version {version} appears in more than one release")
            found[family, arch][version] = path
    versions = {}
    for (family, arch), by_version in sorted(found.items()):
        ordered = sorted(by_version, key=version_key)
        if family == "arch":
            # pacman only resolves the newest package; older files would be orphans.
            ordered = ordered[-1:]
        directory = {"deb": tree / "deb/pool/main/s/sofka"}.get(family, tree / family / arch)
        directory.mkdir(parents=True, exist_ok=True)
        for version in ordered:
            source = by_version[version]
            name = f"sofka-{version}.apk" if family == "alpine" else source.name
            shutil.copyfile(source, directory / name)
        versions.setdefault(family, set()).update(ordered)
    return {family: sorted(available, key=version_key) for family, available in versions.items()}


def materialize_links(tree):
    """repo-add creates symlinks; object storage needs real files."""
    for path in sorted(tree.rglob("*")):
        if path.is_symlink():
            target = path.resolve()
            path.unlink()
            shutil.copyfile(target, path)


def check_tree(tree):
    required = ["sofka.asc", "rpm/sofka.repo", "alpine/sofka.rsa.pub",
                "deb/dists/stable/InRelease"]
    for arch in ARCHITECTURES["deb"]:
        packages = sorted((tree / "deb/pool/main/s/sofka").glob(f"*_{arch}.deb"))
        index = (tree / f"deb/dists/stable/main/binary-{arch}/Packages").read_text()
        if len(re.findall(r"^Filename: ", index, re.MULTILINE)) != len(packages):
            raise ValueError(f"The {arch} Packages index does not list every package")
        required.append(f"deb/dists/stable/main/binary-{arch}/Packages.gz")
    for arch in ARCHITECTURES["rpm"]:
        if any((tree / "rpm" / arch).glob("*.rpm")):
            required += [f"rpm/{arch}/repodata/repomd.xml", f"rpm/{arch}/repodata/repomd.xml.asc"]
    for arch in ARCHITECTURES["arch"]:
        for package in (tree / "arch" / arch).glob("*.pkg.tar.zst"):
            required.append(package.relative_to(tree).as_posix() + ".sig")
        if any((tree / "arch" / arch).glob("*.pkg.tar.zst")):
            required += [f"arch/{arch}/sofka.{name}" for name in ("db", "db.sig", "files", "files.sig")]
    for arch in ARCHITECTURES["alpine"]:
        if any((tree / "alpine" / arch).glob("*.apk")):
            required.append(f"alpine/{arch}/APKINDEX.tar.gz")
    missing = [name for name in required if not (tree / name).is_file()]
    if missing:
        raise ValueError(f"Repository files are missing: {missing}")
    links = [path for path in tree.rglob("*") if path.is_symlink()]
    if links:
        raise ValueError(f"Repository contains symlinks: {links}")


def build(packages, keys, tree):
    if tree.exists() and any(tree.iterdir()):
        raise ValueError(f"The repository output directory must be empty: {tree}")
    tree.mkdir(parents=True, exist_ok=True)
    versions = layout(packages, tree)
    (tree / "rpm").mkdir(exist_ok=True)
    (tree / "rpm/sofka.repo").write_text(RPM_REPO.format(base=BASE_URL))
    (tree / "alpine").mkdir(exist_ok=True)
    for family, (image, script) in CONTAINERS.items():
        script += '\nchown -R "$HOST_UID:$HOST_GID" /repo\n'
        container(image, script, (tree, "/repo", "rw"), (keys, "/keys", "ro"))
    materialize_links(tree)
    check_tree(tree)
    (tree.parent / (tree.name + ".versions.json")).write_text(json.dumps(versions))
    print(f"Built package repositories: {tree}")


def opensuse_guard(tree, version, base):
    """Skip zypper while the newest RPM still requires Fedora's libgcc package name."""
    name = next((tree / "rpm/x86_64").glob(f"sofka-{version}-*.x86_64.rpm")).name.replace("x86_64", "$(uname -m)")
    return (f'curl -fsSLo /tmp/sofka.rpm "{base}/rpm/$(uname -m)/{name}"\n'
            "if rpm -qp --requires /tmp/sofka.rpm | grep -qx libgcc; then\n"
            f'    echo "Skipping zypper: sofka {version} requires libgcc, which openSUSE does not provide"\n'
            "    exit 0\n"
            "fi")


def unsigned_database_check(prepare, base):
    """The documented pacman setup must refuse a repository whose database is unsigned."""
    return "\n".join([
        'sudo() { "$@"; }', prepare,
        "setup() {", SETUP["pacman"].format(base=base), "}",
        "if setup; then",
        '    echo "pacman accepted an unsigned database" >&2',
        "    exit 1",
        "fi",
        "test ! -e /usr/bin/sofka",
    ])


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def verify(tree):
    versions = json.loads((tree.parent / (tree.name + ".versions.json")).read_text())
    with tempfile.TemporaryDirectory() as temporary:
        served = Path(temporary) / "repo"
        shutil.copytree(tree, served)
        unsigned = Path(temporary) / "unsigned"
        shutil.copytree(tree / "arch", unsigned / "arch")
        shutil.copyfile(tree / "sofka.asc", unsigned / "sofka.asc")
        for signature in unsigned.glob("arch/*/sofka.db.sig"):
            signature.unlink()
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(QuietHandler, directory=temporary))
        root = f"http://127.0.0.1:{server.server_address[1]}"
        base = root + "/repo"
        (served / "rpm/sofka.repo").write_text(RPM_REPO.format(base=base))
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            for image, snippet, prepare, older, upgrade, remove in VERIFY:
                family = {"apt": "deb", "dnf": "rpm", "zypper": "rpm", "pacman": "arch", "apk": "alpine"}[snippet]
                latest = versions[family][-1]
                check = f'test "$(sofka --version)" = "sofka {latest}"'
                steps = ['sudo() { "$@"; }', prepare]
                if snippet == "zypper":
                    steps.append(opensuse_guard(tree, latest, base))
                steps += [SETUP[snippet].format(base=base), check, remove]
                older_versions = versions[family][:-1]
                if older and older_versions:
                    steps += [older.format(old=older_versions[-1]),
                              f'test "$(sofka --version)" = "sofka {older_versions[-1]}"', upgrade, check, remove]
                steps.append("test ! -e /usr/bin/sofka")
                print(f"Verifying {snippet} on {image}", flush=True)
                container(image, "\n".join(step for step in steps if step), network=True)
                if snippet == "pacman":
                    print(f"Verifying that pacman rejects an unsigned database on {image}", flush=True)
                    container(image, unsigned_database_check(prepare, root + "/unsigned"), network=True)
        finally:
            server.shutdown()
    print(f"Verified package repositories for sofka {versions['deb'][-1]}")


def upload(tree, bucket, now=None):
    now = int(time.time()) if now is None else now
    current = {path.relative_to(tree).as_posix() for path in tree.rglob("*") if path.is_file()}
    published = set(output("rclone", "lsf", "--recursive", "--files-only", bucket).splitlines())
    # The manifest maps each file the indexes no longer reference to when it left
    # them, and current files to null. Files it does not know leave from now on.
    recorded = json.loads(output("rclone", "cat", f"{bucket}/{MANIFEST}")) if MANIFEST in published else {}
    removed = {name: recorded.get(name) or now for name in published - current - {MANIFEST}}
    packages = [*("- " + pattern for pattern in ENTRY_POINTS), *("+ " + pattern for pattern in PACKAGE_FILES), "- *"]
    referenced = [*("- " + pattern for pattern in ENTRY_POINTS + PACKAGE_FILES), "+ *"]
    entry_points = [*("+ " + pattern for pattern in ENTRY_POINTS), "- *"]
    # Packages, then the metadata they appear in, then the entry points that
    # reference that metadata, so no client sees a link to a missing file.
    for rules, cache in ((packages, PACKAGE_CACHE), (referenced, METADATA_CACHE), (entry_points, METADATA_CACHE)):
        filters = [argument for rule in rules for argument in ("--filter", rule)]
        run("rclone", "copy", tree, bucket, "--checksum", *filters, "--header-upload", cache)
    stale = sorted(name for name, left in removed.items() if now - left > RETENTION)
    if stale:
        with tempfile.NamedTemporaryFile("w", suffix=".txt") as listing:
            listing.write("".join(name + "\n" for name in stale))
            listing.flush()
            run("rclone", "delete", bucket, "--files-from-raw", listing.name)
    manifest = dict.fromkeys(current) | {name: left for name, left in removed.items() if name not in stale}
    run("rclone", "rcat", f"{bucket}/{MANIFEST}", input=json.dumps(manifest, indent=1, sort_keys=True) + "\n", text=True)
    print(f"Uploaded package repositories to {bucket}; removed {len(stale)} files")


def keygen(out):
    out.mkdir(parents=True, exist_ok=False)
    container("debian:stable", f"KEY_UID='{KEY_UID}'\n" + KEYGEN, (out, "/keys", "rw"))
    print(f"Generated signing keys in {out}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("download")
    command.add_argument("--out", type=Path, required=True)
    command.add_argument("--releases", type=int, default=RELEASES)
    command = commands.add_parser("build")
    command.add_argument("--packages", type=Path, required=True)
    command.add_argument("--keys", type=Path, required=True)
    command.add_argument("--out", type=Path, required=True)
    command = commands.add_parser("verify")
    command.add_argument("--tree", type=Path, required=True)
    command = commands.add_parser("upload")
    command.add_argument("--tree", type=Path, required=True)
    command.add_argument("--bucket", default=BUCKET)
    command = commands.add_parser("keygen")
    command.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "download":
        download(args.out, args.releases)
    elif args.command == "build":
        build(args.packages, args.keys, args.out)
    elif args.command == "verify":
        verify(args.tree)
    elif args.command == "upload":
        upload(args.tree, args.bucket)
    else:
        keygen(args.out)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        sys.exit(str(error))
    except (ValueError, OSError) as error:
        sys.exit(str(error))
