//! Application state and input handling.
//!
//! Navigation is a breadcrumb stack: `:cmd` pushes a fresh root view, `enter`
//! drills into a child (workload -> pods, pod -> containers, namespace ->
//! re-scope the previous view), and `esc` pops back.

use std::cell::{Ref, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, DeleteParams, EvictParams, ListParams, LogParams, Patch, PatchParams};
use kube::core::{DynamicObject, TypeMeta};
use kube::discovery::ApiResource;
use kube::runtime::watcher;
use ratatui::widgets::{ListState, TableState};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use crate::k8s::{Cluster, Kind};
use crate::store::{Msg, Pulse, Store, XrayItem, row_key};

/// Maximum number of buffered log lines while following. A chatty pod would
/// otherwise grow the buffer (and the unbounded channel feeding it) without
/// limit; we keep the most recent lines, like k9s' tail buffer.
const MAX_LOG_LINES: usize = 5_000;

/// Larger cap used while autoscroll is paused: we stop trimming so the line
/// indices don't shift under the frozen view (which would make it appear to
/// resume scrolling). Only a runaway firehose during a very long pause hits
/// this; resuming follow trims back to [`MAX_LOG_LINES`].
const MAX_LOG_LINES_PAUSED: usize = 100_000;

/// Log streams batch lines before sending them through the UI channel. This
/// avoids one wake-up/message per line under high-volume workloads while still
/// flushing quickly for low-volume logs.
const LOG_BATCH_LINES: usize = 64;
const LOG_BATCH_MS: u64 = 50;

/// Flux CD resource kinds whose spec has a `suspend: bool` field — every kind
/// with a corresponding `flux suspend/resume` subcommand: kustomize- and
/// helm-controller reconcilers, source-controller fetchers, image-automation
/// controllers, and the notification-controller kinds that support it.
const FLUX_SUSPENDABLE_KINDS: &[&str] = &[
    "kustomizations",
    "helmreleases",
    "gitrepositories",
    "helmrepositories",
    "ocirepositories",
    "buckets",
    "imagerepositories",
    "imageupdateautomations",
    "alerts",
    "receivers",
];

/// Items in the Flux action menu (`t`), in display order. Deliberately a menu
/// — not a single-key toggle — so suspending something always takes an
/// explicit, visible choice rather than one accidental keystroke. "Reconcile
/// now" patches the same `reconcile.fluxcd.io/requestedAt` annotation the
/// `flux reconcile` CLI uses, shared by every controller in the toolkit.
pub const FLUX_MENU_ITEMS: &[&str] = &["Suspend", "Resume", "Reconcile now", "Cancel"];

/// External Secrets Operator kinds that honour the `force-sync` annotation to
/// trigger an immediate secret refresh. Both are namespaced; the cluster-scoped
/// `ClusterExternalSecret` is deliberately left out so the namespaced patch
/// path stays correct.
const EXTERNAL_SECRET_KINDS: &[&str] = &["externalsecrets", "pushsecrets"];

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    Table,
    Command,
    Filter,
    Detail,
    Logs,
    LogFilter,
    Help,
    Namespaces,
    Contexts,
    Containers,
    SetImage,
    Confirm,
    Prompt,
    Pulse,
    Xray,
    Diff,
    Events,
    FluxMenu,
    PortForwards,
    Skins,
}

/// A request for the run loop to suspend the TUI and run an interactive
/// command (exec, edit, port-forward), then resume.
pub enum Suspend {
    Shell(Vec<String>),
}

/// A `kubectl port-forward` running in the background (not `Suspend::Shell`
/// — a forward is meant to keep running while you go do other things, unlike
/// exec/edit which are inherently foreground-interactive). Killed on drop so
/// a quit (or panic-unwind) never leaves an orphaned `kubectl` holding the
/// local port open.
pub struct PortForward {
    ns: String,
    target: String,
    ports: String,
    child: tokio::process::Child,
}

impl PortForward {
    pub fn label(&self) -> String {
        format!("{} {} -n {}", self.target, self.ports, self.ns)
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

enum ConfirmAction {
    /// One or more `(name, ns)` targets to delete (bulk when marked).
    Delete {
        targets: Vec<(String, String)>,
        force: bool,
    },
    /// One or more node names to cordon and drain.
    Drain { targets: Vec<String> },
}

/// What the logs view is currently streaming, so it can be re-streamed when
/// toggling timestamps (k9s `t`).
#[derive(Clone)]
enum LogSource {
    /// Every container of one pod.
    Pod {
        ns: String,
        name: String,
        containers: Vec<String>,
    },
    /// All pods matching a label selector (aggregated workload logs).
    Selector { ns: String, labels: String },
    /// A single container (container picker / previous logs).
    Single {
        ns: String,
        pod: String,
        container: Option<String>,
        previous: bool,
    },
}

enum PromptKind {
    Scale {
        ns: String,
        name: String,
    },
    PortForward {
        ns: String,
        name: String,
    },
    SetImage {
        ns: String,
        name: String,
        plural: String,
        container: String,
    },
}

pub struct Scrollable {
    pub title: String,
    pub lines: VecDeque<String>,
    /// Scroll offset in display rows. `usize` on purpose: a paused log buffer
    /// (100k lines, wrapped) far exceeds `u16`; views that hand this to a
    /// ratatui `Paragraph` clamp at the edge instead.
    pub scroll: usize,
}

/// One command-palette suggestion — either a built-in command (`:ctx`, `:pulse`)
/// or a resource kind from the catalog. Both are fuzzy-matched together.
#[derive(Clone)]
pub struct Suggestion {
    pub label: String,
    pub kind: SuggestKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SuggestKind {
    Command,
    Resource,
}

/// A built-in palette action, plus the names/aliases that select it. The first
/// name is the canonical label shown in the suggestion list; every name is
/// fuzzy-matched and accepted on Enter. Single source of truth for both the
/// suggestions and dispatch.
struct PaletteCommand {
    action: PaletteAction,
    names: &'static [&'static str],
}

#[derive(Clone, Copy)]
enum PaletteAction {
    Quit,
    Ctx,
    Pulse,
    Xray,
    Diff,
    Events,
    PortForwards,
    Skin,
}

const PALETTE_COMMANDS: &[PaletteCommand] = &[
    PaletteCommand {
        action: PaletteAction::Ctx,
        names: &["ctx", "context", "contexts"],
    },
    PaletteCommand {
        action: PaletteAction::Pulse,
        names: &["pulse", "dashboard", "pu"],
    },
    PaletteCommand {
        action: PaletteAction::Xray,
        names: &["xray", "x"],
    },
    PaletteCommand {
        action: PaletteAction::Diff,
        names: &["diff"],
    },
    PaletteCommand {
        action: PaletteAction::Events,
        names: &["events", "event"],
    },
    PaletteCommand {
        action: PaletteAction::PortForwards,
        names: &["pf", "portforwards", "forwards"],
    },
    PaletteCommand {
        action: PaletteAction::Skin,
        names: &["skin", "skins"],
    },
    PaletteCommand {
        action: PaletteAction::Quit,
        names: &["quit", "q", "q!"],
    },
];

impl Scrollable {
    fn empty() -> Self {
        Self {
            title: String::new(),
            lines: VecDeque::new(),
            scroll: 0,
        }
    }
    pub fn scroll_by(&mut self, delta: i32) {
        let max = self.lines.len().saturating_sub(1) as i64;
        self.scroll = (self.scroll as i64 + delta as i64).clamp(0, max) as usize;
    }
}

/// All state for the streaming logs view, grouped so it doesn't sprawl across
/// the top-level `App` struct.
pub struct LogsView {
    pub view: Scrollable,
    pub follow: bool,
    pub filter: String,
    pub wrap: bool,
    pub timestamps: bool,
    pub stopped: bool,
    /// Total rendered rows (post-wrap, post-filter) and inner viewport height
    /// from the last draw. Recorded so key handlers clamp the scroll in the
    /// same *display-row* units the renderer uses — otherwise a wrapped buffer
    /// (rows ≫ lines) makes a pause-then-scroll jump to a stale offset.
    pub viewport_rows: usize,
    pub viewport_h: usize,
    /// Wrap width used at the last draw (0 = wrap off). Lets the message
    /// handler convert trimmed *lines* into the display *rows* they occupied
    /// when shifting a paused scroll anchor.
    pub last_wrap_width: usize,
    /// What is being streamed, so it can be re-streamed (e.g. toggling
    /// timestamps) without re-deriving the source.
    source: Option<LogSource>,
}

impl Default for LogsView {
    fn default() -> Self {
        Self {
            view: Scrollable::empty(),
            follow: true,
            filter: String::new(),
            wrap: false,
            timestamps: false,
            stopped: false,
            viewport_rows: 0,
            viewport_h: 0,
            last_wrap_width: 0,
            source: None,
        }
    }
}

/// A comparable value for one cell, so columns sort numerically where it makes
/// sense (RESTARTS, CPU, AGE…) and lexically otherwise (NAME, STATUS…).
enum SortKey {
    Num(f64),
    Text(String),
}

impl SortKey {
    fn cmp_to(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            // Mixed kinds shouldn't occur within one column; keep it stable.
            (SortKey::Num(_), SortKey::Text(_)) => Ordering::Less,
            (SortKey::Text(_), SortKey::Num(_)) => Ordering::Greater,
        }
    }
}

/// Lazily-rebuilt cache of the display-ordered, filtered row keys. Recomputing
/// the sort + fuzzy filter on every `rows()` call (per frame, per keystroke) is
/// wasteful on large clusters; we rebuild only when the store or filter changes.
#[derive(Default)]
struct RowsCache {
    dirty: bool,
    keys: Vec<String>,
    cells: HashMap<String, CellCacheEntry>,
}

struct CellCacheEntry {
    plural: String,
    resource_version: Option<String>,
    cells: Vec<String>,
    status_idx: Option<usize>,
}

pub(crate) struct TableCellCache<'a> {
    cache: Ref<'a, RowsCache>,
}

impl TableCellCache<'_> {
    pub(crate) fn get(&self, key: &str) -> Option<(&[String], Option<usize>)> {
        self.cache
            .cells
            .get(key)
            .map(|entry| (entry.cells.as_slice(), entry.status_idx))
    }
}

/// A saved view, pushed onto the stack when drilling down.
struct Frame {
    kind: Option<Kind>,
    kind_plural: String,
    namespace: String,
    labels: Option<String>,
    fields: Option<String>,
    filter: String,
    scope_label: Option<String>,
    selected: Option<usize>,
}

pub struct App {
    pub cluster: Cluster,
    pub store: Store,
    pub kind: Option<Kind>,
    pub kind_plural: String,
    /// Active namespace; empty string means "all namespaces".
    pub namespace: String,
    pub labels: Option<String>,
    pub fields: Option<String>,
    /// Drill-down breadcrumb shown in the header, e.g. "deploy/foo".
    pub scope_label: Option<String>,

    pub generation: u64,
    gen_flag: Arc<AtomicU64>,
    pub tasks: Vec<JoinHandle<()>>,
    pub tx: Sender<Msg>,
    stack: Vec<Frame>,

    pub mode: Mode,
    pub table_state: TableState,
    /// Row keys (`ns/name`) marked for bulk actions via SPACE. Cleared whenever
    /// the view is (re)watched. Bulk actions target this set if non-empty, else
    /// the current selection.
    pub marked: HashSet<String>,
    /// Column index (into the displayed headers) to sort the table by, or
    /// `None` for the natural namespace/name order.
    pub sort_column: Option<usize>,
    pub sort_desc: bool,
    pub filter: String,
    pub command: String,
    pub cmd_suggestions: Vec<Suggestion>,
    pub cmd_sel: usize,
    pub flash: String,
    pub flash_err: bool,

    pub detail: Scrollable,
    pub logs: LogsView,

    pub ns_list: Vec<String>,
    pub ns_state: ListState,
    /// Type-to-filter buffer for the namespace switcher; also accepted verbatim
    /// (freeform) so you can switch to a namespace that isn't listed (e.g. when
    /// cluster-wide namespace listing is restricted).
    pub ns_filter: String,

    pub ctx_list: Vec<String>,
    pub ctx_state: ListState,
    /// Type-to-filter buffer for the context switcher.
    pub ctx_filter: String,
    /// User aliases from config, re-applied when switching context.
    pub user_aliases: HashMap<String, String>,
    /// User-defined shell-out plugins.
    pub plugins: Vec<crate::config::Plugin>,
    /// Resource plurals the user may list (None = unknown/all). "*" = all.
    rbac_allowed: Option<HashSet<String>>,
    last_rbac_ns: Option<String>,

    pub container_list: Vec<String>,
    pub container_state: ListState,
    container_pod: Option<(String, String)>, // (ns, name)

    /// Cursor into [`FLUX_MENU_ITEMS`] for the Flux suspend/resume menu.
    pub flux_menu_state: ListState,

    /// Background `kubectl port-forward` processes started with `f`/`F`.
    /// Viewed/stopped via `:pf`; killed automatically on drop.
    pub port_forwards: Vec<PortForward>,
    pub pf_state: ListState,

    pub skin_list: Vec<String>,
    pub skin_state: ListState,
    /// Per-swatch color overrides from config, re-applied when switching skins.
    pub skin_colors: HashMap<String, String>,

    /// Current images aligned with `container_list`, for the Set-Image picker.
    pub image_values: Vec<String>,
    /// (namespace, name, plural) of the object being re-imaged.
    image_target: Option<(String, String, String)>,

    /// Latest metrics snapshot: "ns/name" (pods) or "name" (nodes) -> (cpu_m, mem_bytes).
    pub metrics: HashMap<String, (i64, i64)>,

    pub pulse: Pulse,
    pub xray_items: Vec<XrayItem>,
    pub xray_state: ListState,

    pub confirm_label: String,
    confirm_action: Option<ConfirmAction>,
    pub prompt_label: String,
    pub prompt_input: String,
    prompt_kind: Option<PromptKind>,

    /// Independent lifecycle for log streams so opening logs doesn't tear down
    /// (and later reload) the underlying table/xray view. Tagged separately from
    /// the view `generation` so log lines can be invalidated on their own.
    log_gen: u64,
    log_flag: Arc<AtomicU64>,
    log_tasks: Vec<JoinHandle<()>>,
    event_gen: u64,
    event_task: Option<JoinHandle<()>>,

    pub pending: Option<Suspend>,
    /// Mode to return to when leaving a transient view (logs/detail/diff).
    return_mode: Mode,
    /// Row key (ns/name) selected when a transient view was opened, restored on
    /// return so the cursor lands back on the same object.
    return_selection: Option<String>,
    pub should_quit: bool,
    matcher: SkimMatcherV2,
    rows_cache: RefCell<RowsCache>,
}

impl App {
    pub fn new(cluster: Cluster, tx: Sender<Msg>) -> Self {
        let namespace = cluster.default_namespace.clone();
        Self {
            cluster,
            store: Store::default(),
            kind: None,
            kind_plural: String::new(),
            namespace,
            labels: None,
            fields: None,
            scope_label: None,
            generation: 0,
            gen_flag: Arc::new(AtomicU64::new(0)),
            tasks: Vec::new(),
            tx,
            stack: Vec::new(),
            mode: Mode::Table,
            table_state: TableState::default(),
            marked: HashSet::new(),
            sort_column: None,
            sort_desc: false,
            filter: String::new(),
            command: String::new(),
            cmd_suggestions: Vec::new(),
            cmd_sel: 0,
            flash: "Welcome to sofka — ':' resource · enter drill · d describe · l logs · ? help"
                .into(),
            flash_err: false,
            detail: Scrollable::empty(),
            logs: LogsView::default(),
            ns_list: Vec::new(),
            ns_state: ListState::default(),
            ns_filter: String::new(),
            ctx_list: Vec::new(),
            ctx_state: ListState::default(),
            ctx_filter: String::new(),
            user_aliases: HashMap::new(),
            plugins: Vec::new(),
            rbac_allowed: None,
            last_rbac_ns: None,
            container_list: Vec::new(),
            container_state: ListState::default(),
            container_pod: None,
            flux_menu_state: ListState::default(),
            port_forwards: Vec::new(),
            pf_state: ListState::default(),
            skin_list: crate::theme::BUILTIN_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            skin_state: ListState::default(),
            skin_colors: HashMap::new(),
            image_values: Vec::new(),
            image_target: None,
            metrics: HashMap::new(),
            pulse: Pulse::default(),
            xray_items: Vec::new(),
            xray_state: ListState::default(),
            confirm_label: String::new(),
            confirm_action: None,
            prompt_label: String::new(),
            prompt_input: String::new(),
            prompt_kind: None,
            log_gen: 0,
            log_flag: Arc::new(AtomicU64::new(0)),
            log_tasks: Vec::new(),
            event_gen: 0,
            event_task: None,
            pending: None,
            return_mode: Mode::Table,
            return_selection: None,
            should_quit: false,
            matcher: SkimMatcherV2::default(),
            rows_cache: RefCell::new(RowsCache {
                dirty: true,
                keys: Vec::new(),
                cells: HashMap::new(),
            }),
        }
    }

    pub fn all_namespaces(&self) -> bool {
        self.namespace.is_empty()
    }

    // ----- navigation ----------------------------------------------------

    /// Switch the active resource kind by user input. Pushes the current view
    /// so `esc` can return.
    pub fn switch_kind(&mut self, input: &str) {
        match self.cluster.resolve(input) {
            Some(kind) => {
                // A `:resource` switch is a fresh root view, not a drill-down:
                // clear the breadcrumb so `esc` doesn't replay command history.
                self.stack.clear();
                self.kind_plural = kind.ar.plural.to_lowercase();
                let title = kind.title();
                self.kind = Some(kind);
                self.labels = None;
                self.fields = None;
                self.scope_label = None;
                self.filter.clear();
                self.reset_sort();
                // A stale selection from the previous kind (e.g. row 5 on
                // pods) would otherwise carry over — reset to the top so the
                // new view always starts with its first row selected.
                self.table_state.select(Some(0));
                self.flash = format!("Viewing {title}");
                self.flash_err = false;
                self.start_watch();
            }
            None => {
                self.flash = format!("No resource matches '{}'", input.trim());
                self.flash_err = true;
            }
        }
    }

    fn push_frame(&mut self) {
        if self.kind.is_none() {
            return;
        }
        self.stack.push(Frame {
            kind: self.kind.clone(),
            kind_plural: self.kind_plural.clone(),
            namespace: self.namespace.clone(),
            labels: self.labels.clone(),
            fields: self.fields.clone(),
            filter: self.filter.clone(),
            scope_label: self.scope_label.clone(),
            selected: self.table_state.selected(),
        });
    }

    fn restore(&mut self, f: Frame) {
        self.kind = f.kind;
        self.kind_plural = f.kind_plural;
        self.namespace = f.namespace;
        self.labels = f.labels;
        self.fields = f.fields;
        self.filter = f.filter;
        self.scope_label = f.scope_label;
        self.reset_sort();
        self.table_state.select(f.selected.or(Some(0)));
    }

    fn pop_frame(&mut self) -> bool {
        if let Some(f) = self.stack.pop() {
            self.restore(f);
            self.start_watch();
            true
        } else {
            false
        }
    }

    /// (Re)start the watch for the current kind/namespace/selectors.
    pub fn start_watch(&mut self) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        self.generation += 1;
        self.gen_flag.store(self.generation, Ordering::SeqCst);
        for t in self.tasks.drain(..) {
            t.abort();
        }
        self.store.clear();
        self.metrics.clear();
        self.marked.clear();
        self.invalidate_rows();
        if self.table_state.selected().is_none() {
            self.table_state.select(Some(0));
        }
        let handle = self.cluster.spawn_watch(
            &kind,
            &self.namespace,
            self.labels.clone(),
            self.fields.clone(),
            self.generation,
            self.tx.clone(),
        );
        self.tasks.push(handle);

        if matches!(self.kind_plural.as_str(), "pods" | "nodes") {
            self.spawn_metrics_poll();
        }

