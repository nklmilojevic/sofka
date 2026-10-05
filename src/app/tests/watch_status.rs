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
async fn watch_status_recovers_after_a_start_failure() {
    let (client, attempts) = scripted_watch_client(|attempt| match attempt {
        0 => Answer::Refuse,
        _ => Answer::Open { bookmark: true },
    });
    let (mut app, mut rx) = test_app();
    app.cluster.client = client;
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

/// How the scripted API server answers one pods watch request.
enum Answer {
    /// The connection fails before any response.
    Refuse,
    /// The stream stays open, after the streaming list's end bookmark.
    Open { bookmark: bool },
    /// The connection drops mid-stream, after the end bookmark.
    Cut { bookmark: bool },
}

/// A pods watch whose `n`th request (from 0) is answered per `script`.
fn scripted_watch_client(
    script: fn(usize) -> Answer,
) -> (kube::Client, Arc<std::sync::atomic::AtomicUsize>) {
    use futures_util::stream;
    use hyper::body::{Bytes, Frame};
    use std::sync::atomic::AtomicUsize;

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
            let Some(attempt) = attempt else {
                let body = stream::iter([Ok::<_, std::io::Error>(Frame::data(
                    Bytes::from_static(b"{}"),
                ))]);
                return Ok::<_, kube::Error>(http::Response::new(http_body_util::StreamBody::new(
                    body.boxed(),
                )));
            };
            let (bookmark, cut) = match script(attempt) {
                Answer::Refuse => {
                    let refused = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
                    return Err(kube::Error::Service(refused.into()));
                }
                Answer::Open { bookmark } => (bookmark, false),
                Answer::Cut { bookmark } => (bookmark, true),
            };
            let mut frames = Vec::new();
            if bookmark {
                frames.push(Ok(Frame::data(Bytes::from_static(
                    concat!(
                        "{\"type\":\"BOOKMARK\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",",
                        "\"metadata\":{\"resourceVersion\":\"10\",\"annotations\":",
                        "{\"k8s.io/initial-events-end\":\"true\"}}}}\n"
                    )
                    .as_bytes(),
                ))));
            }
            let frames = if cut {
                frames.push(Err(std::io::Error::from(
                    std::io::ErrorKind::ConnectionReset,
                )));
                stream::iter(frames).boxed()
            } else {
                stream::iter(frames).chain(stream::pending()).boxed()
            };
            Ok(http::Response::new(http_body_util::StreamBody::new(frames)))
        }
    });
    (kube::Client::new(service, "default"), attempts)
}

/// A proxy that caps request duration cuts a synced watch mid-stream. The
/// watch resumes from its resource version, so nothing is missing and there
/// is nothing to report beyond the reconnect count.
#[tokio::test]
async fn a_synced_watch_cut_mid_stream_reconnects_without_an_error() {
    let (client, attempts) = scripted_watch_client(|attempt| match attempt {
        0 => Answer::Cut { bookmark: true },
        _ => Answer::Open { bookmark: false },
    });
    let (mut app, mut rx) = test_app();
    app.cluster.client = client;
    type_resource_query(&mut app, "pods");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = rx.recv().await.unwrap();
            assert!(
                !matches!(msg, Msg::WatchError { .. }),
                "unexpected watch error"
            );
            app.handle_msg(msg);
            if app.watch_reconnects == 1 && attempts.load(Ordering::SeqCst) == 2 {
                break;
            }
        }
    })
    .await
    .expect("watch did not reconnect");
    assert!(!app.flash.contains("watch failed"), "{}", app.flash);
    assert!(!app.flash_err);
    assert_eq!(app.watch_errors, 0);
    assert!(app.store.synced);
    app.handle_key(press(KeyCode::Char('q'))).unwrap();
}

/// A connection that drops again right after reconnecting is a real problem,
/// not a request-duration cap, and stays visible.
#[tokio::test]
async fn a_watch_cut_again_right_after_reconnecting_reports_the_error() {
    let (client, _attempts) = scripted_watch_client(|attempt| match attempt {
        0 => Answer::Cut { bookmark: true },
        1 => Answer::Cut { bookmark: false },
        _ => Answer::Open { bookmark: false },
    });
    let (mut app, mut rx) = test_app();
    app.cluster.client = client;
    type_resource_query(&mut app, "pods");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = rx.recv().await.unwrap();
            let failed = matches!(msg, Msg::WatchError { .. });
            app.handle_msg(msg);
            if failed {
                break;
            }
        }
    })
    .await
    .expect("the repeated cut was not reported");
    assert!(
        app.flash.starts_with("watch failed; retrying: "),
        "{}",
        app.flash
    );
    assert!(
        app.flash.contains("Error reading events stream"),
        "{}",
        app.flash
    );
    assert_eq!(app.watch_reconnects, 1);
    assert_eq!(app.watch_errors, 1);
    app.handle_key(press(KeyCode::Char('q'))).unwrap();
}
