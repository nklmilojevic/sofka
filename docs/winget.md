# WinGet setup

This change implements the scope agreed in
[discussion #700](https://github.com/nklmilojevic/sofka/discussions/700).
The package ID is `nklmilojevic.sofka`.

## First submission

[WinGet Releaser](https://github.com/vedantmgoyal9/winget-releaser) needs an
accepted package version before it can submit updates. The first manifests
are in `packaging/winget/manifests/n/nklmilojevic/sofka/0.29.3/`.
They use the published x64 and ARM64 ZIP files. Their SHA-256 values match
the release `SHA256SUMS` file. Each ZIP contains `sofka.exe` at its root.

1. Fork `microsoft/winget-pkgs` into the `nklmilojevic` account.
2. On Windows, validate the supplied manifests:

   ```powershell
   winget validate --manifest packaging/winget/manifests/n/nklmilojevic/sofka/0.29.3
   ```

3. Enable local manifests from an administrator terminal, then test from
   a normal terminal:

   ```powershell
   winget settings --enable LocalManifestFiles
   winget install --manifest packaging/winget/manifests/n/nklmilojevic/sofka/0.29.3
   sofka --version
   winget uninstall --exact --id nklmilojevic.sofka
   ```

   Test on x64 and ARM64 Windows when both are available. Check that license
   notices remain in the installed package directory. Restore the local
   manifest setting after testing if it was disabled before the test.

4. Copy the supplied `manifests/n/nklmilojevic/sofka/0.29.3/` directory into
   the same path in the fork. Open a pull request to `microsoft/winget-pkgs`.
   Follow its checks and wait for acceptance.

These files record the first submission only. Later manifests are generated
by WinGet Releaser. If the accepted package ID changes, update the workflow
and installation commands before enabling automation.

## Enable release updates

1. Create a classic GitHub personal access token for the account that owns
   the fork. WinGet Releaser requires `public_repo`. Its `workflow` scope
   lets it sync upstream workflow changes in the fork. Without `workflow`,
   maintainers must sync those changes manually. Fine-grained tokens are
   not supported by the action.
2. Store the token as the Sofka repository Actions secret `WINGET_TOKEN`.
   Do not put it in a file or a discussion.
3. After the first manifest is accepted and the workflows are on `main`,
   set the repository Actions variable `WINGET_ENABLED` to `true`.

The release workflow starts `Release WinGet` only after the assets and
checksums are uploaded. It does not submit prereleases. The separate workflow
checks the selected release, both Windows ZIP files, and their checksums
before it submits an update. WinGet review and indexing can delay availability.

A failed dispatch does not fail the release. A failed submission appears in
the separate `Release WinGet` run. Check that run after each release.
Set `WINGET_ENABLED` to `false` to stop automatic dispatches.

## Retry an update

Check for an existing pull request for the same version before retrying.
Only one pull request per package version is permitted. Resolve or close an
existing submission before starting another.

Run the workflow from `main` with the published release tag:

```sh
gh workflow run release-winget.yaml --ref main -f tag=v0.29.3
```

Replace the example tag with the version to submit. Manual runs do not
require `WINGET_ENABLED`, but they still require the accepted package and
`WINGET_TOKEN`. They do not build assets or publish a Sofka release.

See Microsoft's [manifest guide](https://learn.microsoft.com/en-us/windows/package-manager/package/manifest)
for the package submission rules.
