//! sofka — a Kubernetes TUI, reimagined in Rust.
//!
//! A from-scratch reimagining of k9s built on kube-rs + ratatui, async-first.
//!
//! The crate is split into a library and a thin `main.rs` binary so that the
//! hot paths (row ordering, cell extraction, log filtering, wrapping) can be
//! driven directly from `benches/`. Nothing here is a stable public API — the
//! binary is the product; the library exists so the benchmarks and any future
//! integration tests can reach the same code the TUI runs.

pub mod altscroll;
pub mod app;
#[cfg(feature = "bench")]
pub mod benchsupport;
pub mod bundle;
pub mod columns;
pub mod config;
pub mod diagnostics;
pub mod explain;
pub mod filter;
pub mod fleet;
pub mod gitops;
pub mod helm;
pub mod journal;
pub mod k8s;
pub mod keys;
pub mod logfilter;
pub mod nsmem;
pub mod providers;
pub mod rightsize;
pub mod snapshot;
pub mod sortmem;
pub mod state_writer;
pub mod store;
pub mod text;
pub mod theme;
pub mod thresholds;
pub mod timeline;
pub mod ui;
pub mod views;
