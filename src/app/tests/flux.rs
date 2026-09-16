use super::*;
use http_body_util::BodyExt;

fn helmrelease(name: &str) -> Value {
    json!({
        "apiVersion": "helm.toolkit.fluxcd.io/v2",
        "kind": "HelmRelease",
        "metadata": {"name": name, "namespace": "default"}
    })
}

fn helmchart(name: &str) -> Value {
    json!({
        "apiVersion": "source.toolkit.fluxcd.io/v1",
        "kind": "HelmChart",
        "metadata": {"name": name, "namespace": "default"}
    })
}

fn choose(app: &mut App, item: &str) {
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::FluxMenu);
    let index = app
        .action_menu_items()
        .iter()
        .position(|s| *s == item)
        .unwrap();
    for _ in 0..index {
        app.handle_key(press(KeyCode::Char('j'))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
}

#[tokio::test]
async fn helmrelease_reconcile_patches_selected_and_marked_releases() {
    for (force, bulk, fail) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let (mut app, mut rx) = test_app();
        app.switch_kind("helmreleases");
        apply(&mut app, helmrelease("apps"));
        apply(&mut app, helmrelease("infra"));
        let (requests, mut received) = mpsc::unbounded_channel();
        app.cluster.client = kube::Client::new(
            tower::service_fn(move |request: http::Request<kube::client::Body>| {
                let requests = requests.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    assert_eq!(parts.method, http::Method::PATCH);
                    assert_eq!(
                        parts.headers["content-type"],
                        "application/merge-patch+json"
                    );
                    let bytes = body.collect().await.unwrap().to_bytes();
                    let patch: Value = serde_json::from_slice(&bytes).unwrap();
                    requests
                        .send((parts.uri.path().to_string(), patch))
                        .unwrap();
                    let (status, body) = if fail {
                        (
                            403,
                            json!({
                                "apiVersion": "v1", "kind": "Status", "status": "Failure",
                                "reason": "Forbidden", "message": "patch denied", "code": 403
                            }),
                        )
                    } else {
                        (200, helmrelease("apps"))
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
        if bulk {
            app.handle_key(press(KeyCode::Char(' '))).unwrap();
            app.handle_key(press(KeyCode::Char(' '))).unwrap();
            assert_eq!(app.marked.len(), 2);
        }
        let action = if force {
            "force reconcile"
        } else {
            "reconcile"
        };
        choose(
            &mut app,
            if force {
                "Force reconcile"
            } else {
                "Reconcile now"
            },
        );
        assert_eq!(app.mode, Mode::Table);
        assert!(app.marked.is_empty());
        let mut paths = Vec::new();
        for _ in 0..if bulk { 2 } else { 1 } {
            let (path, patch) = tokio::time::timeout(Duration::from_secs(2), received.recv())
                .await
                .unwrap()
                .unwrap();
            paths.push(path);
            let annotations = &patch["metadata"]["annotations"];
            let requested = annotations["reconcile.fluxcd.io/requestedAt"]
                .as_str()
                .unwrap();
            assert!(requested.parse::<Timestamp>().is_ok());
            assert_eq!(
                annotations.as_object().unwrap().len(),
                if force { 2 } else { 1 }
            );
            if force {
                assert_eq!(annotations["reconcile.fluxcd.io/forceAt"], requested);
            } else {
                assert!(annotations.get("reconcile.fluxcd.io/forceAt").is_none());
            }
            assert!(patch.get("spec").is_none());
        }
        paths.sort();
        let kind = app.kind.as_ref().unwrap();
        let prefix = format!(
            "/apis/helm.toolkit.fluxcd.io/{}/namespaces/default/helmreleases",
            kind.ar.version
        );
        let expected = if bulk {
            vec![format!("{prefix}/apps"), format!("{prefix}/infra")]
        } else {
            vec![format!("{prefix}/apps")]
        };
        assert_eq!(paths, expected);
        let reply = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let msg = rx.recv().await.unwrap();
                if matches!(&msg, Msg::Flash { message, .. } if message.starts_with(action)) {
                    break msg;
                }
            }
        })
        .await
        .unwrap();
        app.handle_msg(reply);
        assert_eq!(app.flash_err, fail);
        if fail {
            assert!(app.flash.starts_with("force reconcile apps failed:"));
        } else {
            let target = if bulk { "2 helmreleases" } else { "apps" };
            assert_eq!(app.flash, format!("{action} requested: {target}"));
        }
        assert!(received.try_recv().is_err());
    }
}

#[tokio::test]
async fn helmchart_actions_patch_selected_and_marked_charts() {
    for action in ["Suspend", "Resume", "Reconcile now"] {
        for bulk in [false, true] {
            let (mut app, mut rx) = test_app();
            app.cluster
                .register_kind("source.toolkit.fluxcd.io", "HelmChart", "helmcharts", true);
            app.switch_kind("helmcharts");
            apply(&mut app, helmchart("apps"));
            apply(&mut app, helmchart("infra"));
            let (requests, mut received) = mpsc::unbounded_channel();
            app.cluster.client = kube::Client::new(
                tower::service_fn(move |request: http::Request<kube::client::Body>| {
                    let requests = requests.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        assert_eq!(parts.method, http::Method::PATCH);
                        assert_eq!(
                            parts.headers["content-type"],
                            "application/merge-patch+json"
                        );
                        let bytes = body.collect().await.unwrap().to_bytes();
                        let patch: Value = serde_json::from_slice(&bytes).unwrap();
                        requests
                            .send((parts.uri.path().to_string(), patch))
                            .unwrap();
                        Ok::<_, std::convert::Infallible>(http::Response::new(
                            http_body_util::Full::new(hyper::body::Bytes::from(
                                helmchart("apps").to_string(),
                            )),
                        ))
                    }
                }),
                "default",
            );
            if bulk {
                app.handle_key(press(KeyCode::Char(' '))).unwrap();
                app.handle_key(press(KeyCode::Char(' '))).unwrap();
                assert_eq!(app.marked.len(), 2);
            }
            choose(&mut app, action);
            assert_eq!(app.mode, Mode::Table);
            assert!(app.marked.is_empty());
            let mut paths = Vec::new();
            for _ in 0..if bulk { 2 } else { 1 } {
                let (path, patch) = tokio::time::timeout(Duration::from_secs(2), received.recv())
                    .await
                    .unwrap()
                    .unwrap();
                paths.push(path);
                match action {
                    "Suspend" => assert_eq!(patch, json!({"spec": {"suspend": true}})),
                    "Resume" => assert_eq!(patch, json!({"spec": {"suspend": false}})),
                    _ => {
                        let requested =
                            patch["metadata"]["annotations"]["reconcile.fluxcd.io/requestedAt"]
                                .as_str()
                                .unwrap();
                        assert!(requested.parse::<Timestamp>().is_ok());
                        assert_eq!(
                            patch,
                            json!({"metadata": {"annotations": {
                                "reconcile.fluxcd.io/requestedAt": requested
                            }}})
                        );
                    }
                }
            }
            paths.sort();
            let prefix = "/apis/source.toolkit.fluxcd.io/v1/namespaces/default/helmcharts";
            let expected = if bulk {
                vec![format!("{prefix}/apps"), format!("{prefix}/infra")]
            } else {
                vec![format!("{prefix}/apps")]
            };
            assert_eq!(paths, expected);
            let verb = match action {
                "Suspend" => "suspended",
                "Resume" => "resumed",
                _ => "reconcile requested:",
            };
            let reply = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let msg = rx.recv().await.unwrap();
                    if matches!(&msg, Msg::Flash { message, .. } if message.starts_with(verb)) {
                        break msg;
                    }
                }
            })
            .await
            .unwrap();
            app.handle_msg(reply);
            assert!(!app.flash_err);
            let target = if bulk { "2 helmcharts" } else { "apps" };
            assert_eq!(app.flash, format!("{verb} {target}"));
            assert!(received.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn flux_menu_requires_the_resource_api_group() {
    for (plural, kind, group) in [
        (
            "kustomizations",
            "Kustomization",
            "kustomize.toolkit.fluxcd.io",
        ),
        ("helmreleases", "HelmRelease", "helm.toolkit.fluxcd.io"),
        (
            "gitrepositories",
            "GitRepository",
            "source.toolkit.fluxcd.io",
        ),
        (
            "helmrepositories",
            "HelmRepository",
            "source.toolkit.fluxcd.io",
        ),
        ("helmcharts", "HelmChart", "source.toolkit.fluxcd.io"),
        (
            "ocirepositories",
            "OCIRepository",
            "source.toolkit.fluxcd.io",
        ),
        ("buckets", "Bucket", "source.toolkit.fluxcd.io"),
        (
            "imagerepositories",
            "ImageRepository",
            "image.toolkit.fluxcd.io",
        ),
        (
            "imageupdateautomations",
            "ImageUpdateAutomation",
            "image.toolkit.fluxcd.io",
        ),
        ("alerts", "Alert", "notification.toolkit.fluxcd.io"),
        ("receivers", "Receiver", "notification.toolkit.fluxcd.io"),
    ] {
        let other_flux_group = if group == "source.toolkit.fluxcd.io" {
            "helm.toolkit.fluxcd.io"
        } else {
            "source.toolkit.fluxcd.io"
        };
        for candidate in [group, "example.com", other_flux_group] {
            let (mut app, _rx) = test_app();
            app.cluster.register_kind(candidate, kind, plural, true);
            app.switch_kind(plural);
            apply(
                &mut app,
                json!({
                    "apiVersion": format!("{candidate}/v1"), "kind": kind,
                    "metadata": {"name": "apps", "namespace": "default"}
                }),
            );
            app.handle_key(press(KeyCode::Char(' '))).unwrap();
            app.handle_key(press(KeyCode::Char('t'))).unwrap();
            assert_eq!(
                app.mode,
                if candidate == group {
                    Mode::FluxMenu
                } else {
                    Mode::Table
                },
                "{plural}.{candidate}",
            );
            assert_eq!(app.marked.len(), 1);
            if candidate != group {
                assert!(app.flash.starts_with("suspend/resume only applies to"));
            }
        }
    }
}

#[tokio::test]
async fn force_reconcile_menu_is_limited_to_flux_helmreleases() {
    for (plural, group, kind) in [
        ("helmreleases", "helm.toolkit.fluxcd.io", "HelmRelease"),
        ("helmcharts", "source.toolkit.fluxcd.io", "HelmChart"),
        (
            "kustomizations",
            "kustomize.toolkit.fluxcd.io",
            "Kustomization",
        ),
        (
            "gitrepositories",
            "source.toolkit.fluxcd.io",
            "GitRepository",
        ),
        ("cronjobs", "batch", "CronJob"),
        ("applications", "argoproj.io", "Application"),
    ] {
        let (mut app, _rx) = test_app();
        app.cluster.register_kind(group, kind, plural, true);
        app.switch_kind(plural);
        apply(
            &mut app,
            json!({
                "apiVersion": format!("{group}/v1"), "kind": kind,
                "metadata": {"name": "apps", "namespace": "default"}
            }),
        );
        app.handle_key(press(KeyCode::Char('t'))).unwrap();
        assert_eq!(app.mode, Mode::FluxMenu);
        assert_eq!(
            app.action_menu_items().contains(&"Force reconcile"),
            group == "helm.toolkit.fluxcd.io"
        );
    }
}

#[tokio::test]
async fn flux_menu_cancel_and_readonly_do_not_start_an_action() {
    for (plural, group, kind, object) in [
        (
            "helmreleases",
            "helm.toolkit.fluxcd.io",
            "HelmRelease",
            helmrelease("apps"),
        ),
        (
            "helmcharts",
            "source.toolkit.fluxcd.io",
            "HelmChart",
            helmchart("apps"),
        ),
    ] {
        for cancel in [KeyCode::Esc, KeyCode::Enter] {
            let (mut app, _rx) = test_app();
            app.cluster.register_kind(group, kind, plural, true);
            app.switch_kind(plural);
            apply(&mut app, object.clone());
            app.handle_key(press(KeyCode::Char(' '))).unwrap();
            let flash = app.flash.clone();
            if cancel == KeyCode::Enter {
                choose(&mut app, "Cancel");
            } else {
                app.handle_key(press(KeyCode::Char('t'))).unwrap();
                for _ in 0..3 {
                    app.handle_key(press(KeyCode::Char('j'))).unwrap();
                }
                app.handle_key(press(cancel)).unwrap();
            }
            assert_eq!(app.mode, Mode::Table);
            assert_eq!(app.flash, flash);
            assert_eq!(app.marked.len(), 1);
            app.readonly = true;
            app.handle_key(press(KeyCode::Char('t'))).unwrap();
            assert_eq!(app.mode, Mode::Table);
            assert!(app.flash_err);
            assert_eq!(app.marked.len(), 1);
        }
    }
}

#[tokio::test]
async fn gitops_inventory_navigation_uses_group_and_scope_without_resource_reads() {
    for (plural, owner_kind) in [
        ("kustomizations", "Kustomization"),
        ("helmreleases", "HelmRelease"),
    ] {
        for (id, target_plural, group, namespace) in [
            ("default_web__Service", "services", "", "default"),
            (
                "default_web_serving.knative.dev_Service",
                "services.serving.knative.dev",
                "serving.knative.dev",
                "default",
            ),
            ("_web__Namespace", "namespaces", "", ""),
        ] {
            let root = json!({"apiVersion":"test/v1", "kind":owner_kind,
                "metadata":{"name":"web", "namespace":"default"},
                "status":{"inventory":{"entries":[{"id":id,"v":"v1"}]}}});
            let (mut app, mut rx, responses, requests) = health_report_app(plural, root.clone());
            app.cluster
                .register_kind("serving.knative.dev", "Service", "services", true);
            app.cluster.register_kind("", "Service", "services", true);
            let path = format!(
                "/apis/{}/namespaces/default/{plural}/web",
                app.kind.as_ref().unwrap().ar.api_version
            );
            responses.lock().unwrap().insert(path.clone(), (200, root));
            open_health_report_key(&mut app, true);
            receive_health_report(&mut app, &mut rx, true).await;
            let index = app
                .gitops_items
                .iter()
                .position(|f| f.target.as_ref().is_some_and(|t| t.plural == target_plural))
                .unwrap();
            assert_eq!(
                requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|p| p.as_str() != "/api/v1/namespaces")
                    .cloned()
                    .collect::<Vec<_>>(),
                vec![path]
            );
            app.gitops_state.select(Some(index));
            app.handle_key(press(KeyCode::Enter)).unwrap();
            assert_eq!(app.mode, Mode::Table);
            assert_eq!(app.kind.as_ref().unwrap().ar.group, group);
            assert_eq!(app.namespace, namespace);
            assert_eq!(app.fields.as_deref(), Some("metadata.name=web"));
        }
    }
}

