use std::process::Command;

#[test]
fn completion_scripts_work_without_local_configuration() {
    let dir = std::env::temp_dir().join(format!("sofka-completion-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sofka")).unwrap();
    std::fs::write(dir.join("sofka/config.toml"), "[invalid").unwrap();
    let kubeconfig = dir.join("kubeconfig");
    std::fs::write(&kubeconfig, "invalid: [").unwrap();

    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let output = Command::new(env!("CARGO_BIN_EXE_sofka"))
            .args(["completion", shell])
            .env("HOME", &dir)
            .env("XDG_CONFIG_HOME", &dir)
            .env("KUBECONFIG", &kubeconfig)
            .output()
            .unwrap();
        assert!(output.status.success(), "{shell}: {:?}", output.stderr);
        assert!(output.stderr.is_empty(), "{shell}: {:?}", output.stderr);
        let script = String::from_utf8(output.stdout).unwrap();
        for token in ["sofka", "SOFKA_COMPLETE", shell] {
            assert!(script.contains(token), "{shell}: missing {token}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn completion_requires_a_supported_shell() {
    for args in [vec!["completion"], vec!["completion", "unknown-shell"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_sofka"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("shell") || error.contains("SHELL"));
    }
}

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sofka-complete-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn config(&self, name: &str, context: &str, server: &str) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {context}\ncontexts:\n- name: {context}\n  context:\n    cluster: {context}\n    user: user\nclusters:\n- name: {context}\n  cluster:\n    server: {server}\nusers:\n- name: user\n  user: {{}}\n"
        )).unwrap();
        path
    }

    fn complete(&self, shell: &str, config: &std::ffi::OsStr, words: &[&str]) -> Vec<String> {
        self.complete_with(&[], shell, config, words)
    }

    fn complete_with(
        &self,
        env: &[(&str, &str)],
        shell: &str,
        config: &std::ffi::OsStr,
        words: &[&str],
    ) -> Vec<String> {
        let output = Command::new(env!("CARGO_BIN_EXE_sofka"))
            .arg("--")
            .args(words)
            .envs(env.iter().copied())
            .env("SOFKA_COMPLETE", shell)
            .env_remove("SOFKA_COMPLETE_WORKER")
            .env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .env("XDG_CACHE_HOME", &self.0)
            .env("KUBECONFIG", config)
            .env_remove("_CLAP_IFS")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|s| s.split('\t').next().unwrap().to_owned())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn contexts_follow_merged_kubeconfig_and_explicit_path_in_all_shells() {
    let f = Fixture::new("contexts");
    let first = f.config("one", "dev", "http://127.0.0.1:1");
    let second = f.config("two", "prod", "http://127.0.0.1:1");
    let merged = std::env::join_paths([&first, &second]).unwrap();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        assert_eq!(
            f.complete(shell, &merged, &["sofka", "--context", ""]),
            ["dev", "prod"],
            "{shell}"
        );
        assert_eq!(
            f.complete(shell, &merged, &["sofka", "--context", "pr"]),
            ["prod"],
            "{shell}"
        );
        assert_eq!(
            f.complete(
                shell,
                &merged,
                &[
                    "sofka",
                    "--kubeconfig",
                    second.to_str().unwrap(),
                    "--context",
                    ""
                ]
            ),
            ["prod"],
            "{shell}"
        );
    }
    assert_eq!(
        f.complete("bash", &merged, &["sofka", "--context=pr"]),
        ["--context=prod"]
    );
}

