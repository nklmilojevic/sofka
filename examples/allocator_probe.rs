//! Representative allocation-heavy workload for allocator A/B builds.

#[cfg(feature = "alloc-mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(not(feature = "alloc-mimalloc"), feature = "alloc-snmalloc"))]
#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

use std::hint::black_box;
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::Value;
use sofka::benchsupport as bs;

fn main() {
    let bytes = std::fs::read("/tmp/sofka-full-pods.json").expect("live pod fixture");
    let start = Instant::now();
    for _ in 0..30 {
        black_box(serde_json::from_slice::<Value>(black_box(&bytes)).unwrap());
    }
    let json = start.elapsed();

    let (mut app, _rx) = bs::pods_app(2_000);
    app.filter = "workload-01".to_owned();
    let mut terminal = Terminal::new(TestBackend::new(120, 52)).unwrap();
    terminal
        .draw(|frame| sofka::ui::draw(frame, &mut app))
        .unwrap();
    let start = Instant::now();
    for _ in 0..2_000 {
        terminal
            .draw(|frame| sofka::ui::draw(frame, &mut app))
            .unwrap();
    }
    let render = start.elapsed();

    println!("json_30_ms={:.3}", json.as_secs_f64() * 1e3);
    println!("render_2000_ms={:.3}", render.as_secs_f64() * 1e3);
}
