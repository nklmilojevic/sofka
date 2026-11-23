use super::*;

fn row_names(app: &App) -> Vec<String> {
    app.rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap_or_default())
        .collect()
}

fn cell(app: &App, name: &str, field: &str) -> String {
    app.rows()
        .iter()
        .find(|o| o.metadata.name.as_deref() == Some(name))
        .and_then(|o| o.data[field].as_str())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn ctrl_a_and_palette_commands_open_api_resources_browser() {
    let (mut app, _rx) = test_app();
    app.cluster.apply_discovery(crate::k8s::Discovery::from_aggregated(json!({
        "items": [
            {
                "metadata": {"name": ""},
                "versions": [{
                    "version": "v1",
                    "resources": [
                        {"resource": "pods", "responseKind": {"kind": "Pod"}, "scope": "Namespaced", "verbs": ["get", "list", "watch"], "shortNames": ["po"]},
                        {"resource": "nodes", "responseKind": {"kind": "Node"}, "scope": "Cluster", "verbs": ["get", "list", "watch"], "shortNames": ["no"]}
                    ]
                }]
            },
            {
                "metadata": {"name": "apps"},
                "versions": [{
                    "version": "v1",
                    "resources": [
                        {
                            "resource": "deployments",
                            "responseKind": {"kind": "Deployment"},
                            "scope": "Namespaced",
                            "verbs": ["get", "list", "watch"],
                            "shortNames": ["deploy"],
                            "subresources": [{"subresource": "scale", "verbs": ["get", "patch"]}]
                        }
                    ]
                }]
            },
            {
                "metadata": {"name": "authentication.k8s.io"},
                "versions": [{
                    "version": "v1",
                    "resources": [
                        {"resource": "tokenreviews", "responseKind": {"kind": "TokenReview"}, "scope": "Cluster", "verbs": ["create"]}
                    ]
                }]
            }
        ]
    })));
    app.user_aliases
        .insert("mydep".into(), "deployments".into());
    app.cluster.add_aliases(&app.user_aliases);

    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());
    assert_eq!(app.list_title(), "api-resources");
    assert!(app.store.synced);
    assert_eq!(
        app.display_headers()[..],
        ["NAME", "ALIASES", "APIVERSION", "SCOPE", "KIND"]
    );
    assert_eq!(row_names(&app), ["deployments.apps", "nodes", "pods"]);
    assert_eq!(cell(&app, "deployments.apps", "plural"), "deployments");
    assert_eq!(cell(&app, "deployments.apps", "aliases"), "deploy,dp,mydep");
    assert_eq!(cell(&app, "deployments.apps", "apiVersion"), "apps/v1");
    assert_eq!(cell(&app, "deployments.apps", "scope"), "Namespaced");
    assert_eq!(cell(&app, "deployments.apps", "kind"), "Deployment");
    assert_eq!(cell(&app, "nodes", "scope"), "Cluster");
    assert_eq!(app.flash, "3 API resources");
    assert!(!app.flash_err);
    let rendered = screen_text(&mut app, 120, 20);
    assert!(rendered.contains("○ snapshot"));
    assert!(rendered.contains("deployments.apps"));

    // Wide mode adds SCALABLE.
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_eq!(
        app.display_headers()[..],
        ["NAME", "ALIASES", "APIVERSION", "SCOPE", "KIND", "SCALABLE"]
    );

    // Palette `:aliases` and `:api-resources` open the same browser.
    type_resource_query(&mut app, "pods");
    assert!(!app.api_resources_active());
    type_resource_query(&mut app, "aliases");
    assert!(app.api_resources_active());

    type_resource_query(&mut app, "pods");
    type_resource_query(&mut app, "api-resources");
    assert!(app.api_resources_active());
}

#[tokio::test]
async fn filter_by_alias_or_group_and_enter_opens_exact_resource() {
    let (mut app, _rx) = test_app();
    app.namespace = "prod".into();
    app.cluster.register_discovered_kind(
        "cert-manager.io",
        "v1",
        "Certificate",
        "certificates",
        &["cert", "certs"],
    );

    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());

    // Filter by CRD short name `certs`.
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "certs".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(row_names(&app), ["certificates.cert-manager.io"]);

    // Enter drills into the CRD in the current namespace; Esc returns.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "certificates");
    let ar = &app.kind.as_ref().unwrap().ar;
    assert_eq!(
        (ar.group.as_str(), ar.api_version.as_str()),
        ("cert-manager.io", "cert-manager.io/v1")
    );
    assert_eq!(app.namespace, "prod");

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.api_resources_active());

    // Filter to colliding `events.k8s.io/v1` events and drill into that exact group.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "events.k8s.io".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(row_names(&app), ["events.events.k8s.io"]);

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "events");
    assert_eq!(app.kind.as_ref().unwrap().ar.group, "events.k8s.io");
}

