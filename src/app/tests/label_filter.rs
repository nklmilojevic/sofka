use super::*;

#[tokio::test]
async fn label_filter_matches_hidden_keys_and_values_with_each_pattern_mode() {
    let (mut app, _rx) = test_app();
    type_resource_query(&mut app, "pods");
    for (name, labels) in [
        (
            "alpha",
            json!({"test.example.com/type": "example100", "release": "Stable"}),
        ),
        (
            "beta",
            json!({"owner": "e_x_a_m_p_l_e_1_0_0", "release": "canary"}),
        ),
        ("delta", json!({"empty": ""})),
        ("gamma", json!(null)),
    ] {
        apply(
            &mut app,
            json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": name, "namespace": "default", "labels": labels,
                    "annotations": {"type": "example100"}},
                "spec": {"template": {"metadata": {"labels": {"type": "example100"}}}},
                "status": {"phase": "Running"}
            }),
        );
    }
    let generation = app.generation;
    let cases: &[(&str, &[&str])] = &[
        ("example100", &[]),
        ("label:example100", &["alpha"]),
        ("label:xmpl", &[]),
        ("label:test", &["alpha"]),
        ("label:stable", &["alpha"]),
        ("label:Stable", &["alpha"]),
        ("label:STABLE", &["alpha"]),
        ("label:\"EXAMPLE100\"", &["alpha"]),
        (r"label:/^EXAMPLE[0-9]+$/", &["alpha"]),
        (r"label:/test.example.com\/type/", &["alpha"]),
        ("!label:canary", &["alpha", "delta", "gamma"]),
        ("label:/^$/", &["delta"]),
        ("!label:/^$/", &["alpha", "beta", "gamma"]),
        ("label:Running", &[]),
        ("label:gamma", &[]),
    ];
    for &(filter, expected) in cases {
        retype_filter(&mut app, filter);
        assert_eq!(app.mode, Mode::Table, "{filter}");
        assert_eq!(row_names(&app), expected, "{filter}");
        assert_eq!(app.generation, generation);
        assert_eq!(app.filter_location(), " ·local");
    }
}

#[tokio::test]
async fn label_filter_matches_contiguous_text_across_many_node_labels() {
    let (mut app, _rx) = test_app();
    type_resource_query(&mut app, "nodes");
    for (name, pool, feature) in [
        ("gpu-1", "vllm-gpu", "ssd"),
        ("cpu-1", "general", "ups"),
        ("cpu-2", "general", "ssd"),
    ] {
        apply(
            &mut app,
            json!({
                "apiVersion": "v1", "kind": "Node",
                "metadata": {"name": name, "labels": {
                    "karpenter.sh/nodepool": pool,
                    "feature": feature,
                    "kubernetes.io/hostname": name,
                    "feature.node.kubernetes.io/usb-ff_0bda_8156.present": "true",
                    "topology.kubernetes.io/zone": "us-east-1a",
                }},
            }),
        );
    }
    let cases: &[(&str, &[&str])] = &[
        ("label:vllm", &["gpu-1"]),
        ("label:vllm-gpu", &["gpu-1"]),
        ("label:VLLM", &["gpu-1"]),
        ("label:ups", &["cpu-1"]),
        ("label:nodepool", &["cpu-1", "cpu-2", "gpu-1"]),
        ("!label:ups", &["cpu-2", "gpu-1"]),
    ];
    for &(filter, expected) in cases {
        retype_filter(&mut app, filter);
        assert_eq!(row_names(&app), expected, "{filter}");
    }
}

