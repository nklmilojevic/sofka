use super::*;
use std::sync::Mutex;

type Respond = dyn Fn(&str, &str, usize) -> (u16, TestBody) + Send + Sync;

/// A response body fed by `rx`, ending when its sender is dropped.
fn channel_body(rx: mpsc::Receiver<String>) -> TestBody {
    http_body_util::StreamBody::new(
        futures_util::stream::unfold(rx, |mut rx| async move {
            let text = rx.recv().await?;
            Some((
                Ok(hyper::body::Frame::data(hyper::body::Bytes::from(text))),
                rx,
            ))
        })
        .boxed(),
    )
}

/// The receiver for the first request that takes it, an idle body after.
fn take_body(slot: &Mutex<Option<mpsc::Receiver<String>>>) -> TestBody {
    match slot.lock().unwrap().take() {
        Some(rx) => channel_body(rx),
        None => open_body(""),
    }
}

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
                // 0 stands for a request that never answers, as one sent
                // before the machine slept.
                if status == 0 {
                    std::future::pending::<()>().await;
                }
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

/// The pod a selector test reads by name, if `path` is one.
fn named_pod(path: &str) -> Option<(u16, TestBody)> {
    let name = path.strip_prefix("/api/v1/namespaces/default/pods/")?;
    let uid = name.strip_prefix("web-")?;
    Some((200, closed_body(pod_json(name, uid, 0).to_string())))
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
                // The check before the first request sees no restarts yet.
                let restarts = if nth == 0 { 0 } else { 1 };
                (
                    200,
                    closed_body(pod_json("web", "u1", restarts).to_string()),
                )
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
        Arc::new(|path, _query, nth| {
            if path.ends_with("/log") {
                (200, closed_body("2026-10-06T10:00:00Z bye\n"))
            } else if nth == 0 {
                (200, closed_body(pod_json("web", "u1", 0).to_string()))
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
        Arc::new(|path, _query, _nth| {
            if !path.ends_with("/log") {
                return (200, closed_body(pod_json("web", "u1", 0).to_string()));
            }
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
    assert_eq!(log_queries(&requests).len(), 1);
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
                (200, open_body("2099-01-01T10:00:00Z from a\n"))
            } else if path.ends_with("/web-b/log") {
                (200, open_body("2099-01-01T10:00:05Z from b\n"))
            } else if let Some(pod) = named_pod(path) {
                pod
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

    // The new pod can arrive before or after the first pod's lines. Lines
    // are dated in the future: a notice that arrives before any line sorts
    // at the current time.
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

#[tokio::test]
async fn a_recreated_pod_is_read_from_its_first_line() {
    let (mut app, mut rx) = pod_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, nth| {
            if path.ends_with("/log") {
                if nth == 0 {
                    (200, closed_body("2026-10-06T10:00:05Z old\n"))
                } else {
                    (200, open_body("2026-10-06T10:00:01Z new\n"))
                }
            } else if nth == 0 {
                (200, closed_body(pod_json("web", "u1", 0).to_string()))
            } else {
                (200, closed_body(pod_json("web", "u2", 0).to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "new").await;

    let text = app.filtered_log_text();
    assert!(text.contains("old"), "{text}");
    assert!(text.contains("[sofka] pod recreated"), "{text}");
    let queries = log_queries(&requests);
    assert_eq!(queries.len(), 2);
    for key in ["sinceTime", "sinceSeconds", "tailLines"] {
        assert!(!queries[1].contains_key(key), "{key}: {:?}", queries[1]);
    }
}

#[tokio::test]
async fn a_pod_deleted_between_streams_is_reported_not_refused() {
    let (mut app, mut rx) = pod_logs_app();
    serve(
        &mut app,
        Arc::new(|path, _query, nth| match (path.ends_with("/log"), nth) {
            (true, 0) => (200, closed_body("2026-10-06T10:00:00Z bye\n")),
            (false, 0) => (200, closed_body(pod_json("web", "u1", 0).to_string())),
            _ => not_found(),
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "pod deleted").await;
    assert_eq!(
        app.filtered_log_text(),
        "bye\n[sofka] pod deleted; stream ended"
    );
}

#[tokio::test]
async fn waking_interrupts_a_log_request_that_never_answers() {
    let (mut app, mut rx) = pod_logs_app();
    serve(
        &mut app,
        Arc::new(|path, _query, nth| match (path.ends_with("/log"), nth) {
            (true, 0) => (0, open_body("")),
            (true, _) => (200, open_body("2026-10-06T10:00:00Z after\n")),
            _ => (200, closed_body(pod_json("web", "u1", 0).to_string())),
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    super::resume::tick_after_sleep(&mut app, Duration::from_secs(8 * 60 * 60));
    // Well inside the 30-second open timeout.
    wait_for(&mut app, &mut rx, "after").await;
}

fn selector_logs_app() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("deployments");
    apply(
        &mut app,
        json!({"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}}),
    );
    app.table_state.select(Some(0));
    (app, rx)
}

fn pod_list(pod: serde_json::Value) -> (u16, TestBody) {
    let list = json!({"apiVersion": "v1", "kind": "PodList",
        "metadata": {"resourceVersion": "1"}, "items": [pod]});
    (200, closed_body(list.to_string()))
}

#[tokio::test]
async fn a_pod_that_leaves_the_selector_stops_streaming() {
    let (mut app, mut rx) = selector_logs_app();
    let (log_tx, log_rx) = mpsc::channel(8);
    let (watch_tx, watch_rx) = mpsc::channel(8);
    let logs = Mutex::new(Some(log_rx));
    let watch = Mutex::new(Some(watch_rx));
    serve(
        &mut app,
        Arc::new(move |path, query, _nth| {
            if path.ends_with("/log") {
                (200, take_body(&logs))
            } else if let Some(pod) = named_pod(path) {
                pod
            } else if query.contains("watch=true") {
                (200, take_body(&watch))
            } else {
                pod_list(pod_json("web-a", "a", 0))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    log_tx
        .send("2026-10-06T10:00:00Z first\n".into())
        .await
        .unwrap();
    wait_for(&mut app, &mut rx, "first").await;

    let mut relabeled = pod_json("web-a", "a", 0);
    relabeled["metadata"]["labels"] = json!({"app": "debug"});
    watch_tx
        .send(format!(
            "{}\n",
            json!({"type": "DELETED", "object": relabeled})
        ))
        .await
        .unwrap();
    wait_for(&mut app, &mut rx, "no longer matches").await;

    let _ = log_tx.send("2026-10-06T10:00:01Z late\n".into()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(msg) = rx.try_recv() {
        app.handle_msg(msg);
    }
    assert_eq!(
        app.filtered_log_text(),
        "[web-a] first\n[web-a] [sofka] pod no longer matches the selector; stream ended"
    );
}

#[tokio::test]
async fn a_refused_pod_watch_keeps_existing_streams_and_stops_asking() {
    let (mut app, mut rx) = selector_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|path, query, _nth| {
            if path.ends_with("/log") {
                (200, open_body("2026-10-06T10:00:00Z still here\n"))
            } else if let Some(pod) = named_pod(path) {
                pod
            } else if query.contains("watch=true") {
                (
                    403,
                    closed_body(
                        json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "message": "cannot watch pods", "reason": "Forbidden", "code": 403})
                        .to_string(),
                    ),
                )
            } else {
                pod_list(pod_json("web-a", "a", 0))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "pod watch refused").await;
    wait_for(&mut app, &mut rx, "still here").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let watches = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.contains("watch=true"))
        .count();
    assert_eq!(watches, 1);
}

#[tokio::test]
async fn a_marked_pod_replaced_before_its_stream_opens_is_never_streamed() {
    let (mut app, mut rx) = pod_logs_app();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, _nth| {
            if path.ends_with("/log") {
                (200, open_body("2026-10-06T10:00:00Z unmarked\n"))
            } else {
                (200, closed_body(pod_json("web", "u2", 0).to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "pod replaced").await;

    assert_eq!(
        app.filtered_log_text(),
        "[default/web:app] [sofka] pod replaced; stream ended"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(log_queries(&requests).is_empty());
}

#[tokio::test]
async fn a_pod_replaced_before_its_stream_opens_is_read_once_from_its_start() {
    let (mut app, mut rx) = pod_logs_app();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, _nth| {
            if path.ends_with("/log") {
                (200, open_body("2026-10-06T10:00:05Z first\n"))
            } else {
                (200, closed_body(pod_json("web", "u2", 0).to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "first").await;

    // The notice has no timestamp of its own, so its place among the lines
    // depends on the clock; only membership is fixed.
    let text = app.filtered_log_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines.contains(&"[sofka] pod recreated"), "{text}");
    assert!(lines.contains(&"first"), "{text}");
    let queries = log_queries(&requests);
    assert_eq!(queries.len(), 1);
    for key in ["sinceTime", "sinceSeconds", "tailLines"] {
        assert!(!queries[0].contains_key(key), "{key}: {:?}", queries[0]);
    }
}

#[tokio::test]
async fn waking_interrupts_a_pod_check_that_never_answers() {
    let (mut app, mut rx) = pod_logs_app();
    serve(
        &mut app,
        Arc::new(|path, _query, nth| match (path.ends_with("/log"), nth) {
            (false, 0) => (0, open_body("")),
            (false, _) => (200, closed_body(pod_json("web", "u1", 0).to_string())),
            (true, _) => (200, open_body("2026-10-06T10:00:00Z back\n")),
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    super::resume::tick_after_sleep(&mut app, Duration::from_secs(8 * 60 * 60));
    // Well inside the 10-second pod check timeout.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.filtered_log_text().contains("back") {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .expect("the wake must abandon the stuck pod check");
}

#[tokio::test]
async fn a_marked_pod_is_not_streamed_until_its_identity_is_confirmed() {
    let (mut app, mut rx) = pod_logs_app();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, nth| {
            if path.ends_with("/log") {
                (200, open_body("2026-10-06T10:00:00Z unmarked\n"))
            } else if nth == 0 {
                (
                    500,
                    closed_body(
                        json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "message": "etcd timeout", "reason": "InternalError", "code": 500})
                        .to_string(),
                    ),
                )
            } else {
                (200, closed_body(pod_json("web", "u2", 0).to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "pod replaced").await;
    assert_eq!(
        app.filtered_log_text(),
        "[default/web:app] [sofka] pod replaced; stream ended"
    );
    assert!(log_queries(&requests).is_empty());
}

#[tokio::test]
async fn a_pod_that_fails_while_its_container_waits_ends_the_stream() {
    let (mut app, mut rx) = pod_logs_app();
    serve(
        &mut app,
        Arc::new(|path, _query, nth| {
            if path.ends_with("/log") {
                (
                    400,
                    closed_body(
                        json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                            "message": "container \"app\" in pod \"web\" is waiting to start: ContainerCreating",
                            "reason": "BadRequest", "code": 400})
                        .to_string(),
                    ),
                )
            } else {
                let mut pod = pod_json("web", "u1", 0);
                if nth > 0 {
                    pod["status"]["phase"] = json!("Failed");
                }
                (200, closed_body(pod.to_string()))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "stream ended").await;
    assert_eq!(app.filtered_log_text(), "[sofka] pod failed; stream ended");
}

fn forbidden() -> (u16, TestBody) {
    (
        403,
        closed_body(
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                "message": "forbidden", "reason": "Forbidden", "code": 403})
            .to_string(),
        ),
    )
}

#[tokio::test]
async fn a_marked_pod_is_confirmed_by_listing_when_get_is_refused() {
    let (mut app, mut rx) = pod_logs_app();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    let requests = serve(
        &mut app,
        Arc::new(|path, query, _nth| {
            if path.ends_with("/log") {
                (200, open_body("2026-10-06T10:00:00Z unmarked\n"))
            } else if path.ends_with("/pods/web") {
                forbidden()
            } else {
                assert!(
                    query.contains("fieldSelector=metadata.name%3Dweb"),
                    "{query}"
                );
                pod_list(pod_json("web", "u2", 0))
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "pod replaced").await;
    assert!(log_queries(&requests).is_empty());
}

#[tokio::test]
async fn a_marked_pod_that_cannot_be_read_is_not_streamed() {
    let (mut app, mut rx) = pod_logs_app();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    let requests = serve(
        &mut app,
        Arc::new(|path, _query, _nth| {
            if path.ends_with("/log") {
                (200, open_body("2026-10-06T10:00:00Z unchecked\n"))
            } else {
                forbidden()
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    wait_for(&mut app, &mut rx, "cannot read the pod").await;
    assert!(log_queries(&requests).is_empty());
}
