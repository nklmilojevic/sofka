use super::*;
use http_body_util::BodyExt;

const GROUP: &str = "infra.contrib.fluxcd.io";

fn terraform_app() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.cluster
        .register_discovered_kind(GROUP, "v1alpha2", "Terraform", "terraforms", &["tf"]);
    app.cluster.register_kind(
        "source.toolkit.fluxcd.io",
        "GitRepository",
        "gitrepositories",
        true,
    );
    (app, rx)
}

fn terraform(name: &str) -> Value {
    json!({
        "apiVersion": "infra.contrib.fluxcd.io/v1alpha2", "kind": "Terraform",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"suspend": false, "sourceRef": {"kind": "GitRepository", "name": "infra"}}
    })
}

fn planned(name: &str, plan: &str, destroy: bool) -> Value {
    let mut tf = terraform(name);
    tf["status"] = json!({
        "lastAttemptedRevision": "main@sha1:def",
        "plan": {"pending": plan, "isDestroyPlan": destroy},
        "conditions": [{
            "type": "Ready", "status": "Unknown", "reason": "TerraformPlannedWithChanges",
            "message": format!("Plan generated: set approvePlan: \"{plan}\" to approve this plan.")
        }]
    });
    tf
}

fn applied(name: &str) -> Value {
    let mut tf = terraform(name);
    tf["status"] = json!({
        "lastAppliedRevision": "main@sha1:abc",
        "lastAttemptedRevision": "main@sha1:abc",
        "plan": {"lastApplied": "plan-main-abc", "isDestroyPlan": true},
        "inventory": {"entries": [
            {"n": "vpc", "t": "aws_vpc", "id": "vpc-0123"}
        ]},
        "conditions": [{
            "type": "Ready", "status": "True", "reason": "TerraformAppliedSucceed",
            "message": "Applied successfully: main@sha1:abc"
        }]
    });
    tf
}

fn open_tf(app: &mut App) {
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    type_in_palette(app, "tf");
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "terraforms");
    assert_eq!(app.kind.as_ref().unwrap().ar.group, GROUP);
}

#[tokio::test]
async fn tf_lists_terraforms_with_revision_and_pending_plan() {
    let (mut app, _rx) = terraform_app();
    open_tf(&mut app);
    apply(&mut app, applied("a-applied"));
    apply(&mut app, planned("b-planned", "plan-main-def", false));
    apply(&mut app, planned("c-destroy", "plan-main-0a1", true));
    assert_eq!(
        app.display_headers().to_vec(),
        [
            "NAME",
            "READY",
            "SUSPENDED",
            "MESSAGE",
            "REVISION",
            "PLAN",
            "AGE"
        ]
    );
    let cells: Vec<(String, String, String, String, String)> = {
        let rows = app.rows();
        app.ensure_table_cell_cache(&rows);
        let cache = app.table_cell_cache();
        rows.iter()
            .map(|row| {
                let (cells, status_idx) = cache.get(&row_key(row)).unwrap();
                assert_eq!(status_idx, Some(1));
                (
                    cells[0].to_string(),
                    cells[1].to_string(),
                    cells[2].to_string(),
                    cells[4].to_string(),
                    cells[5].to_string(),
                )
            })
            .collect()
    };
    let cell = |a: &str, b: &str, c: &str, d: &str, e: &str| {
        (a.into(), b.into(), c.into(), d.into(), e.into())
    };
    assert_eq!(
        cells,
        [
            cell("a-applied", "True", "false", "main@sha1:abc", ""),
            cell("b-planned", "Unknown", "false", "", "plan-main-def"),
            cell(
                "c-destroy",
                "Unknown",
                "false",
                "",
                "plan-main-0a1 (destroy)"
            ),
        ]
    );
    let screen = screen_text(&mut app, 200, 20);
    assert!(screen.contains("plan-main-0a1 (destroy)"), "{screen}");
}

