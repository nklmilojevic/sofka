use super::*;
use crate::store::row_key;
use serde_json::json;
use std::time::Instant;
use tokio::sync::mpsc::{self, Receiver};

fn obj(v: serde_json::Value) -> DynamicObject {
    serde_json::from_value(v).unwrap()
}

fn test_app() -> (App, Receiver<Msg>) {
    let (tx, rx) = mpsc::channel(1024);
    (App::new(Cluster::fake(), tx), rx)
}

/// The claim the operation that just started owns, for tests that hand-build
/// the result message it is waiting for.
fn current_claim(app: &App) -> crate::store::StatusClaim {
    app.status_claim
        .as_ref()
        .expect("no operation has claimed the status bar")
        .claim
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

/// A stand-in API server for the nodes view's pod counter: the *watch* is
/// refused, the plain *list* succeeds. That is the RBAC shape — `list` granted,
/// `watch` not — the poll fallback exists for. Records every request path.
async fn mock_pods_api() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const POD_LIST_INITIAL: &str = concat!(
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"7"},"items":["#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"a","namespace":"default","resourceVersion":"1"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}},"#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"b","namespace":"default","resourceVersion":"2"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}},"#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"c","namespace":"default","resourceVersion":"3"},"spec":{"nodeName":"node-b"},"status":{"phase":"Running"}}"#,
        r#"]}"#
    );
    const POD_LIST_UPDATED: &str = concat!(
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"8"},"items":["#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"c","namespace":"default","resourceVersion":"4"},"spec":{"nodeName":"node-b"},"status":{"phase":"Running"}},"#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"d","namespace":"default","resourceVersion":"5"},"spec":{"nodeName":"node-b"},"status":{"phase":"Running"}}"#,
        r#"]}"#
    );
    const FORBIDDEN: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"pods is forbidden: cannot watch at the cluster scope","reason":"Forbidden","code":403}"#;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock pods api");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let (r, mut w) = sock.split();
                let mut reader = BufReader::new(r);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let list_number = {
                    let mut requests = seen.lock().unwrap();
                    requests.push(path.clone());
                    requests
                        .iter()
                        .filter(|p| !p.contains("watch=true"))
                        .count()
                };
                let (status, body) = if path.contains("watch=true") {
                    ("403 Forbidden", FORBIDDEN)
                } else if list_number == 1 {
                    ("200 OK", POD_LIST_INITIAL)
                } else {
                    ("200 OK", POD_LIST_UPDATED)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = w.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), requests)
}

/// A stand-in API server that serves a real list *and* a real watch stream, so
/// the incremental counting path can be driven event by event. `lists` supplies
/// one body per list attempt (the last repeats), which is what lets a resync
/// return a different pod set. The returned sender pushes raw watch frames;
/// dropping it ends the stream.
async fn mock_pods_stream_api(
    lists: Vec<&'static str>,
) -> (
    String,
    mpsc::Sender<String>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock pods api");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let (frame_tx, frame_rx) = mpsc::channel::<String>(64);
    let frames = Arc::new(tokio::sync::Mutex::new(Some(frame_rx)));

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            let frames = Arc::clone(&frames);
            let lists = lists.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.split();
                let mut reader = BufReader::new(r);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let list_number = {
                    let mut requests = seen.lock().unwrap();
                    requests.push(path.clone());
                    requests
                        .iter()
                        .filter(|p| !p.contains("watch=true"))
                        .count()
                };

                if path.contains("watch=true") {
                    // Chunked, so the connection stays open and `watcher` reads
                    // newline-delimited frames off it as they are pushed.
                    let _ = w
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n",
                        )
                        .await;
                    // Only the first watch is scripted. A re-watch after the
                    // scripted stream ends just parks, so the test observes the
                    // resync rather than a reconnect loop.
                    let scripted = frames.lock().await.take();
                    match scripted {
                        Some(mut rx) => {
                            while let Some(frame) = rx.recv().await {
                                let body = format!("{frame}\n");
                                let chunk = format!("{:x}\r\n{body}\r\n", body.len());
                                if w.write_all(chunk.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                            let _ = w.write_all(b"0\r\n\r\n").await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                    return;
                }

                let body = lists[(list_number - 1).min(lists.len() - 1)];
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = w.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), frame_tx, requests)
}

/// A stand-in API server whose pod list always fails with a 500, recording when
/// each attempt arrived so the retry pacing can be measured.
async fn mock_pods_failing_api() -> (String, Arc<std::sync::Mutex<Vec<Instant>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const SERVER_ERROR: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"internal","reason":"InternalError","code":500}"#;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock pods api");
    let addr = listener.local_addr().expect("local addr");
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&attempts);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let (r, mut w) = sock.split();
                let mut reader = BufReader::new(r);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                seen.lock().unwrap().push(Instant::now());
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{SERVER_ERROR}",
                    SERVER_ERROR.len()
                );
                let _ = w.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), attempts)
}

/// Wait for a `Msg::NodePods` carrying exactly `want`. Publications are
/// coalesced to one a second, so intermediate states can be skipped — the test
/// drives one change at a time and waits for each to land.
async fn await_counts(rx: &mut Receiver<Msg>, want: &[(&str, usize)]) {
    let want: std::collections::HashMap<String, usize> =
        want.iter().map(|(n, c)| ((*n).to_string(), *c)).collect();
    let seen = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Msg::NodePods { counts, .. } = rx.recv().await.expect("channel closed")
                && counts == want
            {
                return;
            }
        }
    })
    .await;
    assert!(seen.is_ok(), "never saw counts {want:?}");
}

/// A `Cluster` whose client talks to `url` instead of a real API server.
fn mock_cluster(url: &str) -> Cluster {
    let mut config = kube::Config::new(url.parse().expect("mock url"));
    // The client's own retry policy would turn each 403 into a long stall;
    // this test is about the app's fallback, not the client's retries.
    config.default_retry = false;
    let mut cluster = Cluster::fake();
    cluster.client = Client::try_from(config).expect("mock client");
    cluster.cluster_url = url.into();
    cluster
}

/// RBAC granting `list` but not `watch`. The first list seeds counts, then the
/// refused watch must switch to periodic lists: kube's watcher retries only
/// the watch from the same resourceVersion, so it cannot provide that fallback
/// itself. The mock changes its list response to prove counts remain fresh.
#[tokio::test]
async fn node_pods_survives_a_forbidden_watch_without_hammering_the_api() {
    let (url, seen) = mock_pods_api().await;
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(mock_cluster(&url), tx);

    app.spawn_node_pods_poll();

    let counts = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let msg = rx.recv().await.expect("channel closed");
            if let Msg::NodePods { counts, .. } = msg
                && !counts.contains_key("node-a")
                && counts.get("node-b") == Some(&2)
            {
                break counts;
            }
        }
    })
    .await
    .expect("periodic-list fallback did not publish refreshed counts");
    assert_eq!(counts.len(), 1, "counts: {counts:?}");

    // Now let the fallback hold for a while. It must neither retry the refused
    // watch nor start polling faster than its 10-second interval.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let requests = seen.lock().unwrap();
    let watch_attempts = requests.iter().filter(|p| p.contains("watch=true")).count();
    let list_attempts = requests.len() - watch_attempts;
    assert_eq!(watch_attempts, 1, "fallback must stop retrying the watch");
    assert_eq!(list_attempts, 2, "fallback should wait 10s between lists");
}

/// The incremental path end to end: initial sync, a new pod, a pod moving to a
/// different node, and a delete. Each step is awaited before the next is
/// pushed, because publications are coalesced to one a second.
#[tokio::test]
async fn node_pods_counts_follow_the_watch_incrementally() {
    const INITIAL: &str = concat!(
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"7"},"items":["#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"a","namespace":"default","resourceVersion":"1"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}},"#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"b","namespace":"default","resourceVersion":"2"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}},"#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"c","namespace":"default","resourceVersion":"3"},"spec":{"nodeName":"node-b"},"status":{"phase":"Running"}}"#,
        r#"]}"#
    );

    let (url, frames, _seen) = mock_pods_stream_api(vec![INITIAL]).await;
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(mock_cluster(&url), tx);
    app.spawn_node_pods_poll();

    // Nothing is published until `InitDone`: a partial initial list would walk
    // the PODS column up from zero on every resync.
    await_counts(&mut rx, &[("node-a", 2), ("node-b", 1)]).await;

    frames
        .send(r#"{"type":"ADDED","object":{"apiVersion":"v1","kind":"Pod","metadata":{"name":"d","namespace":"default","resourceVersion":"10"},"spec":{"nodeName":"node-b"},"status":{"phase":"Running"}}}"#.into())
        .await
        .expect("push add");
    await_counts(&mut rx, &[("node-a", 2), ("node-b", 2)]).await;

    // Reassignment: the same pod key reported on a different node has to
    // decrement the old one as well as increment the new one.
    frames
        .send(r#"{"type":"MODIFIED","object":{"apiVersion":"v1","kind":"Pod","metadata":{"name":"a","namespace":"default","resourceVersion":"11"},"spec":{"nodeName":"node-b"},"status":{"phase":"Running"}}}"#.into())
        .await
        .expect("push move");
    await_counts(&mut rx, &[("node-a", 1), ("node-b", 3)]).await;

    // A node's last pod leaving drops the node from the map rather than
    // leaving a zero behind.
    frames
        .send(r#"{"type":"DELETED","object":{"apiVersion":"v1","kind":"Pod","metadata":{"name":"b","namespace":"default","resourceVersion":"12"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}}}"#.into())
        .await
        .expect("push delete");
    await_counts(&mut rx, &[("node-b", 3)]).await;
}

/// A 410 desyncs the watch, so `watcher` re-lists and replays `Init` ->
/// `InitApply` -> `InitDone`. The counts must be rebuilt from that list alone:
/// merging into the old ones would strand nodes that no longer have pods.
#[tokio::test]
async fn node_pods_rebuilds_counts_after_a_desync_resync() {
    const INITIAL: &str = concat!(
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"7"},"items":["#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"a","namespace":"default","resourceVersion":"1"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}},"#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"b","namespace":"default","resourceVersion":"2"},"spec":{"nodeName":"node-a"},"status":{"phase":"Running"}}"#,
        r#"]}"#
    );
    const AFTER_RESYNC: &str = concat!(
        r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"99"},"items":["#,
        r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"e","namespace":"default","resourceVersion":"20"},"spec":{"nodeName":"node-c"},"status":{"phase":"Running"}}"#,
        r#"]}"#
    );

    let (url, frames, _seen) = mock_pods_stream_api(vec![INITIAL, AFTER_RESYNC]).await;
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(mock_cluster(&url), tx);
    app.spawn_node_pods_poll();

    await_counts(&mut rx, &[("node-a", 2)]).await;

    frames
        .send(r#"{"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","status":"Failure","message":"too old resource version","reason":"Expired","code":410}}"#.into())
        .await
        .expect("push desync");
    drop(frames);

    // node-a is gone entirely, not left at its old count.
    await_counts(&mut rx, &[("node-c", 1)]).await;
}

/// A persistently failing initial list must back off. `watcher` emits
/// `Ok(Event::Init)` before *every* list attempt and `StreamBackoff` resets on
/// any `Ok`, so leaving the pacing to `.default_backoff()` pins the retry at
/// the 800ms minimum forever — worse than the 10s poll this replaced.
#[tokio::test]
async fn node_pods_backs_off_when_the_initial_list_keeps_failing() {
    let (url, attempts) = mock_pods_failing_api().await;
    let (tx, _rx) = mpsc::channel(1024);
    let mut app = App::new(mock_cluster(&url), tx);
    app.spawn_node_pods_poll();

    // Three attempts is enough to see the interval grow: the delays are
    // 800ms and 1.6s before jitter, which only ever adds.
    let gaps = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            {
                let seen = attempts.lock().unwrap();
                if seen.len() >= 3 {
                    break vec![
                        seen[1].duration_since(seen[0]),
                        seen[2].duration_since(seen[1]),
                    ];
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("did not see three list attempts");

    assert!(
        gaps[0] >= std::time::Duration::from_millis(700),
        "first retry did not back off at all: {gaps:?}"
    );
    assert!(
        gaps[1] > gaps[0],
        "retry interval did not grow, backoff is being reset: {gaps:?}"
    );
}

/// The row cache's cleanup pass runs at the end of a rebuild; it must both
/// drop stale entries and hand the memory back. `retain` on its own keeps the
/// peak allocation, which is the whole point of the bound.
#[tokio::test]
async fn row_cache_releases_capacity_after_a_large_view_contracts() {
    let (mut app, _rx) = test_app();
    // A filter is what fills `cells`: every object it tests gets an entry,
    // including the ones it rejects.
    app.filter = "zzz-matches-nothing".into();
    for i in 0..2_000 {
        apply(
            &mut app,
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": format!("pod-{i:05}"),
                    "namespace": "default",
                    "resourceVersion": format!("{i}"),
                },
                "status": {"phase": "Running"},
            }),
        );
    }
    assert_eq!(app.row_count(), 0, "filter matches nothing");
    let peak = app.rows_cache.borrow().cells.capacity();
    assert!(peak >= 2_000, "cells cached an entry per tested object");

    for i in 0..1_990 {
        app.handle_msg(Msg::Deleted {
            generation: app.generation,
            key: format!("default/pod-{i:05}"),
        });
    }
    app.row_count();

    let cache = app.rows_cache.borrow();
    assert!(
        cache.cells.len() <= 10,
        "stale entries dropped, got {}",
        cache.cells.len()
    );
    assert!(
        cache.cells.capacity() < peak / 2,
        "capacity must be handed back, not just emptied: {} -> {}",
        peak,
        cache.cells.capacity()
    );
    let settled = cache.cells.capacity();
    drop(cache);

    // ...and then hold still. Shrinking to a capacity that immediately trips
    // the same check again would rehash the table on every single rebuild.
    for _ in 0..3 {
        app.invalidate_rows();
        app.row_count();
    }
    assert_eq!(
        app.rows_cache.borrow().cells.capacity(),
        settled,
        "the shrink must settle, not re-fire on every rebuild"
    );
}

/// Inject a watched object as the current generation would.
fn apply(app: &mut App, v: serde_json::Value) {
    let o = obj(v);
    app.handle_msg(Msg::Applied {
        generation: app.generation,
        key: row_key(&o),
        obj: Box::new(o),
    });
}

/// A Helm release storage Secret, encoded exactly like the real thing
/// (base64 -> base64 -> gzip -> JSON — see `crate::helm`), for exercising the
/// helm/helmhistory views without a live cluster.
fn helm_release_secret(release: &str, ns: &str, revision: i64, status: &str) -> serde_json::Value {
    helm_release_secret_deployed_at(release, ns, revision, status, "2024-01-15T10:30:00Z")
}

fn helm_release_secret_deployed_at(
    release: &str,
    ns: &str,
    revision: i64,
    status: &str,
    last_deployed: &str,
) -> serde_json::Value {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    let release_json = json!({
        "name": release,
        "namespace": ns,
        "version": revision,
        "info": {
            "status": status,
            "description": format!("revision {revision}"),
            "last_deployed": last_deployed,
            "notes": "thanks for installing",
        },
        "chart": {
            "metadata": { "name": "mychart", "version": "1.0.0", "appVersion": "2.0.0" },
        },
        "config": { "replicaCount": revision },
        "manifest": "apiVersion: v1\nkind: ConfigMap\n",
    })
    .to_string();

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(release_json.as_bytes()).unwrap();
    let helm_b64 = BASE64.encode(gz.finish().unwrap());
    let wire_b64 = BASE64.encode(helm_b64);

    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "helm.sh/release.v1",
        "metadata": {
            "name": format!("sh.helm.release.v1.{release}.v{revision}"),
            "namespace": ns,
            "labels": {
                "owner": "helm",
                "name": release,
                "version": revision.to_string(),
                "status": status,
            },
        },
        "data": { "release": wire_b64 },
    })
}

#[tokio::test]
async fn exact_alias_outranks_fuzzy_suggestions() {
    let (mut app, _rx) = test_app();
    // `hr` fuzzy-matches horizontalpodautoscalers too; the alias target
    // must still be the first suggestion.
    app.command = "hr".into();
    app.update_suggestions();
    let first = app.cmd_suggestions.first().expect("has suggestions");
    assert_eq!(first.label, "helmreleases");
    assert!(first.kind == SuggestKind::Resource);

    // A full plural typed exactly stays on top as well.
    app.command = "pods".into();
    app.update_suggestions();
    assert_eq!(app.cmd_suggestions[0].label, "pods");
}

#[tokio::test]
async fn shorter_label_wins_fuzzy_score_ties() {
    // Issue #164: skim scores only the matched characters, so `serv` scores
    // `services` and `serviceaccounts` identically; the alphabetical
    // tie-break buried `services`. Shorter label (denser match) wins ties.
    let (mut app, _rx) = test_app();
    app.cluster
        .catalog
        .extend(["serviceaccounts", "servicemonitors"].map(str::to_string));
    app.cluster.catalog.sort();

    app.command = "serv".into();
    app.update_suggestions();
    assert_eq!(app.cmd_suggestions[0].label, "services");

    // The empty browse list stays purely alphabetical.
    app.command.clear();
    app.update_suggestions();
    let labels: Vec<_> = app
        .cmd_suggestions
        .iter()
        .map(|s| s.label.as_str())
        .collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    assert_eq!(labels, sorted);
}

#[tokio::test]
async fn explicit_palette_search_bypasses_rbac_filter() {
    let (mut app, _rx) = test_app();
    app.cluster
        .catalog
        .extend(["nbgroups.netbird.io", "networkresources.netbird.io"].map(str::to_string));
    app.cluster.catalog.sort();
    app.cluster.catalog.dedup();
    app.rbac_allowed = Some(HashSet::from(["nbgroups".to_string()]));

    // Explicit group and shorthand queries search discovery even when a rules
    // review omits a resource that the authorizer still lets the user open.
    for query in ["netbird", "nb"] {
        app.command = query.into();
        app.update_suggestions();
        let labels: Vec<_> = app
            .cmd_suggestions
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert!(
            labels.contains(&"nbgroups.netbird.io"),
            "{query}: {labels:?}"
        );
        assert!(
            labels.contains(&"networkresources.netbird.io"),
            "{query}: {labels:?}"
        );
    }

    // The empty browse list remains RBAC-filtered.
    app.command.clear();
    app.update_suggestions();
    let labels: Vec<_> = app
        .cmd_suggestions
        .iter()
        .map(|s| s.label.as_str())
        .collect();
    assert!(labels.contains(&"nbgroups.netbird.io"), "{labels:?}");
    assert!(
        !labels.contains(&"networkresources.netbird.io"),
        "{labels:?}"
    );
}

#[test]
fn list_step_clamps_both_ends() {
    let mut s = ListState::default();
    list_step(&mut s, 3, true);
    assert_eq!(s.selected(), Some(1));
    list_step(&mut s, 3, true);
    list_step(&mut s, 3, true); // would be 3, clamps to 2
    assert_eq!(s.selected(), Some(2));
    list_step(&mut s, 3, false);
    assert_eq!(s.selected(), Some(1));
    list_step(&mut s, 3, false);
    list_step(&mut s, 3, false); // clamps at 0
    assert_eq!(s.selected(), Some(0));

    let mut empty = ListState::default();
    list_step(&mut empty, 0, true);
    assert_eq!(empty.selected(), None); // no-op on empty list
}

#[test]
fn scrollable_scroll_clamps() {
    let mut s = Scrollable {
        title: String::new(),
        lines: vec!["a".into(), "b".into(), "c".into()].into(),
        ..Default::default()
    };
    s.scroll_by(100);
    assert_eq!(s.scroll, 2); // last line index
    s.scroll_by(-100);
    assert_eq!(s.scroll, 0);
}

#[test]
fn scrollable_hscroll_clamps_to_widest_line() {
    let mut s = Scrollable {
        title: String::new(),
        lines: vec!["short".into(), "a much longer line".into()].into(),
        ..Default::default()
    };
    s.scroll_h(100);
    assert_eq!(s.hscroll, "a much longer line".len() - 1); // widest line - 1
    s.scroll_h(-100);
    assert_eq!(s.hscroll, 0);
}

#[test]
fn scrollable_wrap_disables_hscroll_and_resets_offset() {
    let mut s = Scrollable {
        title: String::new(),
        lines: vec!["a much longer line".into()].into(),
        ..Default::default()
    };
    s.scroll_h(5);
    assert_eq!(s.hscroll, 5);
    assert!(s.toggle_wrap()); // wrap on
    assert_eq!(s.hscroll, 0); // snapped back to the left margin
    s.scroll_h(5); // no-op while wrapping
    assert_eq!(s.hscroll, 0);
    assert!(!s.toggle_wrap()); // wrap off again
}

#[tokio::test]
async fn move_selection_from_none_lands_on_first_row_not_second() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["a", "b", "c"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    app.table_state.select(None); // simulate no selection at all
    app.move_selection(1); // Down, with nothing selected yet
    assert_eq!(app.table_state.selected(), Some(0), "must not skip row 0");
}

#[tokio::test]
async fn ctrl_f_and_ctrl_b_page_by_the_drawn_viewport_height() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in 0..25 {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": format!("p{n:02}"), "namespace": "default"}}),
        );
    }
    app.table_state.select(Some(0));
    app.table_page_rows = 8;

    app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(8));
    app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(16));
    app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(24), "clamps at the end");

    app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(16));
    app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
    app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
    app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(0), "clamps at the top");

    app.handle_key(press(KeyCode::PageDown)).unwrap();
    assert_eq!(app.table_state.selected(), Some(8));
    app.handle_key(press(KeyCode::PageUp)).unwrap();
    assert_eq!(app.table_state.selected(), Some(0));

    app.table_page_rows = 20;
    app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(20));

    // ctrl-alt-f is a distinct chord left to plugins, not a page move.
    let ctrl_alt_f = KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    app.handle_key(ctrl_alt_f).unwrap();
    assert_eq!(app.table_state.selected(), Some(20));
}

#[tokio::test]
async fn ctrl_f_and_ctrl_b_page_document_views() {
    let (mut app, _rx) = test_app();
    app.detail = Scrollable {
        title: "document".into(),
        lines: (0..100).map(|i| format!("line {i}")).collect(),
        ..Default::default()
    };

    for mode in [Mode::Detail, Mode::Diff, Mode::Events] {
        app.mode = mode;
        app.detail.scroll = 0;

        app.handle_key(press(KeyCode::PageDown)).unwrap();
        assert_eq!(app.detail.scroll, 20);
        app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
        assert_eq!(app.detail.scroll, 40);
        app.handle_key(press(KeyCode::PageUp)).unwrap();
        assert_eq!(app.detail.scroll, 20);
        app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
        assert_eq!(app.detail.scroll, 0);
    }

    // Keep ctrl-alt-f distinct from the exact ctrl-f built-in.
    let ctrl_alt_f = KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    app.handle_key(ctrl_alt_f).unwrap();
    assert_eq!(app.detail.scroll, 0);
}

#[tokio::test]
async fn document_scroll_keeps_the_last_page_filled() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut app, _rx) = test_app();
    app.mode = Mode::Detail;
    app.detail = Scrollable {
        title: "document".into(),
        lines: (0..30).map(|i| format!("line {i}")).collect(),
        ..Default::default()
    };

    // A 24-row terminal leaves 13 content rows after the standard header,
    // footer, prompt, and document border.
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

    app.handle_key(press(KeyCode::Char('G'))).unwrap();
    assert_eq!(app.detail.scroll, 17, "bottom keeps a full viewport");
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    assert_eq!(app.detail.scroll, 17, "cannot scroll past the last page");
    app.handle_key(press(KeyCode::Char('k'))).unwrap();
    assert_eq!(
        app.detail.scroll, 16,
        "up moves immediately from the bottom"
    );

    app.detail = Scrollable {
        title: "short document".into(),
        lines: (0..10).map(|i| format!("line {i}")).collect(),
        ..Default::default()
    };
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    app.handle_key(press(KeyCode::Char('G'))).unwrap();
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    assert_eq!(app.detail.scroll, 0, "a short document never scrolls");

    // A single source line may occupy more display rows than the viewport.
    // ANSI sequences consume zero columns in both the cached layout and the
    // rendered rows, so they cannot hide the line's tail from navigation.
    app.detail = Scrollable {
        title: "wrapped document".into(),
        lines: vec![format!(
            "{}{}LAST\x1b[0m",
            "\x1b[31m".repeat(20),
            "x".repeat(18 * 19)
        )]
        .into(),
        wrap: true,
        ..Default::default()
    };
    let mut term = Terminal::new(TestBackend::new(20, 24)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    app.handle_key(press(KeyCode::Char('G'))).unwrap();
    assert_eq!(app.detail.scroll, 7, "bottom uses wrapped display rows");
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    assert_eq!(app.detail.scroll, 7, "wrapped bottom remains clamped");

    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let buffer = term.backend().buffer();
    let screen = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("LAST"),
        "last wrapped row missing:\n{screen}"
    );

    app.detail = Scrollable {
        title: "tabbed document".into(),
        lines: vec![format!(
            "{}{}TABS",
            "\t".repeat(18 * 10),
            "x".repeat(18 * 19)
        )]
        .into(),
        wrap: true,
        ..Default::default()
    };
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    app.handle_key(press(KeyCode::Char('G'))).unwrap();
    assert_eq!(
        app.detail.scroll, 7,
        "tabs must not inflate the wrapped bottom offset"
    );
}

#[tokio::test]
async fn switching_kind_resets_stale_selection_to_top() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["a", "b", "c"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    app.table_state.select(Some(2)); // simulate cursor left on row 2

    app.switch_kind("deployments");
    assert_eq!(
        app.table_state.selected(),
        Some(0),
        "a fresh view must start with its first row selected, not a stale index"
    );
}

#[tokio::test]
async fn namespace_filter_selects_best_match_not_all() {
    let (mut app, _rx) = test_app();
    app.ns_list = vec![
        "<all>".into(),
        "default".into(),
        "kube-system".into(),
        "prod".into(),
    ];
    app.ns_filter.clear();
    app.ns_state.select(Some(0));
    app.mode = Mode::Namespaces;

    for c in "sys".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    // "kube-system" is the only real match — it should be under the
    // cursor, not the pinned "<all>" at index 0.
    let filtered = app.filtered_namespaces();
    let selected = app.ns_state.selected().and_then(|i| filtered.get(i));
    assert_eq!(selected.map(String::as_str), Some("kube-system"));

    // Clearing back to an empty filter returns the default to <all>.
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    assert_eq!(app.ns_state.selected(), Some(0));
}

#[tokio::test]
async fn filter_match_indices_highlight_matched_chars() {
    let (mut app, _rx) = test_app();
    assert_eq!(app.filter_match_indices("kube-httpcache-0"), None); // no filter

    app.filter = "khc".into();
    let idx = app.filter_match_indices("kube-httpcache-0").unwrap();
    // "k", "h", "c" fuzzy-match in order somewhere in the name.
    assert_eq!(idx.len(), 3);
    assert!(idx.is_sorted());

    app.filter = "zzz".into();
    assert_eq!(app.filter_match_indices("kube-httpcache-0"), None); // no match
}

#[tokio::test]
async fn table_cell_cache_invalidates_on_apply() {
    let (mut app, _rx) = test_app();
    app.kind_plural = "pods".into();
    app.refresh_view_spec();
    apply(
        &mut app,
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web",
                "namespace": "default",
                "resourceVersion": "1"
            },
            "status": {"phase": "Pending"}
        }),
    );
    {
        let rows = app.rows();
        app.ensure_table_cell_cache(&rows);
        let key = row_key(rows[0]);
        let cache = app.table_cell_cache();
        let (cells, _) = cache.get(&key).unwrap();
        assert_eq!(cells[2], "Pending");
    }

    apply(
        &mut app,
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web",
                "namespace": "default",
                "resourceVersion": "2"
            },
            "status": {"phase": "Running"}
        }),
    );
    let rows = app.rows();
    app.ensure_table_cell_cache(&rows);
    let key = row_key(rows[0]);
    let cache = app.table_cell_cache();
    let (cells, _) = cache.get(&key).unwrap();
    assert_eq!(cells[2], "Running");
}

#[tokio::test]
async fn palette_merges_commands_with_resources() {
    let (mut app, _rx) = test_app();

    // Empty query lists resources only, so `:`⏎ never fires a command.
    app.command.clear();
    app.update_suggestions();
    assert!(
        app.cmd_suggestions
            .iter()
            .all(|s| s.kind == SuggestKind::Resource)
    );

    // Typing a command name surfaces it (this was the reported bug: `ctx`
    // used to show nothing).
    app.command = "ctx".into();
    app.update_suggestions();
    assert!(
        app.cmd_suggestions
            .iter()
            .any(|s| s.kind == SuggestKind::Command && s.label == "ctx")
    );

    // Aliases fuzzy-match too, but the canonical label is shown.
    app.command = "dash".into();
    app.update_suggestions();
    assert!(
        app.cmd_suggestions
            .iter()
            .any(|s| s.kind == SuggestKind::Command && s.label == "pulse")
    );
}

#[tokio::test]
async fn palette_command_dispatch() {
    let (mut app, _rx) = test_app();
    assert!(app.run_palette_command("q")); // alias for quit
    assert!(app.should_quit);

    let (mut app, _rx) = test_app();
    assert!(app.run_palette_command("contexts")); // alias resolves
    assert!(!app.run_palette_command("pods")); // resource kind, not a command
    assert!(!app.run_palette_command("")); // empty is never a command
}

#[tokio::test]
async fn palette_opens_from_document_views() {
    // Issue #148: `:` in a describe/YAML view did nothing — the palette was
    // only bound in the table.
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.set_return_mode();
    app.mode = Mode::Detail;

    // `:` opens the palette; esc returns to the detail view, not the table.
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    assert_eq!(app.mode, Mode::Command);
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Detail);

    // Dispatching a kind switch leaves the detail view for the new table.
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "deployments".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.kind_plural, "deployments");

    // Logs view binds `:` too, and esc from the palette lands back on it.
    app.mode = Mode::Logs;
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    assert_eq!(app.mode, Mode::Command);
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Logs);
}

#[tokio::test]
async fn navigation_views_share_palette_and_help_shortcuts() {
    let (mut app, _rx) = test_app();

    for source in [
        Mode::Table,
        Mode::Detail,
        Mode::Logs,
        Mode::Containers,
        Mode::Confirm,
        Mode::Pulse,
        Mode::Xray,
        Mode::Explain,
        Mode::Timeline,
        Mode::Gitops,
        Mode::Diff,
        Mode::Events,
        Mode::FluxMenu,
        Mode::TransferMenu,
        Mode::PortForwards,
        Mode::Skins,
        Mode::Snapshots,
        Mode::Fleet,
        Mode::Find,
    ] {
        app.mode = source;
        app.handle_key(press(KeyCode::Char(':'))).unwrap();
        assert_eq!(app.mode, Mode::Command, "palette from {source:?}");
        assert_eq!(app.palette_return, source);
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, source, "palette return to {source:?}");

        app.handle_key(press(KeyCode::Char('?'))).unwrap();
        assert_eq!(app.mode, Mode::Help, "help from {source:?}");
        assert_eq!(app.help_return, source);
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, source, "help return to {source:?}");
    }

    app.mode = Mode::Help;
    app.help_return = Mode::Explain;
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    assert_eq!(app.mode, Mode::Command);
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Help);
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.mode, Mode::Explain);

    app.mode = Mode::Namespaces;
    app.ns_filter.clear();
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.mode, Mode::Namespaces);
    assert_eq!(app.ns_filter, ":?");
}

#[tokio::test]
async fn palette_from_help_cleans_up_the_underlying_stream() {
    for source in [Mode::Logs, Mode::Events] {
        let (mut app, _rx) = test_app();
        let log_gen = app.log_gen;
        let event_gen = app.event_gen;
        match source {
            Mode::Logs => app.log_tasks.push(tokio::spawn(std::future::pending())),
            Mode::Events => {
                app.event_task = Some(tokio::spawn(std::future::pending()));
            }
            _ => unreachable!(),
        }
        app.mode = source;

        app.handle_key(press(KeyCode::Char('?'))).unwrap();
        app.handle_key(press(KeyCode::Char(':'))).unwrap();
        for c in "skin".chars() {
            app.handle_key(press(KeyCode::Char(c))).unwrap();
        }
        app.handle_key(press(KeyCode::Enter)).unwrap();

        assert_eq!(app.mode, Mode::Skins);
        assert_eq!(app.help_return, Mode::Table);
        match source {
            Mode::Logs => {
                assert!(app.log_tasks.is_empty());
                assert_eq!(app.log_gen, log_gen + 1);
            }
            Mode::Events => {
                assert!(app.event_task.is_none());
                assert_eq!(app.event_gen, event_gen + 1);
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn explain_evidence_escape_walks_back_to_the_table() {
    for (key, evidence_mode) in [('l', Mode::Logs), ('E', Mode::Events)] {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {"name": "api-1", "namespace": "prod"},
                "spec": {"containers": [{"name": "app"}]}
            }),
        );
        app.table_state.select(Some(0));

        app.handle_key(press(KeyCode::Char('X'))).unwrap();
        assert_eq!(app.mode, Mode::Explain);
        app.handle_key(press(KeyCode::Char(key))).unwrap();
        assert_eq!(app.mode, evidence_mode);

        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::Explain);
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.table_state.selected(), Some(0));
    }
}

#[tokio::test]
async fn skin_palette_command_opens_picker() {
    let (mut app, _rx) = test_app();
    assert!(app.run_palette_command("skin"));
    assert_eq!(app.mode, Mode::Skins);
    assert_eq!(
        app.skin_list.first().map(String::as_str),
        Some("catppuccin-mocha")
    );

    app.mode = Mode::Table;
    assert!(app.run_palette_command("skin no-such-skin"));
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash_err);
    assert!(app.flash.contains("unknown skin"), "{}", app.flash);
}

#[tokio::test]
async fn diff_falls_back_to_session_previous_revision() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    let dep = |rv: &str, replicas: i64| {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default", "resourceVersion": rv},
            "spec": {"replicas": replicas}
        })
    };
    apply(&mut app, dep("1", 1));
    apply(&mut app, dep("2", 3));
    app.table_state.select(Some(0));
    app.open_diff();
    assert_eq!(app.mode, Mode::Diff);
    assert!(app.detail.title.contains("session"), "{}", app.detail.title);
    assert!(
        app.detail
            .lines
            .iter()
            .any(|l| l.starts_with('-') && l.contains("replicas: 1"))
    );
    assert!(
        app.detail
            .lines
            .iter()
            .any(|l| l.starts_with('+') && l.contains("replicas: 3"))
    );
    // resourceVersion churn must not appear as diff noise.
    assert!(
        !app.detail
            .lines
            .iter()
            .any(|l| l.contains("resourceVersion"))
    );
}

