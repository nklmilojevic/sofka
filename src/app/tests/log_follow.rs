use super::*;
use std::sync::Mutex;

type Respond = dyn Fn(&str, &str, usize) -> (u16, TestBody) + Send + Sync;

/// Serve the API from `respond(path, query, nth)`, where `nth` counts earlier
/// requests to the same path. Returns every `path?query` it saw.
fn serve(app: &mut App, respond: Arc<Respond>) -> Arc<Mutex<Vec<String>>> {
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            let nth = {
                let mut seen = seen.lock().unwrap();
                let nth = seen
                    .iter()
                    .filter(|r| r.split('?').next() == Some(path.as_str()))
                    .count();
                seen.push(format!("{path}?{query}"));
                nth
            };
            let (status, body) = respond(&path, &query, nth);
            async move {
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder().status(status).body(body).unwrap(),
                )
            }
        }),
        "default",
    );
    requests
}

fn pod_json(name: &str, uid: &str, restarts: i32) -> serde_json::Value {
    json!({"apiVersion": "v1", "kind": "Pod",
    "metadata": {"name": name, "namespace": "default", "uid": uid, "resourceVersion": "2",
        "labels": {"app": "web"}},
    "spec": {"containers": [{"name": "app", "image": "web"}]},
    "status": {"phase": "Running", "containerStatuses": [{
        "name": "app", "restartCount": restarts, "image": "web", "imageID": "",
        "ready": true,
        "lastState": if restarts > 0 {
            json!({"terminated": {"exitCode": 1, "finishedAt": "2099-01-01T00:00:00Z"}})
        } else {
            json!({})
        }}]}})
}

fn pod_logs_app() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("pods");
    apply(&mut app, pod_json("web", "u1", 0));
    app.table_state.select(Some(0));
    (app, rx)
}

fn not_found() -> (u16, TestBody) {
    (
        404,
        closed_body(
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                "message": "pods \"web\" not found", "reason": "NotFound", "code": 404})
            .to_string(),
        ),
    )
}

async fn wait_for(app: &mut App, rx: &mut Receiver<Msg>, text: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !app.filtered_log_text().contains(text) {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {text:?} in:\n{}", app.filtered_log_text()));
}

fn log_queries(requests: &Mutex<Vec<String>>) -> Vec<HashMap<String, String>> {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| r.split_once("/log?"))
        .map(|(_, query)| {
            form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn followed_logs_resume_after_a_container_restart_without_repeating_lines() {
    let (mut app, mut rx) = pod_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, nth| {
            if path.ends_with("/log") {
                if nth == 0 {
                    (
                        200,
                        closed_body("2026-10-06T10:00:00.5Z one\n2026-10-06T10:00:01.2Z two\n"),
                    )
                } else {
                    (
                        200,
                        open_body("2026-10-06T10:00:01.2Z two\n2026-10-06T10:00:02Z three\n"),
                    )
                }
            } else {
                (200, closed_body(pod_json("web", "u1", 1).to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "three").await;

    assert_eq!(
        app.filtered_log_text(),
        "one\ntwo\n[sofka] container restarted (restarts: 1)\nthree"
    );
    let queries = log_queries(&requests);
    assert_eq!(queries.len(), 2);
    assert_eq!(
        queries[1].get("sinceTime").map(String::as_str),
        Some("2026-10-06T10:00:01Z")
    );
    assert!(!queries[1].contains_key("tailLines"));
    assert_eq!(queries[1].get("follow").map(String::as_str), Some("true"));
    app.handle_key(press(KeyCode::Esc)).unwrap();
}

#[tokio::test]
async fn followed_logs_stop_when_the_pod_is_deleted() {
    let (mut app, mut rx) = pod_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, _nth| {
            if path.ends_with("/log") {
                (200, closed_body("2026-10-06T10:00:00Z bye\n"))
            } else {
                not_found()
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "pod deleted").await;

    assert_eq!(
        app.filtered_log_text(),
        "bye\n[sofka] pod deleted; stream ended"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(log_queries(&requests).len(), 1);
}

#[tokio::test]
async fn refused_follow_streams_report_once_and_stop() {
    let (mut app, mut rx) = pod_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|_path, _query, _nth| {
            (
                403,
                closed_body(
                    json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "message": "access denied", "reason": "Forbidden", "code": 403})
                    .to_string(),
                ),
            )
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "access denied").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn workload_logs_follow_pods_a_rollout_creates() {
    let (mut app, mut rx) = test_app();
    app.switch_kind("deployments");
    apply(
        &mut app,
        json!({"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}}),
    );
    app.table_state.select(Some(0));
    let requests = serve(
        &mut app,
        Arc::new(|path, query, _nth| {
            if path.ends_with("/web-a/log") {
                (200, open_body("2026-10-06T10:00:00Z from a\n"))
            } else if path.ends_with("/web-b/log") {
                (200, open_body("2026-10-06T10:00:05Z from b\n"))
            } else if query.contains("watch=true") {
                let added = json!({"type": "ADDED", "object": pod_json("web-b", "b", 0)});
                (200, open_body(format!("{added}\n")))
            } else {
                assert!(query.contains("labelSelector=app%3Dweb"), "{query}");
                let list = json!({"apiVersion": "v1", "kind": "PodList",
                    "metadata": {"resourceVersion": "1"},
                    "items": [pod_json("web-a", "a", 0)]});
                (200, closed_body(list.to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "from b").await;

    // The new pod can arrive before or after the first pod's lines.
    let text = app.filtered_log_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines.contains(&"[web-a] from a"), "{text}");
    let notice = lines
        .iter()
        .position(|l| *l == "[web-b] [sofka] following new pod");
    let first = lines.iter().position(|l| *l == "[web-b] from b");
    assert!(notice.unwrap() < first.unwrap(), "{text}");
    let requests = requests.lock().unwrap();
    let new_pod = requests.iter().find(|r| r.contains("/web-b/log?")).unwrap();
    assert!(!new_pod.contains("tailLines"), "{new_pod}");
    assert!(!new_pod.contains("sinceSeconds"), "{new_pod}");
    let existing = requests.iter().find(|r| r.contains("/web-a/log?")).unwrap();
    assert!(existing.contains("tailLines"), "{existing}");
}

#[tokio::test]
async fn waking_from_sleep_reconnects_followed_logs() {
    let (mut app, mut rx) = pod_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, nth| {
            if path.ends_with("/log") {
                if nth == 0 {
                    (200, open_body("2026-10-06T10:00:00Z before\n"))
                } else {
                    (
                        200,
                        open_body("2026-10-06T10:00:00Z before\n2026-10-06T10:00:09Z after\n"),
                    )
                }
            } else {
                (200, closed_body(pod_json("web", "u1", 0).to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "before").await;

    super::resume::tick_after_sleep(&mut app, Duration::from_secs(8 * 60 * 60));
    wait_for(&mut app, &mut rx, "after").await;

    assert_eq!(app.filtered_log_text(), "before\nafter");
    let queries = log_queries(&requests);
    assert_eq!(queries.len(), 2);
    assert_eq!(
        queries[1].get("sinceTime").map(String::as_str),
        Some("2026-10-06T10:00:00Z")
    );
}
