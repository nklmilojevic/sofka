//! In-memory store of the currently-watched resource set.

use std::collections::HashMap;

use kube::core::DynamicObject;

/// Messages flowing from watch tasks to the UI loop. Tagged with a
/// `generation` so messages from a superseded watch can be discarded.
pub enum Msg {
    Reset {
        generation: u64,
    },
    Applied {
        generation: u64,
        key: String,
        obj: Box<DynamicObject>,
    },
    Deleted {
        generation: u64,
        key: String,
    },
    Synced {
        generation: u64,
    },
    LogLines {
        generation: u64,
        lines: Vec<String>,
    },
    /// Point-in-time usage snapshot from the metrics API, keyed by "ns/name"
    /// (pods) or "name" (nodes) -> (cpu millicores, memory bytes).
    Metrics {
        generation: u64,
        data: HashMap<String, (i64, i64)>,
        /// Per-container usage keyed by `namespace/pod/container`.
        containers: HashMap<String, (i64, i64)>,
    },
    /// CRD `additionalPrinterColumns` fallback for a custom-resource plural,
    /// fetched off-thread (`None` = CRD had nothing usable for the version).
    PrinterColumns {
        generation: u64,
        plural: String,
        view: Box<Option<crate::views::View>>,
    },
    PulseData {
        generation: u64,
        data: Pulse,
    },
    XrayData {
        generation: u64,
        items: Vec<XrayItem>,
    },
    /// Findings for the explain-unhealthy view, gathered off-thread.
    Explain {
        generation: u64,
        title: String,
        findings: Vec<crate::explain::Finding>,
    },
    /// Result of a `:can-i <verb> <resource>` access review, shown as a flash.
    CanIResult {
        generation: u64,
        text: String,
        ok: bool,
    },
    /// Reconciliation-chain findings for the GitOps view, gathered off-thread.
    Gitops {
        generation: u64,
        title: String,
        findings: Vec<crate::explain::Finding>,
    },
    /// Captured output of an `output = "popup"` plugin run.
    PluginOutput {
        generation: u64,
        title: String,
        lines: Vec<String>,
        /// Set when the plugin failed or timed out (a nonzero exit, stderr).
        warn: Option<String>,
    },
    /// Completion notice for an `output = "background"` plugin run (single or
    /// bulk): how many jobs succeeded and the failures (label + reason).
    PluginBulkDone {
        generation: u64,
        name: String,
        ok: usize,
        failed: Vec<String>,
    },
    /// Result of an off-thread `kubectl describe` (or its YAML fallback).
    Detail {
        generation: u64,
        title: String,
        lines: Vec<String>,
        /// Set when describe failed and we fell back to YAML.
        warn: Option<String>,
    },
    /// Live Event rows for the selected object.
    Events {
        generation: u64,
        title: String,
        lines: Vec<String>,
    },
    /// Result of a background `kubectl cp` transfer (`t` on a pod): a
    /// "copied …" summary, or kubectl's error.
    TransferDone {
        generation: u64,
        result: Result<String, String>,
    },
    /// Result of an off-thread log save.
    LogsSaved {
        generation: u64,
        result: Result<std::path::PathBuf, String>,
    },
    /// Result of an off-thread clipboard copy.
    ClipboardCopied {
        generation: u64,
        copied: bool,
        success: String,
        failure: String,
    },
    /// Namespace list for the switcher, fetched off-thread.
    Namespaces {
        generation: u64,
        list: Vec<String>,
    },
    /// Kubeconfig context names for the switcher, fetched off-thread.
    Contexts {
        generation: u64,
        list: Vec<String>,
    },
    /// Result of an off-thread context switch (rebuilds client + discovery).
    ContextSwitched {
        generation: u64,
        name: String,
        result: Result<Box<crate::k8s::Cluster>, String>,
    },
    /// Resource plurals the user may `list`, computed for namespace `ns`
    /// (empty = cluster default). Dropped if the active namespace has since
    /// changed. "*" = all.
    Rbac {
        generation: u64,
        ns: String,
        allowed: std::collections::HashSet<String>,
    },
    /// A log provider autodiscovered in the cluster (no `[providers.logs]`
    /// url configured), cached so later `L` presses skip the service lookup.
    /// Tagged with the view generation: a context switch invalidates it.
    LogProviderDiscovered {
        generation: u64,
        provider: Box<crate::providers::LogProvider>,
    },
    /// Result of a `:debug-clean` node-debugger cleanup: how many pods were
    /// deleted and any per-pod failures (`ns/name: reason`).
    DebuggersCleaned {
        generation: u64,
        deleted: usize,
        failed: Vec<String>,
    },
    /// An assembled diagnostic bundle (`:bundle`), ready to preview and save.
    Bundle {
        generation: u64,
        title: String,
        text: String,
        /// Suggested filename for `:bundle-save`.
        filename: String,
    },
    /// Result of writing a bundle to disk (`:bundle-save`).
    BundleSaved {
        generation: u64,
        result: Result<std::path::PathBuf, String>,
    },
    /// Result of writing a snapshot to disk (`:snapshot`).
    SnapshotSaved {
        generation: u64,
        result: Result<std::path::PathBuf, String>,
    },
    /// One context's summary for the fleet dashboard (`:fleet`), arriving
    /// independently so a slow context never blocks the rest.
    FleetRow {
        generation: u64,
        row: Box<crate::fleet::FleetRow>,
    },
    Error {
        generation: u64,
        error: String,
    },
}

