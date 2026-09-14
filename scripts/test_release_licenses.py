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
            output = root / "release.tar.gz"
            release_licenses.package(binary, notices, output)
            second = root / "second.tar.gz"
            release_licenses.package(binary, notices, second)
            self.assertEqual(output.read_bytes(), second.read_bytes())
            with tarfile.open(output) as archive:
                self.assertEqual(
                    archive.extractfile("sofka").read(), binary.read_bytes()
                )
                self.assertEqual(archive.getmember("sofka").mode, 0o755)
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


if __name__ == "__main__":
    unittest.main()
