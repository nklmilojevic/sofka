use super::*;

/// Fallback delay if the watcher backoff ever runs out of steps. `DefaultBackoff`
/// is unbounded in attempts, so this is belt and braces rather than a real path.
const NODE_PODS_BACKOFF_CEILING: Duration = Duration::from_secs(30);

fn node_pods_watch_forbidden(error: &watcher::Error) -> bool {
    match error {
        watcher::Error::InitialListFailed(kube::Error::Api(status))
        | watcher::Error::WatchStartFailed(kube::Error::Api(status))
        | watcher::Error::WatchFailed(kube::Error::Api(status)) => status.is_forbidden(),
        watcher::Error::WatchError(status) => status.is_forbidden(),
        _ => false,
    }
}

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
                    self.remember_namespace();
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
        self.owner = None;
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
        self.owner = None;
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
            owner: self.owner.clone(),
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
        self.owner = f.owner;
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
        self.clear_progress_flash();
        self.generation += 1;
        self.gen_flag.store(self.generation, Ordering::SeqCst);
        for t in self.tasks.drain(..) {
            t.abort();
        }
        // Stash the outgoing view's rows, then show the incoming view's
        // cached snapshot (if it was visited recently) so navigation renders
        // instantly — the fresh watch relists behind it and swaps in on sync.
        self.stash_view_snapshot();
        let watch_labels = join_selectors(&self.labels, &self.applied_filter_labels);
        let watch_fields = join_selectors(&self.fields, &self.applied_filter_fields);
        let key = ViewKey {
            kind_plural: self.kind_plural.clone(),
            namespace: self.namespace.clone(),
            labels: watch_labels.clone(),
            fields: watch_fields.clone(),
        };
        self.store.clear();
        if let Some(cached) = self.view_cache.get(&key) {
            self.store.seed(cached.clone());
        }
        self.watch_key = Some(key);
        self.metrics.clear();
        self.container_metrics.clear();
        self.node_pods = None;
        self.marked.clear();
        self.clear_rows_cache();
        if self.table_state.selected().is_none() {
            self.table_state.select(Some(0));
        }
        self.refresh_view_spec();
        // The remembered per-kind sort wins over a view's configured initial
        // sort: the memory *is* the user's last explicit choice for this kind.
        self.apply_remembered_sort();
        self.apply_view_sort();
        self.maybe_fetch_printer_columns(&kind);
        let handle = self.cluster.spawn_watch(
            &kind,
            &self.namespace,
            watch_labels,
            watch_fields,
            self.generation,
            self.tx.clone(),
        );
        self.tasks.push(handle);

        if matches!(self.kind_plural.as_str(), "pods" | "nodes") {
            self.spawn_metrics_poll();
        }
        if self.kind_plural == "nodes" {
            self.spawn_node_pods_poll();
        }

        // Refresh RBAC allow-list when the namespace changes.
        if self.last_rbac_ns.as_deref() != Some(self.namespace.as_str()) {
            self.last_rbac_ns = Some(self.namespace.clone());
            self.refresh_rbac();
        }
    }

    /// Stash the current store contents in the view cache under the running
    /// watch's key, so navigating back to this view renders it instantly.
    /// Only a fully-synced set is kept — a partial initial list would read as
    /// "resources disappeared" when redisplayed later.
    pub(super) fn stash_view_snapshot(&mut self) {
        let Some(key) = self.watch_key.take() else {
            return;
        };
        if !self.store.synced || self.store.is_empty() {
            return;
        }
        self.view_cache_order.retain(|k| *k != key);
        self.view_cache_order.push_back(key.clone());
        self.view_cache.insert(key, self.store.take_items());
        self.evict_view_cache();
    }

    /// Drop least-recently-used snapshots until the cache is under both of its
    /// bounds.
    ///
    /// The view count alone is not a memory bound: eight snapshots of a
    /// 2,000-pod view cost ~143 MiB where one costs ~18 MiB (see
    /// `examples/memprobe.rs`), because the cap counts *views*, not objects.
    /// So a second bound caps the total retained objects. The most recent
    /// entry is always kept even if it alone exceeds the object bound —
    /// dropping it would defeat the one case the cache exists for, which is
    /// stepping straight back into the view you just left.
    fn evict_view_cache(&mut self) {
        let too_many_objects = |cache: &HashMap<ViewKey, crate::store::Items>| {
            cache.values().map(HashMap::len).sum::<usize>() > VIEW_CACHE_MAX_OBJECTS
        };
        while self.view_cache_order.len() > VIEW_CACHE_MAX
            || (self.view_cache_order.len() > 1 && too_many_objects(&self.view_cache))
        {
            match self.view_cache_order.pop_front() {
                Some(oldest) => {
                    self.view_cache.remove(&oldest);
                }
                None => break,
            }
        }
    }

    /// Drop every cached view snapshot (context switch: another cluster's
    /// resources, and possibly different RBAC, must never be redisplayed).
    pub(super) fn clear_view_cache(&mut self) {
        self.watch_key = None;
        self.view_cache.clear();
        self.view_cache_order.clear();
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
                let msg = match api.list(&ListParams::default()).await {
                    Ok(list) => {
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
                        Msg::Metrics {
                            generation: genr,
                            data,
                            containers,
                        }
                    }
                    // A present-but-broken metrics API previously died here in
                    // silence, leaving the CPU/MEM columns frozen forever.
                    Err(e) => Msg::MetricsError {
                        generation: genr,
                        error: e.to_string(),
                    },
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    /// Watch the pods API for the nodes view: pod count per node (the PODS
    /// column). Counts non-terminated pods — Succeeded/Failed pods hold no
    /// node resources — mirroring `kubectl describe node`. Replaces a full
    /// cluster-wide pod re-list every 10s with one watch, kept incrementally
    /// up to date and coalesced to at most one `Msg::NodePods` per second.
    ///
    /// RBAC granting `list` but not `watch` still works: a refused watch falls
    /// back to the old 10-second list poll. Other transient watcher failures
    /// use client-go's backoff while the stream heals itself.
    pub(super) fn spawn_node_pods_poll(&mut self) {
        let Some(pkind) = self.cluster.resolve("pods") else {
            return;
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        let ar = pkind.ar.clone();

        let handle = tokio::spawn(async move {
            let api: Api<DynamicObject> = Api::all_with(client, &ar);
            let cfg = watcher::Config::default()
                .any_semantic()
                .fields("status.phase!=Succeeded,status.phase!=Failed");
            // `watcher` re-lists as fast as the stream is polled, so an error
            // that does not clear itself would hammer the API server —
            // measured at ~9,800 requests a second against a test server.
            //
            // The pacing is driven here rather than through `.default_backoff()`
            // because that wrapper resets on *any* `Ok` item, and `watcher`
            // emits `Ok(Event::Init)` before every list attempt. A failing
            // initial list therefore cycles `Init -> error -> minimum delay`
            // forever and never escalates, which is worse than the 10s list
            // poll it replaced. The strategy is still client-go's — 800ms
            // doubling to 30s, jittered, self-resetting after 2 minutes of
            // quiet — but it is reset only once the stream has actually got
            // somewhere: see `established` below.
            let mut stream = watcher(api.clone(), cfg).boxed();
            let mut backoff = watcher::DefaultBackoff::default();
            // Node per pod, kept incrementally so per-node counts never need
            // a full rescan of the cluster's pods.
            let mut pod_nodes: HashMap<String, String> = HashMap::new();
            let mut counts: HashMap<String, usize> = HashMap::new();
            let mut dirty = false;
            // The initial list arrives as a stream of `InitApply`s, so the
            // counts are incomplete until `InitDone`. Publishing mid-init
            // would walk the PODS column up from zero on every (re)sync —
            // the old full-list poll only ever emitted complete counts.
            let mut synced = false;
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut poll_fallback = false;
            loop {
                tokio::select! {
                    maybe_event = stream.next() => {
                        let Some(event) = maybe_event else { break };
                        if flag.load(Ordering::SeqCst) != genr {
                            break;
                        }
                        let retire = |node: &str, counts: &mut HashMap<String, usize>| {
                            if let Some(c) = counts.get_mut(node) {
                                *c = c.saturating_sub(1);
                                if *c == 0 {
                                    counts.remove(node);
                                }
                            }
                        };
                        // Progress, as opposed to another doomed list attempt.
                        // `Init` and the `InitApply`s behind it are replayed on
                        // every attempt, so resetting on those is exactly the
                        // mistake `.default_backoff()` makes.
                        let established = matches!(
                            event,
                            Ok(watcher::Event::Apply(_)
                                | watcher::Event::Delete(_)
                                | watcher::Event::InitDone)
                        );
                        if established {
                            backoff.reset();
                        }
                        match event {
                            Ok(watcher::Event::Apply(obj)) | Ok(watcher::Event::InitApply(obj)) => {
                                let key = row_key(&obj);
                                let new_node = obj
                                    .data
                                    .pointer("/spec/nodeName")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string);
                                let old_node = match &new_node {
                                    Some(n) => pod_nodes.insert(key, n.clone()),
                                    None => pod_nodes.remove(&key),
                                };
                                if old_node != new_node {
                                    if let Some(old) = &old_node {
                                        retire(old, &mut counts);
                                    }
                                    if let Some(new) = &new_node {
                                        *counts.entry(new.clone()).or_insert(0) += 1;
                                    }
                                    dirty = true;
                                }
                            }
                            Ok(watcher::Event::Delete(obj)) => {
                                if let Some(old) = pod_nodes.remove(&row_key(&obj)) {
                                    retire(&old, &mut counts);
                                    dirty = true;
                                }
                            }
                            Ok(watcher::Event::Init) => {
                                pod_nodes.clear();
                                counts.clear();
                                synced = false;
                            }
                            Ok(watcher::Event::InitDone) => {
                                synced = true;
                                dirty = true;
                            }
                            // A list-only RBAC grant cannot maintain counts via
                            // this watcher: after a refused watch, kube retries
                            // that watch from the same resourceVersion rather
                            // than re-listing. Restore the periodic list path in
                            // that case.
                            Err(error) if node_pods_watch_forbidden(&error) => {
                                poll_fallback = true;
                                break;
                            }
                            // Everything else is transient and the stream heals
                            // itself, so pace the next attempt and stay on the
                            // watch. Sleeping here also stops publishing while
                            // the counts are going nowhere; `dirty` survives, so
                            // the next tick after recovery still emits.
                            Err(_) => {
                                let delay =
                                    backoff.next().unwrap_or(NODE_PODS_BACKOFF_CEILING);
                                tokio::time::sleep(delay).await;
                            }
                        }
                    }
                    _ = ticker.tick() => {
                        if flag.load(Ordering::SeqCst) != genr {
                            break;
                        }
                        if dirty && synced {
                            if tx
                                .send(Msg::NodePods {
                                    generation: genr,
                                    counts: counts.clone(),
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                            dirty = false;
                        }
                    }
                }
            }

            if !poll_fallback {
                return;
            }

            let params =
                ListParams::default().fields("status.phase!=Succeeded,status.phase!=Failed");
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                if let Ok(list) = api.list(&params).await {
                    let mut counts: HashMap<String, usize> = HashMap::new();
                    for item in list {
                        if let Some(node) = item
                            .data
                            .pointer("/spec/nodeName")
                            .and_then(serde_json::Value::as_str)
                        {
                            *counts.entry(node.to_string()).or_insert(0) += 1;
                        }
                    }
                    if tx
                        .send(Msg::NodePods {
                            generation: genr,
                            counts,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
        self.tasks.push(handle);
    }

    pub(super) fn bump_generation(&mut self) {
        self.stop_event_stream();
        self.clear_progress_flash();
        self.generation += 1;
        self.gen_flag.store(self.generation, Ordering::SeqCst);
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Reset { generation } if generation == self.generation => {
                // With rows on screen (cached snapshot or established watch)
                // the store buffers the relist and keeps showing them; only a
                // genuine clear invalidates what's rendered.
                if self.store.begin_reset() {
                    self.clear_rows_cache();
                }
            }
            Msg::Applied {
                generation,
                key,
                obj,
            } if generation == self.generation => {
                // Record state changes against the previous version before it's
                // overwritten (session-local timeline), and keep that version
                // for the session diff (`:diff` on objects with no
                // last-applied annotation).
                let prev = self.store.latest(&key);
                self.timeline
                    .observe(&self.kind_plural, &key, prev.map(Arc::as_ref), &obj);
                if let Some(prev) = prev
                    && prev.metadata.resource_version != obj.metadata.resource_version
                {
                    // An `Arc` bump, not a deep copy of the object's whole
                    // JSON body — this runs on every changed watch event.
                    self.prev_revisions
                        .insert(&self.kind_plural, &key, Arc::clone(prev));
                }
                match self.store.apply(key.clone(), *obj) {
                    StoreMutation::Inserted => self.invalidate_row(&key),
                    StoreMutation::Updated => self.invalidate_row_contents(&key),
                    StoreMutation::Buffered | StoreMutation::Removed | StoreMutation::Unchanged => {
                    }
                }
            }
            Msg::Deleted { generation, key } if generation == self.generation => {
                self.timeline.observe_delete(&self.kind_plural, &key);
                if self.store.remove(&key) == StoreMutation::Removed {
                    self.invalidate_row(&key);
                }
            }
            Msg::Synced { generation } if generation == self.generation => {
                if self.store.finish_sync() {
                    self.clear_rows_cache();
                }
            }
            Msg::Error { generation, error } if generation == self.generation => {
                self.watch_errors = self.watch_errors.saturating_add(1);
                self.last_error = Some(error.clone());
                self.borrow_status(format!("error: {error}"), true);
            }
            Msg::StateWriteFailed { id, error } => {
                self.last_state_write_error = Some(error.clone());
                self.borrow_status(format!("state not saved: {error}"), true);
                if let Some(writer) = &self.state_writer {
                    writer.acknowledge_failure(id);
                }
            }
            Msg::Flash {
                generation,
                claim,
                message,
                err,
            } if generation == self.generation => {
                self.set_claimed_status(claim, message, err);
            }
            Msg::Panic(error) => {
                self.last_error = Some(error.clone());
                self.borrow_status(format!("internal error: {error}"), true);
            }
            Msg::Notify(text) => {
                self.borrow_status(format!("🔔 {text}"), false);
                // Delivery happens once per frame in the run loop (see
                // `take_notification`), so a batch of these coalesces.
                self.pending_notify.push(text);
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
                    .is_some_and(|h| matches!(h.as_str(), "CPU" | "MEM" | "%CPU" | "%MEM"));
                if !data.is_empty() || !containers.is_empty() {
                    self.metrics_seen = true;
                }
                self.metrics_error = None;
                self.metrics = data;
                self.container_metrics = containers;
                if sort_uses_metrics {
                    self.invalidate_rows();
                }
            }
            Msg::NodePods { generation, counts } if generation == self.generation => {
                let sort_uses_pods = self
                    .sort_column
                    .and_then(|i| self.display_headers().get(i).cloned())
                    .is_some_and(|h| h == "PODS");
                self.node_pods = Some(counts);
                if sort_uses_pods {
                    self.invalidate_rows();
                }
            }
            Msg::MetricsError { generation, error } if generation == self.generation => {
                self.metrics_error = Some(error);
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
                    // A remembered sort on a printer column only becomes
                    // resolvable now that the CRD's columns are known.
                    self.apply_remembered_sort();
                }
            }
            Msg::FindResults {
                generation,
                claim,
                query,
                items,
                warn,
            } if generation == self.generation => {
                match warn {
                    Some(w) => {
                        self.set_claimed_status(claim, format!("find is incomplete — {w}"), true)
                    }
                    None => self.set_claimed_status(
                        claim,
                        format!("{} hit(s) for '{query}'", items.len()),
                        false,
                    ),
                }
                self.find_query = query;
                self.find_items = items;
                self.find_state
                    .select((!self.find_items.is_empty()).then_some(0));
            }
            Msg::PulseData {
                generation,
                claim,
                data,
            } if generation == self.generation => {
                self.set_recurring_status(
                    claim,
                    data.warn
                        .as_ref()
                        .map(|w| format!("pulse is incomplete — {w}")),
                );
                self.pulse = data;
            }
            Msg::Rbac {
                generation,
                ns,
                allowed,
            } if generation == self.generation && ns == self.namespace => {
                self.rbac_allowed = Some(allowed);
            }
            Msg::XrayData {
                generation,
                claim,
                items,
                warn,
            } if generation == self.generation => {
                self.set_recurring_status(claim, warn.map(|w| format!("xray is incomplete — {w}")));
                let keep = self.xray_state.selected().unwrap_or(0);
                self.xray_items = items;
                self.xray_state
                    .select(Some(keep.min(self.xray_items.len().saturating_sub(1))));
            }
            Msg::Explain {
                generation,
                claim,
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
                // As in the `Msg::Gitops` arm below: the "explaining X…"
                // progress flash has done its job now the findings are up.
                self.clear_claimed_status(claim);
            }
            Msg::Gitops {
                generation,
                claim,
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
                self.clear_claimed_status(claim);
            }
            Msg::PluginOutput {
                generation,
                claim,
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
                    Some(w) => self.set_claimed_status(claim, w, true),
                    None => self.set_claimed_status(claim, "plugin done", false),
                }
            }
            Msg::PluginBulkDone {
                generation,
                claim,
                name,
                ok,
                failed,
            } if generation == self.generation => {
                if failed.is_empty() {
                    self.set_claimed_status(claim, format!("plugin {name}: {ok} ok"), false);
                } else {
                    let shown: Vec<&str> = failed.iter().take(3).map(String::as_str).collect();
                    let more = failed.len().saturating_sub(shown.len());
                    let tail = if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    };
                    self.set_claimed_status(
                        claim,
                        format!(
                            "plugin {name}: {ok} ok, {} failed — {}{tail}",
                            failed.len(),
                            shown.join("; ")
                        ),
                        true,
                    );
                }
            }
            Msg::DebuggersCleaned {
                generation,
                claim,
                deleted,
                failed,
            } if generation == self.generation => {
                if failed.is_empty() {
                    self.set_claimed_status(
                        claim,
                        format!("removed {deleted} node debugger pod(s)"),
                        false,
                    );
                } else {
                    let shown: Vec<&str> = failed.iter().take(3).map(String::as_str).collect();
                    self.set_claimed_status(
                        claim,
                        format!(
                            "debug-clean: removed {deleted}, {} failed — {}",
                            failed.len(),
                            shown.join("; ")
                        ),
                        true,
                    );
                }
            }
            Msg::Bundle {
                generation,
                claim,
                title,
                text,
                filename,
            } if generation == self.generation => {
                self.detail = Scrollable {
                    title: format!("{title} (:bundle-save to write)"),
                    lines: text.lines().map(String::from).collect(),
                    ..Default::default()
                };
                self.pending_bundle = Some((filename, text));
                self.set_return_mode();
                self.mode = Mode::Detail;
                self.set_claimed_status(claim, "bundle ready — review, then :bundle-save", false);
            }
            Msg::BundleSaved {
                generation,
                claim,
                result,
            } if generation == self.generation => match result {
                Ok(path) => self.set_claimed_status(
                    claim,
                    format!("saved bundle → {}", path.display()),
                    false,
                ),
                Err(e) => self.set_claimed_status(claim, format!("bundle save failed: {e}"), true),
            },
            Msg::FleetRow { generation, row } if generation == self.generation => {
                self.apply_fleet_row(*row);
            }
            Msg::SnapshotSaved {
                generation,
                claim,
                result,
            } if generation == self.generation => match result {
                Ok(path) => self.set_claimed_status(
                    claim,
                    format!("saved snapshot → {}", path.display()),
                    false,
                ),
                Err(e) => {
                    self.set_claimed_status(claim, format!("snapshot save failed: {e}"), true)
                }
            },
            Msg::Detail {
                generation,
                claim,
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
                    Some(w) => self.set_claimed_status(claim, w, true),
                    // The "describing X…" progress flash has served its
                    // purpose once the document arrives.
                    None => self.clear_claimed_status(claim),
                }
            }
            Msg::Events {
                generation,
                title,
                lines,
            } if generation == self.event_gen => {
                self.detail.title = title;
                // Through `replace_lines`, not a direct assignment: a
                // refreshed events list can have the same line count as the
                // one it replaces, which would otherwise leave a stale
                // search-match cache behind.
                self.detail.replace_lines(lines.into());
            }
            Msg::TransferDone {
                generation,
                claim,
                result,
            } if generation == self.generation => match result {
                Ok(summary) => self.set_claimed_status(claim, summary, false),
                Err(e) => self.set_claimed_status(claim, format!("cp failed: {e}"), true),
            },
            Msg::LogsSaved {
                generation,
                claim,
                result,
            } if generation == self.generation => match result {
                Ok(path) => self.set_claimed_status(
                    claim,
                    format!("saved logs → {}", path.display()),
                    false,
                ),
                Err(e) => self.set_claimed_status(claim, format!("save failed: {e}"), true),
            },
            Msg::ClipboardCopied {
                generation,
                claim,
                copied,
                success,
                failure,
            } if generation == self.generation => {
                if copied {
                    self.set_claimed_status(claim, success, false);
                } else {
                    self.set_claimed_status(claim, failure, true);
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
            Msg::ContextRenamed {
                generation,
                claim,
                old,
                new,
                result,
            } if generation == self.generation => match result {
                Ok(()) => {
                    // Patch the cached lists in place — kubectl already
                    // rewrote the kubeconfig, so a re-read would say the same.
                    for list in [&mut self.ctx_list, &mut self.all_contexts] {
                        if let Some(c) = list.iter_mut().find(|c| **c == old) {
                            *c = new.clone();
                        }
                        list.sort();
                    }
                    if self.mode == Mode::Contexts {
                        let idx = self.filtered_contexts().iter().position(|c| *c == new);
                        self.ctx_state.select(Some(idx.unwrap_or(0)));
                    }
                    // The live connection is unaffected; only the name moves.
                    if self.cluster.context == old {
                        self.cluster.context = new.clone();
                        if let Some(recents) = self.recent_namespaces.remove(&old) {
                            self.recent_namespaces.insert(new.clone(), recents);
                        }
                    }
                    self.set_claimed_status(claim, format!("renamed context {old} → {new}"), false);
                }
                Err(e) => self.set_claimed_status(claim, format!("rename failed: {e}"), true),
            },
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
