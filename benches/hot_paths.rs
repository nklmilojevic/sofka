//! Baselines for the paths the optimization plan targets.
//!
//! Run with `cargo bench --features bench`.
//!
//! Each group isolates one optimized hot path so changes can be evaluated
//! independently:
//!
//! - `rows_cache`  -> 2.1 (watch event followed by the redraw query)
//! - `filter`      -> 3.1 (uncached cell extraction per keystroke)
//! - `cells`       -> 2.2 (`pod_summary` 3x, `helm::decode` 5x)
//! - `metadata`    -> 3.3 (typed field lookup vs whole-meta serialization)
//! - `log_filter`  -> 4.1 (O(n*m) substring scan)
//! - `log_wrap`    -> 2.3 / 4.2 (full-buffer re-measure per frame)
//! - `cell_extract` / `provider_selection` -> Tier 2/3 follow-up baselines

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use sofka::benchsupport as bs;
use sofka::columns;
use sofka::logfilter::LogMatcher;

/// 2.1 — one watch event followed by the redraw's row-count query. That pair
/// is the real steady-state unit: after 2.1 an ordinary unsorted/unfiltered
/// update keeps the existing key order, while paths that can change ordering
/// or membership are covered by the filter benchmarks below.
fn rows_cache(c: &mut Criterion) {
    let mut g = c.benchmark_group("rows_cache");
    for n in [500usize, 2_000] {
        let (mut app, _rx) = bs::pods_app(n);
        // Warm the caches so the first measured iteration isn't a cold build.
        black_box(app.row_count());
        g.bench_with_input(BenchmarkId::new("event_then_rebuild", n), &n, |b, &n| {
            let mut i = 0usize;
            b.iter(|| {
                bs::touch_one(&mut app, i % n);
                i += 1;
                black_box(app.row_count())
            });
        });
    }
    g.finish();
}

/// 3.1 — the same rebuild with a filter active. `no_match` is the expensive
/// case: every object misses on name, so `fuzzy_match_row` falls through to a
/// full uncached row extraction plus a fuzzy match per cell.
fn filter(c: &mut Criterion) {
    let mut g = c.benchmark_group("filter");
    let n = 2_000usize;
    for (label, pat) in [
        ("name_hit", "workload-00042"),
        ("no_match", "zzzznotpresent"),
        ("broad", "svc"),
    ] {
        let (mut app, _rx) = bs::pods_app(n);
        app.filter = pat.to_string();
        black_box(app.row_count());
        g.bench_with_input(BenchmarkId::new(label, n), &n, |b, &n| {
            let mut i = 0usize;
            b.iter(|| {
                bs::touch_one(&mut app, i % n);
                i += 1;
                black_box(app.row_count())
            });
        });
    }
    g.finish();
}

/// 2.2 — cell extraction per row. `pods` pays `pod_summary` three times;
/// `helm` pays base64 x2 + gunzip + a full JSON parse five times.
fn cells(c: &mut Criterion) {
    let mut g = c.benchmark_group("cells");

    let pods: Vec<_> = (0..256).map(bs::pod).collect();
    let pod_spec = columns::build_spec("pods", None, None, false);
    g.bench_function("pods_256", |b| {
        b.iter(|| {
            for o in &pods {
                black_box(pod_spec.cells(o));
            }
        });
    });

    // Helm is two orders of magnitude slower per row, so it gets far fewer.
    let helm: Vec<_> = (0..16).map(bs::helm_secret).collect();
    let helm_spec = columns::build_spec("helm", None, None, false);
    g.bench_function("helm_16", |b| {
        b.iter(|| {
            for o in &helm {
                black_box(helm_spec.cells(o));
            }
        });
    });

    g.finish();
}

/// 3.3 — a common user-column path through labels. Keep the former
/// serialize-all implementation alongside the production fast path as an
/// explicit baseline; both return the same owned `Value`.
fn metadata(c: &mut Criterion) {
    let mut g = c.benchmark_group("metadata");
    let pods: Vec<_> = (0..2_000).map(bs::pod).collect();
    let pointer = "/metadata/labels/app.kubernetes.io~1name";
    let rest = pointer.strip_prefix("/metadata").unwrap();

    g.bench_function("typed_labels_2000", |b| {
        b.iter(|| {
            for pod in &pods {
                black_box(sofka::views::extract(pod, pointer));
            }
        });
    });
    g.bench_function("serialized_baseline_2000", |b| {
        b.iter(|| {
            for pod in &pods {
                let meta = serde_json::to_value(&pod.metadata).unwrap();
                black_box(meta.pointer(rest).cloned());
            }
        });
    });
    g.finish();
}