        // Refresh RBAC allow-list when the namespace changes.
        if self.last_rbac_ns.as_deref() != Some(self.namespace.as_str()) {
            self.last_rbac_ns = Some(self.namespace.clone());
            self.refresh_rbac();
        }
    }

    /// Query SelfSubjectRulesReview for the active namespace to learn which
    /// resources the user can list, so the palette can hide the rest.
    fn refresh_rbac(&self) {
        use k8s_openapi::api::authorization::v1::{
            SelfSubjectRulesReview, SelfSubjectRulesReviewSpec,
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        // Namespace this review is computed for (echoed back so a stale result
        // from a previous namespace/context is dropped). SelfSubjectRulesReview
        // needs a concrete namespace, so "" falls back to "default".
        let current_ns = self.namespace.clone();
        let review_ns = if current_ns.is_empty() {
            "default".to_string()
        } else {
            current_ns.clone()
        };
        tokio::spawn(async move {
            let review = SelfSubjectRulesReview {
                spec: SelfSubjectRulesReviewSpec {
                    namespace: Some(review_ns),
                },
                ..Default::default()
            };
            let api: Api<SelfSubjectRulesReview> = Api::all(client);
            let Ok(resp) = api.create(&kube::api::PostParams::default(), &review).await else {
                return; // can't review → leave palette unfiltered
            };
            let Some(status) = resp.status else { return };
            // On clusters that delegate authorization (e.g. GKE → Google IAM),
            // the review comes back `incomplete` and can't enumerate what we can
            // actually access. Filtering on a partial list would wrongly hide
            // everything, so leave the palette unfiltered in that case.
            if status.incomplete {
                return;
            }
            let mut allowed = HashSet::new();
            for rule in status.resource_rules {
                let can_list = rule.verbs.iter().any(|v| v == "list" || v == "*");
                if !can_list {
                    continue;
                }
                for res in rule.resources.unwrap_or_default() {
                    if res == "*" {
                        allowed.insert("*".to_string());
                    } else {
                        // strip subresources like "pods/log"
                        allowed.insert(res.split('/').next().unwrap_or(&res).to_string());
                    }
                }
            }
            // Parsed nothing usable → don't hide the whole palette.
            if allowed.is_empty() {
                return;
            }
            let _ = tx
                .send(Msg::Rbac {
                    generation: genr,
                    ns: current_ns,
                    allowed,
                })
                .await;
        });
    }

    /// Whether a resource plural is visible under the current RBAC allow-list.
    fn rbac_visible(&self, plural: &str) -> bool {
        match &self.rbac_allowed {
            None => true,
            Some(set) => set.contains("*") || set.contains(plural),
        }
    }

    /// Poll the metrics API every few seconds for the current pods/nodes view.
    fn spawn_metrics_poll(&mut self) {
        let base = self.kind_plural.clone();
        let Some(mkind) = self.cluster.resolve(&format!("{base}.metrics.k8s.io")) else {
            return; // metrics-server not installed
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        let ns = self.namespace.clone();
        let ar = mkind.ar.clone();
        let namespaced = mkind.namespaced;
        let is_node = base == "nodes";

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
                    Api::namespaced_with(client.clone(), &ns, &ar)
                } else {
                    Api::all_with(client.clone(), &ar)
                };
                if let Ok(list) = api.list(&ListParams::default()).await {
                    let mut data = HashMap::new();
                    for item in list {
                        let name = item.metadata.name.clone().unwrap_or_default();
                        let key = match &item.metadata.namespace {
                            Some(n) => format!("{n}/{name}"),
                            None => name,
                        };
                        data.insert(key, usage_of(&item, is_node));
                    }
                    if tx
                        .send(Msg::Metrics {
                            generation: genr,
                            data,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    fn bump_generation(&mut self) {
        self.stop_event_stream();
        self.generation += 1;
        self.gen_flag.store(self.generation, Ordering::SeqCst);
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Reset { generation } if generation == self.generation => {
                self.store.clear();
                self.clear_rows_cache();
            }
            Msg::Applied {
                generation,
                key,
                obj,
            } if generation == self.generation => {
                self.store.apply(key.clone(), *obj);
                self.invalidate_row(&key);
            }
            Msg::Deleted { generation, key } if generation == self.generation => {
                self.store.remove(&key);
                self.invalidate_row(&key);
            }
            Msg::Synced { generation } if generation == self.generation => self.store.synced = true,
            Msg::Error { generation, error } if generation == self.generation => {
                self.flash = format!("error: {error}");
                self.flash_err = true;
            }
            Msg::LogLines { generation, lines } if generation == self.log_gen => {
                self.push_log_lines(lines);
            }
            Msg::Metrics { generation, data } if generation == self.generation => {
                let sort_uses_metrics = self
                    .sort_column
                    .and_then(|i| self.display_headers().get(i).copied())
                    .is_some_and(|h| matches!(h, "CPU" | "MEM"));
                self.metrics = data;
                if sort_uses_metrics {
                    self.invalidate_rows();
                }
            }
            Msg::PulseData { generation, data } if generation == self.generation => {
                self.pulse = data;
            }
            Msg::Rbac {
                generation,
                ns,
                allowed,
            } if generation == self.generation && ns == self.namespace => {
                self.rbac_allowed = Some(allowed);
            }
            Msg::XrayData { generation, items } if generation == self.generation => {
                let keep = self.xray_state.selected().unwrap_or(0);
                self.xray_items = items;
                self.xray_state
                    .select(Some(keep.min(self.xray_items.len().saturating_sub(1))));
            }
            Msg::Detail {
                generation,
                title,
                lines,
                warn,
            } if generation == self.generation => {
                self.detail = Scrollable {
                    title,
                    lines: lines.into(),
                    scroll: 0,
                };
                self.mode = Mode::Detail;
                if let Some(w) = warn {
                    self.flash_warn(&w);
                }
            }
            Msg::Events {
                generation,
                title,
                lines,
            } if generation == self.event_gen => {
                self.detail.title = title;
                self.detail.lines = lines.into();
                self.detail.scroll = self
                    .detail
                    .scroll
                    .min(self.detail.lines.len().saturating_sub(1));
            }
            Msg::LogsSaved { generation, result } if generation == self.log_gen => match result {
                Ok(path) => {
                    self.flash = format!("saved logs → {}", path.display());
                    self.flash_err = false;
                }
                Err(e) => self.flash_warn(&format!("save failed: {e}")),
            },
            Msg::ClipboardCopied {
                generation,
                copied,
                success,
                failure,
            } if generation == self.generation => {
                if copied {
                    self.flash = success;
                    self.flash_err = false;
                } else {
                    self.flash_warn(&failure);
                }
            }
            Msg::Namespaces { generation, list } if generation == self.generation => {
                // Keep the picker open and preserve the selection if possible.
                let keep = self.ns_state.selected().unwrap_or(0);
                self.ns_list = list;
                self.ns_state
                    .select(Some(keep.min(self.ns_list.len().saturating_sub(1))));
            }
            Msg::Contexts { generation, list } if generation == self.generation => {
                if list.is_empty() {
                    self.mode = Mode::Table;
                    self.flash_warn("no contexts found in kubeconfig");
                } else {
                    let cur = self.cluster.context.clone();
                    let idx = list.iter().position(|c| *c == cur).unwrap_or(0);
                    self.ctx_list = list;
                    self.ctx_state.select(Some(idx));
                }
            }
            Msg::ContextSwitched {
                generation,
                name,
                result,
            } if generation == self.generation => match result {
                Ok(cluster) => self.apply_context_switch(name, cluster),
                Err(e) => self.flash_warn(&format!("context switch failed: {e}")),
            },
            _ => {} // stale generation, drop
        }
    }

    // ----- selection -----------------------------------------------------

    fn push_log_lines<I>(&mut self, lines: I)
    where
        I: IntoIterator<Item = String>,
    {
        // Strip carriage returns so progress output doesn't overwrite a row,
        // and expand tabs to spaces — many loggers separate timestamp/level/body
        // with tabs, which some terminals render awkwardly in a TUI cell.
        self.logs.view.lines.extend(
            lines
                .into_iter()
                .map(|line| line.replace('\r', "").replace('\t', " ")),
        );

        // While following, keep a tight tail buffer. While paused, avoid
        // trimming so indices don't shift under the frozen view (only a huge
        // backlog hits the larger paused cap).
        let cap = if self.logs.follow {
            MAX_LOG_LINES
        } else {
            MAX_LOG_LINES_PAUSED
        };
        let overflow = self.logs.view.lines.len().saturating_sub(cap);
        if overflow == 0 {
            return;
        }

        // If we trim while paused, shift the anchored scroll by the display
        // rows the dropped lines occupied on screen — filtered lines take none,
        // wrapped lines take several — so the frozen view stays put.
        if !self.logs.follow {
            let filter = self.logs.filter.to_lowercase();
            let rows: usize = self
                .logs
                .view
                .lines
                .iter()
                .take(overflow)
                .filter(|l| filter.is_empty() || l.to_lowercase().contains(&filter))
                .map(|l| match self.logs.last_wrap_width {
                    0 => 1,
                    w => crate::ui::wrapped_height(l, w),
                })
                .sum();
            self.logs.view.scroll = self.logs.view.scroll.saturating_sub(rows);
        }
        self.logs.view.lines.drain(0..overflow);
    }

    /// Mark the cached row order/filter stale. Cheap; safe to over-call.
    fn invalidate_rows(&self) {
        self.rows_cache.borrow_mut().dirty = true;
    }

    fn clear_rows_cache(&self) {
        let mut cache = self.rows_cache.borrow_mut();
        cache.dirty = true;
        cache.keys.clear();
        cache.cells.clear();
    }

    fn invalidate_row(&self, key: &str) {
        let mut cache = self.rows_cache.borrow_mut();
        cache.dirty = true;
        cache.cells.remove(key);
    }

    /// Does this object pass the current fuzzy filter?
    fn matches_filter(&self, o: &DynamicObject) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {}",
            o.metadata.namespace.as_deref().unwrap_or(""),
            o.metadata.name.as_deref().unwrap_or("")
        );
        self.matcher.fuzzy_match(&hay, &self.filter).is_some()
    }

    /// Char indices in `name` that matched the active row filter, for
    /// highlighting them in the table. `None` when there's no active filter
    /// (every visible row already passed [`matches_filter`], so this is
    /// purely a rendering aid, not a second filter decision).
    pub fn filter_match_indices(&self, name: &str) -> Option<Vec<usize>> {
        if self.filter.is_empty() {
            return None;
        }
        self.matcher
            .fuzzy_indices(name, &self.filter)
            .map(|(_, idx)| idx)
    }

    fn ensure_rows_cache(&self) {
        let mut cache = self.rows_cache.borrow_mut();
        if !cache.dirty {
            return;
        }

        let headers = self.display_headers();
        let sort_header = self.sort_column.and_then(|i| headers.get(i).copied());
        // (primary sort key, (ns, name) tiebreak, store key)
        let mut entries: Vec<(SortKey, (String, String), String)> = self
            .store
            .iter()
            .filter(|(_, o)| self.matches_filter(o))
            .map(|(k, o)| {
                let primary = match sort_header {
                    Some(h) => self.column_sort_key(o, h),
                    None => SortKey::Text(String::new()),
                };
                let tie = (
                    o.metadata.namespace.clone().unwrap_or_default(),
                    o.metadata.name.clone().unwrap_or_default(),
                );
                (primary, tie, k.clone())
            })
            .collect();
        let desc = self.sort_desc && sort_header.is_some();
        entries.sort_by(|a, b| {
            let mut ord = a.0.cmp_to(&b.0);
            if desc {
                ord = ord.reverse();
            }
            // Ties always fall back to namespace/name ascending.
            ord.then_with(|| a.1.cmp(&b.1))
        });
        cache.keys = entries.into_iter().map(|(_, _, k)| k).collect();
        cache.dirty = false;
    }

    /// Display-ordered, filtered row count, backed by the same cache as
    /// [`rows`]. Use this when only the count is needed so a frame doesn't
    /// rebuild a temporary `Vec<&DynamicObject>` just to call `len()`.
    pub fn row_count(&self) -> usize {
        self.ensure_rows_cache();
        self.rows_cache.borrow().keys.len()
    }

    /// Display-ordered, filtered rows. Backed by a cache that only recomputes
    /// the sort + fuzzy filter when the store, filter, or sort changes.
    pub fn rows(&self) -> Vec<&DynamicObject> {
        self.ensure_rows_cache();
        self.rows_cache
            .borrow()
            .keys
            .iter()
            .filter_map(|k| self.store.get(k))
            .collect()
    }

    pub(crate) fn ensure_table_cell_cache(&self, rows: &[&DynamicObject]) {
        let mut cache = self.rows_cache.borrow_mut();
        for obj in rows {
            let key = row_key(obj);
            let resource_version = obj.metadata.resource_version.clone();
            let stale = cache.cells.get(&key).is_none_or(|entry| {
                entry.plural != self.kind_plural || entry.resource_version != resource_version
            });
            if stale {
                let (cells, status_idx) = crate::columns::cells(obj, &self.kind_plural);
                cache.cells.insert(
                    key,
                    CellCacheEntry {
                        plural: self.kind_plural.clone(),
                        resource_version,
                        cells,
                        status_idx,
                    },
                );
            }
        }
    }

    pub(crate) fn table_cell_cache(&self) -> TableCellCache<'_> {
        TableCellCache {
            cache: self.rows_cache.borrow(),
        }
    }

    /// The headers as displayed: kind columns, with NAMESPACE prepended when
    /// listing across namespaces and CPU/MEM appended for pods/nodes. Kept in
    /// one place so sorting and rendering agree on the column layout.
    pub fn display_headers(&self) -> Vec<&'static str> {
        let mut h = crate::columns::headers(&self.kind_plural);
        if self.show_namespace_column() {
            h.insert(0, "NAMESPACE");
        }
        if self.metrics_columns() {
            h.push("CPU");
            h.push("MEM");
        }
        h
    }

    pub fn show_namespace_column(&self) -> bool {
        self.kind
            .as_ref()
            .map(|k| k.namespaced && self.all_namespaces())
            .unwrap_or(false)
    }

    pub fn metrics_columns(&self) -> bool {
        matches!(self.kind_plural.as_str(), "pods" | "nodes")
    }

    /// Latest (cpu_millicores, mem_bytes) for an object from the metrics map.
    fn metrics_for(&self, o: &DynamicObject) -> (i64, i64) {
        let name = o.metadata.name.clone().unwrap_or_default();
        let key = if self.kind_plural == "pods" {
            format!("{}/{}", o.metadata.namespace.as_deref().unwrap_or(""), name)
        } else {
            name
        };
        self.metrics.get(&key).copied().unwrap_or((0, 0))
    }

    /// Comparable value of `header`'s cell for object `o`.
    fn column_sort_key(&self, o: &DynamicObject, header: &str) -> SortKey {
        match header {
            "NAMESPACE" => SortKey::Text(
                o.metadata
                    .namespace
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase(),
            ),
            // Unknown timestamps sort last (oldest-unknown) in ascending order.
            "AGE" => SortKey::Num(crate::columns::age_secs(o).unwrap_or(i64::MAX) as f64),
            "CPU" => SortKey::Num(self.metrics_for(o).0 as f64),
            "MEM" => SortKey::Num(self.metrics_for(o).1 as f64),
            _ => {
                let base = crate::columns::headers(&self.kind_plural);
                match base.iter().position(|h| *h == header) {
                    Some(i) => {
                        let (cells, _) = crate::columns::cells(o, &self.kind_plural);
                        let v = cells.get(i).cloned().unwrap_or_default();
                        if is_numeric_header(header) {
                            SortKey::Num(parse_leading_num(&v))
                        } else {
                            SortKey::Text(v.to_lowercase())
                        }
                    }
                    None => SortKey::Text(String::new()),
                }
            }
        }
    }

    fn reset_sort(&mut self) {
        self.sort_column = None;
        self.sort_desc = false;
    }

    /// Cycle the sort column: none → first → … → last → none (k9s `S`).
    fn cycle_sort(&mut self) {
        let n = self.display_headers().len();
        if n == 0 {
            return;
        }
        self.sort_column = match self.sort_column {
            None => Some(0),
            Some(i) if i + 1 < n => Some(i + 1),
            Some(_) => None,
        };
        self.sort_desc = false;
        self.invalidate_rows();
        let label = match self.sort_column {
            Some(i) => self
                .display_headers()
                .get(i)
                .copied()
                .unwrap_or("")
                .to_string(),
            None => "default (ns/name)".to_string(),
        };
        self.flash = format!("sort by {label}");
        self.flash_err = false;
    }

    /// Toggle ascending/descending for the active sort column (k9s `I`).
    fn toggle_sort_dir(&mut self) {
        let Some(i) = self.sort_column else {
            self.flash_warn("press S to pick a sort column first");
            return;
        };
        self.sort_desc = !self.sort_desc;
        self.invalidate_rows();
        let label = self
            .display_headers()
            .get(i)
            .copied()
            .unwrap_or("")
            .to_string();
        self.flash = format!(
            "sort by {label} {}",
            if self.sort_desc {
                "↓ desc"
            } else {
                "↑ asc"
            }
        );
        self.flash_err = false;
    }

    pub fn selected_ref(&self) -> Option<&DynamicObject> {
        let rows = self.rows();
        let idx = self.table_state.selected()?;
        rows.get(idx).copied()
    }

    pub fn selected(&self) -> Option<DynamicObject> {
        self.selected_ref().cloned()
    }

    pub fn confirm_allows_force_toggle(&self) -> bool {
        matches!(
            self.confirm_action,
            Some(ConfirmAction::Delete {
                targets: _,
                force: _
            })
        )
    }

    /// Toggle the mark on the current row (SPACE).
    fn toggle_mark(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let key = row_key(obj);
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
    }

    /// `(name, ns)` for every row a bulk action applies to: the marked set
    /// (resolved against the current rows, so stale/hidden keys are dropped) if
    /// any are marked, otherwise the single current selection.
    fn action_targets(&self) -> Vec<(String, String)> {
        let to_pair = |o: &DynamicObject| {
            (
                o.metadata.name.clone().unwrap_or_default(),
                o.metadata.namespace.clone().unwrap_or_default(),
            )
        };
        if self.marked.is_empty() {
            return self.selected_ref().map(to_pair).into_iter().collect();
        }
        self.rows()
            .iter()
            .filter(|o| self.marked.contains(&row_key(o)))
            .map(|o| to_pair(o))
            .collect()
    }

    fn node_action_targets(&self) -> Vec<String> {
        self.action_targets()
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| !name.is_empty())
            .collect()
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.rows().len() as i32;
        if len == 0 {
            return;
        }
        // No current selection means "before the first row", not "already on
        // it" — otherwise pressing Down from an unselected state lands on row
        // 1, skipping row 0 entirely.
        let cur = self.table_state.selected().map(|c| c as i32).unwrap_or(-1);
        let next = (cur + delta).clamp(0, len - 1);
        self.table_state.select(Some(next as usize));
    }

    // ----- drill-down ----------------------------------------------------

    fn drill(&mut self) {
        let Some(obj) = self.selected() else { return };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();

        match self.kind_plural.as_str() {
            "namespaces" => self.set_namespace_and_return(&name),
            "nodes" => self.drill_to_pods(
                String::new(),
                None,
                Some(format!("spec.nodeName={name}")),
                format!("node/{name}"),
            ),
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
                match label_selector(&obj, "matchLabels") {
                    Some(sel) => self.drill_to_pods(
                        ns,
                        Some(sel),
                        None,
                        format!("{}/{name}", trim_s(&self.kind_plural)),
                    ),
                    None => self.flash_warn("no pod selector on this object"),
                }
            }
            "services" => match label_selector(&obj, "selector") {
                Some(sel) => self.drill_to_pods(ns, Some(sel), None, format!("svc/{name}")),
                None => self.flash_warn("service has no selector"),
            },
            "pods" => self.open_containers(&obj),
            // enter on a CRD lists its custom resources, not its YAML.
            "customresourcedefinitions" => self.drill_into_crd(&obj),
            _ => self.open_detail(),
        }
    }

    /// Drill from a CustomResourceDefinition row into a listing of that CRD's
    /// custom resources. Resolves the target kind from discovery (the unambiguous
    /// group-qualified key), falling back to building it straight from the CRD
    /// spec if discovery didn't surface it.
    fn drill_into_crd(&mut self, obj: &DynamicObject) {
        let d = &obj.data;
        let group = d
            .pointer("/spec/group")
            .and_then(Value::as_str)
            .unwrap_or("");
        let plural = d
            .pointer("/spec/names/plural")
            .and_then(Value::as_str)
            .unwrap_or("");
        let ckind = d
            .pointer("/spec/names/kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let scope = d
            .pointer("/spec/scope")
            .and_then(Value::as_str)
            .unwrap_or("Namespaced");
        if plural.is_empty() {
            self.flash_warn("CRD has no plural name");
            return;
        }

        let key = if group.is_empty() {
            plural.to_string()
        } else {
            format!("{plural}.{group}")
        };
        let kind = self.cluster.resolve(&key).or_else(|| {
            let version = crd_served_version(d)?;
            Some(Kind {
                ar: ApiResource {
                    api_version: if group.is_empty() {
                        version.clone()
                    } else {
                        format!("{group}/{version}")
                    },
                    group: group.to_string(),
                    version,
                    kind: ckind.to_string(),
                    plural: plural.to_string(),
                },
                namespaced: scope.eq_ignore_ascii_case("Namespaced"),
            })
        });
        let Some(kind) = kind else {
            self.flash_warn("could not resolve CRD's resource (no served version?)");
            return;
        };

        let crd_name = obj.metadata.name.clone().unwrap_or_default();
        self.push_frame();
        self.kind_plural = kind.ar.plural.to_lowercase();
        self.kind = Some(kind);
        self.namespace = String::new(); // list across all namespaces
        self.labels = None;
        self.fields = None;
        self.scope_label = Some(format!("crd/{crd_name}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ {plural}");
        self.flash_err = false;
        self.start_watch();
    }

    fn drill_to_pods(
        &mut self,
        ns: String,
        labels: Option<String>,
        fields: Option<String>,
        scope: String,
    ) {
        let Some(pods) = self.cluster.resolve("pods") else {
            self.flash_warn("pods kind unavailable");
            return;
        };
        self.push_frame();
        self.kind = Some(pods);
        self.kind_plural = "pods".into();
        self.namespace = ns;
        self.labels = labels;
        self.fields = fields;
        self.scope_label = Some(scope);
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = "↳ drilled into pods".into();
        self.flash_err = false;
        self.start_watch();
    }

    fn set_namespace_and_return(&mut self, name: &str) {
        let ns = if name == "<all>" {
            String::new()
        } else {
            name.to_string()
        };
        // Return to the view we came from if there is one; otherwise (a `:ns`
        // root switch clears the stack) drop into pods scoped to the chosen
        // namespace — namespaces aren't namespaced, so staying on the list would
        // just reload it.
        if let Some(f) = self.stack.pop() {
            self.restore(f);
        } else if let Some(pods) = self.cluster.resolve("pods") {
            self.kind = Some(pods);
            self.kind_plural = "pods".into();
            self.labels = None;
            self.fields = None;
            self.scope_label = None;
            self.filter.clear();
            self.reset_sort();
            self.table_state.select(Some(0));
        }
        self.namespace = ns.clone();
        let label = if ns.is_empty() {
            "all namespaces".to_string()
        } else {
            ns
        };
        self.flash = format!("namespace: {label}");
        self.flash_err = false;
        self.start_watch();
    }

    // ----- detail / describe --------------------------------------------

    /// Remember which view a transient sub-view (logs/detail/diff) was opened
    /// from, so `esc` returns there (e.g. back to the xray tree, not the table).
    fn set_return_mode(&mut self) {
        self.return_mode = if self.mode == Mode::Xray {
            Mode::Xray
        } else {
            Mode::Table
        };
        // Remember the selected row so we can land back on it.
        self.return_selection = self.selected_ref().map(row_key);
    }

    /// Re-select the row remembered by [`set_return_mode`], by identity, so the
    /// cursor returns to the same object even if the list shifted meanwhile.
    fn restore_selection(&mut self) {
        let Some(key) = self.return_selection.take() else {
            return;
        };
        if let Some(i) = self.rows().iter().position(|o| row_key(o) == key) {
            self.table_state.select(Some(i));
        }
    }

    fn open_detail(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let title = obj.metadata.name.clone().unwrap_or_else(|| "object".into());
        self.detail = Scrollable {
            title: format!("{title} — YAML"),
            lines: self.object_yaml(obj).into(),
            scroll: 0,
        };
        self.mode = Mode::Detail;
    }

    /// Describe the selection via `kubectl describe`, off-thread so the UI loop
    /// keeps rendering. Falls back to the object's YAML if kubectl is missing
    /// or fails. The result arrives as `Msg::Detail`.
    fn describe(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let plural = self.kind_plural.clone();
        let ns = obj.metadata.namespace.clone();

        // Compute the YAML fallback up front while we hold the object; the
        // selection may change before the describe completes.
        let yaml = self.object_yaml(obj);
        let yaml_title = format!("{name} — YAML");

        let tx = self.tx.clone();
        let genr = self.generation;
        let mut argv = self.kubectl_base();
        argv.extend(["describe".to_string(), plural, name.clone()]);
        if let Some(ns) = &ns {
            argv.push("-n".into());
            argv.push(ns.clone());
        }
        self.flash = format!("describing {name}…");
        self.flash_err = false;
        tokio::spawn(async move {
            let msg = match tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .await
            {
                Ok(out) if out.status.success() => Msg::Detail {
                    generation: genr,
                    title: format!("{name} — describe"),
                    lines: String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .map(String::from)
                        .collect(),
                    warn: None,
                },
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    Msg::Detail {
                        generation: genr,
                        title: yaml_title,
                        lines: yaml,
                        warn: Some(format!(
                            "kubectl describe failed ({}); showing YAML",
                            err.lines().next().unwrap_or("error")
                        )),
                    }
                }
                Err(_) => Msg::Detail {
                    generation: genr,
                    title: yaml_title,
                    lines: yaml,
                    warn: Some("kubectl not found; showing YAML".into()),
                },
            };
            let _ = tx.send(msg).await;
        });
    }

    /// Render an object as YAML lines, stamping its type if missing.
    fn object_yaml(&self, obj: &DynamicObject) -> Vec<String> {
        let mut obj = obj.clone();
        if let Some(kind) = &self.kind
            && obj.types.is_none()
        {
            obj.types = Some(TypeMeta {
                api_version: kind.ar.api_version.clone(),
                kind: kind.ar.kind.clone(),
            });
        }
        serde_yaml::to_string(&obj)
            .unwrap_or_else(|e| format!("# error: {e}"))
            .lines()
            .map(String::from)
            .collect()
    }

    /// Diff the live object against its `last-applied-configuration` (k9s-style).
    pub fn open_diff(&mut self) {
        use similar::{ChangeTag, TextDiff};
        self.set_return_mode();
        let Some(mut obj) = self.selected() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();

        let last = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
            .cloned();
        let Some(last_json) = last else {
            self.flash_warn("no last-applied-configuration (not applied via kubectl apply)");
            return;
        };
        let last_yaml = serde_json::from_str::<Value>(&last_json)
            .ok()
            .and_then(|v| serde_yaml::to_string(&v).ok())
            .unwrap_or(last_json);

        // Clean the live object for a readable comparison.
        if let Some(ann) = obj.metadata.annotations.as_mut() {
            ann.remove("kubectl.kubernetes.io/last-applied-configuration");
        }
        obj.metadata.managed_fields = None;
        let live_yaml = serde_yaml::to_string(&obj).unwrap_or_default();

        let diff = TextDiff::from_lines(&last_yaml, &live_yaml);
        let mut lines = Vec::new();
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            lines.push(format!("{sign}{}", change.value().trim_end_matches('\n')));
        }
        if lines.iter().all(|l| l.starts_with(' ')) {
            self.flash = "no diff: live matches last-applied".into();
            self.flash_err = false;
            return; // nothing to show — stay on the current view
        }
        self.detail = Scrollable {
            title: format!("{name} — diff (last-applied → live)"),
            lines: lines.into(),
            scroll: 0,
        };
        self.mode = Mode::Diff;
    }

    /// Live Events for the selected object, filtered by object UID when
    /// available. Uses the discovered `events` resource, so core/v1 Events are
    /// preferred but events.k8s.io clusters still work.
    fn open_events(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection for events");
            return;
        };
        let Some(kind) = self.cluster.resolve("events") else {
            self.flash_warn("events kind unavailable");
            return;
        };

        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let title = format!("{name} — events");
        let field = if kind.ar.group == "events.k8s.io" {
            "regarding"
        } else {
            "involvedObject"
        };
        let selector = obj
            .metadata
            .uid
            .as_ref()
            .filter(|uid| !uid.is_empty())
            .map(|uid| format!("{field}.uid={uid}"))
            .unwrap_or_else(|| {
                let mut parts = vec![format!("{field}.name={name}")];
                if !ns.is_empty() {
                    parts.push(format!("{field}.namespace={ns}"));
                }
                parts.join(",")
            });

        self.stop_event_stream();
        let genr = self.event_gen;
        self.detail = Scrollable {
            title: title.clone(),
            lines: vec!["loading events…".into()].into(),
            scroll: 0,
        };
        self.flash = format!("events: {name}");
        self.flash_err = false;
        self.mode = Mode::Events;

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let ar = kind.ar.clone();
        let namespaced = kind.namespaced;
        let watch_ns = ns;
        let is_events_v1 = ar.group == "events.k8s.io";
        let handle = tokio::spawn(async move {
            let api: Api<DynamicObject> = if namespaced && !watch_ns.is_empty() {
                Api::namespaced_with(client, &watch_ns, &ar)
            } else {
                Api::all_with(client, &ar)
            };
            let cfg = watcher::Config::default().any_semantic().fields(&selector);
            let mut stream = watcher(api, cfg).boxed();
            let mut items: HashMap<String, DynamicObject> = HashMap::new();

            while let Some(event) = stream.next().await {
                match event {
                    Ok(watcher::Event::Init) => items.clear(),
                    Ok(watcher::Event::Apply(obj)) | Ok(watcher::Event::InitApply(obj)) => {
                        items.insert(row_key(&obj), obj);
                    }
                    Ok(watcher::Event::Delete(obj)) => {
                        items.remove(&row_key(&obj));
                    }
                    Ok(watcher::Event::InitDone) => {}
                    Err(e) => {
                        let _ = tx
                            .send(Msg::Events {
                                generation: genr,
                                title: title.clone(),
                                lines: vec![format!("error: {e}")],
                            })
                            .await;
                        continue;
                    }
                }

                if !send_event_snapshot(&tx, genr, &title, &items, is_events_v1).await {
                    break;
                }
            }
        });
        self.event_task = Some(handle);
    }

    fn stop_event_stream(&mut self) {
        self.event_gen += 1;
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
    }

    // ----- containers / logs --------------------------------------------

    fn open_containers(&mut self, obj: &DynamicObject) {
        let mut names = container_names(obj);
        if names.is_empty() {
            self.flash_warn("no containers found");
            return;
        }
        names.sort();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.container_pod = Some((ns, name));
        self.container_list = names;
        self.container_state.select(Some(0));
        self.mode = Mode::Containers;
    }

    /// Logs for the current selection. For pods: stream every container. For
    /// workloads/services: list matching pods and aggregate all their logs.
    fn open_logs(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();

        match self.kind_plural.as_str() {
            "pods" => {
                let containers = container_names(obj);
                self.launch_logs(
                    LogSource::Pod {
                        ns,
                        name: name.clone(),
                        containers,
                    },
                    format!("{name} — logs"),
                );
            }
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
                match label_selector(obj, "matchLabels") {
                    Some(labels) => self.launch_logs(
                        LogSource::Selector { ns, labels },
                        format!("{}/{name} — logs (all pods)", trim_s(&self.kind_plural)),
                    ),
                    None => self.flash_warn("no pod selector for logs"),
                }
            }
            "services" => match label_selector(obj, "selector") {
                Some(labels) => self.launch_logs(
                    LogSource::Selector { ns, labels },
                    format!("svc/{name} — logs (all pods)"),
                ),
                None => self.flash_warn("service has no selector"),
            },
            _ => self.flash_warn("logs available for pods and workloads"),
        }
    }

    /// Begin a fresh logs view from a source (resets filter/follow).
    fn launch_logs(&mut self, source: LogSource, title: String) {
        self.set_return_mode();
        self.logs.source = Some(source);
        // Note: we deliberately do NOT touch the view generation here — the
        // underlying table/xray watch keeps running so returning is instant and
        // the selection is preserved. Log streams have their own lifecycle.
        self.logs.view = Scrollable {
            title,
            lines: VecDeque::new(),
            scroll: 0,
        };
        self.logs.follow = true;
        self.logs.filter.clear();
        self.logs.stopped = false;
        self.mode = Mode::Logs;
        self.restart_log_stream();
    }

    /// Re-stream the current source (e.g. after toggling timestamps), keeping
    /// the title, filter, and follow state.
    fn retail_logs(&mut self) {
        if self.logs.source.is_none() {
            return;
        }
        self.logs.view.lines.clear();
        self.logs.view.scroll = 0;
        self.restart_log_stream();
    }

    /// Bump the log generation, abort old log tasks, and spawn fresh ones for
    /// the current source. Independent of the view watch.
    fn restart_log_stream(&mut self) {
        self.stop_log_stream();
        self.start_logs();
    }

    /// Invalidate and abort the current log streams (the view watch is left
    /// running).
    fn stop_log_stream(&mut self) {
        self.log_gen += 1;
        self.log_flag.store(self.log_gen, Ordering::SeqCst);
        for t in self.log_tasks.drain(..) {
            t.abort();
        }
    }

    /// Spawn the streaming task(s) for the current `log_source`.
    fn start_logs(&mut self) {
        let ts = self.logs.timestamps;
        match self.logs.source.clone() {
            Some(LogSource::Pod {
                ns,
                name,
                containers,
            }) => {
                if containers.is_empty() {
                    // Unknown container set (e.g. from xray) — stream the default.
                    self.spawn_one_log(ns, name, None, String::new(), false, ts);
                } else {
                    let multi = containers.len() > 1;
                    for c in containers {
                        let prefix = if multi {
                            format!("[{c}] ")
                        } else {
                            String::new()
                        };
                        self.spawn_one_log(ns.clone(), name.clone(), Some(c), prefix, false, ts);
                    }
                }
            }
            Some(LogSource::Selector { ns, labels }) => self.spawn_selector_logs(ns, labels, ts),
            Some(LogSource::Single {
                ns,
                pod,
                container,
                previous,
            }) => self.spawn_one_log(ns, pod, container, String::new(), previous, ts),
            None => {}
        }
    }

    fn spawn_one_log(
        &mut self,
        ns: String,
        pod: String,
        container: Option<String>,
        prefix: String,
        previous: bool,
        timestamps: bool,
    ) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let handle = tokio::spawn(async move {
            let api: Api<Pod> = Api::namespaced(client, &ns);
            let lp = LogParams {
                follow: !previous,
                previous,
                container,
                timestamps,
                tail_lines: if previous { None } else { Some(300) },
                ..Default::default()
            };
            forward_log_stream(api, pod, lp, prefix, tx, genr, flag).await;
        });
        self.log_tasks.push(handle);
    }

    fn spawn_selector_logs(&mut self, ns: String, labels: String, timestamps: bool) {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.log_gen;
        let flag = self.log_flag.clone();
        let handle = tokio::spawn(async move {
            let list_api: Api<Pod> = if ns.is_empty() {
                Api::all(client.clone())
            } else {
                Api::namespaced(client.clone(), &ns)
            };
            let pods = match list_api.list(&ListParams::default().labels(&labels)).await {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx
                        .send(Msg::LogLines {
                            generation: genr,
                            lines: vec![format!("[error] {e}")],
                        })
                        .await;
                    return;
                }
            };
            if pods.items.is_empty() {
                let _ = tx
                    .send(Msg::LogLines {
                        generation: genr,
                        lines: vec!["(no matching pods)".into()],
                    })
                    .await;
            }
            let mut streams = tokio::task::JoinSet::new();
            for p in pods {
                let pod_ns = p.metadata.namespace.clone().unwrap_or_default();
                let pod_name = p.metadata.name.clone().unwrap_or_default();
                let containers: Vec<String> = p
                    .spec
                    .as_ref()
                    .map(|s| s.containers.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default();
                let multi = containers.len() > 1;
                for c in containers {
                    let prefix = if multi {
                        format!("[{pod_name}:{c}] ")
                    } else {
                        format!("[{pod_name}] ")
                    };
                    let (client, tx, flag) = (client.clone(), tx.clone(), flag.clone());
                    let (pn, pns) = (pod_name.clone(), pod_ns.clone());
                    streams.spawn(async move {
                        let api: Api<Pod> = Api::namespaced(client, &pns);
                        let lp = LogParams {
                            follow: true,
                            container: Some(c),
                            timestamps,
                            tail_lines: Some(100),
                            ..Default::default()
                        };
                        forward_log_stream(api, pn, lp, prefix, tx, genr, flag).await;
                    });
                }
            }
            while streams.join_next().await.is_some() {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
            }
        });
        self.log_tasks.push(handle);
    }

    // ----- actions -------------------------------------------------------

    fn request_delete(&mut self, force: bool) {
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        self.confirm_label = delete_confirm_label(&self.kind_plural, &targets, force);
        self.confirm_action = Some(ConfirmAction::Delete { targets, force });
        self.mode = Mode::Confirm;
    }

    fn spawn_patch_action<F>(
        &self,
        kind: Kind,
        targets: Vec<(String, String)>,
        patch: Patch<Value>,
        error_message: F,
    ) where
        F: Fn(&str, kube::Error) -> String + Send + 'static,
    {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            for (name, ns) in targets {
                let api: Api<DynamicObject> = if kind.namespaced && !ns.is_empty() {
                    Api::namespaced_with(client.clone(), &ns, &kind.ar)
                } else {
                    Api::all_with(client.clone(), &kind.ar)
                };
                if let Err(e) = api.patch(&name, &PatchParams::default(), &patch).await {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: error_message(&name, e),
                        })
                        .await;
                }
            }
        });
    }

    fn do_delete(&mut self, targets: Vec<(String, String)>, force: bool) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = if targets.len() == 1 {
            format!("deleting {}…", targets[0].0)
        } else {
            format!("deleting {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        tokio::spawn(async move {
            let mut dp = DeleteParams::default();
            if force {
                dp = dp.grace_period(0);
            }
            for (name, ns) in targets {
                let api: Api<DynamicObject> = if kind.namespaced && !ns.is_empty() {
                    Api::namespaced_with(client.clone(), &ns, &kind.ar)
                } else {
                    Api::all_with(client.clone(), &kind.ar)
                };
                if let Err(e) = api.delete(&name, &dp).await {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: format!("delete {name} failed: {e}"),
                        })
                        .await;
                }
            }
        });
    }

    fn request_cordon(&mut self, unschedulable: bool) {
        if self.kind_plural != "nodes" {
            self.flash_warn("cordon/uncordon applies to nodes");
            return;
        }
        let targets = self.node_action_targets();
        if targets.is_empty() {
            return;
        }
        self.do_cordon_nodes(targets, unschedulable);
    }

    fn do_cordon_nodes(&mut self, targets: Vec<String>, unschedulable: bool) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let verb = if unschedulable {
            "cordoning"
        } else {
            "uncordoning"
        };
        self.flash = if targets.len() == 1 {
            format!("{verb} {}…", targets[0])
        } else {
            format!("{verb} {} nodes…", targets.len())
        };
        self.flash_err = false;
        let targets = targets
            .into_iter()
            .map(|name| (name, String::new()))
            .collect();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(node_unschedulable_patch(unschedulable)),
            move |name, e| format!("{verb} {name} failed: {e}"),
        );
    }

    fn request_drain(&mut self) {
        if self.kind_plural != "nodes" {
            self.flash_warn("drain applies to nodes");
            return;
        }
        let targets = self.node_action_targets();
        if targets.is_empty() {
            return;
        }
        self.confirm_label = if targets.len() == 1 {
            format!("Drain node {}? Cordon and evict eligible pods.", targets[0])
        } else {
            format!(
                "Drain {} nodes? Cordon and evict eligible pods.",
                targets.len()
            )
        };
        self.confirm_action = Some(ConfirmAction::Drain { targets });
        self.mode = Mode::Confirm;
    }

    fn do_drain_nodes(&mut self, targets: Vec<String>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = if targets.len() == 1 {
            format!("draining {}…", targets[0])
        } else {
            format!("draining {} nodes…", targets.len())
        };
        self.flash_err = false;
        tokio::spawn(async move {
            let nodes: Api<DynamicObject> = Api::all_with(client.clone(), &kind.ar);
            let node_patch = Patch::Merge(node_unschedulable_patch(true));
            let pods: Api<Pod> = Api::all(client.clone());
            for node in targets {
                if let Err(e) = nodes
                    .patch(&node, &PatchParams::default(), &node_patch)
                    .await
                {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: format!("drain {node}: cordon failed: {e}"),
                        })
                        .await;
                    continue;
                }

                let listed = pods
                    .list(&ListParams::default().fields(&format!("spec.nodeName={node}")))
                    .await;
                let pod_list = match listed {
                    Ok(list) => list,
                    Err(e) => {
                        let _ = tx
                            .send(Msg::Error {
                                generation: genr,
                                error: format!("drain {node}: list pods failed: {e}"),
                            })
                            .await;
                        continue;
                    }
                };

                for pod in pod_list.items.iter().filter(|pod| drainable_pod(pod)) {
                    let Some(name) = pod.metadata.name.as_deref() else {
                        continue;
                    };
                    let ns = pod.metadata.namespace.as_deref().unwrap_or("default");
                    let pod_api: Api<Pod> = Api::namespaced(client.clone(), ns);
                    let evict = EvictParams {
                        delete_options: Some(DeleteParams::default()),
                        ..Default::default()
                    };
                    match pod_api.evict(name, &evict).await {
                        Ok(_) => {}
                        Err(e) if eviction_unsupported(&e) => {
                            if let Err(delete_err) =
                                pod_api.delete(name, &DeleteParams::default()).await
                            {
                                let _ = tx
                                    .send(Msg::Error {
                                        generation: genr,
                                        error: format!(
                                            "drain {node}: delete {ns}/{name} failed after eviction fallback: {delete_err}"
                                        ),
                                    })
                                    .await;
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Msg::Error {
                                    generation: genr,
                                    error: format!("drain {node}: evict {ns}/{name} failed: {e}"),
                                })
                                .await;
                        }
                    }
                }
            }
        });
    }

    fn request_attach(&mut self) {
        if self.kind_plural != "pods" {
            self.flash_warn("attach is only available for pods");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let mut argv = self.kubectl_base();
        argv.extend(["attach".into(), "-it".into(), "-n".into(), ns, name]);
        self.pending = Some(Suspend::Shell(argv));
    }

    /// Navigate to the node hosting the selected pod (k9s `o`).
    fn show_node(&mut self) {
        if self.kind_plural != "pods" {
            self.flash_warn("'o' shows the node for a pod");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let Some(node) = obj.data.pointer("/spec/nodeName").and_then(Value::as_str) else {
            self.flash_warn("pod has no node assigned");
            return;
        };
        let node = node.to_string();
        let pod_name = obj.metadata.name.clone().unwrap_or_default();
        let Some(nodes) = self.cluster.resolve("nodes") else {
            self.flash_warn("nodes kind unavailable");
            return;
        };
        self.push_frame();
        self.kind = Some(nodes);
        self.kind_plural = "nodes".into();
        self.namespace = String::new();
        self.labels = None;
        self.fields = Some(format!("metadata.name={node}"));
        self.scope_label = Some(format!("host of {pod_name}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.start_watch();
    }

    /// Jump to the selected object's controller/owner (k9s Shift-J).
    fn jump_owner(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let owners = obj
            .metadata
            .owner_references
            .as_ref()
            .filter(|o| !o.is_empty());
        let Some(owner) = owners.and_then(|o| o.first()) else {
            self.flash_warn("no owner reference");
            return;
        };
        let Some(kind) = self.cluster.resolve(&owner.kind.to_lowercase()) else {
            self.flash_warn(&format!("owner kind {} unresolved", owner.kind));
            return;
        };
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let owner_name = owner.name.clone();
        let child_name = obj.metadata.name.clone().unwrap_or_default();
        self.push_frame();
        self.kind_plural = kind.ar.plural.to_lowercase();
        self.kind = Some(kind);
        self.namespace = ns;
        self.labels = None;
        self.fields = Some(format!("metadata.name={owner_name}"));
        self.scope_label = Some(format!("owner of {child_name}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.start_watch();
    }

    /// Copy the (filtered) log buffer to the clipboard (k9s `c` in logs).
    fn copy_logs(&mut self) {
        let text = self.filtered_log_text();
        if text.is_empty() {
            self.flash_warn("no log lines to copy");
            return;
        }
        let n = text.lines().count();
        self.copy_to_clipboard_async(
            text,
            format!("copied {n} log lines"),
            "no clipboard target found (pbcopy/xclip/wl-copy/OSC 52)",
        );
    }

    /// Save the filtered log buffer to a temp file (k9s Ctrl-S).
    fn save_logs(&mut self) {
        let text = self.filtered_log_text();
        if text.is_empty() {
            self.flash_warn("no log lines to save");
            return;
        }
        let genr = self.log_gen;
        let tx = self.tx.clone();
        let ts = k8s_openapi::jiff::Timestamp::now().as_second();
        let safe: String = self
            .logs
            .view
            .title
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let path = std::env::temp_dir().join(format!("sofka-{safe}-{ts}.log"));
        tokio::spawn(async move {
            let result = tokio::fs::write(&path, text)
                .await
                .map(|_| path)
                .map_err(|e| e.to_string());
            let _ = tx
                .send(Msg::LogsSaved {
                    generation: genr,
                    result,
                })
                .await;
        });
    }

    fn filtered_log_text(&self) -> String {
        let f = self.logs.filter.to_lowercase();
        self.logs
            .view
            .lines
            .iter()
            .filter(|l| f.is_empty() || l.to_lowercase().contains(&f))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Copy the selected resource's name to the system clipboard (k9s `c`).
    fn copy_name(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.copy_to_clipboard_async(
            name.clone(),
            format!("copied: {name}"),
            "no clipboard target found (pbcopy/xclip/wl-copy/OSC 52)",
        );
    }

    fn copy_to_clipboard_async(&self, text: String, success: String, failure: &str) {
        let tx = self.tx.clone();
        let genr = self.generation;
        let failure = failure.to_string();
        tokio::spawn(async move {
            let copied = tokio::task::spawn_blocking(move || copy_to_clipboard(&text))
                .await
                .unwrap_or(false);
            let _ = tx
                .send(Msg::ClipboardCopied {
                    generation: genr,
                    copied,
                    success,
                    failure,
                })
                .await;
        });
    }

    /// Previous-container logs for the selected pod (k9s `p` on a pod row).
    fn open_previous_logs(&mut self) {
        if self.kind_plural != "pods" {
            self.flash_warn("previous logs are for pods (use the container picker elsewhere)");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let containers = container_names(obj);
        let container = containers.into_iter().next();
        self.launch_logs(
            LogSource::Single {
                ns,
                pod: name.clone(),
                container,
                previous: true,
            },
            format!("{name} — previous logs"),
        );
    }

    /// Rollout-restart a workload by stamping the template annotation (k9s `r`).
    fn request_restart(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let now = k8s_openapi::jiff::Timestamp::now().to_string();
        self.flash = format!("restarting {name}…");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Strategic(restart_patch(&now)),
            |_, e| format!("restart failed: {e}"),
        );
    }

    /// Open the Set-Image picker for the selected workload/pod (k9s `i`).
    fn request_set_image(&mut self) {
        let is_pod = self.kind_plural == "pods";
        let workload = matches!(
            self.kind_plural.as_str(),
            "deployments"
                | "statefulsets"
                | "daemonsets"
                | "replicasets"
                | "replicationcontrollers"
        );
        if !is_pod && !workload {
            self.flash_warn("set image applies to pods and workload controllers");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let ptr = if is_pod {
            "/spec/containers"
        } else {
            "/spec/template/spec/containers"
        };
        let Some(cs) = obj.data.pointer(ptr).and_then(Value::as_array) else {
            self.flash_warn("no containers found");
            return;
        };
        let mut names = Vec::new();
        let mut images = Vec::new();
        for c in cs {
            names.push(
                c.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            );
            images.push(
                c.get("image")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
        if names.is_empty() {
            self.flash_warn("no containers found");
            return;
        }
        let target = (
            obj.metadata.namespace.clone().unwrap_or_default(),
            obj.metadata.name.clone().unwrap_or_default(),
            self.kind_plural.clone(),
        );
        self.container_list = names;
        self.image_values = images;
        self.image_target = Some(target);
        self.container_state.select(Some(0));
        self.mode = Mode::SetImage;
    }

    fn key_set_image(&mut self, key: KeyEvent) {
        let len = self.container_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.container_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.container_state, len, false),
            KeyCode::Enter => {
                if let Some(i) = self.container_state.selected()
                    && let Some(container) = self.container_list.get(i).cloned()
                    && let Some((ns, name, plural)) = self.image_target.clone()
                {
                    self.prompt_label = format!("New image for {container}:");
                    self.prompt_input = self.image_values.get(i).cloned().unwrap_or_default();
                    self.prompt_kind = Some(PromptKind::SetImage {
                        ns,
                        name,
                        plural,
                        container,
                    });
                    self.mode = Mode::Prompt;
                }
            }
            _ => {}
        }
    }

    fn do_set_image(
        &mut self,
        ns: String,
        name: String,
        plural: String,
        container: String,
        image: String,
    ) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        self.flash = format!("setting image: {container} → {image}");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Strategic(set_image_patch(&plural, &container, &image)),
            |_, e| format!("set image failed: {e}"),
        );
    }

    fn request_edit(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let mut argv = self.kubectl_base();
        argv.extend(["edit".into(), self.kind_plural.clone(), name]);
        if let Some(ns) = &obj.metadata.namespace {
            argv.push("-n".into());
            argv.push(ns.clone());
        }
        self.pending = Some(Suspend::Shell(argv));
    }

    fn request_exec(&mut self) {
        if self.kind_plural != "pods" {
            self.flash_warn("shell is only available for pods");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let mut argv = self.kubectl_base();
        argv.extend([
            "exec".into(),
            "-it".into(),
            "-n".into(),
            ns,
            name,
            "--".into(),
            "sh".into(),
            "-c".into(),
            "command -v bash >/dev/null 2>&1 && exec bash || exec sh".into(),
        ]);
        self.pending = Some(Suspend::Shell(argv));
    }

    fn request_scale(&mut self) {
        if !matches!(
            self.kind_plural.as_str(),
            "deployments" | "statefulsets" | "replicasets"
        ) {
            self.flash_warn("scale applies to deployments/statefulsets/replicasets");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let cur = obj
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.prompt_label = format!("Scale {name} to replicas (current {cur}):");
        self.prompt_input.clear();
        self.prompt_kind = Some(PromptKind::Scale { ns, name });
        self.mode = Mode::Prompt;
    }

    fn request_port_forward(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        if !matches!(self.kind_plural.as_str(), "pods" | "services") {
            self.flash_warn("port-forward applies to pods/services");
            return;
        }
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.prompt_label = format!("Port-forward {name} (LOCAL:REMOTE, e.g. 8080:80):");
        self.prompt_input.clear();
        self.prompt_kind = Some(PromptKind::PortForward { ns, name });
        self.mode = Mode::Prompt;
    }

    /// Start `kubectl port-forward` in the background (not a foreground
    /// `Suspend::Shell` — a forward should keep running while you keep
    /// browsing). stdio is nulled since the TUI still owns the terminal.
    fn start_port_forward(&mut self, ns: String, target: String, ports: String) {
        let mut argv = self.kubectl_base();
        argv.extend([
            "port-forward".into(),
            "-n".into(),
            ns.clone(),
            target.clone(),
            ports.clone(),
        ]);
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let pf = PortForward {
                    ns,
                    target,
                    ports,
                    child,
                };
                self.flash = format!("port-forwarding {} (:pf to view/stop)", pf.label());
                self.flash_err = false;
                self.port_forwards.push(pf);
            }
            Err(e) => self.flash_warn(&format!("port-forward failed to start: {e}")),
        }
    }

    /// Drop any forward whose `kubectl` process has already exited (pod
    /// restarted, connection dropped, port in use, …), flashing a heads-up.
    /// Called on every tick, so a dead forward doesn't linger in the list.
    pub fn reap_port_forwards(&mut self) {
        let mut i = 0;
        while i < self.port_forwards.len() {
            match self.port_forwards[i].child.try_wait() {
                Ok(Some(_)) => {
                    let pf = self.port_forwards.remove(i);
                    self.flash_warn(&format!("port-forward {} exited", pf.label()));
                }
                _ => i += 1,
            }
        }
    }

    fn open_port_forwards(&mut self) {
        self.pf_state.select(if self.port_forwards.is_empty() {
            None
        } else {
            Some(0)
        });
        self.mode = Mode::PortForwards;
    }

    fn key_port_forwards(&mut self, key: KeyEvent) {
        let len = self.port_forwards.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.pf_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.pf_state, len, false),
            KeyCode::Char('x') | KeyCode::Char('s') => self.stop_selected_port_forward(),
            _ => {}
        }
    }

    fn open_skins(&mut self) {
        self.skin_state.select(if self.skin_list.is_empty() {
            None
        } else {
            Some(0)
        });
        self.mode = Mode::Skins;
    }

    fn key_skins(&mut self, key: KeyEvent) {
        let len = self.skin_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.skin_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.skin_state, len, false),
            KeyCode::Enter => {
                if let Some(name) = self
                    .skin_state
                    .selected()
                    .and_then(|i| self.skin_list.get(i).cloned())
                {
                    self.apply_skin(&name);
                }
                self.mode = Mode::Table;
            }
            _ => {}
        }
    }

    fn apply_skin(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.open_skins();
            return;
        }
        if crate::theme::builtin(&name.to_ascii_lowercase()).is_none() {
            self.flash_warn(&format!("unknown skin: {name}"));
            return;
        }
        let palette = crate::theme::resolve_skin(Some(name), &self.skin_colors);
        crate::theme::set(palette);
        self.flash = format!("skin: {name}");
        self.flash_err = false;
    }

    /// Stop (kill) the selected forward. Others keep running.
    fn stop_selected_port_forward(&mut self) {
        let Some(i) = self.pf_state.selected() else {
            return;
        };
        if i >= self.port_forwards.len() {
            return;
        }
        let pf = self.port_forwards.remove(i); // dropped -> Drop kills the child
        self.flash = format!("stopped port-forward {}", pf.label());
        self.flash_err = false;
        self.pf_state.select(if self.port_forwards.is_empty() {
            None
        } else {
            Some(i.min(self.port_forwards.len() - 1))
        });
    }

    fn do_scale(&mut self, ns: String, name: String, replicas: i32) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        self.flash = format!("scaling {name} → {replicas}");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Merge(scale_patch(replicas)),
            |_, e| format!("scale failed: {e}"),
        );
    }

    /// Open the Flux suspend/resume menu (`t`) for the marked rows, or the
    /// current selection if none are marked. A menu, not a single-key
    /// toggle — suspending something always takes an explicit, visible
    /// choice (`j`/`k` + Enter) rather than one accidental keystroke.
    fn request_flux_menu(&mut self) {
        if !FLUX_SUSPENDABLE_KINDS.contains(&self.kind_plural.as_str()) {
            self.flash_warn("suspend/resume only applies to Flux resources (ks/hr/git-, helm-, oci-repos, buckets, image automation, alerts, receivers)");
            return;
        }
        if self.action_targets().is_empty() {
            return;
        }
        self.flux_menu_state.select(Some(0));
        self.mode = Mode::FluxMenu;
    }

    fn key_flux_menu(&mut self, key: KeyEvent) {
        let len = FLUX_MENU_ITEMS.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.flux_menu_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.flux_menu_state, len, false),
            KeyCode::Enter => {
                let choice = self
                    .flux_menu_state
                    .selected()
                    .and_then(|i| FLUX_MENU_ITEMS.get(i))
                    .copied();
                self.mode = Mode::Table;
                match choice {
                    Some("Suspend") => {
                        let targets = self.action_targets();
                        self.do_set_suspend(targets, true);
                    }
                    Some("Resume") => {
                        let targets = self.action_targets();
                        self.do_set_suspend(targets, false);
                    }
                    Some("Reconcile now") => {
                        let targets = self.action_targets();
                        self.do_reconcile(targets);
                    }
                    _ => {} // "Cancel" or nothing selected — do nothing.
                }
            }
            _ => {}
        }
    }

    fn do_set_suspend(&mut self, targets: Vec<(String, String)>, suspend: bool) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let verb = if suspend { "suspending" } else { "resuming" };
        self.flash = if targets.len() == 1 {
            format!("{verb} {}…", targets[0].0)
        } else {
            format!("{verb} {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(suspend_patch(suspend)),
            move |name, e| format!("{verb} {name} failed: {e}"),
        );
    }

    /// Force an immediate Flux reconciliation, bypassing the normal interval —
    /// patches `reconcile.fluxcd.io/requestedAt`, the same annotation `flux
    /// reconcile` sets, watched by every toolkit controller.
    fn do_reconcile(&mut self, targets: Vec<(String, String)>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let now = k8s_openapi::jiff::Timestamp::now().to_string();
        self.flash = if targets.len() == 1 {
            format!("reconciling {}…", targets[0].0)
        } else {
            format!("reconciling {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(reconcile_patch(&now)),
            |name, e| format!("reconcile {name} failed: {e}"),
        );
    }

    /// Force an immediate External Secrets Operator refresh on the marked rows
    /// (or the current selection), matching the k9s external-secrets plugin.
    fn request_refresh_es(&mut self) {
        if !EXTERNAL_SECRET_KINDS.contains(&self.kind_plural.as_str()) {
            self.flash_warn(
                "refresh only applies to external secrets (externalsecrets, pushsecrets)",
            );
            return;
        }
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        self.do_refresh_es(targets);
    }

    /// Stamp the `force-sync` annotation ESO watches to reconcile a secret out
    /// of band — the same annotation the k9s plugin overwrites. The value only
    /// has to change to trigger a sync; a unix timestamp mirrors k9s' `date +%s`.
    fn do_refresh_es(&mut self, targets: Vec<(String, String)>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let now = k8s_openapi::jiff::Timestamp::now().as_second().to_string();
        self.flash = if targets.len() == 1 {
            format!("refreshing {}…", targets[0].0)
        } else {
            format!("refreshing {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(external_secret_refresh_patch(&now)),
            |name, e| format!("refresh {name} failed: {e}"),
        );
    }

    fn flash_warn(&mut self, msg: &str) {
        self.flash = msg.to_string();
        self.flash_err = true;
    }

    /// Base argv for a `kubectl` shell-out, pinned to the active context so it
    /// can't target a different cluster than the one we're viewing.
    fn kubectl_base(&self) -> Vec<String> {
        let mut argv = vec!["kubectl".to_string()];
        if let Some(ctx) = self.cluster.kubectl_context() {
            argv.push("--context".to_string());
            argv.push(ctx.to_string());
        }
        argv
    }

    // ----- key handling --------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return Ok(());
                }
                KeyCode::Char('d') if self.mode == Mode::Table => {
                    self.request_delete(false);
                    return Ok(());
                }
                KeyCode::Char('k') if self.mode == Mode::Table => {
                    self.request_delete(true); // kill = force delete
                    return Ok(());
                }
                KeyCode::Char('r') if self.mode == Mode::Table => {
                    self.start_watch();
                    return Ok(());
                }
                _ => {}
            }
        }

        match self.mode {
            Mode::Table => self.key_table(key),
            Mode::Command => self.key_command(key),
            Mode::Filter => self.key_filter(key),
            Mode::Detail | Mode::Diff | Mode::Events => self.key_scroll(key, true),
            Mode::Logs => self.key_logs(key),
            Mode::LogFilter => self.key_log_filter(key),
            Mode::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.mode = Mode::Table;
                }
            }
            Mode::Namespaces => self.key_namespaces(key),
            Mode::Contexts => self.key_contexts(key),
            Mode::Containers => self.key_containers(key),
            Mode::SetImage => self.key_set_image(key),
            Mode::Confirm => self.key_confirm(key),
            Mode::Prompt => self.key_prompt(key),
            Mode::Pulse => self.key_pulse(key),
            Mode::Xray => self.key_xray(key),
            Mode::FluxMenu => self.key_flux_menu(key),
            Mode::PortForwards => self.key_port_forwards(key),
            Mode::Skins => self.key_skins(key),
        }
        Ok(())
    }

    fn key_table(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
                self.update_suggestions();
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if !self.marked.is_empty() {
                    self.marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.invalidate_rows();
                } else if !self.pop_frame() {
                    // at root, nothing to pop
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.table_state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.rows().len();
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
            }
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            // k9s: SPACE marks/unmarks the current row for bulk actions, then
            // advances so a range can be marked with repeated taps.
            KeyCode::Char(' ') => {
                self.toggle_mark();
                self.move_selection(1);
            }
            KeyCode::Enter => self.drill(),
            KeyCode::Char('y') => self.open_detail(),
            KeyCode::Char('d') => self.describe(),
            KeyCode::Char('E') => self.open_events(),
            KeyCode::Char('l') => self.open_logs(),
            KeyCode::Char('p') => self.open_previous_logs(),
            KeyCode::Char('e') => self.request_edit(),
            // k9s: `s` = shell on pods, scale on scalable workloads.
            KeyCode::Char('s') => {
                if self.kind_plural == "pods" {
                    self.request_exec();
                } else {
                    self.request_scale();
                }
            }
            KeyCode::Char('a') => self.request_attach(),
            KeyCode::Char('i') => self.request_set_image(),
            KeyCode::Char('o') => self.show_node(),
            KeyCode::Char('c') => self.copy_name(),
            KeyCode::Char('J') => self.jump_owner(),
            KeyCode::Char('C') => self.request_cordon(true),
            KeyCode::Char('U') => self.request_cordon(false),
            KeyCode::Char('D') => self.request_drain(),
            // Sorting: S cycles the column, I inverts the direction.
            KeyCode::Char('S') => self.cycle_sort(),
            KeyCode::Char('I') => self.toggle_sort_dir(),
            // `f`/Shift-F = port-forward.
            KeyCode::Char('f') | KeyCode::Char('F') => self.request_port_forward(),
            KeyCode::Char('n') => self.open_namespaces(),
            // k9s: 0 = all namespaces.
            KeyCode::Char('0') => {
                self.namespace.clear();
                self.flash = "namespace: all namespaces".into();
                self.flash_err = false;
                self.table_state.select(Some(0));
                self.start_watch();
            }
            // k9s: `r` = rollout restart on workloads, force-sync on external
            // secrets, else refresh the watch.
            KeyCode::Char('r') => {
                if matches!(
                    self.kind_plural.as_str(),
                    "deployments" | "statefulsets" | "daemonsets"
                ) {
                    self.request_restart();
                } else if EXTERNAL_SECRET_KINDS.contains(&self.kind_plural.as_str()) {
                    self.request_refresh_es();
                } else {
                    self.start_watch();
                }
            }
            // Flux CD: toggle suspend/resume on the marked rows, or current.
            KeyCode::Char('t') => self.request_flux_menu(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            // User-defined plugins fall through here (built-ins take priority).
            KeyCode::Char(c) => self.try_plugin(c),
            _ => {}
        }
    }

    /// Run a config-defined plugin bound to `c` if it applies to the current kind.
    fn try_plugin(&mut self, c: char) {
        let Some(plugin) = self
            .plugins
            .iter()
            .find(|p| {
                p.key == c
                    && (p.scopes.is_empty() || p.scopes.iter().any(|s| s == &self.kind_plural))
            })
            .cloned()
        else {
            return;
        };
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection for plugin");
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let ctx = self.cluster.context.clone();
        let res = self.kind_plural.clone();
        let subst = |s: &str| {
            s.replace("$NAMESPACE", &ns)
                .replace("$NS", &ns)
                .replace("$NAME", &name)
                .replace("$CONTEXT", &ctx)
                .replace("$RESOURCE", &res)
        };
        let mut argv = vec![subst(&plugin.command)];
        argv.extend(plugin.args.iter().map(|a| subst(a)));
        self.flash = format!("plugin: {}", plugin.name);
        self.flash_err = false;
        self.pending = Some(Suspend::Shell(argv));
    }

    fn key_command(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Table,
            KeyCode::Down | KeyCode::Tab => {
                if !self.cmd_suggestions.is_empty() {
                    self.cmd_sel = (self.cmd_sel + 1) % self.cmd_suggestions.len();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if !self.cmd_suggestions.is_empty() {
                    self.cmd_sel = self
                        .cmd_sel
                        .checked_sub(1)
                        .unwrap_or(self.cmd_suggestions.len() - 1);
                }
            }
            KeyCode::Enter => {
                let typed = self.command.trim().to_string();
                let picked = self.cmd_suggestions.get(self.cmd_sel).cloned();
                self.mode = Mode::Table;
                self.command.clear();
                // An exact typed built-in wins (stable muscle memory), then the
                // highlighted suggestion, then the raw typed text as a resource.
                if self.run_palette_command(&typed) {
                    // handled
                } else if let Some(s) = picked {
                    match s.kind {
                        SuggestKind::Command => {
                            self.run_palette_command(&s.label);
                        }
                        SuggestKind::Resource => self.switch_kind(&s.label),
                    }
                } else if !typed.is_empty() {
                    self.switch_kind(&typed);
                }
            }
            KeyCode::Backspace => {
                self.command.pop();
                self.update_suggestions();
            }
            KeyCode::Char(c) => {
                self.command.push(c);
                self.update_suggestions();
            }
            _ => {}
        }
    }

    /// Run a built-in palette action.
    fn run_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Quit => self.should_quit = true,
            PaletteAction::Ctx => self.open_contexts(),
            PaletteAction::Pulse => self.open_pulse(),
            PaletteAction::Xray => self.open_xray(),
            PaletteAction::Diff => self.open_diff(),
            PaletteAction::Events => self.open_events(),
            PaletteAction::PortForwards => self.open_port_forwards(),
            PaletteAction::Skin => self.open_skins(),
        }
    }

    /// Run a built-in command by any of its names/aliases. Returns `false` for
    /// empty or unknown input (so the caller can fall back to a resource kind).
    fn run_palette_command(&mut self, cmd: &str) -> bool {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return false;
        }
        let mut parts = cmd.split_whitespace();
        if let Some(first) = parts.next()
            && first.eq_ignore_ascii_case("skin")
        {
            let rest = parts.collect::<Vec<_>>().join(" ");
            if rest.is_empty() {
                self.open_skins();
            } else {
                self.apply_skin(&rest);
            }
            return true;
        }
        let action = PALETTE_COMMANDS
            .iter()
            .find(|c| c.names.contains(&cmd))
            .map(|c| c.action);
        match action {
            Some(a) => {
                self.run_action(a);
                true
            }
            None => false,
        }
    }

    /// Recompute the command-palette suggestions: built-in commands and resource
    /// kinds, fuzzy-matched together. An empty query lists the resource catalog
    /// only (the browse default), so pressing `:`⏎ never fires a command.
    fn update_suggestions(&mut self) {
        let q = self.command.trim();
        let mut scored: Vec<(i64, Suggestion)> = Vec::new();

        // Built-in commands: fuzzy over all names, display the canonical one.
        // Skipped for an empty query so they don't pre-empt the resource list.
        if !q.is_empty() {
            for c in PALETTE_COMMANDS {
                let best = c
                    .names
                    .iter()
                    .filter_map(|n| self.matcher.fuzzy_match(n, q))
                    .max();
                if let Some(score) = best {
                    scored.push((
                        score,
                        Suggestion {
                            label: c.names[0].to_string(),
                            kind: SuggestKind::Command,
                        },
                    ));
                }
            }
        }

        // An exact alias/kind/plural hit (e.g. `hr` → helmreleases) outranks
        // every fuzzy match, so a shorthand lands on its target instead of an
        // alphabetically-earlier lookalike (hr → horizontalpodautoscalers).
        let alias_target = if q.is_empty() {
            None
        } else {
            self.cluster.resolve(q).map(|k| k.ar.plural.to_lowercase())
        };

        // Resource catalog (RBAC-filtered).
        for c in self.cluster.catalog.iter().filter(|c| self.rbac_visible(c)) {
            let score = if q.is_empty() {
                Some(0)
            } else if alias_target.as_deref() == Some(c.as_str()) {
                Some(i64::MAX)
            } else {
                self.matcher.fuzzy_match(c, q)
            };
            if let Some(score) = score {
                scored.push((
                    score,
                    Suggestion {
                        label: c.clone(),
                        kind: SuggestKind::Resource,
                    },
                ));
            }
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        self.cmd_suggestions = scored.into_iter().take(100).map(|(_, s)| s).collect();
        self.cmd_sel = 0;
    }

    fn key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Table;
            }
            KeyCode::Enter => self.mode = Mode::Table,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
        self.invalidate_rows();
        self.table_state.select(Some(0));
    }

    fn key_scroll(&mut self, key: KeyEvent, detail: bool) {
        let target = if detail {
            &mut self.detail
        } else {
            &mut self.logs.view
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // The underlying view (table/xray) watch kept running, so there
                // is nothing to restart — just stop the log streams and return,
                // landing back on the same row.
                if !detail {
                    self.stop_log_stream();
                } else if self.mode == Mode::Events {
                    self.stop_event_stream();
                }
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => target.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => target.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => target.scroll_by(20),
            KeyCode::PageUp => target.scroll_by(-20),
            KeyCode::Char('g') | KeyCode::Home => target.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => {
                target.scroll = target.lines.len().saturating_sub(1)
            }
            _ => {}
        }
    }

    fn key_logs(&mut self, key: KeyEvent) {
        // Ctrl-S saves the buffer to a file (k9s).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.save_logs();
            return;
        }
        match key.code {
            // k9s: `s` toggles autoscroll/follow (we also accept `f`).
            KeyCode::Char('s') | KeyCode::Char('f') => {
                self.logs.follow = !self.logs.follow;
                if self.logs.follow {
                    // Resumed tailing — trim the backlog accumulated while paused.
                    let overflow = self.logs.view.lines.len().saturating_sub(MAX_LOG_LINES);
                    if overflow > 0 {
                        self.logs.view.lines.drain(0..overflow);
                    }
                }
                self.flash = format!(
                    "autoscroll: {}",
                    if self.logs.follow { "on" } else { "off" }
                );
                self.flash_err = false;
                return;
            }
            // k9s: `w` toggles line wrap.
            KeyCode::Char('w') => {
                self.logs.wrap = !self.logs.wrap;
                self.flash = format!("wrap: {}", if self.logs.wrap { "on" } else { "off" });
                self.flash_err = false;
                return;
            }
            // k9s: `t` toggles timestamps (re-streams).
            KeyCode::Char('t') => {
                self.logs.timestamps = !self.logs.timestamps;
                self.flash = format!(
                    "timestamps: {}",
                    if self.logs.timestamps { "on" } else { "off" }
                );
                self.flash_err = false;
                if !self.logs.stopped {
                    self.retail_logs();
                }
                return;
            }
            // Stop / resume the live stream.
            KeyCode::Char('x') => {
                if self.logs.stopped {
                    self.logs.stopped = false;
                    self.flash = "log stream resumed".into();
                    self.flash_err = false;
                    self.retail_logs();
                } else {
                    self.logs.stopped = true;
                    self.stop_log_stream(); // abort log tasks; view watch untouched
                    self.flash = "log stream stopped (x to resume)".into();
                    self.flash_err = false;
                }
                return;
            }
            // k9s: `c` copies the (filtered) buffer to the clipboard.
            KeyCode::Char('c') => {
                self.copy_logs();
                return;
            }
            KeyCode::Char('/') => {
                self.mode = Mode::LogFilter;
                return;
            }
            _ => {}
        }
        // Navigation. Any manual upward/relative move drops autoscroll and
        // freezes the view; jumping to the bottom (G/End) re-arms it, like
        // k9s. Scroll is clamped in display-row units (`viewport_rows`) so a
        // wrapped buffer doesn't jump to a stale line index when paused.
        let page = self.logs.viewport_h.max(1);
        // Deepest useful offset: last full page pinned to the viewport bottom.
        let max = self.logs.viewport_rows.saturating_sub(self.logs.viewport_h);
        let cur = self.logs.view.scroll;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.stop_log_stream();
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_add(1).min(max);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_sub(1);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_add(page).min(max);
            }
            KeyCode::PageUp => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_sub(page);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.logs.follow = false;
                self.logs.view.scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                // Resume autoscroll; the next draw anchors to the bottom.
                self.logs.follow = true;
            }
            _ => {}
        }
    }

    fn key_log_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.logs.filter.clear();
                self.mode = Mode::Logs;
            }
            KeyCode::Enter => self.mode = Mode::Logs,
            KeyCode::Backspace => {
                self.logs.filter.pop();
            }
            KeyCode::Char(c) => self.logs.filter.push(c),
            _ => {}
        }
    }

    /// Open the namespace switcher immediately with a loading placeholder, then
    /// fetch the list off-thread (it arrives as `Msg::Namespaces`).
    fn open_namespaces(&mut self) {
        self.ns_list = vec!["<all>".into()];
        self.ns_state.select(Some(0));
        self.ns_filter.clear();
        self.mode = Mode::Namespaces;
        let client = self.cluster.client.clone();
        let kind = self.cluster.resolve("namespaces").map(|k| k.ar);
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let Some(ar) = kind else { return };
            let api: Api<DynamicObject> = Api::all_with(client, &ar);
            if let Ok(list) = api.list(&ListParams::default()).await {
                let mut names: Vec<String> = list
                    .items
                    .into_iter()
                    .filter_map(|o| o.metadata.name)
                    .collect();
                names.sort();
                names.insert(0, "<all>".into());
                let _ = tx
                    .send(Msg::Namespaces {
                        generation: genr,
                        list: names,
                    })
                    .await;
            }
        });
    }

    /// Namespaces for the switcher: `<all>` is always pinned first, the rest
    /// fuzzy-matched against the type-to-filter buffer.
    pub fn filtered_namespaces(&self) -> Vec<String> {
        let mut out = vec!["<all>".to_string()];
        let rest = self.ns_list.iter().filter(|n| n.as_str() != "<all>");
        if self.ns_filter.is_empty() {
            out.extend(rest.cloned());
        } else {
            let mut scored: Vec<(i64, &String)> = rest
                .filter_map(|n| self.matcher.fuzzy_match(n, &self.ns_filter).map(|s| (s, n)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            out.extend(scored.into_iter().map(|(_, n)| n.clone()));
        }
        out
    }

    fn key_namespaces(&mut self, key: KeyEvent) {
        let len = self.filtered_namespaces().len();
        match key.code {
            KeyCode::Esc => {
                // First esc clears the filter and jumps back to the top
                // (`<all>`); a second esc closes the switcher.
                if self.ns_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.ns_filter.clear();
                    self.ns_state.select(Some(0));
                }
            }
            KeyCode::Down => list_step(&mut self.ns_state, len, true),
            KeyCode::Up => list_step(&mut self.ns_state, len, false),
            KeyCode::Enter => {
                let filtered = self.filtered_namespaces();
                let has_real_match = filtered.iter().any(|n| n != "<all>");
                let chosen = if !self.ns_filter.trim().is_empty() && !has_real_match {
                    // Typed text matches no listed namespace → take it verbatim
                    // so you can still switch when listing is restricted.
                    Some(self.ns_filter.trim().to_string())
                } else {
                    self.ns_state
                        .selected()
                        .and_then(|i| filtered.get(i).cloned())
                };
                if let Some(ns) = chosen {
                    self.set_namespace(ns);
                }
            }
            KeyCode::Backspace => {
                self.ns_filter.pop();
                self.select_best_namespace_match();
            }
            KeyCode::Char(c) => {
                self.ns_filter.push(c);
                self.select_best_namespace_match();
            }
            _ => {}
        }
    }

    /// Jump the namespace-switcher cursor to the best fuzzy match after the
    /// filter buffer changes. `<all>` stays pinned at index 0 of the list (so
    /// it's always reachable), but it should only be *selected* by default
    /// when browsing with no filter — once you've typed something with a
    /// real match, that match belongs under the cursor, not `<all>`.
    fn select_best_namespace_match(&mut self) {
        let idx = if !self.ns_filter.is_empty() && self.filtered_namespaces().len() > 1 {
            1 // right after the pinned <all> — the top-scored real match
        } else {
            0
        };
        self.ns_state.select(Some(idx));
    }

    fn set_namespace(&mut self, sel: String) {
        self.namespace = if sel == "<all>" || sel.is_empty() {
            String::new()
        } else {
            sel
        };
        let label = if self.namespace.is_empty() {
            "all namespaces".to_string()
        } else {
            self.namespace.clone()
        };
        self.flash = format!("namespace: {label}");
        self.flash_err = false;
        self.ns_filter.clear();
        self.mode = Mode::Table;
        self.table_state.select(Some(0));
        self.start_watch();
    }

    fn open_contexts(&mut self) {
        self.ctx_filter.clear();
        self.ctx_list.clear();
        self.ctx_state.select(None);
        self.mode = Mode::Contexts;
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let mut list = Cluster::list_contexts();
            list.sort();
            let _ = tx
                .send(Msg::Contexts {
                    generation: genr,
                    list,
                })
                .await;
        });
    }

    /// Contexts for the switcher, fuzzy-matched against the type-to-filter
    /// buffer (see `filtered_namespaces` for the same pattern).
    pub fn filtered_contexts(&self) -> Vec<String> {
        if self.ctx_filter.is_empty() {
            return self.ctx_list.clone();
        }
        let mut scored: Vec<(i64, &String)> = self
            .ctx_list
            .iter()
            .filter_map(|c| {
                self.matcher
                    .fuzzy_match(c, &self.ctx_filter)
                    .map(|s| (s, c))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().map(|(_, c)| c.clone()).collect()
    }

    fn key_contexts(&mut self, key: KeyEvent) {
        let len = self.filtered_contexts().len();
        match key.code {
            KeyCode::Esc => {
                // First esc clears the filter, second closes the switcher.
                if self.ctx_filter.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    self.ctx_filter.clear();
                    self.ctx_state.select(Some(0));
                }
            }
            KeyCode::Down => list_step(&mut self.ctx_state, len, true),
            KeyCode::Up => list_step(&mut self.ctx_state, len, false),
            KeyCode::Enter => {
                if let Some(name) = self
                    .ctx_state
                    .selected()
                    .and_then(|i| self.filtered_contexts().get(i).cloned())
                {
                    self.mode = Mode::Table;
                    self.switch_context(name);
                }
            }
            KeyCode::Backspace => {
                self.ctx_filter.pop();
                self.ctx_state.select(Some(0));
            }
            KeyCode::Char(c) => {
                self.ctx_filter.push(c);
                self.ctx_state.select(Some(0));
            }
            _ => {}
        }
    }

    /// Rebuild the cluster connection against a different kubeconfig context.
    /// Reconnecting re-runs API discovery, which can take seconds, so it runs
    /// off-thread; the new cluster (or error) arrives as `Msg::ContextSwitched`.
    fn switch_context(&mut self, name: String) {
        if name == self.cluster.context {
            return;
        }
        self.flash = format!("switching to {name}…");
        self.flash_err = false;
        // Stop the current context's watches and clear stale rows while we
        // reconnect; the new watch starts when the connection lands.
        self.bump_generation();
        self.store.clear();
        self.invalidate_rows();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let result = Cluster::connect_context(&name)
                .await
                .map(Box::new)
                .map_err(|e| e.to_string());
            let _ = tx
                .send(Msg::ContextSwitched {
                    generation: genr,
                    name,
                    result,
                })
                .await;
        });
    }

    /// Install a freshly-connected cluster from a context switch.
    fn apply_context_switch(&mut self, name: String, mut cluster: Box<Cluster>) {
        cluster.add_aliases(&self.user_aliases);
        self.bump_generation();
        self.namespace = cluster.default_namespace.clone();
        self.cluster = *cluster;
        self.stack.clear();
        self.kind = None;
        self.kind_plural.clear();
        self.labels = None;
        self.fields = None;
        self.scope_label = None;
        self.filter.clear();
        // Permissions differ per cluster — drop the old allow-list.
        self.rbac_allowed = None;
        self.last_rbac_ns = None;
        self.flash = format!("context: {name}");
        self.flash_err = false;
        self.switch_kind("pods");
    }

    /// Open the pulse / cluster-health dashboard (k9s `:pulse`).
    pub fn open_pulse(&mut self) {
        self.bump_generation();
        self.pulse = Pulse::default();
        self.flash = "pulse — cluster health".into();
        self.flash_err = false;
        self.mode = Mode::Pulse;
        self.spawn_pulse();
    }

    fn spawn_pulse(&mut self) {
        let resolve = |n: &str| self.cluster.resolve(n).map(|k| (k.ar, k.namespaced));
        let nodes = resolve("nodes");
        let pods = resolve("pods");
        let deploys = resolve("deployments");
        let sts = resolve("statefulsets");
        let ds = resolve("daemonsets");
        let jobs = resolve("jobs");
        let pvc = resolve("persistentvolumeclaims");

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        // Cluster-health snapshot: always spans every namespace, regardless
        // of whatever namespace filter was active in the table view.
        let ns = String::new();

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                let mut p = Pulse::default();

                if let Some((ar, _)) = &nodes {
                    let items = list_kind(&client, ar, false, "").await;
                    p.nodes_total = items.len();
                    p.nodes_ready = items.iter().filter(|o| node_ready(o)).count();
                }
                if let Some((ar, nsd)) = &pods {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.pods_total = items.len();
                    for o in &items {
                        match phase(o).as_str() {
                            "Running" => p.pods_running += 1,
                            "Pending" => p.pods_pending += 1,
                            "Failed" => p.pods_failed += 1,
                            "Succeeded" => p.pods_succeeded += 1,
                            _ => {}
                        }
                    }
                }
                if let Some((ar, nsd)) = &deploys {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.deploys_total = items.len();
                    p.deploys_ready = items
                        .iter()
                        .filter(|o| ready_eq(o, "/status/readyReplicas", "/spec/replicas"))
                        .count();
                }
                if let Some((ar, nsd)) = &sts {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.sts_total = items.len();
                    p.sts_ready = items
                        .iter()
                        .filter(|o| ready_eq(o, "/status/readyReplicas", "/spec/replicas"))
                        .count();
                }
                if let Some((ar, nsd)) = &ds {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.ds_total = items.len();
                    p.ds_ready = items
                        .iter()
                        .filter(|o| {
                            ready_eq(o, "/status/numberReady", "/status/desiredNumberScheduled")
                        })
                        .count();
                }
                if let Some((ar, nsd)) = &jobs {
                    p.jobs_total = list_kind(&client, ar, *nsd, &ns).await.len();
                }
                if let Some((ar, nsd)) = &pvc {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.pvc_total = items.len();
                    p.pvc_bound = items.iter().filter(|o| phase(o) == "Bound").count();
                }

                if tx
                    .send(Msg::PulseData {
                        generation: genr,
                        data: p,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    fn key_pulse(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Table;
                self.start_watch();
            }
            KeyCode::Char('r') => {
                self.bump_generation();
                self.spawn_pulse();
            }
            _ => {}
        }
    }

    /// Open the xray tree for the current kind (owner → children → containers).
    pub fn open_xray(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        self.bump_generation();
        self.xray_items.clear();
        self.xray_state.select(Some(0));
        self.flash = format!("xray: {}", self.kind_plural);
        self.flash_err = false;
        self.mode = Mode::Xray;
        self.spawn_xray();
    }

    fn spawn_xray(&mut self) {
        let Some((root_ar, root_nsd)) = self.kind.as_ref().map(|k| (k.ar.clone(), k.namespaced))
        else {
            return;
        };
        let root_kind = trim_s(&self.kind_plural).to_string();
        let pool_kinds: Vec<(String, ApiResource, bool)> = xray_pool_plurals(&root_kind)
            .iter()
            .filter_map(|plural| {
                self.cluster
                    .resolve(plural)
                    .map(|k| (trim_s(plural).to_string(), k.ar, k.namespaced))
            })
            .collect();

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        let ns = self.namespace.clone();

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                let roots = list_kind(&client, &root_ar, root_nsd, &ns).await;
                let mut pool: Vec<(String, DynamicObject)> = Vec::new();
                for (label, ar, namespaced) in &pool_kinds {
                    for o in list_kind(&client, ar, *namespaced, &ns).await {
                        pool.push((label.clone(), o));
                    }
                }

                // Index children by owner uid.
                let mut children: HashMap<String, Vec<(String, DynamicObject)>> = HashMap::new();
                for (label, o) in &pool {
                    if let Some(owners) = &o.metadata.owner_references {
                        for owner in owners {
                            children
                                .entry(owner.uid.clone())
                                .or_default()
                                .push((label.clone(), o.clone()));
                        }
                    }
                }

                let mut items = Vec::new();
                for root in &roots {
                    emit_xray(&root_kind, root, 0, &children, &mut items);
                }

                if tx
                    .send(Msg::XrayData {
                        generation: genr,
                        items,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    fn key_xray(&mut self, key: KeyEvent) {
        let len = self.xray_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Table;
                self.start_watch();
            }
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.xray_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.xray_state, len, false),
            KeyCode::Char('g') | KeyCode::Home => self.xray_state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                if len > 0 {
                    self.xray_state.select(Some(len - 1));
                }
            }
            // Enter on a pod/container streams logs.
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(i) = self.xray_state.selected()
                    && let Some(item) = self.xray_items.get(i).cloned()
                {
                    match item.kind.as_str() {
                        "container" => self.launch_logs(
                            LogSource::Single {
                                ns: item.ns,
                                pod: item.name.clone(),
                                container: item.container.clone(),
                                previous: false,
                            },
                            format!(
                                "{}:{} — logs",
                                item.name,
                                item.container.unwrap_or_default()
                            ),
                        ),
                        "pod" => self.launch_logs(
                            LogSource::Pod {
                                ns: item.ns,
                                name: item.name.clone(),
                                containers: vec![],
                            },
                            format!("{} — logs", item.name),
                        ),
                        _ => self.flash_warn("logs available on pods/containers"),
                    }
                }
            }
            KeyCode::Char('r') => {
                self.bump_generation();
                self.spawn_xray();
            }
            _ => {}
        }
    }

    fn key_containers(&mut self, key: KeyEvent) {
        let len = self.container_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.container_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.container_state, len, false),
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.launch_logs(
                        LogSource::Single {
                            ns,
                            pod: name.clone(),
                            container: Some(c.clone()),
                            previous: false,
                        },
                        format!("{name}:{c} — logs"),
                    );
                }
            }
            KeyCode::Char('p') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.launch_logs(
                        LogSource::Single {
                            ns,
                            pod: name.clone(),
                            container: Some(c.clone()),
                            previous: true,
                        },
                        format!("{name}:{c} — previous logs"),
                    );
                }
            }
            _ => {}
        }
    }

    fn key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(action) = self.confirm_action.take() {
                    match action {
                        ConfirmAction::Delete { targets, force } => {
                            self.do_delete(targets, force);
                            self.marked.clear();
                        }
                        ConfirmAction::Drain { targets } => {
                            self.do_drain_nodes(targets);
                            self.marked.clear();
                        }
                    }
                }
                self.mode = Mode::Table;
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                let update = match self.confirm_action.as_mut() {
                    Some(ConfirmAction::Delete { targets, force }) => {
                        *force = !*force;
                        Some((targets.clone(), *force))
                    }
                    _ => None,
                };
                if let Some((targets, force)) = update {
                    self.confirm_label = delete_confirm_label(&self.kind_plural, &targets, force);
                }
            }
            _ => {
                self.confirm_action = None;
                self.mode = Mode::Table;
            }
        }
    }

    fn key_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.prompt_kind = None;
                self.mode = Mode::Table;
            }
            KeyCode::Enter => {
                let input = self.prompt_input.trim().to_string();
                self.mode = Mode::Table;
                match self.prompt_kind.take() {
                    Some(PromptKind::Scale { ns, name }) => match input.parse::<i32>() {
                        Ok(n) if n >= 0 => self.do_scale(ns, name, n),
                        _ => self.flash_warn("invalid replica count"),
                    },
                    Some(PromptKind::PortForward { ns, name }) => {
                        if input.is_empty() {
                            self.flash_warn("no ports given");
                        } else {
                            let target = if self.kind_plural == "services" {
                                format!("svc/{name}")
                            } else {
                                name
                            };
                            self.start_port_forward(ns, target, input);
                        }
                    }
                    Some(PromptKind::SetImage {
                        ns,
                        name,
                        plural,
                        container,
                    }) => {
                        if input.is_empty() {
                            self.flash_warn("no image given");
                        } else {
                            self.do_set_image(ns, name, plural, container, input);
                        }
                    }
                    None => {}
                }
            }
            KeyCode::Backspace => {
                self.prompt_input.pop();
            }
            KeyCode::Char(c) => self.prompt_input.push(c),
            _ => {}
        }
    }
}

