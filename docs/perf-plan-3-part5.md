041. P1 — Repeated fleet refreshes bypass the intended concurrency ceiling.
Evidence: src/app/fleet.rs::key_fleet('r') calls spawn_fleet_gathers without
aborting existing gathers or changing generation. Each invocation allocates
its own four-permit semaphore and appends task handles, so repeated refreshes
can overlap arbitrarily and old results can overwrite newer rows.
Proposal: one fleet refresh task/epoch with cancellation and a shared global
concurrency budget; reap completed handles. Prepare per-context policy outside
the UI thread (open_fleet currently resolves config/kubeconfig for every row).
Expected effect: controlled network/task load and faster opening of large
fleets. Within each context, gather independent summaries concurrently and
reuse connection/discovery state where freshness/auth semantics permit.
Validate later: rapid refresh, old/new reply ordering, many contexts, timeouts,
leaving the dashboard, and policy correctness.

042. P1 — Global find fetches full objects and retains all lists/matches for 200 hits.
Evidence: src/app/find.rs::start_find join_all holds completed full
DynamicObject lists for up to 15 kinds until every request finishes, including
Secret bodies; scoring retains and sorts every match before take(200), although
only names/namespaces are needed.
Proposal: use metadata-only paginated lists with an appropriate compatibility
fallback, process completions incrementally with bounded concurrency, and keep
a stable top-200 heap/partial selection with explicit tie ordering.
Expected effect: much lower transferred/retained payload and sort memory on
large clusters, and useful partial results before the slowest kind finishes.
Validate later: ties across namespaces/kinds, RBAC failures, unsupported metadata
negotiation, pagination consistency, and exact top-200 equivalence.

043. P1 — Right-sizing starts eight historical queries per container without a cap.
Evidence: src/app/rightsize.rs::open_rightsize join_all's all containers;
gather_container concurrently issues eight queries each, including three CPU
subqueries over the same window. Autodiscovered metrics transport stays local
to the task rather than updating the session provider, so later opens rediscover.
Proposal: share a backend request budget, cache discovered transport, and
consider vector queries grouped by container to reduce 8N HTTP requests toward
eight. Reuse historical intermediate series/recording rules only if available
and semantically equivalent. Selectively parse scalar responses (src/rightsize.rs
currently builds a full Value for one sample).
Expected effect: lower backend concurrency/repeated scans and first-use overhead.
Validate later: quantile/max aggregation semantics, absent containers, mixed
failures, timeouts, large windows, and backend query cost as well as UI latency.

044. P1 — Superseded read-only operations continue doing network/CPU work.
Evidence: src/app/find.rs::start_find, src/app/rightsize.rs::open_rightsize,
src/app/details.rs::describe, and src/app/bundle.rs::open_bundle spawn detached
tasks without storing handles. Generation checks discard eventual results,
but do not cancel requests or computation. Repeated requests within the same
generation can overlap; status claims alone do not cancel obsolete work.
Proposal: track cancellable read jobs by purpose and request ID, cancel on
replacement/context departure, and impose end-to-end deadlines/concurrency
limits. Treat mutations separately: never abort a submitted mutation merely
because its originating view closed.
Expected effect: less wasted work and stale-response interference after rapid
navigation/repeated commands. Ensure subprocess children are also terminated.
Validate later: slow requests, repeated commands, leaving/re-entering modes,
context switches, and correct handling of already-completed side effects.

045. P2 — Diagnostic bundles serialize independent gathers and retain duplicate text.
Evidence: src/app/bundle.rs gathers pods, then events, then owner, then each
pod's logs sequentially; max_pods bounds count, not bytes or total duration.
It clones the selected/typed object, redact_to_yaml builds another Value, YAML
is split into owned lines and copied into Doc, and save_bundle clones the full
pending document again.
Proposal: bounded concurrent independent gathers with per-source/whole-bundle
deadlines and byte budgets; share the immutable object/document, serialize
redacted views directly where practical, and stream sections into one buffer
or writer. Preserve the preview and redaction manifest exactly.
Expected effect: faster bundles and lower peak RSS on large objects/logs.
Validate later: partial failures, slow logs, redaction equivalence, save-after-
preview, repeated saves, and no raw Secret data in exported output.

