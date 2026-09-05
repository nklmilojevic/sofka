051. P2 — Time cells repeatedly parse unchanged timestamps and read the clock.
Evidence: src/columns.rs::age_secs reads Timestamp::now per call;
timestamp_secs first clones a timestamp string; running-job/cronjob volatile
cells reparse it each frame. src/views.rs::render_time similarly extracts,
parses, and reads time per custom time cell. Cache warming computes volatile
values once and draw_table can immediately replace them with another rendering.
Proposal: cache parsed absolute timestamps by resource version, capture now
once per frame/filter rebuild, and format only at the next displayed boundary.
Use absolute timestamp sort keys instead of caching elapsed ages captured at
different times; treat running durations specially. Helm UPDATED is currently
rendered into its cell cache, so do not accidentally add per-frame Helm decode
when making its elapsed display tick correctly.
Expected effect: less timer-frame work and coherent time values across rows.
Validate later: future/missing timestamps, age thresholds, running/completed
jobs, cached-sort ordering after updates, and exact humanization boundaries.

052. P2 — Row summaries are shared within one render, but not across filter/sort paths.
Evidence: src/columns.rs::ViewSpec::cell_at/sort_value each create a fresh
CellContext. A structured pod comparison can build all three pod-summary
strings for one field; multiple terms repeat it. Helm sort/comparison may
decode independently of the full-cell cache. ready_condition allocates both
status/message for each of the Flux READY and MESSAGE extractors.
Proposal: cache compact typed row summaries (counts, condition references,
Helm metadata) per resource version and reuse across cells/filter/sort. Compare
raw numeric/borrowed text values rather than formatting then reparsing them.
Expected effect: cheaper compound filters and cold sorted/filtered renders.
Validate later: curated/user overrides, quantity comparison semantics, volatile
fields, pod status precedence, and equivalence of display-based comparisons.

053. P3 — Column helpers build intermediate Strings/Vecs for joined output.
Evidence: src/columns.rs::external_ip/ingress_hosts/ingress_address/
httproute_hostnames/node_roles clone strings before joining; svc_ports builds
one String per port. hpa_metric formats JSON pointers repeatedly; event_message
copies before replacing newlines. node_roles sorts labels already traversed
from a BTreeMap with a shared prefix.
Proposal: join borrowed slices or write into one reserved buffer, traverse HPA
objects with get instead of formatted pointers, and keep an unchanged-text
fast path. Remove redundant ordering only after confirming the source order.
Expected effect: smaller per-row extraction cost, especially array-heavy rows.
Validate later: empty/default cases, output order, multiple ports/addresses,
Unicode values, and exact cell text.

054. P1 — Timeline transition checks allocate even when no transition occurred.
Evidence: src/timeline.rs::pod_transitions calls owned str_at for both phases,
waiting_reason for both objects, and condition returns owned status/reason
pairs before comparing them. lifecycle::handle_msg invokes observe for every
Applied object, including repeated resource versions during relists.
Proposal: compare borrowed strings/typed summaries and allocate only when
emitting an Entry; short-circuit identical known UID/resource-version pairs
where that guarantees identical content. Share pod scans where practical.
Expected effect: less per-event CPU/allocation on normal unchanged-health
updates and relists, without losing meaningful history.
Validate later: unchanged health with new versions, missing versions, UID
replacement, restart/waiting transitions, and notification equivalence.

055. P2 — Explain/GitOps analysis repeats small-array allocations and sort-key copies.
Evidence: src/explain.rs::container_statuses/conditions return new Vecs of
references for each query; pod checks call them repeatedly. Blocker sort clones
names per key extraction; warning-event sort formats times repeatedly then
keeps only six. src/gitops.rs repeatedly calls ready, which owns status/reason/
message strings even for boolean health checks, and repeats revision lookups.
Proposal: return borrowed slices/iterators, derive one pod/Flux summary per
object, compare borrowed names, and precompute time keys with stable top-six
selection. Keep stable tie order where current output depends on it.
Expected effect: cheaper evidence analysis with many pods/events.
Validate later: unchanged ranked findings, equal timestamps/names, missing
conditions, status precedence, and incomplete-evidence behavior.

