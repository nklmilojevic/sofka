# Architecture and development

## Module layout

```
main.rs      CLI (clap), terminal lifecycle, the async select! event loop,
             and the --check / --snapshot headless modes.
app.rs       All application state + input handling (a mode state machine:
             Table / Command / Filter / Detail / Logs / FluxMenu /
             PortForwards / Help / Namespaces / …), split into app/*.rs
             (plugins, bookmarks, workspaces, navigation, …). Spawns watch/
             log/port-forward tasks.
k8s.rs       Cluster connect, API discovery, alias registry + group-priority
             resolution, watch-task spawning, namespace listing.
keys.rs      Key-chord parsing + matching (ctrl-/alt-/shift-, function keys)
             for plugin, bookmark, and workspace bindings, with unit tests.
store.rs     In-memory resource store + the Msg enum that watch tasks send to
             the UI (generation-tagged so stale streams are dropped).
columns.rs   Per-kind column definitions and cell extraction from
             DynamicObjects (the "render" layer), with unit tests.
thresholds.rs Configurable RESTARTS/CPU/MEM/utilization coloring bands
             (global + per-resource), compiled from config, with unit tests.
explain.rs   Deterministic "why is this unhealthy?" analysis — pure, turns
             an object + its pods + events into ranked findings, unit-tested.
adjacent.rs  Which objects connect to which: built-in and configured reference
             rules, child kinds, JSON-Pointer fan-out — pure, unit-tested.
             `app/adjacent.rs` gathers the objects and drives the view.
timeline.rs  Session-local per-object state-change history diffed from the
             watch stream (pure transition logic, unit-tested).
pvcexplore.rs PVC browsing: which pod already mounts a claim, the helper pod
             for one nothing mounts, the `ls` parser, and the size probes and
             progress-bar arithmetic behind a running copy, all pure and
             unit-tested. `app/pvcexplore.rs` drives it, `app/transfer.rs`
             samples a `kubectl cp` in flight.
ui.rs        All ratatui rendering: header, table, scrollable views, popups,
             status bar.
theme.rs     Palette + semantic styles, skin resolution.
diagnostics.rs Build stamp, state/log/snapshot/bundle directories, the
             process-wide API request-latency histogram, and the report
             sections `sofka info` and `:info` both render.
applog.rs    Structured logging: levels, logfmt rendering, and a bounded queue
             feeding one writer thread so a stalled disk never stalls the UI.
redact.rs    What counts as a credential, and how it is stripped from text.
             Shared by the log, the diagnostics reports, and `bundle.rs`.
```

## Data flow

`watcher` tasks push generation-tagged `Msg`s over an `mpsc::UnboundedSender`.
The main `tokio::select!` loop folds them into the `Store`, batching any other
queued updates before it redraws. That same loop also handles terminal input and
a 1s tick (age columns, reaping dead port-forwards). The UI never blocks on the
network.

See [performance design](vs-k9s.md#performance-design) for the performance-relevant
choices in there.

## Plugin packages

`plugins.rs` loads package manifests and validates input values.
It also controls adapter processes and reads JSON reports.
`app/plugins.rs` connects this code to commands, guardrails, and document views.
`plugin_catalog.rs` validates catalogs and selects compatible platform artifacts.
`plugin_catalog/sources.rs` loads trusted custom catalogs and limits downloads
to each source. `plugin_install.rs` verifies, stages,
records, updates, and removes managed packages. `plugin_cli.rs` keeps those
operations independent of the TUI and Kubernetes initialization.

Adapters receive a resource snapshot through standard input.
The shared runner serializes that snapshot outside the UI thread.
Tool arguments and tool result formats belong in the adapter.
See [Create a plugin package](plugin-authoring.md).

## Development

```sh
cargo run -- pods            # run against current context
cargo test                   # unit tests (no cluster required)
cargo clippy --all-targets   # lints (clean)
```

### Test coverage

Install the coverage tools once:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.1 --locked
```

Run `just coverage` to test the code with coverage enabled and generate
`target/llvm-cov/html/index.html`. Open this file in a browser to inspect which
lines the tests execute. The report uses the default Cargo features.

Pull requests run a separate coverage job. Download the `coverage-html` artifact
from the CI run, extract it, and open `index.html`. Reports are kept for 14 days.
There is no minimum coverage percentage. Use the report to find missing tests;
coverage alone does not show whether a test checks the correct behavior.

## Release

After merging the release-ready changes to `main`, run one of:

```sh
just release-patch
just release-minor
just release-major
```

The recipe switches to a clean, up-to-date `main`, bumps `Cargo.toml` and
`Cargo.lock`, commits and pushes the version bump, then creates the GitHub
Release. The release workflow runs off that published release: it uploads the
platform binaries, publishes to crates.io, and warms the Nix cache.
