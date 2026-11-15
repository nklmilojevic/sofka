use super::*;
use crate::theme;
use ratatui::{Terminal, backend::TestBackend, style::Color};

fn badge_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("sofka-context-badge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The first rendered line holding `needle`, with the foreground color of the
/// needle's first cell.
fn find(app: &mut App, needle: &str) -> Option<(String, Color)> {
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height).find_map(|y| {
        let line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        let at = line.find(needle)?;
        let x = u16::try_from(line[..at].chars().count()).unwrap();
        let fg = buffer[(x, y)].fg;
        Some((line, fg))
    })
}

fn context_line(app: &mut App) -> String {
    find(app, "Context:").expect("full header is drawn").0
}

fn switch_to(app: &mut App, context: &str) {
    palette(app, &format!("ctx {context}"));
    land_context(app, context);
}

#[tokio::test]
async fn context_badge_follows_context_switches_in_both_headers() {
    let dir = badge_dir("switch");
    write_config(
        &dir.join("clusters/test-cluster/prod"),
        "readonly = true\n[context_badge]\nlabel = \"prod\"\ncolor = \"red\"\n",
    );
    let (mut app, _rx) = test_app();
    app.all_contexts = vec!["prod".into(), "staging".into(), "test".into()];
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.switch_kind("pods");
    assert!(find(&mut app, "[prod]").is_none());

    switch_to(&mut app, "prod");
    let (line, color) = find(&mut app, "[prod]").expect("full header shows the badge");
    assert!(line.contains("Context:"), "{line}");
    assert!(line.contains("prod  [prod]  [read-only]"), "{line}");
    assert_eq!(color, theme::red());

    app.handle_key(ctrl(KeyCode::Char('e'))).unwrap();
    assert!(app.compact);
    let (line, color) = find(&mut app, "[prod]").expect("compact header shows the badge");
    assert!(line.contains(" sofka "), "{line}");
    assert!(line.contains("prod [prod] [ro]"), "{line}");
    assert_eq!(color, theme::red());

    switch_to(&mut app, "staging");
    assert!(find(&mut app, "[prod]").is_none());
    app.handle_key(ctrl(KeyCode::Char('e'))).unwrap();
    assert!(!app.compact);
    assert!(find(&mut app, "[prod]").is_none());
    assert!(!context_line(&mut app).contains("[read-only]"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn context_picker_rows_show_each_contexts_badge() {
    let dir = badge_dir("picker");
    write_config(
        &dir.join("clusters/prod-cluster"),
        "[context_badge]\nlabel = \"prod\"\ncolor = \"yellow\"\n",
    );
    write_config(
        &dir.join("clusters/prod-cluster/prod-admin"),
        "[context_badge]\nlabel = \"prod admin\"\n",
    );
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    palette(&mut app, "ctx");
    assert_eq!(app.mode, Mode::Contexts);
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod-admin".into(), "prod-view".into(), "test".into()],
    });
    // The list renders before the override files are read.
    assert!(find(&mut app, "prod-admin").is_some());
    assert!(find(&mut app, "[prod").is_none());

    let contexts = [
        ("prod-admin", "prod-cluster"),
        ("prod-view", "prod-cluster"),
        ("test", "test-cluster"),
    ]
    .map(|(context, cluster)| (context.to_string(), cluster.to_string()));
    let badges = app.config.context_badges(&contexts);
    app.handle_msg(Msg::ContextBadges {
        generation: app.generation,
        request: app.ctx_badge_request,
        badges,
    });
    let (line, color) = find(&mut app, "[prod admin]").unwrap();
    assert!(line.contains("prod-admin  [prod admin]"), "{line}");
    assert_eq!(color, theme::yellow(), "color merges from the cluster file");
    let (line, color) = find(&mut app, "[prod]").unwrap();
    assert!(line.contains("prod-view  [prod]"), "{line}");
    assert_eq!(color, theme::yellow());
    let (line, _) = find(&mut app, "● test").unwrap();
    assert!(!line.contains('['), "{line}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn context_badge_color_without_label_warns_and_shows_no_badge() {
    let dir = badge_dir("warn");
    write_config(
        &dir.join("clusters/test-cluster/prod"),
        "[context_badge]\ncolor = \"red\"\n",
    );
    write_config(
        &dir.join("clusters/test-cluster/stage"),
        "[context_badge]\nlabel = \"stage\"\ncolor = \"crimson\"\n",
    );
    let (mut app, _rx) = test_app();
    app.all_contexts = vec!["prod".into(), "stage".into(), "test".into()];
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.switch_kind("pods");

    switch_to(&mut app, "prod");
    let warning = "context_badge: color needs a label; no badge shown";
    assert_eq!(app.config_warnings, [warning]);
    assert!(app.flash_err);
    assert!(app.flash.contains(warning), "{}", app.flash);
    assert!(app.context_badge.is_none());
    let line = context_line(&mut app);
    assert!(!line.contains('['), "{line}");

    switch_to(&mut app, "stage");
    assert_eq!(app.config_warnings.len(), 1);
    assert!(
        app.config_warnings[0].starts_with("context_badge.color: unknown color 'crimson'"),
        "{:?}",
        app.config_warnings
    );
    let (_, color) = find(&mut app, "[stage]").expect("the label still shows");
    assert_eq!(color, theme::text());

    write_config(&dir, "[context_badge]\ncolor = \"red\"\n");
    switch_to(&mut app, "test");
    palette(&mut app, "reload");
    assert_eq!(app.config_warnings, [warning]);
    assert!(app.context_badge.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn reload_applies_an_edited_badge() {
    let dir = badge_dir("reload");
    write_config(&dir, "");
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.switch_kind("pods");
    assert!(find(&mut app, "[local]").is_none());
    write_config(
        &dir.join("clusters/test-cluster"),
        "[context_badge]\nlabel = \"local\"\ncolor = \"green\"\n",
    );
    palette(&mut app, "reload");
    assert!(app.config_warnings.is_empty(), "{:?}", app.config_warnings);
    let (line, color) = find(&mut app, "[local]").unwrap();
    assert!(line.contains("test  [local]"), "{line}");
    assert_eq!(color, theme::green());
    std::fs::remove_dir_all(dir).unwrap();
}

fn badge(label: &str) -> crate::config::ContextBadge {
    crate::config::ContextBadge {
        label: label.into(),
        color: None,
    }
}

fn badges(entries: &[(&str, &str)]) -> HashMap<String, crate::config::ContextBadge> {
    entries
        .iter()
        .map(|(context, label)| (context.to_string(), badge(label)))
        .collect()
}

#[tokio::test]
async fn picker_keeps_only_the_latest_badge_result() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "ctx");
    let first = app.ctx_badge_request;
    app.handle_key(press(KeyCode::Esc)).unwrap();
    palette(&mut app, "ctx");
    assert_eq!(app.mode, Mode::Contexts);
    let second = app.ctx_badge_request;
    assert_ne!(first, second);
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod".into(), "test".into()],
    });
    app.handle_msg(Msg::ContextBadges {
        generation: app.generation,
        request: second,
        badges: badges(&[("prod", "new")]),
    });
    app.handle_msg(Msg::ContextBadges {
        generation: app.generation,
        request: first,
        badges: badges(&[("prod", "old")]),
    });
    let (line, _) = find(&mut app, "[new]").expect("the latest result stays");
    assert!(line.contains("prod  [new]"), "{line}");
    assert!(find(&mut app, "[old]").is_none());
}

