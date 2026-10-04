//! Renew a client certificate issued by a kubeconfig exec plugin.
//!
//! kube-rs refreshes exec tokens per request, but a certificate is part of the
//! TLS configuration and stays fixed for the client's lifetime. Like
//! client-go, run the plugin again shortly before the declared expiry, swap
//! in a client built from the new certificate, and restart the watch on it.
//! The server refusing the client's credentials, with an Unauthorized
//! response or a TLS alert against its certificate, renews it too. That
//! covers clock skew, a revoked certificate, and a plugin that switched to a
//! token kube-rs cannot refresh because it has no expiry. Either trigger
//! keeps renewing every 30 seconds until the watch recovers or a new client
//! is installed.

use super::{App, Msg, WatchFailure};
use crate::k8s::ExecClient;

use k8s_openapi::jiff::{SignedDuration, Timestamp};

/// How long before expiry to renew, leaving room for clock skew.
const RENEW_BEFORE: SignedDuration = SignedDuration::from_secs(60);
/// Pause between attempts. A plugin may return its cached certificate until
/// it expires, or fail until the user logs in again.
const RETRY_AFTER: SignedDuration = SignedDuration::from_secs(30);
const EXPIRED: &str =
    "The client certificate has expired. Log in with your credential provider, then retry.";

impl App {
    pub fn renew_credentials(&mut self) {
        self.renew_credentials_at(Timestamp::now());
    }

    pub(super) fn renew_credentials_at(&mut self, now: Timestamp) {
        self.restart_pending_watch();
        if self.credential_attempt.is_some()
            || self.context_switch_target.is_some()
            || !self.cluster.connected
            || self.credential_retry_at.is_some_and(|at| now < at)
        {
            return;
        }
        let expiring = self
            .cluster
            .credential_expiry()
            .is_some_and(|expiry| now.duration_until(expiry) <= RENEW_BEFORE);
        if !expiring && !self.credential_rejected {
            return;
        }
        let Some(renew) = self.cluster.credential_renewal() else {
            return;
        };
        self.credential_attempt_for_expiry = expiring;
        self.credential_attempts += 1;
        let attempt = self.credential_attempts;
        self.credential_attempt = Some(attempt);
        self.credential_retry_at = Some(now + RETRY_AFTER);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(renew).await {
                Ok(Ok(client)) => Ok(Box::new(client)),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(Msg::CredentialsRenewed { attempt, result }).await;
        });
    }

    pub(super) fn apply_renewed_credentials(
        &mut self,
        attempt: u64,
        result: Result<Box<ExecClient>, String>,
    ) {
        // A context switch replaced the client this renewal was for, even
        // when it reconnected to the same context with the same expiry, or
        // the watch recovered and no longer needs it.
        if self.credential_attempt != Some(attempt) {
            return;
        }
        self.credential_attempt = None;
        let context = self.cluster.context.clone();
        let renewed = match result {
            Ok(renewed) => renewed,
            Err(error) => {
                crate::log_warn!(
                    "cluster.credentials.failed",
                    context = context,
                    error = error
                );
                self.flash_warn(&format!("credential renewal failed: {error}"));
                self.credential_error = Some(error);
                return;
            }
        };
        let now = Timestamp::now();
        let expires = *renewed.client.valid_until();
        if expires.is_some_and(|expires| expires <= now) {
            // Nothing usable came back: keep explaining why requests fail.
            crate::log_warn!(
                "cluster.credentials.expired",
                context = context,
                expires = expires.map(|t| t.to_string()).unwrap_or_default()
            );
            self.credential_error
                .get_or_insert_with(|| EXPIRED.to_string());
            return;
        }
        // The plugin handed back the certificate the client already sends,
        // as a plugin that caches it until expiry does. A token is new each
        // time and always replaces the client.
        if self.cluster.has_certificate_of(&renewed) {
            return;
        }
        crate::log_info!(
            "cluster.credentials.renewed",
            context = context,
            expires = expires.map(|t| t.to_string()).unwrap_or_default()
        );
        self.cluster.install_exec_client(*renewed);
        self.credential_rejected = false;
        self.credential_error = None;
        self.credential_retry_at = None;
        self.resume_pending = Some("renewed cluster credentials");
        self.restart_pending_watch();
    }

    /// A watch failed. Renew on the next tick when the server refused the
    /// credentials of a client an exec plugin issued.
    pub(super) fn note_watch_failure(&mut self, failure: WatchFailure) {
        if failure == WatchFailure::CredentialsRefused && self.cluster.renews_credentials() {
            self.credential_rejected = true;
        }
    }

    /// The watch works again, so the credentials are fine. A renewal that a
    /// refusal started is no longer wanted: replacing the client now would
    /// only restart a healthy watch. One for an expiring certificate is.
    pub(super) fn note_watch_recovered(&mut self) {
        self.credential_rejected = false;
        self.credential_error = None;
        if !self.credential_attempt_for_expiry {
            self.credential_attempt = None;
        }
    }

    /// A watch `error` with the failed renewal's message after it, when the
    /// credentials may explain the failure: the server refused them, or the
    /// certificate has expired and the request got no answer. An API error,
    /// such as a forbidden resource, stays as it is.
    pub(super) fn credential_error_for(
        &self,
        error: &str,
        failure: WatchFailure,
    ) -> Option<String> {
        let expired = self
            .cluster
            .credential_expiry()
            .is_some_and(|expiry| Timestamp::now() >= expiry);
        let explains = match failure {
            WatchFailure::CredentialsRefused => true,
            WatchFailure::NoResponse => expired,
            WatchFailure::Response => false,
        };
        if !explains {
            return None;
        }
        let hint = self.credential_error.as_ref()?;
        Some(format!("{error}; credential renewal failed: {hint}"))
    }

    /// Forget a renewal that belongs to the client a context switch replaced.
    pub(super) fn reset_credential_renewal(&mut self) {
        self.credential_rejected = false;
        self.credential_attempt = None;
        self.credential_retry_at = None;
        self.credential_error = None;
    }
}
