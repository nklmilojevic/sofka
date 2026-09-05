use super::*;

/// The active filter with everything object-independent already resolved.
/// Built once per rebuild by [`App::compile_filter_plan`]; borrows the parsed
/// filter it was compiled from.
enum FilterPlan<'f> {
    Fuzzy { pat: &'f str, mask: u64 },
    Terms(Vec<TermPlan<'f>>),
}

enum TermPlan<'f> {
    Fuzzy {
        pat: &'f str,
        mask: u64,
    },
    NotFuzzy {
        pat: &'f str,
        mask: u64,
    },
    Cmp {
        cmp: &'f crate::filter::Cmp,
        column: CmpColumn,
    },
}

/// Where one comparison term reads its value from.
enum CmpColumn {
    Namespace,
    /// A displayed column whose cell is stable for an object revision — read
    /// from the shared cell cache.
    Cached(usize),
    /// A displayed column that re-renders with wall time (AGE and friends) —
    /// extracted per evaluation.
    Live(usize),
    /// `/status/phase` fallback for kinds without a STATUS column.
    Phase,
    /// The key names no column (the term can never match).
    Missing,
    /// The term compares metrics or age, not a column.
    Unused,
}

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
        cache.sort_keys.clear();
        cache.helm_latest = None;
    }

    pub(super) fn invalidate_row(&self, key: &str) {
        let mut cache = self.rows_cache.borrow_mut();
        cache.dirty = true;
        cache.cells.remove(key);
        cache.sort_keys.remove(key);
    }

    /// Drop derived data for an updated row. Its position and membership are
    /// unchanged when neither filtering nor sorting is active, so keep the
    /// already-built key order in that common watch-event path.
    pub(super) fn invalidate_row_contents(&self, key: &str) {
        let mut cache = self.rows_cache.borrow_mut();
        cache.cells.remove(key);
        cache.sort_keys.remove(key);
        if !self.filter.is_empty()
            || self.sort_column.is_some()
            || self.owner.is_some()
            || self.kind_plural == "helm"
        {
            cache.dirty = true;
        }
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
    ///
    /// Takes the cell cache and the parsed filter from its caller
    /// (`ensure_rows_cache`), which already holds both and evaluates this for
    /// every object in the store.
    fn matches_filter_cached(
        &self,
        o: &DynamicObject,
        key: &RowKey,
        plan: &FilterPlan<'_>,
        cells: &mut HashMap<RowKey, CellCacheEntry>,
    ) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        self.eval_filter(o, key, plan, cells)
    }

    /// Resolve everything about the active filter that does not depend on the
    /// object: each fuzzy pattern's character mask, and which column each
    /// comparison reads. Both used to be recomputed per object — that is, per
    /// term per row of the whole store, on every rebuild.
    ///
    /// Built per rebuild rather than cached alongside the parsed filter, so it
    /// cannot go stale against a spec that changed underneath it (wide toggle,
    /// printer columns arriving, a view switch).
    fn compile_filter_plan<'f>(&self, parsed: &'f crate::filter::ParsedFilter) -> FilterPlan<'f> {
        use crate::filter::{ParsedFilter, Term};
        let fuzzy = |pat: &'f str| (pat, subseq_mask(pat));
        match parsed {
            ParsedFilter::Fuzzy(pat) => {
                let (pat, mask) = fuzzy(pat);
                FilterPlan::Fuzzy { pat, mask }
            }
            ParsedFilter::Structured(s) => FilterPlan::Terms(
                s.terms
                    .iter()
                    .map(|t| match t {
                        Term::Fuzzy(p) => {
                            let (pat, mask) = fuzzy(p);
                            TermPlan::Fuzzy { pat, mask }
                        }
                        Term::NotFuzzy(p) => {
                            let (pat, mask) = fuzzy(p);
                            TermPlan::NotFuzzy { pat, mask }
                        }
                        Term::Cmp(cmp) => TermPlan::Cmp {
                            cmp,
                            column: self.resolve_cmp_column(cmp),
                        },
                    })
                    .collect(),
            ),
        }
    }

    /// Which column a comparison term reads, resolved once for the rebuild.
    /// Mirrors the lookup order the per-object path used: NAMESPACE/NS first,
    /// then a displayed column by case-insensitive header, then the
    /// `/status/phase` fallback for kinds without a STATUS column.
    fn resolve_cmp_column(&self, cmp: &crate::filter::Cmp) -> CmpColumn {
        use crate::filter::CmpValue;
        // cpu/mem/age never read a column — they compare live metrics and the
        // creation timestamp — so they must not resolve to a same-named one.
        if !matches!(cmp.value, CmpValue::Num(_) | CmpValue::Str(_)) {
            return CmpColumn::Unused;
        }
        let key = cmp.key.as_str();
        if key.eq_ignore_ascii_case("namespace") || key.eq_ignore_ascii_case("ns") {
            return CmpColumn::Namespace;
        }
        if let Some(i) = self.spec.header_index(key) {
            // Elapsed-time cells re-render for the same object revision, so
            // they are read live; every other column is exactly the string the
            // cell cache already holds for this revision (`cells()[i]` and
            // `cell_at(o, i)` run the same extractor).
            return if self.spec.volatile_column(&self.kind_plural, i) {
                CmpColumn::Live(i)
            } else {
                CmpColumn::Cached(i)
            };
        }
        if key.eq_ignore_ascii_case("status") {
            return CmpColumn::Phase;
        }
        CmpColumn::Missing
    }

    fn eval_filter(
        &self,
        o: &DynamicObject,
        key: &RowKey,
        plan: &FilterPlan<'_>,
        cells: &mut HashMap<RowKey, CellCacheEntry>,
    ) -> bool {
        match plan {
            FilterPlan::Fuzzy { pat, mask } => {
                pat.is_empty() || self.fuzzy_match_row(o, pat, *mask, key, cells)
            }
            FilterPlan::Terms(terms) => terms.iter().all(|t| match t {
                TermPlan::Fuzzy { pat, mask } => self.fuzzy_match_row(o, pat, *mask, key, cells),
                TermPlan::NotFuzzy { pat, mask } => {
                    !self.fuzzy_match_row(o, pat, *mask, key, cells)
                }
                TermPlan::Cmp { cmp, column } => self.eval_cmp(o, cmp, column, key, cells),
            }),
        }
    }

    /// Does one fuzzy pattern match this row? "namespace name" first (the
    /// original haystack — cheap, and by far the most common hit), then each
    /// rendered column cell individually, so `/10.96` finds a Service by its
    /// CLUSTER-IP. Cells are matched one at a time rather than joined so a
    /// pattern can't match across cell boundaries; the full row is only
    /// rendered when the name haystack missed.
    ///
    /// The cell fallback reads *through the cell cache*. It used to call
    /// `self.spec.cells(o)` directly, re-rendering every column of every
    /// non-matching object on every keystroke and discarding the result — on
    /// a helm view that meant five gunzip+JSON-parse rounds per row per
    /// keypress. Cached by `resourceVersion`, a row is now rendered once per
    /// change instead of once per keystroke.
    fn fuzzy_match_row(
        &self,
        o: &DynamicObject,
        pat: &str,
        pat_mask: u64,
        key: &RowKey,
        cells: &mut HashMap<RowKey, CellCacheEntry>,
    ) -> bool {
        {
            // Built into a reused buffer: this used to `format!` a fresh
            // `String` for every object on every keystroke.
            let mut hay = self.hay_buf.borrow_mut();
            self.write_fuzzy_hay(o, &mut hay);
            if subseq_mask(&hay) & pat_mask == pat_mask
                && self.matcher.fuzzy_match(&hay, pat).is_some()
            {
                return true;
            }
        }
        let entry = self.cell_entry(key, o, cells);
        // No cell can contain every pattern character, so none can match.
        if entry.row_mask & pat_mask != pat_mask {
            return false;
        }
        entry
            .cells
            .iter()
            .zip(&entry.cell_masks)
            .any(|(c, &m)| m & pat_mask == pat_mask && self.matcher.fuzzy_match(c, pat).is_some())
    }

    /// The cached cells for `key`, rendering them if absent or stale.
    ///
    /// A hit is the common case — an unchanged row on a refilter, and every
    /// visible row on an ordinary redraw — so it must not allocate: the
    /// resourceVersion is compared borrowed rather than cloned up front, and
    /// the entry API resolves the key once instead of hashing it again to read
    /// back what the staleness check just looked at.
    fn cell_entry<'c>(
        &self,
        key: &RowKey,
        o: &DynamicObject,
        cells: &'c mut HashMap<RowKey, CellCacheEntry>,
    ) -> &'c CellCacheEntry {
        use std::collections::hash_map::Entry;
        let rv = o.metadata.resource_version.as_deref();
        match cells.entry(RowKey::clone(key)) {
            Entry::Occupied(occupied) => {
                let stale = {
                    let e = occupied.get();
                    e.plural != self.kind_plural || e.resource_version.as_deref() != rv
                };
                let e = occupied.into_mut();
                if stale {
                    *e = self.render_cell_entry(o);
                }
                e
            }
            Entry::Vacant(vacant) => vacant.insert(self.render_cell_entry(o)),
        }
    }

    fn render_cell_entry(&self, o: &DynamicObject) -> CellCacheEntry {
        let (rendered, status_idx) = self.spec.cells(o);
        let cell_masks: Vec<u64> = rendered.iter().map(|c| subseq_mask(c)).collect();
        let row_mask = cell_masks.iter().fold(0u64, |a, m| a | m);
        CellCacheEntry {
            plural: self.kind_plural.clone(),
            resource_version: o.metadata.resource_version.clone(),
            cells: rendered,
            status_idx,
            cell_masks,
            row_mask,
        }
    }

    /// What fuzzy terms match against: "namespace name". Helm rows are backed
    /// by the storage Secret, whose own name (`sh.helm.release.v1.<release>.
    /// v<n>`) isn't what a user typing a filter means — match the release
    /// name instead.
    fn write_fuzzy_hay(&self, o: &DynamicObject, out: &mut String) {
        let name = if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            crate::helm::release_name(o).unwrap_or_default()
        } else {
            o.metadata.name.as_deref().unwrap_or("")
        };
        out.clear();
        out.push_str(o.metadata.namespace.as_deref().unwrap_or(""));
        out.push(' ');
        out.push_str(name);
    }

    /// Evaluate one typed column comparison against an object. `cpu`/`mem`
    /// read the live metrics snapshot, `age` the creation timestamp; any
    /// other key names a displayed column (numeric values compare by the
    /// cell's leading number, text case-insensitively).
    fn eval_cmp(
        &self,
        o: &DynamicObject,
        cmp: &crate::filter::Cmp,
        column: &CmpColumn,
        key: &RowKey,
        cells: &mut HashMap<RowKey, CellCacheEntry>,
    ) -> bool {
        use crate::filter::CmpValue;
        match &cmp.value {
            CmpValue::Cpu(want) => cmp.op.eval(self.metrics_for(o).0.cmp(want)),
            CmpValue::Mem(want) => cmp.op.eval(self.metrics_for(o).1.cmp(want)),
            CmpValue::Duration(want) => match crate::columns::age_secs(o) {
                Some(age) => cmp.op.eval(age.cmp(want)),
                None => false,
            },
            CmpValue::Num(want) => match self.column_cell(o, column, key, cells) {
                Some(cell) => cmp
                    .op
                    .eval(crate::columns::parse_leading_num(&cell).total_cmp(want)),
                None => false,
            },
            // `want` was folded once at parse time. ASCII cells compare through
            // an allocation-free byte iterator; non-ASCII cells use
            // whole-string lowercasing for context-sensitive Unicode mappings.
            CmpValue::Str(want) => match self.column_cell(o, column, key, cells) {
                Some(cell) => cmp.op.eval(crate::filter::cmp_folded_lower(&cell, want)),
                None => false,
            },
        }
    }

    /// The displayed cell a comparison reads, for the column
    /// [`Self::resolve_cmp_column`] already picked.
    ///
    /// Ordinary columns come from the same per-revision cell cache the fuzzy
    /// fallback and the renderer read, so a comparison filter no longer
    /// re-extracts (and, on a Helm view, re-gunzips) one column of every
    /// object on every rebuild. Elapsed-time columns are rendered live because
    /// their value drifts without a new resourceVersion.
    fn column_cell<'a>(
        &self,
        o: &'a DynamicObject,
        column: &CmpColumn,
        key: &RowKey,
        cells: &'a mut HashMap<RowKey, CellCacheEntry>,
    ) -> Option<Cow<'a, str>> {
        match *column {
            // Borrowed: the namespace is already a `String` on the object, and
            // this runs per object per rebuild.
            CmpColumn::Namespace => Some(o.metadata.namespace.as_deref().unwrap_or("").into()),
            CmpColumn::Cached(i) => self
                .cell_entry(key, o, cells)
                .cells
                .get(i)
                .map(|c| Cow::Borrowed(c.as_str())),
            CmpColumn::Live(i) => self.spec.cell_at(o, i).map(Cow::Owned),
            CmpColumn::Phase => {
                let phase = phase(o);
                (!phase.is_empty()).then_some(Cow::Owned(phase))
            }
            CmpColumn::Missing | CmpColumn::Unused => None,
        }
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
    /// the filter pass, so this is purely a rendering aid, not a second
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
        // CPU/MEM (and the node capacity percentages and pod counts) sort by
        // live poll snapshots, which move without a new resourceVersion, so
        // those keys can never be cached.
        let volatile_sort = matches!(sort_header, Some("CPU" | "MEM" | "%CPU" | "%MEM" | "PODS"));
        // The aggregated Helm release list (`helm list` semantics) shows only
        // the latest revision per release; `helmhistory` (one release's full
        // history) shows every revision, so it skips this.
        // Recomputed only when the store actually moved: a rebuild staled by a
        // filter keystroke or a sort toggle reuses the previous dedup.
        if self.kind_plural == "helm" {
            let version = self.store.version();
            if cache
                .helm_latest
                .as_ref()
                .is_none_or(|(v, _)| *v != version)
            {
                cache.helm_latest = Some((version, self.helm_latest_revision_keys()));
            }
        } else if cache.helm_latest.is_some() {
            cache.helm_latest = None;
        }
        // Parsed once, not once per object: the filter check used to re-borrow
        // the filter cache and re-compare the raw filter string for every row.
        let parsed = self.parsed_filter();
        // …and compiled once: pattern masks and comparison-column resolution
        // are object-independent, but used to be redone for every row.
        let plan = self.compile_filter_plan(&parsed);
        // Disjoint field borrows so the filter can warm the cell cache while
        // the sort-key cache is also held.
        let RowsCache {
            cells,
            sort_keys,
            helm_latest,
            ..
        } = &mut *cache;
        let helm_latest = helm_latest.as_ref().map(|(_, keys)| keys);

        // (primary sort key, (ns, name) tiebreak, store key)
        let empty_sort: Rc<str> = Rc::from("");
        let mut entries: Vec<(SortKey, (&str, &str), &RowKey)> =
            Vec::with_capacity(self.store.len());
        for (k, o) in self.store.iter() {
            if let Some(keep) = helm_latest
                && !keep.contains(k)
            {
                continue;
            }
            if let Some(owner) = &self.owner
                && !owner.owns(o)
            {
                continue;
            }
            if !self.matches_filter_cached(o, k, &plan, cells) {
                continue;
            }
            // One watch event marks the whole ordering dirty, so the
            // rebuild touches every object — computed sort keys are
            // cached per resourceVersion so the N-1 unchanged rows reuse
            // theirs instead of re-extracting (and, for helm, re-gunzipping)
            // their cells.
            let primary = match sort_header {
                None => SortKey::Text(empty_sort.clone()),
                Some(h) if volatile_sort => self.column_sort_key(o, h),
                Some(h) => {
                    let rv = o.metadata.resource_version.as_deref();
                    match sort_keys.get(k) {
                        Some(e) if e.header == h && e.resource_version.as_deref() == rv => {
                            e.key.clone()
                        }
                        _ => {
                            let key = self.column_sort_key(o, h);
                            sort_keys.insert(
                                k.clone(),
                                SortKeyEntry {
                                    header: h.to_string(),
                                    resource_version: o.metadata.resource_version.clone(),
                                    key: key.clone(),
                                },
                            );
                            key
                        }
                    }
                }
            };
            let tie = (
                o.metadata.namespace.as_deref().unwrap_or(""),
                o.metadata.name.as_deref().unwrap_or(""),
            );
            entries.push((primary, tie, k));
        }
        // Unstable: the `(namespace, name)` fallback below is a total order
        // for a Kubernetes object set, so stability buys nothing here — and a
        // stable sort allocates an n/2 scratch buffer on every rebuild.
        match sort_header {
            // No sort column: every primary key is the same empty string, so
            // the whole ordering is the namespace/name fallback. Comparing it
            // directly skips a `SortKey` comparison per comparison.
            None => entries.sort_unstable_by(|a, b| a.1.cmp(&b.1)),
            Some(_) => {
                let desc = self.sort_desc;
                entries.sort_unstable_by(|a, b| {
                    let mut ord = a.0.cmp_to(&b.0);
                    if desc {
                        ord = ord.reverse();
                    }
                    // Ties always fall back to namespace/name ascending.
                    ord.then_with(|| a.1.cmp(&b.1))
                });
            }
        }
        // Refilled in place: this is one of the few allocations that scales
        // with the store, and a rebuild happens per watch event whenever a
        // filter or sort is active.
        cache.keys.clear();
        cache
            .keys
            .extend(entries.iter().map(|(_, _, k)| RowKey::clone(k)));
        cache.dirty = false;

        // `cells`/`sort_keys` are otherwise only ever cleared wholesale by
        // `clear_rows_cache` on a view change — bound their growth once they
        // have drifted well past what the current view needs (stale entries
        // for rows removed one-by-one mid-view, rather than by a view
        // switch). The bound is the *store* size, not the visible row count:
        // the filter path warms a cell entry for every object it tests,
        // including the ones it rejects, so a narrow filter legitimately
        // leaves far more cells than keys. Bounding against `keys` there
        // evicted the whole cache on every rebuild and re-rendered every
        // row's cells on the next one.
        //
        // The two maps are filled by different paths — cells by the filter,
        // sort keys by the sort — so they are checked independently rather
        // than one standing in for the other.
        let bound = self.store.len().saturating_mul(2).max(64);
        if cache.cells.len() > bound {
            cache
                .cells
                .retain(|k, _| self.store.get(k.as_ref()).is_some());
        }
        if cache.sort_keys.len() > bound {
            cache
                .sort_keys
                .retain(|k, _| self.store.get(k.as_ref()).is_some());
        }
        // Emptying a map does not hand its table back, and `invalidate_row`
        // already drops a deleted row's entries one at a time — so after a
        // 20k-pod namespace is left for one holding 50, length says nothing
        // and only capacity still shows the 20k-slot allocation. Shrink only
        // when the table dwarfs the view (4x), and shrink to `bound` rather
        // than to fit, so the next rebuild does not trip the same check and
        // rehash again.
        if cache.cells.capacity() > bound.saturating_mul(4) {
            cache.cells.shrink_to(bound);
        }
        if cache.sort_keys.capacity() > bound.saturating_mul(4) {
            cache.sort_keys.shrink_to(bound);
        }
    }

    /// Store keys of the highest-revision secret per (namespace, release) —
    /// label-based (no gunzip/decode needed), used to dedup the aggregated
    /// Helm release list down to one row per release, like `helm list`.
    fn helm_latest_revision_keys(&self) -> HashSet<RowKey> {
        let mut latest: HashMap<(String, String), (i64, RowKey)> = HashMap::new();
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
            .filter_map(|k| self.store.get(k.as_ref()))
            .collect()
    }

    /// The rows for one viewport: `n` display-ordered rows starting at
    /// `offset`. What the table renderer wants per frame — it must not pay
    /// for materializing every off-screen row just to draw one screenful.
    pub fn rows_window(&self, offset: usize, n: usize) -> Vec<&DynamicObject> {
        self.ensure_rows_cache();
        self.rows_cache
            .borrow()
            .keys
            .iter()
            .skip(offset)
            .take(n)
            .filter_map(|k| self.store.get(k.as_ref()))
            .collect()
    }

    /// [`Self::rows_window`] with each row's canonical store key alongside it.
    ///
    /// The renderer needs both, and the key it needs already exists: rebuilding
    /// it from the object meant formatting `"{ns}/{name}"` into a fresh
    /// `String` per visible row per frame — once to warm the cell cache and
    /// again to read it back — and then hashing that instead of the `Rc` the
    /// store and the caches are already keyed by.
    pub(crate) fn rows_window_keyed(
        &self,
        offset: usize,
        n: usize,
    ) -> Vec<(&RowKey, &DynamicObject)> {
        self.ensure_rows_cache();
        self.rows_cache
            .borrow()
            .keys
            .iter()
            .skip(offset)
            .take(n)
            .filter_map(|k| self.store.entry(k.as_ref()))
            .collect()
    }

    /// Every display-ordered row with its canonical store key.
    pub(crate) fn rows_keyed(&self) -> Vec<(&RowKey, &DynamicObject)> {
        self.rows_window_keyed(0, usize::MAX)
    }

    pub(crate) fn ensure_table_cell_cache(&self, rows: &[(&RowKey, &DynamicObject)]) {
        let mut cache = self.rows_cache.borrow_mut();
        for (key, obj) in rows {
            // Shares `cell_entry` with the filter pass, so a row rendered for
            // filtering is already warm for the renderer (and vice versa) and
            // there is one place that decides what "stale" means.
            self.cell_entry(key, obj, &mut cache.cells);
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
        if self.node_capacity_columns() {
            h.push("PODS".into());
        }
        if self.metrics_columns() {
            h.push("CPU".into());
            h.push("MEM".into());
        }
        if self.node_capacity_columns() {
            h.push("%CPU".into());
            h.push("%MEM".into());
        }
        h
    }

    /// Nodes get usage as a percentage of `status.allocatable` next to the
    /// absolute CPU/MEM — "how full is this node" is the number a nodes view
    /// is opened for.
    pub fn node_capacity_columns(&self) -> bool {
        self.kind_plural == "nodes"
    }

    pub(crate) fn view_spec(&self) -> &crate::columns::ViewSpec {
        &self.spec
    }

    /// The coloring thresholds in effect for the current view: the current
    /// kind's per-resource overrides layered over the global defaults, or the
    /// bare defaults when the kind is unknown (synthetic helm views, an
    /// unconnected cluster).
    pub(crate) fn resolved_thresholds(&self) -> crate::thresholds::Thresholds {
        match self.kind.as_ref() {
            Some(k) => self.thresholds.resolve(&k.ar),
            None => self.thresholds.defaults(),
        }
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
        self.col_offset = 0;
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
        // A remembered sort on a wide-only column comes back the moment its
        // column does.
        self.apply_remembered_sort();
        self.flash = format!("wide columns: {}", if self.wide { "on" } else { "off" });
        self.flash_err = false;
    }

    /// Scroll the table columns horizontally (←/→): the NAMESPACE/NAME
    /// prefix stays anchored while the columns after it shift. Clamped so
    /// the last column can always be reached and at least one scrollable
    /// column stays visible.
    pub(super) fn scroll_columns(&mut self, delta: isize) {
        let anchored = usize::from(self.show_namespace_column()) + 1;
        let scrollable = self.display_headers().len().saturating_sub(anchored);
        let max = scrollable.saturating_sub(1);
        self.col_offset = self
            .col_offset
            .min(max)
            .saturating_add_signed(delta)
            .min(max);
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
    ///
    /// Looked up through a reused buffer: this runs per visible row per frame
    /// (and per object per rebuild when sorting or filtering by CPU/MEM), and
    /// it used to clone the object's name and then format a second `String`
    /// for the key on every one of those.
    pub(crate) fn metrics_for(&self, o: &DynamicObject) -> (i64, i64) {
        let name = o.metadata.name.as_deref().unwrap_or_default();
        if self.kind_plural != "pods" {
            return self.metrics.get(name).copied().unwrap_or((0, 0));
        }
        let mut key = self.metrics_key_buf.borrow_mut();
        key.clear();
        key.push_str(o.metadata.namespace.as_deref().unwrap_or(""));
        key.push('/');
        key.push_str(name);
        self.metrics.get(key.as_str()).copied().unwrap_or((0, 0))
    }

    /// Latest pod count for a node from the pods poll; `None` before the
    /// first successful list (renders "-", distinct from a genuinely empty
    /// node).
    pub fn node_pods_for(&self, o: &DynamicObject) -> Option<usize> {
        let name = o.metadata.name.as_deref().unwrap_or_default();
        self.node_pods
            .as_ref()
            .map(|m| m.get(name).copied().unwrap_or(0))
    }

    /// The PODS cell for a node as displayed.
    pub fn node_pods_cell(&self, o: &DynamicObject) -> String {
        match self.node_pods_for(o) {
            Some(n) => n.to_string(),
            None => "-".into(),
        }
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
                    .to_lowercase()
                    .into(),
            ),
            // Unknown timestamps sort last (oldest-unknown) in ascending order.
            "AGE" => SortKey::Num(crate::columns::age_secs(o).unwrap_or(i64::MAX) as f64),
            "CPU" => SortKey::Num(self.metrics_for(o).0 as f64),
            "MEM" => SortKey::Num(self.metrics_for(o).1 as f64),
            // Unknown counts (poll hasn't landed) sort below every real count.
            "PODS" if self.node_capacity_columns() => {
                SortKey::Num(self.node_pods_for(o).map(|c| c as f64).unwrap_or(-1.0))
            }
            // Unknown allocatable sorts below every real percentage.
            "%CPU" if self.node_capacity_columns() => SortKey::Num(
                crate::columns::usage_pct(
                    self.metrics_for(o).0,
                    crate::columns::node_allocatable(o).0,
                )
                .map(|p| p as f64)
                .unwrap_or(-1.0),
            ),
            "%MEM" if self.node_capacity_columns() => SortKey::Num(
                crate::columns::usage_pct(
                    self.metrics_for(o).1,
                    crate::columns::node_allocatable(o).1,
                )
                .map(|p| p as f64)
                .unwrap_or(-1.0),
            ),
            // Humanized time cells ("5d23h") must sort by the underlying
            // timestamp, never the rendered string. Negated epoch seconds so
            // ascending = most recent first, matching AGE; unknowns last.
            "UPDATED" => SortKey::Num(
                crate::helm::decode_summary(o)
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
                None => SortKey::Text(Rc::from("")),
            },
        }
    }

    pub(super) fn reset_sort(&mut self) {
        self.sort_column = None;
        self.sort_desc = false;
    }

    /// Record the active sort for the current kind (and persist it), so the
    /// choice survives view switches and restarts. Called after every user
    /// sort change; with no active sort (the picker's default entry) the
    /// kind's entry is forgotten instead. View switches call `reset_sort`
    /// directly and must NOT land here — a switch isn't a sort choice.
    pub(super) fn remember_sort(&mut self) {
        if self.kind_plural.is_empty() {
            return;
        }
        let kind = self.kind_plural.clone();
        match self
            .sort_column
            .and_then(|i| self.display_headers().get(i).cloned())
        {
            Some(h) => self.sort_memory.set(&kind, &h, self.sort_desc),
            None if self.sort_memory.clear(&kind) => {}
            None => return, // nothing was remembered; skip the disk write
        }
        if let Some(path) = self.sort_memory_path.clone()
            && let Err(e) = self.sort_memory.save(&path)
        {
            self.flash_warn(&format!("failed to save sort state: {e}"));
        }
    }

    /// Restore the remembered sort for the current kind, unless a sort is
    /// already active (a bookmark's sort spec, or a header repinned across a
    /// spec refresh, must win). A remembered header missing from the current
    /// layout is left in memory untouched: CRD printer columns arrive after
    /// the watch starts (see `Msg::PrinterColumns`, which retries this), and
    /// a wide-only column simply stays dormant until `w`.
    pub(super) fn apply_remembered_sort(&mut self) {
        if self.sort_column.is_some() {
            return;
        }
        let Some((header, desc)) = self.sort_memory.get(&self.kind_plural) else {
            return;
        };
        if let Some(i) = self.display_headers().iter().position(|h| *h == header) {
            self.sort_column = Some(i);
            self.sort_desc = desc;
            self.invalidate_rows();
        }
    }

    /// Toggle ascending/descending for the active sort column (k9s `I`).
    pub(super) fn toggle_sort_dir(&mut self) {
        let Some(i) = self.sort_column else {
            self.flash_warn("press S to pick a sort column first");
            return;
        };
        self.sort_desc = !self.sort_desc;
        self.invalidate_rows();
        self.remember_sort();
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
        let idx = self.table_state.selected()?;
        self.ensure_rows_cache();
        let cache = self.rows_cache.borrow();
        self.store.get(cache.keys.get(idx)?.as_ref())
    }

    pub fn selected(&self) -> Option<DynamicObject> {
        self.selected_ref().cloned()
    }

    /// `(header, value)` pairs for the selected row, mirroring the table's
    /// displayed columns (NAMESPACE prefix, view-spec cells with volatile
    /// overrides, PODS/CPU/MEM suffixes) — but with the full cell values,
    /// never the width-truncated text the renderer shows. Empty cells are
    /// dropped: there is nothing to copy from them.
    pub fn selected_row_fields(&self) -> Vec<(String, String)> {
        let Some(obj) = self.selected_ref() else {
            return Vec::new();
        };
        let mut values: Vec<String> = Vec::new();
        if self.show_namespace_column() {
            values.push(obj.metadata.namespace.clone().unwrap_or_default());
        }
        let (cells, _) = self.spec.cells(obj);
        for (i, cell) in cells.into_iter().enumerate() {
            values.push(
                self.spec
                    .volatile(obj, &self.kind_plural, i)
                    .unwrap_or(cell),
            );
        }
        if self.node_capacity_columns() {
            values.push(self.node_pods_cell(obj));
        }
        if self.metrics_columns() {
            let (cpu, mem) = self.metrics_for(obj);
            values.push(crate::columns::fmt_cpu(cpu));
            values.push(crate::columns::fmt_mem(mem));
            if self.node_capacity_columns() {
                let (alloc_cpu, alloc_mem) = crate::columns::node_allocatable(obj);
                values.push(crate::columns::fmt_pct(crate::columns::usage_pct(
                    cpu, alloc_cpu,
                )));
                values.push(crate::columns::fmt_pct(crate::columns::usage_pct(
                    mem, alloc_mem,
                )));
            }
        }
        self.display_headers()
            .into_iter()
            .zip(values)
            .filter(|(_, v)| !v.is_empty())
            .collect()
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
        let len = self.row_count() as i32;
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

    pub(super) fn move_page(&mut self, pages: i32) {
        let page = self.table_page_rows.max(1) as i32;
        self.move_selection(pages.saturating_mul(page));
    }
}
