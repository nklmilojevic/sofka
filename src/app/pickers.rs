use super::*;

/// How many recent namespaces to keep per context in the switcher.
const MAX_RECENT_NAMESPACES: usize = 8;

impl App {
    /// Open the namespace switcher immediately with a loading placeholder, then
    /// fetch the list off-thread (it arrives as `Msg::Namespaces`).
    pub(super) fn open_namespaces(&mut self) {
        // Show whatever is cached immediately (instant reopen); a fresh fetch
        // refreshes it. Only fall back to the bare `<all>` placeholder when the
        // cache is empty.
        if self.ns_list.is_empty() {
            self.ns_list = vec!["<all>".into()];
        }
        self.ns_state.select(Some(0));
        self.ns_filter.clear();
        self.mode = Mode::Namespaces;
        self.spawn_namespace_fetch();
    }

    /// Fetch the namespace list off-thread; it arrives as `Msg::Namespaces` and
    /// refreshes `ns_list`, which backs both the switcher popup and `:<kind>
    /// <ns>` palette completion.
    pub(super) fn spawn_namespace_fetch(&self) {
        let client = self.cluster.client.clone();
        let kind = self.cluster.resolve("namespaces").map(|k| k.ar);
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let Some(ar) = kind else { return };
            let api: Api<DynamicObject> = Api::all_with(client, &ar);
            if let Ok(list) = api.list(&ListParams::default()).await {
                let mut names: Vec<String> = list
                    .items
                    .into_iter()
                    .filter_map(|o| o.metadata.name)
                    .collect();
                names.sort();
                names.insert(0, "<all>".into());
                let _ = tx
                    .send(Msg::Namespaces {
                        generation: genr,
                        list: names,
                    })
                    .await;
            }
        });
    }

    /// Warm the namespace cache when the command palette opens, so `:<kind>
    /// <ns>` can offer completions without waiting for the switcher popup. A
    /// no-op once real namespaces are cached (the `<all>` sentinel doesn't
    /// count).
    pub(super) fn ensure_namespace_cache(&mut self) {
        if !self.ns_list.iter().any(|n| n != "<all>") {
            self.spawn_namespace_fetch();
        }
    }

    /// Namespaces for the switcher: `<all>` is always pinned first. When
    /// browsing (no filter), configured favourites lead, then session recents,
    /// then the remaining namespaces alphabetically. With a filter active,
    /// everything is fuzzy-matched (favourites/recents lose their pinning so
    /// the best textual match wins).
    pub fn filtered_namespaces(&self) -> Vec<String> {
        let mut out = vec!["<all>".to_string()];
        let rest = self.ns_list.iter().filter(|n| n.as_str() != "<all>");
        if !self.ns_filter.is_empty() {
            let mut scored: Vec<(i64, &String)> = rest
                .filter_map(|n| self.matcher.fuzzy_match(n, &self.ns_filter).map(|s| (s, n)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            out.extend(scored.into_iter().map(|(_, n)| n.clone()));
            return out;
        }

        let available: std::collections::HashSet<&str> = rest.map(String::as_str).collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Favourites first, in configured order (pinned even if not currently
        // listable — the switcher still accepts a verbatim pick).
        for f in &self.namespace_favorites {
            if !f.is_empty() && seen.insert(f.clone()) {
                out.push(f.clone());
            }
        }
        // Then session recents that still exist and aren't already favourites.
        for r in self.recent_namespaces_for_context() {
            if available.contains(r.as_str()) && seen.insert(r.clone()) {
                out.push(r);
            }
        }
        // Then everything else (ns_list is already sorted).
        for n in self.ns_list.iter().filter(|n| n.as_str() != "<all>") {
            if seen.insert(n.clone()) {
                out.push(n.clone());
            }
        }
        out
    }

    /// The recent namespaces for the current context, newest first.
    fn recent_namespaces_for_context(&self) -> Vec<String> {
        self.recent_namespaces
            .get(&self.cluster.context)
            .map(|dq| dq.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether `n` is a configured favourite namespace.
    pub fn is_favorite_namespace(&self, n: &str) -> bool {
        self.namespace_favorites.iter().any(|f| f == n)
    }

    /// Whether `n` is a session-recent namespace for the current context.
    pub fn is_recent_namespace(&self, n: &str) -> bool {
        self.recent_namespaces
            .get(&self.cluster.context)
            .is_some_and(|dq| dq.iter().any(|r| r == n))
    }

    /// Record a real namespace selection into the current context's recents
    /// (newest first, deduped, bounded). `<all>`/empty are not recorded.
    pub(super) fn note_recent_namespace(&mut self, ns: &str) {
        if ns.is_empty() || ns == "<all>" {
            return;
        }
        let dq = self
            .recent_namespaces
            .entry(self.cluster.context.clone())
            .or_default();
        dq.retain(|r| r != ns);
        dq.push_front(ns.to_string());
        while dq.len() > MAX_RECENT_NAMESPACES {
            dq.pop_back();
        }
    }

    pub(super) fn key_namespaces(&mut self, key: KeyEvent) {
        let len = self.filtered_namespaces().len();
        match key.code {
            KeyCode::Esc => {
                // First esc clears the filter and jumps back to the top
                // (`<all>`); a second esc closes the switcher.
                if self.ns_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.ns_filter.clear();
                    self.ns_state.select(Some(0));
                }
            }
            KeyCode::Down => list_step(&mut self.ns_state, len, true),
            KeyCode::Up => list_step(&mut self.ns_state, len, false),
            KeyCode::Enter => {
                let filtered = self.filtered_namespaces();
                let has_real_match = filtered.iter().any(|n| n != "<all>");
                let chosen = if !self.ns_filter.trim().is_empty() && !has_real_match {
                    // Typed text matches no listed namespace → take it verbatim
                    // so you can still switch when listing is restricted.
                    Some(self.ns_filter.trim().to_string())
                } else {
                    self.ns_state
                        .selected()
                        .and_then(|i| filtered.get(i).cloned())
                };
                if let Some(ns) = chosen {
                    self.set_namespace(ns);
                }
            }
            KeyCode::Backspace => {
                self.ns_filter.pop();
                self.select_best_namespace_match();
            }
            KeyCode::Char(c) => {
                self.ns_filter.push(c);
                self.select_best_namespace_match();
            }
            _ => {}
        }
    }

    /// Jump the namespace-switcher cursor to the best fuzzy match after the
    /// filter buffer changes. `<all>` stays pinned at index 0 of the list (so
    /// it's always reachable), but it should only be *selected* by default
    /// when browsing with no filter — once you've typed something with a
    /// real match, that match belongs under the cursor, not `<all>`.
    pub(super) fn select_best_namespace_match(&mut self) {
        let idx = if !self.ns_filter.is_empty() && self.filtered_namespaces().len() > 1 {
            1 // right after the pinned <all> — the top-scored real match
        } else {
            0
        };
        self.ns_state.select(Some(idx));
    }

    pub(super) fn set_namespace(&mut self, sel: String) {
        self.namespace = normalize_ns(&sel);
        self.note_recent_namespace(&sel);
        self.flash = format!("namespace: {}", self.namespace_label());
        self.flash_err = false;
        self.ns_filter.clear();
        self.mode = Mode::Table;
        self.table_state.select(Some(0));
        self.record_history();
        self.start_watch();
    }

    /// Start the session in the context picker because the current context's
    /// API server was unreachable at launch (k9s behavior). The connect error
    /// stays visible in the status line while picking.
    pub fn start_disconnected(&mut self, error: &str) {
        let label = if self.cluster.context.is_empty() {
            "cannot connect".to_string()
        } else {
            format!("cannot connect to '{}'", self.cluster.context)
        };
        self.open_contexts();
        self.flash_warn(&format!("{label}: {error} — pick another context"));
    }

    pub(super) fn open_contexts(&mut self) {
        self.ctx_filter.clear();
        self.ctx_list.clear();
        self.ctx_state.select(None);
        self.mode = Mode::Contexts;
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let mut list = Cluster::list_contexts();
            list.sort();
            let _ = tx
                .send(Msg::Contexts {
                    generation: genr,
                    list,
                })
                .await;
        });
    }

    /// Contexts for the switcher, fuzzy-matched against the type-to-filter
    /// buffer (see `filtered_namespaces` for the same pattern).
    pub fn filtered_contexts(&self) -> Vec<String> {
        if self.ctx_filter.is_empty() {
            return self.ctx_list.clone();
        }
        let mut scored: Vec<(i64, &String)> = self
            .ctx_list
            .iter()
            .filter_map(|c| {
                self.matcher
                    .fuzzy_match(c, &self.ctx_filter)
                    .map(|s| (s, c))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().map(|(_, c)| c.clone()).collect()
    }

    pub(super) fn key_contexts(&mut self, key: KeyEvent) {
        let len = self.filtered_contexts().len();
        match key.code {
            KeyCode::Esc => {
                // First esc clears the filter, second closes the switcher.
                if self.ctx_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.ctx_filter.clear();
                    self.ctx_state.select(Some(0));
                }
            }
            KeyCode::Down => list_step(&mut self.ctx_state, len, true),
            KeyCode::Up => list_step(&mut self.ctx_state, len, false),
            KeyCode::Enter => {
                if let Some(name) = self
                    .ctx_state
                    .selected()
                    .and_then(|i| self.filtered_contexts().get(i).cloned())
                {
                    self.mode = Mode::Table;
                    self.switch_context(name);
                }
            }
            KeyCode::Backspace => {
                self.ctx_filter.pop();
                self.ctx_state.select(Some(0));
            }
            KeyCode::Char(c) => {
                self.ctx_filter.push(c);
                self.ctx_state.select(Some(0));
            }
            _ => {}
        }
    }

    /// Rebuild the cluster connection against a different kubeconfig context.
    /// Reconnecting re-runs API discovery, which can take seconds, so it runs
    /// off-thread; the new cluster (or error) arrives as `Msg::ContextSwitched`.
    pub(super) fn switch_context(&mut self, name: String) {
        // Re-selecting the current context is a no-op — unless we never
        // connected to it, in which case picking it again is a retry.
        if name == self.cluster.context && self.cluster.connected {
            return;
        }
        self.flash = format!("switching to {name}…");
        self.flash_err = false;
        // Stop the current context's watches and clear stale rows while we
        // reconnect; the new watch starts when the connection lands.
        self.bump_generation();
        self.store.clear();
        self.invalidate_rows();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let result = Cluster::connect_context(&name)
                .await
                .map(Box::new)
                .map_err(|e| e.to_string());
            let _ = tx
                .send(Msg::ContextSwitched {
                    generation: genr,
                    name,
                    result,
                })
                .await;
        });
    }

    /// Install a freshly-connected cluster from a context switch. Config is
    /// re-resolved so per-cluster/per-context overrides (aliases, plugins,
    /// skin, defaults) follow the new context.
    pub(super) fn apply_context_switch(&mut self, name: String, mut cluster: Box<Cluster>) {
        let resolved = self.config.resolve(&name, &cluster.cluster_name);
        self.user_aliases = resolved.config.aliases;
        self.namespace_favorites = resolved.config.favorite_namespaces;
        self.plugins = resolved.config.plugins;
        self.bookmarks = resolved.config.bookmarks;
        self.workspaces = resolved.config.workspaces;
        self.guardrails = resolved.config.guardrails;
        self.debug = resolved.config.debug;
        self.bundle_cfg = resolved.config.bundle;
        // Tracked debuggers belong to the previous cluster/context.
        self.launched_node_debuggers.clear();
        let mut plugin_warnings = crate::config::plugin_warnings(&self.plugins);
        plugin_warnings.extend(crate::config::bookmark_warnings(&self.bookmarks));
        plugin_warnings.extend(crate::config::workspace_warnings(&self.workspaces));
        plugin_warnings.extend(crate::config::guardrail_warnings(&self.guardrails));
        let (views, view_warnings) = crate::views::compile(&resolved.config.views);
        self.user_views = views;
        let (thresholds, threshold_warnings) =
            crate::thresholds::compile(&resolved.config.thresholds);
        self.thresholds = thresholds;
        let (log_provider, provider_warnings) =
            crate::providers::compile(resolved.config.providers.logs.as_ref());
        self.log_provider = log_provider;
        // Printer-column fallbacks came from the old cluster's CRDs.
        self.crd_views.clear();
        // The timeline recorded the old cluster's objects.
        self.timeline.clear();
        self.skin_colors = resolved.config.skin.colors;
        self.readonly = self.readonly_override.unwrap_or(resolved.config.readonly);
        cluster.add_aliases(&self.user_aliases);
        self.bump_generation();
        self.namespace = resolved
            .config
            .default_namespace
            .unwrap_or_else(|| cluster.default_namespace.clone());
        self.cluster = *cluster;
        self.stack.clear();
        // View history references the old cluster's kinds and namespaces.
        self.history.clear();
        self.history_pos = 0;
        self.kind = None;
        self.kind_plural.clear();
        self.labels = None;
        self.fields = None;
        self.scope_label = None;
        self.filter.clear();
        // The old cluster's namespaces don't apply here — drop them so palette
        // completion re-fetches against the new cluster on the next `:`.
        self.ns_list.clear();
        // Permissions differ per cluster — drop the old allow-list.
        self.rbac_allowed = None;
        self.last_rbac_ns = None;
        crate::theme::set_background(resolved.config.skin.background);
        self.apply_context_skin(resolved.skin_override);
        self.flash = format!("context: {name}");
        self.flash_err = false;
        if let Some(w) = resolved
            .warnings
            .first()
            .or(view_warnings.first())
            .or(plugin_warnings.first())
            .or(threshold_warnings.first())
            .or(provider_warnings.first())
        {
            self.flash_warn(w);
        }
        // Keep `:config` in sync with the layers just resolved for this context.
        self.config_warnings = resolved.warnings;
        self.config_warnings.extend(plugin_warnings);
        self.config_warnings.extend(threshold_warnings);
        // A bookmark/workspace that requested this context lands on its own
        // view(s); a plain switch lands on the context's default resource.
        if self.pending_workspace.is_some() {
            self.apply_pending_workspace();
        } else if self.pending_bookmark.is_some() {
            self.apply_pending_bookmark();
        } else {
            let kind = resolved
                .config
                .default_resource
                .unwrap_or_else(|| "pods".into());
            self.switch_kind(&kind);
        }
    }
}
