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

use k8s_openapi::api::core::v1::Service;
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

/// An app holding `n` synthetic pods, already listed as the pods view.
pub fn pods_app(n: usize) -> (App, Receiver<Msg>) {
    let (mut a, rx) = app();
    a.kind_plural = "pods".to_string();
    seed(&mut a, (0..n).map(pod));
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
    a.kind_plural = "helm".to_string();
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

/// Service-discovery input with many usable candidates. Reverse namespaces
/// ensure the minimum is not an accidental first-item fast path.
pub fn services(n: usize) -> Vec<Service> {
    (0..n)
        .map(|i| {
            serde_json::from_value(json!({
                "metadata": {
                    "name": format!("backend-{i:04}"),
                    "namespace": format!("ns-{:04}", n - i),
                },
                "spec": {
                    "ports": [
                        {"name": "metrics", "port": 8080},
                        {"port": if i.is_multiple_of(2) { 9428 } else { 9090 }},
                    ]
                }
            }))
            .expect("bench Service fixture is valid")
        })
        .collect()
}

/// Production one-pass VictoriaLogs candidate selection.
pub fn pick_log_service(services: &[Service]) -> Option<(String, String, i32)> {
    crate::providers::bench_pick_log_service(services)
}

/// Production one-pass Prometheus/VictoriaMetrics candidate selection.
pub fn pick_metrics_service(services: &[Service]) -> Option<(String, String, i32)> {
    crate::providers::bench_pick_metrics_service(services)
}

/// The allocation-heavy provider-selection implementation before this
/// follow-up: materialize every usable candidate, then call `min_by_key`.
pub fn pick_log_service_collected(services: &[Service]) -> Option<(String, String, i32)> {
    let candidates: Vec<(&Service, i32)> = services
        .iter()
        .filter_map(|service| {
            let ports = service.spec.as_ref()?.ports.as_ref()?;
            let port = ports
                .iter()
                .find(|port| port.name.as_deref() == Some("http"))
                .or_else(|| ports.iter().find(|port| port.port == 9428))
                .or_else(|| ports.first())?;
            Some((service, port.port))
        })
        .collect();
    finish_collected(candidates)
}

/// Metrics-provider form of the former collected implementation.
pub fn pick_metrics_service_collected(services: &[Service]) -> Option<(String, String, i32)> {
    let candidates: Vec<(&Service, i32)> = services
        .iter()
        .filter_map(|service| {
            let ports = service.spec.as_ref()?.ports.as_ref()?;
            let port = ports
                .iter()
                .find(|port| port.name.as_deref() == Some("http"))
                .or_else(|| {
                    ports
                        .iter()
                        .find(|port| matches!(port.port, 9090 | 8428 | 8429))
                })
                .or_else(|| ports.first())?;
            Some((service, port.port))
        })
        .collect();
    finish_collected(candidates)
}

fn finish_collected(candidates: Vec<(&Service, i32)>) -> Option<(String, String, i32)> {
    let (service, port) = candidates.iter().min_by_key(|(service, _)| {
        (
            service.metadata.namespace.as_deref().unwrap_or_default(),
            service.metadata.name.as_deref().unwrap_or_default(),
        )
    })?;
    Some((
        service.metadata.namespace.clone().unwrap_or_default(),
        service.metadata.name.clone().unwrap_or_default(),
        *port,
    ))
}

/// A `deployment -> replicaset -> pod` ownership tree, the shape the xray view
/// indexes: `roots` deployments, one replicaset each, `pods_per_root` pods
/// under that replicaset. Returned as the two lists `spawn_xray` gathers.
pub fn xray_tree(
    roots: usize,
    pods_per_root: usize,
) -> (Vec<DynamicObject>, Vec<(String, DynamicObject)>) {
    let mut deployments = Vec::with_capacity(roots);
    let mut pool = Vec::with_capacity(roots * (pods_per_root + 1));
    for d in 0..roots {
        let duid = format!("dep-{d:012}");
        let ruid = format!("rs-{d:012}");
        deployments.push(
            serde_json::from_value(json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {
                    "name": format!("svc-{d:04}"),
                    "namespace": format!("ns-{}", d % 24),
                    "uid": duid,
                    "creationTimestamp": "2026-08-30T08:00:00Z",
                },
                "spec": { "replicas": pods_per_root },
                "status": { "readyReplicas": pods_per_root },
            }))
            .expect("bench deployment fixture is valid"),
        );
        pool.push((
            "replicaset".to_string(),
            serde_json::from_value(json!({
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "metadata": {
                    "name": format!("svc-{d:04}-7d9f8b6c5d"),
                    "namespace": format!("ns-{}", d % 24),
                    "uid": ruid,
                    "creationTimestamp": "2026-08-30T08:00:00Z",
                    "ownerReferences": [{
                        "apiVersion": "apps/v1",
                        "kind": "Deployment",
                        "name": format!("svc-{d:04}"),
                        "uid": duid,
                    }],
                },
                "spec": { "replicas": pods_per_root },
                "status": { "readyReplicas": pods_per_root },
            }))
            .expect("bench replicaset fixture is valid"),
        ));
        for p in 0..pods_per_root {
            let mut pod = pod(d * pods_per_root + p);
            pod.metadata.namespace = Some(format!("ns-{}", d % 24));
            pod.metadata.owner_references = Some(vec![
                serde_json::from_value(json!({
                    "apiVersion": "apps/v1",
                    "kind": "ReplicaSet",
                    "name": format!("svc-{d:04}-7d9f8b6c5d"),
                    "uid": ruid,
                }))
                .expect("bench owner reference is valid"),
            ]);
            pool.push(("pod".to_string(), pod));
        }
    }
    (deployments, pool)
}

