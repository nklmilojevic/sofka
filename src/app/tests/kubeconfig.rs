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
