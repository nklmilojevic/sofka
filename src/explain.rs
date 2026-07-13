//! Deterministic "why is this unhealthy?" analysis.
//!
//! Given a selected object plus the evidence gathered around it (its owned
//! pods and recent events), [`explain`] correlates the usual Kubernetes
//! failure modes — stalled rollouts, unschedulable pods, image-pull errors,
//! crash loops, OOM kills, and failing probes — into a ranked list of
//! [`Finding`]s. Every conclusion carries the condition, replica count,
//! container state, or event that supports it, and no external service or AI
//! is involved.
//!
//! This module is pure: it reads `DynamicObject`s and produces findings, so it
//! is unit-tested without a cluster. The app layer ([`crate::app`]) gathers the
//! evidence and renders the findings; [`Finding::target`] lets the view jump
//! straight to the resource behind a line.

use kube::core::DynamicObject;
use serde_json::Value;

/// Severity/role of a finding line, driving its color and whether it reads as a
/// heading, a supporting fact, or a problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Section heading (`Rollout`, `Blocking objects`, `Recent evidence`).
    Heading,
    /// Neutral supporting fact.
    Info,
    /// Everything looks healthy.
    Good,
    /// A degraded-but-not-fatal signal.
    Warn,
    /// A fatal signal — the thing that's actually broken.
    Critical,
    /// A raw event / log line quoted as evidence.
    Evidence,
}

/// A resource a finding points at, so the view can jump straight to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub plural: String,
    pub namespace: Option<String>,
    pub name: String,
}

/// One line of the explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub indent: u8,
    pub level: Level,
    pub text: String,
    /// The resource this line is about, when jumping to it makes sense.
    pub target: Option<Target>,
}

impl Finding {
    fn new(indent: u8, level: Level, text: impl Into<String>) -> Self {
        Finding {
            indent,
            level,
            text: text.into(),
            target: None,
        }
    }

    pub fn with_target(mut self, target: Target) -> Self {
        self.target = Some(target);
        self
    }
}

/// The evidence gathered for one object.
pub struct Evidence<'a> {
    /// Display kind, e.g. `Deployment`.
    pub kind: &'a str,
    /// Lowercased plural, e.g. `deployments`.
    pub plural: &'a str,
    /// The object under investigation.
    pub obj: &'a DynamicObject,
    /// Pods related to the object (a workload's children, or the pod itself).
    pub pods: &'a [DynamicObject],
    /// Recent events regarding the object and its pods.
    pub events: &'a [DynamicObject],
    /// Whether events came from the `events.k8s.io` schema (`note`/`series`).
    pub events_v1: bool,
}

/// Produce the ranked explanation for one object.
pub fn explain(ev: &Evidence) -> Vec<Finding> {
    let name = ev.obj.metadata.name.clone().unwrap_or_default();
    let mut out = Vec::new();
    match ev.plural {
        "deployments" | "statefulsets" | "daemonsets" | "replicasets" => {
            explain_workload(ev, &name, &mut out)
        }
        "pods" => explain_pod(ev, &name, &mut out),
        _ => explain_generic(ev, &name, &mut out),
    }
    append_events(ev, &mut out);
    out
}

// ----- workloads ----------------------------------------------------------

