//! Fixtures and thin wrappers so `benches/` can drive the real hot paths.
//!
//! Compiled only under the `bench` feature. It lives inside the crate (rather
//! than in `benches/`) for one reason: several of the paths worth measuring are
//! `pub(crate)` or `pub(super)`, and a benchmark is an external crate. Rather
//! than widening those permanently for the shipped binary, this module reaches
//! them from the inside and re-exports exactly what the benchmarks need.
//!
//! The fixtures deliberately mirror the shape of real API objects — a pod here
//! carries `containerStatuses` with `state`/`restartCount`, because
//! `pod_summary` walks that array and a fixture without it would measure an
//! empty loop.

use kube::core::DynamicObject;
use serde_json::json;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::app::App;
use crate::k8s::Cluster;
use crate::store::{Msg, row_key};

/// `ui::wrapped_height` is `pub(crate)`; benches measure it through here.
pub fn wrapped_height(raw: &str, width: usize) -> usize {
    crate::ui::wrapped_height(raw, width)
}

/// One synthetic pod. `i` varies the namespace, node, phase and restart count
/// so a filter or sort sees a realistic spread rather than N identical rows.
pub fn pod(i: usize) -> DynamicObject {
    let ns = format!("ns-{}", i % 24);
    let phase = match i % 7 {
        0 => "Pending",
        1 => "Succeeded",
        _ => "Running",
    };
    let ready = !i.is_multiple_of(5);
    let restarts = (i % 11) as i64;
    let waiting = if i.is_multiple_of(13) {
        json!({ "waiting": { "reason": "CrashLoopBackOff" } })
    } else {
        json!({ "running": { "startedAt": "2026-08-30T09:00:00Z" } })
    };

    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": format!("workload-{i:05}-7d9f8b6c5d-{:04x}", i * 7919 % 65536),
            "namespace": ns,
            "uid": format!("00000000-0000-0000-0000-{i:012}"),
            "resourceVersion": format!("{}", 100_000 + i),
            "creationTimestamp": "2026-08-30T08:00:00Z",
            "labels": {
                "app.kubernetes.io/name": format!("svc-{}", i % 40),
                "app.kubernetes.io/instance": format!("svc-{}-prod", i % 40),
                "pod-template-hash": format!("{:x}", i * 104_729 % 1_048_576),
            },
            "annotations": {
                "prometheus.io/scrape": "true",
                "prometheus.io/port": "9090",
            },
        },
        "spec": {
            "nodeName": format!("node-{:03}", i % 79),
            "containers": [
                { "name": "app", "image": format!("registry.example.com/svc-{}:v1.4.2", i % 40) },
                { "name": "sidecar", "image": "registry.example.com/envoy:v1.31.0" },
            ],
        },
        "status": {
            "phase": phase,
            "podIP": format!("10.{}.{}.{}", i / 65536 % 256, i / 256 % 256, i % 256),
            "containerStatuses": [
                {
                    "name": "app",
                    "ready": ready,
                    "restartCount": restarts,
                    "state": waiting,
                },
                {
                    "name": "sidecar",
                    "ready": true,
                    "restartCount": 0,
                    "state": { "running": { "startedAt": "2026-08-30T09:00:00Z" } },
                },
            ],
        },
    }))
    .expect("bench pod fixture is valid")
}

/// A Helm release storage Secret, encoded exactly like the real thing
/// (base64 -> base64 -> gzip -> JSON), so `helm::decode` does its real work.
pub fn helm_secret(i: usize) -> DynamicObject {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write as _;

    let name = format!("release-{}", i % 60);
    let ns = format!("ns-{}", i % 24);
    let revision = (i % 5 + 1) as i64;
    // A realistic release carries its rendered manifest — that payload is the
    // reason `decode` is expensive, so the fixture must include one.
    let manifest = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\n".repeat(200);
    let release_json = json!({
        "name": name,
        "namespace": ns,
        "version": revision,
        "info": {
            "status": "deployed",
            "description": format!("Upgrade complete (revision {revision})"),
            "last_deployed": "2026-08-30T10:30:00Z",
            "notes": "thanks for installing",
        },
        "chart": {
            "metadata": { "name": "mychart", "version": "1.0.0", "appVersion": "2.0.0" },
        },
        "config": { "replicaCount": revision },
        "manifest": manifest,
    })
    .to_string();

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(release_json.as_bytes()).expect("gzip fixture");
    let gzipped = gz.finish().expect("gzip fixture");
    let wire = BASE64.encode(BASE64.encode(gzipped));

    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": format!("sh.helm.release.v1.{name}.v{revision}"),
            "namespace": ns,
            "resourceVersion": format!("{}", 200_000 + i),
            "creationTimestamp": "2026-08-30T08:00:00Z",
            "labels": { "owner": "helm", "name": name, "version": revision.to_string() },
        },
        "type": "helm.sh/release.v1",
        "data": { "release": wire },
    }))
    .expect("bench helm fixture is valid")
}

