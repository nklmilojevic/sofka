import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("package_repos", Path(__file__).with_name("package_repos.py"))
repos = importlib.util.module_from_spec(spec)
spec.loader.exec_module(repos)

RELEASE = [
    "sofka_{v}_amd64.deb", "sofka_{v}_arm64.deb",
    "sofka-{v}-1.x86_64.rpm", "sofka-{v}-1.aarch64.rpm",
    "sofka-{v}-1-x86_64.pkg.tar.zst", "sofka-{v}-1-aarch64.pkg.tar.zst",
    "sofka_{v}_x86_64.apk", "sofka_{v}_aarch64.apk",
]


def release_fixture(root, *versions):
    for version in versions:
        directory = root / ("v" + version)
        directory.mkdir(parents=True)
        for name in RELEASE:
            (directory / name.format(v=version)).write_bytes(version.encode())
    return root


class ClassifyTest(unittest.TestCase):
    def test_release_package_names(self):
        self.assertEqual(repos.classify("sofka_0.31.1_amd64.deb"), ("deb", "amd64", "0.31.1"))
        self.assertEqual(repos.classify("sofka-0.31.1-1.aarch64.rpm"), ("rpm", "aarch64", "0.31.1"))
        self.assertEqual(repos.classify("sofka-0.31.1-1-x86_64.pkg.tar.zst"), ("arch", "x86_64", "0.31.1"))
        self.assertEqual(repos.classify("sofka_0.31.1_aarch64.apk"), ("alpine", "aarch64", "0.31.1"))

    def test_rejects_unknown_files(self):
        for name in ("sofka-v0.31.1-x86_64-unknown-linux-gnu.tar.gz", "sofka_0.31.1_i386.deb", "other_1.0_amd64.deb"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                repos.classify(name)

    def test_versions_sort_numerically(self):
        self.assertEqual(sorted(["0.29.10", "0.29.9", "0.30.0"], key=repos.version_key), ["0.29.9", "0.29.10", "0.30.0"])


class LayoutTest(unittest.TestCase):
    def test_places_packages_per_repository_format(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            packages = release_fixture(root / "packages", "0.29.10", "0.29.9", "0.30.0")
            (packages / "v0.30.0/SHA256SUMS").write_text("")
            tree = root / "tree"
            versions = repos.layout(packages, tree)
            files = sorted(path.relative_to(tree).as_posix() for path in tree.rglob("*") if path.is_file())

        self.assertEqual(versions["deb"], ["0.29.9", "0.29.10", "0.30.0"])
        self.assertEqual(versions["arch"], ["0.30.0"])
        self.assertIn("deb/pool/main/s/sofka/sofka_0.29.9_arm64.deb", files)
        self.assertIn("rpm/aarch64/sofka-0.29.10-1.aarch64.rpm", files)
        self.assertIn("alpine/x86_64/sofka-0.30.0.apk", files)
        self.assertEqual([name for name in files if name.startswith("arch/")],
                         ["arch/aarch64/sofka-0.30.0-1-aarch64.pkg.tar.zst", "arch/x86_64/sofka-0.30.0-1-x86_64.pkg.tar.zst"])
        self.assertEqual(len(files), 3 * 6 + 2)

    def test_requires_packages(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(ValueError):
                repos.layout(Path(temporary), Path(temporary) / "tree")


class TreeTest(unittest.TestCase):
    def complete_tree(self, tree):
        for name in ("sofka.asc", "rpm/sofka.repo", "alpine/sofka.rsa.pub", "deb/dists/stable/Release",
                     "deb/dists/stable/InRelease", "deb/dists/stable/Release.gpg",
                     "deb/pool/main/s/sofka/sofka_1.0.0_amd64.deb", "rpm/x86_64/sofka-1.0.0-1.x86_64.rpm",
                     "rpm/x86_64/repodata/repomd.xml", "rpm/x86_64/repodata/repomd.xml.asc",
                     "arch/x86_64/sofka-1.0.0-1-x86_64.pkg.tar.zst", "arch/x86_64/sofka-1.0.0-1-x86_64.pkg.tar.zst.sig",
                     "arch/x86_64/sofka.db", "arch/x86_64/sofka.db.sig", "arch/x86_64/sofka.files",
                     "arch/x86_64/sofka.files.sig", "alpine/x86_64/sofka-1.0.0.apk", "alpine/x86_64/APKINDEX.tar.gz"):
            path = tree / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name)
        for arch in ("amd64", "arm64"):
            index = tree / f"deb/dists/stable/main/binary-{arch}/Packages"
            index.parent.mkdir(parents=True)
            index.write_text("Package: sofka\nFilename: pool/main/s/sofka/sofka_1.0.0_amd64.deb\n" if arch == "amd64" else "")
            index.with_suffix(".gz").write_text("")

    def test_complete_tree_passes(self):
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary)
            self.complete_tree(tree)
            repos.check_tree(tree)

    def test_missing_signature_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary)
            self.complete_tree(tree)
            (tree / "arch/x86_64/sofka-1.0.0-1-x86_64.pkg.tar.zst.sig").unlink()
            with self.assertRaisesRegex(ValueError, "pkg.tar.zst.sig"):
                repos.check_tree(tree)

    def test_unindexed_debian_package_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary)
            self.complete_tree(tree)
            (tree / "deb/pool/main/s/sofka/sofka_1.0.1_amd64.deb").write_text("")
            with self.assertRaisesRegex(ValueError, "amd64"):
                repos.check_tree(tree)

    def test_symlinks_become_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary)
            self.complete_tree(tree)
            database = tree / "arch/x86_64/sofka.db"
            database.unlink()
            (tree / "arch/x86_64/sofka.db.tar.zst").write_bytes(b"database")
            database.symlink_to("sofka.db.tar.zst")
            with self.assertRaisesRegex(ValueError, "symlinks"):
                repos.check_tree(tree)
            repos.materialize_links(tree)
            repos.check_tree(tree)
            self.assertFalse(database.is_symlink())
            self.assertEqual(database.read_bytes(), b"database")