/// 4.1 — the substring/regex scan, over a buffer the size the log view keeps.
fn log_filter(c: &mut Criterion) {
    let mut g = c.benchmark_group("log_filter");
    let lines = bs::log_lines(10_000);
    for (label, pat) in [
        ("empty", ""),
        ("substr_hit", "reconcile"),
        ("substr_miss", "zzzznotpresent"),
        ("substr_late", "duration"),
        ("regex", "/failed to sync [0-9]+/"),
        ("inverse", "!healthz"),
    ] {
        let m = LogMatcher::new(pat);
        g.bench_function(label, |b| {
            b.iter(|| {
                let mut hits = 0usize;
                for l in &lines {
                    if m.matches(l) {
                        hits += 1;
                    }
                }
                black_box(hits)
            });
        });
    }
    g.finish();
}

/// 2.3 / 4.2 — the per-frame height re-measure. `ascii` takes the fast path;
/// `wide` forces the per-char `unicode_width` walk on every tenth line.
fn log_wrap(c: &mut Criterion) {
    let mut g = c.benchmark_group("log_wrap");
    let ascii = bs::log_lines(10_000);
    let wide = bs::log_lines_wide(10_000);
    for (label, lines) in [("ascii", &ascii), ("wide", &wide)] {
        g.bench_function(label, |b| {
            b.iter(|| {
                let mut total = 0usize;
                for l in lines.iter() {
                    total += bs::wrapped_height(l, 120);
                }
                black_box(total)
            });
        });
    }
    g.finish();
}

/// 2.3 — what one *frame* of the log view costs, which is the number that
/// actually matters. `steady` is a redraw with no new lines (scrolling, a
/// cursor move, the 1 Hz tick); `streaming` is a redraw after a batch of new
/// lines arrives. Before the index these were both O(buffer).
fn log_viewport(c: &mut Criterion) {
    use sofka::app::LogsView;

    let mut g = c.benchmark_group("log_viewport");
    let lines = bs::log_lines(10_000);

    for (label, wrap_width) in [("nowrap", 0usize), ("wrap", 120)] {
        // Steady state: the buffer is unchanged between frames.
        let mut logs = LogsView::default();
        logs.view.lines.extend(lines.iter().cloned());
        logs.set_filter("reconcile".into());
        logs.refresh_index(wrap_width); // warm
        g.bench_function(BenchmarkId::new("steady", label), |b| {
            b.iter(|| black_box(logs.refresh_index(wrap_width).total_rows()));
        });

        // Streaming: 50 new lines per frame, the shape of a busy pod.
        let mut logs = LogsView::default();
        logs.view.lines.extend(lines.iter().cloned());
        logs.set_filter("reconcile".into());
        logs.refresh_index(wrap_width);
        let batch: Vec<String> = bs::log_lines(50);
        g.bench_function(BenchmarkId::new("streaming", label), |b| {
            b.iter(|| {
                logs.view.lines.extend(batch.iter().cloned());
                black_box(logs.refresh_index(wrap_width).total_rows())
            });
        });
    }
    g.finish();
}

/// 3.2 — the same rebuild with a *structured* comparison filter. Unlike the
/// fuzzy group above, every object pays `column_cell` (one column extracted
/// per object) plus the comparison itself, so this is where per-object
/// allocation in `eval_cmp` shows up.
fn filter_cmp(c: &mut Criterion) {
    let mut g = c.benchmark_group("filter_cmp");
    let n = 2_000usize;
    for (label, pat) in [
        ("str_curated", "status=Running"),
        ("str_ns", "namespace=ns-3"),
        ("num", "restarts>=5"),
    ] {
        let (mut app, _rx) = bs::pods_app(n);
        app.filter = pat.to_string();
        black_box(app.row_count());
        g.bench_with_input(BenchmarkId::new(label, n), &n, |b, &n| {
            let mut i = 0usize;
            b.iter(|| {
                bs::touch_one(&mut app, i % n);
                i += 1;
                black_box(app.row_count())
            });
        });
    }
    g.finish();
}