046. P1 — Explain/bundle fetch all namespace events to keep a small related subset.
Evidence: src/app/explain.rs::spawn_explain and src/app/bundle.rs::open_bundle
call list_or_warn for the whole event namespace, then filter_events clones
matching DynamicObjects. Pod evidence is also cloned for the single-pod case.
Proposal: use UID/name field-selected event lists for small target sets, or a
shared indexed event inventory when there are many related pods; use references
or move retained events instead of cloning. Balance request count against bytes.
Expected effect: lower evidence-gather latency and memory in busy namespaces.
Validate later: core/v1 versus events.k8s.io selectors, UID/name fallback,
missing identifiers, RBAC, many pods, and incomplete-evidence warnings.

047. P1 — Interactive snapshot capture does all row rendering/serialization on input.
Evidence: src/app/snapshot.rs::take_snapshot builds snapshot_table and calls
Snapshot::render before spawning only the file write. snapshot_table warms the
cell cache for every filtered row and copies every cell; src/snapshot.rs::align_table
scans text widths, creates padded per-row strings, then copies them into output.
Proposal: capture stable object/order/config/metrics references and build the
export in a worker; serialize JSON/YAML through a writer. Text needs a width pass
but can write rows directly into the final buffer without per-row copies.
Expected effect: lower capture key latency and peak memory for large tables.
Validate later: point-in-time consistency, Unicode alignment, filters/sort,
volatile columns, output format equivalence, and updates during capture.

048. P2 — Snapshot browsing performs filesystem scans/reads/deletes synchronously.
Evidence: src/app/snapshot.rs::reload_snapshot_list reads/stats/sorts all files
on the UI thread; path.is_file then entry.metadata can duplicate metadata work.
open_selected_snapshot reads the whole file before constructing the document;
deleting a file immediately triggers another full directory scan.
Proposal: load metadata/content in bounded background jobs, reuse one metadata
result per entry, update the cached list after a deletion, and bound/lazily read
very large snapshots. Keep ordering deterministic and report stale results.
Expected effect: responsive browsing with many captures or slow state storage.
Validate later: files disappearing/changing mid-read, permission failures,
selection after deletion, large files, and refresh of externally added files.

049. P2 — Helm decode retains two base64 buffers plus a growable decompressed buffer.
Evidence: src/helm.rs::release_json allocates helm_encoded, gzipped, and json,
then reads gzip to the end without a decompressed-byte limit. decode_summary
already skips heavy JSON fields, but still decompresses the entire payload.
Proposal: reuse bounded decode scratch buffers or evaluate streaming selective
deserialization; enforce a sensible decompressed limit. Full detail views can
select only manifest/notes/config as needed instead of decoding every field.
Expected effect: lower decode peak memory and allocation churn on large Helm
releases. Streaming may trade memory for CPU; measure both before adoption.
Validate later: real small/large releases, compression ratios, corrupt/truncated
data, missing fields, and visible behavior when a size limit is exceeded.

050. P1 — Custom-column extraction deep-clones values before formatting/sorting.
Evidence: src/views.rs::extract returns obj.data.pointer(...).cloned();
condition_value also clones. render_value clones strings again, and selected
complex metadata fields serialize to a Value before selecting a nested value.
Every user-column render/sort therefore owns intermediates unnecessarily.
Proposal: return a borrowed/owned scalar-or-Value view, traverse compiled
pointer segments, and own only the final display value. Directly access typed
metadata scalars and nested labels/annotations/owners when possible.
Expected effect: fewer allocations and no whole-array/object clone for large
custom columns. JSON pointer escaping and serialization behavior must match.
Validate later: whole metadata, nested arrays/objects, absent/null values,
escaped keys, conditions, all column types, and printer-column fallbacks.