// ----- free helpers ------------------------------------------------------

fn restart_patch(restarted_at: &str) -> Value {
    json!({
        "spec": { "template": { "metadata": { "annotations": {
            "kubectl.kubernetes.io/restartedAt": restarted_at
        }}}}
    })
}

fn set_image_patch(plural: &str, container: &str, image: &str) -> Value {
    let containers = json!([{ "name": container, "image": image }]);
    if plural == "pods" {
        json!({ "spec": { "containers": containers } })
    } else {
        json!({ "spec": { "template": { "spec": { "containers": containers } } } })
    }
}

fn scale_patch(replicas: i32) -> Value {
    json!({ "spec": { "replicas": replicas } })
}

fn suspend_patch(suspend: bool) -> Value {
    json!({ "spec": { "suspend": suspend } })
}

fn reconcile_patch(requested_at: &str) -> Value {
    json!({
        "metadata": { "annotations": { "reconcile.fluxcd.io/requestedAt": requested_at } }
    })
}

fn external_secret_refresh_patch(force_sync: &str) -> Value {
    json!({
        "metadata": { "annotations": { "force-sync": force_sync } }
    })
}

fn node_unschedulable_patch(unschedulable: bool) -> Value {
    json!({ "spec": { "unschedulable": unschedulable } })
}

