use super::*;

#[tokio::test]
async fn watch_error_stays_until_recovery_and_keeps_diagnostics() {
    let (mut app, _rx) = test_app();
    type_resource_query(&mut app, "pods");
    let generation = app.generation;
    app.handle_msg(Msg::WatchError {
        generation,
        error: "connection closed".into(),
        failure: WatchFailure::Response,
    });
    assert_eq!(app.flash, "watch failed; retrying: connection closed");
    app.flash_since = Instant::now() - Duration::from_secs(30);
    app.expire_flash();
    assert!(app.flash_err);
    app.handle_msg(Msg::Reset { generation });
    app.handle_msg(Msg::Synced { generation });
    assert!(app.flash_err, "a list does not prove that the watch works");
    app.handle_msg(Msg::WatchRecovered {
        generation: generation - 1,
    });
    assert!(app.flash_err);
    app.handle_msg(Msg::WatchRecovered { generation });
    assert!(app.flash.is_empty());
    assert!(!app.flash_err);
    assert_eq!(app.watch_errors, 1);
    assert_eq!(app.last_error.as_deref(), Some("connection closed"));
}

#[tokio::test]
async fn watch_recovery_preserves_a_later_error_and_pending_action() {
    let (mut app, _rx) = test_app();
    type_resource_query(&mut app, "pods");
    let generation = app.generation;
    let claim = app.claim_status("deleting api…");
    app.handle_msg(Msg::WatchError {
        generation,
        error: "connection closed".into(),
        failure: WatchFailure::Response,
    });
    app.handle_msg(Msg::WatchRecovered { generation });
    app.handle_msg(Msg::Flash {
        generation,
        claim,
        message: "delete failed: forbidden".into(),
        err: true,
    });
    assert_eq!(app.flash, "delete failed: forbidden");
    app.handle_msg(Msg::WatchRecovered { generation });
    assert_eq!(app.flash, "delete failed: forbidden");
    assert!(app.flash_err);

    app.handle_msg(Msg::WatchError {
        generation,
        error: "connection closed".into(),
        failure: WatchFailure::Response,
    });
    app.handle_msg(Msg::Error {
        generation,
        error: "another failure".into(),
    });
    app.handle_msg(Msg::WatchRecovered { generation });
    assert_eq!(app.flash, "error: another failure");
    assert!(app.flash_err);
}

#[tokio::test]
async fn changing_resources_clears_only_the_old_watch_error() {
    let (mut app, _rx) = test_app();
    type_resource_query(&mut app, "pods");
    let generation = app.generation;
    app.handle_msg(Msg::WatchError {
        generation,
        error: "connection closed".into(),
        failure: WatchFailure::Response,
    });
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(!app.flash.contains("connection closed"));
    app.handle_msg(Msg::WatchError {
        generation,
        error: "old failure".into(),
        failure: WatchFailure::Response,
    });
    assert!(!app.flash.contains("old failure"));
}

#[tokio::test]
async fn watch_status_recovers_after_start_and_body_failures() {
    use futures_util::stream;
    use hyper::body::{Bytes, Frame};
    use std::sync::atomic::AtomicUsize;

    for fail_start in [true, false] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let watch = request.uri().path().ends_with("/pods")
                && request
                    .uri()
                    .query()
                    .is_some_and(|q| q.contains("watch=true"));
            let attempt = watch.then(|| seen.fetch_add(1, Ordering::SeqCst));
            async move {
                let disconnected = || std::io::Error::from(std::io::ErrorKind::ConnectionReset);
                if fail_start && attempt == Some(0) {
                    return Err(kube::Error::Service(disconnected().into()));
                }
                let bookmark = concat!(
                    "{\"type\":\"BOOKMARK\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",",
                    "\"metadata\":{\"resourceVersion\":\"10\",\"annotations\":",
                    "{\"k8s.io/initial-events-end\":\"true\"}}}}\n"
                );
                let mut frames = Vec::new();
                if attempt == Some(0) || (fail_start && attempt == Some(1)) {
                    frames.push(Ok(Frame::data(Bytes::from_static(bookmark.as_bytes()))));
                }
                let frames = if !fail_start && attempt == Some(0) {
                    frames.push(Err(disconnected()));
                    stream::iter(frames).boxed()
                } else if watch {
                    stream::iter(frames).chain(stream::pending()).boxed()
                } else {
                    stream::iter([Ok(Frame::data(Bytes::from_static(b"{}")))]).boxed()
                };
                Ok(http::Response::new(http_body_util::StreamBody::new(frames)))
            }
        });
        let (mut app, mut rx) = test_app();
        app.cluster.client = kube::Client::new(service, "default");
        type_resource_query(&mut app, "pods");
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut failed = false;
            loop {
                let msg = rx.recv().await.unwrap();
                let recovered = matches!(msg, Msg::WatchRecovered { .. });
                if matches!(msg, Msg::WatchError { .. }) {
                    failed = true;
                }
                app.handle_msg(msg);
                if recovered && failed {
                    break;
                }
            }
        })
        .await
        .expect("watch did not recover");
        assert!(app.flash.is_empty(), "{}", app.flash);
        assert!(!app.flash_err);
        assert_eq!(app.watch_errors, 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        app.handle_key(press(KeyCode::Char('q'))).unwrap();
    }
}
