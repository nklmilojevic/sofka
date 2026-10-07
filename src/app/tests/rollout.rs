use super::*;
use http_body_util::BodyExt;

type Requests = mpsc::UnboundedReceiver<(String, String, Value)>;

fn template(app_label: &str, image: &str, hash: Option<&str>) -> Value {
    let mut labels = json!({"app": app_label});
    if let Some(hash) = hash {
        labels["pod-template-hash"] = json!(hash);
    }
    json!({
        "metadata": {"labels": labels},
        "spec": {"containers": [{"name": app_label, "image": image}]},
    })
}

fn deployment(labels: Value, paused: bool, image: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "default", "uid": "web-uid", "labels": labels},
        "spec": {
            "paused": paused,
            "selector": {"matchLabels": {"app": "web"}},
            "template": template("web", image, None),
        },
    })
}

fn replicaset(owner: &str, uid: &str, revision: i64, image: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "ReplicaSet",
        "metadata": {
            "name": format!("{owner}-{revision}"),
            "namespace": "default",
            "uid": format!("{owner}-rs-{revision}"),
            "resourceVersion": revision.to_string(),
            "creationTimestamp": "2026-01-01T00:00:00Z",
            "annotations": {
                "deployment.kubernetes.io/revision": revision.to_string(),
                "kubernetes.io/change-cause": format!("image {image}"),
            },
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment",
                "name": owner, "uid": uid, "controller": true}],
        },
        "spec": {"template": template("web", image, Some("hash"))},
    })
}

/// A Deployments view on `web` with three revisions (images `web:1` to
/// `web:3`, revision 3 in effect) and a ReplicaSet of another Deployment.
fn deployment_app(labels: Value) -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.cluster
        .register_kind("apps", "ReplicaSet", "replicasets", true);
    app.switch_kind("deployments");
    apply(&mut app, deployment(labels, false, "web:3"));
    (app, rx)
}

fn open_history(app: &mut App) {
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    type_in_palette(app, "rollout-history");
    app.handle_key(press(KeyCode::Enter)).unwrap();
}

fn apply_revisions(app: &mut App) {
    for revision in [1, 3, 2] {
        apply(
            app,
            replicaset("web", "web-uid", revision, &format!("web:{revision}")),
        );
    }
    apply(app, replicaset("api", "api-uid", 9, "api:9"));
}

fn select_revision(app: &mut App, revision: i64) {
    let idx = app
        .rows()
        .iter()
        .position(|o| crate::rollout::revision(o) == Some(revision))
        .unwrap();
    app.table_state.select(Some(idx));
}

/// Answers GET with `live` and records every request, PATCH bodies included.
fn mock_workload_api(app: &mut App, live: Value) -> Requests {
    let (requests, received) = mpsc::unbounded_channel();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let requests = requests.clone();
            let live = live.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                if parts.method == http::Method::PATCH {
                    assert_eq!(
                        parts.headers["content-type"],
                        "application/strategic-merge-patch+json"
                    );
                }
                requests
                    .send((parts.method.to_string(), parts.uri.path().to_string(), body))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(200)
                        .body(http_body_util::Full::new(hyper::body::Bytes::from(
                            live.to_string(),
                        )))
                        .unwrap(),
                )
            }
        }),
        "default",
    );
    received
}

