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
    /// Roll a Helm release back to an earlier revision (`helm rollback`) —
    /// always a single revision, never bulk (mirrors k9s: rollback acts on
    /// the one selected history row).
    HelmRollback {
        ns: String,
        name: String,
        revision: String,
    },
    /// Uninstall one or more Helm releases (`helm uninstall`), `(name, ns)`
    /// per release — bulk when marked, like [`ConfirmAction::Delete`].
    HelmUninstall { targets: Vec<(String, String)> },
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

#[derive(Default)]
pub struct Scrollable {
    pub title: String,
    pub lines: VecDeque<String>,
    /// Scroll offset in display rows. `usize` on purpose: a paused log buffer
    /// (100k lines, wrapped) far exceeds `u16`; views that hand this to a
    /// ratatui `Paragraph` clamp at the edge instead.
    pub scroll: usize,
    /// Horizontal scroll offset in columns, for views (`describe`, events) whose
    /// lines run past the right edge. Ignored while `wrap` is on.
    pub hscroll: usize,
    /// Word-wrap toggle. When on, long lines fold instead of being clipped, and
    /// horizontal scrolling is disabled.
    pub wrap: bool,
}

/// One command-palette suggestion — a built-in command (`:ctx`, `:pulse`), a
/// resource kind from the catalog, or an argument completion (a namespace after
/// `:<kind>`, a context after `:ctx`). Fuzzy-matched together.
#[derive(Clone)]
pub struct Suggestion {
    pub label: String,
    pub kind: SuggestKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SuggestKind {
    Command,
    Resource,
    /// Namespace argument for `:<kind> <ns>` — Enter switches kind + namespace.
    Namespace,
    /// Context argument for `:ctx <name>` — Enter switches context.
    Context,
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
    Helm,
}

const PALETTE_COMMANDS: &[PaletteCommand] = &[
    PaletteCommand {
        action: PaletteAction::Ctx,
        names: &["ctx", "context", "contexts"],
    },
    PaletteCommand {
        action: PaletteAction::Helm,
        names: &["helm", "hm"],
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
        Self::default()
    }
    pub fn scroll_by(&mut self, delta: i32) {
        let max = self.lines.len().saturating_sub(1) as i64;
        self.scroll = (self.scroll as i64 + delta as i64).clamp(0, max) as usize;
    }
    /// Scroll horizontally by `delta` columns, clamped to the widest line. A
    /// no-op while wrapping, since wrapped lines have no off-screen right edge.
    pub fn scroll_h(&mut self, delta: i32) {
        if self.wrap {
            return;
        }
        let widest = self
            .lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        let max = widest.saturating_sub(1) as i64;
        self.hscroll = (self.hscroll as i64 + delta as i64).clamp(0, max) as usize;
    }
    /// Toggle word wrap. Turning it on resets the horizontal offset so the view
    /// snaps back to the left margin. Returns the new state.
    pub fn toggle_wrap(&mut self) -> bool {
        self.wrap = !self.wrap;
        if self.wrap {
            self.hscroll = 0;
        }
        self.wrap
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

/// Maximum root views kept in the `[`/`]` history.
const HISTORY_MAX: usize = 50;

/// One root view for the `[`/`]` history: which kind was listed in which
/// namespace. Drill-down state (selectors, filter, scope) is deliberately not
/// kept — history replays root views; the breadcrumb stack handles drills.
#[derive(Clone, PartialEq, Eq)]
struct ViewEntry {
    kind_plural: String,
    namespace: String,
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
    /// Browser-style history of root views for `[`/`]`: every root switch
    /// (kind and/or namespace) is recorded; navigating with `[`/`]` moves the
    /// cursor without re-recording, and a fresh switch truncates the forward
    /// tail — exactly like browser history.
    history: Vec<ViewEntry>,
    history_pos: usize,

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
    /// All kubeconfig context names, cached once at startup for `:ctx <name>`
    /// palette completion (the switcher popup uses `ctx_list`).
    pub all_contexts: Vec<String>,
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
    /// Config loader kept for the session so `:ctx` switches can re-resolve
    /// per-cluster/per-context override files against the new context.
    pub config: crate::config::ConfigLoader,
    /// Skin for contexts without an override: config `skin.name` (or the
    /// auto-detected default), replaced by a manual `:skin` choice.
    pub session_skin: Option<String>,
    /// Effective read-only mode: mutating actions are refused with a flash.
    pub readonly: bool,
    /// Session-wide pin from `--readonly`/`--write`; wins over any config
    /// `readonly` value on every context switch. `None` = config decides.
    pub readonly_override: Option<bool>,

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
            history: Vec::new(),
            history_pos: 0,
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
            all_contexts: Vec::new(),
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
            config: crate::config::ConfigLoader::default(),
            session_skin: None,
            readonly: false,
            readonly_override: None,
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
}

mod actions;
mod dashboards;
mod details;
mod helpers;
mod input;
mod lifecycle;
mod logs;
mod navigation;
mod overlays;
mod pickers;
mod rows;

use helpers::*;

#[cfg(test)]
mod tests;
