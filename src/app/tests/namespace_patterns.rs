use super::*;

fn pattern_app() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.cluster
        .add_aliases(&HashMap::from([("ns".into(), "namespaces".into())]));
    (app, rx)
}

fn resolve(app: &mut App, names: &[&str]) {
    app.handle_msg(Msg::NamespacePattern {
        generation: app.generation,
        request: app.namespace_request,
        pattern: "*-crons".into(),
        action: NamespacePatternAction::Resource("namespaces".into()),
        result: Ok(names.iter().map(|name| (*name).to_string()).collect()),
    });
}

fn watch(app: &mut App, namespace: &str, event: Msg) {
    app.handle_msg(Msg::NamespaceWatch {
        generation: app.generation,
        namespace: namespace.into(),
        event: Box::new(event),
    });
}

fn pod(app: &mut App, namespace: &str, name: &str) {
    let obj = obj(
        json!({"apiVersion":"v1", "kind":"Pod", "metadata":{"namespace":namespace,"name":name}}),
    );
    watch(
        app,
        namespace,
        Msg::Applied {
            generation: app.generation,
            key: row_key(&obj),
            obj: Box::new(obj),
        },
    );
}

#[tokio::test]
async fn namespace_patterns_keep_independent_relists_and_equal_names() {
    let (mut app, _rx) = pattern_app();
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    assert_eq!(
        app.namespace, "default",
        "discovery must keep the old selection"
    );
    resolve(&mut app, &["a-crons", "b-crons"]);
    assert_eq!(app.namespace, "*-crons");
    assert!(app.show_namespace_column());
    assert_eq!(app.namespace_label(), "*-crons (2 namespaces)");
    for ns in ["a-crons", "b-crons"] {
        let generation = app.generation;
        watch(&mut app, ns, Msg::Reset { generation });
        pod(&mut app, ns, "same");
        watch(&mut app, ns, Msg::Synced { generation });
        assert_eq!(app.store.synced, ns == "b-crons");
    }
    assert_eq!(app.store.len(), 2);
    let generation = app.generation;
    watch(&mut app, "a-crons", Msg::Reset { generation });
    pod(&mut app, "b-crons", "live");
    assert_eq!(app.store.len(), 3);
    pod(&mut app, "a-crons", "new");
    watch(&mut app, "a-crons", Msg::Synced { generation });
    assert!(app.store.get("a-crons/same").is_none());
    assert!(app.store.get("a-crons/new").is_some());
    assert!(app.store.get("b-crons/same").is_some());
    assert!(app.store.get("b-crons/live").is_some());
    watch(
        &mut app,
        "b-crons",
        Msg::Error {
            generation,
            error: "forbidden".into(),
        },
    );
    assert!(app.namespace_label().contains("incomplete"));
    assert!(app.flash.contains("b-crons"));
    watch(&mut app, "b-crons", Msg::Reset { generation });
    watch(&mut app, "b-crons", Msg::Synced { generation });
    assert!(!app.namespace_label().contains("incomplete"));
    assert_eq!(app.store.len(), 1);
}

#[tokio::test]
async fn namespace_patterns_keep_selection_on_discovery_failure_and_drop_stale_results() {
    let (mut app, _rx) = pattern_app();
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    resolve(&mut app, &[]);
    assert_eq!(app.namespace, "default");
    assert!(app.flash.contains("no namespaces match"));
    type_resource_query(&mut app, "ns *-crons");
    let request = app.namespace_request;
    app.handle_msg(Msg::NamespacePattern {
        generation: app.generation,
        request,
        pattern: "*-crons".into(),
        action: NamespacePatternAction::Select,
        result: Err("forbidden".into()),
    });
    assert_eq!(app.namespace, "default");
    assert!(app.flash.contains("forbidden"));
    type_resource_query(&mut app, "ns *-crons");
    let request = app.namespace_request;
    let generation = app.generation;
    type_resource_query(&mut app, "ns other");
    app.handle_msg(Msg::NamespacePattern {
        generation,
        request,
        pattern: "*-crons".into(),
        action: NamespacePatternAction::Select,
        result: Ok(vec!["a-crons".into()]),
    });
    assert_eq!(app.namespace, "other");
    app.handle_msg(Msg::NamespaceWatch {
        generation,
        namespace: "a-crons".into(),
        event: Box::new(Msg::Error {
            generation,
            error: "stale".into(),
        }),
    });
    assert!(!app.flash.contains("stale"));
}

