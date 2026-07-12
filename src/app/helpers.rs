use super::*;

// ----- free helpers ------------------------------------------------------

pub(super) fn restart_patch(restarted_at: &str) -> Value {
    json!({
        "spec": { "template": { "metadata": { "annotations": {
            "kubectl.kubernetes.io/restartedAt": restarted_at
        }}}}
    })
}

pub(super) fn set_image_patch(plural: &str, container: &str, image: &str) -> Value {
    let containers = json!([{ "name": container, "image": image }]);
    if plural == "pods" {
        json!({ "spec": { "containers": containers } })
    } else {
        json!({ "spec": { "template": { "spec": { "containers": containers } } } })
    }
}

pub(super) fn scale_patch(replicas: i32) -> Value {
    json!({ "spec": { "replicas": replicas } })
}

pub(super) fn suspend_patch(suspend: bool) -> Value {
    json!({ "spec": { "suspend": suspend } })
}

pub(super) fn reconcile_patch(requested_at: &str) -> Value {
    json!({
        "metadata": { "annotations": { "reconcile.fluxcd.io/requestedAt": requested_at } }
    })
}

pub(super) fn external_secret_refresh_patch(force_sync: &str) -> Value {
    json!({
        "metadata": { "annotations": { "force-sync": force_sync } }
    })
}

pub(super) fn node_unschedulable_patch(unschedulable: bool) -> Value {
    json!({ "spec": { "unschedulable": unschedulable } })
}

pub(super) fn delete_confirm_label(
    kind_plural: &str,
    targets: &[(String, String)],
    force: bool,
    cascade: Cascade,
) -> String {
    let verb = if force { "Force delete" } else { "Delete" };
    // Background is the kubectl default, so only surface the unusual modes.
    let suffix = match cascade {
        Cascade::Background => "",
        Cascade::Foreground => " (cascade: foreground)",
        Cascade::Orphan => " (orphan dependents)",
    };
    if targets.len() == 1 {
        let (name, ns) = &targets[0];
        let where_ns = if ns.is_empty() {
            String::new()
        } else {
            format!(" in {ns}")
        };
        format!("{verb} {} {name}{where_ns}{suffix}?", trim_s(kind_plural))
    } else {
        format!("{verb} {} {}{suffix}?", targets.len(), kind_plural)
    }
}

pub(super) fn drainable_pod(pod: &Pod) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
    if pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
    {
        return false;
    }
    if pod
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner.kind.eq_ignore_ascii_case("DaemonSet"))
        })
    {
        return false;
    }
    !matches!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

pub(super) fn eviction_unsupported(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(api_err) if matches!(api_err.code, 404 | 405))
}

/// Pick a version name to query a CRD's custom resources: the storage version
/// if flagged, else the first served version, else the first listed.
pub(super) fn crd_served_version(d: &Value) -> Option<String> {
    let versions = d.pointer("/spec/versions")?.as_array()?;
    let pick = versions
        .iter()
        .find(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
        .or_else(|| {
            versions
                .iter()
                .find(|v| v.get("served").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| versions.first())?;
    pick.get("name").and_then(Value::as_str).map(String::from)
}

/// Build a `k=v,k2=v2` selector string from `spec/<field>` (matchLabels for
/// workloads, selector map for services).
pub(super) fn label_selector(obj: &DynamicObject, field: &str) -> Option<String> {
    let path = if field == "matchLabels" {
        vec!["spec", "selector", "matchLabels"]
    } else {
        vec!["spec", "selector"]
    };
    let mut cur = &obj.data;
    for p in path {
        cur = cur.get(p)?;
    }
    let map = cur.as_object()?;
    if map.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = map
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|vs| format!("{k}={vs}")))
        .collect();
    parts.sort();
    Some(parts.join(","))
}

pub(super) fn container_names(obj: &DynamicObject) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["containers", "initContainers", "ephemeralContainers"] {
        if let Some(arr) = obj
            .data
            .pointer(&format!("/spec/{key}"))
            .and_then(Value::as_array)
        {
            for c in arr {
                if let Some(n) = c.get("name").and_then(Value::as_str) {
                    names.push(n.to_string());
                }
            }
        }
    }
    names
}

