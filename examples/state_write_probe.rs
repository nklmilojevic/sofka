//! UI-latency and logical-update burst comparison for asynchronous state
//! persistence. The async worker may coalesce queued snapshots for the same
//! file because only the newest UI state needs to reach disk.
//!
//! Run with:
//!   cargo run --release --example state_write_probe --features bench

use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sofka::sortmem::SortMemory;
use sofka::state_writer::StateWriter;

const WRITES: u32 = 200;
const SAMPLES: usize = 21;

fn state() -> SortMemory {
    let mut state = SortMemory::default();
    for i in 0usize..100 {
        state.set(&format!("resource-{i}"), "AGE", i.is_multiple_of(2));
    }
    state
}

fn per_op(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1e6 / f64::from(WRITES)
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn sample_dir() -> PathBuf {
    std::env::temp_dir().join(format!("sofka-state-write-probe-{}", std::process::id()))
}

fn main() {
    let dir = sample_dir();
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("sort.toml");
    let state = state();
    let mut sync_ui = Vec::with_capacity(SAMPLES);
    let mut async_submit = Vec::with_capacity(SAMPLES);
    let mut async_total = Vec::with_capacity(SAMPLES);

    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..WRITES {
            black_box(state.save(black_box(&path))).unwrap();
        }
        sync_ui.push(per_op(start.elapsed()));

        let (ui_tx, _ui_rx) = tokio::sync::mpsc::channel(1);
        let writer = StateWriter::new(ui_tx).unwrap();
        let total_start = Instant::now();
        let submit_start = Instant::now();
        for _ in 0..WRITES {
            black_box(writer.save_sort(black_box(state.clone()), black_box(path.clone()))).unwrap();
        }
        async_submit.push(per_op(submit_start.elapsed()));
        drop(writer); // drains every accepted write
        async_total.push(per_op(total_start.elapsed()));
    }

    let sync_ui = median(sync_ui);
    let async_submit = median(async_submit);
    let async_total = median(async_total);
    println!("state writes ({WRITES} writes/sample, median of {SAMPLES})");
    println!("sync UI path:       {sync_ui:10.3} us/op");
    println!("async UI submit:    {async_submit:10.3} us/op");
    println!("async burst drain:  {async_total:10.3} us/op");
    println!("UI latency speedup: {:10.2}x", sync_ui / async_submit);
    println!("logical throughput: {:10.2}x", sync_ui / async_total);

    let _ = std::fs::remove_dir_all(dir);
}