#[tokio::test]
async fn namespace_patterns_preserve_navigation_and_refresh_membership() {
    let (mut app, _rx) = pattern_app();
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    resolve(&mut app, &["a-crons", "b-crons"]);
    let generation = app.generation;
    for ns in ["a-crons", "b-crons"] {
        watch(&mut app, ns, Msg::Reset { generation });
        pod(&mut app, ns, "same");
        watch(&mut app, ns, Msg::Synced { generation });
    }
    let handles: Vec<_> = app.tasks.iter().map(JoinHandle::abort_handle).collect();
    type_resource_query(&mut app, "nodes");
    tokio::task::yield_now().await;
    assert!(handles.iter().all(|handle| handle.is_finished()));
    assert_eq!(app.namespace, "*-crons");
    assert!(!app.show_namespace_column());
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.store.len(), 2, "history should restore cached rows");
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    resolve(&mut app, &["a-crons"]);
    assert_eq!(app.namespace_label(), "*-crons (1 namespaces)");
    assert!(
        app.store.is_empty(),
        "changed membership must use a different cache key"
    );
    type_resource_query(&mut app, "ns *");
    assert!(app.all_namespaces());
    assert!(!app.namespace_is_pattern());
}

#[tokio::test]
async fn namespace_patterns_object_actions_keep_actual_namespace() {
    let (mut app, _rx) = pattern_app();
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    resolve(&mut app, &["a-crons", "b-crons"]);
    pod(&mut app, "b-crons", "same");
    app.table_state.select(Some(0));
    assert_eq!(
        app.action_targets(),
        vec![("same".into(), "b-crons".into())]
    );
    type_resource_query(&mut app, "can-i");
    assert!(app.flash.contains("select one namespace"));
    type_resource_query(&mut app, "pvc-clean");
    assert!(app.flash.contains("select one namespace"));
}

fn pattern_api(
    names: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> (Cluster, mpsc::UnboundedReceiver<http::Uri>) {
    use futures_util::stream;
    use hyper::body::{Bytes, Frame};
    use std::convert::Infallible;
    let (seen, requests) = mpsc::unbounded_channel();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let uri = request.uri().clone();
        seen.send(uri.clone()).ok();
        let names = names.clone();
        async move {
            let query: HashMap<_, _> = form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                .into_owned()
                .collect();
            let watch = query.get("watch").is_some_and(|s| s == "true");
            let namespace = uri
                .path()
                .strip_prefix("/api/v1/namespaces/")
                .and_then(|tail| tail.strip_suffix("/pods"));
            let metrics_ns = uri
                .path()
                .strip_prefix("/apis/metrics.k8s.io/v1/namespaces/")
                .and_then(|tail| tail.strip_suffix("/pods"));
            let (status, body) = if let Some(ns) = metrics_ns {
                (200, json!({"apiVersion":"metrics.k8s.io/v1", "kind":"PodMetricsList", "items":[{"apiVersion":"metrics.k8s.io/v1", "kind":"PodMetrics", "metadata":{"name":"same", "namespace":ns}, "containers":[{"name":"app", "usage":{"cpu":"10m", "memory":"20Mi"}}]}]}).to_string())
            } else if uri.path() == "/api/v1/namespaces" {
                (200, json!({"apiVersion":"v1", "kind":"NamespaceList", "metadata":{"resourceVersion":"10"}, "items": names.lock().unwrap().iter().map(|name| json!({"apiVersion":"v1", "kind":"Namespace", "metadata":{"name": name}})).collect::<Vec<_>>()}).to_string())
            } else if let Some(ns) = namespace.filter(|ns| *ns != "denied-crons") {
                let pod = json!({"apiVersion":"v1", "kind":"Pod", "metadata":{"name":"same", "namespace":ns,"resourceVersion":"10"}});
                if query.contains_key("sendInitialEvents") {
                    (
                        200,
                        format!(
                            "{}\n{}\n",
                            json!({"type":"ADDED", "object":pod}),
                            json!({"type":"BOOKMARK", "object":{"apiVersion":"v1","kind":"Pod","metadata":{"resourceVersion":"10","annotations":{"k8s.io/initial-events-end":"true"}}}})
                        ),
                    )
                } else if watch {
                    (200, String::new())
                } else {
                    (200, json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"10"},"items":[pod]}).to_string())
                }
            } else {
                (403, json!({"apiVersion":"v1", "kind":"Status", "status":"Failure", "reason":"Forbidden", "message":"forbidden", "code":403}).to_string())
            };
            let frames = stream::iter([Ok::<_, Infallible>(Frame::data(Bytes::from(body)))]);
            let frames = if watch && status == 200 {
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
        }
    });
    let mut cluster = Cluster::fake();
    cluster.client = kube::Client::new(service, "default");
    cluster.add_aliases(&HashMap::from([("ns".into(), "namespaces".into())]));
    (cluster, requests)
}

