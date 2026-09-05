//! Allocation probes for candidates whose value is primarily heap pressure.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use compact_str::CompactString;
use lasso::{Rodeo, Spur};

const N: usize = 20_000;

fn stats_delta(before: &dhat::HeapStats, after: &dhat::HeapStats) -> (usize, usize) {
    (
        after.curr_bytes.saturating_sub(before.curr_bytes),
        after.curr_blocks.saturating_sub(before.curr_blocks),
    )
}

fn value(i: usize) -> String {
    match i % 4 {
        0 => "Running".to_owned(),
        1 => "kube-system".to_owned(),
        2 => format!("node-{}", i % 80),
        _ => format!("namespace-{}", i % 24),
    }
}

fn strings() {
    let before = dhat::HeapStats::get();
    let values: Vec<String> = (0..N).map(value).collect();
    let after = dhat::HeapStats::get();
    let (bytes, blocks) = stats_delta(&before, &after);
    println!("String: {bytes} live bytes, {blocks} live allocations");
    std::hint::black_box(values);
}

fn compact() {
    let before = dhat::HeapStats::get();
    let values: Vec<CompactString> = (0..N).map(|i| CompactString::new(value(i))).collect();
    let after = dhat::HeapStats::get();
    let (bytes, blocks) = stats_delta(&before, &after);
    println!("CompactString: {bytes} live bytes, {blocks} live allocations");
    std::hint::black_box(values);
}

fn interned() {
    let before = dhat::HeapStats::get();
    let mut rodeo: Rodeo<Spur> = Rodeo::new();
    let ids: Vec<_> = (0..N).map(|i| rodeo.get_or_intern(value(i))).collect();
    let after = dhat::HeapStats::get();
    let (bytes, blocks) = stats_delta(&before, &after);
    println!("lasso: {bytes} live bytes, {blocks} live allocations");
    std::hint::black_box((rodeo, ids));
}

fn render(filter: bool) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use sofka::benchsupport as bs;

    let (mut app, _rx) = bs::pods_app(2_000);
    if filter {
        app.filter = "workload-01".to_owned();
    }
    let mut terminal = Terminal::new(TestBackend::new(120, 52)).unwrap();
    terminal
        .draw(|frame| sofka::ui::draw(frame, &mut app))
        .unwrap();
    let before = dhat::HeapStats::get();
    for _ in 0..100 {
        terminal
            .draw(|frame| sofka::ui::draw(frame, &mut app))
            .unwrap();
    }
    let after = dhat::HeapStats::get();
    println!(
        "render filtered={filter}: {:.1} bytes/frame, {:.1} allocations/frame",
        after.total_bytes.saturating_sub(before.total_bytes) as f64 / 100.0,
        after.total_blocks.saturating_sub(before.total_blocks) as f64 / 100.0,
    );
}

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();
    match std::env::args().nth(1).as_deref() {
        Some("string") => strings(),
        Some("compact") => compact(),
        Some("lasso") => interned(),
        Some("render") => render(false),
        Some("render-filtered") => render(true),
        _ => panic!("use string|compact|lasso|render|render-filtered"),
    }
}
