031. P1 — Log retention and queued batches are bounded by counts, not bytes.
Evidence: src/app/logs.rs::push_log_lines retains logs_cfg.buffer lines while
following or 100000 while paused. src/app/mod.rs defines batches by 64 lines;
src/main.rs bounds the shared queue by 4096 messages. Individual strings and
multiline provider records have no corresponding byte ceiling here.
Proposal: add retained-log and batch byte budgets in addition to count limits;
flush batches by either limit and bound exceptionally large individual lines.
Expected effect: predictable memory under large structured logs and producer
bursts. Include queued and in-flight producer data in the memory budget.
Validate later: long lines, multiple sources, trim anchors, multiline records,
and an explicit visible indication when data is truncated or dropped.

032. P1 — Aggregate kubelet logs create an unbounded number of live streams.
Evidence: src/app/logs.rs::spawn_selector_logs spawns a JoinSet task for every
regular container in every matching pod. Each task opens a connection and owns
a buffer and 50 ms flush interval; per_pod_tail limits history, not stream count.
Proposal: add a configurable fan-out budget and expose partial coverage, or
offer provider aggregation for broad scopes. Limit simultaneous connection
setup and consolidate batching across streams. Long-lived streams cannot
simply sit forever behind a concurrency semaphore without a coverage policy.
Expected effect: controlled sockets/tasks/API load on large workloads.
Validate later: thousands of containers, cancellation, quiet versus busy logs,
initial history volume, and clear source-coverage reporting.

033. P2 — Kubelet log ingestion copies even unprefixed lines and loses batch capacity.
Evidence: src/app/helpers.rs::forward_log_stream uses format!(prefix + line)
even when prefix is empty; send_log_batch uses mem::take, leaving a zero-capacity
Vec that must grow again for every batch. Provider batches use the same sender.
Proposal: move unprefixed Strings directly; allocate prefixed strings with
known capacity. Replenish batches at the expected capacity or recycle buffers
if measurements justify a pool; avoid unnecessary per-entry line Vecs.
Expected effect: fewer allocations on the native Kubernetes log path.
Validate later: low-rate partial batches, full bursts, prefixes, stream ending,
generation cancellation, and allocation counts per ingested line.

034. P2 — Notify/event watches share the main watch's missing retry pacing.
Evidence: src/app/notify.rs::toggle_notify immediately ignores watcher errors
and polls again; src/app/details.rs opens its event watcher without a backoff
adapter. Apply finding 023's common progress-aware retry policy here too.
Additional notify scaling: one watch is retained per notified object without
a session cap, independently of the table watch. Consider sharing compatible
subscriptions or a configurable watcher budget while preserving background
notifications when navigating away.
Validate later: persistent API failure, many notify targets, clear/context
switch, and no duplicate transitions from overlapping subscriptions.

035. P2 — Notification output is coalesced per message drain, not per rendered frame.
Evidence: src/main.rs::run calls take_notification/run_notify_command after
each rx drain. Many small drains can launch many subprocesses before one frame;
src/app/notify.rs joins all pending text before truncating to 300 characters.
Proposal: debounce/rate-limit notification delivery on an explicit deadline,
bound queued notification text before concatenation, and limit concurrent
notifier processes. Preserve a count/summary of merged changes.
Expected effect: fewer process launches and temporary strings during rollouts.
Validate later: repeated bursts, slow notifier commands, missing executables,
important error transitions, and timely delivery for isolated notifications.

036. P1 — Events rebuild/sort/send the entire document for every initial item.
Evidence: src/app/details.rs::open_events_for calls send_event_snapshot after
every Init/InitApply/Apply/Delete/InitDone. src/app/helpers.rs formats and sorts
all accumulated events each time. An N-event initial list performs a sequence
of growing full rebuilds and queues redundant documents before the UI draws.
Proposal: publish initial state at InitDone; coalesce later changes to the
frame cadence and cache sorted/formatted event rows or update them incrementally.
Expected effect: avoid roughly quadratic initial formatting and repeated large
messages; reduce UI document-layout invalidations during event bursts.
Validate later: empty/error/relist transitions, deletion, event count updates,
order/ties, search/scroll anchoring, and bounded delay for a single live event.

037. P1 — YAML/diff/Helm document preparation runs synchronously on input.
Evidence: src/app/details.rs::open_detail/show_decoded_secret/open_diff perform
serialization, base64/Helm decoding, or whole-document diff on the UI thread.
object_yaml deep-clones the object even when TypeMeta is already present.
describe serializes a YAML fallback eagerly before starting kubectl, although
a successful describe never uses it.
Proposal: capture a shared immutable object and perform large conversion/diff
work in a bounded blocking worker; prepare fallback only on failure. Serialize
borrowed objects when no mutation is needed, or use a serialization projection.
Expected effect: lower keypress tail latency and peak copies for large CRDs,
Secrets, and Helm releases. Small documents can retain a cheap direct path.
Validate later: selection/context changes while work is pending, cancellation,
type stamping, Secret output, no-difference behavior, and error fallback.

038. P1 — Pulse/Xray repeatedly list whole resource sets in serial.
Evidence: src/app/dashboards.rs::spawn_pulse lists seven kinds sequentially;
spawn_xray lists roots then child kinds sequentially. Both repeat after five
seconds, using list_kind's full unpaginated DynamicObject list.
Proposal: fetch independent kinds with bounded concurrency as a first step;
for sustained dashboards maintain summaries/owner indexes from watches with
fallback polling and backoff. Use paginated accumulation/selective payloads
where full lists are still required; publish partial progress deliberately.
Expected effect: faster dashboards on high-latency clusters and substantially
less repeated API transfer/parsing on large stable clusters.
Validate later: missing kinds/RBAC, partial failures, poll/watch resync,
counts/owner relationships, API load, and concurrency limits.

039. P1 — Xray's owner index deep-clones every child object.
Evidence: src/app/dashboards.rs::spawn_xray keeps the pool of DynamicObjects,
then clones each object into children once for every owner reference. Roots
and pool kinds can also overlap (e.g. pod roots include pods in the pool).
Proposal: index borrowed object references or arena indices into the pool,
share immutable payloads where ownership crosses tasks, and reuse one fetched
inventory for overlapping root/child kinds.
Expected effect: less dashboard peak RSS and refresh CPU, proportional to
object payload and owner count. Keep ownership traversal order deterministic.
Validate later: multiple owners, nested controllers, pod roots, missing UIDs,
cycles/depth protection, and identical flattened tree output.

040. P2 — Sort/namespace/fleet persistence blocks the UI thread.
Evidence: src/app/rows.rs::remember_sort, src/app/pickers.rs::remember_namespace,
and src/app/fleet.rs::toggle_fleet_context directly call save. src/sortmem.rs,
src/nsmem.rs, and src/fleet.rs synchronously create directories, serialize TOML,
and write files. Slow state storage therefore stalls key handling.
Proposal: one background persistence worker, coalescing latest state per path;
write temporary files then atomically rename and flush pending saves on exit.
Expected effect: bounded input latency on slow/encrypted/network storage and
less write amplification from rapid toggles. Preserve visible save errors.
Validate later: rapid state changes, write errors, shutdown flush, and avoiding
an older asynchronous snapshot overwriting a newer one.