#[tokio::test]
async fn diff_without_any_baseline_warns() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    apply(
        &mut app,
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default", "resourceVersion": "1"},
            "spec": {"replicas": 1}
        }),
    );
    app.table_state.select(Some(0));
    app.open_diff();
    assert_ne!(app.mode, Mode::Diff);
    assert!(app.flash.contains("nothing to diff"), "{}", app.flash);
}

#[tokio::test]
async fn diff_prefers_last_applied_when_present() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    let last = r#"{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","namespace":"default"},"spec":{"replicas":2}}"#;
    apply(
        &mut app,
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {
                "name": "web", "namespace": "default", "resourceVersion": "1",
                "annotations": {"kubectl.kubernetes.io/last-applied-configuration": last}
            },
            "spec": {"replicas": 3}
        }),
    );
    app.table_state.select(Some(0));
    app.open_diff();
    assert_eq!(app.mode, Mode::Diff);
    assert!(
        app.detail.title.contains("last-applied"),
        "{}",
        app.detail.title
    );
}

#[tokio::test]
async fn pulse_and_xray_warns_surface_as_flash() {
    let (mut app, _rx) = test_app();
    let data = crate::store::Pulse {
        warn: Some("listing pods: 403".into()),
        ..Default::default()
    };
    let claim = app.claim_status("pulse — cluster health…");
    app.handle_msg(Msg::PulseData {
        generation: app.generation,
        claim,
        data,
    });
    assert!(app.flash_err);
    assert!(app.flash.contains("incomplete"), "{}", app.flash);

    app.flash_err = false;
    let claim = app.claim_status("xray: pods…");
    app.handle_msg(Msg::XrayData {
        generation: app.generation,
        claim,
        items: Vec::new(),
        warn: Some("listing replicasets: 403".into()),
    });
    assert!(app.flash_err);
    assert!(app.flash.contains("incomplete"), "{}", app.flash);
}

#[tokio::test]
async fn recurring_dashboard_poll_can_warn_after_an_initial_success() {
    let (mut app, _rx) = test_app();
    let claim = app.claim_status("pulse — cluster health…");

    app.handle_msg(Msg::PulseData {
        generation: app.generation,
        claim,
        data: crate::store::Pulse::default(),
    });
    assert!(app.flash.is_empty(), "{}", app.flash);

    app.handle_msg(Msg::PulseData {
        generation: app.generation,
        claim,
        data: crate::store::Pulse {
            warn: Some("listing pods: 403".into()),
            ..Default::default()
        },
    });
    assert!(app.flash_err);
    assert!(app.flash.contains("pulse is incomplete"), "{}", app.flash);

    // The next complete poll clears the warning while retaining the recurring
    // claim for future polls.
    app.handle_msg(Msg::PulseData {
        generation: app.generation,
        claim,
        data: crate::store::Pulse::default(),
    });
    assert!(app.flash.is_empty(), "{}", app.flash);

    let claim = app.claim_status("xray: pods…");
    app.handle_msg(Msg::XrayData {
        generation: app.generation,
        claim,
        items: Vec::new(),
        warn: None,
    });
    app.handle_msg(Msg::XrayData {
        generation: app.generation,
        claim,
        items: Vec::new(),
        warn: Some("listing replicasets: 403".into()),
    });
    assert!(app.flash_err);
    assert!(app.flash.contains("xray is incomplete"), "{}", app.flash);
}

#[tokio::test]
async fn metrics_error_is_stored_and_cleared_by_next_success() {
    let (mut app, _rx) = test_app();
    app.handle_msg(Msg::MetricsError {
        generation: app.generation,
        error: "metrics-server 500".into(),
    });
    assert_eq!(app.metrics_error.as_deref(), Some("metrics-server 500"));

    app.handle_msg(Msg::Metrics {
        generation: app.generation,
        data: HashMap::new(),
        containers: HashMap::new(),
    });
    assert_eq!(app.metrics_error, None);
}

#[tokio::test]
async fn saved_forwards_show_as_stopped_until_running() {
    let (mut app, _rx) = test_app();
    app.forwards_cfg = vec![
        crate::config::Forward {
            name: "argocd".into(),
            target: "svc/argocd-server".into(),
            namespace: "argocd".into(),
            ports: "8080:443".into(),
            autostart: false,
            contexts: vec![],
        },
        crate::config::Forward {
            name: "db".into(),
            target: "svc/postgres".into(),
            namespace: "data".into(),
            ports: "5432:5432".into(),
            autostart: false,
            contexts: vec![],
        },
    ];
    let stopped: Vec<&str> = app
        .stopped_configured_forwards()
        .iter()
        .map(|(_, f)| f.name.as_str())
        .collect();
    assert_eq!(stopped, ["argocd", "db"]);

    // A live child linked by name moves the entry out of the stopped tail.
    app.port_forwards.push(PortForward {
        config_name: Some("argocd".into()),
        ns: "argocd".into(),
        target: "svc/argocd-server".into(),
        ports: "8080:443".into(),
        child: spawn_test_child("sleep", "5"),
    });
    assert!(app.forward_running("argocd"));
    let stopped: Vec<&str> = app
        .stopped_configured_forwards()
        .iter()
        .map(|(_, f)| f.name.as_str())
        .collect();
    assert_eq!(stopped, ["db"]);

    // The :pf list is running + stopped; x on the running entry stops it and
    // the entry reappears in the stopped tail.
    app.open_port_forwards();
    assert_eq!(app.pf_state.selected(), Some(0));
    app.key_port_forwards(press(KeyCode::Char('x')));
    assert!(app.port_forwards.is_empty());
    assert_eq!(app.stopped_configured_forwards().len(), 2);
    assert_eq!(app.pf_state.selected(), Some(0), "clamped to combined list");
}

#[tokio::test]
async fn port_forward_prompt_prefills_first_exposed_port() {
    let (mut app, _rx) = test_app();
    app.switch_kind("services");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "web", "namespace": "default", "resourceVersion": "1"},
            "spec": {"ports": [{"port": 8080}, {"port": 9090}]}
        }),
    );
    app.table_state.select(Some(0));
    app.request_port_forward();
    assert_eq!(app.mode, Mode::Prompt);
    assert_eq!(
        app.prompt_input, "8080:8080",
        "first service port, LOCAL:REMOTE"
    );

    // Pods take the first declared container port.
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "db", "namespace": "default", "resourceVersion": "1"},
            "spec": {"containers": [{"name": "pg", "ports": [{"containerPort": 5432}]}]}
        }),
    );
    app.table_state.select(Some(0));
    app.request_port_forward();
    assert_eq!(app.prompt_input, "5432:5432");

    // No declared ports: the prompt stays empty as before.
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "aux", "namespace": "default", "resourceVersion": "1"},
            "spec": {"containers": [{"name": "c"}]}
        }),
    );
    app.table_state.select(Some(0));
    app.request_port_forward();
    assert_eq!(app.prompt_input, "");
}

#[test]
fn forward_context_matching_and_validation() {
    let f = crate::config::Forward {
        name: "x".into(),
        target: "svc/x".into(),
        namespace: "ns".into(),
        ports: "8080:80".into(),
        autostart: true,
        contexts: vec!["home".into()],
    };
    assert!(f.matches_context("home"));
    assert!(!f.matches_context("prod"));
    let all = crate::config::Forward {
        contexts: vec![],
        ..f.clone()
    };
    assert!(all.matches_context("anything"));

    let warnings = crate::config::forward_warnings(&[
        crate::config::Forward {
            name: "".into(),
            ..f.clone()
        },
        crate::config::Forward {
            name: "badports".into(),
            ports: "eighty:80".into(),
            ..f.clone()
        },
        f.clone(),
        f.clone(), // duplicate name
    ]);
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("empty name")));
    assert!(warnings.iter().any(|w| w.contains("not LOCAL:REMOTE")));
    assert!(warnings.iter().any(|w| w.contains("duplicate name")));
}

#[tokio::test]
async fn mouse_click_selects_row_header_click_sorts_wheel_moves() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let pod = |name: &str, restarts: i64| {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "status": {"phase": "Running", "containerStatuses":
                [{"ready": true, "restartCount": restarts, "state": {"running": {}}}]}
        })
    };
    apply(&mut app, pod("a", 5));
    apply(&mut app, pod("b", 1));
    app.table_state.select(Some(0));

    // A frame must render first — that's what records the hit geometry.
    let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let hit = app.table_hit.borrow().clone().expect("geometry recorded");

    let click = |column: u16, row: u16| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // Click the second data row → it becomes the selection.
    app.handle_mouse(click(hit.x_min + 3, hit.rows_y + 1))
        .unwrap();
    assert_eq!(app.table_state.selected(), Some(1));

    // Click the RESTARTS header → sort by it; again → flip direction.
    let ridx = app
        .display_headers()
        .iter()
        .position(|h| h == "RESTARTS")
        .unwrap();
    let (sx, _, _) = hit
        .cols
        .iter()
        .copied()
        .find(|(_, _, i)| *i == ridx)
        .unwrap();
    app.handle_mouse(click(sx, hit.header_y)).unwrap();
    assert_eq!(app.sort_column, Some(ridx));
    assert!(!app.sort_desc);
    app.handle_mouse(click(sx, hit.header_y)).unwrap();
    assert!(app.sort_desc);

    // Wheel scroll maps to the mode's own up/down keys (3 per notch).
    app.table_state.select(Some(0));
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert_eq!(app.table_state.selected(), Some(1), "clamped at the end");

    // A click outside the table (e.g. the header panel) is ignored.
    app.handle_mouse(click(0, 0)).unwrap();
    assert_eq!(app.table_state.selected(), Some(1));
}

#[tokio::test]
async fn horizontal_column_scroll_anchors_name_and_clamps() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "a", "namespace": "default"},
            "status": {"phase": "Running"}
        }),
    );
    app.table_state.select(Some(0));

    let headers = app.display_headers();
    assert_eq!(headers[0], "NAME");
    let scrollable = headers.len() - 1;

    // ← at the left edge is a no-op; → hides the column after NAME.
    app.handle_key(press(KeyCode::Left)).unwrap();
    assert_eq!(app.col_offset, 0);
    app.handle_key(press(KeyCode::Right)).unwrap();
    assert_eq!(app.col_offset, 1);

    // → clamps so the last scrollable column stays visible.
    for _ in 0..headers.len() {
        app.handle_key(press(KeyCode::Right)).unwrap();
    }
    assert_eq!(app.col_offset, scrollable - 1);

    // The rendered geometry skips the hidden columns: NAME (0) stays
    // anchored, then only the last scrollable column follows.
    let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let hit = app.table_hit.borrow().clone().expect("geometry recorded");
    let cols: Vec<usize> = hit.cols.iter().map(|&(_, _, i)| i).collect();
    assert_eq!(cols, vec![0, headers.len() - 1]);

    // Rebuilding the view spec (view switch, wide toggle) resets the scroll.
    app.refresh_view_spec();
    assert_eq!(app.col_offset, 0);
}

#[tokio::test]
async fn document_views_release_mouse_capture_for_text_selection() {
    let (mut app, _rx) = test_app();
    assert!(app.wants_mouse_capture(), "table keeps capture");

    // Every full-screen text view releases capture so click-drag selects text
    // natively (#133), including the filter overlays that keep it on screen.
    // Logs included: the alternate-scroll bursts that used to throw the view
    // off (#152) are repaired in `crate::altscroll`, not by holding capture.
    for mode in [
        Mode::Detail,
        Mode::Diff,
        Mode::Events,
        Mode::Logs,
        Mode::Help,
        Mode::DocFilter,
        Mode::LogFilter,
    ] {
        app.mode = mode;
        assert!(!app.wants_mouse_capture(), "{mode:?} releases capture");
    }

    // Interactive pickers and dashboards still want clicks/wheel captured.
    for mode in [Mode::Table, Mode::Namespaces, Mode::Pulse, Mode::Confirm] {
        app.mode = mode;
        assert!(app.wants_mouse_capture(), "{mode:?} keeps capture");
    }
}

#[tokio::test]
async fn notify_toggles_a_background_watch_per_object() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "web", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));

    assert!(app.run_palette_command("notify"));
    assert_eq!(app.notify_tasks.len(), 1);
    assert!(app.notify_tasks.contains_key("pods/default/web"));
    assert!(app.flash.contains("notify on"), "{}", app.flash);

    // Same command on the same row toggles it off.
    assert!(app.run_palette_command("notify"));
    assert!(app.notify_tasks.is_empty());
    assert!(app.flash.contains("notify off"), "{}", app.flash);
}

#[tokio::test]
async fn notify_survives_view_switches() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "web", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    assert!(app.run_palette_command("notify"));
    assert_eq!(app.notify_tasks.len(), 1);

    // bump_generation (any view switch) aborts self.tasks — the notify watch
    // must not be among them.
    app.switch_kind("services");
    assert_eq!(app.notify_tasks.len(), 1);
    assert!(!app.notify_tasks["pods/default/web"].is_finished());
}

#[tokio::test]
async fn notify_msg_flashes_and_queues_bell() {
    let (mut app, _rx) = test_app();
    app.handle_msg(Msg::Notify("pod/web: Ready True → False".into()));
    assert!(!app.flash_err);
    assert!(app.flash.contains("pod/web"), "{}", app.flash);
    assert_eq!(
        app.take_notification().as_deref(),
        Some("pod/web: Ready True → False")
    );
    assert_eq!(app.take_notification(), None, "consumed once");
}

#[tokio::test]
async fn notification_bursts_coalesce_into_one_delivery() {
    // Notification sinks rate-limit rapid-fire messages (herdr drops all but
    // the first of a burst) — everything pending in one frame batch must
    // leave as a single bounded delivery.
    let (mut app, _rx) = test_app();
    app.handle_msg(Msg::Notify("pod/a: Ready True → False".into()));
    app.handle_msg(Msg::Notify("pod/a: deleted".into()));
    assert_eq!(
        app.take_notification().as_deref(),
        Some("pod/a: Ready True → False · pod/a: deleted")
    );
    assert_eq!(app.take_notification(), None);

    for i in 0..100 {
        app.handle_msg(Msg::Notify(format!("pod/pod-{i}: restarts 0 → 1")));
    }
    let text = app.take_notification().unwrap();
    assert!(text.chars().count() <= 300, "bounded: {}", text.len());
}

#[test]
fn notification_sequences_follow_the_configured_protocol() {
    use crate::config::{NotifyConfig, notify_warnings};

    let dflt = NotifyConfig::default();
    let seq = notification_sequence("pod/web: Ready", &dflt);
    assert!(seq.starts_with('\x07'), "bell on by default");
    assert!(
        seq.contains("\x1b]777;notify;sofka;pod/web: Ready\x07"),
        "titled osc777 is the default: {seq:?}"
    );
    assert!(!seq.contains("]9;"), "one protocol by default: {seq:?}");

    let osc9 = NotifyConfig {
        bell: false,
        desktop: "osc9".into(),
        ..NotifyConfig::default()
    };
    assert_eq!(notification_sequence("hi", &osc9), "\x1b]9;sofka: hi\x07");
    let osc777 = NotifyConfig {
        bell: false,
        desktop: "osc777".into(),
        ..NotifyConfig::default()
    };
    assert_eq!(
        notification_sequence("hi", &osc777),
        "\x1b]777;notify;sofka;hi\x07"
    );

    let both = NotifyConfig {
        bell: false,
        desktop: "both".into(),
        ..NotifyConfig::default()
    };
    let seq = notification_sequence("hi", &both);
    assert!(seq.contains("]9;") && seq.contains("]777;"), "{seq:?}");

    let off = NotifyConfig {
        bell: false,
        desktop: "off".into(),
        ..NotifyConfig::default()
    };
    assert!(notification_sequence("hi", &off).is_empty());

    // Control characters in object-derived text can't smuggle sequences.
    assert_eq!(
        notification_sequence("a\x1b]0;evil\x07b", &osc777),
        "\x1b]777;notify;sofka;a]0;evilb\x07"
    );

    // Unknown protocol behaves as the default (it warned at config load).
    let bad = NotifyConfig {
        bell: false,
        desktop: "growl".into(),
        ..NotifyConfig::default()
    };
    assert!(notification_sequence("hi", &bad).contains("]777;"));
    assert_eq!(notify_warnings(&bad).len(), 1);
    assert!(notify_warnings(&dflt).is_empty());
}

#[test]
fn notify_command_resolution_prefers_explicit_then_herdr() {
    use super::notify::notification_command;
    use crate::config::{NotifyConfig, notify_warnings};

    // No command, not in herdr → nothing to run.
    let dflt = NotifyConfig::default();
    assert_eq!(notification_command(&dflt, false, "hi"), None);

    // In a herdr pane, notifications auto-route through herdr's toast CLI.
    assert_eq!(
        notification_command(&dflt, true, "pod/web: Ready"),
        Some(vec![
            "herdr".into(),
            "notification".into(),
            "show".into(),
            "sofka".into(),
            "--body".into(),
            "pod/web: Ready".into(),
        ])
    );

    // An explicit command wins over herdr; $MESSAGE substitutes as a whole
    // argument (never spliced into a shell string).
    let explicit = NotifyConfig {
        command: vec!["notify-send".into(), "sofka".into(), "$MESSAGE".into()],
        ..NotifyConfig::default()
    };
    assert_eq!(
        notification_command(&explicit, true, "a; rm -rf /"),
        Some(vec![
            "notify-send".into(),
            "sofka".into(),
            "a; rm -rf /".into(),
        ])
    );

    // Without a $MESSAGE placeholder, the message appends as the last arg.
    let bare = NotifyConfig {
        command: vec!["terminal-notifier".into(), "-title".into(), "sofka".into()],
        ..NotifyConfig::default()
    };
    let argv = notification_command(&bare, false, "hi").unwrap();
    assert_eq!(argv.last().map(String::as_str), Some("hi"));

    // An empty executable is invalid: warned at load, ignored at delivery
    // (herdr auto-detection still applies).
    let empty = NotifyConfig {
        command: vec!["".into()],
        ..NotifyConfig::default()
    };
    assert_eq!(notify_warnings(&empty).len(), 1);
    assert!(notification_command(&empty, false, "hi").is_none());
    assert!(notification_command(&empty, true, "hi").is_some_and(|v| v[0] == "herdr"));
}

#[tokio::test]
async fn find_opens_picker_and_enter_navigates_to_the_object() {
    let (mut app, _rx) = test_app();
    app.switch_kind("services");

    // Bare :find is a usage hint, not a search.
    assert!(app.run_palette_command("find"));
    assert!(app.flash.contains("usage"), "{}", app.flash);

    assert!(app.run_palette_command("find web"));
    assert_eq!(app.mode, Mode::Find);
    assert_eq!(app.find_query, "web");

    app.handle_msg(Msg::FindResults {
        generation: app.generation,
        claim: current_claim(&app),
        query: "web".into(),
        items: vec![
            crate::store::FindItem {
                plural: "pods".into(),
                ns: "default".into(),
                name: "web-1".into(),
            },
            crate::store::FindItem {
                plural: "deployments".into(),
                ns: "default".into(),
                name: "web".into(),
            },
        ],
        warn: None,
    });
    assert_eq!(app.find_state.selected(), Some(0));
    assert!(app.flash.contains("2 hit(s)"), "{}", app.flash);

    app.key_find(press(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=web-1"));

    // Incomplete sweeps say so instead of pretending the list is exhaustive.
    // A fresh sweep, because navigating away retired the first one's claim.
    assert!(app.run_palette_command("find web"));
    app.handle_msg(Msg::FindResults {
        generation: app.generation,
        claim: current_claim(&app),
        query: "web".into(),
        items: Vec::new(),
        warn: Some("2 kind(s) could not be listed".into()),
    });
    assert!(app.flash_err);
    assert!(app.flash.contains("incomplete"), "{}", app.flash);
}

#[tokio::test]
async fn panic_msg_flashes_regardless_of_generation() {
    let (mut app, _rx) = test_app();
    app.generation = 7; // a Panic has no generation tag and must never be dropped as stale
    app.handle_msg(Msg::Panic("boom".into()));
    assert!(app.flash_err);
    assert!(app.flash.contains("boom"), "{}", app.flash);
    assert_eq!(app.last_error.as_deref(), Some("boom"));
}

#[tokio::test]
async fn logs_pause_freezes_and_survives_new_lines() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    app.return_mode = Mode::Table;
    // Simulate a drawn frame: 100 display rows, 40-high viewport → the
    // follow anchor (and deepest offset) is row 60.
    app.logs.follow = true;
    app.logs.view.scroll = 60;
    app.logs.viewport_rows = 100;
    app.logs.viewport_h = 40;

    // Scroll up → autoscroll stops and the offset steps back by one row.
    app.handle_key(press(KeyCode::Char('k'))).unwrap();
    assert!(!app.logs.follow);
    assert_eq!(app.logs.view.scroll, 59);

    // Lines keep streaming while paused; the frozen offset must not drift.
    for i in 0..500 {
        app.handle_msg(Msg::LogLines {
            generation: app.log_gen,
            lines: vec![format!("line {i}")],
        });
    }
    assert!(!app.logs.follow);
    assert_eq!(app.logs.view.scroll, 59);

    // `g` goes to the top and stays there (no snap-back to the bottom).
    app.handle_key(press(KeyCode::Char('g'))).unwrap();
    assert!(!app.logs.follow);
    assert_eq!(app.logs.view.scroll, 0);

    // `G` re-arms autoscroll (the next draw will re-anchor to the bottom).
    app.handle_key(press(KeyCode::Char('G'))).unwrap();
    assert!(app.logs.follow);

    // Down-scroll is clamped to the deepest offset (rows - height = 60), so
    // it can't overshoot past the bottom-pinned last page.
    app.logs.view.scroll = 60;
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    assert!(!app.logs.follow);
    assert_eq!(app.logs.view.scroll, 60);
}

/// A fast wheel-down burst in the log view reaches us as alternate-scroll
/// escape sequences, split at the `ESC` byte when a read boundary lands there.
/// Repaired (as the run loop does) they must stay Down keys clamped to the
/// bottom of the buffer, never a stray Esc that closes the view (#152).
#[tokio::test]
async fn rapid_log_wheel_down_clamps_without_leaving_logs() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    app.return_mode = Mode::Table;
    app.logs.follow = true;
    app.logs.view.scroll = 60;
    app.logs.viewport_rows = 100;
    app.logs.viewport_h = 40;

    app.handle_key(press(KeyCode::Char('s'))).unwrap();
    app.handle_key(press(KeyCode::Up)).unwrap();
    assert!(!app.logs.follow);
    assert_eq!(app.logs.view.scroll, 59);
    assert!(
        !app.wants_mouse_capture(),
        "logs release capture for selection"
    );

    let mut repair = crate::altscroll::Repair::default();
    for _ in 0..100 {
        for code in [KeyCode::Esc, KeyCode::Char('['), KeyCode::Char('B')] {
            for key in repair.push(press(code)) {
                app.handle_key(key).unwrap();
            }
        }
        assert_eq!(app.mode, Mode::Logs);
        assert!(app.logs.view.scroll <= 60);
    }
    assert_eq!(app.logs.view.scroll, 60);
    assert!(!repair.pending());
}

#[tokio::test]
async fn drill_into_workload_then_esc_restores() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    assert_eq!(app.kind_plural, "deployments");
    assert!(app.stack.is_empty(), "a `:resource` switch is a fresh root");

    apply(
        &mut app,
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}
        }),
    );
    app.table_state.select(Some(0));
    assert_eq!(app.rows().len(), 1);

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.labels.as_deref(), Some("app=web"));
    assert_eq!(app.scope_label.as_deref(), Some("deployment/web"));
    assert_eq!(app.stack.len(), 1);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.labels, None);
    assert!(app.stack.is_empty());
}

#[tokio::test]
async fn o_on_pod_scopes_to_its_host_node() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "web", "namespace": "default"},
               "spec": {"nodeName": "node-1"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Char('o'))).unwrap();
    assert_eq!(app.kind_plural, "nodes");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=node-1"));
    assert_eq!(app.scope_label.as_deref(), Some("node of pod/web"));
    // The same flash a configured drill gives: one action, one feedback.
    assert_eq!(app.flash, "↳ drilled into nodes");
    assert!(!app.flash_err);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "pods");
}

/// One `[views."<key>"]` stanza, compiled, asserting it compiled cleanly.
fn views_for(key: &str, cfg: crate::config::ViewConfig) -> HashMap<String, crate::views::View> {
    let (views, warnings) = crate::views::compile(&HashMap::from([(key.to_string(), cfg)]));
    assert!(warnings.is_empty(), "{warnings:?}");
    views
}

/// The `[views."<key>"].node` a user writes to teach sofka where a kind the
/// built-in table doesn't list keeps its node name.
fn views_with_node(key: &str, pointer: &str) -> HashMap<String, crate::views::View> {
    views_for(
        key,
        crate::config::ViewConfig {
            node: Some(pointer.to_string()),
            ..Default::default()
        },
    )
}

/// The `[views."<key>"].drill` that sends `enter` to another kind.
fn views_with_drill(key: &str, kind: &str, labels: &str) -> HashMap<String, crate::views::View> {
    views_for(
        key,
        crate::config::ViewConfig {
            drill: Some(crate::config::DrillConfig {
                kind: kind.to_string(),
                labels: Some(labels.to_string()),
                fields: None,
            }),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn enter_on_nodeclaim_scopes_to_its_node() {
    // Karpenter writes the node's name onto the claim at registration; the
    // pair is otherwise linked by providerID, which nodes can't be field-
    // selected by.
    let claim = json!({
        "apiVersion": "karpenter.sh/v1", "kind": "NodeClaim",
        "metadata": {"name": "default-sfpsl"},
        "status": {"nodeName": "ip-10-0-1-2", "providerID": "aws:///us-west-2b/i-0123"}
    });

    // `enter` and `o` are the same jump, and esc unwinds either. No config:
    // the nodeclaims row ships in the built-in table.
    for key in [KeyCode::Enter, KeyCode::Char('o')] {
        let (mut app, _rx) = test_app();
        app.cluster
            .register_kind("karpenter.sh", "NodeClaim", "nodeclaims", false);
        app.switch_kind("nodeclaims");
        apply(&mut app, claim.clone());
        app.table_state.select(Some(0));

        app.handle_key(press(key)).unwrap();
        assert_eq!(app.kind_plural, "nodes");
        assert_eq!(app.fields.as_deref(), Some("metadata.name=ip-10-0-1-2"));
        assert_eq!(
            app.scope_label.as_deref(),
            Some("node of nodeclaim/default-sfpsl")
        );
        assert_eq!(app.stack.len(), 1);
        assert!(!app.flash_err);

        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.kind_plural, "nodeclaims");
        assert_eq!(app.fields, None);
        assert!(app.stack.is_empty());
    }
}

#[tokio::test]
async fn unregistered_nodeclaim_warns_instead_of_navigating() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("karpenter.sh", "NodeClaim", "nodeclaims", false);
    app.switch_kind("nodeclaims");
    // Launched but not yet registered: no status.nodeName to jump to.
    apply(
        &mut app,
        json!({
            "apiVersion": "karpenter.sh/v1", "kind": "NodeClaim",
            "metadata": {"name": "default-pending"},
            "status": {"providerID": "aws:///us-west-2b/i-0123"}
        }),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "nodeclaims");
    assert!(app.stack.is_empty());
    assert!(app.flash_err);
    assert!(app.flash.contains("has no node assigned"), "{}", app.flash);
}

#[tokio::test]
async fn o_on_a_kind_that_names_no_node_warns() {
    let (mut app, _rx) = test_app();
    app.switch_kind("secrets");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Secret",
               "metadata": {"name": "tls", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Char('o'))).unwrap();
    assert_eq!(app.kind_plural, "secrets");
    assert!(app.flash_err);
    assert!(app.flash.contains("names no node"), "{}", app.flash);

    // `enter` still falls through to the detail view.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert_eq!(app.kind_plural, "secrets");
}

#[tokio::test]
async fn a_node_pointer_that_lands_on_a_non_name_says_so() {
    let (mut app, _rx) = test_app();
    // Points at an object, not a name — a config mistake, distinct from a
    // row whose node isn't assigned yet.
    app.user_views = views_with_node("certificates", "/status");

    app.switch_kind("certificates");
    apply(
        &mut app,
        json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "tls", "namespace": "default"},
            "status": {"assignedNode": "node-7"}
        }),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "certificates");
    assert!(app.flash_err);
    assert!(app.flash.contains("is not a node name"), "{}", app.flash);
}

/// A kind the built-in table has never heard of jumps to its node once
/// `[views."…"].node` says where the name lives.
#[tokio::test]
async fn configured_node_pointer_makes_any_kind_jump() {
    let (mut app, _rx) = test_app();
    app.user_views = views_with_node("certificates", "/status/assignedNode");

    app.switch_kind("certificates");
    apply(
        &mut app,
        json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "tls", "namespace": "default"},
            "status": {"assignedNode": "node-7"}
        }),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "nodes");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=node-7"));
    assert_eq!(app.scope_label.as_deref(), Some("node of certificate/tls"));
}

