//! In-memory store of the currently-watched resource set.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use kube::core::DynamicObject;

/// Identity of an asynchronous operation's claim on the shared status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusClaim(pub(crate) u64);

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
    /// Pod count per node from the pods poll on the nodes view, keyed by node
    /// name. Counts non-terminated pods, mirroring `kubectl describe node`.
    NodePods {
        generation: u64,
        counts: HashMap<String, usize>,
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
        claim: StatusClaim,
        data: Pulse,
    },
    XrayData {
        generation: u64,
        claim: StatusClaim,
        items: Vec<XrayItem>,
        /// A list that failed during the gather — the tree may be incomplete.
        warn: Option<String>,
    },
    /// The metrics poll loop failed (a broken metrics-server, not an absent
    /// one — an absent API never starts the loop). Cleared by the next
    /// successful [`Msg::Metrics`].
    MetricsError {
        generation: u64,
        error: String,
    },
    /// Findings for the explain-unhealthy view, gathered off-thread.
    Explain {
        generation: u64,
        claim: StatusClaim,
        title: String,
        findings: Vec<crate::explain::Finding>,
    },
    /// Reconciliation-chain findings for the GitOps view, gathered off-thread.
    Gitops {
        generation: u64,
        claim: StatusClaim,
        title: String,
        findings: Vec<crate::explain::Finding>,
    },
    /// Captured output of an `output = "popup"` plugin run.
    PluginOutput {
        generation: u64,
        claim: StatusClaim,
        title: String,
        lines: Vec<String>,
        /// Set when the plugin failed or timed out (a nonzero exit, stderr).
        warn: Option<String>,
    },
    /// Completion notice for an `output = "background"` plugin run (single or
    /// bulk): how many jobs succeeded and the failures (label + reason).
    PluginBulkDone {
        generation: u64,
        claim: StatusClaim,
        name: String,
        ok: usize,
        failed: Vec<String>,
    },
    /// Result of an off-thread `kubectl describe` (or its YAML fallback).
    Detail {
        generation: u64,
        claim: StatusClaim,
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
        claim: StatusClaim,
        result: Result<String, String>,
    },
    /// Result of an off-thread log save.
    LogsSaved {
        generation: u64,
        claim: StatusClaim,
        result: Result<std::path::PathBuf, String>,
    },
    /// Result of an off-thread clipboard copy.
    ClipboardCopied {
        generation: u64,
        claim: StatusClaim,
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
    /// Result of an off-thread `kubectl config rename-context` (`r` in the
    /// context switcher).
    ContextRenamed {
        generation: u64,
        claim: StatusClaim,
        old: String,
        new: String,
        result: Result<(), String>,
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
        claim: StatusClaim,
        deleted: usize,
        failed: Vec<String>,
    },
    /// An assembled diagnostic bundle (`:bundle`), ready to preview and save.
    Bundle {
        generation: u64,
        claim: StatusClaim,
        title: String,
        text: String,
        /// Suggested filename for `:bundle-save`.
        filename: String,
    },
    /// Result of writing a bundle to disk (`:bundle-save`).
    BundleSaved {
        generation: u64,
        claim: StatusClaim,
        result: Result<std::path::PathBuf, String>,
    },
    /// Result of writing a snapshot to disk (`:snapshot`).
    SnapshotSaved {
        generation: u64,
        claim: StatusClaim,
        result: Result<std::path::PathBuf, String>,
    },
    /// One context's summary for the fleet dashboard (`:fleet`), arriving
    /// independently so a slow context never blocks the rest.
    FleetRow {
        generation: u64,
        row: Box<crate::fleet::FleetRow>,
    },
    /// Results of a `:find <text>` sweep across kinds.
    FindResults {
        generation: u64,
        claim: StatusClaim,
        query: String,
        items: Vec<FindItem>,
        /// Kinds that failed to list — the results may be incomplete.
        warn: Option<String>,
    },
    Error {
        generation: u64,
        error: String,
    },
    /// A background action (delete, restart, scale, drain, helm op, …)
    /// finished; replaces its "…ing" progress flash with a result. Also
    /// carries `:can-i` verdicts, which are the same thing: a one-line answer
    /// from an off-thread task.
    Flash {
        generation: u64,
        claim: StatusClaim,
        message: String,
        /// Render in the error style. This covers both action failures and a
        /// `:can-i` denial, which is an answer rather than a watch error but
        /// still wants to read as a "no".
        err: bool,
    },
    /// A panic in a background task, reported by the process panic hook.
    /// Deliberately generation-free: it must surface no matter which view is
    /// current.
    Panic(String),
    /// A state change on a `:notify`-watched object. Generation-free like
    /// [`Msg::Panic`]: the whole point is firing from any view.
    Notify(String),
}

/// One hit from the global fuzzy find (`:find <text>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindItem {
    pub plural: String,
    pub ns: String,
    pub name: String,
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
    /// A list that failed during the gather — the tiles above under-count.
    pub warn: Option<String>,
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

/// The store's object map. Objects are behind an `Arc` because they are
/// genuinely shared: the live store, the view-cache snapshot for the same
/// scope, and `prev_revisions` all want the same bytes. Cloning used to mean
/// deep-copying a whole `serde_json::Value` per object — the single largest
/// contributor to RSS and to per-event cost. Nothing mutates an object once
/// stored (`apply` replaces wholesale), so sharing is safe.
pub type RowKey = Rc<str>;
pub type Items = HashMap<RowKey, Arc<DynamicObject>>;

