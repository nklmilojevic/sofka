021. P2 — Kubeconfig is synchronously reparsed for related identity lookups.
Evidence: src/k8s.rs::connect reads config through infer, then
current_context_name and cluster_name_for read it again; connect_context reads
it before from_config also calls cluster_name_for. list_contexts is another
read during startup; cluster_name_for_context likewise reparses for callers.
Proposal: resolve client/context/cluster/catalog identity from one kubeconfig
snapshot per connect or explicit refresh, and pass the derived identity along.
Expected effect: lower startup/fleet/context-switch file I/O and parsing cost,
especially with large merged kubeconfigs or slow config storage.
Validate later: KUBECONFIG merging, in-cluster fallback, explicit context,
rename/reload behavior, and credential refresh (do not freeze auth tokens).

022. P3 — Discovery aliases duplicate Kind strings and resolution deep-clones them.
Evidence: src/k8s.rs::discover/add_aliases store Kind copies for qualified,
plural, kind-name, and alias keys; resolve lowercases input and clones Kind.
Proposal: store one immutable Kind per resource with alias-to-index/shared
references, and add borrowed resolution for read-only callers. Cache normalized
resource identities used in completion. Preserve stable group-priority rules.
Expected effect: smaller discovery registry and cheaper repeated resolution in
CRD-heavy clusters. This is a lower-priority allocation optimization.
Validate later: colliding groups/aliases, all served versions, and custom kinds.

023. P1 — Main resource watcher lacks the retry pacing already used for node counts.
Evidence: src/k8s.rs::spawn_watch immediately polls again after emitting a
watch error, with no backoff/sleep adapter. In contrast,
src/app/lifecycle.rs::spawn_node_pods_poll explicitly documents rapid failed
list retries and implements backoff reset only on actual progress.
Proposal: use the same progress-aware retry policy in the main watcher and
audit other watcher loops for equivalent pacing. Deduplicate repeated errors
without suppressing a changed error or successful recovery.
Expected effect: less API/CPU/message-channel load during persistent failures.
Validate later: mock-server failing initial lists, failed watch starts, 410
recovery, 429/Retry-After handling, auth errors, and streaming-list fallback.
The exact retry rate is not measured in this review.

024. P1 — View/history bounds count objects, not retained bytes.
Evidence: src/app/lifecycle.rs::evict_view_cache caps views and total objects
but explicitly retains the newest view even if it exceeds the object limit.
start_watch clones a cached Items map into the live store while retaining the
cached map; relists can additionally hold pending objects. Large Secret/CRD
payloads make equal object counts have very different memory costs.
Proposal: add a byte/weight budget for view snapshots and histories; remove or
move a cached entry when making it active if the navigation lifecycle permits.
Account for shared Arc payloads separately from duplicate map/key overhead.
Expected effect: predictable RSS after browsing large/heavy resource sets.
Validate later: oversized individual views, overlapping snapshots, relists,
cached navigation, and release of superseded object versions.

025. P2 — Metrics polling reparses each container's resource quantities twice.
Evidence: src/app/lifecycle.rs::spawn_metrics_poll calls container_usage_of
for per-container data and usage_of again for pod totals on the same object.
It also constructs a new Api and fresh maps on each five-second poll, and lists
the namespace/all namespaces without using the main watch's drill selectors.
Proposal: parse container usage once and aggregate totals during that pass;
reuse Api and reserve maps. Where supported, scope metrics requests to the
view or avoid retaining unrelated rows, while preserving complete metrics for
containers the user can open. Investigate unchanged-snapshot suppression.
Expected effect: lower poll CPU/memory and potentially less API data for narrow
drill-downs in large namespaces. API selector support needs validation.
Validate later: missing quantities, nodes versus pods, container totals, view
switches, and metrics freshness while opening overlays.

026. P1 — Log-index incrementality is lost once the buffer starts trimming.
Evidence: src/app/logs.rs::push_log_lines calls Scrollable::drain_front on
overflow; drain_front bumps revision. src/app/mod.rs::LogsView::refresh_index
resets and rescans all retained lines whenever revision changes. At the follow
cap, every appended batch can trigger a complete filter/wrap scan again.
Proposal: give lines absolute sequence IDs and maintain a deque/ring index;
retire prefix entries and adjust a cumulative-row base without rescanning the
retained tail. Reuse indexed heights when shifting paused anchors.
Expected effect: sustained full-buffer ingestion stays proportional to new
lines instead of buffer length. This differs from already-optimized pre-cap
append and unchanged paused-viewport paths.
Validate later: full configured buffers and 100k paused buffers, sparse filters,
wrap/width changes, follow toggles, clear/restart, and exact frozen anchors.

027. P1 — Timeline's seen set grows for the entire context session.
Evidence: src/timeline.rs::observe inserts every formatted kind/row key into
seen. MAX_OBJECTS eviction removes history entries only; neither eviction nor
observe_delete removes seen keys. Only clear (e.g. context switch) resets it.
Proposal: bound creation-dedup state with an explicit retention policy, or use
UID/session-watermark/relist-aware tracking that can discard retired objects
without emitting phantom creations when revisiting views.
Expected effect: bounded memory in long sessions on high-churn clusters.
Validate later: far more than 2000 unique objects, eviction, delete/recreate,
relist/revisit, and no false 'created' transitions. Define the history tradeoff.

028. P2 — Timeline and previous-revision keys allocate on every watch update.
Evidence: src/timeline.rs::observe formats tkey then clones it for seen even
for known objects. src/app/mod.rs::PrevRevisions::insert allocates both tuple
strings and clones the tuple before discovering the key already exists; get
also allocates a tuple. src/app/lifecycle.rs clones Msg::Applied's key for Store.
Proposal: use shared/canonical object identity or nested maps with borrowed
lookup; allocate identity only on first insertion. Store::apply can replace
existing values through borrowed lookup instead of allocating a fresh Rc key
for each update (the existing map key is retained).
Expected effect: lower small-allocation rate under busy watches.
Validate later: history eviction, cross-kind identities, relists, previous
revision semantics, and allocations per unchanged/changed event.

029. P2 — Document search cache hits still copy all matching indices.
Evidence: src/app/mod.rs::Scrollable::match_lines clones c.matches on a hit;
src/ui.rs::doc_title calls it each frame just to get length/current position.
focus_first_match and step_match also copy the vector before using one index.
Proposal: expose borrowed/cache-backed matches or count/index accessors; use
binary search for the first match at/after the current line.
Expected effect: allocation-free cached search navigation/title updates, most
noticeable for common patterns in very large documents.
Validate later: same-line-count replacement, search changes, wrap navigation,
no matches, and cycling at either end.

030. P2 — Horizontal document scrolling rescans the entire document per key.
Evidence: src/app/mod.rs::Scrollable::scroll_h recomputes the maximum char
count across every line on every horizontal movement. set_viewport also
allocates an O(lines) ends vector when wrapping is disabled, where row=line.
Proposal: cache width by document revision (incrementally for log appends),
and use direct arithmetic for unwrapped document row lookup.
Expected effect: predictable horizontal key latency and less layout memory.
Validate later: ANSI/wide glyph width semantics, replacement/trim, wrap toggles,
empty documents, and terminal resize.
