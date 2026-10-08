use super::*;

#[derive(Default)]
pub(super) struct LogLineMeta {
    sort_time: Option<i128>,
    pub(super) pretty: Option<String>,
    record: Option<String>,
    record_severity: Option<crate::logfilter::Severity>,
    checked_json: bool,
    pub(super) json_charge: usize,
    timestamp: Option<(usize, String)>,
}

impl LogLineMeta {
    fn parse(line: &str, fallback: Option<i128>) -> Self {
        let start = if line.starts_with('[') {
            line.find("] ").map_or(0, |end| end + 2)
        } else {
            0
        };
        let end = line[start..]
            .find(' ')
            .map_or(line.len(), |end| start + end);
        let Ok(time) = line[start..end].parse::<k8s_openapi::jiff::Timestamp>() else {
            return Self {
                sort_time: Some(
                    fallback.unwrap_or_else(|| k8s_openapi::jiff::Timestamp::now().as_nanosecond()),
                ),
                timestamp: None,
                ..Self::default()
            };
        };
        let end = (end + 1).min(line.len());
        Self {
            sort_time: Some(time.as_nanosecond()),
            timestamp: Some((start, line[start..end].to_owned())),
            ..Self::default()
        }
    }
}

pub(super) const JSON_CACHE_LIMIT: usize = 8 * 1024 * 1024;
const JSON_RECORD_LIMIT: usize = 4096;

impl LogLineMeta {
    fn format_json(&mut self, line: &str, budget: &mut usize) {
        if self.checked_json {
            return;
        }
        self.checked_json = true;
        if line.len() > JSON_RECORD_LIMIT || line.len() > *budget {
            return;
        }
        *budget -= line.len();
        self.json_charge = line.len();
        let source_end = line
            .strip_prefix('[')
            .and_then(|rest| {
                let end = rest.find("] ")?;
                let label = &rest[..end];
                (!label.is_empty()
                    && !rest[end + 2..].trim().is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/._-:".contains(&b)))
                .then_some(end + 3)
            })
            .unwrap_or(0);
        let time_end = self
            .timestamp
            .as_ref()
            .map_or(source_end, |(start, timestamp)| {
                if line.get(*start..).is_some_and(|s| s.starts_with(timestamp)) {
                    start + timestamp.len()
                } else {
                    source_end
                }
            });
        let payload = line[time_end..].trim();
        if !payload.starts_with(['{', '[']) {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        let timestamp_reserve = self
            .timestamp
            .as_ref()
            .map_or(0, |(_, timestamp)| timestamp.len());
        let mut cache = |text: String| {
            let charge = text.len() + time_end + timestamp_reserve;
            if charge > *budget {
                return None;
            }
            *budget -= charge;
            self.json_charge += charge;
            Some(format!("{}{}", &line[..time_end], text))
        };
        // The input and parser depth limits also bound the temporary output.
        // Each view is charged on its own, so an indented form that does not
        // fit cannot keep a short record row raw.
        let record = render_record(&value);
        let record_severity = record.as_ref().and_then(|(_, severity)| *severity);
        let record = record.and_then(|(text, _)| cache(text));
        let pretty = serde_json::to_string_pretty(&value)
            .ok()
            .and_then(&mut cache);
        self.record_severity = record_severity;
        self.record = record;
        self.pretty = pretty;
    }

    pub(super) fn display(&self, view: JsonView) -> Option<&str> {
        match view {
            JsonView::Raw => None,
            JsonView::Record => self.record.as_deref(),
            JsonView::Pretty => self.pretty.as_deref(),
        }
    }
}

/// How the log view shows JSON records. `J` cycles through the variants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JsonView {
    #[default]
    Raw,
    /// One row per structured log record: time, level, message, fields.
    Record,
    /// Indented JSON.
    Pretty,
}

impl JsonView {
    /// The view a `[logs] json_view` value names; `Raw` for anything else,
    /// which `config::logs_warnings` reports.
    pub fn from_config(name: &str) -> Self {
        match name {
            "record" => Self::Record,
            "pretty" => Self::Pretty,
            _ => Self::Raw,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Raw => Self::Record,
            Self::Record => Self::Pretty,
            Self::Pretty => Self::Raw,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Record => "record",
            Self::Pretty => "pretty",
        }
    }
}

const RECORD_TIME_KEYS: &[&str] = &["time", "ts", "timestamp", "@timestamp"];
const RECORD_LEVEL_KEYS: &[&str] = &["level", "lvl", "severity"];
const RECORD_MESSAGE_KEYS: &[&str] = &["msg", "message"];

/// One-line rendering of a structured log record (zap, slog, logrus, pino):
/// `time LEVEL message key=value ...`, with the severity of its level if it
/// has one. `None` when the value is not an object with a level or message
/// field.
fn render_record(
    value: &serde_json::Value,
) -> Option<(String, Option<crate::logfilter::Severity>)> {
    let fields = value.as_object()?;
    let find = |keys: &[&'static str]| {
        keys.iter()
            .find_map(|&key| fields.get(key).map(|value| (key, value)))
    };
    let level = find(RECORD_LEVEL_KEYS);
    let message = find(RECORD_MESSAGE_KEYS);
    if level.is_none() && message.is_none() {
        return None;
    }
    let time = find(RECORD_TIME_KEYS);
    let mut out = String::new();
    let mut push = |part: &str| {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(part);
    };
    if let Some((_, time)) = time {
        push(&record_time(time));
    }
    let level_name = level.map(|(_, level)| record_level(level));
    if let Some(name) = &level_name {
        push(&format!("{:<5}", record_text(name)));
    }
    if let Some((_, message)) = message {
        match message.as_str() {
            Some(text) => push(&record_text(text)),
            None => push(&message.to_string()),
        }
    }
    let used = [time, level, message].map(|field| field.map(|(key, _)| key));
    for (key, value) in fields {
        if used.contains(&Some(key.as_str())) {
            continue;
        }
        push(&format!("{}={}", record_text(key), record_value(value)));
    }
    let severity = level_name.map(|name| crate::logfilter::parse_level(&name.to_ascii_lowercase()));
    Some((out, severity))
}

/// Text with control characters (a multi-line stack trace, terminal escapes)
/// as an escaped JSON string, so it stays on the record's row.
fn record_text(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(char::is_control) {
        serde_json::Value::from(text).to_string().into()
    } else {
        text.into()
    }
}

/// Epoch numbers (zap seconds, pino milliseconds) become RFC 3339; strings
/// are kept as the application wrote them.
fn record_time(time: &serde_json::Value) -> String {
    let Some(number) = time.as_f64() else {
        return time
            .as_str()
            .map_or_else(|| time.to_string(), |text| record_text(text).into_owned());
    };
    let micros = if number.abs() >= 1e11 {
        number * 1e3
    } else {
        number * 1e6
    };
    k8s_openapi::jiff::Timestamp::from_microsecond(micros.round() as i64)
        .map_or_else(|_| time.to_string(), |time| time.to_string())
}

/// Level names in upper case; pino's numeric levels mapped to their names.
fn record_level(level: &serde_json::Value) -> String {
    match level.as_u64() {
        Some(10) => "TRACE".into(),
        Some(20) => "DEBUG".into(),
        Some(30) => "INFO".into(),
        Some(40) => "WARN".into(),
        Some(50) => "ERROR".into(),
        Some(60) => "FATAL".into(),
        _ => level
            .as_str()
            .map_or_else(|| level.to_string(), str::to_ascii_uppercase),
    }
}

/// Bare strings stay bare. Strings that would be ambiguous in `key=value`
/// form or read as another JSON type (`"true"`, `"3"`, `"{}"`), and every
/// other JSON value, use compact JSON.
fn record_value(value: &serde_json::Value) -> String {
    match value.as_str() {
        Some(text)
            if !text.is_empty()
                && !text.starts_with(['{', '['])
                && serde_json::from_str::<serde_json::Value>(text).is_err()
                && !text
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '=')) =>
        {
            text.to_owned()
        }
        _ => value.to_string(),
    }
}

