//! User-configurable table views: custom columns for any resource kind, plus
//! the CRD `additionalPrinterColumns` fallback for unknown custom resources.
//!
//! Views come from config (see [`crate::config::ViewConfig`]) keyed by
//! apiVersion/plural (`"cert-manager.io/v1/certificates"`, `"v1/pods"`),
//! group/plural, bare plural, or lowercased kind — most specific key wins.
//! Column values are extracted with JSON Pointer (RFC 6901) against the
//! object as served by the API: `/metadata/…`, `/apiVersion` and `/kind`
//! resolve alongside `/spec/…` and `/status/…`. Config is validated here at
//! load time; problems become warnings, never panics, so a bad view can't
//! take down the TUI.

use std::collections::HashMap;

use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use kube::discovery::ApiResource;
use serde_json::Value;

/// How a custom column's value is rendered and sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnKind {
    Metric(crate::columns::MetricColumn),
    Builtin,
    #[default]
    Text,
    /// Container image tag, sorted as text.
    ImageTag,
    /// Text that also drives the row's status coloring.
    Status,
    /// Sorted numerically.
    Number,
    /// Kubernetes quantity (`500m`, `1Gi`, `2k`) — sorted by value.
    Quantity,
    QuantityFormat(QuantityFormat),
    /// RFC 3339 timestamp — rendered as compact elapsed time (`3d4h`, or
    /// `in 30d` for the future), sorted by the timestamp.
    Time,
    /// A condition lookup using [`UserColumn::condition_match`] and
    /// [`UserColumn::pointer`]. Shows `status` (`True`/`False`/`Unknown`)
    /// and controls row colors like `Status`.
    Condition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantityFormat {
    Cpu,
    Memory,
}

impl QuantityFormat {
    fn display_value(self, value: f64) -> f64 {
        match self {
            Self::Cpu => (value * 1000.0).round(),
            Self::Memory => value.ceil(),
        }
    }

    fn value(self, value: &Extracted<'_>) -> Option<f64> {
        value.quantity().filter(|value| {
            value.is_finite() && *value >= 0.0 && self.display_value(*value) < i64::MAX as f64
        })
    }

    fn render(self, value: &Extracted<'_>) -> String {
        let Some(number) = self.value(value) else {
            return value.render();
        };
        let sample = Some(self.display_value(number) as i64);
        match self {
            Self::Cpu => crate::columns::fmt_cpu_sample(sample),
            Self::Memory => crate::columns::fmt_mem_sample(sample),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionMatch {
    Type,
    Status,
}

impl ConditionMatch {
    fn field(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Status => "status",
        }
    }
}

/// One compiled custom column.
#[derive(Debug, Clone)]
pub struct UserColumn {
    /// Header, uppercased for display.
    pub header: String,
    /// JSON Pointer, or the value to match for a condition lookup.
    pub pointer: String,
    pub kind: ColumnKind,
    /// Shown only in wide mode (`w`).
    pub wide: bool,
    pub width: Option<u16>,
    pub align: Option<Align>,
    /// The condition field to match against [`Self::pointer`].
    pub condition_match: ConditionMatch,
    /// The output field for a condition lookup. `Condition` reads `status`.
    pub condition_field: Option<String>,
}

/// A compiled per-kind view.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub columns: Vec<UserColumn>,
    /// Initial sort: (header, descending).
    pub sort: Option<(String, bool)>,
    /// Replace the curated columns entirely instead of overlaying them.
    pub replace: bool,
    /// JSON Pointer to the name of the node this kind's objects name — see
    /// [`node_pointer`].
    pub node: Option<String>,
    /// Where `enter` drills to — see [`drill_for`].
    pub drill: Option<Drill>,
    /// References to other objects, for the adjacent view — see
    /// [`crate::adjacent::rules_for`].
    pub refs: Vec<crate::adjacent::RefRule>,
    /// Kinds whose objects this kind owns, scanned for children by the
    /// adjacent view.
    pub children: Vec<String>,
}

/// A configured drill-down: `enter` on a row opens `kind`, scoped by the
/// selectors `labels` and `fields` yield for that row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drill {
    /// Target kind as the user named it (alias, plural, or kind); resolved
    /// against the cluster when the drill happens.
    pub kind: String,
    /// Label selector template with `{name}` / `{namespace}` placeholders.
    pub labels: Option<String>,
    /// Field selector template, same placeholders.
    pub fields: Option<String>,
}

impl Drill {
    /// The label selector for one row: placeholders filled from its metadata.
    pub fn labels_for(&self, obj: &DynamicObject) -> Option<String> {
        self.labels.as_deref().map(|t| fill_placeholders(t, obj))
    }

    /// The field selector for one row, likewise.
    pub fn fields_for(&self, obj: &DynamicObject) -> Option<String> {
        self.fields.as_deref().map(|t| fill_placeholders(t, obj))
    }
}

fn fill_placeholders(template: &str, obj: &DynamicObject) -> String {
    let name = obj.metadata.name.as_deref().unwrap_or_default();
    let namespace = obj.metadata.namespace.as_deref().unwrap_or_default();
    template
        .replace("{name}", name)
        .replace("{namespace}", namespace)
}

/// The placeholders a drill's `labels` template may use.
const DRILL_PLACEHOLDERS: &[&str] = &["name", "namespace"];

/// Kinds whose `enter` has a built-in drill-down (the arms of `App::drill`),
/// by plural. A configured `drill` on one of these would never be consulted,
/// so [`compile`] rejects it with a warning instead of letting it sit there
/// doing nothing. Keep in step with the match in `src/app/navigation.rs`.
pub const BUILTIN_DRILLS: &[&str] = &[
    "namespaces",
    "nodes",
    "deployments",
    "statefulsets",
    "daemonsets",
    "replicasets",
    "jobs",
    "cronjobs",
    "services",
    "pods",
    "customresourcedefinitions",
    "helm",
    "helmhistory",
    "helmreleases",
];

/// The plural a view key names: the last segment of `apiVersion/plural`,
/// `group/plural`, or a bare plural, without any `@namespace` suffix. A key
/// that is a lowercased kind (`pod`) isn't a plural and won't match anything
/// here.
pub(crate) fn key_plural(key: &str) -> &str {
    let last = key.rsplit('/').next().unwrap_or(key);
    last.split('@').next().unwrap_or(last)
}

/// The namespace a view key is qualified with (`pods@prod` → `prod`), if any.
pub(crate) fn key_namespace(key: &str) -> Option<&str> {
    key.rsplit('/').next()?.split_once('@').map(|(_, ns)| ns)
}

/// The `{…}` tokens in a template that aren't in [`DRILL_PLACEHOLDERS`].
fn unknown_placeholders(template: &str) -> Vec<String> {
    let mut unknown = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let token = &after[..end];
        if !DRILL_PLACEHOLDERS.contains(&token) {
            unknown.push(token.to_string());
        }
        rest = &after[end + 1..];
    }
    unknown
}

/// Kinds whose objects name a node, and the JSON Pointer that holds the name.
/// A row that names a node can jump to it (`o`, and `enter` where the kind has
/// no drill-down of its own). Data, not control flow: a kind common enough to
/// ship gets a row here — the treatment `FLUX_OBJECT_COLUMNS` gives Flux —
/// and `[views."…"].node` overrides a row or adds a kind without one.
///
/// Keyed like `[views."…"]` and matched against the same keys, so a row can be
/// as specific as it needs to be: `group/plural` here, because a plural alone
/// would hand Karpenter's pointer to any other group's `nodeclaims`.
const NODE_REFS: &[(&str, &str)] = &[
    ("v1/pods", "/spec/nodeName"),
    // Karpenter writes the node's name onto the claim once it registers.
    ("karpenter.sh/nodeclaims", "/status/nodeName"),
];

/// A per-kind setting, resolved key by key in [`lookup`]'s precedence rather
/// than off whichever single view `lookup` picks: a more specific view that
/// sets only columns mustn't mask a setting on a broader key, which would
/// ignore it with no way to see why.
fn setting<'a, T: ?Sized>(
    views: &'a HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
    pick: impl Fn(&'a View) -> Option<&'a T>,
) -> Option<&'a T> {
    lookup_keys(ar, namespace)
        .into_iter()
        .find_map(|k| views.get(&k).and_then(&pick))
}

/// Where `enter` drills for a kind, per `[views."…"].drill`. Kinds with a
/// built-in drill-down (workloads to pods, CRDs to their resources, …) never
/// consult this.
pub fn drill_for<'a>(
    views: &'a HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Option<&'a Drill> {
    setting(views, ar, namespace, |v| v.drill.as_ref())
}

/// The pointer to a kind's node name: an explicit `[views."…"].node` wins over
/// the built-in table.
pub fn node_pointer<'a>(
    views: &'a HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Option<&'a str> {
    if let Some(pointer) = setting(views, ar, namespace, |v| v.node.as_deref()) {
        return Some(pointer);
    }
    lookup_keys(ar, None).into_iter().find_map(|key| {
        NODE_REFS
            .iter()
            .find(|(row, _)| *row == key)
            .map(|(_, pointer)| *pointer)
    })
}

/// A comparable cell value for typed sorting, mirrored into the app's private
/// sort key (quantities, numbers, and times sort by value, not lexically).
pub enum SortValue {
    Num(f64),
    Text(String),
}