/// The production owner index + tree flatten for one xray refresh.
pub fn xray_flatten(
    root_kind: &str,
    roots: &[DynamicObject],
    pool: &[(String, DynamicObject)],
) -> Vec<crate::store::XrayItem> {
    crate::app::xray_flatten(root_kind, roots, pool)
}

/// `n` core/v1 Events for one object, spread over distinct timestamps and
/// reasons so sorting and formatting see realistic input.
pub fn events(n: usize) -> Vec<DynamicObject> {
    (0..n)
        .map(|i| {
            let reason = ["Scheduled", "Pulled", "Created", "Started", "BackOff"][i % 5];
            let typ = if i.is_multiple_of(9) {
                "Warning"
            } else {
                "Normal"
            };
            serde_json::from_value(json!({
                "apiVersion": "v1",
                "kind": "Event",
                "metadata": {
                    "name": format!("workload.{i:08x}"),
                    "namespace": "ns-0",
                    "uid": format!("ev-{i:012}"),
                    "creationTimestamp": "2026-08-30T08:00:00Z",
                },
                "type": typ,
                "reason": reason,
                "count": (i % 17) + 1,
                "lastTimestamp": format!("2026-08-30T{:02}:{:02}:{:02}Z", i % 24, i % 60, i % 60),
                "message": format!(
                    "Successfully assigned ns-0/workload-{i:05} to node-{:03}; \
                     reconcile completed in {}ms",
                    i % 79,
                    i % 900
                ),
                "involvedObject": {
                    "kind": "Pod",
                    "name": format!("workload-{i:05}"),
                    "namespace": "ns-0",
                    "uid": "00000000-0000-0000-0000-000000000001",
                },
            }))
            .expect("bench event fixture is valid")
        })
        .collect()
}

/// An events document holding `events`, ready to publish.
pub struct EventDocFixture(crate::app::EventDoc);

pub fn event_doc(events: &[DynamicObject]) -> EventDocFixture {
    let mut doc = crate::app::EventDoc::new(false);
    for e in events {
        doc.apply(e);
    }
    EventDocFixture(doc)
}

/// One publish of an accumulated document — what the view rebuilds whenever
/// the events document changes.
pub fn event_doc_render(doc: &EventDocFixture) -> usize {
    doc.0.render().len()
}

