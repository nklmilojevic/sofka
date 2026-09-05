//! Application state and input handling.
//!
//! Navigation is a breadcrumb stack: `:cmd` pushes a fresh root view, `enter`
//! drills into a child (workload -> pods, pod -> containers, namespace ->
//! re-scope the previous view), and `esc` pops back.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
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
    Api, DeleteParams, EvictParams, ListParams, LogParams, Patch, PatchParams, PostParams,
    PropagationPolicy,
};
use kube::core::{DynamicObject, TypeMeta};
use kube::discovery::ApiResource;
use kube::runtime::{utils::Backoff, watcher};
use ratatui::widgets::{ListState, TableState};
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use crate::k8s::{Cluster, Kind};
use crate::store::{Msg, Pulse, RowKey, StatusClaim, Store, StoreMutation, XrayItem, row_key};

pub(crate) use guardrails::ConfirmLevel;

impl App {
    /// Mark the row ordering stale without touching the store — the shape of a
    /// filter keystroke or a sort toggle. `invalidate_rows` is `pub(super)`;
    /// this exposes it to `benchsupport` under the bench feature only, rather
    /// than widening it for the shipped binary.
    #[cfg(feature = "bench")]
    pub fn bench_invalidate_rows(&self) {
        self.invalidate_rows();
    }

    /// Put a bench fixture into the state real navigation leaves behind:
    /// the resolved `Kind` installed and the view spec built for it. Seeding
    /// `kind_plural` alone leaves `spec` built from an empty plural, so every
    /// cached cell, comparison filter and header measured through it belongs
    /// to `DEFAULT_COLUMNS` rather than the kind's real layout.
    #[cfg(feature = "bench")]
    pub fn bench_install_kind(&mut self, plural: &str) {
        self.kind = self.cluster.resolve(plural);
        self.kind_plural = plural.to_string();
        self.refresh_view_spec();
    }
}

/// Larger cap used while autoscroll is paused: we stop trimming so the line
/// indices don't shift under the frozen view (which would make it appear to
/// resume scrolling). Only a runaway firehose during a very long pause hits
/// this; resuming follow trims back to the configured `[logs] buffer`.
const MAX_LOG_LINES_PAUSED: usize = 100_000;

/// Log streams batch lines before sending them through the UI channel. This
/// avoids one wake-up/message per line under high-volume workloads while still
/// flushing quickly for low-volume logs.
const LOG_BATCH_LINES: usize = 64;
const LOG_BATCH_MS: u64 = 50;

/// Initial status-bar hint. Unlike transient action results, this stays visible
/// until another interaction replaces it.
const WELCOME_FLASH: &str =
    "Welcome to sofka — ':' resource · enter drill · d describe · l logs · ? help";

/// How long a finished, successful flash stays on screen before the 1s tick
/// clears it (see [`App::expire_flash`]).
const FLASH_TTL: std::time::Duration = std::time::Duration::from_secs(8);

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

/// The ArgoCD CRD group. Used to disambiguate the very generic `applications`
/// and `applicationsets` plurals — only `argoproj.io` kinds get the `t` menu.
const ARGOCD_GROUP: &str = "argoproj.io";

/// Items in the Flux action menu (`t`), in display order. Deliberately a menu
/// — not a single-key toggle — so suspending something always takes an
/// explicit, visible choice rather than one accidental keystroke. "Reconcile
/// now" patches the same `reconcile.fluxcd.io/requestedAt` annotation the
/// `flux reconcile` CLI uses, shared by every controller in the toolkit.
pub const FLUX_MENU_ITEMS: &[&str] = &["Suspend", "Resume", "Reconcile now", "Cancel"];

/// Items in the CronJob action menu (`t`), in display order. "Trigger now"
/// creates a Job from the CronJob's jobTemplate the same way `kubectl create
/// job --from=cronjob/…` does; Suspend/Resume patch `spec.suspend` exactly
/// like the Flux menu (CronJobs share the field).
pub const CRONJOB_MENU_ITEMS: &[&str] = &["Trigger now", "Suspend", "Resume", "Cancel"];

/// Items in the ArgoCD Application action menu (`t`), in display order. "Suspend"
/// disables auto-sync by removing `spec.syncPolicy.automated` and stashing the
/// original value (including `prune`/`selfHeal`/`allowEmpty`) as a base64
/// annotation; "Resume" restores it from the annotation, or defaults to an
/// empty `automated: {}` when the annotation is absent. "Sync now" patches
/// the top-level `operation` field, which the ArgoCD application controller
/// picks up — the same mechanism the `argocd app sync` API endpoint uses.
/// No `argocd` binary.
pub const ARGOCD_MENU_ITEMS: &[&str] = &["Suspend", "Resume", "Sync now", "Cancel"];

/// Items in the ArgoCD ApplicationSet action menu (`t`). Suspend/Resume
/// toggle `spec.syncPolicy.applicationsSync`. There is no `none`/`disabled`
/// mode — suspend sets it to `create-only` (stops updates/deletes of existing
/// child Applications) and stashes the original value as a base64 annotation;
/// resume restores it from the annotation, or defaults to `"sync"`. There is
/// no "Sync now" — ApplicationSet has no `operation` field; it generates
/// Applications on its own schedule, and syncing those individually is an
/// Application-level action.
pub const ARGOCD_APPSET_MENU_ITEMS: &[&str] = &["Suspend", "Resume", "Cancel"];

/// Items in the pod file-transfer menu (`t` on a pod), in display order. Both
/// directions shell out to `kubectl cp` (which needs `tar` in the container),
/// prompting for the source and destination paths.
pub const TRANSFER_MENU_ITEMS: &[&str] = &["Download from pod", "Upload to pod", "Cancel"];