/// Validate and compile the raw `[views]` config. Invalid columns/sorts are
/// skipped with an actionable warning instead of dropping the whole config.
pub fn compile(
    raw: &HashMap<String, crate::config::ViewConfig>,
) -> (HashMap<String, View>, Vec<String>) {
    let mut views = HashMap::new();
    let mut warnings = Vec::new();
    for (key, cfg) in raw {
        if let Some((resource, namespace)) = key.split_once('@')
            && (resource.is_empty()
                || namespace.is_empty()
                || namespace.len() > 63
                || !namespace.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                || !namespace.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                || !namespace
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'))
        {
            warnings.push(format!(
                "views.\"{key}\": invalid namespace suffix; view ignored"
            ));
            continue;
        }
        let mut columns = Vec::new();
        for c in &cfg.columns {
            let header = c.name.trim().to_uppercase();
            if header.is_empty() {
                warnings.push(format!("views.\"{key}\": column with empty name skipped"));
                continue;
            }
            let mut kind = match c.kind.as_deref() {
                None | Some("text") => ColumnKind::Text,
                Some("status") => ColumnKind::Status,
                Some("number") => ColumnKind::Number,
                Some("quantity") => ColumnKind::Quantity,
                Some("time") => ColumnKind::Time,
                Some("condition") => ColumnKind::Condition,
                Some(other) => {
                    warnings.push(format!(
                        "views.\"{key}\": column {header}: unknown type '{other}' \
                         (expected text/status/number/quantity/time/condition); using text"
                    ));
                    ColumnKind::Text
                }
            };
            let sources = usize::from(!c.path.is_empty())
                + usize::from(c.metric.is_some())
                + usize::from(c.builtin.is_some());
            if sources != 1 {
                warnings.push(format!("views.\"{key}\": column {header}: set exactly one of path, metric, or builtin; column skipped"));
                continue;
            }
            if columns
                .iter()
                .any(|column: &UserColumn| column.header == header)
            {
                warnings.push(format!(
                    "views.\"{key}\": duplicate column {header}; column skipped"
                ));
                continue;
            }
            if c.kind.is_some() && (c.metric.is_some() || c.builtin.is_some()) {
                warnings.push(format!("views.\"{key}\": column {header}: type applies only to path columns; column skipped"));
                continue;
            }
            if let Some(format) = &c.format {
                let (formatted_kind, required_type) = match format.as_str() {
                    "image-tag" => (ColumnKind::ImageTag, "text"),
                    "cpu" => (ColumnKind::QuantityFormat(QuantityFormat::Cpu), "quantity"),
                    "memory" => (
                        ColumnKind::QuantityFormat(QuantityFormat::Memory),
                        "quantity",
                    ),
                    _ => {
                        warnings.push(format!("views.\"{key}\": column {header}: unknown format '{format}' (expected image-tag/cpu/memory); column skipped"));
                        continue;
                    }
                };
                if c.path.is_empty() || c.kind.as_deref().unwrap_or("text") != required_type {
                    warnings.push(format!("views.\"{key}\": column {header}: format applies only to {required_type} path columns; column skipped"));
                    continue;
                }
                kind = formatted_kind;
            }
            let mut pointer = c.path.trim().to_string();
            if let Some(metric) = &c.metric {
                let Some(metric) = crate::columns::MetricColumn::parse(metric) else {
                    warnings.push(format!("views.\"{key}\": column {header}: unknown metric '{metric}'; column skipped"));
                    continue;
                };
                kind = ColumnKind::Metric(metric);
            } else if let Some(builtin) = &c.builtin {
                pointer = builtin.trim().to_uppercase();
                if pointer.is_empty() {
                    warnings.push(format!("views.\"{key}\": column {header}: builtin must name a column; column skipped"));
                    continue;
                }
                kind = ColumnKind::Builtin;
            }
            // A condition column's path is the condition *type* name, not a
            // pointer — conditions are found by name because their array
            // order isn't guaranteed by anything.
            if kind == ColumnKind::Condition {
                if c.path.trim().is_empty() || c.path.contains('/') {
                    warnings.push(format!(
                        "views.\"{key}\": column {header}: a condition column's path is the \
                         condition type name (e.g. \"Ready\"), not a JSON Pointer; column skipped"
                    ));
                    continue;
                }
            } else if !matches!(kind, ColumnKind::Metric(_) | ColumnKind::Builtin)
                && !c.path.starts_with('/')
            {
                warnings.push(format!(
                    "views.\"{key}\": column {header}: path '{}' is not a JSON Pointer \
                     (must start with '/', e.g. /status/phase); column skipped",
                    c.path
                ));
                continue;
            }
            let align = match c.align.as_deref() {
                None => None,
                Some("left") => Some(Align::Left),
                Some("center") => Some(Align::Center),
                Some("right") => Some(Align::Right),
                Some(other) => {
                    warnings.push(format!(
                        "views.\"{key}\": column {header}: unknown align '{other}' \
                         (expected left/center/right); using left"
                    ));
                    None
                }
            };
            columns.push(UserColumn {
                header,
                pointer,
                kind,
                wide: c.wide,
                width: c.width,
                align,
                condition_match: ConditionMatch::Type,
                condition_field: None,
            });
        }
        let mut replace = cfg.replace;
        if replace && columns.is_empty() {
            warnings.push(format!(
                "views.\"{key}\": replace = true with no valid columns; overlaying instead"
            ));
            replace = false;
        }
        let sort = cfg
            .sort
            .as_deref()
            .and_then(|s| parse_sort(key, s, &mut warnings));
        let node = match cfg.node.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            Some(pointer) if pointer.starts_with('/') => Some(pointer.to_string()),
            Some(pointer) => {
                warnings.push(format!(
                    "views.\"{key}\": node '{pointer}' is not a JSON Pointer \
                     (must start with '/', e.g. /status/nodeName); ignored"
                ));
                None
            }
            None => None,
        };
        let drill = cfg.drill.as_ref().and_then(|d| {
            let key_lc = key
                .split_once('@')
                .map_or(key.as_str(), |(resource, _)| resource)
                .to_lowercase();
            let plural = key_plural(&key_lc);
            if BUILTIN_DRILLS.contains(&plural) {
                warnings.push(format!(
                    "views.\"{key}\": drill is ignored — `enter` on {plural} has a \
                     built-in drill-down that config doesn't replace"
                ));
                return None;
            }
            let kind = d.kind.trim();
            if kind.is_empty() {
                warnings.push(format!("views.\"{key}\": drill.kind is empty; ignored"));
                return None;
            }
            let labels = d.labels.as_deref().map(str::trim).filter(|l| !l.is_empty());
            let fields = d.fields.as_deref().map(str::trim).filter(|f| !f.is_empty());
            for (what, template) in [("labels", labels), ("fields", fields)] {
                let unknown = template.map(unknown_placeholders).unwrap_or_default();
                if !unknown.is_empty() {
                    warnings.push(format!(
                        "views.\"{key}\": drill.{what} has unknown placeholder(s) {} \
                         (only {{name}} and {{namespace}}); ignored",
                        unknown.join(", ")
                    ));
                    return None;
                }
            }
            Some(Drill {
                kind: kind.to_string(),
                labels: labels.map(str::to_string),
                fields: fields.map(str::to_string),
            })
        });
        let mut refs = Vec::new();
        for (index, r) in cfg.refs.iter().enumerate() {
            let path = r.path.trim();
            let kind = r.kind.trim();
            let kind_path = r
                .kind_path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty());
            let kinds: Vec<String> = r
                .kinds
                .iter()
                .map(|k| k.trim().to_lowercase())
                .filter(|k| !k.is_empty())
                .collect();
            let group_path = r
                .group_path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty());
            let group = r.group.as_deref().map(str::trim);
            let mut problem = None;
            if !path.starts_with('/') {
                problem = Some(format!(
                    "path '{path}' is not a JSON Pointer (must start with '/')"
                ));
            } else if let Some(p) = kind_path
                && !p.starts_with('/')
            {
                problem = Some(format!("kind_path '{p}' is not a JSON Pointer"));
            } else if kind_path.is_some() && kinds.is_empty() {
                problem = Some("kind_path needs kinds, the kinds it may name".to_string());
            } else if kind_path.is_none() && !kinds.is_empty() {
                problem = Some("kinds needs kind_path, where the kind is read from".to_string());
            } else if kind_path.is_none() && kind.is_empty() {
                problem = Some("kind is empty".to_string());
            } else if let Some(p) = kind_path
                && !wildcards_align(path, p)
            {
                problem = Some(format!(
                    "kind_path '{p}' does not follow the same arrays as path '{path}'"
                ));
            } else if r.group_path.is_some() && group_path.is_none() {
                problem = Some("group_path is empty".to_string());
            } else if let Some(p) = group_path
                && !p.starts_with('/')
            {
                problem = Some(format!("group_path '{p}' is not a JSON Pointer"));
            } else if group_path.is_some() && kind_path.is_none() {
                problem = Some("group_path needs kind_path, the kind it qualifies".to_string());
            } else if group.is_some() && group_path.is_none() {
                problem = Some("group needs group_path, where the group is read from".to_string());
            } else if let Some(p) = group_path
                && !wildcards_align(path, p)
            {
                problem = Some(format!(
                    "group_path '{p}' does not follow the same arrays as path '{path}'"
                ));
            } else if let Some(p) = r.namespace_path.as_deref().map(str::trim)
                && !p.starts_with('/')
            {
                problem = Some(format!("namespace_path '{p}' is not a JSON Pointer"));
            } else if let Some(p) = r.namespace_path.as_deref().map(str::trim)
                && !wildcards_align(path, p)
            {
                problem = Some(format!(
                    "namespace_path '{p}' does not follow the same arrays as path '{path}'"
                ));
            }
            let reverse = match r.reverse.as_deref().map(str::trim) {
                None | Some("") => Some(crate::adjacent::Reverse::Namespace),
                Some(s) => crate::adjacent::Reverse::parse(s),
            };
            if reverse.is_none() {
                problem = Some(format!(
                    "reverse '{}' must be namespace, cluster, or none",
                    r.reverse.as_deref().unwrap_or_default()
                ));
            }
            if let Some(why) = problem {
                warnings.push(format!(
                    "views.\"{key}\": ref {}: {why}; skipped",
                    index + 1
                ));
                continue;
            }
            refs.push(crate::adjacent::RefRule {
                from: key.to_lowercase(),
                path: path.to_string(),
                namespace_path: r
                    .namespace_path
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_string),
                kind: kind.to_string(),
                kind_path: kind_path.map(str::to_string),
                kinds,
                group_path: group_path.map(str::to_string),
                group: group.map(str::to_string),
                relation: r
                    .relation
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("references")
                    .to_string(),
                reverse: reverse.unwrap_or_default(),
            });
        }
        let children = cfg
            .children
            .iter()
            .map(|c| c.trim().to_lowercase())
            .filter(|c| !c.is_empty())
            .collect();
        views.insert(
            key.to_lowercase(),
            View {
                columns,
                sort,
                replace,
                node,
                drill,
                refs,
                children,
            },
        );
    }
    (views, warnings)
}

