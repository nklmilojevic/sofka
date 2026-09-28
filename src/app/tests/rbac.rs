use super::*;
use crate::rbac::Subject;
use std::collections::BTreeMap;
use std::sync::Mutex;

const BASE: &str = "/apis/rbac.authorization.k8s.io/v1";
type Responses = BTreeMap<String, (u16, Value)>;

fn binding(
    kind: &str,
    ns: Option<&str>,
    name: &str,
    role_kind: &str,
    role: &str,
    subjects: Value,
) -> Value {
    json!({"apiVersion":"rbac.authorization.k8s.io/v1", "kind":kind,
        "metadata":{"name":name,"namespace":ns},
        "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":role_kind,"name":role},
        "subjects":subjects})
}

fn role(kind: &str, ns: Option<&str>, name: &str, resource: &str) -> Value {
    json!({"apiVersion":"rbac.authorization.k8s.io/v1", "kind":kind,
        "metadata":{"name":name,"namespace":ns},
        "rules":[{"apiGroups":[""],"resources":[resource],"resourceNames":["allowed"],"verbs":["get"]}]})
}

fn fixtures() -> Responses {
    let user = json!([{"kind":"User","name":"alice","apiGroup":"rbac.authorization.k8s.io"}]);
    let roles = vec![
        binding(
            "RoleBinding",
            Some("a"),
            "one",
            "Role",
            "reader",
            user.clone(),
        ),
        binding(
            "RoleBinding",
            Some("b"),
            "two",
            "Role",
            "reader",
            user.clone(),
        ),
        binding(
            "RoleBinding",
            Some("a"),
            "shared-a",
            "ClusterRole",
            "shared",
            user.clone(),
        ),
        binding(
            "RoleBinding",
            Some("b"),
            "shared-b",
            "ClusterRole",
            "shared",
            user.clone(),
        ),
        binding(
            "RoleBinding",
            Some("a"),
            "sa",
            "Role",
            "reader",
            json!([{"kind":"ServiceAccount","name":"robot","namespace":"a"}]),
        ),
        binding(
            "RoleBinding",
            Some("b"),
            "sa",
            "Role",
            "reader",
            json!([{"kind":"ServiceAccount","name":"robot"}]),
        ),
    ];
    let clusters = vec![binding(
        "ClusterRoleBinding",
        None,
        "team",
        "ClusterRole",
        "admin",
        json!([
            {"kind":"Group","name":"team","apiGroup":"rbac.authorization.k8s.io"}
        ]),
    )];
    BTreeMap::from([
        (
            format!("{BASE}/rolebindings"),
            (
                200,
                json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBindingList","metadata":{},"items":roles}),
            ),
        ),
        (
            format!("{BASE}/clusterrolebindings"),
            (
                200,
                json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBindingList","metadata":{},"items":clusters}),
            ),
        ),
        (
            format!("{BASE}/namespaces/a/roles/reader"),
            (200, role("Role", Some("a"), "reader", "pods/log")),
        ),
        (
            format!("{BASE}/namespaces/b/roles/reader"),
            (200, role("Role", Some("b"), "reader", "secrets")),
        ),
        (
            format!("{BASE}/clusterroles/shared"),
            (200, role("ClusterRole", None, "shared", "configmaps")),
        ),
        (
            format!("{BASE}/clusterroles/admin"),
            (
                200,
                json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"admin"},"rules":[{"nonResourceURLs":["/healthz/*"],"verbs":["get"]}]}),
            ),
        ),
    ])
}

fn client(responses: Responses) -> (kube::Client, Arc<Mutex<Vec<String>>>) {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let requests = paths.clone();
    let client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            if request.uri().path().starts_with(BASE) {
                assert_eq!(request.method(), http::Method::GET);
            }
            let path = request.uri().path().to_string();
            let query = request.uri().query().unwrap_or("");
            requests.lock().unwrap().push(format!("{path}?{query}"));
            let key = if query.contains("continue=next") {
                format!("{path}?next")
            } else {
                path
            };
            let (status, body) = responses.get(&key).cloned().unwrap_or((403,json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","code":403,"message":"test denies this read"})));
            async move {
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .body(http_body_util::Full::new(hyper::body::Bytes::from(
                            body.to_string(),
                        )))
                        .unwrap(),
                )
            }
        }),
        "default",
    );
    (client, paths)
}

