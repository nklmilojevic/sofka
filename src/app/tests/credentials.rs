use super::*;
use k8s_openapi::jiff::SignedDuration;

fn expiry() -> Timestamp {
    "2026-10-01T16:22:39Z".parse().unwrap()
}

fn client_until(valid_until: Timestamp) -> kube::Client {
    let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    kube::Client::try_from(config)
        .unwrap()
        .with_valid_until(Some(valid_until))
}

/// A pod view whose client holds an exec certificate that expires at
/// [`expiry`], issued by a plugin that now fails as an expired login does.
fn app_with_exec_certificate() -> (App, Receiver<Msg>) {
    app_with_exec_certificate_until(expiry())
}

fn app_with_exec_certificate_until(valid_until: Timestamp) -> (App, Receiver<Msg>) {
    let (mut app, rx) = app_with_pod();
    app.cluster.client = client_until(valid_until);
    let mut config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    config.auth_info.exec = Some(
        serde_json::from_value(json!({
            "apiVersion": "client.authentication.k8s.io/v1",
            "command": "sh",
            "args": ["-c", "echo 'login expired' >&2; exit 1"],
            "interactiveMode": "Never"
        }))
        .unwrap(),
    );
    app.cluster.keep_credential_source(config);
    (app, rx)
}

async fn renewal_result(rx: &mut Receiver<Msg>) -> Msg {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("renewal finished")
            .expect("channel open");
        if matches!(msg, Msg::CredentialsRenewed { .. }) {
            return msg;
        }
    }
}

fn renewed(app: &App, valid_until: Timestamp) -> Msg {
    Msg::CredentialsRenewed {
        attempt: app.credential_attempt.expect("a renewal in flight"),
        result: Ok(Box::new(client_until(valid_until))),
    }
}

fn watch_error(app: &mut App, error: &str) {
    app.handle_msg(Msg::WatchError {
        generation: app.generation,
        error: error.into(),
    });
}

/// Switch to `name`, landing a client whose certificate expires at `expiry`.
fn switch_to(app: &mut App, name: &str, expiry: Timestamp) {
    palette(app, &format!("ctx {name}"));
    let mut cluster = Cluster::fake();
    cluster.context = name.into();
    cluster.client = client_until(expiry);
    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: name.into(),
        result: Ok(Box::new(cluster)),
    });
    assert_eq!(app.cluster.context, name);
}

#[tokio::test]
async fn renewal_waits_until_the_certificate_nears_expiry() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry() - SignedDuration::from_mins(5));
    assert!(app.credential_attempt.is_none());

    app.renew_credentials_at(expiry() - SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn clients_without_an_exec_certificate_are_never_renewed() {
    let (mut app, _rx) = app_with_pod();
    app.renew_credentials_at(Timestamp::now() + SignedDuration::from_hours(24 * 365 * 100));
    assert!(app.credential_attempt.is_none());
}

#[tokio::test]
async fn a_renewed_certificate_replaces_the_client_and_restarts_the_watch() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let generation = app.generation;
    let later = expiry() + SignedDuration::from_hours(2);

    app.handle_msg(renewed(&app, later));

    assert_eq!(app.cluster.credential_expiry(), Some(later));
    assert!(app.generation > generation);
    assert_eq!(app.flash, "renewed cluster credentials");
    assert!(app.credential_attempt.is_none());
}

#[tokio::test]
async fn the_same_certificate_again_keeps_the_watch_and_retries_later() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let generation = app.generation;

    app.handle_msg(renewed(&app, expiry()));

    assert_eq!(app.cluster.credential_expiry(), Some(expiry()));
    assert_eq!(app.generation, generation);
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(10));
    assert!(app.credential_attempt.is_none(), "retries wait");
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn a_failed_renewal_explains_the_watch_failure_and_retries_later() {
    let (mut app, mut rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let msg = renewal_result(&mut rx).await;
    app.handle_msg(msg);

    assert!(app.flash_err);
    assert!(
        app.flash
            .starts_with("credential renewal failed: Authentication command failed"),
        "{}",
        app.flash
    );
    watch_error(
        &mut app,
        "failed to start watching object: ServiceError: client error (SendRequest)",
    );
    assert!(
        app.flash.contains("Log in with your credential provider"),
        "{}",
        app.flash
    );

    app.renew_credentials_at(expiry() + SignedDuration::from_secs(10));
    assert!(app.credential_attempt.is_none(), "retries wait");
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn renewal_under_an_overlay_restarts_the_watch_back_on_the_table() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    let generation = app.generation;
    let later = expiry() + SignedDuration::from_hours(2);

    app.handle_msg(renewed(&app, later));
    assert_eq!(app.cluster.credential_expiry(), Some(later));
    assert_eq!(app.generation, generation);

    app.handle_key(press(KeyCode::Esc)).unwrap();
    app.renew_credentials_at(expiry());
    assert!(app.generation > generation);
}

#[tokio::test]
async fn a_renewal_for_a_replaced_client_is_dropped() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let msg = renewed(&app, expiry() + SignedDuration::from_hours(2));
    app.all_contexts = vec!["test".into(), "west".into()];
    // Back on the same context, with a client whose certificate expires at
    // the same time as the one the renewal was for.
    switch_to(&mut app, "west", expiry());
    switch_to(&mut app, "test", expiry());
    assert!(app.credential_attempt.is_none());
    let generation = app.generation;

    app.handle_msg(msg);
    assert_eq!(app.cluster.credential_expiry(), Some(expiry()));
    assert_eq!(app.generation, generation);
}

#[tokio::test]
async fn a_failed_renewal_before_expiry_keeps_watch_errors_as_they_are() {
    let valid_until = Timestamp::now() + SignedDuration::from_secs(30);
    let (mut app, mut rx) = app_with_exec_certificate_until(valid_until);
    app.renew_credentials_at(valid_until);
    let msg = renewal_result(&mut rx).await;
    app.handle_msg(msg);
    assert!(
        app.flash.starts_with("credential renewal failed"),
        "{}",
        app.flash
    );

    watch_error(&mut app, "connection refused");
    assert_eq!(app.flash, "watch failed; retrying: connection refused");
}

#[tokio::test]
async fn a_recovered_watch_forgets_the_failed_renewal() {
    let (mut app, mut rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let msg = renewal_result(&mut rx).await;
    app.handle_msg(msg);
    app.handle_msg(Msg::WatchRecovered {
        generation: app.generation,
    });

    watch_error(&mut app, "connection refused");
    assert_eq!(app.flash, "watch failed; retrying: connection refused");
}