#[tokio::test]
async fn enter_on_nodepool_drills_into_its_nodeclaims() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("karpenter.sh", "NodePool", "nodepools", false);
    app.cluster
        .register_kind("karpenter.sh", "NodeClaim", "nodeclaims", false);
    app.user_views = views_with_drill(
        "karpenter.sh/v1/nodepools",
        "nodeclaims",
        "karpenter.sh/nodepool={name}",
    );
    app.switch_kind("nodepools");
    apply(
        &mut app,
        json!({"apiVersion": "karpenter.sh/v1", "kind": "NodePool",
               "metadata": {"name": "default"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "nodeclaims");
    assert_eq!(app.labels.as_deref(), Some("karpenter.sh/nodepool=default"));
    assert_eq!(app.fields, None);
    assert_eq!(app.namespace, "");
    assert_eq!(app.scope_label.as_deref(), Some("nodepool/default"));
    assert_eq!(app.stack.len(), 1);
    assert!(!app.flash_err);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "nodepools");
    assert_eq!(app.labels, None);
    assert!(app.stack.is_empty());
}

#[tokio::test]
async fn configured_drill_keeps_a_namespaced_target_in_the_rows_namespace() {
    let (mut app, _rx) = test_app();
    // A drill target may be named by alias; `{namespace}` is filled too.
    app.user_views = views_with_drill("certificates", "secret", "cert={name},ns={namespace}");
    app.switch_kind("certificates");
    apply(
        &mut app,
        json!({"apiVersion": "cert-manager.io/v1", "kind": "Certificate",
               "metadata": {"name": "tls", "namespace": "edge"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "secrets");
    assert_eq!(app.namespace, "edge");
    assert_eq!(app.labels.as_deref(), Some("cert=tls,ns=edge"));
    assert_eq!(app.scope_label.as_deref(), Some("certificate/tls"));
}

#[tokio::test]
async fn enter_on_externalsecret_opens_the_secret_it_writes() {
    let (mut app, _rx) = test_app();
    // The target Secret shares the ExternalSecret's name and namespace and
    // carries no label naming it, so this is a field selector, not a label.
    app.user_views = views_for(
        "externalsecrets",
        crate::config::ViewConfig {
            drill: Some(crate::config::DrillConfig {
                kind: "secrets".to_string(),
                labels: None,
                fields: Some("metadata.name={name}".to_string()),
            }),
            ..Default::default()
        },
    );
    app.switch_kind("externalsecrets");
    apply(
        &mut app,
        json!({"apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
               "metadata": {"name": "db-creds", "namespace": "shop"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "secrets");
    assert_eq!(app.namespace, "shop");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=db-creds"));
    assert_eq!(app.labels, None);
    assert_eq!(app.scope_label.as_deref(), Some("externalsecret/db-creds"));
}

/// `views::BUILTIN_DRILLS` is what `compile` warns from, so a plural listed
/// there must really have a built-in arm: a drill smuggled past `compile` for
/// each listed kind the fixture knows must never be honoured. (Kinds the
/// fixture can't switch to are skipped rather than faked.)
#[tokio::test]
async fn builtin_drill_list_matches_the_enter_arms() {
    for plural in crate::views::BUILTIN_DRILLS {
        let (mut app, _rx) = test_app();
        if app.cluster.resolve(plural).is_none() {
            continue;
        }
        app.user_views = HashMap::from([(
            plural.to_string(),
            crate::views::View {
                drill: Some(crate::views::Drill {
                    kind: "secrets".to_string(),
                    labels: None,
                    fields: None,
                }),
                ..Default::default()
            },
        )]);
        app.switch_kind(plural);
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Thing",
                   "metadata": {"name": "a", "namespace": "default"}}),
        );
        app.table_state.select(Some(0));

        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_ne!(
            app.kind_plural, "secrets",
            "{plural} honoured a configured drill"
        );
    }
}

#[tokio::test]
async fn configured_drill_to_an_unknown_kind_warns_and_stays() {
    let (mut app, _rx) = test_app();
    app.user_views = views_with_drill("certificates", "widgets", "cert={name}");
    app.switch_kind("certificates");
    apply(
        &mut app,
        json!({"apiVersion": "cert-manager.io/v1", "kind": "Certificate",
               "metadata": {"name": "tls", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "certificates");
    assert!(app.stack.is_empty());
    assert!(app.flash_err);
    assert!(
        app.flash.contains("widgets kind unavailable"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn configured_drill_wins_over_node_on_enter_but_o_still_jumps() {
    let (mut app, _rx) = test_app();
    app.user_views = views_for(
        "certificates",
        crate::config::ViewConfig {
            node: Some("/status/assignedNode".to_string()),
            drill: Some(crate::config::DrillConfig {
                kind: "secrets".to_string(),
                labels: Some("cert={name}".to_string()),
                fields: None,
            }),
            ..Default::default()
        },
    );
    app.switch_kind("certificates");
    let cert = json!({"apiVersion": "cert-manager.io/v1", "kind": "Certificate",
                      "metadata": {"name": "tls", "namespace": "default"},
                      "status": {"assignedNode": "node-7"}});
    apply(&mut app, cert.clone());
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "secrets");
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "certificates");

    // Rows arrive from the watch after a pop; feed the same one back.
    apply(&mut app, cert);
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('o'))).unwrap();
    assert_eq!(app.kind_plural, "nodes");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=node-7"));
}

#[tokio::test]
async fn cronjob_enter_drills_into_owned_jobs() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(
        &mut app,
        json!({
            "apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {"name": "backup", "namespace": "ops", "uid": "cj-1"},
            "spec": {"schedule": "* * * * *", "jobTemplate": {"spec": {}}}
        }),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "jobs");
    assert_eq!(app.namespace, "ops");
    assert_eq!(app.labels, None);
    assert_eq!(app.fields, None);
    assert_eq!(app.scope_label.as_deref(), Some("cronjob/backup"));
    assert_eq!(
        app.owner,
        Some(OwnerScope {
            kind: "CronJob".into(),
            name: "backup".into(),
            uid: Some("cj-1".into()),
        })
    );
    assert_eq!(app.stack.len(), 1);

    let job = |name: &str, owners: serde_json::Value| {
        json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": name, "namespace": "ops", "ownerReferences": owners},
            "spec": {}
        })
    };
    let owned_by = |kind: &str, name: &str, uid: &str| json!([{"apiVersion": "batch/v1", "kind": kind, "name": name, "uid": uid}]);
    apply(
        &mut app,
        job("backup-28900000", owned_by("CronJob", "backup", "cj-1")),
    );
    apply(&mut app, job("backup-manual-abc", json!([])));
    apply(
        &mut app,
        job("backup-28900001", owned_by("CronJob", "backup", "cj-old")),
    );
    apply(
        &mut app,
        job(
            "backup-full-28900000",
            owned_by("CronJob", "backup-full", "cj-2"),
        ),
    );
    apply(&mut app, job("backups", json!([])));
    apply(&mut app, job("cleanup-1", json!([])));

    assert_eq!(
        row_names(&app),
        vec!["backup-28900000", "backup-manual-abc"]
    );

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "cronjobs");
    assert_eq!(app.owner, None);
    assert!(app.stack.is_empty());
}

#[tokio::test]
async fn cronjob_jobs_owner_change_drops_the_row() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(
        &mut app,
        json!({
            "apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {"name": "backup", "namespace": "ops", "uid": "cj-1"},
            "spec": {"schedule": "* * * * *", "jobTemplate": {"spec": {}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "jobs");

    let job = |owner: &str, rv: &str| {
        json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {
                "name": "run-1", "namespace": "ops", "resourceVersion": rv,
                "ownerReferences": [{"apiVersion": "batch/v1", "kind": "CronJob", "name": owner, "uid": "cj-1"}]
            },
            "spec": {}
        })
    };
    apply(&mut app, job("backup", "1"));
    assert_eq!(row_names(&app), vec!["run-1"]);

    apply(&mut app, job("other", "2"));
    assert!(row_names(&app).is_empty());

    apply(&mut app, job("backup", "3"));
    assert_eq!(row_names(&app), vec!["run-1"]);
}

#[tokio::test]
async fn namespace_switch_drops_the_cronjob_owner_scope() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(
        &mut app,
        json!({
            "apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {"name": "backup", "namespace": "ops"},
            "spec": {"schedule": "* * * * *", "jobTemplate": {"spec": {}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.owner.is_some());

    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert_eq!(app.kind_plural, "jobs");
    assert!(app.all_namespaces());
    assert_eq!(app.owner, None);
    assert_eq!(app.scope_label, None);
    apply(
        &mut app,
        json!({"apiVersion": "batch/v1", "kind": "Job",
               "metadata": {"name": "unrelated", "namespace": "other"}, "spec": {}}),
    );
    assert_eq!(row_names(&app), vec!["unrelated"]);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "cronjobs");
    apply(
        &mut app,
        json!({
            "apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {"name": "backup", "namespace": "ops"},
            "spec": {"schedule": "* * * * *", "jobTemplate": {"spec": {}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.owner.is_some());

    app.set_namespace("other".into());
    assert_eq!(app.namespace, "other");
    assert_eq!(app.owner, None);
    assert_eq!(app.scope_label, None);
}

#[tokio::test]
async fn cronjob_jobs_then_job_drills_into_pods_and_pops_back() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(
        &mut app,
        json!({
            "apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {"name": "backup", "namespace": "ops"},
            "spec": {"schedule": "* * * * *", "jobTemplate": {"spec": {}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "jobs");

    apply(
        &mut app,
        json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {
                "name": "backup-1", "namespace": "ops",
                "ownerReferences": [{"apiVersion": "batch/v1", "kind": "CronJob", "name": "backup", "uid": "x"}]
            },
            "spec": {"selector": {"matchLabels": {"batch.kubernetes.io/controller-uid": "j1"}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.owner, None);
    assert_eq!(
        app.labels.as_deref(),
        Some("batch.kubernetes.io/controller-uid=j1")
    );
    assert_eq!(app.scope_label.as_deref(), Some("job/backup-1"));
    assert_eq!(app.stack.len(), 2);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "jobs");
    assert_eq!(app.scope_label.as_deref(), Some("cronjob/backup"));
    assert!(app.owner.is_some());

    app.switch_kind("jobs");
    assert_eq!(app.owner, None);
    assert!(app.stack.is_empty());
}

#[tokio::test]
async fn root_switch_clears_drill_stack() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "p", "namespace": "default"},
               "spec": {}}),
    );
    // Manually push a frame to simulate having drilled in.
    app.push_frame();
    assert_eq!(app.stack.len(), 1);
    // A fresh `:resource` switch must reset the breadcrumb.
    app.switch_kind("services");
    assert_eq!(app.kind_plural, "services");
    assert!(app.stack.is_empty());
}

#[tokio::test]
async fn filter_narrows_rows_via_cache() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["alpha", "beta", "gamma"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    assert_eq!(app.rows().len(), 3);

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in ['a', 'l', 'p'] {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    let rows = app.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].metadata.name.as_deref(), Some("alpha"));

    // Clearing the filter restores all rows (cache re-derived).
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.rows().len(), 3);
}

#[tokio::test]
async fn delete_message_updates_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "keep", "namespace": "default"}}),
    );
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "gone", "namespace": "default"}}),
    );
    assert_eq!(app.rows().len(), 2);
    app.handle_msg(Msg::Deleted {
        generation: app.generation,
        key: "default/gone".into(),
    });
    let rows = app.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].metadata.name.as_deref(), Some("keep"));
}

#[tokio::test]
async fn space_marks_rows_for_bulk_delete() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["a", "b", "c"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    assert_eq!(app.rows().len(), 3);
    assert_eq!(app.table_state.selected(), Some(0));

    // Mark the first two rows; each SPACE also advances the cursor.
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    assert_eq!(app.marked.len(), 2);
    assert_eq!(app.table_state.selected(), Some(2));

    // A bulk action targets exactly the marked rows.
    let mut targets = app.action_targets();
    targets.sort();
    assert_eq!(
        targets,
        vec![
            ("a".to_string(), "default".to_string()),
            ("b".to_string(), "default".to_string()),
        ]
    );

    // ctrl-d opens a confirm for the marked set…
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label.contains("Delete 2 pods"),
        "{}",
        app.confirm_label
    );

    // …and confirming clears the marks.
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert!(app.marked.is_empty());
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn delete_confirm_force_can_toggle() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "web", "namespace": "default"}}),
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(app.confirm_allows_force_toggle());
    assert!(app.confirm_label.starts_with("Delete pod web"));
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete { force: false, .. })
    ));

    app.handle_key(press(KeyCode::Char('f'))).unwrap();
    assert!(app.confirm_label.starts_with("Force delete pod web"));
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete { force: true, .. })
    ));

    app.handle_key(press(KeyCode::Char('f'))).unwrap();
    assert!(app.confirm_label.starts_with("Delete pod web"));
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete { force: false, .. })
    ));
}

#[tokio::test]
async fn delete_confirm_cascade_can_cycle() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "web", "namespace": "default"}}),
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    // Background is the default and doesn't clutter the label.
    assert!(!app.confirm_label.contains("cascade"));
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete {
            cascade: Cascade::Background,
            ..
        })
    ));

    app.handle_key(press(KeyCode::Char('c'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm, "c must cycle, not cancel");
    assert!(app.confirm_label.contains("(cascade: foreground)"));
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete {
            cascade: Cascade::Foreground,
            ..
        })
    ));

    app.handle_key(press(KeyCode::Char('c'))).unwrap();
    assert!(app.confirm_label.contains("(orphan dependents)"));
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete {
            cascade: Cascade::Orphan,
            ..
        })
    ));

    // Cascade and force compose in the label.
    app.handle_key(press(KeyCode::Char('f'))).unwrap();
    assert!(
        app.confirm_label
            .starts_with("Force delete pod web in default (orphan dependents)"),
        "{}",
        app.confirm_label
    );

    // Full circle back to background.
    app.handle_key(press(KeyCode::Char('c'))).unwrap();
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Delete {
            cascade: Cascade::Background,
            ..
        })
    ));
}

#[tokio::test]
async fn node_drain_key_opens_confirm_for_marked_nodes() {
    let (mut app, _rx) = test_app();
    app.switch_kind("nodes");
    for n in ["node-a", "node-b"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Node",
                   "metadata": {"name": n}}),
        );
    }
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();

    app.handle_key(press(KeyCode::Char('D'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert_eq!(
        app.confirm_label,
        "Drain 2 nodes? Cordon and evict eligible pods."
    );
    assert!(!app.confirm_allows_force_toggle());
    let Some(ConfirmAction::Drain { mut targets }) = app.confirm_action.take() else {
        panic!("expected drain confirm action");
    };
    targets.sort();
    assert_eq!(targets, vec!["node-a".to_string(), "node-b".to_string()]);
}

#[tokio::test]
async fn restart_key_opens_confirm() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    apply(
        &mut app,
        json!({"apiVersion": "apps/v1", "kind": "Deployment",
               "metadata": {"name": "web", "namespace": "default"}}),
    );

    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert_eq!(app.confirm_label, "Restart web in default?");
    assert!(!app.confirm_allows_force_toggle());
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::Restart { ref name, ref ns, .. })
            if name == "web" && ns == "default"
    ));

    // Cancelling leaves the workload untouched.
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.confirm_action.is_none());
}

#[tokio::test]
async fn detail_arrival_clears_progress_flash() {
    let (mut app, _rx) = test_app();
    let claim = app.claim_status("describing web…");
    app.handle_msg(Msg::Detail {
        generation: app.generation,
        claim,
        title: "web — describe".into(),
        lines: vec!["Name: web".into()],
        warn: None,
    });
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.flash.is_empty(), "{}", app.flash);

    // A fallback warning still replaces the progress flash.
    let claim = app.claim_status("describing web…");
    app.handle_msg(Msg::Detail {
        generation: app.generation,
        claim,
        title: "web — YAML".into(),
        lines: vec!["kind: Pod".into()],
        warn: Some("kubectl not found; showing YAML".into()),
    });
    assert!(app.flash.contains("kubectl not found"), "{}", app.flash);
}

#[tokio::test]
async fn welcome_flash_does_not_expire() {
    let (mut app, _rx) = test_app();
    assert_eq!(app.flash, WELCOME_FLASH);

    app.expire_flash();
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();

    assert_eq!(app.flash, WELCOME_FLASH);
    assert!(!app.flash_err);

    // Stickiness rides on a flag rather than on matching the constant, and the
    // first real flash to land clears it.
    app.flash = "deleted web".into();
    app.expire_flash();
    assert!(!app.flash_sticky);
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();
    assert!(app.flash.is_empty(), "{}", app.flash);
}

#[tokio::test]
async fn error_flash_does_not_expire() {
    let (mut app, _rx) = test_app();
    app.flash = "delete failed: forbidden".into();
    app.flash_err = true;

    app.expire_flash();
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();

    assert_eq!(app.flash, "delete failed: forbidden");
    assert!(app.flash_err);
}

#[tokio::test]
async fn successful_transient_flash_expires() {
    let (mut app, _rx) = test_app();
    app.flash = "deleted web".into();

    app.expire_flash();
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();

    assert!(app.flash.is_empty(), "{}", app.flash);
    assert!(!app.flash_err);
}

#[tokio::test]
async fn progress_flash_outlives_the_expiry_window() {
    let (mut app, _rx) = test_app();
    let claim = app.claim_status("draining 3 nodes…");

    // A drain or a bulk delete easily runs past 8s; blanking the bar
    // mid-flight would read as though nothing were happening.
    app.expire_flash();
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(30);
    app.expire_flash();
    assert_eq!(app.flash, "draining 3 nodes…");

    // The result replaces it, and that one does expire on schedule.
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim,
        message: "drain requested: 3 nodes".into(),
        err: false,
    });
    assert_eq!(app.flash, "drain requested: 3 nodes");
    assert!(!app.flash_err);
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();
    assert!(app.flash.is_empty(), "{}", app.flash);
}

#[tokio::test]
async fn refreshing_mid_action_drops_the_orphaned_progress_flash() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let claim = app.claim_status("deleting 3 pods…");
    let stale = app.generation;

    // `r` restarts the watch, which bumps the generation, so the delete's
    // `Msg::Flash` can no longer land. Nothing would replace the progress
    // message and `expire_flash` won't time a `…` one out — hence the bump
    // clears it itself.
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_ne!(app.generation, stale);
    assert!(app.flash.is_empty(), "{}", app.flash);

    // The orphaned result is still dropped, not shown late.
    app.handle_msg(Msg::Flash {
        generation: stale,
        claim,
        message: "deleted 3 pods".into(),
        err: false,
    });
    assert!(app.flash.is_empty(), "{}", app.flash);
}

#[tokio::test]
async fn action_progress_flashes_keep_the_ellipsis_convention() {
    // `expire_flash` reads the trailing `…` as "still running". An action that
    // forgets it gets its progress message blanked out mid-flight, which is
    // exactly the failure the ellipsis guard exists to prevent.
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    let targets = vec![("web".to_string(), "default".to_string())];

    app.do_scale(targets.clone(), 3);
    assert!(app.flash.ends_with('…'), "scale: {}", app.flash);

    app.do_scale(
        vec![
            targets[0].clone(),
            ("api".to_string(), "default".to_string()),
        ],
        3,
    );
    assert!(app.flash.ends_with('…'), "bulk scale: {}", app.flash);

    app.do_set_image(
        "default".into(),
        "web".into(),
        "deployments".into(),
        "app".into(),
        "nginx:1.27".into(),
    );
    assert!(app.flash.ends_with('…'), "set-image: {}", app.flash);

    app.do_flux_suspend(targets.clone(), true);
    assert!(app.flash.ends_with('…'), "suspend: {}", app.flash);

    app.do_flux_reconcile(targets.clone());
    assert!(app.flash.ends_with('…'), "reconcile: {}", app.flash);

    app.do_refresh_es(targets);
    assert!(app.flash.ends_with('…'), "refresh: {}", app.flash);
}

#[tokio::test]
async fn explain_findings_clear_the_progress_flash() {
    let (mut app, _rx) = test_app();
    let claim = app.claim_status("explaining web…");

    // Nothing else replaces this one, and `expire_flash` won't time a `…` out,
    // so the handler has to clear it — as the `Msg::Gitops` arm does.
    app.handle_msg(Msg::Explain {
        generation: app.generation,
        claim,
        title: "explain — web".into(),
        findings: Vec::new(),
    });

    assert_eq!(app.mode, Mode::Explain);
    assert!(app.flash.is_empty(), "{}", app.flash);
    assert!(!app.flash_err);
}

#[tokio::test]
async fn an_action_with_no_targets_claims_nothing() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    app.flash.clear();

    // `request_flux_menu` guards this, but the menu stays open across watch
    // updates — the selected row can be gone by the time Enter lands.
    app.do_flux_suspend(Vec::new(), true);
    assert!(app.flash.is_empty(), "{}", app.flash);
    app.do_flux_reconcile(Vec::new());
    assert!(app.flash.is_empty(), "{}", app.flash);
}

#[tokio::test]
async fn a_finished_report_only_clears_its_own_status_claim() {
    let (mut app, _rx) = test_app();

    // A describe and a delete share a generation. The delete claims the bar
    // after describe; the older report must not clear that in-flight progress.
    let describe_claim = app.claim_status("describing web…");
    let delete_claim = app.claim_status("deleting 3 pods…");
    app.handle_msg(Msg::Detail {
        generation: app.generation,
        claim: describe_claim,
        title: "web — describe".into(),
        lines: vec!["Name: web".into()],
        warn: None,
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: delete_claim,
        message: "deleted 3 pods".into(),
        err: false,
    });
    assert_eq!(app.flash, "deleted 3 pods");

    // Same for an error, which no longer expires on its own — losing it here
    // would lose it for good.
    let explain_claim = app.claim_status("explaining web…");
    let delete_claim = app.claim_status("deleting web…");
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: delete_claim,
        message: "delete web failed: forbidden".into(),
        err: true,
    });
    app.handle_msg(Msg::Explain {
        generation: app.generation,
        claim: explain_claim,
        title: "explain — web".into(),
        findings: Vec::new(),
    });
    assert_eq!(app.mode, Mode::Explain);
    assert!(app.flash_err);
    assert!(app.flash.contains("forbidden"), "{}", app.flash);
}

#[tokio::test]
async fn an_older_action_result_cannot_overwrite_a_newer_action() {
    let (mut app, _rx) = test_app();

    let old = app.claim_status("deleting 3 pods…");
    let new = app.claim_status("scaling web → 3…");
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: old,
        message: "deleted 3 pods".into(),
        err: false,
    });
    assert_eq!(app.flash, "scaling web → 3…");

    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: new,
        message: "scale web failed: forbidden".into(),
        err: true,
    });
    assert!(app.flash_err);
    assert!(app.flash.contains("scale web failed"), "{}", app.flash);

    // An older error also cannot erase a newer successful result.
    let old = app.claim_status("deleting api…");
    let new = app.claim_status("scaling worker → 2…");
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: new,
        message: "scaled worker → 2".into(),
        err: false,
    });
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: old,
        message: "delete api failed: forbidden".into(),
        err: true,
    });
    assert_eq!(app.flash, "scaled worker → 2");
    assert!(!app.flash_err);
}

#[tokio::test]
async fn an_action_failure_is_never_silently_dropped() {
    let (mut app, _rx) = test_app();

    // A delete fails after a describe has claimed the bar. The failure is not
    // this operation's to show, but the bar only holds an unresolved `…`, so
    // borrowing it costs nothing and losing the failure costs a lot.
    let delete = app.claim_status("deleting web…");
    let describe = app.claim_status("describing api…");
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: delete,
        message: "delete web failed: forbidden".into(),
        err: true,
    });
    assert!(app.flash_err);
    assert_eq!(app.flash, "delete web failed: forbidden");
    assert_eq!(
        app.last_action_error.as_deref(),
        Some("delete web failed: forbidden")
    );

    // Borrowing does not take ownership: the describe still reports normally,
    // but completing that report must not erase the sticky action failure.
    app.handle_msg(Msg::Detail {
        generation: app.generation,
        claim: describe,
        title: "api — describe".into(),
        lines: vec!["Name: api".into()],
        warn: None,
    });
    assert_eq!(app.mode, Mode::Detail);
    assert_eq!(app.flash, "delete web failed: forbidden");
    assert!(app.flash_err);
    assert!(app.status_claim.is_none());

    // A failure that cannot even borrow the bar — a newer operation has
    // already put a finished result there — is still recorded for `:debug`.
    let stale = app.claim_status("draining node-1…");
    let newer = app.claim_status("scaling web → 3…");
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: newer,
        message: "scaled web → 3".into(),
        err: false,
    });
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: stale,
        message: "drain node-1 failed: forbidden".into(),
        err: true,
    });
    assert_eq!(app.flash, "scaled web → 3");
    assert!(!app.flash_err);
    assert_eq!(
        app.last_action_error.as_deref(),
        Some("drain node-1 failed: forbidden")
    );
    app.open_info();
    assert!(
        app.detail.lines.iter().any(|l| l
            .to_string()
            .contains("last failure: drain node-1 failed: forbidden")),
        "`:info` does not report the failure"
    );
}

#[tokio::test]
async fn every_async_result_is_scoped_to_its_own_operation() {
    // The claim plumbing has to reach *every* operation that writes the bar,
    // not just the action ones — an unclaimed handler overwrites whatever the
    // user started later.
    let (mut app, _rx) = test_app();

    let find = app.claim_status("finding 'foo'…");
    let delete = app.claim_status("deleting 3 pods…");
    app.handle_msg(Msg::FindResults {
        generation: app.generation,
        claim: find,
        query: "foo".into(),
        items: Vec::new(),
        warn: None,
    });
    assert_eq!(app.flash, "deleting 3 pods…");
    // The results still land, only the status is scoped.
    assert_eq!(app.find_query, "foo");

    app.handle_msg(Msg::TransferDone {
        generation: app.generation,
        claim: find,
        result: Ok("copied a → b".into()),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::SnapshotSaved {
        generation: app.generation,
        claim: find,
        result: Ok(std::path::PathBuf::from("/tmp/snap.json")),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::BundleSaved {
        generation: app.generation,
        claim: find,
        result: Ok(std::path::PathBuf::from("/tmp/bundle.txt")),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::DebuggersCleaned {
        generation: app.generation,
        claim: find,
        deleted: 2,
        failed: Vec::new(),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::ClipboardCopied {
        generation: app.generation,
        claim: find,
        copied: true,
        success: "copied 12 log lines".into(),
        failure: "no clipboard target".into(),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::LogsSaved {
        generation: app.generation,
        claim: find,
        result: Ok(std::path::PathBuf::from("/tmp/sofka.log")),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::PluginBulkDone {
        generation: app.generation,
        claim: find,
        name: "sync".into(),
        ok: 3,
        failed: Vec::new(),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::ContextRenamed {
        generation: app.generation,
        claim: find,
        old: "test".into(),
        new: "staging".into(),
        result: Ok(()),
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    app.handle_msg(Msg::XrayData {
        generation: app.generation,
        claim: find,
        items: Vec::new(),
        warn: None,
    });
    assert_eq!(app.flash, "deleting 3 pods…");

    // And the owner still reports when it lands.
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim: delete,
        message: "deleted 3 pods".into(),
        err: false,
    });
    assert_eq!(app.flash, "deleted 3 pods");
}

#[tokio::test]
async fn an_unclaimed_status_update_invalidates_the_current_claim() {
    let (mut app, _rx) = test_app();
    let claim = app.claim_status("deleting api…");

    // Most existing synchronous status updates assign `flash` directly. The
    // expected-text half of ownership makes those safe without migrating every
    // call site in this fix.
    app.flash = "namespace: prod".into();
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim,
        message: "deleted api".into(),
        err: false,
    });
    assert_eq!(app.flash, "namespace: prod");
}

#[tokio::test]
async fn background_status_borrows_the_bar_without_orphaning_an_action() {
    let (mut app, _rx) = test_app();

    let claim = app.claim_status("deleting api…");
    app.handle_msg(Msg::Error {
        generation: app.generation,
        error: "watch disconnected".into(),
    });
    assert!(app.flash.contains("watch disconnected"), "{}", app.flash);
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim,
        message: "deleted api".into(),
        err: false,
    });
    assert_eq!(app.flash, "deleted api");

    let claim = app.claim_status("scaling web → 3…");
    app.handle_msg(Msg::Panic("worker panic".into()));
    assert!(app.flash.contains("worker panic"), "{}", app.flash);
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim,
        message: "scaled web → 3".into(),
        err: false,
    });
    assert_eq!(app.flash, "scaled web → 3");

    let claim = app.claim_status("draining node-1…");
    app.handle_msg(Msg::Notify("pod/web: Ready True → False".into()));
    assert!(app.flash.starts_with('🔔'), "{}", app.flash);

    // A transient notification may expire before the action. The pending
    // owner still retains its claim against the now-empty bar.
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();
    assert!(app.flash.is_empty(), "{}", app.flash);
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim,
        message: "drain requested: node-1".into(),
        err: false,
    });
    assert_eq!(app.flash, "drain requested: node-1");
}

#[tokio::test]
async fn can_i_verdict_arrives_as_a_flash() {
    let (mut app, _rx) = test_app();
    // `:can-i` shares `Msg::Flash` with the action results. A denial is an
    // answer, not a failure, so it doesn't go through `Msg::Error` — but it
    // still reads as an error and sticks around like one.
    let claim = app.claim_status("can-i delete pods…");
    app.handle_msg(Msg::Flash {
        generation: app.generation,
        claim,
        message: "✗ no — cannot delete pods".into(),
        err: true,
    });
    assert!(app.flash_err);
    assert_eq!(app.last_error, None);

    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(9);
    app.expire_flash();
    assert_eq!(app.flash, "✗ no — cannot delete pods");
}

#[tokio::test]
async fn repeating_an_action_restarts_the_expiry_timer() {
    let (mut app, _rx) = test_app();
    app.set_flash("namespace: prod");

    // 7s on, the same command runs again. The text is identical, so the
    // `flash_seen` diff can't tell a repeat from a flash that has been sitting
    // there all along — only going through the setter can.
    app.flash_since = std::time::Instant::now() - std::time::Duration::from_secs(7);
    app.set_flash("namespace: prod");

    // 14s since the first showing, 7s since the repeat: still up.
    app.flash_since -= std::time::Duration::from_secs(7);
    app.expire_flash();
    assert_eq!(app.flash, "namespace: prod");

    app.flash_since -= std::time::Duration::from_secs(2);
    app.expire_flash();
    assert!(app.flash.is_empty(), "{}", app.flash);
}

#[tokio::test]
async fn scale_prompt_targets_marked_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    for n in ["a", "b", "c"] {
        apply(
            &mut app,
            json!({"apiVersion": "apps/v1", "kind": "Deployment",
                   "metadata": {"name": n, "namespace": "default"},
                   "spec": {"replicas": 2}}),
        );
    }
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    assert_eq!(app.marked.len(), 2);

    app.handle_key(press(KeyCode::Char('s'))).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    assert!(
        app.prompt_label.contains("Scale 2 deployments"),
        "{}",
        app.prompt_label
    );
    assert!(matches!(
        app.prompt_kind,
        Some(PromptKind::Scale { ref targets }) if targets.len() == 2
    ));

    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.marked.is_empty());
    assert!(
        app.flash.contains("scaling 2 deployments → 0"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn scale_prompt_single_row_shows_current_replicas() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    apply(
        &mut app,
        json!({"apiVersion": "apps/v1", "kind": "Deployment",
               "metadata": {"name": "web", "namespace": "default"},
               "spec": {"replicas": 3}}),
    );
    app.handle_key(press(KeyCode::Char('s'))).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    assert!(
        app.prompt_label
            .contains("Scale web to replicas (current 3)"),
        "{}",
        app.prompt_label
    );
}

#[tokio::test]
async fn esc_clears_marks_before_popping() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "a", "namespace": "default"}}),
    );
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    assert_eq!(app.marked.len(), 1);
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.marked.is_empty());
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn switching_kind_clears_marks() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "a", "namespace": "default"}}),
    );
    app.handle_key(press(KeyCode::Char(' '))).unwrap();
    assert_eq!(app.marked.len(), 1);
    app.switch_kind("deployments");
    assert!(app.marked.is_empty());
}

#[tokio::test]
async fn flux_menu_rejects_non_flux_kinds() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "a", "namespace": "default"}}),
    );
    app.request_flux_menu();
    assert!(app.flash_err);
    assert!(app.flash.contains("Flux"), "{}", app.flash);
    assert_eq!(app.mode, Mode::Table); // never opens the menu
}

#[tokio::test]
async fn flux_menu_requires_explicit_choice_not_a_single_key() {
    let (mut app, _rx) = test_app();
    app.switch_kind("kustomizations");
    apply(
        &mut app,
        json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
            "metadata": {"name": "infra", "namespace": "default"},
            "spec": {"suspend": false}
        }),
    );

    // `t` opens the menu — nothing is patched yet.
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::FluxMenu);
    assert_eq!(app.flux_menu_state.selected(), Some(0)); // "Suspend"

    // Esc backs out without doing anything.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(!app.flash.contains("suspending"));

    // Re-open, navigate to "Resume", confirm.
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    assert_eq!(app.flux_menu_state.selected(), Some(1)); // "Resume"
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("resuming"), "{}", app.flash);
}

#[tokio::test]
async fn flux_menu_cancel_item_does_nothing() {
    let (mut app, _rx) = test_app();
    app.switch_kind("kustomizations");
    apply(
        &mut app,
        json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
            "metadata": {"name": "infra", "namespace": "default"},
            "spec": {"suspend": false}
        }),
    );
    let flash_before = app.flash.clone();
    app.request_flux_menu();
    let cancel = FLUX_MENU_ITEMS.iter().position(|s| *s == "Cancel").unwrap();
    app.flux_menu_state.select(Some(cancel));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.flash, flash_before); // no suspend/resume side effect
}

#[tokio::test]
async fn flux_menu_suspend_acts_on_marked_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("kustomizations");
    let ks = |name: &str| {
        json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
            "metadata": {"name": name, "namespace": "default"},
            "spec": {"suspend": false}
        })
    };
    apply(&mut app, ks("infra"));
    apply(&mut app, ks("apps"));
    app.marked.insert("default/infra".into());
    app.marked.insert("default/apps".into());

    app.request_flux_menu();
    app.handle_key(press(KeyCode::Enter)).unwrap(); // "Suspend" (default selection)
    assert!(
        app.flash.contains("suspending 2 kustomizations"),
        "{}",
        app.flash
    );
    assert!(app.marked.is_empty()); // cleared after the bulk action
}

#[tokio::test]
async fn flux_menu_reconcile_now() {
    let (mut app, _rx) = test_app();
    app.switch_kind("kustomizations");
    apply(
        &mut app,
        json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
            "metadata": {"name": "infra", "namespace": "default"},
            "spec": {"suspend": false}
        }),
    );
    app.request_flux_menu();
    let idx = FLUX_MENU_ITEMS
        .iter()
        .position(|s| *s == "Reconcile now")
        .unwrap();
    app.flux_menu_state.select(Some(idx));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("reconciling infra"), "{}", app.flash);
}

fn cronjob(name: &str) -> serde_json::Value {
    json!({
        "apiVersion": "batch/v1", "kind": "CronJob",
        "metadata": {"name": name, "namespace": "default", "uid": "cj-uid-1"},
        "spec": {
            "schedule": "0 * * * *",
            "suspend": false,
            "jobTemplate": {
                "metadata": {
                    "labels": {"app": name},
                    "annotations": {"team": "platform"}
                },
                "spec": {
                    "template": {"spec": {
                        "containers": [{"name": "main", "image": "busybox"}],
                        "restartPolicy": "Never"
                    }}
                }
            }
        }
    })
}

#[tokio::test]
async fn cronjob_menu_opens_with_trigger_first() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(&mut app, cronjob("backup"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::FluxMenu);
    assert_eq!(app.action_menu_items(), CRONJOB_MENU_ITEMS);
    assert_eq!(app.flux_menu_state.selected(), Some(0)); // "Trigger now"

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("triggering backup"), "{}", app.flash);
    assert!(!app.flash_err);
}

#[tokio::test]
async fn cronjob_menu_suspend_and_resume() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(&mut app, cronjob("backup"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    let idx = CRONJOB_MENU_ITEMS
        .iter()
        .position(|s| *s == "Suspend")
        .unwrap();
    app.flux_menu_state.select(Some(idx));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("suspending backup"), "{}", app.flash);

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    let idx = CRONJOB_MENU_ITEMS
        .iter()
        .position(|s| *s == "Resume")
        .unwrap();
    app.flux_menu_state.select(Some(idx));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("resuming backup"), "{}", app.flash);
}

#[tokio::test]
async fn cronjob_trigger_acts_on_marked_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("cronjobs");
    apply(&mut app, cronjob("backup"));
    let mut second = cronjob("cleanup");
    second["metadata"]["uid"] = json!("cj-uid-2");
    apply(&mut app, second);
    app.marked.insert("default/backup".into());
    app.marked.insert("default/cleanup".into());

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap(); // "Trigger now"
    assert!(app.flash.contains("triggering 2 cronjobs"), "{}", app.flash);
    assert!(app.marked.is_empty()); // cleared after the bulk action
}

fn argocd_app(name: &str) -> serde_json::Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1", "kind": "Application",
        "metadata": {"name": name, "namespace": "argocd"},
        "spec": {
            "source": {"repoURL": "https://github.com/example/repo", "targetRevision": "HEAD",
                       "path": "manifests"},
            "destination": {"server": "https://kubernetes.default.svc", "namespace": "default"},
            "syncPolicy": {"automated": {"prune": true, "selfHeal": true}}
        }
    })
}

#[tokio::test]
async fn argocd_menu_opens_with_sync_items() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::FluxMenu);
    assert_eq!(app.action_menu_items(), ARGOCD_MENU_ITEMS);
    assert_eq!(app.flux_menu_state.selected(), Some(0)); // "Suspend"
}

#[tokio::test]
async fn argocd_menu_suspend() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap(); // "Suspend"
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("suspending guestbook"), "{}", app.flash);
}

#[tokio::test]
async fn argocd_menu_resume() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    let idx = ARGOCD_MENU_ITEMS
        .iter()
        .position(|s| *s == "Resume")
        .unwrap();
    app.flux_menu_state.select(Some(idx));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("resuming guestbook"), "{}", app.flash);
}

#[tokio::test]
async fn argocd_menu_sync_now() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    let idx = ARGOCD_MENU_ITEMS
        .iter()
        .position(|s| *s == "Sync now")
        .unwrap();
    app.flux_menu_state.select(Some(idx));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("syncing guestbook"), "{}", app.flash);
}

#[tokio::test]
async fn argocd_menu_suspend_acts_on_marked_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));
    apply(&mut app, argocd_app("frontend"));
    app.marked.insert("argocd/guestbook".into());
    app.marked.insert("argocd/frontend".into());

    app.request_flux_menu();
    app.handle_key(press(KeyCode::Enter)).unwrap(); // "Suspend"
    assert!(
        app.flash.contains("suspending 2 applications"),
        "{}",
        app.flash
    );
    assert!(app.marked.is_empty());
}

#[tokio::test]
async fn argocd_menu_rejects_non_argocd_applications() {
    let (mut app, _rx) = test_app();
    // "applications" from a non-argoproj group should not get the ArgoCD menu.
    // The fake cluster only registers argoproj.io/applications, so a different
    // group would need its own registration — here we verify that a known
    // non-ArgoCD kind is rejected.
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "a", "namespace": "default"}}),
    );
    app.request_flux_menu();
    assert!(app.flash_err);
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn argocd_menu_cancel_item_does_nothing() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));
    let flash_before = app.flash.clone();
    app.request_flux_menu();
    let cancel = ARGOCD_MENU_ITEMS
        .iter()
        .position(|s| *s == "Cancel")
        .unwrap();
    app.flux_menu_state.select(Some(cancel));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.flash, flash_before);
}

#[tokio::test]
async fn argocd_menu_requires_explicit_choice_not_a_single_key() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applications");
    apply(&mut app, argocd_app("guestbook"));

    // `t` opens the menu — nothing is patched yet.
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::FluxMenu);
    assert_eq!(app.flux_menu_state.selected(), Some(0)); // "Suspend"

    // Esc backs out without doing anything.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(!app.flash.contains("suspending"));
}

