//! GitOps ownership and reconciliation analysis for Flux and Argo CD.
//!
//! Every object Flux applies is stamped with `kustomize.toolkit.fluxcd.io/name`
//! (+`/namespace`) or `helm.toolkit.fluxcd.io/name` labels naming the
//! Kustomization / HelmRelease that manages it. From there the chain runs
//! owner → source (GitRepository/OCIRepository/HelmChart/…) → the revision
//! actually applied, plus any `dependsOn` Kustomizations that gate it.
//!
//! Argo CD stamps its own with an `argocd.argoproj.io/tracking-id` annotation
//! or an instance label naming the Application. That chain runs Application →
//! the repo/chart sources it syncs from → the revision actually synced, with
//! the AppProject that constrains it and the ApplicationSet that generated it
//! alongside.
//!
//! This module is pure: it extracts the references to follow (so the app knows
//! what to fetch) and, given the fetched objects, formats the chain into
//! ranked [`Finding`]s with jump targets. The app layer does the fetching and
//! renders/navigates the findings.

use kube::core::DynamicObject;
use serde_json::Value;

use crate::explain::{Finding, Level, Target};

/// The GitOps controller a chain belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Flux,
    Argo,
}

impl Engine {
    /// How the engine is named in findings.
    pub fn label(self) -> &'static str {
        match self {
            Engine::Flux => "Flux",
            Engine::Argo => "Argo CD",
        }
    }
}

/// A reference to a GitOps object: its kind, name, and namespace. The
/// namespace is empty when the stamp doesn't carry one — Argo's instance label
/// only spells one out for Applications outside the controller's namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRef {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

/// A node in the reconciliation chain: the reference, its resolved plural (for
/// a jump target; empty when the kind couldn't be resolved), and the fetched
/// object (`None` when missing or unfetched).
#[derive(Debug, Clone)]
pub struct Node {
    pub reference: ObjRef,
    pub plural: String,
    pub obj: Option<DynamicObject>,
}

impl Node {
    fn target(&self) -> Option<Target> {
        (!self.plural.is_empty()).then(|| Target {
            plural: self.plural.clone(),
            namespace: Some(self.reference.namespace.clone()),
            name: self.reference.name.clone(),
        })
    }
}

/// The owner named by an object's stamps, before it has been fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub engine: Engine,
    pub reference: ObjRef,
    /// The reference came from the generic `app.kubernetes.io/instance` label,
    /// which Helm and hand-written manifests set too. A chain that can't find
    /// the named Application reads as unmanaged rather than as broken.
    pub inferred: bool,
}

/// The owner once fetched: which engine manages the subject, and the node.
#[derive(Debug, Clone)]
pub struct Managed {
    pub engine: Engine,
    pub node: Node,
    pub inferred: bool,
}

/// The gathered reconciliation picture for one selected object.
pub struct Evidence {
    /// e.g. `Deployment/api`.
    pub subject: String,
    /// The selected object is itself the owner (Kustomization, HelmRelease,
    /// Application, ApplicationSet).
    pub self_is_owner: bool,
    /// The managing owner (or the object itself), and its engine.
    pub owner: Option<Managed>,
    /// Flux only: the source the owner reconciles from.
    pub source: Option<Node>,
    /// Flux: the `dependsOn` Kustomizations that gate the owner. Argo: the
    /// AppProject constraining the Application and the ApplicationSet that
    /// generated it.
    pub deps: Vec<Node>,
}

// ----- reference extraction (used by the app to know what to fetch) --------

/// Argo CD's API group. Its CRD plurals (`applications`, `applicationsets`)
/// are generic enough that another CRD can own the bare name, so anything
/// keyed on one of them checks the group too.
pub const ARGO_GROUP: &str = "argoproj.io";

/// Argo CD's own annotation naming the Application that applied an object.
const ARGO_TRACKING_ID: &str = "argocd.argoproj.io/tracking-id";
/// Argo CD's instance label, when `application.instanceLabelKey` is set to it.
const ARGO_INSTANCE_LABEL: &str = "argocd.argoproj.io/instance";
/// The default instance label — set by Helm and hand-written manifests too, so
/// a reference read from it is only [`Owner::inferred`].
const APP_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";

/// The GitOps owner named by a managed object's stamps: Flux's toolkit labels
/// first (they are unambiguous), then Argo CD's tracking annotation and
/// instance labels.
pub fn owner_ref(obj: &DynamicObject) -> Option<Owner> {
    flux_owner_ref(obj).or_else(|| argo_owner_ref(obj))
}

/// The Flux Kustomization/HelmRelease named by an object's toolkit labels.
pub fn flux_owner_ref(obj: &DynamicObject) -> Option<Owner> {
    let labels = obj.metadata.labels.as_ref()?;
    let get = |k: &str| labels.get(k).cloned();
    let flux = |kind: &str, name: String, namespace: String| {
        Some(Owner {
            engine: Engine::Flux,
            reference: ObjRef {
                kind: kind.into(),
                name,
                namespace,
            },
            inferred: false,
        })
    };
    if let Some(name) = get("kustomize.toolkit.fluxcd.io/name") {
        let ns = get("kustomize.toolkit.fluxcd.io/namespace").unwrap_or_default();
        return flux("Kustomization", name, ns);
    }
    if let Some(name) = get("helm.toolkit.fluxcd.io/name") {
        let ns = get("helm.toolkit.fluxcd.io/namespace").unwrap_or_default();
        return flux("HelmRelease", name, ns);
    }
    None
}