pub(super) fn display_height(line: &str, width: usize) -> usize {
    line.split('\n')
        .map(|part| {
            if width == 0 {
                1
            } else {
                crate::ui::wrapped_height(part, width)
            }
        })
        .sum()
}

impl LogsView {
    pub fn display_line(&self, i: usize) -> &str {
        self.line_meta
            .get(i)
            .and_then(|m| m.display(self.json))
            .unwrap_or(&self.view.lines[i])
    }

    /// Severity of the record row shown for line `i`, when record view shows
    /// one. Its level can sit under any supported key or be a pino number,
    /// which the raw-line severity check does not read. A record without a
    /// level keeps the raw line's severity (`"msg":"request error: …"`).
    pub fn record_severity(&self, i: usize) -> Option<crate::logfilter::Severity> {
        if self.json != JsonView::Record {
            return None;
        }
        let meta = self.line_meta.get(i)?;
        meta.record.as_ref()?;
        Some(
            meta.record_severity
                .unwrap_or_else(|| crate::logfilter::severity(&self.view.lines[i])),
        )
    }

    pub(super) fn toggle_json(&mut self) {
        self.set_json(self.json.next());
    }

    pub(super) fn set_json(&mut self, view: JsonView) {
        if self.json == view {
            return;
        }
        let scroll = self.view.scroll;
        let shown = self
            .refresh_index(self.last_wrap_width)
            .first_at_row(scroll);
        self.json = view;
        self.prepare_json();
        self.view.revision = self.view.revision.wrapping_add(1);
        let width = self.last_wrap_width;
        self.viewport_rows = self.refresh_index(width).total_rows();
        if !self.follow {
            self.view.scroll = self
                .index
                .start_row(shown)
                .min(self.viewport_rows.saturating_sub(self.viewport_h));
        }
    }