/// 3.4 — the aggregated Helm release list. Every rebuild dedups the store
/// down to the latest revision per release. `refilter` is the case a cache
/// can help: the order is stale but the store has not changed, so the dedup
/// result is still valid.
fn helm_rows(c: &mut Criterion) {
    let mut g = c.benchmark_group("helm_rows");
    for n in [300usize, 1_200] {
        let (app, _rx) = bs::helm_app(n);
        black_box(app.row_count());
        g.bench_with_input(BenchmarkId::new("refilter", n), &n, |b, _| {
            b.iter(|| {
                bs::invalidate(&app);
                black_box(app.row_count())
            });
        });
    }
    g.finish();
}

/// 2.7 — the context switcher's fleet column: one membership test per listed
/// context, on every frame the picker is open.
fn fleet_marks(c: &mut Criterion) {
    let mut g = c.benchmark_group("fleet_marks");
    for n in [32usize, 128] {
        let (app, _rx) = bs::contexts_app(n);
        g.bench_with_input(BenchmarkId::new("draw", n), &n, |b, _| {
            b.iter(|| black_box(bs::fleet_marks_for_all(&app)));
        });
    }
    g.finish();
}

/// 4.3.2A — where a Helm release decode actually spends its time. Swapping in
/// a SIMD JSON parser is only worth a dependency if the parse dominates.
/// `full` is `helm::decode` end to end; `parse_only` is a DOM parse of the
/// same bytes, which upper-bounds the parse share (the real code deserializes
/// into a typed struct, which is cheaper than building a `Value`).
fn helm_decode(c: &mut Criterion) {
    let mut g = c.benchmark_group("helm_decode");
    let secret = bs::helm_secret(1);
    let json = bs::helm_release_json(1);
    g.bench_function("full", |b| {
        b.iter(|| black_box(sofka::helm::decode(black_box(&secret))));
    });
    g.bench_function("parse_only", |b| {
        b.iter(|| {
            let v: serde_json::Value =
                serde_json::from_slice(black_box(&json)).expect("fixture json");
            black_box(v)
        });
    });
    g.bench_function("parse_typed", |b| {
        b.iter(|| black_box(sofka::helm::parse_release_json(black_box(&json))));
    });
    g.finish();
}

/// Tier 3 — targeted structured-filter extraction. The baseline forces the
/// borrowed JSON string into an owned `String`, matching the old `sget ->
/// String` contract; production consumes the `Cow::Borrowed` directly.
fn cell_extract(c: &mut Criterion) {
    let mut g = c.benchmark_group("cell_extract");
    let pods: Vec<_> = (0..2_000).map(bs::pod).collect();
    let spec = columns::build_spec("pods", None, None, true);

    g.bench_function("borrowed_ip_2000", |b| {
        b.iter(|| {
            for pod in &pods {
                black_box(spec.cell_at(black_box(pod), 4).unwrap());
            }
        });
    });
    g.bench_function("owned_ip_baseline_2000", |b| {
        b.iter(|| {
            for pod in &pods {
                black_box(spec.cell_at(black_box(pod), 4).unwrap().into_owned());
            }
        });
    });
    g.finish();
}

/// Tier 2 — provider autodiscovery. Both implementations use `min_by_key`;
/// the baseline first collects every candidate into a `Vec`, while production
/// feeds the filtered iterator directly into the minimum selection.
fn provider_selection(c: &mut Criterion) {
    let mut g = c.benchmark_group("provider_selection");
    let services = bs::services(256);

    g.bench_function("logs_streaming_256", |b| {
        b.iter(|| black_box(bs::pick_log_service(black_box(&services))));
    });
    g.bench_function("logs_collected_baseline_256", |b| {
        b.iter(|| black_box(bs::pick_log_service_collected(black_box(&services))));
    });
    g.bench_function("metrics_streaming_256", |b| {
        b.iter(|| black_box(bs::pick_metrics_service(black_box(&services))));
    });
    g.bench_function("metrics_collected_baseline_256", |b| {
        b.iter(|| black_box(bs::pick_metrics_service_collected(black_box(&services))));
    });
    g.finish();
}

criterion_group!(
    benches,
    rows_cache,
    filter,
    filter_cmp,
    helm_rows,
    fleet_marks,
    helm_decode,
    cells,
    metadata,
    log_filter,
    log_wrap,
    log_viewport,
    cell_extract,
    provider_selection
);
criterion_main!(benches);