fn wildcards_align(path: &str, sibling: &str) -> bool {
    let path_segs: Vec<&str> = path.split('/').collect();
    let sibling_segs: Vec<&str> = sibling.split('/').collect();
    let stars = |segs: &[&str]| -> Vec<usize> {
        segs.iter()
            .enumerate()
            .filter(|(_, s)| **s == "*")
            .map(|(i, _)| i)
            .collect()
    };
    let path_stars = stars(&path_segs);
    let sibling_stars = stars(&sibling_segs);
    sibling_stars.len() <= path_stars.len()
        && sibling_stars
            .iter()
            .zip(&path_stars)
            .all(|(&s, &p)| sibling_segs[..=s] == path_segs[..=p])
}

/// Parse a view's `sort` value: `"READY"`, `"READY:asc"`, or `"READY:desc"`.
fn parse_sort(key: &str, s: &str, warnings: &mut Vec<String>) -> Option<(String, bool)> {
    let (col, dir) = match s.rsplit_once(':') {
        Some((c, d)) => (c, Some(d.trim())),
        None => (s, None),
    };
    let desc = match dir {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(other) => {
            warnings.push(format!(
                "views.\"{key}\": sort direction '{other}' is not asc/desc; using asc"
            ));
            false
        }
    };
    let col = col.trim().to_uppercase();
    if col.is_empty() {
        warnings.push(format!("views.\"{key}\": empty sort column ignored"));
        return None;
    }
    Some((col, desc))
}

/// The keys a resource's view can be configured under, most specific first:
/// `apiVersion/plural`, `group/plural`, plural, then lowercased kind.
/// Try all namespace-qualified keys before the unqualified keys.
pub(crate) fn lookup_keys(ar: &ApiResource, namespace: Option<&str>) -> Vec<String> {
    let plural = ar.plural.to_lowercase();
    let mut keys = vec![format!("{}/{plural}", ar.api_version.to_lowercase())];
    if !ar.group.is_empty() {
        keys.push(format!("{}/{plural}", ar.group.to_lowercase()));
    }
    keys.push(plural);
    keys.push(ar.kind.to_lowercase());
    if let Some(namespace) = namespace.filter(|ns| !ns.is_empty()) {
        let mut qualified: Vec<_> = keys
            .iter()
            .map(|key| format!("{key}@{namespace}"))
            .collect();
        qualified.extend(keys);
        return qualified;
    }
    keys
}

/// Find the view for a resource, most specific key first.
pub fn lookup<'a>(
    views: &'a HashMap<String, View>,
    ar: &ApiResource,
    namespace: Option<&str>,
) -> Option<&'a View> {
    lookup_keys(ar, namespace)
        .into_iter()
        .find_map(|k| views.get(&k))
}

/// Resolve a JSON Pointer against the object as served by the API:
/// `/metadata/…` and `/apiVersion`/`/kind` come from the typed fields, the
/// rest from the body (`DynamicObject::data` holds spec/status/…).
pub fn extract(obj: &DynamicObject, pointer: &str) -> Option<Value> {
    extract_ref(obj, pointer).map(Extracted::into_value)
}

/// A custom column's extracted value, borrowed wherever the object already
/// holds it in the shape the renderer needs: `DynamicObject::data` is already
/// a `Value` tree, and `ObjectMeta`'s scalars and label/annotation values are
/// already `String`s. A column pointing at a 100-element array or a nested
/// object therefore formats it where it lies instead of deep-cloning the whole
/// subtree first, and only the finished cell allocates.
///
/// `Owned` covers the metadata shapes that have to be serialized to answer the
/// pointer at all (`ownerReferences`, whole `metadata`, timestamps).
pub(crate) enum Extracted<'a> {
    /// A node of the object body.
    Json(&'a Value),
    /// A string the object already stores as one.
    Text(&'a str),
    /// A value that had to be built to answer the pointer.
    Owned(Value),
}

impl<'a> Extracted<'a> {
    /// The string form, when the value is one — text, time and quantity
    /// columns all want this and nothing else.
    fn as_str(&self) -> Option<&str> {
        match self {
            Extracted::Text(s) => Some(s),
            Extracted::Json(v) => v.as_str(),
            Extracted::Owned(v) => v.as_str(),
        }
    }

    /// Give up the borrow. Only [`extract`]'s owned callers pay this.
    fn into_value(self) -> Value {
        match self {
            Extracted::Json(v) => v.clone(),
            Extracted::Text(s) => Value::String(s.to_string()),
            Extracted::Owned(v) => v,
        }
    }

    /// The rendered cell — the one allocation the extraction path owes.
    fn render(&self) -> String {
        match self {
            Extracted::Text(s) => (*s).into(),
            Extracted::Json(v) => render_value(v),
            Extracted::Owned(v) => render_value(v),
        }
    }

    fn number(&self) -> Option<f64> {
        match self {
            Extracted::Text(s) => s.trim().parse().ok(),
            Extracted::Json(v) => number_of(v),
            Extracted::Owned(v) => number_of(v),
        }
    }

    fn quantity(&self) -> Option<f64> {
        match self {
            Extracted::Text(s) => parse_quantity(s),
            Extracted::Json(v) => quantity_of(v),
            Extracted::Owned(v) => quantity_of(v),
        }
    }
}

/// [`extract`] without the clone. See [`Extracted`].
pub(crate) fn extract_ref<'a>(obj: &'a DynamicObject, pointer: &str) -> Option<Extracted<'a>> {
    if let Some(rest) = pointer.strip_prefix("/metadata")
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return extract_metadata(&obj.metadata, rest);
    }
    match pointer {
        "/apiVersion" => {
            return obj
                .types
                .as_ref()
                .map(|t| Extracted::Text(t.api_version.as_str()));
        }
        "/kind" => return obj.types.as_ref().map(|t| Extracted::Text(t.kind.as_str())),
        _ => {}
    }
    obj.data.pointer(pointer).map(Extracted::Json)
}

/// Resolve metadata without serializing the entire `ObjectMeta` for every
/// custom-column cell. Complex or whole-metadata requests still serialize the
/// selected value, preserving the exact JSON Pointer behavior and output.
fn extract_metadata<'a>(meta: &'a kube::core::ObjectMeta, rest: &str) -> Option<Extracted<'a>> {
    if rest.is_empty() {
        return serde_json::to_value(meta).ok().map(Extracted::Owned);
    }
    let path = rest.strip_prefix('/')?;
    let field = path.split_once('/').map_or(path, |(field, _)| field);
    let tail = &path[field.len()..];

    // A `String` field ObjectMeta already holds: the cell is that string.
    // A pointer that walks further into a JSON string matches nothing, which
    // is what serializing it and walking `tail` used to conclude the long way.
    macro_rules! text {
        ($value:expr) => {{
            let s: &str = $value;
            tail.is_empty().then_some(Extracted::Text(s))
        }};
    }

    macro_rules! selected {
        ($value:expr) => {{
            let value = serde_json::to_value($value).ok()?;
            if tail.is_empty() {
                Some(Extracted::Owned(value))
            } else {
                value.pointer(tail).cloned().map(Extracted::Owned)
            }
        }};
    }

    match field {
        "name" => text!(meta.name.as_ref()?),
        "namespace" => text!(meta.namespace.as_ref()?),
        "uid" => text!(meta.uid.as_ref()?),
        "resourceVersion" => text!(meta.resource_version.as_ref()?),
        "generateName" => text!(meta.generate_name.as_ref()?),
        "selfLink" => text!(meta.self_link.as_ref()?),
        "generation" => selected!(meta.generation?),
        "deletionGracePeriodSeconds" => selected!(meta.deletion_grace_period_seconds?),
        "creationTimestamp" => selected!(meta.creation_timestamp.as_ref()?),
        "deletionTimestamp" => selected!(meta.deletion_timestamp.as_ref()?),
        "labels" => extract_string_map(meta.labels.as_ref()?, tail),
        "annotations" => extract_string_map(meta.annotations.as_ref()?, tail),
        "finalizers" => selected!(meta.finalizers.as_ref()?),
        "ownerReferences" => selected!(meta.owner_references.as_ref()?),
        "managedFields" => selected!(meta.managed_fields.as_ref()?),
        _ => None,
    }
}

fn extract_string_map<'a>(
    map: &'a std::collections::BTreeMap<String, String>,
    tail: &str,
) -> Option<Extracted<'a>> {
    if tail.is_empty() {
        return serde_json::to_value(map).ok().map(Extracted::Owned);
    }
    let token = tail.strip_prefix('/')?;
    if token.contains('/') {
        return None;
    }
    // JSON Pointer unescapes `~1` before `~0`; doing so in this order also
    // preserves the RFC-defined meaning of tokens such as `~01`. A label or
    // annotation value is borrowed straight out of the map — the whole map
    // used to be serialized to JSON to read one key out of it.
    if token.contains('~') {
        let key = token.replace("~1", "/").replace("~0", "~");
        map.get(&key).map(|v| Extracted::Text(v.as_str()))
    } else {
        map.get(token).map(|v| Extracted::Text(v.as_str()))
    }
}

