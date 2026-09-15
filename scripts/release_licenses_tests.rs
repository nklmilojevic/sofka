use super::*;
use std::os::unix::{fs::symlink, process::ExitStatusExt};

#[test]
fn digest_matches_sha256_hex_vectors() {
    assert_eq!(
        digest(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        digest(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

fn temporary() -> Temporary {
    Temporary::new(&std::env::temp_dir()).unwrap()
}

fn notices(root: &Path) -> PathBuf {
    let directory = root.join("notices");
    fs::create_dir_all(&directory).unwrap();
    for name in REQUIRED {
        fs::write(directory.join(name), name).unwrap();
    }
    directory
}

#[test]
fn archive_preserves_binary_notices_sources_and_permissions() {
    let root = temporary();
    let binary = root.0.join("binary");
    fs::write(&binary, b"\x7fELF\0unchanged binary").unwrap();
    let notices = notices(&root.0);
    let sources = notices.join("THIRD-PARTY-SOURCES");
    fs::create_dir(&sources).unwrap();
    fs::write(sources.join("matcher.crate"), b"source archive").unwrap();
    let runtime = root.0.join("rust.html");
    fs::write(&runtime, "Rust library notices").unwrap();
    let first = root.0.join("first.tar.gz");
    let second = root.0.join("second.tar.gz");
    package(&binary, &notices, &first, &runtime).unwrap();
    package(&binary, &notices, &second, &runtime).unwrap();
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(first).unwrap()));
    let mut contents = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().into_owned();
        assert!(entry.header().entry_type().is_file());
        assert_eq!(
            entry.header().mode().unwrap(),
            if name == Path::new("sofka") {
                0o755
            } else {
                0o644
            }
        );
        assert_eq!(entry.header().mtime().unwrap(), 0);
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert!(contents.insert(name, bytes).is_none());
    }
    assert_eq!(contents.len(), 6);
    assert_eq!(contents[Path::new("sofka")], fs::read(binary).unwrap());
    assert_eq!(
        contents[Path::new("RUST-LICENSES.html")],
        fs::read(runtime).unwrap()
    );
    assert_eq!(
        contents[Path::new("THIRD-PARTY-SOURCES/matcher.crate")],
        b"source archive"
    );
    for name in REQUIRED {
        assert_eq!(contents[Path::new(name)], name.as_bytes());
    }
}

#[test]
fn packaging_rejects_missing_empty_symlink_and_duplicate_files() {
    let root = temporary();
    let notices = notices(&root.0);
    let binary = root.0.join("binary");
    let runtime = root.0.join("rust.html");
    let output = root.0.join("out.tar.gz");
    fs::write(&binary, "binary").unwrap();
    fs::write(&runtime, "runtime").unwrap();
    for name in REQUIRED {
        let path = notices.join(name);
        fs::write(&path, "").unwrap();
        assert!(package(&binary, &notices, &output, &runtime).is_err());
        fs::remove_file(&path).unwrap();
        assert!(package(&binary, &notices, &output, &runtime).is_err());
        fs::write(path, name).unwrap();
    }
    fs::write(&binary, "").unwrap();
    assert!(package(&binary, &notices, &output, &runtime).is_err());
    fs::write(&binary, "binary").unwrap();
    fs::write(&runtime, "").unwrap();
    assert!(package(&binary, &notices, &output, &runtime).is_err());
    fs::write(&runtime, "runtime").unwrap();
    for path in [&binary, &root.0] {
        symlink(path, notices.join("link")).unwrap();
        assert!(package(&binary, &notices, &output, &runtime).is_err());
        fs::remove_file(notices.join("link")).unwrap();
    }
    for name in ["sofka", "RUST-LICENSES.html"] {
        fs::write(notices.join(name), "duplicate").unwrap();
        assert!(package(&binary, &notices, &output, &runtime).is_err());
        fs::remove_file(notices.join(name)).unwrap();
    }
    assert!(!output.exists());
}

fn response(exit: i32, status: &str) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(exit << 8),
        stdout: status.as_bytes().to_vec(),
        stderr: b"TLS connection error".to_vec(),
    }
}

#[test]
fn download_retries_tls_failure_and_caches_only_complete_bytes() {
    let root = temporary();
    let mut calls = 0;
    let mut delays = Vec::new();
    let url = "https://example.test/LICENSE";
    let bytes = download_with(
        url,
        &root.0,
        |path| {
            calls += 1;
            if calls == 1 {
                fs::write(path, "partial response")?;
                Ok(response(35, "000"))
            } else {
                assert!(!path.exists());
                fs::write(path, "license")?;
                Ok(response(0, "200"))
            }
        },
        |delay| delays.push(delay),
    )
    .unwrap();
    assert_eq!(bytes.unwrap(), b"license");
    assert_eq!(calls, 2);
    assert_eq!(delays, [Duration::from_secs(1)]);
    let cached = download_with(
        url,
        &root.0,
        |_| panic!("Cached data must not be downloaded"),
        |_| {},
    )
    .unwrap();
    assert_eq!(cached.unwrap(), b"license");
}

