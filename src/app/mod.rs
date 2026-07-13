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
use kube::api::{
    Api, DeleteParams, EvictParams, ListParams, LogParams, Patch, PatchParams, PropagationPolicy,
};
use kube::core::{DynamicObject, TypeMeta};
use kube::discovery::ApiResource;
use kube::runtime::watcher;
use ratatui::widgets::{ListState, TableState};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use crate::k8s::{Cluster, Kind};
use crate::store::{Msg, Pulse, Store, XrayItem, row_key};

pub(crate) use guardrails::ConfirmLevel;

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
    /// Typing a search query for a single-document view (YAML/describe, diff,
    /// events, help) — the doc-view counterpart of [`Mode::LogFilter`].
    DocFilter,
    Help,
    Namespaces,
    Contexts,
    Containers,
    SetImage,
    Confirm,
    Prompt,
    Pulse,
    Xray,
    /// Deterministic "why is this unhealthy?" explanation for the selection.
    Explain,
    /// Session-local state-change history for the selection.
    Timeline,
    /// Flux GitOps ownership + reconciliation chain for the selection.
    Gitops,
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

/// How dependents are handled on delete (kubectl `--cascade`, k9s propagation
/// picker). Cycled with `c` in the delete confirm dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cascade {
    Background,
    Foreground,
    Orphan,
}

impl Cascade {
    fn next(self) -> Self {
        match self {
            Cascade::Background => Cascade::Foreground,
            Cascade::Foreground => Cascade::Orphan,
            Cascade::Orphan => Cascade::Background,
        }
    }

    fn policy(self) -> PropagationPolicy {
        match self {
            Cascade::Background => PropagationPolicy::Background,
            Cascade::Foreground => PropagationPolicy::Foreground,
            Cascade::Orphan => PropagationPolicy::Orphan,
        }
    }
}

enum ConfirmAction {
    /// One or more `(name, ns)` targets to delete (bulk when marked).
    Delete {
        targets: Vec<(String, String)>,
        force: bool,
        cascade: Cascade,
        /// A "managed — will be recreated" warning when any target is owned by
        /// Flux or a controller (shown in the dialog).
        managed: Option<String>,
    },
    /// Edit a Flux-managed object (`kubectl edit`) after warning that the edit
    /// will be reverted on the next reconcile.
    Edit { argv: Vec<String> },
    /// Shell into a pod, once a guardrail confirmation is satisfied.
    Exec { ns: String, name: String },
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
    /// Launch a privileged node debug pod (`kubectl debug node/<node>`) after
    /// previewing the host access it grants.
    NodeDebug {
        node: String,
        image: String,
        namespace: String,
        profile: Option<String>,
    },
    /// Delete the node debugger pods sofka launched this session (`:debug-clean`).
    CleanupDebuggers,
    /// Run a confirmed plugin (`confirm`/`dangerous`) once accepted — one job
    /// (label, argv) per target, so a bulk run confirms once.
    Plugin {
        jobs: Vec<(String, Vec<String>)>,
        name: String,
        mode: PluginMode,
        timeout: u64,
    },
}

/// The workspace being cycled: its views and where in them we are.
pub struct ActiveWorkspace {
    pub name: String,
    pub views: Vec<crate::config::WorkspaceView>,
    pub index: usize,
}

/// How a plugin's output is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginMode {
    /// Interactive, inheriting the terminal (default) — suspends the TUI.
    Terminal,
    /// Captured off-thread into a scrollable document view.
    Popup,
    /// Detached; a notification flashes on completion.
    Background,
}

