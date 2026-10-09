use super::*;
use crate::columns::now_secs;

fn workloads_app() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.cluster
        .register_kind("apps", "StatefulSet", "statefulsets", true);
    app.cluster
        .register_kind("apps", "DaemonSet", "daemonsets", true);
    app.namespace = "default".into();
    type_resource_query(&mut app, "wk");
    assert!(app.workloads_active());
    (app, rx)
}

/// Feed one object through the path a workloads watch takes: tagged with
/// its kind's partition, keyed by plural.
fn apply_workload(app: &mut App, value: Value) {
    let o = obj(value);
    let partition = crate::app::workloads::group_plural(&o)
        .unwrap()
        .1
        .to_string();
    let key = crate::app::workloads::key(&o);
    app.handle_msg(Msg::NamespaceWatch {
        generation: app.generation,
        namespace: partition,
        event: Box::new(Msg::Applied {
            generation: app.generation,
            key,
            obj: Box::new(o),
        }),
    });
}

fn deployment(name: &str, ready: i64) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name, "namespace": "default", "generation": 1,
                     "uid": format!("deploy-{name}")},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}},
        "status": {"observedGeneration": 1, "replicas": 2, "updatedReplicas": 2,
                   "readyReplicas": ready}
    })
}

fn pod(name: &str, owner: Option<(&str, &str)>) -> Value {
    let mut pod = json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "default", "uid": format!("pod-{name}")},
        "spec": {"containers": [{"name": "app"}]},
        "status": {
            "phase": "Running",
            "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [{"name": "app", "ready": true, "restartCount": 0,
                                   "state": {"running": {}}}]
        }
    });
    if let Some((kind, owner)) = owner {
        pod["metadata"]["ownerReferences"] = json!([{
            "apiVersion": "apps/v1", "kind": kind, "name": owner,
            "uid": format!("uid-{owner}"), "controller": true
        }]);
    }
    pod
}

fn job(name: &str, cronjob: Option<&str>) -> Value {
    let mut job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"completions": 1},
        "status": {"succeeded": 1,
                   "conditions": [{"type": "Complete", "status": "True"}]}
    });
    if let Some(cronjob) = cronjob {
        job["metadata"]["ownerReferences"] = json!([{
            "apiVersion": "batch/v1", "kind": "CronJob", "name": cronjob,
            "uid": format!("uid-{cronjob}"), "controller": true
        }]);
    }
    job
}

fn seed(app: &mut App) {
    apply_workload(app, deployment("web", 2));
    apply_workload(app, deployment("api", 0));
    apply_workload(
        app,
        json!({"apiVersion": "apps/v1", "kind": "StatefulSet",
               "metadata": {"name": "db", "namespace": "default", "generation": 1},
               "spec": {"replicas": 1},
               "status": {"observedGeneration": 1, "replicas": 1, "updatedReplicas": 1,
                          "readyReplicas": 1}}),
    );
    apply_workload(
        app,
        json!({"apiVersion": "apps/v1", "kind": "DaemonSet",
               "metadata": {"name": "agent", "namespace": "default", "generation": 1},
               "status": {"observedGeneration": 1, "desiredNumberScheduled": 3,
                          "currentNumberScheduled": 3, "updatedNumberScheduled": 3,
                          "numberReady": 3}}),
    );
    apply_workload(
        app,
        json!({"apiVersion": "batch/v1", "kind": "CronJob",
               "metadata": {"name": "nightly", "namespace": "default"},
               "spec": {"schedule": "0 0 * * *"},
               "status": {"lastScheduleTime": "2026-10-09T00:00:00Z",
                          "lastSuccessfulTime": "2026-10-09T00:01:00Z"}}),
    );
    apply_workload(app, job("nightly-1", Some("nightly")));
    apply_workload(app, job("migrate", None));
    apply_workload(app, pod("web-abc", Some(("ReplicaSet", "web-5d4"))));
    apply_workload(app, pod("web", None));
}

fn rows(app: &App) -> Vec<(String, String)> {
    app.rows()
        .iter()
        .map(|o| {
            (
                o.types.as_ref().unwrap().kind.clone(),
                o.metadata.name.clone().unwrap_or_default(),
            )
        })
        .collect()
}

fn has(rows: &[(String, String)], kind: &str, name: &str) -> bool {
    rows.iter().any(|(k, n)| k == kind && n == name)
}

