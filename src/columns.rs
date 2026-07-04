//! Per-kind column definitions and cell extraction from DynamicObjects.
//!
//! This is the "render" layer of the original k9s, reimagined: instead of a
//! hand-written renderer per resource, known kinds get curated columns and
//! everything else falls back to NAME/AGE pulled from metadata.

use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use serde_json::Value;

/// Headers for a kind, excluding the leading NAMESPACE column (the table view
/// prepends that when listing across namespaces).
pub fn headers(plural: &str) -> Vec<&'static str> {
    match plural {
        "pods" => vec!["NAME", "READY", "STATUS", "RESTARTS", "IP", "NODE", "AGE"],
        "deployments" => vec!["NAME", "READY", "UP-TO-DATE", "AVAILABLE", "AGE"],
        "replicasets" => vec!["NAME", "DESIRED", "CURRENT", "READY", "AGE"],
        "statefulsets" => vec!["NAME", "READY", "AGE"],
        "daemonsets" => vec!["NAME", "DESIRED", "CURRENT", "READY", "AVAILABLE", "AGE"],
        "services" => vec!["NAME", "TYPE", "CLUSTER-IP", "EXTERNAL-IP", "PORTS", "AGE"],
        "nodes" => vec!["NAME", "STATUS", "ROLES", "VERSION", "AGE"],
        "namespaces" => vec!["NAME", "STATUS", "AGE"],
        "configmaps" => vec!["NAME", "DATA", "AGE"],
        "secrets" => vec!["NAME", "TYPE", "DATA", "AGE"],
        "jobs" => vec!["NAME", "COMPLETIONS", "DURATION", "AGE"],
        "cronjobs" => vec![
            "NAME",
            "SCHEDULE",
            "SUSPEND",
            "ACTIVE",
            "LAST-SCHEDULE",
            "AGE",
        ],
        "events" => vec![
            "NAME", "TYPE", "REASON", "OBJECT", "MESSAGE", "COUNT", "AGE",
        ],
        "horizontalpodautoscalers" => {
            vec![
                "NAME",
                "REFERENCE",
                "TARGETS",
                "MINPODS",
                "MAXPODS",
                "REPLICAS",
                "AGE",
            ]
        }
        "persistentvolumeclaims" => vec!["NAME", "STATUS", "VOLUME", "CAPACITY", "AGE"],
        "persistentvolumes" => vec!["NAME", "CAPACITY", "STATUS", "CLAIM", "AGE"],
        "ingresses" => vec!["NAME", "CLASS", "HOSTS", "AGE"],
        "endpoints" => vec!["NAME", "ENDPOINTS", "AGE"],
        "customresourcedefinitions" => vec!["NAME", "GROUP", "KIND", "VERSIONS", "SCOPE", "AGE"],
        "kustomizations" | "helmreleases" => {
            vec!["NAME", "READY", "MESSAGE", "REVISION", "SUSPENDED", "AGE"]
        }
        "gitrepositories" | "helmrepositories" | "ocirepositories" | "buckets" => {
            vec![
                "NAME",
                "READY",
                "MESSAGE",
                "REVISION",
                "URL",
                "SUSPENDED",
                "AGE",
            ]
        }
        _ => vec!["NAME", "AGE"],
    }
}

