use super::*;

use crate::fleet::{FleetRow, FleetStatus};

/// Never query more than this many contexts at once, so opening the dashboard
/// against a large fleet doesn't open a connection storm.
const FLEET_CONCURRENCY: usize = 4;
/// Per-context budget: a slow or unreachable context fails here instead of
/// hanging the row (the others are unaffected).
const FLEET_TIMEOUT_SECS: u64 = 8;

impl App {
    /// The fleet contexts in effect: the `[fleet] contexts` config list plus
    /// contexts marked with `space` in the context switcher, minus config
    /// entries unmarked the same way. Marks persist across restarts (see
    /// [`crate::fleet::FleetMarks`]); the config file is never rewritten.
    pub fn fleet_contexts(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .fleet_cfg
            .contexts
            .iter()
            .filter(|c| !self.fleet_marks.removed.contains(c))
            .cloned()
            .collect();
        for c in &self.fleet_marks.added {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }

    /// Whether `ctx` is currently part of the fleet (config + marks).
    ///
    /// Answers the membership question directly rather than materializing
    /// [`Self::fleet_contexts`]: the context switcher asks this once per row
    /// per frame, and building (and cloning into) a `Vec` per row made that
    /// draw quadratic in allocations.
    pub fn is_fleet_context(&self, ctx: &str) -> bool {
        let listed = self.fleet_cfg.contexts.iter().any(|c| c == ctx)
            && !self.fleet_marks.removed.iter().any(|c| c == ctx);
        listed || self.fleet_marks.added.iter().any(|c| c == ctx)
    }

    /// Toggle a context in/out of the fleet (`space` in the context
    /// switcher), saving the marks so the fleet survives restarts.
    pub(super) fn toggle_fleet_context(&mut self, ctx: &str) {
        if self.is_fleet_context(ctx) {
            self.fleet_marks.added.retain(|c| c != ctx);
            // A config-listed context can't be dropped from the config Vec
            // (it comes back on every re-resolve), so it's masked instead.
            if self.fleet_cfg.contexts.iter().any(|c| c == ctx)
                && !self.fleet_marks.removed.iter().any(|c| c == ctx)
            {
                self.fleet_marks.removed.push(ctx.to_string());
            }
            self.flash = format!("fleet − {ctx}");
        } else {
            self.fleet_marks.removed.retain(|c| c != ctx);
            self.fleet_marks.added.push(ctx.to_string());
            self.flash = format!("fleet + {ctx}");
        }
        self.flash_err = false;
        if let Some(path) = self.fleet_marks_path.clone() {
            let result = match &self.state_writer {
                Some(writer) => writer.save_fleet(self.fleet_marks.clone(), path),
                None => self.fleet_marks.save(&path),
            };
            if let Err(e) = result {
                self.flash_warn(&format!("fleet mark not saved: {e}"));
            }
        }
    }

    /// `:fleet` — open the opt-in cross-context health dashboard. Only the
    /// contexts in `[fleet].contexts` (plus session `space`-marks) are
    /// queried; each is gathered off-thread with its own timeout so one slow
    /// context never blocks the rest.
    pub(super) fn open_fleet(&mut self) {
        let contexts = self.fleet_contexts();
        if contexts.is_empty() {
            self.flash_warn(
                "no fleet contexts — set [fleet] contexts = [\"ctx-a\", …] or mark them with space in :ctx",
            );
            return;
        }
        // Leaving the table view: stop its watches (like the pulse dashboard),
        // stashing its rows so returning to it renders instantly.
        self.bump_generation();
        self.stash_view_snapshot();
        self.store.clear();
        self.invalidate_rows();

        // Seed a "connecting" row per context, resolving each one's read-only
        // policy up front (the CLI flag wins; else the per-context config).
        self.fleet_rows = contexts
            .iter()
            .map(|ctx| {
                let cluster = crate::k8s::cluster_name_for_context(ctx);
                let readonly = self
                    .readonly_override
                    .unwrap_or_else(|| self.config.resolve(ctx, &cluster).config.readonly);
                FleetRow::connecting(ctx.clone(), readonly)
            })
            .collect();
        self.fleet_state.select(Some(0));
        self.flash = format!("fleet — {} contexts", self.fleet_rows.len());
        self.flash_err = false;
        self.mode = Mode::Fleet;
        self.spawn_fleet_gathers();
    }

    fn spawn_fleet_gathers(&mut self) {
        let allow_v1_client_cert = self.cluster.allow_v1_client_cert;
        let no_tls_resumption = self.cluster.no_tls_resumption;
        let sema = Arc::new(tokio::sync::Semaphore::new(FLEET_CONCURRENCY));
        for row in &self.fleet_rows {
            let ctx = row.context.clone();
            let readonly = row.readonly;
            let tx = self.tx.clone();
            let genr = self.generation;
            let sema = sema.clone();
            let handle = tokio::spawn(async move {
                // Bound concurrency: hold a permit for the whole gather.
                let _permit = sema.acquire().await;
                let dur = Duration::from_secs(FLEET_TIMEOUT_SECS);
                let row = match tokio::time::timeout(
                    dur,
                    gather_context(&ctx, readonly, allow_v1_client_cert, no_tls_resumption),
                )
                .await
                {
                    Ok(row) => row,
                    Err(_) => {
                        let mut r = FleetRow::connecting(ctx.clone(), readonly);
                        r.status = FleetStatus::Error("timed out".into());
                        r
                    }
                };
                let _ = tx
                    .send(Msg::FleetRow {
                        generation: genr,
                        row: Box::new(row),
                    })
                    .await;
            });
            self.tasks.push(handle);
        }
    }

    /// Apply a gathered summary to its row (matched by context name).
    pub(super) fn apply_fleet_row(&mut self, row: FleetRow) {
        if let Some(slot) = self
            .fleet_rows
            .iter_mut()
            .find(|r| r.context == row.context)
        {
            *slot = row;
        }
    }

    pub(super) fn key_fleet(&mut self, key: KeyInput) {
        let len = self.fleet_rows.len();
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => self.mode = Mode::Table,
            (Some(Action::Down), _) => list_step(&mut self.fleet_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.fleet_state, len, false),
            (Some(Action::Refresh), _) => {
                // Re-gather: reset rows to connecting, keeping resolved policy.
                for r in &mut self.fleet_rows {
                    *r = FleetRow::connecting(r.context.clone(), r.readonly);
                }
                self.spawn_fleet_gathers();
            }
            // Enter switches to the highlighted context via the normal
            // context-switch path, landing on its default view.
            (Some(Action::Accept), _) => {
                if let Some(ctx) = self
                    .fleet_state
                    .selected()
                    .and_then(|i| self.fleet_rows.get(i))
                    .map(|r| r.context.clone())
                {
                    self.mode = Mode::Table;
                    self.switch_context(ctx);
                }
            }
            _ => {}
        }
    }
}

