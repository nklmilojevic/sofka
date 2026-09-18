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

use std::borrow::Cow;
use std::collections::HashMap;

use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use serde_json::Value;

use crate::columns::humanize;
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
    "SyncError",
    "UnknownError",
    "SharedResourceWarning",
    "OrphanedResourceWarning",
    "RepeatedResourceWarning",
];

/// How many managed resources to list before summarising the rest.
pub(crate) const MAX_LISTED: usize = 50;

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
            AutoSync::On { .. } => "on",
            AutoSync::Suspended => "suspended",
            AutoSync::Manual => "manual",
        }
    }

    /// The view has room for the flags the column leaves out.
    pub fn detail(self) -> &'static str {
        match self {
            AutoSync::On {
                prune: true,
                self_heal: true,
            } => "on (prune, selfHeal)",
            AutoSync::On {
                prune: true,
                self_heal: false,
            } => "on (prune)",
            AutoSync::On {
                prune: false,
                self_heal: true,
            } => "on (selfHeal)",
            other => other.label(),
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

    /// `Kind/name`, or `Kind.group/name` when another managed resource would
    /// otherwise render the same text.
    fn line(&self, qualified: bool) -> String {
        let mut s = if qualified && !self.group.is_empty() {
            format!("{}.{}/{}", self.kind, self.group, self.name)
        } else {
            format!("{}/{}", self.kind, self.name)
        };
        let state = match (self.sync.as_str(), self.health.as_str()) {
            ("", "") => String::new(),
            (sync, "") => sync.to_string(),
            ("", health) => health.to_string(),
            (sync, health) => format!("{sync} / {health}"),
        };
        if !state.is_empty() {
            s.push_str(&format!(": {state}"));
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
    // A multi-source Application records one revision per source, in
    // `spec.sources` order, under the plural field and nothing under the
    // singular one. `revisions[0]` therefore matches the source shown.
    for path in [
        "/status/sync/revision",
        "/status/sync/revisions/0",
        "/status/operationState/syncResult/revision",
        "/status/operationState/syncResult/revisions/0",
    ] {
        let rev = str_at(&app.data, path);
        if !rev.is_empty() {
            return rev;
        }
    }
    ""
}

/// `spec.project`.
pub fn project(app: &DynamicObject) -> &str {
    str_at(&app.data, "/spec/project")
}

/// One source an Application deploys from, with the revision deployed from it.
pub struct Source {
    pub repo_url: String,
    /// `path`, `chart`, `ref` and `targetRevision`, whichever the source sets.
    pub detail: String,
    pub revision: String,
}

/// Every source, in `spec.sources` order.
///
/// A multi-source Application pairs `spec.sources[i]` with
/// `status.sync.revisions[i]`, so a chart and the repository holding its values
/// each show where they came from instead of only the first one appearing.
pub fn sources(app: &DynamicObject) -> Vec<Source> {
    let d = &app.data;
    let url = |v: &Value| {
        v.get("repoURL")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let listed = d
        .pointer("/spec/sources")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty());
    if let Some(list) = listed {
        // Mid-operation the revisions can exist only in the sync result, which
        // is the same fallback `revision` makes. Without it the first source
        // would show one and the rest none.
        let revisions = d
            .pointer("/status/sync/revisions")
            .or_else(|| d.pointer("/status/operationState/syncResult/revisions"))
            .and_then(Value::as_array);
        return list
            .iter()
            .enumerate()
            .map(|(i, source)| Source {
                repo_url: url(source),
                detail: source_detail(source),
                revision: revisions
                    .and_then(|r| r.get(i))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect();
    }
    match d.pointer("/spec/source") {
        Some(source) => vec![Source {
            repo_url: url(source),
            detail: source_detail(source),
            revision: revision(app).to_string(),
        }],
        None => Vec::new(),
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

/// `status.conditions[]` on an ApplicationSet: `ErrorOccurred` wins over
/// `ResourcesUpToDate` when both are stale, and the words match ones the
/// rest of the app already colors (`Error` red, `Synced` green) rather than
/// invent a status vocabulary of one.
///
/// `ResourcesUpToDate` is the condition *type*; the controller reuses the
/// name `ApplicationSetUpToDate` as its *reason*, which reads like a type at
/// a glance but never appears in `type` itself.
pub fn applicationset_status(appset: &DynamicObject) -> &'static str {
    let is_true = |t: &str| {
        appset
            .data
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.get("type").and_then(Value::as_str) == Some(t)
                        && c.get("status").and_then(Value::as_str) == Some("True")
                })
            })
    };
    if is_true("ErrorOccurred") {
        "Error"
    } else if is_true("ResourcesUpToDate") {
        "Synced"
    } else {
        "Unknown"
    }
}

/// The `ErrorOccurred` condition's message, when that is why the status reads
/// `Error`.
fn applicationset_error(appset: &DynamicObject) -> Option<&str> {
    appset
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| {
            c.get("type").and_then(Value::as_str) == Some("ErrorOccurred")
                && c.get("status").and_then(Value::as_str) == Some("True")
        })?
        .get("message")
        .and_then(Value::as_str)
}

/// Which generator types are configured, in `spec.generators` order. `matrix`
/// and `merge` generators wrap others, so those unwrap one level to name what
/// they combine instead of reporting "matrix" for every cluster this produces.
pub fn generators(appset: &DynamicObject) -> Vec<String> {
    appset
        .data
        .pointer("/spec/generators")
        .and_then(Value::as_array)
        .map(|gens| gens.iter().flat_map(generator_names).collect())
        .unwrap_or_default()
}