#[tokio::test]
async fn refresh_updates_discovery_and_keeps_rows_on_error() {
    let (mut app, _rx) = test_app();
    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());
    let initial_count = app.rows().len();
    assert!(initial_count > 0);

    // Pressing `r` begins an async refresh while keeping existing rows visible.
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(!app.store.synced);
    assert_eq!(app.rows().len(), initial_count);
    let claim = current_claim(&app);
    let request = app.rediscovery_request;

    // A failure restores synced state and preserves existing rows.
    app.handle_msg(Msg::ApiResourcesRefreshed {
        epoch: app.context_epoch,
        generation: app.generation,
        request,
        claim,
        result: Err("connection refused".into()),
    });
    assert!(app.store.synced);
    assert_eq!(app.rows().len(), initial_count);
    assert!(app.flash_err);
    assert!(app.flash.contains("connection refused"), "{}", app.flash);

    // Refresh again and receive a discovery with a new CRD.
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    let claim = current_claim(&app);
    let request = app.rediscovery_request;
    let fresh = crate::k8s::Discovery::from_legacy(vec![json!({
        "groupVersion": "example.io/v1alpha1",
        "resources": [
            {"name": "widgets", "kind": "Widget", "namespaced": true, "verbs": ["get", "list"], "shortNames": ["wg"]}
        ]
    })]);
    app.handle_msg(Msg::ApiResourcesRefreshed {
        epoch: app.context_epoch,
        generation: app.generation,
        request,
        claim,
        result: Ok(Box::new(fresh)),
    });
    assert!(app.store.synced);
    assert_eq!(row_names(&app), ["widgets.example.io"]);
    assert_eq!(cell(&app, "widgets.example.io", "aliases"), "wg");
    assert_eq!(app.flash, "1 API resource");
    assert!(!app.flash_err);
}

#[tokio::test]
async fn overlapping_refreshes_drop_superseded_responses_before_applying_discovery() {
    let (mut app, _rx) = test_app();
    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());

    // Trigger two overlapping refreshes.
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    let first_claim = current_claim(&app);
    let first_request = app.rediscovery_request;

    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    let second_claim = current_claim(&app);
    let second_request = app.rediscovery_request;
    assert!(second_request > first_request);

    // The newer response arrives first and updates discovery and rows.
    let newer = crate::k8s::Discovery::from_legacy(vec![json!({
        "groupVersion": "newer.io/v1",
        "resources": [
            {"name": "gadgets", "kind": "Gadget", "namespaced": true, "verbs": ["get", "list"], "shortNames": ["gd"]}
        ]
    })]);
    app.handle_msg(Msg::ApiResourcesRefreshed {
        epoch: app.context_epoch,
        generation: app.generation,
        request: second_request,
        claim: second_claim,
        result: Ok(Box::new(newer)),
    });
    assert!(app.store.synced);
    assert_eq!(row_names(&app), ["gadgets.newer.io"]);
    assert!(app.cluster.resolve("gadgets").is_some());

    // The slower first response arrives later and must be ignored.
    let older = crate::k8s::Discovery::from_legacy(vec![json!({
        "groupVersion": "older.io/v1",
        "resources": [
            {"name": "widgets", "kind": "Widget", "namespaced": true, "verbs": ["get", "list"]}
        ]
    })]);
    app.handle_msg(Msg::ApiResourcesRefreshed {
        epoch: app.context_epoch,
        generation: app.generation,
        request: first_request,
        claim: first_claim,
        result: Ok(Box::new(older)),
    });
    assert_eq!(row_names(&app), ["gadgets.newer.io"]);
    assert!(app.cluster.resolve("gadgets").is_some());
    assert!(app.cluster.resolve("widgets").is_none());
}

#[tokio::test]
async fn keys_commands_and_object_plugins_are_blocked_on_kind_rows() {
    let (mut app, _rx) = test_app();
    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());

    for key in [ctrl(KeyCode::Char('d')), press(KeyCode::Char('y'))] {
        app.handle_key(key).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.api_resources_active());
        assert!(app.flash.contains("⏎ opens one"), "{}", app.flash);
    }

    type_resource_query(&mut app, "xray");
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("⏎ opens one"), "{}", app.flash);

    app.plugins = vec![
        crate::config::Plugin {
            key: "ctrl-g".into(),
            name: "objects".into(),
            command: "true".into(),
            palette: Some("objects".into()),
            ..Default::default()
        },
        crate::config::Plugin {
            key: "ctrl-o".into(),
            name: "context".into(),
            command: "true".into(),
            target: Some("context".into()),
            ..Default::default()
        },
    ];

    app.handle_key(ctrl(KeyCode::Char('g'))).unwrap();
    assert!(app.pending.is_none());
    assert!(
        app.flash.contains("api-resources rows are kinds"),
        "{}",
        app.flash
    );

    type_resource_query(&mut app, "objects");
    assert!(app.pending.is_none());
    assert!(
        app.flash.contains("api-resources rows are kinds"),
        "{}",
        app.flash
    );

    app.handle_key(ctrl(KeyCode::Char('o'))).unwrap();
    assert!(matches!(app.pending, Some(Suspend::Shell(_))));
}

