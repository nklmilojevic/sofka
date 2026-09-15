# Release licenses

Each binary archive includes `LICENSE-MIT`, `LICENSE-APACHE`, and
`THIRD-PARTY-LICENSES.txt`, and `RUST-LICENSES.html`. Keep these files with the
installed program.
`THIRD-PARTY-SOURCES/` contains the source packages for dependencies distributed
under MPL 2.0. The notice file also gives exact source download URLs.

The release workflow uses cargo-about 0.9.2 and the release tag's `Cargo.lock`.
It collects licenses for the default build on all eight release targets.
It excludes dependencies used only for tests or build scripts. The Rust
collector adds license and notice files from bundled native source trees.
Cargo downloads the locked sources first. License analysis then runs offline;
source archives and omitted notices use explicit source URLs.
If a crate omits its license files, the collector reads them from the GitHub
commit recorded in that crate's `.cargo_vcs_info.json`.
Kube 4.0.0 records an unavailable commit. Its five crates use the pinned 4.0.0
release commit instead. All 86 Rust source files were checked against that tag.

The accepted licenses are in `about.toml`. A dependency with an unaccepted or
unknown license stops the release. Review its terms before changing this list.
MPL source archives must match their checksums in `Cargo.lock`.

To check the release notices locally:

```sh
cargo install cargo-about --version 0.9.2 --features cli --locked
cargo run --locked --example release-licenses -- generate --output target/release-notices
cargo test --locked --example release-licenses
```

Use an empty output directory. The release tools use Rust, cargo-about, and curl.
The GoReleaser packaging step uses Python through `uv` with dependencies pinned
in `uv.lock`. Downloads retry connection errors, including TLS errors,
and temporary server errors up to three times. A failed download reports its URL
and the curl error. Only successful responses and HTTP 404 results are cached.

The archive check compares the packaged binary with the input binary and checks
the required notice files. The upload job generates checksums and build
attestations after packaging.

Keep future release asset names in the form `sofka-v<version>-<target>.tar.gz`,
with checksums in `SHA256SUMS`. Aqua and mise's Aqua backend use these names.
License files go inside the archive and do not change its filename.

For old releases, use each tag's manifest and lockfile. Repackage the existing
binaries without compiling them again. Keep the original archive hashes and
binary hashes in the correction record. Publish corrected archives with new
checksums and attestations before removing the incomplete downloads. Keep the
standard download names available with corrected bytes and current checksums.

The one-time repair covered all 90 releases from v0.1.0 through v0.27.2. It
published `-licenses.tar.gz` archives, `SHA256SUMS-licenses`, and a
`LICENSE-CORRECTION.json` record for each release. Its attestation records the
archive correction process. It does not claim to rebuild the old binary.
The repair identified the Rust compiler commit in each binary and obtained the
standard library notices from the matching, checksum-verified Rust distribution.
The historical v0.13.4 Linux archives keep their musl target names and include
the musl 1.2.5 copyright file from the source selected by that tag's Nix lockfile.
The incomplete archives were removed. The standard archive names were then
restored with the same bytes as the corrected archives, with updated
`SHA256SUMS` files. Aqua and other package managers need these standard names.
Lockfiles that pin the old archive hashes need updated checksums. The correction
records retain the hashes of the incomplete archives for reference.
After the standard downloads and Homebrew URLs were verified, the temporary
`-licenses.tar.gz` archives and `SHA256SUMS-licenses` files were removed. The
corrected archive hashes in the records apply to the standard download names.

The completed repair workflow and Python repair scripts were removed. Their Git
history, run history, and attestations remain available for reference. The old
`restore-names` command depends on temporary correction assets that were removed.
Do not run the old `retire` command against the restored downloads.