/// Cells for one object, aligned with [`headers`]. The 2nd return value is the
/// index of the column that should be colorized as a status (or None).
pub fn cells(obj: &DynamicObject, plural: &str) -> (Vec<String>, Option<usize>) {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let age = age(obj);
    let d = &obj.data;

    match plural {
        "pods" => {
            let (ready, status, restarts) = pod_summary(obj);
            let ip = sget(d, &["status", "podIP"]).unwrap_or_else(|| "<none>".into());
            let node = sget(d, &["spec", "nodeName"]).unwrap_or_else(|| "<none>".into());
            (vec![name, ready, status, restarts, ip, node, age], Some(2))
        }
        "deployments" => {
            let ready = format!(
                "{}/{}",
                iget(d, &["status", "readyReplicas"]),
                iget(d, &["status", "replicas"])
            );
            let utd = iget(d, &["status", "updatedReplicas"]).to_string();
            let avail = iget(d, &["status", "availableReplicas"]).to_string();
            (vec![name, ready, utd, avail, age], None)
        }
        "replicasets" => {
            let desired = iget(d, &["spec", "replicas"]).to_string();
            let current = iget(d, &["status", "replicas"]).to_string();
            let ready = iget(d, &["status", "readyReplicas"]).to_string();
            (vec![name, desired, current, ready, age], None)
        }
        "statefulsets" => {
            let ready = format!(
                "{}/{}",
                iget(d, &["status", "readyReplicas"]),
                iget(d, &["spec", "replicas"])
            );
            (vec![name, ready, age], None)
        }
        "daemonsets" => (
            vec![
                name,
                iget(d, &["status", "desiredNumberScheduled"]).to_string(),
                iget(d, &["status", "currentNumberScheduled"]).to_string(),
                iget(d, &["status", "numberReady"]).to_string(),
                iget(d, &["status", "numberAvailable"]).to_string(),
                age,
            ],
            None,
        ),
        "services" => {
            let typ = sget(d, &["spec", "type"]).unwrap_or_else(|| "ClusterIP".into());
            let cip = sget(d, &["spec", "clusterIP"]).unwrap_or_else(|| "<none>".into());
            let eip = external_ip(d, &typ);
            let ports = svc_ports(d);
            (vec![name, typ, cip, eip, ports, age], None)
        }
        "nodes" => {
            let status = node_ready(d);
            let roles = node_roles(obj);
            let ver = sget(d, &["status", "nodeInfo", "kubeletVersion"]).unwrap_or_default();
            (vec![name, status, roles, ver, age], Some(1))
        }
        "namespaces" => {
            let status = sget(d, &["status", "phase"]).unwrap_or_else(|| "Active".into());
            (vec![name, status, age], Some(1))
        }
        "configmaps" => {
            let n = count_obj(d, &["data"]) + count_obj(d, &["binaryData"]);
            (vec![name, n.to_string(), age], None)
        }
        "secrets" => {
            let typ = sget(d, &["type"]).unwrap_or_else(|| "Opaque".into());
            let n = count_obj(d, &["data"]);
            (vec![name, typ, n.to_string(), age], None)
        }
        "jobs" => {
            let comp = format!(
                "{}/{}",
                iget(d, &["status", "succeeded"]),
                iget(d, &["spec", "completions"]).max(1)
            );
            let duration = job_duration(d);
            (vec![name, comp, duration, age], None)
        }
        "cronjobs" => {
            let sched = sget(d, &["spec", "schedule"]).unwrap_or_default();
            let suspend = bget(d, &["spec", "suspend"]).to_string();
            let active = count_arr(d, &["status", "active"]).to_string();
            let last_schedule = time_since(d, &["status", "lastScheduleTime"]);
            (vec![name, sched, suspend, active, last_schedule, age], None)
        }
        "events" => {
            let typ = sget(d, &["type"]).unwrap_or_default();
            let reason = sget(d, &["reason"]).unwrap_or_default();
            let involved = event_object(d);
            let message = event_message(d);
            let count = event_count(d).to_string();
            (vec![name, typ, reason, involved, message, count, age], None)
        }
        "horizontalpodautoscalers" => {
            let reference = hpa_reference(d);
            let targets = hpa_targets(d);
            let min = iopt(d, &["spec", "minReplicas"]).unwrap_or(1).to_string();
            let max = iopt(d, &["spec", "maxReplicas"])
                .map(|n| n.to_string())
                .unwrap_or_default();
            let replicas = iopt(d, &["status", "currentReplicas"])
                .unwrap_or(0)
                .to_string();
            (
                vec![name, reference, targets, min, max, replicas, age],
                None,
            )
        }
        "persistentvolumeclaims" => {
            let status = sget(d, &["status", "phase"]).unwrap_or_default();
            let vol = sget(d, &["spec", "volumeName"]).unwrap_or_default();
            let cap = sget(d, &["status", "capacity", "storage"]).unwrap_or_default();
            (vec![name, status, vol, cap, age], Some(1))
        }
        "persistentvolumes" => {
            let cap = sget(d, &["spec", "capacity", "storage"]).unwrap_or_default();
            let status = sget(d, &["status", "phase"]).unwrap_or_default();
            let claim = sget(d, &["spec", "claimRef", "name"]).unwrap_or_default();
            (vec![name, cap, status, claim, age], Some(2))
        }
        "ingresses" => {
            let class = sget(d, &["spec", "ingressClassName"]).unwrap_or_else(|| "<none>".into());
            let hosts = ingress_hosts(d);
            (vec![name, class, hosts, age], None)
        }
        "endpoints" => {
            let eps = count_endpoints(d);
            (vec![name, eps, age], None)
        }
        "customresourcedefinitions" => {
            let group = sget(d, &["spec", "group"]).unwrap_or_default();
            let ckind = sget(d, &["spec", "names", "kind"]).unwrap_or_default();
            let versions = crd_versions(d);
            let scope = sget(d, &["spec", "scope"]).unwrap_or_default();
            (vec![name, group, ckind, versions, scope, age], None)
        }
        "kustomizations" | "helmreleases" => {
            let (ready, msg) = ready_condition(d);
            let revision = flux_revision(d);
            let suspended = bget(d, &["spec", "suspend"]).to_string();
            (vec![name, ready, msg, revision, suspended, age], Some(1))
        }
        "gitrepositories" | "helmrepositories" | "ocirepositories" | "buckets" => {
            let (ready, msg) = ready_condition(d);
            let revision = flux_source_revision(d);
            let url = flux_source_url(d);
            let suspended = bget(d, &["spec", "suspend"]).to_string();
            (
                vec![name, ready, msg, revision, url, suspended, age],
                Some(1),
            )
        }
        _ => (vec![name, age], None),
    }
}