    pub(super) fn prepare_json(&mut self) {
        if self.json == JsonView::Raw {
            return;
        }
        self.line_meta
            .resize_with(self.view.lines.len(), LogLineMeta::default);
        for (line, meta) in self.view.lines.iter().zip(self.line_meta.iter_mut()) {
            meta.format_json(line, &mut self.json_budget);
        }
    }

    fn push_line(&mut self, mut line: String) {
        self.line_meta
            .resize_with(self.view.lines.len(), LogLineMeta::default);
        let mut meta = LogLineMeta::parse(&line, self.line_meta.back().and_then(|m| m.sort_time));
        if !self.timestamps
            && let Some((start, timestamp)) = &meta.timestamp
        {
            line.replace_range(*start..start + timestamp.len(), "");
        }
        if self.json != JsonView::Raw {
            meta.format_json(&line, &mut self.json_budget);
        }
        // Equal timestamps keep their arrival order. Missing timestamps use
        // the newest known time, or the arrival time if no time is known.
        let index = if self
            .line_meta
            .back()
            .is_none_or(|m| m.sort_time <= meta.sort_time)
        {
            self.view.lines.len()
        } else {
            self.line_meta
                .partition_point(|m| m.sort_time <= meta.sort_time)
        };
        if index < self.view.lines.len() {
            if !self.follow && self.matches(&line) {
                self.refresh_index(self.last_wrap_width);
                let mut marker_index = 0;
                if let Some(shown) = self.index.shown.iter().position(|entry| match entry {
                    Some(i) => *i as usize >= index,
                    None => {
                        let after = self.markers[marker_index] > self.line_offset + index;
                        marker_index += 1;
                        after
                    }
                }) && self.index.start_row(shown) <= self.view.scroll
                {
                    self.view.scroll += display_height(
                        meta.display(self.json).unwrap_or(&line),
                        self.last_wrap_width,
                    );
                }
            }
            for marker in &mut self.markers {
                if *marker > self.line_offset + index {
                    *marker += 1;
                }
            }
            self.view.revision = self.view.revision.wrapping_add(1);
        }
        self.line_meta.insert(index, meta);
        self.view.lines.insert(index, line);
    }

    pub(super) fn toggle_timestamps(&mut self) {
        let anchor = if self.follow {
            None
        } else {
            let (scroll, height) = (self.view.scroll, self.viewport_h);
            let index = self.refresh_index(self.last_wrap_width);
            let row = scroll.min(index.total_rows().saturating_sub(height));
            let shown = index.first_at_row(row);
            index.shown.get(shown).map(|line| {
                let marker = index.shown[..shown].iter().filter(|l| l.is_none()).count();
                (*line, marker, row - index.start_row(shown))
            })
        };
        self.timestamps = !self.timestamps;
        for (line, meta) in self.view.lines.iter_mut().zip(&mut self.line_meta) {
            if let Some((start, timestamp)) = &meta.timestamp {
                for formatted in [&mut meta.pretty, &mut meta.record].into_iter().flatten() {
                    if self.timestamps {
                        formatted.insert_str(*start, timestamp);
                    } else {
                        formatted.replace_range(*start..start + timestamp.len(), "");
                    }
                }
                if self.timestamps {
                    line.insert_str(*start, timestamp);
                } else {
                    line.replace_range(*start..start + timestamp.len(), "");
                }
            }
        }
        self.view.revision = self.view.revision.wrapping_add(1);
        self.viewport_rows = self.refresh_index(self.last_wrap_width).total_rows();
        if !self.follow {
            let row = anchor.map_or(0, |(line, marker, offset)| {
                let index = &self.index;
                let shown = match line {
                    Some(line) => index
                        .shown
                        .iter()
                        .position(|entry| entry.is_some_and(|i| i >= line))
                        .or_else(|| index.shown.len().checked_sub(1)),
                    None => index
                        .shown
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| line.is_none())
                        .nth(marker)
                        .map(|(i, _)| i),
                };
                shown.map_or(0, |shown| {
                    let offset = if index.shown[shown] == line {
                        offset.min(index.height_at(shown).saturating_sub(1))
                    } else {
                        0
                    };
                    index.start_row(shown) + offset
                })
            });
            self.view.scroll = row.min(self.viewport_rows.saturating_sub(self.viewport_h));
        }
    }
}

impl App {
    /// Take a new `[logs]` config. The JSON view follows `json_view` only
    /// when the configured value changes, at startup or on a context switch,
    /// so `J` keeps the session's choice across views otherwise.
    pub fn apply_logs_config(&mut self, cfg: crate::config::LogsConfig) {
        if cfg.json_view != self.logs_cfg.json_view {
            self.logs.set_json(JsonView::from_config(&cfg.json_view));
        }
        self.logs_cfg = cfg;
    }

    // ----- selection -----------------------------------------------------