#[test]
fn argocd_suspend_stashes_automated_and_removes_field() {
    let o = obj(argocd_app("guestbook"));
    let p = argocd_suspend_patch(&o, true);
    assert_eq!(p["spec"]["syncPolicy"]["automated"], json!(null));
    let stash = p["metadata"]["annotations"]["sofka.io/argocd-automated"]
        .as_str()
        .unwrap();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stash)
        .unwrap();
    let restored: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(restored, json!({"prune": true, "selfHeal": true}));
}

#[test]
fn argocd_suspend_with_no_automated_does_not_stash() {
    let mut o = obj(argocd_app("guestbook"));
    if let Some(v) = o.data.pointer_mut("/spec/syncPolicy") {
        v.as_object_mut().unwrap().remove("automated");
    }
    let p = argocd_suspend_patch(&o, true);
    assert_eq!(p["spec"]["syncPolicy"]["automated"], json!(null));
    assert!(p["metadata"]["annotations"].is_null());
}

#[test]
fn argocd_resume_restores_from_annotation() {
    use base64::Engine;
    let stash =
        base64::engine::general_purpose::STANDARD.encode(r#"{"prune":true,"selfHeal":true}"#);
    let o = obj(json!({
        "apiVersion": "argoproj.io/v1alpha1", "kind": "Application",
        "metadata": {"name": "guestbook", "namespace": "argocd",
                     "annotations": {"sofka.io/argocd-automated": stash}},
        "spec": {"syncPolicy": {}}
    }));
    let p = argocd_suspend_patch(&o, false);
    assert_eq!(
        p["spec"]["syncPolicy"]["automated"],
        json!({"prune": true, "selfHeal": true})
    );
    assert_eq!(
        p["metadata"]["annotations"]["sofka.io/argocd-automated"],
        json!(null)
    );
}

#[test]
fn argocd_resume_without_annotation_defaults_to_empty() {
    let o = obj(argocd_app("guestbook"));
    let p = argocd_suspend_patch(&o, false);
    assert_eq!(p["spec"]["syncPolicy"]["automated"], json!({}));
    assert!(p["metadata"].is_null() || p["metadata"]["annotations"].is_null());
}

#[test]
fn argocd_appset_suspend_stashes_and_sets_create_only() {
    let o = obj(argocd_appset("guestbook-set"));
    let p = argocd_appset_suspend_patch(&o, true);
    assert_eq!(
        p["spec"]["syncPolicy"]["applicationsSync"],
        json!("create-only")
    );
    let stash = p["metadata"]["annotations"]["sofka.io/argocd-applications-sync"]
        .as_str()
        .unwrap();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stash)
        .unwrap();
    let restored: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(restored, json!("sync"));
}

#[test]
fn argocd_appset_suspend_defaults_stash_to_sync_when_absent() {
    let mut o = obj(argocd_appset("guestbook-set"));
    if let Some(v) = o.data.pointer_mut("/spec/syncPolicy") {
        v.as_object_mut().unwrap().remove("applicationsSync");
    }
    let p = argocd_appset_suspend_patch(&o, true);
    assert_eq!(
        p["spec"]["syncPolicy"]["applicationsSync"],
        json!("create-only")
    );
    let stash = p["metadata"]["annotations"]["sofka.io/argocd-applications-sync"]
        .as_str()
        .unwrap();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stash)
        .unwrap();
    let restored: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(restored, json!("sync"));
}

#[test]
fn argocd_appset_resume_restores_from_annotation() {
    use base64::Engine;
    let stash = base64::engine::general_purpose::STANDARD.encode(r#""create-update""#);
    let o = obj(json!({
        "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
        "metadata": {"name": "guestbook-set", "namespace": "argocd",
                     "annotations": {"sofka.io/argocd-applications-sync": stash}},
        "spec": {"syncPolicy": {"applicationsSync": "create-only"}}
    }));
    let p = argocd_appset_suspend_patch(&o, false);
    assert_eq!(
        p["spec"]["syncPolicy"]["applicationsSync"],
        json!("create-update")
    );
    assert_eq!(
        p["metadata"]["annotations"]["sofka.io/argocd-applications-sync"],
        json!(null)
    );
}

#[test]
fn argocd_appset_resume_without_annotation_defaults_to_sync() {
    let o = obj(argocd_appset("guestbook-set"));
    let p = argocd_appset_suspend_patch(&o, false);
    assert_eq!(p["spec"]["syncPolicy"]["applicationsSync"], json!("sync"));
    assert!(p["metadata"].is_null() || p["metadata"]["annotations"].is_null());
}

#[test]
fn argocd_sync_patch_sets_operation() {
    let p = argocd_sync_patch();
    assert_eq!(p, json!({"operation": {"sync": {}}}));
}

fn argocd_appset(name: &str) -> serde_json::Value {
    json!({
        "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
        "metadata": {"name": name, "namespace": "argocd"},
        "spec": {
            "generators": [{"list": {"elements": [{"cluster": "dev-weu", "url": "https://kubernetes.default.svc"}]}}],
            "template": {
                "metadata": {"name": "app-{{cluster}}"},
                "spec": {
                    "source": {"repoURL": "https://github.com/example/repo", "path": "manifests"},
                    "destination": {"server": "{{url}}", "namespace": "default"},
                    "syncPolicy": {"automated": {"prune": true}}
                }
            },
            "syncPolicy": {"applicationsSync": "sync"}
        }
    })
}

#[tokio::test]
async fn argocd_appset_menu_opens_without_sync() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applicationsets");
    apply(&mut app, argocd_appset("guestbook-set"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::FluxMenu);
    assert_eq!(app.action_menu_items(), ARGOCD_APPSET_MENU_ITEMS);
    // No "Sync now" in the menu.
    assert!(!ARGOCD_APPSET_MENU_ITEMS.contains(&"Sync now"));
}

#[tokio::test]
async fn argocd_appset_menu_suspend() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applicationsets");
    apply(&mut app, argocd_appset("guestbook-set"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap(); // "Suspend"
    assert_eq!(app.mode, Mode::Table);
    assert!(
        app.flash.contains("suspending guestbook-set"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn argocd_appset_menu_resume() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applicationsets");
    apply(&mut app, argocd_appset("guestbook-set"));

    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    let idx = ARGOCD_APPSET_MENU_ITEMS
        .iter()
        .position(|s| *s == "Resume")
        .unwrap();
    app.flux_menu_state.select(Some(idx));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(
        app.flash.contains("resuming guestbook-set"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn argocd_appset_suspend_acts_on_marked_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("applicationsets");
    apply(&mut app, argocd_appset("set-a"));
    apply(&mut app, argocd_appset("set-b"));
    app.marked.insert("argocd/set-a".into());
    app.marked.insert("argocd/set-b".into());

    app.request_flux_menu();
    app.handle_key(press(KeyCode::Enter)).unwrap(); // "Suspend"
    assert!(
        app.flash.contains("suspending 2 applicationsets"),
        "{}",
        app.flash
    );
    assert!(app.marked.is_empty());
}

#[test]
fn cronjob_manual_job_matches_kubectl_create_job_from() {
    let cj: DynamicObject = serde_json::from_value(cronjob("backup")).unwrap();
    let job = cronjob_manual_job(&cj, "abc12").unwrap();
    assert_eq!(job["apiVersion"], "batch/v1");
    assert_eq!(job["kind"], "Job");
    assert_eq!(job["metadata"]["name"], "backup-manual-abc12");
    assert_eq!(job["metadata"]["namespace"], "default");
    assert_eq!(
        job["metadata"]["annotations"]["cronjob.kubernetes.io/instantiate"],
        "manual"
    );
    // jobTemplate metadata carries over alongside the instantiate marker.
    assert_eq!(job["metadata"]["annotations"]["team"], "platform");
    assert_eq!(job["metadata"]["labels"]["app"], "backup");
    // Owner reference points back at the CronJob (non-controller, like kubectl).
    assert_eq!(job["metadata"]["ownerReferences"][0]["kind"], "CronJob");
    assert_eq!(job["metadata"]["ownerReferences"][0]["name"], "backup");
    assert_eq!(job["metadata"]["ownerReferences"][0]["uid"], "cj-uid-1");
    // The Job spec is the jobTemplate's spec, verbatim.
    assert_eq!(
        job["spec"]["template"]["spec"]["containers"][0]["image"],
        "busybox"
    );
}

#[test]
fn cronjob_manual_job_requires_a_job_template() {
    let not_a_cj: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "p", "namespace": "default"}
    }))
    .unwrap();
    assert!(cronjob_manual_job(&not_a_cj, "x").is_none());
}

#[tokio::test]
async fn filter_edit_chords_delete_word_and_clear_line() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    assert_eq!(app.mode, Mode::Filter);
    for c in "hello world".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }

    // option+delete (alt-backspace) kills the last word…
    app.handle_key(alt(KeyCode::Backspace)).unwrap();
    assert_eq!(app.filter, "hello ");
    // …ctrl-w does the same, eating the separator first…
    app.handle_key(ctrl(KeyCode::Char('w'))).unwrap();
    assert_eq!(app.filter, "");

    // …and cmd+delete (ctrl-u) clears the whole line.
    for c in "app=nginx running".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    assert_eq!(app.filter, "");
    assert_eq!(app.mode, Mode::Filter); // still typing, not kicked out
}

#[tokio::test]
async fn command_palette_edit_chords() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "cronjobs kube-system".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(ctrl(KeyCode::Char('w'))).unwrap();
    assert_eq!(app.command, "cronjobs ");
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    assert_eq!(app.command, "");
}

#[tokio::test]
async fn namespace_picker_edit_chords() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.mode, Mode::Namespaces);
    for c in "monitoring".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(ctrl(KeyCode::Char('u'))).unwrap();
    assert_eq!(app.ns_filter, "");
    assert_eq!(app.mode, Mode::Namespaces);
}

#[test]
fn edit_chord_leaves_plain_keys_alone() {
    let mut buf = "abc".to_string();
    assert!(!edit_chord(&press(KeyCode::Char('u')), &mut buf));
    assert!(!edit_chord(&press(KeyCode::Backspace), &mut buf));
    assert_eq!(buf, "abc");
    // Word rubout trims trailing spaces before the word, readline-style.
    let mut buf = "one two   ".to_string();
    assert!(edit_chord(&alt(KeyCode::Backspace), &mut buf));
    assert_eq!(buf, "one ");
}

#[test]
fn manual_job_name_stays_within_limits() {
    assert_eq!(manual_job_name("backup", "abc12"), "backup-manual-abc12");
    let long = "x".repeat(80);
    let name = manual_job_name(&long, "abc12");
    assert_eq!(name.len(), 42 + "-manual-abc12".len());
    assert!(name.len() <= 63);
}

#[tokio::test]
async fn r_force_syncs_external_secrets() {
    let (mut app, _rx) = test_app();
    app.switch_kind("externalsecrets");
    apply(
        &mut app,
        json!({
            "apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
            "metadata": {"name": "creds", "namespace": "default"}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("refreshing creds"), "{}", app.flash);
    assert!(!app.flash_err);
}

#[tokio::test]
async fn refresh_es_rejects_non_es_kinds() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.request_refresh_es();
    assert!(app.flash_err);
    assert!(app.flash.contains("external secrets"), "{}", app.flash);
}

#[tokio::test]
async fn pf_palette_command_opens_the_view() {
    let (mut app, _rx) = test_app();
    assert!(app.run_palette_command("pf"));
    assert_eq!(app.mode, Mode::PortForwards);
}

#[tokio::test]
async fn crd_plural_outranks_builtin_command() {
    let (mut app, _rx) = test_app();
    // The fake cluster serves snapshots.kopiur.home-operations.com, which
    // collides with the `:snapshots` built-in — the CRD must win.
    app.mode = Mode::Command;
    for c in "snapshots".chars() {
        app.key_command(press(KeyCode::Char(c)));
    }
    app.key_command(press(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Table);
    assert!(
        app.flash.contains("snapshots.kopiur.home-operations.com"),
        "{}",
        app.flash
    );

    // `:kind namespace` still works for the shadowed plural.
    app.mode = Mode::Command;
    for c in "snapshots media".chars() {
        app.key_command(press(KeyCode::Char(c)));
    }
    app.key_command(press(KeyCode::Enter));
    assert_eq!(app.namespace, "media");
}

#[tokio::test]
async fn palette_completion_keys_are_rebindable() {
    let (mut app, _rx) = test_app();
    let keys_cfg: crate::config::Config = toml::from_str(
        r#"
        [keys]
        palette_next = "ctrl-n"
        palette_prev = "ctrl-p"
        palette_accept = ["ctrl-y", "enter"]
    "#,
    )
    .unwrap();
    let (palette_keys, warnings) = crate::config::compile_palette_keys(&keys_cfg.keys);
    assert!(warnings.is_empty(), "{warnings:?}");
    app.palette_keys = palette_keys;

    app.mode = Mode::Command;
    app.update_suggestions();
    assert!(app.cmd_suggestions.len() > 1);
    assert_eq!(app.cmd_sel, 0);

    // ctrl-n / ctrl-p move the highlight; the overridden tab/arrows don't.
    app.key_command(ctrl(KeyCode::Char('n')));
    assert_eq!(app.cmd_sel, 1);
    app.key_command(press(KeyCode::Tab));
    app.key_command(press(KeyCode::Down));
    assert_eq!(app.cmd_sel, 1, "tab/down were overridden away");
    app.key_command(ctrl(KeyCode::Char('p')));
    assert_eq!(app.cmd_sel, 0);

    // ctrl-y runs the command line exactly like enter does by default.
    for c in "snapshots media".chars() {
        app.key_command(press(KeyCode::Char(c)));
    }
    app.key_command(ctrl(KeyCode::Char('y')));
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.namespace, "media");

    // Enter stays usable because the config listed it too.
    app.mode = Mode::Command;
    for c in "pods".chars() {
        app.key_command(press(KeyCode::Char(c)));
    }
    app.key_command(press(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn qualified_names_surface_without_doubling_the_list() {
    let (mut app, _rx) = test_app();

    // A group fragment finds the CRD by its qualified name even though the
    // bare plural doesn't match.
    app.command = "kopiur".into();
    app.update_suggestions();
    assert_eq!(
        app.cmd_suggestions[0].label,
        "snapshots.kopiur.home-operations.com"
    );

    // When the bare plural matches and means the same kind, the qualified
    // twin stays hidden.
    app.command = "deploy".into();
    app.update_suggestions();
    let labels: Vec<_> = app.cmd_suggestions.iter().map(|s| &s.label).collect();
    assert!(labels.contains(&&"deployments".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&&"deployments.apps".to_string()),
        "{labels:?}"
    );

    // Browsing with `:` lists each kind once, under its bare plural.
    app.command.clear();
    app.update_suggestions();
    assert!(
        app.cmd_suggestions.iter().all(|s| !s.label.contains('.')),
        "{:?}",
        app.cmd_suggestions
            .iter()
            .map(|s| &s.label)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn typed_qualified_name_opens_the_kind() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Command;
    for c in "snapshots.kopiur.home-operations.com".chars() {
        app.key_command(press(KeyCode::Char(c)));
    }
    app.key_command(press(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Table);
    assert!(
        app.flash.contains("snapshots.kopiur.home-operations.com"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn shadowed_builtin_stays_reachable_via_alias() {
    let (mut app, _rx) = test_app();
    // `snapshots` now resolves to the CRD; the snapshot browser keeps its
    // `dumps` alias. Depending on the machine's state dir it either opens
    // the browser or warns that none are saved — both are the built-in.
    assert!(app.run_palette_command("dumps"));
    assert!(
        app.mode == Mode::Snapshots || app.flash.contains("no snapshots yet"),
        "mode={:?} flash={}",
        app.mode,
        app.flash
    );
}

#[tokio::test]
async fn events_palette_opens_resource_and_e_stays_contextual() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");

    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "events".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();

    assert_eq!(app.mode, Mode::Table);
    let kind = app.kind.as_ref().expect("events resource selected");
    assert_eq!(kind.ar.group, "events.k8s.io");
    assert_eq!(kind.title(), "events.events.k8s.io");
    assert!(!app.flash_err);

    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web",
                "namespace": "default",
                "uid": "pod-uid"
            }
        }),
    );
    app.handle_key(press(KeyCode::Char('E'))).unwrap();

    assert_eq!(app.mode, Mode::Events);
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.detail.title, "web — events");
    app.stop_event_stream();
}

#[test]
fn event_lines_show_core_event_fields() {
    let event = obj(json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {"name": "web.123", "namespace": "default"},
        "type": "Warning",
        "reason": "FailedScheduling",
        "message": "0/3 nodes are available",
        "count": 4,
        "lastTimestamp": "2026-07-04T12:34:56Z"
    }));
    let lines = format_event_lines([&event], false);
    assert!(lines[0].contains("LAST SEEN"));
    assert!(lines[1].contains("Warning"));
    assert!(lines[1].contains("FailedScheduling"));
    assert!(lines[1].contains("0/3 nodes are available"));
    assert!(lines[1].contains("4"));
}

fn spawn_test_child(argv0: &str, arg: &str) -> tokio::process::Child {
    tokio::process::Command::new(argv0)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn `{argv0} {arg}` for test: {e}"))
}

#[tokio::test]
async fn stopping_a_forward_kills_only_that_one() {
    let (mut app, _rx) = test_app();
    app.port_forwards.push(PortForward {
        config_name: None,
        ns: "default".into(),
        target: "pod/a".into(),
        ports: "8080:80".into(),
        child: spawn_test_child("sleep", "30"),
    });
    app.port_forwards.push(PortForward {
        config_name: None,
        ns: "default".into(),
        target: "pod/b".into(),
        ports: "8081:81".into(),
        child: spawn_test_child("sleep", "30"),
    });
    app.pf_state.select(Some(0));
    app.mode = Mode::PortForwards;

    app.handle_key(press(KeyCode::Char('x'))).unwrap();
    assert_eq!(app.port_forwards.len(), 1);
    assert_eq!(app.port_forwards[0].target, "pod/b");
    assert_eq!(app.pf_state.selected(), Some(0)); // cursor stays in range

    // Esc closes the view without touching the remaining forward.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.port_forwards.len(), 1);
}

#[tokio::test]
async fn reap_drops_exited_forwards_and_flashes() {
    let (mut app, _rx) = test_app();
    let mut child = spawn_test_child("true", "");
    child.wait().await.unwrap(); // let it exit before reaping
    app.port_forwards.push(PortForward {
        config_name: None,
        ns: "default".into(),
        target: "pod/a".into(),
        ports: "8080:80".into(),
        child,
    });
    app.reap_port_forwards();
    assert!(app.port_forwards.is_empty());
    assert!(app.flash.contains("exited"), "{}", app.flash);
}

#[test]
fn crd_served_version_prefers_storage_then_served() {
    let d = json!({"spec": {"versions": [
        {"name": "v1beta1", "served": true, "storage": false},
        {"name": "v1", "served": true, "storage": true}
    ]}});
    assert_eq!(crd_served_version(&d).as_deref(), Some("v1"));

    let d2 = json!({"spec": {"versions": [
        {"name": "v2", "served": false},
        {"name": "v1", "served": true}
    ]}});
    assert_eq!(crd_served_version(&d2).as_deref(), Some("v1"));
}

#[test]
fn mutating_action_patch_payloads_are_stable() {
    assert_eq!(
        restart_patch("2026-07-04T12:00:00Z"),
        json!({
            "spec": { "template": { "metadata": { "annotations": {
                "kubectl.kubernetes.io/restartedAt": "2026-07-04T12:00:00Z"
            }}}}
        })
    );
    assert_eq!(
        set_image_patch("pods", "app", "nginx:1.27"),
        json!({ "spec": { "containers": [{ "name": "app", "image": "nginx:1.27" }] } })
    );
    assert_eq!(
        set_image_patch("deployments", "app", "nginx:1.27"),
        json!({
            "spec": { "template": { "spec": {
                "containers": [{ "name": "app", "image": "nginx:1.27" }]
            }}}
        })
    );
    assert_eq!(scale_patch(3), json!({ "spec": { "replicas": 3 } }));
    assert_eq!(suspend_patch(true), json!({ "spec": { "suspend": true } }));
    assert_eq!(
        reconcile_patch("2026-07-04T12:00:00Z"),
        json!({
            "metadata": { "annotations": {
                "reconcile.fluxcd.io/requestedAt": "2026-07-04T12:00:00Z"
            }}
        })
    );
    assert_eq!(
        external_secret_refresh_patch("1783166400"),
        json!({ "metadata": { "annotations": { "force-sync": "1783166400" } } })
    );
    assert_eq!(
        node_unschedulable_patch(true),
        json!({ "spec": { "unschedulable": true } })
    );
    assert_eq!(
        node_unschedulable_patch(false),
        json!({ "spec": { "unschedulable": false } })
    );
}

#[tokio::test]
async fn crd_drill_builds_kind_from_spec() {
    let (mut app, _rx) = test_app();
    let crd = obj(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.com"},
        "spec": {
            "group": "example.com",
            "names": {"plural": "widgets", "kind": "Widget"},
            "scope": "Namespaced",
            "versions": [
                {"name": "v1beta1", "served": true, "storage": false},
                {"name": "v1", "served": true, "storage": true}
            ]
        }
    }));
    app.kind_plural = "customresourcedefinitions".into();
    // Not in the (fake) discovery registry → built straight from the spec.
    app.drill_into_crd(&crd);
    assert_eq!(app.kind_plural, "widgets");
    let k = app.kind.as_ref().unwrap();
    assert_eq!(k.ar.kind, "Widget");
    assert_eq!(k.ar.group, "example.com");
    assert_eq!(k.ar.version, "v1"); // storage version preferred
    assert_eq!(k.ar.api_version, "example.com/v1");
    assert!(k.namespaced);
    assert!(
        app.scope_label
            .as_deref()
            .unwrap()
            .contains("widgets.example.com")
    );
}

#[tokio::test]
async fn log_lines_expand_tabs_and_strip_cr() {
    let (mut app, _rx) = test_app();
    // Caddy-style tab-separated line (level would be color-wrapped too).
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec!["2026/07/01 09:21:14.062\tINFO\tProvisioning WAF\r".into()],
    });
    assert_eq!(
        app.logs.view.lines.back().unwrap(),
        "2026/07/01 09:21:14.062 INFO Provisioning WAF"
    );
}

#[tokio::test]
async fn log_buffer_is_capped() {
    let (mut app, _rx) = test_app();
    let cap = app.logs_cfg.buffer;
    for i in 0..(cap + 50) {
        app.handle_msg(Msg::LogLines {
            generation: app.log_gen,
            lines: vec![format!("line {i}")],
        });
    }
    assert_eq!(app.logs.view.lines.len(), cap);
    // Oldest lines dropped; newest retained.
    assert_eq!(
        app.logs.view.lines.back().unwrap(),
        &format!("line {}", cap + 49)
    );
}

#[tokio::test]
async fn filtered_log_text_respects_active_filter() {
    let (mut app, _rx) = test_app();
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec![
            "api request started".into(),
            "worker finished".into(),
            "api request finished".into(),
        ],
    });

    assert_eq!(
        app.filtered_log_text(),
        "api request started\nworker finished\napi request finished"
    );
    app.logs.set_filter("api".into());
    assert_eq!(
        app.filtered_log_text(),
        "api request started\napi request finished"
    );
}

#[tokio::test]
async fn log_filter_supports_regex_inverse_and_clear() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec![
            "GET /api 200".into(),
            "GET /healthz 200".into(),
            "GET /api 503".into(),
        ],
    });

    // Regex: keep 5xx.
    app.logs.set_filter("/5\\d\\d/".into());
    assert_eq!(app.filtered_log_text(), "GET /api 503");
    assert!(!app.logs.matcher.is_error());

    // Inverse substring: hide health checks.
    app.logs.set_filter("!healthz".into());
    assert_eq!(app.filtered_log_text(), "GET /api 200\nGET /api 503");

    // Bad regex: flagged, matches nothing.
    app.logs.set_filter("/[bad/".into());
    assert!(app.logs.matcher.is_error());
    assert_eq!(app.filtered_log_text(), "");

    // `z` clears the buffer (stream keeps running underneath).
    app.logs.set_filter(String::new());
    app.handle_key(press(KeyCode::Char('z'))).unwrap();
    assert!(app.logs.view.lines.is_empty());
    assert!(app.flash.contains("cleared"), "{}", app.flash);
}

#[tokio::test]
async fn log_save_result_survives_leaving_logs() {
    let (mut app, _rx) = test_app();
    let claim = app.claim_status("saving logs…");
    // Leaving Logs invalidates its stream but not the independent file write.
    app.log_gen += 1;
    app.handle_msg(Msg::LogsSaved {
        generation: app.generation,
        claim,
        result: Ok(std::env::temp_dir().join("sofka-test.log")),
    });
    assert!(app.flash.contains("sofka-test.log"));
    assert!(!app.flash_err);
}

#[tokio::test]
async fn stale_log_save_result_is_dropped_after_a_view_generation_change() {
    let (mut app, _rx) = test_app();
    let stale = app.generation;
    let claim = app.claim_status("saving logs…");
    app.bump_generation();
    app.handle_msg(Msg::LogsSaved {
        generation: stale,
        claim,
        result: Err("old write failed".into()),
    });
    assert!(!app.flash.contains("old write failed"));
}

#[tokio::test]
async fn stale_clipboard_result_is_dropped() {
    let (mut app, _rx) = test_app();
    let stale = app.generation;
    app.bump_generation();

    let claim = app.claim_status("copying to clipboard…");
    app.handle_msg(Msg::ClipboardCopied {
        generation: stale,
        claim,
        copied: false,
        success: "copied stale".into(),
        failure: "stale failed".into(),
    });
    assert!(!app.flash.contains("stale failed"));

    let claim = app.claim_status("copying to clipboard…");
    app.handle_msg(Msg::ClipboardCopied {
        generation: app.generation,
        claim,
        copied: true,
        success: "copied current".into(),
        failure: "current failed".into(),
    });
    assert_eq!(app.flash, "copied current");
    assert!(!app.flash_err);
}

#[test]
fn osc52_sequence_base64_encodes_clipboard_text() {
    assert_eq!(osc52_sequence("sofka"), "\x1b]52;c;c29ma2E=\x07");
}

#[tokio::test]
async fn sort_by_numeric_column_and_invert() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let pod = |name: &str, restarts: i64| {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    {"ready": true, "restartCount": restarts, "state": {"running": {}}}
                ]
            }
        })
    };
    apply(&mut app, pod("a", 5));
    apply(&mut app, pod("b", 1));
    apply(&mut app, pod("c", 9));

    // RESTARTS is the 4th pod column; sort by it numerically (not "1,5,9"
    // as strings, which happens to agree here, but parsing is what matters).
    assert_eq!(app.display_headers()[3], "RESTARTS");
    app.sort_column = Some(3);
    app.invalidate_rows();
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["b", "a", "c"]); // 1, 5, 9 ascending

    app.sort_desc = true;
    app.invalidate_rows();
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["c", "a", "b"]); // descending

    // Switching kinds resets the sort (columns differ).
    app.switch_kind("services");
    assert_eq!(app.sort_column, None);
    assert!(!app.sort_desc);
}

#[tokio::test]
async fn name_sort_uses_natural_numeric_segments() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for name in ["pod-10", "pod-2", "pod-11", "pod-0", "pod-9", "pod-1"] {
        apply(
            &mut app,
            json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {"name": name, "namespace": "default"}
            }),
        );
    }
    let names = |app: &App| -> Vec<String> {
        app.rows()
            .iter()
            .map(|object| object.metadata.name.clone().unwrap())
            .collect()
    };

    assert_eq!(
        names(&app),
        ["pod-0", "pod-1", "pod-2", "pod-9", "pod-10", "pod-11"]
    );

    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    app.sort_picker_state.select(Some(1));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.sort_column, Some(0));
    assert_eq!(
        names(&app),
        ["pod-0", "pod-1", "pod-2", "pod-9", "pod-10", "pod-11"]
    );

    app.handle_key(press(KeyCode::Char('I'))).unwrap();
    assert_eq!(
        names(&app),
        ["pod-11", "pod-10", "pod-9", "pod-2", "pod-1", "pod-0"]
    );
}

#[test]
fn natural_comparison_handles_leading_zeroes_and_large_numbers() {
    assert_eq!(natural_cmp("pod-2", "pod-02"), std::cmp::Ordering::Less);
    assert_eq!(natural_cmp("pod-02a", "pod-2b"), std::cmp::Ordering::Less);
    assert_eq!(
        natural_cmp("pod-99999999999999999999", "pod-100000000000000000000"),
        std::cmp::Ordering::Less
    );
}

#[tokio::test]
async fn sorted_order_updates_when_an_object_changes() {
    // Sort keys are cached per resourceVersion; an update must invalidate the
    // changed row's cached key (via invalidate_row) and re-sort with the new
    // value, while unchanged rows reuse theirs.
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let pod = |name: &str, rv: &str, restarts: i64| {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "default", "resourceVersion": rv},
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    {"ready": true, "restartCount": restarts, "state": {"running": {}}}
                ]
            }
        })
    };
    apply(&mut app, pod("a", "1", 5));
    apply(&mut app, pod("b", "1", 1));
    apply(&mut app, pod("c", "1", 9));
    assert_eq!(app.display_headers()[3], "RESTARTS");
    app.sort_column = Some(3);
    app.invalidate_rows();
    let names = |app: &App| -> Vec<String> {
        app.rows()
            .iter()
            .map(|o| o.metadata.name.clone().unwrap())
            .collect()
    };
    assert_eq!(names(&app), ["b", "a", "c"]); // 1, 5, 9

    apply(&mut app, pod("b", "2", 20));
    assert_eq!(names(&app), ["a", "c", "b"]); // 5, 9, 20

    // Changing the sort column must not reuse keys computed for the old one.
    assert_eq!(app.display_headers()[0], "NAME");
    app.sort_column = Some(0);
    app.invalidate_rows();
    assert_eq!(names(&app), ["a", "b", "c"]);
}

#[tokio::test]
async fn sort_picker_picks_toggles_and_clears() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");

    // `S` opens the picker: default entry pinned first and selected (no sort).
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    assert_eq!(app.mode, Mode::SortPicker);
    assert_eq!(app.filtered_sort_entries()[0], DEFAULT_SORT_LABEL);
    assert_eq!(app.sort_picker_state.selected(), Some(0));

    // Type-to-filter fuzzy-matches columns; the cursor lands on the best
    // match (right after the pinned default) and enter selects it, ascending.
    for c in "rst".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    assert_eq!(app.filtered_sort_entries()[1], "RESTARTS");
    assert_eq!(app.sort_picker_state.selected(), Some(1));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    let restarts = app.display_headers().iter().position(|h| h == "RESTARTS");
    assert!(restarts.is_some());
    assert_eq!(app.sort_column, restarts);
    assert!(!app.sort_desc);
    assert!(app.flash.contains("RESTARTS") && app.flash.contains("asc"));

    // Reopening lands on the active column; re-picking it inverts direction.
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    assert_eq!(app.sort_picker_state.selected(), restarts.map(|i| i + 1));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.sort_column, restarts);
    assert!(app.sort_desc);

    // Picking a different column resets to ascending.
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    app.sort_picker_state.select(Some(1)); // NAME (first column)
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.sort_column, Some(0));
    assert!(!app.sort_desc);

    // The pinned default entry clears the sort.
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    app.sort_picker_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.sort_column, None);
    assert!(!app.sort_desc);
}

#[tokio::test]
async fn sort_choice_is_remembered_per_kind_across_view_switches() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");

    // Pick RESTARTS via the picker, then invert with `I`.
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    for c in "rst".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    app.handle_key(press(KeyCode::Char('I'))).unwrap();
    let restarts = app.display_headers().iter().position(|h| h == "RESTARTS");
    assert_eq!(app.sort_column, restarts);
    assert!(app.sort_desc);
    assert_eq!(app.sort_memory.get("pods"), Some(("RESTARTS".into(), true)));

    // A kind with no memory starts unsorted; the pods memory is untouched.
    app.switch_kind("deployments");
    assert_eq!(app.sort_column, None);
    assert_eq!(app.sort_memory.get("pods"), Some(("RESTARTS".into(), true)));

    // Coming back restores column and direction.
    app.switch_kind("pods");
    assert_eq!(
        app.sort_column,
        app.display_headers().iter().position(|h| h == "RESTARTS")
    );
    assert!(app.sort_desc);

    // A header click is remembered too.
    app.switch_kind("deployments");
    let ready = app
        .display_headers()
        .iter()
        .position(|h| h == "READY")
        .unwrap();
    app.sort_column = Some(ready);
    app.sort_desc = false;
    app.remember_sort();
    assert_eq!(
        app.sort_memory.get("deployments"),
        Some(("READY".into(), false))
    );

    // Picking the pinned default forgets the kind's memory.
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    app.sort_picker_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.sort_column, None);
    assert_eq!(app.sort_memory.get("pods"), None);
    app.switch_kind("deployments");
    app.switch_kind("pods");
    assert_eq!(app.sort_column, None);
}

#[tokio::test]
async fn sort_picker_esc_clears_filter_then_closes() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    app.handle_key(press(KeyCode::Char('x'))).unwrap();
    assert_eq!(app.sort_picker_filter, "x");

    // First esc clears the filter and stays open; second esc closes without
    // touching the sort.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::SortPicker);
    assert!(app.sort_picker_filter.is_empty());
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.sort_column, None);
}

