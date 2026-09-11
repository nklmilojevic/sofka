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
        cache.column_widths = None;
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
        cache.column_widths = None;
        cache.sort_keys.remove(key);
        if !self.filter.is_empty()
            || self.faults_filter_active()
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
        parsed: &crate::filter::ParsedFilter,
        cells: &mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> bool {
        if self.faults_filter_active() && !pod_has_faults(o) {
            return false;
        }
        if self.filter.is_empty() {
            return true;
        }
        self.eval_filter(o, key, parsed, cells, now)
    }

    pub fn faults_filter_active(&self) -> bool {
        self.faults_only && self.kind_plural == "pods"
    }

    fn eval_filter(
        &self,
        o: &DynamicObject,
        key: &RowKey,
        parsed: &crate::filter::ParsedFilter,
        cells: &mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> bool {
        use crate::filter::ParsedFilter;
        match parsed {
            ParsedFilter::Fuzzy(pat) => {
                pat.text().is_empty() || self.pattern_match_row(o, pat, key, cells, now)
            }
            ParsedFilter::Structured(s) => s
                .terms
                .iter()
                .all(|t| self.eval_term(o, key, t, cells, now) == Some(true)),
        }
    }

    fn eval_term(
        &self,
        o: &DynamicObject,
        key: &RowKey,
        term: &crate::filter::Term,
        cells: &mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> Option<bool> {
        use crate::filter::Term;
        match term {
            Term::Text { negate, pat } => {
                Some(negate ^ self.pattern_match_row(o, pat, key, cells, now))
            }
            Term::Label { negate, pat } => Some(
                negate
                    ^ o.metadata.labels.as_ref().is_some_and(|labels| {
                        labels.iter().any(|(key, value)| {
                            self.pattern_matches(pat, key) || self.pattern_matches(pat, value)
                        })
                    }),
            ),
            Term::Cmp(cmp) => self.eval_cmp(o, key, cmp, cells, now),
            Term::All(terms) | Term::Not(terms) | Term::Any(terms) => {
                let any = matches!(term, Term::Any(_));
                let inverse = matches!(term, Term::Not(_));
                let mut result = Some(!any);
                for t in terms {
                    match self.eval_term(o, key, t, cells, now) {
                        Some(value) if value == any => return Some(value ^ inverse),
                        None => result = None,
                        _ => {}
                    }
                }
                result.map(|v| v ^ inverse)
            }
        }
    }

    /// Does one text pattern match this row? "namespace name" first (the
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
    fn pattern_match_row(
        &self,
        o: &DynamicObject,
        pat: &crate::filter::Pattern,
        key: &RowKey,
        cells: &mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> bool {
        // Only fuzzy patterns use the byte mask. Unicode lowercase conversion
        // can change the bytes of a literal or its cell text.
        let pat_mask = match pat {
            crate::filter::Pattern::Fuzzy(_) => subseq_mask(pat.text()),
            _ => 0,
        };
        {
            // Built into a reused buffer: this used to `format!` a fresh
            // `String` for every object on every keystroke.
            let mut hay = self.hay_buf.borrow_mut();
            self.write_fuzzy_hay(o, &mut hay);
            if subseq_mask(&hay) & pat_mask == pat_mask && self.pattern_matches(pat, &hay) {
                return true;
            }
        }
        let entry = self.cell_entry(key, o, cells, now);
        // No cell can contain every pattern character, so none can match.
        if entry.row_mask & pat_mask != pat_mask {
            return false;
        }
        entry
            .cells
            .iter()
            .zip(&entry.cell_masks)
            .any(|(c, &m)| m & pat_mask == pat_mask && self.pattern_matches(pat, c))
    }

    /// One pattern against one string, with no prefiltering.
    fn pattern_matches(&self, pat: &crate::filter::Pattern, text: &str) -> bool {
        use crate::filter::Pattern;
        match pat {
            Pattern::Fuzzy(needle) => self.matcher.score(text, needle).is_some(),
            Pattern::Literal(lit) => lit.matches(text),
            Pattern::Regex(re) => re.is_match(text),
        }
    }

    /// The cached cells for `key`, rendering them if absent or stale.
    ///
    /// Every object of every rebuild reaches this, so the hit path does no
    /// work beyond one lookup: the revision is compared borrowed rather than
    /// cloned, and `entry` hashes the key once instead of probing before and
    /// after the staleness check. Cloning the `Rc<str>` key to hold that
    /// entry is a refcount bump, not a copy of the key text.
    fn cell_entry<'c>(
        &self,
        key: &RowKey,
        o: &DynamicObject,
        cells: &'c mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> &'c CellCacheEntry {
        use std::collections::hash_map::Entry;
        let rv = o.metadata.resource_version.as_deref();
        let fresh = |e: &CellCacheEntry| {
            e.plural == self.kind_plural && e.resource_version.as_deref() == rv
        };
        let slot = match cells.entry(key.clone()) {
            Entry::Occupied(e) if fresh(e.get()) => return e.into_mut(),
            slot => slot,
        };
        let (rendered, status_idx, helm_updated) =
            self.spec
                .cells_with_helm_time(o, now, self.server_table.cells(o));
        let cell_masks: Vec<u64> = rendered.iter().map(|c| subseq_mask(c)).collect();
        let row_mask = cell_masks.iter().fold(0u64, |a, m| a | m);
        let built = CellCacheEntry {
            plural: self.kind_plural.clone(),
            resource_version: o.metadata.resource_version.clone(),
            cells: rendered,
            status_idx,
            helm_updated,
            cell_masks,
            row_mask,
        };
        match slot {
            Entry::Occupied(mut e) => {
                e.insert(built);
                e.into_mut()
            }
            Entry::Vacant(e) => e.insert(built),
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
        key: &RowKey,
        cmp: &crate::filter::Cmp,
        cells: &mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> Option<bool> {
        use crate::filter::CmpValue;
        if let Some(column) = self.spec.formatted_quantity_column(&cmp.key) {
            let wanted = match &cmp.value {
                CmpValue::Num(value) | CmpValue::Quantity { value, .. } => Some(*value),
                CmpValue::Cpu { quantity, .. } | CmpValue::Mem { quantity, .. } => Some(*quantity),
                _ => None,
            };
            if let Some(wanted) = wanted {
                let actual = crate::views::formatted_quantity_value(o, column)?;
                return wanted
                    .is_finite()
                    .then_some(cmp.op.eval(actual.partial_cmp(&wanted)?));
            }
        }
        if let Some(metric) = self.spec.metric(&cmp.key) {
            let actual = self.metric_value(o, metric)? as f64;
            let wanted = match &cmp.value {
                CmpValue::Cpu { milli: v, .. } | CmpValue::Mem { bytes: v, .. } => *v as f64,
                CmpValue::Num(v) | CmpValue::Quantity { value: v, .. } => {
                    if metric.cpu() && !metric.percentage() {
                        v * 1000.0
                    } else {
                        *v
                    }
                }
                CmpValue::Str(v) if metric.percentage() => v.strip_suffix('%')?.parse().ok()?,
                _ => return None,
            };
            return wanted
                .is_finite()
                .then(|| cmp.op.eval(actual.total_cmp(&wanted)));
        }
        if let Some((index, column)) = self.spec.server_column(&cmp.key) {
            let value = self.server_table.cells(o)?.get(index)?;
            if value.is_null() {
                return None;
            }
            let ordering = match &cmp.value {
                CmpValue::Num(want) if column.numeric() => value.as_f64()?.total_cmp(want),
                CmpValue::Str(want) | CmpValue::Quantity { text: want, .. } => {
                    crate::filter::cmp_folded_lower(&crate::server_table::render(Some(value)), want)
                }
                _ => return None,
            };
            return Some(cmp.op.eval(ordering));
        }
        let ordering = match &cmp.value {
            CmpValue::Cpu { milli: want, .. } => self.row_metrics(o, key)?.0.cmp(want),
            CmpValue::Mem { bytes: want, .. } => self.row_metrics(o, key)?.1.cmp(want),
            CmpValue::Duration(want) => crate::columns::age_secs(o, now)?.cmp(want),
            CmpValue::Quantity { text, .. } => {
                let cell = self.column_cell(o, key, &cmp.key, cells, now)?;
                crate::filter::cmp_folded_lower(&cell, text)
            }
            CmpValue::Num(want) => {
                let cell = self.column_cell(o, key, &cmp.key, cells, now)?;
                crate::filter::cell_number(&cell)?.total_cmp(want)
            }
            // `want` was folded once at parse time. ASCII cells compare through
            // an allocation-free byte iterator; non-ASCII cells use
            // whole-string lowercasing for context-sensitive Unicode mappings.
            CmpValue::Str(want) => {
                let cell = self.column_cell(o, key, &cmp.key, cells, now)?;
                crate::filter::cmp_folded_lower(&cell, want)
            }
        };
        Some(cmp.op.eval(ordering))
    }

    fn row_metrics(&self, o: &DynamicObject, key: &RowKey) -> Option<(i64, i64)> {
        let metric_key = match self.kind_plural.as_str() {
            "pods" => key.as_ref(),
            "nodes" => o.metadata.name.as_deref()?,
            _ => return None,
        };
        self.metrics.get(metric_key).copied()
    }

    /// The displayed cell a comparison key names (case-insensitive column
    /// header), plus NAMESPACE and a `/status/phase` fallback for kinds
    /// without a STATUS column. Runs per object per rebuild whenever a
    /// structured filter is active, so the object-borrowing keys are matched
    /// before anything renders and the rest read through the row cache.
    fn column_cell<'c>(
        &self,
        o: &'c DynamicObject,
        row: &RowKey,
        key: &str,
        cells: &'c mut crate::store::FastMap<RowKey, CellCacheEntry>,
        now: i64,
    ) -> Option<Cow<'c, str>> {
        // One match, not a chain of comparisons: these keys borrow straight
        // from the object and never render a cell, and this runs per object
        // per rebuild.
        match key {
            "namespace" | "ns" | "metadata.namespace" => {
                return Some(o.metadata.namespace.as_deref().unwrap_or("").into());
            }
            "metadata.name" => return o.metadata.name.as_deref().map(Cow::Borrowed),
            "spec.nodename" => {
                return o
                    .data
                    .pointer("/spec/nodeName")
                    .and_then(|v| v.as_str())
                    .map(Cow::Borrowed);
            }
            "status.phase" => {
                return o
                    .data
                    .pointer("/status/phase")
                    .and_then(|v| v.as_str())
                    .map(Cow::Borrowed);
            }
            _ => {}
        }
        if let Some(i) = self.spec.header_index(key) {
            if let Some(value) = self.live_cell(o, i) {
                return Some(Cow::Owned(value));
            }
            // Time-derived cells (AGE, a running Job's DURATION, a CronJob's
            // LAST-SCHEDULE, user `time` columns) drift without a new
            // resourceVersion, so the row cache cannot answer for them.
            if let Some(cell) = self.spec.volatile(o, &self.kind_plural, i, now) {
                return Some(Cow::Owned(cell));
            }
            // Through the cell cache, not `cell_at`: one curated cell can cost
            // a full `containerStatuses` walk and three `String`s (READY,
            // STATUS and RESTARTS share one summary), and `cell_at` pays that
            // again for every object on every keystroke. The cached row is
            // keyed by resourceVersion, so the rows the watch did not touch
            // are already rendered — and rendering the whole row on a miss
            // costs one summary, the same walk the single cell needed.
            return Some(Cow::Borrowed(
                self.cell_entry(row, o, cells, now).cells.get(i)?.as_str(),
            ));
        }
        if key == "status" {
            let phase = phase(o);
            return (!phase.is_empty()).then_some(Cow::Owned(phase));
        }
        None
    }

    /// Whether the running watch is scoped by `-l`/`-f` selectors from the
    /// filter — i.e. the active filter is (partly) server-side.
    pub fn filter_server_side(&self) -> bool {
        self.applied_filter_labels.is_some() || self.applied_filter_fields.is_some()
    }

    pub fn filter_location(&self) -> &'static str {
        if self.filter_selectors_pending() {
            return " ·pending ⏎";
        }
        if !self.filter_server_side() {
            return " ·local";
        }
        match &*self.parsed_filter() {
            crate::filter::ParsedFilter::Structured(s) if !s.terms.is_empty() => " ·server+local",
            _ => " ·server",
        }
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

    /// Char indices in `name` that matched the active row filter's text
    /// pattern, for highlighting them in the table. `None` when there's no
    /// active filter or no positive text term (every visible row already
    /// passed the filter pass, so this is purely a rendering aid, not a
    /// second filter decision).
    ///
    /// Memoized per name for the current filter: the renderer asks this for
    /// every visible row on every redraw, and re-running the matcher to get
    /// an answer that cannot have changed is the single most expensive thing
    /// a filtered frame used to do.
    pub fn filter_match_indices(&self, name: &str) -> Option<Rc<[usize]>> {
        if self.filter.is_empty() {
            return None;
        }
        let parsed = self.parsed_filter();
        let pat = parsed.highlight_pattern()?;

        let mut cache = self.highlight_cache.borrow_mut();
        if cache.filter != self.filter {
            cache.filter.clear();
            cache.filter.push_str(&self.filter);
            cache.rows.clear();
        }
        if let Some(hit) = cache.rows.get(name) {
            return hit.clone();
        }
        if cache.rows.len() >= HIGHLIGHT_CACHE_LIMIT {
            cache.rows.clear();
        }
        let idx = self.match_positions(pat, name).map(Rc::from);
        cache.rows.insert(Box::from(name), idx.clone());
        idx
    }

    /// Where `pat` matched in `name`, as char positions. A literal or a regex
    /// matches one contiguous run, so both report the span they landed on;
    /// only fuzzy scatters its positions.
    fn match_positions(&self, pat: &crate::filter::Pattern, name: &str) -> Option<Vec<usize>> {
        use crate::filter::Pattern;
        match pat {
            Pattern::Fuzzy(needle) => self.matcher.indices(name, needle),
            Pattern::Literal(lit) => lit.match_span(name).map(Iterator::collect),
            Pattern::Regex(re) => re.find(name).map(|m| {
                // Byte offsets from the regex, char positions for the cell
                // renderer, which walks `name.chars()`.
                let start = name[..m.start()].chars().count();
                (start..start + m.as_str().chars().count()).collect()
            }),
        }
    }

    pub(super) fn ensure_rows_cache(&self) {
        let mut cache = self.rows_cache.borrow_mut();
        let now = crate::columns::now_secs();
        if !cache.dirty && (!cache.time_sensitive || cache.filter_second == now) {
            return;
        }
        let parsed = self.parsed_filter();
        let headers = self.display_headers();
        let sort_header = self
            .sort_column
            .and_then(|i| headers.get(i).map(String::as_str));
        // AGE and other time-dependent sort keys move without a new
        // resourceVersion, so they can never be cached and the cache must
        // rebuild once per second even without a watch event.
        let time_dependent_sort =
            sort_header.is_some_and(|h| self.spec.is_time_sort(h, &self.kind_plural));
        cache.time_sensitive = match &*parsed {
            crate::filter::ParsedFilter::Structured(s) => {
                s.terms.iter().any(crate::filter::Term::time_sensitive)
            }
            _ => false,
        } || time_dependent_sort;
        cache.filter_second = now;
        cache.column_widths = None;

        // CPU/MEM (and the node capacity percentages and pod counts) sort by
        // live poll snapshots, which move without a new resourceVersion, so
        // those keys can never be cached.
        let volatile_sort =
            sort_header.is_some_and(|h| self.spec.metric(h).is_some()) || time_dependent_sort;
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
        // One clock reading for the whole rebuild. Every AGE cell, DURATION
        // cell and `age >` comparison in this pass is measured against the
        // same instant, so a rebuild that crosses a second boundary cannot
        // sort two rows against two different "now"s.
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
            if !self.matches_filter_cached(o, k, &parsed, cells, now) {
                continue;
            }
            // One watch event marks the whole ordering dirty, so the
            // rebuild touches every object — computed sort keys are
            // cached per resourceVersion so the N-1 unchanged rows reuse
            // theirs instead of re-extracting (and, for helm, re-gunzipping)
            // their cells.
            let primary = match sort_header {
                None => SortKey::Text(empty_sort.clone()),
                Some(h) if volatile_sort => self.column_sort_key(o, h, now),
                Some(h) => {
                    let rv = o.metadata.resource_version.as_deref();
                    match sort_keys.get(k) {
                        Some(e) if e.header == h && e.resource_version.as_deref() == rv => {
                            e.key.clone()
                        }
                        _ => {
                            let key = self.column_sort_key(o, h, now);
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
        let desc = self.sort_desc && sort_header.is_some();
        // Unstable: the `(namespace, name)` fallback below is a total order
        // for a Kubernetes object set, so stability buys nothing here — and a
        // stable sort allocates an n/2 scratch buffer on every rebuild.
        entries.sort_unstable_by(|a, b| {
            let mut ord = a.0.cmp_to(&b.0);
            if desc {
                ord = ord.reverse();
            }
            // Ties always fall back to namespace/name ascending.
            ord.then_with(|| natural_cmp(a.1.0, b.1.0).then_with(|| natural_cmp(a.1.1, b.1.1)))
        });
        cache.keys = entries.into_iter().map(|(_, _, k)| k.clone()).collect();
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
    fn helm_latest_revision_keys(&self) -> crate::store::FastSet<RowKey> {
        let mut latest: crate::store::FastMap<(String, String), (i64, RowKey)> =
            crate::store::FastMap::default();
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

    /// Measure the full filtered list once per data or view change.
    /// Scrolling reuses these widths and renders only the rows on screen.
    pub(crate) fn table_column_widths(&self) -> Vec<u16> {
        use unicode_width::UnicodeWidthStr;

        self.ensure_rows_cache();
        let headers = self.display_headers();
        let mut cache = self.rows_cache.borrow_mut();
        if let Some(widths) = &cache.column_widths
            && Rc::ptr_eq(&widths.headers, &headers)
        {
            return widths.needed.clone();
        }

        let width = |s: &str| u16::try_from(s.width()).unwrap_or(u16::MAX);
        let mut needed: Vec<u16> = headers.iter().map(|h| width(h)).collect();
        let show_ns = self.show_namespace_column();
        let ns_off = usize::from(show_ns);
        let now = crate::columns::now_secs();
        let RowsCache { keys, cells, .. } = &mut *cache;
        for key in keys.iter() {
            let Some(obj) = self.store.get(key.as_ref()) else {
                continue;
            };
            if show_ns {
                needed[0] =
                    needed[0].max(width(obj.metadata.namespace.as_deref().unwrap_or_default()));
            }
            let entry = self.cell_entry(key, obj, cells, now);
            for (i, cell) in entry.cells.iter().enumerate() {
                let cell_width = if let Some(value) =
                    self.spec
                        .volatile_cached(obj, &self.kind_plural, i, now, entry.helm_updated)
                {
                    // Reserve space for elapsed times as the clock advances.
                    width(&value).max(7)
                } else {
                    width(cell)
                };
                needed[i + ns_off] = needed[i + ns_off].max(cell_width);
            }
        }
        cache.column_widths = Some(TableWidthCache {
            headers,
            needed: needed.clone(),
        });
        needed
    }

    #[cfg(test)]
    pub(crate) fn ensure_table_cell_cache(&self, rows: &[&DynamicObject]) {
        let now = crate::columns::now_secs();
        self.ensure_table_cell_cache_at(rows, now);
    }

    pub(crate) fn ensure_table_cell_cache_at(&self, rows: &[&DynamicObject], now: i64) {
        let mut cache = self.rows_cache.borrow_mut();
        for obj in rows {
            // Shares `cell_entry` with the filter pass, so a row rendered for
            // filtering is already warm for the renderer (and vice versa) and
            // there is one place that decides what "stale" means.
            let key = row_key(obj);
            let key = self
                .store
                .key(&key)
                .cloned()
                .unwrap_or_else(|| Rc::from(key));
            self.cell_entry(&key, obj, &mut cache.cells, now);
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
    /// Memoized against the view spec and the column toggles: this is asked
    /// for from a dozen places, several times per frame, and each answer used
    /// to be a freshly built list of owned header strings.
    pub fn display_headers(&self) -> Rc<[String]> {
        let namespace = self.show_namespace_column();
        if let Some(c) = self.header_cache.borrow().as_ref()
            && c.namespace == namespace
            && c.spec_rev == self.spec_rev
        {
            return Rc::clone(&c.headers);
        }

        let mut h = self.spec.headers();
        if namespace {
            h.insert(0, "NAMESPACE".into());
        }

        let headers: Rc<[String]> = Rc::from(h);
        *self.header_cache.borrow_mut() = Some(HeaderCache {
            namespace,
            spec_rev: self.spec_rev,
            headers: Rc::clone(&headers),
        });
        headers
    }

    /// Nodes get usage as a percentage of `status.allocatable` next to the
    /// absolute CPU/MEM — "how full is this node" is the number a nodes view
    /// is opened for.
    pub fn node_capacity_columns(&self) -> bool {
        self.kind_plural == "nodes"
            && self
                .kind
                .as_ref()
                .is_some_and(|kind| kind.ar.group.is_empty())
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
    /// its column header as indices change. A selected sort waits while its
    /// column is hidden and returns when that column is available again.
    /// Cached cells are laid out for the old spec, so they're always dropped.
    pub(super) fn refresh_view_spec(&mut self) {
        let sort = match &self.sort_origin {
            SortOrigin::Selected { header, desc } => Some((header.clone(), *desc)),
            _ => self.sort_column.and_then(|i| {
                self.display_headers()
                    .get(i)
                    .cloned()
                    .map(|h| (h, self.sort_desc))
            }),
        };
        let resource = self.kind.as_ref().map(Kind::resource_key);
        let warnings = crate::columns::view_warnings(
            self.kind.as_ref().map_or("", |kind| kind.ar.group.as_str()),
            &self.kind_plural,
            self.active_user_view(),
        );
        for warning in warnings {
            if !self.config_warnings.contains(&warning) {
                self.config_warnings.push(warning);
            }
        }
        let spec = crate::columns::build_spec(
            self.kind.as_ref().map_or("", |kind| kind.ar.group.as_str()),
            &self.kind_plural,
            self.active_user_view(),
            resource
                .as_ref()
                .and_then(|resource| self.crd_views.get(resource))
                .and_then(Option::as_ref),
            self.wide,
        );
        self.spec = if self.server_table_eligible() && !self.server_table.columns.is_empty() {
            crate::columns::build_table_spec(
                self.kind.as_ref().map_or("", |kind| kind.ar.group.as_str()),
                &self.kind_plural,
                &self.server_table.columns,
                self.wide,
            )
        } else {
            spec
        };
        self.spec_rev = self.spec_rev.wrapping_add(1);
        if let Some((h, desc)) = sort {
            self.sort_column = self.display_headers().iter().position(|x| *x == h);
            self.sort_desc = self.sort_column.is_some() && desc;
        }
        self.clear_rows_cache();
        self.col_offset = 0;
        self.col_scroll_max = 0;
    }

    pub(super) fn view_namespace(&self) -> Option<&str> {
        self.kind.as_ref().filter(|kind| kind.namespaced)?;
        (!self.all_namespaces()).then_some(self.namespace.as_str())
    }

    /// The user-configured view matching the current kind, if any. Synthetic
    /// views (helm/helmhistory) are backed by an unrelated kind (`secrets`),
    /// so they never match.
    pub(super) fn active_user_view(&self) -> Option<&crate::views::View> {
        let kind = self.kind.as_ref()?;
        if kind.ar.plural.to_lowercase() != self.kind_plural {
            return None;
        }
        crate::views::lookup(&self.user_views, &kind.ar, self.view_namespace())
    }

    /// Apply a view's configured initial sort, unless a sort is already
    /// active (a refresh must not clobber the user's choice).
    pub(super) fn apply_view_sort(&mut self) {
        if self.sort_column.is_some()
            || matches!(
                self.sort_origin,
                SortOrigin::Selected { .. } | SortOrigin::Cleared
            )
        {
            return;
        }
        let specific_sort = self.active_user_view().and_then(|v| v.sort.clone());
        let is_specific = specific_sort.is_some();
        let Some((header, desc)) =
            specific_sort.or_else(|| self.user_views.get("*").and_then(|v| v.sort.clone()))
        else {
            return;
        };
        match self.display_headers().iter().position(|h| *h == header) {
            Some(i) => {
                self.sort_column = Some(i);
                self.sort_desc = desc;
                self.sort_origin = SortOrigin::Configured;
                self.invalidate_rows();
            }
            None if is_specific => {
                self.flash_warn(&format!("view sort column '{header}' not found"));
            }
            None => {}
        }
    }

    /// Toggle wide mode (`w`): show/hide wide-only columns.
    pub(super) fn toggle_wide(&mut self) {
        self.wide = !self.wide;
        self.refresh_view_spec();
        // A remembered sort on a wide-only column comes back the moment its
        // column does.
        self.apply_remembered_sort();
        self.apply_view_sort();
        self.flash = format!("wide columns: {}", if self.wide { "on" } else { "off" });
        self.flash_err = false;
    }

    /// Move the table by five terminal cells. Keep the name columns fixed.
    pub(super) fn scroll_columns(&mut self, delta: isize) {
        self.col_offset = self
            .col_offset
            .saturating_add_signed(delta * 5)
            .min(self.col_scroll_max);
    }

    pub fn show_namespace_column(&self) -> bool {
        self.kind
            .as_ref()
            .map(|k| k.namespaced && self.all_namespaces())
            .unwrap_or(false)
    }

    pub fn metrics_columns(&self) -> bool {
        matches!(self.kind_plural.as_str(), "pods" | "nodes")
            && self
                .kind
                .as_ref()
                .is_some_and(|kind| kind.ar.group.is_empty())
    }

    /// Latest (cpu_millicores, mem_bytes) for an object from the metrics map.
    pub(crate) fn metrics_for(&self, o: &DynamicObject) -> Option<(i64, i64)> {
        let name = o.metadata.name.clone().unwrap_or_default();
        let key = if self.kind_plural == "pods" {
            format!("{}/{}", o.metadata.namespace.as_deref().unwrap_or(""), name)
        } else {
            name
        };
        self.metrics.get(&key).copied()
    }

    pub(crate) fn metric_value(
        &self,
        obj: &DynamicObject,
        metric: crate::columns::MetricColumn,
    ) -> Option<i64> {
        let group = self.kind.as_ref().map_or("", |k| k.ar.group.as_str());
        if !metric.supported(group, &self.kind_plural) {
            return None;
        }
        metric.value(obj, self.metrics_for(obj), self.node_pods_for(obj))
    }

    pub(crate) fn live_cell(&self, obj: &DynamicObject, idx: usize) -> Option<String> {
        let metric = self.spec.metric_at(idx)?;
        Some(metric.format(self.metric_value(obj, metric)))
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
    pub(super) fn column_sort_key(&self, o: &DynamicObject, header: &str, now: i64) -> SortKey {
        if let Some(metric) = self.spec.metric(header) {
            return SortKey::Num(
                self.metric_value(o, metric)
                    .map(|v| v as f64)
                    .unwrap_or(-1.0),
            );
        }
        let source_header = self
            .spec
            .header_index(header)
            .and_then(|i| self.spec.canonical_header(i))
            .unwrap_or(header);
        // User/printer columns sort by their declared type (quantity, number,
        // time…), and win over the curated special cases so an overlay that
        // redefines a header sorts by its own values.
        if self.spec.is_user_column(header)
            && let Some(v) =
                self.spec
                    .sort_value_with_table(o, header, now, self.server_table.cells(o))
        {
            return SortKey::from(v);
        }
        match source_header {
            "NAMESPACE" => SortKey::Text(
                o.metadata
                    .namespace
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase()
                    .into(),
            ),
            // Unknown timestamps sort last (oldest-unknown) in ascending order.
            "AGE" => SortKey::Num(crate::columns::age_secs(o, now).unwrap_or(i64::MAX) as f64),
            // Humanized time cells ("5d23h") must sort by the underlying
            // timestamp, never the rendered string. Negated epoch seconds so
            // ascending = most recent first, matching AGE; unknowns last.
            "LAST-SEEN" if self.kind_plural == "events" => SortKey::Num(
                crate::columns::event_last_seen_secs(o)
                    .map(|s| -(s as f64))
                    .unwrap_or(f64::INFINITY),
            ),
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
                SortKey::Num(crate::columns::job_duration_secs(o, now).unwrap_or(i64::MAX) as f64)
            }
            // Helm revisions are plain integers; flux REVISION cells (shas,
            // `main@sha1:…`) stay text.
            "REVISION" if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") => {
                SortKey::Num(crate::helm::revision(o).unwrap_or(0) as f64)
            }
            _ => match self.spec.sort_value(o, header, now) {
                Some(v) => SortKey::from(v),
                None => SortKey::Text(Rc::from("")),
            },
        }
    }

    pub(super) fn reset_sort(&mut self) {
        self.sort_column = None;
        self.sort_desc = false;
        self.sort_origin = SortOrigin::Unset;
    }

    /// Record the active sort for the current kind (and persist it), so the
    /// choice survives view switches and restarts. Called after every user
    /// sort change; with no active sort (the picker's default entry) the
    /// kind's entry is forgotten instead. View switches call `reset_sort`
    /// directly and must NOT land here — a switch isn't a sort choice.
    pub(super) fn remember_sort(&mut self) {
        let header = self
            .sort_column
            .and_then(|i| self.display_headers().get(i).cloned());
        self.sort_origin = match &header {
            Some(header) => SortOrigin::Selected {
                header: header.clone(),
                desc: self.sort_desc,
            },
            None => SortOrigin::Cleared,
        };
        if !self.remember_sort || self.kind_plural.is_empty() {
            return;
        }
        let kind = self.kind_plural.clone();
        match header {
            Some(h) => self.sort_memory.set(&kind, &h, self.sort_desc),
            None if self.sort_memory.clear(&kind) => {}
            None => return, // nothing was remembered; skip the disk write
        }
        if let Some(path) = self.sort_memory_path.clone() {
            let result = match &self.state_writer {
                Some(writer) => writer.save_sort(self.sort_memory.clone(), path),
                None => self.sort_memory.save(&path),
            };
            if let Err(e) = result {
                self.flash_warn(&format!("failed to save sort state: {e}"));
            }
        }
    }

    /// Restore the remembered sort for the current kind. It can replace a
    /// configured default, but an active user or bookmark sort has priority.
    /// A remembered header missing from the current
    /// layout is left in memory untouched: CRD printer columns arrive after
    /// the watch starts (see `Msg::PrinterColumns`, which retries this), and
    /// a wide-only column simply stays dormant until `w`.
    pub(super) fn apply_remembered_sort(&mut self) {
        if !self.remember_sort
            || matches!(
                self.sort_origin,
                SortOrigin::Selected { .. } | SortOrigin::Cleared
            )
            || (self.sort_column.is_some() && self.sort_origin != SortOrigin::Configured)
        {
            return;
        }
        let Some((header, desc)) = self.sort_memory.get(&self.kind_plural) else {
            return;
        };
        if let Some(i) = self.display_headers().iter().position(|h| *h == header) {
            self.sort_column = Some(i);
            self.sort_desc = desc;
            self.sort_origin = SortOrigin::Selected { header, desc };
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

    /// Full `(header, value)` pairs for the selected row, in display order.
    /// Live values replace cached cells. Empty cells are excluded.
    pub fn selected_row_fields(&self) -> Vec<(String, String)> {
        let Some(obj) = self.selected_ref() else {
            return Vec::new();
        };
        let mut values: Vec<String> = Vec::new();
        if self.show_namespace_column() {
            values.push(obj.metadata.namespace.clone().unwrap_or_default());
        }
        let now = crate::columns::now_secs();
        let (cells, _, helm_updated) =
            self.spec
                .cells_with_helm_time(obj, now, self.server_table.cells(obj));
        for (i, cell) in cells.into_iter().enumerate() {
            values.push(
                self.live_cell(obj, i)
                    .or_else(|| {
                        self.spec
                            .volatile_cached(obj, &self.kind_plural, i, now, helm_updated)
                    })
                    .unwrap_or(cell),
            );
        }
        self.display_headers()
            .iter()
            .cloned()
            .zip(values)
            .filter(|(_, v)| !v.is_empty())
            .collect()
    }

    pub fn confirm_allows_force_toggle(&self) -> bool {
        matches!(self.confirm_action, Some(ConfirmAction::Delete { .. }))
    }

    /// Whether the open dialog is a drain, whose three `kubectl drain` options
    /// the hint line offers to toggle.
    pub fn confirm_allows_drain_toggles(&self) -> bool {
        matches!(self.confirm_action, Some(ConfirmAction::Drain { .. }))
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

    pub(super) fn extend_selection(&mut self, delta: i32) {
        let rows: Vec<_> = self
            .rows()
            .iter()
            .map(|obj| (row_key(obj), obj.metadata.uid.clone()))
            .collect();
        if rows.is_empty() {
            self.range_selection = None;
            return;
        }
        let current = self.table_state.selected().unwrap_or(0).min(rows.len() - 1);
        // A changed row order ends the old range before another row is marked.
        if self
            .range_selection
            .as_ref()
            .is_none_or(|range| range.rows != rows)
        {
            self.range_selection = Some(RangeSelection {
                anchor: current,
                rows,
                previous_marks: self.marked.clone(),
            });
        }
        let range = self.range_selection.as_ref().unwrap();
        let next = current
            .saturating_add_signed(delta as isize)
            .min(range.rows.len() - 1);
        self.marked.clone_from(&range.previous_marks);
        for (key, _) in &range.rows[range.anchor.min(next)..=range.anchor.max(next)] {
            self.marked.insert(key.clone());
        }
        self.table_state.select(Some(next));
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

fn pod_has_faults(o: &DynamicObject) -> bool {
    if o.metadata.deletion_timestamp.is_some() {
        return true;
    }
    let d = &o.data;
    match d.pointer("/status/phase").and_then(Value::as_str) {
        Some("Succeeded") => return false,
        Some("Running") => {}
        _ => return true,
    }
    let condition_ready = |kind: &str| {
        d.pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|conditions| {
                conditions.iter().any(|c| {
                    c.get("type").and_then(Value::as_str) == Some(kind)
                        && c.get("status").and_then(Value::as_str) == Some("True")
                })
            })
    };
    if !condition_ready("Ready") {
        return true;
    }
    if d.pointer("/spec/readinessGates")
        .and_then(Value::as_array)
        .is_some_and(|gates| {
            gates.iter().any(|gate| {
                gate.get("conditionType")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| !condition_ready(kind))
            })
        })
    {
        return true;
    }
    let Some(statuses) = d
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
    else {
        return true;
    };
    if statuses.is_empty()
        || statuses.iter().any(|c| {
            c.get("ready").and_then(Value::as_bool) != Some(true)
                || c.pointer("/state/running").is_none()
        })
        || d.pointer("/spec/containers")
            .and_then(Value::as_array)
            .is_some_and(|containers| containers.len() != statuses.len())
    {
        return true;
    }
    d.pointer("/spec/initContainers")
        .and_then(Value::as_array)
        .is_some_and(|containers| {
            containers.iter().any(|container| {
                let status = d
                    .pointer("/status/initContainerStatuses")
                    .and_then(Value::as_array)
                    .and_then(|statuses| {
                        statuses
                            .iter()
                            .find(|s| s.get("name") == container.get("name"))
                    });
                status.is_none_or(|s| {
                    if container.get("restartPolicy").and_then(Value::as_str) == Some("Always") {
                        s.get("ready").and_then(Value::as_bool) != Some(true)
                            || s.pointer("/state/running").is_none()
                    } else {
                        s.pointer("/state/terminated/exitCode")
                            .and_then(Value::as_i64)
                            != Some(0)
                    }
                })
            })
        })
}
