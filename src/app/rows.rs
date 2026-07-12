use super::*;

impl App {
    /// Mark the cached row order/filter stale. Cheap; safe to over-call.
    pub(super) fn invalidate_rows(&self) {
        self.rows_cache.borrow_mut().dirty = true;
    }

    pub(super) fn clear_rows_cache(&self) {
        let mut cache = self.rows_cache.borrow_mut();
        cache.dirty = true;
        cache.keys.clear();
        cache.cells.clear();
    }

    pub(super) fn invalidate_row(&self, key: &str) {
        let mut cache = self.rows_cache.borrow_mut();
        cache.dirty = true;
        cache.cells.remove(key);
    }

    /// The parsed form of the active filter, reparsed only when the string
    /// has changed (never per frame — see [`FilterCache`]).
    pub(super) fn parsed_filter(&self) -> Ref<'_, crate::filter::ParsedFilter> {
        if self.filter_cache.borrow().raw != self.filter {
            let mut cache = self.filter_cache.borrow_mut();
            cache.raw = self.filter.clone();
            cache.parsed = crate::filter::parse(&self.filter);
        }
        Ref::map(self.filter_cache.borrow(), |c| &c.parsed)
    }

    /// Does this object pass the current filter — the legacy fuzzy pattern,
    /// or every local term of a structured expression? `-l`/`-f` selectors
    /// are not evaluated here: the Kubernetes API already applied them to
    /// the watch (see [`Self::sync_filter_selectors`]).
    pub(super) fn matches_filter(&self, o: &DynamicObject) -> bool {
        use crate::filter::{ParsedFilter, Term};
        if self.filter.is_empty() {
            return true;
        }
        let parsed = self.parsed_filter();
        match &*parsed {
            ParsedFilter::Fuzzy(pat) => {
                pat.is_empty() || self.matcher.fuzzy_match(&self.fuzzy_hay(o), pat).is_some()
            }
            ParsedFilter::Structured(s) => s.terms.iter().all(|t| match t {
                Term::Fuzzy(pat) => self.matcher.fuzzy_match(&self.fuzzy_hay(o), pat).is_some(),
                Term::NotFuzzy(pat) => self.matcher.fuzzy_match(&self.fuzzy_hay(o), pat).is_none(),
                Term::Cmp(cmp) => self.eval_cmp(o, cmp),
            }),
        }
    }

    /// What fuzzy terms match against: "namespace name". Helm rows are backed
    /// by the storage Secret, whose own name (`sh.helm.release.v1.<release>.
    /// v<n>`) isn't what a user typing a filter means — match the release
    /// name instead.
    fn fuzzy_hay(&self, o: &DynamicObject) -> String {
        let name = if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            crate::helm::release_name(o).unwrap_or_default()
        } else {
            o.metadata.name.as_deref().unwrap_or("")
        };
        format!("{} {}", o.metadata.namespace.as_deref().unwrap_or(""), name)
    }

    /// Evaluate one typed column comparison against an object. `cpu`/`mem`
    /// read the live metrics snapshot, `age` the creation timestamp; any
    /// other key names a displayed column (numeric values compare by the
    /// cell's leading number, text case-insensitively).
    fn eval_cmp(&self, o: &DynamicObject, cmp: &crate::filter::Cmp) -> bool {
        use crate::filter::CmpValue;
        match &cmp.value {
            CmpValue::Cpu(want) => cmp.op.eval(self.metrics_for(o).0.cmp(want)),
            CmpValue::Mem(want) => cmp.op.eval(self.metrics_for(o).1.cmp(want)),
            CmpValue::Duration(want) => match crate::columns::age_secs(o) {
                Some(age) => cmp.op.eval(age.cmp(want)),
                None => false,
            },
            CmpValue::Num(want) => match self.column_cell(o, &cmp.key) {
                Some(cell) => cmp
                    .op
                    .eval(crate::columns::parse_leading_num(&cell).total_cmp(want)),
                None => false,
            },
            CmpValue::Str(want) => match self.column_cell(o, &cmp.key) {
                Some(cell) => cmp.op.eval(cell.to_lowercase().cmp(&want.to_lowercase())),
                None => false,
            },
        }
    }

    /// The displayed cell a comparison key names (case-insensitive column
    /// header), plus NAMESPACE and a `/status/phase` fallback for kinds
    /// without a STATUS column.
    fn column_cell(&self, o: &DynamicObject, key: &str) -> Option<String> {
        if key.eq_ignore_ascii_case("namespace") || key.eq_ignore_ascii_case("ns") {
            return Some(o.metadata.namespace.clone().unwrap_or_default());
        }
        let base = self.spec.headers();
        if let Some(i) = base.iter().position(|h| h.eq_ignore_ascii_case(key)) {
            let (cells, _) = self.spec.cells(o);
            return cells.get(i).cloned();
        }
        if key.eq_ignore_ascii_case("status") {
            let phase = phase(o);
            return (!phase.is_empty()).then_some(phase);
        }
        None
    }

    /// Whether the running watch is scoped by `-l`/`-f` selectors from the
    /// filter — i.e. the active filter is (partly) server-side.
    pub fn filter_server_side(&self) -> bool {
        self.applied_filter_labels.is_some() || self.applied_filter_fields.is_some()
    }

    /// Parse error of the current filter input, if any.
    pub fn filter_error(&self) -> Option<String> {
        self.parsed_filter().error().map(str::to_string)
    }

    /// True when the filter's `-l`/`-f` selectors differ from what the watch
    /// was started with — ⏎ in the filter prompt applies them server-side.
    pub fn filter_selectors_pending(&self) -> bool {
        let parsed = self.parsed_filter();
        parsed.labels() != self.applied_filter_labels.as_deref()
            || parsed.fields() != self.applied_filter_fields.as_deref()
    }

    /// Char indices in `name` that matched the active row filter's fuzzy
    /// pattern, for highlighting them in the table. `None` when there's no
    /// active filter or no fuzzy term (every visible row already passed
    /// [`matches_filter`], so this is purely a rendering aid, not a second
    /// filter decision).
    pub fn filter_match_indices(&self, name: &str) -> Option<Vec<usize>> {
        if self.filter.is_empty() {
            return None;
        }
        let parsed = self.parsed_filter();
        let needle = parsed.fuzzy_needle()?;
        self.matcher.fuzzy_indices(name, needle).map(|(_, idx)| idx)
    }

    pub(super) fn ensure_rows_cache(&self) {
        let mut cache = self.rows_cache.borrow_mut();
        if !cache.dirty {
            return;
        }

        let headers = self.display_headers();
        let sort_header = self
            .sort_column
            .and_then(|i| headers.get(i).map(String::as_str));
        // The aggregated Helm release list (`helm list` semantics) shows only
        // the latest revision per release; `helmhistory` (one release's full
        // history) shows every revision, so it skips this.
        let helm_latest = (self.kind_plural == "helm").then(|| self.helm_latest_revision_keys());
        // (primary sort key, (ns, name) tiebreak, store key)
        let mut entries: Vec<(SortKey, (String, String), String)> = self
            .store
            .iter()
            .filter(|(_, o)| self.matches_filter(o))
            .filter(|(k, _)| match &helm_latest {
                Some(keep) => keep.contains(*k),
                None => true,
            })
            .map(|(k, o)| {
                let primary = match sort_header {
                    Some(h) => self.column_sort_key(o, h),
                    None => SortKey::Text(String::new()),
                };
                let tie = (
                    o.metadata.namespace.clone().unwrap_or_default(),
                    o.metadata.name.clone().unwrap_or_default(),
                );
                (primary, tie, k.clone())
            })
            .collect();
        let desc = self.sort_desc && sort_header.is_some();
        entries.sort_by(|a, b| {
            let mut ord = a.0.cmp_to(&b.0);
            if desc {
                ord = ord.reverse();
            }
            // Ties always fall back to namespace/name ascending.
            ord.then_with(|| a.1.cmp(&b.1))
        });
        cache.keys = entries.into_iter().map(|(_, _, k)| k).collect();
        cache.dirty = false;
    }

    /// Store keys of the highest-revision secret per (namespace, release) —
    /// label-based (no gunzip/decode needed), used to dedup the aggregated
    /// Helm release list down to one row per release, like `helm list`.
    fn helm_latest_revision_keys(&self) -> HashSet<String> {
        let mut latest: HashMap<(String, String), (i64, String)> = HashMap::new();
        for (k, o) in self.store.iter() {
            let Some(name) = crate::helm::release_name(o) else {
                continue;
            };
            let ns = o.metadata.namespace.clone().unwrap_or_default();
            let ver = crate::helm::revision(o).unwrap_or(0);
            let key = (ns, name.to_string());
            let better = latest.get(&key).is_none_or(|(v, _)| ver > *v);
            if better {
                latest.insert(key, (ver, k.clone()));
            }
        }
        latest.into_values().map(|(_, k)| k).collect()
    }

    /// Display-ordered, filtered row count, backed by the same cache as
    /// [`rows`]. Use this when only the count is needed so a frame doesn't
    /// rebuild a temporary `Vec<&DynamicObject>` just to call `len()`.
    pub fn row_count(&self) -> usize {
        self.ensure_rows_cache();
        self.rows_cache.borrow().keys.len()
    }

    /// Display-ordered, filtered rows. Backed by a cache that only recomputes
    /// the sort + fuzzy filter when the store, filter, or sort changes.
    pub fn rows(&self) -> Vec<&DynamicObject> {
        self.ensure_rows_cache();
        self.rows_cache
            .borrow()
            .keys
            .iter()
            .filter_map(|k| self.store.get(k))
            .collect()
    }

    pub(crate) fn ensure_table_cell_cache(&self, rows: &[&DynamicObject]) {
        let mut cache = self.rows_cache.borrow_mut();
        for obj in rows {
            let key = row_key(obj);
            let resource_version = obj.metadata.resource_version.clone();
            let stale = cache.cells.get(&key).is_none_or(|entry| {
                entry.plural != self.kind_plural || entry.resource_version != resource_version
            });
            if stale {
                let (cells, status_idx) = self.spec.cells(obj);
                cache.cells.insert(
                    key,
                    CellCacheEntry {
                        plural: self.kind_plural.clone(),
                        resource_version,
                        cells,
                        status_idx,
                    },
                );
            }
        }
    }

    pub(crate) fn table_cell_cache(&self) -> TableCellCache<'_> {
        TableCellCache {
            cache: self.rows_cache.borrow(),
        }
    }

    /// The headers as displayed: the active view spec's columns, with
    /// NAMESPACE prepended when listing across namespaces and CPU/MEM appended
    /// for pods/nodes. Kept in one place so sorting and rendering agree on the
    /// column layout.
    pub fn display_headers(&self) -> Vec<String> {
        let mut h = self.spec.headers();
        if self.show_namespace_column() {
            h.insert(0, "NAMESPACE".into());
        }
        if self.metrics_columns() {
            h.push("CPU".into());
            h.push("MEM".into());
        }
        h
    }

    pub(crate) fn view_spec(&self) -> &crate::columns::ViewSpec {
        &self.spec
    }

    /// Rebuild the active column layout from the current kind, user views,
    /// printer-column fallback, and wide mode. An active sort stays pinned to
    /// its column *header* — indices shift when columns appear/disappear (wide
    /// toggle, printer columns arriving) — and resets if the column is gone.
    /// Cached cells are laid out for the old spec, so they're always dropped.
    pub(super) fn refresh_view_spec(&mut self) {
        let sort_header = self
            .sort_column
            .and_then(|i| self.display_headers().get(i).cloned());
        let spec = crate::columns::build_spec(
            &self.kind_plural,
            self.active_user_view(),
            self.crd_views
                .get(&self.kind_plural)
                .and_then(Option::as_ref),
            self.wide,
        );
        self.spec = spec;
        if let Some(h) = sort_header {
            self.sort_column = self.display_headers().iter().position(|x| *x == h);
            if self.sort_column.is_none() {
                self.sort_desc = false;
            }
        }
        self.clear_rows_cache();
    }

    /// The user-configured view matching the current kind, if any. Synthetic
    /// views (helm/helmhistory) are backed by an unrelated kind (`secrets`),
    /// so they never match.
    pub(super) fn active_user_view(&self) -> Option<&crate::views::View> {
        let kind = self.kind.as_ref()?;
        if kind.ar.plural.to_lowercase() != self.kind_plural {
            return None;
        }
        crate::views::lookup(&self.user_views, &kind.ar)
    }

    /// Apply a view's configured initial sort, unless a sort is already
    /// active (a refresh must not clobber the user's choice).
    pub(super) fn apply_view_sort(&mut self) {
        if self.sort_column.is_some() {
            return;
        }
        let Some((header, desc)) = self.active_user_view().and_then(|v| v.sort.clone()) else {
            return;
        };
        match self.display_headers().iter().position(|h| *h == header) {
            Some(i) => {
                self.sort_column = Some(i);
                self.sort_desc = desc;
                self.invalidate_rows();
            }
            None => self.flash_warn(&format!("view sort column '{header}' not found")),
        }
    }

    /// Toggle wide mode (`w`): show/hide wide-only columns.
    pub(super) fn toggle_wide(&mut self) {
        self.wide = !self.wide;
        self.refresh_view_spec();
        self.flash = format!("wide columns: {}", if self.wide { "on" } else { "off" });
        self.flash_err = false;
    }

    pub fn show_namespace_column(&self) -> bool {
        self.kind
            .as_ref()
            .map(|k| k.namespaced && self.all_namespaces())
            .unwrap_or(false)
    }

    pub fn metrics_columns(&self) -> bool {
        matches!(self.kind_plural.as_str(), "pods" | "nodes")
    }

    /// Latest (cpu_millicores, mem_bytes) for an object from the metrics map.
    pub(super) fn metrics_for(&self, o: &DynamicObject) -> (i64, i64) {
        let name = o.metadata.name.clone().unwrap_or_default();
        let key = if self.kind_plural == "pods" {
            format!("{}/{}", o.metadata.namespace.as_deref().unwrap_or(""), name)
        } else {
            name
        };
        self.metrics.get(&key).copied().unwrap_or((0, 0))
    }

    /// Comparable value of `header`'s cell for object `o`.
    pub(super) fn column_sort_key(&self, o: &DynamicObject, header: &str) -> SortKey {
        // User/printer columns sort by their declared type (quantity, number,
        // time…), and win over the curated special cases so an overlay that
        // redefines a header sorts by its own values.
        if self.spec.is_user_column(header)
            && let Some(v) = self.spec.sort_value(o, header)
        {
            return SortKey::from(v);
        }
        match header {
            "NAMESPACE" => SortKey::Text(
                o.metadata
                    .namespace
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase(),
            ),
            // Unknown timestamps sort last (oldest-unknown) in ascending order.
            "AGE" => SortKey::Num(crate::columns::age_secs(o).unwrap_or(i64::MAX) as f64),
            "CPU" => SortKey::Num(self.metrics_for(o).0 as f64),
            "MEM" => SortKey::Num(self.metrics_for(o).1 as f64),
            // Humanized time cells ("5d23h") must sort by the underlying
            // timestamp, never the rendered string. Negated epoch seconds so
            // ascending = most recent first, matching AGE; unknowns last.
            "UPDATED" => SortKey::Num(
                crate::helm::decode(o)
                    .and_then(|r| r.last_deployed_secs)
                    .map(|s| -(s as f64))
                    .unwrap_or(f64::INFINITY),
            ),
            "LAST-SCHEDULE" => SortKey::Num(
                crate::columns::last_schedule_secs(o)
                    .map(|s| -(s as f64))
                    .unwrap_or(f64::INFINITY),
            ),
            "DURATION" if self.kind_plural == "jobs" => {
                SortKey::Num(crate::columns::job_duration_secs(o).unwrap_or(i64::MAX) as f64)
            }
            // Helm revisions are plain integers; flux REVISION cells (shas,
            // `main@sha1:…`) stay text.
            "REVISION" if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") => {
                SortKey::Num(crate::helm::revision(o).unwrap_or(0) as f64)
            }
            _ => match self.spec.sort_value(o, header) {
                Some(v) => SortKey::from(v),
                None => SortKey::Text(String::new()),
            },
        }
    }

    pub(super) fn reset_sort(&mut self) {
        self.sort_column = None;
        self.sort_desc = false;
    }

    /// Cycle the sort column: none → first → … → last → none (k9s `S`).
    pub(super) fn cycle_sort(&mut self) {
        let n = self.display_headers().len();
        if n == 0 {
            return;
        }
        self.sort_column = match self.sort_column {
            None => Some(0),
            Some(i) if i + 1 < n => Some(i + 1),
            Some(_) => None,
        };
        self.sort_desc = false;
        self.invalidate_rows();
        let label = match self.sort_column {
            Some(i) => self.display_headers().get(i).cloned().unwrap_or_default(),
            None => "default (ns/name)".to_string(),
        };
        self.flash = format!("sort by {label}");
        self.flash_err = false;
    }

    /// Toggle ascending/descending for the active sort column (k9s `I`).
    pub(super) fn toggle_sort_dir(&mut self) {
        let Some(i) = self.sort_column else {
            self.flash_warn("press S to pick a sort column first");
            return;
        };
        self.sort_desc = !self.sort_desc;
        self.invalidate_rows();
        let label = self.display_headers().get(i).cloned().unwrap_or_default();
        self.flash = format!(
            "sort by {label} {}",
            if self.sort_desc {
                "↓ desc"
            } else {
                "↑ asc"
            }
        );
        self.flash_err = false;
    }

    pub fn selected_ref(&self) -> Option<&DynamicObject> {
        let rows = self.rows();
        let idx = self.table_state.selected()?;
        rows.get(idx).copied()
    }

    pub fn selected(&self) -> Option<DynamicObject> {
        self.selected_ref().cloned()
    }

    pub fn confirm_allows_force_toggle(&self) -> bool {
        matches!(self.confirm_action, Some(ConfirmAction::Delete { .. }))
    }

    /// Toggle the mark on the current row (SPACE).
    pub(super) fn toggle_mark(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let key = row_key(obj);
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
    }

    /// `(name, ns)` for every row a bulk action applies to: the marked set
    /// (resolved against the current rows, so stale/hidden keys are dropped) if
    /// any are marked, otherwise the single current selection.
    pub(super) fn action_targets(&self) -> Vec<(String, String)> {
        let to_pair = |o: &DynamicObject| {
            (
                o.metadata.name.clone().unwrap_or_default(),
                o.metadata.namespace.clone().unwrap_or_default(),
            )
        };
        if self.marked.is_empty() {
            return self.selected_ref().map(to_pair).into_iter().collect();
        }
        self.rows()
            .iter()
            .filter(|o| self.marked.contains(&row_key(o)))
            .map(|o| to_pair(o))
            .collect()
    }

    /// Same as [`Self::action_targets`], but resolves each Helm storage
    /// `Secret` to its release name via label instead of the raw
    /// `sh.helm.release.v1.<release>.v<n>` secret name — `helm`/
    /// `helmhistory` rows only.
    pub(super) fn helm_action_targets(&self) -> Vec<(String, String)> {
        let to_pair = |o: &DynamicObject| {
            (
                crate::helm::release_name(o).unwrap_or_default().to_string(),
                o.metadata.namespace.clone().unwrap_or_default(),
            )
        };
        if self.marked.is_empty() {
            return self.selected_ref().map(to_pair).into_iter().collect();
        }
        self.rows()
            .iter()
            .filter(|o| self.marked.contains(&row_key(o)))
            .map(|o| to_pair(o))
            .collect()
    }

    pub(super) fn node_action_targets(&self) -> Vec<String> {
        self.action_targets()
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| !name.is_empty())
            .collect()
    }

    pub(super) fn move_selection(&mut self, delta: i32) {
        let len = self.rows().len() as i32;
        if len == 0 {
            return;
        }
        // No current selection means "before the first row", not "already on
        // it" — otherwise pressing Down from an unselected state lands on row
        // 1, skipping row 0 entirely.
        let cur = self.table_state.selected().map(|c| c as i32).unwrap_or(-1);
        let next = (cur + delta).clamp(0, len - 1);
        self.table_state.select(Some(next as usize));
    }
}
