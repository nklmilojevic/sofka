# Release licenses

Each binary archive includes `LICENSE-MIT`, `LICENSE-APACHE`, and
`THIRD-PARTY-LICENSES.txt`. Keep these files with the installed program.
`THIRD-PARTY-SOURCES/` contains the source packages for dependencies distributed
under MPL 2.0. The notice file also gives exact source download URLs.

The release workflow uses cargo-about 0.9.2 and the release tag's `Cargo.lock`.
It collects licenses for the default build on all four supported targets.
It excludes dependencies used only for tests or build scripts. The Python
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
cargo install cargo-about --version 0.9.2 --locked
python3 scripts/release_licenses.py generate --output target/release-notices
python3 -m unittest discover -s scripts -p 'test_release_licenses.py'
```

The archive check compares the packaged binary with the input binary and checks
the required notice files. The upload job generates checksums and build
attestations after packaging.

For old releases, use each tag's manifest and lockfile. Repackage the existing
binaries without compiling them again. Keep the original archive hashes and
binary hashes in the correction record. Publish corrected archives with new
checksums and attestations before removing the incomplete downloads. Update
package manager checksums and URLs when their archive changes.

The `Repair release licenses` workflow accepts one release tag or `all`. It
publishes `-licenses.tar.gz` archives, `SHA256SUMS-licenses`, and a
`LICENSE-CORRECTION.json` record for each release. Its attestation records the
archive correction process. It does not claim to rebuild the old binary.
Original downloads remain available until their replacements are verified and
package managers use the corrected URLs. The separate `retire` command checks
the remote hashes before it removes each original binary archive.
