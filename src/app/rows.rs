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

    /// Does this object pass the current fuzzy filter?
    pub(super) fn matches_filter(&self, o: &DynamicObject) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {}",
            o.metadata.namespace.as_deref().unwrap_or(""),
            o.metadata.name.as_deref().unwrap_or("")
        );
        self.matcher.fuzzy_match(&hay, &self.filter).is_some()
    }

    /// Char indices in `name` that matched the active row filter, for
    /// highlighting them in the table. `None` when there's no active filter
    /// (every visible row already passed [`matches_filter`], so this is
    /// purely a rendering aid, not a second filter decision).
    pub fn filter_match_indices(&self, name: &str) -> Option<Vec<usize>> {
        if self.filter.is_empty() {
            return None;
        }
        self.matcher
            .fuzzy_indices(name, &self.filter)
            .map(|(_, idx)| idx)
    }

    pub(super) fn ensure_rows_cache(&self) {
        let mut cache = self.rows_cache.borrow_mut();
        if !cache.dirty {
            return;
        }

        let headers = self.display_headers();
        let sort_header = self.sort_column.and_then(|i| headers.get(i).copied());
        // (primary sort key, (ns, name) tiebreak, store key)
        let mut entries: Vec<(SortKey, (String, String), String)> = self
            .store
            .iter()
            .filter(|(_, o)| self.matches_filter(o))
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
                let (cells, status_idx) = crate::columns::cells(obj, &self.kind_plural);
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

    /// The headers as displayed: kind columns, with NAMESPACE prepended when
    /// listing across namespaces and CPU/MEM appended for pods/nodes. Kept in
    /// one place so sorting and rendering agree on the column layout.
    pub fn display_headers(&self) -> Vec<&'static str> {
        let mut h = crate::columns::headers(&self.kind_plural);
        if self.show_namespace_column() {
            h.insert(0, "NAMESPACE");
        }
        if self.metrics_columns() {
            h.push("CPU");
            h.push("MEM");
        }
        h
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
            _ => {
                let base = crate::columns::headers(&self.kind_plural);
                match base.iter().position(|h| *h == header) {
                    Some(i) => {
                        let (cells, _) = crate::columns::cells(o, &self.kind_plural);
                        let v = cells.get(i).cloned().unwrap_or_default();
                        if is_numeric_header(header) {
                            SortKey::Num(parse_leading_num(&v))
                        } else {
                            SortKey::Text(v.to_lowercase())
                        }
                    }
                    None => SortKey::Text(String::new()),
                }
            }
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
            Some(i) => self
                .display_headers()
                .get(i)
                .copied()
                .unwrap_or("")
                .to_string(),
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
        let label = self
            .display_headers()
            .get(i)
            .copied()
            .unwrap_or("")
            .to_string();
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
        matches!(
            self.confirm_action,
            Some(ConfirmAction::Delete {
                targets: _,
                force: _
            })
        )
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