async fn reply(rx: &mut Receiver<Msg>) -> Msg {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let msg = rx.recv().await.unwrap();
            if matches!(msg, Msg::RbacReport { .. }) {
                return msg;
            }
        }
    })
    .await
    .expect("RBAC request must finish")
}

fn output(app: &App) -> String {
    app.rbac
        .document
        .lines
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn subjects_enter_refresh_and_back_preserve_scope_and_sources() {
    let (mut app, mut rx) = test_app();
    let (client, paths) = client(fixtures());
    app.cluster.client = client;
    palette(&mut app, "users");
    assert_eq!(app.mode, Mode::Rbac);
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.rbac.subjects, vec![(Subject::User("alice".into()), 4)]);
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    let text = output(&app);
    for expected in [
        "Role a/reader",
        "Role b/reader",
        "RoleBinding a/shared-a",
        "RoleBinding b/shared-b",
        "Scope: namespace a",
        "Scope: namespace b",
        "pods/log",
        "Resource names (restricted): allowed",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(!text.contains("/healthz"));
    assert_eq!(
        paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with(&format!("{BASE}/clusterroles/shared?")))
            .count(),
        1
    );
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    assert!(app.rbac.pending);
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(text, output(&app));
    app.handle_key(press(KeyCode::Esc)).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.rbac.subjects.len(), 1);
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn group_rules_keep_non_resource_urls() {
    let (mut app, mut rx) = test_app();
    app.cluster.client = client(fixtures()).0;
    palette(&mut app, "groups");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.rbac.subjects, vec![(Subject::Group("team".into()), 1)]);
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert!(output(&app).contains("Non-resource URLs: /healthz/*"));
    assert!(output(&app).contains("Scope: cluster and all namespaces"));
}

#[tokio::test]
async fn explicit_policy_and_selected_service_account_use_full_identity() {
    for command in [
        "policy s:a/robot",
        "policy s:b/robot",
        "policy g:team",
        "policy u:alice",
        "policy",
    ] {
        let (mut app, mut rx) = test_app();
        app.cluster.client = client(fixtures()).0;
        if command == "policy" {
            app.cluster
                .register_kind("", "ServiceAccount", "serviceaccounts", true);
            app.switch_kind("serviceaccounts");
            apply(
                &mut app,
                json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"robot","namespace":"a"}}),
            );
            app.table_state.select(Some(0));
        }
        palette(&mut app, command);
        assert_eq!(app.mode, Mode::Rbac, "{command}: {}", app.flash);
        let msg = reply(&mut rx).await;
        app.handle_msg(msg);
        let text = output(&app);
        if command == "policy" || command == "policy s:a/robot" {
            assert!(text.contains("Role a/reader"), "{text}");
            assert!(!text.contains("Role b/reader"), "{text}");
        } else if command == "policy s:b/robot" {
            assert!(text.contains("Role b/reader"));
            assert!(!text.contains("Role a/reader"));
        }
    }
}

#[tokio::test]
async fn policy_rejects_invalid_or_missing_subjects() {
    for command in [
        "policy",
        "policy s:robot",
        "policy s:/robot",
        "policy s:a/",
        "policy s:a/b/c",
        "policy u:",
        "policy x:alice",
        "policy u:alice extra",
    ] {
        let (mut app, _rx) = test_app();
        palette(&mut app, command);
        assert_eq!(app.mode, Mode::Table, "{command}");
        assert!(
            app.flash.contains("usage: :policy"),
            "{command}: {}",
            app.flash
        );
    }
}

