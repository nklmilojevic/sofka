use super::*;

#[derive(Default)]
pub(super) struct LogLineMeta {
    sort_time: Option<i128>,
    pub(super) pretty: Option<String>,
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
        // The input and parser depth limits also bound the temporary output.
        let Ok(pretty) = serde_json::to_string_pretty(&value) else {
            return;
        };
        let timestamp_reserve = self
            .timestamp
            .as_ref()
            .map_or(0, |(_, timestamp)| timestamp.len());
        let charge = pretty.len() + time_end + timestamp_reserve;
        if charge > *budget {
            return;
        }
        *budget -= charge;
        self.json_charge += charge;
        self.pretty = Some(format!("{}{}", &line[..time_end], pretty));
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
        if self.json
            && let Some(pretty) = self.line_meta.get(i).and_then(|m| m.pretty.as_deref())
        {
            pretty
        } else {
            &self.view.lines[i]
        }
    }

    pub(super) fn toggle_json(&mut self) {
        let scroll = self.view.scroll;
        let shown = self
            .refresh_index(self.last_wrap_width)
            .first_at_row(scroll);
        self.json = !self.json;
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
        if !self.json {
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
        if self.json {
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
                        meta.pretty.as_deref().unwrap_or(&line),
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
                if let Some(pretty) = &mut meta.pretty {
                    if self.timestamps {
                        pretty.insert_str(*start, timestamp);
                    } else {
                        pretty.replace_range(*start..start + timestamp.len(), "");
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
                    containers,
                } in pods
                {
                    if containers.is_empty() {
                        let prefix = format!("[{ns}/{name}] ");
                        self.spawn_one_log(ns, name, None, prefix, false);
                    } else {
                        for c in containers {
                            let prefix = format!("[{ns}/{name}:{c}] ");
                            self.spawn_one_log(ns.clone(), name.clone(), Some(c), prefix, false);
                        }
                    }
                }
            }
            Some(LogSource::Pod {
                ns,
                name,
                containers,
            }) => {
                if containers.is_empty() {
                    // Unknown container set (e.g. from xray) — stream the default.
                    self.spawn_one_log(ns, name, None, String::new(), false);
                } else {
                    let multi = containers.len() > 1;
                    for c in containers {
                        let prefix = if multi {
                            format!("[{c}] ")
                        } else {
                            String::new()
                        };
                        self.spawn_one_log(ns.clone(), name.clone(), Some(c), prefix, false);
                    }
                }
            }
            Some(LogSource::Selector { ns, labels }) => self.spawn_selector_logs(ns, labels),
            Some(LogSource::Single {
                ns,
                pod,
                container,
                previous,
            }) => self.spawn_one_log(ns, pod, container, String::new(), previous),
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
        container: Option<String>,
        prefix: String,
        previous: bool,
    ) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let (tail, since) = self.log_tail_and_since();
        let handle = tokio::spawn(async move {
            let api: Api<Pod> = Api::namespaced(client, &ns);
            // The API applies the tail limit within the lookback window.
            // Previous-container logs retain their full history.
            let (tail_lines, since_seconds) = if previous {
                (None, None)
            } else {
                (Some(tail), since)
            };
            let lp = LogParams {
                follow: !previous,
                previous,
                container,
                // Keep timestamps for sorting, even when their text is hidden.
                timestamps: true,
                tail_lines,
                since_seconds,
                ..Default::default()
            };
            forward_log_stream(api, pod, lp, prefix, tx, genr, flag).await;
        });
        self.log_tasks.push(handle);
    }

    /// The configured initial `tail` line count and optional `since` lookback
    /// (epoch seconds), parsed from `[logs]`. An unparseable `since` is
    /// ignored. A `0`–`5` time anchor overrides the config for the session.
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
        let per_pod_tail = tail.min(100);
        let handle = tokio::spawn(async move {
            let list_api: Api<Pod> = if ns.is_empty() {
                Api::all(client.clone())
            } else {
                Api::namespaced(client.clone(), &ns)
            };
            let pods = match list_api.list(&ListParams::default().labels(&labels)).await {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx
                        .send(Msg::LogLines {
                            generation: genr,
                            lines: vec![format!("[error] {e}")],
                        })
                        .await;
                    return;
                }
            };
            if pods.items.is_empty() {
                let _ = tx
                    .send(Msg::LogLines {
                        generation: genr,
                        lines: vec!["(no matching pods)".into()],
                    })
                    .await;
            }
            let mut streams = tokio::task::JoinSet::new();
            for p in pods {
                let pod_ns = p.metadata.namespace.clone().unwrap_or_default();
                let pod_name = p.metadata.name.clone().unwrap_or_default();
                let containers: Vec<String> = p
                    .spec
                    .as_ref()
                    .map(|s| s.containers.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default();
                let multi = containers.len() > 1;
                for c in containers {
                    let prefix = if multi {
                        format!("[{pod_name}:{c}] ")
                    } else {
                        format!("[{pod_name}] ")
                    };
                    let (client, tx, flag) = (client.clone(), tx.clone(), flag.clone());
                    let (pn, pns) = (pod_name.clone(), pod_ns.clone());
                    streams.spawn(async move {
                        let api: Api<Pod> = Api::namespaced(client, &pns);
                        let lp = LogParams {
                            follow: true,
                            container: Some(c),
                            timestamps: true,
                            tail_lines: Some(per_pod_tail),
                            since_seconds: since,
                            ..Default::default()
                        };
                        forward_log_stream(api, pn, lp, prefix, tx, genr, flag).await;
                    });
                }
            }
            while streams.join_next().await.is_some() {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
            }
        });
        self.log_tasks.push(handle);
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
