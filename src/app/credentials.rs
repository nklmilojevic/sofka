//! Renew a client certificate issued by a kubeconfig exec plugin.
//!
//! kube-rs refreshes exec tokens per request, but a certificate is part of the
//! TLS configuration and stays fixed for the client's lifetime. Like
//! client-go, run the plugin again shortly before the declared expiry, swap
//! in a client built from the new certificate, and restart the watch on it.

use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::Client;

use super::{App, Msg};

/// How long before expiry to renew, leaving room for clock skew.
const RENEW_BEFORE: SignedDuration = SignedDuration::from_secs(60);
/// Pause between attempts. A plugin may return its cached certificate until
/// it expires, or fail until the user logs in again.
const RETRY_AFTER: SignedDuration = SignedDuration::from_secs(30);

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
        result: Result<Box<Client>, String>,
    ) {
        // A context switch replaced the client this renewal was for, even
        // when it reconnected to the same context with the same expiry.
        if self.credential_attempt != Some(attempt) {
            return;
        }
        self.credential_attempt = None;
        let context = self.cluster.context.clone();
        let client = match result {
            Ok(client) => client,
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
        self.credential_error = None;
        // The plugin handed back the certificate the client already has.
        if client
            .valid_until()
            .is_none_or(|renewed| Some(renewed) <= self.cluster.credential_expiry())
        {
            return;
        }
        crate::log_info!(
            "cluster.credentials.renewed",
            context = context,
            expires = client
                .valid_until()
                .map(|t| t.to_string())
                .unwrap_or_default()
        );
        self.cluster.client = *client;
        self.credential_retry_at = None;
        self.resume_pending = Some("renewed cluster credentials");
        self.restart_pending_watch();
    }

    /// The failed renewal's message, while the certificate it was meant to
    /// replace has expired. Before then the old certificate still works and
    /// a watch failure has some other cause.
    pub(super) fn expired_credential_error(&self) -> Option<String> {
        let expiry = self.cluster.credential_expiry()?;
        if Timestamp::now() < expiry {
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
