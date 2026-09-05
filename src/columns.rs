//! Per-kind column definitions and cell extraction from DynamicObjects.
//!
//! This is the "render" layer of the original k9s, reimagined: instead of a
//! hand-written renderer per resource, known kinds get curated columns and
//! everything else falls back to NAME/AGE pulled from metadata.

use std::cell::OnceCell;

use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use serde_json::Value;

type CellFn = for<'a> fn(&CellContext<'a>) -> String;

struct Column {
    header: &'static str,
    extract: CellFn,
    is_status: bool,
    /// Only shown in wide mode (`w`), like kubectl's `-o wide` extras.
    wide: bool,
}

/// Per-row scratch space shared by every column extractor for that row.
///
/// The derived values are lazy and computed at most once per row, because
/// several columns are backed by the *same* expensive derivation: READY,
/// STATUS and RESTARTS all come from `pod_summary`, and five Helm columns all
/// come from `helm::decode` (base64 x2 + gunzip + a full JSON parse). Eagerly
/// per column, a Helm row cost ~134 us against ~3 us for a pod row; the whole
/// difference was decoding the same Secret five times and discarding four.
///
/// `age` is lazy for the same reason in reverse — most rows are rendered by
/// column sets that never read it, and it costs a `Timestamp::now()` plus a
/// formatted `String`.
struct CellContext<'a> {
    obj: &'a DynamicObject,
    data: &'a Value,
    name: &'a str,
    age: OnceCell<String>,
    pod: OnceCell<(String, String, String)>,
    helm: OnceCell<Option<crate::helm::Summary>>,
}

impl<'a> CellContext<'a> {
    fn new(obj: &'a DynamicObject) -> Self {
        CellContext {
            obj,
            data: &obj.data,
            name: obj.metadata.name.as_deref().unwrap_or_default(),
            age: OnceCell::new(),
            pod: OnceCell::new(),
            helm: OnceCell::new(),
        }
    }

    fn age(&self) -> &str {
        self.age.get_or_init(|| age(self.obj))
    }

    /// `(READY, STATUS, RESTARTS)` — one walk of `containerStatuses` for all
    /// three columns.
    fn pod(&self) -> &(String, String, String) {
        self.pod.get_or_init(|| pod_summary(self.obj))
    }

    /// The decoded Helm release backing this row's Secret, decoded once.
    fn helm(&self) -> Option<&crate::helm::Summary> {
        self.helm
            .get_or_init(|| crate::helm::decode_summary(self.obj))
            .as_ref()
    }
}

const fn column(header: &'static str, extract: CellFn) -> Column {
    Column {
        header,
        extract,
        is_status: false,
        wide: false,
    }
}

const fn status_column(header: &'static str, extract: CellFn) -> Column {
    Column {
        header,
        extract,
        is_status: true,
        wide: false,
    }
}

const fn wide_column(header: &'static str, extract: CellFn) -> Column {
    Column {
        header,
        extract,
        is_status: false,
        wide: true,
    }
}

const POD_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("READY", col_pod_ready),
    status_column("STATUS", col_pod_status),
    column("RESTARTS", col_pod_restarts),
    wide_column("IP", col_pod_ip),
    wide_column("NODE", col_pod_node),
    column("AGE", col_age),
];

const DEPLOYMENT_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("READY", col_deploy_ready),
    status_column("STATUS", col_deploy_status),
    column("UP-TO-DATE", col_deploy_updated),
    column("AVAILABLE", col_deploy_available),
    column("AGE", col_age),
];

const REPLICASET_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("DESIRED", col_rs_desired),
    column("CURRENT", col_rs_current),
    column("READY", col_rs_ready),
    status_column("STATUS", col_rs_status),
    column("AGE", col_age),
];

const STATEFULSET_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("READY", col_sts_ready),
    status_column("STATUS", col_sts_status),
    column("AGE", col_age),
];

const DAEMONSET_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("DESIRED", col_ds_desired),
    column("CURRENT", col_ds_current),
    column("READY", col_ds_ready),
    column("AVAILABLE", col_ds_available),
    status_column("STATUS", col_ds_status),
    column("AGE", col_age),
];

const SERVICE_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("TYPE", col_service_type),
    column("CLUSTER-IP", col_service_cluster_ip),
    column("EXTERNAL-IP", col_service_external_ip),
    column("PORTS", col_service_ports),
    column("AGE", col_age),
];

const NODE_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    status_column("STATUS", col_node_status),
    column("ROLES", col_node_roles),
    column("TAINTS", col_node_taints),
    column("VERSION", col_node_version),
    column("AGE", col_age),
];

const NAMESPACE_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    status_column("STATUS", col_namespace_status),
    column("AGE", col_age),
];

const CONFIGMAP_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("DATA", col_configmap_data),
    column("AGE", col_age),
];

const SECRET_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("TYPE", col_secret_type),
    column("DATA", col_secret_data),
    column("AGE", col_age),
];

const JOB_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("COMPLETIONS", col_job_completions),
    column("DURATION", col_job_duration),
    column("AGE", col_age),
];

const CRONJOB_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("SCHEDULE", col_cronjob_schedule),
    column("SUSPEND", col_cronjob_suspend),
    column("ACTIVE", col_cronjob_active),
    column("LAST-SCHEDULE", col_cronjob_last_schedule),
    column("AGE", col_age),
];

const EVENT_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("TYPE", col_event_type),
    column("REASON", col_event_reason),
    column("OBJECT", col_event_object),
    column("MESSAGE", col_event_message),
    column("COUNT", col_event_count),
    column("AGE", col_age),
];

const HPA_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("REFERENCE", col_hpa_reference),
    column("TARGETS", col_hpa_targets),
    column("MINPODS", col_hpa_minpods),
    column("MAXPODS", col_hpa_maxpods),
    column("REPLICAS", col_hpa_replicas),
    column("AGE", col_age),
];

const PVC_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    status_column("STATUS", col_pvc_status),
    column("VOLUME", col_pvc_volume),
    column("CAPACITY", col_pvc_capacity),
    column("AGE", col_age),
];

const PV_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("CAPACITY", col_pv_capacity),
    status_column("STATUS", col_pv_status),
    column("CLAIM", col_pv_claim),
    column("AGE", col_age),
];

const INGRESS_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("CLASS", col_ingress_class),
    column("HOSTS", col_ingress_hosts),
    column("ADDRESS", col_ingress_address),
    column("AGE", col_age),
];

const HTTPROUTE_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("HOSTNAMES", col_httproute_hostnames),
    column("AGE", col_age),
];

const ENDPOINT_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("ENDPOINTS", col_endpoint_count),
    column("AGE", col_age),
];

const CRD_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    column("GROUP", col_crd_group),
    column("KIND", col_crd_kind),
    column("VERSIONS", col_crd_versions),
    column("SCOPE", col_crd_scope),
    column("AGE", col_age),
];

const FLUX_OBJECT_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    status_column("READY", col_flux_ready),
    column("MESSAGE", col_flux_message),
    column("REVISION", col_flux_revision),
    column("SUSPENDED", col_flux_suspended),
    column("AGE", col_age),
];

const FLUX_SOURCE_COLUMNS: &[Column] = &[
    column("NAME", col_name),
    status_column("READY", col_flux_ready),
    column("MESSAGE", col_flux_message),
    column("REVISION", col_flux_source_revision),
    column("URL", col_flux_source_url),
    column("SUSPENDED", col_flux_suspended),
    column("AGE", col_age),
];

const DEFAULT_COLUMNS: &[Column] = &[column("NAME", col_name), column("AGE", col_age)];

/// One row per release, at its latest revision — like `helm list`. Backed by
/// the release storage `Secret`s (see `crate::helm`), not a real discovered
/// resource kind.
const HELM_COLUMNS: &[Column] = &[
    column("NAME", col_helm_name),
    column("REVISION", col_helm_revision),
    status_column("STATUS", col_helm_status),
    column("CHART", col_helm_chart),
    column("APP VERSION", col_helm_app_version),
    column("UPDATED", col_helm_updated),
];

/// Every revision of one release — like `helm history <release>`.
const HELM_HISTORY_COLUMNS: &[Column] = &[
    column("REVISION", col_helm_revision),
    status_column("STATUS", col_helm_status),
    column("CHART", col_helm_chart),
    column("APP VERSION", col_helm_app_version),
    column("DESCRIPTION", col_helm_description),
    column("UPDATED", col_helm_updated),
];

fn columns_for(plural: &str) -> &'static [Column] {
    match plural {
        "pods" => POD_COLUMNS,
        "deployments" => DEPLOYMENT_COLUMNS,
        "replicasets" => REPLICASET_COLUMNS,
        "statefulsets" => STATEFULSET_COLUMNS,
        "daemonsets" => DAEMONSET_COLUMNS,
        "services" => SERVICE_COLUMNS,
        "nodes" => NODE_COLUMNS,
        "namespaces" => NAMESPACE_COLUMNS,
        "configmaps" => CONFIGMAP_COLUMNS,
        "secrets" => SECRET_COLUMNS,
        "jobs" => JOB_COLUMNS,
        "cronjobs" => CRONJOB_COLUMNS,
        "events" => EVENT_COLUMNS,
        "horizontalpodautoscalers" => HPA_COLUMNS,
        "persistentvolumeclaims" => PVC_COLUMNS,
        "persistentvolumes" => PV_COLUMNS,
        "ingresses" => INGRESS_COLUMNS,
        "httproutes" => HTTPROUTE_COLUMNS,
        "endpoints" => ENDPOINT_COLUMNS,
        "customresourcedefinitions" => CRD_COLUMNS,
        "kustomizations" | "helmreleases" => FLUX_OBJECT_COLUMNS,
        "gitrepositories" | "helmrepositories" | "ocirepositories" | "buckets" => {
            FLUX_SOURCE_COLUMNS
        }
        "helm" => HELM_COLUMNS,
        "helmhistory" => HELM_HISTORY_COLUMNS,
        _ => DEFAULT_COLUMNS,
    }
}

