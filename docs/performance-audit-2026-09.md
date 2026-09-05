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

## Validation, 2026-09-05

The plan above was written from reading the code and from the baseline table.
`benches/plan_validation.rs` now measures each candidate as an A/B pair inside
one binary, so both arms of a pair see identical codegen and run adjacent in
time.

This section reports **two runs**, as the gate above requires. The second run
is what makes it useful, and not in the way the first run suggested.

### Read the ratios, not the times

Run 2 came back thermally degraded: 54 of 75 benchmarks were slower than run 1,
many by more than 100%, including arms of pairs that had not changed at all.
The absolute numbers from run 2 are therefore worthless, and by extension the
absolute numbers from run 1 are unverified.

What survives is the ratio *within* each pair, because both arms are measured
adjacently under the same conditions. Everything below is stated as a ratio and
labelled by how well that ratio reproduced:

- **stable** — ratios within 25% of each other across runs.
- **noisy** — 25-50% apart, direction consistent.
- **unstable** — more than 50% apart, direction consistent. The effect is real,
  the size is not yet known.
- **reversed** — the two runs disagree about which arm is faster. No claim.

Environment: `main` at v0.22.0, Apple M1, Criterion `--warm-up-time 1
--measurement-time 2 --sample-size 20`. Absolute figures are not comparable to
the baseline table earlier in this document, which was measured on the PR #188
tree. Wire fixtures are live captures from the audit cluster: 771 pods as a
6.2 MiB `PodList`, a 3.7 MiB `PartialObjectMetadataList`, and a 4.0 MiB `Table`.

### Confirmed: stable across both runs

| Item | Prototype gain, run 1 | run 2 |
| --- | ---: | ---: |
| 1. Canonical row identity, 40 rows | 20.3x | 17.6x |
| 2. Borrowed highlight indices, 40 rows | 8.1x | 9.3x |
| 3. In-place framing, 1 KiB chunks | 2.29x | 2.26x |
| 3. In-place framing, 64 KiB chunks | 2.26x | 2.50x |
| 3. Selective parse, 10,000 x 32 B | 2.52x | 3.00x |
| 4. Cached picker and header models | free | free |
| 6. One timestamp per frame | 146x | 170x |
| 10. Gzip output pre-sized from ISIZE | 1.13x | 1.97x |

Items 1 and 2 are the render series and remain the strongest proposal: both
reproduce, both are large, and neither depends on a magnitude the noise can
reach. Item 6's ratio is enormous but its absolute saving is about 4 us per
frame, roughly 1.4% of a frame; it stays justified as the fix for formatting
timestamps that can straddle a second boundary mid-frame, not as a speed win.

Item 4's cached arms measure in picoseconds against hundreds of microseconds.
That is a "the work is gone" result, not a speedup to quote.

### Refuted: stable in the opposite direction

| Item | run 1 | run 2 |
| --- | ---: | ---: |
| 11. Boxing large `Msg` variants | 2.4x slower | 2.6x slower |
| Replace ANSI state machine with `vte` | 2.7x slower | 2.3x slower |
| Replace ANSI state machine with `ansitok` | 15.8x slower | 12.2x slower |
| Parallel filter, 2,000 rows | 8.6x slower | 3.6x slower |
| Parallel filter, 20,000 rows | 1.4x slower | 2.4x slower |

Boxing is the clearest reversal of the plan, and it reproduces. `size_of::<Msg>()`
is 168 bytes, not the 160 recorded earlier, but pushing 4,096 boxed messages
costs 2.4-2.6x the inline enum in both runs: the per-message allocation and
pointer chase dominate the copy that boxing was meant to avoid. **Item 11's
boxing proposal should be struck from the plan, not deferred.**

The hand-written ANSI state machine beats both maintained parsers by a wide
margin in both runs. It should not be replaced.

### Reversed: no claim can be made

| Pair | run 1 | run 2 |
| --- | ---: | ---: |
| 7. Borrowed extraction, nested object column | 1.44x faster | 1.01x slower |
| `simd-json` on `PartialObjectMetadataList` | 1.32x faster | 1.32x slower |
| Parallel filter, 100,000 rows | 1.20x faster | 1.80x slower |

The 100,000-row result matters most, because it was the *only* data point
anywhere in favour of parallel filtering, and it does not reproduce. Item 8's
rejection of parallelism now has no counter-example at any size. Treat it as
settled rather than "reconsider at 10k-100k".

The nested-object extraction reversal does not sink item 7 — the array column
still favours borrowing in both runs — but the object case must be re-measured
before it appears in a proposal.

### Real but unsized: direction holds, magnitude does not