/// The decompressed release JSON inside a fixture secret — exactly the bytes
/// `helm::decode` hands to serde. Lets a bench separate the JSON parse from
/// the base64 + gunzip in front of it.
pub fn helm_release_json(i: usize) -> Vec<u8> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::io::Read as _;

    let secret = helm_secret(i);
    let wire = secret
        .data
        .pointer("/data/release")
        .and_then(serde_json::Value::as_str)
        .expect("fixture carries a release payload");
    let helm_encoded = BASE64.decode(wire).expect("outer base64");
    let gzipped = BASE64.decode(helm_encoded).expect("inner base64");
    let mut gz = flate2::read::GzDecoder::new(&gzipped[..]);
    let mut json = Vec::new();
    gz.read_to_end(&mut json).expect("gunzip");
    json
}

/// Criterion runs benchmarks on a bare thread, but building a kube `Client`
/// spawns a tower buffer worker and panics without a reactor. One process-wide
/// runtime, kept alive for the whole run, is enough: the offline client never
/// actually issues a request.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("bench runtime")
    })
}

/// An offline `App` plus the receiver its messages land in. The receiver must
/// be held: dropping it closes the channel and later sends start failing.
pub fn app() -> (App, Receiver<Msg>) {
    let _guard = runtime().enter();
    let (tx, rx) = mpsc::channel(4096);
    (App::new(Cluster::fake(), tx), rx)
}

/// Feed `objs` through the real watch-event path, exactly as a live stream
/// would — so the store, timeline and caches all end up in their normal state.
pub fn seed(app: &mut App, objs: impl IntoIterator<Item = DynamicObject>) {
    for o in objs {
        let key = row_key(&o);
        app.handle_msg(Msg::Applied {
            generation: app.generation,
            key,
            obj: Box::new(o),
        });
    }
}

/// An app holding `n` synthetic pods, already listed as the pods view — with
/// the resolved kind and its real column spec installed, so cells, filters and
/// headers are the ones the pods view actually renders.
pub fn pods_app(n: usize) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.bench_install_kind("pods");
    seed(&mut a, (0..n).map(pod));
    (a, rx)
}

/// `pods_app` plus a metrics snapshot for every pod, so the CPU/MEM columns
/// render real values instead of the missing-metrics dash.
pub fn pods_app_with_metrics(n: usize) -> (App, Receiver<Msg>) {
    let (mut a, rx) = pods_app(n);
    for i in 0..n {
        let o = pod(i);
        a.metrics.insert(
            row_key(&o),
            (
                ((i * 37) % 4000) as i64,
                ((i * 1024 * 977) % 4_000_000_000) as i64,
            ),
        );
    }
    (a, rx)
}

/// A store-shaped object map of `n` pods — what one entry of the view cache
/// holds. Used by the memory probe to price a view snapshot directly, without
/// driving navigation (which would spawn watches).
pub fn items(n: usize) -> crate::store::Items {
    (0..n)
        .map(pod)
        .map(|o| (row_key(&o).into(), std::sync::Arc::new(o)))
        .collect()
}

/// What seeding a cached view used to cost: a full deep copy of every object's
/// `serde_json::Value` body. Kept so the memory probe can price the old path
/// against the new one in the same process.
pub fn deep_clone_items(items: &crate::store::Items) -> Vec<DynamicObject> {
    items.values().map(|o| (**o).clone()).collect()
}

