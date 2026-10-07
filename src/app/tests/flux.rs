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
async fn flux_operator_actions_patch_the_reconcile_annotations() {
    for (plural, kind) in [
        ("resourcesets", "ResourceSet"),
        ("resourcesetinputproviders", "ResourceSetInputProvider"),
        ("fluxinstances", "FluxInstance"),
    ] {
        let mut actions = vec!["Suspend", "Resume", "Reconcile now"];
        if plural != "resourcesets" {
            actions.push("Force reconcile");
        }
        for action in actions {
            let (mut app, mut rx) = test_app();
            app.cluster
                .register_kind("fluxcd.controlplane.io", kind, plural, true);
            app.switch_kind(plural);
            let object = json!({
                "apiVersion": "fluxcd.controlplane.io/v1", "kind": kind,
                "metadata": {"name": "apps", "namespace": "default"}
            });
            apply(&mut app, object.clone());
            let (requests, mut received) = mpsc::unbounded_channel();
            app.cluster.client = kube::Client::new(
                tower::service_fn(move |request: http::Request<kube::client::Body>| {
                    let requests = requests.clone();
                    let object = object.clone();
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
                            http_body_util::Full::new(hyper::body::Bytes::from(object.to_string())),
                        ))
                    }
                }),
                "default",
            );
            choose(&mut app, action);
            let (path, patch) = tokio::time::timeout(Duration::from_secs(2), received.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                path,
                format!("/apis/fluxcd.controlplane.io/v1/namespaces/default/{plural}/apps")
            );
            assert!(patch.get("spec").is_none(), "{plural} {action}: {patch}");
            let annotations = &patch["metadata"]["annotations"];
            let requested = annotations["reconcile.fluxcd.io/requestedAt"].as_str();
            match action {
                "Suspend" => assert_eq!(
                    patch,
                    json!({"metadata": {"annotations": {
                        "fluxcd.controlplane.io/reconcile": "disabled"
                    }}})
                ),
                "Resume" => {
                    let requested = requested.unwrap();
                    assert!(requested.parse::<Timestamp>().is_ok());
                    assert_eq!(
                        patch,
                        json!({"metadata": {"annotations": {
                            "fluxcd.controlplane.io/reconcile": "enabled",
                            "reconcile.fluxcd.io/requestedAt": requested
                        }}})
                    );
                }
                "Reconcile now" => {
                    assert!(requested.unwrap().parse::<Timestamp>().is_ok());
                    assert_eq!(annotations.as_object().unwrap().len(), 1);
                }
                _ => {
                    assert_eq!(
                        annotations["reconcile.fluxcd.io/forceAt"].as_str(),
                        requested
                    );
                    assert_eq!(annotations.as_object().unwrap().len(), 2);
                }
            }
            let verb = match action {
                "Suspend" => "suspended apps",
                "Resume" => "resumed apps",
                "Reconcile now" => "reconcile requested: apps",
                _ => "force reconcile requested: apps",
            };
            let reply = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let msg = rx.recv().await.unwrap();
                    if matches!(&msg, Msg::Flash { message, .. } if message == verb) {
                        break msg;
                    }
                }
            })
            .await
            .unwrap();
            app.handle_msg(reply);
            assert!(!app.flash_err);
            assert_eq!(app.flash, verb);
        }
    }
}

#[tokio::test]
async fn enter_on_a_resourceset_opens_gitops_with_providers_and_inventory() {
    let root = json!({
        "apiVersion": "fluxcd.controlplane.io/v1", "kind": "ResourceSet",
        "metadata": {
            "name": "apps", "namespace": "default",
            "annotations": {"fluxcd.controlplane.io/reconcile": "disabled"}
        },
        "spec": {
            "inputsFrom": [
                {"kind": "ResourceSetInputProvider", "name": "prs"},
                {"kind": "ResourceSetInputProvider", "selector": {
                    "matchLabels": {"team": "web"},
                    "matchExpressions": [{"key": "env", "operator": "In", "values": ["dev", "prod"]}]
                }}
            ],
            "dependsOn": [{
                "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                "name": "helmreleases.helm.toolkit.fluxcd.io", "ready": true
            }]
        },
        "status": {
            "conditions": [{"type": "Ready", "status": "True"}],
            "inventory": {"entries": [{"id": "default_web__Service", "v": "v1"}]}
        }
    });
    let (mut app, rx) = test_app();
    for (kind, plural) in [
        ("ResourceSet", "resourcesets"),
        ("ResourceSetInputProvider", "resourcesetinputproviders"),
    ] {
        app.cluster
            .register_kind("fluxcd.controlplane.io", kind, plural, true);
    }
    let (mut app, mut rx, responses, _) =
        health_report_app_with(app, rx, "resourcesets", root.clone());
    responses.lock().unwrap().insert(
        "/apis/fluxcd.controlplane.io/v1/namespaces/default/resourcesets/apps".into(),
        (200, root),
    );
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Gitops);
    receive_health_report(&mut app, &mut rx, true).await;
    let texts: Vec<_> = app.gitops_items.iter().map(|f| f.text.as_str()).collect();
    assert!(
        texts.contains(&"ResourceSet/apps — Flux ResourceSet"),
        "{texts:?}"
    );
    assert!(texts.contains(&"suspended"), "{texts:?}");
    assert!(
        texts.contains(&"owner is suspended — not reconciling"),
        "{texts:?}"
    );
    assert!(!texts.contains(&"Source"), "{texts:?}");
    assert!(!texts.contains(&"no source resolved"), "{texts:?}");
    assert!(!texts.contains(&"Depends on"), "{texts:?}");
    assert!(
        !texts.iter().any(|t| t.starts_with("waiting on dependency")),
        "{texts:?}"
    );
    assert!(
        texts.contains(&"selector team=web,env in (dev,prod)"),
        "{texts:?}"
    );
    assert!(texts.contains(&"Managed resources"), "{texts:?}");
    assert!(
        app.gitops_items
            .iter()
            .any(|f| f.target.as_ref().is_some_and(|t| t.plural == "services"))
    );
    let provider = app
        .gitops_items
        .iter()
        .position(|f| f.text == "ResourceSetInputProvider/prs")
        .unwrap();
    app.gitops_state.select(Some(provider));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.kind_plural, "resourcesetinputproviders");
    assert_eq!(app.namespace, "default");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=prs"));
}