#[tokio::test]
async fn ctrl_z_keeps_terraforms_that_wait_for_plan_approval() {
    let (mut app, _rx) = terraform_app();
    open_tf(&mut app);
    apply(&mut app, applied("applied"));
    apply(&mut app, planned("planned", "plan-main-def", false));
    let mut failed = applied("failed");
    failed["status"]["conditions"][0]["status"] = json!("False");
    apply(&mut app, failed);
    let mut suspended = applied("suspended");
    suspended["spec"]["suspend"] = json!(true);
    apply(&mut app, suspended);
    app.handle_key(ctrl(KeyCode::Char('z'))).unwrap();
    assert!(app.faults_filter_active());
    let mut names = row_names(&app);
    names.sort();
    assert_eq!(names, ["failed", "planned", "suspended"]);
}

#[tokio::test]
async fn terraform_menu_patches_selected_and_marked_terraforms() {
    for action in ["Suspend", "Resume", "Reconcile now"] {
        let (mut app, mut rx) = terraform_app();
        open_tf(&mut app);
        apply(&mut app, terraform("network"));
        apply(&mut app, terraform("vpc"));
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
                            terraform("network").to_string(),
                        )),
                    ))
                }
            }),
            "default",
        );
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        app.handle_key(press(KeyCode::Down)).unwrap();
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        assert_eq!(app.marked.len(), 2);

        app.handle_key(press(KeyCode::Char('t'))).unwrap();
        assert_eq!(app.mode, Mode::FluxMenu);
        assert_eq!(app.action_menu_items(), FLUX_MENU_ITEMS);
        let index = FLUX_MENU_ITEMS.iter().position(|s| *s == action).unwrap();
        for _ in 0..index {
            app.handle_key(press(KeyCode::Char('j'))).unwrap();
        }
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.marked.is_empty());

        let mut paths = Vec::new();
        for _ in 0..2 {
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
        let prefix = "/apis/infra.contrib.fluxcd.io/v1alpha2/namespaces/default/terraforms";
        assert_eq!(
            paths,
            [format!("{prefix}/network"), format!("{prefix}/vpc")]
        );
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
        assert_eq!(app.flash, format!("{verb} 2 terraforms"));
    }
}

#[tokio::test]
async fn terraform_menu_needs_the_tofu_controller_group_and_write_access() {
    for group in [GROUP, "example.com"] {
        let (mut app, _rx) = test_app();
        app.cluster
            .register_kind(group, "Terraform", "terraforms", true);
        app.switch_kind(&format!("terraforms.{group}"));
        apply(
            &mut app,
            json!({
                "apiVersion": format!("{group}/v1"), "kind": "Terraform",
                "metadata": {"name": "vpc", "namespace": "default"}
            }),
        );
        app.handle_key(press(KeyCode::Char('t'))).unwrap();
        if group == GROUP {
            assert_eq!(app.mode, Mode::FluxMenu);
            app.handle_key(press(KeyCode::Esc)).unwrap();
            app.readonly = true;
            app.handle_key(press(KeyCode::Char('t'))).unwrap();
            assert!(app.flash_err);
        } else {
            assert!(app.flash.starts_with("suspend/resume only applies to"));
        }
        assert_eq!(app.mode, Mode::Table);
    }
}

fn gitops_texts(app: &App) -> Vec<&str> {
    app.gitops_items.iter().map(|f| f.text.as_str()).collect()
}