fn generator_names(generator: &Value) -> Vec<String> {
    let Some(obj) = generator.as_object() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for (key, val) in obj {
        if key == "selector" {
            continue;
        }
        if matches!(key.as_str(), "matrix" | "merge")
            && let Some(nested) = val.get("generators").and_then(Value::as_array)
        {
            names.extend(nested.iter().flat_map(generator_names));
        } else {
            names.push(key.clone());
        }
    }
    names
}

/// Findings for an ApplicationSet opened directly. sofka evaluates no
/// generator itself: `status.resources[]` is Argo's own record of every
/// Application the generators produced, one entry per app in the same shape
/// `managed_resources` reads for an Application's own managed objects.
pub fn describe_applicationset(
    appset: &DynamicObject,
    subject: &str,
    resources: &[ManagedResource],
) -> Vec<Finding> {
    let mut out = Vec::new();

    let status = applicationset_status(appset);
    let level = match status {
        "Error" => Level::Critical,
        "Synced" => Level::Good,
        _ => Level::Info,
    };
    out.push(finding(0, level, format!("{subject}: {status}")));
    if let Some(msg) = applicationset_error(appset) {
        out.push(finding(1, Level::Critical, msg.to_string()));
    }

    out.push(finding(0, Level::Heading, "Generators"));
    let gens = generators(appset);
    if gens.is_empty() {
        out.push(finding(1, Level::Info, "none configured"));
    } else {
        out.push(finding(1, Level::Info, gens.join(", ")));
    }

    out.push(finding(
        0,
        Level::Heading,
        format!("Applications ({})", resources.len()),
    ));
    if resources.is_empty() {
        out.push(finding(1, Level::Info, "none generated"));
    }
    let mut seen: HashMap<(&str, &str, &str), usize> = HashMap::new();
    for r in resources {
        *seen
            .entry((r.kind.as_str(), r.namespace.as_str(), r.name.as_str()))
            .or_default() += 1;
    }
    for r in resources.iter().take(MAX_LISTED) {
        let qualified = seen
            .get(&(r.kind.as_str(), r.namespace.as_str(), r.name.as_str()))
            .is_some_and(|n| *n > 1);
        let mut f = finding(1, r.level(), r.line(qualified));
        // The Applications an ApplicationSet produces are always objects in
        // the same cluster the ApplicationSet itself was just read from.
        if let Some(t) = r.target(&Destination::Current) {
            f = f.with_target(t);
        }
        out.push(f);
    }
    if resources.len() > MAX_LISTED {
        out.push(finding(
            1,
            Level::Info,
            format!("… and {} more", resources.len() - MAX_LISTED),
        ));
    }

    out
}

/// The `spec.destination` server URL (preferred) or registered cluster name,
/// for the app layer to resolve against the kubeconfig.
pub fn destination_ref(app: &DynamicObject) -> (&str, &str) {
    (
        str_at(&app.data, "/spec/destination/server"),
        str_at(&app.data, "/spec/destination/name"),
    )
}

/// The kubeconfig context serving a `spec.destination.name`, from
/// `(context name, cluster entry name)` pairs in kubeconfig order.
///
/// Argo registers a cluster under a free-form name, and a kubeconfig rarely
/// spells the context the same way. Three tiers, first hit wins: a context
/// named exactly `name`; a context whose cluster entry is named `name`; a
/// context whose cluster entry ends in `/name` — the EKS shape, where
/// `aws eks update-kubeconfig` names the entry by ARN
/// (`arn:aws:eks:…:cluster/eks-dev-general`) while Argo holds the bare
/// cluster name. Within a tier the first context in file order wins, so a
/// short alias and the full name pointing at one cluster both resolve.
///
/// Two guards. The ARN tail is only trusted when exactly one cluster entry
/// carries it — the same bare name in two accounts or regions is ambiguous,
/// and guessing would send `:ctx` to the wrong cluster. And Argo's reserved
/// `in-cluster` is matched by an exact context name only: a cluster entry
/// that happens to be called that must not turn the local Application remote.
pub fn context_for_name(name: &str, contexts: &[(String, String)]) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let first = |pred: &dyn Fn(&str, &str) -> bool| {
        contexts
            .iter()
            .find(|(ctx, cluster)| pred(ctx, cluster))
            .map(|(ctx, _)| ctx.clone())
    };
    let exact = first(&|ctx, _| ctx == name);
    if exact.is_some() || name == "in-cluster" {
        return exact;
    }
    first(&|_, cluster| cluster == name).or_else(|| {
        let mut tails = contexts
            .iter()
            .filter(|(_, cluster)| cluster.rsplit('/').next() == Some(name))
            .map(|(_, cluster)| cluster.as_str());
        let only = tails.next()?;
        if tails.any(|c| c != only) {
            return None;
        }
        first(&|_, cluster| cluster == only)
    })
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

/// Whether `obj` names `uid` among its `ownerReferences`.
///
/// `status.resources[]` is flat: it holds what the Application applies, not what
/// those objects went on to create. The parent/child edges exist only on the
/// children themselves.
pub fn owned_by(obj: &DynamicObject, uid: &str) -> bool {
    !uid.is_empty()
        && obj
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|refs| refs.iter().any(|r| r.uid == uid))
}

