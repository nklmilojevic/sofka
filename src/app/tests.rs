use super::*;
use crate::store::row_key;
use serde_json::json;
use tokio::sync::mpsc::{self, Receiver};

fn obj(v: serde_json::Value) -> DynamicObject {
    serde_json::from_value(v).unwrap()
}

fn test_app() -> (App, Receiver<Msg>) {
    let (tx, rx) = mpsc::channel(1024);
    (App::new(Cluster::fake(), tx), rx)
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
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
async fn events_palette_command_dispatches() {
    let (mut app, _rx) = test_app();
    assert!(app.run_palette_command("events"));
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash_err);
    assert!(app.flash.contains("events"), "{}", app.flash);
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
        ns: "default".into(),
        target: "pod/a".into(),
        ports: "8080:80".into(),
        child: spawn_test_child("sleep", "30"),
    });
    app.port_forwards.push(PortForward {
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
    for i in 0..(MAX_LOG_LINES + 50) {
        app.handle_msg(Msg::LogLines {
            generation: app.log_gen,
            lines: vec![format!("line {i}")],
        });
    }
    assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES);
    // Oldest lines dropped; newest retained.
    assert_eq!(
        app.logs.view.lines.back().unwrap(),
        &format!("line {}", MAX_LOG_LINES + 49)
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
    app.logs.filter = "api".into();
    assert_eq!(
        app.filtered_log_text(),
        "api request started\napi request finished"
    );
}

#[tokio::test]
async fn stale_log_save_result_is_dropped() {
    let (mut app, _rx) = test_app();
    let stale = app.log_gen;
    app.log_gen += 1;

    app.handle_msg(Msg::LogsSaved {
        generation: stale,
        result: Err("old write failed".into()),
    });
    assert!(!app.flash.contains("old write failed"));

    app.handle_msg(Msg::LogsSaved {
        generation: app.log_gen,
        result: Ok(std::env::temp_dir().join("sofka-test.log")),
    });
    assert!(app.flash.contains("sofka-test.log"));
    assert!(!app.flash_err);
}

#[tokio::test]
async fn stale_clipboard_result_is_dropped() {
    let (mut app, _rx) = test_app();
    let stale = app.generation;
    app.bump_generation();

    app.handle_msg(Msg::ClipboardCopied {
        generation: stale,
        copied: false,
        success: "copied stale".into(),
        failure: "stale failed".into(),
    });
    assert!(!app.flash.contains("stale failed"));

    app.handle_msg(Msg::ClipboardCopied {
        generation: app.generation,
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
async fn paused_logs_do_not_trim_below_paused_cap() {
    let (mut app, _rx) = test_app();
    app.logs.follow = false; // autoscroll OFF
    let lg = app.log_gen;
    let line = |i: usize| Msg::LogLines {
        generation: lg,
        lines: vec![format!("line {i}")],
    };
    // Well past the *following* cap, but under the paused cap: nothing is
    // dropped, so a frozen view never appears to resume scrolling.
    for i in 0..(MAX_LOG_LINES + 500) {
        app.handle_msg(line(i));
    }
    assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES + 500);

    // Resuming follow trims the backlog back to the tight cap.
    app.mode = Mode::Logs;
    app.handle_key(press(KeyCode::Char('s'))).unwrap(); // follow on
    assert!(app.logs.follow);
    assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES);
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
            }],
            sort: None,
            replace: false,
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
