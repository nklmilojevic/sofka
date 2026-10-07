//! Rollout history and undo for Deployments, StatefulSets, and DaemonSets.
//!
//! A Deployment keeps one ReplicaSet per pod template it has rolled out, each
//! numbered by its `deployment.kubernetes.io/revision` annotation. StatefulSets
//! and DaemonSets keep ControllerRevisions instead, numbered by their
//! `revision` field, whose `data` is the strategic merge patch that restores
//! that template. Either way the highest revision is the one in effect: rolling
//! back renumbers the restored revision as the newest.
//!
//! Rolling back works the way `kubectl rollout undo` does: it copies the
//! revision's pod template onto the workload, and the controller rolls it out.

use kube::core::DynamicObject;
use serde_json::{Map, Value, json};

/// The synthetic plural of the history view. The view is backed by
/// `replicasets` or `controllerrevisions`, depending on the workload.
pub const VIEW: &str = "rollouthistory";

pub const REVISION_ANNOTATION: &str = "deployment.kubernetes.io/revision";
pub const CHANGE_CAUSE_ANNOTATION: &str = "kubernetes.io/change-cause";
const POD_TEMPLATE_HASH: &str = "pod-template-hash";

/// ReplicaSet annotations the Deployment controller owns, which a rollback
/// must not copy back onto the Deployment (kubectl skips the same set).
const SKIPPED_ANNOTATIONS: &[&str] = &[
    "kubectl.kubernetes.io/last-applied-configuration",
    REVISION_ANNOTATION,
    "deployment.kubernetes.io/revision-history",
    "deployment.kubernetes.io/desired-replicas",
    "deployment.kubernetes.io/max-replicas",
    "deprecated.deployment.rollback.to",
];

/// A workload kind with rollout history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    Deployment,
    StatefulSet,
    DaemonSet,
}

impl Workload {
    pub fn from_plural(plural: &str) -> Option<Self> {
        match plural {
            "deployments" => Some(Self::Deployment),
            "statefulsets" => Some(Self::StatefulSet),
            "daemonsets" => Some(Self::DaemonSet),
            _ => None,
        }
    }

    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "Deployment" => Some(Self::Deployment),
            "StatefulSet" => Some(Self::StatefulSet),
            "DaemonSet" => Some(Self::DaemonSet),
            _ => None,
        }
    }

    pub fn kind(self) -> &'static str {
        match self {
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
            Self::DaemonSet => "DaemonSet",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Deployment => "deployments",
            Self::StatefulSet => "statefulsets",
            Self::DaemonSet => "daemonsets",
        }
    }

    /// The short name used in breadcrumbs (`deploy/web`).
    pub fn short(self) -> &'static str {
        match self {
            Self::Deployment => "deploy",
            Self::StatefulSet => "sts",
            Self::DaemonSet => "ds",
        }
    }

    /// The kind that stores this workload's revisions.
    pub fn backing_plural(self) -> &'static str {
        match self {
            Self::Deployment => "replicasets",
            Self::StatefulSet | Self::DaemonSet => "controllerrevisions",
        }
    }
}

/// A revision object's number: the ReplicaSet annotation, or the
/// ControllerRevision `revision` field.
pub fn revision(obj: &DynamicObject) -> Option<i64> {
    if let Some(n) = obj.data.get("revision").and_then(Value::as_i64) {
        return Some(n);
    }
    obj.metadata
        .annotations
        .as_ref()?
        .get(REVISION_ANNOTATION)?
        .parse()
        .ok()
}

/// The pod template a revision restores, without the parts that only exist
/// on the revision object: a ReplicaSet's `pod-template-hash` label, and a
/// ControllerRevision's `$patch` directive.
pub fn template(obj: &DynamicObject) -> Option<Value> {
    let mut template = obj
        .data
        .pointer("/spec/template")
        .or_else(|| obj.data.pointer("/data/spec/template"))?
        .clone();
    if let Some(map) = template.as_object_mut() {
        map.remove("$patch");
    }
    if let Some(labels) = template
        .pointer_mut("/metadata/labels")
        .and_then(Value::as_object_mut)
    {
        labels.remove(POD_TEMPLATE_HASH);
        if labels.is_empty()
            && let Some(meta) = template
                .pointer_mut("/metadata")
                .and_then(Value::as_object_mut)
        {
            meta.remove("labels");
        }
    }
    Some(template)
}

/// The images of a revision's containers, comma-separated.
pub fn images(obj: &DynamicObject) -> String {
    let Some(template) = template(obj) else {
        return String::new();
    };
    template
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("image").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(",")
}

/// The `kubernetes.io/change-cause` annotation, if the revision has one.
pub fn change_cause(obj: &DynamicObject) -> &str {
    obj.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(CHANGE_CAUSE_ANNOTATION))
        .map_or("", String::as_str)
}

/// The revision in effect: the highest number among `revisions`.
pub fn current<'a>(revisions: impl IntoIterator<Item = &'a DynamicObject>) -> Option<i64> {
    revisions.into_iter().filter_map(revision).max()
}

/// `deployed` for the revision in effect, `superseded` for the rest, as
/// `helm history` labels them.
pub fn status(obj: &DynamicObject, current: Option<i64>) -> &'static str {
    if revision(obj).is_some() && revision(obj) == current {
        "deployed"
    } else {
        "superseded"
    }
}

