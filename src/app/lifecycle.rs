use super::*;

impl App {
    // ----- navigation ----------------------------------------------------

    /// Switch the active resource kind by user input. Pushes the current view
    /// so `esc` can return.
    pub fn switch_kind(&mut self, input: &str) {
        self.switch_kind_ns(input, None);
    }

    /// Switch kind and (optionally) namespace in one move (`:deploy social`).
    /// `all`/`*` as the namespace selects all namespaces.
    pub fn switch_kind_ns(&mut self, input: &str, ns: Option<&str>) {
        match self.cluster.resolve(input) {
            Some(kind) => {
                if let Some(ns) = ns {
                    self.namespace = normalize_ns(ns);
                    self.note_recent_namespace(ns);
                }
                let title = kind.title();
                self.set_root_view(kind);
                self.flash = if ns.is_some() {
                    format!("Viewing {title} in {}", self.namespace_label())
                } else {
                    format!("Viewing {title}")
                };
                self.flash_err = false;
                self.record_history();
                self.start_watch();
            }
            None => {
                self.flash = format!("No resource matches '{}'", input.trim());
                self.flash_err = true;
            }
        }
    }

    /// Install `kind` as a fresh root view (not a drill-down): clear the
    /// breadcrumb so `esc` doesn't replay command history, drop drill
    /// selectors, and reset filter/sort/cursor. A stale selection from the
    /// previous kind (e.g. row 5 on pods) would otherwise carry over — the new
    /// view always starts with its first row selected.
    fn set_root_view(&mut self, kind: Kind) {
        self.stack.clear();
        self.kind_plural = kind.ar.plural.to_lowercase();
        self.kind = Some(kind);
        self.labels = None;
        self.fields = None;
        self.scope_label = None;
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
    }

    /// Open the Helm release list (`:helm`): one row per release at its
    /// latest revision, like `helm list`. Backed by the `secrets` kind
    /// scoped to Helm's own storage labels/type — see `crate::helm` and the
    /// `"helm"` dedup case in `rows::ensure_rows_cache`.
    pub(super) fn open_helm_releases(&mut self) {
        let Some(secrets) = self.cluster.resolve("secrets") else {
            self.flash_warn("secrets kind unavailable");
            return;
        };
        self.stack.clear();
        self.kind = Some(secrets);
        self.kind_plural = "helm".into();
        self.labels = Some("owner=helm".into());
        self.fields = Some("type=helm.sh/release.v1".into());
        self.scope_label = None;
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = "Viewing Helm releases".into();
        self.flash_err = false;
        // Deliberately not recorded in the `[`/`]` root-view history: that
        // history replays entries via `cluster.resolve(kind_plural)` +
        // `set_root_view`, neither of which know about the synthetic "helm"
        // plural (resolve would fail, and set_root_view would reset it back
        // to "secrets" even if it didn't) — recording it would produce a
        // history entry that can't be replayed correctly.
        self.start_watch();
    }

    pub(super) fn namespace_label(&self) -> String {
        if self.namespace.is_empty() {
            "all namespaces".to_string()
        } else {
            self.namespace.clone()
        }
    }