#[tokio::test]
async fn enter_on_a_resourceset_from_another_group_does_not_open_gitops() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.com", "ResourceSet", "resourcesets", true);
    app.switch_kind("resourcesets");
    apply(
        &mut app,
        json!({
            "apiVersion": "example.com/v1", "kind": "ResourceSet",
            "metadata": {"name": "apps", "namespace": "default"}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_ne!(app.mode, Mode::Gitops);
}

#[tokio::test]
async fn gitops_follows_flux_operator_labels_to_the_owner() {
    for (kind, plural, name, labels) in [
        (
            "ResourceSet",
            "resourcesets",
            "apps",
            json!({
                "resourceset.fluxcd.controlplane.io/name": "apps",
                "resourceset.fluxcd.controlplane.io/namespace": "flux-system"
            }),
        ),
        (
            "FluxInstance",
            "fluxinstances",
            "flux",
            json!({
                "app.kubernetes.io/managed-by": "flux-operator",
                "fluxcd.controlplane.io/name": "flux",
                "fluxcd.controlplane.io/namespace": "flux-system"
            }),
        ),
    ] {
        let deployment = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default", "labels": labels}
        });
        let owner = json!({
            "apiVersion": "fluxcd.controlplane.io/v1", "kind": kind,
            "metadata": {"name": name, "namespace": "flux-system"},
            "spec": {"inputsFrom": [{"kind": "ResourceSetInputProvider", "name": "prs"}]},
            "status": {
                "conditions": [{"type": "Ready", "status": "False", "reason": "BuildFailed", "message": "bad template"}]
            }
        });
        let (mut app, rx) = test_app();
        app.cluster
            .register_kind("fluxcd.controlplane.io", kind, plural, true);
        // Another group registered later takes the bare plural.
        app.cluster.register_kind("example.com", kind, plural, true);
        let (mut app, mut rx, responses, requests) =
            health_report_app_with(app, rx, "deployments", deployment.clone());
        responses.lock().unwrap().extend([
            (
                "/apis/apps/v1/namespaces/default/deployments/web".to_string(),
                (200, deployment),
            ),
            (
                format!("/apis/fluxcd.controlplane.io/v1/namespaces/flux-system/{plural}/{name}"),
                (200, owner),
            ),
        ]);
        open_health_report_key(&mut app, true);
        receive_health_report(&mut app, &mut rx, true).await;
        let texts: Vec<_> = app.gitops_items.iter().map(|f| f.text.as_str()).collect();
        let managed = format!("Deployment/web is managed by {kind}/{name}");
        assert!(texts.contains(&managed.as_str()), "{texts:?}");
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.starts_with("/apis/example.com")),
            "{plural} owner read from the wrong group"
        );
        // Only a ResourceSet reads input providers.
        assert_eq!(
            texts.contains(&"ResourceSetInputProvider/prs"),
            kind == "ResourceSet",
            "{texts:?}"
        );
        assert!(!texts.contains(&"Source"), "{texts:?}");
        assert!(
            texts.contains(&"not ready (BuildFailed) — bad template"),
            "{texts:?}"
        );
        let owner_line = format!("{kind}/{name}");
        let owner = app
            .gitops_items
            .iter()
            .position(|f| f.text == owner_line)
            .unwrap();
        app.gitops_state.select(Some(owner));
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.kind_plural, plural);
        assert_eq!(
            app.kind.as_ref().unwrap().ar.group,
            "fluxcd.controlplane.io"
        );
        assert_eq!(app.namespace, "flux-system");
    }
}