fn explain_workload(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let d = &ev.obj.data;
    let spec_replicas = ptr_i64(d, "/spec/replicas");
    let ready = ptr_i64(d, "/status/readyReplicas").unwrap_or(0);
    let updated = ptr_i64(d, "/status/updatedReplicas").unwrap_or(0);
    let available = ptr_i64(d, "/status/availableReplicas").unwrap_or(0);

    // DaemonSets count differently — desired is scheduled onto matching nodes.
    let (desired, ds) = match ev.plural {
        "daemonsets" => (
            ptr_i64(d, "/status/desiredNumberScheduled").unwrap_or(0),
            true,
        ),
        _ => (spec_replicas.unwrap_or(0), false),
    };
    let ds_ready = ptr_i64(d, "/status/numberReady").unwrap_or(0);
    let ready = if ds { ds_ready } else { ready };

    let healthy = ready >= desired && desired > 0
        || (desired == 0 && ptr_i64(d, "/status/replicas").unwrap_or(0) == 0);

    let headline = if healthy {
        format!("{}/{name} is healthy", ev.kind)
    } else if ready == 0 {
        format!("{}/{name} is unavailable", ev.kind)
    } else {
        format!("{}/{name} is degraded", ev.kind)
    };
    out.push(Finding::new(
        0,
        if healthy {
            Level::Good
        } else {
            Level::Critical
        },
        headline,
    ));

    // Rollout summary.
    out.push(Finding::new(0, Level::Heading, "Rollout"));
    if ds {
        out.push(Finding::new(
            1,
            rollout_level(ready, desired),
            format!("desired {desired} · ready {ready} · available {available}"),
        ));
    } else {
        out.push(Finding::new(
            1,
            rollout_level(ready, desired),
            format!(
                "desired {desired} → updated {updated} → ready {ready} (available {available})"
            ),
        ));
    }

    // A generation lag means the controller hasn't observed the latest spec.
    if let (Some(g), Some(og)) = (
        ev.obj.metadata.generation,
        ptr_i64(d, "/status/observedGeneration"),
    ) && g != og
    {
        out.push(Finding::new(
            1,
            Level::Warn,
            format!("spec generation {g} not yet observed (controller at {og})"),
        ));
    }

    // Degraded/stalled conditions with their reasons.
    for cond in conditions(ev.obj) {
        let ty = cstr(cond, "type");
        let status = cstr(cond, "status");
        let reason = cstr(cond, "reason");
        let msg = cstr(cond, "message");
        let bad = match ty {
            // Available=False, or Progressing=False (stalled) are the two that
            // signal trouble; ReplicaFailure=True likewise.
            "Available" => status == "False",
            "Progressing" => status == "False",
            "ReplicaFailure" => status == "True",
            _ => false,
        };
        if bad {
            let level = if reason == "ProgressDeadlineExceeded" || ty == "Available" {
                Level::Critical
            } else {
                Level::Warn
            };
            let detail = join_reason(reason, msg);
            out.push(Finding::new(1, level, format!("{ty}: {detail}")));
        }
    }

    // Blocking pods: the children that aren't ready, worst first.
    let mut blockers: Vec<&DynamicObject> = ev.pods.iter().filter(|p| !pod_is_ready(p)).collect();
    blockers.sort_by_key(|p| p.metadata.name.clone().unwrap_or_default());
    if !blockers.is_empty() {
        out.push(Finding::new(0, Level::Heading, "Blocking objects"));
        for pod in blockers {
            let pname = pod.metadata.name.clone().unwrap_or_default();
            let (rdy, total) = pod_ready_counts(pod);
            let status = pod_status(pod);
            let mut f = Finding::new(
                1,
                pod_level(&status),
                format!("Pod/{pname}  {rdy}/{total}  {status}"),
            );
            f = f.with_target(Target {
                plural: "pods".into(),
                namespace: pod.metadata.namespace.clone(),
                name: pname,
            });
            out.push(f);
            for detail in pod_problems(pod) {
                out.push(Finding::new(2, detail.0, detail.1));
            }
        }
    }
}

fn rollout_level(ready: i64, desired: i64) -> Level {
    if ready >= desired {
        Level::Info
    } else if ready == 0 {
        Level::Critical
    } else {
        Level::Warn
    }
}

// ----- pods ----------------------------------------------------------------

fn explain_pod(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let pod = ev.obj;
    let (rdy, total) = pod_ready_counts(pod);
    let status = pod_status(pod);
    let healthy = pod_is_ready(pod);
    out.push(Finding::new(
        0,
        if healthy {
            Level::Good
        } else {
            pod_level(&status)
        },
        if healthy {
            format!("Pod/{name} is healthy ({rdy}/{total} ready)")
        } else {
            format!("Pod/{name} is {status} ({rdy}/{total} ready)")
        },
    ));

    out.push(Finding::new(0, Level::Heading, "Containers"));
    let problems = pod_problems(pod);
    if problems.is_empty() {
        out.push(Finding::new(1, Level::Info, "all containers ready"));
    } else {
        for (level, text) in problems {
            out.push(Finding::new(1, level, text));
        }
    }

    // Scheduling / node placement.
    if let Some(node) = ptr_str(&pod.data, "/spec/nodeName") {
        out.push(Finding::new(0, Level::Heading, "Placement"));
        out.push(Finding::new(1, Level::Info, format!("node {node}")));
    }
}

