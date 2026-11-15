use super::*;

fn karpenter_kinds(app: &mut App) {
    app.cluster
        .register_kind("karpenter.sh", "NodePool", "nodepools", false);
    app.cluster
        .register_kind("karpenter.sh", "NodeClaim", "nodeclaims", false);
    app.cluster
        .register_kind("karpenter.k8s.aws", "EC2NodeClass", "ec2nodeclasses", false);
    app.cluster.register_kind(
        "karpenter.azure.com",
        "AKSNodeClass",
        "aksnodeclasses",
        false,
    );
}

fn node_pool(name: &str, spec: serde_json::Value, status: serde_json::Value) -> serde_json::Value {
    json!({"apiVersion": "karpenter.sh/v1", "kind": "NodePool",
        "metadata": {"name": name, "uid": format!("{name}-uid")},
        "spec": spec, "status": status})
}

fn node_claim(name: &str, class: serde_json::Value) -> serde_json::Value {
    json!({"apiVersion": "karpenter.sh/v1", "kind": "NodeClaim",
        "metadata": {"name": name, "uid": format!("{name}-uid"),
            "labels": {"karpenter.sh/nodepool": "default"}},
        "spec": {"nodeClassRef": class}})
}

fn ec2_class(name: &str) -> serde_json::Value {
    json!({"group": "karpenter.k8s.aws", "kind": "EC2NodeClass", "name": name})
}

fn aks_class(name: &str) -> serde_json::Value {
    json!({"group": "karpenter.azure.com", "kind": "AKSNodeClass", "name": name})
}

#[tokio::test]
async fn a_configured_nodepool_drill_overrides_the_builtin_one() {
    let (mut app, _rx) = test_app();
    karpenter_kinds(&mut app);
    // `install_views` asserts that compiling the views warns about nothing,
    // which is what `:config` reports.
    install_views(
        &mut app,
        r#"
        [views."karpenter.sh/v1/nodepools"]
        drill = { kind = "nodes", labels = "karpenter.sh/nodepool={name}" }
        "#,
    );
    type_resource_query(&mut app, "nodepools");
    apply(&mut app, node_pool("default", json!({}), json!({})));
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "nodes");
    assert_eq!(app.labels.as_deref(), Some("karpenter.sh/nodepool=default"));
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.kind_plural, "nodepools");
}

#[tokio::test]
async fn another_groups_nodepools_keep_their_own_enter() {
    let (mut app, _rx) = test_app();
    app.cluster
        .register_kind("example.com", "NodePool", "nodepools", false);
    app.cluster
        .register_kind("karpenter.sh", "NodeClaim", "nodeclaims", false);
    type_resource_query(&mut app, "nodepools");
    apply(
        &mut app,
        json!({"apiVersion": "example.com/v1", "kind": "NodePool",
            "metadata": {"name": "default"}}),
    );
    app.table_state.select(Some(0));

    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "nodepools");
    assert!(app.stack.is_empty());
    assert!(matches!(app.mode, Mode::Detail), "{:?}", app.mode);
}

async fn adjacent_of_claim(class: serde_json::Value, path: &str) -> App {
    let root = node_claim("default-abc", class);
    let (mut app, rx) = test_app();
    karpenter_kinds(&mut app);
    let (mut app, mut rx, responses, requests) =
        health_report_app_with(app, rx, "nodeclaims", root.clone());
    {
        let mut replies = responses.lock().unwrap();
        replies.insert(
            "/apis/karpenter.sh/v1/nodeclaims/default-abc".into(),
            (200, root),
        );
        replies.insert(
            "/api/v1/nodes".into(),
            (
                200,
                json!({"apiVersion": "v1", "kind": "NodeList", "metadata": {}, "items": []}),
            ),
        );
        let (group, kind, name) = (
            path.split('/').nth(2).unwrap(),
            if path.contains("ec2") {
                "EC2NodeClass"
            } else {
                "AKSNodeClass"
            },
            path.rsplit('/').next().unwrap(),
        );
        replies.insert(
            path.into(),
            (
                200,
                json!({"apiVersion": format!("{group}/v1"), "kind": kind,
                    "metadata": {"name": name}}),
            ),
        );
    }
    app.handle_key(press(KeyCode::Char('u'))).unwrap();
    receive_adjacent(&mut app, &mut rx).await;
    let other = if path.contains("ec2") {
        "karpenter.azure.com"
    } else {
        "karpenter.k8s.aws"
    };
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains(other)),
        "read the other provider's classes"
    );
    app
}

