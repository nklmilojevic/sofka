use super::*;
use crate::app::ns_resources::{Count, Update, row};

fn open(app: &mut App, ns: &str) -> crate::store::StatusClaim {
    type_resource_query(app, &format!("ns-resources {ns}"));
    assert!(app.ns_resources_active());
    current_claim(app)
}

fn objects(n: u64) -> Count {
    Count::Objects { n, more: false }
}

/// Feed one count through the path the count task takes.
fn counted(app: &mut App, claim: crate::store::StatusClaim, plural: &str, count: Count) {
    let kind = app.cluster.resolve(plural).unwrap();
    counted_kind(app, claim, &kind, count);
}

fn counted_kind(app: &mut App, claim: crate::store::StatusClaim, kind: &Kind, count: Count) {
    app.handle_msg(Msg::NsResources {
        generation: app.generation,
        claim,
        update: Update::Counted {
            row: row(kind, &count).map(Box::new),
            done: 1,
            total: 4,
        },
    });
}

fn done(app: &mut App, claim: crate::store::StatusClaim) {
    app.handle_msg(Msg::NsResources {
        generation: app.generation,
        claim,
        update: Update::Done { warning: None },
    });
}

fn counts(app: &App) -> Vec<(String, String)> {
    app.rows()
        .iter()
        .map(|o| {
            (
                o.metadata.name.clone().unwrap_or_default(),
                o.data["count"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(name, count)| (name.to_string(), count.to_string()))
        .collect()
}

#[tokio::test]
async fn ns_resources_lists_populated_kinds_as_counts_arrive() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    assert_eq!(app.namespace, "prod");
    assert_eq!(app.list_title(), "ns-resources");
    assert_eq!(app.display_headers()[..], ["KIND", "GROUP", "COUNT"]);
    assert!(!app.store.synced);
    assert!(app.flash.contains("counting resources in prod"));

    counted(&mut app, claim, "deployments", objects(4));
    assert_eq!(counts(&app), pairs(&[("deployments.apps", "4")]));
    assert!(app.flash.contains("1/4 kinds"), "{}", app.flash);
    counted(&mut app, claim, "services", objects(0));
    counted(&mut app, claim, "secrets", Count::NoAccess);
    counted(&mut app, claim, "pods", Count::Failed("timed out".into()));
    assert!(!app.store.synced);

    done(&mut app, claim);
    assert!(app.store.synced);
    assert_eq!(
        counts(&app),
        pairs(&[
            ("deployments.apps", "4"),
            ("pods", "error: timed out"),
            ("secrets", "no access"),
        ])
    );
    assert_eq!(
        app.flash,
        "1 kind with objects in prod · 1 no access · 1 failed"
    );
    assert!(app.flash_err);
    assert!(screen_text(&mut app, 120, 20).contains("○ snapshot"));
}

#[tokio::test]
async fn ns_resources_defaults_to_the_current_namespace() {
    let (mut app, _rx) = test_app();
    app.namespace = "staging".into();
    type_resource_query(&mut app, "ns-resources");
    assert!(app.ns_resources_active());
    assert_eq!(app.namespace, "staging");
}

#[tokio::test]
async fn namespace_row_key_opens_its_resources_and_enter_still_opens_pods() {
    let (mut app, _rx) = test_app();
    app.namespace = "default".into();
    app.switch_kind("namespaces");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "prod"}}),
    );

    app.handle_key(press(KeyCode::Char('R'))).unwrap();
    assert!(app.ns_resources_active());
    assert_eq!(app.namespace, "prod");

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "namespaces");
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "prod"}}),
    );
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.namespace, "prod");
}

#[tokio::test]
async fn namespace_row_key_says_where_it_applies_on_other_kinds() {
    let (mut app, _rx) = test_app();
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char('R'))).unwrap();
    assert_eq!(app.kind_plural, "pods");
    assert!(app.flash_err);
    assert!(app.flash.contains(":ns-resources"));
}

#[tokio::test]
async fn enter_opens_the_kind_in_the_namespace_and_esc_returns() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    counted(&mut app, claim, "deployments", objects(4));
    done(&mut app, claim);

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.kind.as_ref().unwrap().ar.group, "apps");
    assert_eq!(app.namespace, "prod");

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert!(app.ns_resources_active());
    assert_eq!(app.namespace, "prod");
    // The last counts stay on screen while the view counts again.
    assert!(!app.store.synced);
    assert_eq!(counts(&app), pairs(&[("deployments.apps", "4")]));
}

#[tokio::test]
async fn enter_opens_a_kind_discovered_after_connecting() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    let widgets = Kind {
        ar: ApiResource {
            group: "example.com".into(),
            version: "v1beta1".into(),
            api_version: "example.com/v1beta1".into(),
            kind: "Widget".into(),
            plural: "widgets".into(),
        },
        namespaced: true,
        scalable: false,
    };
    counted_kind(&mut app, claim, &widgets, objects(2));
    done(&mut app, claim);

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "widgets");
    let ar = &app.kind.as_ref().unwrap().ar;
    assert_eq!(
        (ar.group.as_str(), ar.api_version.as_str()),
        ("example.com", "example.com/v1beta1")
    );
    assert_eq!(app.namespace, "prod");
}

#[tokio::test]
async fn ns_resources_filters_and_sorts_counts_as_numbers() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    counted(&mut app, claim, "deployments", objects(4));
    counted(&mut app, claim, "pods", objects(12));
    counted(&mut app, claim, "secrets", objects(3));
    counted(&mut app, claim, "services", objects(30));
    done(&mut app, claim);

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "pod".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(counts(&app), pairs(&[("pods", "12")]));
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.rows().len(), 4);

    app.handle_key(press(KeyCode::Char('S'))).unwrap();
    for c in "count".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(
        counts(&app),
        pairs(&[
            ("secrets", "3"),
            ("deployments.apps", "4"),
            ("pods", "12"),
            ("services", "30"),
        ])
    );
}