// ----- generic condition-based objects -------------------------------------

fn explain_generic(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let kind = ev.kind;
    let ready = condition(ev.obj, "Ready");
    match ready {
        Some(c) => {
            let status = cstr(c, "status");
            let reason = cstr(c, "reason");
            let msg = cstr(c, "message");
            let (level, verb) = match status {
                "True" => (Level::Good, "is Ready"),
                "False" => (Level::Critical, "is not Ready"),
                _ => (Level::Warn, "readiness is Unknown"),
            };
            out.push(Finding::new(0, level, format!("{kind}/{name} {verb}")));
            let detail = join_reason(reason, msg);
            if !detail.is_empty() {
                out.push(Finding::new(1, level, detail));
            }
        }
        None => {
            out.push(Finding::new(
                0,
                Level::Info,
                format!("{kind}/{name} exposes no Ready condition to assess"),
            ));
        }
    }

    // Any other False/Unknown conditions add detail.
    for cond in conditions(ev.obj) {
        if cstr(cond, "type") == "Ready" {
            continue;
        }
        let status = cstr(cond, "status");
        if status == "False" || status == "Unknown" {
            let ty = cstr(cond, "type");
            let detail = join_reason(cstr(cond, "reason"), cstr(cond, "message"));
            out.push(Finding::new(1, Level::Warn, format!("{ty}: {detail}")));
        }
    }
}

// ----- events --------------------------------------------------------------

fn append_events(ev: &Evidence, out: &mut Vec<Finding>) {
    // Warning events are the useful ones; sort newest first and cap the list.
    let mut warnings: Vec<&DynamicObject> = ev
        .events
        .iter()
        .filter(|e| ptr_str(&e.data, "/type") == Some("Warning"))
        .collect();
    if warnings.is_empty() {
        return;
    }
    // Newest first.
    warnings.sort_by_key(|e| std::cmp::Reverse(event_time(e, ev.events_v1)));
    out.push(Finding::new(0, Level::Heading, "Recent evidence"));
    for e in warnings.into_iter().take(6) {
        let reason = ptr_str(&e.data, "/reason").unwrap_or_default();
        let msg = event_message(e, ev.events_v1);
        let when = compact_time(&event_time(e, ev.events_v1));
        let text = format!("{when}  {reason}: {msg}");
        out.push(Finding::new(1, Level::Evidence, text));
    }
}

// ----- pod diagnostics -----------------------------------------------------

/// The most salient status string for a pod, mirroring the table's STATUS
/// (a container's waiting/terminated reason wins over the phase).
fn pod_status(pod: &DynamicObject) -> String {
    if pod.metadata.deletion_timestamp.is_some() {
        return "Terminating".into();
    }
    for cs in container_statuses(pod) {
        if let Some(r) = ptr_str(cs, "/state/waiting/reason")
            && r != "ContainerCreating"
        {
            return r.to_string();
        }
    }
    for cs in container_statuses(pod) {
        if let Some(r) = ptr_str(cs, "/state/terminated/reason")
            && r != "Completed"
        {
            return r.to_string();
        }
    }
    ptr_str(&pod.data, "/status/phase")
        .unwrap_or("Unknown")
        .to_string()
}

fn pod_ready_counts(pod: &DynamicObject) -> (usize, usize) {
    let statuses = container_statuses(pod);
    let total = statuses.len();
    let ready = statuses
        .iter()
        .filter(|c| c.get("ready").and_then(Value::as_bool) == Some(true))
        .count();
    (ready, total)
}

