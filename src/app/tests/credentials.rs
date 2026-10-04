use super::*;
use crate::k8s::ExecClient;
use k8s_openapi::jiff::SignedDuration;
use std::sync::LazyLock;

/// When the installed certificate expired.
fn expiry() -> Timestamp {
    static EXPIRY: LazyLock<Timestamp> =
        LazyLock::new(|| Timestamp::now() - SignedDuration::from_hours(1));
    *EXPIRY
}

/// The expiry of a freshly issued certificate.
fn fresh() -> Timestamp {
    Timestamp::now() + SignedDuration::from_hours(2)
}

const INSTALLED: &[u8] = b"installed certificate";
const RENEWED: &[u8] = b"renewed certificate";
const SEND_REQUEST: &str =
    "failed to start watching object: ServiceError: client error (SendRequest)";

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
    app.cluster.keep_credential_source(config, INSTALLED);
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

fn renewed(app: &App, valid_until: Timestamp, certificate: &[u8]) -> Msg {
    Msg::CredentialsRenewed {
        attempt: app.credential_attempt.expect("a renewal in flight"),
        result: Ok(Box::new(ExecClient::new(
            client_until(valid_until),
            certificate,
        ))),
    }
}

async fn fail_renewal(app: &mut App, rx: &mut Receiver<Msg>, at: Timestamp) {
    app.renew_credentials_at(at);
    let msg = renewal_result(rx).await;
    app.handle_msg(msg);
    assert!(
        app.flash.starts_with("credential renewal failed"),
        "{}",
        app.flash
    );
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
    let later = fresh();

    app.handle_msg(renewed(&app, later, RENEWED));

    assert_eq!(app.cluster.credential_expiry(), Some(later));
    assert!(app.generation > generation);
    assert_eq!(app.flash, "renewed cluster credentials");
    assert!(app.credential_attempt.is_none());
}

#[tokio::test]
async fn a_different_certificate_with_the_same_expiry_replaces_the_client() {
    let valid_until = Timestamp::now() + SignedDuration::from_secs(30);
    let (mut app, _rx) = app_with_exec_certificate_until(valid_until);
    app.renew_credentials_at(valid_until);
    let generation = app.generation;

    app.handle_msg(renewed(&app, valid_until, RENEWED));

    assert!(app.generation > generation);
    assert_eq!(app.flash, "renewed cluster credentials");
}