#[test]
fn download_caches_404_but_reports_persistent_connection_errors() {
    let root = temporary();
    let missing = "https://example.test/missing";
    assert!(
        download_with(missing, &root.0, |_| Ok(response(0, "404")), |_| {})
            .unwrap()
            .is_none()
    );
    assert!(
        download_with(
            missing,
            &root.0,
            |_| panic!("A cached 404 must not be downloaded"),
            |_| {}
        )
        .unwrap()
        .is_none()
    );
    let url = "https://example.test/LICENSE";
    let mut calls = 0;
    let error = download_with(
        url,
        &root.0,
        |path| {
            calls += 1;
            fs::write(path, "partial")?;
            Ok(response(35, "000"))
        },
        |_| {},
    )
    .unwrap_err()
    .to_string();
    assert_eq!(calls, 4);
    assert!(error.contains(url));
    assert!(error.contains("TLS connection error"));
    assert!(!root.0.join(digest(url.as_bytes())).exists());
    assert!(
        !root
            .0
            .join(digest(url.as_bytes()))
            .with_extension("missing")
            .exists()
    );
}

#[test]
fn download_retries_server_errors_but_not_forbidden_responses() {
    for (status, expected) in [("503", 4), ("429", 4), ("403", 1)] {
        let root = temporary();
        let mut calls = 0;
        assert!(
            download_with(
                "https://example.test/LICENSE",
                &root.0,
                |_| {
                    calls += 1;
                    Ok(response(0, status))
                },
                |_| {}
            )
            .is_err()
        );
        assert_eq!(calls, expected);
    }
}

fn fixture(root: &Path) -> (Report, Lockfile, PathBuf, PathBuf) {
    let manifest = root.join("Cargo.toml");
    fs::write(&manifest, "").unwrap();
    for name in &REQUIRED[..2] {
        fs::write(root.join(name), name).unwrap();
    }
    let dependency = root.join("dependency");
    fs::create_dir(&dependency).unwrap();
    fs::write(dependency.join("Cargo.toml"), "").unwrap();
    fs::write(dependency.join("LICENSE"), "dependency notice").unwrap();
    let package = serde_json::json!({
        "name": "matcher", "version": "1.0.0", "id": "matcher-id",
        "manifest_path": dependency.join("Cargo.toml"),
        "source": "registry+https://github.com/rust-lang/crates.io-index",
        "license": "MPL-2.0"
    });
    let report = serde_json::from_value(serde_json::json!({
        "crates": [{"package": package}],
        "licenses": [{"id": "MPL-2.0", "name": "Mozilla Public License 2.0", "text": "license text", "used_by": [{"crate": {"id": "matcher-id", "name": "matcher", "version": "1.0.0"}}]}]
    })).unwrap();
    let cache = root.join("cache");
    fs::create_dir(&cache).unwrap();
    fs::write(
        cache.join(digest(
            b"https://static.crates.io/crates/matcher/matcher-1.0.0.crate",
        )),
        b"source archive",
    )
    .unwrap();
    let lock = Lockfile {
        package: vec![LockedPackage {
            name: "matcher".into(),
            version: "1.0.0".into(),
            checksum: Some(digest(b"source archive")),
        }],
    };
    (report, lock, fs::canonicalize(manifest).unwrap(), cache)
}

#[test]
fn generation_includes_notices_and_checksum_verified_mpl_sources() {
    let root = temporary();
    let (report, lock, manifest, cache) = fixture(&root.0);
    let output = root.0.join("output");
    write_report(
        report,
        &manifest,
        &output,
        &cache,
        &["x86_64-unknown-linux-gnu".into()],
        lock,
    )
    .unwrap();
    validate_notices(&output).unwrap();
    assert_eq!(
        fs::read(output.join("THIRD-PARTY-SOURCES/matcher-1.0.0.crate")).unwrap(),
        b"source archive"
    );
    let text = fs::read_to_string(output.join("THIRD-PARTY-LICENSES.txt")).unwrap();
    for expected in [
        "matcher 1.0.0",
        "Selected licenses: MPL-2.0",
        "x86_64-unknown-linux-gnu",
        "license text",
        "dependency notice",
    ] {
        assert!(text.contains(expected));
    }
}

#[test]
fn generation_rejects_bad_checksums_empty_reports_and_unselected_licenses() {
    for problem in ["checksum", "empty", "unselected", "text", "source"] {
        let root = temporary();
        let (mut report, mut lock, manifest, cache) = fixture(&root.0);
        match problem {
            "checksum" => lock.package[0].checksum = Some("wrong".into()),
            "empty" => report.crates.clear(),
            "unselected" => report.licenses[0].used_by.clear(),
            "text" => report.licenses[0].text.clear(),
            "source" => {
                report.crates[0].package.source = Some("git+https://example.test/repo".into())
            }
            _ => unreachable!(),
        }
        let output = root.0.join("output");
        assert!(
            write_report(report, &manifest, &output, &cache, &["target".into()], lock).is_err()
        );
        assert!(!output.exists());
    }
}

#[test]
fn collector_includes_nested_and_declared_notices() {
    let root = temporary();
    let (mut report, _, _, _) = fixture(&root.0);
    let package = &mut report.crates[0].package;
    let dependency = package.manifest_path.parent().unwrap();
    for (name, content) in [
        ("native/COPYING", "native license"),
        (".licenses/Author", "author notice"),
        ("terms.txt", "declared notice"),
    ] {
        let path = dependency.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    package.license_file = Some("terms.txt".into());
    assert_eq!(
        license_files(package).unwrap(),
        vec![
            (".licenses/Author".into(), "author notice".into()),
            ("LICENSE".into(), "dependency notice".into()),
            ("native/COPYING".into(), "native license".into()),
            ("terms.txt".into(), "declared notice".into()),
        ]
    );
}