/// Cells whose display value changes with wall time even when the Kubernetes
/// resourceVersion is unchanged. Table rendering can override cached values
/// with these without recomputing every curated column.
pub fn volatile_cell(obj: &DynamicObject, plural: &str, header: &str) -> Option<String> {
    match (plural, header) {
        (_, "AGE") => Some(age(obj)),
        ("jobs", "DURATION") if sget(&obj.data, &["status", "completionTime"]).is_none() => {
            Some(job_duration(&obj.data))
        }
        ("cronjobs", "LAST-SCHEDULE") => {
            Some(time_since(&obj.data, &["status", "lastScheduleTime"]))
        }
        _ => None,
    }
}

// ----- helpers ------------------------------------------------------------

/// Comma-joined CRD version names (`spec.versions[].name`), e.g. `v1,v1beta1`.
fn crd_versions(d: &Value) -> String {
    let names: Vec<&str> = d
        .pointer("/spec/versions")
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(|v| v.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        "<none>".into()
    } else {
        names.join(",")
    }
}

/// (status, message) of the `Ready` condition, the health summary Flux (and
/// most condition-based CRDs) maintain. Missing condition reads as Unknown —
/// e.g. a Kustomization the controller hasn't reconciled yet.
fn ready_condition(d: &Value) -> (String, String) {
    d.pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .map(|c| {
            (
                c.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown")
                    .to_string(),
                c.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .unwrap_or_else(|| ("Unknown".into(), String::new()))
}

/// Last applied revision of a Flux object: Kustomizations (and HelmRelease
/// v2beta*) expose `lastAppliedRevision`; HelmRelease v2 GA moved it into
/// `history`, with `lastAttemptedRevision` as the pre-first-success fallback.
fn flux_revision(d: &Value) -> String {
    sget(d, &["status", "lastAppliedRevision"])
        .or_else(|| {
            d.pointer("/status/history/0/chartVersion")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| sget(d, &["status", "lastAttemptedRevision"]))
        .unwrap_or_default()
}

fn flux_source_revision(d: &Value) -> String {
    sget(d, &["status", "artifact", "revision"])
        .or_else(|| sget(d, &["status", "lastAppliedRevision"]))
        .or_else(|| sget(d, &["status", "lastAttemptedRevision"]))
        .unwrap_or_default()
}

fn flux_source_url(d: &Value) -> String {
    sget(d, &["spec", "url"])
        .or_else(|| {
            let endpoint = sget(d, &["spec", "endpoint"]);
            let bucket = sget(d, &["spec", "bucketName"]);
            match (endpoint, bucket) {
                (Some(endpoint), Some(bucket)) => {
                    Some(format!("{}/{}", endpoint.trim_end_matches('/'), bucket))
                }
                (Some(endpoint), None) => Some(endpoint),
                (None, Some(bucket)) => Some(bucket),
                (None, None) => None,
            }
        })
        .unwrap_or_default()
}

fn sget(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(|s| s.to_string())
}

fn iopt(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_i64()
}

fn iget(v: &Value, path: &[&str]) -> i64 {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_i64().unwrap_or(0)
}

fn bget(v: &Value, path: &[&str]) -> bool {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return false,
        }
    }
    cur.as_bool().unwrap_or(false)
}

fn count_obj(v: &Value, path: &[&str]) -> usize {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_object().map(|o| o.len()).unwrap_or(0)
}

fn count_arr(v: &Value, path: &[&str]) -> usize {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_array().map(|a| a.len()).unwrap_or(0)
}

/// Compact human age, e.g. `3d4h`, `12m`, `45s`.
pub fn age(obj: &DynamicObject) -> String {
    match age_secs(obj) {
        Some(secs) => humanize(secs),
        None => "<unknown>".into(),
    }
}

/// Age in seconds since creation, or `None` if the object has no timestamp.
/// Used both for the AGE column and for sorting by age.
pub fn age_secs(obj: &DynamicObject) -> Option<i64> {
    let ts = obj.metadata.creation_timestamp.as_ref()?;
    Some((Timestamp::now().as_second() - ts.0.as_second()).max(0))
}

fn timestamp_secs(d: &Value, path: &[&str]) -> Option<i64> {
    sget(d, path)
        .and_then(|s| s.parse::<Timestamp>().ok())
        .map(|ts| ts.as_second())
}

fn time_since(d: &Value, path: &[&str]) -> String {
    timestamp_secs(d, path)
        .map(|secs| humanize((Timestamp::now().as_second() - secs).max(0)))
        .unwrap_or_else(|| "<none>".into())
}

fn job_duration(d: &Value) -> String {
    let Some(start) = timestamp_secs(d, &["status", "startTime"]) else {
        return "<none>".into();
    };
    let end = timestamp_secs(d, &["status", "completionTime"])
        .unwrap_or_else(|| Timestamp::now().as_second());
    humanize((end - start).max(0))
}

fn humanize(secs: i64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        if h > 0 && d < 7 {
            format!("{d}d{h}h")
        } else {
            format!("{d}d")
        }
    } else if h > 0 {
        if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// (ready "n/m", status, restarts) for a pod, approximating kubectl logic.
fn pod_summary(obj: &DynamicObject) -> (String, String, String) {
    let d = &obj.data;
    let empty = vec![];
    let statuses = d
        .get("status")
        .and_then(|s| s.get("containerStatuses"))
        .and_then(|c| c.as_array())
        .unwrap_or(&empty);

    let total = statuses.len();
    let mut ready = 0usize;
    let mut restarts = 0i64;
    let mut waiting_reason: Option<String> = None;
    let mut terminated_reason: Option<String> = None;

    for c in statuses {
        if c.get("ready").and_then(Value::as_bool).unwrap_or(false) {
            ready += 1;
        }
        restarts += c.get("restartCount").and_then(Value::as_i64).unwrap_or(0);
        if let Some(r) = c.pointer("/state/waiting/reason").and_then(Value::as_str)
            && (r != "ContainerCreating" || waiting_reason.is_none())
        {
            waiting_reason = Some(r.to_string());
        }
        if let Some(r) = c
            .pointer("/state/terminated/reason")
            .and_then(Value::as_str)
            && r != "Completed"
        {
            terminated_reason = Some(r.to_string());
        }
    }

    let phase = sget(d, &["status", "phase"]).unwrap_or_else(|| "Unknown".into());
    let status = if obj.metadata.deletion_timestamp.is_some() {
        "Terminating".to_string()
    } else if let Some(r) = waiting_reason {
        r
    } else if let Some(r) = terminated_reason {
        r
    } else {
        phase
    };

    (format!("{ready}/{total}"), status, restarts.to_string())
}

fn external_ip(d: &Value, typ: &str) -> String {
    if let Some(ing) = d
        .pointer("/status/loadBalancer/ingress")
        .and_then(Value::as_array)
    {
        let ips: Vec<String> = ing
            .iter()
            .filter_map(|i| {
                i.get("ip")
                    .or_else(|| i.get("hostname"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();
        if !ips.is_empty() {
            return ips.join(",");
        }
    }
    if typ == "LoadBalancer" {
        "<pending>".into()
    } else {
        "<none>".into()
    }
}

fn svc_ports(d: &Value) -> String {
    d.pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ports| {
            ports
                .iter()
                .map(|p| {
                    let port = p.get("port").and_then(Value::as_i64).unwrap_or(0);
                    let proto = p.get("protocol").and_then(Value::as_str).unwrap_or("TCP");
                    format!("{port}/{proto}")
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "<none>".into())
}

fn node_ready(d: &Value) -> String {
    d.pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .map(|c| {
            if c.get("status").and_then(Value::as_str) == Some("True") {
                "Ready".to_string()
            } else {
                "NotReady".to_string()
            }
        })
        .unwrap_or_else(|| "Unknown".into())
}

fn node_roles(obj: &DynamicObject) -> String {
    let mut roles: Vec<String> = obj
        .metadata
        .labels
        .as_ref()
        .map(|l| {
            l.keys()
                .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                .filter(|r| !r.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    roles.sort();
    if roles.is_empty() {
        "<none>".into()
    } else {
        roles.join(",")
    }
}

fn ingress_hosts(d: &Value) -> String {
    d.pointer("/spec/rules")
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|r| r.get("host").and_then(Value::as_str).map(String::from))
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "*".into())
}

fn count_endpoints(d: &Value) -> String {
    let n: usize = d
        .pointer("/subsets")
        .and_then(Value::as_array)
        .map(|subs| {
            subs.iter()
                .map(|s| {
                    s.get("addresses")
                        .and_then(Value::as_array)
                        .map_or(0, |a| a.len())
                        * s.get("ports")
                            .and_then(Value::as_array)
                            .map_or(1, |p| p.len())
                })
                .sum()
        })
        .unwrap_or(0);
    if n == 0 {
        "<none>".into()
    } else {
        n.to_string()
    }
}

fn event_object(d: &Value) -> String {
    let Some(obj) = d.get("regarding").or_else(|| d.get("involvedObject")) else {
        return "<none>".into();
    };
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or_default();
    let name = obj.get("name").and_then(Value::as_str).unwrap_or_default();
    match (kind.is_empty(), name.is_empty()) {
        (false, false) => format!("{kind}/{name}"),
        (false, true) => kind.to_string(),
        (true, false) => name.to_string(),
        (true, true) => "<none>".into(),
    }
}

fn event_message(d: &Value) -> String {
    sget(d, &["message"])
        .or_else(|| sget(d, &["note"]))
        .unwrap_or_default()
        .replace(['\n', '\r'], " ")
}

fn event_count(d: &Value) -> i64 {
    iopt(d, &["series", "count"])
        .or_else(|| iopt(d, &["deprecatedCount"]))
        .or_else(|| iopt(d, &["count"]))
        .unwrap_or(1)
}

fn hpa_reference(d: &Value) -> String {
    let kind = sget(d, &["spec", "scaleTargetRef", "kind"]).unwrap_or_default();
    let name = sget(d, &["spec", "scaleTargetRef", "name"]).unwrap_or_default();
    match (kind.is_empty(), name.is_empty()) {
        (false, false) => format!("{kind}/{name}"),
        (false, true) => kind,
        (true, false) => name,
        (true, true) => "<none>".into(),
    }
}

fn hpa_targets(d: &Value) -> String {
    if let Some(target) = iopt(d, &["spec", "targetCPUUtilizationPercentage"]) {
        let current = iopt(d, &["status", "currentCPUUtilizationPercentage"])
            .map(|n| format!("{n}%"))
            .unwrap_or_else(|| "<unknown>".into());
        return format!("{current}/{target}%");
    }

    let Some(metrics) = d.pointer("/spec/metrics").and_then(Value::as_array) else {
        return "<none>".into();
    };
    if metrics.is_empty() {
        return "<none>".into();
    }

    let current = d
        .pointer("/status/currentMetrics")
        .and_then(Value::as_array);
    let mut targets: Vec<String> = metrics
        .iter()
        .zip(
            current
                .into_iter()
                .flatten()
                .map(Some)
                .chain(std::iter::repeat(None)),
        )
        .take(3)
        .map(|(metric, current)| hpa_metric(metric, current))
        .collect();
    if metrics.len() > 3 {
        targets.push(format!("+{}", metrics.len() - 3));
    }
    targets.join(",")
}

fn hpa_metric(metric: &Value, current: Option<&Value>) -> String {
    let typ = metric
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("Metric");
    let (path, default_name) = match typ {
        "ContainerResource" => ("containerResource", "container"),
        "External" => ("external", "external"),
        "Object" => ("object", "object"),
        "Pods" => ("pods", "pods"),
        "Resource" => ("resource", "resource"),
        _ => ("", typ),
    };
    let name = if path.is_empty() {
        default_name.to_string()
    } else {
        metric
            .pointer(&format!("/{path}/name"))
            .or_else(|| metric.pointer(&format!("/{path}/metric/name")))
            .and_then(Value::as_str)
            .unwrap_or(default_name)
            .to_string()
    };
    let target = if path.is_empty() {
        None
    } else {
        metric.pointer(&format!("/{path}/target"))
    }
    .map(hpa_metric_value)
    .unwrap_or_else(|| "<target>".into());
    let current = if path.is_empty() {
        None
    } else {
        current.and_then(|m| m.pointer(&format!("/{path}/current")))
    }
    .map(hpa_metric_value)
    .unwrap_or_else(|| "?".into());
    format!("{name}: {current}/{target}")
}

fn hpa_metric_value(metric: &Value) -> String {
    if let Some(n) = metric.get("averageUtilization").and_then(Value::as_i64) {
        format!("{n}%")
    } else if let Some(s) = metric.get("averageValue").and_then(Value::as_str) {
        s.to_string()
    } else if let Some(s) = metric.get("value").and_then(Value::as_str) {
        s.to_string()
    } else {
        "?".into()
    }
}

// ----- metrics quantity parsing/formatting -------------------------------

/// Parse a Kubernetes CPU quantity (e.g. `250m`, `1`, `140736419n`) into
/// millicores.
pub fn parse_cpu_milli(s: &str) -> i64 {
    let s = s.trim();
    let (num, scale) = match s.chars().last() {
        Some('n') => (&s[..s.len() - 1], 1.0 / 1_000_000.0),
        Some('u') => (&s[..s.len() - 1], 1.0 / 1_000.0),
        Some('m') => (&s[..s.len() - 1], 1.0),
        _ => (s, 1000.0),
    };
    (num.parse::<f64>().unwrap_or(0.0) * scale).round() as i64
}

/// Parse a Kubernetes memory quantity (e.g. `512Mi`, `1Gi`, `2000000`) into bytes.
pub fn parse_mem_bytes(s: &str) -> i64 {
    let s = s.trim();
    let suffixes: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0f64.powi(4)),
        ("K", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    for (suf, mult) in suffixes {
        if let Some(num) = s.strip_suffix(suf) {
            return (num.trim().parse::<f64>().unwrap_or(0.0) * mult) as i64;
        }
    }
    s.parse::<f64>().unwrap_or(0.0) as i64
}

pub fn fmt_cpu(milli: i64) -> String {
    if milli <= 0 {
        "-".into()
    } else {
        format!("{milli}m")
    }
}

pub fn fmt_mem(bytes: i64) -> String {
    if bytes <= 0 {
        return "-".into();
    }
    let mi = bytes as f64 / (1024.0 * 1024.0);
    if mi >= 1024.0 {
        format!("{:.1}Gi", mi / 1024.0)
    } else {
        format!("{:.0}Mi", mi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cpu_quantities() {
        assert_eq!(parse_cpu_milli("250m"), 250);
        assert_eq!(parse_cpu_milli("1"), 1000);
        assert_eq!(parse_cpu_milli("2"), 2000);
        assert_eq!(parse_cpu_milli("500000000n"), 500); // nanocores
    }

    #[test]
    fn parses_mem_quantities() {
        assert_eq!(parse_mem_bytes("1Ki"), 1024);
        assert_eq!(parse_mem_bytes("1Mi"), 1024 * 1024);
        assert_eq!(parse_mem_bytes("1Gi"), 1024 * 1024 * 1024);
        assert_eq!(fmt_mem(parse_mem_bytes("512Mi")), "512Mi");
    }

    #[test]
    fn humanize_buckets() {
        assert_eq!(humanize(45), "45s");
        assert_eq!(humanize(90), "1m");
        assert_eq!(humanize(3661), "1h1m");
        assert_eq!(humanize(90_000), "1d1h");
        assert_eq!(humanize(700_000), "8d");
    }

    fn obj(v: serde_json::Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn pod_cells_summarize_status_and_restarts() {
        let p = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"nodeName": "node-1"},
            "status": {
                "phase": "Running",
                "podIP": "10.0.0.5",
                "containerStatuses": [
                    {"ready": true, "restartCount": 2, "state": {"running": {}}},
                    {"ready": false, "restartCount": 0,
                     "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]
            }
        }));
        let (cells, status_idx) = cells(&p, "pods");
        assert_eq!(cells[0], "web");
        assert_eq!(cells[1], "1/2"); // ready
        assert_eq!(cells[2], "CrashLoopBackOff"); // waiting reason overrides phase
        assert_eq!(cells[3], "2"); // total restarts
        assert_eq!(cells[4], "10.0.0.5");
        assert_eq!(cells[5], "node-1");
        assert_eq!(status_idx, Some(2));
    }

    #[test]
    fn service_external_ip_pending() {
        let s = obj(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "lb"},
            "spec": {"type": "LoadBalancer", "clusterIP": "10.1.1.1",
                     "ports": [{"port": 80, "protocol": "TCP"}]},
            "status": {"loadBalancer": {}}
        }));
        let (cells, _) = cells(&s, "services");
        assert_eq!(cells[1], "LoadBalancer");
        assert_eq!(cells[3], "<pending>");
        assert_eq!(cells[4], "80/TCP");
    }

    #[test]
    fn crd_cells_show_group_kind_versions_scope() {
        let crd = obj(json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "widgets.example.com"},
            "spec": {
                "group": "example.com",
                "names": {"plural": "widgets", "kind": "Widget"},
                "scope": "Namespaced",
                "versions": [
                    {"name": "v1beta1", "served": true, "storage": false},
                    {"name": "v1", "served": true, "storage": true}
                ]
            }
        }));
        assert_eq!(
            headers("customresourcedefinitions"),
            vec!["NAME", "GROUP", "KIND", "VERSIONS", "SCOPE", "AGE"]
        );
        let (cells, _) = cells(&crd, "customresourcedefinitions");
        assert_eq!(cells[0], "widgets.example.com");
        assert_eq!(cells[1], "example.com");
        assert_eq!(cells[2], "Widget");
        assert_eq!(cells[3], "v1beta1,v1");
        assert_eq!(cells[4], "Namespaced");
    }

    #[test]
    fn kustomization_cells_show_ready_condition_and_revision() {
        let ks = obj(json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
            "kind": "Kustomization",
            "metadata": {"name": "apps", "namespace": "flux-system"},
            "spec": {"suspend": false},
            "status": {
                "lastAppliedRevision": "main@sha1:abc123",
                "conditions": [
                    {"type": "Reconciling", "status": "False"},
                    {"type": "Ready", "status": "True",
                     "message": "Applied revision: main@sha1:abc123"}
                ]
            }
        }));
        let (cells, status_idx) = cells(&ks, "kustomizations");
        assert_eq!(cells[0], "apps");
        assert_eq!(cells[1], "True");
        assert_eq!(cells[2], "Applied revision: main@sha1:abc123");
        assert_eq!(cells[3], "main@sha1:abc123");
        assert_eq!(cells[4], "false");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn helmrelease_cells_fall_back_to_history_revision() {
        let hr = obj(json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": "podinfo", "namespace": "default"},
            "spec": {"suspend": true},
            "status": {
                "history": [{"chartVersion": "6.5.4"}],
                "conditions": [
                    {"type": "Ready", "status": "False",
                     "message": "install retries exhausted"}
                ]
            }
        }));
        let (cells, status_idx) = cells(&hr, "helmreleases");
        assert_eq!(cells[1], "False");
        assert_eq!(cells[2], "install retries exhausted");
        assert_eq!(cells[3], "6.5.4");
        assert_eq!(cells[4], "true");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn flux_source_cells_show_ready_revision_url() {
        let git = obj(json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": {"name": "apps", "namespace": "flux-system"},
            "spec": {"url": "https://github.com/example/apps", "suspend": true},
            "status": {
                "artifact": {"revision": "main@sha1:abc123"},
                "conditions": [
                    {"type": "Ready", "status": "True", "message": "stored artifact"}
                ]
            }
        }));
        let (cells, status_idx) = cells(&git, "gitrepositories");
        assert_eq!(
            headers("gitrepositories"),
            vec![
                "NAME",
                "READY",
                "MESSAGE",
                "REVISION",
                "URL",
                "SUSPENDED",
                "AGE"
            ]
        );
        assert_eq!(cells[0], "apps");
        assert_eq!(cells[1], "True");
        assert_eq!(cells[2], "stored artifact");
        assert_eq!(cells[3], "main@sha1:abc123");
        assert_eq!(cells[4], "https://github.com/example/apps");
        assert_eq!(cells[5], "true");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn bucket_cells_build_url_from_endpoint_and_bucket_name() {
        let bucket = obj(json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1beta2",
            "kind": "Bucket",
            "metadata": {"name": "charts"},
            "spec": {"endpoint": "https://s3.example.com/", "bucketName": "charts"},
            "status": {
                "artifact": {"revision": "sha256:abc123"},
                "conditions": [{"type": "Ready", "status": "False", "message": "denied"}]
            }
        }));
        let (cells, status_idx) = cells(&bucket, "buckets");
        assert_eq!(cells[1], "False");
        assert_eq!(cells[2], "denied");
        assert_eq!(cells[3], "sha256:abc123");
        assert_eq!(cells[4], "https://s3.example.com/charts");
        assert_eq!(cells[5], "false");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn job_cells_include_duration() {
        let job = obj(json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {"name": "migrate"},
            "spec": {"completions": 3},
            "status": {
                "succeeded": 2,
                "startTime": "2024-01-01T00:00:00Z",
                "completionTime": "2024-01-01T01:05:00Z"
            }
        }));
        let (cells, _) = cells(&job, "jobs");
        assert_eq!(
            headers("jobs"),
            vec!["NAME", "COMPLETIONS", "DURATION", "AGE"]
        );
        assert_eq!(cells[1], "2/3");
        assert_eq!(cells[2], "1h5m");
    }

    #[test]
    fn cronjob_cells_include_last_schedule() {
        let cron = obj(json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "metadata": {"name": "backup"},
            "spec": {"schedule": "*/15 * * * *", "suspend": false},
            "status": {"active": [{"name": "backup-1"}]}
        }));
        let (cells, _) = cells(&cron, "cronjobs");
        assert_eq!(
            headers("cronjobs"),
            vec![
                "NAME",
                "SCHEDULE",
                "SUSPEND",
                "ACTIVE",
                "LAST-SCHEDULE",
                "AGE"
            ]
        );
        assert_eq!(cells[1], "*/15 * * * *");
        assert_eq!(cells[2], "false");
        assert_eq!(cells[3], "1");
        assert_eq!(cells[4], "<none>");
    }

    #[test]
    fn event_cells_show_object_message_and_count() {
        let event = obj(json!({
            "apiVersion": "events.k8s.io/v1",
            "kind": "Event",
            "metadata": {"name": "nginx.abc"},
            "type": "Warning",
            "reason": "BackOff",
            "regarding": {"kind": "Pod", "name": "nginx"},
            "note": "Back-off restarting\nfailed container",
            "series": {"count": 7}
        }));
        let (cells, status_idx) = cells(&event, "events");
        assert_eq!(
            headers("events"),
            vec![
                "NAME", "TYPE", "REASON", "OBJECT", "MESSAGE", "COUNT", "AGE"
            ]
        );
        assert_eq!(cells[1], "Warning");
        assert_eq!(cells[2], "BackOff");
        assert_eq!(cells[3], "Pod/nginx");
        assert_eq!(cells[4], "Back-off restarting failed container");
        assert_eq!(cells[5], "7");
        assert_eq!(status_idx, None);
    }

    #[test]
    fn hpa_cells_show_reference_targets_and_replicas() {
        let hpa = obj(json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": {"name": "web"},
            "spec": {
                "scaleTargetRef": {"kind": "Deployment", "name": "web"},
                "minReplicas": 2,
                "maxReplicas": 10,
                "metrics": [{
                    "type": "Resource",
                    "resource": {
                        "name": "cpu",
                        "target": {"type": "Utilization", "averageUtilization": 80}
                    }
                }]
            },
            "status": {
                "currentReplicas": 4,
                "currentMetrics": [{
                    "type": "Resource",
                    "resource": {
                        "name": "cpu",
                        "current": {"averageUtilization": 42}
                    }
                }]
            }
        }));
        let (cells, _) = cells(&hpa, "horizontalpodautoscalers");
        assert_eq!(
            headers("horizontalpodautoscalers"),
            vec![
                "NAME",
                "REFERENCE",
                "TARGETS",
                "MINPODS",
                "MAXPODS",
                "REPLICAS",
                "AGE"
            ]
        );
        assert_eq!(cells[1], "Deployment/web");
        assert_eq!(cells[2], "cpu: 42%/80%");
        assert_eq!(cells[3], "2");
        assert_eq!(cells[4], "10");
        assert_eq!(cells[5], "4");
    }

    #[test]
    fn flux_object_without_status_reads_unknown() {
        let ks = obj(json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
            "kind": "Kustomization",
            "metadata": {"name": "new"}
        }));
        let (cells, _) = cells(&ks, "kustomizations");
        assert_eq!(cells[1], "Unknown");
        assert_eq!(cells[2], "");
        assert_eq!(cells[3], "");
    }

    #[test]
    fn unknown_kind_falls_back_to_name_age() {
        let o = obj(json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "thingy"}
        }));
        let (cells, idx) = cells(&o, "widgets");
        assert_eq!(cells[0], "thingy");
        assert_eq!(cells.len(), 2);
        assert_eq!(idx, None);
    }

    #[test]
    fn curated_headers_and_cells_stay_aligned() {
        let o = obj(json!({
            "apiVersion": "v1",
            "kind": "Object",
            "metadata": {"name": "sample"}
        }));
        let kinds = [
            "pods",
            "deployments",
            "replicasets",
            "statefulsets",
            "daemonsets",
            "services",
            "nodes",
            "namespaces",
            "configmaps",
            "secrets",
            "jobs",
            "cronjobs",
            "events",
            "horizontalpodautoscalers",
            "persistentvolumeclaims",
            "persistentvolumes",
            "ingresses",
            "endpoints",
            "customresourcedefinitions",
            "kustomizations",
            "helmreleases",
            "gitrepositories",
            "helmrepositories",
            "ocirepositories",
            "buckets",
            "widgets",
        ];

        for kind in kinds {
            let headers = headers(kind);
            let (cells, status_idx) = cells(&o, kind);
            assert_eq!(headers.len(), cells.len(), "{kind} column count");
            if let Some(idx) = status_idx {
                assert!(idx < cells.len(), "{kind} status index");
            }
        }
    }
}
