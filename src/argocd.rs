//! Argo CD Application state, managed resources, and what blocks a sync.
//!
//! Everything comes from the Application's own `status`: `sync.status` and
//! `health.status`, `sync.revision`, and `status.resources[]` for the objects it
//! manages. No Argo CD API server is involved.
//!
//! `spec.destination` may name a remote cluster, so the managed objects need not
//! be in the cluster being read. [`Destination`] records which, and only
//! [`Destination::Current`] yields jump targets.
//!
//! This module is pure: it reads `DynamicObject`s and produces findings, so it
//! is unit-tested without a cluster. The app layer gathers and renders.

use kube::core::DynamicObject;
use serde_json::Value;

use crate::explain::{Finding, Level, Target};

/// Annotation holding the `spec.syncPolicy.automated` block sofka removed when
/// it suspended an Application, so resume can put it back exactly.
pub const AUTOMATED_STASH: &str = "sofka.io/argocd-automated";

/// Argo's `annotation` tracking method stamps every managed object with
/// `<app>:<group>/<kind>:<namespace>/<name>`.
const TRACKING_ANNOTATION: &str = "argocd.argoproj.io/tracking-id";

/// Argo's `label` tracking method. The key is configurable
/// (`application.instanceLabelKey`); these are the stock and common overrides.
/// Label values cap at 63 characters, truncating longer Application names.
const INSTANCE_LABELS: &[&str] = &["app.kubernetes.io/instance", "argocd.argoproj.io/instance"];

/// Conditions worth leading with, most serious first. Argo reports several at
/// once; the first of these explains the others.
const CONDITION_PRIORITY: &[&str] = &[
    "ComparisonError",
    "InvalidSpecError",
    "UnknownError",
    "SharedResourceWarning",
    "OrphanedResourceWarning",
    "ExcludedResourceWarning",
];

/// How many managed resources to list before summarising the rest.
const MAX_LISTED: usize = 50;

/// Whether an Application syncs by itself, and if not, why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSync {
    /// `spec.syncPolicy.automated` is present.
    On { prune: bool, self_heal: bool },
    /// sofka removed `automated` and still holds the original in [`AUTOMATED_STASH`].
    Suspended,
    /// No `automated` block and nothing stashed. A normal configuration, not a
    /// fault, which is why this is not folded in with [`Self::Suspended`].
    Manual,
}

impl AutoSync {
    /// The cell the table column shows. Static, because this runs for every
    /// visible row of every frame.
    pub fn label(self) -> &'static str {
        match self {
            AutoSync::On {
                prune: true,
                self_heal: true,
            } => "on (prune,selfHeal)",
            AutoSync::On {
                prune: true,
                self_heal: false,
            } => "on (prune)",
            AutoSync::On {
                prune: false,
                self_heal: true,
            } => "on (selfHeal)",
            AutoSync::On { .. } => "on",
            AutoSync::Suspended => "suspended",
            AutoSync::Manual => "manual",
        }
    }
}

/// Where an Application deploys, resolved against the kubeconfig by the app
/// layer. Only [`Self::Current`] produces jump targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Current,
    Context(String),
    /// No kubeconfig context serves it; the raw `server`/`name` is kept so the
    /// view can still name it.
    Unresolved(String),
}

impl Destination {
    fn label(&self) -> String {
        match self {
            Destination::Current => "this cluster".into(),
            Destination::Context(c) => c.clone(),
            Destination::Unresolved(raw) if raw.is_empty() => "unknown".into(),
            Destination::Unresolved(raw) => raw.clone(),
        }
    }

    fn is_current(&self) -> bool {
        matches!(self, Destination::Current)
    }
}

/// One entry of `status.resources[]`: an object the Application manages.
#[derive(Debug, Clone)]
pub struct ManagedResource {
    pub kind: String,
    /// Empty for core. Kind names collide across groups, so resolving a jump on
    /// kind alone would open the wrong object.
    pub group: String,
    pub namespace: String,
    pub name: String,
    /// `Synced` / `OutOfSync` / empty when Argo hasn't compared it yet.
    pub sync: String,
    /// Empty for objects with no health check, such as ConfigMaps.
    pub health: String,
    /// Resolved by the app layer; empty when the cluster does not know the
    /// kind, in which case there is nothing to jump to.
    pub plural: String,
}

impl ManagedResource {
    fn target(&self, dest: &Destination) -> Option<Target> {
        (dest.is_current() && !self.plural.is_empty()).then(|| Target {
            plural: self.plural.clone(),
            namespace: (!self.namespace.is_empty()).then(|| self.namespace.clone()),
            name: self.name.clone(),
        })
    }