/// Gather one context's summary: connect, then read version, node readiness,
/// unhealthy pods, and Flux failures. Any connection/auth error becomes an
/// `Error` row rather than propagating.
async fn gather_context(
    ctx: &str,
    readonly: bool,
    allow_v1_client_cert: bool,
    no_tls_resumption: bool,
) -> FleetRow {
    let mut row = FleetRow::connecting(ctx.to_string(), readonly);
    let cluster = match Cluster::connect_context(ctx, allow_v1_client_cert, no_tls_resumption).await
    {
        Ok(c) => c,
        Err(e) => {
            row.status = FleetStatus::Error(short_error(&format!("{e:#}")));
            return row;
        }
    };
    row.version = if cluster.server_version.is_empty() {
        "?".into()
    } else {
        cluster.server_version.clone()
    };
    let client = cluster.client.clone();

    // A failed list must not summarize as "0 unhealthy" — record it and mark
    // the row, keeping whatever partial counts did arrive.
    let mut warn = None;

    if let Some(k) = cluster.resolve("nodes") {
        let nodes = list_or_warn(&client, &k.ar, false, "", &mut warn).await;
        row.nodes_total = nodes.len();
        row.nodes_ready = nodes.iter().filter(|o| node_ready(o)).count();
    }

    if let Some(k) = cluster.resolve("pods") {
        let pods = list_or_warn(&client, &k.ar, k.namespaced, "", &mut warn).await;
        update_pod_counts(&mut row, &pods);
    }

    // Flux failures: only report a count when the toolkit CRDs exist.
    let mut flux_failed = None;
    for kind in ["kustomizations", "helmreleases"] {
        if let Some(k) = cluster.resolve(kind) {
            let items = list_or_warn(&client, &k.ar, k.namespaced, "", &mut warn).await;
            let failed = items.iter().filter(|o| ready_is_false(o)).count();
            *flux_failed.get_or_insert(0) += failed;
        }
    }
    row.flux_failed = flux_failed;

    // Argo CD Applications, counted a page at a time. Unlike the Flux kinds
    // above, a cluster routinely holds thousands of these and each object runs
    // to several KiB, so materialising the whole list the way `list_or_warn`
    // does would pull tens of MiB per context — and the fleet gathers contexts
    // concurrently. Only the count is kept; each page is dropped as it goes.
    if let Some(k) = cluster.resolve_in_group("Application", ARGOCD_GROUP) {
        row.argocd_degraded = count_degraded_applications(&client, &k.ar, &mut warn).await;
    }

    row.status = match warn {
        Some(w) => FleetStatus::Error(short_error(&w)),
        None => FleetStatus::Ok,
    };
    row
}