async fn next_flash(rx: &mut Receiver<Msg>) -> (String, bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Msg::Flash { message, err, .. }) = rx.recv().await {
                return (message, err);
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn rollout_history_lists_own_revisions_newest_first() {
    let (mut app, _rx) = deployment_app(json!({}));
    open_history(&mut app);
    assert_eq!(app.kind_plural, "rollouthistory");
    assert_eq!(app.resource_title(), "rollout history");
    assert_eq!(app.scope_label.as_deref(), Some("deploy/web"));
    assert_eq!(app.labels.as_deref(), Some("app=web"));
    apply_revisions(&mut app);

    let revisions: Vec<_> = app
        .rows()
        .iter()
        .map(|o| crate::rollout::revision(o).unwrap())
        .collect();
    assert_eq!(revisions, vec![3, 2, 1]);
    let status = app.spec.header_index("STATUS").unwrap();
    let images = app.spec.header_index("IMAGES").unwrap();
    let cause = app.spec.header_index("CHANGE-CAUSE").unwrap();
    let rows = app.rows();
    let statuses: Vec<_> = rows
        .iter()
        .map(|o| app.live_cell(o, status).unwrap())
        .collect();
    assert_eq!(statuses, vec!["deployed", "superseded", "superseded"]);
    let (cells, _) = app.spec.cells(rows[1], crate::columns::now_secs());
    assert_eq!(cells[images], "web:2");
    assert_eq!(cells[cause], "image web:2");
    drop(rows);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
}

#[tokio::test]
async fn rollout_history_rejects_other_kinds() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p", "namespace": "default"}}),
    );
    open_history(&mut app);
    assert_eq!(app.kind_plural, "pods");
    assert!(
        app.flash.contains("rollout history applies to"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn enter_on_a_revision_diffs_the_current_template_against_it() {
    let (mut app, _rx) = deployment_app(json!({}));
    open_history(&mut app);
    apply_revisions(&mut app);
    select_revision(&mut app, 1);
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Diff);
    assert!(
        app.detail.title.contains("revision 3 → 1"),
        "{}",
        app.detail.title
    );
    let lines: Vec<&str> = app.detail.lines.iter().map(String::as_str).collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with('-') && l.contains("web:3"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with('+') && l.contains("web:1"))
    );
    assert!(!lines.iter().any(|l| l.contains("pod-template-hash")));

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    select_revision(&mut app, 3);
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(
        app.flash.contains("revision 3 is the current revision"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn rollback_patches_the_deployment_with_the_revision_template() {
    let (mut app, mut rx) = deployment_app(json!({}));
    open_history(&mut app);
    apply_revisions(&mut app);
    let mut requests = mock_workload_api(&mut app, deployment(json!({}), false, "web:3"));
    select_revision(&mut app, 1);

    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert_eq!(
        app.confirm_label,
        "Roll back deploy/web in default to revision 1?"
    );
    app.handle_key(press(KeyCode::Char('y'))).unwrap();

    let (message, err) = next_flash(&mut rx).await;
    assert!(!err, "{message}");
    assert_eq!(message, "rolled back web to revision 1");
    let (method, path, _) = requests.recv().await.unwrap();
    assert_eq!(method, "GET");
    assert_eq!(path, "/apis/apps/v1/namespaces/default/deployments/web");
    let (method, path, body) = requests.recv().await.unwrap();
    assert_eq!(method, "PATCH");
    assert_eq!(path, "/apis/apps/v1/namespaces/default/deployments/web");
    assert_eq!(body["spec"]["template"]["$patch"], "replace");
    assert_eq!(
        body["spec"]["template"]["spec"]["containers"][0]["image"],
        "web:1"
    );
    assert_eq!(
        body["spec"]["template"]["metadata"]["labels"],
        json!({"app": "web"})
    );
    assert_eq!(
        body["metadata"]["annotations"],
        json!({"kubernetes.io/change-cause": "image web:1"})
    );
}

#[tokio::test]
async fn rollback_of_a_paused_deployment_is_refused_without_a_patch() {
    let (mut app, mut rx) = deployment_app(json!({}));
    open_history(&mut app);
    apply_revisions(&mut app);
    let mut requests = mock_workload_api(&mut app, deployment(json!({}), true, "web:3"));
    select_revision(&mut app, 2);
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    app.handle_key(press(KeyCode::Char('y'))).unwrap();

    let (message, err) = next_flash(&mut rx).await;
    assert!(err);
    assert!(message.contains("paused"), "{message}");
    let (method, _, _) = requests.recv().await.unwrap();
    assert_eq!(method, "GET");
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn rollback_to_the_current_revision_is_refused() {
    let (mut app, _rx) = deployment_app(json!({}));
    open_history(&mut app);
    apply_revisions(&mut app);
    select_revision(&mut app, 3);
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.confirm_action.is_none());
    assert!(
        app.flash.contains("already the current revision"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn rollback_warns_when_gitops_will_revert_it() {
    let (mut app, _rx) = deployment_app(json!({
        "kustomize.toolkit.fluxcd.io/name": "apps",
        "kustomize.toolkit.fluxcd.io/namespace": "flux-system",
    }));
    open_history(&mut app);
    apply_revisions(&mut app);
    select_revision(&mut app, 1);
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label
            .starts_with("⚠ Managed by Flux Kustomization/apps — the rollback will be reverted"),
        "{}",
        app.confirm_label
    );

    let mut argo = deployment(json!({}), false, "web:3");
    argo["metadata"]["annotations"] =
        json!({"argocd.argoproj.io/tracking-id": "guestbook:apps/Deployment:default/web"});
    for (labels, warned) in [
        (json!({"argocd.argoproj.io/instance": "guestbook"}), true),
        (json!({"app.kubernetes.io/instance": "guestbook"}), false),
    ] {
        let (mut app, _rx) = deployment_app(labels);
        open_history(&mut app);
        apply_revisions(&mut app);
        select_revision(&mut app, 1);
        app.handle_key(press(KeyCode::Char('r'))).unwrap();
        assert_eq!(
            app.confirm_label
                .contains("Managed by Argo CD Application guestbook"),
            warned,
            "{}",
            app.confirm_label
        );
    }

    let (mut app, _rx) = deployment_app(json!({}));
    apply(&mut app, argo);
    open_history(&mut app);
    apply_revisions(&mut app);
    select_revision(&mut app, 1);
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(
        app.confirm_label
            .contains("Managed by Argo CD Application guestbook"),
        "{}",
        app.confirm_label
    );
}

#[tokio::test]
async fn rollback_respects_read_only_and_guardrails() {
    let (mut app, _rx) = deployment_app(json!({}));
    open_history(&mut app);
    apply_revisions(&mut app);
    select_revision(&mut app, 1);
    app.readonly = true;
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(app.confirm_action.is_none());
    assert!(app.flash.contains("read-only"), "{}", app.flash);

    app.readonly = false;
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["rollback".into()],
        resources: vec!["deployments".into()],
        deny: true,
        reason: Some("roll back through Git".into()),
        ..Default::default()
    }];
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(app.confirm_action.is_none());
    assert!(
        app.flash.contains("blocked by guardrail") && app.flash.contains("roll back through Git"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn statefulset_rollback_applies_the_controller_revision_data() {
    let (mut app, mut rx) = test_app();
    app.cluster
        .register_kind("apps", "StatefulSet", "statefulsets", true);
    app.cluster
        .register_kind("apps", "ControllerRevision", "controllerrevisions", true);
    app.switch_kind("statefulsets");
    let sts = json!({
        "apiVersion": "apps/v1", "kind": "StatefulSet",
        "metadata": {"name": "db", "namespace": "default", "uid": "db-uid"},
        "spec": {
            "selector": {"matchLabels": {"app": "db"}},
            "template": template("db", "db:2", None),
        },
    });
    apply(&mut app, sts.clone());
    open_history(&mut app);
    assert_eq!(app.scope_label.as_deref(), Some("sts/db"));
    assert_eq!(app.kind.as_ref().unwrap().ar.plural, "controllerrevisions");
    let revision = |n: i64| {
        let mut data_template = template("db", &format!("db:{n}"), None);
        data_template["$patch"] = json!("replace");
        json!({
            "apiVersion": "apps/v1", "kind": "ControllerRevision",
            "metadata": {
                "name": format!("db-{n}"), "namespace": "default",
                "uid": format!("db-cr-{n}"),
                "ownerReferences": [{"apiVersion": "apps/v1", "kind": "StatefulSet",
                    "name": "db", "uid": "db-uid", "controller": true}],
            },
            "revision": n,
            "data": {"spec": {"template": data_template}},
        })
    };
    apply(&mut app, revision(1));
    apply(&mut app, revision(2));
    let mut requests = mock_workload_api(&mut app, sts);
    select_revision(&mut app, 1);

    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(
        app.confirm_label,
        "Roll back sts/db in default to revision 1?"
    );
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    let (message, err) = next_flash(&mut rx).await;
    assert!(!err, "{message}");
    requests.recv().await.unwrap();
    let (method, path, body) = requests.recv().await.unwrap();
    assert_eq!(method, "PATCH");
    assert_eq!(path, "/apis/apps/v1/namespaces/default/statefulsets/db");
    assert_eq!(body, revision(1)["data"]);
}
