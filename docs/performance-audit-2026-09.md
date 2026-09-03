# Performance optimization audit after PR #188

Date: 2026-09-04

This report audits the effective code after applying
[PR #188](https://github.com/nklmilojevic/sofka/pull/188) to `main` at
`4a22f8712e1a7f1e094f6147418d59053598dc49`. The PR was applied only to a
temporary copy for measurements. The production source was not changed.

The scope is interactive latency, throughput, memory, startup, and binary size
on the four release targets: Linux and macOS on x86-64 and AArch64. Measurements
in this report are from an Apple M1; target-specific proposals must be measured
on the other three native release runners before adoption.

## Executive summary

PRs #185 and #188 already removed the dominant costs: deep object cloning,
whole-buffer log work on every frame, repeated pod/Helm derivation, repeated
Helm deduplication, allocating structured comparisons, full pod re-lists for
node counts, and unbounded derived caches. The remaining work is smaller and is
mostly allocation/lifetime design rather than arithmetic.

The best next optimization is the table render path. A normal 40-row pod frame
currently makes about 200 avoidable `String` allocations before ratatui builds
its widgets: two independently formatted row keys, a resource-version clone on
every cell-cache hit, a separately formatted metrics key, and an owned copy of
every name. At the 16 ms frame ceiling this can reach roughly 12,000 avoidable
allocations per second during a busy watch.

The other high-value target is provider log ingestion. It copies each received
chunk into a second byte vector, allocates a `String` per line, parses every line
into a complete `serde_json::Value`, and then allocates the four retained
strings. That work is directly proportional to log rate and can be replaced by
in-place line framing plus a selective Serde visitor.

There is no justified handwritten assembly target. The important scans already
use `aho-corasick` and `memchr`, which dispatch to AVX2/SSE2 or NEON. `flate2`
already uses SIMD Adler-32. A temporary switch to the new `base64` SIMD engine
did not improve the representative Helm fixture on M1 and made the end-to-end
decode slower. Atomics are also not a broad opportunity: the store is
single-threaded, and the few shared cancellation/epoch flags are already
atomic.

## Measurement environment and baseline

- Host: Apple M1, macOS/Darwin 25.6, AArch64.
- Toolchain: Rust/Cargo 1.98.0, LLVM 22.1.8.
- Benchmark profile: optimized, thin LTO, debug symbols retained.
- Criterion command: `cargo bench --features bench --bench hot_paths -- --warm-up-time 1 --measurement-time 2 --sample-size 20 --noplot`.
- Live context: `docker-desktop`, Kubernetes 1.36.1, 771 pods, 15 namespaces,
  one node. All live operations were list/get/watch/metrics only.
- Correctness baseline: 524 tests passed and one was ignored with all features.

Criterion showed noticeable thermal/scheduler drift after long thin-LTO builds;
the table therefore reports estimates rather than implying false precision.
Changes below 5% need repeated alternating A/B runs.

| Path | Post-PR #188 estimate |
| --- | ---: |
| Row content update + count, 500 objects | 12.6 us |
| Row content update + count, 2,000 objects | 13.0 us |
| Fuzzy filter, name hit over 2,000 pods | 1.03 ms |
| Fuzzy filter, no match over 2,000 pods | 313 us |
| Fuzzy filter, broad match over 2,000 pods | 696 us |
| Structured status comparison, 2,000 pods | 1.19 ms |
| Structured namespace comparison, 2,000 pods | 76.3 us |
| Structured numeric comparison, 2,000 pods | 31.4 us |
| Helm refilter, 1,200 revisions | 12.6 us |
| Fleet membership marks, 128 contexts | 34.2 us |
| Full Helm release decode | 28.2 us |
| Typed Helm JSON parse alone | 16.0 us |
| Pod cells, 256 rows | 398 us |
| Helm cells, 16 rows | 456 us |
| Typed metadata lookup, 2,000 objects | 466 us |
| Former serialize-all metadata baseline | 2.72 ms |
| Log substring hit/miss, 10,000 lines | 406 / 205 us |
| Log regex, 10,000 lines | 357 us |
| Log wrap scan, ASCII/wide, 10,000 lines | 242 / 386 us |
| Steady cached log viewport | 4.9-5.3 ns |
| Append 50 lines and refresh viewport | 3.4-3.9 us |

Five warm live snapshots of 771 pods took 3.12-3.17 seconds (median 3.15 s).
One measured snapshot peaked at 37,240,832 bytes (35.5 MiB) RSS. Connection-only
`--check` took 0.64 s on the first run and 10-20 ms on the four immediately
following warm runs; this is dominated by external TLS/config/cache state and
is not a useful code-optimization baseline by itself.

The normal stripped release build was 14,751,984 bytes. `cargo bloat` reported
about 10.1 MiB of text in its symbol-preserving rebuild: sofka itself was 3.0
MiB, `std` 2.2 MiB, `kube_client` 995 KiB, `rustls` 489 KiB, and Tokio 217 KiB.
This makes feature trimming a binary-size task, not a likely runtime win.

## Ranked recommendations

| Rank | Candidate | Main effect | Confidence |
| ---: | --- | --- | --- |
| 1 | Carry canonical row keys through the render window and fix cache-hit ownership | Frame latency and allocations | High |
| 2 | Cache filter highlight indices and render borrowed name spans | Filtered-frame latency | High |
| 3 | Stream provider JSON lines without intermediate vectors or a full `Value` DOM | Log throughput and allocator pressure | High |
| 4 | Cache display headers, help, and picker results | Modal/frame latency | High |
| 5 | Move/coalesce state-file persistence off the UI thread | Input tail latency | High |
| 6 | Skip invisible-column and unnecessary timer work | Frame/idle CPU | High |
| 7 | Avoid deep `Value` clones in custom-column extraction | Custom-resource latency | Medium-high |
| 8 | Benchmark a faster hasher for internal object/cache maps | Filter/sort/watch throughput | Medium |
| 9 | Reduce retained metadata payloads, especially last-applied annotations | RSS | Workload-dependent |
| 10 | Rework Helm decode buffers/compression, not JSON SIMD first | Helm latency | Workload-dependent |
| 11 | Compact message and history key representations | Burst CPU and bounded memory | Medium-low |
| 12 | Allocator, PGO, and per-target build experiments | Broad runtime/startup | Platform-dependent |

## 1. Canonical row identity through the render path

Relevant code: `App::rows_window`, `App::ensure_table_cell_cache`,
`App::metrics_for`, `draw_table`, `render_name_cell`, and `Store::key`.

For each visible pod row, the current frame does all of the following:

1. `ensure_table_cell_cache` calls `row_key`, formatting `namespace/name`.
2. `cell_entry` clones `metadata.resource_version` before it knows whether the
   cache entry is stale.
3. `draw_table` calls `row_key` again for marks and the cell-cache lookup.
4. The metrics block formats the same `namespace/name` a third time.
5. With no filter, `render_name_cell` converts the borrowed name to an owned
   `String` solely because its return type is `Cell<'static>`.

Recommended design:

- Return a render-window item containing `&RowKey` and `&DynamicObject`, or
  provide an iterator over that pair, so the store's canonical `Rc<str>` key is
  carried through cache warming, marks, metrics, and rendering.
- Compare `resource_version.as_deref()` on cache hits and clone it only when
  inserting/replacing a `CellCacheEntry`.
- Key metrics and marked rows by the canonical row key where their namespace
  semantics match. Nodes can use their existing bare-name key without a new
  allocation.
- Make `render_name_cell<'a>` return `Cell<'a>` and borrow `name` in the
  unfiltered case.
- Cache `display_headers` when `ViewSpec`, namespace mode, or metric-column
  availability changes. Header cells can borrow the cached strings.

Add a render-focused Criterion benchmark and an allocation-count assertion for
an unchanged 40-row frame. Acceptance: at least 90% of the identified
pre-widget allocations disappear and rendered snapshots remain byte-identical.

## 2. Filter highlighting should reuse the filter pass

When a filter is active, `filter_match_indices` invokes `SkimMatcherV2` again
for every visible name on every redraw. `render_name_cell` then converts the
returned vector to a freshly allocated `HashSet<usize>` and constructs owned
strings for alternating highlighted runs.

Store the sorted match indices with the row/filter cache, keyed by row identity
and filter generation. Build them when the filter result is computed or lazily
once per visible row after a filter change. Render by walking the sorted indices
and borrowing UTF-8 slices from the original name; no `HashSet` or run strings
are needed. Keep the current matcher as the source of truth so fuzzy semantics
do not change.

Test ASCII, combining characters, multibyte characters, repeated characters,
inverse/structured filters, and a row update while a filter is active. Benchmark
steady redraws separately from the one-time filter change.

## 3. Provider log ingestion

Relevant code: `providers::drain_lines`, `providers::parse_entry`, and
`Tail::next_entry`.

The current SSE stream path performs avoidable layered ownership:

- `Vec::drain(..=last_nl).collect::<Vec<u8>>()` copies every complete byte and
  shifts any partial tail.
- `String::from_utf8_lossy(...).lines().map(str::to_string)` allocates each line.
- `serde_json::from_str::<Value>` builds a full object DOM.
- `_msg`, `_time`, pod, and container are copied again into the retained entry.

Replace this with a framing function that finds newlines using `memchr_iter`,
parses complete slices in place, and compacts the trailing partial line once per
network chunk. A custom Serde visitor should ignore all fields except the four
configured names and borrow string contents during parsing; allocate only the
fields retained by `LogEntry`. Do not introduce `Bytes` merely to move the same
copy elsewhere—the lifetime should end after the selected fields are built.

Benchmark 1 KiB, 64 KiB, and fragmented chunks; short and 4 KiB messages;
unknown extra fields; invalid UTF-8/JSON; missing `_msg`; and 10k/100k entries.
Report bytes allocated per accepted and rejected entry as well as throughput.

SIMD JSON is a second-stage experiment here. Short JSON lines are often
allocation- and branch-dominated, so compare `serde_json` selective visitation
against `simd-json`/`sonic-rs` only after removing the DOM and framing copies.

## 4. Cache modal and header-derived data

Several draw functions recompute results that change only on input/config:

- `filtered_namespaces`, `filtered_contexts`, `filtered_sort_entries`, and
  `filtered_copy_entries` allocate, fuzzy-match, and sort on every draw. Input
  handlers also call them repeatedly for length, selection, and lookup.
- The copy picker formats `"{header} {value}"` per entry per pass.
- `draw_help` rebuilds roughly 70 static binding lines and their strings every
  frame, then lowercases every line again while searching.
- `display_headers` clones the complete header vector for rendering and other
  consumers.

Cache each picker result against its source revision and filter string. Store a
prebuilt searchable help model; rebuild only when plugins, bookmarks,
workspaces, skin, or help filter change. Reuse the existing `Substring`
implementation for case-insensitive help search instead of allocating lowercase
copies. Keep cached semantic data independent of theme styles so a skin switch
remains correct.

These paths are not throughput-critical, but removing work from `draw` directly
improves the latency of the modal the user is interacting with.

## 5. Do not synchronously persist state inside input handlers

`SortMemory::save`, `NamespaceMemory::save`, and `FleetMarks::save` call
`create_dir_all`, TOML serialization, and `std::fs::write` synchronously from
keypress paths. Local writes are usually fast, but a slow, encrypted, or
network-mounted state directory blocks the UI thread and produces unbounded
tail latency.

Send immutable state snapshots to one background persistence worker. Coalesce
newer snapshots per destination and write through a temporary file plus rename
so a crash cannot leave truncated state. Surface the latest error through the
normal message channel. Flush the final pending snapshots during graceful exit
with a short bound.

This is a latency/correctness improvement, not a throughput benchmark. Test
coalescing, write failure, shutdown flush, and rapid sort/namespace toggles.

## 6. Invisible columns, timestamps, layout, and idle redraws

`draw_table` computes volatile values and cell widths for every column before
filtering horizontally scrolled-out columns. Apply visibility before volatile
formatting and width measurement. Keep status/ready values available when they
are needed for row coloring even if their cells are hidden.

Capture the current timestamp once per frame/rebuild and pass it to AGE,
UPDATED, LAST-SCHEDULE, and running-job duration calculations. The present code
calls `Timestamp::now()` independently for multiple visible rows and can even
straddle a second boundary within one frame.

The table also asks ratatui `Layout` to solve column geometry a second time for
mouse hit testing. Derive hit rectangles directly from the already resolved
fixed widths and spacing.

Finally, the one-second tick redraws every mode unconditionally after checking
port forwards. Redraw immediately only when a process was reaped, a visible
time-derived cell can change, or the active view contains another ticking
element. Schedule the next display boundary for old AGE values rather than
redrawing every second forever. Preserve one-second updates for objects younger
than a minute.

## 7. Custom columns: borrow the source, own only the final cell

`views::extract` returns an owned `serde_json::Value`. A body pointer therefore
clones the selected value, including an entire array/object, before
`render_cell` formats it. Metadata scalar handling is already much better than
the former serialize-all path, as the 466 us versus 2.72 ms benchmark shows.

Add an internal borrowed extraction result for `DynamicObject::data` and direct
metadata scalars/maps. Render from `&Value`; allocate only the final cached cell
string. Keep the existing owned public helper if callers/tests require it.
Sorting should parse borrowed scalar text/numbers directly and only own a text
sort key when it is inserted into the sort cache.

This matters most for CRDs whose printer/custom columns point at arrays or
objects. Benchmark scalar, escaped annotation key, 100-element array, and
nested-object columns.

## 8. Hashing and key layout experiments

The store, row caches, metrics, timeline, view cache, marks, and registries use
the standard randomized `HashMap`. These keys are dominated by Kubernetes
names, UIDs, and `namespace/name` strings. A faster hasher (`foldhash`, `ahash`,
or `rustc-hash`) may help full-store filter/sort passes and watch bursts.

Do not replace every map globally. Benchmark the store plus `RowsCache` first,
including hits, misses, insertion, deletion, a 2,000-row rebuild, and hostile
same-prefix names. Resource names are cluster-controlled input, so document the
hash-flooding tradeoff and retain randomized hashing for boundaries where an
untrusted API server could supply arbitrary keys. Keep a new hasher only if it
improves at least two real hot-path benchmarks by more than 5%.

Related low-risk key changes:

- Replace `PrevRevisions::get(&(kind.to_string(), key.to_string()))`, which
  allocates two strings for a lookup, with a nested map or a single shared key.
- Let `Timeline` borrow row identity until it records an actual transition;
  today `tkey` and `seen.insert(key.clone())` allocate on every observed event.
- Use shared row keys for `marked` and other view-scoped identity sets.

## 9. Retained Kubernetes payloads

The watch strips `managedFields`, but it retains
`kubectl.kubernetes.io/last-applied-configuration`, which can duplicate much of
an object inside one annotation. Every retained object can also be referenced
by the live store, view cache, and previous revisions through `Arc`.

Measure annotation bytes on real clusters before changing semantics. If they
are material, strip the annotation at the watch boundary and fetch the selected
object on demand for YAML/diff views, initially displaying cached content while
the GET completes. Cache the full selected object briefly so toggling detail
views does not repeat network traffic. Never silently omit the annotation from
user-visible YAML.

This can be a large RSS win in GitOps-heavy clusters and a regression in
offline responsiveness, so it remains conditional on measured retained bytes.

## 10. Helm decode buffers and SIMD

PR #188's `decode_summary` is the right first optimization. On the fixture, a
full decode is about 28.2 us and the typed JSON parse alone about 16.0 us. A SIMD
JSON replacement would therefore attack only part of an already cached,
resource-version-scoped operation while adding a dependency and mutable-buffer
requirements.

A temporary prototype enabled `base64` 0.23's `simd-unsafe` feature, installed
one runtime-dispatched `Simd` engine, and used it for both decode layers. The
best repeat produced roughly 32.8 us for full decode while unaffected parse
controls stayed near 16.9-17.0 us. It did not beat the scalar 28.2 us baseline;
the compressed fixture is small enough that dispatch/kernel overhead dominates.
Do not enable this globally from that result.

More promising experiments for unusually large, less-compressible releases:

- Benchmark scalar versus SIMD base64 by encoded size and select SIMD only
  above the measured crossover.
- Pre-size the decompression output from a safely capped gzip ISIZE hint, or a
  conservative compressed-size multiplier, to avoid vector growth.
- Compare miniz_oxide with zlib-ng on all four native targets, including build
  time, binary size, and packaging complexity.
- Reuse decode scratch buffers within one rebuild only if doing so does not
  retain a single pathological release's peak allocation.

Handwritten base64, gzip, or JSON assembly is not warranted; maintained kernels
have broader ISA coverage and much stronger correctness testing.

## 11. Message representation and backpressure

`size_of::<Msg>()` is 160 bytes on AArch64. `DynamicObject` is 440 bytes but is
already boxed in `Applied`. The event channel is bounded at 4,096 entries, so an
upper-bound payload-slot estimate is about 640 KiB before heap-owned variant
contents. This is not an RSS emergency, but moving 160-byte enum values through
the queue is unnecessary during large initial lists.

Measure `Msg` size and watch throughput after boxing coherent payload structs
for the largest variants. Do not box tiny, frequent variants individually
unless the enum shrinks materially. A target of 64 bytes or less is reasonable;
reject the change if watch throughput and peak RSS are flat.

The receiver already drains queued messages before drawing, and the channel is
bounded. Do not coalesce away applied events: timeline and notification logic
may need intermediate transitions. It is safe to coalesce derived redraw/cache
invalidation after every event has been observed, which the current main loop
largely already does.

## 12. Build, allocator, and native-code options

Evaluate these as independent A/B builds, never as one bundle:

- `mimalloc`, `snmalloc`, and the platform allocator, using startup, 2,000-pod
  RSS, provider-log throughput, and allocation-heavy table redraws. Allocators
  can trade lower CPU for higher retained RSS, so require both measurements.
- LLVM instrumentation PGO using a corpus containing startup, a large pod
  watch, filters, table rendering, logs, Helm, and command pickers. PGO is more
  plausible than handwritten assembly because sofka has many branch-heavy
  generic paths.
- `panic = "abort"` for release binary size only. It is not a credible hot-path
  speed optimization, and the benchmark profile cannot demonstrate it.
- Keep thin LTO. `codegen-units = 1` was already rejected by the earlier
  optimization work because cold release build time matters and no sufficient
  runtime win was shown.
- Trim direct Tokio features from `full` only after checking Cargo feature
  unification. Sofka uses runtime, macros, sync, time, process, fs, I/O, and
  networking across production/tests, while kube enables many of the same
  features. Expected runtime gain is zero and binary-size gain is likely small.
- Avoid `target-cpu=native` for public artifacts. If a second optimized x86-64
  artifact is desired, publish it explicitly with an ISA baseline in its name;
  keep runtime dispatch in general binaries.
- BOLT is a conditional Linux x86-64 packaging experiment. It does not cover
  both macOS architectures and should follow PGO, not replace it.

Cargo currently carries base64 0.22 through kube and base64 0.23 directly.
That duplication is controlled by upstream version convergence; forcing one
version locally is not worth a fork. Proc-macro duplicate versions do not enter
the runtime binary.

## Atomics, SIMD, assembly, and parallelism conclusions

`streaming_lists` and the theme epoch use Acquire/Release correctly. Theme
background is a relaxed flag. The generation and log cancellation flags use
`SeqCst`, although they transmit no data and could use relaxed loads/stores if
their invariant remains only “is this generation current?”. That change is
semantically valid but too small to prioritize: the loads occur around network
and channel work, not in a tight compute kernel. If changed, add Loom-style or
stress tests and measure a burst benchmark; do not claim a win from instruction
count alone.

Do not make `Store::version` atomic. `Store` contains `Rc<str>`, is deliberately
`!Send`, and all mutations occur on the UI task. An atomic would add cost and
suggest thread-safety that the type does not have.

The remaining byte scans are either already vectorized by maintained crates or
operate on short labels/cells where setup costs dominate. FNV source-color
hashing and the fuzzy presence mask are loop-carried reductions over short
strings; inline assembly is particularly unsuitable. TLS cryptography is
already handled by ring's platform code.

Parallel row filtering/sorting is also rejected for now. The store is
single-threaded, the common 2,000-row operations are sub-millisecond to about
1.2 ms, and moving objects into a parallel representation would add
synchronization and complicate `Rc`-based caches. Reconsider only with a
measured 10k-100k-object workload that misses the interactive budget.

## Explicitly rejected or already solved

- Repeating PR #188's filter folding, `Cow<str>` namespace comparison, Helm
  summary decode, Helm dedup cache, row-cache shrink, timeline bound, fleet
  membership, unstable-sort, or node pod-watch work.
- SIMD JSON for Helm list rows without new evidence; PR #188 measured and
  rejected it, and the post-merge parse share confirms the decision.
- A blanket `CellFn -> Cow<str>` conversion. Cached cells must outlive a borrow
  of the current object and remain valid when the object is replaced, so they
  ultimately need owned text. Borrow at the final frame boundary instead; use
  `Cow` only in uncached/on-demand paths where the lifetime is real.
- Replacing every `String`, `clone`, or `format!` found by static search. The
  post-PR tree has 641 clone sites, 369 `to_string` sites, and 829 `format!`
  sites, but most are cold actions, error construction, tests, or ownership
  transfers into async tasks.
- Removing the bounded channel or using lock-free structures. Tokio's channel
  is not the measured bottleneck, and backpressure is valuable during lists.
- Fat LTO, universal `target-cpu=native`, handwritten crypto/compression, and
  atomics in single-threaded state.
- Optimizing on-demand bundle/YAML/diff serialization before it appears in a
  user-visible profile. It is off the redraw path and mostly off-thread.

## Recommended implementation order and gates

1. Add table-render, filtered-highlight, picker, and provider-ingest benchmarks
   plus allocation counters. Benchmarks are prerequisites, not optional
   follow-up work.
2. Land canonical row identity, resource-version borrow-on-hit, borrowed name
   rendering, and cached highlight indices as one render-focused series.
3. Land provider framing and selective deserialization separately.
4. Cache header/help/picker models, then remove invisible-column and timer work.
5. Move persistence to one coalescing worker.
6. Run the custom-column borrow, hasher, retained-annotation, message-layout,
   Helm backend, allocator, and PGO experiments independently. Keep only
   results that clear the gates below.

For timing, require two alternating A/B runs, a statistically significant
Criterion result, and at least 5% improvement unless the change eliminates a
known latency hazard. For allocation work, require an allocation-count drop and
no RSS regression. For build options, validate every native release target and
record binary size plus cold build time. Every production change must pass
`cargo fmt --check`, `cargo clippy --all-targets --all-features`,
`cargo test --all-features`, headless snapshots, and targeted Unicode/corrupt
input tests.
