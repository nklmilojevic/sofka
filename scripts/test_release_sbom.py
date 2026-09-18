import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import release_sbom as sbom


class ReleaseSbomTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.target = "aarch64-apple-darwin"
        self.binary = self.root / self.target / "release/sofka"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_bytes(b"stripped release binary")
        self.messages = self.root / "build.jsonl"
        self.root_id = "path+file:///source#sofka@1.2.3"
        self.dep_id = "registry+https://github.com/rust-lang/crates.io-index#used@2.0.0"
        self.unused_id = "registry+https://github.com/rust-lang/crates.io-index#disabled@3.0.0"
        self.metadata = {
            "resolve": {"root": self.root_id},
            "packages": [self.package(self.root_id, "sofka", "1.2.3", None),
                         self.package(self.dep_id, "used", "2.0.0"),
                         self.package(self.unused_id, "disabled", "3.0.0")],
        }
        self.records = [self.artifact(self.dep_id, "proc-macro", features=["derive"]),
                        self.artifact(self.dep_id, "lib", features=["std"]),
                        self.artifact(self.root_id, "bin", self.binary),
                        {"reason": "build-finished", "success": True}]

    @staticmethod
    def package(package_id, name, version, source=sbom.CRATES_IO):
        return {"id": package_id, "name": name, "version": version,
                "source": source, "license": "MIT OR Apache-2.0"}

    @staticmethod
    def artifact(package_id, kind, executable=None, features=None):
        return {"reason": "compiler-artifact", "package_id": package_id,
                "target": {"kind": [kind], "name": "sofka" if executable else "used"},
                "profile": {"test": False, "debug_assertions": False},
                "features": features or [], "fresh": True,
                "executable": str(executable) if executable else None}

    def generate(self, binary=None, target=None):
        self.messages.write_text("".join(json.dumps(record) + "\n" for record in self.records))
        return sbom.generate(self.messages, self.metadata, binary or self.binary,
                             target or self.target, "1.2.3", "a" * 40)

    def test_cached_artifacts_select_packages_and_keep_build_inputs(self):
        document = self.generate()
        packages = {package["name"]: package for package in document["packages"]}
        self.assertEqual(set(packages), {"sofka", "used"})
        self.assertIn("lib, proc-macro", packages["used"]["comment"])
        self.assertIn("derive, std", packages["used"]["comment"])
        self.assertEqual(document["files"][0]["checksums"][0]["checksumValue"],
                         hashlib.sha256(self.binary.read_bytes()).hexdigest())
        self.assertIn("not a dependency graph", document["comment"])
        self.assertNotIn(str(self.root), json.dumps(document))
        self.assertNotIn("/source", json.dumps(document))
        edges = [edge for edge in document["relationships"] if edge["relationshipType"] == "BUILD_DEPENDENCY_OF"]
        self.assertEqual(len(edges), 1)
        self.assertEqual(edges[0]["relatedSpdxElement"], document["documentDescribes"][0])

    def test_incomplete_failed_and_mixed_builds_are_rejected(self):
        original = copy.deepcopy(self.records)
        cases = [original[:-1], original[:-1] + [{"reason": "build-finished", "success": False}],
                 original + [original[0]], original[:-2] + [original[-1]],
                 original[:-1] + [original[-2], original[-1]]]
        for kind in ("test", "bench", "example"):
            record = self.artifact(self.dep_id, kind)
            cases.append([record] + original)
        for field, value in (("test", True), ("debug_assertions", True)):
            record = copy.deepcopy(original[-2])
            record["profile"][field] = value
            cases.append(original[:-2] + [record, original[-1]])
        for records in cases:
            with self.subTest(records=records):
                self.records = records
                with self.assertRaises(ValueError):
                    self.generate()

    def test_changed_binary_wrong_target_and_wrong_version_fail(self):
        packaged = self.root / "packaged/sofka"
        packaged.parent.mkdir()
        packaged.write_bytes(self.binary.read_bytes())
        self.generate(binary=packaged)
        packaged.write_bytes(b"different binary")
        with self.assertRaisesRegex(ValueError, "differs"):
            self.generate(binary=packaged)
        with self.assertRaisesRegex(ValueError, "target/profile"):
            self.generate(target="x86_64-apple-darwin")
        self.metadata["packages"][0]["version"] = "9.9.9"
        with self.assertRaisesRegex(ValueError, "root package"):
            self.generate()

    def test_full_package_identity_prevents_name_version_collisions(self):
        other_id = "git+https://example.com/used#used@2.0.0"
        self.metadata["packages"].append(self.package(other_id, "used", "2.0.0", "git+https://example.com/used"))
        self.records.insert(0, self.artifact(other_id, "lib"))
        packages = self.generate()["packages"]
        used = [package for package in packages if package["name"] == "used"]
        self.assertEqual(len(used), 2)
        self.assertNotEqual(used[0]["SPDXID"], used[1]["SPDXID"])
        self.assertEqual(sum("externalRefs" in package for package in used), 1)
        self.metadata["packages"] = [package for package in self.metadata["packages"] if package["id"] != other_id]
        with self.assertRaisesRegex(ValueError, "matching metadata"):
            self.generate()

    def test_windows_binary_name_is_preserved(self):
        self.target = "aarch64-pc-windows-msvc"
        self.binary = self.root / self.target / "release/sofka.exe"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_bytes(b"PE release binary")
        self.records[-2]["executable"] = str(self.binary)
        self.assertEqual(self.generate()["files"][0]["fileName"], "./sofka.exe")


if __name__ == "__main__":
    unittest.main()