/// Whether `plural` has curated columns (anything beyond the NAME/AGE
/// fallback). Kinds without them are candidates for the CRD printer-column
/// fallback.
pub fn has_curated(plural: &str) -> bool {
    columns_for(plural).as_ptr() != DEFAULT_COLUMNS.as_ptr()
}

/// The full curated headers for a kind (wide columns included), excluding the
/// leading NAMESPACE column (the table view prepends that when listing across
/// namespaces). Live views go through [`ViewSpec`] instead, which folds in
/// user views, printer columns, and wide-mode filtering.
#[cfg(test)]
pub fn headers(plural: &str) -> Vec<&'static str> {
    columns_for(plural).iter().map(|c| c.header).collect()
}

/// Cells for one object, aligned with [`headers`]. The 2nd return value is the
/// index of the column that should be colorized as a status (or None).
#[cfg(test)]
pub fn cells(obj: &DynamicObject, plural: &str) -> (Vec<String>, Option<usize>) {
    let ctx = CellContext::new(obj);
    let columns = columns_for(plural);
    let values = columns.iter().map(|c| (c.extract)(&ctx)).collect();
    let status_idx = columns.iter().position(|c| c.is_status);
    (values, status_idx)
}

/// The resolved column layout for one table view: the kind's curated columns,
/// overlaid with (or replaced by) a user-configured view — or CRD printer
/// columns when the kind is unknown — then filtered by wide mode. Built once
/// per view change, never per row.
pub struct ViewSpec {
    plural: String,
    columns: Vec<SpecColumn>,
    status_idx: Option<usize>,
}

struct SpecColumn {
    header: String,
    source: SpecSource,
    is_status: bool,
    wide: bool,
}

enum SpecSource {
    Curated(CellFn),
    User(crate::views::UserColumn),
}

fn spec_curated(c: &Column) -> SpecColumn {
    SpecColumn {
        header: c.header.to_string(),
        source: SpecSource::Curated(c.extract),
        is_status: c.is_status,
        wide: c.wide,
    }
}

fn spec_user(uc: &crate::views::UserColumn) -> SpecColumn {
    SpecColumn {
        header: uc.header.clone(),
        is_status: matches!(
            uc.kind,
            crate::views::ColumnKind::Status | crate::views::ColumnKind::Condition
        ),
        wide: uc.wide,
        source: SpecSource::User(uc.clone()),
    }
}

/// Resolve the columns for a view. `user` is the explicit view configured for
/// the kind; `crd` is the printer-column fallback, consulted only when the
/// user view defines no columns and `plural` has no curated ones.
pub fn build_spec(
    plural: &str,
    user: Option<&crate::views::View>,
    crd: Option<&crate::views::View>,
    wide: bool,
) -> ViewSpec {
    let mut cols: Vec<SpecColumn> = columns_for(plural).iter().map(spec_curated).collect();
    // A user view only counts as explicit column config when it has columns —
    // a sort-only view (or one whose columns all failed validation) still
    // benefits from the printer-column fallback.
    let view = match user {
        Some(v) if !v.columns.is_empty() => Some(v),
        _ => {
            if has_curated(plural) {
                None
            } else {
                crd
            }
        }
    };
    if let Some(v) = view {
        if v.replace && !v.columns.is_empty() {
            cols = v.columns.iter().map(spec_user).collect();
        } else {
            for uc in &v.columns {
                let sc = spec_user(uc);
                match cols.iter().position(|c| c.header == sc.header) {
                    // A matching header replaces the curated column in place.
                    Some(i) => cols[i] = sc,
                    // New columns land before a trailing AGE so it stays last.
                    None => {
                        let at = if cols.last().is_some_and(|c| c.header == "AGE") {
                            cols.len() - 1
                        } else {
                            cols.len()
                        };
                        cols.insert(at, sc);
                    }
                }
            }
        }
    }
    cols.retain(|c| wide || !c.wide);
    let status_idx = cols.iter().position(|c| c.is_status);
    ViewSpec {
        plural: plural.to_string(),
        columns: cols,
        status_idx,
    }
}

impl ViewSpec {
    pub fn headers(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.header.clone()).collect()
    }

    /// Cells for one object, aligned with [`Self::headers`], plus the index
    /// of the status column (if any).
    pub fn cells(&self, obj: &DynamicObject) -> (Vec<String>, Option<usize>) {
        let ctx = CellContext::new(obj);
        let values = self
            .columns
            .iter()
            .map(|c| match &c.source {
                SpecSource::Curated(extract) => extract(&ctx),
                SpecSource::User(uc) => crate::views::render_cell(obj, uc),
            })
            .collect();
        (values, self.status_idx)
    }

    /// See [`volatile_cell`]. User `time` columns re-render every frame too:
    /// their humanized elapsed value drifts with wall time.
    pub fn volatile(&self, obj: &DynamicObject, plural: &str, idx: usize) -> Option<String> {
        let col = self.columns.get(idx)?;
        match &col.source {
            SpecSource::User(uc) if uc.kind == crate::views::ColumnKind::Time => {
                Some(crate::views::render_cell(obj, uc))
            }
            SpecSource::User(_) => None,
            SpecSource::Curated(_) => volatile_cell(obj, plural, &col.header),
        }
    }

    /// Whether column `idx` can render a different value for the *same* object
    /// revision — the elapsed-time cells [`volatile_cell`] recomputes, and user
    /// `time` columns. Object-independent (and deliberately conservative about
    /// `jobs`/DURATION, which depends on the object), so a caller can decide
    /// once per view whether a cached cell may be reused.
    pub fn volatile_column(&self, plural: &str, idx: usize) -> bool {
        let Some(col) = self.columns.get(idx) else {
            return false;
        };
        match &col.source {
            SpecSource::User(uc) => uc.kind == crate::views::ColumnKind::Time,
            SpecSource::Curated(_) => matches!(
                (plural, col.header.as_str()),
                (_, "AGE") | ("jobs", "DURATION") | ("cronjobs", "LAST-SCHEDULE")
            ),
        }
    }

    /// Index of `header`'s column (case-insensitive), without allocating the
    /// header list.
    pub fn header_index(&self, header: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.header.eq_ignore_ascii_case(header))
    }

    /// The single cell at `idx` for one object. Filter comparisons read one
    /// column of every object — extracting the full row per object per
    /// rebuild is what this avoids.
    pub fn cell_at(&self, obj: &DynamicObject, idx: usize) -> Option<String> {
        let col = self.columns.get(idx)?;
        Some(match &col.source {
            SpecSource::User(uc) => crate::views::render_cell(obj, uc),
            SpecSource::Curated(extract) => {
                let ctx = CellContext::new(obj);
                extract(&ctx)
            }
        })
    }

    /// Whether `header` is a user/printer column (typed sorting applies) as
    /// opposed to a curated one.
    pub fn is_user_column(&self, header: &str) -> bool {
        self.columns
            .iter()
            .any(|c| c.header == header && matches!(c.source, SpecSource::User(_)))
    }

    /// Comparable value of `header`'s cell for `obj`, or `None` when the
    /// header isn't in this spec.
    pub fn sort_value(&self, obj: &DynamicObject, header: &str) -> Option<crate::views::SortValue> {
        let col = self.columns.iter().find(|c| c.header == header)?;
        Some(match &col.source {
            SpecSource::User(uc) => crate::views::sort_value(obj, uc),
            SpecSource::Curated(extract) => {
                let ctx = CellContext::new(obj);
                let v = extract(&ctx);
                if is_numeric_header(&self.plural, header) {
                    crate::views::SortValue::Num(parse_leading_num(&v))
                } else {
                    crate::views::SortValue::Text(v.to_lowercase())
                }
            }
        })
    }

    /// Configured fixed width for the column at `idx`, when it's a user
    /// column that set one.
    pub fn width_at(&self, idx: usize) -> Option<u16> {
        match &self.columns.get(idx)?.source {
            SpecSource::User(uc) => uc.width,
            SpecSource::Curated(_) => None,
        }
    }

    /// Configured alignment for the column at `idx`.
    pub fn align_at(&self, idx: usize) -> Option<crate::views::Align> {
        match &self.columns.get(idx)?.source {
            SpecSource::User(uc) => uc.align,
            SpecSource::Curated(_) => None,
        }
    }
}

/// Columns whose curated cell is a count/number and should sort numerically.
fn is_numeric_header(plural: &str, header: &str) -> bool {
    // READY is a count ("1/2") for workloads, but flux kinds render it as
    // True/False/Unknown, which must sort as text.
    if header == "READY" {
        let cols = columns_for(plural);
        return cols.as_ptr() != FLUX_OBJECT_COLUMNS.as_ptr()
            && cols.as_ptr() != FLUX_SOURCE_COLUMNS.as_ptr();
    }
    matches!(
        header,
        "RESTARTS"
            | "DATA"
            | "ACTIVE"
            | "DESIRED"
            | "CURRENT"
            | "AVAILABLE"
            | "UP-TO-DATE"
            | "COMPLETIONS"
            | "ENDPOINTS"
    )
}

/// Parse the leading number of a cell (`"3"`, `"1/2"` → 1, `"<none>"` → 0).
pub(crate) fn parse_leading_num(s: &str) -> f64 {
    let t = s.trim_start_matches(|c: char| !c.is_ascii_digit() && c != '-');
    let end = t
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(t.len());
    t[..end].parse::<f64>().unwrap_or(0.0)
}