/// What it costs now: a refcount bump per object.
pub fn arc_clone_items(items: &crate::store::Items) -> crate::store::Items {
    items.clone()
}

/// An app holding `n` synthetic Helm release Secrets.
pub fn helm_app(n: usize) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.bench_install_kind("helm");
    seed(&mut a, (0..n).map(helm_secret));
    (a, rx)
}

/// The context switcher's per-row fleet marker, exactly as `draw_contexts`
/// computes it: one membership test per visible context, per frame.
pub fn fleet_marks_for_all(app: &App) -> usize {
    app.filtered_contexts()
        .iter()
        .filter(|c| app.is_fleet_context(c))
        .count()
}

/// An app whose context switcher lists `n` contexts, half of them in the
/// fleet — the shape the switcher draws.
pub fn contexts_app(n: usize) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.ctx_list = (0..n).map(|i| format!("cluster-{i:03}")).collect();
    a.fleet_cfg.contexts = (0..n)
        .step_by(2)
        .map(|i| format!("cluster-{i:03}"))
        .collect();
    (a, rx)
}

/// Mark the row ordering stale *without* a store write — a filter keystroke
/// or sort toggle. The distinction matters for anything cached against store
/// contents: a store write has to recompute it, this does not.
pub fn invalidate(app: &App) {
    app.bench_invalidate_rows();
}

/// Mark the row ordering stale the way one watch event does, so a bench
/// iteration measures a real rebuild rather than a cache hit.
pub fn touch_one(app: &mut App, i: usize) {
    let o = pod(i);
    let key = row_key(&o);
    app.handle_msg(Msg::Applied {
        generation: app.generation,
        key,
        obj: Box::new(o),
    });
}

/// A synthetic log buffer: mostly plain ASCII, with the JSON, klog and
/// ANSI-coloured lines a real stream mixes in.
pub fn log_lines(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 8 {
            0 => format!(
                r#"{{"level":"info","ts":"2026-08-30T10:00:{:02}Z","msg":"reconcile complete","controller":"deployment","attempt":{i}}}"#,
                i % 60
            ),
            1 => format!("E0830 10:00:{:02}.123456       1 controller.go:214] failed to sync {i}", i % 60),
            2 => format!("\x1b[32mINFO\x1b[0m  request served path=/healthz status=200 duration={i}ms"),
            3 => format!("W0830 10:00:{:02}.000000       1 warnings.go:70] deprecated field in use ({i})", i % 60),
            _ => format!(
                "2026-08-30T10:00:{:02}Z  serving request id={i} peer=10.0.{}.{} bytes={}",
                i % 60,
                i / 256 % 256,
                i % 256,
                i * 13 % 8192
            ),
        })
        .collect()
}

/// The same buffer with a wide-character line every so often, so the
/// `wrapped_height` benchmark exercises the non-ASCII path too.
pub fn log_lines_wide(n: usize) -> Vec<String> {
    let mut v = log_lines(n);
    for (i, l) in v.iter_mut().enumerate() {
        if i % 10 == 0 {
            l.push_str(" — 日本語のログ行、幅の計算が必要");
        }
    }
    v
}

/// Sender factory for benches that need to construct messages directly.
pub fn channel() -> (Sender<Msg>, Receiver<Msg>) {
    mpsc::channel(4096)
}

/// A terminal backed by an in-memory buffer, the size a benchmark draws into.
pub fn terminal(w: u16, h: u16) -> ratatui::Terminal<ratatui::backend::TestBackend> {
    ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("bench terminal")
}

/// One full frame of the real UI, drawn into an in-memory backend — the unit
/// the table renderer is actually judged by (per-row cell rendering, width
/// measurement, column layout and mouse geometry all included).
/// One full frame of the real UI, drawn into an in-memory backend.
pub fn render_frame(
    terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    app: &mut App,
) {
    terminal
        .draw(|f| crate::ui::draw(f, app))
        .expect("bench frame");
}

/// The headers the table draws, for asserting a fixture is representative.
pub fn headers(app: &App) -> Vec<String> {
    app.display_headers()
}

/// What the table renderer does per frame before drawing anything: resolve the
/// visible window and warm its cell cache.
pub fn warm_viewport(app: &App, offset: usize, n: usize) -> usize {
    let rows = app.rows_window_keyed(offset, n);
    app.ensure_table_cell_cache(&rows);
    rows.len()
}

