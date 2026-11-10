use super::*;
use crate::theme;
use ratatui::{Terminal, backend::TestBackend};

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(press(code)).unwrap();
}

fn keys(app: &mut App, text: &str) {
    for c in text.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn document(app: &mut App, mode: Mode, lines: impl IntoIterator<Item = String>) {
    app.detail = Scrollable {
        title: "doc".into(),
        lines: lines.into_iter().collect(),
        ..Default::default()
    };
    app.mode = mode;
}

fn numbered(n: usize) -> impl Iterator<Item = String> {
    (0..n).map(|i| format!("line {i}"))
}

fn doc_selection(app: &App) -> Option<(usize, usize)> {
    app.detail
        .selection()
        .map(|selection| (selection.anchor, selection.cursor))
}

fn push_logs(app: &mut App, lines: impl IntoIterator<Item = String>) {
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: lines.into_iter().collect(),
    });
}

fn log_selection(app: &App) -> Option<(usize, usize)> {
    app.logs
        .selection
        .map(|selection| (selection.anchor, selection.cursor))
}

fn screen(term: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = term.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn select_lines_starts_in_logs_and_every_document_view() {
    let (mut app, _rx) = test_app();
    for mode in [Mode::Detail, Mode::Diff, Mode::Events] {
        document(&mut app, mode, numbered(5));
        key(&mut app, KeyCode::Char('V'));
        assert_eq!(doc_selection(&app), Some((0, 0)), "{mode:?}");
        key(&mut app, KeyCode::Char('V'));
        assert_eq!(doc_selection(&app), None, "V again cancels in {mode:?}");
    }

    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(3));
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('V'));
    assert_eq!(log_selection(&app), Some((0, 0)));
}

#[tokio::test]
async fn select_lines_with_nothing_to_select_warns() {
    let (mut app, _rx) = test_app();
    document(&mut app, Mode::Detail, Vec::new());
    key(&mut app, KeyCode::Char('V'));
    assert_eq!(doc_selection(&app), None);
    assert!(app.flash_err);
    assert_eq!(app.flash, "nothing to select");

    app.mode = Mode::Logs;
    key(&mut app, KeyCode::Char('V'));
    assert!(!app.logs.selecting());
    assert_eq!(app.flash, "no log lines to select");
    assert!(app.logs.follow, "autoscroll is untouched");
}

#[tokio::test]
async fn document_selection_starts_on_the_current_search_match() {
    let (mut app, _rx) = test_app();
    document(&mut app, Mode::Detail, numbered(50));
    keys(&mut app, "/line 3");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('n'));
    let matches = app.detail.match_lines();
    let current = matches[app.detail.match_idx];
    assert_eq!(current, 30);
    app.detail.scroll = 0;

    key(&mut app, KeyCode::Char('V'));
    assert_eq!(doc_selection(&app), Some((30, 30)));
}

#[tokio::test]
async fn document_selection_starts_on_the_first_visible_line() {
    let (mut app, _rx) = test_app();
    document(&mut app, Mode::Detail, numbered(100));
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    for _ in 0..7 {
        key(&mut app, KeyCode::Char('j'));
    }
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert_eq!(app.detail.scroll, 7);

    key(&mut app, KeyCode::Char('V'));
    assert_eq!(doc_selection(&app), Some((7, 7)));
}

#[tokio::test]
async fn document_selection_moves_with_navigation_and_match_keys() {
    let (mut app, _rx) = test_app();
    document(
        &mut app,
        Mode::Diff,
        (0..100).map(|i| match i {
            10 | 60 => format!("needle {i}"),
            _ => format!("line {i}"),
        }),
    );
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(doc_selection(&app), Some((0, 2)));
    key(&mut app, KeyCode::Char('k'));
    assert_eq!(doc_selection(&app), Some((0, 1)));
    app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
    assert_eq!(doc_selection(&app), Some((0, 21)));
    app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
    assert_eq!(doc_selection(&app), Some((0, 1)));
    key(&mut app, KeyCode::Char('G'));
    assert_eq!(doc_selection(&app), Some((0, 99)));
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(doc_selection(&app), Some((0, 0)));
    assert_eq!(app.detail.scroll, 0, "the cursor moves, not the scroll");

    keys(&mut app, "/needle");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Diff);
    assert_eq!(
        doc_selection(&app),
        Some((0, 0)),
        "search keeps the selection"
    );
    key(&mut app, KeyCode::Char('n'));
    assert_eq!(doc_selection(&app), Some((0, 60)));
    key(&mut app, KeyCode::Char('N'));
    assert_eq!(doc_selection(&app), Some((0, 10)));

    key(&mut app, KeyCode::Char('o'));
    assert_eq!(doc_selection(&app), Some((10, 0)));
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(doc_selection(&app), Some((10, 1)));
}