#[tokio::test]
async fn copy_picker_lists_full_row_fields_and_filters_on_values() {
    let (mut app, _rx) = test_app();
    app.switch_kind("services");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Service",
               "metadata": {"name": "web", "namespace": "default"},
               "spec": {"type": "ClusterIP", "clusterIP": "10.96.13.5",
                        "ports": [{"port": 80, "protocol": "TCP"}]}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Char('Y'))).unwrap();
    assert_eq!(app.mode, Mode::CopyPicker);
    let fields = app.copy_picker_fields.clone();
    assert!(
        fields.contains(&("NAME".into(), "web".into())),
        "{fields:?}"
    );
    assert!(
        fields.contains(&("CLUSTER-IP".into(), "10.96.13.5".into())),
        "{fields:?}"
    );
    // AGE has no creationTimestamp here — empty cells carry nothing to copy.
    assert!(fields.iter().all(|(_, v)| !v.is_empty()), "{fields:?}");

    // Typing part of the *value* (not the header) finds the IP, and the
    // cursor tracks the best match.
    for c in "96.13".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    let entries = app.filtered_copy_entries();
    assert_eq!(entries[0], ("CLUSTER-IP".into(), "10.96.13.5".into()));
    assert_eq!(app.copy_picker_state.selected(), Some(0));

    // First esc clears the filter and stays open; second esc closes.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::CopyPicker);
    assert!(app.copy_picker_filter.is_empty());
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn copy_picker_without_a_selection_warns_and_stays_in_table() {
    let (mut app, _rx) = test_app();
    app.switch_kind("services");
    app.handle_key(press(KeyCode::Char('Y'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash_err);
    assert!(app.flash.contains("no row selected"));
}

#[tokio::test]
async fn metrics_update_invalidates_metric_sorted_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for name in ["a", "b"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": name, "namespace": "default"}}),
        );
    }

    let cpu_idx = app
        .display_headers()
        .iter()
        .position(|h| *h == "CPU")
        .unwrap();
    app.sort_column = Some(cpu_idx);
    app.sort_desc = true;
    app.invalidate_rows();
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]); // cached before metrics arrive

    app.handle_msg(Msg::Metrics {
        generation: app.generation,
        data: HashMap::from([
            ("default/a".to_string(), (10, 0)),
            ("default/b".to_string(), (100, 0)),
        ]),
        containers: HashMap::new(),
    });
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["b", "a"]);
}

#[tokio::test]
async fn node_capacity_percent_columns_render_and_sort() {
    let (mut app, _rx) = test_app();
    // Pods must not grow the node columns.
    app.switch_kind("pods");
    assert!(!app.display_headers().contains(&"%CPU".to_string()));

    app.switch_kind("nodes");
    let node = |name: &str, cpu: &str| {
        json!({"apiVersion": "v1", "kind": "Node",
               "metadata": {"name": name, "resourceVersion": "1"},
               "status": {"allocatable": {"cpu": cpu, "memory": "8Gi"}}})
    };
    apply(&mut app, node("big", "4"));
    apply(&mut app, node("small", "2"));

    let headers = app.display_headers();
    assert!(headers.contains(&"%CPU".to_string()), "{headers:?}");
    assert!(headers.contains(&"%MEM".to_string()), "{headers:?}");

    // Same absolute usage, different allocatable → percent sort differs from
    // absolute sort: 1000m is 25% of big (4 cores) but 50% of small (2).
    app.handle_msg(Msg::Metrics {
        generation: app.generation,
        data: HashMap::from([
            ("big".to_string(), (1000, 0)),
            ("small".to_string(), (1000, 0)),
        ]),
        containers: HashMap::new(),
    });
    let pct_idx = app
        .display_headers()
        .iter()
        .position(|h| *h == "%CPU")
        .unwrap();
    app.sort_column = Some(pct_idx);
    app.sort_desc = true;
    app.invalidate_rows();
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["small", "big"]);
}

#[tokio::test]
async fn nodes_view_pods_column_counts_and_sorts() {
    let (mut app, _rx) = test_app();
    // Pods must not grow a PODS column.
    app.switch_kind("pods");
    assert!(!app.display_headers().contains(&"PODS".to_string()));

    app.switch_kind("nodes");
    let headers = app.display_headers();
    let pods_idx = headers.iter().position(|h| *h == "PODS").unwrap();
    // PODS sits right before the CPU/MEM usage columns, k9s-style.
    assert_eq!(headers[pods_idx + 1], "CPU", "{headers:?}");

    for n in ["node-a", "node-b"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Node",
                   "metadata": {"name": n, "resourceVersion": "1"}}),
        );
    }

    // Before the first pods list lands the column reads "-", not a fake 0,
    // and the capture stays one cell per column (nodes carry PODS and
    // %CPU/%MEM headers beyond the base spec).
    let (cols, rows) = app.snapshot_table();
    assert!(rows.iter().all(|r| r.len() == cols.len()), "{rows:?}");
    assert!(rows.iter().all(|r| r[pods_idx] == "-"), "{rows:?}");

    app.handle_msg(Msg::NodePods {
        generation: app.generation,
        counts: HashMap::from([("node-b".to_string(), 7)]),
    });
    // node-a has no entry → genuinely zero pods once data exists.
    let (_, rows) = app.snapshot_table();
    let cell = |name: &str| {
        rows.iter()
            .find(|r| r[0] == name)
            .map(|r| r[pods_idx].clone())
            .unwrap()
    };
    assert_eq!(cell("node-a"), "0");
    assert_eq!(cell("node-b"), "7");

    app.sort_column = Some(pods_idx);
    app.sort_desc = true;
    app.invalidate_rows();
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["node-b", "node-a"]);
}

#[tokio::test]
async fn node_pods_update_invalidates_pods_sorted_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("nodes");
    for n in ["node-a", "node-b"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Node",
                   "metadata": {"name": n, "resourceVersion": "1"}}),
        );
    }
    let pods_idx = app
        .display_headers()
        .iter()
        .position(|h| *h == "PODS")
        .unwrap();
    app.sort_column = Some(pods_idx);
    app.sort_desc = true;
    app.invalidate_rows();
    let _ = app.rows(); // warm the sorted row cache

    // A fresh count snapshot must resort without any other invalidation.
    app.handle_msg(Msg::NodePods {
        generation: app.generation,
        counts: HashMap::from([("node-a".to_string(), 2), ("node-b".to_string(), 9)]),
    });
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["node-b", "node-a"]);
}

#[test]
fn node_allocatable_reads_status_quantities() {
    let node = obj(json!({"apiVersion": "v1", "kind": "Node",
        "metadata": {"name": "n"},
        "status": {"allocatable": {"cpu": "3900m", "memory": "8Gi"}}}));
    assert_eq!(
        crate::columns::node_allocatable(&node),
        (Some(3900), Some(8 * 1024 * 1024 * 1024))
    );
    let bare = obj(json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n"}}));
    assert_eq!(crate::columns::node_allocatable(&bare), (None, None));
}

#[test]
fn pod_metrics_are_split_by_container() {
    let metrics = obj(json!({
        "apiVersion": "metrics.k8s.io/v1beta1",
        "kind": "PodMetrics",
        "metadata": {"name": "api", "namespace": "default"},
        "containers": [
            {"name": "app", "usage": {"cpu": "125m", "memory": "64Mi"}},
            {"name": "sidecar", "usage": {"cpu": "50000000n", "memory": "16Mi"}}
        ]
    }));

    assert_eq!(
        container_usage_of(&metrics),
        vec![
            ("app".into(), (125, 64 * 1024 * 1024)),
            ("sidecar".into(), (50, 16 * 1024 * 1024)),
        ]
    );
    assert_eq!(usage_of(&metrics, false), (175, 80 * 1024 * 1024));
}

#[tokio::test]
async fn container_picker_reads_latest_metrics_snapshot() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api", "namespace": "default"},
            "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]}
        }),
    );
    app.handle_msg(Msg::Metrics {
        generation: app.generation,
        data: HashMap::from([("default/api".into(), (175, 80 * 1024 * 1024))]),
        containers: HashMap::from([
            ("default/api/app".into(), (125, 64 * 1024 * 1024)),
            ("default/api/sidecar".into(), (50, 16 * 1024 * 1024)),
        ]),
    });

    let pod = app.selected().unwrap();
    app.open_containers(&pod);
    assert_eq!(
        app.selected_pod_container_metrics("app"),
        Some((125, 64 * 1024 * 1024))
    );
    assert_eq!(app.selected_pod_container_metrics("missing"), None);
}

#[test]
fn container_resources_extracted_per_container() {
    use crate::columns::ContainerResources;
    let pod = obj(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "api", "namespace": "default"},
        "spec": {"containers": [
            {"name": "app", "resources": {
                "requests": {"cpu": "250m", "memory": "64Mi"},
                "limits": {"cpu": "500m", "memory": "128Mi"}
            }},
            // sidecar declares only a request, no limits -> those stay None.
            {"name": "sidecar", "resources": {"requests": {"cpu": "50m"}}}
        ]}
    }));

    let res: std::collections::HashMap<_, _> = container_resources_of(&pod).into_iter().collect();
    assert_eq!(
        res["app"],
        ContainerResources {
            cpu_request: Some(250),
            cpu_limit: Some(500),
            mem_request: Some(64 * 1024 * 1024),
            mem_limit: Some(128 * 1024 * 1024),
        }
    );
    assert_eq!(
        res["sidecar"],
        ContainerResources {
            cpu_request: Some(50),
            cpu_limit: None,
            mem_request: None,
            mem_limit: None,
        }
    );
}

#[test]
fn qos_class_prefers_status_then_computes() {
    // The API server's status.qosClass is authoritative when present.
    let with_status = obj(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "api"},
        "spec": {"containers": [{"name": "app"}]},
        "status": {"qosClass": "Burstable"}
    }));
    assert_eq!(qos_class(&with_status), "Burstable");

    // No status: derive Guaranteed when every resource has request == limit.
    let guaranteed = obj(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "api"},
        "spec": {"containers": [{"name": "app", "resources": {
            "requests": {"cpu": "500m", "memory": "128Mi"},
            "limits": {"cpu": "500m", "memory": "128Mi"}
        }}]}
    }));
    assert_eq!(qos_class(&guaranteed), "Guaranteed");

    // Requests below limits -> Burstable.
    let burstable = obj(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "api"},
        "spec": {"containers": [{"name": "app", "resources": {
            "requests": {"cpu": "250m"},
            "limits": {"cpu": "500m"}
        }}]}
    }));
    assert_eq!(qos_class(&burstable), "Burstable");

    // No requests or limits at all -> BestEffort.
    let besteffort = obj(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "api"},
        "spec": {"containers": [{"name": "app"}]}
    }));
    assert_eq!(qos_class(&besteffort), "BestEffort");
}

#[tokio::test]
async fn container_picker_populates_resources_and_qos() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api", "namespace": "default"},
            "spec": {"containers": [{"name": "app", "resources": {
                "requests": {"cpu": "250m", "memory": "64Mi"},
                "limits": {"cpu": "500m", "memory": "128Mi"}
            }}]},
            "status": {"qosClass": "Burstable"}
        }),
    );

    let pod = app.selected().unwrap();
    app.open_containers(&pod);
    assert_eq!(app.container_qos, "Burstable");
    assert_eq!(app.container_resources["app"].cpu_request, Some(250));
    assert_eq!(
        app.container_resources["app"].mem_limit,
        Some(128 * 1024 * 1024)
    );
}

#[tokio::test]
async fn container_picker_renders_qos_and_utilization() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "mailerlite-app-horizon-mail-7dbdf476f8", "namespace": "default"},
            "spec": {"containers": [
                {"name": "mailerlite-mailerlite-app-horizon-mail", "resources": {
                    "requests": {"cpu": "250m", "memory": "128Mi"},
                    "limits": {"cpu": "500m", "memory": "256Mi"}
                }},
                // istio-proxy declares a request but no memory limit -> "-".
                {"name": "istio-proxy", "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}}
            ]},
            "status": {"qosClass": "Burstable"}
        }),
    );
    let key = "default/mailerlite-app-horizon-mail-7dbdf476f8";
    app.handle_msg(Msg::Metrics {
        generation: app.generation,
        data: HashMap::from([(key.into(), (175, 80 * 1024 * 1024))]),
        containers: HashMap::from([
            (
                format!("{key}/mailerlite-mailerlite-app-horizon-mail"),
                (125, 64 * 1024 * 1024),
            ),
            (format!("{key}/istio-proxy"), (50, 16 * 1024 * 1024)),
        ]),
    });

    let pod = app.selected().unwrap();
    app.open_containers(&pod);

    let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let buffer = term.backend().buffer().clone();
    let screen: String = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // QoS is surfaced in the popup title, and the table has a labeled header.
    assert!(screen.contains("Burstable"), "missing QoS in:\n{screen}");
    assert!(
        screen.contains("NAME") && screen.contains("CPU") && screen.contains("MEM"),
        "missing column header in:\n{screen}"
    );
    // 125m of a 250m request / 500m limit, and 64Mi of 128Mi / 256Mi.
    assert!(
        screen.contains("50%/25%"),
        "missing utilization percentages in:\n{screen}"
    );
    // The istio-proxy's unset memory limit renders as a "-" in its pair
    // (16Mi of a 128Mi request, no limit).
    assert!(
        screen.contains("13%/-"),
        "missing missing-limit indicator in:\n{screen}"
    );
}

#[tokio::test]
async fn narrow_window_keeps_full_external_ip_visible() {
    // #166: with `Fill`-weighted widths, NAME hoarded padding on narrow
    // windows while EXTERNAL-IP was silently trimmed. Content-aware widths
    // must show the whole IP whenever the frame can fit every column's
    // widest value.
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut app, _rx) = test_app();
    app.switch_kind("services");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"type": "LoadBalancer", "clusterIP": "10.0.0.1",
                     "ports": [{"port": 80, "protocol": "TCP"}]},
            "status": {"loadBalancer": {"ingress": [{"ip": "203.0.113.219"}]}}
        }),
    );
    app.table_state.select(Some(0));

    let mut term = Terminal::new(TestBackend::new(70, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let buffer = term.backend().buffer().clone();
    let screen: String = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        screen.contains("203.0.113.219"),
        "external IP trimmed in:\n{screen}"
    );
    assert!(
        screen.contains("LoadBalancer"),
        "type column trimmed in:\n{screen}"
    );
}

#[tokio::test]
async fn logs_keep_view_and_restore_selection() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["a", "b", "c"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    app.table_state.select(Some(1)); // "b"
    assert_eq!(app.selected().unwrap().metadata.name.as_deref(), Some("b"));
    let gen_before = app.generation;

    app.handle_key(press(KeyCode::Char('l'))).unwrap(); // open logs
    assert_eq!(app.mode, Mode::Logs);
    assert_eq!(app.rows().len(), 3, "underlying view stays populated");

    app.handle_key(press(KeyCode::Esc)).unwrap(); // back to table
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(
        app.generation, gen_before,
        "view watch was not torn down/restarted"
    );
    assert_eq!(app.rows().len(), 3, "rows were not blanked + reloaded");
    assert_eq!(
        app.selected().unwrap().metadata.name.as_deref(),
        Some("b"),
        "cursor returned to the same pod"
    );
}

#[tokio::test]
async fn namespace_switcher_pins_all_and_fuzzy_filters() {
    let (mut app, _rx) = test_app();
    app.ns_list = vec![
        "<all>".into(),
        "default".into(),
        "kube-system".into(),
        "prod".into(),
    ];
    // No filter: <all> first, then the rest.
    assert_eq!(app.filtered_namespaces()[0], "<all>");
    assert_eq!(app.filtered_namespaces().len(), 4);

    // Fuzzy filter (subsequence) keeps <all> pinned on top.
    app.ns_filter = "sys".into();
    let f = app.filtered_namespaces();
    assert_eq!(f[0], "<all>");
    assert!(f.contains(&"kube-system".to_string()));
    assert!(!f.contains(&"default".to_string()));

    // Typing a name that matches nothing real → Enter takes it verbatim.
    app.ns_filter = "team-x".into();
    app.mode = Mode::Namespaces;
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.namespace, "team-x");
}

#[tokio::test]
async fn shellouts_pin_to_active_context() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "p", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.request_edit();
    let Some(Suspend::Shell(argv)) = app.pending.take() else {
        panic!("expected a pending shell command");
    };
    // Pinned to the context sofka connected with, not kubectl's default.
    assert_eq!(&argv[..3], ["kubectl", "--context", "test"]);
    assert!(argv.contains(&"edit".to_string()));
    assert_eq!(argv.last().unwrap(), "default"); // -n <ns>
}

#[tokio::test]
async fn snapshot_rejects_unknown_format() {
    let (mut app, _rx) = app_with_pod();
    app.take_snapshot("xml");
    assert!(
        app.flash.contains("unknown snapshot format"),
        "{}",
        app.flash
    );
    assert!(app.flash_err);
}

#[tokio::test]
async fn snapshot_captures_current_columns_and_rows() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["api-1", "api-2"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    app.table_state.select(Some(0));
    let (columns, rows) = app.snapshot_table();
    assert!(columns.iter().any(|c| c == "NAME"));
    assert_eq!(rows.len(), 2);
    // Every captured row carries a cell per column.
    assert!(rows.iter().all(|r| r.len() == columns.len()));
    assert!(rows.iter().any(|r| r.contains(&"api-1".to_string())));
}

#[tokio::test]
async fn bundle_save_without_a_pending_bundle_warns() {
    let (mut app, _rx) = app_with_pod();
    app.save_bundle();
    assert!(app.flash.contains("no bundle to save"), "{}", app.flash);
    assert!(app.flash_err);
}

#[tokio::test]
async fn bundle_needs_a_selection() {
    let (mut app, _rx) = test_app();
    // No kind/selection yet — bundle refuses rather than assembling nothing.
    app.open_bundle();
    assert_ne!(app.mode, Mode::Detail);
    assert!(app.flash_err);
}

#[tokio::test]
async fn bundle_redaction_strips_secret_values_from_yaml() {
    // A real DynamicObject through the app's own YAML path must not leak the
    // Secret's data once redacted.
    let (mut app, _rx) = test_app();
    app.switch_kind("secrets");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Secret",
               "metadata": {"name": "creds", "namespace": "default"},
               "data": {"password": "aHVudGVyMg=="}}),
    );
    app.table_state.select(Some(0));
    let obj = app.selected_ref().unwrap().clone();
    let (yaml, notes) = crate::bundle::redact_to_yaml(&obj, "Secret");
    let joined = yaml.join("\n");
    assert!(
        !joined.contains("aHVudGVyMg=="),
        "secret value leaked: {joined}"
    );
    assert!(joined.contains(crate::bundle::REDACTED));
    assert!(notes.iter().any(|n| n.contains("Secret data")));
}

#[tokio::test]
async fn container_picker_shell_targets_selected_container() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "p", "namespace": "default"},
               "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]}}),
    );
    app.table_state.select(Some(0));
    let obj = app.selected_ref().unwrap().clone();
    app.open_containers(&obj);
    assert_eq!(app.mode, Mode::Containers);
    app.container_state.select(Some(1)); // "sidecar"
    app.handle_key(press(KeyCode::Char('s'))).unwrap();
    let Some(Suspend::Shell(argv)) = app.pending.take() else {
        panic!("expected a pending shell command");
    };
    assert!(argv.contains(&"-c".to_string()));
    let c_idx = argv.iter().position(|a| a == "-c").unwrap();
    assert_eq!(argv[c_idx + 1], "sidecar");
    assert!(argv.contains(&"p".to_string()));
}

#[tokio::test]
async fn transfer_menu_chains_download_prompts() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "p", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::TransferMenu);
    // Enter on the default selection ("Download from pod") → source prompt.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    assert!(
        app.prompt_label.contains("remote path"),
        "{}",
        app.prompt_label
    );
    app.prompt_input = "/var/log/app.log".into();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    // Chains into the destination prompt, prefilled with the file name.
    assert_eq!(app.mode, Mode::Prompt);
    assert!(
        app.prompt_label.contains("local path"),
        "{}",
        app.prompt_label
    );
    assert_eq!(app.prompt_input, "app.log");
}

#[tokio::test]
async fn transfer_argv_pins_context_and_direction() {
    let (app, _rx) = test_app();
    let argv = app.cp_argv(
        "default",
        "p",
        Some("sidecar"),
        false,
        "/var/log/app.log",
        "app.log",
    );
    assert_eq!(&argv[..3], ["kubectl", "--context", "test"]);
    assert_eq!(argv[3], "cp");
    let n = argv.iter().position(|a| a == "-n").unwrap();
    assert_eq!(argv[n + 1], "default");
    let c = argv.iter().position(|a| a == "-c").unwrap();
    assert_eq!(argv[c + 1], "sidecar");
    // Download: remote source, local destination.
    assert_eq!(argv[argv.len() - 2], "p:/var/log/app.log");
    assert_eq!(argv[argv.len() - 1], "app.log");

    // Upload flips the direction (and no -c without a container pin).
    let argv = app.cp_argv("default", "p", None, true, "notes.txt", "/tmp/notes.txt");
    assert_eq!(argv[argv.len() - 2], "notes.txt");
    assert_eq!(argv[argv.len() - 1], "p:/tmp/notes.txt");
    assert!(!argv.contains(&"-c".to_string()));
}

#[tokio::test]
async fn transfer_upload_blocked_in_readonly() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "p", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.readonly = true;
    // The menu itself opens — download doesn't mutate anything.
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::TransferMenu);
    // Choosing "Upload to pod" is refused.
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("read-only"), "{}", app.flash);
    assert_ne!(app.mode, Mode::Prompt);
}

#[tokio::test]
async fn container_picker_transfer_pins_container() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "p", "namespace": "default"},
               "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]}}),
    );
    app.table_state.select(Some(0));
    let obj = app.selected_ref().unwrap().clone();
    app.open_containers(&obj);
    app.container_state.select(Some(1)); // "sidecar"
    app.handle_key(press(KeyCode::Char('t'))).unwrap();
    assert_eq!(app.mode, Mode::TransferMenu);
    assert_eq!(
        app.transfer_target,
        Some(("default".into(), "p".into(), Some("sidecar".into())))
    );
}

#[tokio::test]
async fn paused_logs_do_not_trim_below_paused_cap() {
    let (mut app, _rx) = test_app();
    let cap = app.logs_cfg.buffer;
    app.logs.follow = false; // autoscroll OFF
    let lg = app.log_gen;
    let line = |i: usize| Msg::LogLines {
        generation: lg,
        lines: vec![format!("line {i}")],
    };
    // Well past the *following* cap, but under the paused cap: nothing is
    // dropped, so a frozen view never appears to resume scrolling.
    for i in 0..(cap + 500) {
        app.handle_msg(line(i));
    }
    assert_eq!(app.logs.view.lines.len(), cap + 500);

    // Resuming follow trims the backlog back to the tight cap.
    app.mode = Mode::Logs;
    app.handle_key(press(KeyCode::Char('s'))).unwrap(); // follow on
    assert!(app.logs.follow);
    assert_eq!(app.logs.view.lines.len(), cap);
}

#[tokio::test]
async fn paused_trim_shifts_scroll_in_display_rows() {
    let (mut app, _rx) = test_app();
    app.logs.follow = false;
    app.logs.last_wrap_width = 10; // as if the last draw wrapped at 10 cols
    app.logs.view.scroll = 500;
    let lg = app.log_gen;
    // The first line is the one trimmed later: 25 chars → 3 rows at width 10.
    app.handle_msg(Msg::LogLines {
        generation: lg,
        lines: vec!["a".repeat(25)],
    });
    for i in 1..MAX_LOG_LINES_PAUSED {
        app.handle_msg(Msg::LogLines {
            generation: lg,
            lines: vec![format!("l{i}")],
        });
    }
    assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES_PAUSED);
    assert_eq!(app.logs.view.scroll, 500); // nothing trimmed yet
    // One more line overflows the paused cap: the wrapped first line drains
    // and the frozen anchor shifts by its 3 display rows, not by 1 line.
    app.handle_msg(Msg::LogLines {
        generation: lg,
        lines: vec!["x".into()],
    });
    assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES_PAUSED);
    assert_eq!(app.logs.view.scroll, 497);
}

#[tokio::test]
async fn rbac_for_other_namespace_is_dropped() {
    let (mut app, _rx) = test_app();
    // App starts in the "default" namespace.
    let mut other = HashSet::new();
    other.insert("secrets".to_string());
    app.handle_msg(Msg::Rbac {
        generation: app.generation,
        ns: "kube-system".into(),
        allowed: other,
    });
    assert!(app.rbac_allowed.is_none(), "stale-namespace result dropped");

    let mut here = HashSet::new();
    here.insert("pods".to_string());
    app.handle_msg(Msg::Rbac {
        generation: app.generation,
        ns: "default".into(),
        allowed: here,
    });
    assert!(app.rbac_allowed.is_some());
    assert!(app.rbac_visible("pods"));
    assert!(!app.rbac_visible("secrets"));
}

#[tokio::test]
async fn stale_async_picker_results_are_dropped() {
    let (mut app, _rx) = test_app();
    let stale = app.generation;
    app.bump_generation();

    app.ns_list = vec!["<all>".into()];
    app.handle_msg(Msg::Namespaces {
        generation: stale,
        list: vec!["<all>".into(), "stale".into()],
    });
    assert_eq!(app.ns_list, vec!["<all>".to_string()]);

    app.ctx_list = vec!["test".into()];
    app.handle_msg(Msg::Contexts {
        generation: stale,
        list: vec!["stale-context".into()],
    });
    assert_eq!(app.ctx_list, vec!["test".to_string()]);

    let flash = app.flash.clone();
    app.handle_msg(Msg::ContextSwitched {
        generation: stale,
        name: "old-context".into(),
        result: Err("old failure".into()),
    });
    assert_eq!(app.flash, flash);
}

#[tokio::test]
async fn context_list_result_selects_current_context() {
    let (mut app, _rx) = test_app();
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod".into(), "test".into()],
    });
    assert_eq!(app.ctx_list, vec!["prod".to_string(), "test".to_string()]);
    assert_eq!(app.ctx_state.selected(), Some(1));
}

#[tokio::test]
async fn context_picker_typing_filters_and_backspace_widens() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["dev".into(), "prod".into(), "test".into()],
    });

    app.handle_key(press(KeyCode::Char('k'))).unwrap();
    assert_eq!(app.ctx_state.selected(), Some(1));
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    assert_eq!(app.ctx_state.selected(), Some(2));
    assert!(app.ctx_filter.is_empty(), "navigation keys still browse");

    app.handle_key(press(KeyCode::Char('p'))).unwrap();
    assert!(app.ctx_filtering);
    assert_eq!(app.ctx_filter, "p");
    assert_eq!(app.filtered_contexts(), vec!["prod".to_string()]);
    assert_eq!(app.ctx_state.selected(), Some(0));

    app.handle_key(press(KeyCode::Backspace)).unwrap();
    assert!(app.ctx_filter.is_empty());
    assert_eq!(
        app.filtered_contexts(),
        vec!["dev".to_string(), "prod".to_string(), "test".to_string()]
    );
    assert_eq!(app.ctx_state.selected(), Some(0));

    app.handle_key(press(KeyCode::Char('z'))).unwrap();
    assert!(app.filtered_contexts().is_empty());
    assert_eq!(app.ctx_state.selected(), None);
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    assert_eq!(app.ctx_state.selected(), Some(0));
}

#[tokio::test]
async fn context_picker_enter_switches_to_filtered_selection() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod-east".into(), "prod-west".into(), "test".into()],
    });

    app.handle_key(press(KeyCode::Char('p'))).unwrap();
    assert_eq!(
        app.filtered_contexts(),
        vec!["prod-east".to_string(), "prod-west".to_string()]
    );
    app.handle_key(press(KeyCode::Down)).unwrap();
    assert_eq!(app.ctx_state.selected(), Some(1));
    app.handle_key(press(KeyCode::Enter)).unwrap();

    assert_eq!(app.mode, Mode::Table);
    assert!(app.ctx_filter.is_empty());
    assert!(!app.ctx_filtering);
    assert_eq!(app.flash, "switching to prod-west…");
}

#[tokio::test]
async fn context_picker_esc_while_typing_cancels_filter() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["dev".into(), "test".into()],
    });
    app.handle_key(press(KeyCode::Char('d'))).unwrap();
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(!app.ctx_filtering);
    assert!(app.ctx_filter.is_empty());
    assert_eq!(
        app.mode,
        Mode::Contexts,
        "esc cancels the filter, not the picker"
    );
}

#[tokio::test]
async fn context_rename_prompt_opens_prefilled_and_returns_to_picker() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod".into(), "test".into()],
    });

    app.key_contexts(press(KeyCode::Char('r')));
    assert_eq!(app.mode, Mode::Prompt);
    assert!(app.prompt_over_contexts());
    assert_eq!(app.prompt_input, "test", "prefilled with the selected name");
    assert!(app.prompt_label.contains("Rename context test"));

    // Esc abandons the rename and lands back in the picker.
    app.key_prompt(press(KeyCode::Esc));
    assert_eq!(app.mode, Mode::Contexts);
    assert!(!app.prompt_over_contexts());
}

#[tokio::test]
async fn context_rename_to_existing_name_warns() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod".into(), "test".into()],
    });

    app.key_contexts(press(KeyCode::Char('r')));
    app.prompt_input = "prod".into();
    app.key_prompt(press(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Contexts);
    assert!(app.flash_err);
    assert!(app.flash.contains("already exists"), "{}", app.flash);
}

#[tokio::test]
async fn context_renamed_updates_lists_and_current_context() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Contexts;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod".into(), "test".into()],
    });
    app.all_contexts = vec!["prod".into(), "test".into()];
    app.note_recent_namespace("shop");

    let claim = app.claim_status("renaming test → staging…");
    app.handle_msg(Msg::ContextRenamed {
        generation: app.generation,
        claim,
        old: "test".into(),
        new: "staging".into(),
        result: Ok(()),
    });
    assert_eq!(
        app.ctx_list,
        vec!["prod".to_string(), "staging".to_string()]
    );
    assert_eq!(
        app.all_contexts,
        vec!["prod".to_string(), "staging".to_string()]
    );
    assert_eq!(
        app.cluster.context, "staging",
        "live connection follows the rename"
    );
    assert_eq!(
        app.recent_namespaces.get("staging").map(|dq| dq.len()),
        Some(1),
        "per-context recents follow the rename"
    );
    assert_eq!(
        app.ctx_state.selected(),
        Some(1),
        "cursor lands on the new name"
    );
    assert!(
        app.flash.contains("renamed context test → staging"),
        "{}",
        app.flash
    );

    // A failed rename only flashes.
    let claim = app.claim_status("renaming prod → live…");
    app.handle_msg(Msg::ContextRenamed {
        generation: app.generation,
        claim,
        old: "prod".into(),
        new: "live".into(),
        result: Err("no context exists with the name".into()),
    });
    assert!(app.flash_err);
    assert_eq!(
        app.ctx_list,
        vec!["prod".to_string(), "staging".to_string()]
    );
}

#[tokio::test]
async fn rbac_for_old_generation_is_dropped() {
    let (mut app, _rx) = test_app();
    let stale = app.generation;
    app.bump_generation();

    let mut allowed = HashSet::new();
    allowed.insert("secrets".to_string());
    app.handle_msg(Msg::Rbac {
        generation: stale,
        ns: "default".into(),
        allowed,
    });
    assert!(app.rbac_allowed.is_none());
}

#[test]
fn workload_selector_from_match_labels() {
    let d = obj(json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "web", "namespace": "shop"},
        "spec": {"selector": {"matchLabels": {"app": "web", "tier": "fe"}}}
    }));
    assert_eq!(
        label_selector(&d, "matchLabels").as_deref(),
        Some("app=web,tier=fe")
    );
}

#[test]
fn service_selector_from_plain_map() {
    let s = obj(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": "svc"},
        "spec": {"selector": {"app": "api"}}
    }));
    assert_eq!(label_selector(&s, "selector").as_deref(), Some("app=api"));
}

#[test]
fn no_selector_returns_none() {
    let s = obj(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": "headless"}, "spec": {}
    }));
    assert_eq!(label_selector(&s, "selector"), None);
}

#[test]
fn containers_include_init_and_main() {
    let p = obj(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "p"},
        "spec": {
            "containers": [{"name": "app"}, {"name": "sidecar"}],
            "initContainers": [{"name": "init"}]
        }
    }));
    let names = container_names(&p);
    assert!(names.contains(&"app".to_string()));
    assert!(names.contains(&"sidecar".to_string()));
    assert!(names.contains(&"init".to_string()));
}

#[test]
fn drainable_pod_skips_daemonset_mirror_and_completed_pods() {
    let pod = |v| serde_json::from_value::<Pod>(v).unwrap();
    assert!(drainable_pod(&pod(json!({
        "metadata": {"name": "web", "namespace": "default"},
        "status": {"phase": "Running"}
    }))));
    assert!(!drainable_pod(&pod(json!({
        "metadata": {
            "name": "ds",
            "ownerReferences": [{"kind": "DaemonSet", "name": "agent", "uid": "ds"}]
        },
        "status": {"phase": "Running"}
    }))));
    assert!(!drainable_pod(&pod(json!({
        "metadata": {
            "name": "static",
            "annotations": {"kubernetes.io/config.mirror": "mirror"}
        },
        "status": {"phase": "Running"}
    }))));
    assert!(!drainable_pod(&pod(json!({
        "metadata": {"name": "done"},
        "status": {"phase": "Succeeded"}
    }))));
}

#[test]
fn xray_pool_plurals_include_cronjob_chain() {
    assert_eq!(xray_pool_plurals("cronjob"), &["jobs", "pods"]);
    assert_eq!(xray_pool_plurals("job"), &["pods"]);
    assert_eq!(xray_pool_plurals("pod"), &[] as &[&str]);
    assert_eq!(xray_pool_plurals("deployment"), &["replicasets", "pods"]);
}

#[test]
fn xray_emits_cronjob_job_pod_container_chain() {
    let cron = obj(json!({
        "apiVersion": "batch/v1",
        "kind": "CronJob",
        "metadata": {"name": "backup", "namespace": "default", "uid": "cron-uid"},
        "status": {"active": [{"name": "backup-1"}]}
    }));
    let job = obj(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": "backup-1",
            "namespace": "default",
            "uid": "job-uid",
            "ownerReferences": [{
                "apiVersion": "batch/v1",
                "kind": "CronJob",
                "name": "backup",
                "uid": "cron-uid"
            }]
        },
        "spec": {"completions": 1},
        "status": {"succeeded": 1}
    }));
    let pod = obj(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "backup-1-pod",
            "namespace": "default",
            "uid": "pod-uid",
            "ownerReferences": [{
                "apiVersion": "batch/v1",
                "kind": "Job",
                "name": "backup-1",
                "uid": "job-uid"
            }]
        },
        "spec": {"containers": [{"name": "worker"}]},
        "status": {"phase": "Running"}
    }));
    let mut children = std::collections::HashMap::new();
    children.insert("cron-uid".to_string(), vec![("job".to_string(), job)]);
    children.insert("job-uid".to_string(), vec![("pod".to_string(), pod)]);

    let mut items = Vec::new();
    emit_xray("cronjob", &cron, 0, &children, &mut items);

    assert_eq!(items.len(), 4);
    assert_eq!(items[0].kind, "cronjob");
    assert_eq!(items[0].name, "backup");
    assert_eq!(items[0].depth, 0);
    assert_eq!(items[0].status, "active 1");
    assert_eq!(items[1].kind, "job");
    assert_eq!(items[1].name, "backup-1");
    assert_eq!(items[1].depth, 1);
    assert_eq!(items[1].status, "1/1");
    assert_eq!(items[2].kind, "pod");
    assert_eq!(items[2].name, "backup-1-pod");
    assert_eq!(items[2].depth, 2);
    assert_eq!(items[2].status, "Running");
    assert_eq!(items[3].kind, "container");
    assert_eq!(items[3].name, "backup-1-pod");
    assert_eq!(items[3].depth, 3);
    assert_eq!(items[3].container.as_deref(), Some("worker"));
}