class ChecksumTest(unittest.TestCase):
    def test_assets_must_match_release_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "sofka_1.0.0_amd64.deb").write_bytes(b"package")
            digest = hashlib.sha256(b"package").hexdigest()
            (directory / "SHA256SUMS").write_text(f"{digest}  sofka_1.0.0_amd64.deb\n{'0' * 64}  other.tar.gz\n")
            repos.verify_checksums(directory)
            (directory / "sofka_1.0.0_amd64.deb").write_bytes(b"tampered")
            with self.assertRaises(ValueError):
                repos.verify_checksums(directory)


class UploadTest(unittest.TestCase):
    def test_packages_upload_before_metadata_and_pruning(self):
        with patch.object(repos, "run") as run:
            repos.upload(Path("tree"), "r2:bucket")
        commands = [[str(argument) for argument in call.args] for call in run.call_args_list]
        self.assertEqual([command[1] for command in commands], ["copy", "copy", "sync"])
        packages, metadata, prune = commands
        self.assertEqual(packages[packages.index("--header-upload") + 1], repos.PACKAGE_CACHE)
        self.assertEqual(metadata[metadata.index("--header-upload") + 1], repos.METADATA_CACHE)
        self.assertEqual(packages[packages.index("--filter") + 1], "- /arch/*/sofka.*")
        self.assertEqual(packages[-3:-2], ["- *"])
        self.assertEqual(metadata[metadata.index("--filter") + 1], "+ /arch/*/sofka.*")
        self.assertIn("- *.deb", metadata)
        self.assertNotIn("--header-upload", prune)
        self.assertIn("--delete-after", prune)


class DocumentationTest(unittest.TestCase):
    def test_documentation_shows_the_verified_snippets(self):
        text = (repos.ROOT / "docs/release-packages.md").read_text()
        for name, snippet in repos.SETUP.items():
            with self.subTest(name=name):
                self.assertTrue(snippet.format(base=repos.BASE_URL) in text, name)


if __name__ == "__main__":
    unittest.main()