#[tokio::test]
async fn escape_cancels_a_document_selection_before_the_search() {
    let (mut app, _rx) = test_app();
    document(&mut app, Mode::Events, numbered(10));
    keys(&mut app, "/line");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('j'));

    key(&mut app, KeyCode::Esc);
    assert_eq!(doc_selection(&app), None);
    assert_eq!(app.mode, Mode::Events);
    assert_eq!(app.detail.filter, "line", "the search stays");
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.detail.filter, "");
    assert_eq!(app.mode, Mode::Events);
}

#[tokio::test]
async fn document_selection_follows_a_shorter_refreshed_document() {
    let (mut app, _rx) = test_app();
    document(&mut app, Mode::Events, numbered(10));
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('G'));
    assert_eq!(doc_selection(&app), Some((0, 9)));

    app.handle_msg(Msg::Events {
        generation: app.event_gen,
        title: "events".into(),
        lines: numbered(3).collect(),
    });
    assert_eq!(doc_selection(&app), Some((0, 2)));

    app.handle_msg(Msg::Events {
        generation: app.event_gen,
        title: "events".into(),
        lines: Vec::new(),
    });
    assert_eq!(doc_selection(&app), None);
}

#[tokio::test]
async fn document_selection_scrolls_to_the_cursor_and_marks_its_lines() {
    let (mut app, _rx) = test_app();
    document(&mut app, Mode::Detail, numbered(100));
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();

    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('j'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&term);
    let row = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap();
    assert!(rows[row("line 0")].contains("▌ line 0"));
    assert!(rows[row("line 1")].contains("▌ line 1"));
    assert!(rows[row("line 2")].contains("▶ line 2"));
    assert!(rows[row("line 3")].contains("  line 3"));
    assert!(rows.iter().any(|r| r.contains("▌3 selected")));
    let buffer = term.backend().buffer();
    let x = rows[row("line 1")].chars().position(|c| c == '▌').unwrap() as u16;
    let y = row("line 1") as u16;
    assert_eq!(buffer[(x + 2, y)].bg, theme::surface1());
    let y = row("line 3") as u16;
    assert_ne!(buffer[(x + 2, y)].bg, theme::surface1());

    key(&mut app, KeyCode::Char('G'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(screen(&term).iter().any(|r| r.contains("▶ line 99")));
    assert!(app.detail.scroll > 0);

    key(&mut app, KeyCode::Char('g'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert_eq!(app.detail.scroll, 0);
}

#[tokio::test]
async fn log_selection_starts_on_the_first_visible_line() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(100));
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(app.logs.follow);
    let height = app.logs.viewport_h;
    push_logs(&mut app, ["line 100".to_string()]);

    key(&mut app, KeyCode::Char('V'));
    let first = 101 - height;
    assert_eq!(log_selection(&app), Some((first, first)));

    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Char('g'));
    for _ in 0..5 {
        key(&mut app, KeyCode::Char('j'));
    }
    key(&mut app, KeyCode::Char('V'));
    assert_eq!(log_selection(&app), Some((5, 5)));
}

#[tokio::test]
async fn log_selection_moves_over_the_lines_the_filter_shows() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, (0..60).map(|i| format!("keep {i}")));
    push_logs(&mut app, ["drop".to_string(), "keep 60".to_string()]);
    app.logs.viewport_h = 10;
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(log_selection(&app), Some((0, 1)));
    key(&mut app, KeyCode::Char('k'));
    assert_eq!(log_selection(&app), Some((0, 0)));
    app.handle_key(ctrl(KeyCode::Char('f'))).unwrap();
    assert_eq!(log_selection(&app), Some((0, 10)));
    key(&mut app, KeyCode::Char(' '));
    assert_eq!(log_selection(&app), Some((0, 20)));
    app.handle_key(ctrl(KeyCode::Char('b'))).unwrap();
    assert_eq!(log_selection(&app), Some((0, 10)));
    key(&mut app, KeyCode::Char('G'));
    assert_eq!(log_selection(&app), Some((0, 61)));
    assert!(!app.logs.follow, "G moves the cursor, not autoscroll");
    key(&mut app, KeyCode::Char('o'));
    assert_eq!(log_selection(&app), Some((61, 0)));
    key(&mut app, KeyCode::Char('o'));

    keys(&mut app, "/keep");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Char('k'));
    assert_eq!(log_selection(&app), Some((0, 59)), "skips the hidden line");
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(log_selection(&app), Some((0, 61)));
    assert_eq!(
        app.logs.selected_text(),
        (
            (0..61)
                .map(|i| format!("keep {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            61
        ),
        "the hidden line is not selected"
    );
    assert_eq!(app.logs.selected_lines(), 61);
}

#[tokio::test]
async fn log_selection_pauses_autoscroll_until_it_ends() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(5));
    assert!(app.logs.follow);

    key(&mut app, KeyCode::Char('V'));
    assert!(!app.logs.follow);
    push_logs(&mut app, ["line 5".to_string()]);
    assert_eq!(app.logs.view.lines.len(), 6, "the stream keeps filling");
    assert!(!app.logs.follow);

    key(&mut app, KeyCode::Esc);
    assert!(!app.logs.selecting());
    assert!(app.logs.follow, "autoscroll resumes");
    assert_eq!(app.mode, Mode::Logs, "esc stays in the view");
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Table);
}