    fn level(&self) -> Level {
        match self.health.as_str() {
            "Degraded" | "Missing" => Level::Critical,
            "Progressing" => Level::Warn,
            _ if self.sync == "OutOfSync" => Level::Warn,
            "Healthy" => Level::Good,
            _ => Level::Info,
        }
    }

    fn line(&self) -> String {
        let mut s = format!("{}/{}", self.kind, self.name);
        let state = match (self.sync.as_str(), self.health.as_str()) {
            ("", "") => String::new(),
            (sync, "") => sync.to_string(),
            ("", health) => health.to_string(),
            (sync, health) => format!("{sync} · {health}"),
        };
        if !state.is_empty() {
            s.push_str(&format!(" — {state}"));
        }
        s
    }
}

/// The Application named by a managed object's Argo CD tracking metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRef {
    pub name: String,
    /// The Application's own namespace, when the tracking id carries it. Argo
    /// only writes it for Applications outside its default namespace, so
    /// `None` means "wherever Argo CD lives".
    pub namespace: Option<String>,
    /// True when read from the tracking annotation, which only Argo CD writes.
    /// Helm stamps `app.kubernetes.io/instance` on everything it installs, so an
    /// unmatched label means the object is not Argo's, while an unmatched
    /// annotation is a broken reference.
    pub exact: bool,
}

/// The gathered picture for one Application.
pub struct Evidence {
    /// e.g. `Application/guestbook`.
    pub subject: String,
    /// The object the view was opened from when that was not the Application,
    /// e.g. `Deployment/web`.
    pub via: Option<String>,
    /// `None` alongside a `via` means the object carries no Argo CD tracking.
    pub owner: Option<OwnerRef>,
    /// `None` when it could not be read.
    pub app: Option<DynamicObject>,
    pub destination: Destination,
    /// `spec.destination.namespace`.
    pub destination_namespace: String,
    pub resources: Vec<ManagedResource>,
}

// ----- field accessors ------------------------------------------------------

/// Borrowed, not owned: the fleet dashboard runs these over every Application
/// in a cluster, and there can be thousands.
fn str_at<'a>(d: &'a Value, p: &str) -> &'a str {
    d.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

/// `status.sync.status`: `Synced`, `OutOfSync`, or `Unknown`.
pub fn sync_status(app: &DynamicObject) -> &str {
    str_at(&app.data, "/status/sync/status")
}

/// `status.health.status`: `Healthy`, `Progressing`, `Degraded`, `Missing`.
pub fn health_status(app: &DynamicObject) -> &str {
    str_at(&app.data, "/status/health/status")
}

/// The revision actually deployed. Argo writes it to `status.sync.revision`;
/// mid-operation only the sync result carries it.
pub fn revision(app: &DynamicObject) -> &str {
    match str_at(&app.data, "/status/sync/revision") {
        "" => str_at(&app.data, "/status/operationState/syncResult/revision"),
        rev => rev,
    }
}

/// `spec.project`.
pub fn project(app: &DynamicObject) -> &str {
    str_at(&app.data, "/spec/project")
}

/// The git/helm repository, from a single `spec.source` or the first of
/// `spec.sources` on a multi-source Application.
pub fn repo_url(app: &DynamicObject) -> &str {
    match str_at(&app.data, "/spec/source/repoURL") {
        "" => str_at(&app.data, "/spec/sources/0/repoURL"),
        url => url,
    }
}

/// Whether the Application syncs on its own. See [`AutoSync`] for why this is
/// three states rather than a boolean.
pub fn auto_sync(app: &DynamicObject) -> AutoSync {
    if let Some(automated) = app.data.pointer("/spec/syncPolicy/automated")
        && !automated.is_null()
    {
        let flag = |k: &str| automated.get(k).and_then(Value::as_bool).unwrap_or(false);
        return AutoSync::On {
            prune: flag("prune"),
            self_heal: flag("selfHeal"),
        };
    }
    let stashed = app
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(AUTOMATED_STASH));
    if stashed {
        AutoSync::Suspended
    } else {
        AutoSync::Manual
    }
}

