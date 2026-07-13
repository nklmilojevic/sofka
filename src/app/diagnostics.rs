use super::*;

impl App {
    /// `:info` — a runtime diagnostics view: version/build, config sources,
    /// live cluster identity, discovery/metrics status, watch health, and the
    /// directories sofka uses. Never prints credentials, tokens, or Secret
    /// values — only identifiers and counts.
    pub(super) fn open_info(&mut self) {
        self.set_return_mode();
        let mut lines: Vec<String> = Vec::new();

        lines.push(format!("sofka v{}", crate::diagnostics::VERSION));
        lines.push(format!("  build: {}", crate::diagnostics::build_line()));

        lines.push(String::new());
        lines.push("Cluster".into());
        lines.push(format!("  connected:   {}", self.cluster.connected));
        lines.push(format!(
            "  context:     {}",
            blank_as(&self.cluster.context, "(none)")
        ));
        lines.push(format!(
            "  cluster:     {}",
            blank_as(&self.cluster.cluster_name, "(unknown)")
        ));
        lines.push(format!(
            "  api server:  {}",
            blank_as(&self.cluster.cluster_url, "(unknown)")
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
        lines.push(format!(
            "  metrics API: {}",
            if self.metrics_seen {
                "available (data received)"
            } else {
                "no data yet"
            }
        ));

        lines.push(String::new());
        lines.push("Watch health".into());
        lines.push(format!("  errors: {}", self.watch_errors));
        match &self.last_error {
            Some(e) => lines.push(format!("  last error: {e}")),
            None => lines.push("  last error: none".into()),
        }

        lines.push(String::new());
        lines.push("Config sources".into());
        match self.config.base_path() {
            Some(path) => {
                let state = if self.config.has_base() {
                    "loaded"
                } else if path.exists() {
                    "invalid — using defaults"
                } else {
                    "absent — using defaults"
                };
                lines.push(format!("  {} ({state})", path.display()));
            }
            None => lines.push("  no config directory — using defaults".into()),
        }
        for path in self
            .config
            .override_paths(&self.cluster.context, &self.cluster.cluster_name)
        {
            lines.push(format!(
                "  {} ({})",
                path.display(),
                crate::config::file_state(&path)
            ));
        }

        lines.push(String::new());
        lines.push("Active config".into());
        lines.push(format!(
            "  skin:      {}",
            self.active_skin.as_deref().unwrap_or("auto")
        ));
        lines.push(format!("  readonly:  {}", self.readonly));
        lines.push(format!("  aliases:   {}", self.user_aliases.len()));
        lines.push(format!("  plugins:   {}", self.plugins.len()));
        lines.push(format!("  views:     {}", self.user_views.len()));
        lines.push(format!("  bookmarks: {}", self.bookmarks.len()));
        lines.push(format!("  guardrails: {}", self.guardrails.len()));
        lines.push(format!("  warnings:  {}", self.config_warnings.len()));

        lines.push(String::new());
        lines.push("Directories".into());
        lines.push(format!(
            "  state:     {}",
            crate::diagnostics::state_dir().display()
        ));
        lines.push(format!(
            "  snapshots: {}",
            crate::snapshot::snapshots_dir().display()
        ));
        lines.push(format!("  bundles:   {}", std::env::temp_dir().display()));

        if !self.config_warnings.is_empty() {
            lines.push(String::new());
            lines.push(format!("Warnings [{}]", self.config_warnings.len()));
            for w in &self.config_warnings {
                for (i, l) in w.lines().enumerate() {
                    let bullet = if i == 0 { "• " } else { "  " };
                    lines.push(format!("  {bullet}{l}"));
                }
            }
        }

        self.detail = Scrollable {
            title: "diagnostics (:info)".into(),
            lines: lines.into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }
}

fn blank_as<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}
