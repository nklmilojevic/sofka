# Release packages

Download the package for your operating system and processor from
[GitHub Releases](https://github.com/nklmilojevic/sofka/releases).
Keep all license files with the program.

| System                                  | Format         | Processor       |
| --------------------------------------- | -------------- | --------------- |
| Debian and Ubuntu                       | `.deb`         | amd64, arm64    |
| Fedora, openSUSE, and other RPM systems | `.rpm`         | x86_64, aarch64 |
| Arch Linux and Arch Linux ARM           | `.pkg.tar.zst` | x86_64, aarch64 |
| Alpine Linux                            | `.apk`         | x86_64, aarch64 |
| Other Linux systems                     | `.tar.gz`      | x86_64, aarch64 |
| macOS                                   | `.tar.gz`      | x86_64, aarch64 |
| Windows                                 | `.zip`         | x86_64, aarch64 |

## Package repositories

`https://pkg.sofka.rs` serves signed apt, dnf, zypper, pacman, and apk
repositories. Your normal system upgrade then updates Sofka. The repositories
keep the packages from the last five releases; Arch Linux keeps only the newest.
apt, dnf, zypper, and pacman check the GPG key in `sofka.asc`.
Its user ID is `Sofka packages <packages@sofka.rs>` and its fingerprint is
`4ADE AA25 E1DB B624 5527 9CB4 CAE2 FA8B EBAD 8047`.
apk checks the RSA key in `alpine/sofka.rsa.pub`.

Debian 12 or later and Ubuntu 22.04 or later:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://pkg.sofka.rs/sofka.asc | sudo tee /etc/apt/keyrings/sofka.asc > /dev/null
echo "deb [signed-by=/etc/apt/keyrings/sofka.asc] https://pkg.sofka.rs/deb stable main" | sudo tee /etc/apt/sources.list.d/sofka.list
sudo apt update
sudo apt install sofka
```

Fedora, RHEL, and compatible systems:

```sh
curl -fsSL https://pkg.sofka.rs/rpm/sofka.repo | sudo tee /etc/yum.repos.d/sofka.repo
sudo dnf install sofka
```

openSUSE (sofka 0.31.2 or later; earlier RPMs require Fedora's `libgcc`
package name):

```sh
sudo zypper addrepo https://pkg.sofka.rs/rpm/sofka.repo
sudo zypper install sofka
```

Arch Linux and Arch Linux ARM:

```sh
curl -fsSL https://pkg.sofka.rs/sofka.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key packages@sofka.rs
printf '[sofka]\nSigLevel = Required\nServer = https://pkg.sofka.rs/arch/$arch\n' | sudo tee -a /etc/pacman.conf
sudo pacman -Syu sofka
```

Alpine Linux, as root:

```sh
wget -qO /etc/apk/keys/sofka.rsa.pub https://pkg.sofka.rs/alpine/sofka.rsa.pub
echo https://pkg.sofka.rs/alpine >> /etc/apk/repositories
apk update
apk add sofka
```

The repository packages are the release packages. RPM files are signed for the
repository; the other formats keep their release bytes, and the signed
repository metadata covers their checksums.

## Linux

To install without a repository, download one package into an empty directory,
then use the matching command:

```sh
sudo apt install ./sofka_*.deb
sudo dnf install ./sofka-*.rpm
sudo zypper install ./sofka-*.rpm
sudo pacman -U ./sofka-*.pkg.tar.zst
sudo apk add --allow-untrusted ./sofka_*.apk
```

The APK files are unsigned local packages. Check the release checksum and
attestation before installation. Packages install `sofka` in `/usr/bin` and
license files in `/usr/share/doc/sofka`.

For an update, download the new package and run the same installation command.
For removal, use `sudo apt remove sofka`, `sudo dnf remove sofka`,
`sudo pacman -R sofka`, or `sudo apk del sofka`.
These downloads do not configure a package repository for automatic updates;
use the [package repositories](#package-repositories) for that.

GNU libc archives retain their existing target names. The separate `linux-musl`
archives and APK files contain statically linked binaries for Alpine.
Rust uses its own musl libraries at link time. `musl-gcc` compiles C dependencies;
it is not used as the final linker because its wrapper can break static PIE builds.
DEB dependencies are derived from the binary. RPM and Arch packages declare
the required GNU libc version, compiler runtime, and certificate bundle.
RPMs require the `libgcc_s.so.1` runtime by soname, which Fedora's `libgcc`
and openSUSE's `libgcc_s1` packages both provide.
The GNU builds cannot require a GLIBC symbol newer than 2.35.

## Windows

Extract the complete ZIP into a directory you own. Run `sofka.exe` from
PowerShell or Windows Terminal. Add that directory to your user `PATH` if
you want to run `sofka` from other directories.

The release uses the static Microsoft C runtime. No installation wizard or
GoReleaser Pro license is required. To update, close Sofka and replace the
extracted release files. To remove it, delete that directory and its `PATH` entry.
The executable is not code-signed.

## macOS and Nix

macOS keeps the current archives and Homebrew installation method.
Nix keeps the source-built package, flake, Home Manager module, and Cachix cache.
The release does not add a new Nix User Repository.

## Plugins

The official plugin catalog currently publishes GNU Linux and macOS adapters.
Sofka recognizes Windows catalog entries and resolves packaged `adapter.exe`
files. Windows adapters must also be published by the
[plugin repository](https://github.com/nklmilojevic/sofka-plugins).
Alpine adapters still need separate musl builds.
The new application archives do not imply that those adapters are available.
Plugin packages keep their existing `.tar.zst` format.

## Software bill of materials

Each release target has an SPDX 2.3 JSON file named
`sofka-v<version>-<target>.spdx.json`. The file records the release version,
source revision, target, Rust package names and versions, declared licenses,
and the SHA-256 checksum of the executable in the matching archive.
Package checks also confirm that Linux packages contain the same executable.

The SBOM lists Rust build inputs. Cargo compiler messages select the packages
for that target and build, including cached artifacts, build dependencies, and
procedural macros. Cargo metadata supplies package descriptions; it does not
select the package list. The document does not infer a dependency graph.

This is not an exact list of code linked into the executable. It does not list
Rust standard library components or native libraries. Build dependencies can
appear even when their code is not in the final executable. License values
come from package metadata and do not replace the supplied license notices.

The release stops if Cargo capture or SBOM generation fails, or if the packaged
executable differs from the captured build. Before upload, the workflow checks
that all eight target SBOMs exist and match their archive executable checksums.
The SBOM files are separate assets; archive contents remain unchanged.

## Release development

GoReleaser OSS 2.18.2 runs Cargo on each platform runner, creates archives,
and creates Linux packages through nFPM. The shared configuration is
`.goreleaser.yaml`. `scripts/release_packages.py` selects one target for each
runner, stages notices, checks the binary, and verifies package contents.
Only verified release assets are passed to the upload job.
Each target build runs before GoReleaser to capture Cargo build inputs.
GNU Linux builds also derive runtime dependencies.
GoReleaser then reuses Cargo's build cache. Dependency fields contain plain
values because GoReleaser does not expand templates in those fields.
`SHA256SUMS` and build attestations cover all archive, package, and SBOM files.

The release uses Rust 1.98.1. Its musl targets include musl 1.2.5; the matching
copyright notice is in `scripts/licenses/musl-COPYRIGHT`.
When updating the release toolchain, check the musl version in Rust's
`src/ci/docker/scripts/musl.sh` and update the notice if required.
`about.toml` includes every release target for dependency license collection.

To run the local configuration tests:

```sh
uv run --locked python -m unittest discover -s scripts -p 'test_release*.py'
```

To build a local snapshot on a matching native host, install GoReleaser OSS,
Rust 1.98.1, and `uv`. Python dependencies are pinned in `uv.lock`.
Collect real notices as described
in [release licenses](release-licenses.md), then run:

```sh
uv run --locked python scripts/release_packages.py build \
  --target aarch64-apple-darwin \
  --notices target/release-notices --snapshot
```

Linux hosts also need `readelf`, `dpkg-shlibdeps`, `dpkg-deb`, `bsdtar`, and
Zstandard support. Musl builds need `musl-gcc`. Windows builds need the Visual
Studio C++ build tools and CMake. The CI runners provide these tools.
Snapshots do not publish a release. Their packages have snapshot versions.
They use the next patch version with an `alpha1` suffix, which Alpine accepts.
Outputs are under `target/release-assets/<target>/`; use an empty output directory.

The packaging workflow checks every target and runs package installation,
reinstallation, and removal tests in Linux containers. Arch Linux ARM receives
payload and binary checks because no official test container is available.
Windows and macOS archives are extracted logically and compared with the inputs;
the built executable also runs with `--version` on its native runner.

After the release assets are uploaded, `scripts/package_repos.py` rebuilds the
package repositories from the Linux packages of the last five releases. Each
release must contain all eight Linux packages, which must match its `SHA256SUMS`.
The script signs the repositories in containers and serves the result locally.
The documented setup commands above then install, upgrade, and remove Sofka on
Ubuntu 22.04, Debian, Fedora, openSUSE, Arch Linux, and Alpine with signature
checks enabled, and pacman must reject a repository with an unsigned database.
Only then does rclone upload the tree to the `sofka-packages` R2 bucket behind
`pkg.sofka.rs` in three passes: packages, then the metadata that lists them,
then the entry points clients read first (`InRelease`, `repomd.xml`, the pacman
databases, `APKINDEX.tar.gz`). Metadata is served with `no-cache`, so the CDN
never pairs a new index with an old signature. apt and apk indexes carry their
signatures inline. dnf and pacman fetch an index and its detached signature
separately, so a refresh during the final pass can fail once and succeed on the
next try. `.publish-manifest.json` records when each file left the indexes,
and the file is removed seven days later.
RPM signatures use the package build time, so re-signing produces identical
bytes in each run. Pull requests run the same build and checks with throwaway
keys and do not upload.

The `packages` environment holds `PKG_GPG_PRIVATE_KEY` (armored, without a
passphrase), `PKG_APK_RSA_PRIVATE_KEY` (PEM), `R2_ACCOUNT_ID`,
`R2_ACCESS_KEY_ID`, and `R2_SECRET_ACCESS_KEY`. To rebuild the repositories
without a release, run the "Package repositories" workflow manually.
To run the repository tests:

```sh
uv run --locked python -m unittest discover -s scripts -p 'test_package_repos.py'
```

Keep these four archive names and their internal file paths stable for Aqua and
mise's Aqua backend:

- `sofka-v<version>-x86_64-unknown-linux-gnu.tar.gz`
- `sofka-v<version>-aarch64-unknown-linux-gnu.tar.gz`
- `sofka-v<version>-x86_64-apple-darwin.tar.gz`
- `sofka-v<version>-aarch64-apple-darwin.tar.gz`

New package formats are additional assets. The existing checksum filename
remains `SHA256SUMS`.