    pub(super) fn push_log_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator<Item = String>,
    {
        // Remove carriage returns and replace tabs with spaces for display.
        for line in lines {
            let line = if line.contains('\r') || line.contains('\t') {
                line.chars()
                    .filter_map(|c| match c {
                        '\r' => None,
                        '\t' => Some(' '),
                        c => Some(c),
                    })
                    .collect()
            } else {
                line
            };
            self.logs.push_line(line);
        }

        self.trim_log_buffer();
    }

    pub(super) fn log_buffer_cap(&self) -> usize {
        if self.logs.follow {
            self.logs_cfg.buffer.max(1)
        } else {
            MAX_LOG_LINES_PAUSED
        }
    }

    pub(super) fn trim_log_buffer(&mut self) {
        let cap = self.log_buffer_cap();
        self.logs.limit_markers(cap);
        self.logs
            .drain_front(self.logs.view.lines.len().saturating_sub(cap));
    }

    /// Logs for marked pods or the current selection. Stream every container. For
    /// workloads/services: list matching pods and aggregate all their logs.
    pub(super) fn open_logs(&mut self) {
        if self.kind_plural == "pods" && !self.marked.is_empty() {
            let pods: Vec<_> = self
                .rows()
                .into_iter()
                .filter(|obj| self.marked.contains(&row_key(obj)))
                .map(|obj| PodLogTarget {
                    ns: obj.metadata.namespace.clone().unwrap_or_default(),
                    name: obj.metadata.name.clone().unwrap_or_default(),
                    uid: obj.metadata.uid.clone(),
                    containers: container_names(obj),
                })
                .collect();
            if pods.is_empty() {
                self.flash_warn("no marked pods in the current view");
                return;
            }
            let title = format!("marked pods ({}) - logs", pods.len());
            self.launch_logs(LogSource::Pods(pods), title);
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();

        match self.kind_plural.as_str() {
            "pods" => {
                let containers = container_names(obj);
                self.launch_logs(
                    LogSource::Pod {
                        ns,
                        name: name.clone(),
                        uid: obj.metadata.uid.clone(),
                        containers,
                    },
                    format!("{name} — logs"),
                );
            }
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
                match label_selector(obj, "matchLabels") {
                    Some(labels) => self.launch_logs(
                        LogSource::Selector { ns, labels },
                        format!("{}/{name} — logs (all pods)", trim_s(&self.kind_plural)),
                    ),
                    None => self.flash_warn("no pod selector for logs"),
                }
            }
            "services" => match label_selector(obj, "selector") {
                Some(labels) => self.launch_logs(
                    LogSource::Selector { ns, labels },
                    format!("svc/{name} — logs (all pods)"),
                ),
                None => self.flash_warn("service has no selector"),
            },
            _ => self.flash_warn("logs available for pods and workloads"),
        }
    }