/// How many Applications to pull per request while counting.
const APPLICATION_PAGE: u32 = 500;

/// Stop after this many pages. At 500 per page that is a quarter of a million
/// Applications, far past any real cluster, and it means a server that keeps
/// handing back a `continue` token cannot spin the gather forever.
const APPLICATION_PAGE_LIMIT: usize = 500;

/// Count Applications that are `OutOfSync` or not `Healthy`, paging so the
/// whole collection is never held at once.
///
/// `None` on a failed read: a denied or timed-out list must not summarise as a
/// confident zero, which is also why the error is recorded in `warn`.
pub(super) async fn count_degraded_applications(
    client: &Client,
    ar: &ApiResource,
    warn: &mut Option<String>,
) -> Option<usize> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), ar);
    let mut degraded = 0;
    let mut token: Option<String> = None;
    for _ in 0..APPLICATION_PAGE_LIMIT {
        let mut params = ListParams::default().limit(APPLICATION_PAGE);
        if let Some(t) = &token {
            params = params.continue_token(t);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(e) => {
                warn.get_or_insert(format!("listing {}: {e}", ar.plural));
                return None;
            }
        };
        degraded += page
            .items
            .iter()
            .filter(|o| application_degraded(o))
            .count();
        token = page.metadata.continue_.filter(|t| !t.is_empty());
        if token.is_none() {
            return Some(degraded);
        }
    }
    warn.get_or_insert(format!("listing {}: too many pages", ar.plural));
    None
}

/// An Application counts against the fleet when it has drifted or is not
/// healthy. An empty status is a freshly created Application Argo has not
/// compared yet, which is not a fault.
pub(super) fn application_degraded(obj: &DynamicObject) -> bool {
    // `Progressing` is a rollout in flight, not a fault, and it flaps often
    // enough to make the count twitch. An empty status is an Application Argo
    // has not compared yet.
    crate::argocd::sync_status(obj) == "OutOfSync"
        || matches!(
            crate::argocd::health_status(obj),
            "Degraded" | "Missing" | "Unknown"
        )
}

pub(super) fn update_pod_counts(row: &mut FleetRow, pods: &[DynamicObject]) {
    row.pods_total = pods.len();
    row.pods_unhealthy = pods.iter().filter(|o| !pod_healthy(o)).count();
}

/// A pod counts as healthy when it isn't terminating and is Running-and-ready
/// or Succeeded.
fn pod_healthy(o: &DynamicObject) -> bool {
    if o.metadata.deletion_timestamp.is_some() {
        return false;
    }
    match phase(o).as_str() {
        "Succeeded" => true,
        "Running" => o
            .data
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.get("type").and_then(Value::as_str) == Some("Ready")
                        && c.get("status").and_then(Value::as_str) == Some("True")
                })
            }),
        _ => false,
    }
}

/// Whether an object carries a `Ready` condition explicitly set to `False`
/// (a failing Flux reconciliation).
fn ready_is_false(o: &DynamicObject) -> bool {
    o.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|cs| {
            cs.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_str) == Some("False")
            })
        })
}

/// First line of a connection error, trimmed for the one-line status cell.
fn short_error(e: &str) -> String {
    crate::text::ellipsize(e.lines().next().unwrap_or(e).trim(), 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(phase: &str, deleting: bool) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web",
                "deletionTimestamp": deleting.then_some("2026-09-07T10:00:00Z"),
            },
            "status": {
                "phase": phase,
                "conditions": [{"type": "Ready", "status": "True"}],
            },
        }))
        .unwrap()
    }

    #[test]
    fn terminating_pods_are_unhealthy() {
        for phase in ["Running", "Succeeded"] {
            assert!(pod_healthy(&pod(phase, false)), "{phase}");
            assert!(!pod_healthy(&pod(phase, true)), "{phase}");
        }
    }
}
