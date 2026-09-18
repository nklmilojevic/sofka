use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sofka-catalog-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("config/sofka")).unwrap();
        Self(path)
    }

    fn config(&self, trusted: bool, second: bool) {
        let mut text = format!(
            "official=false\n[[catalogs]]\nname='team'\nurl='mirror/index.json'\ntrusted={trusted}\n"
        );
        if second {
            text.push_str("[[catalogs]]\nname='other'\nurl='other/index.json'\ntrusted=true\n");
        }
        std::fs::write(self.0.join("config/sofka/catalogs.toml"), text).unwrap();
    }

    fn publish(&self, directory: &str, version: &str) {
        self.publish_named(directory, version, "local-test");
    }

    fn publish_named(&self, directory: &str, version: &str, id: &str) {
        let path = self.0.join("config/sofka").join(directory);
        std::fs::create_dir_all(&path).unwrap();
        let manifest = format!(
            r#"schema_version = 1
[package]
version = "{version}"
description = "Local test plugin"
license = "MIT"
sofka = ">=0.0.1"
[plugin]
name = "{id}"
palette = "{id}"
command = "echo"
output = "report"
mutating = false
"#
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let encoder = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        let mut tar = tar::Builder::new(encoder);
        tar.append_data(&mut header, "plugin.toml", manifest.as_bytes())
            .unwrap();
        let archive = tar.into_inner().unwrap().finish().unwrap();
        std::fs::write(path.join("package.tar.zst"), &archive).unwrap();
        let index = serde_json::json!({
            "schema_version": 1, "generated_at": "2026-09-18T00:00:00Z",
            "plugins": [{
                "id": id, "display_name": id, "description": "Local test plugin",
                "publisher": "team", "repository": "https://example.invalid/team",
                "versions": [{
                    "version": version, "sofka": ">=0.0.1", "source_commit": "a".repeat(40),
                    "license": "MIT", "readme": "https://example.invalid/readme",
                    "command": "echo", "target": "selection", "output": "report", "mutating": false,
                    "status": "active", "artifacts": [{"platform": "any", "url": "package.tar.zst",
                        "blake3": blake3::hash(&archive).to_hex().to_string(), "size": archive.len()}]
                }]
            }]
        });
        std::fs::write(path.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sofka"))
            .arg("plugin")
            .args(args)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("KUBECONFIG", self.0.join("absent-kubeconfig"))
            .output()
            .unwrap()
    }

    fn complete(&self, shell: &str, words: &[&str]) -> Vec<String> {
        let output = Command::new(env!("CARGO_BIN_EXE_sofka"))
            .arg("--")
            .args(words)
            .env("SOFKA_COMPLETE", shell)
            .env_remove("SOFKA_COMPLETE_WORKER")
            .env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
            .env_remove("_CLAP_IFS")
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("KUBECONFIG", self.0.join("absent-kubeconfig"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{shell} {words:?}: {:?}",
            output.stderr
        );
        assert!(
            output.stderr.is_empty(),
            "{shell} {words:?}: {:?}",
            output.stderr
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| line.split('\t').next().unwrap().to_owned())
            .collect()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn plugin_catalog_cli_installs_and_updates_offline_from_the_original_source() {
    let fixture = Fixture::new();
    fixture.config(true, false);
    fixture.publish("mirror", "1.0.0");
    let search = fixture.ok(&["search", "--offline", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(rows[0]["catalog"], "team");
    fixture.ok(&["describe", "local-test", "--offline"]);
    fixture.ok(&["install", "local-test", "--catalog", "team", "--offline"]);
    fixture.publish("other", "9.0.0");
    fixture.config(true, true);
    fixture.publish("mirror", "2.0.0");
    fixture.ok(&["update", "--offline"]);
    let output = fixture.ok(&["list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["version"], "2.0.0");
    assert!(
        rows[0]["catalog_source"]
            .as_str()
            .unwrap()
            .ends_with("mirror/index.json")
    );
    let output = fixture.run(&["install", "local-test", "--catalog", "other", "--offline"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different catalog"));
    let output = fixture.ok(&["search", "--offline", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|row| row["installed"] == true)
            .count(),
        1
    );
}

#[test]
fn plugin_catalog_cli_rejects_untrusted_sources_ambiguous_ids_and_bad_checksums() {
    let fixture = Fixture::new();
    fixture.publish("mirror", "1.0.0");
    fixture.publish("other", "1.0.0");
    fixture.config(false, false);
    let output = fixture.run(&["search", "--offline"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not trusted"));
    fixture.config(true, true);
    for command in ["describe", "install"] {
        let output = fixture.run(&[command, "local-test", "--offline"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("multiple catalogs"));
    }
    fixture.ok(&["describe", "local-test", "--catalog", "team", "--offline"]);
    std::fs::write(
        fixture.0.join("config/sofka/mirror/package.tar.zst"),
        b"broken",
    )
    .unwrap();
    assert!(
        !fixture
            .run(&["install", "local-test", "--catalog", "team", "--offline"])
            .status
            .success()
    );
    assert!(!fixture.0.join("config/sofka/plugins/local-test").exists());
}

#[test]
fn targeted_update_does_not_load_unrelated_catalogs() {
    let fixture = Fixture::new();
    fixture.config(true, false);
    fixture.publish("mirror", "1.0.0");
    let config = fixture.0.join("config/sofka/catalogs.toml");
    let team = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, team.replace("official=false", "official=true")).unwrap();
    fixture.ok(&["install", "local-test", "--catalog", "team", "--offline"]);
    fixture.publish("mirror", "2.0.0");
    fixture.ok(&["update", "local-test", "--offline"]);

    std::fs::write(&config, format!("{team}\n[[catalogs]]\nname='unavailable'\nurl='http://127.0.0.1:1/index.json'\ntrusted=true\n")).unwrap();
    fixture.publish("mirror", "3.0.0");
    fixture.ok(&["update", "local-test"]);
    let output = fixture.ok(&["list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["version"], "3.0.0");
}

#[test]
fn catalog_scoped_update_limits_installations_before_triage() {
    let fixture = Fixture::new();
    fixture.config(true, true);
    fixture.publish("mirror", "1.0.0");
    fixture.publish_named("other", "1.0.0", "other-test");
    fixture.ok(&["install", "local-test", "--catalog", "team", "--offline"]);
    fixture.ok(&["install", "other-test", "--catalog", "other", "--offline"]);
    fixture.publish("mirror", "2.0.0");
    fixture.publish_named("other", "2.0.0", "other-test");
    fixture.ok(&["update", "--catalog", "team", "--offline"]);
    let output = fixture.ok(&["list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let version = |id| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()["version"]
            .as_str()
            .unwrap()
    };
    assert_eq!(version("local-test"), "2.0.0");
    assert_eq!(version("other-test"), "1.0.0");
    std::fs::write(
        fixture
            .0
            .join("config/sofka/plugins/other-test/plugin.toml"),
        "modified",
    )
    .unwrap();
    fixture.ok(&["update", "--catalog", "team", "--offline"]);
    assert!(
        !fixture
            .run(&["update", "other-test", "--catalog", "team", "--offline"])
            .status
            .success()
    );
    assert!(
        !fixture
            .run(&["update", "--catalog", "missing", "--offline"])
            .status
            .success()
    );
}

#[test]
fn selected_catalog_completion_includes_duplicate_ids_and_its_own_versions() {
    let fixture = Fixture::new();
    fixture.config(true, true);
    fixture.publish("mirror", "1.0.0");
    fixture.publish("other", "9.0.0");
    fixture.ok(&["search", "--offline"]);
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        assert_eq!(
            fixture.complete(
                shell,
                &["sofka", "plugin", "--catalog", "team", "install", "local"]
            ),
            ["local-test"]
        );
        assert_eq!(
            fixture.complete(
                shell,
                &[
                    "sofka",
                    "plugin",
                    "install",
                    "--catalog",
                    "team",
                    "local-test@"
                ]
            ),
            ["local-test@1.0.0"]
        );
        assert_eq!(
            fixture.complete(
                shell,
                &[
                    "sofka",
                    "plugin",
                    "install",
                    "--catalog=other",
                    "local-test@"
                ]
            ),
            ["local-test@9.0.0"]
        );
        assert!(
            fixture
                .complete(
                    shell,
                    &[
                        "sofka",
                        "plugin",
                        "install",
                        "--catalog",
                        "missing",
                        "local"
                    ]
                )
                .is_empty()
        );
        assert!(
            fixture
                .complete(shell, &["sofka", "plugin", "install", "local"])
                .is_empty()
        );
    }
}