/// An asynchronous operation's ownership of the shared status bar.
#[derive(Debug)]
pub(super) struct ActiveStatusClaim {
    claim: StatusClaim,
    /// Exact text currently displayed for this claim, including a background
    /// message that is only borrowing the bar.
    text: String,
    /// The owner still has a result to deliver. If borrowed transient text
    /// expires first, preserve the claim against an empty bar.
    pending: bool,
}

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
    /// Fuzzy sort-column picker (`S`).
    SortPicker,
    /// Fuzzy field picker over the selected row's cells (`Y`): pick a column
    /// value to copy to the clipboard.
    CopyPicker,
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
    /// Download-or-upload choice for a pod file transfer (`t` on a pod).
    TransferMenu,
    PortForwards,
    Skins,
    /// Browsing saved snapshots (`:snapshots`).
    Snapshots,
    /// Cross-context fleet health dashboard (`:fleet`).
    Fleet,
    /// Global fuzzy-find results picker (`:find <text>`).
    Find,
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
    /// The `[[forwards]]` entry this instance was started from, if any —
    /// links a running child back to its saved config in `:pf`.
    pub(super) config_name: Option<String>,
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
    /// Upload a local file into a pod (`kubectl cp`), once a guardrail
    /// confirmation is satisfied. Upload only — a download doesn't mutate
    /// the pod, so it never needs confirming.
    Transfer {
        ns: String,
        pod: String,
        container: Option<String>,
        src: String,
        dest: String,
    },
    /// One or more node names to cordon and drain.
    Drain { targets: Vec<String> },
    /// Rollout-restart a workload by stamping the pod template's
    /// `restartedAt` annotation (k9s `r`). Single-target — acts on the
    /// selected row, never bulk.
    Restart {
        kind: Kind,
        name: String,
        ns: String,
    },
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
        targets: Vec<(String, String)>,
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
    /// File-transfer path prompts (`t` on a pod), asked in two steps: the
    /// source path first (`src` is `None`), then the destination with the
    /// answered source carried along. `container` pins `-c` when launched
    /// from the container picker.
    Transfer {
        ns: String,
        pod: String,
        container: Option<String>,
        upload: bool,
        src: Option<String>,
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
    /// New name for a kubeconfig context (`r` in the context switcher) —
    /// opened from (and returning to) [`Mode::Contexts`].
    RenameContext {
        old: String,
    },
}

#[derive(Default)]
pub struct Scrollable {
    pub title: String,
    pub lines: VecDeque<String>,
    /// Vertical scroll offset in rendered display rows. `usize` on purpose: a
    /// paused wrapped log buffer can far exceed `u16`.
    pub scroll: usize,
    /// Cached document layout from the last draw. Logs keep their equivalent
    /// viewport and wrapping index in [`LogsView`] instead.
    viewport: Option<DocumentViewport>,
    /// Horizontal scroll offset in columns, for views (`describe`, events) whose
    /// lines run past the right edge. Ignored while `wrap` is on.
    pub hscroll: usize,
    /// Line-wrap toggle. When on, long lines fold instead of being clipped, and
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
    /// Bumped whenever [`Self::lines`] is replaced in place, so the match
    /// cache below can tell "same document" from "same line count".
    revision: u64,
    /// Memoized [`Self::match_lines`], valid for one `(filter, revision,
    /// line count)`.
    ///
    /// `doc_title` calls `match_lines` on every frame just to render the
    /// `[2/5]` counter, and the old implementation lowercased *every line of
    /// the document* into a fresh `String` each time. On a large describe or
    /// YAML view with a `/` search active that was thousands of allocations
    /// at up to 62 Hz.
    match_cache: RefCell<Option<MatchCache>>,
}

struct MatchCache {
    filter: String,
    revision: u64,
    line_count: usize,
    matches: Vec<usize>,
}

struct DocumentViewport {
    width: usize,
    height: usize,
    wrap: bool,
    revision: u64,
    line_count: usize,
    /// Cumulative display-row end for every source line.
    ends: Vec<usize>,
}

impl DocumentViewport {
    fn total_rows(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    fn line_at_row(&self, row: usize) -> usize {
        self.ends.partition_point(|&end| end <= row)
    }

    fn line_start(&self, line: usize) -> usize {
        line.checked_sub(1)
            .and_then(|i| self.ends.get(i).copied())
            .unwrap_or(0)
    }
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
    Snapshot,
    Snapshots,
    Info,
    Fleet,
    Rightsize,
    Find,
    Diff,
    Events,
    PortForwards,
    ProviderLogs,
    Skin,
    Helm,
    Notify,
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
        action: PaletteAction::Snapshot,
        names: &["snapshot", "snap", "dump"],
    },
    PaletteCommand {
        action: PaletteAction::Snapshots,
        names: &["snapshots", "dumps"],
    },
    PaletteCommand {
        action: PaletteAction::Notify,
        names: &["notify", "bell"],
    },
    PaletteCommand {
        action: PaletteAction::Find,
        names: &["find", "fd"],
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
        action: PaletteAction::Info,
        names: &["info", "diagnostics", "about"],
    },
    PaletteCommand {
        action: PaletteAction::Fleet,
        names: &["fleet", "clusters", "multi"],
    },
    PaletteCommand {
        action: PaletteAction::Rightsize,
        names: &["rightsize", "sizing", "vpa"],
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
        let max = self.max_scroll() as i64;
        self.scroll = (self.scroll as i64 + delta as i64).clamp(0, max) as usize;
    }