#[tokio::test]
async fn workloads_view_lists_top_level_workloads_and_toggles_owned_rows() {
    let (mut app, _rx) = workloads_app();
    assert_eq!(app.list_title(), "workloads");
    assert_eq!(
        app.display_headers()[..5],
        ["NAME", "KIND", "READY", "STATUS", "AGE"]
    );
    seed(&mut app);

    let listed = rows(&app);
    assert_eq!(listed.len(), 7, "{listed:?}");
    for (kind, name) in [
        ("Deployment", "web"),
        ("Deployment", "api"),
        ("StatefulSet", "db"),
        ("DaemonSet", "agent"),
        ("CronJob", "nightly"),
        ("Job", "migrate"),
        ("Pod", "web"),
    ] {
        assert!(
            has(&listed, kind, name),
            "{kind}/{name} missing: {listed:?}"
        );
    }
    assert!(!has(&listed, "Pod", "web-abc"));
    assert!(!has(&listed, "Job", "nightly-1"));

    let row = |app: &App, kind: &str, name: &str| {
        app.rows()
            .iter()
            .position(|o| {
                o.types.as_ref().unwrap().kind == kind && o.metadata.name.as_deref() == Some(name)
            })
            .unwrap()
    };
    let cells = |app: &App, index: usize| app.spec.cells(app.rows()[index], now_secs()).0;
    let cells = cells(&app, row(&app, "Deployment", "api"));
    assert_eq!(cells[1..4], ["Deployment", "0/2", "Unavailable"]);
    let cells_at = |app: &App, index: usize| app.spec.cells(app.rows()[index], now_secs()).0;
    assert_eq!(
        cells_at(&app, row(&app, "DaemonSet", "agent"))[1..4],
        ["DaemonSet", "3/3", "Ready"]
    );
    assert_eq!(
        cells_at(&app, row(&app, "CronJob", "nightly"))[1..4],
        ["CronJob", "", "Scheduled"]
    );
    assert_eq!(
        cells_at(&app, row(&app, "Pod", "web"))[1..4],
        ["Pod", "1/1", "Running"]
    );

    app.handle_key(press(KeyCode::Char('O'))).unwrap();
    let listed = rows(&app);
    assert_eq!(listed.len(), 9);
    assert!(has(&listed, "Pod", "web-abc"));
    assert!(has(&listed, "Job", "nightly-1"));
    app.handle_key(press(KeyCode::Char('O'))).unwrap();
    assert_eq!(rows(&app).len(), 7);
    assert_eq!(app.kind_plural, "workloads");
}

#[tokio::test]
async fn ctrl_z_in_workloads_judges_each_row_by_its_own_kind() {
    let (mut app, _rx) = workloads_app();
    seed(&mut app);
    let mut pending = pod("stuck", None);
    pending["status"] = json!({"phase": "Pending"});
    apply_workload(&mut app, pending);
    let mut failed = job("broken", None);
    failed["status"] = json!({"conditions": [{"type": "Failed", "status": "True"}]});
    apply_workload(&mut app, failed);

    app.handle_key(ctrl(KeyCode::Char('z'))).unwrap();
    assert!(app.faults_filter_active());
    let mut listed = rows(&app);
    listed.sort();
    assert_eq!(
        listed,
        [
            ("Deployment".to_string(), "api".to_string()),
            ("Job".to_string(), "broken".to_string()),
            ("Pod".to_string(), "stuck".to_string()),
        ]
    );
}

#[tokio::test]
async fn enter_drills_from_a_workloads_row_and_esc_returns_to_the_view() {
    let (mut app, _rx) = workloads_app();
    seed(&mut app);
    type_filter(&mut app, "web");
    let index = app
        .rows()
        .iter()
        .position(|o| o.types.as_ref().unwrap().kind == "Deployment")
        .unwrap();
    app.table_state.select(Some(index));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(!app.workloads_active());
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.labels.as_deref(), Some("app=web"));

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.workloads_active());
    assert_eq!(app.kind_plural, "workloads");
    assert_eq!(app.filter, "web");
}