/// Cells whose display value changes with wall time even when the Kubernetes
/// resourceVersion is unchanged. Table rendering can override cached values
/// with these without recomputing every curated column.
pub fn volatile_cell(obj: &DynamicObject, plural: &str, header: &str) -> Option<String> {
    match (plural, header) {
        (_, "AGE") => Some(age(obj)),
        ("jobs", "DURATION") if sget(&obj.data, &["status", "completionTime"]).is_none() => {
            Some(job_duration(&obj.data))
        }
        ("cronjobs", "LAST-SCHEDULE") => {
            Some(time_since(&obj.data, &["status", "lastScheduleTime"]))
        }
        _ => None,
    }
}

fn col_name(ctx: &CellContext<'_>) -> String {
    ctx.name.to_string()
}

fn col_age(ctx: &CellContext<'_>) -> String {
    ctx.age().to_string()
}

fn col_pod_ready(ctx: &CellContext<'_>) -> String {
    ctx.pod().0.clone()
}

fn col_pod_status(ctx: &CellContext<'_>) -> String {
    ctx.pod().1.clone()
}

fn col_pod_restarts(ctx: &CellContext<'_>) -> String {
    ctx.pod().2.clone()
}

fn col_pod_ip(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["status", "podIP"]).unwrap_or_else(|| "<none>".into())
}

fn col_pod_node(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "nodeName"]).unwrap_or_else(|| "<none>".into())
}

fn col_deploy_ready(ctx: &CellContext<'_>) -> String {
    format!(
        "{}/{}",
        iget(ctx.data, &["status", "readyReplicas"]),
        iget(ctx.data, &["status", "replicas"])
    )
}

fn col_deploy_updated(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "updatedReplicas"]).to_string()
}

fn col_deploy_available(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "availableReplicas"]).to_string()
}

fn col_rs_desired(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["spec", "replicas"]).to_string()
}

fn col_rs_current(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "replicas"]).to_string()
}

fn col_rs_ready(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "readyReplicas"]).to_string()
}

fn col_sts_ready(ctx: &CellContext<'_>) -> String {
    format!(
        "{}/{}",
        iget(ctx.data, &["status", "readyReplicas"]),
        iget(ctx.data, &["spec", "replicas"])
    )
}

fn col_ds_desired(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "desiredNumberScheduled"]).to_string()
}

fn col_ds_current(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "currentNumberScheduled"]).to_string()
}

fn col_ds_ready(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "numberReady"]).to_string()
}

fn col_ds_available(ctx: &CellContext<'_>) -> String {
    iget(ctx.data, &["status", "numberAvailable"]).to_string()
}

fn col_deploy_status(ctx: &CellContext<'_>) -> String {
    if ctx.obj.metadata.deletion_timestamp.is_some() {
        return "Terminating".into();
    }
    workload_status(ctx.data, WorkloadCounts::deployment(ctx.data))
}

fn col_sts_status(ctx: &CellContext<'_>) -> String {
    if ctx.obj.metadata.deletion_timestamp.is_some() {
        return "Terminating".into();
    }
    workload_status(ctx.data, WorkloadCounts::statefulset(ctx.data))
}

fn col_rs_status(ctx: &CellContext<'_>) -> String {
    if ctx.obj.metadata.deletion_timestamp.is_some() {
        return "Terminating".into();
    }
    workload_status(ctx.data, WorkloadCounts::replicaset(ctx.data))
}

fn col_ds_status(ctx: &CellContext<'_>) -> String {
    if ctx.obj.metadata.deletion_timestamp.is_some() {
        return "Terminating".into();
    }
    workload_status(ctx.data, WorkloadCounts::daemonset(ctx.data))
}

/// The replica counts a workload's health is judged by, normalized across
/// the four controller shapes (Deployment/StatefulSet/ReplicaSet count
/// replicas; DaemonSet counts scheduled nodes).
struct WorkloadCounts {
    desired: i64,
    current: i64,
    ready: i64,
    /// Replicas already on the current template revision — below `desired`
    /// means a rollout is still replacing pods.
    updated: i64,
}

impl WorkloadCounts {
    fn deployment(d: &Value) -> Self {
        WorkloadCounts {
            // spec.replicas defaults to 1 when unset.
            desired: iopt(d, &["spec", "replicas"]).unwrap_or(1),
            current: iget(d, &["status", "replicas"]),
            ready: iget(d, &["status", "readyReplicas"]),
            updated: iget(d, &["status", "updatedReplicas"]),
        }
    }

    fn statefulset(d: &Value) -> Self {
        Self::deployment(d)
    }

    fn replicaset(d: &Value) -> Self {
        WorkloadCounts {
            desired: iopt(d, &["spec", "replicas"]).unwrap_or(1),
            current: iget(d, &["status", "replicas"]),
            ready: iget(d, &["status", "readyReplicas"]),
            // ReplicaSets manage a single template revision; every replica
            // they create is "updated" by definition.
            updated: iget(d, &["status", "replicas"]),
        }
    }

    fn daemonset(d: &Value) -> Self {
        WorkloadCounts {
            desired: iget(d, &["status", "desiredNumberScheduled"]),
            current: iget(d, &["status", "currentNumberScheduled"]),
            ready: iget(d, &["status", "numberReady"]),
            updated: iget(d, &["status", "updatedNumberScheduled"]),
        }
    }
}

/// Rollout-health summary for workload kinds, derived from the object's own
/// replica counts and conditions — the same evidence `X`/explain weighs, so
/// a red row here and an `X` verdict never disagree. The strings feed the
/// STATUS badge and the whole-row colorer (`theme::row_color`), which is how
/// an unhealthy deployment reads red like k9s instead of uniformly blue:
///
/// - `Ready` — every desired replica is ready (healthy blue row)
/// - `ScaledDown` — desired 0, nothing running (faded, like Completed)
/// - `Unavailable` — desired > 0 but zero ready, or Available=False (red)
/// - `Progressing` — a rollout/scale is actively replacing or creating
///   pods (peach, transitional)
/// - `Degraded` — fully rolled out yet pods aren't ready: crash loops,
///   failing probes, unschedulable (red)
/// - `Stalled` — the Progressing condition gave up
///   (ProgressDeadlineExceeded) or ReplicaFailure is set (red)
fn workload_status(d: &Value, c: WorkloadCounts) -> String {
    if c.desired == 0 {
        return if c.current == 0 {
            "ScaledDown".into()
        } else {
            // Scale-to-zero still tearing pods down.
            "Progressing".into()
        };
    }
    if condition_is(d, "ReplicaFailure", "True")
        || condition_reason(d, "Progressing") == Some("ProgressDeadlineExceeded")
    {
        return "Stalled".into();
    }
    if c.ready >= c.desired {
        return if condition_is(d, "Available", "False") {
            "Unavailable".into()
        } else {
            "Ready".into()
        };
    }
    if c.ready == 0 {
        return "Unavailable".into();
    }
    if c.updated < c.desired || c.current < c.desired {
        return "Progressing".into();
    }
    "Degraded".into()
}

/// Status of the condition with type `ty` equals `status`; a missing
/// condition never matches.
fn condition_is(d: &Value, ty: &str, status: &str) -> bool {
    condition(d, ty)
        .and_then(|c| c.get("status"))
        .and_then(Value::as_str)
        == Some(status)
}

fn condition_reason<'a>(d: &'a Value, ty: &str) -> Option<&'a str> {
    condition(d, ty)?.get("reason")?.as_str()
}

fn condition<'a>(d: &'a Value, ty: &str) -> Option<&'a Value> {
    d.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(ty))
}

fn col_service_type(ctx: &CellContext<'_>) -> String {
    service_type(ctx.data)
}

fn col_service_cluster_ip(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "clusterIP"]).unwrap_or_else(|| "<none>".into())
}

fn col_service_external_ip(ctx: &CellContext<'_>) -> String {
    external_ip(ctx.data, &service_type(ctx.data))
}

fn col_service_ports(ctx: &CellContext<'_>) -> String {
    svc_ports(ctx.data)
}

fn col_node_status(ctx: &CellContext<'_>) -> String {
    node_ready(ctx.data)
}

fn col_node_roles(ctx: &CellContext<'_>) -> String {
    node_roles(ctx.obj)
}

fn col_node_taints(ctx: &CellContext<'_>) -> String {
    count_arr(ctx.data, &["spec", "taints"]).to_string()
}

fn col_node_version(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["status", "nodeInfo", "kubeletVersion"]).unwrap_or_default()
}

fn col_namespace_status(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["status", "phase"]).unwrap_or_else(|| "Active".into())
}

fn col_configmap_data(ctx: &CellContext<'_>) -> String {
    (count_obj(ctx.data, &["data"]) + count_obj(ctx.data, &["binaryData"])).to_string()
}

fn col_secret_type(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["type"]).unwrap_or_else(|| "Opaque".into())
}

fn col_secret_data(ctx: &CellContext<'_>) -> String {
    count_obj(ctx.data, &["data"]).to_string()
}

fn col_job_completions(ctx: &CellContext<'_>) -> String {
    format!(
        "{}/{}",
        iget(ctx.data, &["status", "succeeded"]),
        iget(ctx.data, &["spec", "completions"]).max(1)
    )
}

fn col_job_duration(ctx: &CellContext<'_>) -> String {
    job_duration(ctx.data)
}

fn col_cronjob_schedule(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "schedule"]).unwrap_or_default()
}

fn col_cronjob_suspend(ctx: &CellContext<'_>) -> String {
    bget(ctx.data, &["spec", "suspend"]).to_string()
}

fn col_cronjob_active(ctx: &CellContext<'_>) -> String {
    count_arr(ctx.data, &["status", "active"]).to_string()
}

fn col_cronjob_last_schedule(ctx: &CellContext<'_>) -> String {
    time_since(ctx.data, &["status", "lastScheduleTime"])
}

fn col_event_type(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["type"]).unwrap_or_default()
}

fn col_event_reason(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["reason"]).unwrap_or_default()
}

