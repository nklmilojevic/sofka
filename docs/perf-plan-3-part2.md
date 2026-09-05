011. P2 — Picker/help filtering and full-list widget creation repeat on redraw.
Evidence: src/app/pickers.rs::filtered_namespaces/filtered_sort_entries clone,
score, sort, and allocate results; key handlers also recompute them for length
and selection. src/ui.rs::draw_namespaces/draw_contexts/draw_sort_picker/
draw_copy_picker build widgets for all entries, although popups show a subset.
draw_help rebuilds every binding (including parsing configured key chords),
concatenates/lowercases searchable text, and filters it each frame.
Proposal: cache ordered result indices by source/filter revision, prebuild the
help model/search text, and render a viewport slice of large pickers. Reuse
Substring where its case semantics match; keep favorites/recents invalidation.
Expected effect: faster typing/scrolling with many namespaces or contexts and
lower idle modal CPU. Avoid retaining duplicate full values in cached results.
Validate later: pinned entries, ranking ties, config reload, async list arrivals,
selection/scroll offsets, and Unicode help search.

012. P1 — A single huge log/document line is fully wrapped before viewport clipping.
Evidence: src/ui.rs::wrap_line returns a Vec for every wrapped row and copies
characters into owned span buffers. draw_logs and visible_wrapped_rows discard
off-screen wrapped rows only afterward. The source-window index limits source
lines, but does not limit work within one very long source line.
Proposal: stream wrapped segments with a start/end row bound; stop once the
viewport is filled. Cache wrap breakpoints for immutable long lines if deep
scrolling warrants it. Clip unwrapped long lines before styling where possible.
Expected effect: avoid large temporary allocations and long redraws on huge
JSON/log lines, even when only a few terminal rows are visible.
Validate later: huge lines, deep scroll, ANSI state, wide/combining glyphs, and
exact agreement with wrapped_height and scroll anchors.

013. P2 — Log severity/style parsing repeats for unchanged visible lines.
Evidence: src/ui.rs::render_log_line detects/strips ANSI for severity and then
parses ANSI again for body rendering. log_level_color allocates a lowercase
copy, json_field formats its two fixed search patterns, and fallback severity
detection scans for multiple literals separately on each redraw.
Proposal: cache theme-independent severity/prefix/ANSI run metadata per retained
line, or use borrowed case-insensitive scans and static field patterns. A single
multi-pattern scan can preserve the current leftmost marker precedence.
Expected effect: lower paused/steady viewport CPU and allocation rate.
Validate later: authoritative JSON level fields, ANSI-wrapped levels, source
prefixes, timestamps, leftmost precedence, and theme changes. Bound cache bytes.

014. P2 — Document styling and search produce repeated owned span copies.
Evidence: src/ui.rs::highlight_yaml builds String spans even from unmodified
source slices; draw_diff converts Cow to owned; highlight_matches creates
another span set. Static documents are reprocessed on every redraw.
Proposal: borrow spans where source lifetimes permit and cache semantic syntax
ranges and compiled search state by document/filter revision. Apply current
theme at draw time; share ANSI parsing with wrapping/search when possible.
Expected effect: less allocation and repeated text scanning while browsing
large YAML/diff documents. Restrict caches to retained/visible content.
Validate later: filtering, ANSI stripping, multiline/Unicode content, skin
changes, horizontal scrolling, and event documents changing asynchronously.

015. P1 — NAME cells redo fuzzy matching and allocate highlight containers.
Evidence: src/app/rows.rs::filter_match_indices runs fuzzy_indices per visible
name per draw. src/ui.rs::render_name_cell converts its result to a HashSet,
builds owned run strings, and even copies unfiltered names into Cell<'static>.
Proposal: borrow unfiltered names; cache highlight indices by row/filter
revision and walk sorted indices against UTF-8 boundaries without a HashSet.
Expected effect: fewer per-row allocations and less repeated fuzzy work.
Validate later: Unicode/combining characters, repeated matches, structured and
inverse filters, column-only matches, and name/resource-version changes.

016. P2 — Highlighting lowercases the needle and haystack per styled span.
Evidence: src/ui.rs::push_highlighted calls text.to_lowercase() and
needle.to_lowercase() for every span, then allocates strings for all runs.
Proposal: compile the search needle once; use borrowed ASCII match ranges on
the common path and an explicit Unicode offset mapping/fallback on the other.
Share compiled state with filtering where semantics agree. Borrow output spans.
Expected effect: fewer allocations for filtered logs, help, and documents.
Validate later: Unicode lowercase expansion and byte boundaries, multiple
matches, empty input, and the existing cross-span highlighting behavior.

017. P1 — Provider log framing copies bytes and builds a full JSON DOM per line.
Evidence: src/providers.rs::drain_lines scans the accumulated buffer, drains
complete bytes into another Vec, converts each line to String, and parse_entry
parses serde_json::Value before copying the four retained strings. LogEntry::lines
then formats another set of strings. Both transports use the DOM parser.
Proposal: frame complete borrowed slices with memchr, compact the partial tail
once per chunk, and selectively deserialize only configured fields. Allocate
only retained strings; write rendered lines directly into the outgoing batch.
Expected effect: lower CPU/allocator load proportional to provider log rate.
Validate later: fragmented chunks, escaped fields, multiline messages, invalid
UTF-8/JSON, missing fields, backfill/tail seam dedup, and rejected-record cost.

018. P1 — Provider response/record memory is not bounded by bytes.
Evidence: src/providers.rs::LogTail::next_entry grows buf until newline, and
drain_lines rescans an incomplete buffer each chunk (quadratic scanning for a
long fragmented record). Proxy Lines also accumulates a complete line.
fetch_text/fetch_query and tail HTTP-error handling collect entire bodies;
http_error truncates only after the complete body has been collected.
Proposal: track the framing scan offset and enforce configurable record/body
byte ceilings; parse backfill streams incrementally and cap error-body reads.
Expected effect: bounded memory and linear framing work for oversized input.
Validate later: huge newline-free streams, long valid records, oversized error
bodies, cancellation, and a clear user-visible truncated/rejected-data policy.

019. P2 — Provider autodiscovery can take several sequential API round trips.
Evidence: src/providers.rs::discover and discover_metrics await one service
list per ordered selector (five/four respectively). Their candidate helpers
also allocate a Vec solely to select its minimum.
Proposal: consider a bounded concurrent selector sweep that preserves selector
priority, or one suitable filtered service inventory; remove the intermediate
candidate Vec with an iterator minimum. Cache negative discovery briefly to
avoid repeating the full sweep on repeated opens, with explicit refresh.
Expected effect: faster first use and failed discovery on high-latency clusters.
Tradeoff: concurrent/full lists may increase API load or data transferred.
Validate later: priority/tie behavior, RBAC failures, absent services, refresh,
and request count as well as wall-clock latency.

020. P2 — Optional version metadata can still delay initial connection.
Evidence: src/k8s.rs::from_config uses tokio::join!(discover, version), where
version has a two-second timeout. The requests overlap, but join still waits
for the slower optional version request after discovery finishes.
Proposal: let the usable cluster/UI proceed once discovery succeeds and deliver
version metadata asynchronously, or use a shorter startup grace period.
Expected effect: faster startup/context switch behind slow /version proxies.
Validate later: immediate discovery with delayed/failed version, error handling,
generation changes, and updating the header after connection.