#[test]
fn flags_fixed_values_paths_and_free_text() {
    let f = Fixture::new("local");
    let missing = f.0.join("missing");
    let config = missing.as_os_str();
    assert!(
        f.complete("bash", config, &["sofka", "--con"])
            .contains(&"--context".into())
    );
    assert_eq!(
        f.complete("bash", config, &["sofka", "completion", "po"]),
        ["powershell"]
    );
    assert_eq!(
        f.complete("bash", config, &["sofka", "plugin", "--help"]),
        ["--help"]
    );
    let file = f.0.join("file.yaml");
    let dir = f.0.join("folder");
    std::fs::write(&file, "").unwrap();
    std::fs::create_dir(&dir).unwrap();
    let prefix = format!("{}/f", f.0.display());
    let files = f.complete("bash", config, &["sofka", "--kubeconfig", &prefix]);
    assert!(
        files.contains(&file.to_string_lossy().into_owned()),
        "{files:?}"
    );
    assert!(
        f.complete("bash", config, &["sofka", "plugin", "search", &prefix])
            .is_empty()
    );
    let dirs = f.complete("bash", config, &["sofka", "--validate-plugin", &prefix]);
    assert!(
        !dirs.contains(&file.to_string_lossy().into_owned()),
        "{dirs:?}"
    );
    assert!(
        dirs.iter().any(|v| v.starts_with(dir.to_str().unwrap())),
        "{dirs:?}"
    );
    assert!(
        f.complete("bash", config, &["sofka", "--context", ""])
            .is_empty()
    );
}

fn serve(responses: Vec<(&'static str, &'static str)>) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for (path, body) in responses {
            let start = std::time::Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            start.elapsed() < std::time::Duration::from_secs(6),
                            "no request for {path}"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut request = [0; 8192];
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with(&format!("GET {path} ")), "{request}");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (server, handle)
}

#[test]
fn namespaces_use_current_or_explicit_context_without_discovery() {
    const NS: &str = r#"{"apiVersion":"v1","kind":"NamespaceList","metadata":{},"items":[{"metadata":{"name":"payments"}},{"metadata":{"name":"platform"}}]}"#;
    let f = Fixture::new("namespaces");
    let (server, handle) = serve(vec![("/api/v1/namespaces?", NS); 4]);
    let selected = f.config("selected", "selected", &server);
    let other = f.config("other", "other", "http://127.0.0.1:1");
    let merged = std::env::join_paths([&other, &selected]).unwrap();
    assert_eq!(
        f.complete(
            "bash",
            &merged,
            &["sofka", "--context", "selected", "-n", "pay"]
        ),
        ["payments"]
    );
    assert_eq!(
        f.complete(
            "bash",
            selected.as_os_str(),
            &["sofka", "--namespace", "pay"]
        ),
        ["payments"]
    );
    assert_eq!(
        f.complete(
            "bash",
            &merged,
            &["sofka", "--context=selected", "--namespace=pay"]
        ),
        ["--namespace=payments"]
    );
    assert_eq!(
        f.complete(
            "bash",
            &merged,
            &["sofka", "--kubeconfig", selected.to_str().unwrap(), "-npay"]
        ),
        ["-npayments"]
    );
    handle.join().unwrap();
}

#[test]
fn namespaces_complete_when_no_proxy_has_a_trailing_comma() {
    const NS: &str = r#"{"apiVersion":"v1","kind":"NamespaceList","metadata":{},"items":[{"metadata":{"name":"payments"}}]}"#;
    let f = Fixture::new("no-proxy");
    let (server, handle) = serve(vec![("/api/v1/namespaces?", NS)]);
    let config = f.config("config", "ctx", &server);
    // kube-rs refuses a kubeconfig over the empty entry; the excluded server
    // is reached directly instead of through the unreachable proxy.
    assert_eq!(
        f.complete_with(
            &[
                ("HTTPS_PROXY", "http://127.0.0.1:9"),
                ("NO_PROXY", "127.0.0.1,"),
            ],
            "bash",
            config.as_os_str(),
            &["sofka", "-n", "pay"]
        ),
        ["payments"]
    );
    handle.join().unwrap();
}

