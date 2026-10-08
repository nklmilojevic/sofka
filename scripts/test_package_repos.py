import hashlib
import json
import shutil
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

    def test_rejects_a_release_without_every_architecture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            packages = release_fixture(root / "packages", "0.30.0", "0.31.0")
            (packages / "v0.31.0/sofka-0.31.0-1.aarch64.rpm").unlink()
            with self.assertRaisesRegex(ValueError, r"v0.31.0 is missing packages: \[\('rpm', 'aarch64'\)\]"):
                repos.layout(packages, root / "tree")

    def test_rejects_mixed_versions_in_a_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            packages = release_fixture(root / "packages", "0.31.0")
            (packages / "v0.31.0/sofka_0.31.0_arm64.deb").rename(packages / "v0.31.0/sofka_0.30.0_arm64.deb")
            with self.assertRaisesRegex(ValueError, "different versions"):
                repos.layout(packages, root / "tree")

    def test_rejects_a_version_in_two_releases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            packages = release_fixture(root / "packages", "0.31.0")
            shutil.copytree(packages / "v0.31.0", packages / "copy")
            with self.assertRaisesRegex(ValueError, "more than one release"):
                repos.layout(packages, root / "tree")

    def test_requires_packages(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(ValueError):
                repos.layout(Path(temporary), Path(temporary) / "tree")


class TreeTest(unittest.TestCase):
    def complete_tree(self, tree):
        for name in ("sofka.asc", "rpm/sofka.repo", "alpine/sofka.rsa.pub", "deb/dists/stable/InRelease",
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
    NOW = 1_800_000_000

    def upload(self, current, published, manifest=None):
        listing = "\n".join(published + ([repos.MANIFEST] if manifest is not None else []))
        deleted = []

        def output(*args):
            return listing if args[1] == "lsf" else json.dumps(manifest)

        def run(*args, **kwargs):
            if args[1] == "delete":
                deleted.extend(Path(args[args.index("--files-from-raw") + 1]).read_text().splitlines())

        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary)
            for name in current:
                (tree / name).parent.mkdir(parents=True, exist_ok=True)
                (tree / name).write_text(name)
            with patch.object(repos, "output", side_effect=output), patch.object(repos, "run", side_effect=run) as mock:
                repos.upload(tree, "r2:bucket", now=self.NOW)
        calls = [call.args for call in mock.call_args_list]
        return calls, deleted, json.loads(mock.call_args_list[-1].kwargs["input"])

    def test_packages_then_referenced_metadata_then_entry_points(self):
        calls, _, _ = self.upload(["deb/dists/stable/InRelease"], [])
        self.assertEqual([call[1] for call in calls], ["copy", "copy", "copy", "rcat"])
        packages, referenced, entry_points = ([str(argument) for argument in call] for call in calls[:3])

        def rules(command):
            return [command[index + 1] for index, argument in enumerate(command) if argument == "--filter"]

        self.assertEqual(packages[packages.index("--header-upload") + 1], repos.PACKAGE_CACHE)
        self.assertEqual(referenced[referenced.index("--header-upload") + 1], repos.METADATA_CACHE)
        self.assertEqual(entry_points[entry_points.index("--header-upload") + 1], repos.METADATA_CACHE)
        self.assertEqual(rules(packages)[-1], "- *")
        self.assertIn("- /rpm/*/repodata/repomd.xml", rules(referenced))
        self.assertIn("- *.deb", rules(referenced))
        self.assertEqual(rules(referenced)[-1], "+ *")
        self.assertEqual(rules(entry_points), [*("+ " + pattern for pattern in repos.ENTRY_POINTS), "- *"])

    def test_retention_starts_when_a_file_leaves_the_indexes(self):
        expired = self.NOW - repos.RETENTION - 1
        recent = self.NOW - repos.RETENTION + 60
        calls, deleted, manifest = self.upload(
            current=["sofka.asc", "rpm/x86_64/new.rpm"],
            published=["sofka.asc", "rpm/x86_64/just-dropped.rpm", "rpm/x86_64/expired.rpm",
                       "rpm/x86_64/recent.rpm", "rpm/x86_64/unknown.rpm"],
            # just-dropped.rpm was in the indexes at the previous publish, long ago.
            manifest={"sofka.asc": None, "rpm/x86_64/just-dropped.rpm": None, "rpm/x86_64/expired.rpm": expired,
                      "rpm/x86_64/recent.rpm": recent, "rpm/x86_64/gone.rpm": expired},
        )
        self.assertEqual(deleted, ["rpm/x86_64/expired.rpm"])
        self.assertEqual([call[1] for call in calls], ["copy", "copy", "copy", "delete", "rcat"])
        self.assertEqual(manifest, {"sofka.asc": None, "rpm/x86_64/new.rpm": None,
                                    "rpm/x86_64/just-dropped.rpm": self.NOW, "rpm/x86_64/recent.rpm": recent,
                                    "rpm/x86_64/unknown.rpm": self.NOW})

    def test_without_a_manifest_nothing_is_removed(self):
        calls, deleted, manifest = self.upload(current=["sofka.asc"], published=["sofka.asc", "rpm/x86_64/old.rpm"])
        self.assertEqual(deleted, [])
        self.assertNotIn("delete", [call[1] for call in calls])
        self.assertEqual(manifest, {"sofka.asc": None, "rpm/x86_64/old.rpm": self.NOW})


class DocumentationTest(unittest.TestCase):
    def test_documentation_shows_the_verified_snippets(self):
        text = (repos.ROOT / "docs/release-packages.md").read_text()
        for name, snippet in repos.SETUP.items():
            with self.subTest(name=name):
                self.assertTrue(snippet.format(base=repos.BASE_URL) in text, name)

    def test_readme_shows_the_verified_apt_and_dnf_snippets(self):
        text = (repos.ROOT / "README.md").read_text()
        for name in ("apt", "dnf"):
            with self.subTest(name=name):
                self.assertTrue(repos.SETUP[name].format(base=repos.BASE_URL) in text, name)


if __name__ == "__main__":
    unittest.main()
