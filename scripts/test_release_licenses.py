import hashlib
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import release_licenses
import repair_release_licenses


class ReleaseLicensesTests(unittest.TestCase):
    def test_archive_preserves_binary_and_includes_notices_and_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "binary"
            binary.write_bytes(b"\x7fELF\x00unchanged binary")
            notices = root / "notices"
            notices.mkdir()
            for name in release_licenses.REQUIRED:
                (notices / name).write_text(name)
            sources = notices / "THIRD-PARTY-SOURCES"
            sources.mkdir()
            (sources / "matcher.crate").write_bytes(b"source archive")
            runtime = root / "rust.html"
            runtime.write_text("Rust library notices")
            output = root / "release.tar.gz"
            release_licenses.package(binary, notices, output, runtime)
            second = root / "second.tar.gz"
            release_licenses.package(binary, notices, second, runtime)
            self.assertEqual(output.read_bytes(), second.read_bytes())
            with tarfile.open(output) as archive:
                self.assertEqual(
                    archive.extractfile("sofka").read(), binary.read_bytes()
                )
                self.assertEqual(archive.getmember("sofka").mode, 0o755)
                self.assertEqual(
                    archive.extractfile("RUST-LICENSES.html").read(),
                    runtime.read_bytes(),
                )
                for name in release_licenses.REQUIRED:
                    self.assertEqual(archive.extractfile(name).read(), name.encode())
                self.assertEqual(
                    archive.extractfile("THIRD-PARTY-SOURCES/matcher.crate").read(),
                    b"source archive",
                )

    def test_packaging_rejects_missing_or_empty_notices(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in release_licenses.REQUIRED:
                (root / name).write_text("notice")
            for name in release_licenses.REQUIRED:
                with self.subTest(name=name):
                    (root / name).write_text("")
                    with self.assertRaisesRegex(ValueError, "Missing or empty"):
                        release_licenses.validate_notices(root)
                    (root / name).unlink()
                    with self.assertRaisesRegex(ValueError, "Missing or empty"):
                        release_licenses.validate_notices(root)
                    (root / name).write_text("notice")

    def test_collects_nested_native_notices_and_declared_license_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            native = root / "native"
            native.mkdir()
            (native / "NOTICE.txt").write_text("native attribution")
            (native / "COPYING").write_text("native license")
            (root / "terms.txt").write_text("declared license")
            inherited = root / ".licenses"
            inherited.mkdir()
            (inherited / "Author-MIT").write_text("inherited copyright and license")
            (root / "main.rs").write_text("source")
            files = dict(
                release_licenses.license_files(
                    {
                        "manifest_path": str(root / "Cargo.toml"),
                        "license_file": "terms.txt",
                    }
                )
            )
            self.assertEqual(
                files,
                {
                    ".licenses/Author-MIT": "inherited copyright and license",
                    "native/NOTICE.txt": "native attribution",
                    "native/COPYING": "native license",
                    "terms.txt": "declared license",
                },
            )

    def test_repair_rejects_symlinks_and_duplicate_binary_entries(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "invalid.tar.gz"
            with tarfile.open(path, "w:gz") as archive:
                entry = tarfile.TarInfo("sofka")
                entry.type = tarfile.SYMTYPE
                entry.linkname = "/etc/passwd"
                archive.addfile(entry)
            with self.assertRaisesRegex(ValueError, "one regular sofka"):
                repair_release_licenses.binary_from_archive(path)
            with tarfile.open(path, "w:gz") as archive:
                archive.addfile(tarfile.TarInfo("sofka"))
                archive.addfile(tarfile.TarInfo("./sofka"))
            with self.assertRaisesRegex(ValueError, "one regular sofka"):
                repair_release_licenses.binary_from_archive(path)

    def test_retire_does_not_delete_without_matching_replacements(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            release = root / "v1.0.0"
            release.mkdir()
            record = {
                "tag": "v1.0.0",
                "assets": [
                    {
                        "original_name": "original.tar.gz",
                        "original_sha256": "old",
                        "corrected_name": "corrected.tar.gz",
                        "corrected_sha256": "new",
                    }
                ],
            }
            (release / "LICENSE-CORRECTION.json").write_text(json.dumps(record))
            with patch.object(
                repair_release_licenses, "gh", return_value='{"assets": []}'
            ) as github:
                with self.assertRaisesRegex(ValueError, "Missing verified replacement"):
                    repair_release_licenses.retire(root)
                self.assertEqual(github.call_count, 1)
                self.assertEqual(github.call_args.args[0], "api")

    def test_correction_lifecycle_checks_hashes_before_publication_and_removal(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "v1.0.0"
            original = destination / "original"
            original.mkdir(parents=True)
            binary = b"original binary bytes"
            assets = []
            for target in repair_release_licenses.TARGETS:
                path = original / f"sofka-v1.0.0-{target}.tar.gz"
                with tarfile.open(path, "w:gz") as archive:
                    entry = tarfile.TarInfo("sofka")
                    entry.size = len(binary)
                    archive.addfile(entry, io.BytesIO(binary))
                assets.append(
                    {
                        "name": path.name,
                        "digest": "sha256:" + repair_release_licenses.digest(path),
                    }
                )
            release = {"tag_name": "v1.0.0", "assets": assets}
            source = io.BytesIO()
            with tarfile.open(fileobj=source, mode="w"):
                pass

            def generate(_manifest, notices, _collector, _cache, _targets=None):
                notices.mkdir(exist_ok=True)
                for name in release_licenses.REQUIRED:
                    (notices / name).write_text(name)

            def git(args, **_kwargs):
                return "0" * 40 if args[1] == "rev-parse" else source.getvalue()

            with (
                patch.object(release_licenses, "generate", side_effect=generate),
                patch.object(
                    repair_release_licenses.subprocess, "check_output", side_effect=git
                ),
                patch.object(repair_release_licenses, "rust_notice", return_value=None),
            ):
                good_digest = assets[0]["digest"]
                assets[0]["digest"] = "sha256:wrong"
                with self.assertRaisesRegex(
                    ValueError, "GitHub asset checksum mismatch"
                ):
                    repair_release_licenses.prepare(release, root, "cargo-about")
                assets[0]["digest"] = good_digest
                repair_release_licenses.prepare(release, root, "cargo-about")

            record = json.loads((destination / "LICENSE-CORRECTION.json").read_text())
            for asset in record["assets"]:
                self.assertEqual(
                    asset["binary_sha256"], hashlib.sha256(binary).hexdigest()
                )
            remote = {a["name"]: dict(a) for a in assets}
            remote["SHA256SUMS"] = {
                "name": "SHA256SUMS",
                "digest": "sha256:checksum-file",
            }
            deletions = []
            uploads = []
            body = "Original release notes"

            def github(*args):
                nonlocal body
                if args[0] == "api":
                    if "--input" in args:
                        body = json.loads(Path(args[-1]).read_text())["body"]
                    return json.dumps(
                        {"id": 123, "assets": list(remote.values()), "body": body}
                    )
                if args[1] == "upload":
                    uploads.append(args)
                    for name in args[5:]:
                        path = Path(name)
                        remote[path.name] = {
                            "name": path.name,
                            "digest": "sha256:" + repair_release_licenses.digest(path),
                        }
                elif args[1] == "edit":
                    body = Path(args[-1]).read_text()
                elif args[1] == "download":
                    return "".join(
                        f"{a['original_sha256']}  {a['original_name']}\n"
                        for a in record["assets"]
                    )
                elif args[1] == "delete-asset":
                    deletions.append(args[3])
                    del remote[args[3]]
                else:
                    self.fail(f"Unexpected GitHub operation: {args}")
                return ""

            with patch.object(repair_release_licenses, "gh", side_effect=github):
                changed = destination / record["assets"][0]["corrected_name"]
                saved = changed.read_bytes()
                changed.write_bytes(b"changed archive")
                with self.assertRaisesRegex(ValueError, "Changed correction archive"):
                    repair_release_licenses.publish(root)
                self.assertEqual(uploads, [])
                changed.write_bytes(saved)
                repair_release_licenses.publish(root)
                repair_release_licenses.publish(root)
                self.assertEqual(len(uploads), 1)
                self.assertTrue(body.startswith("Original release notes"))
                self.assertEqual(body.count("<!-- release-license-correction -->"), 1)
                for key, error in [
                    ("corrected_name", "Missing verified replacement"),
                    ("original_name", "Original asset changed"),
                ]:
                    asset = remote[record["assets"][-1][key]]
                    saved_digest = asset["digest"]
                    asset["digest"] = "sha256:wrong"
                    with self.assertRaisesRegex(ValueError, error):
                        repair_release_licenses.retire(root)
                    self.assertEqual(deletions, [])
                    asset["digest"] = saved_digest
                repair_release_licenses.retire(root)
                repair_release_licenses.retire(root)
            self.assertEqual(
                set(deletions),
                {a["original_name"] for a in record["assets"]} | {"SHA256SUMS"},
            )
            self.assertEqual(len(deletions), 5)
            self.assertEqual(len(remote), 6)


if __name__ == "__main__":
    unittest.main()
