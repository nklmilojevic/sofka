use super::*;

use crate::fleet::{FleetRow, FleetStatus};

/// Never query more than this many contexts at once, so opening the dashboard
/// against a large fleet doesn't open a connection storm.
const FLEET_CONCURRENCY: usize = 4;
/// Per-context budget: a slow or unreachable context fails here instead of
/// hanging the row (the others are unaffected).
const FLEET_TIMEOUT_SECS: u64 = 8;

impl App {
    /// The fleet clusters in effect: the `[fleet] contexts` config list plus
    /// clusters marked with `space` in the context switcher, minus config
    /// entries unmarked the same way. Marks persist across restarts (see
    /// [`crate::fleet::FleetMarks`]); the config file is never rewritten.
    ///
    /// Config entries are plain context names, which name a context in the
    /// default kubeconfig; marks carry the kubeconfig too, so a context from
    /// an added file can join the fleet without colliding with a same-named
    /// one elsewhere.
    pub fn fleet_contexts(&self) -> Vec<crate::kubeconfigs::ClusterId> {
        let mut out: Vec<crate::kubeconfigs::ClusterId> = self
            .fleet_cfg
            .contexts
            .iter()
            .filter(|c| !self.fleet_marks.removed.contains(c))
            .map(crate::kubeconfigs::ClusterId::new)
            .collect();
        for c in &self.fleet_marks.added {
            let id = crate::kubeconfigs::ClusterId::from_state_key(c);
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Whether `id` is currently part of the fleet (config + marks).
    ///
    /// Answers the membership question directly rather than materializing
    /// [`Self::fleet_contexts`]: the context switcher asks this once per row
    /// per frame, and building (and cloning into) a `Vec` per row made that
    /// draw quadratic in allocations.
    pub fn is_fleet_context(&self, id: &crate::kubeconfigs::ClusterId) -> bool {
        let key = id.state_key();
        let listed = id.path().is_none()
            && self.fleet_cfg.contexts.contains(&id.context)
            && !self.fleet_marks.removed.contains(&key);
        listed || self.fleet_marks.added.contains(&key)
    }

    /// Toggle a cluster in/out of the fleet (`space` in the context
    /// switcher), saving the marks so the fleet survives restarts.
    pub(super) fn toggle_fleet_context(&mut self, id: &crate::kubeconfigs::ClusterId) {
        let key = id.state_key();
        let label = self.cluster_label(id);
        if self.is_fleet_context(id) {
            self.fleet_marks.added.retain(|c| *c != key);
            // A config-listed context can't be dropped from the config Vec
            // (it comes back on every re-resolve), so it's masked instead.
            if self.fleet_cfg.contexts.contains(&id.context)
                && !self.fleet_marks.removed.contains(&key)
            {
                self.fleet_marks.removed.push(key);
            }
            self.flash = format!("fleet − {label}");
        } else {
            self.fleet_marks.removed.retain(|c| *c != key);
            self.fleet_marks.added.push(key);
            self.flash = format!("fleet + {label}");
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
            .map(|id| {
                let cluster = crate::k8s::cluster_name_for_context(id);
                let readonly = self
                    .readonly_override
                    .unwrap_or_else(|| self.config.resolve(&id.context, &cluster).config.readonly);
                FleetRow::connecting(id.clone(), self.cluster_label(id), readonly)
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
        let sema = Arc::new(tokio::sync::Semaphore::new(FLEET_CONCURRENCY));
        for row in &self.fleet_rows {
            let id = row.id.clone();
            let label = row.label.clone();
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
                    gather_context(&id, label.clone(), readonly, allow_v1_client_cert),
                )
                .await
                {
                    Ok(row) => row,
                    Err(_) => {
                        let mut r = FleetRow::connecting(id.clone(), label, readonly);
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

    /// Apply a gathered summary to its row (matched by cluster).
    pub(super) fn apply_fleet_row(&mut self, row: FleetRow) {
        if let Some(slot) = self.fleet_rows.iter_mut().find(|r| r.id == row.id) {
            *slot = row;
        }
    }

    pub(super) fn key_fleet(&mut self, key: KeyEvent) {
        let len = self.fleet_rows.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.fleet_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.fleet_state, len, false),
            KeyCode::Char('r') => {
                // Re-gather: reset rows to connecting, keeping resolved policy.
                for r in &mut self.fleet_rows {
                    *r = FleetRow::connecting(r.id.clone(), r.label.clone(), r.readonly);
                }
                self.spawn_fleet_gathers();
            }
            // Enter switches to the highlighted cluster via the normal
            // context-switch path, landing on its default view.
            KeyCode::Enter => {
                if let Some(id) = self
                    .fleet_state
                    .selected()
                    .and_then(|i| self.fleet_rows.get(i))
                    .map(|r| r.id.clone())
                {
                    self.mode = Mode::Table;
                    self.switch_context(id);
                }
            }
            _ => {}
        }
    }
}

/// Gather one cluster's summary: connect, then read version, node readiness,
/// unhealthy pods, and Flux failures. Any connection/auth error becomes an
/// `Error` row rather than propagating.
async fn gather_context(
    id: &crate::kubeconfigs::ClusterId,
    label: String,
    readonly: bool,
    allow_v1_client_cert: bool,
) -> FleetRow {
    let mut row = FleetRow::connecting(id.clone(), label, readonly);
    let cluster = match Cluster::connect_context(id, allow_v1_client_cert).await {
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

    row.status = match warn {
        Some(w) => FleetStatus::Error(short_error(&w)),
        None => FleetStatus::Ok,
    };
    row
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