    /// Display name for synthetic views (Helm releases/history), which are
    /// backed by a real kind (`secrets`) that has nothing to do with what's
    /// on screen. `None` for ordinary kind-backed views.
    fn synthetic_title(&self) -> Option<&'static str> {
        match self.kind_plural.as_str() {
            "helm" => Some("helm"),
            "helmhistory" => Some("helm history"),
            _ => None,
        }
    }

    /// The "Resource:" label shown in the header. Usually just `self.kind`'s
    /// title, but naming synthetic views after `kind_plural` instead keeps
    /// the header honest about what's actually being browsed.
    pub fn resource_title(&self) -> String {
        match self.synthetic_title() {
            Some(t) => t.to_string(),
            None => self
                .kind
                .as_ref()
                .map(|k| k.title())
                .unwrap_or_else(|| "—".into()),
        }
    }

    /// The list panel's border title (k9s-style bare plural), with the same
    /// synthetic-view exception as `resource_title` so the Helm views don't
    /// leak their backing `secrets` kind.
    pub fn list_title(&self) -> String {
        match self.synthetic_title() {
            Some(t) => t.to_string(),
            None => self
                .kind
                .as_ref()
                .map(|k| k.ar.plural.clone())
                .unwrap_or_else(|| "resources".into()),
        }
    }

    // ----- view history (`[` / `]`) ---------------------------------------

    /// Record the current root view (kind + namespace). Called after every
    /// root switch; navigating with `[`/`]` bypasses this so hopping through
    /// history doesn't rewrite it. A new entry truncates the forward tail.
    pub(super) fn record_history(&mut self) {
        if self.kind.is_none() {
            return;
        }
        let entry = ViewEntry {
            kind_plural: self.kind_plural.clone(),
            namespace: self.namespace.clone(),
        };
        if self.history.get(self.history_pos) == Some(&entry) {
            return;
        }
        self.history.truncate(self.history_pos + 1);
        self.history.push(entry);
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
        self.history_pos = self.history.len() - 1;
    }

    pub(super) fn history_back(&mut self) {
        if self.history_pos == 0 {
            self.flash_warn("already at oldest view");
            return;
        }
        self.history_pos -= 1;
        self.apply_history_entry();
    }

    pub(super) fn history_forward(&mut self) {
        if self.history_pos + 1 >= self.history.len() {
            self.flash_warn("already at newest view");
            return;
        }
        self.history_pos += 1;
        self.apply_history_entry();
    }

    fn apply_history_entry(&mut self) {
        let Some(entry) = self.history.get(self.history_pos).cloned() else {
            return;
        };
        let Some(kind) = self.cluster.resolve(&entry.kind_plural) else {
            self.flash_warn(&format!("cannot resolve '{}' anymore", entry.kind_plural));
            return;
        };
        self.namespace = entry.namespace;
        let title = kind.title();
        self.set_root_view(kind);
        self.flash = format!(
            "history {}/{}: {title} in {}",
            self.history_pos + 1,
            self.history.len(),
            self.namespace_label()
        );
        self.flash_err = false;
        self.start_watch();
    }

    pub(super) fn push_frame(&mut self) {
        if self.kind.is_none() {
            return;
        }
        self.stack.push(Frame {
            kind: self.kind.clone(),
            kind_plural: self.kind_plural.clone(),
            namespace: self.namespace.clone(),
            labels: self.labels.clone(),
            fields: self.fields.clone(),
            filter: self.filter.clone(),
            scope_label: self.scope_label.clone(),
            selected: self.table_state.selected(),
        });
    }

    pub(super) fn restore(&mut self, f: Frame) {
        self.kind = f.kind;
        self.kind_plural = f.kind_plural;
        self.namespace = f.namespace;
        self.labels = f.labels;
        self.fields = f.fields;
        self.filter = f.filter;
        self.scope_label = f.scope_label;
        self.reset_sort();
        self.table_state.select(f.selected.or(Some(0)));
    }

    pub(super) fn pop_frame(&mut self) -> bool {
        if let Some(f) = self.stack.pop() {
            self.restore(f);
            self.start_watch();
            true
        } else {
            false
        }
    }

    /// (Re)start the watch for the current kind/namespace/selectors. `-l`/
    /// `-f` selectors from the filter are merged with any drill-down
    /// selectors and sent to the API, so those filter terms are evaluated
    /// server-side; the generation bump drops the superseded stream.
    pub fn start_watch(&mut self) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let (filter_labels, filter_fields) = {
            let parsed = self.parsed_filter();
            (
                parsed.labels().map(str::to_string),
                parsed.fields().map(str::to_string),
            )
        };
        self.applied_filter_labels = filter_labels;
        self.applied_filter_fields = filter_fields;
        self.generation += 1;
        self.gen_flag.store(self.generation, Ordering::SeqCst);
        for t in self.tasks.drain(..) {
            t.abort();
        }
        self.store.clear();
        self.metrics.clear();
        self.container_metrics.clear();
        self.marked.clear();
        self.invalidate_rows();
        if self.table_state.selected().is_none() {
            self.table_state.select(Some(0));
        }
        self.refresh_view_spec();
        self.apply_view_sort();
        self.maybe_fetch_printer_columns(&kind);
        let handle = self.cluster.spawn_watch(
            &kind,
            &self.namespace,
            join_selectors(&self.labels, &self.applied_filter_labels),
            join_selectors(&self.fields, &self.applied_filter_fields),
            self.generation,
            self.tx.clone(),
        );
        self.tasks.push(handle);

        if matches!(self.kind_plural.as_str(), "pods" | "nodes") {
            self.spawn_metrics_poll();
        }

        // Refresh RBAC allow-list when the namespace changes.
        if self.last_rbac_ns.as_deref() != Some(self.namespace.as_str()) {
            self.last_rbac_ns = Some(self.namespace.clone());
            self.refresh_rbac();
        }
    }

    /// For a custom resource with neither curated columns nor a user view,
    /// fetch its CRD off-thread and read `additionalPrinterColumns` for the
    /// watched version — a better automatic fallback than NAME/AGE. Results
    /// (including "nothing usable") are cached per plural for the session.
    fn maybe_fetch_printer_columns(&mut self, kind: &Kind) {
        let user_has_columns = self
            .active_user_view()
            .is_some_and(|v| !v.columns.is_empty());
        if crate::columns::has_curated(&self.kind_plural)
            || kind.ar.group.is_empty()
            || kind.ar.plural.to_lowercase() != self.kind_plural
            || self.crd_views.contains_key(&self.kind_plural)
            || user_has_columns
        {
            return;
        }
        let Some(crd_kind) = self.cluster.resolve("customresourcedefinitions") else {
            return;
        };
        let client = self.cluster.client.clone();
        let name = format!("{}.{}", self.kind_plural, kind.ar.group);
        let version = kind.ar.version.clone();
        let plural = self.kind_plural.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let handle = tokio::spawn(async move {
            let api: Api<DynamicObject> = Api::all_with(client, &crd_kind.ar);
            // No CRD (aggregated API) or no permission → stay on NAME/AGE.
            let Ok(crd) = api.get(&name).await else {
                return;
            };
            let view = crate::views::printer_columns_view(&crd.data, &version);
            let _ = tx
                .send(Msg::PrinterColumns {
                    generation: genr,
                    plural,
                    view: Box::new(view),
                })
                .await;
        });
        self.tasks.push(handle);
    }

    /// Restart the watch when the filter's `-l`/`-f` selectors no longer
    /// match what it was started with — applying them server-side, or
    /// dropping them once cleared. No-op otherwise, so local-only filter
    /// edits never cost a rewatch.
    pub(super) fn sync_filter_selectors(&mut self) {
        if !self.filter_selectors_pending() {
            return;
        }
        self.start_watch();
        if self.filter_server_side() {
            let mut parts = Vec::new();
            if let Some(l) = &self.applied_filter_labels {
                parts.push(format!("-l {l}"));
            }
            if let Some(f) = &self.applied_filter_fields {
                parts.push(format!("-f {f}"));
            }
            self.flash = format!("server-side filter: {}", parts.join(" "));
        } else {
            self.flash = "server-side filter cleared".into();
        }
        self.flash_err = false;
    }

    /// Query SelfSubjectRulesReview for the active namespace to learn which
    /// resources the user can list, so the palette can hide the rest.
    pub(super) fn refresh_rbac(&self) {
        use k8s_openapi::api::authorization::v1::{
            SelfSubjectRulesReview, SelfSubjectRulesReviewSpec,
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        // Namespace this review is computed for (echoed back so a stale result
        // from a previous namespace/context is dropped). SelfSubjectRulesReview
        // needs a concrete namespace, so "" falls back to "default".
        let current_ns = self.namespace.clone();
        let review_ns = if current_ns.is_empty() {
            "default".to_string()
        } else {
            current_ns.clone()
        };
        tokio::spawn(async move {
            let review = SelfSubjectRulesReview {
                spec: SelfSubjectRulesReviewSpec {
                    namespace: Some(review_ns),
                },
                ..Default::default()
            };
            let api: Api<SelfSubjectRulesReview> = Api::all(client);
            let Ok(resp) = api.create(&kube::api::PostParams::default(), &review).await else {
                return; // can't review → leave palette unfiltered
            };
            let Some(status) = resp.status else { return };
            // On clusters that delegate authorization (e.g. GKE → Google IAM),
            // the review comes back `incomplete` and can't enumerate what we can
            // actually access. Filtering on a partial list would wrongly hide
            // everything, so leave the palette unfiltered in that case.
            if status.incomplete {
                return;
            }
            let mut allowed = HashSet::new();
            for rule in status.resource_rules {
                let can_list = rule.verbs.iter().any(|v| v == "list" || v == "*");
                if !can_list {
                    continue;
                }
                for res in rule.resources.unwrap_or_default() {
                    if res == "*" {
                        allowed.insert("*".to_string());
                    } else {
                        // strip subresources like "pods/log"
                        allowed.insert(res.split('/').next().unwrap_or(&res).to_string());
                    }
                }
            }
            // Parsed nothing usable → don't hide the whole palette.
            if allowed.is_empty() {
                return;
            }
            let _ = tx
                .send(Msg::Rbac {
                    generation: genr,
                    ns: current_ns,
                    allowed,
                })
                .await;
        });
    }

    /// Whether a resource plural is visible under the current RBAC allow-list.
    pub(super) fn rbac_visible(&self, plural: &str) -> bool {
        match &self.rbac_allowed {
            None => true,
            Some(set) => set.contains("*") || set.contains(plural),
        }
    }

    /// Poll the metrics API every few seconds for the current pods/nodes view.
    pub(super) fn spawn_metrics_poll(&mut self) {
        let base = self.kind_plural.clone();
        let Some(mkind) = self.cluster.resolve(&format!("{base}.metrics.k8s.io")) else {
            return; // metrics-server not installed
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        let ns = self.namespace.clone();
        let ar = mkind.ar.clone();
        let namespaced = mkind.namespaced;
        let is_node = base == "nodes";

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
                    Api::namespaced_with(client.clone(), &ns, &ar)
                } else {
                    Api::all_with(client.clone(), &ar)
                };
                if let Ok(list) = api.list(&ListParams::default()).await {
                    let mut data = HashMap::new();
                    let mut containers = HashMap::new();
                    for item in list {
                        let name = item.metadata.name.clone().unwrap_or_default();
                        let key = match &item.metadata.namespace {
                            Some(n) => format!("{n}/{name}"),
                            None => name,
                        };
                        if !is_node {
                            for (container, usage) in container_usage_of(&item) {
                                containers.insert(format!("{key}/{container}"), usage);
                            }
                        }
                        data.insert(key, usage_of(&item, is_node));
                    }
                    if tx
                        .send(Msg::Metrics {
                            generation: genr,
                            data,
                            containers,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    pub(super) fn bump_generation(&mut self) {
        self.stop_event_stream();
        self.generation += 1;
        self.gen_flag.store(self.generation, Ordering::SeqCst);
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Reset { generation } if generation == self.generation => {
                self.store.clear();
                self.clear_rows_cache();
            }
            Msg::Applied {
                generation,
                key,
                obj,
            } if generation == self.generation => {
                // Record state changes against the previous version before it's
                // overwritten (session-local timeline).
                self.timeline
                    .observe(&self.kind_plural, &key, self.store.get(&key), &obj);
                self.store.apply(key.clone(), *obj);
                self.invalidate_row(&key);
            }
            Msg::Deleted { generation, key } if generation == self.generation => {
                self.timeline.observe_delete(&self.kind_plural, &key);
                self.store.remove(&key);
                self.invalidate_row(&key);
            }
            Msg::Synced { generation } if generation == self.generation => self.store.synced = true,
            Msg::Error { generation, error } if generation == self.generation => {
                self.flash = format!("error: {error}");
                self.flash_err = true;
            }
            Msg::LogLines { generation, lines } if generation == self.log_gen => {
                self.push_log_lines(lines);
            }
            Msg::LogProviderDiscovered {
                generation,
                provider,
            } if generation == self.generation => {
                // Cache the resolution (discovered transport and/or detected
                // field names) for later `L` presses. A fully-configured
                // provider stays authoritative and is never replaced.
                if self
                    .log_provider
                    .as_ref()
                    .is_none_or(|p| p.needs_discovery() || p.needs_field_detection())
                {
                    self.log_provider = Some(*provider);
                }
            }
            Msg::Metrics {
                generation,
                data,
                containers,
            } if generation == self.generation => {
                let sort_uses_metrics = self
                    .sort_column
                    .and_then(|i| {
                        let headers = self.display_headers();
                        headers.get(i).cloned()
                    })
                    .is_some_and(|h| matches!(h.as_str(), "CPU" | "MEM"));
                self.metrics = data;
                self.container_metrics = containers;
                if sort_uses_metrics {
                    self.invalidate_rows();
                }
            }
            Msg::PrinterColumns {
                generation,
                plural,
                view,
            } if generation == self.generation => {
                let for_current = plural == self.kind_plural;
                self.crd_views.insert(plural, *view);
                if for_current {
                    self.refresh_view_spec();
                }
            }
            Msg::PulseData { generation, data } if generation == self.generation => {
                self.pulse = data;
            }
            Msg::Rbac {
                generation,
                ns,
                allowed,
            } if generation == self.generation && ns == self.namespace => {
                self.rbac_allowed = Some(allowed);
            }
            Msg::XrayData { generation, items } if generation == self.generation => {
                let keep = self.xray_state.selected().unwrap_or(0);
                self.xray_items = items;
                self.xray_state
                    .select(Some(keep.min(self.xray_items.len().saturating_sub(1))));
            }
            Msg::Explain {
                generation,
                title,
                findings,
            } if generation == self.generation => {
                self.explain_items = findings;
                self.explain_title = title;
                // Land the cursor on the first navigable finding, else the top.
                let first = self
                    .explain_items
                    .iter()
                    .position(|f| f.target.is_some())
                    .unwrap_or(0);
                self.explain_state
                    .select((!self.explain_items.is_empty()).then_some(first));
                self.mode = Mode::Explain;
            }
            Msg::CanIResult {
                generation,
                text,
                ok,
            } if generation == self.generation => {
                self.flash = text;
                self.flash_err = !ok;
            }
            Msg::Gitops {
                generation,
                title,
                findings,
            } if generation == self.generation => {
                self.gitops_items = findings;
                self.gitops_title = title;
                let first = self
                    .gitops_items
                    .iter()
                    .position(|f| f.target.is_some())
                    .unwrap_or(0);
                self.gitops_state
                    .select((!self.gitops_items.is_empty()).then_some(first));
                self.mode = Mode::Gitops;
            }
            Msg::PluginOutput {
                generation,
                title,
                lines,
                warn,
            } if generation == self.generation => {
                self.detail = Scrollable {
                    title,
                    lines: lines.into(),
                    ..Default::default()
                };
                self.mode = Mode::Detail;
                match warn {
                    Some(w) => self.flash_warn(&w),
                    None => {
                        self.flash = "plugin done".into();
                        self.flash_err = false;
                    }
                }
            }
            Msg::PluginBulkDone {
                generation,
                name,
                ok,
                failed,
            } if generation == self.generation => {
                if failed.is_empty() {
                    self.flash = format!("plugin {name}: {ok} ok");
                    self.flash_err = false;
                } else {
                    let shown: Vec<&str> = failed.iter().take(3).map(String::as_str).collect();
                    let more = failed.len().saturating_sub(shown.len());
                    let tail = if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    };
                    self.flash_warn(&format!(
                        "plugin {name}: {ok} ok, {} failed — {}{tail}",
                        failed.len(),
                        shown.join("; ")
                    ));
                }
            }
            Msg::Detail {
                generation,
                title,
                lines,
                warn,
            } if generation == self.generation => {
                self.detail = Scrollable {
                    title,
                    lines: lines.into(),
                    ..Default::default()
                };
                self.mode = Mode::Detail;
                if let Some(w) = warn {
                    self.flash_warn(&w);
                }
            }
            Msg::Events {
                generation,
                title,
                lines,
            } if generation == self.event_gen => {
                self.detail.title = title;
                self.detail.lines = lines.into();
                self.detail.scroll = self
                    .detail
                    .scroll
                    .min(self.detail.lines.len().saturating_sub(1));
            }
            Msg::LogsSaved { generation, result } if generation == self.log_gen => match result {
                Ok(path) => {
                    self.flash = format!("saved logs → {}", path.display());
                    self.flash_err = false;
                }
                Err(e) => self.flash_warn(&format!("save failed: {e}")),
            },
            Msg::ClipboardCopied {
                generation,
                copied,
                success,
                failure,
            } if generation == self.generation => {
                if copied {
                    self.flash = success;
                    self.flash_err = false;
                } else {
                    self.flash_warn(&failure);
                }
            }
            Msg::Namespaces { generation, list } if generation == self.generation => {
                // Keep the picker open and preserve the selection if possible.
                let keep = self.ns_state.selected().unwrap_or(0);
                self.ns_list = list;
                self.ns_state
                    .select(Some(keep.min(self.ns_list.len().saturating_sub(1))));
            }
            Msg::Contexts { generation, list } if generation == self.generation => {
                if list.is_empty() {
                    self.mode = Mode::Table;
                    self.flash_warn("no contexts found in kubeconfig");
                } else {
                    let cur = self.cluster.context.clone();
                    let idx = list.iter().position(|c| *c == cur).unwrap_or(0);
                    self.ctx_list = list;
                    self.ctx_state.select(Some(idx));
                }
            }
            Msg::ContextSwitched {
                generation,
                name,
                result,
            } if generation == self.generation => match result {
                Ok(cluster) => self.apply_context_switch(name, cluster),
                Err(e) => {
                    self.flash_warn(&format!("context switch failed: {e}"));
                    // Never connected anywhere yet — put the picker back up
                    // instead of stranding the user on an empty table.
                    if !self.cluster.connected {
                        self.open_contexts();
                    }
                }
            },
            _ => {} // stale generation, drop
        }
    }
}
