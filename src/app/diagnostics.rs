use super::*;

impl App {
    /// `:info` — a runtime diagnostics view: version/build, config sources,
    /// live cluster identity, discovery/metrics status, watch health, API
    /// request latency, logging, and the directories sofka uses.
    ///
    /// Identifiers, paths, and counts only. Every value that could carry a
    /// credential — the API server URL, an error string from the client —
    /// goes through [`crate::redact`] on the way in, so a `:info` screen is
    /// safe to paste into an issue.
    pub fn open_info(&mut self) {
        self.set_return_mode();
        let mut lines = crate::diagnostics::version_lines();

        lines.push(String::new());
        lines.push("Cluster".into());
        lines.push(format!("  connected:   {}", self.cluster.connected));
        lines.push(format!(
            "  context:     {}",
            crate::diagnostics::safe_or(&self.cluster.context, "(none)")
        ));
        lines.push(format!(
            "  cluster:     {}",
            crate::diagnostics::safe_or(&self.cluster.cluster_name, "(unknown)")
        ));
        lines.push(format!(
            "  api server:  {}",
            crate::diagnostics::safe_or(&self.cluster.cluster_url, "(unknown)")
        ));
        lines.push(format!(
            "  k8s rev:     {}",
            crate::diagnostics::safe_or(&self.cluster.server_version, "(unknown)")
        ));
        lines.push(format!(
            "  namespace:   {}",
            if self.namespace.is_empty() {
                "(all)"
            } else {
                &self.namespace
            }
        ));
        lines.push(format!(
            "  discovery:   {} resource kinds",
            self.cluster.catalog.len()
        ));
        if let Some(note) = &self.cluster.discovery_fallback {
            lines.push(format!("    • {}", crate::diagnostics::safe(note)));
        }
        for w in &self.cluster.discovery_warnings {
            lines.push(format!("    • {}", crate::diagnostics::safe(w)));
        }
        lines.push(format!(
            "  metrics API: {}",
            if self.metrics_seen {
                "available (data received)"
            } else {
                "no data yet"
            }
        ));
        if let Some(e) = &self.metrics_error {
            lines.push(format!(
                "  metrics poll error: {}",
                crate::diagnostics::safe(e)
            ));
        }

        lines.push(String::new());
        lines.push("Watch health".into());
        lines.push(format!("  errors:     {}", self.watch_errors));
        lines.push(format!("  reconnects: {}", self.watch_reconnects));
        match &self.last_error {
            Some(e) => lines.push(format!("  last error: {}", crate::diagnostics::safe(e))),
            None => lines.push("  last error: none".into()),
        }

        lines.push(String::new());
        lines.push("Actions".into());
        match &self.last_action_error {
            Some(e) => lines.push(format!("  last failure: {}", crate::diagnostics::safe(e))),
            None => lines.push("  last failure: none".into()),
        }

        let latency = crate::diagnostics::latency_lines();
        if !latency.is_empty() {
            lines.push(String::new());
            lines.extend(latency);
        }

        lines.push(String::new());
        lines.extend(crate::diagnostics::config_source_lines(
            &self.config,
            &self.cluster.context,
            &self.cluster.cluster_name,
        ));

        lines.push(String::new());
        lines.push("Active config".into());
        lines.push(format!(
            "  skin:       {}",
            self.active_skin.as_deref().unwrap_or("auto")
        ));
        lines.push(format!("  readonly:   {}", self.readonly));
        lines.push(format!("  aliases:    {}", self.user_aliases.len()));
        lines.push(crate::diagnostics::named_line(
            "plugins",
            self.plugins.iter().map(|p| p.name.as_str()),
            self.plugins.len(),
        ));
        lines.push(crate::diagnostics::named_line(
            "views",
            self.user_views.keys().map(String::as_str),
            self.user_views.len(),
        ));
        lines.push(format!("  bookmarks:  {}", self.bookmarks.len()));
        lines.push(format!("  guardrails: {}", self.guardrails.len()));
        lines.push(format!("  warnings:   {}", self.config_warnings.len()));

        lines.push(String::new());
        lines.extend(crate::diagnostics::logging_lines());

        lines.push(String::new());
        lines.extend(crate::diagnostics::directory_lines());
        if let Some(e) = &self.last_state_write_error {
            lines.push(format!(
                "  last state write error: {}",
                crate::diagnostics::safe(e)
            ));
        }

        if !self.config_warnings.is_empty() {
            lines.push(String::new());
            lines.extend(crate::diagnostics::warning_lines(&self.config_warnings));
        }

        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: "diagnostics (:info)".into(),
            redact_header: true,
            lines: lines
                .into_iter()
                .map(|line| crate::diagnostics::safe(&line).into_owned())
                .collect(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }

    /// Startup warnings go to stderr, which the TUI hides at once, so point at
    /// `:config` from the status line instead.
    pub fn flash_config_warnings(&mut self) {
        let n = self.config_warnings.len();
        if n == 0 || self.flash_err {
            return;
        }
        self.flash_warn(&format!(
            "config loaded with {n} warning(s) — :config for details"
        ));
    }

    pub fn flash_discovery_warnings(&mut self) {
        let n = self.cluster.discovery_warnings.len();
        if n == 0 || self.flash_err {
            return;
        }
        let noun = if n == 1 { "group" } else { "groups" };
        self.flash_warn(&format!(
            "API discovery could not read {n} API {noun}. Refer to :info for details."
        ));
    }
}