fn pod_is_ready(pod: &DynamicObject) -> bool {
    // A pod is healthy when its Ready condition is True, or (Succeeded) it
    // completed. Fall back to container readiness when conditions are absent.
    if let Some(c) = condition(pod, "Ready")
        && cstr(c, "status") == "True"
    {
        return true;
    }
    if ptr_str(&pod.data, "/status/phase") == Some("Succeeded") {
        return true;
    }
    let (rdy, total) = pod_ready_counts(pod);
    total > 0 && rdy == total && condition(pod, "Ready").is_none()
}

fn pod_level(status: &str) -> Level {
    match status {
        "CrashLoopBackOff"
        | "ImagePullBackOff"
        | "ErrImagePull"
        | "OOMKilled"
        | "Error"
        | "Evicted"
        | "CreateContainerConfigError"
        | "CreateContainerError"
        | "InvalidImageName" => Level::Critical,
        "Completed" | "Succeeded" => Level::Info,
        _ => Level::Warn,
    }
}

/// Per-container problems for one pod: waiting/terminated reasons, crash-loop
/// last-exit detail, OOM kills, restart counts, and probe failures.
fn pod_problems(pod: &DynamicObject) -> Vec<(Level, String)> {
    let mut out = Vec::new();

    // Unschedulable / pending scheduling shows up on the PodScheduled condition.
    if let Some(c) = condition(pod, "PodScheduled")
        && cstr(c, "status") == "False"
    {
        let detail = join_reason(cstr(c, "reason"), cstr(c, "message"));
        out.push((Level::Critical, format!("not scheduled: {detail}")));
    }

    for cs in container_statuses(pod) {
        let cname = cs.get("name").and_then(Value::as_str).unwrap_or("?");
        let restarts = cs.get("restartCount").and_then(Value::as_i64).unwrap_or(0);
        let ready = cs.get("ready").and_then(Value::as_bool) == Some(true);

        if let Some(reason) = ptr_str(cs, "/state/waiting/reason") {
            if reason == "ContainerCreating" || reason == "PodInitializing" {
                out.push((Level::Info, format!("{cname}: {reason}")));
                continue;
            }
            let msg = ptr_str(cs, "/state/waiting/message").unwrap_or_default();
            // For a crash loop the *last* termination explains the cause.
            let last = last_termination(cs);
            let mut text = format!("{cname}: {reason}");
            if let Some((lreason, code)) = &last
                && reason == "CrashLoopBackOff"
            {
                text.push_str(&format!(
                    " (last: {lreason}, exit {code}, {restarts} restarts)"
                ));
            }
            if !msg.is_empty() && reason != "CrashLoopBackOff" {
                text.push_str(&format!(" — {}", truncate(msg, 100)));
            }
            out.push((pod_level(reason), text));
        } else if let Some(reason) = ptr_str(cs, "/state/terminated/reason") {
            if reason != "Completed" {
                let code = ptr_i64(cs, "/state/terminated/exitCode").unwrap_or(0);
                out.push((
                    pod_level(reason),
                    format!("{cname}: terminated {reason} (exit {code})"),
                ));
            }
        } else if !ready {
            // Running but not ready: almost always a failing readiness probe.
            out.push((
                Level::Warn,
                format!("{cname}: running but not ready (readiness probe failing?)"),
            ));
        } else if restarts > 0 {
            // Healthy now, but it has restarted — surface the OOM/last cause.
            if let Some((lreason, code)) = last_termination(cs) {
                out.push((
                    Level::Warn,
                    format!("{cname}: {restarts} restarts (last: {lreason}, exit {code})"),
                ));
            }
        }
    }
    out
}

/// `(reason, exitCode)` of a container's last termination, if any.
fn last_termination(cs: &Value) -> Option<(String, i64)> {
    let term = cs.pointer("/lastState/terminated")?;
    let reason = term
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("Error")
        .to_string();
    let code = term.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
    Some((reason, code))
}

// ----- small accessors -----------------------------------------------------

fn container_statuses(pod: &DynamicObject) -> Vec<&Value> {
    pod.data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn conditions(obj: &DynamicObject) -> Vec<&Value> {
    obj.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn condition<'a>(obj: &'a DynamicObject, ty: &str) -> Option<&'a Value> {
    conditions(obj)
        .into_iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(ty))
}

fn cstr<'a>(cond: &'a Value, key: &str) -> &'a str {
    cond.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn ptr_str<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    v.pointer(p).and_then(Value::as_str)
}

