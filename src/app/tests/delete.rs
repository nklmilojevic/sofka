use super::*;

fn delete_app(names: &[&str]) -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("pods");
    for name in names {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": name, "namespace": "default"}}),
        );
    }
    app.table_state.select(Some(0));
    (app, rx)
}

fn record_delete_requests(app: &mut App) -> mpsc::UnboundedReceiver<(http::Method, String, Value)> {
    let (sent, requests) = mpsc::unbounded_channel();
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |req: http::Request<kube::client::Body>| {
            let sent = sent.clone();
            async move {
                let method = req.method().clone();
                let path = req.uri().path().to_string();
                let body: Value =
                    serde_json::from_slice(&req.into_body().collect_bytes().await.unwrap())
                        .unwrap();
                sent.send((method, path, body)).unwrap();
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(200)
                        .body(http_body_util::Full::new(hyper::body::Bytes::from(
                            json!({"apiVersion": "v1", "kind": "Status", "status": "Success"})
                                .to_string(),
                        )))
                        .unwrap(),
                )
            }
        }),
        "default",
    );
    requests
}

async fn delete_bodies(
    keys: &[KeyEvent],
    names: &[&str],
    mark_all: bool,
) -> Vec<(http::Method, String, Value)> {
    let (mut app, mut rx) = delete_app(names);
    let mut requests = record_delete_requests(&mut app);
    if mark_all {
        for _ in names {
            app.handle_key(press(KeyCode::Char(' '))).unwrap();
            app.handle_key(press(KeyCode::Down)).unwrap();
        }
    }
    for key in keys {
        app.handle_key(*key).unwrap();
    }
    let mut bodies = Vec::new();
    for _ in names {
        bodies.push(
            tokio::time::timeout(Duration::from_secs(2), requests.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let done = matches!(msg, Msg::Flash { .. });
        app.handle_msg(msg);
        if done {
            break;
        }
    }
    assert!(!app.flash_err, "{}", app.flash);
    assert!(app.flash.starts_with("deleted "), "{}", app.flash);
    assert!(requests.try_recv().is_err());
    bodies
}

#[tokio::test]
async fn delete_sends_background_propagation_and_no_grace_period() {
    let requests = delete_bodies(
        &[ctrl(KeyCode::Char('d')), press(KeyCode::Char('y'))],
        &["web"],
        false,
    )
    .await;
    let (method, path, body) = &requests[0];
    assert_eq!(method, http::Method::DELETE);
    assert_eq!(path, "/api/v1/namespaces/default/pods/web");
    assert_eq!(body["propagationPolicy"], "Background");
    assert!(body.get("gracePeriodSeconds").is_none(), "{body}");
}

#[tokio::test]
async fn force_delete_sends_a_zero_grace_period() {
    let requests = delete_bodies(
        &[ctrl(KeyCode::Char('k')), press(KeyCode::Char('y'))],
        &["web"],
        false,
    )
    .await;
    let (method, path, body) = &requests[0];
    assert_eq!(method, http::Method::DELETE);
    assert_eq!(path, "/api/v1/namespaces/default/pods/web");
    assert_eq!(body["gracePeriodSeconds"], 0);
    assert_eq!(body["propagationPolicy"], "Background");
}

#[tokio::test]
async fn toggling_force_in_the_confirm_changes_the_body() {
    for key in ['d', 'k'] {
        let requests = delete_bodies(
            &[
                ctrl(KeyCode::Char(key)),
                press(KeyCode::Char('f')),
                press(KeyCode::Char('y')),
            ],
            &["web"],
            false,
        )
        .await;
        let body = &requests[0].2;
        if key == 'd' {
            assert_eq!(body["gracePeriodSeconds"], 0);
        } else {
            assert!(body.get("gracePeriodSeconds").is_none(), "{body}");
        }
        assert_eq!(body["propagationPolicy"], "Background");
    }
}

#[tokio::test]
async fn cascade_choice_reaches_the_request() {
    for (cycles, policy) in [(1, "Foreground"), (2, "Orphan")] {
        let mut keys = vec![ctrl(KeyCode::Char('d'))];
        keys.extend(std::iter::repeat_n(press(KeyCode::Char('c')), cycles));
        keys.push(press(KeyCode::Char('y')));
        let requests = delete_bodies(&keys, &["web"], false).await;
        let body = &requests[0].2;
        assert_eq!(body["propagationPolicy"], policy);
        assert!(body.get("gracePeriodSeconds").is_none(), "{body}");
    }
}

#[tokio::test]
async fn a_marked_delete_sends_one_request_per_row() {
    let mut requests = delete_bodies(
        &[ctrl(KeyCode::Char('d')), press(KeyCode::Char('y'))],
        &["a", "b"],
        true,
    )
    .await;
    assert_eq!(requests.len(), 2);
    requests.sort_by(|a, b| a.1.cmp(&b.1));
    for ((method, path, body), name) in requests.iter().zip(["a", "b"]) {
        assert_eq!(method, http::Method::DELETE);
        assert_eq!(path, &format!("/api/v1/namespaces/default/pods/{name}"));
        assert_eq!(body["propagationPolicy"], "Background");
        assert!(body.get("gracePeriodSeconds").is_none(), "{body}");
    }
}

#[tokio::test]
async fn read_only_mode_sends_nothing() {
    for key in ['d', 'k'] {
        let (mut app, _rx) = delete_app(&["web"]);
        let mut requests = record_delete_requests(&mut app);
        app.readonly = true;
        app.handle_key(ctrl(KeyCode::Char(key))).unwrap();
        assert!(app.flash.contains("read-only"), "{}", app.flash);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), requests.recv())
                .await
                .is_err()
        );
    }
}