| Pair | run 1 | run 2 |
| --- | ---: | ---: |
| 3. Selective parse vs DOM, 1,000 x 32 B | 2.80x | 1.79x |
| 3. Selective parse vs DOM, 1,000 x 4 KiB | 1.87x | 1.14x |
| 3. In-place framing, 17-byte fragments | 1.60x | 1.11x |
| 7. Borrowed extraction, scalar column | 1.10x | 2.15x |
| 7. Borrowed extraction, 100-element array | 1.83x | 1.44x |
| 8. `foldhash` vs std hasher, 20,000 keys | 2.66x | 3.93x |
| 8. `ahash` vs std hasher, 20,000 keys | 2.21x | 4.12x |
| `nucleo-matcher` vs `SkimMatcherV2`, 20,000 names | 3.33x | 2.03x |
| `simd-json` on the 4.0 MiB `Table` | 1.41x | 2.67x |
| Relaxed vs SeqCst atomic loads | 2.20x | 1.57x |

Every row here favours the prototype in both runs, so the direction is safe to
act on; the size is not safe to quote. Item 3's framing and selective parse both
stay well above the 5% gate at their worst observed ratio, which is what the gate
actually asks. The relaxed-atomics row is direction-stable and irrelevant: it
saves about 0.4 ns per load.

### Corrections to this document

**SIMD JSON must be split by payload size, not rejected outright.** The executive
summary rejects it wholesale. That is correct for provider log lines and wrong for
the initial list decode:

- Log lines: the selective Serde visitor beats `simd-json` in both runs (2.02x
  and 1.24x at 32 B, 2.02x and 2.63x at 4 KiB). At 4 KiB messages `simd-json` is
  slower than the plain DOM parse it was meant to replace. Item 3's visitor is the
  answer here and `simd-json` should not follow it.
- The 6.2 MiB `PodList`: `simd-json` is **1.64x and 1.60x** faster — the most
  stable non-trivial result in the whole suite. This is a startup-latency item the
  audit missed entirely, because it only ever considered SIMD JSON for Helm and
  logs. It needs evaluation against the real typed `DynamicObject` path, which is
  not a `Value` DOM parse, before it becomes a proposal. The metadata and table
  fixtures do not reproduce and should not be cited.

**The base64 SIMD claim is stated wrong.** The summary says the SIMD engine "did
not improve the representative Helm fixture on M1 and made the end-to-end decode
slower". In isolation the SIMD engine wins at every size in both runs, including
the 588-byte fixture (1.62x, 1.96x) and 16 KiB (1.98x, 1.85x). Both observations
can hold: base64 is a small share of a 28.2 us decode whose JSON parse alone is
16.0 us, so a ~70 ns saving vanishes into run-to-run drift. The conclusion — do
not enable it globally — is unchanged. The stated reason should be "base64 is not
the bottleneck", not "SIMD base64 is slower".

### New candidates this run surfaced

| Candidate | run 1 | run 2 | Status |
| --- | ---: | ---: | --- |
| `nucleo-matcher` for fuzzy filtering | 3.33x | 2.03x | unstable, always >2x |
| `CompactString` for row keys | 3.65x | 4.17x | stable |
| `foldhash` for internal maps | 2.66x | 3.93x | unstable, always >2.6x |
| Struct-of-arrays row scan, 100,000 | 1.57x | 1.53x | stable |
| `lasso` interning for row keys | 1.11x | 1.21x | stable, marginal |

The fuzzy matcher is the largest unclaimed win and appears nowhere in the ranked
list. `SkimMatcherV2` is on the filter path and, through `filter_match_indices`
(`src/app/rows.rs:256`), on the highlight path that item 2 addresses. Item 2
assumes the current matcher stays as the source of truth, so a matcher swap is a
separate proposal with its own correctness surface: match semantics, scoring
order, Unicode and combining characters, and the existing filter tests.

`foldhash` and `ahash` both clear 2x in both runs, but this does **not** satisfy
item 8's gate, which requires two *real* hot-path benchmarks over 5%. This is a
synthetic map workload; the store and `RowsCache` benchmarks item 8 asks for
still need writing. The hash-flooding tradeoff in item 8 also still applies:
these keys are cluster-controlled.

`CompactString` is the surviving half of item 11 now that boxing is refuted. It
is 24 bytes, identical to `String`, so it shrinks nothing in `Msg`; the win is
removing the allocation for names under the inline threshold, which is most
Kubernetes names.

### What this run says about the method

Two runs was the right gate and it earned its keep: it removed three claims that
one run would have shipped, including the only evidence that ever favoured
parallel filtering. It also showed that this host cannot produce trustworthy
absolute timings back-to-back — the second run of anything is measured on a hot
machine.

Before any of the unstable rows is quoted as a number in a pull request, it needs
a third run from cold, ideally with the pair order shuffled so that position in
the suite is not confounded with thermal state. The stable rows do not need this;
their direction and rough size reproduced under deliberately worse conditions.

### Not yet measured

Item 12 needs separate builds per allocator; `examples/allocator_probe.rs` is
registered for that and has not been run. PGO, `panic = "abort"`, and binary size
remain unmeasured. Item 9, retained annotation bytes, needs a live cluster survey
rather than a benchmark.
