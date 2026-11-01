use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

/// Views compiled from TOML, keeping the compile warnings for the caller.
fn pin_views(app: &mut App, toml_text: &str) {
    let cfg: crate::config::Config = toml::from_str(toml_text).unwrap();
    let (views, warnings) = crate::views::compile(&cfg.views);
    assert!(warnings.is_empty(), "{warnings:?}");
    app.user_views = views;
}

fn pod(name: &str, node: &str) -> serde_json::Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": "default"},
        "spec": {"nodeName": node},
        "status": {"phase": "Running"}
    })
}

/// A pods table that needs horizontal scrolling at 80 columns in wide mode.
fn pods_app(toml_text: &str, all_namespaces: bool) -> App {
    let (mut app, _rx) = test_app();
    pin_views(&mut app, toml_text);
    if all_namespaces {
        app.namespace.clear();
    }
    app.switch_kind("pods");
    apply(
        &mut app,
        pod(
            "web",
            "node-abcdefghijklmnopqrstuvwxyz-0123456789-long-value",
        ),
    );
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    app.handle_key(press(KeyCode::Home)).unwrap();
    app
}

fn draw(term: &mut Terminal<TestBackend>, app: &mut App) -> crate::app::mouse::TableHit {
    term.draw(|f| crate::ui::draw(f, app)).unwrap();
    app.table_hit.borrow().clone().expect("geometry recorded")
}

/// Headers of the visible columns, left to right.
fn visible(app: &App, hit: &crate::app::mouse::TableHit) -> Vec<String> {
    let headers = app.display_headers();
    hit.cols.iter().map(|c| headers[c.2].clone()).collect()
}

/// The text the table shows on row `y`, inside the borders.
fn line(term: &Terminal<TestBackend>, hit: &crate::app::mouse::TableHit, y: u16) -> String {
    (hit.x_min..hit.x_max)
        .map(|x| term.backend().buffer()[(x, y)].symbol())
        .collect()
}

#[tokio::test]
async fn pinned_columns_render_first_and_stay_fixed_while_the_rest_scrolls() {
    let mut app = pods_app(
        r#"
        [views."v1/pods"]
        pin = ["status", " NAME "]
    "#,
        false,
    );
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert!(app.col_scroll_max > 5);
    assert_eq!(
        visible(&app, &hit)[..3],
        ["STATUS", "NAME", "READY"],
        "pinned columns first, in list order, then the others in their order"
    );
    let header = line(&term, &hit, hit.header_y);
    assert!(header.trim_start().starts_with("STATUS"), "{header}");

    app.handle_key(press(KeyCode::Right)).unwrap();
    let moved = draw(&mut term, &mut app);
    assert_eq!(app.col_offset, 5);
    assert_eq!(hit.cols[..2], moved.cols[..2], "pinned columns stay put");
    let frozen_end = hit.cols[2].0;
    let before: Vec<String> = (hit.x_min..hit.x_max)
        .map(|x| {
            term.backend().buffer()[(x, hit.rows_y)]
                .symbol()
                .to_string()
        })
        .collect();
    app.handle_key(press(KeyCode::Left)).unwrap();
    draw(&mut term, &mut app);
    for x in frozen_end..hit.x_max.saturating_sub(5) {
        assert_eq!(
            term.backend().buffer()[(x + 5, hit.rows_y)].symbol(),
            before[usize::from(x - hit.x_min)],
            "the scrolling columns moved by five cells"
        );
    }

    // Header clicks sort by the header that was clicked, not by its place.
    let status = app
        .display_headers()
        .iter()
        .position(|h| h == "STATUS")
        .unwrap();
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.cols[0].0,
        row: hit.header_y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    assert_eq!(app.sort_column, Some(status));
}

#[tokio::test]
async fn an_empty_pin_list_lets_every_column_scroll() {
    for all_namespaces in [false, true] {
        let mut app = pods_app(
            r#"
            [views."v1/pods"]
            pin = []
        "#,
            all_namespaces,
        );
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let hit = draw(&mut term, &mut app);
        let first = if all_namespaces { "NAMESPACE" } else { "NAME" };
        assert_eq!(visible(&app, &hit)[0], first);
        let before = term.backend().buffer().clone();
        app.handle_key(press(KeyCode::Right)).unwrap();
        draw(&mut term, &mut app);
        assert_eq!(app.col_offset, 5);
        // Only the selection marker stays; the first column scrolls too.
        for y in [hit.header_y, hit.rows_y] {
            for x in hit.x_min + 2..hit.x_max - 5 {
                assert_eq!(
                    term.backend().buffer()[(x, y)].symbol(),
                    before[(x + 5, y)].symbol()
                );
            }
        }
    }
}

