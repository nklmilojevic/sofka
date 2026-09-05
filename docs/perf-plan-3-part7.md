061. P1 — Log/document copy/save duplicates the entire text before background I/O.
Evidence: src/app/actions.rs::filtered_log_text and doc_text clone every line
into a temporary Vec, then join into another full String on the UI thread.
copy_logs/copy_doc rescan the joined text for a line count. save_logs only moves
the subsequent write off-thread; it reruns matching instead of reusing LogIndex.
Proposal: append borrowed selected lines into one reserved buffer with a count,
reuse a valid filter index, and capture immutable shared lines for background
streaming exports/clipboard preparation on large buffers.
Expected effect: lower copy/save latency and peak memory for paused logs.
Validate later: filter semantics, concurrent appends/trims, separators, empty
documents, snapshot-at-invocation behavior, and clipboard fallback.

062. P2 — Independent bulk API actions execute one target at a time.
Evidence: src/app/actions.rs::spawn_patch_action/do_delete/do_argocd_suspend,
manual Job creation, and Helm uninstall iterate targets and await each result
before starting the next. Total latency accumulates per-target round trips.
Proposal: evaluate a small configurable concurrency limit for independent
targets, reuse Api objects per namespace, and aggregate per-target outcomes.
Do not parallelize drain/eviction or dependent Helm operations by default;
preserve PDBs, confirmation boundaries, action ordering requirements, and API
rate limits. This changes operational behavior and needs deliberate review.
Expected effect: faster large bulk operations on high-latency clusters.
Validate later: partial failures, throttling, ordering requirements, journal/
status output, and safe completion after navigating away.

063. P2 — Config resolution clones/merges full TOML trees for narrow queries.
Evidence: src/config.rs::validate parses typed Config and then the raw TOML;
resolve clones the base tree and each overlay, merges an extra complete overlay
tree solely to read skin.name, then deserializes Config. Callers resolve again
for the base skin or only a fleet readonly flag. reload_config/open_config_info
also read/parse files synchronously on the UI thread.
Proposal: retain validated parsed/typed state, track just skin override presence
while merging, cache resolutions per config revision, and read/validate config
in a worker with atomic application of the completed result.
Expected effect: less reload/context/fleet setup work for large configurations
and responsive input on slow config storage.
Validate later: merge precedence, arrays replacing tables/scalars, precise error
messages, invalid-override fallback, missing files, and readonly/skin semantics.

064. P2 — Bookmark/workspace navigation can start and immediately replace watches.
Evidence: src/app/bookmarks.rs::apply_bookmark_local first calls switch_kind_ns
(starts a watch), then applies filter selectors (may call start_watch again),
then opens Pulse/Xray (bumps generation and starts a separate gather). Workspace
views reuse this path. Rapid navigation can repeat setup and pending requests.
Proposal: resolve the complete destination state (kind, namespace, selectors,
sort, mode) before committing one lifecycle transition and starting needed I/O.
Expected effect: fewer watch/list/metrics/RBAC starts and less cache churn.
Validate later: server-side bookmark filters, async context bookmarks, invalid
resources, workspace cycling, dashboard destinations, and history behavior.

065. P2 — Threshold/view lookup constructs normalized key vectors repeatedly.
Evidence: src/thresholds.rs::Compiled::resolve allocates lowercased/formatted
candidate keys whenever per-resource thresholds exist; draw_table calls it
each frame. src/views.rs::lookup_keys does the same for view/drill/node lookup.
Proposal: resolve thresholds and view settings once per kind/config revision,
or precompute resource lookup keys with discovery. Retain the current direct
default fast path when there are no overrides.
Expected effect: eliminate fixed per-frame allocations under custom thresholds.
Validate later: apiVersion/group/plural/kind precedence, synthetic views,
config/context changes, and dynamic printer columns.