#[tokio::test]
async fn enter_on_each_rbac_kind_reads_its_rules() {
    for (kind, plural, ns, object) in [
        (
            "Role",
            "roles",
            Some("a"),
            role("Role", Some("a"), "reader", "pods"),
        ),
        (
            "ClusterRole",
            "clusterroles",
            None,
            role("ClusterRole", None, "reader", "pods"),
        ),
        (
            "RoleBinding",
            "rolebindings",
            Some("a"),
            binding(
                "RoleBinding",
                Some("a"),
                "reader",
                "ClusterRole",
                "shared",
                json!([]),
            ),
        ),
        (
            "ClusterRoleBinding",
            "clusterrolebindings",
            None,
            binding(
                "ClusterRoleBinding",
                None,
                "reader",
                "ClusterRole",
                "shared",
                json!([]),
            ),
        ),
    ] {
        let (mut app, mut rx) = test_app();
        let mut responses = fixtures();
        let path = match ns {
            Some(ns) => format!("{BASE}/namespaces/{ns}/{plural}/reader"),
            None => format!("{BASE}/{plural}/reader"),
        };
        responses.insert(path, (200, object.clone()));
        app.cluster.client = client(responses).0;
        app.cluster
            .register_kind("rbac.authorization.k8s.io", kind, plural, ns.is_some());
        app.switch_kind(plural);
        apply(&mut app, object);
        app.table_state.select(Some(0));
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Rbac, "{kind}");
        let msg = reply(&mut rx).await;
        app.handle_msg(msg);
        assert!(app.rbac.warnings.is_empty(), "{:?}", app.rbac.warnings);
        assert!(output(&app).contains("Verbs: get"));
        if kind == "RoleBinding" {
            assert!(output(&app).contains("Scope: namespace a"));
        }
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::Table);
    }
}