/// Cluster-health snapshot for the pulse dashboard.
#[derive(Clone, Default)]
pub struct Pulse {
    pub nodes_ready: usize,
    pub nodes_total: usize,
    pub pods_running: usize,
    pub pods_pending: usize,
    pub pods_failed: usize,
    pub pods_succeeded: usize,
    pub pods_total: usize,
    pub deploys_ready: usize,
    pub deploys_total: usize,
    pub sts_ready: usize,
    pub sts_total: usize,
    pub ds_ready: usize,
    pub ds_total: usize,
    pub jobs_total: usize,
    pub pvc_bound: usize,
    pub pvc_total: usize,
}

/// A flattened node in the xray tree (owner → children → containers).
#[derive(Clone)]
pub struct XrayItem {
    pub depth: usize,
    pub kind: String,
    pub name: String,
    pub ns: String,
    pub status: String,
    /// Set when this row is a container leaf (its pod is `name`).
    pub container: Option<String>,
}

/// Stable identity for a resource row.
pub fn row_key(obj: &DynamicObject) -> String {
    match (&obj.metadata.namespace, &obj.metadata.name) {
        (Some(ns), Some(name)) => format!("{ns}/{name}"),
        (None, Some(name)) => name.clone(),
        _ => obj
            .metadata
            .uid
            .clone()
            .unwrap_or_else(|| "<unknown>".into()),
    }
}

#[derive(Default)]
pub struct Store {
    items: HashMap<String, DynamicObject>,
    /// Fresh rows accumulating during a (re)list while `items` still shows the
    /// previous set — a cached view snapshot or the pre-relist state. Swapped
    /// in wholesale on `Synced`, so stale rows are replaced atomically instead
    /// of the table blanking out while the initial list streams in.
    pending: Option<HashMap<String, DynamicObject>>,
    pub synced: bool,
}

impl Store {
    pub fn clear(&mut self) {
        self.items.clear();
        self.pending = None;
        self.synced = false;
    }

    /// Replace the contents with a cached snapshot from a previous visit to
    /// this view — shown (unsynced) until the new watch's initial list lands.
    pub fn seed(&mut self, items: HashMap<String, DynamicObject>) {
        self.items = items;
        self.pending = None;
        self.synced = false;
    }

    /// Move the items out (for stashing in the view cache), leaving the store
    /// empty.
    pub fn take_items(&mut self) -> HashMap<String, DynamicObject> {
        self.pending = None;
        self.synced = false;
        std::mem::take(&mut self.items)
    }

    /// Handle a watch (re)list starting. With rows on screen (a seeded cache
    /// snapshot, or an established watch relisting) the incoming list is
    /// buffered so they stay visible until `finish_sync` swaps it in; an empty
    /// store keeps the old behavior of applying rows as they stream in.
    /// Returns whether the visible items were cleared.
    pub fn begin_reset(&mut self) -> bool {
        self.synced = false;
        if self.items.is_empty() {
            self.pending = None;
            true
        } else {
            self.pending = Some(HashMap::new());
            false
        }
    }

    /// Mark the initial list complete, swapping in the buffered rows if a
    /// reset was in progress. Returns whether a swap replaced the visible set.
    pub fn finish_sync(&mut self) -> bool {
        self.synced = true;
        match self.pending.take() {
            Some(fresh) => {
                self.items = fresh;
                true
            }
            None => false,
        }
    }

    pub fn apply(&mut self, key: String, obj: DynamicObject) {
        match &mut self.pending {
            Some(pending) => pending.insert(key, obj),
            None => self.items.insert(key, obj),
        };
    }

    pub fn remove(&mut self, key: &str) {
        match &mut self.pending {
            Some(pending) => pending.remove(key),
            None => self.items.remove(key),
        };
    }

    /// The newest known version of `key`: the in-flight buffered one during a
    /// reset, else the visible one. Used as the "previous version" for
    /// timeline diffs, where [`Self::get`]'s stale visible copy would be wrong
    /// if the same object came through the buffer twice.
    pub fn latest(&self, key: &str) -> Option<&DynamicObject> {
        self.pending
            .as_ref()
            .and_then(|p| p.get(key))
            .or_else(|| self.items.get(key))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, key: &str) -> Option<&DynamicObject> {
        self.items.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &DynamicObject)> {
        self.items.iter()
    }
}