/// Render one custom column's cell. Missing values read as `<none>`.
pub fn render_cell(obj: &DynamicObject, col: &UserColumn, now: i64) -> String {
    if matches!(col.kind, ColumnKind::Metric(_)) {
        return "-".into();
    }
    if col.kind == ColumnKind::Condition {
        return condition_value(obj, &col.pointer, col.condition_match, "status")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "<none>".into());
    }
    let Some(v) = cell_value(obj, col) else {
        return "<none>".into();
    };
    match col.kind {
        ColumnKind::Time => render_time(&v, now),
        ColumnKind::ImageTag => render_image_tag(&v),
        ColumnKind::QuantityFormat(format) => format.render(&v),
        _ => v.render(),
    }
}

pub fn formatted_quantity_value(obj: &DynamicObject, col: &UserColumn) -> Option<f64> {
    let ColumnKind::QuantityFormat(format) = col.kind else {
        return None;
    };
    format.value(&cell_value(obj, col)?)
}

fn render_image_tag(v: &Extracted<'_>) -> String {
    let Some(image) = v.as_str().filter(|image| !image.is_empty()) else {
        return v.render();
    };
    let (name, digest) = image
        .split_once('@')
        .map_or((image, None), |(name, digest)| (name, Some(digest)));
    let last_component = name.rsplit('/').next().unwrap_or(name);
    if let Some((_, tag)) = last_component.rsplit_once(':') {
        tag.into()
    } else if digest.is_some() {
        "-".into()
    } else {
        "latest".into()
    }
}

/// Read a non-`Condition` column through a JSON Pointer or condition filter.
fn cell_value<'a>(obj: &'a DynamicObject, col: &UserColumn) -> Option<Extracted<'a>> {
    match col.condition_field.as_deref() {
        Some(field) => {
            condition_value(obj, &col.pointer, col.condition_match, field).map(Extracted::Json)
        }
        None => extract_ref(obj, &col.pointer),
    }
}

/// Read the status of a condition selected by type name.
pub fn condition_status(obj: &DynamicObject, cond_type: &str) -> Option<String> {
    condition_value(obj, cond_type, ConditionMatch::Type, "status")?
        .as_str()
        .map(str::to_string)
}

/// Read the first available output field from matching conditions.
fn condition_value<'a>(
    obj: &'a DynamicObject,
    value: &str,
    selector: ConditionMatch,
    field: &str,
) -> Option<&'a Value> {
    obj.data
        .pointer("/status/conditions")?
        .as_array()?
        .iter()
        .filter(|c| c.get(selector.field()).and_then(Value::as_str) == Some(value))
        .find_map(|c| c.get(field))
}

fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "<none>".into(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Timestamps render as compact elapsed time (`3d4h`); a future timestamp
/// (e.g. a certificate's `notAfter`) reads `in 30d`. Values that don't parse
/// as RFC 3339 fall back to the raw string.
fn render_time(v: &Extracted<'_>, now: i64) -> String {
    let Some(s) = v.as_str() else {
        return v.render();
    };
    match s.parse::<Timestamp>() {
        Ok(ts) => {
            let delta = now - ts.as_second();
            if delta >= 0 {
                crate::columns::humanize(delta)
            } else {
                format!("in {}", crate::columns::humanize(-delta))
            }
        }
        Err(_) => s.into(),
    }
}

/// Comparable value of a custom column's cell: numbers, quantities, and times
/// sort by value (missing/unparseable last in ascending order), text sorts
/// case-insensitively.
pub fn sort_value(obj: &DynamicObject, col: &UserColumn, now: i64) -> SortValue {
    if col.kind == ColumnKind::Condition {
        return SortValue::Text(
            condition_value(obj, &col.pointer, col.condition_match, "status")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
                .to_lowercase(),
        );
    }
    let v = cell_value(obj, col);
    match col.kind {
        ColumnKind::ImageTag => SortValue::Text(
            v.as_ref()
                .map(render_image_tag)
                .unwrap_or_default()
                .to_lowercase(),
        ),
        ColumnKind::Number => {
            SortValue::Num(v.as_ref().and_then(Extracted::number).unwrap_or(f64::MAX))
        }
        ColumnKind::Quantity => {
            SortValue::Num(v.as_ref().and_then(Extracted::quantity).unwrap_or(f64::MAX))
        }
        ColumnKind::QuantityFormat(format) => SortValue::Num(
            v.as_ref()
                .and_then(|value| format.value(value))
                .unwrap_or(f64::MAX),
        ),
        // Elapsed seconds, like AGE: ascending = most recent (or furthest in
        // the future) first, unknowns last.
        ColumnKind::Time => SortValue::Num(
            v.as_ref()
                .and_then(Extracted::as_str)
                .and_then(|s| s.parse::<Timestamp>().ok())
                .map(|ts| (now - ts.as_second()) as f64)
                .unwrap_or(f64::MAX),
        ),
        // Condition is handled above (its "pointer" is a condition name, not
        // something `extract` understands).
        ColumnKind::Text
        | ColumnKind::Status
        | ColumnKind::Condition
        | ColumnKind::Metric(_)
        | ColumnKind::Builtin => SortValue::Text(
            v.as_ref()
                .map(Extracted::render)
                .unwrap_or_default()
                .to_lowercase()
                .to_string(),
        ),
    }
}

fn number_of(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn quantity_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_quantity(s),
        _ => None,
    }
}

/// Parse a Kubernetes quantity (`500m` → 0.5, `1Gi` → 1073741824, `2k` →
/// 2000) into its base-unit value.
pub fn parse_quantity(s: &str) -> Option<f64> {
    let s = s.trim();
    // Two-char binary suffixes must be tried before the one-char decimal ones
    // (`1Gi` would otherwise strip a bogus trailing `i`).
    const SUFFIXES: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("Pi", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("Ei", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];
    for (suf, mult) in SUFFIXES {
        if let Some(num) = s.strip_suffix(suf) {
            return num.trim().parse::<f64>().ok().map(|n| n * mult);
        }
    }
    s.parse::<f64>().ok()
}

/// Build a view from CRD printer columns for resources without a user view.
/// Equality filters on condition type or status select the first available
/// output field. Other filters and wildcards are skipped. Columns with
/// priority above zero appear only in wide mode, like kubectl's `-o wide`.
pub fn printer_columns_view(crd: &Value, version: &str) -> Option<View> {
    let versions = crd.pointer("/spec/versions")?.as_array()?;
    let ver = versions
        .iter()
        .find(|v| v.get("name").and_then(Value::as_str) == Some(version))?;
    let cols = ver.get("additionalPrinterColumns")?.as_array()?;
    let columns: Vec<UserColumn> = cols
        .iter()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?;
            let json_path = c.get("jsonPath").and_then(Value::as_str)?;
            let wide = c.get("priority").and_then(Value::as_i64).unwrap_or(0) > 0;
            let kind_from_crd = || match c.get("type").and_then(Value::as_str) {
                Some("integer" | "number") => ColumnKind::Number,
                Some("date") => ColumnKind::Time,
                _ => ColumnKind::Text,
            };
            // Only the status output controls condition colors.
            // Other output fields keep the CRD column type.
            if let Some((condition_match, cond, field)) = condition_json_path(json_path) {
                let (kind, condition_field) = if field == "status" {
                    (ColumnKind::Condition, None)
                } else {
                    (kind_from_crd(), Some(field))
                };
                return Some(UserColumn {
                    header: name.to_uppercase(),
                    pointer: cond,
                    kind,
                    wide,
                    width: None,
                    align: None,
                    condition_match,
                    condition_field,
                });
            }
            let pointer = json_path_to_pointer(json_path)?;
            Some(UserColumn {
                header: name.to_uppercase(),
                pointer,
                kind: kind_from_crd(),
                wide,
                width: None,
                align: None,
                condition_match: ConditionMatch::Type,
                condition_field: None,
            })
        })
        .collect();
    if columns.is_empty() {
        None
    } else {
        Some(View {
            columns,
            sort: None,
            replace: false,
            node: None,
            drill: None,
            refs: Vec::new(),
            children: Vec::new(),
        })
    }
}

/// Recognize a condition equality filter on `type` or `status` with a simple
/// output field. Accept single or double quotes, spaces inside the filter,
/// and optional outer braces.
/// Return the selector, value, and output field, or `None` for other paths.
pub fn condition_json_path(path: &str) -> Option<(ConditionMatch, String, String)> {
    let p = path.trim();
    let p = p
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(p);
    let rest = p.strip_prefix(".status.conditions[?(")?;
    let rest = rest.trim_start().strip_prefix("@.")?;
    let (selector, rest) = rest.split_once("==")?;
    let selector = match selector.trim_end() {
        "type" => ConditionMatch::Type,
        "status" => ConditionMatch::Status,
        _ => return None,
    };
    let (quoted, tail) = rest.split_once(")]")?;
    let field = tail.strip_prefix('.')?;
    if field.is_empty()
        || field.contains(['.', '*', '?', '@', '(', ')', '[', ']', '"', '\'', '\\', ' '])
    {
        return None;
    }
    let t = quoted.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))?;
    (!t.is_empty() && !t.contains(['"', '\'']))
        .then(|| (selector, t.to_string(), field.to_string()))
}

/// Convert a simple kubectl JSONPath (`.status.phase`,
/// `.spec.ports[0].port`) to a JSON Pointer. Expressions with filters,
/// wildcards, quoting, or recursive descent aren't representable — `None`.
pub fn json_path_to_pointer(path: &str) -> Option<String> {
    let path = path.trim();
    let path = path
        .strip_prefix('{')
        .and_then(|p| p.strip_suffix('}'))
        .unwrap_or(path);
    let rest = path.strip_prefix('.')?;
    if rest.is_empty()
        || rest.contains(['*', '?', '@', '(', ')', '"', '\'', '\\', ' '])
        || rest.contains("..")
    {
        return None;
    }
    let mut out = String::new();
    for seg in rest.split('.') {
        if seg.is_empty() {
            return None;
        }
        let mut seg = seg;
        loop {
            match seg.split_once('[') {
                Some((head, tail)) => {
                    if !head.is_empty() {
                        out.push('/');
                        out.push_str(&escape_segment(head));
                    }
                    let (idx, more) = tail.split_once(']')?;
                    idx.parse::<usize>().ok()?;
                    out.push('/');
                    out.push_str(idx);
                    if more.is_empty() {
                        break;
                    }
                    seg = more;
                }
                None => {
                    out.push('/');
                    out.push_str(&escape_segment(seg));
                    break;
                }
            }
        }
    }
    Some(out)
}

