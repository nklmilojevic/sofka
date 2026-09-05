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
    #[default]
    Text,
    /// Text that also drives the row's status coloring.
    Status,
    /// Sorted numerically.
    Number,
    /// Kubernetes quantity (`500m`, `1Gi`, `2k`) — sorted by value.
    Quantity,
    /// RFC 3339 timestamp — rendered as compact elapsed time (`3d4h`, or
    /// `in 30d` for the future), sorted by the timestamp.
    Time,
    /// A `status.conditions` entry looked up **by its `type` name** (held in
    /// [`UserColumn::pointer`]) instead of a fragile array index — condition
    /// order isn't guaranteed by anything. Renders the condition's `status`
    /// (`True`/`False`/`Unknown`) and drives row coloring like `Status`.
    Condition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// One compiled custom column.
#[derive(Debug, Clone)]
pub struct UserColumn {
    /// Header, uppercased for display.
    pub header: String,
    /// JSON Pointer into the object — except when this column looks up a
    /// `status.conditions` entry by `type` name, where it's the condition
    /// type (`Ready`, `Available`, …). That happens for
    /// [`ColumnKind::Condition`] and for printer columns whose JSONPath
    /// selected a non-status field of a named condition (see
    /// [`Self::condition_field`]).
    pub pointer: String,
    pub kind: ColumnKind,
    /// Shown only in wide mode (`w`).
    pub wide: bool,
    pub width: Option<u16>,
    pub align: Option<Align>,
    /// When set, [`Self::pointer`] is a condition `type` name and this is
    /// the field to read from that condition (`reason`, `message`, …).
    /// Used by CRD printer columns like
    /// `.status.conditions[?(@.type=="Ready")].message`.
    /// [`ColumnKind::Condition`] always reads `status` and leaves this `None`.
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
    "services",
    "pods",
    "customresourcedefinitions",
    "helm",
    "helmhistory",
    "helmreleases",
];

/// The plural a view key names: the last segment of `apiVersion/plural`,
/// `group/plural`, or a bare plural. A key that is a lowercased kind (`pod`)
/// isn't a plural and won't match anything here.
fn key_plural(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
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
    pick: impl Fn(&'a View) -> Option<&'a T>,
) -> Option<&'a T> {
    lookup_keys(ar)
        .into_iter()
        .find_map(|k| views.get(&k).and_then(&pick))
}

/// Where `enter` drills for a kind, per `[views."…"].drill`. Kinds with a
/// built-in drill-down (workloads to pods, CRDs to their resources, …) never
/// consult this.
pub fn drill_for<'a>(views: &'a HashMap<String, View>, ar: &ApiResource) -> Option<&'a Drill> {
    setting(views, ar, |v| v.drill.as_ref())
}

/// The pointer to a kind's node name: an explicit `[views."…"].node` wins over
/// the built-in table.
pub fn node_pointer<'a>(views: &'a HashMap<String, View>, ar: &ApiResource) -> Option<&'a str> {
    if let Some(pointer) = setting(views, ar, |v| v.node.as_deref()) {
        return Some(pointer);
    }
    lookup_keys(ar).into_iter().find_map(|key| {
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
        let mut columns = Vec::new();
        for c in &cfg.columns {
            let header = c.name.trim().to_uppercase();
            if header.is_empty() {
                warnings.push(format!("views.\"{key}\": column with empty name skipped"));
                continue;
            }
            let kind = match c.kind.as_deref() {
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
            } else if !c.path.starts_with('/') {
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
                pointer: c.path.trim().to_string(),
                kind,
                wide: c.wide,
                width: c.width,
                align,
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
            let key_lc = key.to_lowercase();
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
        views.insert(
            key.to_lowercase(),
            View {
                columns,
                sort,
                replace,
                node,
                drill,
            },
        );
    }
    (views, warnings)
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
fn lookup_keys(ar: &ApiResource) -> Vec<String> {
    let plural = ar.plural.to_lowercase();
    let mut keys = vec![format!("{}/{plural}", ar.api_version.to_lowercase())];
    if !ar.group.is_empty() {
        keys.push(format!("{}/{plural}", ar.group.to_lowercase()));
    }
    keys.push(plural);
    keys.push(ar.kind.to_lowercase());
    keys
}

/// Find the view for a resource, most specific key first.
pub fn lookup<'a>(views: &'a HashMap<String, View>, ar: &ApiResource) -> Option<&'a View> {
    lookup_keys(ar).into_iter().find_map(|k| views.get(&k))
}

/// Resolve a JSON Pointer against the object as served by the API:
/// `/metadata/…` and `/apiVersion`/`/kind` come from the typed fields, the
/// rest from the body (`DynamicObject::data` holds spec/status/…).
pub fn extract(obj: &DynamicObject, pointer: &str) -> Option<Value> {
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
                .map(|t| Value::String(t.api_version.clone()));
        }
        "/kind" => return obj.types.as_ref().map(|t| Value::String(t.kind.clone())),
        _ => {}
    }
    obj.data.pointer(pointer).cloned()
}