fn col_event_object(ctx: &CellContext<'_>) -> String {
    event_object(ctx.data)
}

fn col_event_message(ctx: &CellContext<'_>) -> String {
    event_message(ctx.data)
}

fn col_event_count(ctx: &CellContext<'_>) -> String {
    event_count(ctx.data).to_string()
}

fn col_hpa_reference(ctx: &CellContext<'_>) -> String {
    hpa_reference(ctx.data)
}

fn col_hpa_targets(ctx: &CellContext<'_>) -> String {
    hpa_targets(ctx.data)
}

fn col_hpa_minpods(ctx: &CellContext<'_>) -> String {
    iopt(ctx.data, &["spec", "minReplicas"])
        .unwrap_or(1)
        .to_string()
}

fn col_hpa_maxpods(ctx: &CellContext<'_>) -> String {
    iopt(ctx.data, &["spec", "maxReplicas"])
        .map(|n| n.to_string())
        .unwrap_or_default()
}

fn col_hpa_replicas(ctx: &CellContext<'_>) -> String {
    iopt(ctx.data, &["status", "currentReplicas"])
        .unwrap_or(0)
        .to_string()
}

fn col_pvc_status(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["status", "phase"]).unwrap_or_default()
}

fn col_pvc_volume(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "volumeName"]).unwrap_or_default()
}

fn col_pvc_capacity(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["status", "capacity", "storage"]).unwrap_or_default()
}

fn col_pv_capacity(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "capacity", "storage"]).unwrap_or_default()
}

fn col_pv_status(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["status", "phase"]).unwrap_or_default()
}

fn col_pv_claim(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "claimRef", "name"]).unwrap_or_default()
}

fn col_ingress_class(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "ingressClassName"]).unwrap_or_else(|| "<none>".into())
}

fn col_ingress_hosts(ctx: &CellContext<'_>) -> String {
    ingress_hosts(ctx.data)
}

fn col_ingress_address(ctx: &CellContext<'_>) -> String {
    ingress_address(ctx.data)
}

fn col_httproute_hostnames(ctx: &CellContext<'_>) -> String {
    httproute_hostnames(ctx.data)
}

fn col_endpoint_count(ctx: &CellContext<'_>) -> String {
    count_endpoints(ctx.data)
}

fn col_crd_group(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "group"]).unwrap_or_default()
}

fn col_crd_kind(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "names", "kind"]).unwrap_or_default()
}

fn col_crd_versions(ctx: &CellContext<'_>) -> String {
    crd_versions(ctx.data)
}

fn col_crd_scope(ctx: &CellContext<'_>) -> String {
    sget(ctx.data, &["spec", "scope"]).unwrap_or_default()
}

fn col_flux_ready(ctx: &CellContext<'_>) -> String {
    ready_condition(ctx.data).0
}

fn col_flux_message(ctx: &CellContext<'_>) -> String {
    ready_condition(ctx.data).1
}

fn col_flux_revision(ctx: &CellContext<'_>) -> String {
    flux_revision(ctx.data)
}

fn col_flux_source_revision(ctx: &CellContext<'_>) -> String {
    flux_source_revision(ctx.data)
}

fn col_flux_source_url(ctx: &CellContext<'_>) -> String {
    flux_source_url(ctx.data)
}

fn col_flux_suspended(ctx: &CellContext<'_>) -> String {
    bget(ctx.data, &["spec", "suspend"]).to_string()
}

// A Helm release row's underlying object is the raw storage `Secret`
// (`ctx.name`/`ctx.obj` are the ugly `sh.helm.release.v1.<release>.v<n>`
// name), never the release itself — every cell here goes through
// `crate::helm` instead of `ctx.name`/`ctx.data`.

fn col_helm_name(ctx: &CellContext<'_>) -> String {
    crate::helm::release_name(ctx.obj)
        .unwrap_or(ctx.name)
        .to_string()
}

fn col_helm_revision(ctx: &CellContext<'_>) -> String {
    crate::helm::revision(ctx.obj)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into())
}

fn col_helm_status(ctx: &CellContext<'_>) -> String {
    ctx.helm()
        .map(|r| r.status.clone())
        .unwrap_or_else(|| "<invalid>".into())
}

fn col_helm_chart(ctx: &CellContext<'_>) -> String {
    match ctx.helm() {
        Some(r) if !r.chart_name.is_empty() => format!("{}-{}", r.chart_name, r.chart_version),
        _ => "<unknown>".into(),
    }
}

fn col_helm_app_version(ctx: &CellContext<'_>) -> String {
    ctx.helm()
        .map(|r| r.app_version.clone())
        .unwrap_or_default()
}

fn col_helm_description(ctx: &CellContext<'_>) -> String {
    ctx.helm()
        .map(|r| r.description.clone())
        .unwrap_or_default()
}

fn col_helm_updated(ctx: &CellContext<'_>) -> String {
    match ctx.helm().and_then(|r| r.last_deployed_secs) {
        Some(secs) => humanize((Timestamp::now().as_second() - secs).max(0)),
        None => "<unknown>".into(),
    }
}

// ----- helpers ------------------------------------------------------------

fn service_type(d: &Value) -> String {
    sget(d, &["spec", "type"]).unwrap_or_else(|| "ClusterIP".into())
}

/// Comma-joined CRD version names (`spec.versions[].name`), e.g. `v1,v1beta1`.
fn crd_versions(d: &Value) -> String {
    let names: Vec<&str> = d
        .pointer("/spec/versions")
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(|v| v.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        "<none>".into()
    } else {
        names.join(",")
    }
}

