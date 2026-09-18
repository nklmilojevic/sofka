import importlib.util
import copy
import io
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
from types import SimpleNamespace
import tarfile
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch
import zipfile

import yaml

import release_sbom

spec = importlib.util.spec_from_file_location("release_packages", Path(__file__).with_name("release_packages.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def sbom_fixture(root, target="aarch64-apple-darwin", payload=b"test binary"):
    binary = root / "target" / target / "release" / ("sofka.exe" if target.endswith("msvc") else "sofka")
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_bytes(payload)
    messages = root / "messages.jsonl"
    records = [{"reason": "compiler-artifact", "package_id": "dependency", "features": [],
                "profile": {"test": False}, "target": {"kind": ["lib"]}},
               {"reason": "compiler-artifact", "package_id": "root", "features": [],
                "profile": {"test": False, "debug_assertions": False},
                "target": {"kind": ["bin"], "name": "sofka"}, "executable": str(binary)},
               {"reason": "build-finished", "success": True}]
    messages.write_text("".join(json.dumps(record) + "\n" for record in records))
    metadata = {"resolve": {"root": "root"}, "packages": [
        {"id": "root", "name": "sofka", "version": "1.2.3", "license": "MIT"},
        {"id": "dependency", "name": "example", "version": "1.0.0", "license": "MIT",
         "source": release_sbom.CRATES_IO}]}
    return release_sbom.generate(messages, metadata, binary, target, "1.2.3", "a" * 40)


def damaged_sboms(valid):
    for field in ("spdxVersion", "creationInfo", "relationships"):
        document = copy.deepcopy(valid)
        del document[field]
        yield document
    document = copy.deepcopy(valid)
    del document["packages"][0]["downloadLocation"]
    yield document
    document = copy.deepcopy(valid)
    document["packages"].append(copy.deepcopy(document["packages"][0]))
    yield document
    document = copy.deepcopy(valid)
    document["files"][0]["checksums"][0]["checksumValue"] = "invalid"
    yield document
    document = copy.deepcopy(valid)
    document["packages"] = [package for package in document["packages"]
                            if package["SPDXID"] in document["documentDescribes"]]
    yield document
    document = copy.deepcopy(valid)
    document["relationships"] = [relation for relation in document["relationships"]
                                 if relation["relationshipType"] != "BUILD_DEPENDENCY_OF"]
    yield document


class ReleasePackagesTest(unittest.TestCase):
    def test_capture_uses_target_flags_environment_and_json_messages(self):
        for target in release.TARGETS:
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temporary:
                stage = Path(temporary)
                config = release.configure(target, stage, stage / "dist")
                build = config["builds"][0]
                def fake_run(*args, **kwargs):
                    if args[1] == "metadata":
                        return subprocess.CompletedProcess(args, 0, stdout='{"packages": []}')
                    kwargs["stdout"].write('{"reason":"build-finished","success":true}\n')
                with patch.object(release, "run", side_effect=fake_run) as run:
                    messages, metadata, binary = release.capture_build(config, target, stage, {"PATH": "test"})
                capture = run.call_args_list[0]
                self.assertEqual(capture.args, (build["tool"], build["command"], "--target=" + target,
                                               *build["flags"], "--message-format=json-render-diagnostics"))
                self.assertEqual(capture.kwargs["env"], {"PATH": "test", **dict(
                    entry.split("=", 1) for entry in build.get("env", []))})
                self.assertEqual(run.call_args_list[1].kwargs["env"], capture.kwargs["env"])
                self.assertIn(target, run.call_args_list[1].args)
                self.assertEqual(run.call_args_list[1].kwargs["encoding"], "utf-8")
                self.assertEqual(json.loads(messages.read_text())["success"], True)
                self.assertEqual(metadata, {"packages": []})
                self.assertEqual(binary.name, "sofka.exe" if target.endswith("msvc") else "sofka")

    def test_failed_capture_stops_before_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            target = "aarch64-apple-darwin"
            config = release.configure(target, stage, stage / "dist")
            with patch.object(release, "run", side_effect=subprocess.CalledProcessError(1, "cargo")) as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    release.capture_build(config, target, stage, {})
                self.assertEqual(run.call_count, 1)

    def test_sbom_failure_prevents_packaging_and_asset_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
            notices = root / "notices"
            notices.mkdir()
            for name in release.REQUIRED:
                (notices / name).write_text(name)
            rust = root / "share/doc/rust/COPYRIGHT-library.html"
            rust.parent.mkdir(parents=True)
            rust.write_text("Rust license")
            target = "aarch64-apple-darwin"
            args = SimpleNamespace(target=target, notices=notices, goreleaser="goreleaser", snapshot=True)
            generator = Mock(side_effect=ValueError("incomplete Cargo build"))
            with patch.object(release, "ROOT", root), \
                    patch.object(release, "output", return_value=str(root)), \
                    patch.object(release, "configure", return_value={"builds": [{}]}), \
                    patch.object(release, "capture_build", return_value=(root / "messages", {}, root / "sofka")), \
                    patch.dict("sys.modules", {"release_sbom": SimpleNamespace(generate=generator)}), \
                    patch.object(release, "run") as run, \
                    patch.object(release, "publish_assets") as publish:
                with self.assertRaisesRegex(ValueError, "incomplete Cargo build"):
                    release.build_packages(args)
                run.assert_not_called()
                publish.assert_not_called()
                self.assertFalse((root / "target/release-assets").exists())

    def test_asset_collection_requires_sbom(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "archive.tar.gz"
            archive.write_bytes(b"archive")
            artifacts = [{"type": "Archive", "path": str(archive)},
                         {"type": "Binary", "path": str(root / "missing-binary")}]
            document = sbom_fixture(root)
            for invalid in (None, {}, {"files": [], "packages": []}, *damaged_sboms(document)):
                with self.assertRaises(ValueError):
                    release.publish_assets(artifacts, root / "out", invalid, "test")
                self.assertFalse((root / "out").exists())
            release.publish_assets(artifacts, root / "out", document, "test")
            self.assertEqual({path.name for path in (root / "out").iterdir()},
                             {"archive.tar.gz", "test.spdx.json"})
            self.assertEqual(json.loads((root / "out/test.spdx.json").read_text()), document)

    def test_upload_requires_all_target_sboms_and_matching_archive_hashes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for target in release.TARGETS:
                name = "sofka-v1.2.3-" + target
                payload = target.encode()
                binary = "sofka.exe" if target.endswith("msvc") else "sofka"
                if target.endswith("msvc"):
                    with zipfile.ZipFile(root / (name + ".zip"), "w") as archive:
                        archive.writestr(binary, payload)
                else:
                    with tarfile.open(root / (name + ".tar.gz"), "w:gz") as archive:
                        item = tarfile.TarInfo(binary)
                        item.size = len(payload)
                        archive.addfile(item, io.BytesIO(payload))
                document = sbom_fixture(root, target, payload)
                sbom = root / (name + ".spdx.json")
                sbom.write_text(json.dumps(document))
            release.verify_sboms(root, "1.2.3")
            valid = sbom.read_text()
            for invalid in damaged_sboms(document):
                sbom.write_text(json.dumps(invalid))
                with self.assertRaises(ValueError):
                    release.verify_sboms(root, "1.2.3")
            document["files"][0]["checksums"][0]["checksumValue"] = "0" * 64
            sbom.write_text(json.dumps(document))
            with self.assertRaises(ValueError):
                release.verify_sboms(root, "1.2.3")
            sbom.unlink()
            with self.assertRaises(ValueError):
                release.verify_sboms(root, "1.2.3")
            sbom.write_text(valid)
            (root / "unexpected.spdx.json").write_text(valid)
            with self.assertRaises(ValueError):
                release.verify_sboms(root, "1.2.3")

    def test_linux_notice_entries_preserve_nested_paths_and_permissions(self):
        with tempfile.TemporaryDirectory() as temporary:
            stage = Path(temporary)
            notices = stage / "notices"
            nested = notices / "THIRD-PARTY-SOURCES/example/LICENSE"
            nested.parent.mkdir(parents=True)
            nested.write_text("dependency license")
            (notices / "LICENSE-MIT").write_text("project license")
            for target in release.TARGETS:
                if "linux" not in target:
                    continue
                config = release.configure(target, stage, stage / "dist")
                contents = config["nfpms"][0]["contents"]
                self.assertEqual(
                    {item["dst"] for item in contents},
                    {"/usr/share/doc/sofka/" + name for name in (
                        "copyright", "LICENSE-MIT", "THIRD-PARTY-SOURCES/example/LICENSE"
                    )},
                )
                for item in contents:
                    self.assertNotIn("type", item)
                    self.assertEqual(item["file_info"]["mode"], 0o644)

    @unittest.skipUnless(shutil.which("sh"), "requires a POSIX shell")
    def test_install_scripts_have_valid_shell_syntax(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
            for target in release.TARGETS:
                if "linux" not in target:
                    continue
                directory = root / "target/release-assets" / target
                directory.mkdir(parents=True)
                suffixes = [".apk"] if target.endswith("musl") else [".deb", ".rpm", ".pkg.tar.zst"]
                for suffix in suffixes:
                    (directory / ("sofka" + suffix)).touch()
                with patch.object(release, "ROOT", root), patch.object(release, "run") as run:
                    release.install_test(target)
                for call in run.call_args_list:
                    subprocess.run(["sh", "-n", "-c", call.args[-1]], check=True)

    def test_targets_match_license_collection_and_goreleaser(self):
        about = tomllib.loads((release.ROOT / "about.toml").read_text())
        config = yaml.safe_load((release.ROOT / ".goreleaser.yaml").read_text())
        self.assertEqual(set(about["targets"]), set(release.TARGETS))
        self.assertEqual(set(config["builds"][0]["targets"]), set(release.TARGETS))

    def test_each_runner_builds_only_its_target_and_compatible_packages(self):
        for target in release.TARGETS:
            with self.subTest(target=target):
                config = release.configure(target, Path("target/stage"), Path("target/dist"))
                self.assertEqual(config["builds"][0]["targets"], [target])
                self.assertEqual(config["builds"][0]["command"], "build")
                self.assertIn("--locked", config["builds"][0]["flags"])
                if target.endswith("musl"):
                    self.assertEqual(config["nfpms"][0]["formats"], ["apk"])
                    self.assertIn("RUSTFLAGS=-C link-self-contained=yes", config["builds"][0]["env"])
                    self.assertFalse(any("_LINKER=" in item for item in config["builds"][0]["env"]))
                elif "linux" in target:
                    self.assertEqual(config["nfpms"][0]["formats"], ["deb", "rpm", "archlinux"])
                    dependencies = {"deb": {"dependencies": ["libc6 (>= 2.34)", "ca-certificates"]}}
                    configured = release.configure(target, Path("stage"), Path("dist"), dependencies)
                    self.assertEqual(configured["nfpms"][0]["overrides"], dependencies)
                else:
                    self.assertNotIn("nfpms", config)
                if "windows" in target:
                    self.assertIn("RUSTFLAGS=-C target-feature=+crt-static", config["builds"][0]["env"])

    def test_aqua_archive_names_remain_compatible(self):
        config = release.configure("aarch64-apple-darwin", Path("stage"), Path("dist"))
        archive = config["archives"][0]
        self.assertEqual(archive["name_template"], "{{ .ProjectName }}-{{ .Tag }}-{{ .Target }}")
        self.assertEqual(archive["formats"], ["tar.gz"])
        self.assertEqual(archive["format_overrides"], [{"goos": "windows", "formats": ["zip"]}])
        self.assertEqual(config["checksum"]["name_template"], "SHA256SUMS")

    def test_glibc_limit_and_missing_symbols(self):
        self.assertEqual(release.glibc_version("GLIBC_2.9 GLIBC_2.34 GLIBC_2.17"), "2.34")
        for symbols in ("", "GLIBC_2.36", "GLIBC_2.35.1"):
            with self.assertRaises(ValueError):
                release.glibc_version(symbols)

    def test_notices_reject_missing_files_and_reserved_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            notices = root / "notices"
            notices.mkdir()
            rust = root / "rust.html"
            rust.write_text("Rust notices")
            with self.assertRaises(ValueError):
                release.stage_notices(notices, root / "out", rust, "x86_64-pc-windows-msvc")
            for name in release.REQUIRED:
                (notices / name).write_text(name)
            (notices / "sofka.exe").write_bytes(b"unexpected executable")
            with self.assertRaises(ValueError):
                release.stage_notices(notices, root / "out", rust, "x86_64-pc-windows-msvc")

    def test_archive_verification_rejects_missing_or_changed_notices(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            notices = root / "notices"
            notices.mkdir()
            (notices / "LICENSE-MIT").write_bytes(b"license")
            for extension in ("tar.gz", "zip"):
                binary = root / ("sofka.exe" if extension == "zip" else "sofka")
                binary.write_bytes(b"test binary")
                path = root / ("archive." + extension)
                for license_data in (b"license", b"changed", None):
                    files = {binary.name: binary.read_bytes()}
                    if license_data is not None:
                        files["LICENSE-MIT"] = license_data
                    if extension == "zip":
                        with zipfile.ZipFile(path, "w") as archive:
                            for name, data in files.items():
                                archive.writestr(name, data)
                    else:
                        with tarfile.open(path, "w:gz") as archive:
                            for name, data in files.items():
                                item = tarfile.TarInfo(name)
                                item.size = len(data)
                                archive.addfile(item, io.BytesIO(data))
                    if license_data == b"license":
                        release.verify_archive(path, binary, notices)
                    else:
                        with self.assertRaises(ValueError):
                            release.verify_archive(path, binary, notices)


if __name__ == "__main__":
    unittest.main()