#[tokio::test]
async fn an_unpinned_namespace_column_is_the_first_scrolling_column() {
    let mut app = pods_app(
        r#"
        [views."v1/pods"]
        pin = ["NAME"]
    "#,
        true,
    );
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[..3], ["NAME", "NAMESPACE", "READY"]);
    app.handle_key(press(KeyCode::Right)).unwrap();
    let moved = draw(&mut term, &mut app);
    assert_eq!(hit.cols[0], moved.cols[0], "NAME stays put");
    assert_ne!(hit.cols[1], moved.cols[1], "NAMESPACE scrolls");
}

#[tokio::test]
async fn pinning_namespace_needs_no_namespace_column_and_fixes_it_when_shown() {
    let mut app = pods_app(
        r#"
        [views."v1/pods"]
        pin = ["NAMESPACE", "NAME"]
    "#,
        false,
    );
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert!(app.config_warnings.is_empty(), "{:?}", app.config_warnings);
    assert_eq!(visible(&app, &hit)[..2], ["NAME", "READY"]);

    app.switch_kind_ns("pods", Some("all"));
    apply(
        &mut app,
        pod(
            "web",
            "node-abcdefghijklmnopqrstuvwxyz-0123456789-long-value",
        ),
    );
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[..3], ["NAMESPACE", "NAME", "READY"]);
    app.handle_key(press(KeyCode::Right)).unwrap();
    let moved = draw(&mut term, &mut app);
    assert_eq!(hit.cols[..2], moved.cols[..2]);
    assert!(app.config_warnings.is_empty(), "{:?}", app.config_warnings);
}

#[tokio::test]
async fn an_unknown_pin_header_warns_and_the_rest_of_the_list_applies() {
    let (mut app, _rx) = test_app();
    pin_views(
        &mut app,
        r#"
        [views."v1/pods"]
        pin = ["NOPE", "NODE", "STATUS"]
    "#,
    );
    app.switch_kind("pods");
    apply(&mut app, pod("web", "node-1"));
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(
        app.config_warnings,
        ["view pods: pin column NOPE is not in this view; ignored"],
        "NODE is a wide column, so it exists even while it is hidden"
    );
    assert_eq!(visible(&app, &hit)[..2], ["STATUS", "NAME"]);

    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[..3], ["NODE", "STATUS", "NAME"]);
    assert_eq!(app.config_warnings.len(), 1, "{:?}", app.config_warnings);
}

#[tokio::test]
async fn without_pin_namespace_and_name_stay_fixed() {
    let mut app = pods_app(
        r#"
        [views."v1/pods"]
        sort = "AGE"
    "#,
        true,
    );
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[..3], ["NAMESPACE", "NAME", "READY"]);
    app.handle_key(press(KeyCode::Right)).unwrap();
    let moved = draw(&mut term, &mut app);
    assert_eq!(hit.cols[..2], moved.cols[..2]);
}

