071. P3 — Dependency feature scope has a concrete build-time cleanup candidate.
Evidence: Cargo.toml enables kube's derive feature, but source/bench/example
searches find no CustomResource derive or kube derive-macro use; resources are
DynamicObject or generated k8s_openapi types. Tokio also enables full although
only a subset of its modules is referenced directly (transitive needs matter).
Proposal: audit the resolved feature graph and remove unused direct features,
starting with kube derive; explicitly list Tokio features only if doing so
actually removes dependencies after feature unification. No dependency bumps.
Expected effect: potentially less cold compilation/proc-macro work and smaller
dependency surface. Link-time elimination may already remove runtime impact.
Validate later: normal, all-targets, bench, profiling, and platform builds; do
not infer a saving merely from shortening the manifest feature list.

072. P3 — Static --info output still pays for a multithread async runtime.
Evidence: src/main.rs::main constructs a new_multi_thread runtime and calls
run_main; only inside run_main does args.info print diagnostics and return.
Clap's --help/--version already exit before runtime creation.
Proposal: resolve configuration and handle the static --info branch before
building Tokio; retain exactly the same diagnostics and configuration warnings.
Expected effect: fewer threads and less startup work for this short-lived CLI
path, with no claimed effect on the interactive application's steady state.
Validate later: stdout/stderr/exit status parity and kubeconfig override order.

073. P3 — Escape-sequence repair allocates a tiny Vec for ordinary key events.
Evidence: src/altscroll.rs::Repair::push returns vec![key] in its normal pass-
through branch; all return paths contain at most three events. flush similarly
returns at most two. main dispatches the returned events immediately.
Proposal: return a small fixed-capacity iterator/array-plus-length or dispatch
through a callback, avoiding a dependency solely for three stack-resident keys.
Expected effect: one small allocation avoided per repaired input event, most
noticeable in rapid scroll/repeat bursts; lower priority than redraw costs.
Validate later: all CSI/SS3 repair, timeout, malformed/replayed sequence, and
modifier tests, plus end-to-end handle_key ordering.

074. P2 — Clipboard delivery can occupy blocking workers indefinitely.
Evidence: src/app/actions.rs::copy_to_clipboard_async starts a spawn_blocking
job per request. helpers.rs::copy_to_clipboard synchronously write_all's the
entire payload to a child and waits without a deadline. A present but stuck
clipboard tool prevents trying the next candidate. OSC 52 allocates both a
base64 buffer and a second full escape-sequence buffer.
Proposal: bounded clipboard-job concurrency, explicit subprocess I/O deadlines
with kill/reap on failure, and a single output buffer for OSC 52. Set a clear
large-payload policy; serialize terminal escape writes with normal terminal
output so worker delivery cannot interleave mid-frame.
Expected effect: bounded threads/retained payloads and reliable fallback when
clipboard helpers hang; lower peak memory for large copies.
Validate later: pipe backpressure, hung/missing helpers, repeated copies, remote
terminal fallback, complete Unicode payloads, and shutdown cleanup.

075. P2 — Leaving large transient views retains their full backing payloads.
Evidence: src/app/input.rs::key_scroll returns to the parent mode and stops
streams without clearing logs.view or detail. App also retains pending_bundle,
explain_source, gitops_source, and their derived result collections; context
switch cleanup clears store/view cache/timeline but not these document fields.
Proposal: define a byte-aware retention policy for inactive views, releasing
unneeded text/index/style/source buffers on close or under memory pressure.
Treat the intentionally retained last bundle separately: keep :bundle-save
working, or explicitly spool it to an appropriately protected temporary file.
Expected effect: lower idle memory after inspecting large logs, YAML, diffs,
and bundles. Retention is not inherently a leak; preserve intentional reuse.
Validate later: reopening/return navigation, pending save/copy jobs, context
switches, and sensitive-data lifetime, with retained bytes measured separately
from allocator RSS that may not immediately fall after objects are dropped.

