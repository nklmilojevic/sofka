081. P1 — App benchmark fixtures do not install the actual resource column spec.
Evidence: benchsupport.rs::pods_app/helm_app set kind_plural and seed objects,
but leave App.kind as None and never call refresh_view_spec. App::new builds
spec with an empty plural; rows.rs uses that stored spec for cached cells,
comparison filters, headers, and drawing. Thus App-based pod/Helm benchmarks
and render probes do not exercise the same layout as real navigation. The
standalone cells benchmark does explicitly build the correct specs.
Proposal: provide a side-effect-free fixture initializer that installs the
resolved Kind and actual ViewSpec before seeding, without starting watches.
Assert headers, kind identity, and a known comparison-filter result in fixtures.
Expected effect: representative performance evidence for filters/rendering/
allocation decisions; existing App-based measurements need revalidation.
Validate later: fixture/real-navigation parity, offline operation, configured
and wide columns, metrics columns, and nonzero expected filtered result counts.

082. P1 — Helm benchmark cardinality collapses to 120 distinct storage keys.
Evidence: benchsupport.rs::helm_secret(i) uses release=i%60, namespace=i%24,
and revision=i%5+1. This identity repeats every 120 inputs, and revision is
fixed for a given release name. Store overwrites repeated keys, so helm_app(300)
and helm_app(1200) both retain only 120 objects and do not model multi-revision
histories. Those are the sizes used by hot_paths.rs::helm_rows.
Proposal: generate independent release, namespace, and increasing revision
identities; assert both requested stored-object count and expected latest-row
count. Include skewed history lengths and realistic incompressible manifests.
Expected effect: accurate scaling/memory evidence for Helm dedup/decoding/cache
choices, including 077, instead of differently labeled equal-size workloads.
Validate later: uniqueness, revision variety, dedup result, and bytes per payload.

083. P2 — Watch-update benchmarks replay identical objects and time fixture creation.
Evidence: benchsupport.rs::touch_one rebuilds pod(i) inside the timed iteration;
pod(i) has the same resourceVersion and contents each time. Consequently the
fixture's JSON construction/deserialization is included, while genuine changed-
revision behavior such as prev_revisions insertion and stale cell recomputation
is absent. This helper drives rows_cache/filter/filter_cmp in hot_paths.rs.
Proposal: prepare inputs outside isolated apply/render timing, increment RV and
change a relevant field for true updates, and name no-op replay as a separate
case. Keep a distinct whole-ingestion benchmark if fixture/parse work is wanted.
Expected effect: separates transport, store, timeline, and invalidation costs;
avoids choosing optimizations from a workload that misses their affected path.
Validate later: assert the intended cache hit/miss and membership/order change,
and add burst, relist, delete/recreate, marked-row, and metrics-sort scenarios.

084. P2 — Prototype timings need equivalence and reproducibility gates.
Evidence: plan_validation.rs compares cached-reference access with whole model
rebuilds (excluding invalidation), VecDeque messages instead of Tokio mpsc,
and ANSI byte counters with vte character callbacks. Provider DOM parsing
accepts non-string optional pod/container fields as empty, whereas the typed
prototype rejects them. wire_json silently skips absent /tmp fixtures, and
allocator_probe requires one such external fixture. None establishes complete
production-path equivalence merely by returning a checksum.
Proposal: retain these as explicitly scoped micro-cost probes, add differential
output/edge-case checks, recorded sanitized fixture hashes and sizes, and a
representative end-to-end suite with invalidation, retention, real channel
contention, and terminal-output accounting. Report missing fixtures visibly.
Expected effect: avoids adopting an optimization based on mismatched semantics
or unrepresentative synthetic wins. This is an evidence-quality finding.
Validate later: identical logical results where equivalence is intended,
Unicode/ANSI/JSON edge cases, deterministic datasets, and per-target results.

085. P1 — Full resource bodies and last-applied annotations dominate retained data options.
Evidence: k8s.rs clears managedFields on watched objects but otherwise stores
full DynamicObjects. Store, view cache, and previous revisions retain these
through Arc. details.rs explicitly reads last-applied-configuration, whose
serialized object text remains alongside the live body when present. Many
summary views use only a small subset of the retained fields.
Proposal: first account for retained bytes by field/resource. If duplication is
material, offer a summary/lazy-detail representation or separately byte-cache
large annotations, fetching the full selected object on demand when necessary.
Consider server Table/metadata responses only for views whose requirements
they actually satisfy; custom columns, filtering, timeline, snapshots, and
offline detail/diff behavior need an explicit full-object fallback.
Expected effect: potentially much greater heap/network reduction than changing
allocators, especially for annotation-heavy objects. Benefit depends on real
payloads; managedFields removal is already implemented, not a new proposal.
Validate later: annotation prevalence/size, full-versus-summary output parity,
RBAC/loading/error behavior, revision-consistent diffs, and request budgets.