/// The patch that rolls `workload` back to the template in `rev`.
///
/// For a Deployment this replaces the pod template and copies the
/// ReplicaSet's own annotations (its change cause, for one) back onto the
/// Deployment. For a StatefulSet or DaemonSet the ControllerRevision's `data`
/// already is that patch.
pub fn undo_patch(workload: Workload, rev: &DynamicObject) -> Option<Value> {
    match workload {
        Workload::Deployment => {
            let mut template = template(rev)?;
            template
                .as_object_mut()?
                .insert("$patch".into(), Value::String("replace".into()));
            let annotations: Map<String, Value> = rev
                .metadata
                .annotations
                .iter()
                .flatten()
                .filter(|(k, _)| !SKIPPED_ANNOTATIONS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            let mut patch = json!({ "spec": { "template": template } });
            if !annotations.is_empty() {
                patch["metadata"] = json!({ "annotations": annotations });
            }
            Some(patch)
        }
        Workload::StatefulSet | Workload::DaemonSet => rev.data.get("data").cloned(),
    }
}

/// Why `workload` cannot be rolled back to `rev` right now, if it cannot.
pub fn undo_blocker(workload: &DynamicObject, rev: &DynamicObject) -> Option<String> {
    if workload
        .data
        .pointer("/spec/paused")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some("the deployment is paused; resume it before rolling back".into());
    }
    let revision = revision(rev).unwrap_or_default();
    match (template(rev), workload.data.pointer("/spec/template")) {
        (Some(target), Some(live)) if &target == live => Some(format!(
            "the current template already matches revision {revision}"
        )),
        (None, _) => Some(format!("revision {revision} has no pod template")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    fn rs(revision: i64, image: &str) -> DynamicObject {
        obj(json!({
            "apiVersion": "apps/v1",
            "kind": "ReplicaSet",
            "metadata": {
                "name": format!("web-{revision}"),
                "namespace": "default",
                "annotations": {
                    REVISION_ANNOTATION: revision.to_string(),
                    "deployment.kubernetes.io/desired-replicas": "2",
                    CHANGE_CAUSE_ANNOTATION: format!("set image {image}"),
                },
            },
            "spec": {
                "template": {
                    "metadata": {"labels": {"app": "web", POD_TEMPLATE_HASH: "abc"}},
                    "spec": {"containers": [{"name": "web", "image": image}]},
                },
            },
        }))
    }

    fn controller_revision(revision: i64, image: &str) -> DynamicObject {
        obj(json!({
            "apiVersion": "apps/v1",
            "kind": "ControllerRevision",
            "metadata": {"name": format!("db-{revision}"), "namespace": "default"},
            "revision": revision,
            "data": {"spec": {"template": {
                "$patch": "replace",
                "metadata": {"labels": {"app": "db"}},
                "spec": {"containers": [{"name": "db", "image": image}]},
            }}},
        }))
    }

    #[test]
    fn revision_reads_replicaset_annotation_and_controllerrevision_field() {
        assert_eq!(revision(&rs(3, "web:1")), Some(3));
        assert_eq!(revision(&controller_revision(7, "db:1")), Some(7));
        assert_eq!(revision(&obj(json!({"metadata": {"name": "x"}}))), None);
    }

    #[test]
    fn template_drops_revision_only_fields() {
        let t = template(&rs(1, "web:1")).unwrap();
        assert_eq!(t["metadata"]["labels"], json!({"app": "web"}));
        let t = template(&controller_revision(1, "db:1")).unwrap();
        assert!(t.get("$patch").is_none());
        assert_eq!(t["spec"]["containers"][0]["image"], "db:1");
    }

    #[test]
    fn status_marks_only_the_highest_revision_deployed() {
        let revs = [rs(1, "a"), rs(3, "b"), rs(2, "c")];
        let current = current(&revs);
        assert_eq!(current, Some(3));
        assert_eq!(status(&revs[1], current), "deployed");
        assert_eq!(status(&revs[0], current), "superseded");
        assert_eq!(
            status(&obj(json!({"metadata": {"name": "x"}})), None),
            "superseded"
        );
    }

    #[test]
    fn deployment_undo_replaces_template_and_copies_own_annotations() {
        let patch = undo_patch(Workload::Deployment, &rs(2, "web:2")).unwrap();
        assert_eq!(patch["spec"]["template"]["$patch"], "replace");
        assert_eq!(
            patch["spec"]["template"]["metadata"]["labels"],
            json!({"app": "web"})
        );
        assert_eq!(
            patch["metadata"]["annotations"],
            json!({CHANGE_CAUSE_ANNOTATION: "set image web:2"})
        );
    }

    #[test]
    fn statefulset_undo_applies_the_revision_data() {
        let rev = controller_revision(4, "db:4");
        let patch = undo_patch(Workload::StatefulSet, &rev).unwrap();
        assert_eq!(patch, rev.data["data"]);
    }

    #[test]
    fn undo_blocker_refuses_paused_and_already_current() {
        let rev = rs(2, "web:2");
        let paused = obj(json!({
            "metadata": {"name": "web"},
            "spec": {"paused": true, "template": {}},
        }));
        assert!(undo_blocker(&paused, &rev).unwrap().contains("paused"));

        let same = obj(json!({
            "metadata": {"name": "web"},
            "spec": {"template": template(&rev).unwrap()},
        }));
        assert!(
            undo_blocker(&same, &rev)
                .unwrap()
                .contains("already matches revision 2")
        );

        let other = obj(json!({
            "metadata": {"name": "web"},
            "spec": {"template": template(&rs(3, "web:3")).unwrap()},
        }));
        assert_eq!(undo_blocker(&other, &rev), None);
    }
}