    /// Provider (`[providers.logs]`) logs for the current selection: pods,
    /// workloads and services (via their selector), and whole namespaces.
    /// Mirrors [`App::open_logs`], but the backend answers instead of the
    /// kubelet — so it also covers restarted and deleted pods.
    pub(super) fn open_provider_logs(&mut self) {
        if self.log_provider_invalid() {
            return;
        }
        if let Some(link) = self.active_log_link() {
            self.open_log_link(&link, None);
            return;
        }
        let label = format!("victorialogs ({})", self.provider_lookback_label());
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        use crate::providers::LogRequest;

        match self.kind_plural.as_str() {
            "pods" => {
                let multi_container = container_names(obj).len() > 1;
                self.launch_logs(
                    LogSource::Provider {
                        request: LogRequest::Pod {
                            ns,
                            pod: name.clone(),
                            container: None,
                            multi_container,
                        },
                    },
                    format!("{name} — {label}"),
                );
            }
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
                match label_selector(obj, "matchLabels") {
                    Some(labels) => self.launch_logs(
                        LogSource::Provider {
                            request: LogRequest::Selector { ns, labels },
                        },
                        format!("{}/{name} — {label}", trim_s(&self.kind_plural)),
                    ),
                    None => self.flash_warn("no pod selector for logs"),
                }
            }
            "services" => match label_selector(obj, "selector") {
                Some(labels) => self.launch_logs(
                    LogSource::Provider {
                        request: LogRequest::Selector { ns, labels },
                    },
                    format!("svc/{name} — {label}"),
                ),
                None => self.flash_warn("service has no selector"),
            },
            "namespaces" => self.launch_logs(
                LogSource::Provider {
                    request: LogRequest::Namespace { ns: name.clone() },
                },
                format!("ns/{name} — {label}"),
            ),
            _ => self.flash_warn("provider logs cover pods, workloads, services, and namespaces"),
        }
    }

    /// The link provider `L` uses: the configured one, else Cloud Logging for
    /// a GKE cluster without a `[providers.logs]` section. A section that
    /// failed to compile still counts, so a broken config never falls back
    /// to a backend the user did not pick.
    fn active_log_link(&self) -> Option<crate::providers::LogLink> {
        if self.log_provider_configured {
            return self.log_link.clone();
        }
        crate::providers::LogLink::detect(&self.cluster.cluster_name)
    }

    /// Warn when `[providers.logs]` is present but did not compile, instead
    /// of falling back to a backend the user did not pick.
    fn log_provider_invalid(&mut self) -> bool {
        let invalid =
            self.log_provider_configured && self.log_link.is_none() && self.log_provider.is_none();
        if invalid {
            self.flash_warn("[providers.logs] is invalid, see :config");
        }
        invalid
    }

    /// Open the log UI link for the selected row, or for one container of a
    /// pod from the container picker.
    fn open_log_link(
        &mut self,
        link: &crate::providers::LogLink,
        container: Option<(String, String, String)>,
    ) {
        let mut target = crate::providers::LinkTarget {
            context: self.cluster.context.clone(),
            cluster: self.cluster.cluster_name.clone(),
            ..Default::default()
        };
        if let Some((ns, pod, container)) = container {
            target.resource = "pods".into();
            target.namespace = ns;
            target.name = pod;
            target.container = Some(container);
        } else {
            let Some(obj) = self.selected_ref() else {
                return;
            };
            target.resource = self.kind_plural.clone();
            target.name = obj.metadata.name.clone().unwrap_or_default();
            // A namespace row is its own namespace.
            target.namespace = if self.kind_plural == "namespaces" {
                target.name.clone()
            } else {
                obj.metadata.namespace.clone().unwrap_or_default()
            };
            target.selector =
                crate::providers::link_selector(&self.kind_plural, &obj.data).unwrap_or_default();
        }
        match link.url(&target) {
            Ok(url) => self.open_url(url),
            Err(e) => self.flash_warn(&e),
        }
    }

    /// Open `url` in the browser. Without one (over SSH, or no opener), copy
    /// it to the clipboard, and show it when that fails too.
    fn open_url(&mut self, url: String) {
        #[cfg(test)]
        self.opened_links.push(url);
        #[cfg(not(test))]
        {
            let claim = self.claim_status("opening logs in the browser…");
            let tx = self.tx.clone();
            let generation = self.generation;
            tokio::spawn(async move {
                let failure = format!("log link: {url}");
                let (copied, success) = tokio::task::spawn_blocking(move || {
                    if open_in_browser(&url) {
                        (true, "opened logs in the browser".to_string())
                    } else if copy_to_clipboard(&url) {
                        (true, format!("no browser, copied log link: {url}"))
                    } else {
                        (false, String::new())
                    }
                })
                .await
                .unwrap_or((false, String::new()));
                let _ = tx
                    .send(Msg::ClipboardCopied {
                        generation,
                        claim,
                        copied,
                        success,
                        failure,
                    })
                    .await;
            });
        }
    }

    /// The lookback window shown in provider-view titles: the configured (or
    /// previously discovered) provider's, else the default an autodiscovered
    /// one will use.
    pub(super) fn provider_lookback_label(&self) -> String {
        self.log_provider
            .as_ref()
            .map(|p| p.lookback_label.clone())
            .unwrap_or_else(|| crate::providers::DEFAULT_LOOKBACK.into())
    }

    /// Apply a new lookback period typed into the `T` prompt: validate it,
    /// remember it on the session provider (so later `L` presses keep it),
    /// retitle the view, and re-run the backfill + tail.
    pub(super) fn apply_provider_lookback(&mut self, input: &str) {
        let secs = match crate::providers::parse_lookback(input) {
            Ok(secs) => secs,
            Err(e) => {
                self.flash_warn(&format!("lookback: {e}"));
                return;
            }
        };
        let label = input.trim().to_string();
        self.log_provider
            .get_or_insert_default()
            .set_lookback(secs, label.clone());

        // Titles end in "victorialogs (<lookback>)" — rewrite the suffix.
        if let Some(idx) = self.logs.view.title.rfind("victorialogs (") {
            self.logs.view.title.truncate(idx);
            self.logs
                .view
                .title
                .push_str(&format!("victorialogs ({label})"));
        }
        self.flash = format!("lookback: {label}");
        self.flash_err = false;
        if !self.logs.stopped {
            self.retail_logs();
        }
    }

    /// Provider logs for one container, from the container picker.
    pub(super) fn launch_provider_container_logs(
        &mut self,
        ns: String,
        pod: String,
        container: String,
    ) {
        if self.log_provider_invalid() {
            return;
        }
        if let Some(link) = self.active_log_link() {
            self.open_log_link(&link, Some((ns, pod, container)));
            return;
        }
        let title = format!(
            "{pod}:{container} — victorialogs ({})",
            self.provider_lookback_label()
        );
        self.launch_logs(
            LogSource::Provider {
                request: crate::providers::LogRequest::Pod {
                    ns,
                    pod,
                    container: Some(container),
                    multi_container: false,
                },
            },
            title,
        );
    }

    /// Begin a fresh logs view from a source (resets filter/follow).
    pub(super) fn launch_logs(&mut self, source: LogSource, title: String) {
        self.set_return_mode();
        self.logs.source = Some(source);
        self.logs.since_anchor = None;
        // Note: we deliberately do NOT touch the view generation here — the
        // underlying table/xray watch keeps running so returning is instant and
        // the selection is preserved. Log streams have their own lifecycle.
        self.logs.view = Scrollable {
            title,
            lines: VecDeque::new(),
            ..Default::default()
        };
        // A new Scrollable starts at revision 0, which can match the previous
        // buffer's revision. Do not let refresh_index mistake the replacement
        // for an append and retain stale line positions or wrapped heights.
        self.logs.clear_lines();
        self.logs.follow = true;
        self.logs.set_filter(String::new());
        self.logs.warnings_only = false;
        self.logs.stopped = false;
        self.mode = Mode::Logs;
        self.restart_log_stream();
    }

    /// Restart the current source and keep the title, filter, and follow state.
    pub(super) fn retail_logs(&mut self) {
        if self.logs.source.is_none() {
            return;
        }
        self.logs.clear_lines();
        self.logs.view.scroll = 0;
        self.restart_log_stream();
    }

    /// Bump the log generation, abort old log tasks, and spawn fresh ones for
    /// the current source. Independent of the view watch.
    pub(super) fn restart_log_stream(&mut self) {
        self.stop_log_stream();
        self.start_logs();
    }

    /// Invalidate and abort the current log streams (the view watch is left
    /// running).
    pub(super) fn stop_log_stream(&mut self) {
        self.log_gen += 1;
        self.log_flag.store(self.log_gen, Ordering::SeqCst);
        for t in self.log_tasks.drain(..) {
            t.abort();
        }
    }

    /// Spawn the streaming task(s) for the current `log_source`.
    pub(super) fn start_logs(&mut self) {
        match self.logs.source.clone() {
            Some(LogSource::Pods(pods)) => {
                for PodLogTarget {
                    ns,
                    name,
                    uid,
                    containers,
                } in pods
                {
                    // The marked set is fixed, so a replacement is not followed.
                    let instance = || match &uid {
                        Some(uid) => log_follow::Instance::Only(uid.clone()),
                        None => log_follow::Instance::Named(None),
                    };
                    if containers.is_empty() {
                        let prefix = format!("[{ns}/{name}] ");
                        self.spawn_one_log(ns, name, instance(), None, prefix, false);
                    } else {
                        for c in containers {
                            let prefix = format!("[{ns}/{name}:{c}] ");
                            self.spawn_one_log(
                                ns.clone(),
                                name.clone(),
                                instance(),
                                Some(c),
                                prefix,
                                false,
                            );
                        }
                    }
                }
            }
            Some(LogSource::Pod {
                ns,
                name,
                uid,
                containers,
            }) => {
                if containers.is_empty() {
                    // Unknown container set (e.g. from xray) — stream the default.
                    let instance = log_follow::Instance::Named(uid);
                    self.spawn_one_log(ns, name, instance, None, String::new(), false);
                } else {
                    let multi = containers.len() > 1;
                    for c in containers {
                        let prefix = if multi {
                            format!("[{c}] ")
                        } else {
                            String::new()
                        };
                        self.spawn_one_log(
                            ns.clone(),
                            name.clone(),
                            log_follow::Instance::Named(uid.clone()),
                            Some(c),
                            prefix,
                            false,
                        );
                    }
                }
            }
            Some(LogSource::Selector { ns, labels }) => self.spawn_selector_logs(ns, labels),
            Some(LogSource::Single {
                ns,
                pod,
                container,
                previous,
            }) => self.spawn_one_log(
                ns,
                pod,
                log_follow::Instance::Named(None),
                container,
                String::new(),
                previous,
            ),
            Some(LogSource::Provider { request }) => self.spawn_provider_logs(request),
            None => {}
        }
    }

    /// Start the provider history query and live stream for `request`.
    /// Use the log generation for stream restarts and view exits. If no
    /// provider is configured, discover and cache a VictoriaLogs service.
    pub(super) fn spawn_provider_logs(&mut self, request: crate::providers::LogRequest) {
        let provider = self.log_provider.clone().unwrap_or_default();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let view_gen = self.generation;
        let handle = tokio::spawn(async move {
            let mut provider = provider;
            let mut info: Vec<String> = Vec::new();
            let mut resolved = false;

            if provider.needs_discovery() {
                match crate::providers::discover(client.clone(), &provider).await {
                    Ok(p) => {
                        info.push(format!("[provider] using {}", p.location()));
                        provider = p;
                        resolved = true;
                    }
                    Err(e) => {
                        let mut lines = vec![format!("[error] {e}")];
                        let _ = send_log_batch(&tx, genr, &mut lines).await;
                        return;
                    }
                }
            }

            // Shippers disagree on the namespace/pod/container field names;
            // without explicit config, ask the backend which convention it
            // ingested. Detection failures fall back to the defaults and are
            // retried on the next launch (nothing is pinned).
            if provider.needs_field_detection() {
                match provider.detect_fields().await {
                    Ok(Some(p)) => {
                        if p.field_names() != provider.field_names() {
                            info.push(format!(
                                "[provider] detected log fields: {}",
                                p.field_names()
                            ));
                        }
                        provider = p;
                        resolved = true;
                    }
                    Ok(None) => info.push(format!(
                        "[provider] no known log field convention found — using {} (set [providers.logs.fields] if logs are missing)",
                        provider.field_names()
                    )),
                    Err(e) => info.push(format!(
                        "[provider] field detection failed: {e} — using {}",
                        provider.field_names()
                    )),
                }
            }

            if resolved {
                let _ = tx
                    .send(Msg::LogProviderDiscovered {
                        generation: view_gen,
                        provider: Box::new(provider.clone()),
                    })
                    .await;
            }
            if !info.is_empty() && !send_log_batch(&tx, genr, &mut info).await {
                return;
            }
            provider_log_task(provider, request, client, tx, genr, flag).await;
        });
        self.log_tasks.push(handle);
    }

    pub(super) fn spawn_one_log(
        &mut self,
        ns: String,
        pod: String,
        instance: log_follow::Instance,
        container: Option<String>,
        prefix: String,
        previous: bool,
    ) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let (tail, since) = self.log_tail_and_since();
        // The API applies the tail limit within the lookback window.
        // Previous-container logs retain their full history.
        let (tail_lines, since_seconds) = if previous {
            (None, None)
        } else {
            (Some(tail), since)
        };
        let stream = log_follow::LogStream {
            api: Api::namespaced(client, &ns),
            pod,
            instance,
            params: LogParams {
                follow: !previous,
                previous,
                container,
                // Keep timestamps for sorting, even when their text is hidden.
                timestamps: true,
                tail_lines,
                since_seconds,
                ..Default::default()
            },
            prefix,
            tx,
            generation: genr,
            flag,
            wake: self.log_wake.subscribe(),
        };
        self.log_tasks.push(tokio::spawn(stream.run()));
    }

    /// The configured initial `tail` line count and optional `since` lookback
    /// in seconds, parsed from `[logs]`. An unparseable `since` is ignored.
    /// A custom lookback or time anchor overrides the config for this view.
    pub(super) fn log_tail_and_since(&self) -> (i64, Option<i64>) {
        let tail = self.logs_cfg.tail.max(1);
        let since = match self.logs.since_anchor {
            Some(0) => None, // `0`: forced plain tail
            Some(secs) => Some(secs),
            None => self
                .logs_cfg
                .since
                .as_deref()
                .and_then(|s| crate::providers::parse_lookback(s).ok()),
        };
        (tail, since)
    }

    pub(super) fn apply_log_lookback(&mut self, input: &str) {
        if self.provider_logs_active() {
            self.apply_provider_lookback(input);
            return;
        }
        let input = input.trim();
        let secs = if input == "tail" {
            0
        } else {
            match crate::providers::parse_lookback(input) {
                Ok(secs) => secs,
                Err(error) => {
                    self.flash_warn(&format!("lookback: {error}; use s/m/h/d or tail"));
                    return;
                }
            }
        };
        self.set_log_lookback(secs, input);
    }

    /// Apply a `0`–`5` time anchor (k9s): `0` re-tails, `1`–`5` re-stream the
    /// last 1m/5m/15m/30m/1h. On a provider view the digit sets the provider
    /// lookback instead (`0` = the default window).
    pub(super) fn apply_log_anchor(&mut self, key: char) {
        let (secs, label) = match key {
            '0' => (0, "tail"),
            '1' => (60, "1m"),
            '2' => (300, "5m"),
            '3' => (900, "15m"),
            '4' => (1800, "30m"),
            '5' => (3600, "1h"),
            _ => return,
        };
        if self.provider_logs_active() {
            let label = if key == '0' {
                crate::providers::DEFAULT_LOOKBACK
            } else {
                label
            };
            self.apply_provider_lookback(label);
            return;
        }
        self.set_log_lookback(secs, label);
    }

    fn set_log_lookback(&mut self, secs: i64, label: &str) {
        self.logs.since_anchor = Some(secs);
        self.flash = if secs == 0 {
            format!("showing tail ({} lines)", self.logs_cfg.tail.max(1))
        } else {
            format!("showing last {label}")
        };
        self.flash_err = false;
        if !self.logs.stopped {
            self.retail_logs();
        }
    }

    pub(super) fn spawn_selector_logs(&mut self, ns: String, labels: String) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let (tail, since) = self.log_tail_and_since();
        // Bound the per-pod tail so an aggregate over many pods stays sane; the
        // follow buffer trims the total anyway.
        let follow = log_follow::SelectorLogs {
            client,
            ns,
            labels,
            tail: tail.min(100),
            since,
            tx,
            generation: genr,
            flag,
            wake: self.log_wake.subscribe(),
        };
        self.log_tasks.push(tokio::spawn(follow.run()));
    }
}

