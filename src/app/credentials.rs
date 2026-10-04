//! Renew a client certificate issued by a kubeconfig exec plugin.
//!
//! kube-rs refreshes exec tokens per request, but a certificate is part of the
//! TLS configuration and stays fixed for the client's lifetime. Like
//! client-go, run the plugin again shortly before the declared expiry, swap
//! in a client built from the new certificate, and restart the watch on it.

use super::{App, Msg};
use crate::k8s::ExecClient;
use k8s_openapi::jiff::{SignedDuration, Timestamp};

/// How long before expiry to renew, leaving room for clock skew.
const RENEW_BEFORE: SignedDuration = SignedDuration::from_secs(60);
/// Pause between attempts. A plugin may return its cached certificate until
/// it expires, or fail until the user logs in again.
const RETRY_AFTER: SignedDuration = SignedDuration::from_secs(30);
const EXPIRED: &str =
    "The client certificate has expired. Log in with your credential provider, then retry.";

/// A request the server or the TLS handshake refused before any API answer:
/// what an expired client certificate produces.
fn is_authentication_failure(error: &str) -> bool {
    error.contains("client error (") || error.to_ascii_lowercase().contains("unauthorized")
}

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
        let Some(expiry) = self.cluster.credential_expiry() else {
            return;
        };
        if now.duration_until(expiry) > RENEW_BEFORE {
            return;
        }
        let Some(renew) = self.cluster.credential_renewal() else {
            return;
        };
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
        // when it reconnected to the same context with the same expiry.
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
        // as a plugin that caches it until expiry does.
        if self.cluster.has_certificate_of(&renewed) {
            return;
        }
        crate::log_info!(
            "cluster.credentials.renewed",
            context = context,
            expires = expires.map(|t| t.to_string()).unwrap_or_default()
        );
        self.cluster.install_exec_client(*renewed);
        self.credential_error = None;
        self.credential_retry_at = None;
        self.resume_pending = Some("renewed cluster credentials");
        self.restart_pending_watch();
    }

    /// The failed renewal's message for a watch `error` the expired
    /// certificate explains: the TLS handshake or an unauthenticated
    /// response. Before expiry the old certificate still works, and other
    /// failures, such as a forbidden resource, keep their own message.
    pub(super) fn credential_error_for(&self, error: &str) -> Option<String> {
        let expiry = self.cluster.credential_expiry()?;
        if Timestamp::now() < expiry || !is_authentication_failure(error) {
            return None;
        }
        self.credential_error.clone()
    }

    /// Forget a renewal that belongs to the client a context switch replaced.
    pub(super) fn reset_credential_renewal(&mut self) {
        self.credential_attempt = None;
        self.credential_retry_at = None;
        self.credential_error = None;
    }
}
