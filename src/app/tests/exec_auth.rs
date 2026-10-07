use super::*;
use crate::k8s::ConnectError;

fn needs_input() -> ConnectError {
    ConnectError {
        message: "building kube client".into(),
        needs_input: true,
    }
}

fn switch_failed(app: &mut App, name: &str, error: ConnectError) {
    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: name.into(),
        result: Err(error),
    });
}

fn needs_input_watch_error(app: &mut App) {
    app.handle_msg(Msg::WatchError {
        generation: app.generation,
        error: "MFA code required".into(),
        failure: WatchFailure::NeedsInput,
    });
}

#[tokio::test]
async fn a_switch_that_needs_input_offers_to_authenticate() {
    let (mut app, _rx) = test_app();
    switch_failed(&mut app, "eks", needs_input());
    assert_eq!(app.mode, Mode::Confirm);
    assert!(app.confirm_label.contains("'eks' needs terminal input"));
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    let Some(Suspend::Authenticate { context, .. }) = app.pending.take() else {
        panic!("no authentication queued");
    };
    assert_eq!(context, "eks");
}

#[tokio::test]
async fn other_switch_failures_do_not_offer_to_authenticate() {
    let (mut app, _rx) = test_app();
    switch_failed(&mut app, "eks", "connection refused".into());
    assert_ne!(app.mode, Mode::Confirm);
    assert!(app.pending.is_none());
}

#[tokio::test]
async fn declining_before_any_connection_returns_to_the_picker() {
    let (mut app, _rx) = test_app();
    app.cluster.connected = false;
    app.start_context_picker(None);
    switch_failed(&mut app, "eks", needs_input());
    assert_eq!(app.mode, Mode::Confirm);
    app.handle_key(press(KeyCode::Char('n'))).unwrap();
    assert!(app.pending.is_none());
    assert_eq!(app.mode, Mode::Contexts);
    assert!(app.flash.contains("context switch failed"), "{}", app.flash);
}

#[tokio::test]
async fn a_retrying_watch_asks_once_per_connection() {
    let (mut app, _rx) = test_app();
    needs_input_watch_error(&mut app);
    assert_eq!(app.mode, Mode::Confirm);
    assert!(app.confirm_label.contains("'test'"));
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    needs_input_watch_error(&mut app);
    assert_eq!(app.mode, Mode::Table);
    assert!(app.flash.contains("MFA code required"), "{}", app.flash);
}

#[tokio::test]
async fn the_offer_never_interrupts_typing() {
    let (mut app, _rx) = test_app();
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    assert_eq!(app.mode, Mode::Command);
    needs_input_watch_error(&mut app);
    assert_eq!(app.mode, Mode::Command);
}

#[tokio::test]
async fn the_offer_never_interrupts_filtering_the_picker() {
    let (mut app, _rx) = test_app();
    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    for c in "ctx".chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Contexts);
    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    assert!(app.ctx_filtering);
    needs_input_watch_error(&mut app);
    assert_eq!(app.mode, Mode::Contexts);
    assert!(app.ctx_filtering);
}

fn accept(app: &mut App) -> (String, bool) {
    assert_eq!(app.mode, Mode::Confirm);
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    let Some(Suspend::Authenticate { context, switch }) = app.pending.take() else {
        panic!("no authentication queued");
    };
    (context, switch)
}

#[tokio::test]
async fn authenticating_the_live_context_keeps_the_view_and_restarts_the_watch() {
    let (mut app, _rx) = test_app();
    app.switch_kind("deployments");
    app.filter = "api".into();
    let generation = app.generation;
    needs_input_watch_error(&mut app);
    let (context, switch) = accept(&mut app);
    assert_eq!(context, "test");
    assert!(!switch);
    app.authenticated(context, switch, Ok(()));
    assert!(app.context_switch_target.is_none());
    assert_eq!(app.kind_plural, "deployments");
    assert_eq!(app.filter, "api");
    assert!(app.generation > generation, "watch not restarted");
    assert_eq!(app.flash, "authenticated");
}

#[tokio::test]
async fn an_authenticated_retry_still_lands_where_the_switch_was_going() {
    for accepted in [true, false] {
        let (mut app, _rx) = test_app();
        bind_bookmark(&mut app, "services", "west");
        app.handle_key(ctrl(KeyCode::Char('y'))).unwrap();
        assert!(app.pending_bookmark.is_some());
        switch_failed(&mut app, "west", needs_input());
        assert!(app.pending_bookmark.is_some(), "dropped before the answer");
        if accepted {
            let (context, switch) = accept(&mut app);
            assert!(switch);
            app.authenticated(context, switch, Ok(()));
            assert_eq!(
                app.context_switch_target,
                Some((app.generation, "west".to_string()))
            );
            assert!(app.pending_bookmark.is_some(), "retry lost the bookmark");
        } else {
            app.handle_key(press(KeyCode::Char('n'))).unwrap();
            assert!(app.pending_bookmark.is_none(), "declined switch kept it");
        }
    }
}

/// An Argo CD jump reloads the live context on purpose; authenticating that
/// reload retries it rather than only restarting the watch.
#[tokio::test]
async fn an_authenticated_reload_of_the_live_context_is_retried() {
    let (mut app, _rx) = test_app();
    app.pending_bookmark = Some(crate::config::Bookmark {
        name: "bm".into(),
        resource: "services".into(),
        ..Default::default()
    });
    switch_failed(&mut app, "test", needs_input());
    assert!(app.pending_bookmark.is_some());
    let (context, switch) = accept(&mut app);
    app.authenticated(context, switch, Ok(()));
    assert_eq!(
        app.context_switch_target,
        Some((app.generation, "test".to_string()))
    );
    assert!(app.pending_bookmark.is_some());
}

#[tokio::test]
async fn authenticating_another_context_reconnects_and_a_failure_is_shown() {
    let (mut app, _rx) = test_app();
    switch_failed(&mut app, "eks", needs_input());
    let (context, switch) = accept(&mut app);
    app.authenticated(context, switch, Ok(()));
    assert_eq!(
        app.context_switch_target,
        Some((app.generation, "eks".to_string()))
    );

    let (mut app, _rx) = test_app();
    app.authenticated(
        "eks".into(),
        true,
        Err("aws exited with exit status: 255".into()),
    );
    assert!(app.context_switch_target.is_none());
    assert!(
        app.flash.contains("authentication failed: aws exited"),
        "{}",
        app.flash
    );
}
