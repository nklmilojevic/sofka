use super::*;
use std::time::{Duration, SystemTime};

/// Ticks one second apart on both clocks, then one where the wall clock ran
/// `slept` further than the monotonic clock.
fn tick_after_sleep(app: &mut App, slept: Duration) {
    let mono = Instant::now();
    let wall = SystemTime::now();
    app.detect_resume_at(mono, wall);
    let second = Duration::from_secs(1);
    app.detect_resume_at(mono + second, wall + second + slept);
}

#[tokio::test]
async fn waking_from_sleep_restarts_the_watch() {
    let (mut app, _rx) = app_with_pod();
    let generation = app.generation;
    tick_after_sleep(&mut app, Duration::ZERO);
    assert_eq!(app.generation, generation);

    tick_after_sleep(&mut app, Duration::from_secs(8 * 60 * 60));
    assert!(app.generation > generation);
    assert_eq!(app.flash, "reconnected after sleep");

    let generation = app.generation;
    app.detect_resume();
    assert_eq!(app.generation, generation, "one wake restarts once");
}

#[tokio::test]
async fn small_clock_corrections_do_not_restart_the_watch() {
    let (mut app, _rx) = app_with_pod();
    let generation = app.generation;
    tick_after_sleep(&mut app, Duration::from_secs(5));
    let mono = Instant::now();
    let wall = SystemTime::now();
    app.detect_resume_at(mono, wall);
    app.detect_resume_at(
        mono + Duration::from_secs(1),
        wall - Duration::from_secs(3600),
    );
    assert_eq!(app.generation, generation);
}

#[tokio::test]
async fn waking_under_an_overlay_restarts_once_back_on_the_table() {
    let (mut app, _rx) = app_with_pod();
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.mode, Mode::Help);
    let generation = app.generation;

    tick_after_sleep(&mut app, Duration::from_secs(60));
    assert_eq!(app.generation, generation);
    assert_eq!(app.mode, Mode::Help);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    app.detect_resume();
    assert!(app.generation > generation);
}
