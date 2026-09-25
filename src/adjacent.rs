//! Adjacency: the objects directly connected to one object — what owns it,
//! what it owns, what its spec names, and what names it. Pure: the rule
//! tables, the pointer walking, and the gather plan live here, unit-tested;
//! `app/adjacent.rs` runs the plan against the cluster and drives the view.

use std::collections::{HashMap, HashSet};

use kube::core::DynamicObject;
use kube::discovery::ApiResource;
use serde_json::Value;

use crate::store::AdjacentItem;
use crate::views::{View, key_namespace, key_plural, lookup_keys};

/// Which way a connection runs, as the row shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// The selection's owner (`↑ owned by`).
    Owner,
    /// An object the selection owns (`↓ owns`).
    Child,
    /// An object the selection's spec names (`→ mounts`).
    Names,
    /// An object whose spec names the selection (`← mounts`).
    NamedBy,
}

impl Direction {
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Owner => "↑",
            Self::Child => "↓",
            Self::Names => "→",
            Self::NamedBy => "←",
        }
    }
}

/// How the reverse direction of a [`RefRule`] is listed: which objects of the
/// rule's source kind name the selected object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reverse {
    /// List the source kind in the selected row's namespace — the namespace
    /// the table is showing, for a cluster-scoped row.
    #[default]
    Namespace,
    /// List the source kind across the cluster.
    Cluster,
    /// Don't look for usages through this rule.
    None,
}

impl Reverse {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "namespace" => Some(Self::Namespace),
            "cluster" => Some(Self::Cluster),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// One reference from objects of a kind to objects of another kind: "a Pod's
/// `spec.nodeName` names a Node".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRule {
    /// The source kind, keyed like `[views."…"]`: `apiVersion/plural`,
    /// `group/plural`, or a bare plural.
    pub from: String,
    /// JSON Pointer into the source object; a `*` segment fans out over every
    /// element of an array.
    pub path: String,
    /// Where the target's namespace lives when it isn't the source's own — a
    /// PersistentVolume's `claimRef.namespace`. Its `*` segments take the
    /// same array elements `path` did, so each name pairs with its own
    /// namespace.
    pub namespace_path: Option<String>,
    /// Target kind (alias, plural, or kind), resolved against the cluster.
    pub kind: String,
    pub kind_path: Option<String>,
    pub kinds: Vec<String>,
    /// Where the element's API group lives, beside its kind: `""` names the
    /// core group, and a missing group takes `group`, else any group.
    pub group_path: Option<String>,
    pub group: Option<String>,
    /// Relation label shown on the row: "mounts", "runs on".
    pub relation: String,
    pub reverse: Reverse,
}

impl RefRule {
    pub fn is_dynamic(&self) -> bool {
        self.kind_path.is_some()
    }

    pub fn target_label(&self) -> String {
        if self.kinds.is_empty() {
            self.kind.clone()
        } else {
            self.kinds.join("|")
        }
    }
}

struct BuiltinRef {
    from: &'static str,
    path: &'static str,
    namespace_path: Option<&'static str>,
    kind: &'static str,
    relation: &'static str,
    reverse: Reverse,
}

impl BuiltinRef {
    fn rule(&self) -> RefRule {
        RefRule {
            from: self.from.to_string(),
            path: self.path.to_string(),
            namespace_path: self.namespace_path.map(str::to_string),
            kind: self.kind.to_string(),
            kind_path: None,
            kinds: Vec::new(),
            group_path: None,
            group: None,
            relation: self.relation.to_string(),
            reverse: self.reverse,
        }
    }
}

const fn r(
    from: &'static str,
    path: &'static str,
    kind: &'static str,
    relation: &'static str,
    reverse: Reverse,
) -> BuiltinRef {
    BuiltinRef {
        from,
        path,
        namespace_path: None,
        kind,
        relation,
        reverse,
    }
}