#[tokio::test]
async fn pinned_columns_wider_than_the_table_disable_scrolling() {
    let mut app = pods_app(
        r#"
        [views."v1/pods"]
        pin = ["NAME", "READY", "STATUS", "RESTARTS", "AGE", "IP", "NODE"]
    "#,
        false,
    );
    let mut term = Terminal::new(TestBackend::new(40, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    let pinned_end = hit.cols.iter().map(|c| c.1).max().unwrap();
    assert_eq!(pinned_end, hit.x_max, "the pinned columns fill the table");
    assert_eq!(app.col_scroll_max, 0);
    app.handle_key(press(KeyCode::Right)).unwrap();
    assert_eq!(app.col_offset, 0);
}

#[tokio::test]
async fn a_narrow_table_shares_the_name_minimum_between_the_pinned_columns() {
    let long = "x".repeat(70);
    for (pin, pinned) in [("[\"NODE\"]", 1u16), ("[\"NODE\", \"NAME\"]", 2)] {
        let (mut app, _rx) = test_app();
        pin_views(&mut app, &format!("[views.\"v1/pods\"]\npin = {pin}"));
        app.switch_kind("pods");
        apply(&mut app, pod(&long, &long));
        app.handle_key(press(KeyCode::Char('w'))).unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let hit = draw(&mut term, &mut app);
        let width = |c: (u16, u16, usize)| c.1 - c.0;
        assert_eq!(visible(&app, &hit)[0], "NODE");
        assert_eq!(width(hit.cols[0]), (80 - 4) / (2 * pinned), "{pin}");
        if pinned == 1 {
            // An unpinned NAME scrolls, so it keeps its full width.
            let name = app
                .display_headers()
                .iter()
                .position(|h| h == "NAME")
                .unwrap();
            assert_eq!(app.table_column_widths()[name], 70);
            assert!(app.col_scroll_max >= 70);
        }
    }
}

#[tokio::test]
async fn pin_applies_to_replace_namespace_and_named_layout_views() {
    let (mut app, _rx) = test_app();
    pin_views(
        &mut app,
        r#"
        [views."v1/pods"]
        replace = true
        pin = ["STATUS"]
        columns = [
          { name = "NAME", builtin = "NAME" },
          { name = "STATUS", builtin = "STATUS" },
          { name = "AGE", builtin = "AGE" },
        ]

        [views."pods@default"]
        pin = ["AGE"]

        [views."pods:slim"]
        replace = true
        pin = ["PHASE"]
        columns = [
          { name = "NAME", builtin = "NAME" },
          { name = "PHASE", path = "/status/phase" },
        ]
    "#,
    );
    app.switch_kind("pods");
    apply(&mut app, pod("web", "node-1"));
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[0], "AGE", "the @namespace view wins");

    app.switch_kind_ns("pods", Some("all"));
    apply(&mut app, pod("web", "node-1"));
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[..3], ["STATUS", "NAMESPACE", "NAME"]);

    palette(&mut app, "layout slim");
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit), ["PHASE", "NAMESPACE", "NAME"]);
    assert!(app.config_warnings.is_empty(), "{:?}", app.config_warnings);
}

#[tokio::test]
async fn sorting_filtering_and_selection_follow_headers_under_pin() {
    let (mut app, _rx) = test_app();
    pin_views(
        &mut app,
        r#"
        [views."v1/pods"]
        pin = ["NODE", "NAME"]
    "#,
    );
    app.switch_kind("pods");
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    apply(&mut app, pod("b", "node-a"));
    apply(&mut app, pod("a", "node-c"));
    apply(&mut app, pod("c", "node-b"));
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    let rows = |term: &Terminal<TestBackend>, hit: &crate::app::mouse::TableHit| -> Vec<String> {
        (0..hit.rows_h)
            .map(|i| line(term, hit, hit.rows_y + i))
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                l.trim_start_matches('▌')
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };
    // Click NODE (pinned first): rows sort by node, not by the first data column.
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.cols[0].0,
        row: hit.header_y,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(
        rows(&term, &hit),
        ["node-a b", "node-b c", "node-c a"],
        "sorted by NODE"
    );

    app.handle_key(press(KeyCode::Down)).unwrap();
    let selected = app.selected().and_then(|o| o.metadata.name);
    assert_eq!(selected.as_deref(), Some("c"));

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    for c in "node-c".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(rows(&term, &hit), ["node-c a"]);
}

#[tokio::test]
async fn pins_on_crd_printer_columns_are_checked_once_the_columns_arrive() {
    let (mut app, _rx) = test_app();
    pin_views(
        &mut app,
        r#"
        [views.certificates]
        pin = ["READY", "NOPE"]
    "#,
    );
    palette(&mut app, "certificates");
    assert!(app.config_warnings.is_empty(), "{:?}", app.config_warnings);

    let crd = json!({"spec": {"versions": [{
        "name": "v1", "served": true, "storage": true,
        "additionalPrinterColumns": [
            {"name": "Ready", "type": "string", "jsonPath": ".status.ready"}
        ]
    }]}});
    app.handle_msg(Msg::PrinterColumns {
        generation: app.generation,
        resource: app.cluster.resolve("certificates").unwrap().resource_key(),
        view: Box::new(crate::views::printer_columns_view(&crd, "v1")),
    });
    apply(
        &mut app,
        json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "tls", "namespace": "default"},
            "status": {"ready": "True"}
        }),
    );
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    let hit = draw(&mut term, &mut app);
    assert_eq!(visible(&app, &hit)[..2], ["READY", "NAME"]);
    assert_eq!(
        app.config_warnings,
        ["view cert-manager.io/certificates: pin column NOPE is not in this view; ignored"]
    );
}