/// A one-word state for a descendant, or empty when the kind has nothing worth
/// summarising on a tree line.
pub fn descendant_state(obj: &DynamicObject) -> Cow<'_, str> {
    // A pod stuck pulling or crash-looping reports `Running`-adjacent phases
    // that hide the reason, so the waiting reason wins when there is one.
    if let Some(reason) = waiting_reason(obj) {
        return Cow::Borrowed(reason);
    }
    let phase = str_at(&obj.data, "/status/phase");
    if !phase.is_empty() {
        return Cow::Borrowed(phase);
    }
    if let Some(state) = job_state(obj) {
        return Cow::Borrowed(state);
    }
    // Desired comes from the spec: `status.replicas` is what exists right now,
    // so a scaling ReplicaSet would otherwise read as fully ready.
    let desired = obj
        .data
        .pointer("/spec/replicas")
        .or_else(|| obj.data.pointer("/status/replicas"))
        .and_then(Value::as_i64);
    let ready = obj
        .data
        .pointer("/status/readyReplicas")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    match desired {
        Some(0) | None => Cow::Borrowed(""),
        Some(n) => Cow::Owned(format!("{ready}/{n} ready")),
    }
}

/// Whether `spec.replicas` is explicitly zero. An absent value defaults to one,
/// and list responses omit per-item kinds, so the caller supplies the kind.
pub fn scaled_to_zero(obj: &DynamicObject) -> bool {
    obj.data.pointer("/spec/replicas").and_then(Value::as_i64) == Some(0)
}

/// The first container waiting reason, e.g. `CrashLoopBackOff`.
fn waiting_reason(obj: &DynamicObject) -> Option<&str> {
    obj.data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|c| c.pointer("/state/waiting/reason").and_then(Value::as_str))
        .filter(|r| !r.is_empty())
}

/// `Complete` or `Failed` from a Job's conditions.
///
/// Searched by type rather than by position: the job controller holds the
/// terminal condition back until every pod is gone and marks the outcome with
/// `SuccessCriteriaMet` or `FailureTarget` meanwhile, so on any supported
/// Kubernetes the first true condition is not the one that says how it ended.
///
/// `FailureTarget` and `SuccessCriteriaMet` are surfaced too, mapped the same
/// way `col_job_status` maps them for the Jobs table: a Job can sit in that
/// delay for a while, and the search must see a failure the moment the
/// controller commits to it rather than wait for pod cleanup to finish.
fn job_state(obj: &DynamicObject) -> Option<&str> {
    let conditions = obj
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?;
    let is_true = |kind: &str| {
        conditions.iter().any(|c| {
            c.get("type").and_then(Value::as_str) == Some(kind)
                && c.get("status").and_then(Value::as_str) == Some("True")
        })
    };
    if is_true("Failed") || is_true("FailureTarget") {
        Some("Failed")
    } else if is_true("Complete") {
        Some("Complete")
    } else if is_true("SuccessCriteriaMet") {
        Some("Completing")
    } else {
        None
    }
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
    // Deliberately no fallback: Argo also records informational conditions, and
    // surfacing one as a blocker would hide "synced and healthy".
    None
}

// ----- findings -------------------------------------------------------------

/// Render the Application into ranked findings with jump targets.
pub fn describe(ev: &Evidence, now: i64) -> Vec<Finding> {
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
        headline(ev, app, sync, health, now),
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
        format!("auto-sync: {}", auto.detail()),
    ));
    let sources = sources(app);
    // Shown per source when they each carry one, so it would only be repeated
    // here. Without them this is the only place a revision appears at all.
    let per_source_revisions = sources.len() > 1 && sources.iter().any(|s| !s.revision.is_empty());
    let rev = revision(app);
    if !per_source_revisions && !rev.is_empty() {
        out.push(finding(
            1,
            Level::Info,
            format!("revision {}", short_revision(rev)),
        ));
    }

    // Source block.
    out.push(finding(0, Level::Heading, "Source"));
    if sources.is_empty() {
        out.push(finding(1, Level::Info, "no source in spec"));
    }
    for source in &sources {
        out.push(finding(1, Level::Info, short(&source.repo_url)));
        let mut detail = source.detail.clone();
        // With one source the revision is already on the Application block. With
        // several there is no single deployed revision, so each carries its own.
        if sources.len() > 1 && !source.revision.is_empty() {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(&format!("revision {}", short_revision(&source.revision)));
        }
        if !detail.is_empty() {
            out.push(finding(2, Level::Info, detail));
        }
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
    // Two managed resources can differ only by API group, and would otherwise
    // be two identical-looking rows.
    let mut seen: HashMap<(&str, &str, &str), usize> = HashMap::new();
    for r in &ev.resources {
        *seen
            .entry((r.kind.as_str(), r.namespace.as_str(), r.name.as_str()))
            .or_default() += 1;
    }
    for r in ev.resources.iter().take(MAX_LISTED) {
        let qualified = seen
            .get(&(r.kind.as_str(), r.namespace.as_str(), r.name.as_str()))
            .is_some_and(|n| *n > 1);
        let mut f = finding(1, r.level(), r.line(qualified));
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
    out.push(finding(0, Level::Heading, "Blocking"));
    for (level, text) in sync_summary(ev, app, sync, health) {
        out.push(finding(1, level, text));
    }

    out
}

fn headline(ev: &Evidence, app: &DynamicObject, sync: &str, health: &str, now: i64) -> String {
    // Opened from a managed object, the useful headline is the relationship,
    // not the Application alone.
    let subject = match &ev.via {
        Some(via) => format!("{via} is managed by {}", ev.subject),
        None => ev.subject.clone(),
    };
    let base = match (sync, health) {
        ("", "") => format!("{subject}: no status yet"),
        (s, "") => format!("{subject}: {s}"),
        ("", h) => format!("{subject}: {h}"),
        (s, h) => format!("{subject}: {s} / {h}"),
    };
    // A degraded Application that just broke and one that has sat broken for
    // a week look identical without this — Argo updates the timestamp on
    // every health transition, healthy or not, so it always names how long
    // the *current* state has held.
    match health_since(app, now) {
        Some(since) if !health.is_empty() => format!("{base} (since {since})"),
        _ => base,
    }
}

/// How long an Application has held its current health, from
/// `status.health.lastTransitionTime`. `None` when Argo hasn't written the
/// field (older versions may not), it doesn't parse, or it is somehow ahead
/// of `now` (clock skew, not a real case worth reporting as negative).
pub fn health_since(app: &DynamicObject, now: i64) -> Option<String> {
    let raw = app
        .data
        .pointer("/status/health/lastTransitionTime")?
        .as_str()?;
    let secs = raw.parse::<Timestamp>().ok()?.as_second();
    (now >= secs).then(|| humanize(now - secs))
}

fn headline_level(sync: &str, health: &str) -> Level {
    match health {
        "Degraded" | "Missing" | "Unknown" => Level::Critical,
        // A rollout in flight is not a fault.
        "Progressing" => Level::Info,
        "Healthy" if sync == "Synced" => Level::Good,
        _ => Level::Warn,
    }
}

/// `path` / `chart` plus `targetRevision`, whichever the source declares.
fn source_detail(source: &Value) -> String {
    let mut parts = Vec::new();
    for (label, field) in [
        ("path", "path"),
        ("chart", "chart"),
        ("ref", "ref"),
        ("targetRevision", "targetRevision"),
    ] {
        let value = source
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty());
        if let Some(value) = value {
            parts.push(format!("{label} {value}"));
        }
    }
    parts.join(" · ")
}