/// Resolve metadata without serializing the entire `ObjectMeta` for every
/// custom-column cell. Complex or whole-metadata requests still serialize the
/// selected value, preserving the exact JSON Pointer behavior and output.
fn extract_metadata(meta: &kube::core::ObjectMeta, rest: &str) -> Option<Value> {
    if rest.is_empty() {
        return serde_json::to_value(meta).ok();
    }
    let path = rest.strip_prefix('/')?;
    let field = path.split_once('/').map_or(path, |(field, _)| field);
    let tail = &path[field.len()..];

    macro_rules! selected {
        ($value:expr) => {{
            let value = serde_json::to_value($value).ok()?;
            if tail.is_empty() {
                Some(value)
            } else {
                value.pointer(tail).cloned()
            }
        }};
    }

    match field {
        "name" => selected!(meta.name.as_ref()?),
        "namespace" => selected!(meta.namespace.as_ref()?),
        "uid" => selected!(meta.uid.as_ref()?),
        "resourceVersion" => selected!(meta.resource_version.as_ref()?),
        "generateName" => selected!(meta.generate_name.as_ref()?),
        "selfLink" => selected!(meta.self_link.as_ref()?),
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

fn extract_string_map(
    map: &std::collections::BTreeMap<String, String>,
    tail: &str,
) -> Option<Value> {
    if tail.is_empty() {
        return serde_json::to_value(map).ok();
    }
    let token = tail.strip_prefix('/')?;
    if token.contains('/') {
        return None;
    }
    // JSON Pointer unescapes `~1` before `~0`; doing so in this order also
    // preserves the RFC-defined meaning of tokens such as `~01`.
    if token.contains('~') {
        let key = token.replace("~1", "/").replace("~0", "~");
        map.get(&key).cloned().map(Value::String)
    } else {
        map.get(token).cloned().map(Value::String)
    }
}

/// Render one custom column's cell. Missing values read as `<none>`.
pub fn render_cell(obj: &DynamicObject, col: &UserColumn) -> String {
    if col.kind == ColumnKind::Condition {
        return condition_status(obj, &col.pointer).unwrap_or_else(|| "<none>".into());
    }
    let Some(v) = cell_value(obj, col) else {
        return "<none>".into();
    };
    match col.kind {
        ColumnKind::Time => render_time(&v),
        _ => render_value(&v),
    }
}

/// Value of a non-`Condition` column: a JSON Pointer extract, or a named
/// field of a `status.conditions` entry looked up by type.
fn cell_value(obj: &DynamicObject, col: &UserColumn) -> Option<Value> {
    match col.condition_field.as_deref() {
        Some(field) => condition_value(obj, &col.pointer, field),
        None => extract(obj, &col.pointer),
    }
}

/// The `status` of the `status.conditions` entry whose `type` is `cond_type`,
/// found by name — array order isn't guaranteed by anything.
pub fn condition_status(obj: &DynamicObject, cond_type: &str) -> Option<String> {
    condition_value(obj, cond_type, "status")?
        .as_str()
        .map(str::to_string)
}

/// One field of the `status.conditions` entry whose `type` is `cond_type`,
/// found by name — array order isn't guaranteed by anything.
fn condition_value(obj: &DynamicObject, cond_type: &str, field: &str) -> Option<Value> {
    obj.data
        .pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(cond_type))?
        .get(field)
        .cloned()
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
fn render_time(v: &Value) -> String {
    let Some(s) = v.as_str() else {
        return render_value(v);
    };
    match s.parse::<Timestamp>() {
        Ok(ts) => {
            let delta = Timestamp::now().as_second() - ts.as_second();
            if delta >= 0 {
                crate::columns::humanize(delta)
            } else {
                format!("in {}", crate::columns::humanize(-delta))
            }
        }
        Err(_) => s.to_string(),
    }
}

/// Comparable value of a custom column's cell: numbers, quantities, and times
/// sort by value (missing/unparseable last in ascending order), text sorts
/// case-insensitively.
pub fn sort_value(obj: &DynamicObject, col: &UserColumn) -> SortValue {
    if col.kind == ColumnKind::Condition {
        return SortValue::Text(
            condition_status(obj, &col.pointer)
                .unwrap_or_default()
                .to_lowercase(),
        );
    }
    let v = cell_value(obj, col);
    match col.kind {
        ColumnKind::Number => SortValue::Num(v.as_ref().and_then(number_of).unwrap_or(f64::MAX)),
        ColumnKind::Quantity => {
            SortValue::Num(v.as_ref().and_then(quantity_of).unwrap_or(f64::MAX))
        }
        // Elapsed seconds, like AGE: ascending = most recent (or furthest in
        // the future) first, unknowns last.
        ColumnKind::Time => SortValue::Num(
            v.as_ref()
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<Timestamp>().ok())
                .map(|ts| (Timestamp::now().as_second() - ts.as_second()) as f64)
                .unwrap_or(f64::MAX),
        ),
        // Condition is handled above (its "pointer" is a condition name, not
        // something `extract` understands).
        ColumnKind::Text | ColumnKind::Status | ColumnKind::Condition => SortValue::Text(
            v.as_ref()
                .map(render_value)
                .unwrap_or_default()
                .to_lowercase(),
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

/// Build a fallback view from a CRD's `additionalPrinterColumns` for one
/// served `version` — the automatic upgrade over NAME/AGE for custom
/// resources without an explicit user view. Condition-filter JSONPath of
/// the form `.status.conditions[?(@.type=="Ready")].field` is translated
/// into a by-name lookup of that field; other filters and wildcards are
/// skipped. Columns with `priority > 0` become wide-only, matching
/// kubectl's `-o wide`.
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
            // `.status.conditions[?(@.type=="Ready")].status` (and the same
            // filter for `.reason` / `.message` / other simple fields) used
            // to be dropped as untranslatable. It's a condition lookup by
            // name. Only the `status` field is a coloring `Condition`
            // column; the rest keep the CRD's declared type.
            if let Some((cond, field)) = condition_json_path(json_path) {
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
        })
    }
}

/// Recognize a condition-lookup JSONPath —
/// `.status.conditions[?(@.type=="Ready")].status` (single or double quotes),
/// and the same filter for `.reason` / `.message` / other simple fields —
/// and return `(condition type, field)`. Anything else is `None`.
pub fn condition_json_path(path: &str) -> Option<(String, String)> {
    let p = path.trim();
    let p = p
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(p);
    let rest = p.strip_prefix(".status.conditions[?(@.type==")?;
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
    (!t.is_empty() && !t.contains(['"', '\''])).then(|| (t.to_string(), field.to_string()))
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

    fn col(pointer: &str, kind: ColumnKind) -> UserColumn {
        UserColumn {
            header: "COL".into(),
            pointer: pointer.into(),
            kind,
            wide: false,
            width: None,
            align: None,
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
        assert_eq!(render_cell(&obj, col), "False");
        match sort_value(&obj, col) {
            SortValue::Text(t) => assert_eq!(t, "false"),
            SortValue::Num(_) => panic!("conditions sort as text"),
        }
        // A missing condition reads as <none>, not a panic or a lie.
        let bare: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1", "kind": "Certificate",
            "metadata": {"name": "new"}
        }))
        .unwrap();
        assert_eq!(render_cell(&bare, col), "<none>");
    }

    #[test]
    fn condition_json_path_recognizes_the_canonical_filter() {
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].status"#),
            Some(("Ready".into(), "status".into()))
        );
        assert_eq!(
            condition_json_path(".status.conditions[?(@.type=='Available')].status"),
            Some(("Available".into(), "status".into()))
        );
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].reason"#),
            Some(("Ready".into(), "reason".into()))
        );
        assert_eq!(
            condition_json_path(r#".status.conditions[?(@.type=="Ready")].message"#),
            Some(("Ready".into(), "message".into()))
        );
        assert_eq!(
            condition_json_path("{.status.conditions[?(@.type=='Ready')].lastTransitionTime}"),
            Some(("Ready".into(), "lastTransitionTime".into()))
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
            lookup(&views, &ar).unwrap().sort,
            Some(("BY-KIND".into(), false))
        );
        views.insert("certificates".to_string(), mk("by-plural"));
        assert_eq!(
            lookup(&views, &ar).unwrap().sort,
            Some(("BY-PLURAL".into(), false))
        );
        views.insert("cert-manager.io/certificates".to_string(), mk("by-group"));
        assert_eq!(
            lookup(&views, &ar).unwrap().sort,
            Some(("BY-GROUP".into(), false))
        );
        views.insert("cert-manager.io/v1/certificates".to_string(), mk("by-gvr"));
        assert_eq!(
            lookup(&views, &ar).unwrap().sort,
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
            node_pointer(&views, &ar("", "Pod", "pods")),
            Some("/spec/nodeName")
        );
        let claims = ar("karpenter.sh", "NodeClaim", "nodeclaims");
        assert_eq!(node_pointer(&views, &claims), Some("/status/nodeName"));
        // A kind the table doesn't list names no node until config says where.
        let pools = ar("karpenter.sh", "NodePool", "nodepools");
        assert_eq!(node_pointer(&views, &pools), None);
        // Rows are scoped to their group: a same-named plural elsewhere
        // (or PodMetrics, whose plural is also `pods`) gets nothing.
        let other_claims = ar("example.com", "NodeClaim", "nodeclaims");
        assert_eq!(node_pointer(&views, &other_claims), None);
        let pod_metrics = ar("metrics.k8s.io", "PodMetrics", "pods");
        assert_eq!(node_pointer(&views, &pod_metrics), None);

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
            node_pointer(&configured, &claims),
            Some("/status/providerID")
        );
        assert_eq!(node_pointer(&configured, &pools), Some("/status/host"));
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
        assert_eq!(lookup(&views, &widgets).unwrap().node, None);
        assert_eq!(node_pointer(&views, &widgets), Some("/status/host"));
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
        assert_eq!(node_pointer(&views, &pods), Some("/status/hostName"));
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
        let drill = drill_for(&views, &pools).expect("drill configured");
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
        assert_eq!(drill_for(&views, &pools).unwrap().labels_for(&pool), None);
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
        assert_eq!(render_cell(&o, &col("/spec/size", ColumnKind::Text)), "3");
        assert_eq!(render_cell(&o, &col("/spec/on", ColumnKind::Text)), "true");
        assert_eq!(
            render_cell(&o, &col("/spec/tags", ColumnKind::Text)),
            "[\"a\"]"
        );
        assert_eq!(
            render_cell(&o, &col("/spec/nope", ColumnKind::Text)),
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
        assert_eq!(render_cell(&o, &col("/spec/past", ColumnKind::Time)), "1h");
        assert_eq!(
            render_cell(&o, &col("/spec/future", ColumnKind::Time)),
            "in 30d"
        );
        assert_eq!(
            render_cell(&o, &col("/spec/junk", ColumnKind::Time)),
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
        let q = |p: &str| num(sort_value(&o, &col(p, ColumnKind::Quantity)));
        assert!(q("/spec/small") < q("/spec/big"));
        // Lexically "10" < "9".
        let n = |p: &str| num(sort_value(&o, &col(p, ColumnKind::Number)));
        assert!(n("/spec/nine") < n("/spec/ten"));
        // Ascending time = most recent first (smaller elapsed).
        let t = |p: &str| num(sort_value(&o, &col(p, ColumnKind::Time)));
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
        assert_eq!(render_cell(&obj, &view.columns[0]), "False");
        assert_eq!(render_cell(&obj, &view.columns[1]), "DependencyNotReady");
        assert_eq!(render_cell(&obj, &view.columns[2]), "waiting on source");
        match sort_value(&obj, &view.columns[1]) {
            SortValue::Text(t) => assert_eq!(t, "dependencynotready"),
            SortValue::Num(_) => panic!("reason sorts as text"),
        }

        let spec = crate::columns::build_spec("widgets", None, Some(&view), false);
        assert_eq!(spec.headers(), vec!["NAME", "A", "B", "C", "AGE"]);
        let (cells, status_idx) = spec.cells(&obj);
        assert_eq!(
            &cells[1..4],
            ["False", "DependencyNotReady", "waiting on source"]
        );
        // Only the status field colors the row.
        assert_eq!(status_idx, Some(1));
    }
}