#[tokio::test]
async fn the_same_certificate_again_keeps_the_watch_and_retries_later() {
    let valid_until = Timestamp::now() + SignedDuration::from_secs(30);
    let (mut app, _rx) = app_with_exec_certificate_until(valid_until);
    app.renew_credentials_at(valid_until);
    let generation = app.generation;

    app.handle_msg(renewed(&app, valid_until, INSTALLED));

    assert_eq!(app.generation, generation);
    app.renew_credentials_at(valid_until + SignedDuration::from_secs(10));
    assert!(app.credential_attempt.is_none(), "retries wait");
    app.renew_credentials_at(valid_until + SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn a_failed_renewal_explains_authentication_failures_and_retries_later() {
    let (mut app, mut rx) = app_with_exec_certificate();
    fail_renewal(&mut app, &mut rx, expiry()).await;
    assert!(app.flash_err);
    assert!(
        app.flash
            .starts_with("credential renewal failed: Authentication command failed"),
        "{}",
        app.flash
    );

    watch_error(&mut app, SEND_REQUEST);
    assert!(
        app.flash.contains("Log in with your credential provider"),
        "{}",
        app.flash
    );
    watch_error(&mut app, "ApiError: Unauthorized: Unauthorized");
    assert!(
        app.flash.contains("Log in with your credential provider"),
        "{}",
        app.flash
    );
    watch_error(&mut app, "ApiError: pods is forbidden: Forbidden");
    assert_eq!(
        app.flash,
        "watch failed; retrying: ApiError: pods is forbidden: Forbidden"
    );

    app.renew_credentials_at(expiry() + SignedDuration::from_secs(10));
    assert!(app.credential_attempt.is_none(), "retries wait");
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn an_expired_certificate_from_the_plugin_keeps_the_login_hint() {
    let (mut app, mut rx) = app_with_exec_certificate();
    fail_renewal(&mut app, &mut rx, expiry()).await;
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    let generation = app.generation;

    app.handle_msg(renewed(&app, expiry(), INSTALLED));

    assert_eq!(app.generation, generation);
    watch_error(&mut app, SEND_REQUEST);
    assert!(
        app.flash.contains("Authentication command failed"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn an_expired_certificate_is_never_installed() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let generation = app.generation;

    app.handle_msg(renewed(&app, expiry(), RENEWED));

    assert_eq!(app.generation, generation);
    watch_error(&mut app, SEND_REQUEST);
    assert!(
        app.flash
            .contains("The client certificate has expired. Log in"),
        "{}",
        app.flash
    );
}

#[tokio::test]
async fn renewal_under_an_overlay_restarts_the_watch_back_on_the_table() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    let generation = app.generation;
    let later = fresh();

    app.handle_msg(renewed(&app, later, RENEWED));
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
    let msg = renewed(&app, fresh(), RENEWED);
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
    fail_renewal(&mut app, &mut rx, valid_until).await;

    watch_error(&mut app, SEND_REQUEST);
    assert_eq!(app.flash, format!("watch failed; retrying: {SEND_REQUEST}"));
}

#[tokio::test]
async fn a_recovered_watch_forgets_the_failed_renewal() {
    let (mut app, mut rx) = app_with_exec_certificate();
    fail_renewal(&mut app, &mut rx, expiry()).await;
    app.handle_msg(Msg::WatchRecovered {
        generation: app.generation,
    });

    watch_error(&mut app, SEND_REQUEST);
    assert_eq!(app.flash, format!("watch failed; retrying: {SEND_REQUEST}"));
}

const UNAUTHORIZED: &str = "failed to start watching object: ApiError: Unauthorized: Unauthorized";

#[tokio::test]
async fn an_unauthenticated_watch_renews_a_certificate_before_its_expiry() {
    let now = Timestamp::now();
    let (mut app, _rx) = app_with_exec_certificate_until(fresh());
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_none());

    watch_error(&mut app, "ApiError: pods is forbidden: Forbidden");
    app.renew_credentials_at(now);
    assert!(
        app.credential_attempt.is_none(),
        "forbidden is not a credential failure"
    );

    watch_error(&mut app, UNAUTHORIZED);
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn a_token_from_the_plugin_replaces_the_client_and_renews_when_rejected() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let generation = app.generation;
    let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    let token = kube::Client::try_from(config).unwrap();

    app.handle_msg(Msg::CredentialsRenewed {
        attempt: app.credential_attempt.unwrap(),
        result: Ok(Box::new(ExecClient::token(token))),
    });
    assert!(app.generation > generation);
    assert_eq!(app.cluster.credential_expiry(), None);

    let now = Timestamp::now();
    app.renew_credentials_at(now);
    assert!(
        app.credential_attempt.is_none(),
        "a token has no expiry to renew at"
    );
    watch_error(&mut app, UNAUTHORIZED);
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn an_unauthenticated_watch_without_an_exec_plugin_renews_nothing() {
    let (mut app, _rx) = app_with_pod();
    watch_error(&mut app, UNAUTHORIZED);
    app.renew_credentials_at(Timestamp::now());
    assert!(app.credential_attempt.is_none());
}

#[tokio::test]
async fn a_certificate_refused_in_the_handshake_renews_before_its_expiry() {
    let now = Timestamp::now();
    let (mut app, mut rx) = app_with_exec_certificate_until(fresh());
    watch_error(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_some());

    // Through an ordinary outage the plugin fails too; that stays quiet.
    let msg = renewal_result(&mut rx).await;
    app.handle_msg(msg);
    assert_eq!(app.flash, format!("watch failed; retrying: {SEND_REQUEST}"));
    watch_error(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now + SignedDuration::from_secs(10));
    assert!(app.credential_attempt.is_none(), "retries wait");
    app.renew_credentials_at(now + SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn a_new_certificate_after_a_handshake_failure_replaces_the_client() {
    let (mut app, _rx) = app_with_exec_certificate_until(fresh());
    watch_error(&mut app, SEND_REQUEST);
    app.renew_credentials_at(Timestamp::now());
    let generation = app.generation;

    app.handle_msg(renewed(&app, fresh(), RENEWED));

    assert!(app.generation > generation);
    assert_eq!(app.flash, "renewed cluster credentials");
}

#[tokio::test]
async fn transport_failures_of_a_token_client_renew_nothing() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    let token = kube::Client::try_from(config).unwrap();
    app.handle_msg(Msg::CredentialsRenewed {
        attempt: app.credential_attempt.unwrap(),
        result: Ok(Box::new(ExecClient::token(token))),
    });

    watch_error(&mut app, SEND_REQUEST);
    app.renew_credentials_at(Timestamp::now());
    assert!(app.credential_attempt.is_none());
}
