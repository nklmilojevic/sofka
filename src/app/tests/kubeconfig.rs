use super::*;

#[tokio::test]
async fn wrapped_kubeconfig_reaches_discovery_on_startup_and_context_selection() {
    const CHILD: &str = "SOFKA_TEST_WRAPPED_KUBECONFIG";
    if let Ok(phase) = std::env::var(CHILD) {
        let error = if phase == "startup" {
            Cluster::connect(false, false)
                .await
                .err()
                .unwrap()
                .to_string()
        } else {
            let (mut app, mut rx) = test_app();
            app.cluster.connected = false;
            app.start_context_picker(None);
            app.handle_msg(Msg::Contexts {
                generation: app.generation,
                list: vec!["target".into()],
            });
            app.handle_key(press(KeyCode::Enter)).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Msg::ContextSwitched { result, .. } = rx.recv().await.unwrap() {
                        break result.err().unwrap();
                    }
                }
            })
            .await
            .unwrap()
        };
        assert!(error.contains("running API discovery"), "{error}");
        return;
    }
    let directory =
        std::env::temp_dir().join(format!("sofka-wrapped-kubeconfig-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("kubeconfig");
    for binary in [false, true] {
        for wrapped in [false, true] {
            std::fs::write(&path, crate::k8s::kubeconfig::fixture(binary, wrapped)).unwrap();
            for phase in ["startup", "context"] {
                let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
                command.args(["--exact", "app::tests::kubeconfig::wrapped_kubeconfig_reaches_discovery_on_startup_and_context_selection", "--nocapture"])
                    .env(CHILD, phase).env("KUBECONFIG", &path);
                for name in [
                    "HTTPS_PROXY",
                    "https_proxy",
                    "HTTP_PROXY",
                    "http_proxy",
                    "ALL_PROXY",
                    "all_proxy",
                    "KUBE_RS_DEBUG_OVERRIDE_URL",
                    "KUBE_RS_DEBUG_IMPERSONATE_USER",
                    "KUBE_RS_DEBUG_IMPERSONATE_GROUP",
                ] {
                    command.env_remove(name);
                }
                let output = tokio::time::timeout(Duration::from_secs(15), command.output())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "binary={binary}, wrapped={wrapped}, phase={phase}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn plugin_reload_reads_the_changed_kubeconfig_file() {
    const CHILD: &str = "SOFKA_TEST_PLUGIN_KUBECONFIG";
    if let Ok(directory) = std::env::var(CHILD) {
        let directory = std::path::Path::new(&directory);
        let config = |name| {
            format!(
                "apiVersion: v1
kind: Config
contexts:
- name: {name}
  context:
    cluster: example
    user: example
current-context: {name}
"
            )
        };
        let path = directory.join("config");
        let replacement = directory.join("replacement");
        std::fs::write(&path, config("before")).unwrap();
        std::fs::write(&replacement, config("after")).unwrap();
        let (mut app, mut rx) = test_app();
        let context = app.cluster.context.clone();
        let mut plugin = kubeconfig_reload_plugin();
        let report = plugin.args[0].clone();
        plugin.command = "/bin/sh".into();
        plugin.args = vec![
            "-c".into(),
            r#"cp "$1" "$2" && printf '%s' "$3""#.into(),
            "reload-test".into(),
            replacement.to_str().unwrap().into(),
            path.to_str().unwrap().into(),
            report,
        ];
        app.plugins = vec![plugin];
        plugin_command(&mut app, "example-plugin");
        app.handle_msg(plugin_result(&mut rx).await);
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = rx.recv().await.unwrap();
                if matches!(msg, Msg::Contexts { .. } | Msg::Error { .. }) {
                    break msg;
                }
            }
        })
        .await
        .unwrap();
        app.handle_msg(result);
        assert_eq!(app.mode, Mode::Contexts);
        assert_eq!(app.ctx_list, ["after"]);
        assert_eq!(app.cluster.context, context);
        return;
    }
    let directory =
        std::env::temp_dir().join(format!("sofka-plugin-kubeconfig-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app::tests::kubeconfig::plugin_reload_reads_the_changed_kubeconfig_file",
            "--nocapture",
        ])
        .env(CHILD, &directory)
        .env("KUBECONFIG", directory.join("config"))
        .output()
        .await
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