056. P2 — GitOps source/dependency reads are serial and duplicate selected objects.
Evidence: src/app/gitops.rs::spawn_gitops clones the stored source and clones
again when selection is the owner. After owner resolution, source and every
dependsOn reference are fetched one by one. flux_kind_map is rebuilt each time.
Proposal: share immutable source objects, cache the resolved Flux kind map per
discovery revision, and fetch independent source/dependency references with
bounded concurrency and deduplication. Include this job in finding 044's
cancellation/request-generation scheme; explain refreshes need the same scheme.
Expected effect: faster chain inspection with many dependencies and less waste
from repeated refreshes. Preserve dependency order and warning selection.
Validate later: missing/duplicate/cross-namespace references, RBAC, refreshed
sources, and out-of-order completion.

057. P1 — Plugin output limits apply after unlimited capture, and timeout does not kill.
Evidence: src/app/input.rs::spawn_plugin awaits Command::output under timeout,
buffers up to eight ordered jobs, and collects all Output values before calling
bounded_lines. The 1 MiB/5000-line limits therefore bound display, not capture
or aggregate RSS. The command is not configured with kill_on_drop; dropping
the timed-out output future does not explicitly terminate/reap its child.
Proposal: stream stdout/stderr into bounded buffers while draining/discarding
excess, enforce both per-job and aggregate output limits, and explicitly kill
and reap on timeout. Process completed results promptly; preserve presentation
order with small indexed summaries instead of holding all raw output.
Expected effect: bounded memory/process lifetime for chatty/hung bulk plugins.
Validate later: endless output, both pipes full, timeout, child descendants,
one slow early job, failures, truncation indicators, and mutating-job semantics.

058. P2 — Actions deep-clone selected/marked objects just to derive targets.
Evidence: src/app/actions.rs::action_target_objects clones complete objects;
recreated_note uses those copies only to count ownership warnings. Marked
selection scans all rows and formats row keys. Other consumers need only
specific specs/patch inputs. src/app/details.rs::restore_selection similarly
materializes rows and formats every candidate identity to find one key.
Proposal: iterate canonical keys/borrowed objects and extract only required
owned fields; use ordered key lookup for selection restoration. Keep immutable
Arc snapshots only where asynchronous work actually needs the full object.
Expected effect: faster bulk-confirmation preparation and less transient RSS.
Validate later: marked versus filtered rows, removed objects, ownership warnings,
target order, selection restoration, and patches derived from the correct row.

059. P2 — Configured key bindings are reparsed during key dispatch.
Evidence: src/app/input.rs::try_plugin_key parses each plugin chord while
searching for a match. This happens on unhandled table keys; help also parses
these bindings repeatedly (finding 011).
Proposal: compile all plugin/bookmark/workspace chords during config load into
an ordered dispatch index, preserving built-in/bookmark/workspace/plugin
precedence and per-resource scopes. Compile guard patterns if they become hot.
Expected effect: less input-path allocation with many configured shortcuts.
Validate later: modifier/function-key normalization, duplicate precedence,
reserved keys, scope restrictions, and reload replacing old bindings.

060. P2 — Palette completion clones/resolves/sorts more candidates than it displays.
Evidence: src/app/input.rs::update_suggestions repeatedly resolves and formats
resource identities for catalog entries; suggest_namespaces clones the whole
namespace list. All completion paths allocate matching labels and sort before
keeping 100 entries, including empty-query lists whose source order is stable.
Proposal: precompute catalog identities/search masks, score borrowed candidates,
keep the best 100 with stable tie semantics, and own only retained labels.
Cache the empty-query browse list by config/discovery/RBAC revision.
Expected effect: lower per-keystroke cost with large namespace/CRD catalogs.
Validate later: exact aliases, qualified-name dedup, resource/command collisions,
bookmarks/workspaces, ties, and argument completion.
