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
    let Some(Suspend::Authenticate { context }) = app.pending.take() else {
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
async fn authenticating_reconnects_and_a_failure_is_shown() {
    let (mut app, _rx) = test_app();
    switch_failed(&mut app, "eks", needs_input());
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let Some(Suspend::Authenticate { context }) = app.pending.take() else {
        panic!("no authentication queued");
    };
    app.authenticated(context, Ok(()));
    assert_eq!(
        app.context_switch_target,
        Some((app.generation, "eks".to_string()))
    );

    let (mut app, _rx) = test_app();
    app.authenticated("eks".into(), Err("aws exited with exit status: 255".into()));
    assert!(app.context_switch_target.is_none());
    assert!(
        app.flash.contains("authentication failed: aws exited"),
        "{}",
        app.flash
    );
}
