use super::*;
use http_body_util::BodyExt;

fn workload(kind: &str, name: &str, ns: &str) -> Value {
    json!({"apiVersion": "apps/v1", "kind": kind,
        "metadata": {"name": name, "namespace": ns}})
}

fn marked_workloads(plural: &str, kind: &str) -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.cluster.register_kind("apps", kind, plural, true);
    app.switch_kind(plural);
    app.namespace.clear();
    apply(&mut app, workload(kind, "web", "a"));
    apply(&mut app, workload(kind, "web", "b"));
    apply(&mut app, workload(kind, "untouched", "c"));
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    assert_eq!(app.marked.len(), 2);
    assert_eq!(
        app.selected_ref().unwrap().metadata.namespace.as_deref(),
        Some("c")
    );
    (app, rx)
}

#[tokio::test]
async fn restart_patches_only_confirmed_targets_and_continues_after_failure() {
    for (plural, kind) in [
        ("deployments", "Deployment"),
        ("statefulsets", "StatefulSet"),
        ("daemonsets", "DaemonSet"),
    ] {
        for bulk in [false, true] {
            for failures in 0..=if bulk { 2 } else { 1 } {
                let fail = failures > 0;
                let (mut app, mut rx) = marked_workloads(plural, kind);
                if !bulk {
                    app.handle_key(press(KeyCode::Esc)).unwrap();
                }
                let (requests, mut received) = mpsc::unbounded_channel();
                app.cluster.client = kube::Client::new(
                    tower::service_fn(move |request: http::Request<kube::client::Body>| {
                        let requests = requests.clone();
                        async move {
                            let (parts, body) = request.into_parts();
                            assert_eq!(parts.method, http::Method::PATCH);
                            assert_eq!(
                                parts.headers["content-type"],
                                "application/strategic-merge-patch+json"
                            );
                            let bytes = body.collect().await.unwrap().to_bytes();
                            let patch: Value = serde_json::from_slice(&bytes).unwrap();
                            let path = parts.uri.path().to_string();
                            requests.send((path.clone(), patch)).unwrap();
                            let denied =
                                fail && (failures == 2 || !bulk || path.contains("/namespaces/a/"));
                            let (status, body) = if denied {
                                (
                                    403,
                                    json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                                    "reason": "Forbidden", "message": "patch denied", "code": 403}),
                                )
                            } else {
                                (200, workload(kind, "web", "b"))
                            };
                            Ok::<_, std::convert::Infallible>(
                                http::Response::builder()
                                    .status(status)
                                    .body(http_body_util::Full::new(hyper::body::Bytes::from(
                                        body.to_string(),
                                    )))
                                    .unwrap(),
                            )
                        }
                    }),
                    "default",
                );
                let typed = bulk && plural == "daemonsets";
                if typed {
                    app.guardrails = vec![crate::config::Guardrail {
                        actions: vec!["restart".into()],
                        confirmation: Some("type-resource-name".into()),
                        ..Default::default()
                    }];
                }
                app.handle_key(press(KeyCode::Char('r'))).unwrap();
                if typed {
                    assert_eq!(app.mode, Mode::Prompt);
                    assert!(app.prompt_label.contains("'2'"));
                } else {
                    assert_eq!(app.mode, Mode::Confirm);
                    assert_eq!(
                        app.confirm_label,
                        if bulk {
                            format!("Restart 2 {plural}?")
                        } else {
                            "Restart untouched in c?".into()
                        }
                    );
                }
                assert!(received.try_recv().is_err());
                // Changes after confirmation opens must not change its targets.
                app.marked.clear();
                app.marked.insert("c/untouched".into());
                app.table_state.select(Some(0));
                if typed {
                    app.handle_key(press(KeyCode::Char('2'))).unwrap();
                    app.handle_key(press(KeyCode::Enter)).unwrap();
                } else {
                    app.handle_key(press(KeyCode::Char('y'))).unwrap();
                }
                assert!(app.marked.is_empty());
                let mut paths = Vec::new();
                let mut patches = Vec::new();
                for _ in 0..if bulk { 2 } else { 1 } {
                    let (path, patch) =
                        tokio::time::timeout(Duration::from_secs(2), received.recv())
                            .await
                            .unwrap()
                            .unwrap();
                    let timestamp = patch["spec"]["template"]["metadata"]["annotations"]["kubectl.kubernetes.io/restartedAt"].as_str().unwrap();
                    assert!(timestamp.parse::<Timestamp>().is_ok());
                    assert_eq!(patch, restart_patch(timestamp));
                    paths.push(path);
                    patches.push(patch);
                }
                assert_eq!(
                    paths,
                    if bulk {
                        vec![
                            format!("/apis/apps/v1/namespaces/a/{plural}/web"),
                            format!("/apis/apps/v1/namespaces/b/{plural}/web"),
                        ]
                    } else {
                        vec![format!("/apis/apps/v1/namespaces/c/{plural}/untouched")]
                    }
                );
                assert!(patches.iter().all(|patch| patch == &patches[0]));
                tokio::time::timeout(Duration::from_secs(2), async {
                    while let Some(msg) = rx.recv().await {
                        if let Msg::Flash {
                            ref message, err, ..
                        } = msg
                        {
                            assert_eq!(err, fail);
                            assert!(
                                message.contains(if fail {
                                    if bulk {
                                        "restart web in a failed:"
                                    } else {
                                        "restart untouched in c failed:"
                                    }
                                } else {
                                    "restarted"
                                }),
                                "{message}"
                            );
                            if failures == 2 {
                                assert!(message.contains("restart web in b failed:"), "{message}");
                            }
                            let expected = message.clone();
                            app.handle_msg(msg);
                            assert_eq!(app.flash, expected);
                            assert_eq!(app.flash_err, fail);
                            if fail {
                                assert_eq!(
                                    app.last_action_error.as_deref(),
                                    Some(expected.as_str())
                                );
                            }
                            break;
                        }
                    }
                })
                .await
                .unwrap();
                app.cluster.client = Cluster::fake().client;
                assert!(
                    tokio::time::timeout(Duration::from_secs(2), received.recv())
                        .await
                        .unwrap()
                        .is_none()
                );
                while let Ok(msg) = rx.try_recv() {
                    assert!(!matches!(msg, Msg::Flash { .. }));
                }
            }
        }
    }
}