#[tokio::test]
async fn adjacent_on_a_nodeclaim_follows_its_nodeclass_for_each_provider() {
    let app = adjacent_of_claim(
        ec2_class("default"),
        "/apis/karpenter.k8s.aws/v1/ec2nodeclasses/default",
    )
    .await;
    let items: Vec<_> = app
        .adjacent_items
        .iter()
        .map(|it| (it.name.as_str(), it.plural.as_str(), it.relation.as_str()))
        .collect();
    assert_eq!(
        items,
        [("default", "ec2nodeclasses.karpenter.k8s.aws", "shaped by")]
    );

    let app = adjacent_of_claim(
        aks_class("system"),
        "/apis/karpenter.azure.com/v1/aksnodeclasses/system",
    )
    .await;
    let items: Vec<_> = app
        .adjacent_items
        .iter()
        .map(|it| (it.name.as_str(), it.plural.as_str(), it.relation.as_str()))
        .collect();
    assert_eq!(
        items,
        [("system", "aksnodeclasses.karpenter.azure.com", "shaped by")]
    );
}

#[tokio::test]
async fn adjacent_on_a_nodeclass_lists_the_claims_it_shapes_cluster_wide() {
    let root = json!({"apiVersion": "karpenter.k8s.aws/v1", "kind": "EC2NodeClass",
        "metadata": {"name": "default", "uid": "class-uid"}});
    let (mut app, rx) = test_app();
    karpenter_kinds(&mut app);
    let (mut app, mut rx, responses, requests) =
        health_report_app_with(app, rx, "ec2nodeclasses", root.clone());
    {
        let mut replies = responses.lock().unwrap();
        replies.insert(
            "/apis/karpenter.k8s.aws/v1/ec2nodeclasses/default".into(),
            (200, root),
        );
        replies.insert(
            "/apis/karpenter.sh/v1/nodeclaims".into(),
            (
                200,
                json!({"apiVersion": "karpenter.sh/v1", "kind": "NodeClaimList",
                "metadata": {}, "items": [
                    node_claim("shaped", ec2_class("default")),
                    node_claim("other-class", ec2_class("gpu")),
                    node_claim("other-provider", aks_class("default")),
                ]}),
            ),
        );
    }
    app.handle_key(press(KeyCode::Char('u'))).unwrap();
    receive_adjacent(&mut app, &mut rx).await;
    let items: Vec<_> = app
        .adjacent_items
        .iter()
        .map(|it| (it.name.as_str(), it.relation.as_str(), it.direction))
        .collect();
    assert_eq!(
        items,
        [("shaped", "shaped by", crate::adjacent::Direction::NamedBy)]
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|path| !path.contains("/namespaces/")),
        "NodeClaims are listed across the cluster"
    );

    // `enter` lands on the claim, `esc` comes back.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.kind_plural, "nodeclaims");
}

#[tokio::test]
async fn nodepool_columns_show_usage_against_limits_in_text() {
    let (mut app, _rx) = test_app();
    karpenter_kinds(&mut app);
    type_resource_query(&mut app, "nodepools");
    apply(
        &mut app,
        node_pool(
            "tight",
            json!({
                "template": {"spec": {"nodeClassRef": ec2_class("default")}},
                "limits": {"cpu": "100", "memory": "400Gi"},
                "disruption": {"budgets": [
                    {"nodes": "10%"},
                    {"nodes": "0", "reasons": ["Drifted", "Underutilized"],
                     "schedule": "0 9 * * mon-fri", "duration": "8h"},
                ]},
            }),
            json!({
                "nodes": 3,
                "resources": {"cpu": "96", "memory": "100Gi", "nodes": "3"},
                "conditions": [{"type": "Ready", "status": "True"}],
            }),
        ),
    );
    apply(
        &mut app,
        node_pool(
            "open",
            json!({"template": {"spec": {"nodeClassRef": ec2_class("spot")}}}),
            json!({
                "resources": {"cpu": "1500m", "memory": "512Mi", "nodes": "2"},
                "conditions": [{"type": "Ready", "status": "False"}],
            }),
        ),
    );
    apply(
        &mut app,
        node_pool("zero", json!({"limits": {"cpu": "0"}}), json!({})),
    );

    let (headers, rows) = app.snapshot_table();
    assert_eq!(
        headers,
        [
            "NAME",
            "NODECLASS",
            "NODES",
            "CPU",
            "MEMORY",
            "BUDGET",
            "READY",
            "AGE"
        ]
    );
    // The first row takes the cursor bar, so the capped pool sorts second.
    assert_eq!(
        rows[1][..7],
        [
            "tight",
            "default",
            "3",
            "96/100 (96%)",
            "100.0Gi/400.0Gi (25%)",
            "10%, 0 Drifted+Underutilized @0 9 * * mon-fri for 8h",
            "True",
        ]
    );
    // No limit: the amount alone, never a percentage of nothing.
    assert_eq!(
        rows[0][..7],
        ["open", "spot", "2", "1.5", "512Mi", "", "False"]
    );
    assert_eq!(
        rows[2][..7],
        ["zero", "", "0", "0/0 (100%)", "0", "", "Unknown"]
    );

    // The percentage is in the text; the tint repeats it for the critical
    // pool and leaves the quiet one in its row color.
    assert_eq!(
        health_row_color(&mut app, "tight", "96/100"),
        crate::theme::red()
    );
    assert_eq!(
        health_row_color(&mut app, "tight", "100.0Gi/"),
        crate::theme::blue()
    );

    // CPU sorts by the amount used, not the text.
    select_sort_with_keys(&mut app, "CPU");
    assert_eq!(row_names(&app), ["zero", "open", "tight"]);
}

