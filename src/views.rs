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
    /// JSON Pointer into the object.
    pub pointer: String,
    pub kind: ColumnKind,
    /// Shown only in wide mode (`w`).
    pub wide: bool,
    pub width: Option<u16>,
    pub align: Option<Align>,
}

/// A compiled per-kind view.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub columns: Vec<UserColumn>,
    /// Initial sort: (header, descending).
    pub sort: Option<(String, bool)>,
    /// Replace the curated columns entirely instead of overlaying them.
    pub replace: bool,
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
            if !c.path.starts_with('/') {
                warnings.push(format!(
                    "views.\"{key}\": column {header}: path '{}' is not a JSON Pointer \
                     (must start with '/', e.g. /status/phase); column skipped",
                    c.path
                ));
                continue;
            }
            let kind = match c.kind.as_deref() {
                None | Some("text") => ColumnKind::Text,
                Some("status") => ColumnKind::Status,
                Some("number") => ColumnKind::Number,
                Some("quantity") => ColumnKind::Quantity,
                Some("time") => ColumnKind::Time,
                Some(other) => {
                    warnings.push(format!(
                        "views.\"{key}\": column {header}: unknown type '{other}' \
                         (expected text/status/number/quantity/time); using text"
                    ));
                    ColumnKind::Text
                }
            };
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
                pointer: c.path.clone(),
                kind,
                wide: c.wide,
                width: c.width,
                align,
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
        views.insert(
            key.to_lowercase(),
            View {
                columns,
                sort,
                replace,
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

/// Find the view for a resource, most specific key first:
/// `apiVersion/plural`, `group/plural`, plural, then lowercased kind.
pub fn lookup<'a>(views: &'a HashMap<String, View>, ar: &ApiResource) -> Option<&'a View> {
    let plural = ar.plural.to_lowercase();
    let mut keys = vec![format!("{}/{plural}", ar.api_version.to_lowercase())];
    if !ar.group.is_empty() {
        keys.push(format!("{}/{plural}", ar.group.to_lowercase()));
    }
    keys.push(plural);
    keys.push(ar.kind.to_lowercase());
    keys.iter().find_map(|k| views.get(k))
}

/// Resolve a JSON Pointer against the object as served by the API:
/// `/metadata/…` and `/apiVersion`/`/kind` come from the typed fields, the
/// rest from the body (`DynamicObject::data` holds spec/status/…).
pub fn extract(obj: &DynamicObject, pointer: &str) -> Option<Value> {
    if let Some(rest) = pointer.strip_prefix("/metadata")
        && (rest.is_empty() || rest.starts_with('/'))
    {
        let meta = serde_json::to_value(&obj.metadata).ok()?;
        return if rest.is_empty() {
            Some(meta)
        } else {
            meta.pointer(rest).cloned()
        };
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

/// Render one custom column's cell. Missing values read as `<none>`.
pub fn render_cell(obj: &DynamicObject, col: &UserColumn) -> String {
    let Some(v) = extract(obj, &col.pointer) else {
        return "<none>".into();
    };
    match col.kind {
        ColumnKind::Time => render_time(&v),
        _ => render_value(&v),
    }
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
    let v = extract(obj, &col.pointer);
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
        ColumnKind::Text | ColumnKind::Status => SortValue::Text(
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
/// resources without an explicit user view. Columns whose JSONPath can't be
/// expressed as a JSON Pointer (filters, wildcards) are skipped; columns with
/// `priority > 0` become wide-only, matching kubectl's `-o wide`.
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
            let pointer = json_path_to_pointer(c.get("jsonPath").and_then(Value::as_str)?)?;
            let kind = match c.get("type").and_then(Value::as_str) {
                Some("integer" | "number") => ColumnKind::Number,
                Some("date") => ColumnKind::Time,
                _ => ColumnKind::Text,
            };
            let wide = c.get("priority").and_then(Value::as_i64).unwrap_or(0) > 0;
            Some(UserColumn {
                header: name.to_uppercase(),
                pointer,
                kind,
                wide,
                width: None,
                align: None,
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
        })
    }
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
        }
    }

    fn compile_toml(text: &str) -> (HashMap<String, View>, Vec<String>) {
        let cfg: crate::config::Config = toml::from_str(text).unwrap();
        compile(&cfg.views)
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
                        {"name": "Skipped", "type": "string",
                         "jsonPath": ".status.conditions[?(@.type=='Ready')].status"}
                     ]}
                ]
            }
        });
        let view = printer_columns_view(&crd, "v1").unwrap();
        assert!(!view.replace);
        let headers: Vec<&str> = view.columns.iter().map(|c| c.header.as_str()).collect();
        // The filter-expression column is skipped, the rest map over.
        assert_eq!(headers, vec!["PHASE", "REPLICAS", "AGE", "DETAIL"]);
        assert_eq!(view.columns[0].kind, ColumnKind::Text);
        assert_eq!(view.columns[1].kind, ColumnKind::Number);
        assert_eq!(view.columns[2].kind, ColumnKind::Time);
        assert!(view.columns[3].wide, "priority>0 becomes wide-only");
        // The version without printer columns yields nothing.
        assert!(printer_columns_view(&crd, "v1alpha1").is_none());
        assert!(printer_columns_view(&crd, "v9").is_none());
    }
}
