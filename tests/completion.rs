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
        for token in [
            "sofka",
            "namespace",
            "context",
            "completion",
            "plugin",
            "offline",
        ] {
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
