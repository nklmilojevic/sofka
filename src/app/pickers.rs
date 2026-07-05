use super::*;

impl App {
    /// Open the namespace switcher immediately with a loading placeholder, then
    /// fetch the list off-thread (it arrives as `Msg::Namespaces`).
    pub(super) fn open_namespaces(&mut self) {
        self.ns_list = vec!["<all>".into()];
        self.ns_state.select(Some(0));
        self.ns_filter.clear();
        self.mode = Mode::Namespaces;
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

    /// Namespaces for the switcher: `<all>` is always pinned first, the rest
    /// fuzzy-matched against the type-to-filter buffer.
    pub fn filtered_namespaces(&self) -> Vec<String> {
        let mut out = vec!["<all>".to_string()];
        let rest = self.ns_list.iter().filter(|n| n.as_str() != "<all>");
        if self.ns_filter.is_empty() {
            out.extend(rest.cloned());
        } else {
            let mut scored: Vec<(i64, &String)> = rest
                .filter_map(|n| self.matcher.fuzzy_match(n, &self.ns_filter).map(|s| (s, n)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            out.extend(scored.into_iter().map(|(_, n)| n.clone()));
        }
        out
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
        self.flash = format!("namespace: {}", self.namespace_label());
        self.flash_err = false;
        self.ns_filter.clear();
        self.mode = Mode::Table;
        self.table_state.select(Some(0));
        self.record_history();
        self.start_watch();
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
        if name == self.cluster.context {
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

    /// Install a freshly-connected cluster from a context switch.
    pub(super) fn apply_context_switch(&mut self, name: String, mut cluster: Box<Cluster>) {
        cluster.add_aliases(&self.user_aliases);
        self.bump_generation();
        self.namespace = cluster.default_namespace.clone();
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
        // Permissions differ per cluster — drop the old allow-list.
        self.rbac_allowed = None;
        self.last_rbac_ns = None;
        self.apply_context_skin(&name);
        self.flash = format!("context: {name}");
        self.flash_err = false;
        self.switch_kind("pods");
    }
}
