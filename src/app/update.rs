use super::*;

impl App {
    /// Look up the latest release off the UI thread. `:check-update` passes
    /// `manual`, which skips the daily cache and reports every outcome on the
    /// status bar; the startup check stays silent unless a newer release exists.
    pub fn start_update_check(&mut self, manual: bool) {
        let claim = manual.then(|| self.claim_status("checking for a newer sofka release…"));
        let fetch = self.update_fetcher;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = fetch(manual).await;
            let _ = tx.send(Msg::UpdateCheck { claim, result }).await;
        });
    }

    pub(super) fn finish_update_check(
        &mut self,
        claim: Option<StatusClaim>,
        result: Result<crate::update::Release, String>,
    ) {
        let release = match result {
            Ok(release) => release,
            Err(error) => {
                crate::log_warn!("update.check", error = error);
                if let Some(claim) = claim {
                    self.set_claimed_status(claim, format!("update check failed: {error}"), true);
                }
                return;
            }
        };
        let notice = release
            .is_newer()
            .then(|| crate::update::notice(&release, crate::update::InstallMethod::current()));
        self.latest_release = Some(release);
        match (claim, notice) {
            (Some(claim), Some(notice)) => self.set_claimed_status(claim, notice, false),
            (Some(claim), None) => self.set_claimed_status(
                claim,
                format!(
                    "sofka v{} is the latest release",
                    crate::diagnostics::VERSION
                ),
                false,
            ),
            // The startup check does not replace a warning or a pending
            // operation's status; the header keeps showing the new version.
            (None, Some(notice)) if !self.flash_err && self.status_claim.is_none() => {
                self.set_flash(notice)
            }
            (None, _) => {}
        }
    }

    /// Start from the release the last check on disk found, so `:info` and
    /// the header know it without a request, even with `update_check = false`.
    pub fn load_cached_release(&mut self, path: &std::path::Path) {
        self.latest_release = crate::update::cached(path);
    }

    /// The newer release to advertise in the header, if any.
    pub fn available_update(&self) -> Option<&crate::update::Release> {
        self.latest_release
            .as_ref()
            .filter(|release| release.is_newer())
    }
}