#[tokio::test]
async fn gitops_on_another_groups_resourceset_is_not_a_flux_owner() {
    for (kind, plural) in [
        ("ResourceSet", "resourcesets"),
        ("FluxInstance", "fluxinstances"),
    ] {
        let object = json!({
            "apiVersion": "example.com/v1", "kind": kind,
            "metadata": {"name": "apps", "namespace": "default"}
        });
        let (mut app, rx) = test_app();
        app.cluster.register_kind("example.com", kind, plural, true);
        let (mut app, mut rx, responses, _) =
            health_report_app_with(app, rx, plural, object.clone());
        responses.lock().unwrap().insert(
            format!("/apis/example.com/v1/namespaces/default/{plural}/apps"),
            (200, object),
        );
        open_health_report_key(&mut app, true);
        receive_health_report(&mut app, &mut rx, true).await;
        let texts: Vec<_> = app.gitops_items.iter().map(|f| f.text.as_str()).collect();
        let unmanaged = format!("{kind}/apps is not managed by Flux");
        assert!(texts.contains(&unmanaged.as_str()), "{texts:?}");
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
        ("resourcesets", "ResourceSet", "fluxcd.controlplane.io"),
        (
            "resourcesetinputproviders",
            "ResourceSetInputProvider",
            "fluxcd.controlplane.io",
        ),
        ("fluxinstances", "FluxInstance", "fluxcd.controlplane.io"),
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
async fn force_reconcile_menu_is_limited_to_kinds_that_honour_force_at() {
    for (plural, group, kind, force) in [
        (
            "helmreleases",
            "helm.toolkit.fluxcd.io",
            "HelmRelease",
            true,
        ),
        (
            "resourcesetinputproviders",
            "fluxcd.controlplane.io",
            "ResourceSetInputProvider",
            true,
        ),
        (
            "fluxinstances",
            "fluxcd.controlplane.io",
            "FluxInstance",
            true,
        ),
        (
            "resourcesets",
            "fluxcd.controlplane.io",
            "ResourceSet",
            false,
        ),
        ("helmcharts", "source.toolkit.fluxcd.io", "HelmChart", false),
        (
            "kustomizations",
            "kustomize.toolkit.fluxcd.io",
            "Kustomization",
            false,
        ),
        (
            "gitrepositories",
            "source.toolkit.fluxcd.io",
            "GitRepository",
            false,
        ),
        ("cronjobs", "batch", "CronJob", false),
        ("applications", "argoproj.io", "Application", false),
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
            force,
            "{plural}"
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
        for (id, target_plural, group, namespace, name) in [
            ("default_web__Service", "services", "", "default", "web"),
            (
                "default_web_serving.knative.dev_Service",
                "services.serving.knative.dev",
                "serving.knative.dev",
                "default",
                "web",
            ),
            ("_web__Namespace", "namespaces", "", "", "web"),
            (
                "default_system__controller__web_rbac.authorization.k8s.io_Role",
                "roles.rbac.authorization.k8s.io",
                "rbac.authorization.k8s.io",
                "default",
                "system:controller:web",
            ),
            (
                "default_system__web_rbac.authorization.k8s.io_RoleBinding",
                "rolebindings.rbac.authorization.k8s.io",
                "rbac.authorization.k8s.io",
                "default",
                "system:web",
            ),
            (
                "_system__controller__web_rbac.authorization.k8s.io_ClusterRole",
                "clusterroles.rbac.authorization.k8s.io",
                "rbac.authorization.k8s.io",
                "",
                "system:controller:web",
            ),
            (
                "_system__web_rbac.authorization.k8s.io_ClusterRoleBinding",
                "clusterrolebindings.rbac.authorization.k8s.io",
                "rbac.authorization.k8s.io",
                "",
                "system:web",
            ),
        ] {
            let root = json!({"apiVersion":"test/v1", "kind":owner_kind,
                "metadata":{"name":"web", "namespace":"default"},
                "status":{"inventory":{"entries":[{"id":id,"v":"v1"}]}}});
            let (mut app, mut rx, responses, requests) = health_report_app(plural, root.clone());
            app.cluster
                .register_kind("serving.knative.dev", "Service", "services", true);
            app.cluster.register_kind("", "Service", "services", true);
            for (kind, plural, namespaced) in [
                ("Role", "roles", true),
                ("RoleBinding", "rolebindings", true),
                ("ClusterRole", "clusterroles", false),
                ("ClusterRoleBinding", "clusterrolebindings", false),
            ] {
                app.cluster
                    .register_kind("rbac.authorization.k8s.io", kind, plural, namespaced);
            }
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
            assert!(app.gitops_items[index].text.contains(&format!("/{name} (")));
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
            assert_eq!(app.fields, Some(format!("metadata.name={name}")));
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