/// Merge a drill-down selector with a filter selector into one comma-joined
/// Kubernetes selector (`None` when neither is set).
pub(super) fn join_selectors(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a},{b}")),
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (None, None) => None,
    }
}

/// Normalize a user-typed namespace argument: `all`, `*`, and `<all>` mean
/// "all namespaces" (the empty string internally).
pub(super) fn normalize_ns(ns: &str) -> String {
    let t = ns.trim();
    if t == "all" || t == "*" || t == "<all>" {
        String::new()
    } else {
        t.to_string()
    }
}

/// Trim a trailing plural "s" for breadcrumb labels (deployments -> deployment).
pub(super) fn trim_s(plural: &str) -> &str {
    plural.strip_suffix('s').unwrap_or(plural)
}

pub(super) fn xray_pool_plurals(root_kind: &str) -> &'static [&'static str] {
    match root_kind {
        "pod" => &[],
        "cronjob" => &["jobs", "pods"],
        "job" | "daemonset" | "replicaset" | "statefulset" => &["pods"],
        "deployment" => &["replicasets", "pods"],
        _ => &["replicasets", "pods"],
    }
}

impl App {
    /// Whether the current kind supports the Flux suspend/resume menu (`t`).
    pub fn flux_suspendable(&self) -> bool {
        FLUX_SUSPENDABLE_KINDS.contains(&self.kind_plural.as_str())
    }

    /// Whether the current kind is an External Secrets resource that honours
    /// the force-sync annotation (`r`).
    pub fn external_secret_kind(&self) -> bool {
        EXTERNAL_SECRET_KINDS.contains(&self.kind_plural.as_str())
    }
}

/// Columns whose cell is a count/number and should sort numerically.
pub(super) fn is_numeric_header(header: &str) -> bool {
    matches!(
        header,
        "READY"
            | "RESTARTS"
            | "DATA"
            | "ACTIVE"
            | "DESIRED"
            | "CURRENT"
            | "AVAILABLE"
            | "UP-TO-DATE"
            | "COMPLETIONS"
            | "ENDPOINTS"
    )
}

/// Parse the leading number of a cell (`"3"`, `"1/2"` → 1, `"<none>"` → 0).
pub(super) fn parse_leading_num(s: &str) -> f64 {
    let t = s.trim_start_matches(|c: char| !c.is_ascii_digit() && c != '-');
    let end = t
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(t.len());
    t[..end].parse::<f64>().unwrap_or(0.0)
}

/// Move a list selection one step, clamped to `[0, len)`. Shared by every
/// modal picker (namespaces, contexts, containers, set-image, xray).
pub(super) fn list_step(state: &mut ListState, len: usize, down: bool) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let next = if down {
        (i + 1).min(len - 1)
    } else {
        i.saturating_sub(1)
    };
    state.select(Some(next));
}

/// Copy text to the system clipboard via the first available OS tool, falling
/// back to OSC 52 for remote terminals where local clipboard tools are absent.
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // tool not installed — try the next one
        };
        // Write must finish (and the pipe close) before we wait, or the child
        // can block; report success only if the write and the process succeed.
        let wrote = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        if wrote && ok {
            return true;
        }
    }
    copy_to_clipboard_osc52(text)
}

pub(super) fn copy_to_clipboard_osc52(text: &str) -> bool {
    use std::fs::OpenOptions;
    use std::io::{Write, stdout};

    let sequence = osc52_sequence(text);
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        return tty
            .write_all(sequence.as_bytes())
            .and_then(|_| tty.flush())
            .is_ok();
    }

    let mut out = stdout();
    out.write_all(sequence.as_bytes())
        .and_then(|_| out.flush())
        .is_ok()
}

pub(super) fn osc52_sequence(text: &str) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