#[test]
fn resources_include_crds_short_names_and_qualified_names() {
    let f = Fixture::new("resources");
    let (server, handle) = serve(vec![
        (
            "/apis",
            r#"{"apiVersion":"apidiscovery.k8s.io/v2","kind":"APIGroupDiscoveryList","items":[{"metadata":{"name":"example.io"},"versions":[{"version":"v1","resources":[{"resource":"widgets","responseKind":{"group":"example.io","version":"v1","kind":"Widget"},"scope":"Namespaced","singularResource":"widget","verbs":["list"],"shortNames":["wd"]}]}]}]}"#,
        ),
        (
            "/api",
            r#"{"apiVersion":"apidiscovery.k8s.io/v2","kind":"APIGroupDiscoveryList","items":[]}"#,
        ),
    ]);
    let config = f.config("config", "dev", &server);
    let values = f.complete("bash", config.as_os_str(), &["sofka", "--resource", "w"]);
    assert_eq!(values, ["wd", "widget", "widgets", "widgets.example.io"]);
    handle.join().unwrap();
}

#[test]
fn plugin_candidates_use_cached_catalog_and_managed_installations() {
    use serde_json::json;
    let f = Fixture::new("plugins");
    let cache = f.0.join("sofka/plugins");
    std::fs::create_dir_all(cache.join("managed")).unwrap();
    std::fs::create_dir_all(cache.join("manual")).unwrap();
    std::fs::write(cache.join("managed/.sofka-install.json"), "{}").unwrap();
    let mut catalog = json!({
        "schema_version": 1, "commit": "0".repeat(40), "fetched_at": 0,
        "catalog": {"schema_version": 1, "generated_at": "2026-09-17T00:00:00Z", "plugins": [{
            "id": "resource-summary", "display_name": "Resource summary", "description": "Show resources",
            "publisher": "sofka", "repository": "https://github.com/nklmilojevic/sofka-plugins",
            "versions": [{"version": "0.1.0", "sofka": ">=0.1.0, <1.0.0", "source_commit": "0".repeat(40),
                "license": "MIT", "readme": "https://example.com/readme", "command": "./resource-summary",
                "target": "selection", "output": "report", "mutating": false, "status": "active",
                "artifacts": [{"platform": "any", "url": "https://github.com/nklmilojevic/sofka-plugins/releases/download/resource-summary-v0.1.0/resource-summary.tar.zst",
                    "blake3": "0".repeat(64), "size": 10}]}]
        }]}
    });
    let base = catalog["catalog"]["plugins"][0]["versions"][0].clone();
    let mut withdrawn = base.clone();
    withdrawn["version"] = json!("0.2.0");
    withdrawn["status"] = json!("withdrawn");
    withdrawn["withdrawal_reason"] = json!("Test withdrawal");
    let mut incompatible = base.clone();
    incompatible["version"] = json!("0.3.0");
    incompatible["sofka"] = json!(">=99.0.0");
    let mut wrong_platform = base.clone();
    wrong_platform["version"] = json!("0.4.0");
    wrong_platform["artifacts"][0]["platform"] = json!(
        if sofka::plugin_catalog::platform().unwrap() == "x86_64-unknown-linux-gnu" {
            "aarch64-apple-darwin"
        } else {
            "x86_64-unknown-linux-gnu"
        }
    );
    let mut prerelease = base;
    prerelease["version"] = json!("0.5.0-beta.1");
    catalog["catalog"]["plugins"][0]["versions"]
        .as_array_mut()
        .unwrap()
        .extend([withdrawn.clone(), incompatible, wrong_platform, prerelease]);
    let mut unavailable = catalog["catalog"]["plugins"][0].clone();
    unavailable["id"] = json!("resource-withdrawn");
    unavailable["versions"] = json!([withdrawn]);
    catalog["catalog"]["plugins"]
        .as_array_mut()
        .unwrap()
        .push(unavailable);
    std::fs::write(cache.join("catalog-cache.json"), catalog.to_string()).unwrap();
    let missing = f.0.join("missing");
    for command in ["describe", "install"] {
        assert_eq!(
            f.complete(
                "bash",
                missing.as_os_str(),
                &["sofka", "plugin", command, "res"]
            ),
            if command == "install" {
                vec!["resource-summary"]
            } else {
                vec!["resource-summary", "resource-withdrawn"]
            }
        );
        assert_eq!(
            f.complete(
                "bash",
                missing.as_os_str(),
                &["sofka", "plugin", command, "resource-summary@"]
            ),
            if command == "install" {
                vec!["resource-summary@0.1.0", "resource-summary@0.5.0-beta.1"]
            } else {
                vec![
                    "resource-summary@0.1.0",
                    "resource-summary@0.2.0",
                    "resource-summary@0.3.0",
                    "resource-summary@0.4.0",
                    "resource-summary@0.5.0-beta.1",
                ]
            }
        );
    }
    for command in ["update", "remove"] {
        assert_eq!(
            f.complete(
                "bash",
                missing.as_os_str(),
                &["sofka", "plugin", command, "m"]
            ),
            ["managed"]
        );
    }
    assert_eq!(
        f.complete(
            "bash",
            missing.as_os_str(),
            &["sofka", "plugin", "install", "first", "res"]
        ),
        ["resource-summary"]
    );
}