async fn drain_until(app: &mut App, rx: &mut Receiver<Msg>, done: impl Fn(&App) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done(app) {
            app.handle_msg(rx.recv().await.unwrap());
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "namespace timeout: ns={} flash={} synced={} rows={} errors={:?}",
            app.namespace,
            app.flash,
            app.store.synced,
            app.store.len(),
            app.namespace_errors
        )
    });
}

#[tokio::test]
async fn namespace_patterns_api_watches_only_matches_and_refreshes_discovery() {
    let names = std::sync::Arc::new(std::sync::Mutex::new(vec![
        "a-crons".into(),
        "b-crons".into(),
        "other".into(),
    ]));
    let (cluster, mut requests) = pattern_api(names.clone());
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(cluster, tx);
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    drain_until(&mut app, &mut rx, |a| {
        a.namespace_is_pattern() && a.store.synced
    })
    .await;
    assert_eq!(app.store.len(), 2);
    let mut paths = Vec::new();
    while let Ok(uri) = requests.try_recv() {
        paths.push(uri.path().to_string());
    }
    assert!(paths.iter().any(|p| p == "/api/v1/namespaces/a-crons/pods"));
    assert!(paths.iter().any(|p| p == "/api/v1/namespaces/b-crons/pods"));
    assert!(
        !paths.iter().any(|p| p == "/api/v1/pods"
            || p == "/api/v1/namespaces/other/pods"
            || p.contains('*'))
    );
    *names.lock().unwrap() = vec!["b-crons".into(), "c-crons".into()];
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    drain_until(&mut app, &mut rx, |a| {
        a.store.synced && a.store.get("c-crons/same").is_some()
    })
    .await;
    assert_eq!(app.store.len(), 2);
    assert!(app.store.get("a-crons/same").is_none());
    type_resource_query(&mut app, "pods -n ?-crons /same");
    drain_until(&mut app, &mut rx, |a| {
        a.namespace == "?-crons" && a.store.synced
    })
    .await;
    assert_eq!(app.filter, "same");
    assert_eq!(app.store.len(), 2);
}

#[tokio::test]
async fn namespace_patterns_api_reports_partial_access() {
    let names = std::sync::Arc::new(std::sync::Mutex::new(vec![
        "a-crons".into(),
        "denied-crons".into(),
    ]));
    let (cluster, _) = pattern_api(names);
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(cluster, tx);
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    drain_until(&mut app, &mut rx, |a| {
        a.namespace_errors.contains_key("denied-crons") && a.store.len() == 1
    })
    .await;
    assert!(app.namespace_label().contains("incomplete"));
    assert!(!app.store.synced);
    assert!(app.store.get("a-crons/same").is_some());
}

#[tokio::test]
async fn namespace_patterns_api_metrics_stay_in_selected_namespaces() {
    let names = std::sync::Arc::new(std::sync::Mutex::new(vec![
        "a-crons".into(),
        "b-crons".into(),
        "other".into(),
    ]));
    let (mut cluster, mut requests) = pattern_api(names);
    cluster.register_kind("metrics.k8s.io", "PodMetrics", "pods", true);
    cluster.register_kind("", "Pod", "pods", true);
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(cluster, tx);
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    drain_until(&mut app, &mut rx, |a| {
        a.namespace_is_pattern() && a.metrics.len() == 2
    })
    .await;
    assert_eq!(app.metrics["a-crons/same"], (10, 20 * 1024 * 1024));
    assert!(app.metrics.contains_key("b-crons/same"));
    let mut paths = Vec::new();
    while let Ok(uri) = requests.try_recv() {
        paths.push(uri.path().to_string());
    }
    assert!(
        paths
            .iter()
            .any(|p| p == "/apis/metrics.k8s.io/v1/namespaces/a-crons/pods")
    );
    assert!(
        paths
            .iter()
            .any(|p| p == "/apis/metrics.k8s.io/v1/namespaces/b-crons/pods")
    );
    assert!(
        !paths
            .iter()
            .any(|p| p == "/apis/metrics.k8s.io/v1/pods" || p.contains("/other/"))
    );
}