066. P3 — Repeated theme accessor calls still check the palette epoch individually.
Evidence: src/theme.rs already caches Palette per thread, eliminating ordinary
RwLock reads, but each accessor still calls palette and loads EPOCH; ui.rs calls
many swatch/style accessors per row/span. source_color already demonstrates a
single theme::snapshot for related colors.
Proposal: pass a frame-local Palette/style set into inner rendering helpers
where profiling shows repeated accessor cost. Keep theme updates atomic between
frames; do not weaken synchronization merely to save an atomic load.
Expected effect: a possible small render improvement; generated code may already
remove copies, so this is lower priority and needs a render profile.
Validate later: skin/background changes and cross-thread palette users.

067. P3 — Short text/filter helpers do avoidable extra scans or copies.
Evidence: src/text.rs::ellipsize counts the entire string before taking a short
prefix; notification/error callers often request only 60–300 characters.
src/app/input.rs::key_log_filter clones the filter to test edit_chord and again
for ordinary character edits; LogsView::set_filter recompiles unconditionally.
src/filter.rs parses structured input with a detection pass plus token Vec,
and parse_duration accumulates digits in a temporary String.
Proposal: bounded char-index truncation (borrow unchanged text where useful),
mutate the filter once then recompile only on actual change, and use iterator/
numeric accumulation if filter parsing is measured to matter.
Expected effect: small allocation/scan reduction; parsing is already cached and
short filters make most of this cold-path work. Do not overengineer it.
Validate later: Unicode truncation, unchanged editing keys, regex error state,
structured syntax, and checked duration overflow.

068. P1 — Large non-table views still build widgets for every row on every frame.
Evidence: src/ui.rs::draw_xray/draw_fleet/draw_findings/draw_snapshots iterate
entire data sets and clone/format all items before List clips them. draw_palette
builds up to 100 entries for a 12-row popup. The table and document renderers
already use viewport windows, but these paths do not.
Proposal: apply viewport slicing with local selection/offset translation to
these lists, prioritizing Xray and large findings/snapshot sets. Cache immutable
labels and timeline timestamp formatting where it helps.
Expected effect: rendering scales with screen height rather than cluster tree
size or saved-file count. Small bounded lists can stay simple.
Validate later: list scrolling/highlight offsets, jumps, async item replacement,
empty views, narrow terminals, and full-list selection semantics.

069. P2 — Derived work and redraws continue for data hidden beneath another mode.
Evidence: src/app/logs.rs::launch_logs deliberately keeps the underlying table/
Xray tasks alive; src/main.rs marks the frame dirty after any received message,
including stale-generation messages that handle_msg ignores. ui::draw draws
the full table beneath modal popups even where the popup obscures most of it.
Proposal: keep live state updates for instant return, but track visible dirty
regions/sources so hidden or ignored updates do not redraw the active view.
Defer expensive hidden-view derivations; optionally reduce hidden poll cadence
with an explicit freshness policy. Reuse a background render for stable popups.
Expected effect: less CPU/terminal work while logs/documents/modals are active.
Validate later: returning with fresh state, status/notification updates, header
counts, generation drops, theme/resize changes, and popup occlusion.

070. P2 — Nix package inputs include files unrelated to the shipped binary.
Evidence: package.nix uses lib.cleanSource ./. without a build-specific file
filter. The source tree includes docs/demo.gif, docs/sophie.png, prose, benches,
examples, and workflow files; cleaning VCS/editor files is not a Rust-input
allowlist. These files participate in the package source derivation.
Proposal: use a maintained source filter/fileset for actual package inputs,
including manifests, lockfile, Rust sources, licenses, and any required assets.
Keep flake/check inputs separate; do not blindly exclude files Cargo packages.
Expected effect: fewer package rebuilds/cache misses after documentation or
tooling-only changes, and less source-copy/hash work. Runtime is unaffected.
Validate later: Nix build/package contents on all supported platforms, changes
to each real build input invalidating the build, and docs-only cache reuse.