#[tokio::test]
async fn log_selection_keeps_paused_autoscroll_paused() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(5));
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('V'));
    assert!(!app.logs.selecting());
    assert!(!app.logs.follow);

    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('s'));
    assert!(!app.logs.selecting(), "the follow key ends the selection");
    assert!(app.logs.follow);
}

#[tokio::test]
async fn log_selection_stays_on_its_lines_as_new_lines_arrive() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(
        &mut app,
        [
            "2024-01-01T00:00:01Z one",
            "2024-01-01T00:00:03Z three",
            "2024-01-01T00:00:04Z four",
        ]
        .map(String::from),
    );
    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('G'));
    key(&mut app, KeyCode::Char('o'));
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(log_selection(&app), Some((2, 1)));
    assert_eq!(app.logs.selected_text().0, "three\nfour");

    push_logs(
        &mut app,
        ["2024-01-01T00:00:05Z five", "2024-01-01T00:00:00Z zero"].map(String::from),
    );
    assert_eq!(app.logs.view.lines.front().unwrap(), "zero");
    assert_eq!(app.logs.selected_text().0, "three\nfour");
    assert_eq!(log_selection(&app), Some((3, 2)));

    push_logs(&mut app, ["2024-01-01T00:00:02Z two".to_string()]);
    assert_eq!(log_selection(&app), Some((4, 3)));
    assert_eq!(app.logs.selected_text().0, "three\nfour");
}

#[tokio::test]
async fn log_selection_clamps_to_the_oldest_kept_line_when_trimmed() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    key(&mut app, KeyCode::Char('g'));
    push_logs(&mut app, (0..MAX_LOG_LINES_PAUSED).map(|i| format!("l{i}")));
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.logs.selected_text().0, "l0\nl1\nl2");

    push_logs(&mut app, ["new 1".to_string(), "new 2".to_string()]);
    assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES_PAUSED);
    assert_eq!(app.logs.view.lines.front().unwrap(), "l2");
    assert_eq!(log_selection(&app), Some((2, 2)));
    assert_eq!(app.logs.selected_text().0, "l2");

    push_logs(&mut app, ["new 3".to_string()]);
    assert_eq!(log_selection(&app), Some((3, 3)));
    assert_eq!(app.logs.selected_text().0, "l3");
    key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.logs.selected_text().0, "l3\nl4");
}

#[tokio::test]
async fn clearing_the_log_buffer_ends_the_selection() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(5));
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('z'));
    assert!(!app.logs.selecting());
    assert!(app.logs.follow);
}