fn delete_confirm_label(kind_plural: &str, targets: &[(String, String)], force: bool) -> String {
    let verb = if force { "Force delete" } else { "Delete" };
    if targets.len() == 1 {
        let (name, ns) = &targets[0];
        let where_ns = if ns.is_empty() {
            String::new()
        } else {
            format!(" in {ns}")
        };
        format!("{verb} {} {name}{where_ns}?", trim_s(kind_plural))
    } else {
        format!("{verb} {} {}?", targets.len(), kind_plural)
    }
}

fn drainable_pod(pod: &Pod) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
    if pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
    {
        return false;
    }
    if pod
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner.kind.eq_ignore_ascii_case("DaemonSet"))
        })
    {
        return false;
    }
    !matches!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

fn eviction_unsupported(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(api_err) if matches!(api_err.code, 404 | 405))
}

/// Pick a version name to query a CRD's custom resources: the storage version
/// if flagged, else the first served version, else the first listed.
fn crd_served_version(d: &Value) -> Option<String> {
    let versions = d.pointer("/spec/versions")?.as_array()?;
    let pick = versions
        .iter()
        .find(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
        .or_else(|| {
            versions
                .iter()
                .find(|v| v.get("served").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| versions.first())?;
    pick.get("name").and_then(Value::as_str).map(String::from)
}

/// Build a `k=v,k2=v2` selector string from `spec/<field>` (matchLabels for
/// workloads, selector map for services).
fn label_selector(obj: &DynamicObject, field: &str) -> Option<String> {
    let path = if field == "matchLabels" {
        vec!["spec", "selector", "matchLabels"]
    } else {
        vec!["spec", "selector"]
    };
    let mut cur = &obj.data;
    for p in path {
        cur = cur.get(p)?;
    }
    let map = cur.as_object()?;
    if map.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = map
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|vs| format!("{k}={vs}")))
        .collect();
    parts.sort();
    Some(parts.join(","))
}

fn container_names(obj: &DynamicObject) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["containers", "initContainers", "ephemeralContainers"] {
        if let Some(arr) = obj
            .data
            .pointer(&format!("/spec/{key}"))
            .and_then(Value::as_array)
        {
            for c in arr {
                if let Some(n) = c.get("name").and_then(Value::as_str) {
                    names.push(n.to_string());
                }
            }
        }
    }
    names
}