#[tokio::test]
async fn nodeclaim_columns_show_placement_and_disruption_in_text() {
    let (mut app, _rx) = test_app();
    karpenter_kinds(&mut app);
    type_resource_query(&mut app, "nodeclaims");
    apply(
        &mut app,
        json!({"apiVersion": "karpenter.sh/v1", "kind": "NodeClaim",
        "metadata": {"name": "default-abc", "labels": {
            "karpenter.sh/nodepool": "default",
            "node.kubernetes.io/instance-type": "m7g.large",
            "karpenter.sh/capacity-type": "spot",
            "topology.kubernetes.io/zone": "eu-west-1a",
        }},
        "spec": {"nodeClassRef": ec2_class("default")},
        "status": {"nodeName": "ip-10-0-1-2", "conditions": [
            {"type": "Ready", "status": "True"},
            {"type": "Drifted", "status": "True"},
            {"type": "Consolidatable", "status": "True"},
        ]}}),
    );
    apply(
        &mut app,
        json!({"apiVersion": "karpenter.sh/v1", "kind": "NodeClaim",
            "metadata": {"name": "default-def"},
            "status": {"conditions": [{"type": "Ready", "status": "Unknown"}]}}),
    );

    let (headers, rows) = app.snapshot_table();
    assert_eq!(
        headers,
        [
            "NAME",
            "POOL",
            "INSTANCE",
            "CAPACITY",
            "ZONE",
            "NODE",
            "READY",
            "DRIFTED",
            "CONSOLIDATABLE",
            "AGE"
        ]
    );
    assert_eq!(
        rows[0][..9],
        [
            "default-abc",
            "default",
            "m7g.large",
            "spot",
            "eu-west-1a",
            "ip-10-0-1-2",
            "True",
            "True",
            "True",
        ]
    );
    assert_eq!(
        rows[1][..9],
        [
            "default-def",
            "",
            "",
            "",
            "",
            "",
            "Unknown",
            "False",
            "False"
        ]
    );

    type_filter(&mut app, "drifted=true");
    assert_eq!(row_names(&app), ["default-abc"]);
}

#[tokio::test]
async fn user_columns_still_overlay_the_karpenter_columns() {
    let (mut app, _rx) = test_app();
    karpenter_kinds(&mut app);
    install_views(
        &mut app,
        r#"
        [[views."karpenter.sh/v1/nodepools".columns]]
        name = "WEIGHT"
        path = "/spec/weight"
        "#,
    );
    type_resource_query(&mut app, "nodepools");
    apply(
        &mut app,
        node_pool("default", json!({"weight": 10}), json!({})),
    );
    let (headers, rows) = app.snapshot_table();
    assert!(headers.contains(&"BUDGET".to_string()), "{headers:?}");
    let weight = headers.iter().position(|h| h == "WEIGHT").unwrap();
    assert_eq!(rows[0][weight], "10");
}

#[tokio::test]
async fn nodepool_usage_sorts_by_the_raw_quantity_not_the_rounded_cell() {
    let (mut app, _rx) = test_app();
    karpenter_kinds(&mut app);
    type_resource_query(&mut app, "nodepools");
    // Name order is the reverse of the amounts, which render alike.
    for (name, cpu, memory) in [
        ("a-more", "1002m", "536870913"),
        ("b-less", "1001m", "512Mi"),
    ] {
        apply(
            &mut app,
            node_pool(
                name,
                json!({}),
                json!({"resources": {"cpu": cpu, "memory": memory}}),
            ),
        );
    }
    let (headers, rows) = app.snapshot_table();
    let (cpu, memory) = (
        headers.iter().position(|h| h == "CPU").unwrap(),
        headers.iter().position(|h| h == "MEMORY").unwrap(),
    );
    assert_eq!(rows[0][cpu], rows[1][cpu]);
    assert_eq!(rows[0][memory], rows[1][memory]);

    select_sort_with_keys(&mut app, "CPU");
    assert_eq!(row_names(&app), ["b-less", "a-more"]);
    select_sort_with_keys(&mut app, "MEMORY");
    assert_eq!(row_names(&app), ["b-less", "a-more"]);
}
