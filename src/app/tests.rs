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
    });
    let names: Vec<String> = app
        .rows()
        .iter()
        .map(|o| o.metadata.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["b", "a"]);
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
        key: 'g',
        name: "argocd-sync".into(),
        command: "argocd".into(),
        args: vec![],
        scopes: vec![],
    }];
    app.flash.clear();
    app.try_plugin('g');
    assert!(app.pending.is_none(), "plugin must not run");
    assert!(app.flash.contains("read-only"));

    // Read paths stay open: describe still works.
    app.flash.clear();
    app.handle_key(press(KeyCode::Char('d'))).unwrap();
    assert!(!app.flash.contains("read-only"));
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
async fn doc_search_filters_detail_view() {
    let (mut app, _rx) = test_app();
    apply(
        &mut app,
        json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web", "namespace": "default"}}),
    );
    app.table_state.select(Some(0));
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(app.mode, Mode::Detail);

    // `/` opens the search prompt for the detail view; typing builds the query.
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    assert_eq!(app.mode, Mode::DocFilter);
    assert_eq!(app.doc_filter_return, Mode::Detail);
    for c in "kind".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    assert_eq!(app.detail.filter, "kind");

    // Enter keeps the query and returns to the view.
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert_eq!(app.detail.filter, "kind");

    // First esc clears the search (stays), second esc leaves the view.
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Detail);
    assert!(app.detail.filter.is_empty());
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn doc_search_esc_clears_and_scroll_resets() {
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
    // Typing snaps the view back to the top so the first match is visible.
    assert_eq!(app.detail.scroll, 0);
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
async fn copy_doc_respects_active_search() {
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

    // No search: the whole document.
    assert_eq!(
        app.filtered_doc_text(),
        "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web"
    );

    // Active search: only the matching lines (case-insensitive).
    app.detail.filter = "KIND".into();
    assert_eq!(app.filtered_doc_text(), "kind: Pod");
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