/// Trim a trailing plural "s" for breadcrumb labels (deployments -> deployment).
fn trim_s(plural: &str) -> &str {
    plural.strip_suffix('s').unwrap_or(plural)
}

fn xray_pool_plurals(root_kind: &str) -> &'static [&'static str] {
    match root_kind {
        "pod" => &[],
        "cronjob" => &["jobs", "pods"],
        "job" | "daemonset" | "replicaset" | "statefulset" => &["pods"],
        "deployment" => &["replicasets", "pods"],
        _ => &["replicasets", "pods"],
    }
}

/// Columns whose cell is a count/number and should sort numerically.
fn is_numeric_header(header: &str) -> bool {
    matches!(
        header,
        "READY"
            | "RESTARTS"
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
fn parse_leading_num(s: &str) -> f64 {
    let t = s.trim_start_matches(|c: char| !c.is_ascii_digit() && c != '-');
    let end = t
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(t.len());
    t[..end].parse::<f64>().unwrap_or(0.0)
}

/// Move a list selection one step, clamped to `[0, len)`. Shared by every
/// modal picker (namespaces, contexts, containers, set-image, xray).
fn list_step(state: &mut ListState, len: usize, down: bool) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let next = if down {
        (i + 1).min(len - 1)
    } else {
        i.saturating_sub(1)
    };
    state.select(Some(next));
}

/// Copy text to the system clipboard via the first available OS tool, falling
/// back to OSC 52 for remote terminals where local clipboard tools are absent.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // tool not installed — try the next one
        };
        // Write must finish (and the pipe close) before we wait, or the child
        // can block; report success only if the write and the process succeed.
        let wrote = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        if wrote && ok {
            return true;
        }
    }
    copy_to_clipboard_osc52(text)
}

