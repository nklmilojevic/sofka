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
    let (mut app, rx) = app_with_pod();
    app.cluster.client = client_until(expiry());
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
        context: app.cluster.context.clone(),
        expiry: expiry(),
        result: Ok(Box::new(client_until(valid_until))),
    }
}

#[tokio::test]
async fn renewal_waits_until_the_certificate_nears_expiry() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry() - SignedDuration::from_mins(5));
    assert!(!app.credential_renewing);

    app.renew_credentials_at(expiry() - SignedDuration::from_secs(30));
    assert!(app.credential_renewing);
}

#[tokio::test]
async fn clients_without_an_exec_certificate_are_never_renewed() {
    let (mut app, _rx) = app_with_pod();
    app.renew_credentials_at(Timestamp::now() + SignedDuration::from_hours(24 * 365 * 100));
    assert!(!app.credential_renewing);
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
    assert!(!app.credential_renewing);
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
    assert!(!app.credential_renewing, "retries wait");
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    assert!(app.credential_renewing);
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
    app.handle_msg(Msg::WatchError {
        generation: app.generation,
        error: "failed to start watching object: ServiceError: client error (SendRequest)".into(),
    });
    assert!(
        app.flash.contains("Log in with your credential provider"),
        "{}",
        app.flash
    );

    app.renew_credentials_at(expiry() + SignedDuration::from_secs(10));
    assert!(!app.credential_renewing, "retries wait");
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    assert!(app.credential_renewing);
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
    palette(&mut app, "ctx west");
    let mut cluster = Cluster::fake();
    cluster.context = "west".into();
    app.handle_msg(Msg::ContextSwitched {
        generation: app.generation,
        name: "west".into(),
        result: Ok(Box::new(cluster)),
    });
    assert!(!app.credential_renewing);

    app.handle_msg(msg);
    assert_eq!(app.cluster.context, "west");
    assert_eq!(app.cluster.credential_expiry(), None);
}
