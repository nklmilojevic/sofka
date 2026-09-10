use super::*;

/// How many recent namespaces to keep per context in the switcher.
const MAX_RECENT_NAMESPACES: usize = 8;

/// The sort picker's pinned first entry: clears the sort back to the default
/// (namespace, name) ordering.
pub const DEFAULT_SORT_LABEL: &str = "default (ns/name)";

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
    pub fn filtered_namespaces(&self) -> Rc<Vec<String>> {
        if let Some(m) = self.picker_memos.borrow().namespaces.as_ref()
            && m.filter == self.ns_filter
            && m.context == self.cluster.context
            && m.ns_list == self.ns_list
            && m.favorites == self.namespace_favorites
            && m.recents
                .iter()
                .map(String::as_str)
                .eq(self.recent_namespaces_for_context())
        {
            return Rc::clone(&m.value);
        }

        let mut out = vec!["<all>".to_string()];
        let rest = self.ns_list.iter().filter(|n| n.as_str() != "<all>");
        if !self.ns_filter.is_empty() {
            let mut scored: Vec<(i64, &String)> = rest
                .filter_map(|n| self.matcher.score(n, &self.ns_filter).map(|s| (s, n)))
                .collect();
            scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            out.extend(scored.into_iter().map(|(_, n)| n.clone()));
            return self.remember_namespaces(out);
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
            if available.contains(r) && seen.insert(r.to_string()) {
                out.push(r.to_string());
            }
        }
        // Then everything else (ns_list is already sorted).
        for n in self.ns_list.iter().filter(|n| n.as_str() != "<all>") {
            if seen.insert(n.clone()) {
                out.push(n.clone());
            }
        }
        self.remember_namespaces(out)
    }

    fn remember_namespaces(&self, out: Vec<String>) -> Rc<Vec<String>> {
        let value = Rc::new(out);
        self.picker_memos.borrow_mut().namespaces = Some(NamespaceMemo {
            filter: self.ns_filter.clone(),
            context: self.cluster.context.clone(),
            ns_list: self.ns_list.clone(),
            favorites: self.namespace_favorites.clone(),
            recents: self
                .recent_namespaces_for_context()
                .map(str::to_string)
                .collect(),
            value: Rc::clone(&value),
        });
        value
    }

    /// The recent namespaces for the current context, newest first. Borrowed:
    /// every caller only reads them.
    fn recent_namespaces_for_context(&self) -> impl Iterator<Item = &str> + Clone {
        self.recent_namespaces
            .get(&self.cluster.context)
            .into_iter()
            .flat_map(|dq| dq.iter().map(String::as_str))
    }

    /// The namespaces bound to the digit keys `1`-`9`, one entry per key
    /// (empty = that digit selects nothing). Configured favourites keep their
    /// configured slot; visited namespaces fill whatever is left in the order
    /// they were given a slot, so the digits still do something without a
    /// `favorite_namespaces` list.
    pub fn namespace_shortcuts(&self) -> Vec<String> {
        let mut slots: Vec<String> = self
            .namespace_favorites
            .iter()
            .take(Action::FAVORITE_NAMESPACES.len())
            .cloned()
            .collect();
        slots.resize(Action::FAVORITE_NAMESPACES.len(), String::new());
        if slots.iter().all(|s| !s.is_empty()) {
            return slots;
        }
        let assigned: Vec<&str> = self
            .recent_shortcuts
            .get(&self.cluster.context)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .filter(|a| !slots.iter().any(|s| s == a))
            .collect();
        let mut assigned = assigned.into_iter();
        for slot in slots.iter_mut().filter(|s| s.is_empty()) {
            match assigned.next() {
                Some(a) => *slot = a.to_string(),
                None => break,
            }
        }
        slots
    }

    /// Give `ns` a digit slot, leaving every slot already handed out where it
    /// is: the header advertises this mapping permanently, so picking one
    /// namespace must not move another one's digit. Once every slot is taken,
    /// the least recently used of them is handed over — one digit changes,
    /// the rest stay put.
    fn assign_namespace_shortcut(&mut self, ns: &str) {
        if self.is_favorite_namespace(ns) {
            return;
        }
        let assigned = self.recent_shortcuts.get(&self.cluster.context);
        if assigned.is_some_and(|list| list.iter().any(|a| a == ns)) {
            return;
        }
        let victim = assigned
            .filter(|list| list.len() >= Action::FAVORITE_NAMESPACES.len())
            .and_then(|list| {
                list.iter()
                    .enumerate()
                    // Absent from the recents entirely (it fell out of that
                    // shorter list) ranks as oldest.
                    .max_by_key(|(_, a)| {
                        self.recent_namespaces_for_context()
                            .position(|r| r == a.as_str())
                            .unwrap_or(usize::MAX)
                    })
                    .map(|(index, _)| index)
            });
        let list = self
            .recent_shortcuts
            .entry(self.cluster.context.clone())
            .or_default();
        match victim {
            Some(index) => list[index] = ns.to_string(),
            None => list.push(ns.to_string()),
        }
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
        // Before the deque moves: the slot handed over must be the one that
        // was least recently used at the time of this pick.
        self.assign_namespace_shortcut(ns);
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

    /// Persist the active namespace as the current context's last pick, so
    /// the next launch (and the next `:ctx` back here) restores it. Called
    /// after every explicit namespace choice — not after drill-downs,
    /// history, or bookmarks, which scope a view rather than pick a home.
    pub(super) fn remember_namespace(&mut self) {
        if !self
            .namespace_memory
            .set(&self.cluster.context, &self.namespace)
        {
            return;
        }
        if let Some(path) = self.namespace_memory_path.clone() {
            let result = match &self.state_writer {
                Some(writer) => writer.save_namespace(self.namespace_memory.clone(), path),
                None => self.namespace_memory.save(&path),
            };
            if let Err(e) = result {
                self.flash_warn(&format!("failed to save namespace state: {e}"));
            }
        }
    }

    /// Open the sort-column picker (k9s cycles with `S`; sofka jumps straight
    /// to a column instead, which scales to wide mode and custom views). The
    /// cursor starts on the active sort so enter-without-typing re-selects it,
    /// which toggles direction.
    pub(super) fn open_sort_picker(&mut self) {
        if self.display_headers().is_empty() {
            self.flash_warn("no columns to sort by");
            return;
        }
        self.sort_picker_filter.clear();
        // Entry 0 is the pinned default ordering; columns follow in display
        // order, so the active sort column sits at index + 1.
        self.sort_picker_state
            .select(Some(self.sort_column.map_or(0, |i| i + 1)));
        self.mode = Mode::SortPicker;
    }

    /// Entries for the sort picker: the default ordering is always pinned
    /// first; column headers are fuzzy-matched against the type-to-filter
    /// buffer (see `filtered_namespaces` for the same pattern).
    pub fn filtered_sort_entries(&self) -> Rc<Vec<String>> {
        let headers = self.display_headers();
        if let Some(m) = self.picker_memos.borrow().sort_entries.as_ref()
            && m.filter == self.sort_picker_filter
            && Rc::ptr_eq(&m.headers, &headers)
        {
            return Rc::clone(&m.value);
        }

        let mut out = vec![DEFAULT_SORT_LABEL.to_string()];
        if self.sort_picker_filter.is_empty() {
            out.extend(headers.iter().cloned());
            return self.remember_sort_entries(headers, out);
        }
        let mut scored: Vec<(i64, String)> = headers
            .iter()
            .filter_map(|h| {
                self.matcher
                    .score(h, &self.sort_picker_filter)
                    .map(|s| (s, h.clone()))
            })
            .collect();
        scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out.extend(scored.into_iter().map(|(_, h)| h));
        self.remember_sort_entries(headers, out)
    }

    fn remember_sort_entries(&self, headers: Rc<[String]>, out: Vec<String>) -> Rc<Vec<String>> {
        let value = Rc::new(out);
        self.picker_memos.borrow_mut().sort_entries = Some(SortEntryMemo {
            filter: self.sort_picker_filter.clone(),
            headers,
            value: Rc::clone(&value),
        });
        value
    }

    pub(super) fn key_sort_picker(&mut self, key: KeyInput) {
        if edit_action(key.action, &mut self.sort_picker_filter) {
            self.select_best_sort_match();
            return;
        }
        let len = self.filtered_sort_entries().len();
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                // First esc clears the filter, second closes the picker.
                if self.sort_picker_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.sort_picker_filter.clear();
                    self.select_best_sort_match();
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.sort_picker_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.sort_picker_state, len, false),
            (Some(Action::Accept), _) => {
                if let Some(entry) = self
                    .sort_picker_state
                    .selected()
                    .and_then(|i| self.filtered_sort_entries().get(i).cloned())
                {
                    self.apply_sort_choice(&entry);
                }
            }
            (Some(Action::Backspace), _) => {
                self.sort_picker_filter.pop();
                self.select_best_sort_match();
            }
            (None, KeyCode::Char(c)) => {
                self.sort_picker_filter.push(c);
                self.select_best_sort_match();
            }
            _ => {}
        }
    }

    /// Jump the sort-picker cursor to the best fuzzy match after the filter
    /// buffer changes (the pinned default at index 0 should only hold the
    /// cursor while browsing — see `select_best_namespace_match`).
    fn select_best_sort_match(&mut self) {
        let idx = if self.sort_picker_filter.is_empty() {
            self.sort_column.map_or(0, |i| i + 1)
        } else if self.filtered_sort_entries().len() > 1 {
            1 // right after the pinned default — the top-scored column
        } else {
            0
        };
        self.sort_picker_state.select(Some(idx));
    }

    pub(super) fn sort_by_age(&mut self) {
        if !self.display_headers().iter().any(|h| h == "AGE") {
            self.flash_warn("view has no AGE column");
            return;
        }
        self.apply_sort_choice("AGE");
    }

    /// Sort by a picked entry: the default entry clears the sort, a new column
    /// sorts ascending, and re-picking the active column toggles direction
    /// (the spreadsheet idiom).
    fn apply_sort_choice(&mut self, entry: &str) {
        self.mode = Mode::Table;
        self.sort_picker_filter.clear();
        if entry == DEFAULT_SORT_LABEL {
            self.reset_sort();
            self.remember_sort();
            self.invalidate_rows();
            self.flash = format!("sort by {DEFAULT_SORT_LABEL}");
            self.flash_err = false;
            return;
        }
        let Some(idx) = self.display_headers().iter().position(|h| h == entry) else {
            return;
        };
        self.sort_desc = self.sort_column == Some(idx) && !self.sort_desc;
        self.sort_column = Some(idx);
        self.invalidate_rows();
        self.remember_sort();
        self.flash = format!(
            "sort by {entry} {}",
            if self.sort_desc {
                "↓ desc"
            } else {
                "↑ asc"
            }
        );
        self.flash_err = false;
    }

    /// Open the copy-field picker (`Y`): every displayed column of the
    /// selected row with its full (untruncated) value, ⏎ copies the value to
    /// the clipboard. The fields are captured here so a watch update can't
    /// shift entries while the picker is open.
    pub(super) fn open_copy_picker(&mut self) {
        let fields = self.selected_row_fields();
        if fields.is_empty() {
            self.flash_warn("no row selected");
            return;
        }
        self.copy_picker_fields = fields;
        self.copy_picker_filter.clear();
        self.copy_picker_state.select(Some(0));
        self.mode = Mode::CopyPicker;
    }

    /// Entries for the copy picker: the captured `(header, value)` pairs,
    /// fuzzy-matched against both the header and the value (so typing part
    /// of an IP finds it as readily as typing the column name).
    pub fn filtered_copy_entries(&self) -> Rc<Vec<(String, String)>> {
        if let Some(m) = self.picker_memos.borrow().copy_entries.as_ref()
            && m.filter == self.copy_picker_filter
            && m.fields == self.copy_picker_fields
        {
            return Rc::clone(&m.value);
        }
        if self.copy_picker_filter.is_empty() {
            return self.remember_copy_entries(self.copy_picker_fields.clone());
        }
        let mut scored: Vec<(i64, (String, String))> = self
            .copy_picker_fields
            .iter()
            .filter_map(|(h, v)| {
                self.matcher
                    .score(&format!("{h} {v}"), &self.copy_picker_filter)
                    .map(|s| (s, (h.clone(), v.clone())))
            })
            .collect();
        scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.cmp(&b.1.0)));
        self.remember_copy_entries(scored.into_iter().map(|(_, e)| e).collect())
    }

    fn remember_copy_entries(&self, out: Vec<(String, String)>) -> Rc<Vec<(String, String)>> {
        let value = Rc::new(out);
        self.picker_memos.borrow_mut().copy_entries = Some(CopyEntryMemo {
            filter: self.copy_picker_filter.clone(),
            fields: self.copy_picker_fields.clone(),
            value: Rc::clone(&value),
        });
        value
    }

    pub(super) fn key_copy_picker(&mut self, key: KeyInput) {
        if edit_action(key.action, &mut self.copy_picker_filter) {
            self.select_best_copy_match();
            return;
        }
        let len = self.filtered_copy_entries().len();
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                // First esc clears the filter, second closes the picker.
                if self.copy_picker_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.copy_picker_filter.clear();
                    self.select_best_copy_match();
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.copy_picker_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.copy_picker_state, len, false),
            (Some(Action::Accept), _) => {
                if let Some((header, value)) = self
                    .copy_picker_state
                    .selected()
                    .and_then(|i| self.filtered_copy_entries().get(i).cloned())
                {
                    self.copy_field(&header, value);
                }
            }
            (Some(Action::Backspace), _) => {
                self.copy_picker_filter.pop();
                self.select_best_copy_match();
            }
            (None, KeyCode::Char(c)) => {
                self.copy_picker_filter.push(c);
                self.select_best_copy_match();
            }
            _ => {}
        }
    }

    /// Keep the cursor on the best fuzzy match while typing (see
    /// `select_best_sort_match` — same idiom, no pinned entry here).
    fn select_best_copy_match(&mut self) {
        self.copy_picker_state.select(Some(0));
    }

    /// Copy a picked field's value to the clipboard and close the picker.
    /// The flash echoes what was copied, truncated so a long value (a list
    /// of ports, a node selector) can't flood the one-line status bar.
    fn copy_field(&mut self, header: &str, value: String) {
        self.mode = Mode::Table;
        self.copy_picker_filter.clear();
        let mut shown: String = value.chars().take(60).collect();
        if shown.len() < value.len() {
            shown.push('…');
        }
        self.copy_to_clipboard_async(
            value,
            format!("copied {header}: {shown}"),
            "no clipboard target found (pbcopy/xclip/wl-copy/OSC 52)",
        );
    }

    pub(super) fn key_namespaces(&mut self, key: KeyInput) {
        if edit_action(key.action, &mut self.ns_filter) {
            self.select_best_namespace_match();
            return;
        }
        let len = self.filtered_namespaces().len();
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                // First esc clears the filter and jumps back to the top
                // (`<all>`); a second esc closes the switcher.
                if self.ns_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.ns_filter.clear();
                    self.ns_state.select(Some(0));
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.ns_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.ns_state, len, false),
            (Some(Action::Accept), _) => {
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
            (Some(Action::Backspace), _) => {
                self.ns_filter.pop();
                self.select_best_namespace_match();
            }
            (None, KeyCode::Char(c)) => {
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
        self.save_history_filter();
        self.namespace = normalize_ns(&sel);
        self.drop_owner_scope();
        self.note_recent_namespace(&sel);
        self.remember_namespace();
        self.set_flash(format!("namespace: {}", self.namespace_label()));
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
        self.ctx_filtering = false;
        self.ctx_list.clear();
        self.ctx_state.select(None);
        self.mode = Mode::Contexts;
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            match Cluster::list_contexts() {
                Ok(mut list) => {
                    list.sort();
                    let _ = tx
                        .send(Msg::Contexts {
                            generation: genr,
                            list,
                        })
                        .await;
                }
                // An unreadable kubeconfig must say so — an empty picker
                // over a parse error looks like "you have no contexts".
                Err(e) => {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: e,
                        })
                        .await;
                }
            }
        });
    }

    /// Contexts for the switcher, fuzzy-matched against the type-to-filter
    /// buffer (see `filtered_namespaces` for the same pattern).
    pub fn filtered_contexts(&self) -> Rc<Vec<String>> {
        if let Some(m) = self.picker_memos.borrow().contexts.as_ref()
            && m.filter == self.ctx_filter
            && m.ctx_list == self.ctx_list
        {
            return Rc::clone(&m.value);
        }
        if self.ctx_filter.is_empty() {
            return self.remember_contexts(self.ctx_list.clone());
        }
        let mut scored: Vec<(i64, &String)> = self
            .ctx_list
            .iter()
            .filter_map(|c| self.matcher.score(c, &self.ctx_filter).map(|s| (s, c)))
            .collect();
        scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        self.remember_contexts(scored.into_iter().map(|(_, c)| c.clone()).collect())
    }

    fn remember_contexts(&self, out: Vec<String>) -> Rc<Vec<String>> {
        let value = Rc::new(out);
        self.picker_memos.borrow_mut().contexts = Some(ContextMemo {
            filter: self.ctx_filter.clone(),
            ctx_list: self.ctx_list.clone(),
            value: Rc::clone(&value),
        });
        value
    }

    /// Contexts type-to-filter like the namespace picker. Existing action keys
    /// remain available while browsing; `/` explicitly starts filter input
    /// when a context name begins with one of those keys.
    pub(super) fn key_contexts(&mut self, key: KeyInput) {
        let len = self.filtered_contexts().len();
        if self.ctx_filtering {
            if edit_action(key.action, &mut self.ctx_filter) {
                self.select_best_context_match();
                return;
            }
            match (key.action, key.code) {
                (Some(Action::Back), _) => {
                    self.ctx_filter.clear();
                    self.ctx_filtering = false;
                    self.select_current_context();
                }
                (Some(Action::Accept), _) => self.switch_selected_context(),
                (Some(Action::Down), _) => list_step(&mut self.ctx_state, len, true),
                (Some(Action::Up), _) => list_step(&mut self.ctx_state, len, false),
                (Some(Action::Backspace), _) => {
                    self.ctx_filter.pop();
                    self.select_best_context_match();
                }
                (None, KeyCode::Char(c)) => {
                    self.ctx_filter.push(c);
                    self.select_best_context_match();
                }
                _ => {}
            }
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                if self.ctx_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.ctx_filter.clear();
                    self.select_current_context();
                }
            }
            (Some(Action::Filter), _) => self.ctx_filtering = true,
            (Some(Action::Rename), _) => self.open_rename_context(),
            // Space toggles the highlighted context in/out of the `:fleet`
            // dashboard for this session (the bulk-mark idiom).
            (Some(Action::FleetMark), _) => {
                if let Some(name) = self
                    .ctx_state
                    .selected()
                    .and_then(|i| self.filtered_contexts().get(i).cloned())
                {
                    self.toggle_fleet_context(&name);
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.ctx_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.ctx_state, len, false),
            (Some(Action::Accept), _) => self.switch_selected_context(),
            (None, KeyCode::Char(c)) => {
                self.ctx_filtering = true;
                self.ctx_filter.push(c);
                self.select_best_context_match();
            }
            _ => {}
        }
    }

    fn select_best_context_match(&mut self) {
        let selected = (!self.filtered_contexts().is_empty()).then_some(0);
        self.ctx_state.select(selected);
    }

    fn switch_selected_context(&mut self) {
        if let Some(name) = self
            .ctx_state
            .selected()
            .and_then(|i| self.filtered_contexts().get(i).cloned())
        {
            self.mode = Mode::Table;
            self.ctx_filter.clear();
            self.ctx_filtering = false;
            self.switch_context(name);
        }
    }

    /// Put the switcher cursor on the active context (fallback: the top).
    fn select_current_context(&mut self) {
        let idx = self
            .filtered_contexts()
            .iter()
            .position(|c| *c == self.cluster.context)
            .unwrap_or(0);
        self.ctx_state.select(Some(idx));
    }

    /// Prompt for a new name for the selected context (`r` in the switcher),
    /// prefilled with the current name.
    fn open_rename_context(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let Some(old) = self
            .ctx_state
            .selected()
            .and_then(|i| self.filtered_contexts().get(i).cloned())
        else {
            return;
        };
        self.prompt_label = format!("Rename context {old} to:");
        self.prompt_input = old.clone();
        self.prompt_kind = Some(PromptKind::RenameContext { old });
        self.mode = Mode::Prompt;
    }

    /// Rename a kubeconfig context off-thread via `kubectl config
    /// rename-context` (which also updates `current-context` when it pointed
    /// at the old name); the outcome arrives as `Msg::ContextRenamed`.
    pub(super) fn rename_context(&mut self, old: String, new: String) {
        if new == old {
            return;
        }
        if self.ctx_list.contains(&new) {
            self.flash_warn(&format!("context '{new}' already exists"));
            return;
        }
        let claim = self.claim_status(format!("renaming {old} → {new}…"));
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let out = tokio::process::Command::new("kubectl")
                .args(["config", "rename-context", &old, &new])
                .output()
                .await;
            let result = match out {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Err(e) => Err(format!("kubectl failed to start: {e}")),
            };
            let _ = tx
                .send(Msg::ContextRenamed {
                    generation: genr,
                    claim,
                    old,
                    new,
                    result,
                })
                .await;
        });
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
        // Re-selecting the target of the live connection is also a no-op. A
        // bookmark, workspace, or query may be waiting for it to land, and a
        // replacement connection would otherwise invalidate that destination.
        if self
            .context_switch_target
            .as_ref()
            .is_some_and(|(generation, target)| *generation == self.generation && target == &name)
        {
            return;
        }
        // One deferred navigation at a time. Whatever asked for this switch
        // owns what lands when it completes, so anything armed by an earlier
        // switch is dropped here rather than left to fire on a later one.
        // Below both no-op returns, deliberately: a re-select that starts no
        // switch must not disarm one already in flight. Callers that arm a new
        // deferred action clear its competing slots after this returns.
        self.pending_resource_query = None;
        self.pending_bookmark = None;
        self.pending_workspace = None;
        // Stop the current context's watches and clear stale rows while we
        // reconnect; the new watch starts when the connection lands. The rows
        // are stashed first — if the switch fails we stay on this context,
        // where they're still valid (a successful switch drops the cache).
        // Bump first: this switch's own progress flash belongs to the new
        // generation, and the bump clears any left over from the old one.
        // The browser (and any helper pod it created) belongs to the context
        // being left: nothing in the new one can serve it.
        self.leave_pvc_explore();
        self.bump_generation();
        self.context_switch_target = Some((self.generation, name.clone()));
        self.set_flash(format!("switching to {name}…"));
        self.stash_view_snapshot();
        self.store.clear();
        self.invalidate_rows();
        let tx = self.tx.clone();
        let genr = self.generation;
        let allow_v1_client_cert = self.cluster.allow_v1_client_cert;
        tokio::spawn(async move {
            let result = Cluster::connect_context(&name, allow_v1_client_cert)
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
        self.stop_notifications();
        let resolved = self.config.resolve(&name, &cluster.cluster_name);
        self.user_aliases = resolved.config.aliases;
        self.namespace_favorites = resolved.config.favorite_namespaces;
        self.remember_sort = resolved.config.remember_sort.unwrap_or(true);
        self.hide_header = resolved.config.hide_header;
        self.terminal_title = resolved.config.terminal_title.unwrap_or(true);
        self.plugins = resolved.config.plugins;
        self.bookmarks = resolved.config.bookmarks;
        self.workspaces = resolved.config.workspaces;
        self.guardrails = resolved.config.guardrails;
        self.debug = resolved.config.debug;
        self.bundle_cfg = resolved.config.bundle;
        self.pvc_cfg = resolved.config.pvc_explore;
        self.logs_cfg = resolved.config.logs;
        self.fleet_cfg = resolved.config.fleet;
        // Tracked debuggers belong to the previous cluster/context.
        self.launched_node_debuggers.clear();
        let mut plugin_warnings = crate::config::plugin_warnings(&self.plugins);
        plugin_warnings.extend(crate::config::bookmark_warnings(&self.bookmarks));
        plugin_warnings.extend(crate::config::workspace_warnings(&self.workspaces));
        plugin_warnings.extend(crate::config::guardrail_warnings(&self.guardrails));
        plugin_warnings.extend(crate::config::pvc_explore_warnings(&self.pvc_cfg));
        let (views, view_warnings) = crate::views::compile(&resolved.config.views);
        self.user_views = views;
        let (thresholds, threshold_warnings) =
            crate::thresholds::compile(&resolved.config.thresholds);
        self.thresholds = thresholds;
        let (log_provider, provider_warnings) =
            crate::providers::compile(resolved.config.providers.logs.as_ref());
        self.log_provider = log_provider;
        let (metrics_provider, _mw) =
            crate::providers::compile_metrics(resolved.config.providers.metrics.as_ref());
        self.metrics_provider = metrics_provider;
        // Printer-column fallbacks came from the old cluster's CRDs.
        self.crd_views.clear();
        // Cached view snapshots hold the old cluster's resources.
        self.clear_view_cache();
        // The timeline recorded the old cluster's objects.
        self.timeline.clear();
        self.skin_colors = resolved.config.skin.colors;
        self.readonly = self.readonly_override.unwrap_or(resolved.config.readonly);
        cluster.add_aliases(&self.user_aliases);
        self.bump_generation();
        // Where you last were in this context beats its config default.
        self.namespace = self
            .namespace_memory
            .get(&cluster.context)
            .or(resolved.config.default_namespace)
            .unwrap_or_else(|| cluster.default_namespace.clone());
        self.cluster = *cluster;
        plugin_warnings.extend(self.configure_keys(&resolved.config.keys));
        self.stack.clear();
        // View history references the old cluster's kinds and namespaces.
        self.history.clear();
        self.history_pos = 0;
        self.kind = None;
        self.kind_plural.clear();
        self.labels = None;
        self.fields = None;
        self.owner = None;
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
        let first_warning = resolved
            .warnings
            .first()
            .or(view_warnings.first())
            .or(plugin_warnings.first())
            .or(threshold_warnings.first())
            .or(provider_warnings.first())
            .cloned();
        // Keep `:config` in sync with the layers just resolved for this context.
        self.config_warnings = resolved.warnings;
        self.config_warnings.extend(plugin_warnings);
        self.config_warnings.extend(threshold_warnings);
        // A bookmark/workspace that requested this context lands on its own
        // view(s); a plain switch lands on the context's default resource.
        if let Some(mut query) = self.pending_resource_query.take() {
            query.context = None;
            self.apply_resource_query(query);
        } else if self.pending_workspace.is_some() {
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
        if let Some(w) = &first_warning {
            self.flash_warn(w);
        }
        self.flash_discovery_warnings();
        // Saved forwards for the new context. Running ones from the previous
        // context are deliberately left alone (kubectl pinned their context
        // at spawn); autostart only adds what's missing here.
        self.forwards_cfg = resolved.config.forwards;
        self.notify_cfg = resolved.config.notify;
        self.start_autostart_forwards();
    }
}