#[tokio::test]
async fn refresh_counts_again_and_swaps_in_the_new_snapshot() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    counted(&mut app, claim, "deployments", objects(4));
    counted(&mut app, claim, "pods", objects(12));
    done(&mut app, claim);
    let generation = app.generation;

    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(app.generation > generation);
    assert!(!app.store.synced);
    let claim = current_claim(&app);

    // A count from the superseded run is dropped.
    app.handle_msg(Msg::NsResources {
        generation,
        claim,
        update: Update::Counted {
            row: row(&app.cluster.resolve("secrets").unwrap(), &objects(9)).map(Box::new),
            done: 1,
            total: 1,
        },
    });
    counted(&mut app, claim, "pods", objects(13));
    assert_eq!(
        counts(&app),
        pairs(&[("deployments.apps", "4"), ("pods", "12")])
    );
    done(&mut app, claim);
    assert_eq!(counts(&app), pairs(&[("pods", "13")]));
}

#[tokio::test]
async fn keys_that_act_on_objects_do_nothing_on_kind_rows() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    counted(&mut app, claim, "deployments", objects(4));
    done(&mut app, claim);

    for key in [ctrl(KeyCode::Char('d')), press(KeyCode::Char('y'))] {
        app.handle_key(key).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.ns_resources_active());
        assert!(app.flash.contains("⏎ opens one"), "{}", app.flash);
    }
    type_resource_query(&mut app, "xray");
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("⏎ opens one"), "{}", app.flash);
}

#[tokio::test]
async fn contexts_key_opens_the_picker_from_kind_rows() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    counted(&mut app, claim, "deployments", objects(4));
    done(&mut app, claim);
    use_keys(&mut app, CONTEXTS_KEY);

    app.handle_key(ctrl(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Contexts);
    assert!(!app.flash.contains("⏎ opens one"), "{}", app.flash);
}

#[tokio::test]
async fn view_history_returns_to_ns_resources() {
    let (mut app, _rx) = test_app();
    open(&mut app, "prod");
    type_resource_query(&mut app, "pods");
    assert_eq!(app.kind_plural, "pods");

    app.handle_key(press(KeyCode::Char('['))).unwrap();
    assert!(app.ns_resources_active());
    assert_eq!(app.namespace, "prod");
    assert!(app.flash.contains("resources in prod"), "{}", app.flash);
}

#[tokio::test]
async fn namespace_patterns_are_refused_before_the_view_changes() {
    let (mut app, _rx) = test_app();
    app.namespace = "default".into();
    app.switch_kind("pods");
    let history = app.history.len();

    type_resource_query(&mut app, "ns-resources prod-*");
    assert_eq!(app.kind_plural, "pods");
    assert_eq!(app.namespace, "default");
    assert_eq!(app.history.len(), history);
    assert!(app.flash_err);
    assert!(app.flash.contains("not a pattern"), "{}", app.flash);

    app.namespace = "prod-*".into();
    type_resource_query(&mut app, "ns-resources");
    assert_eq!(app.kind_plural, "pods");
    assert!(app.flash.contains("not a pattern"), "{}", app.flash);
}

#[tokio::test]
async fn switching_the_view_to_a_namespace_pattern_is_refused() {
    let (mut app, _rx) = test_app();
    open(&mut app, "prod");
    let generation = app.generation;
    type_resource_query(&mut app, "namespaces prod-*");
    assert!(app.ns_resources_active());
    assert_eq!(app.namespace, "prod");
    assert!(app.flash.contains("not a pattern"), "{}", app.flash);
    assert_eq!(app.generation, generation);
}

#[tokio::test]
async fn object_plugins_are_refused_on_kind_rows() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    counted(&mut app, claim, "deployments", objects(4));
    done(&mut app, claim);
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
        app.flash.contains("ns-resources rows are kinds"),
        "{}",
        app.flash
    );
    type_resource_query(&mut app, "objects");
    assert!(app.pending.is_none());
    assert!(
        app.flash.contains("ns-resources rows are kinds"),
        "{}",
        app.flash
    );

    app.handle_key(ctrl(KeyCode::Char('o'))).unwrap();
    assert!(matches!(app.pending, Some(Suspend::Shell(_))));
}

#[tokio::test]
async fn enter_opens_the_version_that_was_counted() {
    let (mut app, _rx) = test_app();
    let claim = open(&mut app, "prod");
    let mut deployments = app.cluster.resolve("deployments").unwrap();
    deployments.ar.version = "v2".into();
    deployments.ar.api_version = "apps/v2".into();
    counted_kind(&mut app, claim, &deployments, objects(4));
    done(&mut app, claim);

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.kind.as_ref().unwrap().ar.api_version, "apps/v2");
}

#[tokio::test]
async fn a_custom_resource_sharing_the_plural_keeps_its_own_view() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.com", "NsResource", "ns-resources", true);
    app.switch_kind("ns-resources.example.com");
    assert_eq!(app.kind_plural, "ns-resources");
    assert!(!app.ns_resources_active());
    apply(
        &mut app,
        json!({"apiVersion": "example.com/v1", "kind": "NsResource",
               "metadata": {"name": "one", "namespace": "default"}}),
    );
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert!(!app.flash.contains("⏎ opens one"), "{}", app.flash);
    assert_ne!(app.mode, Mode::Table);
}