/// What an N-event initial list costs end to end: ingest every event, then
/// publish the document the view finally draws.
pub fn event_doc_initial(events: &[DynamicObject]) -> usize {
    event_doc_render(&event_doc(events))
}

/// A wide object: `entries` status conditions plus a label/annotation block the
/// size a real CRD carries. Stands in for the big documents the YAML/describe
/// views build on a keypress.
pub fn fat_object(entries: usize) -> DynamicObject {
    let conditions: Vec<_> = (0..entries)
        .map(|i| {
            json!({
                "type": format!("Condition{i}"),
                "status": if i.is_multiple_of(3) { "False" } else { "True" },
                "lastTransitionTime": "2026-08-30T08:00:00Z",
                "reason": format!("Reason{i}"),
                "message": format!(
                    "controller {i} reconciled the resource and reported a \
                     detailed status message about revision {}",
                    1000 + i
                ),
            })
        })
        .collect();
    let labels: serde_json::Map<String, serde_json::Value> = (0..entries / 4)
        .map(|i| {
            (
                format!("example.com/label-{i}"),
                json!(format!("value-{i}")),
            )
        })
        .collect();
    serde_json::from_value(json!({
        "apiVersion": "example.com/v1",
        "kind": "Widget",
        "metadata": {
            "name": "fat-widget",
            "namespace": "ns-0",
            "uid": "00000000-0000-0000-0000-000000000042",
            "resourceVersion": "987654",
            "creationTimestamp": "2026-08-30T08:00:00Z",
            "labels": labels,
            "annotations": { "example.com/notes": "x".repeat(4096) },
        },
        "spec": { "replicas": 3, "template": { "conditions": conditions.clone() } },
        "status": { "conditions": conditions },
    }))
    .expect("bench fat object fixture is valid")
}

/// `n` log lines of `bytes` each — the pathological case a byte budget exists
/// for: one structured record dumped whole onto a single line.
pub fn log_lines_long(n: usize, bytes: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let mut s = format!("{i:06} ");
            s.push_str(&"payload=".repeat(bytes / 8));
            s.truncate(bytes.max(s.find(' ').unwrap_or(0) + 1));
            s
        })
        .collect()
}

/// Run `lines` through the ingest batching both log paths share, flushing a
/// full batch the way the stream loop does. Returns the number of batches.
pub fn ingest_lines(prefix: &str, lines: impl IntoIterator<Item = String>) -> usize {
    crate::app::ingest_lines(prefix, lines)
}

/// An app whose follow buffer is already at its retention limit, so the next
/// push has to trim.
pub fn logs_app_at_capacity(buffer: usize) -> App {
    let (mut app, _rx) = app();
    app.logs_cfg.buffer = buffer;
    app.logs.follow = true;
    app.logs.view.lines.extend(log_lines(buffer));
    app
}

/// Lines currently retained in the follow buffer.
pub fn log_line_count(app: &App) -> usize {
    app.logs.view.lines.len()
}

/// One `d` keypress on a changed object: clean both revisions and walk the
/// unified diff. Runs on the UI thread, so this is keypress latency.
pub fn diff_document(previous: &DynamicObject, live: &DynamicObject) -> usize {
    let baseline = serde_yaml::to_string(previous).expect("fixture yaml");
    crate::app::diff_document(&baseline, live.clone()).len()
}

/// A changed revision of `fat_object`: same shape, different status.
pub fn fat_object_changed(entries: usize) -> DynamicObject {
    let mut o = fat_object(entries);
    if let Some(conds) = o
        .data
        .pointer_mut("/status/conditions")
        .and_then(serde_json::Value::as_array_mut)
    {
        for (i, c) in conds.iter_mut().enumerate() {
            if i.is_multiple_of(4) {
                c["status"] = json!("Unknown");
                c["message"] = json!(format!("controller {i} is re-reconciling"));
            }
        }
    }
    o.metadata.resource_version = Some("987999".into());
    o
}