/// References between the kinds common enough to ship. Data, not control
/// flow: `[[views."…".refs]]` adds rows for anything else, and the reverse
/// direction of every row is what finds usages ("pods mounting this PVC").
const BUILTIN_REFS: &[BuiltinRef] = &[
    r(
        "v1/pods",
        "/spec/nodeName",
        "nodes",
        "runs on",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/serviceAccountName",
        "serviceaccounts",
        "runs as",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/priorityClassName",
        "priorityclasses",
        "prioritized by",
        Reverse::None,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/persistentVolumeClaim/claimName",
        "persistentvolumeclaims",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/configMap/name",
        "configmaps",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/secret/secretName",
        "secrets",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/projected/sources/*/configMap/name",
        "configmaps",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/volumes/*/projected/sources/*/secret/name",
        "secrets",
        "mounts",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/envFrom/*/configMapRef/name",
        "configmaps",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/envFrom/*/secretRef/name",
        "secrets",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/env/*/valueFrom/configMapKeyRef/name",
        "configmaps",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/containers/*/env/*/valueFrom/secretKeyRef/name",
        "secrets",
        "reads",
        Reverse::Namespace,
    ),
    r(
        "v1/pods",
        "/spec/imagePullSecrets/*/name",
        "secrets",
        "pulls with",
        Reverse::Namespace,
    ),
    r(
        "v1/persistentvolumeclaims",
        "/spec/storageClassName",
        "storageclasses",
        "provisioned by",
        Reverse::Namespace,
    ),
    r(
        "v1/persistentvolumeclaims",
        "/spec/volumeAttributesClassName",
        "volumeattributesclasses",
        "attributes from",
        Reverse::Namespace,
    ),
    r(
        "v1/persistentvolumeclaims",
        "/spec/volumeName",
        "persistentvolumes",
        "bound to",
        Reverse::None,
    ),
    BuiltinRef {
        from: "v1/persistentvolumes",
        path: "/spec/claimRef/name",
        namespace_path: Some("/spec/claimRef/namespace"),
        kind: "persistentvolumeclaims",
        relation: "bound to",
        reverse: Reverse::None,
    },
    r(
        "v1/persistentvolumes",
        "/spec/storageClassName",
        "storageclasses",
        "provisioned by",
        Reverse::Cluster,
    ),
    r(
        "v1/serviceaccounts",
        "/secrets/*/name",
        "secrets",
        "tokens in",
        Reverse::None,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/rules/*/http/paths/*/backend/service/name",
        "services",
        "routes to",
        Reverse::Namespace,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/defaultBackend/service/name",
        "services",
        "routes to",
        Reverse::Namespace,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/tls/*/secretName",
        "secrets",
        "tls from",
        Reverse::Namespace,
    ),
    r(
        "networking.k8s.io/ingresses",
        "/spec/ingressClassName",
        "ingressclasses",
        "class",
        Reverse::None,
    ),
    r(
        "karpenter.sh/nodeclaims",
        "/status/nodeName",
        "nodes",
        "became",
        Reverse::Cluster,
    ),
];

/// Kinds owned through `ownerReferences` by objects of a kind — where a
/// row's children are looked for. `[views."…"].children` adds to it.
const BUILTIN_CHILDREN: &[(&str, &[&str])] = &[
    ("apps/deployments", &["replicasets"]),
    ("apps/replicasets", &["pods"]),
    ("apps/statefulsets", &["pods", "controllerrevisions"]),
    ("apps/daemonsets", &["pods", "controllerrevisions"]),
    ("batch/jobs", &["pods"]),
    ("batch/cronjobs", &["jobs"]),
    ("v1/services", &["endpointslices"]),
    ("karpenter.sh/nodepools", &["nodeclaims"]),
    ("karpenter.sh/nodeclaims", &["nodes"]),
];

/// Every reference whose source is `ar` in `namespace`: built-in rows keyed
/// like the view keys, then the `refs` of every configured view for the kind,
/// `@namespace`-qualified views included. Additive across keys — a specific
/// view's rows don't hide a broader key's.
pub fn rules_for(
    views: &HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Vec<RefRule> {
    let keys = lookup_keys(ar, namespace);
    let mut rules: Vec<RefRule> = BUILTIN_REFS
        .iter()
        .filter(|b| keys.iter().any(|k| k == b.from))
        .map(BuiltinRef::rule)
        .collect();
    for key in &keys {
        if let Some(view) = views.get(key) {
            rules.extend(view.refs.iter().cloned());
        }
    }
    rules
}

/// The plurals to scan for a row's children, built-in and configured, each
/// once.
pub fn children_for(
    views: &HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Vec<String> {
    let keys = lookup_keys(ar, namespace);
    let builtin = BUILTIN_CHILDREN
        .iter()
        .filter(|(from, _)| keys.iter().any(|k| k == from))
        .flat_map(|(_, kinds)| kinds.iter().map(|k| k.to_string()));
    let configured = keys
        .iter()
        .filter_map(|k| views.get(k))
        .flat_map(|v| v.children.iter().cloned());
    let mut seen = HashSet::new();
    builtin
        .chain(configured)
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Every reference known, built-in and configured. The reverse lookup scans
/// these for rules whose target is the selected kind.
pub fn all_rules(views: &HashMap<String, View>) -> Vec<RefRule> {
    let mut rules: Vec<RefRule> = BUILTIN_REFS.iter().map(BuiltinRef::rule).collect();
    for view in views.values() {
        rules.extend(view.refs.iter().cloned());
    }
    rules
}

// ----- pointers ---------------------------------------------------------

fn segments(path: &str) -> Option<Vec<String>> {
    let rest = path.strip_prefix('/')?;
    Some(
        rest.split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect(),
    )
}

/// Walk `segments` from `node`, fanning out at `*`, and record every
/// non-empty string reached together with the array indices the `*`s took.
fn expand(
    node: &Value,
    segments: &[String],
    taken: &mut Vec<usize>,
    out: &mut Vec<(Vec<usize>, String)>,
) {
    let Some((head, tail)) = segments.split_first() else {
        if let Value::String(s) = node
            && !s.is_empty()
        {
            out.push((taken.clone(), s.clone()));
        }
        return;
    };
    match (head.as_str(), node) {
        ("*", Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                taken.push(i);
                expand(item, tail, taken, out);
                taken.pop();
            }
        }
        (key, Value::Object(map)) => {
            if let Some(child) = map.get(key) {
                expand(child, tail, taken, out);
            }
        }
        (index, Value::Array(items)) => {
            if let Some(child) = index.parse::<usize>().ok().and_then(|i| items.get(i)) {
                expand(child, tail, taken, out);
            }
        }
        _ => {}
    }
}

/// The strings at `path` in `obj`. A `*` segment fans out over every element
/// of an array, so one pointer can name every claim a pod mounts. Empty
/// strings and non-strings are dropped; order follows the document.
pub fn pointer_values(obj: &Value, path: &str) -> Vec<String> {
    pointer_pairs(obj, path, None)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// [`pointer_values`], each paired with the string at `namespace_path` for
/// the same array elements — the `*`s there take the indices `path` took, so
/// an element without a namespace yields `None` rather than shifting the
/// namespaces of the elements after it.
pub fn pointer_pairs(
    obj: &Value,
    path: &str,
    namespace_path: Option<&str>,
) -> Vec<(String, Option<String>)> {
    pointer_hits(obj, path, namespace_path, None, None)
        .into_iter()
        .map(|hit| (hit.name, hit.namespace))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub name: String,
    pub namespace: Option<String>,
    pub kind: Option<String>,
    pub group: Option<String>,
}

pub fn pointer_hits(
    obj: &Value,
    path: &str,
    namespace_path: Option<&str>,
    kind_path: Option<&str>,
    group_path: Option<&str>,
) -> Vec<Hit> {
    let Some(segs) = segments(path) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    expand(obj, &segs, &mut Vec::new(), &mut hits);
    let ns_segs = namespace_path.and_then(segments);
    let kind_segs = kind_path.and_then(segments);
    let group_segs = group_path.and_then(segments);
    hits.into_iter()
        .map(|(taken, name)| Hit {
            name,
            namespace: ns_segs
                .as_deref()
                .and_then(|segs| value_at(obj, segs, &taken)),
            kind: kind_segs
                .as_deref()
                .and_then(|segs| value_at(obj, segs, &taken)),
            group: group_segs
                .as_deref()
                .and_then(|segs| node_at(obj, segs, &taken))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect()
}

fn rule_hits(obj: &Value, rule: &RefRule) -> Vec<Hit> {
    pointer_hits(
        obj,
        &rule.path,
        rule.namespace_path.as_deref(),
        rule.kind_path.as_deref(),
        rule.group_path.as_deref(),
    )
}

/// The string at a pointer whose `*`s are filled from `indices`, in order.
fn value_at(obj: &Value, segs: &[String], indices: &[usize]) -> Option<String> {
    match node_at(obj, segs, indices)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// The value at a pointer whose `*`s are filled from `indices`, in order.
fn node_at<'a>(obj: &'a Value, segs: &[String], indices: &[usize]) -> Option<&'a Value> {
    let mut node = obj;
    let mut idx = indices.iter();
    for seg in segs {
        node = match (seg.as_str(), node) {
            ("*", Value::Array(items)) => items.get(*idx.next()?)?,
            (key, Value::Object(map)) => map.get(key)?,
            (index, Value::Array(items)) => items.get(index.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(node)
}

// ----- the gather plan --------------------------------------------------

/// A kind resolved against the cluster, as the gather needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRef {
    pub ar: ApiResource,
    pub namespaced: bool,
    pub plural: String,
}

/// How a plan looks kinds up. The app answers from the cluster's registry;
/// tests answer from a table.
pub trait Kinds {
    /// By alias, plural, or kind — what `:` accepts.
    fn by_name(&self, name: &str) -> Option<KindRef>;
    /// By kind within an API group, as an `ownerReference` names it — so a
    /// kind shared between groups (`Cluster`) resolves to the right one.
    fn by_kind_in_group(&self, kind: &str, group: &str) -> Option<KindRef>;
}

/// Resolve a `[views."…"]`-style key (`v1/pods`, `apps/deployments`, `pods`)
/// to a kind, holding it to the group or apiVersion the key names.
pub fn resolve_view_key(kinds: &impl Kinds, key: &str) -> Option<KindRef> {
    let plural = key_plural(key);
    let Some((prefix, _)) = key.rsplit_once('/') else {
        return kinds.by_name(plural);
    };
    let prefix = prefix.to_lowercase();
    let group = prefix.split_once('/').map_or(prefix.as_str(), |(g, _)| g);
    let name = if group == "v1" {
        plural.to_string()
    } else {
        format!("{plural}.{group}")
    };
    let resolved = kinds.by_name(&name)?;
    let ar = &resolved.ar;
    (prefix == ar.api_version.to_lowercase() || prefix == ar.group.to_lowercase())
        .then_some(resolved)
}

/// A rule read forwards from the selection: the objects it names, as
/// (name, namespace) pairs to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub rule: RefRule,
    pub target: KindRef,
    pub refs: Vec<(String, String)>,
}

/// A rule read backwards: list `from` in `scope` ("" = all namespaces) and
/// keep the objects that name the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backward {
    pub rule: RefRule,
    pub from: KindRef,
    pub scope: String,
    pub default: Option<KindRef>,
}

/// Everything the gather will read for one selection, decided up front on
/// the UI thread where the kind registry lives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Owners to GET, from `ownerReferences`.
    pub owners: Vec<(KindRef, String)>,
    /// Kinds to LIST and match by owner UID.
    pub children: Vec<KindRef>,
    pub forward: Vec<Forward>,
    pub backward: Vec<Backward>,
    /// References the plan couldn't follow — an owner kind that isn't served,
    /// a rule with nowhere to read. Each is a line the view shows.
    pub warns: Vec<String>,
}

/// Build the plan for `obj`, an object of `source`. `table_ns` is the
/// namespace the table shows, used as the usage scope for a cluster-scoped
/// row that has none of its own.
pub fn plan(
    views: &HashMap<String, View>,
    kinds: &impl Kinds,
    source: &KindRef,
    obj: &DynamicObject,
    table_ns: &str,
) -> Plan {
    let ns = obj.metadata.namespace.clone().unwrap_or_default();
    let scope_ns = if ns.is_empty() { table_ns } else { ns.as_str() };
    let row_ns = (!ns.is_empty()).then_some(ns.as_str());
    let mut plan = Plan::default();

    for o in obj.metadata.owner_references.iter().flatten() {
        let group = o.api_version.split_once('/').map_or("", |(g, _)| g);
        match kinds.by_kind_in_group(&o.kind, group) {
            Some(kind) => plan.owners.push((kind, o.name.clone())),
            None => {
                plan.warns.push(format!(
                    "owner kind {} ({}) is not served here",
                    o.kind, o.api_version
                ));
            }
        }
    }
    plan.children = children_for(views, &source.ar, row_ns)
        .iter()
        .filter_map(|p| kinds.by_name(p))
        .collect();

    let value = serde_json::to_value(obj).unwrap_or(Value::Null);
    // A rule whose target kind isn't served here — a class from a CSI driver
    // that isn't installed — simply contributes nothing.
    for rule in rules_for(views, &source.ar, row_ns) {
        let targets = Targets::resolve(kinds, &rule);
        if let Some(why) = &targets.stray_default {
            plan.warns.push(why.clone());
        }
        if targets.all.is_empty() {
            continue;
        }
        if !rule.is_dynamic()
            && let Some(why) =
                unreachable_namespace(&rule, source.namespaced, targets.all[0].namespaced)
        {
            plan.warns.push(why);
            continue;
        }
        let mut forwards: Vec<Forward> = Vec::new();
        for hit in rule_hits(&value, &rule) {
            let Some(target) = targets.pick(&rule, &hit) else {
                continue;
            };
            if rule.is_dynamic()
                && let Some(why) =
                    unreachable_namespace(&rule, source.namespaced, target.namespaced)
            {
                if !plan.warns.contains(&why) {
                    plan.warns.push(why);
                }
                continue;
            }
            let target_ns = match hit.namespace {
                Some(target_ns) => target_ns,
                // The row's own namespace covers a missing one — unless the
                // row has none: then there is nowhere to read, and a GET in
                // no namespace would quietly find nothing.
                None if !target.namespaced || !ns.is_empty() => ns.clone(),
                None => {
                    plan.warns.push(format!(
                        "ref {} → {}: {} has no namespace at {}",
                        rule.from,
                        rule.target_label(),
                        hit.name,
                        rule.namespace_path.as_deref().unwrap_or_default()
                    ));
                    continue;
                }
            };
            match forwards.iter_mut().find(|f| f.target == *target) {
                Some(forward) => forward.refs.push((hit.name, target_ns)),
                None => forwards.push(Forward {
                    rule: rule.clone(),
                    target: target.clone(),
                    refs: vec![(hit.name, target_ns)],
                }),
            }
        }
        plan.forward.extend(forwards);
    }
    for rule in all_rules(views) {
        if rule.reverse == Reverse::None {
            continue;
        }
        let targets = Targets::resolve(kinds, &rule);
        if !targets.all.iter().any(|t| same_kind(t, source)) {
            continue;
        }
        let Some(from) = resolve_view_key(kinds, &rule.from) else {
            continue;
        };
        if let Some(why) = unreachable_namespace(&rule, from.namespaced, source.namespaced) {
            plan.warns.push(why);
            continue;
        }
        // A rule declared under `kind@namespace` describes that namespace's
        // objects only, so that's where its usages are looked for.
        let scope = match (key_namespace(&rule.from), rule.reverse) {
            (Some(pinned), _) if from.namespaced => pinned.to_string(),
            (None, Reverse::Namespace) if from.namespaced => scope_ns.to_string(),
            _ => String::new(),
        };
        plan.backward.push(Backward {
            rule,
            from,
            scope,
            default: targets.default,
        });
    }
    plan
}

struct Targets {
    all: Vec<KindRef>,
    default: Option<KindRef>,
    stray_default: Option<String>,
}

impl Targets {
    fn resolve(kinds: &impl Kinds, rule: &RefRule) -> Self {
        let default = (!rule.kind.is_empty())
            .then(|| kinds.by_name(&rule.kind))
            .flatten();
        if !rule.is_dynamic() {
            return Self {
                all: default.clone().into_iter().collect(),
                default,
                stray_default: None,
            };
        }
        let mut all: Vec<KindRef> = Vec::new();
        for candidate in rule.kinds.iter().filter_map(|k| kinds.by_name(k)) {
            if !all.iter().any(|t| same_kind(t, &candidate)) {
                all.push(candidate);
            }
        }
        let (default, stray_default) = match default {
            Some(d) if all.iter().any(|t| same_kind(t, &d)) => (Some(d), None),
            Some(_) => (
                None,
                Some(format!(
                    "ref {} → {}: kind {} is not one of kinds",
                    rule.from,
                    rule.target_label(),
                    rule.kind
                )),
            ),
            None => (None, None),
        };
        Self {
            all,
            default,
            stray_default,
        }
    }

    fn pick(&self, rule: &RefRule, hit: &Hit) -> Option<&KindRef> {
        if !rule.is_dynamic() {
            return self.all.first();
        }
        match hit.kind.as_deref() {
            Some(named) => self.all.iter().find(|t| element_names(rule, hit, t, named)),
            None => self
                .default
                .as_ref()
                .and_then(|d| self.all.iter().find(|t| default_names(rule, hit, d, t))),
        }
    }
}

fn same_kind(a: &KindRef, b: &KindRef) -> bool {
    a.ar.plural == b.ar.plural && a.ar.group == b.ar.group
}

fn kind_is_named(kind: &KindRef, name: &str) -> bool {
    kind.ar.kind.eq_ignore_ascii_case(name)
        || kind.plural.eq_ignore_ascii_case(name)
        || (!kind.ar.group.is_empty()
            && format!("{}.{}", kind.plural, kind.ar.group).eq_ignore_ascii_case(name))
}

/// Whether an element without a kind names `kind` through the rule's
/// `default`: the default's kind, in the group the element gives or the
/// rule's default group, else the default itself.
fn default_names(rule: &RefRule, hit: &Hit, default: &KindRef, kind: &KindRef) -> bool {
    match hit.group.as_deref().or(rule.group.as_deref()) {
        Some(group) => {
            kind.ar.kind.eq_ignore_ascii_case(&default.ar.kind)
                && kind.ar.group.eq_ignore_ascii_case(group)
        }
        None => same_kind(default, kind),
    }
}

/// Whether an element naming kind `named` names `kind`: the group it gives,
/// or the rule's default, must be `kind`'s too.
fn element_names(rule: &RefRule, hit: &Hit, kind: &KindRef, named: &str) -> bool {
    kind_is_named(kind, named)
        && hit
            .group
            .as_deref()
            .or(rule.group.as_deref())
            .is_none_or(|group| kind.ar.group.eq_ignore_ascii_case(group))
}

/// Why a rule can't be followed: a cluster-scoped object naming a namespaced
/// one says nothing about which namespace, unless `namespace_path` does.
/// Better one warning than a GET in no namespace that quietly finds nothing.
fn unreachable_namespace(
    rule: &RefRule,
    from_namespaced: bool,
    to_namespaced: bool,
) -> Option<String> {
    (!from_namespaced && to_namespaced && rule.namespace_path.is_none()).then(|| {
        format!(
            "ref {} → {}: a namespaced kind named from a cluster-scoped one needs namespace_path",
            rule.from,
            rule.target_label()
        )
    })
}

// ----- matching the gathered objects ------------------------------------

/// Whether `o` lists `uid` among its owners.
pub fn owned_by(o: &DynamicObject, uid: Option<&str>) -> bool {
    let Some(uid) = uid else {
        return false;
    };
    o.metadata
        .owner_references
        .iter()
        .flatten()
        .any(|own| own.uid == uid)
}

/// Whether `o`, an object of `rule`'s source kind, names the selection. A
/// namespaced selection is named within a namespace: the one the rule's
/// `namespace_path` gives for that element, else `o`'s own.
pub fn names_source(
    o: &DynamicObject,
    rule: &RefRule,
    default: Option<&KindRef>,
    source: &KindRef,
    source_name: &str,
    source_ns: Option<&str>,
) -> bool {
    let value = serde_json::to_value(o).unwrap_or(Value::Null);
    rule_hits(&value, rule).into_iter().any(|hit| {
        if hit.name != source_name || !hit_names_kind(rule, &hit, default, source) {
            return false;
        }
        let Some(source_ns) = source_ns else {
            return true;
        };
        let named_ns = match &rule.namespace_path {
            Some(_) => hit.namespace,
            None => o.metadata.namespace.clone(),
        };
        named_ns.as_deref() == Some(source_ns)
    })
}

fn hit_names_kind(rule: &RefRule, hit: &Hit, default: Option<&KindRef>, source: &KindRef) -> bool {
    if !rule.is_dynamic() {
        return true;
    }
    match hit.kind.as_deref() {
        Some(named) => element_names(rule, hit, source, named),
        None => default.is_some_and(|d| default_names(rule, hit, d, source)),
    }
}

/// Drop repeats — the same object reached the same way twice, as a ConfigMap
/// mounted as a volume and again through a projected one — keeping the first.
pub fn dedup(items: &mut Vec<AdjacentItem>) {
    let mut seen = HashSet::new();
    items.retain(|it| {
        seen.insert((
            it.direction,
            it.relation.clone(),
            it.plural.clone(),
            it.namespace.clone(),
            it.name.clone(),
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ar(group: &str, kind: &str, plural: &str) -> ApiResource {
        ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: if group.is_empty() {
                "v1".into()
            } else {
                format!("{group}/v1")
            },
            kind: kind.into(),
            plural: plural.into(),
        }
    }

    fn kind(group: &str, kind: &str, plural: &str, namespaced: bool) -> KindRef {
        KindRef {
            ar: ar(group, kind, plural),
            namespaced,
            plural: plural.into(),
        }
    }

    /// A registry answering from a table, the way `Cluster::fake` would.
    struct Table(Vec<KindRef>);

    impl Kinds for Table {
        fn by_name(&self, name: &str) -> Option<KindRef> {
            self.0
                .iter()
                .find(|k| {
                    k.plural == name
                        || k.ar.kind.eq_ignore_ascii_case(name)
                        || (!k.ar.group.is_empty()
                            && format!("{}.{}", k.plural, k.ar.group) == name)
                })
                .cloned()
        }
        fn by_kind_in_group(&self, kind: &str, group: &str) -> Option<KindRef> {
            self.0
                .iter()
                .find(|k| {
                    k.ar.kind.eq_ignore_ascii_case(kind) && k.ar.group.eq_ignore_ascii_case(group)
                })
                .cloned()
        }
    }

    struct Aliased(Table, &'static str, &'static str);

    impl Kinds for Aliased {
        fn by_name(&self, name: &str) -> Option<KindRef> {
            let name = if name == self.1 { self.2 } else { name };
            self.0.by_name(name)
        }
        fn by_kind_in_group(&self, kind: &str, group: &str) -> Option<KindRef> {
            self.0.by_kind_in_group(kind, group)
        }
    }

    fn cluster() -> Table {
        Table(vec![
            kind("", "Pod", "pods", true),
            kind("", "Node", "nodes", false),
            kind("", "PersistentVolumeClaim", "persistentvolumeclaims", true),
            kind("", "PersistentVolume", "persistentvolumes", false),
            kind("", "ConfigMap", "configmaps", true),
            kind("", "Secret", "secrets", true),
            kind("apps", "StatefulSet", "statefulsets", true),
            kind("apps", "ReplicaSet", "replicasets", true),
            kind("postgresql.cnpg.io", "Cluster", "clusters", true),
            kind("cluster.x-k8s.io", "Cluster", "clusters", true),
            kind(
                "storage.k8s.io",
                "VolumeAttributesClass",
                "volumeattributesclasses",
                false,
            ),
        ])
    }

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn pointer_values_fan_out_over_arrays() {
        let pod = json!({"spec": {
            "nodeName": "ip-10-0-1-2",
            "volumes": [
                {"name": "data", "persistentVolumeClaim": {"claimName": "data-db-0"}},
                {"name": "cfg", "configMap": {"name": "db-config"}},
                {"name": "more", "persistentVolumeClaim": {"claimName": "wal-db-0"}},
            ],
            "containers": [{"envFrom": [{"secretRef": {"name": "db-creds"}}, {"configMapRef": {"name": "db-env"}}]}],
        }});
        assert_eq!(pointer_values(&pod, "/spec/nodeName"), ["ip-10-0-1-2"]);
        assert_eq!(
            pointer_values(&pod, "/spec/volumes/*/persistentVolumeClaim/claimName"),
            ["data-db-0", "wal-db-0"]
        );
        assert_eq!(
            pointer_values(&pod, "/spec/containers/*/envFrom/*/secretRef/name"),
            ["db-creds"]
        );
        assert_eq!(
            pointer_values(&pod, "/spec/volumes/1/configMap/name"),
            ["db-config"]
        );
        // Missing, non-string, and empty values contribute nothing.
        assert!(pointer_values(&pod, "/spec/serviceAccountName").is_empty());
        assert!(pointer_values(&pod, "/spec/volumes").is_empty());
        assert!(pointer_values(&json!({"a": ""}), "/a").is_empty());
        assert!(pointer_values(&pod, "spec/nodeName").is_empty());
        // Escaped keys, as in `path` for columns.
        let labelled = json!({"metadata": {"labels": {"karpenter.sh/nodepool": "default"}}});
        assert_eq!(
            pointer_values(&labelled, "/metadata/labels/karpenter.sh~1nodepool"),
            ["default"]
        );
    }

    #[test]
    fn namespace_pairs_follow_the_same_array_element() {
        let pv = json!({"spec": {"claimRef": {"name": "data", "namespace": "db"}}});
        assert_eq!(
            pointer_pairs(&pv, "/spec/claimRef/name", Some("/spec/claimRef/namespace")),
            [("data".to_string(), Some("db".to_string()))]
        );
        // The second element has no namespace: it gets None, and the third
        // keeps its own — nothing shifts.
        let multi = json!({"spec": {"refs": [
            {"name": "a", "namespace": "one"},
            {"name": "b"},
            {"name": "c", "namespace": "three"},
        ]}});
        assert_eq!(
            pointer_pairs(&multi, "/spec/refs/*/name", Some("/spec/refs/*/namespace")),
            [
                ("a".to_string(), Some("one".to_string())),
                ("b".to_string(), None),
                ("c".to_string(), Some("three".to_string())),
            ]
        );
        // A namespace path with fewer `*`s than the name path reads a fixed
        // field; with more, it can't be paired and yields None.
        assert_eq!(
            pointer_pairs(&multi, "/spec/refs/*/name", Some("/spec/refs/0/namespace")),
            [
                ("a".to_string(), Some("one".to_string())),
                ("b".to_string(), Some("one".to_string())),
                ("c".to_string(), Some("one".to_string())),
            ]
        );
        assert_eq!(
            pointer_pairs(&pv, "/spec/claimRef/name", Some("/spec/other/*/namespace")),
            [("data".to_string(), None)]
        );
    }

    #[test]
    fn builtin_rules_are_keyed_like_views() {
        let views = HashMap::new();
        let pods = rules_for(&views, &ar("", "Pod", "pods"), None);
        assert!(
            pods.iter()
                .any(|r| r.kind == "nodes" && r.relation == "runs on")
        );
        assert!(pods.iter().any(|r| r.kind == "persistentvolumeclaims"));
        // A same-named plural in another group gets nothing: PodMetrics
        // shares `pods` but has no spec to point into.
        assert!(rules_for(&views, &ar("metrics.k8s.io", "PodMetrics", "pods"), None).is_empty());
        let pv = rules_for(
            &views,
            &ar("", "PersistentVolume", "persistentvolumes"),
            None,
        );
        let claim = pv
            .iter()
            .find(|r| r.kind == "persistentvolumeclaims")
            .unwrap();
        assert_eq!(
            claim.namespace_path.as_deref(),
            Some("/spec/claimRef/namespace")
        );
        assert_eq!(
            children_for(&views, &ar("apps", "Deployment", "deployments"), None),
            ["replicasets"]
        );
        assert!(children_for(&views, &ar("", "Secret", "secrets"), None).is_empty());
    }

    #[test]
    fn configured_refs_and_children_add_to_the_builtin_rows() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [views."karpenter.sh/v1/nodeclaims"]
                children = ["nodes", "ec2nodeclasses"]

                [[views."karpenter.sh/v1/nodeclaims".refs]]
                path = "/spec/nodeClassRef/name"
                kind = "ec2nodeclasses"
                relation = "shaped by"
                reverse = "cluster"

                [[views.pods.refs]]
                path = "/metadata/annotations/example.com~1owner"
                kind = "teams"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let claims = ar("karpenter.sh", "NodeClaim", "nodeclaims");
        let rules = rules_for(&views, &claims, None);
        // The built-in node row and the configured class row both apply.
        assert!(rules.iter().any(|r| r.kind == "nodes"));
        let class = rules.iter().find(|r| r.kind == "ec2nodeclasses").unwrap();
        assert_eq!(
            (class.relation.as_str(), class.reverse),
            ("shaped by", Reverse::Cluster)
        );
        // Built-in `nodes` and the configured copy of it collapse to one.
        assert_eq!(
            children_for(&views, &claims, None),
            ["nodes", "ec2nodeclasses"]
        );

        let pods = rules_for(&views, &ar("", "Pod", "pods"), None);
        let team = pods.iter().find(|r| r.kind == "teams").unwrap();
        assert_eq!(
            (team.relation.as_str(), team.reverse),
            ("references", Reverse::Namespace)
        );
        assert!(all_rules(&views).iter().any(|r| r.kind == "teams"));
    }

    #[test]
    fn namespaced_view_keys_apply_to_that_namespace_only() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views."pods@prod".refs]]
                path = "/metadata/annotations/example.com~1owner"
                kind = "teams"
                reverse = "cluster"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let pods = ar("", "Pod", "pods");
        assert!(
            rules_for(&views, &pods, Some("prod"))
                .iter()
                .any(|r| r.kind == "teams")
        );
        assert!(
            rules_for(&views, &pods, Some("dev"))
                .iter()
                .all(|r| r.kind != "teams")
        );
        assert!(
            rules_for(&views, &pods, None)
                .iter()
                .all(|r| r.kind != "teams")
        );

        // Read backwards from a team, the rule's pods are listed in prod
        // whatever `reverse` says: that's the only namespace it describes.
        let mut kinds = cluster();
        kinds.0.push(kind("example.com", "Team", "teams", false));
        let team = obj(
            json!({"apiVersion": "example.com/v1", "kind": "Team", "metadata": {"name": "core"}}),
        );
        let plan = self::plan(
            &views,
            &kinds,
            &kind("example.com", "Team", "teams", false),
            &team,
            "dev",
        );
        let pods_rule = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "pods")
            .unwrap();
        assert_eq!(pods_rule.scope, "prod");
    }

    #[test]
    fn a_cluster_scoped_source_needs_a_namespace_path_for_namespaced_targets() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                # Nothing says which namespace the ConfigMap is in.
                [[views.nodes.refs]]
                path = "/metadata/annotations/example.com~1config"
                kind = "configmaps"

                # This one does.
                [[views.nodes.refs]]
                path = "/metadata/annotations/example.com~1secret"
                kind = "secrets"
                namespace_path = "/metadata/annotations/example.com~1secret-namespace"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(
            warnings.is_empty(),
            "compile can't know scopes: {warnings:?}"
        );
        let kinds = cluster();
        let node = obj(json!({"apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "n1", "annotations": {
                "example.com/config": "tuning", "example.com/secret": "tls", "example.com/secret-namespace": "edge"}}}));
        let plan = self::plan(&views, &kinds, &kind("", "Node", "nodes", false), &node, "");
        // The rule with a namespace path is followed into that namespace…
        let secret = plan
            .forward
            .iter()
            .find(|f| f.target.plural == "secrets")
            .unwrap();
        assert_eq!(secret.refs, [("tls".to_string(), "edge".to_string())]);
        // A row whose namespace annotation is missing can't be followed
        // either: the ref is dropped and named in the warning.
        let bare = obj(json!({"apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "n2", "annotations": {"example.com/secret": "tls"}}}));
        let plan_bare = self::plan(&views, &kinds, &kind("", "Node", "nodes", false), &bare, "");
        assert!(
            plan_bare
                .forward
                .iter()
                .all(|f| f.target.plural != "secrets")
        );
        assert!(
            plan_bare.warns.join("; ").contains(
                "tls has no namespace at /metadata/annotations/example.com~1secret-namespace"
            ),
            "{:?}",
            plan_bare.warns
        );
        // …the one without is skipped, and the view says why instead of
        // quietly reading in no namespace and finding nothing.
        assert!(plan.forward.iter().all(|f| f.target.plural != "configmaps"));
        assert!(
            plan.warns.join("; ").contains("nodes → configmaps")
                && plan.warns.join("; ").contains("namespace_path"),
            "{:?}",
            plan.warns
        );
        // Read backwards from a ConfigMap, the same rule is skipped for the
        // same reason; the pods-mount-configmaps rule still runs.
        let cm = obj(
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "tuning", "namespace": "kube-system"}}),
        );
        let plan = self::plan(
            &views,
            &kinds,
            &kind("", "ConfigMap", "configmaps", true),
            &cm,
            "kube-system",
        );
        assert!(plan.backward.iter().all(|b| b.from.plural != "nodes"));
        assert!(plan.backward.iter().any(|b| b.from.plural == "pods"));
        assert!(plan.warns.join("; ").contains("namespace_path"));
    }

    #[test]
    fn view_keys_resolve_to_their_group() {
        let kinds = cluster();
        assert!(resolve_view_key(&kinds, "pods").is_some());
        assert!(resolve_view_key(&kinds, "v1/pods").is_some());
        assert!(resolve_view_key(&kinds, "apps/replicasets").is_some());
        assert!(resolve_view_key(&kinds, "apps/v1/replicasets").is_some());
        // A key naming another group must not resolve to the core kind.
        assert!(resolve_view_key(&kinds, "metrics.k8s.io/pods").is_none());
        assert!(resolve_view_key(&kinds, "nope").is_none());
        // A namespace suffix is not part of the plural.
        assert!(resolve_view_key(&kinds, "v1/pods@prod").is_some());
        for key in [
            "cluster.x-k8s.io/clusters",
            "cluster.x-k8s.io/v1/clusters",
            "cluster.x-k8s.io/v1/clusters@prod",
        ] {
            assert_eq!(
                resolve_view_key(&kinds, key).unwrap().ar.group,
                "cluster.x-k8s.io"
            );
        }
        assert!(resolve_view_key(&kinds, "cluster.x-k8s.io/v2/clusters").is_none());
    }

    #[test]
    fn plan_for_a_pod_reads_owner_children_and_references() {
        let views = HashMap::new();
        let kinds = cluster();
        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "db-0", "namespace": "db", "uid": "p-1",
                         "ownerReferences": [
                             // The owner's group picks the right `Cluster` of two.
                             {"apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster", "name": "db", "uid": "c-1"},
                             {"apiVersion": "unknown.io/v1", "kind": "Widget", "name": "w", "uid": "w-1"}]},
            "spec": {"nodeName": "ip-10-0-1-2",
                     "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "data-db-0"}},
                                 {"name": "cfg", "configMap": {"name": "db-config"}},
                                 {"name": "proj", "projected": {"sources": [{"configMap": {"name": "db-config"}}]}}]}
        }));
        let plan = self::plan(&views, &kinds, &kind("", "Pod", "pods", true), &pod, "db");

        assert_eq!(plan.owners.len(), 1);
        assert_eq!(plan.owners[0].0.ar.group, "postgresql.cnpg.io");
        assert_eq!(plan.owners[0].1, "db");
        assert!(
            plan.warns.join("; ").contains("Widget (unknown.io/v1)"),
            "{:?}",
            plan.warns
        );
        // Pods own nothing.
        assert!(plan.children.is_empty());

        let named: Vec<(&str, &str, &str)> = plan
            .forward
            .iter()
            .flat_map(|f| {
                f.refs
                    .iter()
                    .map(move |(n, ns)| (f.rule.relation.as_str(), n.as_str(), ns.as_str()))
            })
            .collect();
        assert!(named.contains(&("runs on", "ip-10-0-1-2", "db")));
        assert!(named.contains(&("mounts", "data-db-0", "db")));
        // The volume and the projected volume are two rules naming the same
        // ConfigMap; the gather dedups the rows they produce.
        assert_eq!(
            named.iter().filter(|(_, n, _)| *n == "db-config").count(),
            2
        );
        // Kinds not served (serviceaccounts) and empty pointers add nothing.
        assert!(named.iter().all(|(rel, _, _)| *rel != "runs as"));

        // Backwards: nothing built in names a pod.
        assert!(plan.backward.is_empty());
    }

    #[test]
    fn plan_for_a_claim_scopes_usages_by_the_rule() {
        let views = HashMap::new();
        let kinds = cluster();
        let pvc = obj(json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": {"name": "data-db-0", "namespace": "db", "uid": "v-1"},
            "spec": {"volumeName": "pvc-1", "volumeAttributesClassName": "gold"}
        }));
        let source = kind("", "PersistentVolumeClaim", "persistentvolumeclaims", true);
        let plan = self::plan(&views, &kinds, &source, &pvc, "db");
        // Pods mount claims: listed in the claim's namespace.
        let pods = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "pods")
            .unwrap();
        assert_eq!(
            (pods.rule.relation.as_str(), pods.scope.as_str()),
            ("mounts", "db")
        );
        // PV → PVC is `reverse = none`: not scanned.
        assert!(
            plan.backward
                .iter()
                .all(|b| b.from.plural != "persistentvolumes")
        );
        // The class is cluster-scoped: no namespace on the GET.
        let vac = plan
            .forward
            .iter()
            .find(|f| f.target.plural == "volumeattributesclasses")
            .unwrap();
        assert!(!vac.target.namespaced);

        // A cluster-scoped row takes the table's namespace for usages.
        let vac_obj = obj(
            json!({"apiVersion": "storage.k8s.io/v1", "kind": "VolumeAttributesClass",
                                 "metadata": {"name": "gold"}}),
        );
        let source = kind(
            "storage.k8s.io",
            "VolumeAttributesClass",
            "volumeattributesclasses",
            false,
        );
        let plan = self::plan(&views, &kinds, &source, &vac_obj, "shop");
        let claims = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "persistentvolumeclaims")
            .unwrap();
        assert_eq!(claims.scope, "shop");
    }

    #[test]
    fn matching_gathered_objects() {
        let pod = obj(json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "db-0", "namespace": "db",
                         "ownerReferences": [{"apiVersion": "apps/v1", "kind": "StatefulSet", "name": "db", "uid": "s-1"}]},
            "spec": {"volumes": [{"persistentVolumeClaim": {"claimName": "data-db-0"}}]}}));
        assert!(owned_by(&pod, Some("s-1")));
        assert!(!owned_by(&pod, Some("s-2")));
        assert!(!owned_by(&pod, None));

        let mounts = rules_for(&HashMap::new(), &ar("", "Pod", "pods"), None)
            .into_iter()
            .find(|r| r.path.contains("persistentVolumeClaim"))
            .unwrap();
        let pvc = kind("", "PersistentVolumeClaim", "persistentvolumeclaims", true);
        // Same name in the same namespace: a match; same name elsewhere: not.
        assert!(names_source(
            &pod,
            &mounts,
            None,
            &pvc,
            "data-db-0",
            Some("db")
        ));
        assert!(!names_source(
            &pod,
            &mounts,
            None,
            &pvc,
            "data-db-0",
            Some("other")
        ));
        assert!(!names_source(
            &pod,
            &mounts,
            None,
            &pvc,
            "wal-db-0",
            Some("db")
        ));
        // A cluster-scoped selection has no namespace to match.
        assert!(names_source(&pod, &mounts, None, &pvc, "data-db-0", None));

        // With a namespace path, the pointed-at namespace decides, not the
        // referencing object's own.
        let pv = obj(json!({"apiVersion": "v1", "kind": "PersistentVolume",
            "metadata": {"name": "pvc-1"},
            "spec": {"claimRef": {"name": "data-db-0", "namespace": "db"}}}));
        let bound = rules_for(
            &HashMap::new(),
            &ar("", "PersistentVolume", "persistentvolumes"),
            None,
        )
        .into_iter()
        .find(|r| r.namespace_path.is_some())
        .unwrap();
        assert!(names_source(
            &pv,
            &bound,
            None,
            &pvc,
            "data-db-0",
            Some("db")
        ));
        assert!(!names_source(
            &pv,
            &bound,
            None,
            &pvc,
            "data-db-0",
            Some("other")
        ));
    }

    #[test]
    fn a_dynamic_kind_is_read_from_the_object() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kind = "secretstores"
                kind_path = "/spec/secretStoreRef/kind"
                kinds = ["secretstores", "clustersecretstores"]
                relation = "reads from"

                [[views.clusterrolebindings.refs]]
                path = "/subjects/*/name"
                namespace_path = "/subjects/*/namespace"
                kind_path = "/subjects/*/kind"
                kinds = ["serviceaccounts"]
                relation = "binds"
                reverse = "cluster"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut kinds = cluster();
        kinds.0.extend([
            kind("", "ServiceAccount", "serviceaccounts", true),
            kind(
                "rbac.authorization.k8s.io",
                "ClusterRoleBinding",
                "clusterrolebindings",
                false,
            ),
            kind(
                "external-secrets.io",
                "ExternalSecret",
                "externalsecrets",
                true,
            ),
            kind("external-secrets.io", "SecretStore", "secretstores", true),
            kind(
                "external-secrets.io",
                "ClusterSecretStore",
                "clustersecretstores",
                false,
            ),
        ]);
        let es = kind(
            "external-secrets.io",
            "ExternalSecret",
            "externalsecrets",
            true,
        );
        let forward_to = |value: Value| {
            let plan = self::plan(&views, &kinds, &es, &obj(value), "shop");
            assert!(plan.warns.is_empty(), "{:?}", plan.warns);
            plan.forward
                .into_iter()
                .map(|f| (f.target.plural, f.refs))
                .collect::<Vec<_>>()
        };
        let es_obj = |store_ref: Value| {
            json!({"apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
                "metadata": {"name": "db", "namespace": "shop"},
                "spec": {"secretStoreRef": store_ref}})
        };
        assert_eq!(
            forward_to(es_obj(
                json!({"name": "vault", "kind": "ClusterSecretStore"})
            )),
            [(
                "clustersecretstores".to_string(),
                vec![("vault".to_string(), "shop".to_string())]
            )]
        );
        assert_eq!(
            forward_to(es_obj(json!({"name": "local", "kind": "SecretStore"}))),
            [(
                "secretstores".to_string(),
                vec![("local".to_string(), "shop".to_string())]
            )]
        );
        assert_eq!(
            forward_to(es_obj(json!({"name": "local"}))),
            [(
                "secretstores".to_string(),
                vec![("local".to_string(), "shop".to_string())]
            )]
        );
        assert!(forward_to(es_obj(json!({"name": "x", "kind": "Widget"}))).is_empty());

        let crb = obj(
            json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
            "metadata": {"name": "admins"},
            "subjects": [
                {"kind": "User", "name": "alice"},
                {"kind": "ServiceAccount", "name": "deployer", "namespace": "ci"},
                {"kind": "Group", "name": "ops"},
                {"kind": "ServiceAccount", "name": "reader", "namespace": "audit"},
            ]}),
        );
        let crb_kind = kind(
            "rbac.authorization.k8s.io",
            "ClusterRoleBinding",
            "clusterrolebindings",
            false,
        );
        let plan = self::plan(&views, &kinds, &crb_kind, &crb, "");
        assert!(plan.warns.is_empty(), "{:?}", plan.warns);
        assert_eq!(plan.forward.len(), 1);
        assert_eq!(plan.forward[0].target.plural, "serviceaccounts");
        assert_eq!(
            plan.forward[0].refs,
            [
                ("deployer".to_string(), "ci".to_string()),
                ("reader".to_string(), "audit".to_string()),
            ]
        );

        let store = kind("external-secrets.io", "SecretStore", "secretstores", true);
        let cluster_store = kind(
            "external-secrets.io",
            "ClusterSecretStore",
            "clustersecretstores",
            false,
        );
        let store_obj = obj(
            json!({"apiVersion": "external-secrets.io/v1", "kind": "SecretStore",
            "metadata": {"name": "local", "namespace": "shop"}}),
        );
        let plan = self::plan(&views, &kinds, &store, &store_obj, "shop");
        let back = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "externalsecrets")
            .expect("externalsecrets are listed for a store");
        assert_eq!(back.scope, "shop");
        let rule = &back.rule;
        let names_local = obj(es_obj(json!({"name": "local", "kind": "SecretStore"})));
        let names_default = obj(es_obj(json!({"name": "local"})));
        let names_cluster = obj(es_obj(
            json!({"name": "local", "kind": "ClusterSecretStore"}),
        ));
        assert!(names_source(
            &names_local,
            rule,
            back.default.as_ref(),
            &store,
            "local",
            Some("shop")
        ));
        assert!(names_source(
            &names_default,
            rule,
            back.default.as_ref(),
            &store,
            "local",
            Some("shop")
        ));
        assert!(!names_source(
            &names_cluster,
            rule,
            back.default.as_ref(),
            &store,
            "local",
            Some("shop")
        ));
        assert!(names_source(
            &names_cluster,
            rule,
            back.default.as_ref(),
            &cluster_store,
            "local",
            None
        ));
        assert!(!names_source(
            &names_default,
            rule,
            back.default.as_ref(),
            &cluster_store,
            "local",
            None
        ));
        assert!(!names_source(
            &names_local,
            rule,
            back.default.as_ref(),
            &cluster_store,
            "local",
            None
        ));
        let secret = obj(json!({"apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": "local", "namespace": "shop"}}));
        let plan = self::plan(
            &views,
            &kinds,
            &kind("", "Secret", "secrets", true),
            &secret,
            "shop",
        );
        assert!(
            plan.backward
                .iter()
                .all(|b| b.from.plural != "externalsecrets")
        );
    }

    #[test]
    fn a_dynamic_kind_needs_its_candidates() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kind_path = "/spec/secretStoreRef/kind"

                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kinds = ["secretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kind_path = "spec/secretStoreRef/kind"
                kinds = ["secretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/data/*/sourceRef/name"
                kind_path = "/spec/other/*/kind"
                kinds = ["secretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/data/*/sourceRef/name"
                kind_path = "/spec/data/*/sourceRef/*/kind"
                kinds = ["secretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/data/*/sourceRef/name"
                namespace_path = "/spec/other/*/namespace"
                kind_path = "/spec/data/*/sourceRef/kind"
                kinds = ["secretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/data/*/sourceRef/name"
                namespace_path = "/spec/data/0/sourceRef/namespace"
                kind_path = "/spec/data/*/sourceRef/kind"
                kinds = ["secretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kind_path = "/spec/secretStoreRef/kind"
                kinds = ["SecretStores", " "]
                "#,
            )
            .unwrap()
            .views,
        );
        assert_eq!(warnings.len(), 6, "{warnings:?}");
        assert!(warnings[0].contains("ref 1") && warnings[0].contains("kinds"));
        assert!(warnings[1].contains("ref 2") && warnings[1].contains("kind_path"));
        assert!(warnings[2].contains("ref 3") && warnings[2].contains("JSON Pointer"));
        for (i, what) in [(3, "ref 4"), (4, "ref 5"), (5, "ref 6")] {
            assert!(
                warnings[i].contains(what) && warnings[i].contains("same arrays"),
                "{}",
                warnings[i]
            );
        }
        assert!(warnings[3].contains("kind_path") && warnings[4].contains("kind_path"));
        assert!(warnings[5].contains("namespace_path"));
        let rules = &views["externalsecrets"].refs;
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0].namespace_path.as_deref(),
            Some("/spec/data/0/sourceRef/namespace")
        );
        let rules = &rules[1..];
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].kinds, ["secretstores"]);
        assert_eq!(rules[0].kind, "");
        assert!(rules[0].is_dynamic());
    }

    #[test]
    fn a_group_path_tells_kinds_of_the_same_name_apart() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group_path = "/spec/parentRefs/*/group"
                group = "gateway.networking.k8s.io"
                kind = "gateways.gateway.networking.k8s.io"
                kinds = ["gateways.networking.istio.io", "gateways.gateway.networking.k8s.io", "services"]
                relation = "attaches to"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let istio = kind("networking.istio.io", "Gateway", "gateways", true);
        let gateway = kind("gateway.networking.k8s.io", "Gateway", "gateways", true);
        let service = kind("", "Service", "services", true);
        let route_kind = kind("gateway.networking.k8s.io", "HTTPRoute", "httproutes", true);
        let mut kinds = cluster();
        kinds.0.extend([
            istio.clone(),
            gateway.clone(),
            service.clone(),
            route_kind.clone(),
        ]);
        let route = obj(
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
            "metadata": {"name": "web", "namespace": "shop"},
            "spec": {"parentRefs": [
                {"name": "public", "kind": "Gateway", "group": "gateway.networking.k8s.io"},
                {"name": "mesh", "kind": "Gateway", "group": "networking.istio.io"},
                {"name": "defaulted", "kind": "Gateway"},
                {"name": "api", "kind": "Service", "group": ""},
                {"name": "stray", "kind": "Gateway", "group": "example.com"},
                {"name": "kindless-mesh", "group": "networking.istio.io"},
                {"name": "kindless"},
            ]}}),
        );
        let plan = self::plan(&views, &kinds, &route_kind, &route, "shop");
        assert!(plan.warns.is_empty(), "{:?}", plan.warns);
        let forward: Vec<_> = plan
            .forward
            .iter()
            .map(|f| {
                (
                    f.target.ar.group.as_str(),
                    f.target.plural.as_str(),
                    f.refs.clone(),
                )
            })
            .collect();
        let refs = |names: &[&str]| -> Vec<(String, String)> {
            names
                .iter()
                .map(|n| (n.to_string(), "shop".to_string()))
                .collect()
        };
        assert_eq!(
            forward,
            [
                (
                    "gateway.networking.k8s.io",
                    "gateways",
                    refs(&["public", "defaulted", "kindless"])
                ),
                (
                    "networking.istio.io",
                    "gateways",
                    refs(&["mesh", "kindless-mesh"])
                ),
                ("", "services", refs(&["api"])),
            ]
        );

        let istio_obj = obj(
            json!({"apiVersion": "networking.istio.io/v1", "kind": "Gateway",
            "metadata": {"name": "public", "namespace": "shop"}}),
        );
        let plan = self::plan(&views, &kinds, &istio, &istio_obj, "shop");
        let back = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "httproutes")
            .expect("routes are listed for an Istio gateway");
        let names = |source: &KindRef, name: &str| {
            names_source(
                &route,
                &back.rule,
                back.default.as_ref(),
                source,
                name,
                Some("shop"),
            )
        };
        assert!(names(&gateway, "public"));
        assert!(!names(&istio, "public"));
        assert!(names(&istio, "mesh"));
        assert!(!names(&gateway, "mesh"));
        assert!(names(&gateway, "defaulted"));
        assert!(!names(&istio, "defaulted"));
        assert!(names(&service, "api"));
        assert!(!names(&gateway, "stray") && !names(&istio, "stray"));
        assert!(names(&istio, "kindless-mesh"));
        assert!(!names(&gateway, "kindless-mesh"));
        assert!(names(&gateway, "kindless"));
        assert!(!names(&istio, "kindless"));
    }

    #[test]
    fn without_a_group_default_an_element_without_a_group_matches_by_kind() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group_path = "/spec/parentRefs/*/group"
                kinds = ["gateways.networking.istio.io", "gateways.gateway.networking.k8s.io"]
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let route_kind = kind("gateway.networking.k8s.io", "HTTPRoute", "httproutes", true);
        let mut kinds = cluster();
        kinds.0.extend([
            kind("networking.istio.io", "Gateway", "gateways", true),
            kind("gateway.networking.k8s.io", "Gateway", "gateways", true),
            route_kind.clone(),
        ]);
        let route = obj(
            json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
            "metadata": {"name": "web", "namespace": "shop"},
            "spec": {"parentRefs": [{"name": "public", "kind": "Gateway"}]}}),
        );
        let plan = self::plan(&views, &kinds, &route_kind, &route, "shop");
        assert_eq!(plan.forward.len(), 1);
        assert_eq!(plan.forward[0].target.ar.group, "networking.istio.io");
    }

    #[test]
    fn a_group_path_follows_the_kind_path_rules() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind = "gateways"
                group_path = "/spec/parentRefs/*/group"

                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group_path = "spec/parentRefs/*/group"
                kinds = ["gateways"]

                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group_path = "/spec/other/*/group"
                kinds = ["gateways"]

                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group = "gateway.networking.k8s.io"
                kinds = ["gateways"]

                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group_path = "/spec/parentRefs/*/group"
                group = ""
                kinds = ["services"]

                [[views.httproutes.refs]]
                path = "/spec/parentRefs/*/name"
                kind_path = "/spec/parentRefs/*/kind"
                group_path = " "
                kinds = ["gateways"]
                "#,
            )
            .unwrap()
            .views,
        );
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(warnings[0].contains("ref 1") && warnings[0].contains("needs kind_path"));
        assert!(warnings[1].contains("ref 2") && warnings[1].contains("JSON Pointer"));
        assert!(warnings[2].contains("ref 3") && warnings[2].contains("same arrays"));
        assert!(warnings[3].contains("ref 4") && warnings[3].contains("needs group_path"));
        assert!(warnings[4].contains("ref 6") && warnings[4].contains("group_path is empty"));
        let rules = &views["httproutes"].refs;
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].group_path.as_deref(),
            Some("/spec/parentRefs/*/group")
        );
        assert_eq!(rules[0].group.as_deref(), Some(""));
    }

    #[test]
    fn a_default_kind_is_matched_to_the_candidates_by_the_cluster() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kind = "SecretStore"
                kind_path = "/spec/secretStoreRef/kind"
                kinds = ["secretstores", "clustersecretstores"]

                [[views.externalsecrets.refs]]
                path = "/spec/data/*/sourceRef/name"
                kind = "secretstores"
                kind_path = "/spec/data/*/sourceRef/kind"
                kinds = ["clustersecretstores"]
                relation = "pulls"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut kinds = cluster();
        kinds.0.extend([
            kind(
                "external-secrets.io",
                "ExternalSecret",
                "externalsecrets",
                true,
            ),
            kind("external-secrets.io", "SecretStore", "secretstores", true),
            kind(
                "external-secrets.io",
                "ClusterSecretStore",
                "clustersecretstores",
                false,
            ),
        ]);
        let es = kind(
            "external-secrets.io",
            "ExternalSecret",
            "externalsecrets",
            true,
        );
        let es_obj = obj(
            json!({"apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
            "metadata": {"name": "db", "namespace": "shop"},
            "spec": {"secretStoreRef": {"name": "local"},
                     "data": [{"sourceRef": {"name": "vault"}}]}}),
        );
        let plan = self::plan(&views, &kinds, &es, &es_obj, "shop");
        assert_eq!(plan.forward.len(), 1);
        assert_eq!(plan.forward[0].target.plural, "secretstores");
        assert_eq!(
            plan.forward[0].refs,
            [("local".to_string(), "shop".to_string())]
        );
        assert_eq!(plan.warns.len(), 1, "{:?}", plan.warns);
        assert!(
            plan.warns[0].contains("kind secretstores is not one of kinds"),
            "{}",
            plan.warns[0]
        );

        let store = kind("external-secrets.io", "SecretStore", "secretstores", true);
        let store_obj = obj(
            json!({"apiVersion": "external-secrets.io/v1", "kind": "SecretStore",
            "metadata": {"name": "vault", "namespace": "shop"}}),
        );
        let plan = self::plan(&views, &kinds, &store, &store_obj, "shop");
        assert!(plan.backward.iter().all(|b| b.rule.relation != "pulls"));
    }

    #[test]
    fn an_alias_default_names_the_source_both_ways() {
        let (views, warnings) = crate::views::compile(
            &toml::from_str::<crate::config::Config>(
                r#"
                [[views.externalsecrets.refs]]
                path = "/spec/secretStoreRef/name"
                kind = "css"
                kind_path = "/spec/secretStoreRef/kind"
                kinds = ["secretstores", "clustersecretstores"]
                reverse = "cluster"
                "#,
            )
            .unwrap()
            .views,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut table = cluster();
        table.0.extend([
            kind(
                "external-secrets.io",
                "ExternalSecret",
                "externalsecrets",
                true,
            ),
            kind("external-secrets.io", "SecretStore", "secretstores", true),
            kind(
                "external-secrets.io",
                "ClusterSecretStore",
                "clustersecretstores",
                false,
            ),
        ]);
        let kinds = Aliased(table, "css", "clustersecretstores");
        let es = kind(
            "external-secrets.io",
            "ExternalSecret",
            "externalsecrets",
            true,
        );
        let es_obj = obj(
            json!({"apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
            "metadata": {"name": "db", "namespace": "shop"},
            "spec": {"secretStoreRef": {"name": "vault"}}}),
        );
        let plan = self::plan(&views, &kinds, &es, &es_obj, "shop");
        assert!(plan.warns.is_empty(), "{:?}", plan.warns);
        assert_eq!(plan.forward.len(), 1);
        assert_eq!(plan.forward[0].target.plural, "clustersecretstores");

        let cluster_store = kind(
            "external-secrets.io",
            "ClusterSecretStore",
            "clustersecretstores",
            false,
        );
        let store_obj = obj(
            json!({"apiVersion": "external-secrets.io/v1", "kind": "ClusterSecretStore",
            "metadata": {"name": "vault"}}),
        );
        let plan = self::plan(&views, &kinds, &cluster_store, &store_obj, "shop");
        let back = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "externalsecrets")
            .expect("externalsecrets are listed for a cluster store");
        assert_eq!(
            back.default.as_ref().map(|d| d.plural.as_str()),
            Some("clustersecretstores")
        );
        assert!(names_source(
            &es_obj,
            &back.rule,
            back.default.as_ref(),
            &cluster_store,
            "vault",
            None
        ));
        let store = kind("external-secrets.io", "SecretStore", "secretstores", true);
        let plan = self::plan(&views, &kinds, &store, &store_obj, "shop");
        let back = plan
            .backward
            .iter()
            .find(|b| b.from.plural == "externalsecrets")
            .unwrap();
        assert!(!names_source(
            &es_obj,
            &back.rule,
            back.default.as_ref(),
            &store,
            "vault",
            Some("shop")
        ));
    }

    #[test]
    fn dedup_drops_repeats_wherever_they_sit() {
        let item = |relation: &str, name: &str| AdjacentItem {
            direction: Direction::Names,
            relation: relation.into(),
            kind: "ConfigMap".into(),
            plural: "configmaps".into(),
            namespace: Some("db".into()),
            name: name.into(),
            object: Box::new(obj(json!({"metadata": {"name": name}}))),
        };
        let mut items = vec![
            item("mounts", "a"),
            item("mounts", "b"),
            item("mounts", "a"),
            item("reads", "a"),
        ];
        dedup(&mut items);
        let left: Vec<(&str, &str)> = items
            .iter()
            .map(|i| (i.relation.as_str(), i.name.as_str()))
            .collect();
        assert_eq!(left, [("mounts", "a"), ("mounts", "b"), ("reads", "a")]);
    }
}