/// An app sitting in the logs view over an `n`-line buffer, following the tail
/// — the shape a busy pod's log stream leaves on screen.
pub fn logs_app(n: usize, filter: &str, wrap: bool) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.mode = crate::app::Mode::Logs;
    a.logs.view.title = "sherlock/app — logs".into();
    a.logs.view.lines.extend(log_lines(n));
    a.logs.follow = true;
    a.logs.wrap = wrap;
    if !filter.is_empty() {
        a.logs.set_filter(filter.to_string());
    }
    (a, rx)
}

/// The same, with one enormous single-line JSON record at the tail: the
/// viewport shows a few rows of it, and the renderer has to decide how much of
/// it to lay out.
pub fn logs_app_huge_line(n: usize, bytes: usize) -> (App, Receiver<Msg>) {
    let (mut a, rx) = logs_app(n, "", true);
    let payload = "x".repeat(bytes / 2);
    a.logs.view.lines.push_back(format!(
        r#"{{"level":"info","msg":"huge","blob":"{payload}"}}"#
    ));
    (a, rx)
}

/// An app showing a YAML document (`describe`/detail), the static-document
/// case that is re-styled on every redraw.
pub fn doc_app(n: usize, filter: &str) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.mode = crate::app::Mode::Detail;
    a.detail = crate::app::Scrollable::doc("web-7d9f8b6c5d — yaml".into(), yaml_lines(n));
    if !filter.is_empty() {
        a.detail.filter = filter.to_string();
    }
    (a, rx)
}

/// A YAML-shaped document: keys, nested values, lists and a few comments.
pub fn yaml_lines(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| match i % 8 {
            0 => "  containerStatuses:".to_string(),
            1 => format!("    - name: app-{i}"),
            2 => format!("      image: registry.example.com/svc-{}:v1.4.2", i % 40),
            3 => format!("      ready: {}", i % 3 != 0),
            4 => format!("      restartCount: {}", i % 11),
            5 => format!("  # managed by kustomize, do not edit ({i})"),
            6 => format!("      startedAt: 2026-08-30T09:00:{:02}Z", i % 60),
            _ => format!(
                "      podIP: 10.{}.{}.{}",
                i / 65536 % 256,
                i / 256 % 256,
                i % 256
            ),
        })
        .collect()
}

/// The `?` help overlay, optionally with a search typed into it.
pub fn help_app(filter: &str) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.bench_install_kind("pods");
    a.mode = crate::app::Mode::Help;
    a.help_filter = filter.to_string();
    (a, rx)
}

/// The namespace switcher over `n` namespaces, optionally filtered.
pub fn ns_picker_app(n: usize, filter: &str) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.mode = crate::app::Mode::Namespaces;
    a.ns_list = (0..n).map(|i| format!("team-{i:04}-workloads")).collect();
    a.ns_filter = filter.to_string();
    a.ns_state.select(Some(0));
    (a, rx)
}

/// A chunk of provider log records exactly as the wire delivers them:
/// newline-delimited JSON objects carrying the fields the parser reads.
pub fn provider_chunk(n: usize) -> Vec<u8> {
    let mut out = String::new();
    for i in 0..n {
        out.push_str(&format!(
            r#"{{"_time":"2026-08-30T10:00:{:02}.{:03}Z","_msg":"reconcile complete for workload-{i} in {}ms","kubernetes.pod_name":"workload-{:05}-7d9f8b6c5d-abcd","kubernetes.container_name":"app","kubernetes.namespace_name":"ns-{}"}}"#,
            i % 60,
            i % 1000,
            i % 250,
            i,
            i % 24,
        ));
        out.push('\n');
    }
    out.into_bytes()
}

/// One oversized record with no newline until the very end — the fragmented
/// arrival the framing rescan is quadratic in.
pub fn provider_long_record(bytes: usize) -> Vec<u8> {
    let mut out = r#"{"_time":"2026-08-30T10:00:00Z","_msg":""#.to_string();
    out.push_str(&"y".repeat(bytes));
    out.push_str("\"}\n");
    out.into_bytes()
}
