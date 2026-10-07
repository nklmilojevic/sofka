use super::*;
use crate::update::Release;

fn release(version: &str) -> Release {
    Release {
        version: version.into(),
        url: format!("https://github.com/nklmilojevic/sofka/releases/tag/v{version}"),
    }
}

async fn finish(app: &mut App, rx: &mut Receiver<Msg>) {
    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("update check did not report")
        .expect("channel closed");
    assert!(matches!(msg, Msg::UpdateCheck { .. }));
    app.handle_msg(msg);
}

#[tokio::test]
async fn check_update_command_reports_a_newer_release() {
    let (mut app, mut rx) = test_app();
    app.update_fetcher = |force| {
        assert!(force, ":check-update skips the daily cache");
        Box::pin(async { Ok(release("999.0.0")) })
    };
    palette(&mut app, "check-update");
    assert!(app.flash.contains("checking"), "{}", app.flash);
    finish(&mut app, &mut rx).await;
    assert!(!app.flash_err);
    assert!(
        app.flash.starts_with(&format!(
            "sofka v999.0.0 is available (running v{})",
            crate::diagnostics::VERSION
        )),
        "{}",
        app.flash
    );
    assert_eq!(app.available_update(), Some(&release("999.0.0")));
}

#[tokio::test]
async fn check_update_command_reports_up_to_date_and_failures() {
    let (mut app, mut rx) = test_app();
    app.update_fetcher = |_force| Box::pin(async { Ok(release(crate::diagnostics::VERSION)) });
    palette(&mut app, "update-check");
    finish(&mut app, &mut rx).await;
    assert_eq!(
        app.flash,
        format!(
            "sofka v{} is the latest release",
            crate::diagnostics::VERSION
        )
    );
    assert_eq!(app.available_update(), None);

    app.update_fetcher = |_force| Box::pin(async { Err("rate limited".to_string()) });
    palette(&mut app, "check-update");
    finish(&mut app, &mut rx).await;
    assert!(app.flash_err);
    assert_eq!(app.flash, "update check failed: rate limited");
}

#[tokio::test]
async fn startup_check_only_speaks_up_for_a_newer_release() {
    let (mut app, mut rx) = test_app();
    app.set_flash("welcome");
    app.update_fetcher = |force| {
        assert!(!force, "the startup check uses the daily cache");
        Box::pin(async { Err("offline".to_string()) })
    };
    app.start_update_check(false);
    finish(&mut app, &mut rx).await;
    assert_eq!(app.flash, "welcome");

    app.update_fetcher = |_force| Box::pin(async { Ok(release(crate::diagnostics::VERSION)) });
    app.start_update_check(false);
    finish(&mut app, &mut rx).await;
    assert_eq!(app.flash, "welcome");

    app.update_fetcher = |_force| Box::pin(async { Ok(release("999.0.0")) });
    app.start_update_check(false);
    finish(&mut app, &mut rx).await;
    assert!(app.flash.contains("v999.0.0 is available"), "{}", app.flash);
}

#[tokio::test]
async fn startup_check_keeps_an_existing_status_message() {
    let (mut app, mut rx) = test_app();
    app.update_fetcher = |_force| Box::pin(async { Ok(release("999.0.0")) });
    app.flash_warn("config warning");
    app.start_update_check(false);
    finish(&mut app, &mut rx).await;
    assert_eq!(app.flash, "config warning");
    assert_eq!(app.available_update(), Some(&release("999.0.0")));
}

#[tokio::test]
async fn info_shows_the_latest_release_and_the_check_setting() {
    let (mut app, mut rx) = test_app();
    palette(&mut app, "info");
    let text = |app: &App| {
        app.detail
            .lines
            .iter()
            .map(|line| line.as_str().to_string())
            .collect::<Vec<_>>()
    };
    let lines = text(&app);
    assert!(
        lines.contains(&"  latest:   not checked".to_string()),
        "{lines:#?}"
    );
    assert!(lines.contains(&"  checks:   off (update_check = false)".to_string()));

    app.handle_key(press(KeyCode::Esc)).unwrap();
    app.update_check = true;
    app.update_fetcher = |_force| Box::pin(async { Ok(release("999.0.0")) });
    palette(&mut app, "check-update");
    finish(&mut app, &mut rx).await;
    palette(&mut app, "info");
    let lines = text(&app);
    assert!(
        lines.contains(&"  latest:   v999.0.0 (newer)".to_string()),
        "{lines:#?}"
    );
    assert!(lines.contains(
        &"  notes:    https://github.com/nklmilojevic/sofka/releases/tag/v999.0.0".to_string()
    ));
    assert!(lines.contains(&"  checks:   daily (update_check = true)".to_string()));
}

#[tokio::test]
async fn info_shows_the_cached_release_with_checks_off() {
    let dir = std::env::temp_dir().join(format!("sofka-update-info-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("update-check.toml");
    std::fs::write(
        &path,
        "checked_at = 1\n[release]\nversion = \"999.0.0\"\nurl = \"https://github.com/nklmilojevic/sofka/releases/tag/v999.0.0\"\n",
    )
    .unwrap();
    let (mut app, _rx) = test_app();
    app.load_cached_release(&path);
    let _ = std::fs::remove_dir_all(&dir);

    palette(&mut app, "info");
    let lines: Vec<String> = app
        .detail
        .lines
        .iter()
        .map(|line| line.as_str().to_string())
        .collect();
    assert!(
        lines.contains(&"  latest:   v999.0.0 (newer)".to_string()),
        "{lines:#?}"
    );
    assert!(lines.contains(&"  checks:   off (update_check = false)".to_string()));
}

#[tokio::test]
async fn compact_header_keeps_the_update_after_the_notice_expires() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut app, mut rx) = test_app();
    app.update_fetcher = |_force| Box::pin(async { Ok(release("999.0.0")) });
    app.handle_key(ctrl(KeyCode::Char('e'))).unwrap();
    assert!(app.compact);
    app.start_update_check(false);
    finish(&mut app, &mut rx).await;
    assert!(app.flash.contains("is available"), "{}", app.flash);

    app.flash_since = Instant::now() - Duration::from_secs(60);
    app.expire_flash();
    assert_eq!(app.flash, "");

    let mut term = Terminal::new(TestBackend::new(120, 10)).unwrap();
    let header = |term: &mut Terminal<TestBackend>, app: &mut App| {
        term.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buffer = term.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect::<String>()
    };
    let line = header(&mut term, &mut app);
    assert!(line.contains("v999.0.0 available"), "{line}");

    app.flash_warn("watch failed");
    let line = header(&mut term, &mut app);
    let warning = line.find("watch failed").expect(&line);
    assert!(warning < line.find("v999.0.0 available").expect(&line));
}
