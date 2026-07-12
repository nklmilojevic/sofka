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
            None => {}
        }
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