/// Run one provider-logs session: resolve the request to a concrete scope,
/// backfill the lookback window, then follow the live tail. Errors land in
/// the log buffer as `[error]` lines (matching the kubelet streams), so a
/// misconfigured or unreachable backend degrades visibly, never fatally.
async fn provider_log_task(
    provider: crate::providers::LogProvider,
    request: crate::providers::LogRequest,
    client: Client,
    tx: Sender<Msg>,
    generation: u64,
    flag: Arc<AtomicU64>,
) {
    use crate::providers::{LogRequest, LogScope, Prefix};

    let (scope, prefix) = match request {
        LogRequest::Pod {
            ns,
            pod,
            container,
            multi_container,
        } => {
            let prefix = if container.is_none() && multi_container {
                Prefix::Container
            } else {
                Prefix::None
            };
            (LogScope::Pod { ns, pod, container }, prefix)
        }
        LogRequest::Namespace { ns } => (LogScope::Namespace { ns }, Prefix::PodContainer),
        LogRequest::Selector { ns, labels } => {
            let api: Api<Pod> = if ns.is_empty() {
                Api::all(client)
            } else {
                Api::namespaced(client, &ns)
            };
            let pods = match api.list(&ListParams::default().labels(&labels)).await {
                Ok(list) => list,
                Err(e) => {
                    let mut lines = vec![format!("[error] listing pods: {e}")];
                    let _ = send_log_batch(&tx, generation, &mut lines).await;
                    return;
                }
            };
            let names: Vec<String> = pods
                .items
                .iter()
                .filter_map(|p| p.metadata.name.clone())
                .collect();
            if names.is_empty() {
                let mut lines = vec!["(no matching pods)".to_string()];
                let _ = send_log_batch(&tx, generation, &mut lines).await;
                return;
            }
            (LogScope::Pods { ns, pods: names }, Prefix::PodContainer)
        }
    };

    if flag.load(Ordering::SeqCst) != generation {
        return;
    }

    // Backfill the lookback window. Remember the newest timestamp so the
    // seam with the tail (which may replay a little history) de-duplicates.
    let mut backfill_max: i128 = i128::MIN;
    match provider.query(&scope).await {
        Ok(entries) => {
            let mut lines: Vec<String> = Vec::new();
            for e in &entries {
                if let Some(n) = e.nanos {
                    backfill_max = backfill_max.max(n);
                }
                lines.extend(e.lines(prefix, true));
            }
            if lines.is_empty() {
                lines.push(format!("(no logs in the last {})", provider.lookback_label));
            }
            if !send_log_batch(&tx, generation, &mut lines).await {
                return;
            }
        }
        Err(e) => {
            let mut lines = vec![format!("[error] {e}")];
            let _ = send_log_batch(&tx, generation, &mut lines).await;
            return;
        }
    }

    if flag.load(Ordering::SeqCst) != generation {
        return;
    }

    let mut tail = match provider.tail(&scope).await {
        Ok(t) => t,
        Err(e) => {
            let mut lines = vec![format!("[error] live tail unavailable: {e}")];
            let _ = send_log_batch(&tx, generation, &mut lines).await;
            return;
        }
    };

    // Same batching cadence as the kubelet streams: coalesce bursts, flush
    // quickly when quiet.
    use tokio::time::MissedTickBehavior;
    let mut batch: Vec<String> = Vec::with_capacity(LOG_BATCH_LINES);
    let mut flush = tokio::time::interval(Duration::from_millis(LOG_BATCH_MS));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        if flag.load(Ordering::SeqCst) != generation {
            return;
        }
        tokio::select! {
            next = tail.next_entry() => match next {
                Ok(Some(e)) => {
                    // Skip anything the backfill already showed (the tail may
                    // replay a little history at the seam).
                    if e.nanos.is_none_or(|n| n > backfill_max) {
                        batch.extend(e.lines(prefix, true));
                    }
                    if batch.len() >= LOG_BATCH_LINES
                        && !send_log_batch(&tx, generation, &mut batch).await
                    {
                        return;
                    }
                }
                Ok(None) => {
                    batch.push("[provider] log stream ended".to_string());
                    let _ = send_log_batch(&tx, generation, &mut batch).await;
                    return;
                }
                Err(e) => {
                    batch.push(format!("[error] {e}"));
                    let _ = send_log_batch(&tx, generation, &mut batch).await;
                    return;
                }
            },
            _ = flush.tick(), if !batch.is_empty() => {
                if !send_log_batch(&tx, generation, &mut batch).await {
                    return;
                }
            }
        }
    }
}