#[tokio::test]
async fn restart_cancel_and_guardrails_keep_marks() {
    let (mut app, _rx) = marked_workloads("deployments", "Deployment");
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert!(app.confirm_action.is_none());
    assert_eq!(app.marked.len(), 2);
    for rule in [
        crate::config::Guardrail {
            max_bulk: Some(1),
            ..Default::default()
        },
        crate::config::Guardrail {
            namespaces: vec!["a".into()],
            deny: true,
            ..Default::default()
        },
    ] {
        app.guardrails = vec![crate::config::Guardrail {
            actions: vec!["restart".into()],
            ..rule
        }];
        app.handle_key(press(KeyCode::Char('r'))).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.confirm_action.is_none());
        assert!(app.flash.contains("guardrail"));
        assert_eq!(app.marked.len(), 2);
    }
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["restart".into()],
        confirmation: Some("type-resource-name".into()),
        ..Default::default()
    }];
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    for c in "wrong".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("did not match"));
    assert_eq!(app.marked.len(), 2);
    app.guardrails.clear();
    app.readonly = true;
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.confirm_action.is_none());
}

#[tokio::test]
async fn restart_does_not_fall_back_when_marks_are_stale() {
    let (mut app, _rx) = marked_workloads("deployments", "Deployment");
    app.marked.clear();
    app.marked.insert("default/missing".into());
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.confirm_action.is_none());
}
