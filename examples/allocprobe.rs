//! Allocation counts for the render and rebuild paths.
//!
//! The wall-clock frame benchmark is dominated by the terminal buffer write
//! and swings ±25% run to run on a laptop, which is far too coarse to resolve
//! a change in per-row work. Allocation counts are deterministic, so they can.
//!
//! `cargo run --release --example allocprobe --features bench`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use sofka::benchsupport as bs;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size(), Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(new, Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` `iters` times and report allocations per iteration. One warm-up
/// iteration first, so lazily-built caches are not charged to the steady state.
fn measure(label: &str, iters: usize, mut f: impl FnMut()) {
    f();
    let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    for _ in 0..iters {
        f();
    }
    let allocs = ALLOCS.load(Relaxed) - a0;
    let bytes = BYTES.load(Relaxed) - b0;
    println!(
        "{label:<34} {:>9.1} allocs/iter {:>12.0} bytes/iter",
        allocs as f64 / iters as f64,
        bytes as f64 / iters as f64,
    );
}

fn main() {
    let iters = 200;

    // One redraw of a 47-row viewport over a 500-row store.
    {
        let (mut app, _rx) = bs::pods_app_with_metrics(500);
        app.table_state.select(Some(0));
        let mut term = bs::terminal(200, 50);
        measure("frame/table/500", iters, || {
            bs::render_frame(&mut term, &mut app)
        });
    }

    // The same with three columns scrolled off the left edge.
    {
        let (mut app, _rx) = bs::pods_app_with_metrics(2_000);
        app.table_state.select(Some(0));
        app.col_offset = 3;
        let mut term = bs::terminal(200, 50);
        measure("frame/table_scrolled/2000", iters, || {
            bs::render_frame(&mut term, &mut app)
        });
    }

    // A watch event followed by the redraw it caused.
    {
        let (mut app, _rx) = bs::pods_app_with_metrics(2_000);
        app.table_state.select(Some(0));
        let mut term = bs::terminal(200, 50);
        let mut i = 0usize;
        measure("frame/event_then_frame/2000", iters, || {
            bs::touch_one(&mut app, i % 2_000);
            i += 1;
            bs::render_frame(&mut term, &mut app);
        });
    }

    // Rebuilds: one watch event, then the row query the redraw makes.
    for (label, pat) in [
        ("rebuild/unfiltered/2000", ""),
        ("rebuild/fuzzy_hit/2000", "workload-00042"),
        ("rebuild/fuzzy_miss/2000", "zzzznotpresent"),
        ("rebuild/cmp_status/2000", "status=Running"),
        ("rebuild/cmp_restarts/2000", "restarts>=5"),
    ] {
        let (mut app, _rx) = bs::pods_app_with_metrics(2_000);
        app.filter = pat.to_string();
        let mut i = 0usize;
        measure(label, iters, || {
            bs::touch_one(&mut app, i % 2_000);
            i += 1;
            std::hint::black_box(app.row_count());
        });
    }

    // ---- log, document and overlay frames; provider ingest ----

    for (label, filter, wrap) in [
        ("log_frame/plain", "", false),
        ("log_frame/wrapped", "", true),
        ("log_frame/filtered", "reconcile", false),
    ] {
        let (mut app, _rx) = bs::logs_app(10_000, filter, wrap);
        let mut term = bs::terminal(200, 50);
        measure(label, iters, || bs::render_frame(&mut term, &mut app));
    }

    {
        let (mut app, _rx) = bs::logs_app_huge_line(1_000, 256 * 1024);
        let mut term = bs::terminal(200, 50);
        measure("log_frame/huge_line", iters, || {
            bs::render_frame(&mut term, &mut app)
        });
    }

    for (label, filter) in [("doc_frame/plain", ""), ("doc_frame/filtered", "image")] {
        let (mut app, _rx) = bs::doc_app(5_000, filter);
        let mut term = bs::terminal(200, 50);
        measure(label, iters, || bs::render_frame(&mut term, &mut app));
    }

    for (label, filter) in [("overlay/help", ""), ("overlay/help_search", "log")] {
        let (mut app, _rx) = bs::help_app(filter);
        let mut term = bs::terminal(200, 50);
        measure(label, iters, || bs::render_frame(&mut term, &mut app));
    }

    for (label, filter) in [
        ("overlay/ns_browse", ""),
        ("overlay/ns_filtered", "team-01"),
    ] {
        let (mut app, _rx) = bs::ns_picker_app(500, filter);
        let mut term = bs::terminal(200, 50);
        measure(label, iters, || bs::render_frame(&mut term, &mut app));
    }

    {
        let (mut app, _rx) = bs::pods_app(2_000);
        app.filter = "workload".to_string();
        app.table_state.select(Some(0));
        std::hint::black_box(app.row_count());
        let mut term = bs::terminal(200, 50);
        measure("name_cells/filtered_frame", iters, || {
            bs::render_frame(&mut term, &mut app)
        });
    }

    {
        let chunk = bs::provider_chunk(2_000);
        measure("provider/chunk_2000", iters, || {
            std::hint::black_box(sofka::providers::bench_ingest_chunk(&chunk));
        });
    }

    {
        let record = bs::provider_long_record(512 * 1024);
        measure("provider/fragmented_512k", 20, || {
            std::hint::black_box(sofka::providers::bench_ingest_fragmented(&record, 4096));
        });
    }
}