#[tokio::test]
async fn row_actions_use_the_row_kind_and_skip_marks_of_other_kinds() {
    let (mut app, mut rx) = workloads_app();
    seed(&mut app);
    let position = |app: &App, kind: &str, name: &str| {
        app.rows()
            .iter()
            .position(|o| {
                o.types.as_ref().unwrap().kind == kind && o.metadata.name.as_deref() == Some(name)
            })
            .unwrap()
    };
    app.table_state
        .select(Some(position(&app, "Deployment", "web")));
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    app.table_state.select(Some(position(&app, "Pod", "web")));
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    assert_eq!(app.marked.len(), 2, "same name, different kinds");
    app.table_state.select(Some(position(&app, "Pod", "web")));

    let (sent, mut requests) = mpsc::unbounded_channel();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let sent = sent.clone();
            async move {
                sent.send((req.method().clone(), req.uri().path().to_string()))
                    .unwrap();
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(200)
                        .body(kube::client::Body::from(
                            br#"{"kind":"Status","apiVersion":"v1","status":"Success"}"#.to_vec(),
                        ))
                        .unwrap(),
                )
            }
        }),
        "default",
    );

    app.handle_key(ctrl(KeyCode::Char('d'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert_eq!(app.kind_plural, "workloads");
    match &app.confirm_action {
        Some(ConfirmAction::Delete { targets, .. }) => {
            assert_eq!(targets, &[("web".to_string(), "default".to_string())]);
        }
        _ => panic!("expected a delete confirmation"),
    }
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    let (method, path) = requests.recv().await.unwrap();
    assert_eq!(method, http::Method::DELETE);
    assert_eq!(path, "/api/v1/namespaces/default/pods/web");
    while let Ok(msg) = rx.try_recv() {
        app.handle_msg(msg);
    }
    assert_eq!(app.kind_plural, "workloads");

    app.table_state
        .select(Some(position(&app, "Deployment", "web")));
    app.handle_key(press(KeyCode::Char('s'))).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    assert!(
        app.prompt_label.contains("Scale web"),
        "{}",
        app.prompt_label
    );
    assert_eq!(app.kind_plural, "workloads");
}

#[tokio::test]
async fn workloads_rows_of_one_kind_relist_without_touching_the_others() {
    let (mut app, _rx) = workloads_app();
    seed(&mut app);
    let generation = app.generation;
    let event = |event: Msg| Msg::NamespaceWatch {
        generation,
        namespace: "deployments".into(),
        event: Box::new(event),
    };
    app.handle_msg(event(Msg::Reset { generation }));
    let o = obj(deployment("web", 2));
    app.handle_msg(event(Msg::Applied {
        generation,
        key: crate::app::workloads::key(&o),
        obj: Box::new(o),
    }));
    app.handle_msg(event(Msg::Synced { generation }));
    let listed = rows(&app);
    assert!(has(&listed, "Deployment", "web"));
    assert!(!has(&listed, "Deployment", "api"), "relist dropped api");
    assert!(has(&listed, "Pod", "web"));
    assert!(has(&listed, "StatefulSet", "db"));
}

#[tokio::test]
async fn workloads_view_returns_through_view_history() {
    let (mut app, _rx) = workloads_app();
    type_resource_query(&mut app, "pods");
    assert!(!app.workloads_active());
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert!(app.workloads_active());
    assert_eq!(app.kind_plural, "workloads");
    assert_eq!(app.namespace, "default");
}

#[tokio::test]
async fn workloads_watches_stamp_each_kind_on_rows_of_the_same_name() {
    use futures_util::stream;
    use hyper::body::{Bytes, Frame};
    use std::convert::Infallible;

    let service = tower::service_fn(|request: http::Request<kube::client::Body>| async move {
        let uri = request.uri().clone();
        let query = uri.query().unwrap_or("");
        let (status, body) = if query.contains("sendInitialEvents") {
            (
                400,
                json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                       "reason": "BadRequest", "code": 400,
                       "message": "sendInitialEvents is not supported"})
                .to_string(),
            )
        } else if query.contains("watch=true") {
            (200, String::new())
        } else {
            // List items carry no apiVersion/kind, as a real list response
            // may omit them; the view must stamp the kind itself.
            let item = json!({"metadata": {"name": "web", "namespace": "default",
                                           "resourceVersion": "1"}});
            (
                200,
                json!({"apiVersion": "v1", "kind": "List",
                       "metadata": {"resourceVersion": "1"}, "items": [item]})
                .to_string(),
            )
        };
        let watch = query.contains("watch=true") && status == 200;
        let frames = stream::iter([Ok::<_, Infallible>(Frame::data(Bytes::from(body)))]);
        let frames = if watch {
            frames.chain(stream::pending()).boxed()
        } else {
            frames.boxed()
        };
        Ok::<_, Infallible>(
            http::Response::builder()
                .status(status)
                .body(http_body_util::StreamBody::new(frames))
                .unwrap(),
        )
    });
    let (mut app, mut rx) = test_app();
    app.cluster.client = kube::Client::new(service, "default");
    app.cluster
        .register_kind("apps", "StatefulSet", "statefulsets", true);
    app.cluster
        .register_kind("apps", "DaemonSet", "daemonsets", true);
    app.namespace = "default".into();
    type_resource_query(&mut app, "wk");

    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.store.synced {
            let msg = rx.recv().await.expect("watch channel closed");
            app.handle_msg(msg);
        }
    })
    .await
    .expect("workloads view never synced");

    app.handle_key(press(KeyCode::Char('O'))).unwrap();
    let mut kinds: Vec<String> = rows(&app).into_iter().map(|(kind, _)| kind).collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            "CronJob",
            "DaemonSet",
            "Deployment",
            "Job",
            "Pod",
            "StatefulSet"
        ]
    );
}