#[test]
fn trim_plural_suffix() {
    assert_eq!(trim_s("deployments"), "deployment");
    assert_eq!(trim_s("pods"), "pod");
}

// ----- `:kind namespace` + view history -----------------------------------

#[tokio::test]
async fn command_with_namespace_switches_both() {
    let (mut app, _rx) = test_app();
    // A cached namespace so the second word completes against something.
    app.ns_list = vec!["<all>".into(), "social".into(), "kube-system".into()];
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "deployments soc".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    // Once the second word begins, suggestions complete the namespace argument
    // (not the resource kind), fuzzy-matched against the cache.
    let first = app.cmd_suggestions.first().expect("namespace suggestion");
    assert_eq!(first.kind, SuggestKind::Namespace);
    assert_eq!(first.label, "social");
    // Enter applies the highlighted namespace completion, switching both.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.namespace, "social");
}

#[tokio::test]
async fn command_with_unlisted_namespace_is_freeform() {
    let (mut app, _rx) = test_app();
    // No cache match → no completion, but the typed namespace still applies
    // verbatim (listing may be RBAC-restricted).
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "deployments social".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.namespace, "social");
}

#[tokio::test]
async fn command_completes_context_argument() {
    let (mut app, _rx) = test_app();
    app.all_contexts = vec!["prod-eu".into(), "staging".into(), "dev".into()];
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "ctx prod".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    let first = app.cmd_suggestions.first().expect("context suggestion");
    assert_eq!(first.kind, SuggestKind::Context);
    assert_eq!(first.label, "prod-eu");
}

#[tokio::test]
async fn command_namespace_all_means_all_namespaces() {
    let (mut app, _rx) = test_app();
    app.namespace = "default".into();
    app.switch_kind_ns("pods", Some("all"));
    assert_eq!(app.kind_plural, "pods");
    assert!(app.all_namespaces());
    app.switch_kind_ns("deployments", Some("*"));
    assert!(app.all_namespaces());
}

#[tokio::test]
async fn view_history_brackets_walk_back_and_forward() {
    let (mut app, _rx) = test_app();
    app.namespace = "default".into();
    app.switch_kind("pods");
    app.switch_kind_ns("deployments", Some("social"));
    app.switch_kind_ns("kustomizations", Some("all"));

    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(
        (app.kind_plural.as_str(), app.namespace.as_str()),
        ("deployments", "social")
    );
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(
        (app.kind_plural.as_str(), app.namespace.as_str()),
        ("pods", "default")
    );
    // At the oldest entry `[` stays put and warns.
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert!(app.flash_err);

    app.handle_key(press(KeyCode::Char(']'))).unwrap();
    app.handle_key(press(KeyCode::Char(']'))).unwrap();
    assert_eq!(
        (app.kind_plural.as_str(), app.namespace.as_str()),
        ("kustomizations", "")
    );
    // At the newest entry `]` stays put and warns.
    app.handle_key(press(KeyCode::Char(']'))).unwrap();
    assert_eq!(app.kind_plural, "kustomizations");
    assert!(app.flash_err);
}

#[tokio::test]
async fn new_switch_after_back_truncates_forward_history() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.switch_kind("deployments");
    app.history_back(); // -> pods
    app.switch_kind("services"); // truncates the deployments tail
    app.history_forward();
    assert_eq!(app.kind_plural, "services", "forward tail must be gone");
    app.history_back();
    assert_eq!(app.kind_plural, "pods");
}

#[tokio::test]
async fn history_dedupes_consecutive_identical_views() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.switch_kind("pods");
    app.history_back();
    assert!(app.flash_err, "one entry recorded — nothing to go back to");
}

#[tokio::test]
async fn namespace_switch_is_recorded_in_history() {
    let (mut app, _rx) = test_app();
    app.namespace = "default".into();
    app.switch_kind("pods");
    app.set_namespace("social".into());
    app.history_back();
    assert_eq!(
        (app.kind_plural.as_str(), app.namespace.as_str()),
        ("pods", "default")
    );
    app.history_forward();
    assert_eq!(
        (app.kind_plural.as_str(), app.namespace.as_str()),
        ("pods", "social")
    );
}

// ----- Helm ---------------------------------------------------------------

#[test]
fn helmrelease_storage_resolves_like_helm_controller() {
    use crate::helm::helmrelease_storage;
    let hr = |spec: serde_json::Value| {
        obj(json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2", "kind": "HelmRelease",
            "metadata": {"name": "podinfo", "namespace": "flux-system"},
            "spec": spec
        }))
    };
    // Defaults: metadata name, object namespace.
    assert_eq!(
        helmrelease_storage(&hr(json!({}))),
        ("podinfo".to_string(), "flux-system".to_string())
    );
    // Explicit releaseName + storageNamespace win.
    assert_eq!(
        helmrelease_storage(&hr(
            json!({"releaseName": "custom", "storageNamespace": "apps"})
        )),
        ("custom".to_string(), "apps".to_string())
    );
    // targetNamespace without releaseName composes `<target>-<name>`.
    assert_eq!(
        helmrelease_storage(&hr(json!({"targetNamespace": "prod"}))),
        ("prod-podinfo".to_string(), "flux-system".to_string())
    );
}

#[tokio::test]
async fn enter_on_helmrelease_opens_helm_history() {
    let (mut app, _rx) = test_app();
    app.switch_kind("helmreleases");
    apply(
        &mut app,
        json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2", "kind": "HelmRelease",
            "metadata": {"name": "podinfo", "namespace": "flux-system"},
            "spec": {"storageNamespace": "apps"}
        }),
    );
    app.table_state.select(Some(0));
    app.drill();
    assert_eq!(app.kind_plural, "helmhistory");
    assert_eq!(app.namespace, "apps");
    assert_eq!(app.labels.as_deref(), Some("owner=helm,name=podinfo"));
    assert_eq!(app.fields.as_deref(), Some("type=helm.sh/release.v1"));
    // The backing kind must be the secrets watch, not the HelmRelease CRD.
    assert_eq!(
        app.kind.as_ref().map(|k| k.ar.plural.as_str()),
        Some("secrets")
    );
    // Esc returns to the HelmRelease list.
    assert!(app.pop_frame());
    assert_eq!(app.kind_plural, "helmreleases");
}

#[tokio::test]
async fn helm_list_shows_only_latest_revision_per_release() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "superseded"),
    );
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );
    // A second, unrelated release must not affect the first's dedup.
    apply(
        &mut app,
        helm_release_secret("other", "default", 1, "deployed"),
    );

    let rows = app.rows();
    assert_eq!(rows.len(), 2, "one row per release, not per revision");
    let myapp_row = rows
        .iter()
        .find(|o| crate::helm::release_name(o) == Some("myapp"))
        .expect("myapp row present");
    assert_eq!(crate::helm::revision(myapp_row), Some(2));

    let (cells, _) = crate::columns::cells(myapp_row, "helm");
    assert_eq!(
        cells[0], "myapp",
        "NAME cell shows the release, not the secret"
    );
    assert_eq!(cells[1], "2");
    assert_eq!(cells[2], "deployed");
    assert_eq!(cells[3], "mychart-1.0.0");
}

/// The latest-revision dedup is cached against the store's mutation counter,
/// so a rebuild staled by a filter or sort reuses it. A new revision arriving
/// *after* the rows were read must still invalidate it.
#[tokio::test]
async fn helm_list_dedup_refreshes_when_a_new_revision_arrives() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "deployed"),
    );
    // Read once so the dedup is cached, then supersede it.
    assert_eq!(app.rows().len(), 1);
    assert_eq!(crate::helm::revision(app.rows()[0]), Some(1));

    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );

    let rows = app.rows();
    assert_eq!(rows.len(), 1, "still one row per release");
    assert_eq!(
        crate::helm::revision(rows[0]),
        Some(2),
        "a cached dedup must not pin the superseded revision"
    );
}

#[tokio::test]
async fn helm_filter_matches_release_name_not_secret_name() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "deployed"),
    );

    app.filter = "myapp".into();
    app.invalidate_rows();
    assert_eq!(app.rows().len(), 1, "filter matches the release name");

    // The raw secret name would never be typed by a user filtering releases.
    app.filter = "sh.helm.release".into();
    app.invalidate_rows();
    assert_eq!(
        app.rows().len(),
        0,
        "filter must not fall back to the ugly secret name"
    );
}

#[tokio::test]
async fn helm_enter_drills_into_release_history() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "helmhistory");
    assert_eq!(app.labels.as_deref(), Some("owner=helm,name=myapp"));
    assert_eq!(app.scope_label.as_deref(), Some("helm/myapp"));
    assert_eq!(app.stack.len(), 1);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "helm");
    assert!(app.stack.is_empty());
}

#[tokio::test]
async fn helm_history_shows_every_revision_and_enter_shows_values() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap(); // -> helmhistory, fresh watch

    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "superseded"),
    );
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );
    assert_eq!(
        app.rows().len(),
        2,
        "history shows every revision, no dedup"
    );

    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.title.contains("values"), "{}", app.detail.title);
    assert!(app.detail.lines.iter().any(|l| l.contains("replicaCount")));
}

#[tokio::test]
async fn helm_describe_shows_notes_and_yaml_key_shows_manifest() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "deployed"),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Char('d'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.title.contains("notes"), "{}", app.detail.title);
    assert!(
        app.detail
            .lines
            .iter()
            .any(|l| l.contains("thanks for installing"))
    );

    app.mode = Mode::Table;
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(
        app.detail.title.contains("manifest"),
        "{}",
        app.detail.title
    );
    assert!(app.detail.lines.iter().any(|l| l.contains("ConfigMap")));
}

#[tokio::test]
async fn helm_ctrl_d_opens_uninstall_confirm_not_generic_delete() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "deployed"),
    );
    app.table_state.select(Some(0));

    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label.contains("Uninstall"),
        "{}",
        app.confirm_label
    );
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::HelmUninstall { .. })
    ));

    // Confirming runs the (off-thread) `helm uninstall` and returns to Table
    // — it must not touch the k8s delete API for the release's own Secret.
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("uninstalling"), "{}", app.flash);
}

#[tokio::test]
async fn helm_r_key_opens_rollback_confirm_with_selected_revision() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap(); // -> helmhistory

    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "superseded"),
    );
    apply(
        &mut app,
        helm_release_secret("myapp", "default", 2, "deployed"),
    );
    let old_idx = app
        .rows()
        .iter()
        .position(|o| crate::helm::revision(o) == Some(1))
        .unwrap();
    app.table_state.select(Some(old_idx));

    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label.contains("revision 1"),
        "{}",
        app.confirm_label
    );
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::HelmRollback { ref revision, .. }) if revision == "1"
    ));
}

#[tokio::test]
async fn helm_base_pins_to_active_context() {
    let (app, _rx) = test_app();
    assert_eq!(app.helm_base(), vec!["helm", "--kube-context", "test"]);
}

#[tokio::test]
async fn helm_sorts_updated_and_revision_by_value_not_text() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    let rel = |name, rev, at| helm_release_secret_deployed_at(name, "default", rev, "deployed", at);
    apply(&mut app, rel("alpha", 4, "2024-03-01T00:00:00Z"));
    apply(&mut app, rel("beta", 61, "2024-01-01T00:00:00Z"));
    apply(&mut app, rel("gamma", 11951, "2024-02-01T00:00:00Z"));
    let headers = app.display_headers();

    // UPDATED sorts by the deploy timestamp (ascending = most recent first,
    // like AGE), never the humanized "5d23h" cell text.
    app.sort_column = headers.iter().position(|h| *h == "UPDATED");
    app.invalidate_rows();
    let names: Vec<&str> = app
        .rows()
        .into_iter()
        .map(|o| crate::helm::release_name(o).unwrap())
        .collect();
    assert_eq!(names, ["alpha", "gamma", "beta"]);

    // REVISION sorts numerically ("11951" would sort before "4" as text).
    app.sort_column = headers.iter().position(|h| *h == "REVISION");
    app.invalidate_rows();
    let revs: Vec<i64> = app
        .rows()
        .into_iter()
        .map(|o| crate::helm::revision(o).unwrap())
        .collect();
    assert_eq!(revs, [4, 61, 11951]);
}

#[tokio::test]
async fn helm_resource_title_names_the_view_not_the_backing_secret() {
    let (mut app, _rx) = test_app();
    app.open_helm_releases();
    // The view is backed by the real `secrets` kind, but neither the header
    // nor the list-panel title may say "secrets" — that's meaningless to
    // someone browsing Helm releases.
    assert_eq!(app.resource_title(), "helm");
    assert_eq!(app.list_title(), "helm");

    apply(
        &mut app,
        helm_release_secret("myapp", "default", 1, "deployed"),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.resource_title(), "helm history");
    assert_eq!(app.list_title(), "helm history");
}

#[tokio::test]
async fn readonly_blocks_mutating_actions() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.readonly = true;

    app.request_delete(false);
    assert_eq!(app.mode, Mode::Table, "delete confirm must not open");
    assert!(app.flash.contains("read-only"));

    app.flash.clear();
    app.request_edit();
    assert!(app.pending.is_none(), "edit must not shell out");
    assert!(app.flash.contains("read-only"));

    app.flash.clear();
    app.request_exec();
    assert!(app.pending.is_none(), "shell must not open");
    assert!(app.flash.contains("read-only"));

    app.plugins = vec![crate::config::Plugin {
        key: "g".into(),
        name: "argocd-sync".into(),
        command: "argocd".into(),
        ..Default::default()
    }];
    app.flash.clear();
    assert!(
        app.try_plugin_key(press(KeyCode::Char('g'))),
        "plugin chord matched"
    );
    assert!(app.pending.is_none(), "plugin must not run");
    assert!(app.flash.contains("read-only"));

    // Read paths stay open: describe still works.
    app.flash.clear();
    app.handle_key(press(KeyCode::Char('d'))).unwrap();
    assert!(!app.flash.contains("read-only"));
}

#[tokio::test]
async fn modifier_plugin_chord_does_not_trigger_plain_key_builtin() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["a", "b", "c"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    app.plugins = vec![crate::config::Plugin {
        key: "ctrl-g".into(),
        name: "gen".into(),
        command: "true".into(),
        ..Default::default()
    }];
    app.table_state.select(Some(2));

    // ctrl-g runs the plugin (write mode) — and must NOT fire the built-in `g`
    // (go to top), which would move the cursor to row 0.
    let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    app.handle_key(ctrl_g).unwrap();
    assert_eq!(
        app.table_state.selected(),
        Some(2),
        "ctrl-g must not jump to top"
    );
    assert!(
        matches!(app.pending, Some(Suspend::Shell(_))),
        "ctrl-g should run the plugin"
    );
    app.pending = None;

    // Plain `g` still triggers the built-in go-to-top.
    app.handle_key(press(KeyCode::Char('g'))).unwrap();
    assert_eq!(app.table_state.selected(), Some(0), "plain g goes to top");
    assert!(app.pending.is_none(), "plain g is not the plugin");
}

/// Set up a one-pod table with the cursor on it, for plugin dispatch tests.
fn app_with_pod() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
               "metadata": {"name": "a", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    (app, rx)
}

/// A pod carrying Flux kustomize toolkit labels (so it reads as managed).
fn flux_managed_pod(app: &mut App) {
    app.switch_kind("pods");
    apply(
        app,
        json!({"apiVersion":"v1","kind":"Pod","metadata":{
            "name":"a","namespace":"default","labels":{
                "kustomize.toolkit.fluxcd.io/name":"apps",
                "kustomize.toolkit.fluxcd.io/namespace":"flux-system"}}}),
    );
    app.table_state.select(Some(0));
}

#[tokio::test]
async fn editing_flux_managed_object_confirms_with_revert_warning() {
    let (mut app, _rx) = test_app();
    flux_managed_pod(&mut app);
    app.request_edit();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(app.pending.is_none(), "must not edit before confirming");
    assert!(
        app.confirm_label.contains("Managed by Flux") && app.confirm_label.contains("reverted"),
        "{}",
        app.confirm_label
    );
    // Confirming opens the editor.
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert!(matches!(app.pending, Some(Suspend::Shell(_))));
}

#[tokio::test]
async fn editing_unmanaged_object_skips_the_warning() {
    let (mut app, _rx) = app_with_pod(); // plain pod, no toolkit labels
    app.request_edit();
    assert_ne!(app.mode, Mode::Confirm, "unmanaged edit needs no confirm");
    assert!(matches!(app.pending, Some(Suspend::Shell(_))));
}

#[tokio::test]
async fn mutating_action_is_recorded_in_the_journal() {
    let (mut app, _rx) = app_with_pod();
    assert!(app.journal.is_empty(), "journal starts empty");
    app.request_edit(); // unmanaged edit records straight away
    assert_eq!(app.journal.len(), 1);
    // The palette command opens it as a scrollable document.
    app.open_journal();
    assert_eq!(app.mode, Mode::Detail);
    let body = app
        .detail
        .lines
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("edit"), "{body}");
    assert!(
        body.contains(&app.cluster.context),
        "context column: {body}"
    );
}

#[tokio::test]
async fn deleting_managed_object_warns_it_will_be_recreated() {
    let (mut app, _rx) = test_app();
    flux_managed_pod(&mut app);
    app.request_delete(false);
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label.contains("managed by Flux") && app.confirm_label.contains("recreated"),
        "{}",
        app.confirm_label
    );
}

#[tokio::test]
async fn guardrail_denies_an_action() {
    let (mut app, _rx) = app_with_pod();
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["delete".into()],
        deny: true,
        reason: Some("prod is locked".into()),
        ..Default::default()
    }];
    app.request_delete(false);
    assert_ne!(app.mode, Mode::Confirm);
    assert!(app.confirm_action.is_none(), "denied — nothing to confirm");
    assert!(
        app.flash.contains("blocked by guardrail") && app.flash.contains("prod is locked"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn guardrail_caps_bulk_delete() {
    let (mut app, _rx) = app_with_two_marked_pods();
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["delete".into()],
        max_bulk: Some(1),
        ..Default::default()
    }];
    app.request_delete(false);
    assert_ne!(app.mode, Mode::Confirm);
    assert!(app.flash.contains("exceeds the max"), "{}", app.flash);
}

#[tokio::test]
async fn guardrail_type_resource_name_confirmation() {
    let (mut app, _rx) = app_with_pod(); // single pod "a"
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["delete".into()],
        confirmation: Some("type-resource-name".into()),
        ..Default::default()
    }];
    app.request_delete(false);
    assert_eq!(app.mode, Mode::Prompt, "typed confirmation opens a prompt");

    // A wrong name cancels without deleting.
    app.prompt_input = "wrong".into();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("did not match"), "{}", app.flash);

    // The exact name proceeds.
    app.request_delete(false);
    app.prompt_input = "a".into();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(app.flash.contains("deleting"), "{}", app.flash);
}

#[tokio::test]
async fn debug_prompt_prefills_image_then_shells_out_to_kubectl_debug() {
    let (mut app, _rx) = app_with_pod();
    app.debug = crate::config::DebugConfig {
        image: "nicolaka/netshoot:latest".into(),
        command: Vec::new(),
        ..Default::default()
    };
    app.request_debug(None);
    assert_eq!(app.mode, Mode::Prompt);
    // The prompt is prefilled with the configured default image.
    assert_eq!(app.prompt_input, "nicolaka/netshoot:latest");
    assert!(matches!(app.prompt_kind, Some(PromptKind::Debug { .. })));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    let Some(Suspend::Shell(argv)) = app.pending.take() else {
        panic!("debug should suspend into a kubectl debug shell");
    };
    assert!(argv.iter().any(|a| a == "debug"));
    assert!(argv.iter().any(|a| a == "-it"));
    assert!(
        argv.iter().any(|a| a == "--image=nicolaka/netshoot:latest"),
        "{argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a.starts_with("--target")),
        "no target when launched from the pod row: {argv:?}"
    );
    // Recorded in the journal as a debug action.
    assert!(app.journal.lines().iter().any(|l| l.contains("debug")));
}

#[tokio::test]
async fn debug_from_container_picker_pins_target() {
    let (mut app, _rx) = app_with_pod();
    app.do_debug(
        "default".into(),
        "a".into(),
        Some("app".into()),
        "busybox:latest".into(),
    );
    let Some(Suspend::Shell(argv)) = app.pending.take() else {
        panic!("expected a debug shell");
    };
    assert!(argv.iter().any(|a| a == "--target=app"), "{argv:?}");
}

/// A one-node table with the cursor on it.
fn app_with_node() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("nodes");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "node-a"}}),
    );
    app.table_state.select(Some(0));
    (app, rx)
}

#[tokio::test]
async fn node_debug_previews_host_access_then_launches_debug_pod() {
    let (mut app, _rx) = app_with_node();
    app.debug = crate::config::DebugConfig {
        node_image: "busybox:latest".into(),
        node_namespace: "kube-system".into(),
        node_profile: Some("sysadmin".into()),
        ..Default::default()
    };
    // `:debug` on a node previews the host access and requires confirmation.
    app.request_debug(None);
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label.contains("/host") && app.confirm_label.contains("host PID"),
        "{}",
        app.confirm_label
    );
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::NodeDebug { .. })
    ));

    // Confirming launches kubectl debug node/… and tracks it for cleanup.
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    let Some(Suspend::Shell(argv)) = app.pending.take() else {
        panic!("node debug should suspend into a shell");
    };
    assert!(argv.iter().any(|a| a == "debug"));
    assert!(argv.iter().any(|a| a == "node/node-a"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--image=busybox:latest"));
    assert!(argv.iter().any(|a| a == "--profile=sysadmin"));
    assert!(argv.iter().any(|a| a == "kube-system"));
    assert_eq!(
        app.launched_node_debuggers,
        vec![("kube-system".into(), "node-a".into())]
    );
    assert!(
        app.journal.lines().iter().any(|l| l.contains("node-debug")),
        "recorded in journal"
    );
}

#[tokio::test]
async fn node_debug_is_gated_by_readonly_and_guardrail() {
    let (mut app, _rx) = app_with_node();
    app.readonly = true;
    app.request_debug(None);
    assert_ne!(app.mode, Mode::Confirm, "read-only blocks node debug");

    let (mut app, _rx) = app_with_node();
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["node-debug".into()],
        deny: true,
        ..Default::default()
    }];
    app.request_debug(None);
    assert_ne!(app.mode, Mode::Confirm);
    assert!(app.flash.contains("blocked by guardrail"), "{}", app.flash);
}

#[tokio::test]
async fn debug_clean_without_launches_warns() {
    let (mut app, _rx) = app_with_node();
    app.request_debug_cleanup();
    assert_ne!(app.mode, Mode::Confirm);
    assert!(app.flash.contains("no node debuggers"), "{}", app.flash);
}

#[tokio::test]
async fn debug_clean_confirms_when_debuggers_were_launched() {
    let (mut app, _rx) = app_with_node();
    app.launched_node_debuggers = vec![("default".into(), "node-a".into())];
    app.request_debug_cleanup();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(
        app.confirm_label.contains("node-a"),
        "{}",
        app.confirm_label
    );
    assert!(matches!(
        app.confirm_action,
        Some(ConfirmAction::CleanupDebuggers)
    ));
}

#[tokio::test]
async fn debug_is_blocked_in_readonly_and_by_guardrail() {
    // Read-only mode.
    let (mut app, _rx) = app_with_pod();
    app.readonly = true;
    app.request_debug(None);
    assert_ne!(app.mode, Mode::Prompt, "read-only blocks debug");
    assert!(app.flash.contains("read-only"));

    // A guardrail denying the `debug` action.
    let (mut app, _rx) = app_with_pod();
    app.guardrails = vec![crate::config::Guardrail {
        actions: vec!["debug".into()],
        deny: true,
        reason: Some("no debug on prod".into()),
        ..Default::default()
    }];
    app.request_debug(None);
    assert_ne!(app.mode, Mode::Prompt);
    assert!(app.flash.contains("blocked by guardrail"), "{}", app.flash);
}

#[tokio::test]
async fn readonly_gates_mutating_plugins_only() {
    let (mut app, _rx) = app_with_pod();
    app.readonly = true;

    // The default (mutating) plugin is blocked in read-only mode.
    app.plugins = vec![crate::config::Plugin {
        key: "g".into(),
        name: "mut".into(),
        command: "true".into(),
        ..Default::default()
    }];
    assert!(app.try_plugin_key(press(KeyCode::Char('g'))));
    assert!(app.pending.is_none(), "mutating plugin blocked");
    assert!(app.flash.contains("read-only"));

    // An explicitly read-only plugin runs even with --readonly.
    app.plugins = vec![crate::config::Plugin {
        key: "h".into(),
        name: "ro".into(),
        command: "true".into(),
        mutating: Some(false),
        ..Default::default()
    }];
    app.flash.clear();
    assert!(app.try_plugin_key(press(KeyCode::Char('h'))));
    assert!(
        matches!(app.pending, Some(Suspend::Shell(_))),
        "read-only plugin runs"
    );
}

#[tokio::test]
async fn dangerous_plugin_confirms_before_running() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![crate::config::Plugin {
        key: "g".into(),
        name: "danger".into(),
        command: "true".into(),
        dangerous: true,
        ..Default::default()
    }];
    app.try_plugin_key(press(KeyCode::Char('g')));
    assert_eq!(app.mode, Mode::Confirm);
    assert!(app.pending.is_none(), "must not run before confirmation");
    assert!(app.confirm_label.contains("danger") && app.confirm_label.contains('⚠'));

    // Accepting runs it.
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(
        matches!(app.pending, Some(Suspend::Shell(_))),
        "runs after y"
    );
}

#[tokio::test]
async fn plugin_placeholders_substitute_as_separate_args() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![crate::config::Plugin {
        key: "g".into(),
        name: "p".into(),
        command: "echo".into(),
        args: vec!["$RESOURCE".into(), "$NAMESPACE".into(), "$NAME".into()],
        ..Default::default()
    }];
    app.try_plugin_key(press(KeyCode::Char('g')));
    let Some(Suspend::Shell(argv)) = &app.pending else {
        panic!("plugin did not run");
    };
    // Each placeholder is one whole argv entry (boundaries preserved).
    assert_eq!(
        argv,
        &vec![
            "echo".to_string(),
            "pods".into(),
            "default".into(),
            "a".into()
        ]
    );
}

/// Two marked pods, cursor on the first.
fn app_with_two_marked_pods() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.switch_kind("pods");
    for n in ["a", "b"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    let keys: Vec<String> = app.rows().iter().map(|o| row_key(o)).collect();
    for k in keys {
        app.marked.insert(k);
    }
    app.table_state.select(Some(0));
    (app, rx)
}

#[tokio::test]
async fn bulk_terminal_plugin_is_refused() {
    let (mut app, _rx) = app_with_two_marked_pods();
    app.plugins = vec![crate::config::Plugin {
        key: "g".into(),
        name: "t".into(),
        command: "echo".into(),
        args: vec!["$NAME".into()],
        ..Default::default() // terminal output — can't run over a marked set
    }];
    app.try_plugin_key(press(KeyCode::Char('g')));
    assert!(
        app.flash.contains("marked set") && app.flash_err,
        "flash: {}",
        app.flash
    );
    assert!(app.pending.is_none(), "terminal bulk must not run");
}

#[tokio::test]
async fn bookmark_applies_resource_namespace_filter_and_sort() {
    let (mut app, _rx) = test_app();
    app.bookmarks = vec![crate::config::Bookmark {
        name: "api fails".into(),
        resource: "pods".into(),
        namespace: Some("prod".into()),
        filter: Some("status!=Running".into()),
        sort: Some("NAME:desc".into()),
        ..Default::default()
    }];
    assert!(app.apply_bookmark_named("api fails"));
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.namespace, "prod");
    assert_eq!(app.filter, "status!=Running");
    // NAME is the first column; sort landed on it, descending.
    assert_eq!(app.sort_column, Some(0));
    assert!(app.sort_desc);
    assert!(app.flash.contains("api fails"));

    // Unknown bookmark name is a no-op.
    assert!(!app.apply_bookmark_named("nope"));
}

#[tokio::test]
async fn namespace_switcher_pins_favorites_then_recents() {
    let (mut app, _rx) = test_app();
    app.ns_list = ["<all>", "alpha", "beta", "checkout", "monitoring"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    app.namespace_favorites = vec!["monitoring".into()];
    // Recents, oldest→newest (newest ends up first).
    app.note_recent_namespace("checkout");
    app.note_recent_namespace("alpha");

    // Browsing: <all>, favourite, recents (newest first), then the rest.
    assert_eq!(
        app.filtered_namespaces(),
        vec!["<all>", "monitoring", "alpha", "checkout", "beta"]
    );
    assert!(app.is_favorite_namespace("monitoring"));
    assert!(app.is_recent_namespace("alpha") && !app.is_recent_namespace("beta"));

    // A filter falls back to pure fuzzy ranking (no pinning).
    app.ns_filter = "beta".into();
    assert_eq!(app.filtered_namespaces(), vec!["<all>", "beta"]);
}

#[tokio::test]
async fn last_picked_namespace_is_remembered_per_context() {
    let (mut app, _rx) = test_app();
    let dir = std::env::temp_dir().join(format!("sofka-nsmem-app-{}", std::process::id()));
    let path = dir.join("namespaces.toml");
    app.namespace_memory_path = Some(path.clone());
    app.switch_kind("pods");

    // Every explicit pick lands in memory and on disk.
    app.set_namespace("payments".into());
    assert_eq!(app.namespace_memory.get("test"), Some("payments".into()));
    assert_eq!(
        crate::nsmem::NamespaceMemory::load(&path).get("test"),
        Some("payments".into())
    );
    app.switch_kind_ns("deployments", Some("checkout"));
    assert_eq!(app.namespace_memory.get("test"), Some("checkout".into()));
    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert_eq!(app.namespace_memory.get("test"), Some(String::new()));
    assert_eq!(
        crate::nsmem::NamespaceMemory::load(&path).get("test"),
        Some(String::new())
    );

    // Drill-downs and history don't count as picks.
    app.set_namespace("payments".into());
    app.push_frame();
    app.namespace = "kube-system".into();
    app.pop_frame();
    assert_eq!(app.namespace_memory.get("test"), Some("payments".into()));

    // Other contexts are untouched.
    assert_eq!(app.namespace_memory.get("other"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn recent_namespaces_dedupe_and_bound() {
    let (mut app, _rx) = test_app();
    for n in ["a", "b", "c", "a"] {
        app.note_recent_namespace(n);
    }
    // `a` moved to the front, no duplicate.
    let recents: Vec<String> = app
        .recent_namespaces
        .get(&app.cluster.context)
        .unwrap()
        .iter()
        .cloned()
        .collect();
    assert_eq!(recents, vec!["a", "c", "b"]);

    // Bounded to 8, newest kept.
    for i in 0..12 {
        app.note_recent_namespace(&format!("ns{i}"));
    }
    let dq = app.recent_namespaces.get(&app.cluster.context).unwrap();
    assert_eq!(dq.len(), 8);
    assert_eq!(dq.front().unwrap(), "ns11");
    // `<all>` and empty are never recorded.
    app.note_recent_namespace("<all>");
    app.note_recent_namespace("");
    assert_eq!(
        app.recent_namespaces
            .get(&app.cluster.context)
            .unwrap()
            .len(),
        8
    );
}

#[tokio::test]
async fn workspace_opens_first_view_and_tab_cycles() {
    let (mut app, _rx) = test_app();
    app.workspaces = vec![crate::config::Workspace {
        key: Some("ctrl-w".into()),
        name: "ops".into(),
        context: None,
        views: vec![
            crate::config::WorkspaceView {
                name: "pods".into(),
                resource: "pods".into(),
                namespace: Some("checkout".into()),
                ..Default::default()
            },
            crate::config::WorkspaceView {
                name: "deploys".into(),
                resource: "deployments".into(),
                ..Default::default()
            },
        ],
    }];
    assert!(app.open_workspace_named("ops"));
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.namespace, "checkout");
    assert!(app.flash.contains("[1/2]"));

    // Tab advances to the second view; wraps back on the third press.
    app.handle_key(press(KeyCode::Tab)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert!(app.flash.contains("[2/2]"));
    app.handle_key(press(KeyCode::Tab)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert!(app.flash.contains("[1/2]"));

    // Shift-Tab goes back.
    app.handle_key(press(KeyCode::BackTab)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
}

#[tokio::test]
async fn tab_is_a_noop_without_an_active_workspace() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    assert!(!app.cycle_workspace(true));
    assert_eq!(app.kind_plural, "pods");
}

#[tokio::test]
async fn bookmark_key_chord_triggers_it() {
    let (mut app, _rx) = test_app();
    app.bookmarks = vec![crate::config::Bookmark {
        key: Some("z".into()),
        name: "go pods".into(),
        resource: "pods".into(),
        ..Default::default()
    }];
    assert!(app.try_bookmark_key(press(KeyCode::Char('z'))));
    assert_eq!(app.kind_plural, "pods");
    // A key with no bookmark bound is not claimed.
    assert!(!app.try_bookmark_key(press(KeyCode::Char('q'))));
}

#[tokio::test]
async fn bookmarks_appear_in_command_palette() {
    let (mut app, _rx) = test_app();
    app.bookmarks = vec![crate::config::Bookmark {
        name: "Prod API".into(),
        resource: "pods".into(),
        ..Default::default()
    }];
    app.command = "prod".into();
    app.update_suggestions();
    assert!(
        app.cmd_suggestions
            .iter()
            .any(|s| s.kind == SuggestKind::Bookmark && s.label == "Prod API"),
        "bookmark missing from palette suggestions"
    );
}

#[tokio::test]
async fn bulk_background_plugin_runs_over_all_marked() {
    let (mut app, _rx) = app_with_two_marked_pods();
    app.plugins = vec![crate::config::Plugin {
        key: "g".into(),
        name: "bg".into(),
        command: "true".into(),
        output: Some("background".into()),
        ..Default::default()
    }];
    app.try_plugin_key(press(KeyCode::Char('g')));
    // Bulk dispatch over the two marked rows; background never suspends.
    assert!(app.flash.contains("×2"), "flash: {}", app.flash);
    assert!(app.pending.is_none(), "background must not suspend the TUI");
}

#[tokio::test]
async fn context_switch_resolves_readonly_and_cli_pin_wins() {
    let dir = std::env::temp_dir().join(format!("sofka-readonly-test-{}", std::process::id()));
    let cluster_dir = dir.join("clusters").join("test-cluster");
    std::fs::create_dir_all(&cluster_dir).unwrap();
    std::fs::write(cluster_dir.join("config.toml"), "readonly = true\n").unwrap();

    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));

    // No CLI pin: the per-cluster override flips read-only on.
    app.apply_context_switch("prod".into(), Box::new(Cluster::fake()));
    assert!(app.readonly);

    // A `--write` pin survives switching into the read-only cluster.
    app.readonly_override = Some(false);
    app.apply_context_switch("prod-again".into(), Box::new(Cluster::fake()));
    assert!(!app.readonly);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn disconnected_start_opens_context_picker() {
    let (tx, _rx) = mpsc::channel(1024);
    let mut cluster = Cluster::fake();
    cluster.connected = false;
    let mut app = App::new(cluster, tx);

    app.start_disconnected("tcp connect error: Connection refused");
    assert_eq!(app.mode, Mode::Contexts);
    assert!(app.flash_err);
    assert!(
        app.flash.contains("cannot connect to 'test'"),
        "{}",
        app.flash
    );
    assert!(app.flash.contains("Connection refused"), "{}", app.flash);
}

#[tokio::test]
async fn reselecting_never_connected_context_retries() {
    let (mut app, _rx) = test_app();

    // Connected: picking the current context again is a no-op.
    app.flash.clear();
    app.switch_context("test".into());
    assert!(app.flash.is_empty());

    // Never connected: picking the same context retries the connection.
    app.cluster.connected = false;
    app.switch_context("test".into());
    assert!(app.flash.contains("switching to test"), "{}", app.flash);
}

#[tokio::test]
async fn failed_switch_while_disconnected_reopens_picker() {
    let (mut app, _rx) = test_app();
    app.cluster.connected = false;
    app.mode = Mode::Table;

    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: "prod".into(),
        result: Err("connection refused".into()),
    });
    assert!(app.flash.contains("context switch failed"), "{}", app.flash);
    assert_eq!(app.mode, Mode::Contexts, "picker must come back up");

    // Once connected, a failed switch just flashes — no picker takeover.
    app.cluster.connected = true;
    app.mode = Mode::Table;
    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: "prod".into(),
        result: Err("connection refused".into()),
    });
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn doc_search_highlights_without_filtering() {
    let (mut app, _rx) = test_app();
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    let total = app.detail.lines.len();

    // `/` opens the search prompt for the detail view; typing builds the query.
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    assert_eq!(app.mode, Mode::DocFilter);
    assert_eq!(app.doc_filter_return, Mode::Detail);
    for c in "kind".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    assert_eq!(app.detail.filter, "kind");

    // Enter keeps the query, returns to the view, and jumps to the first match
    // — the full document stays rendered (no lines removed).
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert_eq!(app.detail.filter, "kind");
    assert_eq!(
        app.detail.lines.len(),
        total,
        "search must not filter lines"
    );
    let matches = app.detail.match_lines();
    assert_eq!(matches.len(), 1, "one `kind:` line");
    assert_eq!(app.detail.scroll, matches[0], "jumped to the match");

    // First esc clears the search (stays), second esc leaves the view.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.filter.is_empty());
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn doc_search_n_and_capital_n_step_between_matches() {
    let (mut app, _rx) = test_app();
    app.detail = Scrollable {
        title: "x — YAML".into(),
        // Matches on lines 1, 3, 5.
        lines: vec![
            "alpha".into(),
            "needle one".into(),
            "beta".into(),
            "needle two".into(),
            "gamma".into(),
            "needle three".into(),
        ]
        .into(),
        ..Default::default()
    };
    app.mode = Mode::Detail;

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "needle".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    // Finalized on the first match.
    assert_eq!(app.detail.scroll, 1);
    assert_eq!(app.detail.match_idx, 0);

    // `n` walks forward, then wraps back to the first.
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.detail.scroll, 3);
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.detail.scroll, 5);
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert_eq!(app.detail.scroll, 1, "wrapped to the first match");

    // `N` walks backward (wrapping to the last).
    app.handle_key(press(KeyCode::Char('N'))).unwrap();
    assert_eq!(app.detail.scroll, 5);
}