/// Whether the Application is unhealthy and nothing in its own status says why.
///
/// True is the case worth searching the cluster for: with
/// `controller.resource.health.persist` off, which is the Argo CD default,
/// `status.resources[].health` is never written and the health rolled up to the
/// Application is all there is.
pub fn health_unexplained(app: &DynamicObject, resources: &[ManagedResource]) -> bool {
    matches!(health_status(app), "Degraded" | "Missing") && health_causes(app, resources).is_empty()
}

/// The causes that account for an Application being unhealthy, most serious
/// first. Empty when its status names none.
///
/// Drift is left out: an OutOfSync resource says the cluster differs from git,
/// which is not a reason anything is Degraded, and treating it as one hides the
/// real fault. Suspension is left out for the same reason.
fn health_causes(app: &DynamicObject, resources: &[ManagedResource]) -> Vec<(Level, String)> {
    let mut out = Vec::new();

    let phase = str_at(&app.data, "/status/operationState/phase");
    if matches!(phase, "Failed" | "Error") {
        let msg = str_at(&app.data, "/status/operationState/message");
        out.push((Level::Critical, join(&format!("last sync {phase}"), msg)));
    }

    if let Some((kind, message)) = primary_condition(app) {
        let level = match kind.as_str() {
            "ComparisonError" | "InvalidSpecError" | "SyncError" | "UnknownError" => {
                Level::Critical
            }
            _ => Level::Warn,
        };
        out.push((level, join(&kind, &message)));
    }

    let broken: Vec<&ManagedResource> = resources
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

    out
}

/// Why the Application will not reconcile on its own. Kept apart from
/// [`health_causes`] because a suspended sync policy explains why nothing is
/// being fixed, never why something is broken.
fn policy_causes(app: &DynamicObject) -> Vec<(Level, String)> {
    if auto_sync(app) == AutoSync::Suspended {
        return vec![(
            Level::Warn,
            "auto-sync suspended, so this Application will not sync itself".into(),
        )];
    }
    Vec::new()
}

/// Managed resources the cluster no longer matches.
fn drift_causes(resources: &[ManagedResource]) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    let drifted: Vec<&ManagedResource> =
        resources.iter().filter(|r| r.sync == "OutOfSync").collect();
    for r in drifted.iter().take(5) {
        out.push((Level::Warn, format!("{}/{} is OutOfSync", r.kind, r.name)));
    }
    if drifted.len() > 5 {
        out.push((
            Level::Warn,
            format!("… and {} more OutOfSync", drifted.len() - 5),
        ));
    }
    out
}

