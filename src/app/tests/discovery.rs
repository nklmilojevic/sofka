use super::*;

const TEMPLATE_GROUP: &str = "template.openshift.io";

fn verbs(read_write: bool) -> serde_json::Value {
    if read_write {
        json!([
            "create",
            "delete",
            "deletecollection",
            "get",
            "list",
            "patch",
            "update",
            "watch"
        ])
    } else {
        json!(["create"])
    }
}

/// `template.openshift.io/v1` as openshift-apiserver serves it in legacy
/// discovery. Template is served twice: create-only as `processedtemplates`,
/// which sorts first, and listable as `templates`.
fn legacy_template_group() -> serde_json::Value {
    json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": "template.openshift.io/v1",
        "resources": [
            {"name": "brokertemplateinstances", "singularName": "brokertemplateinstance",
             "namespaced": false, "kind": "BrokerTemplateInstance", "verbs": verbs(true)},
            {"name": "processedtemplates", "singularName": "processedtemplate",
             "namespaced": true, "kind": "Template", "verbs": verbs(false)},
            {"name": "templateinstances", "singularName": "templateinstance",
             "namespaced": true, "kind": "TemplateInstance", "verbs": verbs(true)},
            {"name": "templateinstances/status", "singularName": "",
             "namespaced": true, "kind": "TemplateInstance", "verbs": ["get", "patch", "update"]},
            {"name": "templates", "singularName": "template",
             "namespaced": true, "kind": "Template", "verbs": verbs(true)}
        ]
    })
}

/// The same group in aggregated discovery.
fn aggregated_template_group() -> serde_json::Value {
    let resource = |plural: &str, kind: &str, scope: &str, read_write: bool| {
        json!({
            "resource": plural,
            "responseKind": {"group": "", "version": "", "kind": kind},
            "scope": scope,
            "singularResource": plural.trim_end_matches('s'),
            "verbs": verbs(read_write)
        })
    };
    let mut instances = resource("templateinstances", "TemplateInstance", "Namespaced", true);
    instances["subresources"] = json!([{
        "subresource": "status",
        "responseKind": {"group": "", "version": "", "kind": "TemplateInstance"},
        "verbs": ["get", "patch", "update"]
    }]);
    json!({
        "kind": "APIGroupDiscoveryList",
        "apiVersion": "apidiscovery.k8s.io/v2",
        "items": [{
            "metadata": {"name": TEMPLATE_GROUP},
            "versions": [{
                "version": "v1",
                "freshness": "Current",
                "resources": [
                    resource("brokertemplateinstances", "BrokerTemplateInstance", "Cluster", true),
                    resource("processedtemplates", "Template", "Namespaced", false),
                    instances,
                    resource("templates", "Template", "Namespaced", true)
                ]
            }]
        }]
    })
}

fn openshift_discoveries() -> [(&'static str, crate::k8s::Discovery); 2] {
    [
        (
            "legacy",
            crate::k8s::Discovery::from_legacy(vec![legacy_template_group()]),
        ),
        (
            "aggregated",
            crate::k8s::Discovery::from_aggregated(aggregated_template_group()),
        ),
    ]
}

fn suggestions(app: &mut App, typed: &str) -> Vec<String> {
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in typed.chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    let labels = app
        .cmd_suggestions
        .iter()
        .map(|s| s.label.clone())
        .collect();
    app.handle_key(press(KeyCode::Esc)).unwrap();
    labels
}

#[tokio::test]
async fn openshift_templates_resolve_beside_create_only_processedtemplates() {
    for (shape, discovery) in openshift_discoveries() {
        let (mut app, _rx) = test_app();
        app.cluster.apply_discovery(discovery);

        palette(&mut app, "templates.template.openshift.io");
        assert!(!app.flash_err, "{shape}: {}", app.flash);
        assert_eq!(
            app.flash, "Viewing templates.template.openshift.io",
            "{shape}"
        );
        let kind = app.kind.clone().expect("a kind is open");
        assert_eq!(kind.ar.group, TEMPLATE_GROUP, "{shape}");
        assert_eq!(kind.ar.plural, "templates", "{shape}");
        assert_eq!(kind.ar.kind, "Template", "{shape}");
        assert!(kind.namespaced, "{shape}");

        palette(&mut app, "templateinstances.template.openshift.io");
        assert_eq!(app.kind_plural, "templateinstances", "{shape}");

        palette(&mut app, "template");
        assert_eq!(app.kind_plural, "templates", "{shape}");
        assert_eq!(
            app.cluster
                .resolve_in_group("Template", TEMPLATE_GROUP)
                .map(|k| k.ar.plural),
            Some("templates".to_string()),
            "{shape}"
        );

        let labels = suggestions(&mut app, "templates");
        assert!(
            labels.iter().any(|l| l == "templates"),
            "{shape}: {labels:?}"
        );
        assert!(
            !app.cluster
                .catalog
                .iter()
                .any(|name| name.starts_with("processedtemplates")),
            "{shape}: {:?}",
            app.cluster.catalog
        );
    }
}