pub(super) async fn forward_log_stream(
    api: Api<Pod>,
    pod: String,
    lp: LogParams,
    prefix: String,
    tx: Sender<Msg>,
    generation: u64,
    flag: Arc<AtomicU64>,
) {
    use futures_util::{AsyncBufReadExt, TryStreamExt};
    use tokio::time::MissedTickBehavior;

    let stream = match api.log_stream(&pod, &lp).await {
        Ok(stream) => stream,
        Err(e) => {
            let _ = tx
                .send(Msg::LogLines {
                    generation,
                    lines: vec![format!("[error] {e}")],
                })
                .await;
            return;
        }
    };

    let mut lines = stream.lines();
    let mut batch = Vec::with_capacity(LOG_BATCH_LINES);
    let mut flush = tokio::time::interval(Duration::from_millis(LOG_BATCH_MS));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if flag.load(Ordering::SeqCst) != generation {
            break;
        }

        tokio::select! {
            next = lines.try_next() => {
                match next {
                    Ok(Some(line)) => {
                        batch.push(format!("{prefix}{line}"));
                        if batch.len() >= LOG_BATCH_LINES
                            && !send_log_batch(&tx, generation, &mut batch).await
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        batch.push(format!("[error] {e}"));
                        break;
                    }
                }
            }
            _ = flush.tick(), if !batch.is_empty() => {
                if !send_log_batch(&tx, generation, &mut batch).await {
                    break;
                }
            }
        }
    }

    if flag.load(Ordering::SeqCst) == generation {
        let _ = send_log_batch(&tx, generation, &mut batch).await;
    }
}

pub(super) async fn send_log_batch(
    tx: &Sender<Msg>,
    generation: u64,
    batch: &mut Vec<String>,
) -> bool {
    if batch.is_empty() {
        return true;
    }
    let lines = std::mem::take(batch);
    tx.send(Msg::LogLines { generation, lines }).await.is_ok()
}

pub(super) async fn send_event_snapshot(
    tx: &Sender<Msg>,
    generation: u64,
    title: &str,
    items: &HashMap<String, DynamicObject>,
    events_v1: bool,
) -> bool {
    tx.send(Msg::Events {
        generation,
        title: title.to_string(),
        lines: format_event_lines(items.values(), events_v1),
    })
    .await
    .is_ok()
}

