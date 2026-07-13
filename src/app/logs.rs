use super::*;

impl App {
    // ----- selection -----------------------------------------------------

    pub(super) fn push_log_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator<Item = String>,
    {
        // Strip carriage returns so progress output doesn't overwrite a row,
        // and expand tabs to spaces — many loggers separate timestamp/level/body
        // with tabs, which some terminals render awkwardly in a TUI cell.
        self.logs.view.lines.extend(
            lines
                .into_iter()
                .map(|line| line.replace('\r', "").replace('\t', " ")),
        );

        // While following, keep a tight tail buffer. While paused, avoid
        // trimming so indices don't shift under the frozen view (only a huge
        // backlog hits the larger paused cap).
        let cap = if self.logs.follow {
            MAX_LOG_LINES
        } else {
            MAX_LOG_LINES_PAUSED
        };
        let overflow = self.logs.view.lines.len().saturating_sub(cap);
        if overflow == 0 {
            return;
        }

        // If we trim while paused, shift the anchored scroll by the display
        // rows the dropped lines occupied on screen — filtered lines take none,
        // wrapped lines take several — so the frozen view stays put.
        if !self.logs.follow {
            let filter = self.logs.filter.to_lowercase();
            let rows: usize = self
                .logs
                .view
                .lines
                .iter()
                .take(overflow)
                .filter(|l| filter.is_empty() || l.to_lowercase().contains(&filter))
                .map(|l| match self.logs.last_wrap_width {
                    0 => 1,
                    w => crate::ui::wrapped_height(l, w),
                })
                .sum();
            self.logs.view.scroll = self.logs.view.scroll.saturating_sub(rows);
        }
        self.logs.view.lines.drain(0..overflow);
    }

    // ----- containers / logs --------------------------------------------

    pub(super) fn open_containers(&mut self, obj: &DynamicObject) {
        let mut names = container_names(obj);
        if names.is_empty() {
            self.flash_warn("no containers found");
            return;
        }
        names.sort();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.container_pod = Some((ns, name));
        self.container_list = names;
        self.container_resources = container_resources_of(obj).into_iter().collect();
        self.container_qos = qos_class(obj);
        self.container_state.select(Some(0));
        self.mode = Mode::Containers;
    }

    /// Latest metrics for a container in the pod currently shown by the
    /// container picker. `None` distinguishes unavailable Metrics Server data
    /// from a measured zero value.
    pub fn selected_pod_container_metrics(&self, container: &str) -> Option<(i64, i64)> {
        let (namespace, pod) = self.container_pod.as_ref()?;
        self.container_metrics
            .get(&format!("{namespace}/{pod}/{container}"))
            .copied()
    }

    /// Logs for the current selection. For pods: stream every container. For
    /// workloads/services: list matching pods and aggregate all their logs.
    pub(super) fn open_logs(&mut self) {
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
        self.logs.follow = true;
        self.logs.filter.clear();
        self.logs.stopped = false;
        self.mode = Mode::Logs;
        self.restart_log_stream();
    }

    /// Re-stream the current source (e.g. after toggling timestamps), keeping
    /// the title, filter, and follow state.
    pub(super) fn retail_logs(&mut self) {
        if self.logs.source.is_none() {
            return;
        }
        self.logs.view.lines.clear();
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
        let ts = self.logs.timestamps;
        match self.logs.source.clone() {
            Some(LogSource::Pod {
                ns,
                name,
                containers,
            }) => {
                if containers.is_empty() {
                    // Unknown container set (e.g. from xray) — stream the default.
                    self.spawn_one_log(ns, name, None, String::new(), false, ts);
                } else {
                    let multi = containers.len() > 1;
                    for c in containers {
                        let prefix = if multi {
                            format!("[{c}] ")
                        } else {
                            String::new()
                        };
                        self.spawn_one_log(ns.clone(), name.clone(), Some(c), prefix, false, ts);
                    }
                }
            }
            Some(LogSource::Selector { ns, labels }) => self.spawn_selector_logs(ns, labels, ts),
            Some(LogSource::Single {
                ns,
                pod,
                container,
                previous,
            }) => self.spawn_one_log(ns, pod, container, String::new(), previous, ts),
            Some(LogSource::Provider { request }) => self.spawn_provider_logs(request, ts),
            None => {}
        }
    }

    /// Spawn the provider backfill + live tail task for `request`. Shares the
    /// log-stream lifecycle (`log_gen`/`log_flag`), so stop/resume, timestamp
    /// re-streams, and view exits all work unchanged. With nothing configured,
    /// the task first autodiscovers a VictoriaLogs service in the cluster and
    /// reports it back for caching.
    pub(super) fn spawn_provider_logs(
        &mut self,
        request: crate::providers::LogRequest,
        timestamps: bool,
    ) {
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
            provider_log_task(provider, request, client, tx, genr, flag, timestamps).await;
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
        timestamps: bool,
    ) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let handle = tokio::spawn(async move {
            let api: Api<Pod> = Api::namespaced(client, &ns);
            let lp = LogParams {
                follow: !previous,
                previous,
                container,
                timestamps,
                tail_lines: if previous { None } else { Some(300) },
                ..Default::default()
            };
            forward_log_stream(api, pod, lp, prefix, tx, genr, flag).await;
        });
        self.log_tasks.push(handle);
    }

    pub(super) fn spawn_selector_logs(&mut self, ns: String, labels: String, timestamps: bool) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
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
                            timestamps,
                            tail_lines: Some(100),
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
    timestamps: bool,
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
                lines.extend(e.lines(prefix, timestamps));
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
                        batch.extend(e.lines(prefix, timestamps));
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