fn ptr_i64(v: &Value, p: &str) -> Option<i64> {
    v.pointer(p).and_then(Value::as_i64)
}

/// Join a condition's reason and message into `Reason: message`, dropping
/// whichever is empty.
fn join_reason(reason: &str, msg: &str) -> String {
    match (reason.is_empty(), msg.is_empty()) {
        (false, false) => format!("{reason}: {}", truncate(msg, 120)),
        (false, true) => reason.to_string(),
        (true, false) => truncate(msg, 120),
        (true, true) => "(no detail)".into(),
    }
}

fn event_message(e: &DynamicObject, events_v1: bool) -> String {
    let d = &e.data;
    let raw = if events_v1 {
        ptr_str(d, "/note").or_else(|| ptr_str(d, "/message"))
    } else {
        ptr_str(d, "/message").or_else(|| ptr_str(d, "/note"))
    }
    .unwrap_or_default();
    truncate(&raw.replace('\n', " "), 120)
}

fn event_time(e: &DynamicObject, events_v1: bool) -> String {
    let d = &e.data;
    let v = if events_v1 {
        ptr_str(d, "/series/lastObservedTime")
            .or_else(|| ptr_str(d, "/eventTime"))
            .or_else(|| ptr_str(d, "/deprecatedLastTimestamp"))
    } else {
        ptr_str(d, "/lastTimestamp").or_else(|| ptr_str(d, "/eventTime"))
    };
    v.map(String::from)
        .or_else(|| {
            e.metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0.to_string())
        })
        .unwrap_or_default()
}

