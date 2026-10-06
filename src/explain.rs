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
    /// Whether `pods` was listed. A failed list is not evidence that no pod
    /// exists.
    pub pods_listed: bool,
    /// Other objects the analysis reads: the Jobs a CronJob owns.
    pub related: &'a [DynamicObject],
    /// Whether `related` was listed.
    pub related_listed: bool,
    /// The cluster's StorageClasses, for a PVC. `None` when they could not be
    /// listed, so a missing class is not mistaken for a deleted one.
    pub storage_classes: Option<&'a [DynamicObject]>,
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
        "jobs" => explain_job(ev, &name, &mut out),
        "cronjobs" => explain_cronjob(ev, &name, &mut out),
        "persistentvolumeclaims" => explain_pvc(ev, &name, &mut out),
        "nodes" => explain_node(ev, &name, &mut out),
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
    let available = ptr_i64(
        d,
        if ds {
            "/status/numberAvailable"
        } else {
            "/status/availableReplicas"
        },
    )
    .unwrap_or(0);

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
    push_unready_pods("Blocking objects", ev.pods, None, out);
}

/// List the pods that are not ready, with their problems, under `heading`.
/// `cap` bounds the list for views that can hold hundreds of pods.
fn push_unready_pods(
    heading: &str,
    pods: &[DynamicObject],
    cap: Option<usize>,
    out: &mut Vec<Finding>,
) {
    let mut blockers: Vec<&DynamicObject> = pods.iter().filter(|p| !pod_is_ready(p)).collect();
    blockers.sort_by_key(|p| {
        (
            p.metadata.namespace.clone().unwrap_or_default(),
            p.metadata.name.clone().unwrap_or_default(),
        )
    });
    if blockers.is_empty() {
        return;
    }
    out.push(Finding::new(0, Level::Heading, heading));
    let shown = cap.unwrap_or(usize::MAX);
    for pod in blockers.iter().take(shown) {
        out.push(pod_line(pod));
        for detail in pod_problems(pod) {
            out.push(Finding::new(2, detail.0, detail.1));
        }
    }
    if blockers.len() > shown {
        out.push(Finding::new(
            1,
            Level::Info,
            format!("… and {} more", blockers.len() - shown),
        ));
    }
}

/// `Pod/name  ready/total  status`, pointing at the pod.
fn pod_line(pod: &DynamicObject) -> Finding {
    let pname = pod.metadata.name.clone().unwrap_or_default();
    let (rdy, total) = pod_ready_counts(pod);
    let status = pod_status(pod);
    Finding::new(
        1,
        pod_level(&status),
        format!("Pod/{pname}  {rdy}/{total}  {status}"),
    )
    .with_target(Target {
        plural: "pods".into(),
        namespace: pod.metadata.namespace.clone(),
        name: pname,
    })
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

// ----- jobs and cronjobs ---------------------------------------------------

/// How a Job ended or stands: its level and a short description.
fn job_state(job: &DynamicObject) -> (Level, String) {
    let is_true = |ty: &str| condition(job, ty).filter(|c| cstr(c, "status") == "True");
    if let Some(c) = is_true("Failed").or_else(|| is_true("FailureTarget")) {
        return (
            Level::Critical,
            format!(
                "failed: {}",
                join_reason(cstr(c, "reason"), cstr(c, "message"))
            ),
        );
    }
    if is_true("Complete").is_some() || is_true("SuccessCriteriaMet").is_some() {
        return (Level::Good, "succeeded".into());
    }
    let suspended = job.data.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true);
    if suspended || is_true("Suspended").is_some() {
        return (Level::Warn, "suspended".into());
    }
    (Level::Info, "running".into())
}

fn explain_job(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let d = &ev.obj.data;
    let active = ptr_i64(d, "/status/active").unwrap_or(0);
    let succeeded = ptr_i64(d, "/status/succeeded").unwrap_or(0);
    let failed = ptr_i64(d, "/status/failed").unwrap_or(0);
    let backoff_limit = ptr_i64(d, "/spec/backoffLimit").unwrap_or(6);
    // Under OnFailure a failing container restarts in place, so the pod
    // stays active and `status.failed` stays put. The Job controller checks
    // the restarts of active pods against backoffLimit separately from the
    // failed pods, so the Job fails when either count reaches it.
    let restarts: i64 = if ptr_str(d, "/spec/template/spec/restartPolicy") == Some("OnFailure") {
        ev.pods
            .iter()
            .filter(|p| !pod_finished(p))
            .flat_map(|p| {
                ["/status/containerStatuses", "/status/initContainerStatuses"]
                    .into_iter()
                    .filter_map(|path| p.data.pointer(path).and_then(Value::as_array))
                    .flatten()
            })
            .filter_map(|cs| cs.get("restartCount").and_then(Value::as_i64))
            .sum()
    } else {
        0
    };
    let retries = failed.max(restarts);
    let (level, state) = job_state(ev.obj);
    let done = matches!(level, Level::Good | Level::Critical);

    let headline = match (level, state.as_str()) {
        (Level::Critical, _) => format!("Job/{name} failed"),
        (Level::Good, _) => format!("Job/{name} completed"),
        (Level::Warn, _) => format!("Job/{name} is suspended"),
        _ if retries > 0 => format!("Job/{name} is retrying after failures"),
        _ => format!("Job/{name} is running"),
    };
    let head_level = if level == Level::Info && retries > 0 {
        Level::Warn
    } else {
        level
    };
    out.push(Finding::new(0, head_level, headline));
    if let Some(reason) = state.strip_prefix("failed: ") {
        out.push(Finding::new(1, Level::Critical, reason));
    }

    out.push(Finding::new(0, Level::Heading, "Progress"));
    let target = match ptr_i64(d, "/spec/completions") {
        Some(c) => format!("succeeded {succeeded}/{c}"),
        None => format!("succeeded {succeeded}"),
    };
    out.push(Finding::new(
        1,
        Level::Info,
        format!("{target} · active {active} · failed {failed}"),
    ));
    if retries > 0 && !done {
        let restarted = if restarts > 0 {
            format!(
                ": {failed} failed pods, {restarts} container restarts in active pods (restartPolicy OnFailure)"
            )
        } else {
            String::new()
        };
        out.push(Finding::new(
            1,
            if retries >= backoff_limit {
                Level::Critical
            } else {
                Level::Warn
            },
            format!("{retries} of {backoff_limit} allowed failures used (backoffLimit){restarted}"),
        ));
    }

    push_unready_pods("Failed or unready pods", ev.pods, None, out);
}