#[tokio::test]
async fn doc_search_esc_in_prompt_clears_query() {
    let (mut app, _rx) = test_app();
    app.detail = Scrollable {
        title: "x — YAML".into(),
        lines: (0..100).map(|i| format!("line {i}")).collect(),
        scroll: 50,
        ..Default::default()
    };
    app.mode = Mode::Detail;

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    app.handle_key(press(KeyCode::Char('9'))).unwrap();
    // Typing does not move the document — the search highlights in place.
    assert_eq!(app.detail.scroll, 50);
    // Esc in the prompt aborts the search entirely.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.filter.is_empty());
}

#[tokio::test]
async fn help_search_uses_own_buffer() {
    let (mut app, _rx) = test_app();
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.mode, Mode::Help);

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    assert_eq!(app.mode, Mode::DocFilter);
    assert_eq!(app.doc_filter_return, Mode::Help);
    for c in "logs".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Help);
    assert_eq!(app.help_filter, "logs");
    assert!(
        app.detail.filter.is_empty(),
        "help search must not touch detail"
    );

    // Esc clears the search first, then closes help; reopening starts clean.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Help);
    assert!(app.help_filter.is_empty());
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    app.help_filter = "stale".into();
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert!(app.help_filter.is_empty());
}

#[tokio::test]
async fn copy_doc_copies_the_whole_document() {
    let (mut app, _rx) = test_app();
    app.detail = Scrollable {
        title: "web — YAML".into(),
        lines: vec![
            "apiVersion: v1".to_string(),
            "kind: Pod".to_string(),
            "metadata:".to_string(),
            "  name: web".to_string(),
        ]
        .into(),
        ..Default::default()
    };

    let whole = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web";
    assert_eq!(app.doc_text(), whole);

    // An active search highlights in place; it does not filter, so copy still
    // returns the whole document.
    app.detail.filter = "KIND".into();
    assert_eq!(app.doc_text(), whole);
}

#[tokio::test]
async fn copy_doc_on_empty_view_warns() {
    let (mut app, _rx) = test_app();
    app.detail = Scrollable {
        title: "empty".into(),
        ..Default::default()
    };
    app.mode = Mode::Detail;
    app.handle_key(press(KeyCode::Char('c'))).unwrap();
    assert!(app.flash_err);
    assert!(app.flash.contains("nothing to copy"));
    assert_eq!(app.mode, Mode::Detail, "copy must not leave the view");
}

#[tokio::test]
async fn x_decodes_secret_data_into_detail_view() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let (mut app, _rx) = test_app();
    app.switch_kind("secrets");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": "creds", "namespace": "default"},
        "type": "Opaque",
        "data": {
            "password": BASE64.encode("hunter2"),
            "config.yaml": BASE64.encode("a: 1\nb: 2\n"),
            "cert.der": BASE64.encode([0u8, 159, 146, 150]),
        }}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('x'))).unwrap();

    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.title.contains("decoded"), "{}", app.detail.title);
    let lines: Vec<&str> = app.detail.lines.iter().map(String::as_str).collect();
    assert!(lines.contains(&"password: hunter2"), "{lines:?}");
    // Multiline values render as a stringData-style literal block.
    let block_start = lines
        .iter()
        .position(|l| *l == "config.yaml: |")
        .expect("literal block header");
    assert_eq!(lines[block_start + 1], "  a: 1");
    assert_eq!(lines[block_start + 2], "  b: 2");
    // Binary values get a placeholder, not mojibake.
    assert!(lines.contains(&"cert.der: <binary: 4 bytes>"), "{lines:?}");

    // Esc returns to the table on the same row.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn x_decodes_secret_from_inside_the_detail_view() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let (mut app, _rx) = test_app();
    app.switch_kind("secrets");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": "creds", "namespace": "default"},
        "type": "Opaque",
        "data": {"password": BASE64.encode("hunter2")}}),
    );
    app.table_state.select(Some(0));

    // Open the YAML view (stands in for describe — same Mode::Detail), then
    // decode from inside it without backing out to the table.
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.title.contains("YAML"), "{}", app.detail.title);
    app.handle_key(press(KeyCode::Char('x'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.title.contains("decoded"), "{}", app.detail.title);
    let lines: Vec<&str> = app.detail.lines.iter().map(String::as_str).collect();
    assert!(lines.contains(&"password: hunter2"), "{lines:?}");

    // Esc still returns to where the original view was opened from.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn x_on_secret_without_data_warns() {
    let (mut app, _rx) = test_app();
    app.switch_kind("secrets");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": "empty", "namespace": "default"},
            "type": "Opaque"}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('x'))).unwrap();
    assert_eq!(app.mode, Mode::Table, "no data — stay on the table");
    assert!(app.flash.contains("no data"));
}

#[tokio::test]
async fn x_outside_secrets_is_left_to_plugins() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('x'))).unwrap();
    assert_eq!(
        app.mode,
        Mode::Table,
        "x must not open a decode view for pods"
    );
}

// ----- custom views, printer columns, wide mode ---------------------------

fn install_views(app: &mut App, toml_text: &str) {
    let cfg: crate::config::Config = toml::from_str(toml_text).unwrap();
    let (views, warnings) = crate::views::compile(&cfg.views);
    assert!(warnings.is_empty(), "{warnings:?}");
    app.user_views = views;
}

fn certificate(name: &str, ready: &str, not_after: &str, cpu: &str) -> serde_json::Value {
    json!({
        "apiVersion": "cert-manager.io/v1",
        "kind": "Certificate",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"cpu": cpu},
        "status": {
            "conditions": [{"type": "Ready", "status": ready}],
            "notAfter": not_after
        }
    })
}

#[tokio::test]
async fn user_view_overlays_columns_and_applies_initial_sort() {
    let (mut app, _rx) = test_app();
    install_views(
        &mut app,
        r#"
        [views."cert-manager.io/v1/certificates"]
        sort = "EXPIRES:desc"

        [[views."cert-manager.io/v1/certificates".columns]]
        name = "READY"
        path = "/status/conditions/0/status"
        type = "status"

        [[views."cert-manager.io/v1/certificates".columns]]
        name = "EXPIRES"
        path = "/status/notAfter"
        type = "time"
        "#,
    );
    app.switch_kind("certificates");

    // Overlay: custom columns slot in before the trailing AGE.
    assert_eq!(app.display_headers(), ["NAME", "READY", "EXPIRES", "AGE"]);
    // The configured initial sort is active (EXPIRES, descending).
    assert_eq!(app.sort_column, Some(2));
    assert!(app.sort_desc);

    apply(
        &mut app,
        certificate("old", "True", "2020-01-01T00:00:00Z", "1"),
    );
    apply(
        &mut app,
        certificate("new", "False", "2099-01-01T00:00:00Z", "1"),
    );

    // Descending time = oldest (largest elapsed) first.
    let rows = app.rows();
    assert_eq!(rows[0].metadata.name.as_deref(), Some("old"));

    // Cells come from the JSON Pointers; the status column drives coloring.
    let rows = app.rows();
    app.ensure_table_cell_cache(&rows);
    let key = row_key(rows[1]);
    let cache = app.table_cell_cache();
    let (cells, status_idx) = cache.get(&key).unwrap();
    assert_eq!(cells[0], "new");
    assert_eq!(cells[1], "False");
    // Humanized future timestamp ("in 27000d"-ish, drifting with wall time).
    assert!(cells[2].starts_with("in "), "{}", cells[2]);
    assert_eq!(status_idx, Some(1));
}

#[tokio::test]
async fn user_view_adds_provider_label_columns_to_curated_nodes() {
    let (mut app, _rx) = test_app();
    install_views(
        &mut app,
        r#"
        [[views."v1/nodes".columns]]
        name = "NODEPOOL"
        path = "/metadata/labels/karpenter.sh~1nodepool"

        [[views."v1/nodes".columns]]
        name = "ZONE"
        path = "/metadata/labels/topology.kubernetes.io~1zone"

        [[views."v1/nodes".columns]]
        name = "INSTANCE"
        path = "/metadata/labels/node.kubernetes.io~1instance-type"

        [[views."v1/nodes".columns]]
        name = "TYPE"
        path = "/metadata/labels/karpenter.sh~1capacity-type"
        "#,
    );
    app.switch_kind("nodes");

    assert_eq!(
        app.display_headers(),
        [
            "NAME", "STATUS", "ROLES", "TAINTS", "VERSION", "NODEPOOL", "ZONE", "INSTANCE", "TYPE",
            "AGE", "PODS", "CPU", "MEM", "%CPU", "%MEM"
        ]
    );

    apply(
        &mut app,
        json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {
                "name": "worker-1",
                "labels": {
                    "karpenter.sh/nodepool": "general",
                    "topology.kubernetes.io/zone": "eu-west-1a",
                    "node.kubernetes.io/instance-type": "m7i.large",
                    "karpenter.sh/capacity-type": "spot"
                }
            },
            "status": {
                "nodeInfo": {"kubeletVersion": "v1.33.4"}
            }
        }),
    );

    let rows = app.rows();
    app.ensure_table_cell_cache(&rows);
    let key = row_key(rows[0]);
    let cache = app.table_cell_cache();
    let (cells, _) = cache.get(&key).unwrap();
    assert_eq!(cells[4], "v1.33.4");
    assert_eq!(&cells[5..9], ["general", "eu-west-1a", "m7i.large", "spot"]);
}

#[tokio::test]
async fn user_view_replace_swaps_out_curated_columns() {
    let (mut app, _rx) = test_app();
    install_views(
        &mut app,
        r#"
        [views.certificates]
        replace = true

        [[views.certificates.columns]]
        name = "NAME"
        path = "/metadata/name"

        [[views.certificates.columns]]
        name = "CPU"
        path = "/spec/cpu"
        type = "quantity"
        "#,
    );
    app.switch_kind("certificates");
    assert_eq!(app.display_headers(), ["NAME", "CPU"]);

    // Quantities sort by value: 500m < 2 despite "2" < "500m" lexically.
    apply(&mut app, certificate("big", "True", "", "2"));
    apply(&mut app, certificate("small", "True", "", "500m"));
    app.sort_column = Some(1);
    app.invalidate_rows();
    let rows = app.rows();
    assert_eq!(rows[0].metadata.name.as_deref(), Some("small"));
    assert_eq!(rows[1].metadata.name.as_deref(), Some("big"));
}

#[tokio::test]
async fn printer_columns_msg_upgrades_name_age_fallback() {
    let (mut app, _rx) = test_app();
    app.switch_kind("certificates");
    assert_eq!(app.display_headers(), ["NAME", "AGE"]);

    let crd = json!({
        "spec": {
            "versions": [{
                "name": "v1", "served": true, "storage": true,
                "additionalPrinterColumns": [
                    {"name": "Ready", "type": "string", "jsonPath": ".status.ready"},
                    {"name": "Detail", "type": "string", "priority": 1,
                     "jsonPath": ".status.message"}
                ]
            }]
        }
    });
    let view = crate::views::printer_columns_view(&crd, "v1");
    app.handle_msg(Msg::PrinterColumns {
        generation: app.generation,
        plural: "certificates".into(),
        view: Box::new(view),
    });
    // Narrow mode hides the priority>0 column; wide shows it.
    assert_eq!(app.display_headers(), ["NAME", "READY", "AGE"]);
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_eq!(app.display_headers(), ["NAME", "READY", "DETAIL", "AGE"]);

    // A stale-generation message must be dropped.
    app.switch_kind("pods");
    app.handle_msg(Msg::PrinterColumns {
        generation: app.generation - 1,
        plural: "widgets".into(),
        view: Box::new(None),
    });
    assert!(!app.crd_views.contains_key("widgets"));
}

#[tokio::test]
async fn user_view_wins_over_printer_columns() {
    let (mut app, _rx) = test_app();
    install_views(
        &mut app,
        r#"
        [[views.certificates.columns]]
        name = "MINE"
        path = "/status/mine"
        "#,
    );
    app.switch_kind("certificates");
    app.handle_msg(Msg::PrinterColumns {
        generation: app.generation,
        plural: "certificates".into(),
        view: Box::new(Some(crate::views::View {
            columns: vec![crate::views::UserColumn {
                header: "THEIRS".into(),
                pointer: "/status/theirs".into(),
                kind: crate::views::ColumnKind::Text,
                wide: false,
                width: None,
                align: None,
                condition_field: None,
            }],
            ..Default::default()
        })),
    });
    assert_eq!(app.display_headers(), ["NAME", "MINE", "AGE"]);
}

#[tokio::test]
async fn wide_toggle_reveals_pod_columns_and_keeps_sort() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    assert_eq!(
        app.display_headers(),
        ["NAME", "READY", "STATUS", "RESTARTS", "AGE", "CPU", "MEM"]
    );

    // Sort by AGE, then widen: the sort must follow the column's new index.
    app.sort_column = Some(4);
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_eq!(
        app.display_headers(),
        [
            "NAME", "READY", "STATUS", "RESTARTS", "IP", "NODE", "AGE", "CPU", "MEM"
        ]
    );
    assert_eq!(app.sort_column, Some(6));

    // Narrow again while sorted on a wide-only column: sort resets.
    app.sort_column = Some(4); // IP
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_eq!(app.sort_column, None);
}

#[tokio::test]
async fn rightsize_rejects_non_workload_kinds() {
    let (mut app, _rx) = test_app();
    app.switch_kind("configmaps");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "ConfigMap",
               "metadata": {"name": "cm", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.open_rightsize();
    assert_ne!(app.mode, Mode::Detail);
    assert!(app.flash.contains("right-size applies"), "{}", app.flash);
}

#[tokio::test]
async fn rightsize_report_renders_verdicts_and_patch() {
    // The report + patch are pure; exercise them directly with known numbers.
    use crate::rightsize::{ContainerRec, Quantiles};
    let recs = vec![ContainerRec {
        container: "app".into(),
        cpu: Quantiles {
            p50: Some(40.0),
            p95: Some(100.0),
            p99: Some(130.0),
        },
        mem: Quantiles {
            p50: Some(100_000_000.0),
            p95: Some(130_000_000.0),
            p99: Some(140_000_000.0),
        },
        cpu_request: Some(500.0),        // way over P95 → over-provisioned
        mem_request: Some(64_000_000.0), // under P95 → under-provisioned
        oom: Some(0.0),
        throttle: Some(0.0),
        suggested_cpu: Some(crate::rightsize::suggest(100.0, 15)),
        suggested_mem: Some(crate::rightsize::suggest(130_000_000.0, 15)),
    }];
    assert_eq!(recs[0].cpu_verdict(), crate::rightsize::Verdict::Over);
    assert_eq!(recs[0].mem_verdict(), crate::rightsize::Verdict::Under);
    let patch = crate::rightsize::patch_preview(&recs).unwrap();
    assert!(patch.contains("\"name\": \"app\""));
    assert!(patch.contains("cpu") && patch.contains("memory"));
}

#[tokio::test]
async fn fleet_without_configured_contexts_warns() {
    let (mut app, _rx) = test_app();
    app.open_fleet();
    assert_ne!(
        app.mode,
        Mode::Fleet,
        "no contexts → dashboard stays closed"
    );
    assert!(app.flash.contains("no fleet contexts"), "{}", app.flash);
}

#[tokio::test]
async fn fleet_toggle_in_context_switcher_edits_marks() {
    let (mut app, _rx) = test_app();
    app.fleet_cfg = crate::config::FleetConfig {
        contexts: vec!["prod".into()],
    };

    // Space on a non-member adds it after the config entries.
    app.ctx_list = vec!["prod".into(), "staging".into()];
    app.ctx_state.select(Some(1));
    app.mode = Mode::Contexts;
    app.key_contexts(press(KeyCode::Char(' ')));
    assert_eq!(app.fleet_contexts(), vec!["prod", "staging"]);
    assert!(app.is_fleet_context("staging"));
    assert!(app.flash.contains("fleet + staging"), "{}", app.flash);
    assert_eq!(app.mode, Mode::Contexts, "toggling stays in the switcher");

    // Space again removes it.
    app.key_contexts(press(KeyCode::Char(' ')));
    assert_eq!(app.fleet_contexts(), vec!["prod"]);
    assert!(app.flash.contains("fleet − staging"), "{}", app.flash);

    // A config-listed context can be masked out for the session too…
    app.ctx_state.select(Some(0));
    app.key_contexts(press(KeyCode::Char(' ')));
    assert!(app.fleet_contexts().is_empty());
    // …and re-added.
    app.key_contexts(press(KeyCode::Char(' ')));
    assert_eq!(app.fleet_contexts(), vec!["prod"]);
}

#[tokio::test]
async fn fleet_opens_with_marked_contexts_only() {
    let (mut app, _rx) = test_app();
    assert!(app.fleet_cfg.contexts.is_empty());
    app.fleet_marks.added.push("staging".into());
    app.open_fleet();
    assert_eq!(app.mode, Mode::Fleet);
    assert_eq!(app.fleet_rows.len(), 1);
    assert_eq!(app.fleet_rows[0].context, "staging");
}

#[tokio::test]
async fn fleet_marks_persist_across_restarts() {
    let dir = std::env::temp_dir().join(format!("sofka-fleet-test-{}", std::process::id()));
    let path = dir.join("fleet.toml");
    let _ = std::fs::remove_dir_all(&dir);

    // Toggling with a persist path saves the marks…
    let (mut app, _rx) = test_app();
    app.fleet_marks_path = Some(path.clone());
    app.ctx_list = vec!["prod".into(), "staging".into()];
    app.ctx_state.select(Some(1));
    app.mode = Mode::Contexts;
    app.key_contexts(press(KeyCode::Char(' ')));
    assert!(path.exists(), "marks file written on toggle");
    assert!(!app.flash_err, "{}", app.flash);

    // …and a fresh app (a restart) loads them back.
    let loaded = crate::fleet::FleetMarks::load(&path);
    assert_eq!(loaded, app.fleet_marks);
    assert_eq!(loaded.added, vec!["staging".to_string()]);

    // A missing or corrupt file degrades to empty marks, never an error.
    std::fs::write(&path, "not toml [[[").unwrap();
    assert_eq!(crate::fleet::FleetMarks::load(&path), Default::default());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fleet_seeds_connecting_rows_and_applies_summaries() {
    let (mut app, _rx) = test_app();
    app.fleet_cfg = crate::config::FleetConfig {
        contexts: vec!["prod".into(), "staging".into()],
    };
    app.open_fleet();
    assert_eq!(app.mode, Mode::Fleet);
    // One connecting row per configured context, up front.
    assert_eq!(app.fleet_rows.len(), 2);
    assert!(
        app.fleet_rows
            .iter()
            .all(|r| r.status == crate::fleet::FleetStatus::Connecting)
    );

    // A gathered summary lands and replaces the matching row by context name.
    let mut row = crate::fleet::FleetRow::connecting("staging".into(), false);
    row.status = crate::fleet::FleetStatus::Ok;
    row.version = "v1.31.0".into();
    row.nodes_ready = 3;
    row.nodes_total = 3;
    app.handle_msg(Msg::FleetRow {
        generation: app.generation,
        row: Box::new(row),
    });
    let staging = app
        .fleet_rows
        .iter()
        .find(|r| r.context == "staging")
        .unwrap();
    assert_eq!(staging.status, crate::fleet::FleetStatus::Ok);
    assert_eq!(staging.version, "v1.31.0");
    // The other context is untouched.
    assert_eq!(
        app.fleet_rows
            .iter()
            .find(|r| r.context == "prod")
            .unwrap()
            .status,
        crate::fleet::FleetStatus::Connecting
    );
}

#[tokio::test]
async fn ctrl_e_toggles_compact_mode_from_any_mode() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    assert!(!app.compact);

    // Toggles on from the table…
    app.handle_key(ctrl(KeyCode::Char('e'))).unwrap();
    assert!(app.compact);
    assert_eq!(app.mode, Mode::Table, "compact toggle doesn't change mode");

    // …and off again from a doc view (it's global, not table-only).
    app.mode = Mode::Detail;
    app.handle_key(ctrl(KeyCode::Char('e'))).unwrap();
    assert!(!app.compact);
    assert_eq!(app.mode, Mode::Detail);
}

#[tokio::test]
async fn crd_drill_seeds_printer_columns_from_the_crd() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let crd = obj(json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.com"},
        "spec": {
            "group": "example.com",
            "names": {"plural": "widgets", "kind": "Widget"},
            "scope": "Namespaced",
            "versions": [{
                "name": "v1", "served": true, "storage": true,
                "additionalPrinterColumns": [
                    {"name": "Phase", "type": "string", "jsonPath": ".status.phase"}
                ]
            }]
        }
    }));
    app.drill_into_crd(&crd);
    assert_eq!(app.kind_plural, "widgets");
    assert_eq!(app.display_headers(), ["NAMESPACE", "NAME", "PHASE", "AGE"]);
}

#[tokio::test]
async fn invalid_view_sort_column_warns_instead_of_crashing() {
    let (mut app, _rx) = test_app();
    install_views(
        &mut app,
        r#"
        [views.certificates]
        sort = "NOPE"
        "#,
    );
    app.switch_kind("certificates");
    assert_eq!(app.sort_column, None);
    assert!(app.flash.contains("NOPE"), "{}", app.flash);
    assert!(app.flash_err);
}

/// Type `/`, the filter text, then ⏎ — the way a user applies a filter.
fn type_filter(app: &mut App, text: &str) {
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in text.chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
}

fn row_names(app: &App) -> Vec<String> {
    app.rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn legacy_fuzzy_filter_with_spaces_is_one_pattern() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["alpha", "beta"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    // "default alp" fuzzy-matches across the "namespace name" haystack —
    // exactly the pre-grammar behavior (spaces are pattern chars, not ANDs).
    type_filter(&mut app, "default alp");
    assert_eq!(row_names(&app), ["alpha"]);
    assert!(!app.filter_server_side());
}

#[tokio::test]
async fn inverse_filter_hides_fuzzy_matches() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["api-1", "api-1-canary", "worker"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    type_filter(&mut app, "!canary");
    assert_eq!(row_names(&app), ["api-1", "worker"]);

    // Terms AND together: positive fuzzy + inverse.
    app.filter = "api !canary".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["api-1"]);
}

#[tokio::test]
async fn fuzzy_filter_matches_any_column_cell() {
    let (mut app, _rx) = test_app();
    app.switch_kind("services");
    for (n, ip) in [("api", "10.96.13.5"), ("web", "172.20.44.9")] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Service",
                   "metadata": {"name": n, "namespace": "default"},
                   "spec": {"type": "ClusterIP", "clusterIP": ip,
                            "ports": [{"port": 80, "protocol": "TCP"}]}}),
        );
    }
    // An IP substring matches via the CLUSTER-IP cell, not the name.
    type_filter(&mut app, "10.96");
    assert_eq!(row_names(&app), ["api"]);

    // Name matching still works exactly as before.
    app.filter = "web".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["web"]);

    // Inverse terms see the cells too: hide the 10.96 service.
    app.filter = "!10.96".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["web"]);
}

#[tokio::test]
async fn status_filter_matches_status_column() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "crashy", "namespace": "default"},
        "status": {"phase": "Running", "containerStatuses": [
            {"ready": false, "restartCount": 3,
             "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
        ]}}),
    );
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "healthy", "namespace": "default"},
        "status": {"phase": "Running", "containerStatuses": [
            {"ready": true, "restartCount": 0, "state": {"running": {}}}
        ]}}),
    );

    // Equality is case-insensitive so nobody has to remember CamelCase.
    type_filter(&mut app, "status=crashloopbackoff");
    assert_eq!(row_names(&app), ["crashy"]);

    app.filter = "status!=CrashLoopBackOff".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["healthy"]);
}

#[tokio::test]
async fn restarts_filter_compares_numerically() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for (name, restarts) in [("calm", 0), ("flappy", 7)] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "status": {"phase": "Running", "containerStatuses": [
                {"ready": true, "restartCount": restarts, "state": {"running": {}}}
            ]}}),
        );
    }
    type_filter(&mut app, "restarts>=5");
    assert_eq!(row_names(&app), ["flappy"]);

    app.filter = "restarts<5".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["calm"]);
}

#[tokio::test]
async fn age_filter_compares_creation_timestamp() {
    use k8s_openapi::jiff::Timestamp;
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let now = Timestamp::now().as_second();
    for (name, age_secs) in [("old", 3 * 3600), ("fresh", 600)] {
        let created = Timestamp::from_second(now - age_secs).unwrap().to_string();
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": name, "namespace": "default",
                                "creationTimestamp": created}}),
        );
    }
    type_filter(&mut app, "age<2h");
    assert_eq!(row_names(&app), ["fresh"]);

    app.filter = "age>2h".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["old"]);
}

#[tokio::test]
async fn cpu_and_memory_filters_use_metrics_snapshot() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    for n in ["hungry", "light"] {
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": n, "namespace": "default"}}),
        );
    }
    let mut data = HashMap::new();
    data.insert("default/hungry".to_string(), (600, 2 * 1024 * 1024 * 1024));
    data.insert("default/light".to_string(), (100, 64 * 1024 * 1024));
    app.handle_msg(Msg::Metrics {
        generation: app.generation,
        data,
        containers: HashMap::new(),
    });

    type_filter(&mut app, "cpu>500m");
    assert_eq!(row_names(&app), ["hungry"]);

    app.filter = "memory>1Gi".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["hungry"]);

    app.filter = "mem<=512Mi".into();
    app.invalidate_rows();
    assert_eq!(row_names(&app), ["light"]);
}

#[tokio::test]
async fn label_selector_goes_server_side_on_enter_and_survives_navigation() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let before = app.generation;

    type_filter(&mut app, "-l app=api,env=prod");
    assert_eq!(
        app.applied_filter_labels.as_deref(),
        Some("app=api,env=prod")
    );
    assert!(app.filter_server_side());
    assert_eq!(
        app.generation,
        before + 1,
        "⏎ must restart the watch with the selector"
    );
    assert!(app.flash.contains("server-side"), "{}", app.flash);

    // A refresh (ctrl-r) keeps the selector: it derives from the filter.
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .unwrap();
    assert_eq!(
        app.applied_filter_labels.as_deref(),
        Some("app=api,env=prod")
    );

    // Switching namespace with `0` keeps the filter — and the selector.
    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert!(app.all_namespaces());
    assert_eq!(
        app.applied_filter_labels.as_deref(),
        Some("app=api,env=prod")
    );

    // Esc clears the filter and widens the watch back out.
    let gen_before_clear = app.generation;
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.filter.is_empty());
    assert_eq!(app.applied_filter_labels, None);
    assert!(!app.filter_server_side());
    assert_eq!(app.generation, gen_before_clear + 1);
}

#[tokio::test]
async fn field_selector_goes_server_side_on_enter() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    type_filter(&mut app, "-f spec.nodeName=node-3");
    assert_eq!(
        app.applied_filter_fields.as_deref(),
        Some("spec.nodeName=node-3")
    );
    assert_eq!(app.applied_filter_labels, None);
    assert!(app.filter_server_side());
}

#[tokio::test]
async fn local_filter_edits_never_restart_the_watch() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let before = app.generation;
    type_filter(&mut app, "api");
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.generation, before, "no `-l`/`-f` → no rewatch");
    assert!(!app.filter_server_side());
}

#[tokio::test]
async fn drill_clears_server_selector_and_pop_restores_it() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    type_filter(&mut app, "-l env=prod");
    assert_eq!(app.applied_filter_labels.as_deref(), Some("env=prod"));

    apply(
        &mut app,
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}
        }),
    );
    app.table_state.select(Some(0));

    // Drill: like the fuzzy filter, the filter (and with it the server-side
    // selector) is cleared for the child view; the drill's own selector takes
    // over.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert!(app.filter.is_empty());
    assert_eq!(app.applied_filter_labels, None);
    assert_eq!(app.labels.as_deref(), Some("app=web"));

    // Pop: the saved frame restores the filter, and the rewatch re-applies
    // its selector server-side.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.filter, "-l env=prod");
    assert_eq!(app.applied_filter_labels.as_deref(), Some("env=prod"));
    assert_eq!(app.labels, None);
}

#[tokio::test]
async fn root_switch_and_history_clear_server_selector_like_fuzzy() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    type_filter(&mut app, "-l app=api");
    assert!(app.filter_server_side());

    // A fresh root view clears the filter (fuzzy always worked this way) —
    // and therefore the selector.
    app.switch_kind("deployments");
    assert!(app.filter.is_empty());
    assert_eq!(app.applied_filter_labels, None);

    // History replay lands on the root view without the old filter.
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert!(app.filter.is_empty());
    assert_eq!(app.applied_filter_labels, None);
}

#[tokio::test]
async fn malformed_filter_enter_warns_and_stays_local() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let before = app.generation;
    type_filter(&mut app, "-l");
    assert!(app.flash_err);
    assert!(app.flash.contains("-l"), "{}", app.flash);
    assert_eq!(app.generation, before, "broken selector must not rewatch");
    assert!(!app.filter_server_side());
    assert!(app.filter_error().is_some());
}

#[tokio::test]
async fn structured_filter_highlights_first_fuzzy_term() {
    let (mut app, _rx) = test_app();
    app.filter = "!zzz khc status=Running".into();
    let idx = app.filter_match_indices("kube-httpcache-0").unwrap();
    assert_eq!(idx.len(), 3);

    // No positive fuzzy term → nothing to highlight.
    app.filter = "-l app=api".into();
    assert_eq!(app.filter_match_indices("kube-httpcache-0"), None);
}

#[test]
fn join_selectors_merges_drill_and_filter() {
    let some = |s: &str| Some(s.to_string());
    assert_eq!(join_selectors(&None, &None), None);
    assert_eq!(join_selectors(&some("a=b"), &None), some("a=b"));
    assert_eq!(join_selectors(&None, &some("c=d")), some("c=d"));
    assert_eq!(join_selectors(&some("a=b"), &some("c=d")), some("a=b,c=d"));
}

// ----- config reload (`:reload`) and validation (`:config`) ---------------

