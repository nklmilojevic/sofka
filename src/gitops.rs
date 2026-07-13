//! Flux GitOps ownership and reconciliation analysis.
//!
//! Every object Flux applies is stamped with `kustomize.toolkit.fluxcd.io/name`
//! (+`/namespace`) or `helm.toolkit.fluxcd.io/name` labels naming the
//! Kustomization / HelmRelease that manages it. From there the chain runs
//! owner → source (GitRepository/OCIRepository/HelmChart/…) → the revision
//! actually applied, plus any `dependsOn` Kustomizations that gate it.
//!
//! This module is pure: it extracts the references to follow (so the app knows
//! what to fetch) and, given the fetched objects, formats the chain into
//! ranked [`Finding`]s with jump targets. The app layer does the fetching and
//! renders/navigates the findings.

use kube::core::DynamicObject;
use serde_json::Value;

use crate::explain::{Finding, Level, Target};

/// A reference to a Flux object: its kind, name, and namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FluxRef {
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

/// A node in the reconciliation chain: the reference, its resolved plural (for
/// a jump target; empty when the kind couldn't be resolved), and the fetched
/// object (`None` when missing or unfetched).
#[derive(Debug, Clone)]
pub struct Node {
    pub reference: FluxRef,
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

/// The gathered reconciliation picture for one selected object.
pub struct Evidence {
    /// e.g. `Deployment/api`.
    pub subject: String,
    /// The selected object is itself the Kustomization/HelmRelease.
    pub self_is_owner: bool,
    /// The managing Kustomization/HelmRelease (or the object itself).
    pub owner: Option<Node>,
    /// The source the owner reconciles from.
    pub source: Option<Node>,
    /// `dependsOn` Kustomizations that gate the owner.
    pub deps: Vec<Node>,
}

// ----- reference extraction (used by the app to know what to fetch) --------

/// The Flux owner named by a managed object's toolkit labels, if any.
pub fn owner_ref(obj: &DynamicObject) -> Option<FluxRef> {
    let labels = obj.metadata.labels.as_ref()?;
    let get = |k: &str| labels.get(k).cloned();
    if let Some(name) = get("kustomize.toolkit.fluxcd.io/name") {
        return Some(FluxRef {
            kind: "Kustomization".into(),
            name,
            namespace: get("kustomize.toolkit.fluxcd.io/namespace").unwrap_or_default(),
        });
    }
    if let Some(name) = get("helm.toolkit.fluxcd.io/name") {
        return Some(FluxRef {
            kind: "HelmRelease".into(),
            name,
            namespace: get("helm.toolkit.fluxcd.io/namespace").unwrap_or_default(),
        });
    }
    None
}

/// Whether `plural` is a Flux Kustomization/HelmRelease (an owner kind).
pub fn is_owner_plural(plural: &str) -> bool {
    matches!(plural, "kustomizations" | "helmreleases")
}

/// The source a Kustomization/HelmRelease reconciles from. Kustomizations use
/// `spec.sourceRef`; HelmRelease v2 GA uses `spec.chartRef`, older ones
/// `spec.chart.spec.sourceRef` (a HelmChart the controller creates).
pub fn source_ref(owner: &DynamicObject) -> Option<FluxRef> {
    let d = &owner.data;
    let owner_ns = owner.metadata.namespace.clone().unwrap_or_default();
    let from = |v: &Value| -> Option<FluxRef> {
        Some(FluxRef {
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
pub fn depends_on(owner: &DynamicObject) -> Vec<FluxRef> {
    let owner_ns = owner.metadata.namespace.clone().unwrap_or_default();
    owner
        .data
        .pointer("/spec/dependsOn")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(|d| {
                    Some(FluxRef {
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

// ----- object state accessors ----------------------------------------------

/// `(status, reason, message)` of the object's `Ready` condition.
pub fn ready(obj: &DynamicObject) -> Option<(String, String, String)> {
    let cond = obj
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))?;
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

// ----- findings -------------------------------------------------------------

/// Render the reconciliation chain into ranked findings with jump targets.
pub fn describe(ev: &Evidence) -> Vec<Finding> {
    let mut out = Vec::new();

    let Some(owner) = &ev.owner else {
        out.push(finding(
            0,
            Level::Info,
            format!("{} is not managed by Flux", ev.subject),
        ));
        out.push(finding(
            1,
            Level::Info,
            "no kustomize/helm toolkit labels found",
        ));
        return out;
    };

    // Headline.
    if ev.self_is_owner {
        out.push(finding(
            0,
            owner_health_level(owner),
            format!("{} — Flux {}", ev.subject, owner.reference.kind),
        ));
    } else {
        out.push(finding(
            0,
            owner_health_level(owner),
            format!(
                "{} is managed by {}/{}",
                ev.subject, owner.reference.kind, owner.reference.name
            ),
        ));
    }

    // Owner block.
    out.push(finding(0, Level::Heading, "Owner"));
    push_flux_object(&mut out, owner, ev.self_is_owner);

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
    for line in reconciliation_summary(ev, owner) {
        out.push(finding(1, line.0, line.1));
    }

    out
}

/// The owner's health, for the headline and its own line.
fn owner_health_level(owner: &Node) -> Level {
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

/// Append the owner's own detail lines (state, suspend, revisions).
fn push_flux_object(out: &mut Vec<Finding>, owner: &Node, self_is_owner: bool) {
    // The owner's identity line (with a jump target unless it's the selection).
    let mut head = finding(
        1,
        owner_health_level(owner),
        format!("{}/{}", owner.reference.kind, owner.reference.name),
    );
    if !self_is_owner && let Some(t) = owner.target() {
        head = head.with_target(t);
    }
    out.push(head);

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

/// What's blocking reconciliation, or that it's healthy.
fn reconciliation_summary(ev: &Evidence, owner: &Node) -> Vec<(Level, String)> {
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
    let s = s.trim();
    if s.chars().count() <= 80 {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(79).collect();
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

    fn node(kind: &str, name: &str, plural: &str, o: Option<Value>) -> Node {
        Node {
            reference: FluxRef {
                kind: kind.into(),
                name: name.into(),
                namespace: "flux-system".into(),
            },
            plural: plural.into(),
            obj: o.map(obj),
        }
    }

    #[test]
    fn owner_ref_reads_toolkit_labels() {
        let d = obj(json!({
            "apiVersion":"apps/v1","kind":"Deployment",
            "metadata":{"name":"api","namespace":"apps","labels":{
                "kustomize.toolkit.fluxcd.io/name":"apps",
                "kustomize.toolkit.fluxcd.io/namespace":"flux-system"}}
        }));
        let r = owner_ref(&d).unwrap();
        assert_eq!(r.kind, "Kustomization");
        assert_eq!(r.name, "apps");
        assert_eq!(r.namespace, "flux-system");

        let helm = obj(json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"c","labels":{"helm.toolkit.fluxcd.io/name":"podinfo",
                "helm.toolkit.fluxcd.io/namespace":"default"}}
        }));
        assert_eq!(owner_ref(&helm).unwrap().kind, "HelmRelease");

        // No labels → not managed.
        assert!(owner_ref(&obj(json!({"metadata":{"name":"x"}}))).is_none());
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
    fn unmanaged_object_says_so() {
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: None,
            source: None,
            deps: vec![],
        };
        let f = describe(&ev);
        assert!(f[0].text.contains("not managed by Flux"));
    }

    #[test]
    fn healthy_chain_reports_reconciled_and_targets() {
        let owner = node(
            "Kustomization",
            "apps",
            "kustomizations",
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
            Some(json!({
                "metadata":{"name":"flux-system","namespace":"flux-system"},
                "status":{"artifact":{"revision":"main@sha1:abcdef"},
                    "conditions":[{"type":"Ready","status":"True"}]}
            })),
        );
        let ev = Evidence {
            subject: "Deployment/api".into(),
            self_is_owner: false,
            owner: Some(owner),
            source: Some(source),
            deps: vec![],
        };
        let f = describe(&ev);
        let all: Vec<&str> = f.iter().map(|x| x.text.as_str()).collect();
        let joined = all.join("\n");
        assert!(joined.contains("managed by Kustomization/apps"), "{joined}");
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
            Some(json!({
                "metadata":{"name":"apps","namespace":"flux-system"},
                "status":{"conditions":[{"type":"Ready","status":"False","reason":"DependencyNotReady"}]}
            })),
        );
        let source = node(
            "GitRepository",
            "flux-system",
            "gitrepositories",
            Some(json!({
                "metadata":{"name":"flux-system"},
                "status":{"conditions":[{"type":"Ready","status":"False","reason":"GitOperationFailed"}]}
            })),
        );
        let dep = node(
            "Kustomization",
            "infra",
            "kustomizations",
            Some(json!({
                "metadata":{"name":"infra"},
                "status":{"conditions":[{"type":"Ready","status":"False"}]}
            })),
        );
        let ev = Evidence {
            subject: "Kustomization/apps".into(),
            self_is_owner: true,
            owner: Some(owner),
            source: Some(source),
            deps: vec![dep],
        };
        let f = describe(&ev);
        let joined: String = f
            .iter()
            .map(|x| x.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
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
            Some(json!({
                "metadata":{"name":"apps"},
                "spec":{"suspend":true},
                "status":{"conditions":[{"type":"Ready","status":"True"}]}
            })),
        );
        let ev = Evidence {
            subject: "Kustomization/apps".into(),
            self_is_owner: true,
            owner: Some(owner),
            source: None,
            deps: vec![],
        };
        let joined: String = describe(&ev)
            .iter()
            .map(|x| x.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("suspended"), "{joined}");
    }
}