fn copy_to_clipboard_osc52(text: &str) -> bool {
    use std::fs::OpenOptions;
    use std::io::{Write, stdout};

    let sequence = osc52_sequence(text);
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        return tty
            .write_all(sequence.as_bytes())
            .and_then(|_| tty.flush())
            .is_ok();
    }

    let mut out = stdout();
    out.write_all(sequence.as_bytes())
        .and_then(|_| out.flush())
        .is_ok()
}

fn osc52_sequence(text: &str) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

async fn forward_log_stream(
    api: Api<Pod>,
    pod: String,
    lp: LogParams,
    prefix: String,
    tx: Sender<Msg>,
    generation: u64,
    flag: Arc<AtomicU64>,
) {
    use futures_util::{AsyncBufReadExt, TryStreamExt};
    use tokio::time::MissedTickBehavior;

    let stream = match api.log_stream(&pod, &lp).await {
        Ok(stream) => stream,
        Err(e) => {
            let _ = tx
                .send(Msg::LogLines {
                    generation,
                    lines: vec![format!("[error] {e}")],
                })
                .await;
            return;
        }
    };

    let mut lines = stream.lines();
    let mut batch = Vec::with_capacity(LOG_BATCH_LINES);
    let mut flush = tokio::time::interval(Duration::from_millis(LOG_BATCH_MS));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if flag.load(Ordering::SeqCst) != generation {
            break;
        }

        tokio::select! {
            next = lines.try_next() => {
                match next {
                    Ok(Some(line)) => {
                        batch.push(format!("{prefix}{line}"));
                        if batch.len() >= LOG_BATCH_LINES
                            && !send_log_batch(&tx, generation, &mut batch).await
                        {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        batch.push(format!("[error] {e}"));
                        break;
                    }
                }
            }
            _ = flush.tick(), if !batch.is_empty() => {
                if !send_log_batch(&tx, generation, &mut batch).await {
                    break;
                }
            }
        }
    }

    if flag.load(Ordering::SeqCst) == generation {
        let _ = send_log_batch(&tx, generation, &mut batch).await;
    }
}

async fn send_log_batch(tx: &Sender<Msg>, generation: u64, batch: &mut Vec<String>) -> bool {
    if batch.is_empty() {
        return true;
    }
    let lines = std::mem::take(batch);
    tx.send(Msg::LogLines { generation, lines }).await.is_ok()
}

async fn send_event_snapshot(
    tx: &Sender<Msg>,
    generation: u64,
    title: &str,
    items: &HashMap<String, DynamicObject>,
    events_v1: bool,
) -> bool {
    tx.send(Msg::Events {
        generation,
        title: title.to_string(),
        lines: format_event_lines(items.values(), events_v1),
    })
    .await
    .is_ok()
}

fn format_event_lines<'a, I>(events: I, events_v1: bool) -> Vec<String>
where
    I: IntoIterator<Item = &'a DynamicObject>,
{
    let mut rows: Vec<(String, String)> = events
        .into_iter()
        .map(|event| {
            let seen = event_time(event, events_v1);
            (seen.clone(), event_line(event, events_v1, &seen))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut lines = vec![format!(
        "{:<20} {:<8} {:<24} {:>5} {}",
        "LAST SEEN", "TYPE", "REASON", "COUNT", "MESSAGE"
    )];
    if rows.is_empty() {
        lines.push("(no events)".into());
    } else {
        lines.extend(rows.into_iter().map(|(_, line)| line));
    }
    lines
}

fn event_line(event: &DynamicObject, events_v1: bool, seen: &str) -> String {
    let typ = svalue(&event.data, &["type"]).unwrap_or_default();
    let reason = svalue(&event.data, &["reason"]).unwrap_or_default();
    let count = event_count(event, events_v1);
    let message = if events_v1 {
        svalue(&event.data, &["note"])
            .or_else(|| svalue(&event.data, &["message"]))
            .unwrap_or_default()
    } else {
        svalue(&event.data, &["message"])
            .or_else(|| svalue(&event.data, &["note"]))
            .unwrap_or_default()
    };
    format!(
        "{:<20} {:<8} {:<24} {:>5} {}",
        compact_event_time(seen),
        typ,
        reason,
        count,
        message.replace('\n', " ")
    )
}

fn event_count(event: &DynamicObject, events_v1: bool) -> i64 {
    if events_v1 {
        ivalue(&event.data, &["series", "count"])
            .or_else(|| ivalue(&event.data, &["deprecatedCount"]))
            .unwrap_or(1)
    } else {
        ivalue(&event.data, &["count"]).unwrap_or(1)
    }
}

fn event_time(event: &DynamicObject, events_v1: bool) -> String {
    let data = &event.data;
    let value = if events_v1 {
        svalue(data, &["series", "lastObservedTime"])
            .or_else(|| svalue(data, &["eventTime"]))
            .or_else(|| svalue(data, &["deprecatedLastTimestamp"]))
            .or_else(|| svalue(data, &["deprecatedFirstTimestamp"]))
    } else {
        svalue(data, &["lastTimestamp"])
            .or_else(|| svalue(data, &["eventTime"]))
            .or_else(|| svalue(data, &["firstTimestamp"]))
    };
    value
        .or_else(|| {
            event
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0.to_string())
        })
        .unwrap_or_default()
}

fn compact_event_time(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('Z');
    if let Some((date, time)) = trimmed.split_once('T') {
        let time = time.split('.').next().unwrap_or(time);
        format!("{date} {time}")
    } else {
        raw.to_string()
    }
}

fn svalue(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(String::from)
}

fn ivalue(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_i64()
}

/// Recursively flatten an object and its owned children into xray rows.
fn emit_xray(
    kind: &str,
    obj: &DynamicObject,
    depth: usize,
    children: &std::collections::HashMap<String, Vec<(String, DynamicObject)>>,
    items: &mut Vec<XrayItem>,
) {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let ns = obj.metadata.namespace.clone().unwrap_or_default();
    items.push(XrayItem {
        depth,
        kind: kind.to_string(),
        name: name.clone(),
        ns: ns.clone(),
        status: xray_status(kind, obj),
        container: None,
    });

    if let Some(uid) = &obj.metadata.uid
        && let Some(kids) = children.get(uid)
    {
        for (clabel, cobj) in kids {
            emit_xray(clabel, cobj, depth + 1, children, items);
        }
    }

    // Pods expand into their containers as leaves.
    if kind == "pod" {
        for c in container_names(obj) {
            items.push(XrayItem {
                depth: depth + 1,
                kind: "container".into(),
                name: name.clone(),
                ns: ns.clone(),
                status: String::new(),
                container: Some(c),
            });
        }
    }
}

fn xray_status(kind: &str, o: &DynamicObject) -> String {
    match kind {
        "pod" => phase(o),
        "job" => format!(
            "{}/{}",
            o.data
                .pointer("/status/succeeded")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/spec/completions")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1),
        ),
        "cronjob" => format!(
            "active {}",
            o.data
                .pointer("/status/active")
                .and_then(Value::as_array)
                .map_or(0, |items| items.len()),
        ),
        "deployment" | "replicaset" | "statefulset" => format!(
            "{}/{}",
            o.data
                .pointer("/status/readyReplicas")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/spec/replicas")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        "daemonset" => format!(
            "{}/{}",
            o.data
                .pointer("/status/numberReady")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/status/desiredNumberScheduled")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        _ => String::new(),
    }
}

/// List all objects of a kind (namespaced to `ns` when applicable).
async fn list_kind(
    client: &Client,
    ar: &ApiResource,
    namespaced: bool,
    ns: &str,
) -> Vec<DynamicObject> {
    let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    api.list(&ListParams::default())
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

fn phase(o: &DynamicObject) -> String {
    o.data
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn node_ready(o: &DynamicObject) -> bool {
    o.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_str) == Some("True")
            })
        })
        .unwrap_or(false)
}

/// True when the two integer pointers are equal and non-zero (e.g. ready == desired).
fn ready_eq(o: &DynamicObject, ready_ptr: &str, want_ptr: &str) -> bool {
    let r = o
        .data
        .pointer(ready_ptr)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let w = o
        .data
        .pointer(want_ptr)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    w > 0 && r >= w
}