#[tokio::test]
async fn renamed_context_keeps_its_badge_and_drops_results_in_flight() {
    let (mut app, _rx) = test_app();
    palette(&mut app, "ctx");
    let before = app.ctx_badge_request;
    app.handle_msg(Msg::Contexts {
        generation: app.generation,
        list: vec!["prod".into(), "test".into()],
    });
    app.handle_msg(Msg::ContextBadges {
        generation: app.generation,
        request: before,
        badges: badges(&[("prod", "prod")]),
    });
    let claim = app.claim_status("renaming prod → live…");
    app.handle_msg(Msg::ContextRenamed {
        generation: app.generation,
        claim,
        old: "prod".into(),
        new: "live".into(),
        result: Ok(()),
    });
    let (line, _) = find(&mut app, "[prod]").expect("the renamed row keeps its badge");
    assert!(line.contains("live  [prod]"), "{line}");
    assert_ne!(app.ctx_badge_request, before, "renaming re-resolves badges");
    app.handle_msg(Msg::ContextBadges {
        generation: app.generation,
        request: before,
        badges: badges(&[("prod", "prod")]),
    });
    assert!(find(&mut app, "live  [prod]").is_some());
    app.handle_msg(Msg::ContextBadges {
        generation: app.generation,
        request: app.ctx_badge_request,
        badges: badges(&[("live", "live")]),
    });
    let (line, _) = find(&mut app, "[live]").unwrap();
    assert!(line.contains("live  [live]"), "{line}");
}

#[tokio::test]
async fn info_redacts_the_badge_label_in_the_header() {
    let dir = badge_dir("redact");
    write_config(
        &dir.join("clusters/test-cluster/prod"),
        "[context_badge]\nlabel = \"prod 10.20.30.40\"\n",
    );
    let (mut app, _rx) = test_app();
    app.all_contexts = vec!["prod".into(), "test".into()];
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.switch_kind("pods");
    switch_to(&mut app, "prod");
    assert!(context_line(&mut app).contains("[prod 10.20.30.40]"));
    for compact in [false, true] {
        app.compact = compact;
        palette(&mut app, "info");
        assert_eq!(app.mode, Mode::Detail);
        let (line, _) = find(&mut app, "[prod ").expect("the badge still shows");
        assert!(!line.contains("10.20.30.40"), "{line}");
        app.handle_key(press(KeyCode::Esc)).unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_long_badge_never_hides_the_read_only_marker() {
    let dir = badge_dir("narrow");
    write_config(
        &dir.join("clusters/test-cluster"),
        "readonly = true\n[context_badge]\nlabel = \"production-eu-west-1-primary\"\n",
    );
    let (mut app, _rx) = test_app();
    app.config = crate::config::ConfigLoader::from_dir(Some(dir.clone()));
    app.switch_kind("pods");
    palette(&mut app, "reload");
    assert!(app.readonly);
    // The logo gives up its column first, so the full header only clips the
    // badge once the info is too narrow even without the logo.
    for (compact, width) in [(false, 60), (true, 60)] {
        app.compact = compact;
        let screen = screen_text(&mut app, width, 20);
        let marker = if compact { "[ro]" } else { "[read-only]" };
        let line = screen
            .lines()
            .find(|line| line.contains("[produc"))
            .unwrap_or_else(|| panic!("badge is shown:\n{screen}"));
        assert!(line.contains("…]"), "the label is clipped: {line}");
        assert!(line.contains(marker), "{line}");
    }
    // Too narrow for even a clipped badge: it gives way to the marker.
    app.compact = false;
    let screen = screen_text(&mut app, 34, 20);
    let line = screen
        .lines()
        .find(|line| line.contains("Context:"))
        .unwrap();
    assert!(line.contains("test  [read-only]"), "{line}");
    assert!(!line.contains('…'), "{line}");
    std::fs::remove_dir_all(dir).unwrap();
}