/// (status, message) of the `Ready` condition, the health summary Flux (and
/// most condition-based CRDs) maintain. Missing condition reads as Unknown —
/// e.g. a Kustomization the controller hasn't reconciled yet.
fn ready_condition(d: &Value) -> (String, String) {
    d.pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .map(|c| {
            (
                c.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown")
                    .to_string(),
                c.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .unwrap_or_else(|| ("Unknown".into(), String::new()))
}

/// Last applied revision of a Flux object: Kustomizations (and HelmRelease
/// v2beta*) expose `lastAppliedRevision`; HelmRelease v2 GA moved it into
/// `history`, with `lastAttemptedRevision` as the pre-first-success fallback.
fn flux_revision(d: &Value) -> String {
    sget(d, &["status", "lastAppliedRevision"])
        .or_else(|| {
            d.pointer("/status/history/0/chartVersion")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .or_else(|| sget(d, &["status", "lastAttemptedRevision"]))
        .unwrap_or_default()
}

fn flux_source_revision(d: &Value) -> String {
    sget(d, &["status", "artifact", "revision"])
        .or_else(|| sget(d, &["status", "lastAppliedRevision"]))
        .or_else(|| sget(d, &["status", "lastAttemptedRevision"]))
        .unwrap_or_default()
}

fn flux_source_url(d: &Value) -> String {
    sget(d, &["spec", "url"])
        .or_else(|| {
            let endpoint = sget(d, &["spec", "endpoint"]);
            let bucket = sget(d, &["spec", "bucketName"]);
            match (endpoint, bucket) {
                (Some(endpoint), Some(bucket)) => {
                    Some(format!("{}/{}", endpoint.trim_end_matches('/'), bucket))
                }
                (Some(endpoint), None) => Some(endpoint),
                (None, Some(bucket)) => Some(bucket),
                (None, None) => None,
            }
        })
        .unwrap_or_default()
}

fn sget(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(|s| s.to_string())
}

fn iopt(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_i64()
}

fn iget(v: &Value, path: &[&str]) -> i64 {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_i64().unwrap_or(0)
}

fn bget(v: &Value, path: &[&str]) -> bool {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return false,
        }
    }
    cur.as_bool().unwrap_or(false)
}

fn count_obj(v: &Value, path: &[&str]) -> usize {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_object().map(|o| o.len()).unwrap_or(0)
}

fn count_arr(v: &Value, path: &[&str]) -> usize {
    let mut cur = v;
    for p in path {
        match cur.get(p) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_array().map(|a| a.len()).unwrap_or(0)
}

/// Compact human age, e.g. `3d4h`, `12m`, `45s`.
pub fn age(obj: &DynamicObject) -> String {
    match age_secs(obj) {
        Some(secs) => humanize(secs),
        None => "<unknown>".into(),
    }
}

/// Age in seconds since creation, or `None` if the object has no timestamp.
/// Used both for the AGE column and for sorting by age.
pub fn age_secs(obj: &DynamicObject) -> Option<i64> {
    let ts = obj.metadata.creation_timestamp.as_ref()?;
    Some((Timestamp::now().as_second() - ts.0.as_second()).max(0))
}

fn timestamp_secs(d: &Value, path: &[&str]) -> Option<i64> {
    sget(d, path)
        .and_then(|s| s.parse::<Timestamp>().ok())
        .map(|ts| ts.as_second())
}

/// Epoch seconds of a CronJob's `status.lastScheduleTime` — the sortable
/// value behind the humanized LAST-SCHEDULE cell.
pub fn last_schedule_secs(obj: &DynamicObject) -> Option<i64> {
    timestamp_secs(&obj.data, &["status", "lastScheduleTime"])
}

/// Elapsed seconds behind a Job's humanized DURATION cell (running jobs
/// measure against now).
pub fn job_duration_secs(obj: &DynamicObject) -> Option<i64> {
    let start = timestamp_secs(&obj.data, &["status", "startTime"])?;
    let end = timestamp_secs(&obj.data, &["status", "completionTime"])
        .unwrap_or_else(|| Timestamp::now().as_second());
    Some((end - start).max(0))
}

fn time_since(d: &Value, path: &[&str]) -> String {
    timestamp_secs(d, path)
        .map(|secs| humanize((Timestamp::now().as_second() - secs).max(0)))
        .unwrap_or_else(|| "<none>".into())
}

fn job_duration(d: &Value) -> String {
    let Some(start) = timestamp_secs(d, &["status", "startTime"]) else {
        return "<none>".into();
    };
    let end = timestamp_secs(d, &["status", "completionTime"])
        .unwrap_or_else(|| Timestamp::now().as_second());
    humanize((end - start).max(0))
}

/// Compact duration, e.g. `3d4h`, `12m`, `45s`.
pub(crate) fn humanize(secs: i64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        if h > 0 && d < 7 {
            format!("{d}d{h}h")
        } else {
            format!("{d}d")
        }
    } else if h > 0 {
        if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        }
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// (ready "n/m", status, restarts) for a pod, approximating kubectl logic.
fn pod_summary(obj: &DynamicObject) -> (String, String, String) {
    let d = &obj.data;
    let empty = vec![];
    let statuses = d
        .get("status")
        .and_then(|s| s.get("containerStatuses"))
        .and_then(|c| c.as_array())
        .unwrap_or(&empty);

    let total = statuses.len();
    let mut ready = 0usize;
    let mut restarts = 0i64;
    let mut waiting_reason: Option<String> = None;
    let mut terminated_reason: Option<String> = None;

    for c in statuses {
        if c.get("ready").and_then(Value::as_bool).unwrap_or(false) {
            ready += 1;
        }
        restarts += c.get("restartCount").and_then(Value::as_i64).unwrap_or(0);
        if let Some(r) = c.pointer("/state/waiting/reason").and_then(Value::as_str)
            && (r != "ContainerCreating" || waiting_reason.is_none())
        {
            waiting_reason = Some(r.to_string());
        }
        if let Some(r) = c
            .pointer("/state/terminated/reason")
            .and_then(Value::as_str)
            && r != "Completed"
        {
            terminated_reason = Some(r.to_string());
        }
    }

    let phase = sget(d, &["status", "phase"]).unwrap_or_else(|| "Unknown".into());
    let status = if obj.metadata.deletion_timestamp.is_some() {
        "Terminating".to_string()
    } else if let Some(r) = waiting_reason {
        r
    } else if let Some(r) = terminated_reason {
        r
    } else {
        phase
    };

    (format!("{ready}/{total}"), status, restarts.to_string())
}

fn external_ip(d: &Value, typ: &str) -> String {
    if let Some(ing) = d
        .pointer("/status/loadBalancer/ingress")
        .and_then(Value::as_array)
    {
        let ips: Vec<String> = ing
            .iter()
            .filter_map(|i| {
                i.get("ip")
                    .or_else(|| i.get("hostname"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();
        if !ips.is_empty() {
            return ips.join(",");
        }
    }
    if typ == "LoadBalancer" {
        "<pending>".into()
    } else {
        "<none>".into()
    }
}

fn svc_ports(d: &Value) -> String {
    d.pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ports| {
            ports
                .iter()
                .map(|p| {
                    let port = p.get("port").and_then(Value::as_i64).unwrap_or(0);
                    let proto = p.get("protocol").and_then(Value::as_str).unwrap_or("TCP");
                    format!("{port}/{proto}")
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "<none>".into())
}

fn node_ready(d: &Value) -> String {
    d.pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .map(|c| {
            if c.get("status").and_then(Value::as_str) == Some("True") {
                "Ready".to_string()
            } else {
                "NotReady".to_string()
            }
        })
        .unwrap_or_else(|| "Unknown".into())
}

fn node_roles(obj: &DynamicObject) -> String {
    let mut roles: Vec<String> = obj
        .metadata
        .labels
        .as_ref()
        .map(|l| {
            l.keys()
                .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                .filter(|r| !r.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    roles.sort();
    if roles.is_empty() {
        "<none>".into()
    } else {
        roles.join(",")
    }
}

fn ingress_hosts(d: &Value) -> String {
    d.pointer("/spec/rules")
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|r| r.get("host").and_then(Value::as_str).map(String::from))
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "*".into())
}

fn ingress_address(d: &Value) -> String {
    d.pointer("/status/loadBalancer/ingress")
        .and_then(Value::as_array)
        .map(|ing| {
            ing.iter()
                .filter_map(|i| {
                    i.get("ip")
                        .or_else(|| i.get("hostname"))
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".into())
}

fn httproute_hostnames(d: &Value) -> String {
    d.pointer("/spec/hostnames")
        .and_then(Value::as_array)
        .map(|hosts| {
            hosts
                .iter()
                .filter_map(|h| h.as_str().map(String::from))
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "*".into())
}

fn count_endpoints(d: &Value) -> String {
    let n: usize = d
        .pointer("/subsets")
        .and_then(Value::as_array)
        .map(|subs| {
            subs.iter()
                .map(|s| {
                    s.get("addresses")
                        .and_then(Value::as_array)
                        .map_or(0, |a| a.len())
                        * s.get("ports")
                            .and_then(Value::as_array)
                            .map_or(1, |p| p.len())
                })
                .sum()
        })
        .unwrap_or(0);
    if n == 0 {
        "<none>".into()
    } else {
        n.to_string()
    }
}

fn event_object(d: &Value) -> String {
    let Some(obj) = d.get("regarding").or_else(|| d.get("involvedObject")) else {
        return "<none>".into();
    };
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or_default();
    let name = obj.get("name").and_then(Value::as_str).unwrap_or_default();
    match (kind.is_empty(), name.is_empty()) {
        (false, false) => format!("{kind}/{name}"),
        (false, true) => kind.to_string(),
        (true, false) => name.to_string(),
        (true, true) => "<none>".into(),
    }
}

fn event_message(d: &Value) -> String {
    sget(d, &["message"])
        .or_else(|| sget(d, &["note"]))
        .unwrap_or_default()
        .replace(['\n', '\r'], " ")
}

fn event_count(d: &Value) -> i64 {
    iopt(d, &["series", "count"])
        .or_else(|| iopt(d, &["deprecatedCount"]))
        .or_else(|| iopt(d, &["count"]))
        .unwrap_or(1)
}

fn hpa_reference(d: &Value) -> String {
    let kind = sget(d, &["spec", "scaleTargetRef", "kind"]).unwrap_or_default();
    let name = sget(d, &["spec", "scaleTargetRef", "name"]).unwrap_or_default();
    match (kind.is_empty(), name.is_empty()) {
        (false, false) => format!("{kind}/{name}"),
        (false, true) => kind,
        (true, false) => name,
        (true, true) => "<none>".into(),
    }
}

fn hpa_targets(d: &Value) -> String {
    if let Some(target) = iopt(d, &["spec", "targetCPUUtilizationPercentage"]) {
        let current = iopt(d, &["status", "currentCPUUtilizationPercentage"])
            .map(|n| format!("{n}%"))
            .unwrap_or_else(|| "<unknown>".into());
        return format!("{current}/{target}%");
    }

    let Some(metrics) = d.pointer("/spec/metrics").and_then(Value::as_array) else {
        return "<none>".into();
    };
    if metrics.is_empty() {
        return "<none>".into();
    }

    let current = d
        .pointer("/status/currentMetrics")
        .and_then(Value::as_array);
    let mut targets: Vec<String> = metrics
        .iter()
        .zip(
            current
                .into_iter()
                .flatten()
                .map(Some)
                .chain(std::iter::repeat(None)),
        )
        .take(3)
        .map(|(metric, current)| hpa_metric(metric, current))
        .collect();
    if metrics.len() > 3 {
        targets.push(format!("+{}", metrics.len() - 3));
    }
    targets.join(",")
}

fn hpa_metric(metric: &Value, current: Option<&Value>) -> String {
    let typ = metric
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("Metric");
    let (path, default_name) = match typ {
        "ContainerResource" => ("containerResource", "container"),
        "External" => ("external", "external"),
        "Object" => ("object", "object"),
        "Pods" => ("pods", "pods"),
        "Resource" => ("resource", "resource"),
        _ => ("", typ),
    };
    let name = if path.is_empty() {
        default_name.to_string()
    } else {
        metric
            .pointer(&format!("/{path}/name"))
            .or_else(|| metric.pointer(&format!("/{path}/metric/name")))
            .and_then(Value::as_str)
            .unwrap_or(default_name)
            .to_string()
    };
    let target = if path.is_empty() {
        None
    } else {
        metric.pointer(&format!("/{path}/target"))
    }
    .map(hpa_metric_value)
    .unwrap_or_else(|| "<target>".into());
    let current = if path.is_empty() {
        None
    } else {
        current.and_then(|m| m.pointer(&format!("/{path}/current")))
    }
    .map(hpa_metric_value)
    .unwrap_or_else(|| "?".into());
    format!("{name}: {current}/{target}")
}

fn hpa_metric_value(metric: &Value) -> String {
    if let Some(n) = metric.get("averageUtilization").and_then(Value::as_i64) {
        format!("{n}%")
    } else if let Some(s) = metric.get("averageValue").and_then(Value::as_str) {
        s.to_string()
    } else if let Some(s) = metric.get("value").and_then(Value::as_str) {
        s.to_string()
    } else {
        "?".into()
    }
}

// ----- metrics quantity parsing/formatting -------------------------------

/// Parse a Kubernetes CPU quantity (e.g. `250m`, `1`, `140736419n`) into
/// millicores.
pub fn parse_cpu_milli(s: &str) -> i64 {
    let s = s.trim();
    let (num, scale) = match s.chars().last() {
        Some('n') => (&s[..s.len() - 1], 1.0 / 1_000_000.0),
        Some('u') => (&s[..s.len() - 1], 1.0 / 1_000.0),
        Some('m') => (&s[..s.len() - 1], 1.0),
        _ => (s, 1000.0),
    };
    (num.parse::<f64>().unwrap_or(0.0) * scale).round() as i64
}

/// Parse a Kubernetes memory quantity (e.g. `512Mi`, `1Gi`, `2000000`) into bytes.
pub fn parse_mem_bytes(s: &str) -> i64 {
    let s = s.trim();
    let suffixes: &[(&str, f64)] = &[
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0f64.powi(4)),
        ("K", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    for (suf, mult) in suffixes {
        if let Some(num) = s.strip_suffix(suf) {
            return (num.trim().parse::<f64>().unwrap_or(0.0) * mult) as i64;
        }
    }
    s.parse::<f64>().unwrap_or(0.0) as i64
}

/// A node's allocatable CPU (millicores) and memory (bytes) from
/// `status.allocatable` — the pool the scheduler actually hands out, i.e. the
/// right denominator for "how full is this node".
pub fn node_allocatable(o: &DynamicObject) -> (Option<i64>, Option<i64>) {
    let read = |field: &str, parse: fn(&str) -> i64| {
        o.data
            .pointer(&format!("/status/allocatable/{field}"))
            .and_then(Value::as_str)
            .map(parse)
            .filter(|v| *v > 0)
    };
    (
        read("cpu", parse_cpu_milli),
        read("memory", parse_mem_bytes),
    )
}

pub fn fmt_cpu(milli: i64) -> String {
    if milli <= 0 {
        "-".into()
    } else {
        format!("{milli}m")
    }
}

pub fn fmt_mem(bytes: i64) -> String {
    if bytes <= 0 {
        return "-".into();
    }
    let mi = bytes as f64 / (1024.0 * 1024.0);
    if mi >= 1024.0 {
        format!("{:.1}Gi", mi / 1024.0)
    } else {
        format!("{:.0}Mi", mi)
    }
}

/// A container's declared CPU/memory requests and limits, in millicores and
/// bytes. `None` marks an unset request or limit so callers can tell a missing
/// declaration apart from an explicit zero.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContainerResources {
    pub cpu_request: Option<i64>,
    pub cpu_limit: Option<i64>,
    pub mem_request: Option<i64>,
    pub mem_limit: Option<i64>,
}

/// Usage as an integer percentage of `base` (a request or limit). `None` when
/// the base is unset or non-positive, so the UI distinguishes a missing
/// request/limit from a real 0%.
pub fn usage_pct(usage: i64, base: Option<i64>) -> Option<i64> {
    match base {
        Some(b) if b > 0 => Some(((usage as f64 / b as f64) * 100.0).round() as i64),
        _ => None,
    }
}

/// Render a percentage from [`usage_pct`]; `None` (missing request/limit) shows
/// as `-`.
pub fn fmt_pct(pct: Option<i64>) -> String {
    match pct {
        Some(p) => format!("{p}%"),
        None => "-".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cpu_quantities() {
        assert_eq!(parse_cpu_milli("250m"), 250);
        assert_eq!(parse_cpu_milli("1"), 1000);
        assert_eq!(parse_cpu_milli("2"), 2000);
        assert_eq!(parse_cpu_milli("500000000n"), 500); // nanocores
    }

    #[test]
    fn parses_mem_quantities() {
        assert_eq!(parse_mem_bytes("1Ki"), 1024);
        assert_eq!(parse_mem_bytes("1Mi"), 1024 * 1024);
        assert_eq!(parse_mem_bytes("1Gi"), 1024 * 1024 * 1024);
        assert_eq!(fmt_mem(parse_mem_bytes("512Mi")), "512Mi");
    }

    #[test]
    fn usage_percent_distinguishes_missing_base_from_zero() {
        // Half of a 250m request rounds to 50%.
        assert_eq!(usage_pct(125, Some(250)), Some(50));
        // A measured zero against a real request is 0%, not "missing".
        assert_eq!(usage_pct(0, Some(250)), Some(0));
        // No request/limit declared -> None, so the UI shows "-".
        assert_eq!(usage_pct(125, None), None);
        assert_eq!(usage_pct(125, Some(0)), None);
        assert_eq!(fmt_pct(usage_pct(125, Some(250))), "50%");
        assert_eq!(fmt_pct(usage_pct(125, None)), "-");
    }

    #[test]
    fn humanize_buckets() {
        assert_eq!(humanize(45), "45s");
        assert_eq!(humanize(90), "1m");
        assert_eq!(humanize(3661), "1h1m");
        assert_eq!(humanize(90_000), "1d1h");
        assert_eq!(humanize(700_000), "8d");
    }

    fn obj(v: serde_json::Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn pod_cells_summarize_status_and_restarts() {
        let p = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"nodeName": "node-1"},
            "status": {
                "phase": "Running",
                "podIP": "10.0.0.5",
                "containerStatuses": [
                    {"ready": true, "restartCount": 2, "state": {"running": {}}},
                    {"ready": false, "restartCount": 0,
                     "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]
            }
        }));
        let (cells, status_idx) = cells(&p, "pods");
        assert_eq!(cells[0], "web");
        assert_eq!(cells[1], "1/2"); // ready
        assert_eq!(cells[2], "CrashLoopBackOff"); // waiting reason overrides phase
        assert_eq!(cells[3], "2"); // total restarts
        assert_eq!(cells[4], "10.0.0.5");
        assert_eq!(cells[5], "node-1");
        assert_eq!(status_idx, Some(2));
    }

    fn deploy(spec_replicas: i64, status: serde_json::Value) -> DynamicObject {
        obj(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {"replicas": spec_replicas},
            "status": status
        }))
    }

    #[test]
    fn deployment_status_reflects_replica_health() {
        // All ready → healthy.
        let d = deploy(
            3,
            json!({"replicas": 3, "readyReplicas": 3, "updatedReplicas": 3}),
        );
        let (c, status_idx) = cells(&d, "deployments");
        assert_eq!(c[1], "3/3");
        assert_eq!(c[2], "Ready");
        assert_eq!(status_idx, Some(2));

        // Zero ready with replicas desired → unavailable (red).
        let d = deploy(
            3,
            json!({"replicas": 3, "readyReplicas": 0, "updatedReplicas": 3}),
        );
        assert_eq!(cells(&d, "deployments").0[2], "Unavailable");

        // Rollout replacing pods (updated < desired) → progressing.
        let d = deploy(
            3,
            json!({"replicas": 4, "readyReplicas": 2, "updatedReplicas": 1}),
        );
        assert_eq!(cells(&d, "deployments").0[2], "Progressing");

        // Fully rolled out but pods unready (crash loops, failed probes) →
        // degraded, not "progressing" — nothing is coming to fix it.
        let d = deploy(
            3,
            json!({"replicas": 3, "readyReplicas": 2, "updatedReplicas": 3}),
        );
        assert_eq!(cells(&d, "deployments").0[2], "Degraded");

        // Scaled to zero reads faded, not broken.
        let d = deploy(0, json!({}));
        assert_eq!(cells(&d, "deployments").0[2], "ScaledDown");
        // ... and still tearing down reads transitional.
        let d = deploy(0, json!({"replicas": 2, "readyReplicas": 2}));
        assert_eq!(cells(&d, "deployments").0[2], "Progressing");
    }

    #[test]
    fn deployment_status_honors_conditions() {
        // ProgressDeadlineExceeded → the rollout gave up.
        let d = deploy(
            3,
            json!({
                "replicas": 3, "readyReplicas": 1, "updatedReplicas": 3,
                "conditions": [{"type": "Progressing", "status": "False",
                                "reason": "ProgressDeadlineExceeded"}]
            }),
        );
        assert_eq!(cells(&d, "deployments").0[2], "Stalled");

        // Available=False overrides a numerically-satisfied ready count.
        let d = deploy(
            3,
            json!({
                "replicas": 3, "readyReplicas": 3, "updatedReplicas": 3,
                "conditions": [{"type": "Available", "status": "False"}]
            }),
        );
        assert_eq!(cells(&d, "deployments").0[2], "Unavailable");
    }

    #[test]
    fn statefulset_daemonset_replicaset_status() {
        let sts = obj(json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": {"name": "db", "namespace": "default"},
            "spec": {"replicas": 3},
            "status": {"replicas": 3, "readyReplicas": 2, "updatedReplicas": 2}
        }));
        // Rolling update in flight (updated < desired) → progressing.
        assert_eq!(cells(&sts, "statefulsets").0[2], "Progressing");

        let ds = obj(json!({
            "apiVersion": "apps/v1", "kind": "DaemonSet",
            "metadata": {"name": "agent", "namespace": "default"},
            "status": {"desiredNumberScheduled": 5, "currentNumberScheduled": 5,
                       "numberReady": 3, "updatedNumberScheduled": 5}
        }));
        // Fully rolled out, nodes unready → degraded. STATUS follows AVAILABLE.
        assert_eq!(cells(&ds, "daemonsets").0[5], "Degraded");

        // An old, scaled-down ReplicaSet must read faded, not broken.
        let rs = obj(json!({
            "apiVersion": "apps/v1", "kind": "ReplicaSet",
            "metadata": {"name": "web-abc", "namespace": "default"},
            "spec": {"replicas": 0},
            "status": {"replicas": 0}
        }));
        assert_eq!(cells(&rs, "replicasets").0[4], "ScaledDown");

        // spec.replicas unset defaults to 1 — a bare RS with one ready pod
        // is healthy, not degraded.
        let rs = obj(json!({
            "apiVersion": "apps/v1", "kind": "ReplicaSet",
            "metadata": {"name": "web-abc", "namespace": "default"},
            "status": {"replicas": 1, "readyReplicas": 1}
        }));
        assert_eq!(cells(&rs, "replicasets").0[4], "Ready");
    }

    #[test]
    fn terminating_workload_reads_terminating() {
        let d = obj(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "default",
                         "deletionTimestamp": "2024-01-01T00:00:00Z"},
            "spec": {"replicas": 3},
            "status": {"replicas": 3, "readyReplicas": 3, "updatedReplicas": 3}
        }));
        assert_eq!(cells(&d, "deployments").0[2], "Terminating");
    }

    #[test]
    fn node_cells_count_taints() {
        let n = obj(json!({
            "apiVersion": "v1", "kind": "Node",
            "metadata": {"name": "cp-1",
                         "labels": {"node-role.kubernetes.io/control-plane": ""}},
            "spec": {"taints": [
                {"key": "node-role.kubernetes.io/control-plane", "effect": "NoSchedule"},
                {"key": "dedicated", "value": "gpu", "effect": "NoExecute"}
            ]},
            "status": {"conditions": [{"type": "Ready", "status": "True"}],
                       "nodeInfo": {"kubeletVersion": "v1.31.0"}}
        }));
        let (node_cells, status_idx) = cells(&n, "nodes");
        assert_eq!(node_cells[0], "cp-1");
        assert_eq!(node_cells[1], "Ready");
        assert_eq!(node_cells[2], "control-plane");
        assert_eq!(node_cells[3], "2");
        assert_eq!(node_cells[4], "v1.31.0");
        assert_eq!(status_idx, Some(1));

        let bare = obj(json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "w-1"}}));
        let (bare_cells, _) = cells(&bare, "nodes");
        assert_eq!(bare_cells[3], "0");
    }

    #[test]
    fn service_external_ip_pending() {
        let s = obj(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "lb"},
            "spec": {"type": "LoadBalancer", "clusterIP": "10.1.1.1",
                     "ports": [{"port": 80, "protocol": "TCP"}]},
            "status": {"loadBalancer": {}}
        }));
        let (cells, _) = cells(&s, "services");
        assert_eq!(cells[1], "LoadBalancer");
        assert_eq!(cells[3], "<pending>");
        assert_eq!(cells[4], "80/TCP");
    }

    #[test]
    fn crd_cells_show_group_kind_versions_scope() {
        let crd = obj(json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "widgets.example.com"},
            "spec": {
                "group": "example.com",
                "names": {"plural": "widgets", "kind": "Widget"},
                "scope": "Namespaced",
                "versions": [
                    {"name": "v1beta1", "served": true, "storage": false},
                    {"name": "v1", "served": true, "storage": true}
                ]
            }
        }));
        assert_eq!(
            headers("customresourcedefinitions"),
            vec!["NAME", "GROUP", "KIND", "VERSIONS", "SCOPE", "AGE"]
        );
        let (cells, _) = cells(&crd, "customresourcedefinitions");
        assert_eq!(cells[0], "widgets.example.com");
        assert_eq!(cells[1], "example.com");
        assert_eq!(cells[2], "Widget");
        assert_eq!(cells[3], "v1beta1,v1");
        assert_eq!(cells[4], "Namespaced");
    }

    #[test]
    fn kustomization_cells_show_ready_condition_and_revision() {
        let ks = obj(json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
            "kind": "Kustomization",
            "metadata": {"name": "apps", "namespace": "flux-system"},
            "spec": {"suspend": false},
            "status": {
                "lastAppliedRevision": "main@sha1:abc123",
                "conditions": [
                    {"type": "Reconciling", "status": "False"},
                    {"type": "Ready", "status": "True",
                     "message": "Applied revision: main@sha1:abc123"}
                ]
            }
        }));
        let (cells, status_idx) = cells(&ks, "kustomizations");
        assert_eq!(cells[0], "apps");
        assert_eq!(cells[1], "True");
        assert_eq!(cells[2], "Applied revision: main@sha1:abc123");
        assert_eq!(cells[3], "main@sha1:abc123");
        assert_eq!(cells[4], "false");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn helmrelease_cells_fall_back_to_history_revision() {
        let hr = obj(json!({
            "apiVersion": "helm.toolkit.fluxcd.io/v2",
            "kind": "HelmRelease",
            "metadata": {"name": "podinfo", "namespace": "default"},
            "spec": {"suspend": true},
            "status": {
                "history": [{"chartVersion": "6.5.4"}],
                "conditions": [
                    {"type": "Ready", "status": "False",
                     "message": "install retries exhausted"}
                ]
            }
        }));
        let (cells, status_idx) = cells(&hr, "helmreleases");
        assert_eq!(cells[1], "False");
        assert_eq!(cells[2], "install retries exhausted");
        assert_eq!(cells[3], "6.5.4");
        assert_eq!(cells[4], "true");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn flux_source_cells_show_ready_revision_url() {
        let git = obj(json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1",
            "kind": "GitRepository",
            "metadata": {"name": "apps", "namespace": "flux-system"},
            "spec": {"url": "https://github.com/example/apps", "suspend": true},
            "status": {
                "artifact": {"revision": "main@sha1:abc123"},
                "conditions": [
                    {"type": "Ready", "status": "True", "message": "stored artifact"}
                ]
            }
        }));
        let (cells, status_idx) = cells(&git, "gitrepositories");
        assert_eq!(
            headers("gitrepositories"),
            vec![
                "NAME",
                "READY",
                "MESSAGE",
                "REVISION",
                "URL",
                "SUSPENDED",
                "AGE"
            ]
        );
        assert_eq!(cells[0], "apps");
        assert_eq!(cells[1], "True");
        assert_eq!(cells[2], "stored artifact");
        assert_eq!(cells[3], "main@sha1:abc123");
        assert_eq!(cells[4], "https://github.com/example/apps");
        assert_eq!(cells[5], "true");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn bucket_cells_build_url_from_endpoint_and_bucket_name() {
        let bucket = obj(json!({
            "apiVersion": "source.toolkit.fluxcd.io/v1beta2",
            "kind": "Bucket",
            "metadata": {"name": "charts"},
            "spec": {"endpoint": "https://s3.example.com/", "bucketName": "charts"},
            "status": {
                "artifact": {"revision": "sha256:abc123"},
                "conditions": [{"type": "Ready", "status": "False", "message": "denied"}]
            }
        }));
        let (cells, status_idx) = cells(&bucket, "buckets");
        assert_eq!(cells[1], "False");
        assert_eq!(cells[2], "denied");
        assert_eq!(cells[3], "sha256:abc123");
        assert_eq!(cells[4], "https://s3.example.com/charts");
        assert_eq!(cells[5], "false");
        assert_eq!(status_idx, Some(1));
    }

    #[test]
    fn job_cells_include_duration() {
        let job = obj(json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {"name": "migrate"},
            "spec": {"completions": 3},
            "status": {
                "succeeded": 2,
                "startTime": "2024-01-01T00:00:00Z",
                "completionTime": "2024-01-01T01:05:00Z"
            }
        }));
        let (cells, _) = cells(&job, "jobs");
        assert_eq!(
            headers("jobs"),
            vec!["NAME", "COMPLETIONS", "DURATION", "AGE"]
        );
        assert_eq!(cells[1], "2/3");
        assert_eq!(cells[2], "1h5m");
    }

    #[test]
    fn cronjob_cells_include_last_schedule() {
        let cron = obj(json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "metadata": {"name": "backup"},
            "spec": {"schedule": "*/15 * * * *", "suspend": false},
            "status": {"active": [{"name": "backup-1"}]}
        }));
        let (cells, _) = cells(&cron, "cronjobs");
        assert_eq!(
            headers("cronjobs"),
            vec![
                "NAME",
                "SCHEDULE",
                "SUSPEND",
                "ACTIVE",
                "LAST-SCHEDULE",
                "AGE"
            ]
        );
        assert_eq!(cells[1], "*/15 * * * *");
        assert_eq!(cells[2], "false");
        assert_eq!(cells[3], "1");
        assert_eq!(cells[4], "<none>");
    }

    #[test]
    fn event_cells_show_object_message_and_count() {
        let event = obj(json!({
            "apiVersion": "events.k8s.io/v1",
            "kind": "Event",
            "metadata": {"name": "nginx.abc"},
            "type": "Warning",
            "reason": "BackOff",
            "regarding": {"kind": "Pod", "name": "nginx"},
            "note": "Back-off restarting\nfailed container",
            "series": {"count": 7}
        }));
        let (cells, status_idx) = cells(&event, "events");
        assert_eq!(
            headers("events"),
            vec![
                "NAME", "TYPE", "REASON", "OBJECT", "MESSAGE", "COUNT", "AGE"
            ]
        );
        assert_eq!(cells[1], "Warning");
        assert_eq!(cells[2], "BackOff");
        assert_eq!(cells[3], "Pod/nginx");
        assert_eq!(cells[4], "Back-off restarting failed container");
        assert_eq!(cells[5], "7");
        assert_eq!(status_idx, None);
    }

    #[test]
    fn hpa_cells_show_reference_targets_and_replicas() {
        let hpa = obj(json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": {"name": "web"},
            "spec": {
                "scaleTargetRef": {"kind": "Deployment", "name": "web"},
                "minReplicas": 2,
                "maxReplicas": 10,
                "metrics": [{
                    "type": "Resource",
                    "resource": {
                        "name": "cpu",
                        "target": {"type": "Utilization", "averageUtilization": 80}
                    }
                }]
            },
            "status": {
                "currentReplicas": 4,
                "currentMetrics": [{
                    "type": "Resource",
                    "resource": {
                        "name": "cpu",
                        "current": {"averageUtilization": 42}
                    }
                }]
            }
        }));
        let (cells, _) = cells(&hpa, "horizontalpodautoscalers");
        assert_eq!(
            headers("horizontalpodautoscalers"),
            vec![
                "NAME",
                "REFERENCE",
                "TARGETS",
                "MINPODS",
                "MAXPODS",
                "REPLICAS",
                "AGE"
            ]
        );
        assert_eq!(cells[1], "Deployment/web");
        assert_eq!(cells[2], "cpu: 42%/80%");
        assert_eq!(cells[3], "2");
        assert_eq!(cells[4], "10");
        assert_eq!(cells[5], "4");
    }

    #[test]
    fn flux_object_without_status_reads_unknown() {
        let ks = obj(json!({
            "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
            "kind": "Kustomization",
            "metadata": {"name": "new"}
        }));
        let (cells, _) = cells(&ks, "kustomizations");
        assert_eq!(cells[1], "Unknown");
        assert_eq!(cells[2], "");
        assert_eq!(cells[3], "");
    }

    #[test]
    fn ingress_cells_show_class_hosts_and_address() {
        let ing = obj(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": {"name": "web"},
            "spec": {
                "ingressClassName": "nginx",
                "rules": [{"host": "app.example.com"}, {"host": "api.example.com"}]
            },
            "status": {"loadBalancer": {"ingress": [{"ip": "203.0.113.10"}]}}
        }));
        let (cells, _) = cells(&ing, "ingresses");
        assert_eq!(
            headers("ingresses"),
            ["NAME", "CLASS", "HOSTS", "ADDRESS", "AGE"]
        );
        assert_eq!(cells[1], "nginx");
        assert_eq!(cells[2], "app.example.com,api.example.com");
        assert_eq!(cells[3], "203.0.113.10");
    }

    #[test]
    fn ingress_address_falls_back_to_hostname_then_none() {
        let hostname = obj(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": {"name": "lb"},
            "status": {"loadBalancer": {"ingress": [{"hostname": "abc.elb.amazonaws.com"}]}}
        }));
        assert_eq!(cells(&hostname, "ingresses").0[3], "abc.elb.amazonaws.com");

        let pending = obj(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": {"name": "pending"}
        }));
        assert_eq!(cells(&pending, "ingresses").0[3], "<none>");
    }

    #[test]
    fn httproute_cells_show_hostnames() {
        let route = obj(json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "kind": "HTTPRoute",
            "metadata": {"name": "web"},
            "spec": {"hostnames": ["app.example.com", "www.example.com"]}
        }));
        let (row, idx) = cells(&route, "httproutes");
        assert_eq!(headers("httproutes"), ["NAME", "HOSTNAMES", "AGE"]);
        assert_eq!(row[1], "app.example.com,www.example.com");
        assert_eq!(idx, None);

        let wildcard = obj(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
            "metadata": {"name": "any"}
        }));
        assert_eq!(cells(&wildcard, "httproutes").0[1], "*");
    }

    #[test]
    fn unknown_kind_falls_back_to_name_age() {
        let o = obj(json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "thingy"}
        }));
        let (cells, idx) = cells(&o, "widgets");
        assert_eq!(cells[0], "thingy");
        assert_eq!(cells.len(), 2);
        assert_eq!(idx, None);
    }

    #[test]
    fn curated_headers_and_cells_stay_aligned() {
        let o = obj(json!({
            "apiVersion": "v1",
            "kind": "Object",
            "metadata": {"name": "sample"}
        }));
        let kinds = [
            "pods",
            "deployments",
            "replicasets",
            "statefulsets",
            "daemonsets",
            "services",
            "nodes",
            "namespaces",
            "configmaps",
            "secrets",
            "jobs",
            "cronjobs",
            "events",
            "horizontalpodautoscalers",
            "persistentvolumeclaims",
            "persistentvolumes",
            "ingresses",
            "httproutes",
            "endpoints",
            "customresourcedefinitions",
            "kustomizations",
            "helmreleases",
            "gitrepositories",
            "helmrepositories",
            "ocirepositories",
            "buckets",
            "widgets",
        ];

        for kind in kinds {
            let headers = headers(kind);
            let (cells, status_idx) = cells(&o, kind);
            assert_eq!(headers.len(), cells.len(), "{kind} column count");
            if let Some(idx) = status_idx {
                assert!(idx < cells.len(), "{kind} status index");
            }
        }
    }

    // ----- view specs (curated + user/printer columns + wide) --------------

    fn user_col(
        header: &str,
        pointer: &str,
        kind: crate::views::ColumnKind,
    ) -> crate::views::UserColumn {
        crate::views::UserColumn {
            header: header.into(),
            pointer: pointer.into(),
            kind,
            wide: false,
            width: None,
            align: None,
            condition_field: None,
        }
    }

    fn view(columns: Vec<crate::views::UserColumn>, replace: bool) -> crate::views::View {
        crate::views::View {
            columns,
            replace,
            ..Default::default()
        }
    }

    #[test]
    fn spec_overlay_replaces_matching_headers_and_inserts_before_age() {
        use crate::views::ColumnKind;
        let v = view(
            vec![
                // Collides with the curated STATUS header: replaces in place.
                user_col("STATUS", "/status/phase", ColumnKind::Status),
                // New column: lands before the trailing AGE.
                user_col("NODE-IP", "/status/hostIP", ColumnKind::Text),
            ],
            false,
        );
        let spec = build_spec("pods", Some(&v), None, false);
        assert_eq!(
            spec.headers(),
            vec!["NAME", "READY", "STATUS", "RESTARTS", "NODE-IP", "AGE"]
        );
        let o = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "web"},
            "status": {"phase": "Running", "hostIP": "10.0.0.9"}
        }));
        let (cells, status_idx) = spec.cells(&o);
        assert_eq!(cells[2], "Running");
        assert_eq!(cells[4], "10.0.0.9");
        assert_eq!(status_idx, Some(2));
    }

    #[test]
    fn spec_replace_swaps_out_curated_columns_entirely() {
        use crate::views::ColumnKind;
        let v = view(
            vec![
                user_col("NAME", "/metadata/name", ColumnKind::Text),
                user_col("PHASE", "/status/phase", ColumnKind::Text),
            ],
            true,
        );
        let spec = build_spec("pods", Some(&v), None, true);
        assert_eq!(spec.headers(), vec!["NAME", "PHASE"]);
    }

    #[test]
    fn spec_wide_mode_gates_wide_only_columns() {
        let narrow = build_spec("pods", None, None, false);
        assert_eq!(
            narrow.headers(),
            vec!["NAME", "READY", "STATUS", "RESTARTS", "AGE"]
        );
        let wide = build_spec("pods", None, None, true);
        assert_eq!(
            wide.headers(),
            vec!["NAME", "READY", "STATUS", "RESTARTS", "IP", "NODE", "AGE"]
        );
    }

    #[test]
    fn spec_crd_fallback_applies_only_without_curated_or_user_view() {
        use crate::views::ColumnKind;
        let crd = view(
            vec![user_col("PHASE", "/status/phase", ColumnKind::Text)],
            false,
        );
        // Unknown kind: printer columns upgrade the NAME/AGE fallback.
        let spec = build_spec("widgets", None, Some(&crd), false);
        assert_eq!(spec.headers(), vec!["NAME", "PHASE", "AGE"]);
        // Curated kind: printer columns never apply.
        let spec = build_spec("pods", None, Some(&crd), false);
        assert_eq!(
            spec.headers(),
            vec!["NAME", "READY", "STATUS", "RESTARTS", "AGE"]
        );
        // Explicit user view outranks printer columns.
        let user = view(
            vec![user_col("MINE", "/status/mine", ColumnKind::Text)],
            false,
        );
        let spec = build_spec("widgets", Some(&user), Some(&crd), false);
        assert_eq!(spec.headers(), vec!["NAME", "MINE", "AGE"]);
        // A sort-only user view (no columns) still gets printer columns.
        let sort_only = crate::views::View {
            sort: Some(("PHASE".into(), false)),
            ..Default::default()
        };
        let spec = build_spec("widgets", Some(&sort_only), Some(&crd), false);
        assert_eq!(spec.headers(), vec!["NAME", "PHASE", "AGE"]);
    }

    #[test]
    fn spec_sort_value_is_typed_for_user_columns() {
        use crate::views::{ColumnKind, SortValue};
        let v = view(
            vec![user_col("CPU", "/spec/cpu", ColumnKind::Quantity)],
            false,
        );
        let spec = build_spec("widgets", Some(&v), None, false);
        let o = obj(json!({
            "apiVersion": "example.com/v1", "kind": "Widget",
            "metadata": {"name": "w"},
            "spec": {"cpu": "500m"}
        }));
        assert!(spec.is_user_column("CPU"));
        assert!(!spec.is_user_column("NAME"));
        match spec.sort_value(&o, "CPU").unwrap() {
            SortValue::Num(n) => assert_eq!(n, 0.5),
            SortValue::Text(t) => panic!("expected numeric sort, got '{t}'"),
        }
        assert!(spec.sort_value(&o, "MISSING").is_none());
    }

    #[test]
    fn flux_ready_sorts_as_text_pod_ready_stays_numeric() {
        use crate::views::SortValue;
        let flux = |status: &str| {
            obj(json!({
                "apiVersion": "kustomize.toolkit.fluxcd.io/v1",
                "kind": "Kustomization",
                "metadata": {"name": "apps", "namespace": "flux-system"},
                "status": {"conditions": [{"type": "Ready", "status": status}]}
            }))
        };
        let spec = build_spec("kustomizations", None, None, false);
        let ready = |status: &str| match spec.sort_value(&flux(status), "READY").unwrap() {
            SortValue::Text(t) => t,
            SortValue::Num(n) => panic!("flux READY must sort as text, got {n}"),
        };
        assert!(ready("False") < ready("True"));
        assert!(ready("True") < ready("Unknown"));

        let pod = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "p"},
            "status": {"containerStatuses": [
                {"ready": true, "restartCount": 0, "state": {"running": {}}},
                {"ready": false, "restartCount": 0, "state": {"running": {}}}
            ]}
        }));
        let spec = build_spec("pods", None, None, false);
        match spec.sort_value(&pod, "READY").unwrap() {
            SortValue::Num(n) => assert_eq!(n, 1.0),
            SortValue::Text(t) => panic!("pod READY must sort numerically, got '{t}'"),
        }
    }
}