/// What is stopping this Application from being synced and healthy, most
/// serious first, or a single line saying nothing is.
fn sync_summary(
    ev: &Evidence,
    app: &DynamicObject,
    sync: &str,
    health: &str,
) -> Vec<(Level, String)> {
    let mut out = policy_causes(app);
    let health_causes = health_causes(app, &ev.resources);
    let unexplained = health_causes.is_empty();
    out.extend(health_causes);

    // Argo can roll a health up from live cluster state it does not publish per
    // resource, leaving nothing above to name. Decided from the health causes
    // alone: neither drift nor a suspended policy answers why something broke.
    if unexplained && matches!(health, "Degraded" | "Missing" | "Progressing") {
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

    out.extend(drift_causes(&ev.resources));

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
            (s, h) => format!("{s} / {h}{at}"),
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
    use crate::columns::now_secs;
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

    /// Argo CD only fills in `status.resources[].health` when
    /// `controller.resource.health.persist` is turned on, which it is not by
    /// default, so an unhealthy Application usually says nothing about why.
    #[test]
    fn degraded_health_with_a_silent_resource_list_is_unexplained() {
        let mut obj = healthy();
        obj.data["status"]["health"]["status"] = json!("Degraded");
        obj.data["status"]["resources"] = json!([
            {"version": "v1", "kind": "Service", "namespace": "guestbook",
             "name": "guestbook-ui", "status": "Synced"}
        ]);
        let resources = managed_resources(&obj);
        assert!(health_unexplained(&obj, &resources));
    }

    #[test]
    fn a_degraded_resource_or_a_condition_explains_the_health() {
        let mut broken = healthy();
        broken.data["status"]["health"]["status"] = json!("Degraded");
        broken.data["status"]["resources"] = json!([
            {"version": "v1", "kind": "Service", "namespace": "guestbook",
             "name": "guestbook-ui", "status": "Synced", "health": {"status": "Degraded"}}
        ]);
        assert!(!health_unexplained(&broken, &managed_resources(&broken)));

        let mut reported = healthy();
        reported.data["status"]["health"]["status"] = json!("Degraded");
        reported.data["status"]["conditions"] =
            json!([{"type": "SyncError", "message": "repo not found"}]);
        assert!(!health_unexplained(
            &reported,
            &managed_resources(&reported)
        ));
    }

    #[test]
    fn a_healthy_application_is_never_unexplained() {
        let obj = healthy();
        assert!(!health_unexplained(&obj, &managed_resources(&obj)));
    }

    #[test]
    fn healthy_application_reports_synced_and_healthy() {
        let ev = evidence(healthy(), Destination::Current);
        let out = describe(&ev, now_secs());
        assert_eq!(out[0].text, "Application/guestbook: Synced / Healthy");
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
        // The column stays at the three states; the flags live in the view.
        let on = AutoSync::On {
            prune: true,
            self_heal: true,
        };
        assert_eq!(on.label(), "on");
        assert_eq!(on.detail(), "on (prune, selfHeal)");
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
        let out = describe(&evidence(obj, Destination::Current), now_secs());
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
        let out = describe(&evidence(obj, Destination::Current), now_secs());
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
        let out = describe(&evidence(obj, Destination::Current), now_secs());
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
        let out = describe(&evidence(obj, Destination::Current), now_secs());
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "Service/guestbook-ui is OutOfSync")
        );
    }

    /// A rollout in flight is not a fault, an unknown health is.
    #[test]
    fn headline_level_follows_the_sync_health_pair() {
        let at = |sync: &str, health: &str| {
            let mut obj = healthy();
            obj.data["status"]["sync"]["status"] = json!(sync);
            obj.data["status"]["health"]["status"] = json!(health);
            describe(&evidence(obj, Destination::Current), now_secs())[0].level
        };
        assert_eq!(at("Synced", "Healthy"), Level::Good);
        assert_eq!(at("Synced", "Progressing"), Level::Info);
        assert_eq!(at("OutOfSync", "Healthy"), Level::Warn);
        for health in ["Degraded", "Missing", "Unknown"] {
            assert_eq!(at("Synced", health), Level::Critical, "{health}");
        }
    }

    /// `status.health.lastTransitionTime` is the same field regardless of
    /// which way health changed, so age reads the same for a recovery as for
    /// a fresh break.
    #[test]
    fn health_since_reads_the_health_transition_timestamp() {
        let mut obj = healthy();
        let now = 1_700_000_000;
        obj.data["status"]["health"]["lastTransitionTime"] = json!("2023-11-14T22:09:20Z");
        assert_eq!(health_since(&obj, now).as_deref(), Some("4m"));
    }

    /// Older Argo CD versions may not write the field at all, and a clock
    /// skewed ahead of ours must not report a negative age.
    #[test]
    fn health_since_is_none_without_a_usable_timestamp() {
        let missing = healthy();
        assert_eq!(health_since(&missing, 1_700_000_000), None);

        let mut unparseable = healthy();
        unparseable.data["status"]["health"]["lastTransitionTime"] = json!("not a timestamp");
        assert_eq!(health_since(&unparseable, 1_700_000_000), None);

        let mut future = healthy();
        future.data["status"]["health"]["lastTransitionTime"] = json!("2023-11-14T22:09:20Z");
        assert_eq!(health_since(&future, 1_700_000_000 - 300), None);
    }

    /// The headline separates something that just broke from something
    /// that's been sitting broken for a week — the whole point of nr5.
    #[test]
    fn the_headline_names_how_long_the_current_health_has_held() {
        let now = 1_700_000_000;
        let mut obj = healthy();
        obj.data["status"]["health"]["status"] = json!("Degraded");
        obj.data["status"]["health"]["lastTransitionTime"] = json!("2023-11-14T22:09:20Z");
        let out = describe(&evidence(obj, Destination::Current), now);
        assert_eq!(
            out[0].text,
            "Application/guestbook: Synced / Degraded (since 4m)"
        );
    }

    /// No timestamp, no guess: the headline reads exactly as it did before
    /// this field existed.
    #[test]
    fn the_headline_omits_staleness_without_a_timestamp() {
        let out = describe(&evidence(healthy(), Destination::Current), now_secs());
        assert_eq!(out[0].text, "Application/guestbook: Synced / Healthy");
    }

    /// A failed operation is the immediate cause and outranks the conditions
    /// Argo leaves behind it.
    #[test]
    fn a_failed_operation_is_reported_before_conditions() {
        let mut obj = healthy();
        obj.data["status"]["operationState"] =
            json!({"phase": "Failed", "message": "one or more objects failed"});
        obj.data["status"]["conditions"] =
            json!([{"type": "SyncError", "message": "could not apply"}]);
        let out = describe(&evidence(obj, Destination::Current), now_secs());
        let summary: Vec<&str> = out
            .iter()
            .skip_while(|f| f.text != "Blocking")
            .map(|f| f.text.as_str())
            .collect();
        let operation = summary
            .iter()
            .position(|t| t.starts_with("last sync Failed"))
            .expect("operation line");
        let condition = summary
            .iter()
            .position(|t| t.starts_with("SyncError"))
            .expect("condition line");
        assert!(operation < condition, "{summary:?}");
    }

    /// `ComparisonError` outranks the warnings Argo reports alongside it.
    #[test]
    fn comparison_error_wins_over_a_shared_resource_warning() {
        let mut obj = healthy();
        obj.data["status"]["conditions"] = json!([
            {"type": "SharedResourceWarning", "message": "also owned by other-app"},
            {"type": "ComparisonError", "message": "rpc error: code = Unknown"}
        ]);
        let out = describe(&evidence(obj, Destination::Current), now_secs());
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
        let out = describe(&evidence(obj, Destination::Current), now_secs());
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "last sync Failed — one or more objects failed")
        );
    }

    /// A destination name matches a context by its own name, by its cluster
    /// entry, or by the last `/` segment of that entry (the EKS ARN shape).
    #[test]
    fn destination_name_resolves_by_context_cluster_or_arn_tail() {
        let arn = |c: &str| format!("arn:aws:eks:us-east-1:1:cluster/{c}");
        let contexts = vec![
            ("dev".to_string(), arn("eks-dev-general")),
            ("eks-dev-general".to_string(), arn("eks-dev-general")),
            ("eks-prod-general".to_string(), arn("eks-prod-general")),
            ("prod".to_string(), arn("eks-prod-general")),
            ("staging".to_string(), "staging-cluster".to_string()),
        ];
        let resolve = |n: &str| context_for_name(n, &contexts);

        // An exact context name wins over an alias sharing the cluster,
        // whichever of the two the kubeconfig lists first.
        assert_eq!(
            resolve("eks-dev-general").as_deref(),
            Some("eks-dev-general")
        );
        assert_eq!(
            resolve("eks-prod-general").as_deref(),
            Some("eks-prod-general")
        );
        assert_eq!(resolve("prod").as_deref(), Some("prod"));
        // The cluster entry name.
        assert_eq!(resolve("staging-cluster").as_deref(), Some("staging"));
        // The ARN tail, when only an alias serves the cluster.
        let alias_only = vec![("prod".to_string(), arn("eks-prod-general"))];
        assert_eq!(
            context_for_name("eks-prod-general", &alias_only).as_deref(),
            Some("prod")
        );
        // Not a suffix match: `general` is not the tail of the ARN.
        assert_eq!(resolve("general"), None);
        assert_eq!(resolve("nowhere"), None);
        assert_eq!(resolve(""), None);
    }

    /// The same bare cluster name in two accounts is not a match: guessing
    /// would switch to the wrong cluster. Two contexts on one entry are fine.
    #[test]
    fn ambiguous_arn_tail_does_not_resolve() {
        let contexts = vec![
            (
                "acct-a".to_string(),
                "arn:aws:eks:us-east-1:111:cluster/eks-general".to_string(),
            ),
            (
                "acct-b".to_string(),
                "arn:aws:eks:eu-west-1:222:cluster/eks-general".to_string(),
            ),
            (
                "acct-a-alias".to_string(),
                "arn:aws:eks:us-east-1:111:cluster/eks-general".to_string(),
            ),
        ];
        assert_eq!(context_for_name("eks-general", &contexts), None);
        // Drop the second account and the tail is unique again, with the
        // first of the two contexts on that entry.
        let unique = vec![contexts[0].clone(), contexts[2].clone()];
        assert_eq!(
            context_for_name("eks-general", &unique).as_deref(),
            Some("acct-a")
        );
    }

    /// Argo's reserved local name is only claimed by a context called exactly
    /// that; a cluster entry called `in-cluster` under another context name
    /// leaves the convention to decide.
    #[test]
    fn in_cluster_matches_an_exact_context_only() {
        let by_entry = vec![("local".to_string(), "in-cluster".to_string())];
        assert_eq!(context_for_name("in-cluster", &by_entry), None);
        let by_context = vec![("in-cluster".to_string(), "kind-local".to_string())];
        assert_eq!(
            context_for_name("in-cluster", &by_context).as_deref(),
            Some("in-cluster")
        );
        // Other names still match through the entry.
        assert_eq!(
            context_for_name("kind-local", &by_context).as_deref(),
            Some("in-cluster")
        );
    }

    /// The whole point of [`Destination`]: a managed resource in another
    /// cluster must not offer a jump that would resolve here.
    #[test]
    fn remote_destination_yields_no_jump_targets() {
        let ev = evidence(healthy(), Destination::Context("sandbox-east".into()));
        let out = describe(&ev, now_secs());
        assert!(out.iter().all(|f| f.target.is_none()));
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "destination sandbox-east/guestbook")
        );
    }

    #[test]
    fn current_destination_yields_a_jump_target() {
        let out = describe(&evidence(healthy(), Destination::Current), now_secs());
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
        let out = describe(&ev, now_secs());
        assert!(
            texts(&out)
                .iter()
                .any(|t| t == "destination https://10.0.1.5/guestbook")
        );
        assert!(out.iter().all(|f| f.target.is_none()));
    }

    #[test]
    fn a_single_source_list_reads_like_a_single_source() {
        let mut obj = healthy();
        obj.data["spec"]["source"] = json!(null);
        obj.data["spec"]["sources"] = json!([
            {"repoURL": "https://example.com/repo", "path": "app", "targetRevision": "v1.2.3"}
        ]);
        assert_eq!(sources(&obj)[0].repo_url, "https://example.com/repo");
        let out = describe(&evidence(obj, Destination::Current), now_secs());
        let texts = texts(&out);
        assert!(
            texts
                .iter()
                .any(|t| t == "path app · targetRevision v1.2.3")
        );
        // One source, so the revision stays on the Application block.
        assert!(texts.iter().any(|t| t.starts_with("revision ")));
    }

    /// Mid-sync Argo records the revisions only in the operation result, and
    /// reading one field without the other shows the first source's revision
    /// and none of the others'.
    #[test]
    fn sources_read_revisions_from_the_sync_result_too() {
        let mut obj = healthy();
        obj.data["spec"]["source"] = json!(null);
        obj.data["spec"]["sources"] = json!([
            {"repoURL": "harbor.example/charts", "chart": "web"},
            {"repoURL": "https://git.example/gitops.git", "ref": "values"}
        ]);
        obj.data["status"]["sync"] = json!({"status": "Synced"});
        obj.data["status"]["operationState"] = json!({
            "phase": "Running",
            "syncResult": {"revisions": ["1.2.0", "9f7a301596ed7c2e9fcb4f3067425432c30d014e"]}
        });

        let sources = sources(&obj);
        assert_eq!(sources[0].revision, "1.2.0");
        assert_eq!(
            sources[1].revision, "9f7a301596ed7c2e9fcb4f3067425432c30d014e",
            "the second source lost its revision"
        );
    }

    /// Argo does not always publish per-source revisions. When it has not, the
    /// Application-level one is the only revision there is, so dropping it
    /// would leave none at all.
    #[test]
    fn multi_source_without_per_source_revisions_keeps_the_application_one() {
        let mut obj = healthy();
        obj.data["spec"]["source"] = json!(null);
        obj.data["spec"]["sources"] = json!([
            {"repoURL": "harbor.example/charts", "chart": "web"},
            {"repoURL": "https://git.example/gitops.git", "ref": "values"}
        ]);
        obj.data["status"]["sync"] = json!({"status": "Synced", "revision": "abc1234"});

        let texts = texts(&describe(&evidence(obj, Destination::Current), now_secs()));
        assert!(
            texts.iter().any(|t| t == "revision abc1234"),
            "the only revision went missing: {texts:?}"
        );
    }

    /// An empty `sources` list is not a declaration that there are none.
    #[test]
    fn an_empty_source_list_falls_back_to_the_single_source() {
        let mut obj = healthy();
        obj.data["spec"]["sources"] = json!([]);
        let sources = sources(&obj);
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].repo_url,
            "https://github.com/argoproj/argocd-example-apps"
        );
    }

    /// A chart plus the repository holding its values is the common shape, and
    /// showing only the first hides where the values came from.
    #[test]
    fn every_source_is_shown_with_the_revision_deployed_from_it() {
        let mut obj = healthy();
        obj.data["spec"]["source"] = json!(null);
        obj.data["spec"]["sources"] = json!([
            {"repoURL": "harbor.example/charts", "chart": "web", "targetRevision": "1.2.0"},
            {"repoURL": "https://git.example/gitops.git", "ref": "values",
             "targetRevision": "master"}
        ]);
        obj.data["status"]["sync"] = json!({
            "status": "Synced",
            "revisions": ["1.2.0", "9f7a301596ed7c2e9fcb4f3067425432c30d014e"]
        });

        let out = describe(&evidence(obj, Destination::Current), now_secs());
        let texts = texts(&out);
        assert!(
            texts.iter().any(|t| t == "harbor.example/charts"),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "https://git.example/gitops.git"),
            "the second source is missing: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t == "chart web · targetRevision 1.2.0 · revision 1.2.0"),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t == "ref values · targetRevision master · revision 9f7a301"),
            "{texts:?}"
        );
        // No single deployed revision to put on the Application block.
        assert!(
            !texts.iter().any(|t| t.starts_with("revision ")),
            "{texts:?}"
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
        let out = describe(&ev, now_secs());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Application/gone not found");
    }

    /// Chart versions and branch names are not SHAs and must survive intact.
    /// Most Applications in the wild are multi-source, and those record their
    /// revisions under the plural field only. Reading the singular one alone
    /// leaves the column blank.
    #[test]
    fn multi_source_revisions_come_from_the_plural_field() {
        let mut obj = healthy();
        obj.data["status"]["sync"] = json!({
            "status": "Synced",
            "revisions": ["1.2.0", "9f7a301596ed7c2e9fcb4f3067425432c30d014e"]
        });
        assert_eq!(revision(&obj), "1.2.0");

        // The singular field still wins when Argo wrote it.
        obj.data["status"]["sync"]["revision"] = json!("abc1234");
        assert_eq!(revision(&obj), "abc1234");
    }

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
        let out = describe(&ev, now_secs());
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
        let out = describe(&ev, now_secs());
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
        let out = describe(&ev, now_secs());
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
        let out = describe(&ev, now_secs());
        assert_eq!(
            out[0].text,
            "Deployment/guestbook-ui is managed by Application/guestbook: Synced / Healthy"
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
        let out = describe(&evidence(obj, Destination::Current), now_secs());
        assert!(texts(&out).iter().any(|t| t == "… and 3 more"));
    }

    /// A Job carries its terminal condition after the newer staging one, and
    /// both are true at once.
    #[test]
    fn a_jobs_outcome_is_read_by_condition_type_not_by_order() {
        let failed = app(json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "backup-1", "namespace": "default"},
            "status": {"conditions": [
                {"type": "FailureTarget", "status": "True"},
                {"type": "Failed", "status": "True"}]}
        }));
        assert_eq!(descendant_state(&failed), "Failed");

        let complete = app(json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "backup-2", "namespace": "default"},
            "status": {"conditions": [
                {"type": "SuccessCriteriaMet", "status": "True"},
                {"type": "Complete", "status": "True"}]}
        }));
        assert_eq!(descendant_state(&complete), "Complete");
    }

    /// The controller commits to `FailureTarget` or `SuccessCriteriaMet` before
    /// it terminates the pods, and that delay can last a while. The search must
    /// not wait for cleanup to finish before it sees the outcome.
    #[test]
    fn a_jobs_interim_condition_is_read_before_the_terminal_one_lands() {
        let failing = app(json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "backup-3", "namespace": "default"},
            "status": {"conditions": [{"type": "FailureTarget", "status": "True"}]}
        }));
        assert_eq!(descendant_state(&failing), "Failed");

        let succeeding = app(json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "backup-4", "namespace": "default"},
            "status": {"conditions": [{"type": "SuccessCriteriaMet", "status": "True"},
                                      {"type": "Complete", "status": "False"}]}
        }));
        assert_eq!(descendant_state(&succeeding), "Completing");
    }

    /// A job with no interim or terminal condition yet has nothing worth
    /// summarising.
    #[test]
    fn a_job_without_a_terminal_condition_has_no_state() {
        let running = app(json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "backup-5", "namespace": "default"},
            "status": {"active": 1}
        }));
        assert_eq!(descendant_state(&running), "");
    }

    fn appset(value: serde_json::Value) -> DynamicObject {
        serde_json::from_value(value).expect("valid ApplicationSet")
    }

    #[test]
    fn applicationset_status_prefers_error_over_up_to_date() {
        let error = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-a"},
            "status": {"conditions": [
                {"type": "ResourcesUpToDate", "reason": "ApplicationSetUpToDate", "status": "True"},
                {"type": "ErrorOccurred", "status": "True", "message": "bad generator"}
            ]}
        }));
        assert_eq!(applicationset_status(&error), "Error");
        assert_eq!(applicationset_error(&error), Some("bad generator"));

        // The controller's own reason string for this condition is
        // `ApplicationSetUpToDate` — easy to mistake for the `type`, which is
        // `ResourcesUpToDate`. This is the exact shape a real ApplicationSet
        // reports once reconciled.
        let synced = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-b"},
            "status": {"conditions": [
                {"type": "ErrorOccurred", "status": "False", "reason": "ApplicationSetUpToDate"},
                {"type": "ParametersGenerated", "status": "True"},
                {"type": "ResourcesUpToDate", "status": "True", "reason": "ApplicationSetUpToDate"}
            ]}
        }));
        assert_eq!(applicationset_status(&synced), "Synced");
        assert_eq!(applicationset_error(&synced), None);

        let unknown = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-c"},
            "status": {}
        }));
        assert_eq!(applicationset_status(&unknown), "Unknown");
    }

    /// `matrix` and `merge` generators wrap others; the names they combine
    /// matter more than the wrapper, so those unwrap one level.
    #[test]
    fn generators_unwraps_matrix_and_merge_but_not_a_plain_generator() {
        let a = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-a"},
            "spec": {"generators": [
                {"git": {"repoURL": "x"}},
                {"matrix": {"generators": [{"list": {}}, {"clusters": {}}]}}
            ]}
        }));
        assert_eq!(generators(&a), vec!["git", "list", "clusters"]);
    }

    /// A `selector` sits alongside the generator, not inside it — it must not
    /// be read as a generator kind of its own.
    #[test]
    fn generators_skips_the_selector_key() {
        let a = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-a"},
            "spec": {"generators": [
                {"clusters": {}, "selector": {"matchLabels": {"env": "prod"}}}
            ]}
        }));
        assert_eq!(generators(&a), vec!["clusters"]);
    }

    /// The headline, generator summary, and produced-Applications list all
    /// come from `status.resources[]` the same way an Application's own
    /// managed resources do, jump targets included.
    #[test]
    fn describe_applicationset_lists_generators_and_produced_apps() {
        let a = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-a"},
            "spec": {"generators": [{"git": {}}]},
            "status": {
                "conditions": [{"type": "ResourcesUpToDate", "status": "True"}],
                "resources": [
                    {"group": "argoproj.io", "kind": "Application", "namespace": "argocd",
                     "name": "team-a-dev", "status": "Synced", "health": {"status": "Healthy"}}
                ]
            }
        }));
        let mut resources = managed_resources(&a);
        for r in &mut resources {
            r.plural = "applications".into();
        }
        let findings = describe_applicationset(&a, "ApplicationSet/team-a", &resources);
        assert_eq!(findings[0].level, Level::Good);
        assert_eq!(findings[0].text, "ApplicationSet/team-a: Synced");
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Heading && f.text == "Generators")
        );
        assert!(findings.iter().any(|f| f.text == "git"));
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Heading && f.text == "Applications (1)")
        );
        let app_line = findings
            .iter()
            .find(|f| f.text.starts_with("Application/team-a-dev"))
            .expect("produced Application line");
        assert!(app_line.target.is_some());
    }

    /// Nothing in `describe_applicationset` should assume a generator ran
    /// cleanly — the error message is a finding of its own, not folded into
    /// the headline.
    #[test]
    fn describe_applicationset_surfaces_the_generator_error() {
        let a = appset(json!({
            "apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationSet",
            "metadata": {"name": "team-a"},
            "status": {"conditions": [
                {"type": "ErrorOccurred", "status": "True", "message": "invalid repo URL"}
            ]}
        }));
        let findings = describe_applicationset(&a, "ApplicationSet/team-a", &[]);
        assert_eq!(findings[0].level, Level::Critical);
        assert!(
            findings
                .iter()
                .any(|f| f.level == Level::Critical && f.text == "invalid repo URL")
        );
    }
}