pub(super) fn format_event_lines<'a, I>(events: I, events_v1: bool) -> Vec<String>
where
    I: IntoIterator<Item = &'a DynamicObject>,
{
    let mut rows: Vec<(String, String)> = events
        .into_iter()
        .map(|event| {
            let seen = event_time(event, events_v1);
            (seen.clone(), event_line(event, events_v1, &seen))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut lines = vec![format!(
        "{:<20} {:<8} {:<24} {:>5} {}",
        "LAST SEEN", "TYPE", "REASON", "COUNT", "MESSAGE"
    )];
    if rows.is_empty() {
        lines.push("(no events)".into());
    } else {
        lines.extend(rows.into_iter().map(|(_, line)| line));
    }
    lines
}

pub(super) fn event_line(event: &DynamicObject, events_v1: bool, seen: &str) -> String {
    let typ = svalue(&event.data, &["type"]).unwrap_or_default();
    let reason = svalue(&event.data, &["reason"]).unwrap_or_default();
    let count = event_count(event, events_v1);
    let message = if events_v1 {
        svalue(&event.data, &["note"])
            .or_else(|| svalue(&event.data, &["message"]))
            .unwrap_or_default()
    } else {
        svalue(&event.data, &["message"])
            .or_else(|| svalue(&event.data, &["note"]))
            .unwrap_or_default()
    };
    format!(
        "{:<20} {:<8} {:<24} {:>5} {}",
        compact_event_time(seen),
        typ,
        reason,
        count,
        message.replace('\n', " ")
    )
}

pub(super) fn event_count(event: &DynamicObject, events_v1: bool) -> i64 {
    if events_v1 {
        ivalue(&event.data, &["series", "count"])
            .or_else(|| ivalue(&event.data, &["deprecatedCount"]))
            .unwrap_or(1)
    } else {
        ivalue(&event.data, &["count"]).unwrap_or(1)
    }
}

pub(super) fn event_time(event: &DynamicObject, events_v1: bool) -> String {
    let data = &event.data;
    let value = if events_v1 {
        svalue(data, &["series", "lastObservedTime"])
            .or_else(|| svalue(data, &["eventTime"]))
            .or_else(|| svalue(data, &["deprecatedLastTimestamp"]))
            .or_else(|| svalue(data, &["deprecatedFirstTimestamp"]))
    } else {
        svalue(data, &["lastTimestamp"])
            .or_else(|| svalue(data, &["eventTime"]))
            .or_else(|| svalue(data, &["firstTimestamp"]))
    };
    value
        .or_else(|| {
            event
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0.to_string())
        })
        .unwrap_or_default()
}

pub(super) fn compact_event_time(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('Z');
    if let Some((date, time)) = trimmed.split_once('T') {
        let time = time.split('.').next().unwrap_or(time);
        format!("{date} {time}")
    } else {
        raw.to_string()
    }
}

pub(super) fn svalue(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(String::from)
}

pub(super) fn ivalue(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_i64()
}

/// Recursively flatten an object and its owned children into xray rows.
pub(super) fn emit_xray(
    kind: &str,
    obj: &DynamicObject,
    depth: usize,
    children: &std::collections::HashMap<String, Vec<(String, DynamicObject)>>,
    items: &mut Vec<XrayItem>,
) {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let ns = obj.metadata.namespace.clone().unwrap_or_default();
    items.push(XrayItem {
        depth,
        kind: kind.to_string(),
        name: name.clone(),
        ns: ns.clone(),
        status: xray_status(kind, obj),
        container: None,
    });

    if let Some(uid) = &obj.metadata.uid
        && let Some(kids) = children.get(uid)
    {
        for (clabel, cobj) in kids {
            emit_xray(clabel, cobj, depth + 1, children, items);
        }
    }

    // Pods expand into their containers as leaves.
    if kind == "pod" {
        for c in container_names(obj) {
            items.push(XrayItem {
                depth: depth + 1,
                kind: "container".into(),
                name: name.clone(),
                ns: ns.clone(),
                status: String::new(),
                container: Some(c),
            });
        }
    }
}

pub(super) fn xray_status(kind: &str, o: &DynamicObject) -> String {
    match kind {
        "pod" => phase(o),
        "job" => format!(
            "{}/{}",
            o.data
                .pointer("/status/succeeded")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/spec/completions")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1),
        ),
        "cronjob" => format!(
            "active {}",
            o.data
                .pointer("/status/active")
                .and_then(Value::as_array)
                .map_or(0, |items| items.len()),
        ),
        "deployment" | "replicaset" | "statefulset" => format!(
            "{}/{}",
            o.data
                .pointer("/status/readyReplicas")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/spec/replicas")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        "daemonset" => format!(
            "{}/{}",
            o.data
                .pointer("/status/numberReady")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/status/desiredNumberScheduled")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        _ => String::new(),
    }
}

/// List all objects of a kind (namespaced to `ns` when applicable).
pub(super) async fn list_kind(
    client: &Client,
    ar: &ApiResource,
    namespaced: bool,
    ns: &str,
) -> Vec<DynamicObject> {
    let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    api.list(&ListParams::default())
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

pub(super) fn phase(o: &DynamicObject) -> String {
    o.data
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub(super) fn node_ready(o: &DynamicObject) -> bool {
    o.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_str) == Some("True")
            })
        })
        .unwrap_or(false)
}

/// True when the two integer pointers are equal and non-zero (e.g. ready == desired).
pub(super) fn ready_eq(o: &DynamicObject, ready_ptr: &str, want_ptr: &str) -> bool {
    let r = o
        .data
        .pointer(ready_ptr)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let w = o
        .data
        .pointer(want_ptr)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    w > 0 && r >= w
}

/// Extract (cpu millicores, memory bytes) from a metrics-API object.
pub(super) fn usage_of(obj: &DynamicObject, is_node: bool) -> (i64, i64) {
    use crate::columns::{parse_cpu_milli, parse_mem_bytes};
    if is_node {
        let cpu = obj
            .data
            .pointer("/usage/cpu")
            .and_then(Value::as_str)
            .map(parse_cpu_milli)
            .unwrap_or(0);
        let mem = obj
            .data
            .pointer("/usage/memory")
            .and_then(Value::as_str)
            .map(parse_mem_bytes)
            .unwrap_or(0);
        (cpu, mem)
    } else {
        let mut cpu = 0;
        let mut mem = 0;
        if let Some(cs) = obj.data.pointer("/containers").and_then(Value::as_array) {
            for c in cs {
                if let Some(s) = c.pointer("/usage/cpu").and_then(Value::as_str) {
                    cpu += parse_cpu_milli(s);
                }
                if let Some(s) = c.pointer("/usage/memory").and_then(Value::as_str) {
                    mem += parse_mem_bytes(s);
                }
            }
        }
        (cpu, mem)
    }
}