fn compact_time(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('Z');
    match trimmed.split_once('T') {
        Some((_date, time)) => time.split('.').next().unwrap_or(time).to_string(),
        None => raw.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    fn ev<'a>(
        kind: &'a str,
        plural: &'a str,
        o: &'a DynamicObject,
        pods: &'a [DynamicObject],
        events: &'a [DynamicObject],
    ) -> Evidence<'a> {
        Evidence {
            kind,
            plural,
            obj: o,
            pods,
            events,
            events_v1: false,
        }
    }

    fn texts(f: &[Finding]) -> Vec<String> {
        f.iter().map(|x| x.text.clone()).collect()
    }

    #[test]
    fn healthy_deployment_reads_good() {
        let d = obj(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "api", "generation": 3},
            "spec": {"replicas": 3},
            "status": {"readyReplicas": 3, "updatedReplicas": 3, "availableReplicas": 3,
                       "replicas": 3, "observedGeneration": 3}
        }));
        let f = explain(&ev("Deployment", "deployments", &d, &[], &[]));
        assert_eq!(f[0].level, Level::Good);
        assert!(f[0].text.contains("healthy"));
    }

    #[test]
    fn stalled_rollout_flags_progress_deadline_and_blocking_pods() {
        let d = obj(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "api", "generation": 5},
            "spec": {"replicas": 5},
            "status": {
                "readyReplicas": 2, "updatedReplicas": 3, "availableReplicas": 2,
                "replicas": 5, "observedGeneration": 5,
                "conditions": [
                    {"type": "Available", "status": "False", "reason": "MinimumReplicasUnavailable",
                     "message": "Deployment does not have minimum availability."},
                    {"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded",
                     "message": "ReplicaSet api-7df9 has timed out progressing."}
                ]
            }
        }));
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "api-7df9-r2m9", "namespace": "prod"},
            "status": {
                "phase": "Pending",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{
                    "name": "api", "ready": false, "restartCount": 0,
                    "state": {"waiting": {"reason": "ImagePullBackOff",
                        "message": "Back-off pulling image ghcr.io/acme/api:1.8.2"}}
                }]
            }
        }));
        let f = explain(&ev(
            "Deployment",
            "deployments",
            &d,
            std::slice::from_ref(&pod),
            &[],
        ));
        let all = texts(&f).join("\n");
        assert!(
            f[0].level == Level::Critical && f[0].text.contains("degraded"),
            "{all}"
        );
        assert!(all.contains("desired 5 → updated 3 → ready 2"), "{all}");
        assert!(all.contains("ProgressDeadlineExceeded"), "{all}");
        assert!(all.contains("Available"), "{all}");
        // Blocking pod is listed and carries a jump target.
        let blocker = f.iter().find(|x| x.text.contains("api-7df9-r2m9")).unwrap();
        assert_eq!(
            blocker.target.as_ref().unwrap(),
            &Target {
                plural: "pods".into(),
                namespace: Some("prod".into()),
                name: "api-7df9-r2m9".into()
            }
        );
        assert!(all.contains("ImagePullBackOff"), "{all}");
    }

    #[test]
    fn crashloop_pod_reports_last_exit_and_restarts() {
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "worker-0"},
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{
                    "name": "worker", "ready": false, "restartCount": 7,
                    "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "back-off 5m0s"}},
                    "lastState": {"terminated": {"reason": "Error", "exitCode": 1}}
                }]
            }
        }));
        let f = explain(&ev("Pod", "pods", &pod, &[], &[]));
        let all = texts(&f).join("\n");
        assert!(f[0].level == Level::Critical, "{all}");
        assert!(all.contains("CrashLoopBackOff"), "{all}");
        assert!(all.contains("last: Error, exit 1, 7 restarts"), "{all}");
    }

    #[test]
    fn oom_and_probe_failures_are_detected() {
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "svc-1"},
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [
                    {"name": "app", "ready": false, "restartCount": 0, "state": {"running": {}}},
                    {"name": "cache", "ready": true, "restartCount": 2, "state": {"running": {}},
                     "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}}
                ]
            }
        }));
        let f = explain(&ev("Pod", "pods", &pod, &[], &[]));
        let all = texts(&f).join("\n");
        assert!(all.contains("running but not ready"), "{all}");
        assert!(
            all.contains("OOMKilled") && all.contains("exit 137"),
            "{all}"
        );
    }

    #[test]
    fn unschedulable_pod_is_explained() {
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "big"},
            "status": {
                "phase": "Pending",
                "conditions": [{"type": "PodScheduled", "status": "False",
                    "reason": "Unschedulable",
                    "message": "0/3 nodes are available: 3 Insufficient cpu."}]
            }
        }));
        let f = explain(&ev("Pod", "pods", &pod, &[], &[]));
        let all = texts(&f).join("\n");
        assert!(all.contains("not scheduled"), "{all}");
        assert!(all.contains("Insufficient cpu"), "{all}");
    }

    #[test]
    fn generic_object_uses_ready_condition() {
        let cert = obj(json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "tls"},
            "status": {"conditions": [
                {"type": "Ready", "status": "False", "reason": "Failed",
                 "message": "order errored"}
            ]}
        }));
        let f = explain(&ev("Certificate", "certificates", &cert, &[], &[]));
        assert_eq!(f[0].level, Level::Critical);
        assert!(f[0].text.contains("is not Ready"));
        assert!(texts(&f).join("\n").contains("order errored"));
    }

    #[test]
    fn warning_events_appended_newest_first() {
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}],
                       "containerStatuses": [{"name": "c", "ready": true, "state": {"running": {}}}]}
        }));
        let e1 = obj(
            json!({"type": "Warning", "reason": "BackOff", "message": "older",
            "lastTimestamp": "2026-01-01T14:03:00Z"}),
        );
        let e2 = obj(
            json!({"type": "Warning", "reason": "Failed", "message": "newer",
            "lastTimestamp": "2026-01-01T14:05:00Z"}),
        );
        let e3 = obj(
            json!({"type": "Normal", "reason": "Pulled", "message": "ignored",
            "lastTimestamp": "2026-01-01T14:06:00Z"}),
        );
        let events = [e1, e2, e3];
        let f = explain(&ev("Pod", "pods", &pod, &[], &events));
        let ev_lines: Vec<_> = f.iter().filter(|x| x.level == Level::Evidence).collect();
        assert_eq!(ev_lines.len(), 2, "only Warnings, Normal dropped");
        assert!(ev_lines[0].text.contains("Failed: newer"), "newest first");
        assert!(ev_lines[1].text.contains("BackOff: older"));
    }
}
