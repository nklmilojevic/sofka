# Release packages

Download the package for your operating system and processor from
[GitHub Releases](https://github.com/nklmilojevic/sofka/releases).
Keep all license files with the program.

| System                            | Format         | Processor       |
| --------------------------------- | -------------- | --------------- |
| Debian and Ubuntu                 | `.deb`         | amd64, arm64    |
| Fedora and compatible RPM systems | `.rpm`         | x86_64, aarch64 |
| Arch Linux and Arch Linux ARM     | `.pkg.tar.zst` | x86_64, aarch64 |
| Alpine Linux                      | `.apk`         | x86_64, aarch64 |
| Other Linux systems               | `.tar.gz`      | x86_64, aarch64 |
| macOS                             | `.tar.gz`      | x86_64, aarch64 |
| Windows                           | `.zip`         | x86_64, aarch64 |

## Linux

Download one package into an empty directory, then use the matching command:

```sh
sudo apt install ./sofka_*.deb
sudo dnf install ./sofka-*.rpm
sudo pacman -U ./sofka-*.pkg.tar.zst
sudo apk add --allow-untrusted ./sofka_*.apk
```

The APK files are unsigned local packages. Check the release checksum and
attestation before installation. Packages install `sofka` in `/usr/bin` and
license files in `/usr/share/doc/sofka`.

For an update, download the new package and run the same installation command.
For removal, use `sudo apt remove sofka`, `sudo dnf remove sofka`,
`sudo pacman -R sofka`, or `sudo apk del sofka`.
These downloads do not configure a package repository for automatic updates.

GNU libc archives retain their existing target names. The separate `linux-musl`
archives and APK files contain statically linked binaries for Alpine.
DEB dependencies are derived from the binary. RPM and Arch packages declare
the required GNU libc version, compiler runtime, and certificate bundle.
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
Windows and Alpine plugin support needs a matching change in Sofka's platform
selection and in the [plugin repository](https://github.com/nklmilojevic/sofka-plugins).
The new application archives do not imply that those adapters are available.
Plugin packages keep their existing `.tar.zst` format.

## Release development

GoReleaser OSS 2.18.1 runs Cargo on each platform runner, creates archives,
and creates Linux packages through nFPM. The shared configuration is
`.goreleaser.yaml`. `scripts/release_packages.py` selects one target for each
runner, stages notices, checks the binary, and verifies package contents.
Only verified release assets are passed to the upload job.
`SHA256SUMS` and build attestations cover all archive and package formats.

The release uses Rust 1.97.0. Its musl targets include musl 1.2.5; the matching
copyright notice is in `scripts/licenses/musl-COPYRIGHT`.
When updating the release toolchain, check the musl version in Rust's
`src/ci/docker/scripts/musl.sh` and update the notice if required.
`about.toml` includes every release target for dependency license collection.

To run the local configuration tests:

```sh
uv run --locked python -m unittest discover -s scripts -p test_release_packages.py
```

To build a local snapshot on a matching native host, install GoReleaser OSS,
Rust 1.97.0, and `uv`. Python dependencies are pinned in `uv.lock`.
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
Outputs are under `target/release-assets/<target>/`; use an empty output directory.

The packaging workflow checks every target and runs package installation,
reinstallation, and removal tests in Linux containers. Arch Linux ARM receives
payload and binary checks because no official test container is available.
Windows and macOS archives are extracted logically and compared with the inputs;
the built executable also runs with `--version` on its native runner.

Keep these four archive names and their internal file paths stable for Aqua and
mise's Aqua backend:

- `sofka-v<version>-x86_64-unknown-linux-gnu.tar.gz`
- `sofka-v<version>-aarch64-unknown-linux-gnu.tar.gz`
- `sofka-v<version>-x86_64-apple-darwin.tar.gz`
- `sofka-v<version>-aarch64-apple-darwin.tar.gz`

New package formats are additional assets. The existing checksum filename
remains `SHA256SUMS`.
