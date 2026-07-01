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
        "jobs" => vec!["NAME", "COMPLETIONS", "AGE"],
        "cronjobs" => vec!["NAME", "SCHEDULE", "SUSPEND", "ACTIVE", "AGE"],
        "persistentvolumeclaims" => vec!["NAME", "STATUS", "VOLUME", "CAPACITY", "AGE"],
        "persistentvolumes" => vec!["NAME", "CAPACITY", "STATUS", "CLAIM", "AGE"],
        "ingresses" => vec!["NAME", "CLASS", "HOSTS", "AGE"],
        "endpoints" => vec!["NAME", "ENDPOINTS", "AGE"],
        "customresourcedefinitions" => vec!["NAME", "GROUP", "KIND", "VERSIONS", "SCOPE", "AGE"],
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
            (vec![name, comp, age], None)
        }
        "cronjobs" => {
            let sched = sget(d, &["spec", "schedule"]).unwrap_or_default();
            let suspend = bget(d, &["spec", "suspend"]).to_string();
            let active = count_arr(d, &["status", "active"]).to_string();
            (vec![name, sched, suspend, active, age], None)
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
        _ => (vec![name, age], None),
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

fn sget(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(|s| s.to_string())
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
}