#[tokio::test]
async fn denied_binding_lists_and_missing_roles_are_incomplete() {
    let (mut app, mut rx) = test_app();
    let mut responses = fixtures();
    responses.remove(&format!("{BASE}/clusterrolebindings"));
    responses.insert(format!("{BASE}/namespaces/a/roles/reader"),(404,json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":"role is missing"})));
    app.cluster.client = client(responses).0;
    palette(&mut app, "policy u:alice");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    let text = output(&app);
    assert!(text.contains("INCOMPLETE: ClusterRoleBindings could not be read"));
    assert!(text.contains("INCOMPLETE: Role a/reader could not be read"));
    assert!(text.contains("Resources: secrets"));
    assert!(text.contains("Rules unavailable."));
}

#[tokio::test]
async fn pagination_deduplicates_subjects_and_keeps_counts() {
    let (mut app, mut rx) = test_app();
    let mut responses = fixtures();
    let page = responses.get_mut(&format!("{BASE}/rolebindings")).unwrap();
    page.1["metadata"]["continue"] = json!("next");
    let second = binding(
        "RoleBinding",
        Some("c"),
        "extra",
        "Role",
        "reader",
        json!([
            {"kind":"User","name":"alice"},{"kind":"User","name":"alice"},{"kind":"User","name":"bob"}
        ]),
    );
    responses.insert(format!("{BASE}/rolebindings?next"),(200,json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBindingList","metadata":{},"items":[second]})));
    app.cluster.client = client(responses).0;
    palette(&mut app, "users");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(
        app.rbac.subjects,
        vec![
            (Subject::User("alice".into()), 5),
            (Subject::User("bob".into()), 1)
        ]
    );
}

#[tokio::test]
async fn old_responses_cannot_replace_new_views_or_return_after_exit() {
    let (mut app, mut rx) = test_app();
    app.cluster.client = client(fixtures()).0;
    palette(&mut app, "users");
    let old = reply(&mut rx).await;
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    app.handle_msg(old);
    assert!(app.rbac.pending);
    assert!(app.rbac.subjects.is_empty());
    let current = reply(&mut rx).await;
    app.handle_key(press(KeyCode::Esc)).unwrap();
    app.handle_msg(current);
    assert_eq!(app.mode, Mode::Table);
    assert!(app.rbac.subjects.is_empty());
    palette(&mut app, "users");
    let old = reply(&mut rx).await;
    app.generation += 1;
    app.handle_msg(old);
    assert!(app.rbac.subjects.is_empty());
}

#[tokio::test]
async fn namespaced_binding_keeps_role_scope_and_does_not_grant_urls() {
    let (mut app, mut rx) = test_app();
    let mut responses = fixtures();
    responses.insert(
        format!("{BASE}/namespaces/a/rolebindings/reader"),
        (
            200,
            binding(
                "RoleBinding",
                Some("a"),
                "reader",
                "Role",
                "reader",
                json!([]),
            ),
        ),
    );
    responses.insert(format!("{BASE}/namespaces/a/roles/reader"), (200, json!({
        "apiVersion":"rbac.authorization.k8s.io/v1", "kind":"Role", "metadata":{"name":"reader","namespace":"a"},
        "rules":[{"apiGroups":[""],"resources":["pods/log"],"resourceNames":["one","two"],"verbs":["get","watch"]}]
    })));
    app.cluster.client = client(responses).0;
    app.cluster.register_kind(
        "rbac.authorization.k8s.io",
        "RoleBinding",
        "rolebindings",
        true,
    );
    app.switch_kind("rolebindings");
    apply(
        &mut app,
        binding(
            "RoleBinding",
            Some("a"),
            "reader",
            "Role",
            "reader",
            json!([]),
        ),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    let text = output(&app);
    assert!(text.contains("Role: Role a/reader"), "{text}");
    assert!(text.contains("Resource names (restricted): one, two"));
    assert!(text.contains("Verbs: get, watch"));
    assert!(!text.contains("secrets"));

    let mut responses = fixtures();
    responses.insert(format!("{BASE}/clusterroles/shared"), (200, json!({
        "apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"shared"},
        "rules":[{"nonResourceURLs":["/healthz"],"verbs":["get"]}]
    })));
    app.cluster.client = client(responses).0;
    palette(&mut app, "policy u:alice");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    let text = output(&app);
    assert!(text.contains("Non-resource URLs: /healthz"));
    assert!(text.contains("Not granted by this namespaced binding."));
}

#[tokio::test]
async fn subject_view_renders_incomplete_reads_and_keeps_data_under_palette() {
    let (mut app, mut rx) = test_app();
    let mut responses = fixtures();
    responses.remove(&format!("{BASE}/clusterrolebindings"));
    app.cluster.client = client(responses).0;
    palette(&mut app, "users");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("INCOMPLETE:"), "{text}");
    assert!(text.contains("User alice"), "{text}");
    assert!(text.contains("4 binding(s)"), "{text}");
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Rbac);
    assert_eq!(app.rbac.subjects.len(), 1);
}

#[tokio::test]
async fn enter_on_roles_in_another_api_group_keeps_the_configured_drill() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.com", "Role", "roles", true);
    let cfg: crate::config::Config = toml::from_str(
        r#"
        [views."example.com/roles"]
        drill = { kind = "secrets", fields = "metadata.name={name}" }
    "#,
    )
    .unwrap();
    let (views, warnings) = crate::views::compile(&cfg.views);
    assert!(warnings.is_empty(), "{warnings:?}");
    app.user_views = views;
    app.switch_kind("roles");
    apply(
        &mut app,
        json!({"apiVersion":"example.com/v1","kind":"Role","metadata":{"name":"custom","namespace":"default"}}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "secrets");
    assert_eq!(app.fields.as_deref(), Some("metadata.name=custom"));
}

fn subject_fixtures(names: &[&str]) -> Responses {
    let mut responses = fixtures();
    responses
        .get_mut(&format!("{BASE}/rolebindings"))
        .unwrap()
        .1["items"] = json!(
        names
            .iter()
            .map(|name| binding(
                "RoleBinding",
                Some("a"),
                name,
                "Role",
                "reader",
                json!([{"kind":"User","name":name}])
            ))
            .collect::<Vec<_>>()
    );
    responses
}

#[tokio::test]
async fn refresh_and_back_keep_subject_identity_when_rows_move_or_disappear() {
    let (mut app, mut rx) = test_app();
    app.cluster.client = client(subject_fixtures(&["alice", "bob", "carol"])).0;
    palette(&mut app, "users");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    app.handle_key(press(KeyCode::Down)).unwrap();
    assert_eq!(app.rbac.selection.selected(), Some(1));
    app.cluster.client = client(subject_fixtures(&["aaron", "alice", "bob", "carol"])).0;
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    let old = reply(&mut rx).await;
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    app.handle_msg(old);
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.rbac.selection.selected(), Some(2));
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert!(app.rbac.document.title.ends_with("User bob"));
    app.cluster.client = client(subject_fixtures(&["bob", "carol"])).0;
    app.handle_key(press(KeyCode::Esc)).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.rbac.selection.selected(), Some(0));
    assert_eq!(app.rbac.subjects[0].0, Subject::User("bob".into()));
    app.cluster.client = client(subject_fixtures(&["alice", "carol"])).0;
    app.handle_key(press(KeyCode::Char('r'))).unwrap();
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.rbac.selection.selected(), None);
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert!(matches!(
        app.rbac.query,
        Some(super::super::rbac::Query::Subjects(_))
    ));
    assert!(!app.rbac.pending);
}