/// RFC 6901 escaping for one pointer segment.
fn escape_segment(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: serde_json::Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn column_sources_validate_without_discarding_other_columns() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
            [views."v1/pods"]
            columns = [
                { name = "NAME", builtin = "NAME" },
                { name = "CPU", metric = "cpu" },
                { name = "CPU", metric = "memory" },
                { name = "BOTH", path = "/spec/cpu", metric = "cpu" },
                { name = "UNKNOWN", metric = "cpus" },
                { name = "BAD", builtin = "" },
                { name = "TYPED", metric = "memory", type = "text" },
            ]
        "#,
        )
        .unwrap();
        let (views, warnings) = compile(&cfg.views);
        assert_eq!(views["v1/pods"].columns.len(), 2);
        assert_eq!(warnings.len(), 5);
    }

    #[test]
    fn image_tag_formats_validate_without_discarding_valid_columns() {
        let (views, warnings) = compile_toml(
            r#"
            [views.pods]
            columns = [
                { name = "TAG", path = "/spec/image", format = "image-tag" },
                { name = "TEXT", path = "/spec/image", type = "text", format = "image-tag" },
                { name = "RAW", path = "/spec/image" },
                { name = "UNKNOWN", path = "/spec/image", format = "split" },
                { name = "CPU", metric = "cpu", format = "image-tag" },
                { name = "NAME", builtin = "NAME", format = "image-tag" },
                { name = "NUMBER", path = "/spec/image", type = "number", format = "image-tag" },
                { name = "QUANTITY", path = "/spec/image", type = "quantity", format = "image-tag" },
                { name = "TIME", path = "/spec/image", type = "time", format = "image-tag" },
                { name = "STATUS", path = "/spec/image", type = "status", format = "image-tag" },
                { name = "CONDITION", path = "Ready", type = "condition", format = "image-tag" },
                { name = "BADPATH", path = "image", format = "image-tag" },
            ]
            "#,
        );
        let columns = &views["pods"].columns;
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].kind, ColumnKind::ImageTag);
        assert_eq!(columns[1].kind, ColumnKind::ImageTag);
        assert_eq!(columns[2].kind, ColumnKind::Text);
        assert_eq!(warnings.len(), 9, "{warnings:?}");
        assert!(warnings[0].contains("unknown format 'split'"));
        assert!(
            warnings[1..8]
                .iter()
                .all(|w| w.contains("format applies only to text path columns"))
        );
        assert!(warnings[8].contains("JSON Pointer"));
    }

    #[test]
    fn image_tag_rendering_and_sorting_use_the_tag() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for (image, expected) in [
            (json!("registry:5000/app:1.2.3"), "1.2.3"),
            (json!("registry:5000/team/app"), "latest"),
            (json!("app"), "latest"),
            (json!(format!("app@{digest}")), "-"),
            (json!(format!("registry:5000/app@{digest}")), "-"),
            (json!(format!("app:1.2.3@{digest}")), "1.2.3"),
            (json!(format!("registry:5000/app:RC1@{digest}")), "RC1"),
            (json!("[::1]:5000/app:2"), "2"),
            (json!(""), ""),
            (Value::Null, "<none>"),
            (json!(42), "42"),
        ] {
            let o = obj(json!({"apiVersion": "v1", "kind": "Pod", "spec": {"image": image}}));
            let column = col("/spec/image", ColumnKind::ImageTag);
            assert_eq!(render_cell(&o, &column, 0), expected);
            assert!(
                matches!(sort_value(&o, &column, 0), SortValue::Text(value) if value == expected.to_lowercase())
            );
        }
    }

    fn col(pointer: &str, kind: ColumnKind) -> UserColumn {
        UserColumn {
            header: "COL".into(),
            pointer: pointer.into(),
            kind,
            wide: false,
            width: None,
            align: None,
            condition_match: ConditionMatch::Type,
            condition_field: None,
        }
    }

    fn compile_toml(text: &str) -> (HashMap<String, View>, Vec<String>) {
        let cfg: crate::config::Config = toml::from_str(text).unwrap();
        compile(&cfg.views)
    }

    #[test]
    fn condition_columns_look_up_by_type_name_not_index() {
        let (views, warnings) = compile_toml(
            r#"
            [views."cert-manager.io/v1/certificates"]
            [[views."cert-manager.io/v1/certificates".columns]]
            name = "READY"
            path = "Ready"
            type = "condition"

            [[views."cert-manager.io/v1/certificates".columns]]
            name = "BAD"
            path = "/status/conditions/0/status"
            type = "condition"
            "#,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("condition type name"), "{warnings:?}");
        let view = &views["cert-manager.io/v1/certificates"];
        assert_eq!(view.columns.len(), 1);
        let col = &view.columns[0];
        assert_eq!(col.kind, ColumnKind::Condition);
        assert_eq!(col.pointer, "Ready");

        // Lookup is by condition type, regardless of array order.
        let obj: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "tls"},
            "status": {"conditions": [
                {"type": "Issuing", "status": "True"},
                {"type": "Ready", "status": "False"},
            ]}
        }))
        .unwrap();
        assert_eq!(render_cell(&obj, col, crate::columns::now_secs()), "False");
        match sort_value(&obj, col, crate::columns::now_secs()) {
            SortValue::Text(t) => assert_eq!(t, "false"),
            SortValue::Num(_) => panic!("conditions sort as text"),
        }
        // A missing condition reads as <none>, not a panic or a lie.
        let bare: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "new"}
        }))
        .unwrap();
        assert_eq!(
            render_cell(&bare, col, crate::columns::now_secs()),
            "<none>"
        );
    }

    #[test]
    fn condition_json_path_recognizes_the_canonical_filter() {
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].status"#),
            Some((ConditionMatch::Type, "Ready".into(), "status".into()))
        );
        assert_eq!(
            condition_json_path(".status.conditions[?(@.type=='Available')].status"),
            Some((ConditionMatch::Type, "Available".into(), "status".into()))
        );
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].reason"#),
            Some((ConditionMatch::Type, "Ready".into(), "reason".into()))
        );
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].message"#),
            Some((ConditionMatch::Type, "Ready".into(), "message".into()))
        );
        assert_eq!(
            condition_json_path("{.status.conditions[?(@.type=='Ready')].lastTransitionTime}"),
            Some((
                ConditionMatch::Type,
                "Ready".into(),
                "lastTransitionTime".into()
            ))
        );
        // cert-manager writes the filter with spaces around `==`.
        for path in [
            r#".status.conditions[?(@.type == "Ready")].status"#,
            r#".status.conditions[?( @.type=="Ready" )].status"#,
            "{.status.conditions[?(@.type == 'Ready')].status}",
        ] {
            assert_eq!(
                condition_json_path(path),
                Some((ConditionMatch::Type, "Ready".into(), "status".into()))
            );
        }
        assert_eq!(
            condition_json_path(".status.conditions[?(@.status == 'True')].reason"),
            Some((ConditionMatch::Status, "True".into(), "reason".into()))
        );
        // Nested fields, wildcards, and non-condition paths stay untranslated.
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].foo.bar"#),
            None
        );
        assert_eq!(condition_json_path(".status.phase"), None);
    }

    #[test]
    fn compiles_roadmap_example() {
        let (views, warnings) = compile_toml(
            r#"
            [views."cert-manager.io/v1/certificates"]
            sort = "READY"

            [[views."cert-manager.io/v1/certificates".columns]]
            name = "READY"
            path = "/status/conditions/0/status"
            type = "status"

            [[views."cert-manager.io/v1/certificates".columns]]
            name = "EXPIRES"
            path = "/status/notAfter"
            type = "time"
            wide = true
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let v = &views["cert-manager.io/v1/certificates"];
        assert_eq!(v.sort, Some(("READY".into(), false)));
        assert!(!v.replace);
        assert_eq!(v.columns.len(), 2);
        assert_eq!(v.columns[0].kind, ColumnKind::Status);
        assert_eq!(v.columns[1].kind, ColumnKind::Time);
        assert!(v.columns[1].wide);
    }

    #[test]
    fn invalid_columns_warn_and_are_skipped_not_fatal() {
        let (views, warnings) = compile_toml(
            r#"
            [views.widgets]
            sort = "PHASE:desc"
            replace = true

            [[views.widgets.columns]]
            name = "BAD"
            path = "status.phase"

            [[views.widgets.columns]]
            name = "ODD"
            path = "/status/phase"
            type = "fancy"
            align = "diagonal"
            "#,
        );
        // Bad pointer skipped, unknown type/align degrade to defaults.
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("JSON Pointer"));
        let v = &views["widgets"];
        assert_eq!(v.columns.len(), 1);
        assert_eq!(v.columns[0].kind, ColumnKind::Text);
        assert_eq!(v.columns[0].align, None);
        assert_eq!(v.sort, Some(("PHASE".into(), true)));
    }

    #[test]
    fn replace_without_columns_degrades_to_overlay() {
        let (views, warnings) = compile_toml(
            r#"
            [views.widgets]
            replace = true

            [[views.widgets.columns]]
            name = "BAD"
            path = "no-slash"
            "#,
        );
        assert!(!views["widgets"].replace);
        assert!(warnings.iter().any(|w| w.contains("replace")));
    }

    #[test]
    fn namespace_view_keys_use_two_precedence_groups() {
        let ar = ApiResource {
            group: "example.com".into(),
            version: "v1".into(),
            api_version: "example.com/v1".into(),
            kind: "Widget".into(),
            plural: "widgets".into(),
        };
        let keys = [
            "example.com/v1/widgets@batch",
            "example.com/widgets@batch",
            "widgets@batch",
            "widget@batch",
            "example.com/v1/widgets",
            "example.com/widgets",
            "widgets",
            "widget",
        ];
        let mut views: HashMap<_, _> = keys
            .iter()
            .map(|key| {
                (
                    key.to_string(),
                    View {
                        sort: Some((key.to_string(), false)),
                        ..Default::default()
                    },
                )
            })
            .collect();
        for key in keys {
            assert_eq!(
                lookup(&views, &ar, Some("batch"))
                    .unwrap()
                    .sort
                    .as_ref()
                    .unwrap()
                    .0,
                key
            );
            views.remove(key);
        }
        assert!(lookup(&views, &ar, Some("batch")).is_none());
        let (views, warnings) = compile_toml(
            r#"
            [views."widgets@batch"]
            sort = "NAME"
            [views.widgets]
            sort = "AGE"
        "#,
        );
        assert!(warnings.is_empty());
        for namespace in [None, Some(""), Some("other")] {
            assert_eq!(
                lookup(&views, &ar, namespace).unwrap().sort,
                Some(("AGE".into(), false))
            );
        }
    }

    #[test]
    fn invalid_namespace_suffixes_warn_and_skip_the_view() {
        for key in [
            "pods@",
            "@batch",
            "pods@a@b",
            "pods@Batch",
            "pods@-batch",
            "pods@batch-",
            "pods@a.b",
            "pods@a_b",
            "pods@all namespaces",
        ] {
            let (views, warnings) = compile_toml(&format!("[views.\"{key}\"]"));
            assert!(views.is_empty(), "{key}");
            assert_eq!(warnings.len(), 1, "{key}");
            assert!(warnings[0].contains("invalid namespace suffix"));
        }
        let key = format!("pods@{}", "a".repeat(64));
        let (views, warnings) = compile_toml(&format!("[views.\"{key}\"]"));
        assert!(views.is_empty());
        assert_eq!(warnings.len(), 1);
        let (_, warnings) = compile_toml(
            r#"[views."pods@batch"]
            drill = { kind = "secrets" }
        "#,
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("drill is ignored"))
        );
    }

    #[test]
    fn lookup_prefers_most_specific_key() {
        let ar = ApiResource {
            group: "cert-manager.io".into(),
            version: "v1".into(),
            api_version: "cert-manager.io/v1".into(),
            kind: "Certificate".into(),
            plural: "certificates".into(),
        };
        let mk = |sort: &str| View {
            sort: Some((sort.to_uppercase(), false)),
            ..Default::default()
        };
        let mut views = HashMap::new();
        views.insert("certificate".to_string(), mk("by-kind"));
        assert_eq!(
            lookup(&views, &ar, None).unwrap().sort,
            Some(("BY-KIND".into(), false))
        );
        views.insert("certificates".to_string(), mk("by-plural"));
        assert_eq!(
            lookup(&views, &ar, None).unwrap().sort,
            Some(("BY-PLURAL".into(), false))
        );
        views.insert("cert-manager.io/certificates".to_string(), mk("by-group"));
        assert_eq!(
            lookup(&views, &ar, None).unwrap().sort,
            Some(("BY-GROUP".into(), false))
        );
        views.insert("cert-manager.io/v1/certificates".to_string(), mk("by-gvr"));
        assert_eq!(
            lookup(&views, &ar, None).unwrap().sort,
            Some(("BY-GVR".into(), false))
        );
    }

    #[test]
    fn node_pointer_falls_back_to_the_builtin_table() {
        let ar = |group: &str, kind: &str, plural: &str| ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: if group.is_empty() {
                "v1".into()
            } else {
                format!("{group}/v1")
            },
            kind: kind.into(),
            plural: plural.into(),
        };
        let views = HashMap::new();
        assert_eq!(
            node_pointer(&views, &ar("", "Pod", "pods"), None),
            Some("/spec/nodeName")
        );
        let claims = ar("karpenter.sh", "NodeClaim", "nodeclaims");
        assert_eq!(
            node_pointer(&views, &claims, None),
            Some("/status/nodeName")
        );
        // A kind the table doesn't list names no node until config says where.
        let pools = ar("karpenter.sh", "NodePool", "nodepools");
        assert_eq!(node_pointer(&views, &pools, None), None);
        // Rows are scoped to their group: a same-named plural elsewhere
        // (or PodMetrics, whose plural is also `pods`) gets nothing.
        let other_claims = ar("example.com", "NodeClaim", "nodeclaims");
        assert_eq!(node_pointer(&views, &other_claims, None), None);
        let pod_metrics = ar("metrics.k8s.io", "PodMetrics", "pods");
        assert_eq!(node_pointer(&views, &pod_metrics, None), None);

        let (configured, warnings) = compile_toml(
            r#"
            [views."karpenter.sh/v1/nodeclaims"]
            node = "/status/providerID"

            [views.nodepools]
            node = "/status/host"
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // Config overrides a shipped row and adds a kind without one.
        assert_eq!(
            node_pointer(&configured, &claims, None),
            Some("/status/providerID")
        );
        assert_eq!(
            node_pointer(&configured, &pools, None),
            Some("/status/host")
        );
    }

    #[test]
    fn a_narrower_view_without_node_does_not_mask_a_broader_one() {
        let widgets = ApiResource {
            group: "example.com".into(),
            version: "v1".into(),
            api_version: "example.com/v1".into(),
            kind: "Widget".into(),
            plural: "widgets".into(),
        };
        // The most specific key wins for columns and sort, but it sets no
        // `node` — the broader key's must still be found.
        let (views, warnings) = compile_toml(
            r#"
            [views."example.com/v1/widgets"]
            sort = "PHASE"

            [views.widgets]
            node = "/status/host"
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(lookup(&views, &widgets, None).unwrap().node, None);
        assert_eq!(node_pointer(&views, &widgets, None), Some("/status/host"));
    }

    #[test]
    fn configured_node_overrides_the_table_and_validates() {
        let pods = ApiResource {
            group: String::new(),
            version: "v1".into(),
            api_version: "v1".into(),
            kind: "Pod".into(),
            plural: "pods".into(),
        };
        let (views, warnings) = compile_toml(
            r#"
            [views.pods]
            node = "/status/hostName"

            [views.widgets]
            node = "status.host"
            "#,
        );
        assert_eq!(node_pointer(&views, &pods, None), Some("/status/hostName"));
        // A path that isn't a pointer is dropped with a warning, like columns.
        assert_eq!(views["widgets"].node, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("JSON Pointer"), "{}", warnings[0]);
    }

    #[test]
    fn drill_compiles_and_fills_placeholders_from_the_row() {
        let pools = ApiResource {
            group: "karpenter.sh".into(),
            version: "v1".into(),
            api_version: "karpenter.sh/v1".into(),
            kind: "NodePool".into(),
            plural: "nodepools".into(),
        };
        let (views, warnings) = compile_toml(
            r#"
            [views."karpenter.sh/v1/nodepools"]
            sort = "NAME"

            [views.nodepools]
            drill = { kind = "nodeclaims", labels = "karpenter.sh/nodepool={name}" }
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // Found on the broader key even though the narrower one matches first.
        let drill = drill_for(&views, &pools, None).expect("drill configured");
        assert_eq!(drill.kind, "nodeclaims");
        let pool = obj(json!({"metadata": {"name": "default"}}));
        assert_eq!(
            drill.labels_for(&pool).as_deref(),
            Some("karpenter.sh/nodepool=default")
        );
        assert_eq!(drill.fields_for(&pool), None);
        // A target that needs no selector is allowed.
        let (views, warnings) = compile_toml(
            r#"
            [views.nodepools]
            drill = { kind = "nodeclaims" }
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            drill_for(&views, &pools, None).unwrap().labels_for(&pool),
            None
        );
    }

    #[test]
    fn drill_on_a_kind_with_a_builtin_drilldown_warns() {
        let (views, warnings) = compile_toml(
            r#"
            [views.pods]
            drill = { kind = "secrets", fields = "metadata.name={name}" }

            [views."apps/v1/deployments"]
            drill = { kind = "pods" }

            [views.helmreleases]
            drill = { kind = "secrets" }
            "#,
        );
        for key in ["pods", "apps/v1/deployments", "helmreleases"] {
            assert_eq!(views[key].drill, None, "{key}");
        }
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("views.\"pods\"") && w.contains("built-in drill-down")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("`enter` on deployments")),
            "{warnings:?}"
        );
    }

    #[test]
    fn refs_validate_and_default() {
        let (views, warnings) = compile_toml(
            r#"
            [views.widgets]
            children = ["Gadgets", ""]

            [[views.widgets.refs]]
            path = "/spec/owner"
            kind = "teams"

            [[views.widgets.refs]]
            path = "spec/owner"
            kind = "teams"

            [[views.widgets.refs]]
            path = "/spec/owner"
            kind = ""

            [[views.widgets.refs]]
            path = "/spec/owner"
            kind = "teams"
            reverse = "everywhere"
            "#,
        );
        let view = &views["widgets"];
        assert_eq!(view.children, ["gadgets"]);
        assert_eq!(view.refs.len(), 1, "{:?}", view.refs);
        let r = &view.refs[0];
        assert_eq!(
            (r.from.as_str(), r.relation.as_str()),
            ("widgets", "references")
        );
        assert_eq!(r.reverse, crate::adjacent::Reverse::Namespace);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings[0].contains("ref 2") && warnings[0].contains("JSON Pointer"),
            "{warnings:?}"
        );
        assert!(
            warnings[1].contains("ref 3") && warnings[1].contains("kind is empty"),
            "{warnings:?}"
        );
        assert!(
            warnings[2].contains("ref 4") && warnings[2].contains("reverse 'everywhere'"),
            "{warnings:?}"
        );
    }

    #[test]
    fn drill_fields_take_the_same_placeholders() {
        let (views, warnings) = compile_toml(
            r#"
            [views.externalsecrets]
            drill = { kind = "secrets", fields = "metadata.name={name},metadata.namespace={namespace}" }

            [views.widgets]
            drill = { kind = "pods", fields = "spec.nodeName={node}" }
            "#,
        );
        let drill = views["externalsecrets"].drill.as_ref().unwrap();
        let es = obj(json!({"metadata": {"name": "db-creds", "namespace": "shop"}}));
        assert_eq!(
            drill.fields_for(&es).as_deref(),
            Some("metadata.name=db-creds,metadata.namespace=shop")
        );
        assert_eq!(drill.labels_for(&es), None);
        assert_eq!(views["widgets"].drill, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("drill.fields"), "{}", warnings[0]);
    }

    #[test]
    fn drill_validates_kind_and_placeholders() {
        let (views, warnings) = compile_toml(
            r#"
            [views.widgets]
            drill = { kind = " ", labels = "app={name}" }

            [views.gadgets]
            drill = { kind = "pods", labels = "owner={uid}" }
            "#,
        );
        assert_eq!(views["widgets"].drill, None);
        assert_eq!(views["gadgets"].drill, None);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("drill.kind is empty")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("placeholder(s) uid")),
            "{warnings:?}"
        );
    }

    /// The borrowed extraction must answer every pointer exactly as the owned
    /// one did — it is the same function now, so this pins the shapes that
    /// actually get borrowed rather than cloned.
    #[test]
    fn borrowed_extraction_matches_the_owned_value_it_replaced() {
        let big: Vec<Value> = (0..100).map(|i| json!({"i": i})).collect();
        let o = obj(json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": {
                "name": "w1",
                "namespace": "team",
                "labels": {"app": "web"},
                "annotations": {"a.example.com/note": "hi", "with/slash": "s"},
                "generation": 7,
            },
            "spec": {"size": 3, "tags": ["a", "b"], "items": big,
                     "nested": {"deep": {"leaf": "found"}}},
            "status": {"phase": "Ready"}
        }));

        // Borrowed straight out of the body: no subtree is cloned to read it.
        assert!(matches!(
            extract_ref(&o, "/spec/items"),
            Some(Extracted::Json(_))
        ));
        assert!(matches!(
            extract_ref(&o, "/spec/nested/deep"),
            Some(Extracted::Json(_))
        ));
        // Borrowed straight out of ObjectMeta / a label map.
        assert!(matches!(
            extract_ref(&o, "/metadata/name"),
            Some(Extracted::Text("w1"))
        ));
        assert!(matches!(
            extract_ref(&o, "/metadata/labels/app"),
            Some(Extracted::Text("web"))
        ));
        assert!(matches!(
            extract_ref(&o, "/metadata/annotations/with~1slash"),
            Some(Extracted::Text("s"))
        ));
        assert!(matches!(
            extract_ref(&o, "/apiVersion"),
            Some(Extracted::Text("example.com/v1"))
        ));
        // Still owned: these have to be built to answer the pointer at all.
        assert!(matches!(
            extract_ref(&o, "/metadata/generation"),
            Some(Extracted::Owned(_))
        ));
        assert!(matches!(
            extract_ref(&o, "/metadata/labels"),
            Some(Extracted::Owned(_))
        ));

        // Whatever the representation, the owned answer is unchanged.
        for pointer in [
            "/spec/items",
            "/spec/nested/deep",
            "/spec/tags/1",
            "/metadata/name",
            "/metadata/namespace",
            "/metadata/labels",
            "/metadata/labels/app",
            "/metadata/annotations/a.example.com~1note",
            "/metadata/annotations/with~1slash",
            "/metadata/generation",
            "/metadata",
            "/apiVersion",
            "/kind",
            "/status/phase",
            // A pointer walking into a string still matches nothing.
            "/metadata/name/nope",
            "/metadata/labels/app/nope",
            "/spec/missing",
        ] {
            let borrowed = extract_ref(&o, pointer).map(Extracted::into_value);
            assert_eq!(borrowed, extract(&o, pointer), "pointer {pointer}");
        }
    }

    /// Rendering and sorting read the borrowed value, so they must produce
    /// what they produced when every extraction was a fresh `Value`.
    #[test]
    fn borrowed_cells_render_and_sort_unchanged() {
        let o = obj(json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": {"name": "w1", "labels": {"replicas": "12"}},
            "spec": {"tags": ["a", "b"], "cpu": "250m", "count": 42},
            "status": {"phase": "Ready"}
        }));
        let now = crate::columns::now_secs();

        assert_eq!(
            render_cell(&o, &col("/metadata/name", ColumnKind::Text), now),
            "w1"
        );
        assert_eq!(
            render_cell(&o, &col("/status/phase", ColumnKind::Text), now),
            "Ready"
        );
        assert_eq!(
            render_cell(&o, &col("/spec/tags", ColumnKind::Text), now),
            r#"["a","b"]"#
        );
        assert_eq!(
            render_cell(&o, &col("/spec/count", ColumnKind::Number), now),
            "42"
        );
        assert_eq!(
            render_cell(&o, &col("/nope", ColumnKind::Text), now),
            "<none>"
        );

        // A quantity and a number reached through a borrowed label string.
        match sort_value(&o, &col("/spec/cpu", ColumnKind::Quantity), now) {
            SortValue::Num(n) => assert_eq!(n, 0.25),
            SortValue::Text(t) => panic!("quantity sorts numerically, got {t}"),
        }
        match sort_value(
            &o,
            &col("/metadata/labels/replicas", ColumnKind::Number),
            now,
        ) {
            SortValue::Num(n) => assert_eq!(n, 12.0),
            SortValue::Text(t) => panic!("number sorts numerically, got {t}"),
        }
        match sort_value(&o, &col("/metadata/name", ColumnKind::Text), now) {
            SortValue::Text(t) => assert_eq!(t, "w1"),
            SortValue::Num(n) => panic!("text sorts as text, got {n}"),
        }
    }

    #[test]
    fn extracts_pointers_across_metadata_typemeta_and_body() {
        let o = obj(json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": {"name": "w1", "labels": {"app": "web"}},
            "spec": {"size": 3, "tags": ["a", "b"]},
            "status": {"phase": "Ready"}
        }));
        assert_eq!(extract(&o, "/status/phase"), Some(json!("Ready")));
        assert_eq!(extract(&o, "/spec/tags/1"), Some(json!("b")));
        assert_eq!(extract(&o, "/metadata/name"), Some(json!("w1")));
        assert_eq!(extract(&o, "/metadata/labels/app"), Some(json!("web")));
        assert_eq!(extract(&o, "/apiVersion"), Some(json!("example.com/v1")));
        assert_eq!(extract(&o, "/kind"), Some(json!("Widget")));
        assert_eq!(extract(&o, "/spec/missing"), None);
        // `/metadataX` must not be mistaken for a metadata pointer.
        assert_eq!(extract(&o, "/metadataX"), None);
    }

    #[test]
    fn typed_metadata_paths_match_objectmeta_serialization() {
        let o = obj(json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": {
                "name": "w1",
                "namespace": "team-a",
                "uid": "uid-1",
                "resourceVersion": "42",
                "generateName": "widget-",
                "generation": 7,
                "creationTimestamp": "2024-01-02T03:04:05Z",
                "deletionTimestamp": "2024-02-03T04:05:06Z",
                "deletionGracePeriodSeconds": 30,
                "labels": {
                    "app.kubernetes.io/name": "web",
                    "plain": "value"
                },
                "annotations": {"til~de/key": "kept"},
                "finalizers": ["example.com/cleanup"],
                "ownerReferences": [{
                    "apiVersion": "apps/v1",
                    "kind": "Deployment",
                    "name": "owner",
                    "uid": "owner-uid"
                }],
                "managedFields": [{"manager": "controller", "operation": "Apply"}]
            }
        }));
        let serialized = serde_json::to_value(&o.metadata).unwrap();
        for pointer in [
            "/metadata",
            "/metadata/name",
            "/metadata/namespace",
            "/metadata/uid",
            "/metadata/resourceVersion",
            "/metadata/generateName",
            "/metadata/generation",
            "/metadata/creationTimestamp",
            "/metadata/deletionTimestamp",
            "/metadata/deletionGracePeriodSeconds",
            "/metadata/labels",
            "/metadata/labels/app.kubernetes.io~1name",
            "/metadata/annotations/til~0de~1key",
            "/metadata/finalizers/0",
            "/metadata/ownerReferences/0/name",
            "/metadata/managedFields/0/manager",
            "/metadata/missing",
            "/metadata/labels/missing",
        ] {
            let rest = pointer.strip_prefix("/metadata").unwrap();
            let expected = if rest.is_empty() {
                Some(serialized.clone())
            } else {
                serialized.pointer(rest).cloned()
            };
            assert_eq!(extract(&o, pointer), expected, "pointer {pointer}");
        }
    }

    #[test]
    fn renders_missing_scalars_and_compound_values() {
        let o = obj(json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "w1"},
            "spec": {"size": 3, "on": true, "tags": ["a"]}
        }));
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/size", ColumnKind::Text),
                crate::columns::now_secs()
            ),
            "3"
        );
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/on", ColumnKind::Text),
                crate::columns::now_secs()
            ),
            "true"
        );
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/tags", ColumnKind::Text),
                crate::columns::now_secs()
            ),
            "[\"a\"]"
        );
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/nope", ColumnKind::Text),
                crate::columns::now_secs()
            ),
            "<none>"
        );
    }

    #[test]
    fn renders_time_as_elapsed_and_future_with_prefix() {
        let past = Timestamp::now().as_second() - 3600;
        let future = Timestamp::now().as_second() + 86_400 * 30;
        let o = obj(json!({
            "apiVersion": "v1", "kind": "W",
            "metadata": {"name": "w"},
            "spec": {
                "past": Timestamp::from_second(past).unwrap().to_string(),
                "future": Timestamp::from_second(future).unwrap().to_string(),
                "junk": "not-a-time"
            }
        }));
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/past", ColumnKind::Time),
                crate::columns::now_secs()
            ),
            "1h"
        );
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/future", ColumnKind::Time),
                crate::columns::now_secs()
            ),
            "in 30d"
        );
        assert_eq!(
            render_cell(
                &o,
                &col("/spec/junk", ColumnKind::Time),
                crate::columns::now_secs()
            ),
            "not-a-time"
        );
    }

    #[test]
    fn parses_quantities_by_value() {
        assert_eq!(parse_quantity("500m"), Some(0.5));
        assert_eq!(parse_quantity("2"), Some(2.0));
        assert_eq!(parse_quantity("1Gi"), Some(1024.0 * 1024.0 * 1024.0));
        assert_eq!(parse_quantity("2k"), Some(2000.0));
        let nanos = parse_quantity("100n").unwrap();
        assert!((nanos - 1e-7).abs() < 1e-12);
        assert_eq!(parse_quantity("nope"), None);
    }

    fn num(sv: SortValue) -> f64 {
        match sv {
            SortValue::Num(n) => n,
            SortValue::Text(t) => panic!("expected Num, got Text({t})"),
        }
    }

    #[test]
    fn quantities_numbers_and_times_sort_by_value_not_lexically() {
        let o = obj(json!({
            "apiVersion": "v1", "kind": "W",
            "metadata": {"name": "w"},
            "spec": {
                "small": "500m", "big": "1Gi",
                "nine": 9, "ten": "10",
                "old": "2020-01-01T00:00:00Z", "new": "2030-01-01T00:00:00Z"
            }
        }));
        // Lexically "1Gi" < "500m" — by value it must be the other way.
        let q = |p: &str| {
            num(sort_value(
                &o,
                &col(p, ColumnKind::Quantity),
                crate::columns::now_secs(),
            ))
        };
        assert!(q("/spec/small") < q("/spec/big"));
        // Lexically "10" < "9".
        let n = |p: &str| {
            num(sort_value(
                &o,
                &col(p, ColumnKind::Number),
                crate::columns::now_secs(),
            ))
        };
        assert!(n("/spec/nine") < n("/spec/ten"));
        // Ascending time = most recent first (smaller elapsed).
        let t = |p: &str| {
            num(sort_value(
                &o,
                &col(p, ColumnKind::Time),
                crate::columns::now_secs(),
            ))
        };
        assert!(t("/spec/new") < t("/spec/old"));
        // Missing values sort last in ascending order.
        assert_eq!(q("/spec/missing"), f64::MAX);
        assert_eq!(n("/spec/missing"), f64::MAX);
        assert_eq!(t("/spec/missing"), f64::MAX);
    }

    #[test]
    fn converts_simple_json_paths_to_pointers() {
        assert_eq!(
            json_path_to_pointer(".status.phase"),
            Some("/status/phase".into())
        );
        assert_eq!(
            json_path_to_pointer(".spec.ports[0].port"),
            Some("/spec/ports/0/port".into())
        );
        assert_eq!(
            json_path_to_pointer("{.metadata.name}"),
            Some("/metadata/name".into())
        );
        assert_eq!(json_path_to_pointer(".spec.a~b"), Some("/spec/a~0b".into()));
        // Filters, wildcards, recursion: not representable as pointers.
        assert_eq!(
            json_path_to_pointer(r#".status.conditions[?(@.type=="Ready")].status"#),
            None
        );
        assert_eq!(json_path_to_pointer(".spec.containers[*].image"), None);
        assert_eq!(json_path_to_pointer("..name"), None);
        assert_eq!(json_path_to_pointer("status.phase"), None);
    }

    #[test]
    fn builds_printer_column_view_from_crd() {
        let crd = json!({
            "spec": {
                "group": "example.com",
                "versions": [
                    {"name": "v1alpha1", "served": true},
                    {"name": "v1", "served": true, "storage": true,
                     "additionalPrinterColumns": [
                        {"name": "Phase", "type": "string", "jsonPath": ".status.phase"},
                        {"name": "Replicas", "type": "integer", "jsonPath": ".spec.replicas"},
                        {"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"},
                        {"name": "Detail", "type": "string", "priority": 1,
                         "jsonPath": ".status.message"},
                        {"name": "Ready", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type=='Ready')].status"},
                        {"name": "Reason", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type=='Ready')].reason"},
                        {"name": "Message", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type=='Ready')].message"},
                        {"name": "Since", "type": "date",
                         "jsonPath": ".status.conditions[?(@.type=='Ready')].lastTransitionTime"},
                        {"name": "Skipped", "type": "string",
                         "jsonPath": ".status.items[*].name"}
                     ]}
                ]
            }
        });
        let view = printer_columns_view(&crd, "v1").unwrap();
        assert!(!view.replace);
        let headers: Vec<&str> = view.columns.iter().map(|c| c.header.as_str()).collect();
        // Condition filters translate to a by-name lookup of the selected
        // field; only the genuinely untranslatable wildcard column is skipped.
        assert_eq!(
            headers,
            vec![
                "PHASE", "REPLICAS", "AGE", "DETAIL", "READY", "REASON", "MESSAGE", "SINCE"
            ]
        );
        assert_eq!(view.columns[0].kind, ColumnKind::Text);
        assert_eq!(view.columns[1].kind, ColumnKind::Number);
        assert_eq!(view.columns[2].kind, ColumnKind::Time);
        assert!(view.columns[3].wide, "priority>0 becomes wide-only");
        assert_eq!(view.columns[4].kind, ColumnKind::Condition);
        assert_eq!(view.columns[4].pointer, "Ready");
        assert_eq!(view.columns[4].condition_field, None);
        assert_eq!(view.columns[5].kind, ColumnKind::Text);
        assert_eq!(view.columns[5].pointer, "Ready");
        assert_eq!(view.columns[5].condition_field.as_deref(), Some("reason"));
        assert_eq!(view.columns[6].kind, ColumnKind::Text);
        assert_eq!(view.columns[6].condition_field.as_deref(), Some("message"));
        assert_eq!(view.columns[7].kind, ColumnKind::Time);
        assert_eq!(
            view.columns[7].condition_field.as_deref(),
            Some("lastTransitionTime")
        );
        // The version without printer columns yields nothing.
        assert!(printer_columns_view(&crd, "v1alpha1").is_none());
        assert!(printer_columns_view(&crd, "v9").is_none());
    }

    #[test]
    fn status_filters_read_first_available_output() {
        for quote in ["'", "\""] {
            for status in ["True", "False", "Unknown"] {
                let path =
                    format!("{{.status.conditions[?(@.status=={quote}{status}{quote})].reason}}");
                assert_eq!(
                    condition_json_path(&path),
                    Some((ConditionMatch::Status, status.into(), "reason".into()))
                );
                let crd = json!({"spec": {"versions": [{"name": "v1", "additionalPrinterColumns": [
                    {"name": "Reason", "type": "string", "jsonPath": path}
                ]}]}});
                let view = printer_columns_view(&crd, "v1").unwrap();
                let col = &view.columns[0];
                let object = obj(json!({"status": {"conditions": [
                    {"status": "Other", "reason": "Wrong"},
                    {"status": status},
                    {"status": status, "reason": "First"},
                    {"status": status, "reason": "Second"}
                ]}}));
                assert_eq!(render_cell(&object, col, 0), "First");
                assert!(matches!(sort_value(&object, col, 0), SortValue::Text(t) if t == "first"));
                for conditions in [
                    json!([]),
                    json!([{"status": "Other", "reason": "Wrong"}]),
                    json!([{"status": status}]),
                    Value::Null,
                ] {
                    let object = obj(json!({"status": {"conditions": conditions}}));
                    assert_eq!(render_cell(&object, col, 0), "<none>");
                }
            }
        }
        for path in [
            ".status.conditions[?(@.reason=='Ready')].type",
            ".status.conditions[?(@.status!='True')].type",
            ".status.conditions[?(@.status=='True')].foo.bar",
            ".status.conditions[?(@.status=='True')].*",
        ] {
            assert_eq!(condition_json_path(path), None);
        }
    }

    #[test]
    fn printer_columns_keep_condition_reason_and_message() {
        let crd = json!({
            "spec": {
                "versions": [{
                    "name": "v1", "served": true, "storage": true,
                    "additionalPrinterColumns": [
                        {"name": "A", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type==\"Ready\")].status"},
                        {"name": "B", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type==\"Ready\")].reason"},
                        {"name": "C", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type==\"Ready\")].message"}
                    ]
                }]
            }
        });
        let view = printer_columns_view(&crd, "v1").unwrap();
        let headers: Vec<&str> = view.columns.iter().map(|c| c.header.as_str()).collect();
        assert_eq!(headers, vec!["A", "B", "C"]);

        let obj = obj(json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "w"},
            "status": {"conditions": [
                {"type": "Available", "status": "True"},
                {
                    "type": "Ready",
                    "status": "False",
                    "reason": "DependencyNotReady",
                    "message": "waiting on source"
                }
            ]}
        }));
        assert_eq!(
            render_cell(&obj, &view.columns[0], crate::columns::now_secs()),
            "False"
        );
        assert_eq!(
            render_cell(&obj, &view.columns[1], crate::columns::now_secs()),
            "DependencyNotReady"
        );
        assert_eq!(
            render_cell(&obj, &view.columns[2], crate::columns::now_secs()),
            "waiting on source"
        );
        match sort_value(&obj, &view.columns[1], crate::columns::now_secs()) {
            SortValue::Text(t) => assert_eq!(t, "dependencynotready"),
            SortValue::Num(_) => panic!("reason sorts as text"),
        }

        let spec = crate::columns::build_spec("example.com", "widgets", None, Some(&view), false);
        assert_eq!(spec.headers(), vec!["NAME", "A", "B", "C", "AGE"]);
        let (cells, status_idx) = spec.cells(&obj, crate::columns::now_secs());
        assert_eq!(
            &cells[1..4],
            ["False", "DependencyNotReady", "waiting on source"]
        );
        // Only the status field colors the row.
        assert_eq!(status_idx, Some(1));
    }
}