/// The objects the Application manages, straight out of `status.resources[]`.
/// `plural` is left empty here; the app layer fills it in once it has resolved
/// each kind against the cluster.
pub fn managed_resources(app: &DynamicObject) -> Vec<ManagedResource> {
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    app.data
        .pointer("/status/resources")
        .and_then(Value::as_array)
        .map(|rs| {
            rs.iter()
                .map(|r| ManagedResource {
                    kind: s(r, "kind"),
                    group: s(r, "group"),
                    namespace: s(r, "namespace"),
                    name: s(r, "name"),
                    sync: s(r, "status"),
                    health: r
                        .pointer("/health/status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    plural: String::new(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `spec.destination` server URL (preferred) or registered cluster name,
/// for the app layer to resolve against the kubeconfig.
pub fn destination_ref(app: &DynamicObject) -> (&str, &str) {
    (
        str_at(&app.data, "/spec/destination/server"),
        str_at(&app.data, "/spec/destination/name"),
    )
}

/// `spec.destination.namespace`.
pub fn destination_namespace(app: &DynamicObject) -> &str {
    str_at(&app.data, "/spec/destination/namespace")
}

/// The Application managing `obj`, from Argo's tracking metadata. The
/// annotation is preferred: exact, and not subject to the label length cap.
pub fn owner_ref(obj: &DynamicObject) -> Option<OwnerRef> {
    let annotated = obj
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(TRACKING_ANNOTATION))
        // The annotation is `<app>:<group>/<kind>:<namespace>/<name>`; only the
        // first field names the Application.
        .and_then(|id| id.split(':').next())
        .filter(|v| !v.is_empty());
    if let Some(instance) = annotated {
        return Some(parse_instance(instance, true));
    }
    let labels = obj.metadata.labels.as_ref()?;
    let instance = INSTANCE_LABELS
        .iter()
        .find_map(|k| labels.get(*k))
        .filter(|v| !v.is_empty())?;
    Some(parse_instance(instance, false))
}

/// Whether `app` lists this object in `status.resources[]`.
///
/// Two Argo CD instances in one cluster can hold same-named Applications, and
/// the tracking metadata rarely says which namespace to look in.
pub fn manages(app: &DynamicObject, kind: &str, group: &str, namespace: &str, name: &str) -> bool {
    // Reads `status.resources[]` in place: an Application can list hundreds of
    // resources, and this only needs to know whether one of them is ours.
    app.data
        .pointer("/status/resources")
        .and_then(Value::as_array)
        .is_some_and(|rs| {
            rs.iter().any(|r| {
                let f = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default();
                f("kind") == kind && f("group") == group && f("name") == name
                    // Cluster-scoped objects carry no namespace on either side.
                    && f("namespace") == namespace
            })
        })
}

/// Split Argo's instance name into namespace and name.
///
/// Argo writes `<namespace>_<name>` for an Application outside its own
/// namespace, a bare `<name>` otherwise. Kubernetes forbids underscores in both
/// names and namespaces, so the first one is unambiguously the separator.
fn parse_instance(instance: &str, exact: bool) -> OwnerRef {
    match instance.split_once('_') {
        Some((ns, name)) if !ns.is_empty() && !name.is_empty() => OwnerRef {
            name: name.to_string(),
            namespace: Some(ns.to_string()),
            exact,
        },
        _ => OwnerRef {
            name: instance.to_string(),
            namespace: None,
            exact,
        },
    }
}

/// The condition that best explains the Application's state, by
/// [`CONDITION_PRIORITY`], falling back to the first condition present.
fn primary_condition(app: &DynamicObject) -> Option<(String, String)> {
    let conds = app
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?;
    let read = |c: &Value| {
        (
            c.get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            c.get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    };
    for want in CONDITION_PRIORITY {
        if let Some(c) = conds
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some(want))
        {
            return Some(read(c));
        }
    }
    conds.first().map(read)
}

// ----- findings -------------------------------------------------------------

/// Render the Application into ranked findings with jump targets.
pub fn describe(ev: &Evidence) -> Vec<Finding> {
    let mut out = Vec::new();

    let Some(app) = &ev.app else {
        return match (&ev.via, &ev.owner) {
            // Opened from an object carrying no Argo CD tracking at all.
            (Some(via), None) => vec![
                finding(0, Level::Info, format!("{via} is not managed by Argo CD")),
                finding(
                    1,
                    Level::Info,
                    "no tracking-id annotation or instance label found",
                ),
            ],
            // The tracking annotation is written only by Argo, so one that
            // names a missing Application is a genuinely broken reference.
            (Some(via), Some(owner)) if owner.exact => vec![
                finding(
                    0,
                    Level::Warn,
                    format!("{via} is tracked by {}, which was not found", owner.name),
                ),
                finding(
                    1,
                    Level::Info,
                    match &owner.namespace {
                        Some(ns) => format!("looked in namespace {ns}"),
                        None => "looked in every namespace".into(),
                    },
                ),
            ],
            // Only an instance label matched, and no Application by that name
            // claims the object. Helm stamps the same label on everything it
            // installs, so this reads as "not Argo's" rather than as a broken
            // Argo reference.
            (Some(via), Some(owner)) => vec![
                finding(0, Level::Info, format!("{via} is not managed by Argo CD")),
                finding(
                    1,
                    Level::Info,
                    format!(
                        "an instance label names {}, but no Application by that name claims this object (Helm sets that label too)",
                        owner.name
                    ),
                ),
            ],
            _ => vec![finding(0, Level::Warn, format!("{} not found", ev.subject))],
        };
    };

    let sync = sync_status(app);
    let health = health_status(app);

    out.push(finding(
        0,
        headline_level(sync, health),
        headline(ev, sync, health),
    ));

    // Application block.
    out.push(finding(0, Level::Heading, "Application"));
    let project = project(app);
    if !project.is_empty() {
        out.push(finding(1, Level::Info, format!("project {project}")));
    }
    let dest = ev.destination.label();
    let dest_line = if ev.destination_namespace.is_empty() {
        format!("destination {dest}")
    } else {
        format!("destination {dest}/{}", ev.destination_namespace)
    };
    out.push(finding(1, Level::Info, dest_line));
    let auto = auto_sync(app);
    let auto_level = match auto {
        AutoSync::Suspended => Level::Warn,
        _ => Level::Info,
    };
    out.push(finding(
        1,
        auto_level,
        format!("auto-sync: {}", auto.label()),
    ));

    // Source block.
    out.push(finding(0, Level::Heading, "Source"));
    let repo = repo_url(app);
    if repo.is_empty() {
        out.push(finding(1, Level::Info, "no source in spec"));
    } else {
        out.push(finding(1, Level::Info, short(repo)));
    }
    if let Some(line) = source_detail(app) {
        out.push(finding(2, Level::Info, line));
    }
    let rev = revision(app);
    if !rev.is_empty() {
        out.push(finding(
            2,
            Level::Info,
            format!("revision {}", short_revision(rev)),
        ));
    }

    // Managed resources.
    out.push(finding(
        0,
        Level::Heading,
        format!("Managed resources ({})", ev.resources.len()),
    ));
    if ev.resources.is_empty() {
        out.push(finding(1, Level::Info, "none reported"));
    }
    for r in ev.resources.iter().take(MAX_LISTED) {
        let mut f = finding(1, r.level(), r.line());
        if let Some(t) = r.target(&ev.destination) {
            f = f.with_target(t);
        }
        out.push(f);
    }
    if ev.resources.len() > MAX_LISTED {
        out.push(finding(
            1,
            Level::Info,
            format!("… and {} more", ev.resources.len() - MAX_LISTED),
        ));
    }

    // What's blocking, or that nothing is.
    out.push(finding(0, Level::Heading, "Sync"));
    for (level, text) in sync_summary(ev, app, sync, health) {
        out.push(finding(1, level, text));
    }

    out
}

fn headline(ev: &Evidence, sync: &str, health: &str) -> String {
    // Opened from a managed object, the useful headline is the relationship,
    // not the Application alone.
    let subject = match &ev.via {
        Some(via) => format!("{via} is managed by {}", ev.subject),
        None => ev.subject.clone(),
    };
    match (sync, health) {
        ("", "") => format!("{subject} — no status yet"),
        (s, "") => format!("{subject} — {s}"),
        ("", h) => format!("{subject} — {h}"),
        (s, h) => format!("{subject} — {s} · {h}"),
    }
}

fn headline_level(sync: &str, health: &str) -> Level {
    match health {
        "Degraded" | "Missing" => Level::Critical,
        "Progressing" => Level::Warn,
        "Healthy" if sync == "Synced" => Level::Good,
        "Healthy" => Level::Warn,
        _ => Level::Warn,
    }
}

/// `path` / `chart` plus `targetRevision`, whichever the source declares.
fn source_detail(app: &DynamicObject) -> Option<String> {
    let d = &app.data;
    let at = |field: &str| match str_at(d, &format!("/spec/source/{field}")) {
        "" => str_at(d, &format!("/spec/sources/0/{field}")).to_string(),
        v => v.to_string(),
    };
    let mut parts = Vec::new();
    for (label, field) in [
        ("path", "path"),
        ("chart", "chart"),
        ("targetRevision", "targetRevision"),
    ] {
        let value = at(field);
        if !value.is_empty() {
            parts.push(format!("{label} {value}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What is stopping this Application from being synced and healthy, most
/// serious first, or a single line saying nothing is.
fn sync_summary(
    ev: &Evidence,
    app: &DynamicObject,
    sync: &str,
    health: &str,
) -> Vec<(Level, String)> {
    let mut out = Vec::new();

    if auto_sync(app) == AutoSync::Suspended {
        out.push((
            Level::Warn,
            "auto-sync suspended, so this Application will not sync itself".into(),
        ));
    }

    if let Some((kind, message)) = primary_condition(app) {
        let level = match kind.as_str() {
            "ComparisonError" | "InvalidSpecError" | "UnknownError" => Level::Critical,
            _ => Level::Warn,
        };
        out.push((level, join(&kind, &message)));
    }

    let phase = str_at(&app.data, "/status/operationState/phase");
    if matches!(phase, "Failed" | "Error") {
        let msg = str_at(&app.data, "/status/operationState/message");
        out.push((Level::Critical, join(&format!("last sync {phase}"), msg)));
    }

    let broken: Vec<&ManagedResource> = ev
        .resources
        .iter()
        .filter(|r| matches!(r.health.as_str(), "Degraded" | "Missing"))
        .collect();
    for r in broken.iter().take(5) {
        out.push((
            Level::Critical,
            format!("{}/{} is {}", r.kind, r.name, r.health),
        ));
    }
    if broken.len() > 5 {
        out.push((
            Level::Critical,
            format!("… and {} more unhealthy", broken.len() - 5),
        ));
    }

    let drifted: Vec<&ManagedResource> = ev
        .resources
        .iter()
        .filter(|r| r.sync == "OutOfSync")
        .collect();
    for r in drifted.iter().take(5) {
        out.push((Level::Warn, format!("{}/{} is OutOfSync", r.kind, r.name)));
    }
    if drifted.len() > 5 {
        out.push((
            Level::Warn,
            format!("… and {} more OutOfSync", drifted.len() - 5),
        ));
    }

    // Argo can roll a health up from live cluster state it does not publish per
    // resource, leaving nothing above to name.
    if out.is_empty() && matches!(health, "Degraded" | "Missing" | "Progressing") {
        let level = if health == "Progressing" {
            Level::Warn
        } else {
            Level::Critical
        };
        let message = str_at(&app.data, "/status/health/message");
        out.push((
            level,
            join(
                &format!("{health}, but no managed resource reports it"),
                message,
            ),
        ));
    }

    if out.is_empty() {
        let rev = revision(app);
        let at = if rev.is_empty() {
            String::new()
        } else {
            format!(" at {}", short_revision(rev))
        };
        let line = match (sync, health) {
            ("Synced", "Healthy") => format!("synced and healthy{at}"),
            ("", "") => "no status reported yet".to_string(),
            (s, h) => format!("{s} · {h}{at}"),
        };
        let level = if sync == "Synced" && health == "Healthy" {
            Level::Good
        } else {
            Level::Info
        };
        out.push((level, line));
    }

    out
}

fn finding(indent: u8, level: Level, text: impl Into<String>) -> Finding {
    Finding {
        indent,
        level,
        text: text.into(),
        target: None,
    }
}

/// Join a label and a message, dropping the message when empty.
fn join(label: &str, message: &str) -> String {
    if message.is_empty() {
        label.to_string()
    } else {
        format!("{label} — {}", short(message))
    }
}

/// Trim to something that fits on a line.
fn short(s: &str) -> String {
    crate::text::ellipsize(s.trim(), 80)
}

/// Abbreviate a git SHA, leaving chart versions and branch names alone.
fn short_revision(rev: &str) -> String {
    let rev = rev.trim();
    if rev.len() >= 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        rev[..7].to_string()
    } else {
        short(rev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app(value: serde_json::Value) -> DynamicObject {
        serde_json::from_value(value).expect("valid Application")
    }

    /// A Synced/Healthy Application with one managed Deployment.
    fn healthy() -> DynamicObject {
        app(json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Application",
            "metadata": {"name": "guestbook", "namespace": "argocd"},
            "spec": {
                "project": "default",
                "source": {
                    "repoURL": "https://github.com/argoproj/argocd-example-apps",
                    "path": "guestbook",
                    "targetRevision": "HEAD"
                },
                "destination": {"server": "https://kubernetes.default.svc", "namespace": "guestbook"},
                "syncPolicy": {"automated": {"prune": true, "selfHeal": true}}
            },
            "status": {
                "sync": {"status": "Synced", "revision": "53e28ff20cc530b9ada2173fbbd64d48338583ba"},
                "health": {"status": "Healthy"},
                "resources": [
                    {"version": "v1", "kind": "Service", "namespace": "guestbook", "name": "guestbook-ui",
                     "status": "Synced", "health": {"status": "Healthy"}}
                ]
            }
        }))
    }

    fn evidence(obj: DynamicObject, destination: Destination) -> Evidence {
        let mut resources = managed_resources(&obj);
        for r in &mut resources {
            r.plural = format!("{}s", r.kind.to_lowercase());
        }
        Evidence {
            subject: "Application/guestbook".into(),
            via: None,
            owner: None,
            destination_namespace: destination_namespace(&obj).to_string(),
            app: Some(obj),
            destination,
            resources,
        }
    }

    fn texts(findings: &[Finding]) -> Vec<String> {
        findings.iter().map(|f| f.text.clone()).collect()
    }

    #[test]
    fn healthy_application_reports_synced_and_healthy() {
        let ev = evidence(healthy(), Destination::Current);
        let out = describe(&ev);
        assert_eq!(out[0].text, "Application/guestbook — Synced · Healthy");
        assert_eq!(out[0].level, Level::Good);
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "synced and healthy at 53e28ff")
        );
    }

    #[test]
    fn automated_block_reads_as_on_with_its_flags() {
        assert_eq!(
            auto_sync(&healthy()),
            AutoSync::On {
                prune: true,
                self_heal: true
            }
        );
        assert_eq!(
            AutoSync::On {
                prune: true,
                self_heal: true
            }
            .label(),
            "on (prune,selfHeal)"
        );
    }

    /// A missing `automated` block is only "suspended" when sofka stashed it;
    /// otherwise the Application is simply synced by hand.
    #[test]
    fn missing_automated_is_manual_unless_sofka_stashed_it() {
        let mut obj = healthy();
        obj.data["spec"]["syncPolicy"] = json!({});
        assert_eq!(auto_sync(&obj), AutoSync::Manual);

        obj.metadata.annotations = Some(
            [(AUTOMATED_STASH.to_string(), "e30=".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(auto_sync(&obj), AutoSync::Suspended);
    }

    #[test]
    fn suspended_auto_sync_is_called_out_as_blocking() {
        let mut obj = healthy();
        obj.data["spec"]["syncPolicy"] = json!({});
        obj.metadata.annotations = Some(
            [(AUTOMATED_STASH.to_string(), "e30=".to_string())]
                .into_iter()
                .collect(),
        );
        let out = describe(&evidence(obj, Destination::Current));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t.starts_with("auto-sync suspended"))
        );
    }

    #[test]
    fn degraded_resource_is_named_in_the_summary() {
        let mut obj = healthy();
        obj.data["status"]["health"]["status"] = json!("Degraded");
        obj.data["status"]["resources"][0]["health"]["status"] = json!("Degraded");
        let out = describe(&evidence(obj, Destination::Current));
        assert_eq!(out[0].level, Level::Critical);
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "Service/guestbook-ui is Degraded")
        );
    }

    /// Argo computes some health from live state without publishing it per
    /// resource, so the summary must say that rather than restate the headline.
    #[test]
    fn a_degraded_application_no_resource_explains_says_so() {
        let mut obj = healthy();
        obj.data["status"]["health"]["status"] = json!("Degraded");
        obj.data["status"]["resources"][0]["health"] = json!(null);
        let out = describe(&evidence(obj, Destination::Current));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "Degraded, but no managed resource reports it"),
            "{:?}",
            texts(&out)
        );
    }

    #[test]
    fn out_of_sync_resource_is_named_in_the_summary() {
        let mut obj = healthy();
        obj.data["status"]["sync"]["status"] = json!("OutOfSync");
        obj.data["status"]["resources"][0]["status"] = json!("OutOfSync");
        let out = describe(&evidence(obj, Destination::Current));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "Service/guestbook-ui is OutOfSync")
        );
    }

    /// `ComparisonError` outranks the warnings Argo reports alongside it.
    #[test]
    fn comparison_error_wins_over_a_shared_resource_warning() {
        let mut obj = healthy();
        obj.data["status"]["conditions"] = json!([
            {"type": "SharedResourceWarning", "message": "also owned by other-app"},
            {"type": "ComparisonError", "message": "rpc error: code = Unknown"}
        ]);
        let out = describe(&evidence(obj, Destination::Current));
        let summary = texts(&out);
        assert!(
            summary
                .iter()
                .any(|t| t.starts_with("ComparisonError — rpc error"))
        );
        assert!(
            !summary
                .iter()
                .any(|t| t.starts_with("SharedResourceWarning"))
        );
    }

    #[test]
    fn failed_operation_is_reported() {
        let mut obj = healthy();
        obj.data["status"]["operationState"] =
            json!({"phase": "Failed", "message": "one or more objects failed"});
        let out = describe(&evidence(obj, Destination::Current));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "last sync Failed — one or more objects failed")
        );
    }

    /// The whole point of [`Destination`]: a managed resource in another
    /// cluster must not offer a jump that would resolve here.
    #[test]
    fn remote_destination_yields_no_jump_targets() {
        let ev = evidence(healthy(), Destination::Context("sandbox-east".into()));
        let out = describe(&ev);
        assert!(out.iter().all(|f| f.target.is_none()));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "destination sandbox-east/guestbook")
        );
    }

    #[test]
    fn current_destination_yields_a_jump_target() {
        let out = describe(&evidence(healthy(), Destination::Current));
        let target = out.iter().find_map(|f| f.target.clone());
        assert_eq!(
            target,
            Some(Target {
                plural: "services".into(),
                namespace: Some("guestbook".into()),
                name: "guestbook-ui".into()
            })
        );
    }

    #[test]
    fn unresolved_destination_falls_back_to_the_raw_server() {
        let ev = evidence(
            healthy(),
            Destination::Unresolved("https://10.0.1.5".into()),
        );
        let out = describe(&ev);
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "destination https://10.0.1.5/guestbook")
        );
        assert!(out.iter().all(|f| f.target.is_none()));
    }

    #[test]
    fn multi_source_applications_read_the_first_source() {
        let mut obj = healthy();
        obj.data["spec"]["source"] = json!(null);
        obj.data["spec"]["sources"] = json!([
            {"repoURL": "https://example.com/repo", "path": "app", "targetRevision": "v1.2.3"}
        ]);
        assert_eq!(repo_url(&obj), "https://example.com/repo");
        let out = describe(&evidence(obj, Destination::Current));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "path app · targetRevision v1.2.3")
        );
    }

    #[test]
    fn a_missing_application_says_so_instead_of_rendering_blanks() {
        let ev = Evidence {
            subject: "Application/gone".into(),
            via: None,
            owner: None,
            app: None,
            destination: Destination::Current,
            destination_namespace: String::new(),
            resources: Vec::new(),
        };
        let out = describe(&ev);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Application/gone not found");
    }

    /// Chart versions and branch names are not SHAs and must survive intact.
    #[test]
    fn only_full_shas_are_abbreviated() {
        assert_eq!(
            short_revision("53e28ff20cc530b9ada2173fbbd64d48338583ba"),
            "53e28ff"
        );
        assert_eq!(short_revision("1.2.3"), "1.2.3");
        assert_eq!(short_revision("HEAD"), "HEAD");
    }

    fn managed(meta: serde_json::Value) -> DynamicObject {
        let mut m = json!({"name": "web", "namespace": "default"});
        for (k, v) in meta.as_object().expect("object") {
            m[k] = v.clone();
        }
        app(json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": m}))
    }

    #[test]
    fn tracking_annotation_names_the_application() {
        let obj = managed(json!({
            "annotations": {"argocd.argoproj.io/tracking-id": "guestbook:apps/Deployment:default/web"}
        }));
        assert_eq!(
            owner_ref(&obj),
            Some(OwnerRef {
                name: "guestbook".into(),
                namespace: None,
                exact: true
            })
        );
    }

    /// Argo prefixes the namespace only for Applications outside its own, and
    /// neither names nor namespaces may contain `_`, so the split is exact.
    #[test]
    fn tracking_id_carries_the_application_namespace_when_prefixed() {
        let obj = managed(json!({
            "annotations": {"argocd.argoproj.io/tracking-id": "team-a_guestbook:apps/Deployment:default/web"}
        }));
        assert_eq!(
            owner_ref(&obj),
            Some(OwnerRef {
                name: "guestbook".into(),
                namespace: Some("team-a".into()),
                exact: true
            })
        );
    }

    #[test]
    fn instance_label_is_used_when_there_is_no_annotation() {
        for key in ["app.kubernetes.io/instance", "argocd.argoproj.io/instance"] {
            let obj = managed(json!({"labels": {key: "guestbook"}}));
            assert_eq!(
                owner_ref(&obj).map(|o| o.name),
                Some("guestbook".to_string()),
                "{key}"
            );
        }
    }

    /// The annotation is exact; the label is truncated at 63 characters.
    #[test]
    fn the_annotation_wins_over_the_label() {
        let obj = managed(json!({
            "annotations": {"argocd.argoproj.io/tracking-id": "real-app:apps/Deployment:default/web"},
            "labels": {"app.kubernetes.io/instance": "truncated-na"}
        }));
        assert_eq!(owner_ref(&obj).map(|o| o.name), Some("real-app".into()));
    }

    #[test]
    fn an_untracked_object_has_no_owner() {
        assert_eq!(owner_ref(&managed(json!({}))), None);
        assert_eq!(owner_ref(&managed(json!({"labels": {"app": "web"}}))), None);
    }

    #[test]
    fn an_untracked_object_is_reported_as_unmanaged() {
        let ev = Evidence {
            subject: "Application/?".into(),
            via: Some("Deployment/web".into()),
            owner: None,
            app: None,
            destination: Destination::Current,
            destination_namespace: String::new(),
            resources: Vec::new(),
        };
        let out = describe(&ev);
        assert_eq!(out[0].text, "Deployment/web is not managed by Argo CD");
        assert_eq!(out[0].level, Level::Info);
    }

    /// Tracked but dangling: the Application was deleted, or renamed, and the
    /// label was left behind. That is a warning, not "unmanaged".
    #[test]
    fn a_dangling_tracking_reference_is_a_warning() {
        let ev = Evidence {
            subject: "Application/gone".into(),
            via: Some("Deployment/web".into()),
            owner: Some(OwnerRef {
                name: "gone".into(),
                namespace: Some("argocd".into()),
                exact: true,
            }),
            app: None,
            destination: Destination::Current,
            destination_namespace: String::new(),
            resources: Vec::new(),
        };
        let out = describe(&ev);
        assert_eq!(out[0].level, Level::Warn);
        assert_eq!(
            out[0].text,
            "Deployment/web is tracked by gone, which was not found"
        );
        assert_eq!(out[1].text, "looked in namespace argocd");
    }

    /// Helm stamps `app.kubernetes.io/instance` on everything it installs, so a
    /// label that matches no Application must not be reported as a broken Argo
    /// reference.
    #[test]
    fn a_label_only_reference_that_matches_nothing_reads_as_unmanaged() {
        let obj = managed(json!({"labels": {"app.kubernetes.io/instance": "rke2-coredns"}}));
        let owner = owner_ref(&obj).expect("label is read");
        assert!(!owner.exact);

        let ev = Evidence {
            subject: "Application/rke2-coredns".into(),
            via: Some("Deployment/rke2-coredns".into()),
            owner: Some(owner),
            app: None,
            destination: Destination::Current,
            destination_namespace: String::new(),
            resources: Vec::new(),
        };
        let out = describe(&ev);
        assert_eq!(out[0].level, Level::Info);
        assert_eq!(
            out[0].text,
            "Deployment/rke2-coredns is not managed by Argo CD"
        );
        assert!(out[1].text.contains("Helm sets that label too"));
    }

    #[test]
    fn manages_matches_on_kind_group_namespace_and_name() {
        let a = healthy(); // manages core Service/guestbook-ui in ns guestbook
        assert!(manages(&a, "Service", "", "guestbook", "guestbook-ui"));
        assert!(!manages(&a, "Deployment", "", "guestbook", "guestbook-ui"));
        assert!(!manages(&a, "Service", "", "other", "guestbook-ui"));
        assert!(!manages(&a, "Service", "", "guestbook", "other"));
    }

    /// Kind names collide across groups; a Knative Service is not a core one.
    #[test]
    fn manages_does_not_confuse_kinds_from_different_groups() {
        let a = healthy();
        assert!(!manages(
            &a,
            "Service",
            "serving.knative.dev",
            "guestbook",
            "guestbook-ui"
        ));
    }

    /// A cluster-scoped object carries no namespace on either side.
    #[test]
    fn manages_matches_cluster_scoped_objects() {
        let mut a = healthy();
        a.data["status"]["resources"] = json!([
            {"version": "v1", "kind": "ClusterRole", "name": "reader", "status": "Synced"}
        ]);
        assert!(manages(&a, "ClusterRole", "", "", "reader"));
    }

    /// The group from `status.resources[]` reaches the jump target's plural
    /// lookup; losing it is how a Knative Service opens the core one.
    #[test]
    fn managed_resources_keep_their_api_group() {
        let mut a = healthy();
        a.data["status"]["resources"] = json!([
            {"group": "serving.knative.dev", "version": "v1", "kind": "Service",
             "namespace": "guestbook", "name": "web", "status": "Synced"},
            {"version": "v1", "kind": "Service", "namespace": "guestbook",
             "name": "web", "status": "Synced"}
        ]);
        let rs = managed_resources(&a);
        assert_eq!(rs[0].group, "serving.knative.dev");
        assert_eq!(rs[1].group, "");
    }

    #[test]
    fn opening_from_a_managed_object_leads_the_headline_with_the_relationship() {
        let mut ev = evidence(healthy(), Destination::Current);
        ev.via = Some("Deployment/guestbook-ui".into());
        let out = describe(&ev);
        assert_eq!(
            out[0].text,
            "Deployment/guestbook-ui is managed by Application/guestbook — Synced · Healthy"
        );
    }

    #[test]
    fn long_resource_lists_are_capped_with_a_remainder_line() {
        let mut obj = healthy();
        let many: Vec<serde_json::Value> = (0..MAX_LISTED + 3)
            .map(|i| {
                json!({"version": "v1", "kind": "ConfigMap", "namespace": "guestbook",
                       "name": format!("cm-{i}"), "status": "Synced"})
            })
            .collect();
        obj.data["status"]["resources"] = json!(many);
        let out = describe(&evidence(obj, Destination::Current));
        assert!(texts(&out).iter().any(|t| t == "… and 3 more"));
    }
}