fn explain_cronjob(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let d = &ev.obj.data;
    let suspended = d.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true);
    let mut jobs: Vec<&DynamicObject> = ev.related.iter().collect();
    // Newest first.
    jobs.sort_by_key(|j| std::cmp::Reverse(j.metadata.creation_timestamp.as_ref().map(|t| t.0)));
    let latest = jobs.first().map(|j| (*j, job_state(j)));

    let (level, headline) = if suspended {
        (Level::Warn, format!("CronJob/{name} is suspended"))
    } else if !ev.related_listed {
        (
            Level::Warn,
            format!("CronJob/{name}: its Jobs could not be listed, so its runs are unknown"),
        )
    } else {
        match &latest {
            Some((_, (Level::Critical, _))) => (
                Level::Critical,
                format!("CronJob/{name}: the last run failed"),
            ),
            Some((_, (Level::Good, _))) => (
                Level::Good,
                format!("CronJob/{name} is healthy (the last run succeeded)"),
            ),
            Some(_) => (Level::Info, format!("CronJob/{name} has a run in progress")),
            None => (Level::Info, format!("CronJob/{name} has no runs on record")),
        }
    };
    out.push(Finding::new(0, level, headline));

    out.push(Finding::new(0, Level::Heading, "Schedule"));
    let schedule = ptr_str(d, "/spec/schedule").unwrap_or("?");
    let zone = ptr_str(d, "/spec/timeZone")
        .map(|z| format!(" ({z})"))
        .unwrap_or_default();
    out.push(Finding::new(
        1,
        Level::Info,
        format!("schedule {schedule}{zone}"),
    ));
    let last_scheduled = ptr_str(d, "/status/lastScheduleTime");
    let last_success = ptr_str(d, "/status/lastSuccessfulTime");
    if let Some(t) = last_scheduled {
        out.push(Finding::new(
            1,
            Level::Info,
            format!("last scheduled {}", short_datetime(t)),
        ));
    }
    match last_success {
        Some(t) => out.push(Finding::new(
            1,
            Level::Info,
            format!("last success {}", short_datetime(t)),
        )),
        None if last_scheduled.is_some() => {
            out.push(Finding::new(1, Level::Warn, "no successful run on record"))
        }
        None => {}
    }
    let active = d
        .pointer("/status/active")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if active > 0 && ptr_str(d, "/spec/concurrencyPolicy") == Some("Forbid") {
        out.push(Finding::new(
            1,
            Level::Warn,
            "concurrencyPolicy Forbid: new runs are skipped while a job is active",
        ));
    }

    if !jobs.is_empty() {
        out.push(Finding::new(0, Level::Heading, "Recent jobs"));
        for job in jobs.iter().take(5) {
            let jname = job.metadata.name.clone().unwrap_or_default();
            let (level, state) = job_state(job);
            let created = job
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| short_datetime(&t.0.to_string()))
                .unwrap_or_default();
            out.push(
                Finding::new(1, level, format!("Job/{jname}  {created}  {state}")).with_target(
                    Target {
                        plural: "jobs".into(),
                        namespace: job.metadata.namespace.clone(),
                        name: jname,
                    },
                ),
            );
        }
    }
    if let Some((job, _)) = latest {
        let jname = job.metadata.name.as_deref().unwrap_or_default();
        push_unready_pods(&format!("Pods of Job/{jname}"), ev.pods, None, out);
    }
}

// ----- persistent volume claims --------------------------------------------

