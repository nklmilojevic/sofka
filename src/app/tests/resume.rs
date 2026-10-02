use super::*;
use crate::app::resume::Clock;
use std::time::Duration;

/// Two ticks one awake second apart, with `slept` passing in between.
pub(super) fn tick_after_sleep(app: &mut App, slept: Duration) {
    let start = Duration::from_secs(1_000);
    app.detect_resume_at(Clock {
        awake: start,
        total: start,
    });
    let second = Duration::from_secs(1);
    app.detect_resume_at(Clock {
        awake: start + second,
        total: start + second + slept,
    });
}

/// One awake second after the previous tick.
fn tick(app: &mut App) {
    let last = app.resume_clock.expect("no previous tick");
    let second = Duration::from_secs(1);
    app.detect_resume_at(Clock {
        awake: last.awake + second,
        total: last.total + second,
    });
}

const NIGHT: Duration = Duration::from_secs(8 * 60 * 60);

#[tokio::test]
async fn waking_from_sleep_restarts_the_watch_once() {
    let (mut app, _rx) = app_with_pod();
    let generation = app.generation;
    tick_after_sleep(&mut app, Duration::ZERO);
    assert_eq!(app.generation, generation);

    tick_after_sleep(&mut app, NIGHT);
    assert!(app.generation > generation);
    assert_eq!(app.flash, "reconnected after sleep");

    let generation = app.generation;
    tick(&mut app);
    assert_eq!(app.generation, generation, "one wake restarts once");
}

#[tokio::test]
async fn short_naps_do_not_restart_the_watch() {
    let (mut app, _rx) = app_with_pod();
    let generation = app.generation;
    tick_after_sleep(&mut app, Duration::from_secs(29));
    assert_eq!(app.generation, generation);
}

#[tokio::test]
async fn the_os_clocks_advance() {
    let (mut app, _rx) = app_with_pod();
    app.detect_resume();
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    assert!(app.resume_clock.is_some());
}

#[tokio::test]
async fn waking_under_an_overlay_restarts_once_back_on_the_table() {
    let (mut app, _rx) = app_with_pod();
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert_eq!(app.mode, Mode::Help);
    let generation = app.generation;

    tick_after_sleep(&mut app, NIGHT);
    assert_eq!(app.generation, generation);
    assert_eq!(app.mode, Mode::Help);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    tick(&mut app);
    assert!(app.generation > generation);
}

#[tokio::test]
async fn waking_during_a_context_switch_keeps_the_switch() {
    for lands in [true, false] {
        let (mut app, _rx) = app_with_pod();
        app.all_contexts = vec!["test".into(), "west".into()];
        palette(&mut app, "ctx west");
        let target = Some((app.generation, "west".to_string()));
        assert_eq!(app.context_switch_target, target);

        tick_after_sleep(&mut app, NIGHT);
        assert_eq!(app.context_switch_target, target, "lands={lands}");

        let mut cluster = Cluster::fake();
        cluster.context = "west".into();
        app.handle_msg(Msg::ContextSwitched {
            generation: app.generation,
            name: "west".into(),
            result: if lands {
                Ok(Box::new(cluster))
            } else {
                Err("unreachable".into())
            },
        });
        let generation = app.generation;
        let flash = app.flash.clone();
        tick(&mut app);
        if lands {
            assert_eq!(app.cluster.context, "west");
            assert_eq!(app.generation, generation, "the new client is fresh");
        } else {
            assert!(app.generation > generation, "the old watch restarts");
            assert_eq!(app.flash, flash, "the failure stays visible");
            assert!(app.flash_err);
        }
    }
}
