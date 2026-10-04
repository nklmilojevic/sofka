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
pub(super) fn fresh() -> Timestamp {
    Timestamp::now() + SignedDuration::from_hours(2)
}

const INSTALLED: &[u8] = b"installed certificate";
const RENEWED: &[u8] = b"renewed certificate";
const SEND_REQUEST: &str =
    "failed to start watching object: ServiceError: client error (SendRequest)";
const LOGIN: &str = "Log in with your credential provider";

fn client_until(valid_until: Timestamp) -> kube::Client {
    let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    kube::Client::try_from(config)
        .unwrap()
        .with_valid_until(Some(valid_until))
}

fn token_client() -> kube::Client {
    let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
    kube::Client::try_from(config).unwrap()
}

/// A pod view whose client holds an exec certificate that expires at
/// [`expiry`], issued by a plugin that now fails as an expired login does.
fn app_with_exec_certificate() -> (App, Receiver<Msg>) {
    app_with_exec_certificate_until(expiry())
}

fn app_with_exec_certificate_until(valid_until: Timestamp) -> (App, Receiver<Msg>) {
    let (mut app, rx) = app_with_pod();
    give_exec_certificate(&mut app, valid_until);
    (app, rx)
}

/// Connect `app` through an exec plugin whose certificate expires at
/// `valid_until`, and which now fails as an expired login does.
pub(super) fn give_exec_certificate(app: &mut App, valid_until: Timestamp) {
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

fn renewed_token(app: &App) -> Msg {
    Msg::CredentialsRenewed {
        attempt: app.credential_attempt.expect("a renewal in flight"),
        result: Ok(Box::new(ExecClient::token(token_client()))),
    }
}

/// The plugin runs and fails, as with an expired login.
pub(super) async fn fail_renewal(app: &mut App, rx: &mut Receiver<Msg>, at: Timestamp) {
    app.renew_credentials_at(at);
    assert!(app.credential_attempt.is_some());
    let msg = renewal_result(rx).await;
    app.handle_msg(msg);
    assert!(
        app.flash.starts_with("credential renewal failed"),
        "{}",
        app.flash
    );
}

/// A watch error the API server answered, such as a forbidden resource.
fn answered(app: &mut App, error: &str) {
    app.handle_msg(Msg::WatchError {
        generation: app.generation,
        error: error.into(),
        failure: WatchFailure::Response,
    });
}

/// A watch request that got no answer, as through an outage.
fn unanswered(app: &mut App, error: &str) {
    app.handle_msg(Msg::WatchError {
        generation: app.generation,
        error: error.into(),
        failure: WatchFailure::NoResponse,
    });
}

/// A watch error the server's refusal of the credentials caused.
fn refused(app: &mut App, error: &str) {
    app.handle_msg(Msg::WatchError {
        generation: app.generation,
        error: error.into(),
        failure: WatchFailure::CredentialsRefused,
    });
}

fn recovered(app: &mut App) {
    app.handle_msg(Msg::WatchRecovered {
        generation: app.generation,
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
    refused(&mut app, "ApiError: Unauthorized");
    app.renew_credentials_at(Timestamp::now());
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
async fn a_failed_renewal_explains_refused_requests_and_retries_later() {
    let (mut app, mut rx) = app_with_exec_certificate();
    fail_renewal(&mut app, &mut rx, expiry()).await;
    assert!(app.flash_err);
    assert!(
        app.flash
            .starts_with("credential renewal failed: Authentication command failed"),
        "{}",
        app.flash
    );

    // With the certificate expired, a refusal or a request that got no
    // answer keeps its own text and carries the renewal failure after it.
    for failure in [WatchFailure::CredentialsRefused, WatchFailure::NoResponse] {
        app.handle_msg(Msg::WatchError {
            generation: app.generation,
            error: SEND_REQUEST.into(),
            failure,
        });
        assert!(
            app.flash
                .starts_with(&format!("watch failed; retrying: {SEND_REQUEST}; ")),
            "{failure:?}: {}",
            app.flash
        );
        assert!(app.flash.contains(LOGIN), "{failure:?}: {}", app.flash);
    }
    // A missing permission is the API server's answer, not the credentials.
    answered(&mut app, "ApiError: pods is forbidden: Forbidden");
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
async fn a_failed_renewal_before_expiry_leaves_unrelated_failures_alone() {
    let valid_until = Timestamp::now() + SignedDuration::from_secs(30);
    let (mut app, mut rx) = app_with_exec_certificate_until(valid_until);
    fail_renewal(&mut app, &mut rx, valid_until).await;

    unanswered(&mut app, SEND_REQUEST);
    assert_eq!(app.flash, format!("watch failed; retrying: {SEND_REQUEST}"));
    refused(&mut app, "ApiError: Unauthorized");
    assert!(app.flash.contains(LOGIN), "{}", app.flash);
}

#[tokio::test]
async fn an_expired_certificate_from_the_plugin_keeps_the_login_hint() {
    let (mut app, mut rx) = app_with_exec_certificate();
    fail_renewal(&mut app, &mut rx, expiry()).await;
    app.renew_credentials_at(expiry() + SignedDuration::from_secs(30));
    let generation = app.generation;

    app.handle_msg(renewed(&app, expiry(), INSTALLED));

    assert_eq!(app.generation, generation);
    refused(&mut app, SEND_REQUEST);
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
    refused(&mut app, SEND_REQUEST);
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
async fn a_recovered_watch_forgets_the_failed_renewal() {
    let (mut app, mut rx) = app_with_exec_certificate();
    fail_renewal(&mut app, &mut rx, expiry()).await;
    recovered(&mut app);

    refused(&mut app, SEND_REQUEST);
    assert_eq!(app.flash, format!("watch failed; retrying: {SEND_REQUEST}"));
}

#[tokio::test]
async fn refused_credentials_renew_a_certificate_before_its_expiry() {
    let now = Timestamp::now();
    let (mut app, _rx) = app_with_exec_certificate_until(fresh());
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_none());

    // An outage or a forbidden resource is not the credentials' fault.
    unanswered(&mut app, SEND_REQUEST);
    answered(&mut app, "ApiError: pods is forbidden: Forbidden");
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_none());

    // A TLS alert against the certificate, or an Unauthorized response.
    refused(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn refused_credentials_retry_on_the_timer_until_the_watch_recovers() {
    let now = Timestamp::now();
    let (mut app, mut rx) = app_with_exec_certificate_until(fresh());
    refused(&mut app, SEND_REQUEST);
    fail_renewal(&mut app, &mut rx, now).await;

    // No further watch error is needed: the watch backs off between them.
    app.renew_credentials_at(now + SignedDuration::from_secs(10));
    assert!(app.credential_attempt.is_none(), "retries wait");
    app.renew_credentials_at(now + SignedDuration::from_secs(30));
    assert!(app.credential_attempt.is_some());
    let msg = renewal_result(&mut rx).await;
    app.handle_msg(msg);

    recovered(&mut app);
    app.renew_credentials_at(now + SignedDuration::from_secs(60));
    assert!(app.credential_attempt.is_none());
}

#[tokio::test]
async fn a_refused_replacement_certificate_is_renewed_again() {
    let now = Timestamp::now();
    let (mut app, _rx) = app_with_exec_certificate_until(fresh());
    refused(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now);
    app.handle_msg(renewed(&app, fresh(), RENEWED));

    refused(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_some());
}

#[tokio::test]
async fn a_recovered_watch_keeps_its_client_when_a_refusal_renewal_lands() {
    let now = Timestamp::now();
    let (mut app, _rx) = app_with_exec_certificate_until(fresh());
    refused(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now);
    let msg = renewed(&app, fresh(), RENEWED);
    recovered(&mut app);
    let generation = app.generation;
    let installed = app.cluster.credential_expiry();

    app.handle_msg(msg);

    assert_eq!(app.generation, generation);
    assert_eq!(app.cluster.credential_expiry(), installed);
}

#[tokio::test]
async fn a_recovered_watch_still_takes_a_certificate_renewed_for_expiry() {
    let valid_until = Timestamp::now() + SignedDuration::from_secs(30);
    let (mut app, _rx) = app_with_exec_certificate_until(valid_until);
    app.renew_credentials_at(valid_until);
    let msg = renewed(&app, fresh(), RENEWED);
    recovered(&mut app);
    let generation = app.generation;

    app.handle_msg(msg);

    assert!(app.generation > generation);
}

#[tokio::test]
async fn a_token_from_the_plugin_replaces_the_client_and_renews_when_refused() {
    let (mut app, _rx) = app_with_exec_certificate();
    app.renew_credentials_at(expiry());
    let generation = app.generation;

    app.handle_msg(renewed_token(&app));
    assert!(app.generation > generation);
    assert_eq!(app.cluster.credential_expiry(), None);

    let now = Timestamp::now();
    app.renew_credentials_at(now);
    assert!(
        app.credential_attempt.is_none(),
        "a token has no expiry to renew at"
    );
    unanswered(&mut app, SEND_REQUEST);
    app.renew_credentials_at(now);
    assert!(
        app.credential_attempt.is_none(),
        "an outage is not a refusal"
    );
    refused(&mut app, "ApiError: Unauthorized");
    app.renew_credentials_at(now);
    assert!(app.credential_attempt.is_some());
}