fn explain_pvc(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let pvc = ev.obj;
    let d = &pvc.data;
    let phase = ptr_str(d, "/status/phase").unwrap_or("Pending");
    let volume = ptr_str(d, "/spec/volumeName").filter(|v| !v.is_empty());
    let modes: Vec<&str> = d
        .pointer("/spec/accessModes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let label = format!("PersistentVolumeClaim/{name}");

    match phase {
        "Bound" => {
            out.push(Finding::new(0, Level::Good, format!("{label} is bound")));
            let capacity = ptr_str(d, "/status/capacity/storage").unwrap_or("?");
            let class = ptr_str(d, "/spec/storageClassName").unwrap_or("-");
            out.push(Finding::new(
                1,
                Level::Info,
                format!(
                    "volume {} · {capacity} · {} · class {class}",
                    volume.unwrap_or("?"),
                    modes.join(",")
                ),
            ));
            let requested = ptr_str(d, "/spec/resources/requests/storage");
            // A volume larger than requested is fine; only a request the
            // volume has not grown to yet is a pending resize.
            let bytes = |q: &str| crate::views::parse_quantity(q);
            if let Some(requested) = requested
                && let (Some(want), Some(have)) = (bytes(requested), bytes(capacity))
                && want > have
            {
                out.push(Finding::new(
                    1,
                    Level::Warn,
                    format!("requested {requested}, capacity is still {capacity}"),
                ));
            }
        }
        "Lost" => {
            out.push(Finding::new(0, Level::Critical, format!("{label} is Lost")));
            out.push(Finding::new(
                1,
                Level::Critical,
                format!(
                    "its PersistentVolume {} no longer exists; the data is not reachable",
                    volume.unwrap_or("?")
                ),
            ));
        }
        _ => {
            out.push(Finding::new(
                0,
                Level::Critical,
                format!("{label} is Pending"),
            ));
            out.push(Finding::new(0, Level::Heading, "Binding"));
            explain_pvc_binding(ev, volume, out);
        }
    }

    // Resize and modification conditions report trouble when True.
    for cond in conditions(pvc) {
        if cstr(cond, "status") == "True" {
            let ty = cstr(cond, "type");
            let detail = join_reason(cstr(cond, "reason"), cstr(cond, "message"));
            out.push(Finding::new(1, Level::Warn, format!("{ty}: {detail}")));
        }
    }

    if pvc.metadata.deletion_timestamp.is_some() && !ev.pods.is_empty() {
        out.push(Finding::new(
            1,
            Level::Warn,
            "being deleted, but held by pvc-protection until no pod uses it",
        ));
    }

    if ev.pods.is_empty() {
        return;
    }
    out.push(Finding::new(0, Level::Heading, "Used by"));
    let only_one_node = modes.contains(&"ReadWriteOnce")
        && !modes.contains(&"ReadWriteMany")
        && !modes.contains(&"ReadOnlyMany");
    // Finished pods keep naming the claim but no longer hold the volume.
    let mut nodes: Vec<&str> = ev
        .pods
        .iter()
        .filter(|p| !pod_finished(p))
        .filter_map(|p| ptr_str(&p.data, "/spec/nodeName"))
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    if only_one_node && nodes.len() > 1 {
        out.push(Finding::new(
            1,
            Level::Critical,
            format!(
                "ReadWriteOnce claim is used by pods on {} nodes ({}); only one node can attach it",
                nodes.len(),
                nodes.join(", ")
            ),
        ));
    }
    for pod in ev.pods {
        let mut line = pod_line(pod);
        if pod_is_ready(pod) {
            line.level = Level::Info;
        }
        out.push(line);
        if !pod_is_ready(pod) {
            for detail in pod_problems(pod) {
                out.push(Finding::new(2, detail.0, detail.1));
            }
        }
    }
}

/// Why a Pending claim has no volume yet.
fn explain_pvc_binding(ev: &Evidence, volume: Option<&str>, out: &mut Vec<Finding>) {
    let d = &ev.obj.data;
    if let Some(v) = volume {
        out.push(Finding::new(
            1,
            Level::Warn,
            format!("waiting to bind to PersistentVolume {v}"),
        ));
        return;
    }
    let class = match ptr_str(d, "/spec/storageClassName") {
        Some("") => {
            out.push(Finding::new(
                1,
                Level::Warn,
                "storageClassName is empty, so only a matching pre-created PersistentVolume can bind it",
            ));
            return;
        }
        Some(name) => {
            let found = ev.storage_classes.map(|all| {
                all.iter()
                    .find(|sc| sc.metadata.name.as_deref() == Some(name))
            });
            match found {
                Some(None) => {
                    out.push(Finding::new(
                        1,
                        Level::Critical,
                        format!("StorageClass {name} does not exist"),
                    ));
                    return;
                }
                Some(Some(sc)) => Some(sc),
                None => None,
            }
        }
        None => {
            let default = ev.storage_classes.map(|all| {
                all.iter().find(|sc| {
                    sc.metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get("storageclass.kubernetes.io/is-default-class"))
                        .is_some_and(|v| v == "true")
                })
            });
            match default {
                Some(None) => {
                    out.push(Finding::new(
                        1,
                        Level::Critical,
                        "no storageClassName is set and the cluster has no default StorageClass",
                    ));
                    return;
                }
                Some(Some(sc)) => {
                    let name = sc.metadata.name.as_deref().unwrap_or_default();
                    out.push(Finding::new(
                        1,
                        Level::Info,
                        format!("uses the default StorageClass {name}"),
                    ));
                    Some(sc)
                }
                None => None,
            }
        }
    };

    let Some(class) = class else {
        out.push(Finding::new(
            1,
            Level::Warn,
            "no volume provisioned yet (StorageClasses could not be read; see the events below)",
        ));
        return;
    };
    let provisioner = ptr_str(&class.data, "/provisioner").unwrap_or("?");
    let mode = ptr_str(&class.data, "/volumeBindingMode").unwrap_or("Immediate");
    let selected_node = ev
        .obj
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("volume.kubernetes.io/selected-node"));
    let scheduled = selected_node.is_some()
        || ev
            .pods
            .iter()
            .any(|p| ptr_str(&p.data, "/spec/nodeName").is_some());
    if mode == "WaitForFirstConsumer" && !scheduled {
        let text = if !ev.pods_listed {
            "binding waits for a pod that uses the claim (WaitForFirstConsumer); pods could not be listed to check for one"
        } else if ev.pods.is_empty() {
            "binding waits for a pod that uses the claim (WaitForFirstConsumer), and no pod does"
        } else {
            "binding waits for the pod that uses it to be scheduled (WaitForFirstConsumer)"
        };
        out.push(Finding::new(1, Level::Warn, text));
    } else {
        out.push(Finding::new(
            1,
            Level::Warn,
            format!("{provisioner} has not provisioned a volume yet; see the events below"),
        ));
    }
    if let Some(node) = selected_node {
        out.push(Finding::new(
            1,
            Level::Info,
            format!("the scheduler picked node {node} for the volume"),
        ));
    }
}