/// Extract (cpu millicores, memory bytes) from a metrics-API object.
fn usage_of(obj: &DynamicObject, is_node: bool) -> (i64, i64) {
    use crate::columns::{parse_cpu_milli, parse_mem_bytes};
    if is_node {
        let cpu = obj
            .data
            .pointer("/usage/cpu")
            .and_then(Value::as_str)
            .map(parse_cpu_milli)
            .unwrap_or(0);
        let mem = obj
            .data
            .pointer("/usage/memory")
            .and_then(Value::as_str)
            .map(parse_mem_bytes)
            .unwrap_or(0);
        (cpu, mem)
    } else {
        let mut cpu = 0;
        let mut mem = 0;
        if let Some(cs) = obj.data.pointer("/containers").and_then(Value::as_array) {
            for c in cs {
                if let Some(s) = c.pointer("/usage/cpu").and_then(Value::as_str) {
                    cpu += parse_cpu_milli(s);
                }
                if let Some(s) = c.pointer("/usage/memory").and_then(Value::as_str) {
                    mem += parse_mem_bytes(s);
                }
            }
        }
        (cpu, mem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::row_key;
    use serde_json::json;
    use tokio::sync::mpsc::{self, Receiver};

    fn obj(v: serde_json::Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    fn test_app() -> (App, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(1024);
        (App::new(Cluster::fake(), tx), rx)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Inject a watched object as the current generation would.
    fn apply(app: &mut App, v: serde_json::Value) {
        let o = obj(v);
        app.handle_msg(Msg::Applied {
            generation: app.generation,
            key: row_key(&o),
            obj: Box::new(o),
        });
    }

    #[tokio::test]
    async fn exact_alias_outranks_fuzzy_suggestions() {
        let (mut app, _rx) = test_app();
        // `hr` fuzzy-matches horizontalpodautoscalers too; the alias target
        // must still be the first suggestion.
        app.command = "hr".into();
        app.update_suggestions();
        let first = app.cmd_suggestions.first().expect("has suggestions");
        assert_eq!(first.label, "helmreleases");
        assert!(first.kind == SuggestKind::Resource);

        // A full plural typed exactly stays on top as well.
        app.command = "pods".into();
        app.update_suggestions();
        assert_eq!(app.cmd_suggestions[0].label, "pods");
    }

    #[test]
    fn list_step_clamps_both_ends() {
        let mut s = ListState::default();
        list_step(&mut s, 3, true);
        assert_eq!(s.selected(), Some(1));
        list_step(&mut s, 3, true);
        list_step(&mut s, 3, true); // would be 3, clamps to 2
        assert_eq!(s.selected(), Some(2));
        list_step(&mut s, 3, false);
        assert_eq!(s.selected(), Some(1));
        list_step(&mut s, 3, false);
        list_step(&mut s, 3, false); // clamps at 0
        assert_eq!(s.selected(), Some(0));

        let mut empty = ListState::default();
        list_step(&mut empty, 0, true);
        assert_eq!(empty.selected(), None); // no-op on empty list
    }

    #[test]
    fn scrollable_scroll_clamps() {
        let mut s = Scrollable {
            title: String::new(),
            lines: vec!["a".into(), "b".into(), "c".into()].into(),
            scroll: 0,
        };
        s.scroll_by(100);
        assert_eq!(s.scroll, 2); // last line index
        s.scroll_by(-100);
        assert_eq!(s.scroll, 0);
    }

    #[tokio::test]
    async fn move_selection_from_none_lands_on_first_row_not_second() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        for n in ["a", "b", "c"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Pod",
                       "metadata": {"name": n, "namespace": "default"}}),
            );
        }
        app.table_state.select(None); // simulate no selection at all
        app.move_selection(1); // Down, with nothing selected yet
        assert_eq!(app.table_state.selected(), Some(0), "must not skip row 0");
    }

    #[tokio::test]
    async fn switching_kind_resets_stale_selection_to_top() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        for n in ["a", "b", "c"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Pod",
                       "metadata": {"name": n, "namespace": "default"}}),
            );
        }
        app.table_state.select(Some(2)); // simulate cursor left on row 2

        app.switch_kind("deployments");
        assert_eq!(
            app.table_state.selected(),
            Some(0),
            "a fresh view must start with its first row selected, not a stale index"
        );
    }

    #[tokio::test]
    async fn namespace_filter_selects_best_match_not_all() {
        let (mut app, _rx) = test_app();
        app.ns_list = vec![
            "<all>".into(),
            "default".into(),
            "kube-system".into(),
            "prod".into(),
        ];
        app.ns_filter.clear();
        app.ns_state.select(Some(0));
        app.mode = Mode::Namespaces;

        for c in "sys".chars() {
            app.handle_key(press(KeyCode::Char(c))).unwrap();
        }
        // "kube-system" is the only real match — it should be under the
        // cursor, not the pinned "<all>" at index 0.
        let filtered = app.filtered_namespaces();
        let selected = app.ns_state.selected().and_then(|i| filtered.get(i));
        assert_eq!(selected.map(String::as_str), Some("kube-system"));

        // Clearing back to an empty filter returns the default to <all>.
        app.handle_key(press(KeyCode::Backspace)).unwrap();
        app.handle_key(press(KeyCode::Backspace)).unwrap();
        app.handle_key(press(KeyCode::Backspace)).unwrap();
        assert_eq!(app.ns_state.selected(), Some(0));
    }

    #[tokio::test]
    async fn filter_match_indices_highlight_matched_chars() {
        let (mut app, _rx) = test_app();
        assert_eq!(app.filter_match_indices("kube-httpcache-0"), None); // no filter

        app.filter = "khc".into();
        let idx = app.filter_match_indices("kube-httpcache-0").unwrap();
        // "k", "h", "c" fuzzy-match in order somewhere in the name.
        assert_eq!(idx.len(), 3);
        assert!(idx.is_sorted());

        app.filter = "zzz".into();
        assert_eq!(app.filter_match_indices("kube-httpcache-0"), None); // no match
    }

    #[tokio::test]
    async fn table_cell_cache_invalidates_on_apply() {
        let (mut app, _rx) = test_app();
        app.kind_plural = "pods".into();
        apply(
            &mut app,
            json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "web",
                    "namespace": "default",
                    "resourceVersion": "1"
                },
                "status": {"phase": "Pending"}
            }),
        );
        {
            let rows = app.rows();
            app.ensure_table_cell_cache(&rows);
            let key = row_key(rows[0]);
            let cache = app.table_cell_cache();
            let (cells, _) = cache.get(&key).unwrap();
            assert_eq!(cells[2], "Pending");
        }

        apply(
            &mut app,
            json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "web",
                    "namespace": "default",
                    "resourceVersion": "2"
                },
                "status": {"phase": "Running"}
            }),
        );
        let rows = app.rows();
        app.ensure_table_cell_cache(&rows);
        let key = row_key(rows[0]);
        let cache = app.table_cell_cache();
        let (cells, _) = cache.get(&key).unwrap();
        assert_eq!(cells[2], "Running");
    }

    #[tokio::test]
    async fn palette_merges_commands_with_resources() {
        let (mut app, _rx) = test_app();

        // Empty query lists resources only, so `:`⏎ never fires a command.
        app.command.clear();
        app.update_suggestions();
        assert!(
            app.cmd_suggestions
                .iter()
                .all(|s| s.kind == SuggestKind::Resource)
        );

        // Typing a command name surfaces it (this was the reported bug: `ctx`
        // used to show nothing).
        app.command = "ctx".into();
        app.update_suggestions();
        assert!(
            app.cmd_suggestions
                .iter()
                .any(|s| s.kind == SuggestKind::Command && s.label == "ctx")
        );

        // Aliases fuzzy-match too, but the canonical label is shown.
        app.command = "dash".into();
        app.update_suggestions();
        assert!(
            app.cmd_suggestions
                .iter()
                .any(|s| s.kind == SuggestKind::Command && s.label == "pulse")
        );
    }

    #[tokio::test]
    async fn palette_command_dispatch() {
        let (mut app, _rx) = test_app();
        assert!(app.run_palette_command("q")); // alias for quit
        assert!(app.should_quit);

        let (mut app, _rx) = test_app();
        assert!(app.run_palette_command("contexts")); // alias resolves
        assert!(!app.run_palette_command("pods")); // resource kind, not a command
        assert!(!app.run_palette_command("")); // empty is never a command
    }

    #[tokio::test]
    async fn skin_palette_command_opens_picker() {
        let (mut app, _rx) = test_app();
        assert!(app.run_palette_command("skin"));
        assert_eq!(app.mode, Mode::Skins);
        assert_eq!(
            app.skin_list.first().map(String::as_str),
            Some("catppuccin-mocha")
        );

        app.mode = Mode::Table;
        assert!(app.run_palette_command("skin no-such-skin"));
        assert_eq!(app.mode, Mode::Table);
        assert!(app.flash_err);
        assert!(app.flash.contains("unknown skin"), "{}", app.flash);
    }

    #[tokio::test]
    async fn logs_pause_freezes_and_survives_new_lines() {
        let (mut app, _rx) = test_app();
        app.mode = Mode::Logs;
        app.return_mode = Mode::Table;
        // Simulate a drawn frame: 100 display rows, 40-high viewport → the
        // follow anchor (and deepest offset) is row 60.
        app.logs.follow = true;
        app.logs.view.scroll = 60;
        app.logs.viewport_rows = 100;
        app.logs.viewport_h = 40;

        // Scroll up → autoscroll stops and the offset steps back by one row.
        app.handle_key(press(KeyCode::Char('k'))).unwrap();
        assert!(!app.logs.follow);
        assert_eq!(app.logs.view.scroll, 59);

        // Lines keep streaming while paused; the frozen offset must not drift.
        for i in 0..500 {
            app.handle_msg(Msg::LogLines {
                generation: app.log_gen,
                lines: vec![format!("line {i}")],
            });
        }
        assert!(!app.logs.follow);
        assert_eq!(app.logs.view.scroll, 59);

        // `g` goes to the top and stays there (no snap-back to the bottom).
        app.handle_key(press(KeyCode::Char('g'))).unwrap();
        assert!(!app.logs.follow);
        assert_eq!(app.logs.view.scroll, 0);

        // `G` re-arms autoscroll (the next draw will re-anchor to the bottom).
        app.handle_key(press(KeyCode::Char('G'))).unwrap();
        assert!(app.logs.follow);

        // Down-scroll is clamped to the deepest offset (rows - height = 60), so
        // it can't overshoot past the bottom-pinned last page.
        app.logs.view.scroll = 60;
        app.handle_key(press(KeyCode::Char('j'))).unwrap();
        assert!(!app.logs.follow);
        assert_eq!(app.logs.view.scroll, 60);
    }

    #[tokio::test]
    async fn drill_into_workload_then_esc_restores() {
        let (mut app, _rx) = test_app();
        app.switch_kind("deployments");
        assert_eq!(app.kind_plural, "deployments");
        assert!(app.stack.is_empty(), "a `:resource` switch is a fresh root");

        apply(
            &mut app,
            json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": "web", "namespace": "default"},
                "spec": {"selector": {"matchLabels": {"app": "web"}}}
            }),
        );
        app.table_state.select(Some(0));
        assert_eq!(app.rows().len(), 1);

        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.kind_plural, "pods");
        assert_eq!(app.labels.as_deref(), Some("app=web"));
        assert_eq!(app.scope_label.as_deref(), Some("deployment/web"));
        assert_eq!(app.stack.len(), 1);

        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.kind_plural, "deployments");
        assert_eq!(app.labels, None);
        assert!(app.stack.is_empty());
    }

    #[tokio::test]
    async fn root_switch_clears_drill_stack() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "p", "namespace": "default"},
                   "spec": {}}),
        );
        // Manually push a frame to simulate having drilled in.
        app.push_frame();
        assert_eq!(app.stack.len(), 1);
        // A fresh `:resource` switch must reset the breadcrumb.
        app.switch_kind("services");
        assert_eq!(app.kind_plural, "services");
        assert!(app.stack.is_empty());
    }

    #[tokio::test]
    async fn filter_narrows_rows_via_cache() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        for n in ["alpha", "beta", "gamma"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Pod",
                       "metadata": {"name": n, "namespace": "default"}}),
            );
        }
        assert_eq!(app.rows().len(), 3);

        app.handle_key(press(KeyCode::Char('/'))).unwrap();
        for c in ['a', 'l', 'p'] {
            app.handle_key(press(KeyCode::Char(c))).unwrap();
        }
        let rows = app.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metadata.name.as_deref(), Some("alpha"));

        // Clearing the filter restores all rows (cache re-derived).
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.rows().len(), 3);
    }

    #[tokio::test]
    async fn delete_message_updates_rows() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "keep", "namespace": "default"}}),
        );
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "gone", "namespace": "default"}}),
        );
        assert_eq!(app.rows().len(), 2);
        app.handle_msg(Msg::Deleted {
            generation: app.generation,
            key: "default/gone".into(),
        });
        let rows = app.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metadata.name.as_deref(), Some("keep"));
    }

    #[tokio::test]
    async fn space_marks_rows_for_bulk_delete() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        for n in ["a", "b", "c"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Pod",
                       "metadata": {"name": n, "namespace": "default"}}),
            );
        }
        assert_eq!(app.rows().len(), 3);
        assert_eq!(app.table_state.selected(), Some(0));

        // Mark the first two rows; each SPACE also advances the cursor.
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        assert_eq!(app.marked.len(), 2);
        assert_eq!(app.table_state.selected(), Some(2));

        // A bulk action targets exactly the marked rows.
        let mut targets = app.action_targets();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("a".to_string(), "default".to_string()),
                ("b".to_string(), "default".to_string()),
            ]
        );

        // ctrl-d opens a confirm for the marked set…
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.mode, Mode::Confirm);
        assert!(
            app.confirm_label.contains("Delete 2 pods"),
            "{}",
            app.confirm_label
        );

        // …and confirming clears the marks.
        app.handle_key(press(KeyCode::Char('y'))).unwrap();
        assert!(app.marked.is_empty());
        assert_eq!(app.mode, Mode::Table);
    }

    #[tokio::test]
    async fn delete_confirm_force_can_toggle() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "web", "namespace": "default"}}),
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.mode, Mode::Confirm);
        assert!(app.confirm_allows_force_toggle());
        assert!(app.confirm_label.starts_with("Delete pod web"));
        assert!(matches!(
            app.confirm_action,
            Some(ConfirmAction::Delete { force: false, .. })
        ));

        app.handle_key(press(KeyCode::Char('f'))).unwrap();
        assert!(app.confirm_label.starts_with("Force delete pod web"));
        assert!(matches!(
            app.confirm_action,
            Some(ConfirmAction::Delete { force: true, .. })
        ));

        app.handle_key(press(KeyCode::Char('f'))).unwrap();
        assert!(app.confirm_label.starts_with("Delete pod web"));
        assert!(matches!(
            app.confirm_action,
            Some(ConfirmAction::Delete { force: false, .. })
        ));
    }

    #[tokio::test]
    async fn node_drain_key_opens_confirm_for_marked_nodes() {
        let (mut app, _rx) = test_app();
        app.switch_kind("nodes");
        for n in ["node-a", "node-b"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Node",
                       "metadata": {"name": n}}),
            );
        }
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        app.handle_key(press(KeyCode::Char(' '))).unwrap();

        app.handle_key(press(KeyCode::Char('D'))).unwrap();
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(
            app.confirm_label,
            "Drain 2 nodes? Cordon and evict eligible pods."
        );
        assert!(!app.confirm_allows_force_toggle());
        let Some(ConfirmAction::Drain { mut targets }) = app.confirm_action.take() else {
            panic!("expected drain confirm action");
        };
        targets.sort();
        assert_eq!(targets, vec!["node-a".to_string(), "node-b".to_string()]);
    }

    #[tokio::test]
    async fn esc_clears_marks_before_popping() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "a", "namespace": "default"}}),
        );
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        assert_eq!(app.marked.len(), 1);
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert!(app.marked.is_empty());
        assert_eq!(app.mode, Mode::Table);
    }

    #[tokio::test]
    async fn switching_kind_clears_marks() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "a", "namespace": "default"}}),
        );
        app.handle_key(press(KeyCode::Char(' '))).unwrap();
        assert_eq!(app.marked.len(), 1);
        app.switch_kind("deployments");
        assert!(app.marked.is_empty());
    }

    #[tokio::test]
    async fn flux_menu_rejects_non_flux_kinds() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "a", "namespace": "default"}}),
        );
        app.request_flux_menu();
        assert!(app.flash_err);
        assert!(app.flash.contains("Flux"), "{}", app.flash);
        assert_eq!(app.mode, Mode::Table); // never opens the menu
    }

    #[tokio::test]
    async fn flux_menu_requires_explicit_choice_not_a_single_key() {
        let (mut app, _rx) = test_app();
        app.switch_kind("kustomizations");
        apply(
            &mut app,
            json!({
                "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
                "metadata": {"name": "infra", "namespace": "default"},
                "spec": {"suspend": false}
            }),
        );

        // `t` opens the menu — nothing is patched yet.
        app.handle_key(press(KeyCode::Char('t'))).unwrap();
        assert_eq!(app.mode, Mode::FluxMenu);
        assert_eq!(app.flux_menu_state.selected(), Some(0)); // "Suspend"

        // Esc backs out without doing anything.
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(!app.flash.contains("suspending"));

        // Re-open, navigate to "Resume", confirm.
        app.handle_key(press(KeyCode::Char('t'))).unwrap();
        app.handle_key(press(KeyCode::Char('j'))).unwrap();
        assert_eq!(app.flux_menu_state.selected(), Some(1)); // "Resume"
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.flash.contains("resuming"), "{}", app.flash);
    }

    #[tokio::test]
    async fn flux_menu_cancel_item_does_nothing() {
        let (mut app, _rx) = test_app();
        app.switch_kind("kustomizations");
        apply(
            &mut app,
            json!({
                "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
                "metadata": {"name": "infra", "namespace": "default"},
                "spec": {"suspend": false}
            }),
        );
        let flash_before = app.flash.clone();
        app.request_flux_menu();
        let cancel = FLUX_MENU_ITEMS.iter().position(|s| *s == "Cancel").unwrap();
        app.flux_menu_state.select(Some(cancel));
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.flash, flash_before); // no suspend/resume side effect
    }

    #[tokio::test]
    async fn flux_menu_suspend_acts_on_marked_rows() {
        let (mut app, _rx) = test_app();
        app.switch_kind("kustomizations");
        let ks = |name: &str| {
            json!({
                "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
                "metadata": {"name": name, "namespace": "default"},
                "spec": {"suspend": false}
            })
        };
        apply(&mut app, ks("infra"));
        apply(&mut app, ks("apps"));
        app.marked.insert("default/infra".into());
        app.marked.insert("default/apps".into());

        app.request_flux_menu();
        app.handle_key(press(KeyCode::Enter)).unwrap(); // "Suspend" (default selection)
        assert!(
            app.flash.contains("suspending 2 kustomizations"),
            "{}",
            app.flash
        );
        assert!(app.marked.is_empty()); // cleared after the bulk action
    }

    #[tokio::test]
    async fn flux_menu_reconcile_now() {
        let (mut app, _rx) = test_app();
        app.switch_kind("kustomizations");
        apply(
            &mut app,
            json!({
                "apiVersion": "kustomize.toolkit.fluxcd.io/v1", "kind": "Kustomization",
                "metadata": {"name": "infra", "namespace": "default"},
                "spec": {"suspend": false}
            }),
        );
        app.request_flux_menu();
        let idx = FLUX_MENU_ITEMS
            .iter()
            .position(|s| *s == "Reconcile now")
            .unwrap();
        app.flux_menu_state.select(Some(idx));
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.flash.contains("reconciling infra"), "{}", app.flash);
    }

    #[tokio::test]
    async fn r_force_syncs_external_secrets() {
        let (mut app, _rx) = test_app();
        app.switch_kind("externalsecrets");
        apply(
            &mut app,
            json!({
                "apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
                "metadata": {"name": "creds", "namespace": "default"}
            }),
        );
        app.table_state.select(Some(0));
        app.handle_key(press(KeyCode::Char('r'))).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert!(app.flash.contains("refreshing creds"), "{}", app.flash);
        assert!(!app.flash_err);
    }

    #[tokio::test]
    async fn refresh_es_rejects_non_es_kinds() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        app.request_refresh_es();
        assert!(app.flash_err);
        assert!(app.flash.contains("external secrets"), "{}", app.flash);
    }

    #[tokio::test]
    async fn pf_palette_command_opens_the_view() {
        let (mut app, _rx) = test_app();
        assert!(app.run_palette_command("pf"));
        assert_eq!(app.mode, Mode::PortForwards);
    }

    #[tokio::test]
    async fn events_palette_command_dispatches() {
        let (mut app, _rx) = test_app();
        assert!(app.run_palette_command("events"));
        assert_eq!(app.mode, Mode::Table);
        assert!(app.flash_err);
        assert!(app.flash.contains("events"), "{}", app.flash);
    }

    #[test]
    fn event_lines_show_core_event_fields() {
        let event = obj(json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": {"name": "web.123", "namespace": "default"},
            "type": "Warning",
            "reason": "FailedScheduling",
            "message": "0/3 nodes are available",
            "count": 4,
            "lastTimestamp": "2026-07-04T12:34:56Z"
        }));
        let lines = format_event_lines([&event], false);
        assert!(lines[0].contains("LAST SEEN"));
        assert!(lines[1].contains("Warning"));
        assert!(lines[1].contains("FailedScheduling"));
        assert!(lines[1].contains("0/3 nodes are available"));
        assert!(lines[1].contains("4"));
    }

    fn spawn_test_child(argv0: &str, arg: &str) -> tokio::process::Child {
        tokio::process::Command::new(argv0)
            .arg(arg)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn `{argv0} {arg}` for test: {e}"))
    }

    #[tokio::test]
    async fn stopping_a_forward_kills_only_that_one() {
        let (mut app, _rx) = test_app();
        app.port_forwards.push(PortForward {
            ns: "default".into(),
            target: "pod/a".into(),
            ports: "8080:80".into(),
            child: spawn_test_child("sleep", "30"),
        });
        app.port_forwards.push(PortForward {
            ns: "default".into(),
            target: "pod/b".into(),
            ports: "8081:81".into(),
            child: spawn_test_child("sleep", "30"),
        });
        app.pf_state.select(Some(0));
        app.mode = Mode::PortForwards;

        app.handle_key(press(KeyCode::Char('x'))).unwrap();
        assert_eq!(app.port_forwards.len(), 1);
        assert_eq!(app.port_forwards[0].target, "pod/b");
        assert_eq!(app.pf_state.selected(), Some(0)); // cursor stays in range

        // Esc closes the view without touching the remaining forward.
        app.handle_key(press(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(app.port_forwards.len(), 1);
    }

    #[tokio::test]
    async fn reap_drops_exited_forwards_and_flashes() {
        let (mut app, _rx) = test_app();
        let mut child = spawn_test_child("true", "");
        child.wait().await.unwrap(); // let it exit before reaping
        app.port_forwards.push(PortForward {
            ns: "default".into(),
            target: "pod/a".into(),
            ports: "8080:80".into(),
            child,
        });
        app.reap_port_forwards();
        assert!(app.port_forwards.is_empty());
        assert!(app.flash.contains("exited"), "{}", app.flash);
    }

    #[test]
    fn crd_served_version_prefers_storage_then_served() {
        let d = json!({"spec": {"versions": [
            {"name": "v1beta1", "served": true, "storage": false},
            {"name": "v1", "served": true, "storage": true}
        ]}});
        assert_eq!(crd_served_version(&d).as_deref(), Some("v1"));

        let d2 = json!({"spec": {"versions": [
            {"name": "v2", "served": false},
            {"name": "v1", "served": true}
        ]}});
        assert_eq!(crd_served_version(&d2).as_deref(), Some("v1"));
    }

    #[test]
    fn mutating_action_patch_payloads_are_stable() {
        assert_eq!(
            restart_patch("2026-07-04T12:00:00Z"),
            json!({
                "spec": { "template": { "metadata": { "annotations": {
                    "kubectl.kubernetes.io/restartedAt": "2026-07-04T12:00:00Z"
                }}}}
            })
        );
        assert_eq!(
            set_image_patch("pods", "app", "nginx:1.27"),
            json!({ "spec": { "containers": [{ "name": "app", "image": "nginx:1.27" }] } })
        );
        assert_eq!(
            set_image_patch("deployments", "app", "nginx:1.27"),
            json!({
                "spec": { "template": { "spec": {
                    "containers": [{ "name": "app", "image": "nginx:1.27" }]
                }}}
            })
        );
        assert_eq!(scale_patch(3), json!({ "spec": { "replicas": 3 } }));
        assert_eq!(suspend_patch(true), json!({ "spec": { "suspend": true } }));
        assert_eq!(
            reconcile_patch("2026-07-04T12:00:00Z"),
            json!({
                "metadata": { "annotations": {
                    "reconcile.fluxcd.io/requestedAt": "2026-07-04T12:00:00Z"
                }}
            })
        );
        assert_eq!(
            external_secret_refresh_patch("1783166400"),
            json!({ "metadata": { "annotations": { "force-sync": "1783166400" } } })
        );
        assert_eq!(
            node_unschedulable_patch(true),
            json!({ "spec": { "unschedulable": true } })
        );
        assert_eq!(
            node_unschedulable_patch(false),
            json!({ "spec": { "unschedulable": false } })
        );
    }

    #[tokio::test]
    async fn crd_drill_builds_kind_from_spec() {
        let (mut app, _rx) = test_app();
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
        app.kind_plural = "customresourcedefinitions".into();
        // Not in the (fake) discovery registry → built straight from the spec.
        app.drill_into_crd(&crd);
        assert_eq!(app.kind_plural, "widgets");
        let k = app.kind.as_ref().unwrap();
        assert_eq!(k.ar.kind, "Widget");
        assert_eq!(k.ar.group, "example.com");
        assert_eq!(k.ar.version, "v1"); // storage version preferred
        assert_eq!(k.ar.api_version, "example.com/v1");
        assert!(k.namespaced);
        assert!(
            app.scope_label
                .as_deref()
                .unwrap()
                .contains("widgets.example.com")
        );
    }

    #[tokio::test]
    async fn log_lines_expand_tabs_and_strip_cr() {
        let (mut app, _rx) = test_app();
        // Caddy-style tab-separated line (level would be color-wrapped too).
        app.handle_msg(Msg::LogLines {
            generation: app.log_gen,
            lines: vec!["2026/07/01 09:21:14.062\tINFO\tProvisioning WAF\r".into()],
        });
        assert_eq!(
            app.logs.view.lines.back().unwrap(),
            "2026/07/01 09:21:14.062 INFO Provisioning WAF"
        );
    }

    #[tokio::test]
    async fn log_buffer_is_capped() {
        let (mut app, _rx) = test_app();
        for i in 0..(MAX_LOG_LINES + 50) {
            app.handle_msg(Msg::LogLines {
                generation: app.log_gen,
                lines: vec![format!("line {i}")],
            });
        }
        assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES);
        // Oldest lines dropped; newest retained.
        assert_eq!(
            app.logs.view.lines.back().unwrap(),
            &format!("line {}", MAX_LOG_LINES + 49)
        );
    }

    #[tokio::test]
    async fn filtered_log_text_respects_active_filter() {
        let (mut app, _rx) = test_app();
        app.handle_msg(Msg::LogLines {
            generation: app.log_gen,
            lines: vec![
                "api request started".into(),
                "worker finished".into(),
                "api request finished".into(),
            ],
        });

        assert_eq!(
            app.filtered_log_text(),
            "api request started\nworker finished\napi request finished"
        );
        app.logs.filter = "api".into();
        assert_eq!(
            app.filtered_log_text(),
            "api request started\napi request finished"
        );
    }

    #[tokio::test]
    async fn stale_log_save_result_is_dropped() {
        let (mut app, _rx) = test_app();
        let stale = app.log_gen;
        app.log_gen += 1;

        app.handle_msg(Msg::LogsSaved {
            generation: stale,
            result: Err("old write failed".into()),
        });
        assert!(!app.flash.contains("old write failed"));

        app.handle_msg(Msg::LogsSaved {
            generation: app.log_gen,
            result: Ok(std::env::temp_dir().join("sofka-test.log")),
        });
        assert!(app.flash.contains("sofka-test.log"));
        assert!(!app.flash_err);
    }

    #[tokio::test]
    async fn stale_clipboard_result_is_dropped() {
        let (mut app, _rx) = test_app();
        let stale = app.generation;
        app.bump_generation();

        app.handle_msg(Msg::ClipboardCopied {
            generation: stale,
            copied: false,
            success: "copied stale".into(),
            failure: "stale failed".into(),
        });
        assert!(!app.flash.contains("stale failed"));

        app.handle_msg(Msg::ClipboardCopied {
            generation: app.generation,
            copied: true,
            success: "copied current".into(),
            failure: "current failed".into(),
        });
        assert_eq!(app.flash, "copied current");
        assert!(!app.flash_err);
    }

    #[test]
    fn osc52_sequence_base64_encodes_clipboard_text() {
        assert_eq!(osc52_sequence("sofka"), "\x1b]52;c;c29ma2E=\x07");
    }

    #[tokio::test]
    async fn sort_by_numeric_column_and_invert() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        let pod = |name: &str, restarts: i64| {
            json!({
                "apiVersion": "v1", "kind": "Pod",
                "metadata": {"name": name, "namespace": "default"},
                "status": {
                    "phase": "Running",
                    "containerStatuses": [
                        {"ready": true, "restartCount": restarts, "state": {"running": {}}}
                    ]
                }
            })
        };
        apply(&mut app, pod("a", 5));
        apply(&mut app, pod("b", 1));
        apply(&mut app, pod("c", 9));

        // RESTARTS is the 4th pod column; sort by it numerically (not "1,5,9"
        // as strings, which happens to agree here, but parsing is what matters).
        assert_eq!(app.display_headers()[3], "RESTARTS");
        app.sort_column = Some(3);
        app.invalidate_rows();
        let names: Vec<String> = app
            .rows()
            .iter()
            .map(|o| o.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["b", "a", "c"]); // 1, 5, 9 ascending

        app.sort_desc = true;
        app.invalidate_rows();
        let names: Vec<String> = app
            .rows()
            .iter()
            .map(|o| o.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["c", "a", "b"]); // descending

        // Switching kinds resets the sort (columns differ).
        app.switch_kind("services");
        assert_eq!(app.sort_column, None);
        assert!(!app.sort_desc);
    }

    #[tokio::test]
    async fn metrics_update_invalidates_metric_sorted_rows() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        for name in ["a", "b"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Pod",
                       "metadata": {"name": name, "namespace": "default"}}),
            );
        }

        let cpu_idx = app
            .display_headers()
            .iter()
            .position(|h| *h == "CPU")
            .unwrap();
        app.sort_column = Some(cpu_idx);
        app.sort_desc = true;
        app.invalidate_rows();
        let names: Vec<String> = app
            .rows()
            .iter()
            .map(|o| o.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["a", "b"]); // cached before metrics arrive

        app.handle_msg(Msg::Metrics {
            generation: app.generation,
            data: HashMap::from([
                ("default/a".to_string(), (10, 0)),
                ("default/b".to_string(), (100, 0)),
            ]),
        });
        let names: Vec<String> = app
            .rows()
            .iter()
            .map(|o| o.metadata.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["b", "a"]);
    }

    #[tokio::test]
    async fn logs_keep_view_and_restore_selection() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        for n in ["a", "b", "c"] {
            apply(
                &mut app,
                json!({"apiVersion": "v1", "kind": "Pod",
                       "metadata": {"name": n, "namespace": "default"}}),
            );
        }
        app.table_state.select(Some(1)); // "b"
        assert_eq!(app.selected().unwrap().metadata.name.as_deref(), Some("b"));
        let gen_before = app.generation;

        app.handle_key(press(KeyCode::Char('l'))).unwrap(); // open logs
        assert_eq!(app.mode, Mode::Logs);
        assert_eq!(app.rows().len(), 3, "underlying view stays populated");

        app.handle_key(press(KeyCode::Esc)).unwrap(); // back to table
        assert_eq!(app.mode, Mode::Table);
        assert_eq!(
            app.generation, gen_before,
            "view watch was not torn down/restarted"
        );
        assert_eq!(app.rows().len(), 3, "rows were not blanked + reloaded");
        assert_eq!(
            app.selected().unwrap().metadata.name.as_deref(),
            Some("b"),
            "cursor returned to the same pod"
        );
    }

    #[tokio::test]
    async fn namespace_switcher_pins_all_and_fuzzy_filters() {
        let (mut app, _rx) = test_app();
        app.ns_list = vec![
            "<all>".into(),
            "default".into(),
            "kube-system".into(),
            "prod".into(),
        ];
        // No filter: <all> first, then the rest.
        assert_eq!(app.filtered_namespaces()[0], "<all>");
        assert_eq!(app.filtered_namespaces().len(), 4);

        // Fuzzy filter (subsequence) keeps <all> pinned on top.
        app.ns_filter = "sys".into();
        let f = app.filtered_namespaces();
        assert_eq!(f[0], "<all>");
        assert!(f.contains(&"kube-system".to_string()));
        assert!(!f.contains(&"default".to_string()));

        // Typing a name that matches nothing real → Enter takes it verbatim.
        app.ns_filter = "team-x".into();
        app.mode = Mode::Namespaces;
        app.handle_key(press(KeyCode::Enter)).unwrap();
        assert_eq!(app.namespace, "team-x");
    }

    #[tokio::test]
    async fn shellouts_pin_to_active_context() {
        let (mut app, _rx) = test_app();
        app.switch_kind("pods");
        apply(
            &mut app,
            json!({"apiVersion": "v1", "kind": "Pod",
                   "metadata": {"name": "p", "namespace": "default"}}),
        );
        app.table_state.select(Some(0));
        app.request_edit();
        let Some(Suspend::Shell(argv)) = app.pending.take() else {
            panic!("expected a pending shell command");
        };
        // Pinned to the context sofka connected with, not kubectl's default.
        assert_eq!(&argv[..3], ["kubectl", "--context", "test"]);
        assert!(argv.contains(&"edit".to_string()));
        assert_eq!(argv.last().unwrap(), "default"); // -n <ns>
    }

    #[tokio::test]
    async fn paused_logs_do_not_trim_below_paused_cap() {
        let (mut app, _rx) = test_app();
        app.logs.follow = false; // autoscroll OFF
        let lg = app.log_gen;
        let line = |i: usize| Msg::LogLines {
            generation: lg,
            lines: vec![format!("line {i}")],
        };
        // Well past the *following* cap, but under the paused cap: nothing is
        // dropped, so a frozen view never appears to resume scrolling.
        for i in 0..(MAX_LOG_LINES + 500) {
            app.handle_msg(line(i));
        }
        assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES + 500);

        // Resuming follow trims the backlog back to the tight cap.
        app.mode = Mode::Logs;
        app.handle_key(press(KeyCode::Char('s'))).unwrap(); // follow on
        assert!(app.logs.follow);
        assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES);
    }

    #[tokio::test]
    async fn paused_trim_shifts_scroll_in_display_rows() {
        let (mut app, _rx) = test_app();
        app.logs.follow = false;
        app.logs.last_wrap_width = 10; // as if the last draw wrapped at 10 cols
        app.logs.view.scroll = 500;
        let lg = app.log_gen;
        // The first line is the one trimmed later: 25 chars → 3 rows at width 10.
        app.handle_msg(Msg::LogLines {
            generation: lg,
            lines: vec!["a".repeat(25)],
        });
        for i in 1..MAX_LOG_LINES_PAUSED {
            app.handle_msg(Msg::LogLines {
                generation: lg,
                lines: vec![format!("l{i}")],
            });
        }
        assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES_PAUSED);
        assert_eq!(app.logs.view.scroll, 500); // nothing trimmed yet
        // One more line overflows the paused cap: the wrapped first line drains
        // and the frozen anchor shifts by its 3 display rows, not by 1 line.
        app.handle_msg(Msg::LogLines {
            generation: lg,
            lines: vec!["x".into()],
        });
        assert_eq!(app.logs.view.lines.len(), MAX_LOG_LINES_PAUSED);
        assert_eq!(app.logs.view.scroll, 497);
    }

    #[tokio::test]
    async fn rbac_for_other_namespace_is_dropped() {
        let (mut app, _rx) = test_app();
        // App starts in the "default" namespace.
        let mut other = HashSet::new();
        other.insert("secrets".to_string());
        app.handle_msg(Msg::Rbac {
            generation: app.generation,
            ns: "kube-system".into(),
            allowed: other,
        });
        assert!(app.rbac_allowed.is_none(), "stale-namespace result dropped");

        let mut here = HashSet::new();
        here.insert("pods".to_string());
        app.handle_msg(Msg::Rbac {
            generation: app.generation,
            ns: "default".into(),
            allowed: here,
        });
        assert!(app.rbac_allowed.is_some());
        assert!(app.rbac_visible("pods"));
        assert!(!app.rbac_visible("secrets"));
    }

    #[tokio::test]
    async fn stale_async_picker_results_are_dropped() {
        let (mut app, _rx) = test_app();
        let stale = app.generation;
        app.bump_generation();

        app.ns_list = vec!["<all>".into()];
        app.handle_msg(Msg::Namespaces {
            generation: stale,
            list: vec!["<all>".into(), "stale".into()],
        });
        assert_eq!(app.ns_list, vec!["<all>".to_string()]);

        app.ctx_list = vec!["test".into()];
        app.handle_msg(Msg::Contexts {
            generation: stale,
            list: vec!["stale-context".into()],
        });
        assert_eq!(app.ctx_list, vec!["test".to_string()]);

        let flash = app.flash.clone();
        app.handle_msg(Msg::ContextSwitched {
            generation: stale,
            name: "old-context".into(),
            result: Err("old failure".into()),
        });
        assert_eq!(app.flash, flash);
    }

    #[tokio::test]
    async fn context_list_result_selects_current_context() {
        let (mut app, _rx) = test_app();
        app.handle_msg(Msg::Contexts {
            generation: app.generation,
            list: vec!["prod".into(), "test".into()],
        });
        assert_eq!(app.ctx_list, vec!["prod".to_string(), "test".to_string()]);
        assert_eq!(app.ctx_state.selected(), Some(1));
    }

    #[tokio::test]
    async fn rbac_for_old_generation_is_dropped() {
        let (mut app, _rx) = test_app();
        let stale = app.generation;
        app.bump_generation();

        let mut allowed = HashSet::new();
        allowed.insert("secrets".to_string());
        app.handle_msg(Msg::Rbac {
            generation: stale,
            ns: "default".into(),
            allowed,
        });
        assert!(app.rbac_allowed.is_none());
    }

    #[test]
    fn workload_selector_from_match_labels() {
        let d = obj(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "shop"},
            "spec": {"selector": {"matchLabels": {"app": "web", "tier": "fe"}}}
        }));
        assert_eq!(
            label_selector(&d, "matchLabels").as_deref(),
            Some("app=web,tier=fe")
        );
    }

    #[test]
    fn service_selector_from_plain_map() {
        let s = obj(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "svc"},
            "spec": {"selector": {"app": "api"}}
        }));
        assert_eq!(label_selector(&s, "selector").as_deref(), Some("app=api"));
    }

    #[test]
    fn no_selector_returns_none() {
        let s = obj(json!({
            "apiVersion": "v1", "kind": "Service",
            "metadata": {"name": "headless"}, "spec": {}
        }));
        assert_eq!(label_selector(&s, "selector"), None);
    }

    #[test]
    fn containers_include_init_and_main() {
        let p = obj(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "p"},
            "spec": {
                "containers": [{"name": "app"}, {"name": "sidecar"}],
                "initContainers": [{"name": "init"}]
            }
        }));
        let names = container_names(&p);
        assert!(names.contains(&"app".to_string()));
        assert!(names.contains(&"sidecar".to_string()));
        assert!(names.contains(&"init".to_string()));
    }

    #[test]
    fn drainable_pod_skips_daemonset_mirror_and_completed_pods() {
        let pod = |v| serde_json::from_value::<Pod>(v).unwrap();
        assert!(drainable_pod(&pod(json!({
            "metadata": {"name": "web", "namespace": "default"},
            "status": {"phase": "Running"}
        }))));
        assert!(!drainable_pod(&pod(json!({
            "metadata": {
                "name": "ds",
                "ownerReferences": [{"kind": "DaemonSet", "name": "agent", "uid": "ds"}]
            },
            "status": {"phase": "Running"}
        }))));
        assert!(!drainable_pod(&pod(json!({
            "metadata": {
                "name": "static",
                "annotations": {"kubernetes.io/config.mirror": "mirror"}
            },
            "status": {"phase": "Running"}
        }))));
        assert!(!drainable_pod(&pod(json!({
            "metadata": {"name": "done"},
            "status": {"phase": "Succeeded"}
        }))));
    }

    #[test]
    fn xray_pool_plurals_include_cronjob_chain() {
        assert_eq!(xray_pool_plurals("cronjob"), &["jobs", "pods"]);
        assert_eq!(xray_pool_plurals("job"), &["pods"]);
        assert_eq!(xray_pool_plurals("pod"), &[] as &[&str]);
        assert_eq!(xray_pool_plurals("deployment"), &["replicasets", "pods"]);
    }

    #[test]
    fn xray_emits_cronjob_job_pod_container_chain() {
        let cron = obj(json!({
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "metadata": {"name": "backup", "namespace": "default", "uid": "cron-uid"},
            "status": {"active": [{"name": "backup-1"}]}
        }));
        let job = obj(json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {
                "name": "backup-1",
                "namespace": "default",
                "uid": "job-uid",
                "ownerReferences": [{
                    "apiVersion": "batch/v1",
                    "kind": "CronJob",
                    "name": "backup",
                    "uid": "cron-uid"
                }]
            },
            "spec": {"completions": 1},
            "status": {"succeeded": 1}
        }));
        let pod = obj(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "backup-1-pod",
                "namespace": "default",
                "uid": "pod-uid",
                "ownerReferences": [{
                    "apiVersion": "batch/v1",
                    "kind": "Job",
                    "name": "backup-1",
                    "uid": "job-uid"
                }]
            },
            "spec": {"containers": [{"name": "worker"}]},
            "status": {"phase": "Running"}
        }));
        let mut children = std::collections::HashMap::new();
        children.insert("cron-uid".to_string(), vec![("job".to_string(), job)]);
        children.insert("job-uid".to_string(), vec![("pod".to_string(), pod)]);

        let mut items = Vec::new();
        emit_xray("cronjob", &cron, 0, &children, &mut items);

        assert_eq!(items.len(), 4);
        assert_eq!(items[0].kind, "cronjob");
        assert_eq!(items[0].name, "backup");
        assert_eq!(items[0].depth, 0);
        assert_eq!(items[0].status, "active 1");
        assert_eq!(items[1].kind, "job");
        assert_eq!(items[1].name, "backup-1");
        assert_eq!(items[1].depth, 1);
        assert_eq!(items[1].status, "1/1");
        assert_eq!(items[2].kind, "pod");
        assert_eq!(items[2].name, "backup-1-pod");
        assert_eq!(items[2].depth, 2);
        assert_eq!(items[2].status, "Running");
        assert_eq!(items[3].kind, "container");
        assert_eq!(items[3].name, "backup-1-pod");
        assert_eq!(items[3].depth, 3);
        assert_eq!(items[3].container.as_deref(), Some("worker"));
    }

    #[test]
    fn trim_plural_suffix() {
        assert_eq!(trim_s("deployments"), "deployment");
        assert_eq!(trim_s("pods"), "pod");
    }
}