/// The Argo CD Application named by an object's tracking annotation or
/// instance label.
///
/// The tracking annotation is `<instance>:<group>/<Kind>:<ns>/<name>`; both it
/// and the labels carry the Application's *instance name*, which is
/// `<namespace>_<name>` for an Application outside the controller's namespace
/// and a bare name inside it (a bare name leaves the namespace to the caller
/// to find — Kubernetes names can't contain `_`, so the split is unambiguous).
pub fn argo_owner_ref(obj: &DynamicObject) -> Option<Owner> {
    let annotations = obj.metadata.annotations.as_ref();
    let labels = obj.metadata.labels.as_ref();
    let tracked = annotations
        .and_then(|a| a.get(ARGO_TRACKING_ID))
        .and_then(|v| v.split(':').next())
        .filter(|s| !s.is_empty());
    if let Some(instance) = tracked {
        return Some(argo_owner(instance, false));
    }
    if let Some(instance) = labels.and_then(|l| l.get(ARGO_INSTANCE_LABEL)) {
        return Some(argo_owner(instance, false));
    }
    // The generic instance label is a weak signal — Helm sets it to the
    // release name — so it only counts when the Application turns up.
    let instance = labels.and_then(|l| l.get(APP_INSTANCE_LABEL))?;
    Some(argo_owner(instance, true))
}

fn argo_owner(instance: &str, inferred: bool) -> Owner {
    let (namespace, name) = match instance.split_once('_') {
        Some((ns, name)) => (ns.to_string(), name.to_string()),
        None => (String::new(), instance.to_string()),
    };
    Owner {
        engine: Engine::Argo,
        reference: ObjRef {
            kind: "Application".into(),
            name,
            namespace,
        },
        inferred,
    }
}

/// The engine whose owner kind `plural` is, when the selection is itself an
/// owner. `applications`/`applicationsets` are generic plurals, so the Argo
/// kinds are only recognized in their own API group.
pub fn owner_engine(plural: &str, group: &str) -> Option<Engine> {
    match plural {
        "kustomizations" | "helmreleases" => Some(Engine::Flux),
        "applications" | "applicationsets" if group == ARGO_GROUP => Some(Engine::Argo),
        _ => None,
    }
}

/// The source a Kustomization/HelmRelease reconciles from. Kustomizations use
/// `spec.sourceRef`; HelmRelease v2 GA uses `spec.chartRef`, older ones
/// `spec.chart.spec.sourceRef` (a HelmChart the controller creates).
pub fn source_ref(owner: &DynamicObject) -> Option<ObjRef> {
    let d = &owner.data;
    let owner_ns = owner.metadata.namespace.clone().unwrap_or_default();
    let from = |v: &Value| -> Option<ObjRef> {
        Some(ObjRef {
            kind: v.get("kind").and_then(Value::as_str)?.to_string(),
            name: v.get("name").and_then(Value::as_str)?.to_string(),
            namespace: v
                .get("namespace")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| owner_ns.clone()),
        })
    };
    d.pointer("/spec/sourceRef")
        .and_then(from)
        .or_else(|| d.pointer("/spec/chartRef").and_then(from))
        .or_else(|| d.pointer("/spec/chart/spec/sourceRef").and_then(from))
}

