use std::process::Command;

#[test]
fn headless_start_without_current_context_reports_selection_instructions() {
    let dir = std::env::temp_dir().join(format!("sofka-startup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kubeconfig");
    for current in ["", "current-context: ''\n"] {
        std::fs::write(
            &path,
            format!(
                "{current}apiVersion: v1\nkind: Config\ncontexts:\n- name: prod\n  context:\n    cluster: prod\n- name: staging\n  context:\n    cluster: prod\nclusters:\n- name: prod\n  cluster:\n    server: https://127.0.0.1:1\n"
            ),
        )
        .unwrap();
        for mode in ["--check", "--snapshot"] {
            let output = Command::new(env!("CARGO_BIN_EXE_sofka"))
                .arg(mode)
                .env("KUBECONFIG", &path)
                .env("HOME", &dir)
                .env("XDG_CONFIG_HOME", &dir)
                .env("XDG_CACHE_HOME", &dir)
                .env_remove("SOFKA_COMPLETE")
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(
                error.contains(&format!("no current-context in {}", path.display())),
                "{error}"
            );
            assert!(error.contains("--context <name>"), "{error}");
            assert!(error.contains("sofka ctx"), "{error}");
            assert!(!error.contains("in-cluster"), "{error}");
            assert!(!error.contains("is KUBECONFIG"), "{error}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
