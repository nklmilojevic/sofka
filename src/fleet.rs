//! Cross-context fleet dashboard model (`:fleet`).
//!
//! An opt-in summary of several kubeconfig contexts at once — connectivity,
//! Kubernetes version, node readiness, unhealthy workloads, Flux failures, and
//! the read-only policy — so you can eyeball a fleet without switching through
//! contexts one at a time. Only explicitly configured contexts are queried, and
//! each is gathered independently so one slow cluster never blocks the rest.
//! Only these non-sensitive summaries are held in memory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Fleet membership edits made with `space` in the context switcher,
/// persisted to `<state-dir>/fleet.toml` so marks survive restarts. This is
/// deliberately a separate state file: sofka never rewrites the user's config,
/// so `[fleet] contexts` stays the hand-edited base list and these marks
/// overlay it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetMarks {
    /// Contexts added on top of `[fleet] contexts`, in toggle order.
    pub added: Vec<String>,
    /// Config-listed contexts masked out with `space`.
    pub removed: Vec<String>,
}

impl FleetMarks {
    /// Where marks live: `<state-dir>/fleet.toml`.
    pub fn default_path() -> PathBuf {
        crate::diagnostics::state_dir().join("fleet.toml")
    }

    /// Load persisted marks. A missing or unparsable file is an empty set —
    /// the dashboard must still open when state was never written or got
    /// hand-mangled.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist to `path`. The file is replaced atomically, so a crash or a
    /// second sofka writing at the same moment cannot leave a torn file that
    /// [`Self::load`] would quietly read as empty.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        crate::atomicfile::write(path, &text)
    }
}

/// Where a context's summary stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FleetStatus {
    /// Still connecting / gathering.
    Connecting,
    /// Reachable and summarized.
    Ok,
    /// Unreachable, unauthenticated, or the gather timed out.
    Error(String),
}

/// One context's summary row.
#[derive(Clone, Debug)]
pub struct FleetRow {
    pub context: String,
    pub status: FleetStatus,
    /// API-server version (`git_version`), empty until known.
    pub version: String,
    pub nodes_ready: usize,
    pub nodes_total: usize,
    /// Pods not `Running`/`Succeeded` (or Running-but-not-ready).
    pub pods_unhealthy: usize,
    pub pods_total: usize,
    /// Flux resources with `Ready=False`; `None` when the cluster has no Flux
    /// toolkit CRDs.
    pub flux_failed: Option<usize>,
    /// The read-only policy resolved for this context.
    pub readonly: bool,
}

impl FleetRow {
    /// A freshly-seeded row shown while the gather is in flight.
    pub fn connecting(context: String, readonly: bool) -> Self {
        FleetRow {
            context,
            status: FleetStatus::Connecting,
            version: String::new(),
            nodes_ready: 0,
            nodes_total: 0,
            pods_unhealthy: 0,
            pods_total: 0,
            flux_failed: None,
            readonly,
        }
    }

    /// Whether the context reads as fully healthy: reachable, every node ready,
    /// no unhealthy pods, and no Flux failures.
    pub fn is_healthy(&self) -> bool {
        self.status == FleetStatus::Ok
            && self.nodes_ready == self.nodes_total
            && self.pods_unhealthy == 0
            && self.flux_failed.unwrap_or(0) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_row() -> FleetRow {
        FleetRow {
            context: "prod".into(),
            status: FleetStatus::Ok,
            version: "v1.30.2".into(),
            nodes_ready: 3,
            nodes_total: 3,
            pods_unhealthy: 0,
            pods_total: 40,
            flux_failed: Some(0),
            readonly: true,
        }
    }

    #[test]
    fn connecting_row_is_not_healthy() {
        let r = FleetRow::connecting("staging".into(), false);
        assert_eq!(r.status, FleetStatus::Connecting);
        assert!(!r.is_healthy());
    }

    #[test]
    fn healthy_requires_ready_nodes_no_bad_pods_no_flux_failures() {
        assert!(ok_row().is_healthy());

        let mut r = ok_row();
        r.nodes_ready = 2; // a node down
        assert!(!r.is_healthy());

        let mut r = ok_row();
        r.pods_unhealthy = 1;
        assert!(!r.is_healthy());

        let mut r = ok_row();
        r.flux_failed = Some(2);
        assert!(!r.is_healthy());
    }

    #[test]
    fn errored_context_is_never_healthy() {
        let mut r = ok_row();
        r.status = FleetStatus::Error("deadline elapsed".into());
        assert!(!r.is_healthy());
    }

    #[test]
    fn no_flux_crds_does_not_count_as_a_failure() {
        let mut r = ok_row();
        r.flux_failed = None;
        assert!(r.is_healthy());
    }
}