#[tokio::test]
async fn view_history_returns_to_api_resources() {
    let (mut app, _rx) = test_app();
    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());

    type_resource_query(&mut app, "pods");
    assert_eq!(app.kind_plural, "pods");

    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert!(app.api_resources_active());
    assert!(app.flash.contains("api-resources"), "{}", app.flash);
}

#[tokio::test]
async fn configured_ctrl_a_binding_takes_precedence_over_api_resources() {
    let (mut app, _rx) = test_app();
    app.bookmarks = vec![crate::config::Bookmark {
        name: "Nodes".into(),
        key: Some("ctrl-a".into()),
        resource: "nodes".into(),
        ..Default::default()
    }];

    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(!app.api_resources_active());
    assert_eq!(app.kind_plural, "nodes");
}

#[tokio::test]
async fn custom_resource_sharing_plural_keeps_its_own_view() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.com", "ApiResource", "api-resources", true);
    app.switch_kind("api-resources.example.com");
    assert_eq!(app.kind_plural, "api-resources");
    assert!(!app.api_resources_active());

    apply(
        &mut app,
        json!({"apiVersion": "example.com/v1", "kind": "ApiResource",
               "metadata": {"name": "one", "namespace": "default"}}),
    );
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert!(!app.flash.contains("⏎ opens one"), "{}", app.flash);
    assert_ne!(app.mode, Mode::Table);
}

#[tokio::test]
async fn history_distinguishes_browser_from_colliding_crd_in_both_directions() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.com", "ApiResource", "api-resources", true);

    // Open the synthetic browser via Ctrl+A, then switch to the colliding CRD.
    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());

    type_resource_query(&mut app, "api-resources.example.com");
    assert!(!app.api_resources_active());
    assert_eq!(app.kind_plural, "api-resources");
    assert_eq!(app.kind.as_ref().unwrap().ar.group, "example.com");

    // `[` returns to the synthetic browser (not the colliding CRD).
    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert!(app.api_resources_active());

    // `]` returns to the colliding CRD with its exact API group.
    app.handle_key(press(KeyCode::Char(']'))).unwrap();
    assert!(!app.api_resources_active());
    assert_eq!(app.kind_plural, "api-resources");
    assert_eq!(app.kind.as_ref().unwrap().ar.group, "example.com");
}

#[tokio::test]
async fn colliding_plurals_render_distinct_qualified_names_in_name_column() {
    let (mut app, _rx) = test_app();
    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "events".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(row_names(&app), ["events", "events.events.k8s.io"]);

    let rendered = screen_text(&mut app, 120, 20);
    assert!(rendered.contains("events.events.k8s.io"));
}

#[tokio::test]
async fn same_group_and_plural_across_api_versions_stay_separate_rows() {
    let (mut app, _rx) = test_app();
    app.cluster.apply_discovery(crate::k8s::Discovery::from_aggregated(json!({
        "items": [
            {
                "metadata": {"name": "example.io"},
                "versions": [
                    {
                        "version": "v1alpha1",
                        "resources": [
                            {"resource": "widgets", "responseKind": {"kind": "LegacyWidget"}, "scope": "Namespaced", "verbs": ["get", "list"]}
                        ]
                    },
                    {
                        "version": "v1",
                        "resources": [
                            {"resource": "widgets", "responseKind": {"kind": "Widget"}, "scope": "Namespaced", "verbs": ["get", "list"]}
                        ]
                    }
                ]
            }
        ]
    })));

    app.handle_key(ctrl(KeyCode::Char('a'))).unwrap();
    assert!(app.api_resources_active());
    assert_eq!(app.rows().len(), 2);
    assert_eq!(app.flash, "2 API resources");

    let rendered = screen_text(&mut app, 120, 20);
    assert!(rendered.contains("example.io/v1alpha1"));
    assert!(rendered.contains("LegacyWidget"));
    assert!(rendered.contains("example.io/v1"));
    assert!(rendered.contains("Widget"));

    // Enter opens each version's exact kind independently.
    let first_version = app.rows()[0].data["apiVersion"]
        .as_str()
        .unwrap()
        .to_string();
    let second_version = app.rows()[1].data["apiVersion"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first_version, second_version);

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind.as_ref().unwrap().ar.api_version, first_version);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.api_resources_active());
    app.handle_key(press(KeyCode::Char('j'))).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind.as_ref().unwrap().ar.api_version, second_version);
}