#[tokio::test]
async fn role_reads_run_in_bounded_parallel_and_output_stays_ordered() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut app, mut rx) = test_app();
    let mut responses = fixtures();
    let mut bindings = Vec::new();
    for i in 0..20 {
        let name = format!("role-{i:02}");
        bindings.push(binding(
            "RoleBinding",
            Some("a"),
            &name,
            "Role",
            &name,
            json!([{"kind":"User","name":"alice"}]),
        ));
        responses.insert(
            format!("{BASE}/namespaces/a/roles/{name}"),
            (200, role("Role", Some("a"), &name, "pods")),
        );
    }
    bindings.push(binding(
        "RoleBinding",
        Some("b"),
        "duplicate",
        "ClusterRole",
        "shared",
        json!([{"kind":"User","name":"alice"}]),
    ));
    bindings.push(binding(
        "RoleBinding",
        Some("c"),
        "duplicate",
        "ClusterRole",
        "shared",
        json!([{"kind":"User","name":"alice"}]),
    ));
    responses
        .get_mut(&format!("{BASE}/rolebindings"))
        .unwrap()
        .1["items"] = json!(bindings);
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (a, p, c) = (active.clone(), peak.clone(), calls.clone());
    app.cluster.client = kube::Client::new(
        tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let path = request.uri().path().to_string();
            let (status, body) = responses.get(&path).cloned().unwrap_or((403, json!({
                "apiVersion":"v1", "kind":"Status", "status":"Failure", "reason":"Forbidden", "code":403, "message":"unused test request"
            })));
            let role_read = path.contains("/roles/") || path.contains("/clusterroles/");
            let (active, peak, calls) = (a.clone(), p.clone(), c.clone());
            async move {
                if role_read {
                    calls.lock().unwrap().push(path.clone());
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(if path.ends_with("00") {
                        40
                    } else {
                        10
                    }))
                    .await;
                    active.fetch_sub(1, Ordering::SeqCst);
                }
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .body(http_body_util::Full::new(hyper::body::Bytes::from(
                            body.to_string(),
                        )))
                        .unwrap(),
                )
            }
        }),
        "default",
    );
    palette(&mut app, "policy u:alice");
    let msg = reply(&mut rx).await;
    app.handle_msg(msg);
    let peak = peak.load(Ordering::SeqCst);
    assert!(peak > 1 && peak <= 8, "peak concurrency: {peak}");
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.ends_with("/shared"))
            .count(),
        1
    );
    assert_eq!(calls.lock().unwrap().len(), 21);
    let lines: Vec<_> = app
        .rbac
        .document
        .lines
        .iter()
        .filter(|line| line.starts_with("Binding:"))
        .collect();
    let mut sorted = lines.clone();
    sorted.sort();
    assert_eq!(lines, sorted);
    assert!(app.rbac.warnings.is_empty());
}