    pub(crate) fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    pub(crate) fn set_viewport(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let stale = self.viewport.as_ref().is_none_or(|viewport| {
            viewport.width != width
                || viewport.wrap != self.wrap
                || viewport.revision != self.revision
                || viewport.line_count != self.lines.len()
        });
        if stale {
            let mut rows = 0usize;
            let ends = self
                .lines
                .iter()
                .map(|line| {
                    let line_rows = if self.wrap {
                        crate::ui::wrapped_height(line, width)
                    } else {
                        1
                    };
                    rows = rows.saturating_add(line_rows);
                    rows
                })
                .collect();
            self.viewport = Some(DocumentViewport {
                width,
                height,
                wrap: self.wrap,
                revision: self.revision,
                line_count: self.lines.len(),
                ends,
            });
        } else if let Some(viewport) = self.viewport.as_mut() {
            viewport.height = height;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub(crate) fn visible_source_window(&self) -> (usize, usize, usize) {
        let Some(viewport) = self.viewport.as_ref() else {
            let start = self.scroll.min(self.lines.len());
            return (start, self.lines.len(), 0);
        };
        if viewport.height == 0 {
            return (0, 0, 0);
        }

        let start = viewport.line_at_row(self.scroll).min(self.lines.len());
        let row_offset = self.scroll.saturating_sub(viewport.line_start(start));
        let visible_end = self.scroll.saturating_add(viewport.height);
        let end = viewport
            .ends
            .partition_point(|&line_end| line_end < visible_end)
            .saturating_add(1)
            .min(self.lines.len());
        (start, end, row_offset)
    }

    fn max_scroll(&self) -> usize {
        self.viewport.as_ref().map_or_else(
            || self.lines.len().saturating_sub(1),
            |viewport| viewport.total_rows().saturating_sub(viewport.height),
        )
    }

    fn scroll_to_line(&mut self, line: usize) {
        let row = self
            .viewport
            .as_ref()
            .map_or(line, |viewport| viewport.line_start(line));
        self.scroll = row.min(self.max_scroll());
    }

    fn source_line_at_scroll(&self) -> usize {
        self.viewport
            .as_ref()
            .map_or(self.scroll, |viewport| viewport.line_at_row(self.scroll))
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
    /// Toggle line wrap. Turning it on resets the horizontal offset so the view
    /// snaps back to the left margin. Returns the new state.
    pub fn toggle_wrap(&mut self) -> bool {
        let current_line = self.source_line_at_scroll();
        let dimensions = self
            .viewport
            .as_ref()
            .map(|viewport| (viewport.width, viewport.height));
        self.wrap = !self.wrap;
        if self.wrap {
            self.hscroll = 0;
        }
        self.viewport = None;
        if let Some((width, height)) = dimensions {
            self.set_viewport(width, height);
            self.scroll_to_line(current_line);
        }
        self.wrap
    }

    /// A fresh document view. The binary uses this because `Scrollable` has
    /// private cache fields it can't name from outside the library.
    pub fn doc(title: String, lines: Vec<String>) -> Self {
        Scrollable {
            title,
            lines: lines.into(),
            ..Default::default()
        }
    }

    /// Bumped whenever the existing lines are disturbed (replaced, trimmed
    /// from the front, cleared) — anything that shifts or invalidates
    /// previously-computed line indices. A pure append does *not* bump it, so
    /// derived indices can be extended instead of rebuilt.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Drop `n` lines from the front (log-buffer trimming). Shifts every
    /// index, so it bumps the revision.
    pub fn drain_front(&mut self, n: usize) {
        self.lines.drain(0..n);
        self.revision = self.revision.wrapping_add(1);
        self.viewport = None;
    }

    /// Drop every line.
    pub fn clear_lines(&mut self) {
        self.lines.clear();
        self.revision = self.revision.wrapping_add(1);
        self.viewport = None;
    }

    /// Replace the document, invalidating the search-match cache.
    ///
    /// Every other site builds a whole new `Scrollable` (which starts with an
    /// empty cache); this is the one path that swaps the lines underneath a
    /// live view — a refreshed events list — where the new document can have
    /// the same line count as the old one.
    pub fn replace_lines(&mut self, lines: VecDeque<String>) {
        let dimensions = self
            .viewport
            .as_ref()
            .map(|viewport| (viewport.width, viewport.height));
        self.lines = lines;
        self.revision = self.revision.wrapping_add(1);
        self.viewport = None;
        if let Some((width, height)) = dimensions {
            self.set_viewport(width, height);
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    /// Line indices (0-based) containing the active search query, matched
    /// case-insensitively as a substring. Empty when no search is active.
    ///
    /// Memoized: this is called once per frame from `doc_title` and again per
    /// keypress from `n`/`N`, always over the whole document.
    pub fn match_lines(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return Vec::new();
        }
        let line_count = self.lines.len();
        if let Some(c) = self.match_cache.borrow().as_ref()
            && c.revision == self.revision
            && c.line_count == line_count
            && c.filter == self.filter
        {
            return c.matches.clone();
        }

        // Plain case-insensitive substring — deliberately *not* `LogMatcher`,
        // which would give `!` and `/re/` special meaning that the document
        // search has never had. Same SIMD-backed automaton underneath, without
        // lowercasing every line into a throwaway `String`.
        let matcher = crate::logfilter::Substring::new(&self.filter);
        let matches: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| matcher.matches(l))
            .map(|(i, _)| i)
            .collect();

        *self.match_cache.borrow_mut() = Some(MatchCache {
            filter: self.filter.clone(),
            revision: self.revision,
            line_count,
            matches: matches.clone(),
        });
        matches
    }

    /// Finalize a search: scroll to the first match at or after the current
    /// position (wrapping to the first if none follow), so `⏎` lands on a hit
    /// without disturbing the rest of the document. No-op with no matches.
    pub fn focus_first_match(&mut self) {
        let matches = self.match_lines();
        if matches.is_empty() {
            return;
        }
        let current_line = self.source_line_at_scroll();
        let pos = matches.iter().position(|&i| i >= current_line).unwrap_or(0);
        self.match_idx = pos;
        self.scroll_to_line(matches[pos]);
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
        self.scroll_to_line(matches[self.match_idx]);
    }
}

/// Which buffer lines pass the filter, and where each lands in display rows.
///
/// `draw_logs` used to rebuild both of these from scratch on every frame: a
/// `Vec<&String>` of every matching line, plus — with wrap on — a
/// `wrapped_height` walk over every one of them. The buffer holds up to 5,000
/// lines while following and 100,000 while paused, and the viewport shows ~40,
/// so that was O(buffer) work at up to 62 Hz to render O(viewport).
///
/// This index is keyed on `(filter, wrap width, revision)` and, crucially,
/// *extends* when lines are appended rather than rebuilding — a following log
/// stream grows the buffer every batch, so a rebuild-on-length-change cache
/// would never hit.
#[derive(Default)]
pub struct LogIndex {
    filter: String,
    /// Wrap width in columns, or 0 when wrapping is off.
    wrap_width: usize,
    revision: u64,
    /// How many buffer lines have been folded in so far.
    consumed: usize,
    /// Buffer indices that pass the filter, ascending.
    shown: Vec<u32>,
    /// Cumulative display rows *through* `shown[i]`, so the first row of
    /// `shown[i]` is `ends[i-1]` (0 for i == 0). Only maintained when
    /// wrapping; without wrap every line is exactly one row.
    ends: Vec<u32>,
    total_rows: usize,
}

impl LogIndex {
    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn shown_len(&self) -> usize {
        self.shown.len()
    }

    /// Buffer index of the `i`th shown line.
    pub fn line_at(&self, i: usize) -> Option<usize> {
        self.shown.get(i).map(|&x| x as usize)
    }

    /// Display row where the `i`th shown line starts.
    pub fn start_row(&self, i: usize) -> usize {
        if self.wrap_width == 0 {
            return i;
        }
        match i.checked_sub(1) {
            None => 0,
            Some(prev) => self.ends.get(prev).copied().unwrap_or(0) as usize,
        }
    }

    /// Display rows occupied by the `i`th shown line.
    pub fn height_at(&self, i: usize) -> usize {
        if self.wrap_width == 0 {
            return 1;
        }
        self.ends.get(i).copied().unwrap_or(0) as usize - self.start_row(i)
    }

    /// Index of the first shown line that reaches display row `row` — a
    /// binary search over the cumulative row ends, replacing a linear walk
    /// from the top of the buffer.
    pub fn first_at_row(&self, row: usize) -> usize {
        if self.wrap_width == 0 {
            return row.min(self.shown.len());
        }
        self.ends.partition_point(|&end| (end as usize) <= row)
    }

    fn reset(&mut self, filter: &str, wrap_width: usize, revision: u64) {
        self.filter.clear();
        self.filter.push_str(filter);
        self.wrap_width = wrap_width;
        self.revision = revision;
        self.consumed = 0;
        self.shown.clear();
        self.ends.clear();
        self.total_rows = 0;
    }
}

/// All state for the streaming logs view, grouped so it doesn't sprawl across
/// the top-level `App` struct.
pub struct LogsView {
    pub view: Scrollable,
    pub follow: bool,
    pub filter: String,
    /// Compiled form of [`Self::filter`] (substring / regex / inverse). Rebuilt
    /// by [`Self::set_filter`] whenever the filter text changes.
    pub matcher: crate::logfilter::LogMatcher,
    pub wrap: bool,
    pub timestamps: bool,
    pub stopped: bool,
    /// Fullscreen (`F`, k9s): the pane takes the whole frame with no header,
    /// borders, or status line, so terminal text selection copies clean lines.
    /// Session-sticky like `wrap`/`timestamps`; seeded from `[logs] fullscreen`.
    pub fullscreen: bool,
    /// Time anchor for the kubelet streams, set by the `0`–`5` keys (k9s):
    /// `Some(secs)` streams only logs newer than `secs`, `Some(0)` forces the
    /// plain tail, `None` follows the config (`[logs] since`/`tail`).
    pub since_anchor: Option<i64>,
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
    /// Filter/wrap index over [`Self::view`], maintained incrementally.
    index: LogIndex,
}

impl Default for LogsView {
    fn default() -> Self {
        Self {
            view: Scrollable::empty(),
            follow: true,
            filter: String::new(),
            matcher: crate::logfilter::LogMatcher::default(),
            wrap: false,
            timestamps: false,
            stopped: false,
            fullscreen: false,
            since_anchor: None,
            viewport_rows: 0,
            viewport_h: 0,
            last_wrap_width: 0,
            source: None,
            index: LogIndex::default(),
        }
    }
}

impl LogsView {
    /// Replace the filter text and recompile its matcher (substring / regex /
    /// inverse) in one place, so the cached matcher never drifts.
    pub fn set_filter(&mut self, filter: String) {
        self.matcher = crate::logfilter::LogMatcher::new(&filter);
        self.filter = filter;
    }

    /// Whether `line` passes the active filter (empty filter = everything).
    pub fn matches(&self, line: &str) -> bool {
        self.matcher.matches(line)
    }

    /// The index as it stands. Call [`Self::refresh_index`] first — this does
    /// not update it. Split from the refresh so the renderer can hold the
    /// index and the line buffer at the same time.
    pub fn index(&self) -> &LogIndex {
        &self.index
    }

    /// Bring the filter/wrap index up to date with the buffer.
    ///
    /// Rebuilds only when the filter, wrap width, or buffer revision changed;
    /// otherwise folds in just the lines appended since the last call. Pass
    /// `wrap_width = 0` when wrapping is off.
    pub fn refresh_index(&mut self, wrap_width: usize) -> &LogIndex {
        // Field-level destructuring: the loop needs `&mut index` while reading
        // `view` and `matcher`, which a plain `&mut self` borrow would forbid.
        let LogsView {
            view,
            filter,
            matcher,
            index,
            ..
        } = self;

        let revision = view.revision();
        let len = view.lines.len();
        let reusable = index.revision == revision
            && index.wrap_width == wrap_width
            && index.filter == *filter
            && index.consumed <= len;
        if !reusable {
            index.reset(filter, wrap_width, revision);
        }

        for i in index.consumed..len {
            let Some(line) = view.lines.get(i) else {
                break;
            };
            if !matcher.matches(line) {
                continue;
            }
            index.shown.push(i as u32);
            if wrap_width > 0 {
                index.total_rows += crate::ui::wrapped_height(line, wrap_width);
                index.ends.push(index.total_rows as u32);
            } else {
                index.total_rows += 1;
            }
        }
        index.consumed = len;
        index
    }

    /// Title label for the active `0`–`5` time anchor, if any.
    pub fn anchor_label(&self) -> Option<&'static str> {
        match self.since_anchor? {
            0 => Some("tail"),
            60 => Some("1m"),
            300 => Some("5m"),
            900 => Some("15m"),
            1800 => Some("30m"),
            3600 => Some("1h"),
            _ => None,
        }
    }
}

/// A comparable value for one cell, so columns sort numerically where it makes
/// sense (RESTARTS, CPU, AGE…) and lexically otherwise (NAME, STATUS…).
#[derive(Clone)]
enum SortKey {
    Num(f64),
    Text(Rc<str>),
}

impl From<crate::views::SortValue> for SortKey {
    fn from(v: crate::views::SortValue) -> Self {
        match v {
            crate::views::SortValue::Num(n) => SortKey::Num(n),
            crate::views::SortValue::Text(t) => SortKey::Text(t.into()),
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

/// Maximum previous object revisions retained for the session diff.
const PREV_REVISIONS_MAX: usize = 256;

/// Previous revisions of objects the watch saw change, so `:diff` can show
/// previous → live for GitOps-managed objects (whose
/// `last-applied-configuration` is empty — nothing `kubectl apply`s them).
/// Bounded FIFO keyed by (kind plural, store key); survives view switches so
/// drilling away and back keeps the baseline.
#[derive(Default)]
pub(super) struct PrevRevisions {
    map: HashMap<(String, String), Arc<DynamicObject>>,
    order: VecDeque<(String, String)>,
}

impl PrevRevisions {
    pub(super) fn insert(&mut self, kind: &str, key: &str, obj: Arc<DynamicObject>) {
        let k = (kind.to_string(), key.to_string());
        if self.map.insert(k.clone(), obj).is_none() {
            self.order.push_back(k);
            while self.order.len() > PREV_REVISIONS_MAX {
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                }
            }
        }
    }

    pub(super) fn get(&self, kind: &str, key: &str) -> Option<&DynamicObject> {
        self.map
            .get(&(kind.to_string(), key.to_string()))
            .map(Arc::as_ref)
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
    keys: Vec<RowKey>,
    cells: HashMap<RowKey, CellCacheEntry>,
    /// Computed primary sort keys, valid per (sort header, resourceVersion) —
    /// a rebuild touches every object, but only changed rows re-extract.
    sort_keys: HashMap<RowKey, SortKeyEntry>,
    /// Helm view only: the latest-revision dedup, paired with the store
    /// version it was computed from. A rebuild staled by a filter keystroke or
    /// a sort toggle leaves the store untouched, so the dedup still holds.
    helm_latest: Option<(u64, HashSet<RowKey>)>,
}

struct CellCacheEntry {
    plural: String,
    resource_version: Option<String>,
    cells: Vec<String>,
    status_idx: Option<usize>,
    /// Per-cell character-presence masks, and their union across the row.
    /// See [`subseq_mask`]: a cheap necessary condition for a fuzzy
    /// subsequence match, used to skip cells (and whole rows) without paying
    /// for a Skim match.
    cell_masks: Vec<u64>,
    row_mask: u64,
}

/// A 64-bit "which characters occur here" summary, used as a prefilter before
/// the fuzzy matcher.
///
/// A fuzzy match is a subsequence match, so every character of the pattern
/// must occur in the haystack. If any pattern character is missing, the match
/// is impossible and Skim — which allocates and runs a DP over the pair — need
/// never be called. Bytes are folded to lowercase and bucketed mod 64, so
/// collisions only ever produce a *false positive* (the real matcher then
/// decides); a false negative is impossible, which is what makes this safe.
fn subseq_mask(s: &str) -> u64 {
    let mut m = 0u64;
    for b in s.as_bytes() {
        m |= 1u64 << (b.to_ascii_lowercase() % 64);
    }
    m
}

struct SortKeyEntry {
    header: String,
    resource_version: Option<String>,
    key: SortKey,
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

/// Maximum view snapshots kept for instant redisplay when navigating back to
/// a recently-watched view (least-recently-used beyond this).
const VIEW_CACHE_MAX: usize = 8;

/// Second, and in practice the binding, bound on the view cache: the total
/// objects it may retain across all snapshots. A view-count cap is not a
/// memory cap — on a large cluster eight snapshots of a 2,000-pod view cost
/// roughly eight times one snapshot. Objects are `Arc`-shared with the live
/// store, so the live view is nearly free; this bounds the *departed* ones.
const VIEW_CACHE_MAX_OBJECTS: usize = 10_000;

/// Identity of a watch scope, used to key cached view snapshots. Two visits
/// with the same key list exactly the same server-side set, so the previous
/// rows are safe to show while the fresh watch syncs. `labels`/`fields` are
/// the *merged* selectors the watch was started with (drill-down + `-l`/`-f`
/// filter terms).
#[derive(Clone, PartialEq, Eq, Hash)]
struct ViewKey {
    kind_plural: String,
    namespace: String,
    labels: Option<String>,
    fields: Option<String>,
}

/// One root view for the `[`/`]` history: which kind was listed in which
/// namespace. Drill-down state (selectors, filter, scope) is deliberately not
/// kept — history replays root views; the breadcrumb stack handles drills.
#[derive(Clone, PartialEq, Eq)]
struct ViewEntry {
    kind_plural: String,
    namespace: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerScope {
    pub kind: String,
    pub name: String,
    pub uid: Option<String>,
}

impl OwnerScope {
    pub fn owns(&self, obj: &DynamicObject) -> bool {
        let refs = obj.metadata.owner_references.as_deref().unwrap_or_default();
        if refs.is_empty() {
            let prefix = format!("{}-", self.name);
            return obj
                .metadata
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with(&prefix));
        }
        refs.iter().any(|r| {
            r.kind.eq_ignore_ascii_case(&self.kind)
                && r.name == self.name
                && match &self.uid {
                    Some(uid) if !r.uid.is_empty() => r.uid == *uid,
                    _ => true,
                }
        })
    }
}

/// A saved view, pushed onto the stack when drilling down.
struct Frame {
    kind: Option<Kind>,
    kind_plural: String,
    namespace: String,
    labels: Option<String>,
    fields: Option<String>,
    owner: Option<OwnerScope>,
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
    pub owner: Option<OwnerScope>,
    /// Drill-down breadcrumb shown in the header, e.g. "deploy/foo".
    pub scope_label: Option<String>,

    pub generation: u64,
    gen_flag: Arc<AtomicU64>,
    pub tasks: Vec<JoinHandle<()>>,
    pub tx: Sender<Msg>,
    stack: Vec<Frame>,
    /// Scope of the running watch, so its rows can be stashed under the right
    /// key when the user navigates away.
    watch_key: Option<ViewKey>,
    /// Snapshots of recently-left views: shown instantly (marked syncing) when
    /// the user navigates back, while the fresh watch relists in the
    /// background. Bounded by [`VIEW_CACHE_MAX`]; cleared on context switch.
    view_cache: HashMap<ViewKey, crate::store::Items>,
    /// LRU order for [`Self::view_cache`] (front = oldest).
    view_cache_order: VecDeque<ViewKey>,
    /// Browser-style history of root views for `[`/`]`: every root switch
    /// (kind and/or namespace) is recorded; navigating with `[`/`]` moves the
    /// cursor without re-recording, and a fresh switch truncates the forward
    /// tail — exactly like browser history.
    history: Vec<ViewEntry>,
    history_pos: usize,

    pub mode: Mode,
    pub table_state: TableState,
    pub table_page_rows: usize,
    /// Row keys (`ns/name`) marked for bulk actions via SPACE. Cleared whenever
    /// the view is (re)watched. Bulk actions target this set if non-empty, else
    /// the current selection.
    pub marked: HashSet<String>,
    /// Column index (into the displayed headers) to sort the table by, or
    /// `None` for the natural namespace/name order.
    pub sort_column: Option<usize>,
    pub sort_desc: bool,
    /// Horizontal column scroll: how many columns after the anchored
    /// NAMESPACE/NAME prefix are hidden off the left edge (←/→ in the
    /// table). Clamped by `draw_table`, since the header set can change
    /// underneath it; reset when the view spec is rebuilt.
    pub col_offset: usize,
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
    /// Last flash text observed by [`App::expire_flash`], so a change can be
    /// detected (and re-timestamped) without touching every call site that
    /// sets `flash` directly.
    pub(super) flash_seen: String,
    pub(super) flash_since: std::time::Instant,
    /// Keeps the current flash on screen indefinitely. Only the welcome hint
    /// starts sticky; the first flash that replaces it clears the flag, so no
    /// call site has to.
    pub(super) flash_sticky: bool,
    /// Monotonic source for asynchronous status-bar ownership claims.
    pub(super) next_status_claim: u64,
    /// Current claim and the exact text it owns. Comparing the text makes a
    /// direct status assignment invalidate the claim even before the next
    /// expiry tick observes that assignment.
    pub(super) status_claim: Option<ActiveStatusClaim>,
    /// Last action failure, recorded even when a newer operation owned the
    /// status bar and the message could not be shown. Surfaced by `:debug`
    /// so a failure is never lost outright.
    pub last_action_error: Option<String>,

    pub detail: Scrollable,
    /// Search query for the help view (`?`), which has no backing
    /// [`Scrollable`] — its lines are built at render time.
    pub help_filter: String,
    /// Which view help was opened from, so closing it returns to that view.
    pub help_return: Mode,
    /// Which doc view (`Detail`/`Diff`/`Events`/`Help`) the `/` search prompt
    /// was opened from, so the renderer keeps drawing it underneath and
    /// enter/esc return to it.
    pub doc_filter_return: Mode,
    /// Which navigation view the `:` command palette was opened from, so esc
    /// returns there and the renderer keeps drawing it underneath the popup.
    pub palette_return: Mode,
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
    /// Type-to-filter buffer for the context switcher. Plain action keys remain
    /// available until filtering starts.
    pub ctx_filter: String,
    /// Whether the context switcher is accepting filter input (started by
    /// typing or explicitly with `/` for names that begin with an action key).
    pub ctx_filtering: bool,
    pub sort_picker_state: ListState,
    /// Type-to-filter buffer for the sort-column picker.
    pub sort_picker_filter: String,
    pub copy_picker_state: ListState,
    /// Type-to-filter buffer for the copy-field picker.
    pub copy_picker_filter: String,
    /// The selected row's `(header, value)` pairs, captured when the copy
    /// picker opens so a watch update can't shift entries mid-pick.
    pub copy_picker_fields: Vec<(String, String)>,
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
    /// Log-view options (`[logs]`), re-applied on context switch and `:reload`.
    pub logs_cfg: crate::config::LogsConfig,
    /// Cross-context fleet dashboard config (`[fleet]`).
    pub fleet_cfg: crate::config::FleetConfig,
    /// Fleet dashboard rows (one per configured context), filled in as each
    /// context's summary lands.
    pub fleet_rows: Vec<crate::fleet::FleetRow>,
    pub fleet_state: ListState,
    /// Fleet membership marks (`space` in the context switcher), overlaying
    /// `[fleet] contexts`. Persisted to `fleet_marks_path` on every toggle.
    pub fleet_marks: crate::fleet::FleetMarks,
    /// Where fleet marks persist (`<state-dir>/fleet.toml`, set at startup);
    /// `None` (tests) keeps them in memory only.
    pub fleet_marks_path: Option<std::path::PathBuf>,
    /// Remembered sort per kind (`S`/`I`/header click), restored on every
    /// view start. Persisted to `sort_memory_path` on every change.
    pub sort_memory: crate::sortmem::SortMemory,
    /// Where remembered sorts persist (`<state-dir>/sort.toml`, set at
    /// startup); `None` (tests) keeps them in memory only.
    pub sort_memory_path: Option<std::path::PathBuf>,
    /// Last namespace picked per context (`n`/`0`/`:ns`/`:<kind> <ns>`),
    /// restored at launch and on `:ctx`. Persisted to `namespace_memory_path`
    /// on every pick.
    pub namespace_memory: crate::nsmem::NamespaceMemory,
    /// Where remembered namespaces persist (`<state-dir>/namespaces.toml`,
    /// set at startup); `None` (tests) keeps them in memory only.
    pub namespace_memory_path: Option<std::path::PathBuf>,
    /// Global fuzzy-find (`:find`) results and picker cursor.
    pub find_items: Vec<crate::store::FindItem>,
    pub find_state: ListState,
    pub find_query: String,
    /// The last bundle assembled by `:bundle`, previewed in the detail view and
    /// written to disk by `:bundle-save`: `(filename, text)`.
    pub pending_bundle: Option<(String, String)>,
    /// Session-local log of mutating actions taken (`:journal`).
    pub journal: crate::journal::Journal,
    /// Count of watch/stream errors seen this session, for `:info` diagnostics.
    pub watch_errors: u64,
    /// The most recent error message, for `:info` diagnostics.
    pub last_error: Option<String>,
    /// Whether the Metrics API has ever returned data this session.
    pub metrics_seen: bool,
    /// The metrics poll's most recent failure (`None` while it works), for
    /// `:info` — a broken metrics-server must not read as "usage is zero".
    pub metrics_error: Option<String>,
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

    /// Cursor into [`FLUX_MENU_ITEMS`] / [`CRONJOB_MENU_ITEMS`] for the `t`
    /// action menu (Flux suspend/resume, CronJob trigger/suspend/resume).
    pub flux_menu_state: ListState,

    /// Cursor into [`TRANSFER_MENU_ITEMS`] for the pod file-transfer menu
    /// (`t` on a pod), and the `(ns, pod, container)` it acts on.
    pub transfer_menu_state: ListState,
    pub transfer_target: Option<(String, String, Option<String>)>,

    /// Background `kubectl port-forward` processes started with `f`/`F`.
    /// Viewed/stopped via `:pf`; killed automatically on drop.
    pub port_forwards: Vec<PortForward>,
    pub pf_state: ListState,
    /// Saved `[[forwards]]` from config: shown in `:pf` even while stopped,
    /// startable with one keystroke, autostarted on connect when configured.
    pub forwards_cfg: Vec<crate::config::Forward>,
    /// `[notify]` delivery options (bell, desktop-notification protocol).
    pub notify_cfg: crate::config::NotifyConfig,
    /// Compiled `[keys]` palette-completion bindings.
    pub palette_keys: crate::config::PaletteKeys,

    pub skin_list: Vec<String>,
    pub skin_state: ListState,
    /// Saved snapshots for the `:snapshots` browser: `(path, display label)`,
    /// newest first. Rebuilt each time the browser opens.
    pub snapshot_list: Vec<(std::path::PathBuf, String)>,
    pub snapshot_state: ListState,
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
    /// Latest pod count per node (nodes view PODS column). `None` until the
    /// first successful pods list, so "no data yet" renders as "-" instead of
    /// a misleading 0.
    pub node_pods: Option<HashMap<String, usize>>,

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
    /// Parent of the explain view. Kept separately because an evidence view
    /// (logs/events) temporarily uses `return_mode` to return to Explain.
    explain_return: Mode,
    /// GitOps view: the reconciliation-chain findings, cursor, title, and the
    /// object being investigated (kept so `r` can re-gather).
    pub gitops_items: Vec<crate::explain::Finding>,
    pub gitops_state: ListState,
    pub gitops_title: String,
    pub gitops_source: Option<DynamicObject>,
    /// Session-local per-object state-change history, fed by the table watch.
    pub timeline: crate::timeline::Timeline,
    /// Table geometry from the last frame, for mouse hit-testing. A RefCell
    /// because the renderer records it while the frame still borrows rows.
    pub(super) table_hit: RefCell<Option<mouse::TableHit>>,
    /// Active `:notify` watches, keyed by `plural/ns/name`. Each is its own
    /// single-object background watcher, deliberately NOT in [`Self::tasks`]:
    /// a notify must survive `bump_generation` (view switches) and fire from
    /// anywhere until toggled off.
    pub(super) notify_tasks: HashMap<String, tokio::task::JoinHandle<()>>,
    /// Notifications waiting for the main loop to deliver (bell, desktop
    /// escape sequence, notifier subprocess). Drained once per frame and
    /// joined, so a burst arriving in one batch is one delivery — sinks
    /// rate-limit rapid-fire notifications.
    pub(super) pending_notify: Vec<String>,
    /// Previous object revisions for the session diff (`:diff` fallback).
    pub(super) prev_revisions: PrevRevisions,
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
    /// Scratch buffer for the fuzzy filter's "namespace name" haystack, reused
    /// across rows so the filter pass doesn't allocate a `String` per object.
    hay_buf: RefCell<String>,
    /// Scratch buffer for the `"{namespace}/{name}"` metrics-map key, reused
    /// for the same reason: it is built per visible row per frame.
    metrics_key_buf: RefCell<String>,

    /// Compiled log provider from `[providers.logs]`, re-resolved on context
    /// switch and `:reload` so each cluster can point at its own backend.
    pub log_provider: Option<crate::providers::LogProvider>,
    /// Prometheus/VictoriaMetrics backend for right-sizing (`:rightsize`),
    /// resolved to the API-server proxy on first use when autodiscovered.
    pub metrics_provider: Option<crate::providers::MetricsProvider>,
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
    /// Compact mode (`ctrl-e`): collapse the header to one line and hide the
    /// footer, so a tiled/multiplexed pane shows mostly table.
    pub compact: bool,
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
            owner: None,
            scope_label: None,
            generation: 0,
            gen_flag: Arc::new(AtomicU64::new(0)),
            tasks: Vec::new(),
            tx,
            stack: Vec::new(),
            watch_key: None,
            view_cache: HashMap::new(),
            view_cache_order: VecDeque::new(),
            history: Vec::new(),
            history_pos: 0,
            mode: Mode::Table,
            table_state: TableState::default(),
            table_page_rows: 10,
            marked: HashSet::new(),
            sort_column: None,
            sort_desc: false,
            col_offset: 0,
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
            flash: WELCOME_FLASH.into(),
            flash_err: false,
            // Pre-seeded so the first tick sees no change and leaves the
            // welcome hint's sticky flag alone.
            flash_seen: WELCOME_FLASH.into(),
            flash_since: std::time::Instant::now(),
            flash_sticky: true,
            next_status_claim: 0,
            status_claim: None,
            last_action_error: None,
            detail: Scrollable::empty(),
            help_filter: String::new(),
            help_return: Mode::Table,
            doc_filter_return: Mode::Detail,
            palette_return: Mode::Table,
            logs: LogsView::default(),
            ns_list: Vec::new(),
            ns_state: ListState::default(),
            namespace_favorites: Vec::new(),
            recent_namespaces: HashMap::new(),
            ns_filter: String::new(),
            ctx_list: Vec::new(),
            ctx_state: ListState::default(),
            ctx_filter: String::new(),
            ctx_filtering: false,
            sort_picker_state: ListState::default(),
            sort_picker_filter: String::new(),
            copy_picker_state: ListState::default(),
            copy_picker_filter: String::new(),
            copy_picker_fields: Vec::new(),
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
            logs_cfg: crate::config::LogsConfig::default(),
            fleet_cfg: crate::config::FleetConfig::default(),
            fleet_rows: Vec::new(),
            fleet_state: ListState::default(),
            fleet_marks: crate::fleet::FleetMarks::default(),
            fleet_marks_path: None,
            sort_memory: crate::sortmem::SortMemory::default(),
            sort_memory_path: None,
            namespace_memory: crate::nsmem::NamespaceMemory::default(),
            namespace_memory_path: None,
            find_items: Vec::new(),
            find_state: ListState::default(),
            find_query: String::new(),
            pending_bundle: None,
            journal: crate::journal::Journal::default(),
            watch_errors: 0,
            last_error: None,
            metrics_seen: false,
            metrics_error: None,
            rbac_allowed: None,
            last_rbac_ns: None,
            container_list: Vec::new(),
            container_state: ListState::default(),
            container_pod: None,
            container_resources: HashMap::new(),
            container_qos: String::new(),
            flux_menu_state: ListState::default(),
            transfer_menu_state: ListState::default(),
            transfer_target: None,
            port_forwards: Vec::new(),
            forwards_cfg: Vec::new(),
            notify_cfg: crate::config::NotifyConfig::default(),
            palette_keys: crate::config::PaletteKeys::default(),
            pf_state: ListState::default(),
            skin_list: crate::theme::BUILTIN_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            skin_state: ListState::default(),
            snapshot_list: Vec::new(),
            snapshot_state: ListState::default(),
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
            node_pods: None,
            pulse: Pulse::default(),
            xray_items: Vec::new(),
            xray_state: ListState::default(),
            explain_items: Vec::new(),
            explain_state: ListState::default(),
            explain_title: String::new(),
            explain_source: None,
            explain_return: Mode::Table,
            gitops_items: Vec::new(),
            gitops_state: ListState::default(),
            gitops_title: String::new(),
            gitops_source: None,
            timeline: crate::timeline::Timeline::default(),
            table_hit: RefCell::new(None),
            notify_tasks: HashMap::new(),
            pending_notify: Vec::new(),
            prev_revisions: PrevRevisions::default(),
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
            hay_buf: RefCell::new(String::new()),
            metrics_key_buf: RefCell::new(String::new()),
            rows_cache: RefCell::new(RowsCache {
                dirty: true,
                keys: Vec::new(),
                cells: HashMap::new(),
                sort_keys: HashMap::new(),
                helm_latest: None,
            }),
            log_provider: None,
            metrics_provider: None,
            user_views: HashMap::new(),
            thresholds: crate::thresholds::Compiled::default(),
            crd_views: HashMap::new(),
            wide: false,
            compact: false,
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

    /// Whether the active prompt was opened from the context switcher, so the
    /// renderer keeps the picker underneath it and esc/enter return there.
    pub fn prompt_over_contexts(&self) -> bool {
        matches!(self.prompt_kind, Some(PromptKind::RenameContext { .. }))
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
mod diagnostics;
mod explain;
mod find;
mod fleet;
mod gitops;
mod guardrails;
mod helpers;
mod input;
mod journal;
mod lifecycle;
mod logs;
mod mouse;
mod navigation;
mod notify;
mod overlays;
mod pickers;
mod rightsize;
mod rows;
mod snapshot;
mod timeline;
mod workspaces;

use helpers::*;
pub use notify::notification_sequence;
pub use pickers::DEFAULT_SORT_LABEL;

#[cfg(test)]
mod tests;