// ----- nodes ---------------------------------------------------------------

fn explain_node(ev: &Evidence, name: &str, out: &mut Vec<Finding>) {
    let node = ev.obj;
    let d = &node.data;
    match condition(node, "Ready") {
        Some(c) => {
            let since = c
                .get("lastTransitionTime")
                .and_then(Value::as_str)
                .map(|t| format!(" since {}", short_datetime(t)))
                .unwrap_or_default();
            let (level, verb) = match cstr(c, "status") {
                "True" => (Level::Good, format!("is Ready{since}")),
                "False" => (Level::Critical, format!("is NotReady{since}")),
                _ => (
                    Level::Warn,
                    format!("readiness is Unknown{since} (the kubelet stopped reporting)"),
                ),
            };
            out.push(Finding::new(0, level, format!("Node/{name} {verb}")));
            if level != Level::Good {
                let detail = join_reason(cstr(c, "reason"), cstr(c, "message"));
                out.push(Finding::new(1, level, detail));
            }
        }
        None => out.push(Finding::new(
            0,
            Level::Warn,
            format!("Node/{name} reports no Ready condition"),
        )),
    }

    // Pressure conditions report a problem when True, others when False.
    // Unknown is a warning either way.
    for cond in conditions(node) {
        let ty = cstr(cond, "type");
        if ty == "Ready" {
            continue;
        }
        let status = cstr(cond, "status");
        let pressure = matches!(
            ty,
            "MemoryPressure" | "DiskPressure" | "PIDPressure" | "NetworkUnavailable"
        );
        if status == "Unknown" || status == if pressure { "True" } else { "False" } {
            let detail = join_reason(cstr(cond, "reason"), cstr(cond, "message"));
            out.push(Finding::new(1, Level::Warn, format!("{ty}: {detail}")));
        }
    }

    let cordoned = d.pointer("/spec/unschedulable").and_then(Value::as_bool) == Some(true);
    let taints: Vec<&Value> = d
        .pointer("/spec/taints")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    if cordoned || !taints.is_empty() {
        out.push(Finding::new(0, Level::Heading, "Scheduling"));
    }
    if cordoned {
        out.push(Finding::new(
            1,
            Level::Warn,
            "cordoned: new pods are not scheduled here",
        ));
    }
    for taint in taints {
        let key = cstr(taint, "key");
        // The cordon taint repeats the line above.
        if cordoned && key == "node.kubernetes.io/unschedulable" {
            continue;
        }
        let value = cstr(taint, "value");
        let effect = cstr(taint, "effect");
        let text = if value.is_empty() {
            format!("taint {key}:{effect}")
        } else {
            format!("taint {key}={value}:{effect}")
        };
        // Taints the node controller sets mirror a problem with the node.
        let level = if key.starts_with("node.kubernetes.io/") && effect != "PreferNoSchedule" {
            Level::Warn
        } else {
            Level::Info
        };
        out.push(Finding::new(1, level, text));
    }

    let running = ev.pods.iter().filter(|p| !pod_finished(p)).count();
    if let Some(allocatable) = ptr_str(d, "/status/allocatable/pods")
        .and_then(|p| p.parse::<usize>().ok())
        .filter(|_| ev.pods_listed)
    {
        out.push(Finding::new(0, Level::Heading, "Capacity"));
        out.push(Finding::new(
            1,
            if running >= allocatable {
                Level::Critical
            } else {
                Level::Info
            },
            if running >= allocatable {
                format!("pod capacity is full ({running}/{allocatable} pods)")
            } else {
                format!("{running}/{allocatable} pods")
            },
        ));
    }

    let unhealthy: Vec<DynamicObject> = ev
        .pods
        .iter()
        .filter(|p| ptr_str(&p.data, "/status/phase") != Some("Succeeded"))
        .cloned()
        .collect();
    push_unready_pods("Unhealthy pods on this node", &unhealthy, Some(10), out);
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

    // Node pressure conditions report a problem when True; Ready and generic
    // conditions report a problem when False. Unknown remains a warning.
    for cond in conditions(ev.obj) {
        let ty = cstr(cond, "type");
        if ty == "Ready" {
            continue;
        }
        let status = cstr(cond, "status");
        let pressure = ev.plural == "nodes"
            && matches!(
                ty,
                "MemoryPressure" | "DiskPressure" | "PIDPressure" | "NetworkUnavailable"
            );
        if status == "Unknown" || status == if pressure { "True" } else { "False" } {
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
    // Newest first. Deliberately a *stable* sort: events routinely share a
    // timestamp, and preserving the API server's order for those reads better
    // than an arbitrary shuffle.
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

fn pod_finished(pod: &DynamicObject) -> bool {
    matches!(
        ptr_str(&pod.data, "/status/phase"),
        Some("Succeeded" | "Failed")
    )
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
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
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
                text.push_str(&format!(" — {}", one_line(msg)));
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
        (false, false) => format!("{reason}: {}", one_line(msg)),
        (false, true) => reason.to_string(),
        (true, false) => one_line(msg),
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
    one_line(raw)
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

/// `2026-10-06 10:00:05` from an RFC 3339 time.
fn short_datetime(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('Z');
    let trimmed = trimmed.split('.').next().unwrap_or(trimmed);
    trimmed.replacen('T', " ", 1)
}

fn compact_time(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('Z');
    match trimmed.split_once('T') {
        Some((_date, time)) => time.split('.').next().unwrap_or(time).to_string(),
        None => raw.to_string(),
    }
}

use crate::text::one_line;

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
            pods_listed: true,
            related: &[],
            related_listed: true,
            storage_classes: None,
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
    #[test]
    fn node_pressure_polarity_is_separate_from_readiness() {
        for condition in [
            "MemoryPressure",
            "DiskPressure",
            "PIDPressure",
            "NetworkUnavailable",
        ] {
            for state in ["True", "False", "Unknown"] {
                let node = obj(
                    json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "worker"},
                    "status": {"conditions": [{"type": "Ready", "status": "True"},
                        {"type": condition, "status": state, "reason": "ConditionReason"}]}}),
                );
                let findings = explain(&ev("Node", "nodes", &node, &[], &[]));
                assert_eq!(findings[0].level, Level::Good);
                let pressure = findings.iter().find(|f| f.text.starts_with(condition));
                if state == "False" {
                    assert!(pressure.is_none(), "{condition}={state}");
                } else {
                    assert_eq!(pressure.unwrap().level, Level::Warn, "{condition}={state}");
                    assert!(pressure.unwrap().text.contains("ConditionReason"));
                }
            }
        }
        for (state, level) in [("False", Level::Critical), ("Unknown", Level::Warn)] {
            let node = obj(
                json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "worker"},
                "status": {"conditions": [{"type": "Ready", "status": state}]}}),
            );
            assert_eq!(
                explain(&ev("Node", "nodes", &node, &[], &[]))[0].level,
                level
            );
        }
        let custom = obj(
            json!({"metadata": {"name": "custom"}, "status": {"conditions": [
            {"type": "MemoryPressure", "status": "False"}]}}),
        );
        assert!(
            explain(&ev("Custom", "customs", &custom, &[], &[]))
                .iter()
                .any(|f| f.level == Level::Warn && f.text.starts_with("MemoryPressure"))
        );
    }
    #[test]
    fn daemonset_available_count_uses_its_status_field() {
        for available in [Some(3), Some(1), Some(0), None] {
            let mut daemonset = obj(json!({"apiVersion": "apps/v1", "kind": "DaemonSet",
                "metadata": {"name": "agent"}, "status": {"desiredNumberScheduled": 3,
                    "currentNumberScheduled": 3, "updatedNumberScheduled": 3, "numberReady": 3}}));
            if let Some(n) = available {
                daemonset.data["status"]["numberAvailable"] = json!(n);
            }
            let findings = explain(&ev("DaemonSet", "daemonsets", &daemonset, &[], &[]));
            assert_eq!(findings[0].level, Level::Good);
            assert!(
                findings.iter().any(|f| f.text
                    == format!("desired 3 · ready 3 · available {}", available.unwrap_or(0)))
            );
        }
    }

    fn has(f: &[Finding], level: Level, text: &str) -> bool {
        f.iter().any(|x| x.level == level && x.text.contains(text))
    }

    #[test]
    fn failed_job_reports_its_reason_and_failed_pods() {
        let job = obj(json!({"apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "migrate"},
            "spec": {"completions": 1, "backoffLimit": 2},
            "status": {"failed": 3, "conditions": [{"type": "Failed", "status": "True",
                "reason": "BackoffLimitExceeded", "message": "Job has reached the specified backoff limit"}]}}));
        let pod = obj(json!({"metadata": {"name": "migrate-x"},
            "status": {"phase": "Failed", "containerStatuses": [{"name": "app", "ready": false,
                "restartCount": 0, "state": {"terminated": {"reason": "Error", "exitCode": 2}}}]}}));
        let f = explain(&ev("Job", "jobs", &job, std::slice::from_ref(&pod), &[]));
        assert_eq!(f[0].level, Level::Critical);
        assert_eq!(f[0].text, "Job/migrate failed");
        assert!(has(
            &f,
            Level::Critical,
            "BackoffLimitExceeded: Job has reached"
        ));
        assert!(has(&f, Level::Info, "succeeded 0/1 · active 0 · failed 3"));
        assert!(has(&f, Level::Critical, "app: terminated Error (exit 2)"));
        assert!(!f.iter().any(|x| x.text.contains("allowed failures")));
    }

    #[test]
    fn running_job_with_failures_counts_against_its_backoff_limit() {
        let job = obj(json!({"metadata": {"name": "sync"},
            "spec": {"backoffLimit": 4}, "status": {"active": 1, "failed": 2}}));
        let f = explain(&ev("Job", "jobs", &job, &[], &[]));
        assert_eq!(f[0].level, Level::Warn);
        assert_eq!(f[0].text, "Job/sync is retrying after failures");
        assert!(has(&f, Level::Warn, "2 of 4 allowed failures used"));

        let done = obj(json!({"metadata": {"name": "sync"},
            "status": {"succeeded": 1, "conditions": [{"type": "Complete", "status": "True"}]}}));
        assert_eq!(
            explain(&ev("Job", "jobs", &done, &[], &[]))[0].level,
            Level::Good
        );
    }

    #[test]
    fn cronjob_reports_its_last_run_schedule_and_blocked_runs() {
        let cron = obj(json!({"apiVersion": "batch/v1", "kind": "CronJob",
            "metadata": {"name": "report"},
            "spec": {"schedule": "*/5 * * * *", "concurrencyPolicy": "Forbid"},
            "status": {"active": [{"name": "report-3"}],
                "lastScheduleTime": "2026-10-06T10:00:00Z"}}));
        let job = |name: &str, created: &str, status: Value| {
            obj(json!({"metadata": {"name": name, "creationTimestamp": created}, "status": status}))
        };
        let jobs = [
            job(
                "report-1",
                "2026-10-06T09:50:00Z",
                json!({"conditions": [{"type": "Complete", "status": "True"}]}),
            ),
            job(
                "report-2",
                "2026-10-06T09:55:00Z",
                json!({"conditions": [{"type": "Failed", "status": "True", "reason": "DeadlineExceeded"}]}),
            ),
        ];
        let mut e = ev("CronJob", "cronjobs", &cron, &[], &[]);
        e.related = &jobs;
        let f = explain(&e);
        assert_eq!(f[0].level, Level::Critical);
        assert!(f[0].text.contains("the last run failed"));
        assert!(has(&f, Level::Info, "schedule */5 * * * *"));
        assert!(has(&f, Level::Info, "last scheduled 2026-10-06 10:00:00"));
        assert!(has(&f, Level::Warn, "no successful run on record"));
        assert!(has(&f, Level::Warn, "concurrencyPolicy Forbid"));
        let recent: Vec<&Finding> = f.iter().filter(|x| x.text.starts_with("Job/")).collect();
        assert!(recent[0].text.starts_with("Job/report-2"), "newest first");
        assert_eq!(recent[0].target.as_ref().unwrap().plural, "jobs");

        let suspended = obj(json!({"metadata": {"name": "report"},
            "spec": {"schedule": "@daily", "suspend": true}}));
        let f = explain(&ev("CronJob", "cronjobs", &suspended, &[], &[]));
        assert_eq!(f[0].level, Level::Warn);
        assert!(f[0].text.contains("suspended"));
    }

    fn pending_pvc(class: Option<&str>) -> DynamicObject {
        let mut pvc = json!({"metadata": {"name": "data"},
            "spec": {"accessModes": ["ReadWriteOnce"],
                "resources": {"requests": {"storage": "1Gi"}}},
            "status": {"phase": "Pending"}});
        if let Some(class) = class {
            pvc["spec"]["storageClassName"] = json!(class);
        }
        obj(pvc)
    }

    fn storage_class(name: &str, mode: &str, default: bool) -> DynamicObject {
        let mut sc = json!({"metadata": {"name": name}, "provisioner": "ebs.csi.aws.com",
            "volumeBindingMode": mode});
        if default {
            sc["metadata"]["annotations"] =
                json!({"storageclass.kubernetes.io/is-default-class": "true"});
        }
        obj(sc)
    }

    fn explain_pvc_with(
        pvc: &DynamicObject,
        pods: &[DynamicObject],
        classes: Option<&[DynamicObject]>,
    ) -> Vec<Finding> {
        let mut e = ev(
            "PersistentVolumeClaim",
            "persistentvolumeclaims",
            pvc,
            pods,
            &[],
        );
        e.storage_classes = classes;
        explain(&e)
    }

    #[test]
    fn pending_pvc_names_the_missing_storage_class() {
        let classes = [storage_class("gp3", "Immediate", true)];
        let f = explain_pvc_with(&pending_pvc(Some("fast")), &[], Some(&classes));
        assert_eq!(f[0].level, Level::Critical);
        assert!(has(&f, Level::Critical, "StorageClass fast does not exist"));

        let f = explain_pvc_with(&pending_pvc(None), &[], Some(&[]));
        assert!(has(
            &f,
            Level::Critical,
            "the cluster has no default StorageClass"
        ));

        let f = explain_pvc_with(&pending_pvc(None), &[], Some(&classes));
        assert!(has(&f, Level::Info, "uses the default StorageClass gp3"));
        assert!(has(
            &f,
            Level::Warn,
            "ebs.csi.aws.com has not provisioned a volume yet"
        ));

        // Unreadable classes are unknown, never reported missing.
        let f = explain_pvc_with(&pending_pvc(Some("fast")), &[], None);
        assert!(!f.iter().any(|x| x.text.contains("does not exist")));
        assert!(has(&f, Level::Warn, "StorageClasses could not be read"));
    }

    #[test]
    fn wait_for_first_consumer_without_a_pod_is_explained() {
        let classes = [storage_class("local", "WaitForFirstConsumer", false)];
        let f = explain_pvc_with(&pending_pvc(Some("local")), &[], Some(&classes));
        assert!(has(
            &f,
            Level::Warn,
            "WaitForFirstConsumer), and no pod does"
        ));

        let pod = obj(json!({"metadata": {"name": "db-0"},
            "status": {"phase": "Pending", "conditions": [{"type": "PodScheduled",
                "status": "False", "reason": "Unschedulable", "message": "0/3 nodes are available"}]}}));
        let f = explain_pvc_with(
            &pending_pvc(Some("local")),
            std::slice::from_ref(&pod),
            Some(&classes),
        );
        assert!(has(&f, Level::Warn, "pod that uses it to be scheduled"));
        assert!(has(
            &f,
            Level::Critical,
            "not scheduled: Unschedulable: 0/3 nodes"
        ));
    }

    #[test]
    fn bound_rwo_claim_used_on_two_nodes_is_flagged() {
        let pvc = obj(json!({"metadata": {"name": "data"},
            "spec": {"accessModes": ["ReadWriteOnce"], "volumeName": "pv-1",
                "storageClassName": "gp3", "resources": {"requests": {"storage": "2Gi"}}},
            "status": {"phase": "Bound", "capacity": {"storage": "1Gi"}}}));
        let pod = |name: &str, node: &str| {
            obj(
                json!({"metadata": {"name": name}, "spec": {"nodeName": node},
                "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]}}),
            )
        };
        let pods = [pod("a", "node-1"), pod("b", "node-2")];
        let f = explain_pvc_with(&pvc, &pods, None);
        assert_eq!(f[0].level, Level::Good);
        assert!(has(
            &f,
            Level::Info,
            "volume pv-1 · 1Gi · ReadWriteOnce · class gp3"
        ));
        assert!(has(&f, Level::Warn, "requested 2Gi, capacity is still 1Gi"));
        assert!(has(
            &f,
            Level::Critical,
            "used by pods on 2 nodes (node-1, node-2)"
        ));
        assert!(
            f.iter()
                .any(|x| x.text.starts_with("Pod/a") && x.target.is_some())
        );
    }

    #[test]
    fn node_reports_cordon_taints_capacity_and_unhealthy_pods() {
        let node = obj(json!({"metadata": {"name": "worker"},
            "spec": {"unschedulable": true, "taints": [
                {"key": "node.kubernetes.io/unschedulable", "effect": "NoSchedule"},
                {"key": "node.kubernetes.io/disk-pressure", "effect": "NoSchedule"},
                {"key": "gpu", "value": "true", "effect": "NoSchedule"}]},
            "status": {"allocatable": {"pods": "2"}, "conditions": [
                {"type": "Ready", "status": "False", "reason": "KubeletNotReady",
                 "lastTransitionTime": "2026-10-06T09:00:00Z"}]}}));
        let pod = |name: &str, ready: &str| {
            obj(json!({"metadata": {"name": name, "namespace": "default"},
                "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": ready}]}}))
        };
        let pods = [pod("ok", "True"), pod("stuck", "False")];
        let f = explain(&ev("Node", "nodes", &node, &pods, &[]));
        assert_eq!(
            f[0].text,
            "Node/worker is NotReady since 2026-10-06 09:00:00"
        );
        assert_eq!(f[0].level, Level::Critical);
        assert!(has(&f, Level::Warn, "cordoned"));
        assert!(
            !f.iter()
                .any(|x| x.text.contains("node.kubernetes.io/unschedulable"))
        );
        assert!(has(
            &f,
            Level::Warn,
            "taint node.kubernetes.io/disk-pressure:NoSchedule"
        ));
        assert!(has(&f, Level::Info, "taint gpu=true:NoSchedule"));
        assert!(has(&f, Level::Critical, "pod capacity is full (2/2 pods)"));
        assert!(f.iter().any(|x| x.text.starts_with("Pod/stuck")));
        assert!(!f.iter().any(|x| x.text.starts_with("Pod/ok")));
    }

    #[test]
    fn unhealthy_pods_on_a_node_are_capped() {
        let node = obj(json!({"metadata": {"name": "worker"},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]}}));
        let pods: Vec<DynamicObject> = (0..12)
            .map(|i| {
                obj(json!({"metadata": {"name": format!("p{i:02}")},
                "status": {"phase": "Pending"}}))
            })
            .collect();
        let f = explain(&ev("Node", "nodes", &node, &pods, &[]));
        assert_eq!(f.iter().filter(|x| x.text.starts_with("Pod/")).count(), 10);
        assert!(has(&f, Level::Info, "… and 2 more"));
    }

    #[test]
    fn on_failure_restarts_count_against_the_backoff_limit() {
        let job = obj(json!({"metadata": {"name": "sync"},
            "spec": {"backoffLimit": 6, "template": {"spec": {"restartPolicy": "OnFailure"}}},
            "status": {"active": 1}}));
        let pod = obj(json!({"metadata": {"name": "sync-x"},
            "status": {"phase": "Running", "containerStatuses": [{"name": "app", "ready": false,
                "restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}}));
        let f = explain(&ev("Job", "jobs", &job, std::slice::from_ref(&pod), &[]));
        assert_eq!(f[0].text, "Job/sync is retrying after failures");
        assert!(has(
            &f,
            Level::Warn,
            "3 of 6 allowed failures used (backoffLimit): 0 failed pods, 3 container restarts"
        ));

        // Failed pods and restarts are separate limits, and a failed pod's
        // restarts are already in `status.failed`.
        let job = obj(json!({"metadata": {"name": "sync"},
            "spec": {"backoffLimit": 4, "template": {"spec": {"restartPolicy": "OnFailure"}}},
            "status": {"active": 1, "failed": 2}}));
        let restarted = |name: &str, phase: &str, init: i64, main: i64| {
            obj(
                json!({"metadata": {"name": name}, "status": {"phase": phase,
                "initContainerStatuses": [{"name": "setup", "restartCount": init}],
                "containerStatuses": [{"name": "app", "ready": false, "restartCount": main}]}}),
            )
        };
        let pods = [
            restarted("gone", "Failed", 0, 5),
            restarted("live", "Running", 1, 2),
        ];
        let f = explain(&ev("Job", "jobs", &job, &pods, &[]));
        assert!(has(
            &f,
            Level::Warn,
            "3 of 4 allowed failures used (backoffLimit): 2 failed pods, 3 container restarts"
        ));
    }

    #[test]
    fn unlisted_evidence_is_unknown_not_empty() {
        let cron = obj(json!({"metadata": {"name": "report"}, "spec": {"schedule": "@daily"}}));
        let mut e = ev("CronJob", "cronjobs", &cron, &[], &[]);
        e.related_listed = false;
        let f = explain(&e);
        assert!(f[0].text.contains("its Jobs could not be listed"));
        assert!(!f.iter().any(|x| x.text.contains("no runs on record")));

        let classes = [storage_class("local", "WaitForFirstConsumer", false)];
        let pvc = pending_pvc(Some("local"));
        let mut e = ev(
            "PersistentVolumeClaim",
            "persistentvolumeclaims",
            &pvc,
            &[],
            &[],
        );
        e.storage_classes = Some(&classes);
        e.pods_listed = false;
        let f = explain(&e);
        assert!(has(
            &f,
            Level::Warn,
            "pods could not be listed to check for one"
        ));
        assert!(!f.iter().any(|x| x.text.contains("and no pod does")));

        let node = obj(json!({"metadata": {"name": "worker"},
            "status": {"allocatable": {"pods": "110"},
                "conditions": [{"type": "Ready", "status": "True"}]}}));
        let mut e = ev("Node", "nodes", &node, &[], &[]);
        e.pods_listed = false;
        assert!(!explain(&e).iter().any(|x| x.text.contains("pods")));
    }

    #[test]
    fn a_scheduled_consumer_moves_the_blame_to_provisioning() {
        let classes = [storage_class("local", "WaitForFirstConsumer", false)];
        let pod = obj(
            json!({"metadata": {"name": "db-0"}, "spec": {"nodeName": "node-1"},
            "status": {"phase": "Pending"}}),
        );
        let f = explain_pvc_with(
            &pending_pvc(Some("local")),
            std::slice::from_ref(&pod),
            Some(&classes),
        );
        assert!(has(
            &f,
            Level::Warn,
            "ebs.csi.aws.com has not provisioned a volume yet"
        ));
        assert!(!f.iter().any(|x| x.text.contains("to be scheduled")));

        let mut pvc = pending_pvc(Some("local"));
        pvc.metadata.annotations = Some(
            [(
                "volume.kubernetes.io/selected-node".to_string(),
                "node-2".to_string(),
            )]
            .into(),
        );
        let f = explain_pvc_with(&pvc, &[], Some(&classes));
        assert!(has(&f, Level::Warn, "has not provisioned a volume yet"));
        assert!(has(&f, Level::Info, "the scheduler picked node node-2"));
    }

    #[test]
    fn resize_and_attachment_checks_ignore_equal_sizes_and_finished_pods() {
        let bound = |requested: &str, capacity: &str| {
            obj(json!({"metadata": {"name": "data"},
                "spec": {"accessModes": ["ReadWriteOnce"], "volumeName": "pv-1",
                    "resources": {"requests": {"storage": requested}}},
                "status": {"phase": "Bound", "capacity": {"storage": capacity}}}))
        };
        for (requested, capacity) in [("1024Mi", "1Gi"), ("1Gi", "2Gi")] {
            let f = explain_pvc_with(&bound(requested, capacity), &[], None);
            assert!(
                !f.iter().any(|x| x.text.contains("capacity is still")),
                "{requested} {capacity}"
            );
        }
        let pod = |name: &str, node: &str, phase: &str| {
            obj(
                json!({"metadata": {"name": name}, "spec": {"nodeName": node},
                "status": {"phase": phase}}),
            )
        };
        let pods = [
            pod("old", "node-1", "Succeeded"),
            pod("new", "node-2", "Running"),
        ];
        let f = explain_pvc_with(&bound("1Gi", "1Gi"), &pods, None);
        assert!(
            !f.iter()
                .any(|x| x.text.contains("only one node can attach it"))
        );
    }

    #[test]
    fn a_ready_node_says_since_when() {
        let node = obj(json!({"metadata": {"name": "worker"},
            "status": {"conditions": [{"type": "Ready", "status": "True",
                "lastTransitionTime": "2026-10-01T08:00:00Z"}]}}));
        let f = explain(&ev("Node", "nodes", &node, &[], &[]));
        assert_eq!(f[0].text, "Node/worker is Ready since 2026-10-01 08:00:00");
    }
}