#[tokio::test]
async fn gitops_on_a_terraform_follows_its_own_source_and_terraform_dependencies() {
    for owner in [None, Some("Kustomization"), Some("ResourceSet")] {
        let mut vpc = planned("vpc", "plan-main-def", false);
        vpc["spec"]["dependsOn"] = json!([{"name": "network"}]);
        vpc["status"]["inventory"] = json!({"entries": [
            {"n": "subnet", "t": "aws_subnet", "id": "subnet-0123"}
        ]});
        match owner {
            Some("Kustomization") => {
                vpc["metadata"]["labels"] = json!({
                    "kustomize.toolkit.fluxcd.io/name": "apps",
                    "kustomize.toolkit.fluxcd.io/namespace": "flux-system"
                });
            }
            Some(_) => {
                vpc["metadata"]["labels"] = json!({
                    "resourceset.fluxcd.controlplane.io/name": "apps",
                    "resourceset.fluxcd.controlplane.io/namespace": "flux-system"
                });
            }
            None => {}
        }
        let (mut app, rx) = terraform_app();
        app.cluster.register_kind(
            "fluxcd.controlplane.io",
            "ResourceSet",
            "resourcesets",
            true,
        );
        let (mut app, mut rx, responses, requests) =
            health_report_app_with(app, rx, "terraforms", vpc.clone());
        let tf = "/apis/infra.contrib.fluxcd.io/v1alpha2/namespaces/default/terraforms";
        responses.lock().unwrap().extend([
            (format!("{tf}/vpc"), (200, vpc)),
            (format!("{tf}/network"), (200, applied("network"))),
            (
                "/apis/source.toolkit.fluxcd.io/v1/namespaces/default/gitrepositories/infra"
                    .to_string(),
                (
                    200,
                    json!({
                        "apiVersion": "source.toolkit.fluxcd.io/v1", "kind": "GitRepository",
                        "metadata": {"name": "infra", "namespace": "default"},
                        "status": {
                            "artifact": {"revision": "main@sha1:def"},
                            "conditions": [{"type": "Ready", "status": "True"}]
                        }
                    }),
                ),
            ),
            (
                "/apis/kustomize.toolkit.fluxcd.io/v1/namespaces/flux-system/kustomizations/apps"
                    .to_string(),
                (
                    200,
                    json!({
                        "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
                        "metadata": {"name": "apps", "namespace": "flux-system"},
                        "spec": {"sourceRef": {"kind": "GitRepository", "name": "flux-system"}},
                        "status": {"conditions": [{"type": "Ready", "status": "True"}]}
                    }),
                ),
            ),
            (
                "/apis/fluxcd.controlplane.io/v1/namespaces/flux-system/resourcesets/apps"
                    .to_string(),
                (
                    200,
                    json!({
                        "apiVersion": "fluxcd.controlplane.io/v1", "kind": "ResourceSet",
                        "metadata": {"name": "apps", "namespace": "flux-system"},
                        "status": {"conditions": [{"type": "Ready", "status": "True"}]}
                    }),
                ),
            ),
        ]);
        open_health_report_key(&mut app, true);
        receive_health_report(&mut app, &mut rx, true).await;
        let texts = gitops_texts(&app);

        let headline = match owner {
            Some(kind) => format!("Terraform/vpc is managed by {kind}/apps"),
            None => "Terraform/vpc — Flux Terraform".to_string(),
        };
        assert_eq!(texts[0], headline, "{texts:?}");
        // The headline follows the Terraform waiting for approval, not its
        // healthy owner.
        assert_eq!(app.gitops_items[0].level, crate::explain::Level::Warn);
        if let Some(kind) = owner {
            let owner_line = format!("{kind}/apps");
            assert!(texts.contains(&owner_line.as_str()), "{texts:?}");
        }
        assert!(texts.contains(&"Terraform/vpc"), "{texts:?}");
        let source = texts.iter().position(|t| *t == "Source").unwrap();
        assert_eq!(texts[source + 1], "GitRepository/infra", "{texts:?}");
        assert!(!texts.contains(&"GitRepository/flux-system"), "{texts:?}");
        assert!(texts.contains(&"network — ready"), "{texts:?}");
        assert!(
            texts.contains(
                &"not ready (TerraformPlannedWithChanges) — Plan generated: set approvePlan: \"plan-main-def\" to approve this plan."
            ),
            "{texts:?}"
        );
        assert!(!texts.contains(&"Managed resources"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("invalid inventory")),
            "{texts:?}"
        );
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.ends_with("/kustomizations/network")),
            "dependency read as a Kustomization"
        );

        let dep = app
            .gitops_items
            .iter()
            .position(|f| f.text == "network — ready")
            .unwrap();
        app.gitops_state.select(Some(dep));
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.kind_plural, "terraforms");
        assert_eq!(app.kind.as_ref().unwrap().ar.group, GROUP);
    }
}
