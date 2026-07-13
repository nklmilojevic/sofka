use super::*;

use crate::fleet::{FleetRow, FleetStatus};

/// Never query more than this many contexts at once, so opening the dashboard
/// against a large fleet doesn't open a connection storm.
const FLEET_CONCURRENCY: usize = 4;
/// Per-context budget: a slow or unreachable context fails here instead of
/// hanging the row (the others are unaffected).
const FLEET_TIMEOUT_SECS: u64 = 8;

impl App {
    /// `:fleet` — open the opt-in cross-context health dashboard. Only the
    /// contexts in `[fleet].contexts` are queried; each is gathered off-thread
    /// with its own timeout so one slow context never blocks the rest.
    pub(super) fn open_fleet(&mut self) {
        if self.fleet_cfg.contexts.is_empty() {
            self.flash_warn("no fleet contexts — set [fleet] contexts = [\"ctx-a\", …]");
            return;
        }
        // Leaving the table view: stop its watches (like the pulse dashboard).
        self.bump_generation();
        self.store.clear();
        self.invalidate_rows();

        // Seed a "connecting" row per context, resolving each one's read-only
        // policy up front (the CLI flag wins; else the per-context config).
        self.fleet_rows = self
            .fleet_cfg
            .contexts
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
                let row = match tokio::time::timeout(dur, gather_context(&ctx, readonly)).await {
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

    pub(super) fn key_fleet(&mut self, key: KeyEvent) {
        let len = self.fleet_rows.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.fleet_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.fleet_state, len, false),
            KeyCode::Char('r') => {
                // Re-gather: reset rows to connecting, keeping resolved policy.
                for r in &mut self.fleet_rows {
                    *r = FleetRow::connecting(r.context.clone(), r.readonly);
                }
                self.spawn_fleet_gathers();
            }
            // Enter switches to the highlighted context via the normal
            // context-switch path, landing on its default view.
            KeyCode::Enter => {
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
async fn gather_context(ctx: &str, readonly: bool) -> FleetRow {
    let mut row = FleetRow::connecting(ctx.to_string(), readonly);
    let cluster = match Cluster::connect_context(ctx).await {
        Ok(c) => c,
        Err(e) => {
            row.status = FleetStatus::Error(short_error(&format!("{e:#}")));
            return row;
        }
    };
    let client = cluster.client.clone();

    row.version = match client.apiserver_version().await {
        Ok(info) => info.git_version,
        Err(_) => "?".into(),
    };

    if let Some(k) = cluster.resolve("nodes") {
        let nodes = list_kind(&client, &k.ar, false, "").await;
        row.nodes_total = nodes.len();
        row.nodes_ready = nodes.iter().filter(|o| node_ready(o)).count();
    }

    if let Some(k) = cluster.resolve("pods") {
        let pods = list_kind(&client, &k.ar, k.namespaced, "").await;
        row.pods_total = pods.len();
        row.pods_unhealthy = pods.iter().filter(|o| !pod_healthy(o)).count();
    }

    // Flux failures: only report a count when the toolkit CRDs exist.
    let mut flux_failed = None;
    for kind in ["kustomizations", "helmreleases"] {
        if let Some(k) = cluster.resolve(kind) {
            let items = list_kind(&client, &k.ar, k.namespaced, "").await;
            let failed = items.iter().filter(|o| ready_is_false(o)).count();
            *flux_failed.get_or_insert(0) += failed;
        }
    }
    row.flux_failed = flux_failed;

    row.status = FleetStatus::Ok;
    row
}

/// A pod counts as healthy when it's Running-and-ready or Succeeded.
fn pod_healthy(o: &DynamicObject) -> bool {
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
    let first = e.lines().next().unwrap_or(e).trim();
    if first.len() > 60 {
        format!("{}…", &first[..59])
    } else {
        first.to_string()
    }
}