#[cfg(unix)]
#[test]
fn stalled_credential_helpers_are_quiet_and_bounded() {
    let f = Fixture::new("timeout");
    let config = f.config("config", "dev", "https://127.0.0.1:1");
    let text = std::fs::read_to_string(&config).unwrap().replace(
        "user: {}",
        r#"user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: /bin/sh
      args: ["-c", "echo credential-helper-error >&2; sleep 30"]
      interactiveMode: Never"#,
    );
    std::fs::write(&config, text).unwrap();
    let start = std::time::Instant::now();
    assert!(
        f.complete("bash", config.as_os_str(), &["sofka", "-n", ""])
            .is_empty()
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn generated_bash_script_completes_context_values() {
    let f = Fixture::new("bash-script");
    let config = f.config("config", "prod", "http://127.0.0.1:1");
    let script = Command::new(env!("CARGO_BIN_EXE_sofka"))
        .args(["completion", "bash"])
        .output()
        .unwrap();
    assert!(script.status.success());
    let path = f.0.join("completion.bash");
    std::fs::write(&path, script.stdout).unwrap();
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_sofka"))
        .parent()
        .unwrap();
    let mut paths = vec![bin.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#"
source "$1"
COMP_WORDS=(sofka --context pr)
COMP_CWORD=2
COMP_TYPE=9
_clap_complete_sofka sofka pr --context
printf '%s\n' "${COMPREPLY[@]}"
COMP_WORDS=(sofka --context = pr)
COMP_CWORD=3
_clap_complete_sofka sofka pr =
printf '%s\n' "${COMPREPLY[@]}"
"#,
            "bash",
        ])
        .arg(path)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("KUBECONFIG", config)
        .env("HOME", &f.0)
        .env_remove("SOFKA_COMPLETE")
        .env_remove("SOFKA_COMPLETE_WORKER")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "prod\nprod\n");
}

#[test]
fn resource_aliases_respect_context_overrides_without_writing_config() {
    let f = Fixture::new("aliases");
    let config = f.config("kubeconfig", "dev", "http://127.0.0.1:1");
    let dir = f.0.join("sofka");
    std::fs::create_dir_all(dir.join("clusters/dev/dev")).unwrap();
    let base = dir.join("config.toml");
    let text = "[aliases]\nmybase = 'pods'\n[keys]\npalette_next = 'ctrl-n'\n";
    std::fs::write(&base, text).unwrap();
    std::fs::write(
        dir.join("clusters/dev/dev/config.toml"),
        "[aliases]\nmycontext = 'deployments'\n",
    )
    .unwrap();
    assert_eq!(
        f.complete("bash", config.as_os_str(), &["sofka", "my"]),
        ["mybase", "mycontext"]
    );
    assert_eq!(std::fs::read_to_string(base).unwrap(), text);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
}