#[tokio::test]
async fn label_filter_keeps_boundaries_and_combines_local_terms() {
    let (mut app, _rx) = test_app();
    type_resource_query(&mut app, "pods");
    for (name, labels, phase) in [
        ("alpha", json!({"ab": "cd", "ef": "gh"}), "Running"),
        ("beta", json!({"ab": "cd", "role": "worker"}), "Pending"),
        ("gamma", json!({"ab": "canary", "ef": "gh"}), "Running"),
    ] {
        apply(
            &mut app,
            json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": name, "namespace": "default", "labels": labels},
                "status": {"phase": phase}
            }),
        );
    }
    let cases: &[(&str, &[&str])] = &[
        ("label:abcd", &[]),
        ("label:cdgh", &[]),
        ("label:abef", &[]),
        ("label:cd label:gh", &["alpha"]),
        (
            "(label:cd || label:worker) && !label:canary",
            &["alpha", "beta"],
        ),
        ("label:cd status=Running", &["alpha"]),
        ("!(label:cd || label:worker)", &["gamma"]),
    ];
    for &(filter, expected) in cases {
        retype_filter(&mut app, filter);
        assert_eq!(row_names(&app), expected, "{filter}");
    }
    retype_filter(&mut app, "label:cd");
    assert_eq!(app.filter_match_indices("cd"), None);
    retype_filter(&mut app, "label:cd alpha");
    assert_eq!(row_names(&app), ["alpha"]);
    assert_eq!(app.filter_match_indices("alpha").unwrap().len(), 5);
}

#[tokio::test]
async fn label_filter_updates_live_for_custom_resources_and_clears_with_escape() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.io", "Widget", "widgets", true);
    type_resource_query(&mut app, "widgets.example.io");
    let mut widget = json!({
        "apiVersion": "example.io/v1", "kind": "Widget",
        "metadata": {"name": "alpha", "namespace": "default", "resourceVersion": "1",
            "labels": {"test.example.com/type": "example100"}}
    });
    apply(&mut app, widget.clone());
    let generation = app.generation;
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "label:example100".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    assert_eq!(app.mode, Mode::Filter);
    assert_eq!(row_names(&app), ["alpha"]);
    widget["metadata"]["resourceVersion"] = json!("2");
    widget["metadata"]["labels"] = json!({"type": "other"});
    apply(&mut app, widget.clone());
    assert!(app.rows().is_empty());
    widget["metadata"]["resourceVersion"] = json!("3");
    widget["metadata"]["labels"] = json!({"type": "example100"});
    apply(&mut app, widget);
    assert_eq!(row_names(&app), ["alpha"]);
    app.handle_key(press(KeyCode::Char('z'))).unwrap();
    assert!(app.rows().is_empty());
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    assert_eq!(row_names(&app), ["alpha"]);
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.filter.is_empty());
    assert_eq!(app.mode, Mode::Table);
    type_filter(&mut app, "label:missing");
    assert!(app.rows().is_empty());
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(row_names(&app), ["alpha"]);
    assert_eq!(app.generation, generation);
}

#[tokio::test]
async fn invalid_label_filters_keep_the_editor_open() {
    for filter in [
        "label:",
        "!label:",
        "label: api",
        "label:\"\"",
        "label://",
        "label:/[/",
    ] {
        let (mut app, _rx) = test_app();
        type_resource_query(&mut app, "pods");
        let generation = app.generation;
        type_filter(&mut app, filter);
        assert_eq!(app.mode, Mode::Filter, "{filter}");
        assert!(app.filter_error().is_some(), "{filter}");
        assert!(app.flash_err);
        assert_eq!(app.generation, generation);
    }
}