#[tokio::test]
async fn gitops_inventory_reports_unavailable_entries_and_blocks_navigation() {
    for (inventory, kube_config, expected) in [
        (Value::Null, Value::Null, "no inventory reported"),
        (
            json!({"entries":{}}),
            Value::Null,
            "invalid inventory entries",
        ),
        (
            json!({"entries":[]}),
            Value::Null,
            "no managed resources reported",
        ),
        (
            json!({"entries":[{"id":"bad","v":"v1"}]}),
            Value::Null,
            "invalid inventory entry",
        ),
        (
            json!({"entries":[{"id":"default_web__Service"}]}),
            Value::Null,
            "invalid inventory entry",
        ),
        (
            json!({"entries":[{"id":"default_web_unknown.io_Unknown","v":"v1"}]}),
            Value::Null,
            "resource kind unavailable",
        ),
        (
            json!({"entries":[{"id":"_web__Service","v":"v1"}]}),
            Value::Null,
            "invalid namespace scope",
        ),
        (
            json!({"entries":[{"id":"default_web__Service","v":"v1"}]}),
            json!({"secretRef":{"name":"remote"}}),
            "remote cluster configured",
        ),
        (
            json!({"entries":[{"id":"default_web__Service","v":"v1"}]}),
            json!({"configMapRef":{"name":"remote"}}),
            "remote cluster configured",
        ),
    ] {
        let root = json!({"apiVersion":"helm.toolkit.fluxcd.io/v2", "kind":"HelmRelease",
            "metadata":{"name":"web", "namespace":"default"},
            "spec":{"kubeConfig":kube_config}, "status":{"inventory":inventory}});
        let (mut app, mut rx, responses, _) = health_report_app("helmreleases", root.clone());
        let path = format!(
            "/apis/{}/namespaces/default/helmreleases/web",
            app.kind.as_ref().unwrap().ar.api_version
        );
        responses
            .lock()
            .unwrap()
            .insert(path.clone(), (200, root.clone()));
        open_health_report_key(&mut app, true);
        receive_health_report(&mut app, &mut rx, true).await;
        assert!(
            app.gitops_items.iter().any(|f| f.text.contains(expected)),
            "{expected}"
        );
        let heading = app
            .gitops_items
            .iter()
            .position(|f| f.text == "Managed resources")
            .unwrap();
        for index in heading + 1..app.gitops_items.len() {
            assert!(app.gitops_items[index].target.is_none());
            app.gitops_state.select(Some(index));
            app.handle_key(press(KeyCode::Enter)).unwrap();
            assert_eq!(app.mode, Mode::Gitops);
        }
        let mut updated = root;
        updated["spec"] = json!({});
        updated["status"]["inventory"] =
            json!({"entries":[{"id":"default_web__Service","v":"v1"}]});
        responses.lock().unwrap().insert(path, (200, updated));
        app.handle_key(press(KeyCode::Char('r'))).unwrap();
        receive_health_report(&mut app, &mut rx, true).await;
        assert!(
            app.gitops_items
                .iter()
                .any(|f| f.target.as_ref().is_some_and(|t| t.plural == "services"))
        );
    }
}

#[tokio::test]
async fn gitops_inventory_limits_large_lists() {
    let entries: Vec<_> = (0..502)
        .map(|i| json!({"id":format!("default_web-{i}__Service"),"v":"v1"}))
        .collect();
    let root = json!({"apiVersion":"helm.toolkit.fluxcd.io/v2", "kind":"HelmRelease",
        "metadata":{"name":"web", "namespace":"default"}, "status":{"inventory":{"entries":entries}}});
    let (mut app, mut rx, responses, requests) = health_report_app("helmreleases", root.clone());
    let path = format!(
        "/apis/{}/namespaces/default/helmreleases/web",
        app.kind.as_ref().unwrap().ar.api_version
    );
    responses.lock().unwrap().insert(path.clone(), (200, root));
    open_health_report_key(&mut app, true);
    receive_health_report(&mut app, &mut rx, true).await;
    assert_eq!(
        app.gitops_items
            .iter()
            .filter(|f| f.target.is_some())
            .count(),
        500
    );
    assert!(
        app.gitops_items
            .iter()
            .any(|f| f.text == "2 more inventory entries omitted")
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() != "/api/v1/namespaces")
            .cloned()
            .collect::<Vec<_>>(),
        vec![path]
    );
}