076. P2 — Non-plugin command output is also fully buffered when little is used.
Evidence: actions.rs::run_helm/start_transfer and pickers.rs::rename_context
use Command.output(), ignore successful stdout, and use only small portions of
stderr on failure. details.rs::describe fully collects stdout and then copies
it into line Strings. These calls have no explicit application job deadline.
Proposal: discard truly unused stdout, drain bounded diagnostic head/tail data,
and stream describe output into a byte-limited document. Share the process-job
infrastructure proposed in 057/074, but give mutations and file transfers
explicit cancellation/timeout semantics: killing a client is not rollback.
Expected effect: bounded output memory and fewer indefinitely retained jobs;
large describe results avoid an extra full-document intermediate.
Validate later: successful verbose tools, large/erroring output, missing tools,
slow auth plugins, process cleanup, and reporting uncertain mutation outcomes.

077. P1 — The Helm release summary downloads and retains all revision payloads.
Evidence: lifecycle.rs::open_helm_releases watches Secrets with owner=helm and
type=helm.sh/release.v1 across the selected scope. rows.rs deduplicates only the
visible rows to each release's latest revision; Store still owns the older
Secrets, including their encoded compressed release bodies.
Proposal: consider a metadata-first Helm summary/watch to identify latest
revision keys, lazy-fetching and byte-caching only payloads needed for visible
columns/details. History can fetch the requested release's revisions on demand.
This is a separate architectural option from faster decoding in 049/052.
Expected effect: potentially large network/heap savings in scopes with many
releases and long histories, at the cost of additional GETs and loading states.
Validate later: whether label metadata suffices for each field, RBAC/API
compatibility, latest-revision deletion/fallback, updates during fetch, history
navigation, and request-count tradeoffs. Do not discard revisions blindly.

078. P2 — Namespace cache misses do not distinguish loading, empty, or failed.
Evidence: pickers.rs::ensure_namespace_cache starts a fetch whenever there is
no real cached name; open_namespaces starts another fetch on every open.
spawn_namespace_fetch has no in-flight marker/job handle, silently drops list
errors, and lists full DynamicObjects solely to extract metadata.name.
Proposal: explicit loading/ready/failed cache state, one refresh per context,
failure retry cooldown, and metadata-only paged listing with supported API
fallback. Keep a manual refresh and verbatim namespace entry. The name-only
node-debugger cleanup list in actions.rs can use the same metadata technique
while retaining its spec.nodeName server selector and deletion safeguards.
Expected effect: no duplicate requests during slow/forbidden/empty responses,
less parsing/transfer memory, and faster useful namespace completion.
Validate later: rapid palette/picker reopen, RBAC denial, empty list, context
switch during fetch, stale refresh results, and metadata API negotiation.

079. P2 — Xray traversal has no cycle/depth/output guard and repeats shared subtrees.
Evidence: dashboards.rs indexes every owner reference, including multiple
parents; helpers.rs::emit_xray recursively follows that index without tracking
the current UID path, a depth bound, or an emitted-row budget. The usual
Deployment/ReplicaSet/Pod tree is shallow, but malformed cyclic references or
multiply owned descendants are not guarded at this layer.
Proposal: per-path cycle detection, a depth/output budget with visible truncated
or cycle markers, and shared-node representation/cached summaries where one
descendant is reachable along many paths. An iterative traversal can also
avoid call-stack exhaustion; do not silently erase legitimate multiple owners.
Expected effect: bounded work for pathological object graphs and less repeated
subtree expansion; normal trees may see little change beyond 039/068.
Validate later: self-cycle, two-node cycle, deep chains, diamond ownership,
missing owner UIDs, and stable navigation targets for abbreviated entries.

080. P1 — The streaming viewport benchmark grows its buffer without bound.
Evidence: benches/hot_paths.rs::log_viewport creates one LogsView outside the
Criterion iteration loop, then extends it by 50 lines on every streaming
iteration. It never applies production retention/trim logic or resets the
fixture. Warmup and subsequent samples therefore run at changing buffer sizes
and can consume excessive memory; this also misses the trim rescan in 026.
Proposal: model a fixed-capacity saturated production buffer with append plus
front eviction, and add a separately named append-only case with a bounded,
reset fixture. Separate cache-hit, filter change, width change, trim, and huge-
line cases. Update stale benchmark comments claiming already-removed repeated
pod_summary/Helm-decode work so results are interpreted against current code.
Expected effect: bounded benchmark resource use and representative evidence for
log optimization decisions, not a claimed product speedup.
Validate later: stable retained line/byte counts throughout measurement and
matching production message/retention paths. No benchmark was run in this audit.