/// The `dependsOn` Kustomizations gating `owner` (namespace defaults to the
/// owner's).
pub fn depends_on(owner: &DynamicObject) -> Vec<ObjRef> {
    let owner_ns = owner.metadata.namespace.clone().unwrap_or_default();
    owner
        .data
        .pointer("/spec/dependsOn")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(|d| {
                    Some(ObjRef {
                        kind: "Kustomization".into(),
                        name: d.get("name").and_then(Value::as_str)?.to_string(),
                        namespace: d
                            .get("namespace")
                            .and_then(Value::as_str)
                            .map(String::from)
                            .unwrap_or_else(|| owner_ns.clone()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The AppProject an Application (or an ApplicationSet's template) names.
/// Projects live beside the Application, in the controller's namespace.
pub fn argo_project_ref(owner: &DynamicObject) -> Option<ObjRef> {
    let name = str_at(owner, "/spec/project")
        .or_else(|| str_at(owner, "/spec/template/spec/project"))
        .filter(|p| !p.is_empty())?;
    Some(ObjRef {
        kind: "AppProject".into(),
        name,
        namespace: owner.metadata.namespace.clone().unwrap_or_default(),
    })
}

/// The ApplicationSet that generated an Application, from its owner reference.
pub fn argo_parent_ref(owner: &DynamicObject) -> Option<ObjRef> {
    let parent = owner
        .metadata
        .owner_references
        .as_ref()?
        .iter()
        .find(|o| o.kind == "ApplicationSet")?;
    Some(ObjRef {
        kind: "ApplicationSet".into(),
        name: parent.name.clone(),
        namespace: owner.metadata.namespace.clone().unwrap_or_default(),
    })
}

// ----- object state accessors ----------------------------------------------

/// `(status, reason, message)` of the object's `Ready` condition.
pub fn ready(obj: &DynamicObject) -> Option<(String, String, String)> {
    condition(obj, "Ready")
}

/// `(status, reason, message)` of one of the object's conditions.
fn condition(obj: &DynamicObject, ty: &str) -> Option<(String, String, String)> {
    let cond = obj
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(ty))?;
    let s = |k: &str| {
        cond.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some((s("status"), s("reason"), s("message")))
}

fn suspended(obj: &DynamicObject) -> bool {
    obj.data.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true)
}

fn str_at(obj: &DynamicObject, p: &str) -> Option<String> {
    obj.data
        .pointer(p)
        .and_then(Value::as_str)
        .map(String::from)
}

fn applied_revision(obj: &DynamicObject) -> Option<String> {
    str_at(obj, "/status/lastAppliedRevision")
        .or_else(|| str_at(obj, "/status/history/0/chartVersion"))
}

fn attempted_revision(obj: &DynamicObject) -> Option<String> {
    str_at(obj, "/status/lastAttemptedRevision")
}

fn source_revision(obj: &DynamicObject) -> Option<String> {
    str_at(obj, "/status/artifact/revision")
}

/// Whether the object is an Argo ApplicationSet rather than an Application.
fn is_appset(node: &Node) -> bool {
    node.reference.kind == "ApplicationSet"
}

/// Argo sync state: `(status, revision)` — `Synced`/`OutOfSync`/`Unknown`.
fn argo_sync(obj: &DynamicObject) -> (String, String) {
    (
        str_at(obj, "/status/sync/status").unwrap_or_else(|| "Unknown".into()),
        str_at(obj, "/status/sync/revision").unwrap_or_default(),
    )
}

/// Argo health state: `(status, message)` — `Healthy`/`Degraded`/`Missing`/…
fn argo_health(obj: &DynamicObject) -> (String, String) {
    (
        str_at(obj, "/status/health/status").unwrap_or_else(|| "Unknown".into()),
        str_at(obj, "/status/health/message").unwrap_or_default(),
    )
}

/// The last sync operation's `(phase, message)`, when one has run.
fn argo_operation(obj: &DynamicObject) -> Option<(String, String)> {
    let phase = str_at(obj, "/status/operationState/phase")?;
    let msg = str_at(obj, "/status/operationState/message").unwrap_or_default();
    Some((phase, msg))
}

/// Whether the Application syncs itself (`spec.syncPolicy.automated`). Sofka's
/// own suspend removes that block, so a manual Application is one nothing will
/// reconcile until someone syncs it.
fn argo_automated(obj: &DynamicObject) -> bool {
    obj.data.pointer("/spec/syncPolicy/automated").is_some()
}

/// `(out-of-sync, total)` resources the Application last reported.
fn argo_resource_drift(obj: &DynamicObject) -> (usize, usize) {
    let Some(resources) = obj
        .data
        .pointer("/status/resources")
        .and_then(Value::as_array)
    else {
        return (0, 0);
    };
    let drifted = resources
        .iter()
        .filter(|r| {
            r.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "Synced")
        })
        .count();
    (drifted, resources.len())
}

/// One line per Argo source: the repo, the revision asked for, and the path or
/// chart inside it. Reads an Application's `spec.source`/`spec.sources` or an
/// ApplicationSet's `spec.template.spec` equivalents.
pub fn argo_sources(obj: &DynamicObject) -> Vec<String> {
    let d = &obj.data;
    let mut out = Vec::new();
    for base in ["/spec", "/spec/template/spec"] {
        if let Some(list) = d
            .pointer(&format!("{base}/sources"))
            .and_then(Value::as_array)
        {
            out.extend(list.iter().filter_map(argo_source_line));
        }
        if let Some(line) = d
            .pointer(&format!("{base}/source"))
            .and_then(argo_source_line)
        {
            out.push(line);
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

fn argo_source_line(src: &Value) -> Option<String> {
    let s = |k: &str| src.get(k).and_then(Value::as_str).unwrap_or_default();
    let repo = s("repoURL");
    if repo.is_empty() {
        return None;
    }
    let mut line = repo.to_string();
    let rev = s("targetRevision");
    if !rev.is_empty() {
        line.push_str(&format!(" @ {rev}"));
    }
    let inner = match (s("chart"), s("path")) {
        ("", "") | ("", ".") => String::new(),
        ("", path) => format!(" ({path})"),
        (chart, _) => format!(" (chart {chart})"),
    };
    line.push_str(&inner);
    Some(line)
}

// ----- findings -------------------------------------------------------------

/// Render the reconciliation chain into ranked findings with jump targets.
pub fn describe(ev: &Evidence) -> Vec<Finding> {
    // An owner only guessed at from `app.kubernetes.io/instance` and not
    // actually in the cluster was never an owner — Helm sets that label too.
    let owner = match &ev.owner {
        Some(o) if !(o.inferred && o.node.obj.is_none()) => o,
        _ => return unmanaged(ev),
    };
    match owner.engine {
        Engine::Flux => describe_flux(ev, owner),
        Engine::Argo => describe_argo(ev, owner),
    }
}

fn unmanaged(ev: &Evidence) -> Vec<Finding> {
    vec![
        finding(
            0,
            Level::Info,
            format!("{} is not managed by Flux or Argo CD", ev.subject),
        ),
        finding(
            1,
            Level::Info,
            "no Flux toolkit labels and no Argo CD tracking stamp found",
        ),
    ]
}

fn describe_flux(ev: &Evidence, owner: &Managed) -> Vec<Finding> {
    let mut out = Vec::new();
    let node = &owner.node;

    out.push(headline(ev, owner));

    // Owner block.
    out.push(finding(0, Level::Heading, "Owner"));
    push_flux_object(&mut out, node, ev.self_is_owner);

    // Source block.
    out.push(finding(0, Level::Heading, "Source"));
    match &ev.source {
        Some(src) => {
            let mut f = finding(1, source_level(src), source_line(src));
            if let Some(t) = src.target() {
                f = f.with_target(t);
            }
            out.push(f);
            if let Some(rev) = src.obj.as_ref().and_then(source_revision) {
                out.push(finding(2, Level::Info, format!("revision {}", short(&rev))));
            }
        }
        None => out.push(finding(1, Level::Info, "no source resolved")),
    }

    // Dependencies.
    if !ev.deps.is_empty() {
        out.push(finding(0, Level::Heading, "Depends on"));
        for dep in &ev.deps {
            let mut f = finding(1, dep_level(dep), dep_line(dep));
            if let Some(t) = dep.target() {
                f = f.with_target(t);
            }
            out.push(f);
        }
    }

    // Reconciliation summary: what, if anything, is blocking.
    out.push(finding(0, Level::Heading, "Reconciliation"));
    for line in flux_reconciliation_summary(ev, node) {
        out.push(finding(1, line.0, line.1));
    }

    out
}

fn describe_argo(ev: &Evidence, owner: &Managed) -> Vec<Finding> {
    let mut out = Vec::new();
    let node = &owner.node;

    out.push(headline(ev, owner));

    // Owner block.
    out.push(finding(0, Level::Heading, "Owner"));
    push_argo_object(&mut out, node, ev.self_is_owner);

    // Sources are fields on the Application, not objects of their own.
    out.push(finding(0, Level::Heading, "Source"));
    let sources = node.obj.as_ref().map(argo_sources).unwrap_or_default();
    if sources.is_empty() {
        out.push(finding(1, Level::Info, "no source resolved"));
    } else {
        for line in sources {
            out.push(finding(1, Level::Info, short(&line)));
        }
    }

    // The AppProject constraining it and the ApplicationSet that generated it.
    if !ev.deps.is_empty() {
        out.push(finding(0, Level::Heading, "Related"));
        for dep in &ev.deps {
            let mut f = finding(1, related_level(dep), related_line(dep));
            if let Some(t) = dep.target() {
                f = f.with_target(t);
            }
            out.push(f);
        }
    }

    out.push(finding(0, Level::Heading, "Reconciliation"));
    for (level, text) in argo_reconciliation_summary(node) {
        out.push(finding(1, level, text));
    }

    out
}

/// The first line: what the subject is, or who manages it.
fn headline(ev: &Evidence, owner: &Managed) -> Finding {
    let node = &owner.node;
    let level = match owner.engine {
        Engine::Flux => flux_owner_level(node),
        Engine::Argo => argo_owner_level(node),
    };
    if ev.self_is_owner {
        finding(
            0,
            level,
            format!(
                "{} — {} {}",
                ev.subject,
                owner.engine.label(),
                node.reference.kind
            ),
        )
    } else {
        finding(
            0,
            level,
            format!(
                "{} is managed by {} {}/{}",
                ev.subject,
                owner.engine.label(),
                node.reference.kind,
                node.reference.name
            ),
        )
    }
}

/// The Flux owner's health, for the headline and its own line.
fn flux_owner_level(owner: &Node) -> Level {
    match owner.obj.as_ref() {
        None => Level::Warn,
        Some(o) if suspended(o) => Level::Warn,
        Some(o) => match ready(o) {
            Some((s, _, _)) if s == "True" => Level::Good,
            Some((s, _, _)) if s == "False" => Level::Critical,
            _ => Level::Warn,
        },
    }
}

/// The Argo owner's health: degraded/missing health or a failed sync is
/// critical, drift or a manual sync policy is a warning.
fn argo_owner_level(owner: &Node) -> Level {
    let Some(o) = owner.obj.as_ref() else {
        return Level::Warn;
    };
    if is_appset(owner) {
        return match condition(o, "ErrorOccurred") {
            Some((s, _, _)) if s == "True" => Level::Critical,
            _ => Level::Good,
        };
    }
    let (sync, _) = argo_sync(o);
    match argo_health(o).0.as_str() {
        "Degraded" | "Missing" => Level::Critical,
        "Progressing" | "Suspended" | "Unknown" => Level::Warn,
        _ if sync != "Synced" => Level::Warn,
        _ => Level::Good,
    }
}

/// Append the Flux owner's own detail lines (state, suspend, revisions).
fn push_flux_object(out: &mut Vec<Finding>, owner: &Node, self_is_owner: bool) {
    out.push(owner_identity(
        owner,
        self_is_owner,
        flux_owner_level(owner),
    ));

    let Some(o) = owner.obj.as_ref() else {
        out.push(finding(2, Level::Warn, "owner not found in cluster"));
        return;
    };
    if suspended(o) {
        out.push(finding(2, Level::Warn, "suspended"));
    }
    match ready(o) {
        Some((s, reason, msg)) => {
            let level = match s.as_str() {
                "True" => Level::Good,
                "False" => Level::Critical,
                _ => Level::Warn,
            };
            out.push(finding(
                2,
                level,
                format!("Ready: {}", join(&s, &reason, &msg)),
            ));
        }
        None => out.push(finding(2, Level::Warn, "no Ready condition yet")),
    }
    if let Some(rev) = applied_revision(o) {
        out.push(finding(
            2,
            Level::Info,
            format!("applied revision {}", short(&rev)),
        ));
    }
    if let Some(att) = attempted_revision(o)
        && applied_revision(o).as_deref() != Some(att.as_str())
    {
        out.push(finding(
            2,
            Level::Warn,
            format!("last attempted {} (not yet applied)", short(&att)),
        ));
    }
}

/// Append the Argo owner's own detail lines (sync, health, revision, policy).
fn push_argo_object(out: &mut Vec<Finding>, owner: &Node, self_is_owner: bool) {
    out.push(owner_identity(
        owner,
        self_is_owner,
        argo_owner_level(owner),
    ));

    let Some(o) = owner.obj.as_ref() else {
        out.push(finding(2, Level::Warn, "owner not found in cluster"));
        return;
    };
    if is_appset(owner) {
        push_appset_conditions(out, o);
        return;
    }
    let (sync, revision) = argo_sync(o);
    out.push(finding(
        2,
        match sync.as_str() {
            "Synced" => Level::Good,
            "OutOfSync" => Level::Warn,
            _ => Level::Info,
        },
        format!("Sync: {sync}"),
    ));
    let (health, health_msg) = argo_health(o);
    out.push(finding(
        2,
        argo_health_level(&health),
        format!("Health: {}", join(&health, "", &health_msg)),
    ));
    if !revision.is_empty() {
        out.push(finding(
            2,
            Level::Info,
            format!("synced revision {}", short_rev(&revision)),
        ));
    }
    let (policy_level, policy) = if argo_automated(o) {
        (Level::Info, "sync policy: automated")
    } else {
        (Level::Warn, "sync policy: manual (no automated sync)")
    };
    out.push(finding(2, policy_level, policy));
    if let Some((phase, msg)) = argo_operation(o)
        && phase != "Succeeded"
    {
        out.push(finding(
            2,
            argo_phase_level(&phase),
            format!("last sync {}", join(&phase, "", &msg)),
        ));
    }
}

/// An ApplicationSet has no sync/health — it reports on generating its
/// Applications through conditions instead.
fn push_appset_conditions(out: &mut Vec<Finding>, o: &DynamicObject) {
    let mut reported = false;
    for ty in ["ErrorOccurred", "ParametersGenerated", "ResourcesUpToDate"] {
        let Some((status, reason, msg)) = condition(o, ty) else {
            continue;
        };
        reported = true;
        // ErrorOccurred inverts: True is the bad one.
        let bad = if ty == "ErrorOccurred" {
            status == "True"
        } else {
            status != "True"
        };
        let level = if bad { Level::Critical } else { Level::Good };
        out.push(finding(
            2,
            level,
            format!("{ty}: {}", join(&status, &reason, &msg)),
        ));
    }
    if !reported {
        out.push(finding(2, Level::Warn, "no conditions reported yet"));
    }
}

/// The owner's identity line, with a jump target unless it's the selection.
fn owner_identity(owner: &Node, self_is_owner: bool, level: Level) -> Finding {
    let mut head = finding(
        1,
        level,
        format!("{}/{}", owner.reference.kind, owner.reference.name),
    );
    if !self_is_owner && let Some(t) = owner.target() {
        head = head.with_target(t);
    }
    head
}

fn argo_health_level(health: &str) -> Level {
    match health {
        "Healthy" => Level::Good,
        "Degraded" | "Missing" => Level::Critical,
        _ => Level::Warn,
    }
}

fn argo_phase_level(phase: &str) -> Level {
    match phase {
        "Succeeded" => Level::Good,
        "Failed" | "Error" => Level::Critical,
        _ => Level::Warn,
    }
}

fn source_level(src: &Node) -> Level {
    match src.obj.as_ref().and_then(ready) {
        Some((s, _, _)) if s == "True" => Level::Good,
        Some((s, _, _)) if s == "False" => Level::Critical,
        None if src.obj.is_none() => Level::Warn,
        _ => Level::Warn,
    }
}

fn source_line(src: &Node) -> String {
    match src.obj.as_ref() {
        None => format!("{}/{} (not found)", src.reference.kind, src.reference.name),
        Some(_) => format!("{}/{}", src.reference.kind, src.reference.name),
    }
}

fn dep_level(dep: &Node) -> Level {
    match dep.obj.as_ref().and_then(ready) {
        Some((s, _, _)) if s == "True" => Level::Good,
        Some((s, _, _)) if s == "False" => Level::Critical,
        _ => Level::Warn,
    }
}

fn dep_line(dep: &Node) -> String {
    let state = match dep.obj.as_ref().and_then(ready) {
        Some((s, _, _)) if s == "True" => "ready",
        Some((s, _, _)) if s == "False" => "not ready",
        None if dep.obj.is_none() => "not found",
        _ => "unknown",
    };
    format!("{} — {state}", dep.reference.name)
}

/// Argo's related objects (AppProject, ApplicationSet) carry no Ready
/// condition — the only thing to report is whether they exist.
fn related_level(dep: &Node) -> Level {
    if dep.obj.is_some() {
        Level::Info
    } else {
        Level::Warn
    }
}

fn related_line(dep: &Node) -> String {
    let suffix = if dep.obj.is_some() {
        ""
    } else {
        " (not found)"
    };
    format!("{}/{}{suffix}", dep.reference.kind, dep.reference.name)
}

/// What's blocking Flux reconciliation, or that it's healthy.
fn flux_reconciliation_summary(ev: &Evidence, owner: &Node) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    if let Some(o) = owner.obj.as_ref()
        && suspended(o)
    {
        out.push((Level::Warn, "owner is suspended — not reconciling".into()));
    }
    if let Some(src) = &ev.source
        && !is_ready(src)
    {
        out.push((
            Level::Critical,
            format!("blocked: source {} is not ready", src.reference.name),
        ));
    }
    for dep in &ev.deps {
        if !is_ready(dep) {
            out.push((
                Level::Critical,
                format!("waiting on dependency {}", dep.reference.name),
            ));
        }
    }
    if out.is_empty() {
        match owner.obj.as_ref().and_then(ready) {
            Some((s, _, _)) if s == "True" => {
                let rev = owner
                    .obj
                    .as_ref()
                    .and_then(applied_revision)
                    .map(|r| format!(" at {}", short(&r)))
                    .unwrap_or_default();
                out.push((Level::Good, format!("reconciled{rev}")));
            }
            Some((_, reason, msg)) => {
                out.push((Level::Critical, join("not ready", &reason, &msg)));
            }
            None => out.push((Level::Info, "not yet reconciled".into())),
        }
    }
    out
}

/// What's blocking the Argo Application, or that it's synced and healthy.
fn argo_reconciliation_summary(owner: &Node) -> Vec<(Level, String)> {
    let Some(o) = owner.obj.as_ref() else {
        return vec![(Level::Warn, "owner not found — cannot say".into())];
    };
    if is_appset(owner) {
        return match condition(o, "ErrorOccurred") {
            Some((s, reason, msg)) if s == "True" => {
                vec![(Level::Critical, join("generation failed", &reason, &msg))]
            }
            _ => vec![(Level::Good, "generating applications".into())],
        };
    }

    let mut out = Vec::new();
    if !argo_automated(o) {
        out.push((
            Level::Warn,
            "sync policy is manual — nothing will reconcile drift".into(),
        ));
    }
    let (sync, revision) = argo_sync(o);
    if sync != "Synced" {
        let (drifted, total) = argo_resource_drift(o);
        let detail = if total > 0 {
            format!(" — {drifted} of {total} resources differ")
        } else {
            String::new()
        };
        out.push((Level::Warn, format!("{sync} with the source{detail}")));
    }
    let (health, health_msg) = argo_health(o);
    if health != "Healthy" {
        out.push((
            argo_health_level(&health),
            join(&format!("health is {health}"), "", &health_msg),
        ));
    }
    if let Some((phase, msg)) = argo_operation(o)
        && matches!(phase.as_str(), "Failed" | "Error")
    {
        out.push((Level::Critical, join("last sync failed", "", &msg)));
    }
    if out.is_empty() {
        let rev = if revision.is_empty() {
            String::new()
        } else {
            format!(" at {}", short_rev(&revision))
        };
        out.push((Level::Good, format!("synced and healthy{rev}")));
    }
    out
}

fn is_ready(node: &Node) -> bool {
    matches!(node.obj.as_ref().and_then(ready), Some((s, _, _)) if s == "True")
}

fn finding(indent: u8, level: Level, text: impl Into<String>) -> Finding {
    Finding {
        indent,
        level,
        text: text.into(),
        target: None,
    }
}

/// Join `status`, `reason`, `message` compactly (dropping empties).
fn join(status: &str, reason: &str, msg: &str) -> String {
    let mut s = status.to_string();
    if !reason.is_empty() {
        s.push_str(&format!(" ({reason})"));
    }
    if !msg.is_empty() {
        s.push_str(&format!(" — {}", short(msg)));
    }
    s
}

/// Trim a revision/message to something that fits on a line.
fn short(s: &str) -> String {
    crate::text::ellipsize(s.trim(), 80)
}

/// Argo records the full 40-character commit SHA; show it the way git does.
/// Anything else (a chart version, a branch) is left alone.
fn short_rev(s: &str) -> String {
    let s = s.trim();
    if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return s[..7].to_string();
    }
    short(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    fn node(kind: &str, name: &str, plural: &str, ns: &str, o: Option<Value>) -> Node {
        Node {
            reference: ObjRef {
                kind: kind.into(),
                name: name.into(),
                namespace: ns.into(),
            },
            plural: plural.into(),
            obj: o.map(obj),
        }
    }

    fn managed(engine: Engine, node: Node) -> Managed {
        Managed {
            engine,
            node,
            inferred: false,
        }
    }

    fn lines(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(|f| f.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn owner_ref_reads_toolkit_labels() {
        let d = obj(json!({
            "apiVersion":"apps/v1","kind":"Deployment",
            "metadata":{"name":"api","namespace":"apps","labels":{
                "kustomize.toolkit.fluxcd.io/name":"apps",
                "kustomize.toolkit.fluxcd.io/namespace":"flux-system"}}
        }));
        let o = owner_ref(&d).unwrap();
        assert_eq!(o.engine, Engine::Flux);
        assert_eq!(o.reference.kind, "Kustomization");
        assert_eq!(o.reference.name, "apps");
        assert_eq!(o.reference.namespace, "flux-system");

        let helm = obj(json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"c","labels":{"helm.toolkit.fluxcd.io/name":"podinfo",
                "helm.toolkit.fluxcd.io/namespace":"default"}}
        }));
        assert_eq!(owner_ref(&helm).unwrap().reference.kind, "HelmRelease");

        // No labels → not managed.
        assert!(owner_ref(&obj(json!({"metadata":{"name":"x"}}))).is_none());
    }

    #[test]
    fn argo_owner_ref_prefers_the_tracking_annotation() {
        // The annotation wins over both labels, and carries the app name in
        // its first field.
        let d = obj(json!({
            "apiVersion":"apps/v1","kind":"Deployment",
            "metadata":{"name":"api","namespace":"prod",
                "annotations":{"argocd.argoproj.io/tracking-id":
                    "api:apps/Deployment:prod/api"},
                "labels":{"app.kubernetes.io/instance":"something-else"}}
        }));
        let o = owner_ref(&d).unwrap();
        assert_eq!(o.engine, Engine::Argo);
        assert_eq!(o.reference.kind, "Application");
        assert_eq!(o.reference.name, "api");
        // No namespace in the stamp — the app layer searches for it.
        assert_eq!(o.reference.namespace, "");
        assert!(!o.inferred);

        // An app outside the controller's namespace is stamped `<ns>_<name>`.
        let elsewhere = obj(json!({
            "metadata":{"name":"api","labels":{
                "argocd.argoproj.io/instance":"team-a_api"}}
        }));
        let o = owner_ref(&elsewhere).unwrap();
        assert_eq!(o.reference.namespace, "team-a");
        assert_eq!(o.reference.name, "api");
        assert!(!o.inferred);

        // The generic label is only a guess — Helm sets it too.
        let weak = obj(json!({
            "metadata":{"name":"api","labels":{"app.kubernetes.io/instance":"api"}}
        }));
        assert!(owner_ref(&weak).unwrap().inferred);
    }

    #[test]
    fn flux_labels_win_over_an_argo_instance_label() {
        // Flux applying a chart that sets `app.kubernetes.io/instance` must
        // not read as Argo-managed.
        let d = obj(json!({
            "metadata":{"name":"api","labels":{
                "helm.toolkit.fluxcd.io/name":"podinfo",
                "app.kubernetes.io/instance":"podinfo"}}
        }));
        let o = owner_ref(&d).unwrap();
        assert_eq!(o.engine, Engine::Flux);
    }

    #[test]
    fn owner_kinds_are_group_checked_for_argo() {
        assert_eq!(owner_engine("kustomizations", ""), Some(Engine::Flux));
        assert_eq!(
            owner_engine("applications", "argoproj.io"),
            Some(Engine::Argo)
        );
        // Some other CRD that happens to be called `applications`.
        assert_eq!(owner_engine("applications", "app.k8s.io"), None);
        assert_eq!(owner_engine("pods", ""), None);
    }

    #[test]
    fn extracts_source_ref_and_depends_on() {
        let ks = obj(json!({
            "apiVersion":"kustomize.toolkit.fluxcd.io/v1","kind":"Kustomization",
            "metadata":{"name":"apps","namespace":"flux-system"},
            "spec":{
                "sourceRef":{"kind":"GitRepository","name":"flux-system"},
                "dependsOn":[{"name":"infra"},{"name":"crds","namespace":"other"}]
            }
        }));
        let src = source_ref(&ks).unwrap();
        assert_eq!(src.kind, "GitRepository");
        assert_eq!(src.name, "flux-system");
        assert_eq!(src.namespace, "flux-system"); // defaulted to owner ns
        let deps = depends_on(&ks);
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].namespace, "flux-system");
        assert_eq!(deps[1].namespace, "other");
    }

    #[test]
    fn extracts_argo_project_parent_and_sources() {
        let app = obj(json!({
            "apiVersion":"argoproj.io/v1alpha1","kind":"Application",
            "metadata":{"name":"api","namespace":"argocd","ownerReferences":[
                {"apiVersion":"argoproj.io/v1alpha1","kind":"ApplicationSet",
                 "name":"prod-apps","uid":"1"}]},
            "spec":{"project":"prod",
                "sources":[
                    {"repoURL":"https://github.com/acme/gitops","path":"apps/api",
                     "targetRevision":"main"},
                    {"repoURL":"https://charts.acme.io","chart":"redis",
                     "targetRevision":"18.1.2"}]}
        }));
        let project = argo_project_ref(&app).unwrap();
        assert_eq!(project.kind, "AppProject");
        assert_eq!(project.name, "prod");
        assert_eq!(project.namespace, "argocd");
        let parent = argo_parent_ref(&app).unwrap();
        assert_eq!(parent.name, "prod-apps");
        assert_eq!(
            argo_sources(&app),
            vec![
                "https://github.com/acme/gitops @ main (apps/api)".to_string(),
                "https://charts.acme.io @ 18.1.2 (chart redis)".to_string(),
            ]
        );

        // An ApplicationSet keeps all of that under its template.
        let appset = obj(json!({
            "apiVersion":"argoproj.io/v1alpha1","kind":"ApplicationSet",
            "metadata":{"name":"prod-apps","namespace":"argocd"},
            "spec":{"template":{"spec":{"project":"prod",
                "source":{"repoURL":"https://github.com/acme/gitops","path":"apps"}}}}
        }));
        assert_eq!(argo_project_ref(&appset).unwrap().name, "prod");
        assert_eq!(
            argo_sources(&appset),
            vec!["https://github.com/acme/gitops (apps)".to_string()]
        );
    }

    #[test]
    fn unmanaged_object_says_so() {
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: None,
            source: None,
            deps: vec![],
        };
        let f = describe(&ev);
        assert!(f[0].text.contains("not managed by Flux or Argo CD"));
    }

    #[test]
    fn an_inferred_owner_that_is_not_there_reads_as_unmanaged() {
        // `app.kubernetes.io/instance` on a Helm-installed object names no
        // Application — saying "owner not found" would be a false alarm.
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: Some(Managed {
                engine: Engine::Argo,
                node: node("Application", "api", "applications", "", None),
                inferred: true,
            }),
            source: None,
            deps: vec![],
        };
        assert!(describe(&ev)[0].text.contains("not managed by"));
    }

    #[test]
    fn healthy_chain_reports_reconciled_and_targets() {
        let owner = node(
            "Kustomization",
            "apps",
            "kustomizations",
            "flux-system",
            Some(json!({
                "metadata":{"name":"apps","namespace":"flux-system"},
                "status":{"lastAppliedRevision":"main@sha1:abcdef",
                    "conditions":[{"type":"Ready","status":"True","reason":"ReconciliationSucceeded"}]}
            })),
        );
        let source = node(
            "GitRepository",
            "flux-system",
            "gitrepositories",
            "flux-system",
            Some(json!({
                "metadata":{"name":"flux-system","namespace":"flux-system"},
                "status":{"artifact":{"revision":"main@sha1:abcdef"},
                    "conditions":[{"type":"Ready","status":"True"}]}
            })),
        );
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: Some(managed(Engine::Flux, owner)),
            source: Some(source),
            deps: vec![],
        };
        let f = describe(&ev);
        let joined = lines(&f);
        assert!(
            joined.contains("managed by Flux Kustomization/apps"),
            "{joined}"
        );
        assert!(
            joined.contains("applied revision main@sha1:abcdef"),
            "{joined}"
        );
        assert!(joined.contains("reconciled at"), "{joined}");
        // The owner line carries a jump target.
        let owner_line = f.iter().find(|x| x.text == "Kustomization/apps").unwrap();
        assert_eq!(owner_line.target.as_ref().unwrap().plural, "kustomizations");
    }

    #[test]
    fn blocked_by_source_and_dependency() {
        let owner = node(
            "Kustomization",
            "apps",
            "kustomizations",
            "flux-system",
            Some(json!({
                "metadata":{"name":"apps","namespace":"flux-system"},
                "status":{"conditions":[{"type":"Ready","status":"False","reason":"DependencyNotReady"}]}
            })),
        );
        let source = node(
            "GitRepository",
            "flux-system",
            "gitrepositories",
            "flux-system",
            Some(json!({
                "metadata":{"name":"flux-system"},
                "status":{"conditions":[{"type":"Ready","status":"False","reason":"GitOperationFailed"}]}
            })),
        );
        let dep = node(
            "Kustomization",
            "infra",
            "kustomizations",
            "flux-system",
            Some(json!({
                "metadata":{"name":"infra"},
                "status":{"conditions":[{"type":"Ready","status":"False"}]}
            })),
        );
        let ev = Evidence {
            subject: "Kustomization/apps".into(),
            self_is_owner: true,
            owner: Some(managed(Engine::Flux, owner)),
            source: Some(source),
            deps: vec![dep],
        };
        let joined = lines(&describe(&ev));
        assert!(
            joined.contains("blocked: source flux-system is not ready"),
            "{joined}"
        );
        assert!(joined.contains("waiting on dependency infra"), "{joined}");
    }

    #[test]
    fn suspended_owner_is_flagged() {
        let owner = node(
            "Kustomization",
            "apps",
            "kustomizations",
            "flux-system",
            Some(json!({
                "metadata":{"name":"apps"},
                "spec":{"suspend":true},
                "status":{"conditions":[{"type":"Ready","status":"True"}]}
            })),
        );
        let ev = Evidence {
            subject: "Kustomization/apps".into(),
            self_is_owner: true,
            owner: Some(managed(Engine::Flux, owner)),
            source: None,
            deps: vec![],
        };
        let joined = lines(&describe(&ev));
        assert!(joined.contains("suspended"), "{joined}");
    }

    fn argo_app(status: Value) -> Value {
        json!({
            "apiVersion":"argoproj.io/v1alpha1","kind":"Application",
            "metadata":{"name":"api","namespace":"argocd"},
            "spec":{"project":"prod",
                "syncPolicy":{"automated":{"prune":true,"selfHeal":true}},
                "source":{"repoURL":"https://github.com/acme/gitops","path":"apps/api",
                    "targetRevision":"main"}},
            "status": status
        })
    }

    #[test]
    fn argo_chain_reports_synced_and_healthy_with_targets() {
        let owner = node(
            "Application",
            "api",
            "applications",
            "argocd",
            Some(argo_app(json!({
                "sync":{"status":"Synced",
                    "revision":"0123456789abcdef0123456789abcdef01234567"},
                "health":{"status":"Healthy"},
                "operationState":{"phase":"Succeeded"}
            }))),
        );
        let project = node(
            "AppProject",
            "prod",
            "appprojects",
            "argocd",
            Some(json!({"metadata":{"name":"prod"}})),
        );
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: Some(managed(Engine::Argo, owner)),
            source: None,
            deps: vec![project],
        };
        let f = describe(&ev);
        let joined = lines(&f);
        assert!(
            joined.contains("managed by Argo CD Application/api"),
            "{joined}"
        );
        assert!(joined.contains("Sync: Synced"), "{joined}");
        assert!(joined.contains("Health: Healthy"), "{joined}");
        // The 40-char SHA is abbreviated the way git shows it.
        assert!(joined.contains("synced revision 0123456"), "{joined}");
        assert!(joined.contains("sync policy: automated"), "{joined}");
        assert!(
            joined.contains("https://github.com/acme/gitops @ main (apps/api)"),
            "{joined}"
        );
        assert!(joined.contains("synced and healthy at 0123456"), "{joined}");
        // Both the Application and its project are jumpable.
        let app_line = f.iter().find(|x| x.text == "Application/api").unwrap();
        assert_eq!(app_line.target.as_ref().unwrap().plural, "applications");
        let project_line = f.iter().find(|x| x.text == "AppProject/prod").unwrap();
        assert_eq!(project_line.target.as_ref().unwrap().plural, "appprojects");
    }

    #[test]
    fn argo_drift_degraded_health_and_failed_sync_are_flagged() {
        let owner = node(
            "Application",
            "api",
            "applications",
            "argocd",
            Some(argo_app(json!({
                "sync":{"status":"OutOfSync","revision":"main"},
                "health":{"status":"Degraded","message":"pod api-0 is in CrashLoopBackOff"},
                "operationState":{"phase":"Failed","message":"one or more objects failed"},
                "resources":[
                    {"kind":"Deployment","name":"api","status":"OutOfSync"},
                    {"kind":"Service","name":"api","status":"Synced"},
                    {"kind":"ConfigMap","name":"api","status":"Synced"}]
            }))),
        );
        let ev = Evidence {
            subject: "Application/api".into(),
            self_is_owner: true,
            owner: Some(managed(Engine::Argo, owner)),
            source: None,
            deps: vec![],
        };
        let f = describe(&ev);
        let joined = lines(&f);
        assert!(
            joined.contains("Application/api — Argo CD Application"),
            "{joined}"
        );
        assert!(
            joined.contains("OutOfSync with the source — 1 of 3 resources differ"),
            "{joined}"
        );
        assert!(
            joined.contains("health is Degraded — pod api-0 is in CrashLoopBackOff"),
            "{joined}"
        );
        assert!(
            joined.contains("last sync failed — one or more objects failed"),
            "{joined}"
        );
        // The headline reads critical, not merely "not ready".
        assert_eq!(f[0].level, Level::Critical);
    }

    #[test]
    fn argo_manual_sync_policy_is_flagged() {
        // What sofka's own `t` → Suspend leaves behind: no automation.
        let mut app = argo_app(json!({
            "sync":{"status":"OutOfSync"},"health":{"status":"Healthy"}
        }));
        app["spec"].as_object_mut().unwrap().remove("syncPolicy");
        let owner = node("Application", "api", "applications", "argocd", Some(app));
        let ev = Evidence {
            subject: "Application/api".into(),
            self_is_owner: true,
            owner: Some(managed(Engine::Argo, owner)),
            source: None,
            deps: vec![],
        };
        let joined = lines(&describe(&ev));
        assert!(
            joined.contains("sync policy: manual (no automated sync)"),
            "{joined}"
        );
        assert!(
            joined.contains("sync policy is manual — nothing will reconcile drift"),
            "{joined}"
        );
    }

    #[test]
    fn argo_applicationset_reports_its_conditions() {
        let owner = node(
            "ApplicationSet",
            "prod-apps",
            "applicationsets",
            "argocd",
            Some(json!({
                "apiVersion":"argoproj.io/v1alpha1","kind":"ApplicationSet",
                "metadata":{"name":"prod-apps","namespace":"argocd"},
                "spec":{"template":{"spec":{"project":"prod"}}},
                "status":{"conditions":[
                    {"type":"ErrorOccurred","status":"True","reason":"ApplicationGenerationFromParamsError",
                     "message":"error generating params"},
                    {"type":"ResourcesUpToDate","status":"False"}]}
            })),
        );
        let ev = Evidence {
            subject: "ApplicationSet/prod-apps".into(),
            self_is_owner: true,
            owner: Some(managed(Engine::Argo, owner)),
            source: None,
            deps: vec![],
        };
        let f = describe(&ev);
        let joined = lines(&f);
        assert!(
            joined.contains("ErrorOccurred: True (ApplicationGenerationFromParamsError)"),
            "{joined}"
        );
        assert!(joined.contains("generation failed"), "{joined}");
        assert_eq!(f[0].level, Level::Critical);
    }

    #[test]
    fn a_missing_argo_owner_is_reported_not_swallowed() {
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: Some(managed(
                Engine::Argo,
                node("Application", "api", "applications", "argocd", None),
            )),
            source: None,
            deps: vec![],
        };
        let joined = lines(&describe(&ev));
        assert!(joined.contains("owner not found in cluster"), "{joined}");
    }
}
