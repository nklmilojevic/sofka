import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import tomllib
import unittest
import zipfile

import yaml

spec = importlib.util.spec_from_file_location("release_packages", Path(__file__).with_name("release_packages.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleasePackagesTest(unittest.TestCase):
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