/// How a store operation affected the rows currently visible to the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreMutation {
    Buffered,
    Inserted,
    Updated,
    Removed,
    Unchanged,
}

#[derive(Default)]
pub struct Store {
    items: Items,
    /// Bumped by every mutation. Lets a derived view (the Helm latest-revision
    /// dedup) cache its result and reuse it across rebuilds that were staled
    /// by a filter or sort change rather than by the store moving.
    version: u64,
    /// Fresh rows accumulating during a (re)list while `items` still shows the
    /// previous set — a cached view snapshot or the pre-relist state. Swapped
    /// in wholesale on `Synced`, so stale rows are replaced atomically instead
    /// of the table blanking out while the initial list streams in.
    pending: Option<Items>,
    pub synced: bool,
}

impl Store {
    pub fn clear(&mut self) {
        self.version += 1;
        self.items.clear();
        self.pending = None;
        self.synced = false;
    }

    /// Replace the contents with a cached snapshot from a previous visit to
    /// this view — shown (unsynced) until the new watch's initial list lands.
    pub fn seed(&mut self, items: Items) {
        self.version += 1;
        self.items = items;
        self.pending = None;
        self.synced = false;
    }

    /// Move the items out (for stashing in the view cache), leaving the store
    /// empty.
    pub fn take_items(&mut self) -> Items {
        self.version += 1;
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
        self.version += 1;
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
        self.version += 1;
        self.synced = true;
        match self.pending.take() {
            Some(fresh) => {
                self.items = fresh;
                true
            }
            None => false,
        }
    }

    pub fn apply(&mut self, key: String, obj: DynamicObject) -> StoreMutation {
        self.version += 1;
        let key: RowKey = key.into();
        let obj = Arc::new(obj);
        match &mut self.pending {
            Some(pending) => {
                pending.insert(key, obj);
                StoreMutation::Buffered
            }
            None => match self.items.insert(key, obj) {
                Some(_) => StoreMutation::Updated,
                None => StoreMutation::Inserted,
            },
        }
    }

    pub fn remove(&mut self, key: &str) -> StoreMutation {
        self.version += 1;
        match &mut self.pending {
            Some(pending) => {
                pending.remove(key);
                StoreMutation::Buffered
            }
            None => match self.items.remove(key) {
                Some(_) => StoreMutation::Removed,
                None => StoreMutation::Unchanged,
            },
        }
    }

    /// The newest known version of `key`: the in-flight buffered one during a
    /// reset, else the visible one. Used as the "previous version" for
    /// timeline diffs, where [`Self::get`]'s stale visible copy would be wrong
    /// if the same object came through the buffer twice.
    pub fn latest(&self, key: &str) -> Option<&Arc<DynamicObject>> {
        self.pending
            .as_ref()
            .and_then(|p| p.get(key))
            .or_else(|| self.items.get(key))
    }

    /// Monotonic mutation counter — see [`Self::version`]'s field docs.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&DynamicObject> {
        self.items.get(key).map(AsRef::as_ref)
    }

    pub fn key(&self, key: &str) -> Option<&RowKey> {
        self.items.get_key_value(key).map(|(key, _)| key)
    }

    /// An object together with the canonical key it is stored under, so a
    /// caller that needs both does not have to rebuild the key it looked the
    /// object up with.
    pub fn entry(&self, key: &str) -> Option<(&RowKey, &DynamicObject)> {
        self.items.get_key_value(key).map(|(k, v)| (k, v.as_ref()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RowKey, &DynamicObject)> {
        self.items.iter().map(|(k, v)| (k, v.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str) -> DynamicObject {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name, "namespace": "default" },
        }))
        .expect("pod fixture")
    }

    /// Every mutating path must advance the counter, because the Helm
    /// latest-revision dedup is cached against it: a path that changed `items`
    /// without bumping would leave that cache selecting rows for a store it no
    /// longer describes. `items` is private and hands out no `&mut`, so these
    /// seven methods are the complete set of ways it can change.
    #[test]
    fn every_mutation_advances_the_version() {
        fn advanced(store: &Store, last: &mut u64, what: &str) {
            assert!(
                store.version() > *last,
                "{what} must advance the store version ({} -> {})",
                *last,
                store.version()
            );
            *last = store.version();
        }

        let mut store = Store::default();
        let mut last = store.version();

        store.apply("default/a".into(), pod("a"));
        advanced(&store, &mut last, "apply");

        store.remove("default/a");
        advanced(&store, &mut last, "remove");

        // Conservative: a remove that matched nothing still bumps. Over-
        // bumping only costs a recompute; under-bumping serves stale rows.
        store.remove("default/gone");
        advanced(&store, &mut last, "remove (no such key)");

        let mut seeded = Items::new();
        seeded.insert(Rc::from("default/b"), Arc::new(pod("b")));
        store.seed(seeded);
        advanced(&store, &mut last, "seed");

        // Non-empty store, so this buffers rather than clearing.
        assert!(!store.begin_reset(), "seeded store buffers the relist");
        advanced(&store, &mut last, "begin_reset");

        store.apply("default/c".into(), pod("c"));
        advanced(&store, &mut last, "apply (buffered during relist)");

        assert!(store.finish_sync(), "the buffered set is swapped in");
        advanced(&store, &mut last, "finish_sync");

        store.take_items();
        advanced(&store, &mut last, "take_items");

        store.clear();
        advanced(&store, &mut last, "clear");
    }
}