#[tokio::test]
async fn log_selection_marks_its_rows_with_a_gutter_and_background() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(3));
    key(&mut app, KeyCode::Char('m'));
    push_logs(&mut app, ["line 3".to_string(), "x".repeat(200)]);
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(
        !screen(&term).iter().any(|r| r.contains("▌")),
        "no gutter without a selection"
    );

    key(&mut app, KeyCode::Char('g'));
    key(&mut app, KeyCode::Char('V'));
    for _ in 0..3 {
        key(&mut app, KeyCode::Char('j'));
    }
    key(&mut app, KeyCode::Char('w'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&term);
    let row = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap();
    assert!(rows[row("line 0")].contains("▌ line 0"));
    assert!(rows[row("line 1")].contains("▌ line 1"));
    assert!(rows[row("line 2")].contains("▌ line 2"));
    let dashes = "-".repeat(20);
    assert!(
        rows[row(&dashes)].contains(&format!("  {dashes}")),
        "the marker is not selected"
    );
    assert!(rows[row("line 3")].contains("▶ line 3"));
    assert!(rows.iter().any(|r| r.contains("▌4 selected")));
    let buffer = term.backend().buffer();
    let x = rows[row("line 2")].chars().position(|c| c == '▌').unwrap() as u16;
    assert_eq!(buffer[(x + 2, row("line 2") as u16)].bg, theme::surface1());
    assert_ne!(buffer[(x + 2, row("xxxx") as u16)].bg, theme::surface1());
    let wrapped = rows.iter().filter(|r| r.contains("xxxx")).count();
    assert!(wrapped > 1, "the long line wraps");
    assert!(
        rows.iter()
            .filter(|r| r.contains("xxxx"))
            .all(|r| r.contains("  xxxx")),
        "the line after the selection keeps an empty gutter"
    );

    key(&mut app, KeyCode::Char('j'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&term);
    let wrapped: Vec<_> = rows.iter().filter(|r| r.contains("xxxx")).collect();
    assert!(wrapped[0].contains("▶ xxxx"));
    assert!(wrapped[1..].iter().all(|r| r.contains("▌ xxxx")));
}

#[tokio::test]
async fn log_selection_scrolls_to_the_cursor() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(100));
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('g'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert_eq!(app.logs.view.scroll, 0);
    assert!(screen(&term).iter().any(|r| r.contains("▶ line 0")));

    key(&mut app, KeyCode::Char('G'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(screen(&term).iter().any(|r| r.contains("▶ line 99")));
}

#[tokio::test]
async fn selection_keys_show_in_the_footer_and_help() {
    let (mut app, _rx) = test_app();
    app.mode = Mode::Logs;
    push_logs(&mut app, numbered(3));
    key(&mut app, KeyCode::Char('V'));
    let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(
        screen(&term)
            .iter()
            .any(|r| r.contains("o:other end") && r.contains("y:copy") && r.contains("esc:cancel"))
    );

    for (scope, mode) in [
        ("logs", Mode::Logs),
        ("detail", Mode::Detail),
        ("diff", Mode::Diff),
        ("events", Mode::Events),
    ] {
        for action in [
            Action::SelectLines,
            Action::CopyLines,
            Action::SelectionOtherEnd,
        ] {
            assert!(
                app.keymap
                    .entries()
                    .any(|(s, a, _)| s == scope && a == action),
                "{scope} {action:?}"
            );
        }
        assert_eq!(app.scope_for(mode), scope);
    }
}

#[tokio::test]
async fn diff_selection_keeps_diff_colors_and_marks_its_lines() {
    let (mut app, _rx) = test_app();
    document(
        &mut app,
        Mode::Diff,
        [
            "+added".to_string(),
            "-removed".to_string(),
            " same".to_string(),
        ]
        .into_iter()
        .chain(std::iter::once(format!("+{}", "w".repeat(150))))
        .chain(numbered(60)),
    );
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    key(&mut app, KeyCode::Char('V'));
    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('w'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&term);
    let row = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap();
    assert!(rows[row("+added")].contains("▌ +added"));
    assert!(rows[row("-removed")].contains("▶ -removed"));
    assert!(rows[row(" same")].contains("   same"));
    let buffer = term.backend().buffer();
    let x = rows[row("+added")].chars().position(|c| c == '+').unwrap() as u16;
    let added = &buffer[(x, row("+added") as u16)];
    assert_eq!(added.fg, theme::green());
    assert_eq!(added.bg, theme::surface1());
    let x = rows[row(" same")].chars().position(|c| c == 's').unwrap() as u16;
    assert_ne!(buffer[(x, row(" same") as u16)].bg, theme::surface1());

    key(&mut app, KeyCode::Char('j'));
    key(&mut app, KeyCode::Char('j'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let rows = screen(&term);
    let wrapped: Vec<_> = rows.iter().filter(|r| r.contains("wwww")).collect();
    assert!(wrapped.len() > 1, "the long line wraps");
    assert!(wrapped[0].contains("▶ +www"));
    assert!(wrapped[1..].iter().all(|r| r.contains("▌ www")));

    key(&mut app, KeyCode::Char('G'));
    term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    assert!(app.detail.scroll > 0, "the cursor is scrolled into view");
    assert!(screen(&term).iter().any(|r| r.contains("▶ line 59")));
}