/// What the logs view is currently streaming, so it can be re-streamed when
/// toggling timestamps (k9s `t`).
#[derive(Clone, Debug)]
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
    /// The configured external log provider (`[providers.logs]`), queried for
    /// the selection instead of the kubelet — survives pod restarts and covers
    /// deleted pods and whole namespaces.
    Provider {
        request: crate::providers::LogRequest,
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
    /// Debug image for an ephemeral debug container (`:debug`), prefilled with
    /// the configured default. `target` pins `--target=<container>` when the
    /// workflow was launched from the container picker.
    Debug {
        ns: String,
        pod: String,
        target: Option<String>,
    },
    /// New lookback period for the provider logs view (`T`) — the only
    /// prompt opened from (and returning to) [`Mode::Logs`].
    ProviderLookback,
    /// A guardrail typed-confirmation: the action runs only if the input
    /// matches `expected` (a resource or context name).
    GuardConfirm {
        expected: String,
        action: Box<ConfirmAction>,
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
    /// Case-insensitive substring search (`/`), vim-style: the full document
    /// stays rendered with every match highlighted, and `n`/`N` step between
    /// them. Reset whenever a fresh document replaces the view. (The help view
    /// keeps its own filtering search in `help_filter`.)
    pub filter: String,
    /// Which match `n`/`N` last landed on (0-based into [`Self::match_lines`]),
    /// for the `[cur/total]` counter and relative stepping.
    pub match_idx: usize,
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
    /// A saved bookmark — Enter applies its full navigation command.
    Bookmark,
    /// A saved workspace — Enter opens it (lands on its first view).
    Workspace,
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
    Explain,
    Timeline,
    Gitops,
    CanI,
    Journal,
    Debug,
    DebugClean,
    Bundle,
    BundleSave,
    Diff,
    Events,
    PortForwards,
    ProviderLogs,
    Skin,
    Helm,
    Reload,
    ConfigInfo,
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
        action: PaletteAction::Explain,
        names: &["explain", "why", "diagnose"],
    },
    PaletteCommand {
        action: PaletteAction::Timeline,
        names: &["timeline", "tl", "history"],
    },
    PaletteCommand {
        action: PaletteAction::Gitops,
        names: &["gitops", "flux", "reconcile", "recon"],
    },
    PaletteCommand {
        action: PaletteAction::CanI,
        names: &["can-i", "cani", "can"],
    },
    PaletteCommand {
        action: PaletteAction::Journal,
        names: &["journal", "audit", "actions"],
    },
    PaletteCommand {
        action: PaletteAction::Debug,
        names: &["debug", "ephemeral", "dbg"],
    },
    PaletteCommand {
        action: PaletteAction::DebugClean,
        names: &["debug-clean", "debug-cleanup", "dbgclean"],
    },
    PaletteCommand {
        action: PaletteAction::Bundle,
        names: &["bundle", "diag", "incident"],
    },
    PaletteCommand {
        action: PaletteAction::BundleSave,
        names: &["bundle-save", "bundle-write"],
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
        action: PaletteAction::ProviderLogs,
        names: &["vlogs", "plogs", "providerlogs"],
    },
    PaletteCommand {
        action: PaletteAction::Skin,
        names: &["skin", "skins"],
    },
    PaletteCommand {
        action: PaletteAction::Reload,
        names: &["reload"],
    },
    PaletteCommand {
        action: PaletteAction::ConfigInfo,
        names: &["config", "cfg"],
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

    /// Line indices (0-based) containing the active search query, matched
    /// case-insensitively as a substring. Empty when no search is active.
    pub fn match_lines(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return Vec::new();
        }
        let needle = self.filter.to_lowercase();
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Finalize a search: scroll to the first match at or after the current
    /// position (wrapping to the first if none follow), so `⏎` lands on a hit
    /// without disturbing the rest of the document. No-op with no matches.
    pub fn focus_first_match(&mut self) {
        let matches = self.match_lines();
        if matches.is_empty() {
            return;
        }
        let pos = matches.iter().position(|&i| i >= self.scroll).unwrap_or(0);
        self.match_idx = pos;
        self.scroll = matches[pos];
    }

    /// Step to the next (`forward`) or previous match, wrapping around, and
    /// scroll it into view. No-op with no matches.
    pub fn step_match(&mut self, forward: bool) {
        let matches = self.match_lines();
        let n = matches.len();
        if n == 0 {
            return;
        }
        let cur = self.match_idx.min(n - 1);
        self.match_idx = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        self.scroll = matches[self.match_idx];
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

impl From<crate::views::SortValue> for SortKey {
    fn from(v: crate::views::SortValue) -> Self {
        match v {
            crate::views::SortValue::Num(n) => SortKey::Num(n),
            crate::views::SortValue::Text(t) => SortKey::Text(t),
        }
    }
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

/// The active filter string alongside its parsed form, so the grammar is
/// reparsed only when the string actually changes — never per frame or row.
struct FilterCache {
    raw: String,
    parsed: crate::filter::ParsedFilter,
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
    /// Parsed form of `filter`, refreshed lazily when the string changes so
    /// neither row matching nor rendering reparses it per frame.
    filter_cache: RefCell<FilterCache>,
    /// Server-side selectors (`-l`/`-f` filter terms) the running watch was
    /// started with. Compared against the parsed filter to know when a
    /// restart is needed and to mark the filter as server-side in the UI.
    applied_filter_labels: Option<String>,
    applied_filter_fields: Option<String>,
    pub command: String,
    pub cmd_suggestions: Vec<Suggestion>,
    pub cmd_sel: usize,
    pub flash: String,
    pub flash_err: bool,

    pub detail: Scrollable,
    /// Search query for the help view (`?`), which has no backing
    /// [`Scrollable`] — its lines are built at render time.
    pub help_filter: String,
    /// Which doc view (`Detail`/`Diff`/`Events`/`Help`) the `/` search prompt
    /// was opened from, so the renderer keeps drawing it underneath and
    /// enter/esc return to it.
    pub doc_filter_return: Mode,
    pub logs: LogsView,

    pub ns_list: Vec<String>,
    pub ns_state: ListState,
    /// Namespaces pinned to the top of the switcher (config `favorite_namespaces`),
    /// re-applied on context switch and `:reload`.
    pub namespace_favorites: Vec<String>,
    /// Session-local recently-selected namespaces, newest first, per context.
    pub recent_namespaces: HashMap<String, VecDeque<String>>,
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
    /// Saved navigation commands (`[[bookmarks]]`), re-applied on context
    /// switch and `:reload`.
    pub bookmarks: Vec<crate::config::Bookmark>,
    /// A bookmark waiting for an in-flight context switch to land before its
    /// resource/namespace/filter/sort are applied.
    pub pending_bookmark: Option<crate::config::Bookmark>,
    /// Saved workspaces (`[[workspaces]]`), re-applied on context switch and
    /// `:reload`.
    pub workspaces: Vec<crate::config::Workspace>,
    /// Declarative guardrails (`[[guardrails]]`), re-applied on context switch
    /// and `:reload`.
    pub guardrails: Vec<crate::config::Guardrail>,
    /// Ephemeral-debug-container defaults (`[debug]`) for `:debug`, re-applied
    /// on context switch and `:reload`.
    pub debug: crate::config::DebugConfig,
    /// Node debugger pods launched this session, as `(namespace, node)`, so
    /// `:debug-clean` can find and delete them. Cleared on context switch.
    pub launched_node_debuggers: Vec<(String, String)>,
    /// Diagnostic-bundle (`:bundle`) options, re-applied on context switch and
    /// `:reload`.
    pub bundle_cfg: crate::config::BundleConfig,
    /// The last bundle assembled by `:bundle`, previewed in the detail view and
    /// written to disk by `:bundle-save`: `(filename, text)`.
    pub pending_bundle: Option<(String, String)>,
    /// Session-local log of mutating actions taken (`:journal`).
    pub journal: crate::journal::Journal,
    /// A workspace waiting for an in-flight context switch before it opens.
    pub pending_workspace: Option<crate::config::Workspace>,
    /// The workspace currently being cycled with `Tab`/`Shift-Tab`, if any.
    pub active_workspace: Option<ActiveWorkspace>,
    /// Resource plurals the user may list (None = unknown/all). "*" = all.
    rbac_allowed: Option<HashSet<String>>,
    last_rbac_ns: Option<String>,

    pub container_list: Vec<String>,
    pub container_state: ListState,
    container_pod: Option<(String, String)>, // (ns, name)
    /// Declared requests/limits for the pod shown by the container picker,
    /// keyed by container name. Drives the request/limit percentage columns.
    pub container_resources: HashMap<String, crate::columns::ContainerResources>,
    /// QoS class of the pod shown by the container picker (empty if unknown).
    pub container_qos: String,

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
    /// Name of the palette currently installed (session skin or a per-context
    /// override), shown by `:config`. `None` until any skin is applied.
    pub active_skin: Option<String>,
    /// Validation problems from the most recent config (re)load — invalid
    /// base config, skipped override layers, bad skin values. Shown by
    /// `:config`; replaced wholesale on every `:reload`.
    pub config_warnings: Vec<String>,
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
    /// Latest pod-container metrics: "ns/pod/container" -> (cpu_m, mem_bytes).
    pub container_metrics: HashMap<String, (i64, i64)>,

    pub pulse: Pulse,
    pub xray_items: Vec<XrayItem>,
    pub xray_state: ListState,
    /// Findings from the explain-unhealthy view, and the row cursor over them
    /// (used to jump to the evidence behind a line).
    pub explain_items: Vec<crate::explain::Finding>,
    pub explain_state: ListState,
    pub explain_title: String,
    /// The object the explain view is investigating, kept so `r` can re-gather.
    pub explain_source: Option<DynamicObject>,
    /// GitOps view: the reconciliation-chain findings, cursor, title, and the
    /// object being investigated (kept so `r` can re-gather).
    pub gitops_items: Vec<crate::explain::Finding>,
    pub gitops_state: ListState,
    pub gitops_title: String,
    pub gitops_source: Option<DynamicObject>,
    /// Session-local per-object state-change history, fed by the table watch.
    pub timeline: crate::timeline::Timeline,
    /// The `(plural, row_key)` the timeline view is showing, and its cursor.
    pub timeline_target: Option<(String, String)>,
    pub timeline_state: ListState,

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

    /// Compiled log provider from `[providers.logs]`, re-resolved on context
    /// switch and `:reload` so each cluster can point at its own backend.
    pub log_provider: Option<crate::providers::LogProvider>,
    /// Compiled custom views from config, re-resolved on context switch.
    pub user_views: HashMap<String, crate::views::View>,
    /// Compiled warning/critical coloring thresholds from config, re-resolved
    /// on context switch and `:reload`.
    pub thresholds: crate::thresholds::Compiled,
    /// CRD printer-column fallbacks fetched per plural for this cluster
    /// (`None` = fetched, nothing usable). Cleared on context switch.
    crd_views: HashMap<String, Option<crate::views::View>>,
    /// Wide mode (`w`): show wide-only columns.
    pub wide: bool,
    /// Active column layout for the current view; rebuilt by
    /// [`App::refresh_view_spec`] whenever kind/views/wide change.
    spec: crate::columns::ViewSpec,
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
            filter_cache: RefCell::new(FilterCache {
                raw: String::new(),
                parsed: crate::filter::parse(""),
            }),
            applied_filter_labels: None,
            applied_filter_fields: None,
            command: String::new(),
            cmd_suggestions: Vec::new(),
            cmd_sel: 0,
            flash: "Welcome to sofka — ':' resource · enter drill · d describe · l logs · ? help"
                .into(),
            flash_err: false,
            detail: Scrollable::empty(),
            help_filter: String::new(),
            doc_filter_return: Mode::Detail,
            logs: LogsView::default(),
            ns_list: Vec::new(),
            ns_state: ListState::default(),
            namespace_favorites: Vec::new(),
            recent_namespaces: HashMap::new(),
            ns_filter: String::new(),
            ctx_list: Vec::new(),
            ctx_state: ListState::default(),
            ctx_filter: String::new(),
            all_contexts: Vec::new(),
            user_aliases: HashMap::new(),
            plugins: Vec::new(),
            bookmarks: Vec::new(),
            pending_bookmark: None,
            workspaces: Vec::new(),
            pending_workspace: None,
            active_workspace: None,
            guardrails: Vec::new(),
            debug: crate::config::DebugConfig::default(),
            launched_node_debuggers: Vec::new(),
            bundle_cfg: crate::config::BundleConfig::default(),
            pending_bundle: None,
            journal: crate::journal::Journal::default(),
            rbac_allowed: None,
            last_rbac_ns: None,
            container_list: Vec::new(),
            container_state: ListState::default(),
            container_pod: None,
            container_resources: HashMap::new(),
            container_qos: String::new(),
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
            active_skin: None,
            config_warnings: Vec::new(),
            readonly: false,
            readonly_override: None,
            image_values: Vec::new(),
            image_target: None,
            metrics: HashMap::new(),
            container_metrics: HashMap::new(),
            pulse: Pulse::default(),
            xray_items: Vec::new(),
            xray_state: ListState::default(),
            explain_items: Vec::new(),
            explain_state: ListState::default(),
            explain_title: String::new(),
            explain_source: None,
            gitops_items: Vec::new(),
            gitops_state: ListState::default(),
            gitops_title: String::new(),
            gitops_source: None,
            timeline: crate::timeline::Timeline::default(),
            timeline_target: None,
            timeline_state: ListState::default(),
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
            log_provider: None,
            user_views: HashMap::new(),
            thresholds: crate::thresholds::Compiled::default(),
            crd_views: HashMap::new(),
            wide: false,
            spec: crate::columns::build_spec("", None, None, false),
        }
    }

    pub fn all_namespaces(&self) -> bool {
        self.namespace.is_empty()
    }

    /// Whether the active prompt was opened from the logs view, so the
    /// renderer keeps the logs (not the table) underneath it.
    pub fn prompt_over_logs(&self) -> bool {
        matches!(self.prompt_kind, Some(PromptKind::ProviderLookback))
    }

    /// Whether the logs view is showing the external log provider (enables
    /// provider-only keys like `T`).
    pub fn provider_logs_active(&self) -> bool {
        matches!(self.logs.source, Some(LogSource::Provider { .. }))
    }
}

mod actions;
mod authz;
mod bookmarks;
mod bundle;
mod dashboards;
mod details;
mod explain;
mod gitops;
mod guardrails;
mod helpers;
mod input;
mod journal;
mod lifecycle;
mod logs;
mod navigation;
mod overlays;
mod pickers;
mod rows;
mod timeline;
mod workspaces;

use helpers::*;

#[cfg(test)]
mod tests;
