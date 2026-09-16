use super::*;

fn roles(app: &App) -> Vec<String> {
    let (headers, rows) = app.snapshot_table();
    let index = headers.iter().position(|header| header == "ROLES").unwrap();
    rows.into_iter().map(|row| row[index].clone()).collect()
}

#[tokio::test]
async fn custom_node_roles_reload_filter_and_sort() {
    let dir = std::env::temp_dir().join(format!("sofka-node-roles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    palette(&mut app, "nodes");
    for (name, labels) in [
        ("a", json!({"example.com/role":"worker"})),
        (
            "z",
            json!({"node-role.example.com/control-plane":"", "example.com/role":"control-plane", "kubernetes.io/role":"worker", "node-role.kubernetes.io/worker":""}),
        ),
        (
            "empty",
            json!({"node-role.example.com/":"", "example.com/role":"", "unrelated":"ignored"}),
        ),
    ] {
        apply(
            &mut app,
            json!({"apiVersion":"v1", "kind":"Node", "metadata":{"name":name, "labels":labels}}),
        );
    }
    assert_eq!(roles(&app), ["<none>", "<none>", "worker"]);
    let generation = app.generation;
    let custom = r#"
[node_roles]
label_prefixes = ["", "node-role.example.com/", "node-role.kubernetes.io/", "node-role.example.com/"]
label_keys = ["example.com/role", "kubernetes.io/role"]
[views.nodes]
sort = "ROLES:asc"
"#;
    write_config(&dir, custom);
    palette(&mut app, "reload");
    assert_eq!(app.generation, generation);
    assert_eq!(
        app.config_warnings,
        ["node_roles.label_prefixes: empty prefix ignored"]
    );
    assert_eq!(row_names(&app), ["empty", "z", "a"]);
    assert_eq!(roles(&app), ["<none>", "control-plane,worker", "worker"]);
    type_filter(&mut app, "roles=worker");
    assert_eq!(row_names(&app), ["a"]);
    retype_filter(&mut app, "roles=control-plane,worker");
    assert_eq!(row_names(&app), ["z"]);
    retype_filter(&mut app, "");
    write_config(&dir, "[node_roles]\nlabel_prefixes = []\nlabel_keys = []\n");
    palette(&mut app, "reload");
    assert!(app.config_warnings.is_empty());
    let (headers, rows) = app.snapshot_table();
    let index = headers.iter().position(|header| header == "ROLES").unwrap();
    for row in rows {
        assert_eq!(row[index], if row[0] == "z" { "worker" } else { "<none>" });
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn node_roles_follow_context_overrides() {
    let dir = std::env::temp_dir().join(format!("sofka-node-roles-context-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_config(
        &dir,
        "[node_roles]\nlabel_prefixes = ['base-role/']\nlabel_keys = ['base/role']\n",
    );
    write_config(
        &dir.join("clusters/prod"),
        "[node_roles]\nlabel_prefixes = ['cluster/']\n",
    );
    write_config(
        &dir.join("clusters/prod/west"),
        "[node_roles]\nlabel_keys = ['context/role']\n",
    );
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    for (context, cluster_name, expected) in [
        ("west", "prod", "cluster,context,worker"),
        ("east", "prod", "base-value,cluster,worker"),
        ("other", "other", "base,base-value,worker"),
    ] {
        palette(&mut app, &format!("nodes --context {context}"));
        let mut cluster = Cluster::fake();
        cluster.context = context.into();
        cluster.cluster_name = cluster_name.into();
        app.handle_msg(Msg::ContextSwitched {
            generation: app.generation,
            name: context.into(),
            result: Ok(Box::new(cluster)),
        });
        assert_eq!(app.kind_plural, "nodes");
        apply(
            &mut app,
            json!({"apiVersion":"v1", "kind":"Node", "metadata":{"name":"node", "labels":{
                "base-role/base":"", "base/role":"base-value", "cluster/cluster":"", "context/role":"context", "node-role.kubernetes.io/worker":""
            }}}),
        );
        assert_eq!(roles(&app), [expected]);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn custom_node_roles_respect_view_sources() {
    let dir = std::env::temp_dir().join(format!("sofka-node-roles-view-{}", std::process::id()));
    for (column, expected) in [
        (r#"{ name = "ROLES", builtin = "ROLES" }"#, "worker"),
        (
            r#"{ name = "ROLES", path = "/metadata/labels/other" }"#,
            "explicit",
        ),
    ] {
        let (mut app, _rx) = test_app();
        write_config(
            &dir,
            &format!(
                r#"
[node_roles]
label_keys = ["example.com/role"]
[views.nodes]
replace = true
columns = [{column}]
"#
            ),
        );
        app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
        palette(&mut app, "reload");
        palette(&mut app, "nodes");
        apply(
            &mut app,
            json!({"apiVersion":"v1", "kind":"Node", "metadata":{"name":"node", "labels":{
                "example.com/role":"worker", "other":"explicit"
            }}}),
        );
        assert_eq!(roles(&app), [expected]);
        assert!(app.config_warnings.is_empty(), "{:?}", app.config_warnings);
    }
    std::fs::remove_dir_all(dir).unwrap();
}