#[tokio::test]
async fn namespace_patterns_return_from_namespace_list_and_cancel_older_request() {
    let (mut app, _rx) = pattern_app();
    type_resource_query(&mut app, "namespaces");
    type_resource_query(&mut app, "ns *-crons");
    let older = app.namespace_request;
    type_resource_query(&mut app, "ns *-crons");
    app.handle_msg(Msg::NamespacePattern {
        generation: app.generation,
        request: older,
        pattern: "*-crons".into(),
        action: NamespacePatternAction::Select,
        result: Ok(vec!["old-crons".into()]),
    });
    assert_eq!(app.namespace, "default");
    assert_eq!(app.kind_plural, "namespaces");
    resolve(&mut app, &["a-crons"]);
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.namespace, "*-crons");
    assert_eq!(app.watch_namespaces(), vec!["a-crons"]);
}

#[tokio::test]
async fn namespace_patterns_failed_restore_keeps_pattern_without_fallback() {
    for result in [Ok(vec![]), Err("namespace listing forbidden".into())] {
        let (mut app, _rx) = pattern_app();
        app.namespace = "*-crons".into();
        type_resource_query(&mut app, "pods");
        let generation = app.generation;
        let task_count = app.tasks.len();
        app.handle_msg(Msg::NamespacePattern {
            generation,
            request: app.namespace_request,
            pattern: "*-crons".into(),
            action: NamespacePatternAction::Resume,
            result,
        });
        assert_eq!(app.namespace, "*-crons");
        assert_eq!(app.namespace_label(), "*-crons (unresolved)");
        assert!(app.watch_namespaces().is_empty());
        assert!(app.store.is_empty());
        assert!(app.watch_key.is_none());
        assert_eq!(
            app.generation, generation,
            "failure must not start a fallback watch"
        );
        assert_eq!(app.tasks.len(), task_count);
        assert!(
            app.flash
                .contains("pattern unresolved; no resources loaded")
        );
        app.handle_key(ctrl(KeyCode::Char('r'))).unwrap();
        resolve(&mut app, &["a-crons"]);
        assert_eq!(app.watch_namespaces(), vec!["a-crons"]);
    }
}

#[tokio::test]
async fn namespace_patterns_api_failed_restore_never_loads_default_namespace() {
    let names = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (cluster, mut requests) = pattern_api(names.clone());
    let (tx, mut rx) = mpsc::channel(1024);
    let mut app = App::new(cluster, tx);
    app.namespace = "*-crons".into();
    type_resource_query(&mut app, "pods");
    drain_until(&mut app, &mut rx, |a| {
        a.flash.contains("pattern unresolved; no resources loaded")
    })
    .await;
    assert_eq!(app.namespace, "*-crons");
    assert!(app.store.is_empty());
    while let Ok(uri) = requests.try_recv() {
        assert_eq!(uri.path(), "/api/v1/namespaces");
    }
    *names.lock().unwrap() = vec!["a-crons".into()];
    app.handle_key(ctrl(KeyCode::Char('r'))).unwrap();
    drain_until(&mut app, &mut rx, |a| a.store.synced).await;
    assert!(app.store.get("a-crons/same").is_some());
    assert!(app.store.get("default/same").is_none());
}

#[tokio::test]
async fn namespace_patterns_relist_removes_cached_and_live_keys_only_in_its_namespace() {
    let (mut app, _rx) = pattern_app();
    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "ns *-crons");
    resolve(&mut app, &["a-crons", "b-crons"]);
    let generation = app.generation;
    for ns in ["a-crons", "b-crons"] {
        watch(&mut app, ns, Msg::Reset { generation });
        pod(&mut app, ns, "cached");
        watch(&mut app, ns, Msg::Synced { generation });
    }
    type_resource_query(&mut app, "nodes");
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(app.store.len(), 2);
    let generation = app.generation;
    watch(&mut app, "a-crons", Msg::Reset { generation });
    watch(&mut app, "a-crons", Msg::Synced { generation });
    assert!(app.store.get("a-crons/cached").is_none());
    assert!(app.store.get("b-crons/cached").is_some());
    pod(&mut app, "a-crons", "live");
    pod(&mut app, "a-crons", "deleted");
    watch(
        &mut app,
        "a-crons",
        Msg::Deleted {
            generation,
            key: "a-crons/deleted".into(),
        },
    );
    watch(&mut app, "a-crons", Msg::Reset { generation });
    watch(&mut app, "a-crons", Msg::Synced { generation });
    assert_eq!(app.store.len(), 1);
    assert!(app.store.get("b-crons/cached").is_some());
}