fn write_config(dir: &std::path::Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

#[tokio::test]
async fn reload_applies_config_changes_live() {
    let dir = std::env::temp_dir().join(format!("sofka-app-reload-ok-{}", std::process::id()));
    write_config(&dir, "[aliases]\ndep = \"deployments\"\n");
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));

    app.reload_config();
    assert_eq!(
        app.user_aliases.get("dep").map(String::as_str),
        Some("deployments")
    );
    assert!(app.cluster.resolve("dep").is_some(), "alias registered");
    assert!(!app.flash_err);
    assert!(app.config_warnings.is_empty());

    // Edit the file on disk: `:reload` picks up new aliases, mode, and skin
    // without a restart. (The skin is the built-in default so the write to
    // the global palette is value-identical — parallel tests read it.)
    write_config(
        &dir,
        "readonly = true\n[aliases]\nks = \"services\"\n[skin]\nname = \"catppuccin-mocha\"\n",
    );
    app.reload_config();
    assert!(app.readonly);
    assert_eq!(
        app.user_aliases.get("ks").map(String::as_str),
        Some("services")
    );
    assert!(!app.user_aliases.contains_key("dep"), "aliases replaced");
    assert_eq!(app.session_skin.as_deref(), Some("catppuccin-mocha"));
    assert_eq!(app.active_skin.as_deref(), Some("catppuccin-mocha"));
    assert!(app.flash.contains("config reloaded"), "{}", app.flash);
    assert!(!app.flash_err);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn failed_reload_keeps_last_known_good_config() {
    let dir = std::env::temp_dir().join(format!("sofka-app-reload-bad-{}", std::process::id()));
    write_config(&dir, "readonly = true\n[aliases]\ndep = \"deployments\"\n");
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.reload_config();
    assert!(app.readonly);

    // A type error on disk: the running config must stay exactly as it was.
    write_config(&dir, "readonly = \"yes\"\n");
    app.reload_config();
    assert!(app.readonly, "previous readonly kept");
    assert_eq!(
        app.user_aliases.get("dep").map(String::as_str),
        Some("deployments")
    );
    assert!(app.flash_err);
    assert!(app.flash.contains("previous config kept"), "{}", app.flash);
    // The recorded error names the file, the offending key, and the problem.
    let err = &app.config_warnings[0];
    assert!(err.contains("config.toml"), "{err}");
    assert!(err.contains("readonly"), "{err}");
    assert!(err.contains("expected a boolean"), "{err}");

    // A later good edit recovers without a restart.
    write_config(&dir, "readonly = false\n");
    app.reload_config();
    assert!(!app.readonly);
    assert!(app.config_warnings.is_empty());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn reload_reports_skin_validation_warnings() {
    let dir = std::env::temp_dir().join(format!("sofka-app-reload-skin-{}", std::process::id()));
    write_config(
        &dir,
        "[skin]\nname = \"no-such-skin\"\n[skin.colors]\ngreen = \"zzz\"\n",
    );
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.reload_config();
    assert!(app.flash_err);
    assert!(app.flash.contains("warning"), "{}", app.flash);
    assert!(
        app.config_warnings
            .iter()
            .any(|w| w.contains("skin.name") && w.contains("no-such-skin")),
        "{:?}",
        app.config_warnings
    );
    assert!(
        app.config_warnings
            .iter()
            .any(|w| w.contains("skin.colors.green") && w.contains("invalid hex")),
        "{:?}",
        app.config_warnings
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn reload_palette_command_dispatches() {
    let (mut app, _rx) = test_app();
    assert!(app.run_palette_command("reload"));
    assert!(app.flash.contains("config reloaded"), "{}", app.flash);
}

#[tokio::test]
async fn info_view_reports_version_cluster_and_watch_health() {
    let (mut app, _rx) = test_app();
    app.cluster.server_version = "v1.36.2-eks-bca9cf6".into();
    app.watch_errors = 3;
    app.last_error = Some("connection refused".into());
    assert!(app.run_palette_command("info"));
    assert_eq!(app.mode, Mode::Detail);
    let text = app
        .detail
        .lines
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains(&format!("sofka v{}", crate::diagnostics::VERSION)));
    assert!(text.contains("Cluster"), "{text}");
    assert!(text.contains("api server:"), "{text}");
    assert!(text.contains("k8s rev:     v1.36.2-eks-bca9cf6"), "{text}");
    assert!(text.contains("errors: 3"), "{text}");
    assert!(text.contains("connection refused"), "{text}");
    assert!(text.contains("Directories"), "{text}");
    // Never leaks credentials — the report is identifiers and counts only.
    assert!(!text.to_lowercase().contains("bearer"), "{text}");
}

#[tokio::test]
async fn config_view_lists_sources_active_skin_and_warnings() {
    let dir = std::env::temp_dir().join(format!("sofka-app-cfg-view-{}", std::process::id()));
    write_config(&dir, "[skin]\nname = \"catppuccin-mocha\"\n");
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.reload_config();
    app.config_warnings = vec!["skin.colors.red: invalid hex 'x' (expected #rrggbb)".into()];

    assert!(app.run_palette_command("config"));
    assert_eq!(app.mode, Mode::Detail);
    let text = app
        .detail
        .lines
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let base = dir.join("config.toml").display().to_string();
    assert!(text.contains(&base) && text.contains("(loaded)"), "{text}");
    assert!(text.contains("skin: catppuccin-mocha"), "{text}");
    assert!(text.contains("skin.colors.red"), "{text}");

    // Esc returns to the table, like any doc view.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ----- provider logs (VictoriaLogs) ------------------------------------

fn install_provider(app: &mut App) {
    let cfg = crate::config::LogProviderConfig {
        kind: "victorialogs".into(),
        // Unroutable on purpose: the spawned backfill task fails into the
        // log buffer; these tests only assert on launch-time state.
        url: "http://localhost:1".into(),
        ..Default::default()
    };
    let (provider, warnings) = crate::providers::compile(Some(&cfg));
    assert!(warnings.is_empty(), "{warnings:?}");
    app.log_provider = provider;
}

#[tokio::test]
async fn provider_logs_from_pod_row() {
    let (mut app, _rx) = test_app();
    install_provider(&mut app);
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api-1", "namespace": "prod"},
            "spec": {"containers": [{"name": "app"}, {"name": "istio"}]}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('L'))).unwrap();

    assert_eq!(app.mode, Mode::Logs);
    assert!(
        app.logs.view.title.contains("victorialogs (1h)"),
        "{}",
        app.logs.view.title
    );
    match &app.logs.source {
        Some(LogSource::Provider {
            request:
                crate::providers::LogRequest::Pod {
                    ns,
                    pod,
                    container,
                    multi_container,
                },
        }) => {
            assert_eq!(ns, "prod");
            assert_eq!(pod, "api-1");
            assert!(container.is_none());
            assert!(*multi_container);
        }
        other => panic!("unexpected source: {other:?}"),
    }

    // Provider lines ride the shared log channel/generation.
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec!["hello from vlogs".into()],
    });
    assert_eq!(app.logs.view.lines[0], "hello from vlogs");

    // Esc returns without disturbing the table watch.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn provider_logs_discover_when_unconfigured() {
    let (mut app, _rx) = test_app();
    assert!(app.log_provider.is_none());
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api-1", "namespace": "prod"},
            "spec": {"containers": [{"name": "app"}]}
        }),
    );
    app.table_state.select(Some(0));

    // No config: the view still opens (with default lookback in the title);
    // the spawned task autodiscovers before querying.
    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    assert!(
        app.logs.view.title.contains("victorialogs (1h)"),
        "{}",
        app.logs.view.title
    );

    // A successful discovery is reported back and cached for later presses…
    let cfg = crate::config::LogProviderConfig {
        kind: "victorialogs".into(),
        url: "http://localhost:1".into(),
        ..Default::default()
    };
    let discovered = crate::providers::compile(Some(&cfg)).0.unwrap();
    app.handle_msg(Msg::LogProviderDiscovered {
        generation: app.generation,
        provider: Box::new(discovered),
    });
    assert!(app.log_provider.is_some());

    // …but a stale discovery (older view generation, e.g. after a context
    // switch) is dropped.
    let (mut app2, _rx2) = test_app();
    let cfg2 = crate::config::LogProviderConfig {
        kind: "victorialogs".into(),
        url: "http://localhost:1".into(),
        ..Default::default()
    };
    let stale = crate::providers::compile(Some(&cfg2)).0.unwrap();
    app2.bump_generation();
    app2.handle_msg(Msg::LogProviderDiscovered {
        generation: app2.generation - 1,
        provider: Box::new(stale),
    });
    assert!(app2.log_provider.is_none());
}

#[tokio::test]
async fn provider_logs_scopes_workload_namespace_and_rejects_others() {
    let (mut app, _rx) = test_app();
    install_provider(&mut app);

    app.switch_kind("deployments");
    apply(
        &mut app,
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "prod"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    match &app.logs.source {
        Some(LogSource::Provider {
            request: crate::providers::LogRequest::Selector { ns, labels },
        }) => {
            assert_eq!(ns, "prod");
            assert_eq!(labels, "app=web");
        }
        other => panic!("unexpected source: {other:?}"),
    }
    app.handle_key(press(KeyCode::Esc)).unwrap();

    app.switch_kind("namespaces");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": {"name": "prod"}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    match &app.logs.source {
        Some(LogSource::Provider {
            request: crate::providers::LogRequest::Namespace { ns },
        }) => assert_eq!(ns, "prod"),
        other => panic!("unexpected source: {other:?}"),
    }
    app.handle_key(press(KeyCode::Esc)).unwrap();

    app.switch_kind("secrets");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": "creds", "namespace": "prod"}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("provider logs"), "{}", app.flash);
}

#[tokio::test]
async fn provider_logs_for_one_container_from_picker() {
    let (mut app, _rx) = test_app();
    install_provider(&mut app);
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api-1", "namespace": "prod"},
            "spec": {"containers": [{"name": "app"}, {"name": "istio"}]}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Containers);

    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    assert!(
        app.logs.view.title.starts_with("api-1:app —"),
        "{}",
        app.logs.view.title
    );
    match &app.logs.source {
        Some(LogSource::Provider {
            request:
                crate::providers::LogRequest::Pod {
                    container: Some(c), ..
                },
        }) => assert_eq!(c, "app"),
        other => panic!("unexpected source: {other:?}"),
    }
}

#[tokio::test]
async fn provider_lookback_prompt_changes_period_and_requeries() {
    let (mut app, _rx) = test_app();
    install_provider(&mut app);
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api-1", "namespace": "prod"},
            "spec": {"containers": [{"name": "app"}]}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert!(app.logs.view.title.contains("victorialogs (1h)"));

    // `T` prompts for a period, drawn over the logs view.
    app.handle_key(press(KeyCode::Char('T'))).unwrap();
    assert_eq!(app.mode, Mode::Prompt);
    assert!(app.prompt_over_logs());
    assert!(
        app.prompt_label.contains("current: 1h"),
        "{}",
        app.prompt_label
    );

    // Esc returns to the logs view, keeping the period.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    assert!(app.logs.view.title.contains("(1h)"));

    // A valid period retitles, updates the session provider, and re-queries
    // (new log generation).
    let gen_before = app.log_gen;
    app.handle_key(press(KeyCode::Char('T'))).unwrap();
    app.handle_key(press(KeyCode::Char('4'))).unwrap();
    app.handle_key(press(KeyCode::Char('h'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    assert!(
        app.logs.view.title.contains("victorialogs (4h)"),
        "{}",
        app.logs.view.title
    );
    assert_eq!(app.log_provider.as_ref().unwrap().lookback_label, "4h");
    assert!(app.log_gen > gen_before, "lookback change must re-stream");
    assert_eq!(app.flash, "lookback: 4h");

    // Garbage is rejected with a warning; nothing changes.
    app.handle_key(press(KeyCode::Char('T'))).unwrap();
    for c in "soon".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    assert!(app.flash_err);
    assert!(app.flash.contains("lookback"), "{}", app.flash);
    assert!(app.logs.view.title.contains("(4h)"));

    // Later provider launches inherit the changed period.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    app.handle_key(press(KeyCode::Char('L'))).unwrap();
    assert!(
        app.logs.view.title.contains("victorialogs (4h)"),
        "{}",
        app.logs.view.title
    );
}

#[tokio::test]
async fn lookback_key_only_applies_to_provider_logs() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    apply(
        &mut app,
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api-1", "namespace": "prod"},
            "spec": {"containers": [{"name": "app"}]}
        }),
    );
    app.table_state.select(Some(0));

    // Kubelet logs: `T` explains itself instead of prompting.
    app.handle_key(press(KeyCode::Char('l'))).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    app.handle_key(press(KeyCode::Char('T'))).unwrap();
    assert_eq!(app.mode, Mode::Logs);
    assert!(app.flash.contains("provider logs"), "{}", app.flash);
}

#[tokio::test]
async fn logs_fullscreen_toggles_with_shift_f() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    app.return_mode = Mode::Table;

    assert!(!app.logs.fullscreen);
    app.handle_key(press(KeyCode::Char('F'))).unwrap();
    assert!(app.logs.fullscreen);
    assert!(app.flash.contains("fullscreen: on"), "{}", app.flash);
    assert_eq!(app.mode, Mode::Logs, "toggling must not leave the view");

    app.handle_key(press(KeyCode::Char('F'))).unwrap();
    assert!(!app.logs.fullscreen);
}

#[tokio::test]
async fn logs_time_anchors_restream_kubelet_logs() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    app.return_mode = Mode::Table;
    app.logs.source = Some(LogSource::Pod {
        ns: "default".into(),
        name: "web".into(),
        containers: vec![],
    });

    // No anchor: the config decides (default = tail, no since).
    assert_eq!(app.log_tail_and_since(), (app.logs_cfg.tail, None));

    // `2` anchors to the last 5m and re-streams (generation bumps).
    let gen_before = app.log_gen;
    app.handle_key(press(KeyCode::Char('2'))).unwrap();
    assert_eq!(app.logs.since_anchor, Some(300));
    assert_eq!(app.logs.anchor_label(), Some("5m"));
    assert_eq!(app.log_tail_and_since().1, Some(300));
    assert!(app.log_gen > gen_before, "anchor must restart the stream");
    assert!(app.flash.contains("5m"), "{}", app.flash);

    // `0` forces the plain tail, even over a configured `since`.
    app.logs_cfg.since = Some("4h".into());
    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert_eq!(app.logs.since_anchor, Some(0));
    assert_eq!(app.log_tail_and_since(), (app.logs_cfg.tail, None));
    assert_eq!(app.logs.anchor_label(), Some("tail"));

    // Without an anchor the configured `since` applies again.
    app.logs.since_anchor = None;
    assert_eq!(app.log_tail_and_since().1, Some(4 * 3600));
}

#[tokio::test]
async fn logs_time_anchors_set_provider_lookback() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    app.return_mode = Mode::Table;
    app.logs.source = Some(LogSource::Provider {
        request: crate::providers::LogRequest::Namespace {
            ns: "default".into(),
        },
    });
    app.logs.view.title = "ns/default — victorialogs (1h)".into();

    // `3` maps to the 15m window on the provider, not the kubelet anchor.
    app.handle_key(press(KeyCode::Char('3'))).unwrap();
    assert_eq!(app.provider_lookback_label(), "15m");
    assert!(
        app.logs.view.title.ends_with("victorialogs (15m)"),
        "{}",
        app.logs.view.title
    );
    assert_eq!(app.logs.since_anchor, None);

    // `0` resets to the default lookback window.
    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert_eq!(
        app.provider_lookback_label(),
        crate::providers::DEFAULT_LOOKBACK
    );
}

// ----- view cache (instant redisplay on navigation) -------------------------

fn pod(name: &str) -> serde_json::Value {
    json!({"apiVersion": "v1", "kind": "Pod",
           "metadata": {"name": name, "namespace": "default"}})
}

/// Drive the current view through a full initial sync with the given pods.
fn sync_view(app: &mut App, names: &[&str]) {
    app.handle_msg(Msg::Reset {
        generation: app.generation,
    });
    for n in names {
        apply(app, pod(n));
    }
    app.handle_msg(Msg::Synced {
        generation: app.generation,
    });
}

#[tokio::test]
async fn returning_to_a_view_shows_cached_rows_instantly() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    sync_view(&mut app, &["a", "b"]);

    app.switch_kind("deployments");
    assert_eq!(app.store.len(), 0, "no cache for a first visit");

    app.switch_kind("pods");
    assert_eq!(
        app.store.len(),
        2,
        "cached rows shown before the watch syncs"
    );
    assert!(app.store.get("default/a").is_some());
    assert!(
        !app.store.synced,
        "cached rows must be marked syncing, not live"
    );
}

#[tokio::test]
async fn relist_over_cached_rows_swaps_atomically_on_sync() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    sync_view(&mut app, &["a"]);
    app.switch_kind("deployments");
    app.switch_kind("pods"); // seeded with cached "a"

    // The fresh watch relists: the stale row stays visible while the new
    // set streams in, then the sync swaps it in wholesale.
    app.handle_msg(Msg::Reset {
        generation: app.generation,
    });
    apply(&mut app, pod("b"));
    assert!(
        app.store.get("default/a").is_some(),
        "stale row still shown mid-relist"
    );
    assert!(
        app.store.get("default/b").is_none(),
        "incoming rows buffer until the sync"
    );

    app.handle_msg(Msg::Synced {
        generation: app.generation,
    });
    assert!(app.store.synced);
    assert!(
        app.store.get("default/a").is_none(),
        "stale row swapped out"
    );
    assert!(app.store.get("default/b").is_some());
    assert_eq!(app.store.len(), 1);
}

#[tokio::test]
async fn unchanged_row_order_reuses_shared_keys_on_content_updates() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let pod_state = |rv: &str, phase: &str| {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {
                "name": "a", "namespace": "default", "resourceVersion": rv
            },
            "status": {"phase": phase}
        })
    };
    apply(&mut app, pod_state("1", "Pending"));
    let rows = app.rows();
    app.ensure_table_cell_cache(&rows);
    drop(rows);

    let store_key = app.store.key("default/a").unwrap();
    let cache = app.rows_cache.borrow();
    assert!(std::rc::Rc::ptr_eq(store_key, &cache.keys[0]));
    assert!(std::rc::Rc::ptr_eq(
        store_key,
        cache.cells.get_key_value("default/a").unwrap().0
    ));
    assert!(!cache.dirty);
    drop(cache);

    apply(&mut app, pod_state("2", "Running"));
    let cache = app.rows_cache.borrow();
    assert!(
        !cache.dirty,
        "an unsorted, unfiltered update keeps row order"
    );
    assert!(!cache.cells.contains_key("default/a"));
    drop(cache);
    assert_eq!(
        app.rows()[0].data.pointer("/status/phase"),
        Some(&json!("Running"))
    );
}

#[tokio::test]
async fn updates_still_rebuild_when_filter_membership_can_change() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    let pod_state = |rv: &str, phase: &str| {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {
                "name": "a", "namespace": "default", "resourceVersion": rv
            },
            "status": {"phase": phase}
        })
    };
    apply(&mut app, pod_state("1", "Running"));
    app.filter = "Running".into();
    app.invalidate_rows();
    assert_eq!(app.row_count(), 1);

    apply(&mut app, pod_state("2", "Pending"));
    assert!(app.rows_cache.borrow().dirty);
    assert_eq!(app.row_count(), 0);
}

#[tokio::test]
async fn buffered_relist_updates_do_not_invalidate_visible_rows_until_swap() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    sync_view(&mut app, &["a"]);
    assert_eq!(app.row_count(), 1);
    assert!(!app.rows_cache.borrow().dirty);

    app.handle_msg(Msg::Reset {
        generation: app.generation,
    });
    apply(&mut app, pod("b"));
    assert!(!app.rows_cache.borrow().dirty);
    assert_eq!(app.rows()[0].metadata.name.as_deref(), Some("a"));

    app.handle_msg(Msg::Synced {
        generation: app.generation,
    });
    assert!(app.rows_cache.borrow().dirty);
    assert_eq!(app.rows()[0].metadata.name.as_deref(), Some("b"));
}

#[tokio::test]
async fn first_visit_still_streams_rows_progressively() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_msg(Msg::Reset {
        generation: app.generation,
    });
    apply(&mut app, pod("a"));
    assert_eq!(
        app.store.len(),
        1,
        "with nothing on screen, rows appear as they stream in"
    );
    assert!(!app.store.synced);
}

#[tokio::test]
async fn unsynced_view_is_not_cached() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_msg(Msg::Reset {
        generation: app.generation,
    });
    apply(&mut app, pod("a")); // partial list, never synced

    app.switch_kind("deployments");
    app.switch_kind("pods");
    assert_eq!(
        app.store.len(),
        0,
        "a partial list must not be redisplayed as if complete"
    );
}

#[tokio::test]
async fn context_switch_drops_cached_views() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    sync_view(&mut app, &["a"]);
    app.switch_kind("deployments");

    app.apply_context_switch("prod".into(), Box::new(Cluster::fake()));
    app.switch_kind("pods");
    assert_eq!(
        app.store.len(),
        0,
        "another cluster's rows must never be redisplayed"
    );
}

#[tokio::test]
async fn view_cache_evicts_least_recently_used() {
    let (mut app, _rx) = test_app();
    // Fill the cache beyond its cap with distinct (kind, namespace) views.
    app.switch_kind("pods");
    sync_view(&mut app, &["a"]);
    for i in 0..VIEW_CACHE_MAX {
        app.switch_kind_ns("pods", Some(&format!("ns{i}")));
        sync_view(&mut app, &["x"]);
    }
    app.switch_kind_ns("pods", Some("default"));
    assert_eq!(
        app.store.len(),
        0,
        "the oldest snapshot is evicted once the cache is full"
    );
    // The most recent one is still cached.
    sync_view(&mut app, &["y"]);
    app.switch_kind_ns("pods", Some(&format!("ns{}", VIEW_CACHE_MAX - 1)));
    assert_eq!(app.store.len(), 1);
}

/// A view-count cap is not a memory cap: two 2,000-pod views cost twice one.
/// The object bound is what keeps a big cluster's cache from multiplying, so
/// a large view must evict earlier than `VIEW_CACHE_MAX` would.
#[tokio::test]
async fn view_cache_evicts_on_total_objects_not_just_view_count() {
    let (mut app, _rx) = test_app();
    // Two views, each holding more than half the object budget, so the second
    // one alone pushes the total over and evicts the first — well before the
    // view-count cap of VIEW_CACHE_MAX would.
    let big: Vec<String> = (0..(VIEW_CACHE_MAX_OBJECTS * 2 / 3))
        .map(|i| format!("p{i}"))
        .collect();
    let big: Vec<&str> = big.iter().map(String::as_str).collect();

    app.switch_kind_ns("pods", Some("first"));
    sync_view(&mut app, &big);
    app.switch_kind_ns("pods", Some("second"));
    sync_view(&mut app, &big);

    // Returning to the first view finds nothing: its snapshot was evicted to
    // stay under the object budget, even though only two views were cached.
    app.switch_kind_ns("pods", Some("first"));
    assert_eq!(
        app.store.len(),
        0,
        "a large snapshot must be evicted on the object bound, not held \
         until the view-count cap"
    );
}

/// The entry you just left is the one the cache exists for, so it survives
/// even when it alone busts the object budget.
#[tokio::test]
async fn view_cache_keeps_the_most_recent_snapshot_however_large() {
    let (mut app, _rx) = test_app();
    let huge: Vec<String> = (0..(VIEW_CACHE_MAX_OBJECTS + 100))
        .map(|i| format!("p{i}"))
        .collect();
    let huge: Vec<&str> = huge.iter().map(String::as_str).collect();

    app.switch_kind_ns("pods", Some("huge"));
    sync_view(&mut app, &huge);
    app.switch_kind_ns("pods", Some("elsewhere"));
    app.switch_kind_ns("pods", Some("huge"));

    assert_eq!(
        app.store.len(),
        huge.len(),
        "the most recent snapshot is kept even when it exceeds the budget"
    );
}

/// `match_lines` is memoized because `doc_title` calls it every frame. The
/// cache keys on `(filter, revision, line count)`; these pin each dimension.
#[tokio::test]
async fn doc_search_cache_follows_the_filter() {
    let mut doc = Scrollable::doc(
        "t".into(),
        vec!["alpha".into(), "beta".into(), "alpha again".into()],
    );

    doc.filter = "alpha".into();
    assert_eq!(doc.match_lines(), vec![0, 2]);
    // Second call must come from the cache and still be right.
    assert_eq!(doc.match_lines(), vec![0, 2]);

    doc.filter = "beta".into();
    assert_eq!(
        doc.match_lines(),
        vec![1],
        "stale matches after filter change"
    );

    doc.filter.clear();
    assert!(doc.match_lines().is_empty());
}

/// The hazard the `revision` counter exists for: a refreshed events list can
/// replace the document with one of *identical* line count while a search is
/// active, which a `(filter, line count)` key alone would not notice.
#[tokio::test]
async fn doc_search_cache_survives_a_same_length_document_swap() {
    let mut doc = Scrollable::doc("t".into(), vec!["hit".into(), "miss".into()]);
    doc.filter = "hit".into();
    assert_eq!(doc.match_lines(), vec![0]);

    doc.replace_lines(vec!["miss".into(), "hit".into()].into());
    assert_eq!(
        doc.match_lines(),
        vec![1],
        "stale cache served after a same-length document swap"
    );
}

/// Document search is a plain substring, not the log view's grammar: `!` and
/// `/re/` are literal characters here and must not invert or compile.
#[tokio::test]
async fn doc_search_is_a_plain_substring_not_the_log_filter_grammar() {
    let mut doc = Scrollable::doc(
        "t".into(),
        vec![
            "plain line".into(),
            "has !bang in it".into(),
            "has /slashes/ in it".into(),
        ],
    );

    doc.filter = "!bang".into();
    assert_eq!(
        doc.match_lines(),
        vec![1],
        "`!` must be literal, not inverse"
    );

    doc.filter = "/slashes/".into();
    assert_eq!(doc.match_lines(), vec![2], "`/re/` must be literal");
}

/// Search is case-insensitive and must stay exact across multi-byte lines.
#[tokio::test]
async fn doc_search_is_case_insensitive_and_multibyte_safe() {
    let mut doc = Scrollable::doc(
        "t".into(),
        vec![
            "Image: nginx".into(),
            "日本語 image 測定".into(),
            "none".into(),
        ],
    );
    doc.filter = "IMAGE".into();
    assert_eq!(doc.match_lines(), vec![0, 1]);
}

// ---- log view index (2.3) -------------------------------------------------
//
// The index is maintained incrementally across frames, so the property that
// matters is that it never diverges from a from-scratch computation, through
// appends, filter changes, wrap toggles, trims and clears.

/// Recompute shown-line indices and total display rows the naive way.
fn naive_log_index(logs: &LogsView, wrap_width: usize) -> (Vec<u32>, usize) {
    let shown: Vec<u32> = logs
        .view
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| logs.matches(l))
        .map(|(i, _)| i as u32)
        .collect();
    let total = if wrap_width > 0 {
        shown
            .iter()
            .map(|&i| crate::ui::wrapped_height(&logs.view.lines[i as usize], wrap_width))
            .sum()
    } else {
        shown.len()
    };
    (shown, total)
}

fn assert_index_matches_naive(logs: &mut LogsView, wrap_width: usize, what: &str) {
    let (want_shown, want_total) = naive_log_index(logs, wrap_width);
    let idx = logs.refresh_index(wrap_width);
    let got: Vec<u32> = (0..idx.shown_len())
        .map(|i| idx.line_at(i).unwrap() as u32)
        .collect();
    assert_eq!(got, want_shown, "shown lines diverged after {what}");
    assert_eq!(
        idx.total_rows(),
        want_total,
        "total rows diverged after {what}"
    );

    // Row arithmetic must be self-consistent: starts ascend, and each line's
    // start plus its height is the next line's start.
    let mut expect_start = 0usize;
    for i in 0..idx.shown_len() {
        assert_eq!(
            idx.start_row(i),
            expect_start,
            "start_row({i}) after {what}"
        );
        // The first line reaching this row must be this one.
        assert_eq!(
            idx.first_at_row(expect_start),
            i,
            "first_at_row after {what}"
        );
        expect_start += idx.height_at(i);
    }
    assert_eq!(expect_start, idx.total_rows(), "heights must sum to total");
}

#[tokio::test]
async fn log_index_tracks_appends_incrementally() {
    let mut logs = LogsView::default();
    for w in [0usize, 20] {
        logs.view.clear_lines();
        logs.set_filter(String::new());
        assert_index_matches_naive(&mut logs, w, "empty buffer");

        for batch in 0..5 {
            logs.view
                .lines
                .extend((0..20).map(|i| format!("batch {batch} line {i} some padding text here")));
            assert_index_matches_naive(&mut logs, w, "append");
        }
    }
}

#[tokio::test]
async fn log_index_rebuilds_on_filter_and_wrap_changes() {
    let mut logs = LogsView::default();
    logs.view
        .lines
        .extend((0..60).map(|i| format!("line {i} {}", if i % 3 == 0 { "keep" } else { "drop" })));

    assert_index_matches_naive(&mut logs, 0, "no filter");

    logs.set_filter("keep".into());
    assert_index_matches_naive(&mut logs, 0, "filter applied");

    // Wrap on, at a width that forces multi-row lines.
    assert_index_matches_naive(&mut logs, 8, "wrap on");
    // ...and back off.
    assert_index_matches_naive(&mut logs, 0, "wrap off");

    logs.set_filter("drop".into());
    assert_index_matches_naive(&mut logs, 8, "filter changed while wrapped");

    logs.set_filter(String::new());
    assert_index_matches_naive(&mut logs, 8, "filter cleared");
}

#[tokio::test]
async fn log_index_rebuilds_when_the_buffer_is_trimmed_or_cleared() {
    let mut logs = LogsView::default();
    logs.view
        .lines
        .extend((0..50).map(|i| format!("line {i} with enough text to wrap somewhere")));
    logs.set_filter("line".into());
    assert_index_matches_naive(&mut logs, 12, "initial");

    // Trimming the front shifts every index — the index must not reuse stale
    // positions.
    logs.view.drain_front(20);
    assert_index_matches_naive(&mut logs, 12, "drain_front");

    logs.view
        .lines
        .extend((0..10).map(|i| format!("post-trim {i}")));
    assert_index_matches_naive(&mut logs, 12, "append after trim");

    logs.view.clear_lines();
    assert_index_matches_naive(&mut logs, 12, "clear");
    assert_eq!(logs.index().total_rows(), 0);
}

#[tokio::test]
async fn launching_logs_invalidates_the_previous_buffers_index() {
    let (mut app, _rx) = test_app();
    app.logs.wrap = true;
    app.logs.set_filter(String::new());
    app.logs
        .view
        .lines
        .extend((0..30).map(|i| format!("old line {i} xxxxxxxxxxxxxxxx")));
    app.logs.refresh_index(10);

    app.launch_logs(
        LogSource::Pod {
            ns: "default".into(),
            name: "new".into(),
            containers: Vec::new(),
        },
        "new".into(),
    );
    app.logs.view.lines.extend((0..40).map(|i| format!("n{i}")));

    let want = naive_log_index(&app.logs, 10).1;
    assert_eq!(app.logs.refresh_index(10).total_rows(), want);
}

#[tokio::test]
async fn log_index_first_at_row_finds_the_line_covering_a_row() {
    let mut logs = LogsView::default();
    // Deterministic heights: at width 10, a 25-char line is 3 rows.
    logs.view
        .lines
        .extend((0..6).map(|i| format!("{i}bcdefghijklmnopqrstuvwx")));
    let idx = logs.refresh_index(10);
    assert_eq!(idx.shown_len(), 6);
    let h0 = idx.height_at(0);
    assert!(h0 > 1, "test needs wrapped lines, got height {h0}");

    // Every row in a line's span must map back to that line.
    for i in 0..idx.shown_len() {
        let start = idx.start_row(i);
        for r in start..start + idx.height_at(i) {
            assert_eq!(idx.first_at_row(r), i, "row {r} should belong to line {i}");
        }
    }
}

// ---- fuzzy prefilter soundness (3.1) --------------------------------------

/// The mask prefilter must only ever produce false *positives*. If a pattern
/// really is a subsequence of a haystack, the mask test must let it through —
/// otherwise rows silently vanish from a filtered list.
#[test]
fn subseq_mask_never_rejects_a_real_subsequence() {
    let cases = [
        ("nginx-deployment-7d9f8b6c5d-x4k2p", "nginx"),
        ("nginx-deployment-7d9f8b6c5d-x4k2p", "ngxdep"),
        ("NGINX-Deployment", "nginx"),
        ("kube-system", "KUBE"),
        ("10.96.0.1", "10.96"),
        ("CrashLoopBackOff", "clbo"),
        ("日本語 temp", "temp"),
        ("日本語 temp", "日本"),
        ("a", "a"),
        ("abc", ""),
    ];
    for (hay, pat) in cases {
        let hm = subseq_mask(hay);
        let pm = subseq_mask(pat);
        assert_eq!(
            hm & pm,
            pm,
            "mask rejected {pat:?} which is a subsequence of {hay:?}"
        );
    }
}

/// End-to-end: the filtered row set must equal what an unfiltered fuzzy match
/// over the same cells would produce. Guards the prefilter *and* the
/// cache-through path together.
#[tokio::test]
async fn filtering_matches_a_naive_fuzzy_pass() {
    for pat in ["web", "kube", "zzz", "run", "10.", "WEB", "nginx"] {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        app.handle_msg(Msg::Reset {
            generation: app.generation,
        });
        for i in 0..40 {
            apply(
                &mut app,
                json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": format!("web-{i}"),
                        "namespace": if i % 2 == 0 { "default" } else { "kube-system" },
                        "resourceVersion": format!("{i}"),
                        "creationTimestamp": "2026-08-30T08:00:00Z",
                    },
                    "spec": { "nodeName": format!("node-{i}") },
                    "status": {
                        "phase": "Running",
                        "podIP": format!("10.0.0.{i}"),
                        "containerStatuses": [
                            { "name": "app", "ready": true, "restartCount": 0,
                              "state": { "running": { "startedAt": "2026-08-30T09:00:00Z" } } }
                        ],
                    },
                }),
            );
        }
        app.handle_msg(Msg::Synced {
            generation: app.generation,
        });

        // Naive expectation: name haystack, else any rendered cell.
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
        let spec = crate::columns::build_spec("pods", None, None, false);
        let mut want: Vec<String> = Vec::new();
        for (k, o) in app.store.iter() {
            let hay = format!(
                "{} {}",
                o.metadata.namespace.as_deref().unwrap_or(""),
                o.metadata.name.as_deref().unwrap_or("")
            );
            let hit = matcher.fuzzy_match(&hay, pat).is_some() || {
                let (cells, _) = spec.cells(o);
                cells.iter().any(|c| matcher.fuzzy_match(c, pat).is_some())
            };
            if hit {
                want.push(k.to_string());
            }
        }
        want.sort();

        app.filter = pat.to_string();
        app.invalidate_rows();
        let mut got: Vec<String> = (0..app.row_count())
            .filter_map(|i| app.rows_window(i, 1).first().map(|o| row_key(o)))
            .collect();
        got.sort();

        assert_eq!(got, want, "filter {pat:?} diverged from a naive fuzzy pass");
    }
}
