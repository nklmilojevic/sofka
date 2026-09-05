001. P1 — Cell-cache hits allocate resource-version strings.
Evidence: src/app/rows.rs::cell_entry clones metadata.resource_version before
checking whether the cached entry is stale. Both filtering and table cache
warming use this function, so unchanged rows still allocate on a cache hit.
Proposal: compare borrowed Option<&str> values and clone only when replacing
an entry; consider an entry API to avoid the second hash lookup.
Expected effect: fewer small allocations per visible row and per refilter.
Validate later: allocation counts for unchanged frames and repeated filters;
preserve invalidation for resource-version/kind changes and missing versions.

002. P1 — Message draining can monopolize the UI event loop.
Evidence: src/main.rs::run drains rx.try_recv() until empty inside one select
branch. The channel is bounded at 4096 messages, but producers may refill it
while it drains; capacity does not bound the time spent processing a burst.
Proposal: give each drain a message-count or elapsed-time budget, then yield
to input/render scheduling. Coalesce replaceable metrics snapshots and, where
safe, resource updates; preserve reset/sync/delete order and timeline history.
Expected effect: bounded input and redraw latency during watch/log storms.
Validate later: sustained producer load, input latency, queue pressure, and
correctness across generation changes and relists.

003. P2 — Unconditional timer redraws and no-op input redraws.
Evidence: src/main.rs::run marks every one-second tick dirty after reaping
forwards/expiring flash, regardless of whether anything visible changed;
dispatch draws after every nonempty key batch, even an ignored key.
Proposal: return visible-change flags from housekeeping/input handling and
schedule redraws only for visible age/time boundaries or changed state.
Expected effect: less idle CPU and terminal/widget work, especially in static
documents and modal views. Keep notification and escape-repair deadlines live.
Validate later: all ticking fields, flash expiry, forwards, resize, key repeats,
and document/filter modes; measure idle wakeups and frames.

004. P2 — Headless snapshots always wait about three seconds.
Evidence: src/main.rs::snapshot waits until a fixed three-second deadline,
with 250 ms receive timeouts, even after a complete watch sync and metrics.
Proposal: finish once required sources are ready, with a short quiet window
and the existing timeout as a fallback; optionally expose a wait policy.
Expected effect: faster headless smoke/snapshot workflows. Readiness must cover
the selected dashboard and optional metrics, not only Store::synced.
Validate later: fast/slow/missing metrics, empty resources, relists, errors,
and deterministic rendered snapshots.

005. P1 — Filtered/sorted watch updates rebuild and sort the whole row set.
Evidence: src/app/rows.rs::invalidate_row_contents marks all rows dirty for
any active filter, sort, owner scope, or Helm view. ensure_rows_cache then
iterates the entire store and sorts all accepted rows; it also allocates fresh
entries and key vectors. The ordinary unsorted update path is already cached.
Proposal: retain reusable buffers first; for large busy views, maintain row
membership/order incrementally when one row changes, with full rebuilds for
filter/sort/scope changes. Treat volatile metrics and age predicates separately.
Expected effect: less O(N log N) work per redraw under filtered/sorted watches.
Validate later: mixed updates/inserts/deletes, tie ordering, selection anchoring,
Helm latest-revision replacement, and small versus large store sizes.

006. P3 — Filter pattern masks and column resolution repeat per object.
Evidence: src/app/rows.rs::fuzzy_match_row recomputes subseq_mask(pat) for each
row/term; column_cell resolves a header by name on each structured comparison.
Proposal: compile masks and resolved comparison-column accessors with the
parsed filter, invalidating the latter when the view's columns change.
Expected effect: smaller refilter cost; retain the existing cheap rejection
and allocation-free namespace comparisons. Measure before adding complexity.
Validate later: mixed structured/fuzzy filters and dynamic printer columns.

007. P1 — Canonical row identities are discarded and reconstructed in rendering.
Evidence: src/app/rows.rs::rows_window returns objects without their existing
RowKey; ensure_table_cell_cache formats row_key and looks it up again.
src/ui.rs::draw_table formats row_key again and a separate metrics key.
src/app/rows.rs::metrics_for additionally clones the object's name.
Proposal: carry (&RowKey, &DynamicObject) through the viewport and use borrowed
canonical keys for cache/marks/metrics lookups; borrow node names directly.
Expected effect: remove several string allocations/hash lookups per visible row
and per metric comparison/sort. Consider an iterator to avoid the viewport Vec.
Validate later: namespaced/cluster-scoped rows, missing metadata, marks, metrics,
and unchanged-frame allocations.

008. P2 — Table layout metadata and header hints are rebuilt on every frame.
Evidence: src/app/rows.rs::display_headers clones/builds all header strings;
src/ui.rs::draw_table clones them again for widgets and recomputes alignments,
header indices, and column rules. draw_header/header_hints build formatted
static binding lines, even before checking whether the hints fit the terminal.
Proposal: cache semantic headers, column indices/rules/alignments, and hints
by view/config revision; borrow strings and check visibility before building
hints. Resolve theme styling when rendering so skin changes remain correct.
Expected effect: less fixed work on every frame, including small tables.
Validate later: wide mode, CRD columns arriving, namespace mode, skins, compact
mode, sort arrows, and narrow terminal snapshots.

009. P2 — Hidden columns still incur formatting and width scans.
Evidence: src/ui.rs::draw_table calls spec.volatile for all base cells, formats
all metrics/node percentages, and measures all cell widths before applying
col_visible to the resulting widget cells.
Proposal: apply the visible-column mask before formatting and measurement;
retain only additional status/ready values needed to determine row color.
Expected effect: cheaper horizontally scrolled and wide/custom views.
Validate later: hidden STATUS/READY, custom NAME columns, exact widths, metrics
coloring, and transitions when scrolling columns back into view.

010. P3 — Table mouse geometry repeats column layout work.
Evidence: src/ui.rs::draw_table already resolves fixed widths with
distribute_column_widths, then clones constraints and invokes another Layout
for hit-testing before giving the same widths to Table.
Proposal: derive click ranges from the fixed widths, origin, and spacing,
or reuse final geometry if ratatui exposes it. The Layout cache may already
reduce solver cost; measure the actual residual cost before changing this.
Validate later: tiny terminal widths, clipping, highlight spacing, horizontal
scroll, and header-click sorting.