#[tokio::test]
async fn label_filter_preserves_api_selectors_without_new_requests_on_local_edits() {
    for streaming in [true, false] {
        let (cluster, mut requests) = selector_api(streaming);
        let (tx, mut rx) = mpsc::channel(1024);
        let mut app = App::new(cluster, tx);
        type_resource_query(
            &mut app,
            "pods -n prod /-l env -f spec.nodeName=node-3 label:api",
        );
        sync_selector_view(&mut app, &mut rx).await;
        assert_eq!(row_names(&app), ["api"]);
        assert_eq!(app.filter_location(), " ·server+local");
        for _ in 0..if streaming { 1 } else { 3 } {
            let uri = next_selector_request(&mut requests).await;
            let query: HashMap<_, _> = form_urlencoded::parse(uri.query().unwrap().as_bytes())
                .into_owned()
                .collect();
            assert_eq!(uri.path(), "/api/v1/namespaces/prod/pods");
            assert_eq!(query.get("labelSelector").map(String::as_str), Some("env"));
            assert_eq!(
                query.get("fieldSelector").map(String::as_str),
                Some("spec.nodeName=node-3")
            );
        }
        while requests.try_recv().is_ok() {}
        let generation = app.generation;
        for (pattern, expected) in [("missing", vec![]), ("api", vec!["api"])] {
            retype_filter(
                &mut app,
                &format!("-l env -f spec.nodeName=node-3 label:{pattern}"),
            );
            assert_eq!(row_names(&app), expected);
            assert_eq!(app.generation, generation);
        }
        tokio::task::yield_now().await;
        assert!(requests.try_recv().is_err());
        retype_filter(&mut app, "-l env -f spec.nodeName=node-3");
        assert_eq!(app.filter_location(), " ·server");
        assert_eq!(row_names(&app), ["api"]);
        assert_eq!(app.generation, generation);
    }
}

#[tokio::test]
async fn saved_label_filters_apply_from_bookmark_and_workspace_keys() {
    for workspace in [false, true] {
        let (mut app, _rx) = test_app();
        let view = crate::config::WorkspaceView {
            name: "API pods".into(),
            resource: "pods".into(),
            namespace: Some("prod".into()),
            filter: Some("label:example100".into()),
            sort: Some("NAME:desc".into()),
            ..Default::default()
        };
        let key = if workspace {
            app.workspaces = vec![crate::config::Workspace {
                key: Some("ctrl-w".into()),
                name: "ops".into(),
                views: vec![view],
                ..Default::default()
            }];
            ctrl(KeyCode::Char('w'))
        } else {
            let mut bookmark = view.as_bookmark();
            bookmark.key = Some("z".into());
            app.bookmarks = vec![bookmark];
            press(KeyCode::Char('z'))
        };
        app.handle_key(key).unwrap();
        assert_eq!(app.namespace, "prod");
        assert_eq!(app.filter, "label:example100");
        assert_eq!(app.filter_location(), " ·local");
        for (name, value) in [
            ("alpha", "example100"),
            ("beta", "other"),
            ("gamma", "example100"),
        ] {
            apply(
                &mut app,
                json!({
                    "apiVersion": "v1", "kind": "Pod",
                    "metadata": {"name": name, "namespace": "prod", "labels": {"type": value}}
                }),
            );
        }
        assert_eq!(row_names(&app), ["gamma", "alpha"]);
    }
}

#[tokio::test]
async fn label_filter_follows_namespace_refresh_history_and_drill_rules() {
    let (mut app, _rx) = test_app();
    let filter = "-l env=prod label:example100";
    type_resource_query(&mut app, &format!("deployments -n prod /{filter}"));
    app.ns_list = vec!["<all>".into(), "prod".into(), "qa".into()];
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    for c in "qa".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.namespace, "qa");
    assert_eq!(app.filter, filter);
    app.handle_key(ctrl(KeyCode::Char('r'))).unwrap();
    assert_eq!(app.filter, filter);
    app.handle_key(press(KeyCode::Char('0'))).unwrap();
    assert!(app.all_namespaces());
    assert_eq!(app.filter, filter);
    apply(
        &mut app,
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "qa", "labels": {"type": "example100", "env": "prod"}},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}
        }),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.filter, "-l 'env=prod'");
    assert_eq!(
        app.watch_key.as_ref().unwrap().labels.as_deref(),
        Some("app=web,env=prod")
    );
    app.handle_key(press(KeyCode::Esc)).unwrap();
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.filter, filter);
    type_resource_query(&mut app, "services");
    assert!(app.filter.is_empty());
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.filter, filter);
    app.handle_key(press(KeyCode::Char(']'))).unwrap();
    assert_eq!(app.kind_plural, "services");
    assert!(app.filter.is_empty());
}
