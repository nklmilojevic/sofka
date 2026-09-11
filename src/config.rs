//! User configuration in TOML or YAML under `$XDG_CONFIG_HOME/sofka`
//! (falling back to `~/.config/sofka`), with optional
//! per-cluster / per-context overrides, k9s-style:
//!
//! ```text
//! sofka/
//! ├── config.toml                  # base, applies everywhere
//! ├── conf.d/
//! │   └── *.toml, *.yaml, *.yml    # drop-ins merged over the base, in name order
//! └── clusters/
//!     └── <cluster>/               # kubeconfig *cluster* name
//!         ├── config.toml          # every context on this cluster
//!         └── <context>/           # kubeconfig *context* name
//!             └── config.toml      # this context only
//! ```
//!
//! Override files are partial configs merged over the base (cluster level
//! first, then context level): tables like `[aliases]` and `[skin.colors]`
//! merge key-by-key, everything else — strings, booleans, arrays like
//! `[[plugins]]` — replaces the base value. Directory names are the
//! kubeconfig names sanitized for the filesystem: any character other than
//! ASCII letters, digits, `.`, `_` and `-` becomes `-`, so an EKS context
//! `arn:aws:eks:eu-west-1:123:cluster/prod` lives in
//! `arn-aws-eks-eu-west-1-123-cluster-prod/`.
//!
//! Example:
//! ```toml
//! default_namespace = "kube-system"
//! default_resource  = "deployments"
//!
//! [aliases]
//! dep = "deployments"
//! ti  = "deployments"   # whatever shortcuts you like
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

mod document;
mod key_migration;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Namespace to start in when none is given on the CLI.
    pub default_namespace: Option<String>,
    /// Resource to open on launch when none is given on the CLI.
    pub default_resource: Option<String>,
    /// Disable every action that could modify the cluster (or run arbitrary
    /// commands): delete, edit, scale, restart, set-image, cordon/drain,
    /// Flux suspend/resume/reconcile, Helm rollback/uninstall, shell/attach,
    /// and plugins. Overridden by the `--readonly`/`--write` CLI flags. Set
    /// it in a per-cluster/per-context override file to lock down just prod.
    pub readonly: bool,
    /// Hide the header, including the logo. Defaults to false.
    pub hide_header: bool,
    /// Start in compact mode; runtime toggles survive reloads and context switches.
    pub compact_mode: bool,
    /// Custom alias -> canonical resource (plural/kind) mappings.
    pub aliases: HashMap<String, String>,
    /// Namespaces pinned to the top of the switcher (a curated team list, in
    /// the given order). Session-local recents follow them.
    pub favorite_namespaces: Vec<String>,
    /// User-defined shell-out plugins bound to keys.
    pub plugins: Vec<Plugin>,
    /// Saved navigation commands bound to keys and the palette — see
    /// [`Bookmark`]. Validated by [`bookmark_warnings`].
    pub bookmarks: Vec<Bookmark>,
    /// Task-oriented collections of views — see [`Workspace`]. Validated by
    /// [`workspace_warnings`].
    pub workspaces: Vec<Workspace>,
    /// Declarative safety policies gating dangerous actions — see
    /// [`Guardrail`]. Validated by [`guardrail_warnings`].
    pub guardrails: Vec<Guardrail>,
    /// Custom table views keyed by resource — see [`ViewConfig`]. Compiled
    /// and validated by [`crate::views::compile`].
    pub views: HashMap<String, ViewConfig>,
    /// Color skin: a built-in palette name plus optional per-swatch overrides.
    pub skin: Skin,
    /// Optional external observability backends — see [`Providers`]. Compiled
    /// and validated by [`crate::providers::compile`].
    pub providers: Providers,
    /// Warning/critical thresholds for value-based cell coloring — see
    /// [`Thresholds`]. Compiled and validated by [`crate::thresholds::compile`].
    pub thresholds: Thresholds,
    /// Defaults for the ephemeral-debug-container workflow (`:debug`) — see
    /// [`DebugConfig`].
    pub debug: DebugConfig,
    /// Diagnostic-bundle (`:bundle`) options — see [`BundleConfig`].
    pub bundle: BundleConfig,
    /// Helper-pod defaults for the PVC browser — see [`PvcExploreConfig`].
    pub pvc_explore: PvcExploreConfig,
    /// Log-view options — see [`LogsConfig`].
    pub logs: LogsConfig,
    /// Cross-context fleet dashboard (`:fleet`) — see [`FleetConfig`].
    pub fleet: FleetConfig,
    /// Node drain policy — see [`DrainConfig`].
    pub drain: DrainConfig,
    /// Saved port-forwards — see [`Forward`]. Validated by
    /// [`forward_warnings`].
    pub forwards: Vec<Forward>,
    /// Mouse support (wheel scroll, click-to-select, header-click sort).
    /// `None` = on. Set `mouse = false` to keep the terminal's native mouse
    /// behavior (text selection) everywhere. Document views release capture
    /// on their own regardless — see [`crate::app::App::wants_mouse_capture`].
    pub mouse: Option<bool>,
    /// Navigation steps per received mouse wheel event in views with mouse
    /// capture. Defaults to 3; `0` is treated as 1 with a warning.
    pub mouse_scroll_lines: Option<u16>,
    /// Set the terminal title to the context and namespace. Defaults to true.
    pub terminal_title: Option<bool>,
    /// Save and restore sort choices per kind. Defaults to true.
    pub remember_sort: Option<bool>,
    /// How `:notify` events are delivered — see [`NotifyConfig`].
    pub notify: NotifyConfig,
    /// Built-in keyboard bindings, validated by [`crate::keymap::Keymap::compile`].
    pub keys: KeysConfig,
    /// Structured application logging — see [`LoggingConfig`].
    pub logging: LoggingConfig,
    pub journal: JournalConfig,
}

/// Delivery for `:notify` events, besides the status-line flash.
///
/// ```toml
/// [notify]
/// bell = true         # ring the terminal bell
/// desktop = "osc777"  # "osc777" | "osc9" | "both" | "off"
/// # command = ["notify-send", "sofka", "$MESSAGE"]   # optional subprocess
/// ```
///
/// `osc777` (the default) is the rxvt-style title+body form Ghostty
/// recommends (Ghostty, kitty, WezTerm, foot, urxvt). `osc9` is the
/// iTerm2-style body-only notification for terminals that speak only that
/// (iTerm2, Windows Terminal; also Ghostty, kitty, WezTerm, foot).
/// Terminals ignore protocols they don't speak, but one that speaks both
/// would show two notifications under `both` — hence a choice, not a
/// broadcast.
///
/// `command` runs a local notifier subprocess instead of relying on escape
/// sequences — the route that works inside terminal multiplexers, which
/// swallow OSC sequences from their panes. `$MESSAGE` substitutes as a whole
/// argument (never spliced into a shell string); without a `$MESSAGE`
/// placeholder the message is appended as the final argument. Inside
/// **herdr** no configuration is needed: sofka detects the pane environment
/// and delivers through `herdr notification show`, which honours herdr's own
/// `ui.toast` delivery (in-app, outer terminal, or system).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Ring the terminal bell on a notification.
    pub bell: bool,
    /// Desktop-notification escape protocol: `osc777`, `osc9`, `both`, `off`.
    pub desktop: String,
    /// Notifier subprocess (argv). Empty = none configured (herdr panes
    /// auto-detect). `$MESSAGE` substitutes the notification text.
    pub command: Vec<String>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            bell: true,
            desktop: "osc777".into(),
            command: Vec::new(),
        }
    }
}

/// Validate `[notify]`: an unknown `desktop` value warns and behaves as the
/// default (`osc777`); a `command` with an empty executable warns and is
/// ignored.
pub fn notify_warnings(cfg: &NotifyConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    match cfg.desktop.as_str() {
        "osc9" | "osc777" | "both" | "off" => {}
        other => warnings.push(format!(
            "notify: desktop '{other}' is not osc777/osc9/both/off; using osc777"
        )),
    }
    if cfg.command.first().is_some_and(|exe| exe.trim().is_empty()) {
        warnings.push("notify: command has an empty executable; ignored".into());
    }
    warnings
}

/// Keep key settings as TOML values so key errors cannot discard other settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct KeysConfig(pub toml::Value);

impl Default for KeysConfig {
    fn default() -> Self {
        Self(toml::Value::Table(toml::Table::new()))
    }
}

/// A named, saved port-forward. Shows up in `:pf` even when stopped, so one
/// keystroke starts it; `autostart = true` also starts it on connect (and on
/// every context switch, when its `contexts` list matches).
///
/// ```toml
/// [[forwards]]
/// name = "argocd"
/// target = "svc/argocd-server"   # kubectl syntax: pod/…, svc/…, deploy/…
/// namespace = "argocd"
/// ports = "8080:443"             # LOCAL:REMOTE
/// autostart = true               # start when sofka connects (default false)
/// contexts = ["home"]            # optional: only in these contexts (exact)
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Forward {
    pub name: String,
    /// Forward target in kubectl syntax (`pod/web`, `svc/api`, `deploy/api`).
    pub target: String,
    pub namespace: String,
    /// `LOCAL:REMOTE` port pair (kubectl also accepts a bare port).
    pub ports: String,
    /// Start automatically when sofka connects / switches to a matching
    /// context.
    pub autostart: bool,
    /// Kubeconfig context names this forward applies to (exact match).
    /// Empty = every context.
    pub contexts: Vec<String>,
}

impl Forward {
    /// Whether this forward applies to `context`.
    pub fn matches_context(&self, context: &str) -> bool {
        self.contexts.is_empty() || self.contexts.iter().any(|c| c == context)
    }
}

/// Validate `[[forwards]]`, returning one warning per problem (the entries
/// stay usable where possible; unusable ones are flagged, not fatal).
pub fn forward_warnings(forwards: &[Forward]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for f in forwards {
        let label = if f.name.is_empty() {
            "<unnamed>"
        } else {
            &f.name
        };
        if f.name.trim().is_empty() {
            warnings.push("forwards: entry with empty name".to_string());
        } else if !seen.insert(f.name.clone()) {
            warnings.push(format!("forwards.{label}: duplicate name"));
        }
        if f.target.trim().is_empty() {
            warnings.push(format!("forwards.{label}: empty target"));
        }
        if f.ports.trim().is_empty()
            || !f
                .ports
                .split(':')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            || f.ports.split(':').count() > 2
        {
            warnings.push(format!(
                "forwards.{label}: ports '{}' is not LOCAL:REMOTE (e.g. 8080:80)",
                f.ports
            ));
        }
    }
    warnings
}

/// The opt-in cross-context fleet dashboard (`:fleet`). Only the kubeconfig
/// contexts listed here are ever queried.
///
/// ```toml
/// [fleet]
/// contexts = ["prod-eu", "prod-us", "staging"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Kubeconfig context names to summarize. Empty = the dashboard is off.
    pub contexts: Vec<String>,
}

/// What a node drain is allowed to evict, mirroring the `kubectl drain`
/// flags of the same names. These are the defaults the drain dialog opens
/// with; `i`, `e` and `f` toggle them per drain.
///
/// ```toml
/// [drain]
/// ignore_daemonsets    = true   # skip DaemonSet pods instead of refusing
/// delete_emptydir_data = false  # evict pods whose emptyDir data is lost
/// force                = false  # evict pods no controller will recreate
/// ```
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DrainConfig {
    /// Skip DaemonSet-managed pods (`--ignore-daemonsets`). Off means a node
    /// running any DaemonSet pod refuses to drain, as `kubectl` does.
    pub ignore_daemonsets: bool,
    /// Evict pods with an `emptyDir` volume, whose contents are deleted with
    /// the pod (`--delete-emptydir-data`).
    pub delete_emptydir_data: bool,
    /// Evict pods that no controller will recreate — bare pods, and pods left
    /// by a deleted owner (`--force`).
    pub force: bool,
}

impl Default for DrainConfig {
    fn default() -> Self {
        // Matches what sofka did before the flags existed: DaemonSet pods were
        // always skipped, emptyDir and uncontrolled pods always blocked.
        Self {
            ignore_daemonsets: true,
            delete_emptydir_data: false,
            force: false,
        }
    }
}

/// Log-view controls (kubelet streams).
///
/// ```toml
/// [logs]
/// tail = 300         # initial lines fetched per stream
/// buffer = 5000      # max lines retained while following (bounded tail)
/// since = "1h"       # optional: only logs newer than this, within the tail limit
/// fullscreen = false # open log views fullscreen (F toggles; k9s fullScreenLogs)
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogsConfig {
    /// Initial lines requested per stream (`kubectl logs --tail`).
    pub tail: i64,
    /// Maximum lines retained in the follow buffer before the oldest are
    /// dropped (keeps a chatty pod from growing memory without bound).
    pub buffer: usize,
    /// Optional lookback (`30m`, `4h`, `2d`): stream only logs newer than this.
    /// The initial request also keeps the configured `tail` limit.
    pub since: Option<String>,
    /// Start log views fullscreen — the pane takes the whole frame, without
    /// header or borders (k9s `fullScreenLogs`). `F` toggles per session.
    pub fullscreen: bool,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            tail: 300,
            buffer: 5000,
            since: None,
            fullscreen: false,
        }
    }
}

/// Optional action history on disk. Changes take effect on restart.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JournalConfig {
    pub enabled: bool,
    pub file: Option<PathBuf>,
    pub max_size_mb: u64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            file: None,
            max_size_mb: 8,
        }
    }
}

impl JournalConfig {
    pub fn path(&self) -> PathBuf {
        self.file
            .clone()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| crate::diagnostics::state_dir().join("journal.jsonl"))
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_size_mb.saturating_mul(1024 * 1024).max(64 * 1024)
    }
}

/// Structured application logging — sofka's own diagnostics, not pod logs
/// (those are [`LogsConfig`]).
///
/// ```toml
/// [logging]
/// level = "info"           # off (default) | error | warn | info | debug | trace
/// # file = "/tmp/sofka.log"  # default: <state-dir>/logs/sofka.log
/// max_size_mb = 8          # rotate to <file>.1 past this size
/// ```
///
/// `SOFKA_LOG=debug` overrides `level` for one run, which is how you turn
/// logging on for a session without editing config. Every value written is
/// redacted first (see [`crate::redact`]), so the log can be attached to a bug
/// report as-is.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// `off` | `error` | `warn` | `info` | `debug` | `trace`.
    pub level: String,
    /// Log file. Defaults to `<state-dir>/logs/sofka.log`.
    pub file: Option<PathBuf>,
    /// Size at which the log rotates to `<file>.1`.
    pub max_size_mb: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "off".into(),
            file: None,
            max_size_mb: 8,
        }
    }
}

impl LoggingConfig {
    /// Where the log goes, config first and the state directory otherwise.
    pub fn path(&self) -> PathBuf {
        self.file
            .clone()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(crate::diagnostics::default_log_path)
    }

    /// Rotation threshold in bytes, floored at 64KiB — a smaller cap would
    /// rotate faster than a session can be read back.
    pub fn max_bytes(&self) -> u64 {
        self.max_size_mb.saturating_mul(1024 * 1024).max(64 * 1024)
    }
}

/// Validate `[logging]`: an unparseable level (in config or `SOFKA_LOG`) warns
/// and leaves logging off rather than guessing at a verbosity.
pub fn logging_warnings(cfg: &LoggingConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    if crate::applog::Level::parse(&cfg.level).is_none() {
        warnings.push(format!(
            "logging: level '{}' is not off/error/warn/info/debug/trace; logging disabled",
            cfg.level
        ));
    }
    if cfg.file.as_ref().is_some_and(|p| p.as_os_str().is_empty()) {
        warnings.push("logging: file is empty; using the default log path".into());
    }
    warnings
}

/// Options for the `:bundle` diagnostic-bundle export.
///
/// ```toml
/// [bundle]
/// anonymize = true    # replace context/cluster identity with placeholders
/// log_lines = 200     # max recent log lines per pod
/// max_pods = 3        # cap how many pods contribute logs
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BundleConfig {
    /// Replace the context and cluster identity with placeholders (for sharing
    /// a bundle without leaking which cluster it came from).
    pub anonymize: bool,
    /// Maximum recent log lines fetched per pod.
    pub log_lines: i64,
    /// Cap on how many pods contribute logs to a workload's bundle.
    pub max_pods: usize,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            anonymize: false,
            log_lines: 200,
            max_pods: 3,
        }
    }
}

/// Fallback TTL for a PVC-explore helper pod when [`PvcExploreConfig::ttl`] is
/// unreadable. Also the default itself.
pub const PVC_DEFAULT_TTL_SECS: u64 = 1_800;

/// Defaults for the PVC browser (`x` on a PVC, `:pvc-explore`).
///
/// A claim that some running pod already mounts is browsed through that pod
/// and none of this applies. When nothing mounts it, sofka offers to create a
/// short-lived pod that does — these are that pod's image and lifetime.
///
/// ```toml
/// [pvc_explore]
/// image = "busybox:1.37"   # helper-pod image; needs a shell and `ls`
/// ttl = "30m"              # helper pod self-destructs after this
/// ```
///
/// The image needs `sh`, `ls`, and — for transfers, which go through
/// `kubectl cp` — `tar`. busybox has all three.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PvcExploreConfig {
    /// Image for the helper pod.
    pub image: String,
    /// How long the helper pod lives before it deletes itself, as a duration
    /// like `"30m"`. Set on the pod as both a `sleep` and
    /// `activeDeadlineSeconds`, so it expires even if sofka never gets to
    /// delete it. Validated by [`pvc_explore_warnings`].
    pub ttl: String,
}

impl Default for PvcExploreConfig {
    fn default() -> Self {
        Self {
            image: "busybox:1.37".into(),
            ttl: "30m".into(),
        }
    }
}

/// Validate `[pvc_explore]`: an empty image or an unparseable/absurd TTL.
pub fn pvc_explore_warnings(cfg: &PvcExploreConfig) -> Vec<String> {
    let mut out = Vec::new();
    if cfg.image.trim().is_empty() {
        out.push("pvc_explore: image is empty — helper pods cannot be created".into());
    }
    match crate::providers::parse_lookback(&cfg.ttl) {
        Err(e) => out.push(format!("pvc_explore: ttl: {e}; using 30m")),
        Ok(secs) if secs <= 0 => {
            out.push(format!(
                "pvc_explore: ttl {:?} must be positive; using 30m",
                cfg.ttl
            ));
        }
        Ok(_) => {}
    }
    out
}

/// Defaults for `:debug`, which attaches an ephemeral debug container to the
/// selected pod via `kubectl debug`. The image prompt is prefilled with
/// [`Self::image`]; leaving [`Self::command`] empty launches an interactive
/// shell (bash if the debug image has it, else sh), mirroring the pod shell.
///
/// On a **node** (`:debug` on a node row) it instead runs
/// `kubectl debug node/<node>`, which schedules a privileged diagnostic pod
/// that mounts the host filesystem at `/host` and joins the host namespaces —
/// so sofka previews exactly that access and requires confirmation first.
/// Node debuggers sofka launches this session can be removed with
/// `:debug-clean`.
///
/// ```toml
/// [debug]
/// image = "nicolaka/netshoot:latest"   # ephemeral (in-pod) debug image
/// command = ["bash"]                   # entrypoint; omit for a shell
/// node_image = "nicolaka/netshoot:latest"  # node debug pod image
/// node_namespace = "default"           # namespace the node debugger lands in
/// node_profile = "sysadmin"            # kubectl debug --profile (optional)
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    /// Image the `:debug` prompt is prefilled with (ephemeral container).
    pub image: String,
    /// Entrypoint for the ephemeral container. Empty = an interactive shell.
    pub command: Vec<String>,
    /// Image for a node debug pod (`kubectl debug node/<node>`).
    pub node_image: String,
    /// Namespace the node debugger pod is created in (so `:debug-clean` knows
    /// where to find it).
    pub node_namespace: String,
    /// `kubectl debug --profile` for node debuggers (`legacy`, `sysadmin`,
    /// `netadmin`, …). `None` = kubectl's default (`legacy`).
    pub node_profile: Option<String>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        // busybox is tiny and always pulls; a good least-surprise default.
        Self {
            image: "busybox:latest".into(),
            command: Vec::new(),
            node_image: "busybox:latest".into(),
            node_namespace: "default".into(),
            node_profile: None,
        }
    }
}

/// Warning/critical thresholds that decide when a RESTARTS/CPU/MEM cell (or the
/// container picker's request/limit utilization) turns a warning or critical
/// tint. Global `[thresholds]` values apply everywhere; `[thresholds.resources.
/// <key>]` overrides them for one resource kind, keyed like `[views]`
/// (`apiVersion/plural`, `group/plural`, plural, or lowercased kind). Anything
/// left unset keeps sofka's built-in defaults, so an empty config colors
/// exactly as before. Compiled by [`crate::thresholds::compile`].
///
/// ```toml
/// [thresholds]
/// restarts = { warn = 3, critical = 10 }
/// cpu = { warn = "200m", critical = "1" }     # absolute usage
/// memory = { warn = "256Mi", critical = "1Gi" }
/// utilization = { warn = 75, critical = 90 }  # percent of request/limit
///
/// [thresholds.resources.pods]                 # per-kind override
/// restarts = { warn = 5, critical = 20 }
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// Global defaults, applied to every resource.
    #[serde(flatten)]
    pub defaults: ThresholdSet,
    /// Per-resource overrides layered over [`Self::defaults`].
    pub resources: HashMap<String, ThresholdSet>,
}

/// One layer of thresholds: each metric is optional so a partial override only
/// touches the bounds it names.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ThresholdSet {
    /// Summed container restart count (RESTARTS cell).
    pub restarts: Option<CountBand>,
    /// Absolute CPU usage as a Kubernetes quantity (`200m`, `1`).
    pub cpu: Option<QuantityBand>,
    /// Absolute memory usage as a Kubernetes quantity (`256Mi`, `1Gi`).
    pub memory: Option<QuantityBand>,
    /// Usage as a percentage of a container's request/limit.
    pub utilization: Option<CountBand>,
}

/// A numeric warn/critical band (counts, percentages). Either bound may be
/// omitted to disable that level.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct CountBand {
    pub warn: Option<i64>,
    pub critical: Option<i64>,
}

/// A Kubernetes-quantity warn/critical band (CPU/memory), parsed at compile
/// time. Either bound may be omitted to disable that level.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct QuantityBand {
    pub warn: Option<String>,
    pub critical: Option<String>,
}

/// External provider integrations. Sofka stays fully usable without any of
/// these; they add views over data the Kubernetes API doesn't keep. Set them
/// per cluster with override files so each cluster points at its own backend.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Providers {
    /// A log-search backend, queried for the selected object with `L` — see
    /// [`crate::providers`] for the full example.
    pub logs: Option<LogProviderConfig>,
    /// A Prometheus-compatible metrics backend for right-sizing (`:rightsize`).
    pub metrics: Option<MetricsProviderConfig>,
}

/// A Prometheus-compatible historical-metrics backend for `:rightsize`. Works
/// against Prometheus or VictoriaMetrics (same query API). Everything is
/// optional except `type`, including the section: without a `url`, sofka
/// autodiscovers a Prometheus/VictoriaMetrics query service in the cluster and
/// reaches it through the API-server proxy.
///
/// ```toml
/// [providers.metrics]
/// type = "prometheus"          # or "victoriametrics" (same query API)
/// url = "https://prom.example.com"  # omit to autodiscover in-cluster
/// window = "7d"                # lookback for the P50/P95/P99 quantiles
/// step = "5m"                  # subquery resolution for CPU rate()
/// headroom = 15                # percent added over P95 for the suggestion
///
/// [providers.metrics.headers]  # optional: sent with every request
/// Authorization = "Bearer <token>"
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct MetricsProviderConfig {
    /// Backend kind: `"prometheus"` or `"victoriametrics"` (same query API).
    #[serde(rename = "type")]
    pub kind: String,
    /// Base URL, e.g. `https://prom.example.com`. Empty/omitted: autodiscover.
    pub url: String,
    /// Lookback window for the quantiles (`"7d"`, `"24h"`).
    pub window: Option<String>,
    /// Subquery resolution for the CPU `rate()` (`"5m"`).
    pub step: Option<String>,
    /// Percent headroom added over P95 for the suggested request.
    pub headroom: Option<u32>,
    /// Extra HTTP headers, e.g. an Authorization bearer token.
    pub headers: HashMap<String, String>,
}

/// One log backend. Only `type = "victorialogs"` is supported today.
///
/// Everything here is optional except `type` — even the whole section:
/// without one (or without `url`), sofka autodiscovers a VictoriaLogs
/// service in the cluster and reaches it through the API-server proxy.
///
/// ```toml
/// [providers.logs]
/// type = "victorialogs"
/// url = "https://vlogs.example.com"   # omit to autodiscover in-cluster
/// lookback = "1h"          # optional: initial query window (s/m/h/d)
/// limit = 300              # optional: lines fetched by the initial query
///
/// [providers.logs.headers]         # optional: sent with every request
/// Authorization = "Bearer <token>"
///
/// [providers.logs.fields]          # optional: ingested field names
/// namespace = "kubernetes.pod_namespace"
/// pod = "kubernetes.pod_name"
/// container = "kubernetes.container_name"
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct LogProviderConfig {
    /// Backend kind: `"victorialogs"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Base URL of the backend, e.g. `https://vlogs.example.com` or
    /// `http://localhost:9428` (via a port-forward). Empty/omitted:
    /// autodiscover a VictoriaLogs service and use the API-server proxy.
    pub url: String,
    /// How far back the initial query reaches (`"30m"`, `"1h"`, `"2d"`).
    pub lookback: Option<String>,
    /// Number of lines fetched by the initial query.
    pub limit: Option<usize>,
    /// Extra HTTP headers, e.g. an Authorization bearer token.
    pub headers: HashMap<String, String>,
    /// Log-record field names as ingested by the log shipper.
    pub fields: LogProviderFields,
}

/// Field-name mapping for a log backend. Defaults match the vector setup from
/// the VictoriaLogs Kubernetes docs; other shippers name these differently
/// (discover yours via `/select/logsql/stream_field_names`).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct LogProviderFields {
    pub namespace: Option<String>,
    pub pod: Option<String>,
    pub container: Option<String>,
}

/// A custom table view for one resource kind. Keyed in `[views]` by
/// apiVersion/plural (`"cert-manager.io/v1/certificates"`, `"v1/pods"`),
/// group/plural, bare plural, or lowercased kind. Columns overlay the curated
/// defaults (matching headers replace in place, new ones land before AGE)
/// unless `replace = true` swaps them out entirely. `path` is a JSON Pointer
/// (RFC 6901) into the object as served by the API.
///
/// ```toml
/// [views."cert-manager.io/v1/certificates"]
/// sort = "EXPIRES:desc"   # initial sort column, ":asc" (default) or ":desc"
///
/// [[views."cert-manager.io/v1/certificates".columns]]
/// name = "READY"
/// path = "/status/conditions/0/status"
/// type = "status"         # text (default) / status / number / quantity / time
///
/// [[views."cert-manager.io/v1/certificates".columns]]
/// name = "EXPIRES"
/// path = "/status/notAfter"
/// type = "time"
/// wide = true              # only shown in wide mode (`w`)
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ViewConfig {
    /// Initial sort: a column header, optionally suffixed `:asc`/`:desc`.
    pub sort: Option<String>,
    /// Replace the curated columns instead of overlaying them.
    pub replace: bool,
    /// JSON Pointer to the name of the node this kind's objects name
    /// (e.g. `/status/nodeName`), making `enter`/`o` jump to that node.
    pub node: Option<String>,
    /// Where `enter` drills to: another kind, listed under a label selector
    /// built from the selected row. See [`DrillConfig`].
    pub drill: Option<DrillConfig>,
    /// References this kind's objects make to other objects, for the
    /// adjacent view (`u`). See [`RefConfig`].
    pub refs: Vec<RefConfig>,
    /// Kinds this kind's objects own (through `ownerReferences`), scanned for
    /// children by the adjacent view.
    pub children: Vec<String>,
    pub columns: Vec<ViewColumnConfig>,
}

/// One reference in `[[views."…".refs]]`: a field of this kind's objects that
/// names an object of another kind. The adjacent view (`u`) follows it both
/// ways — from a row to what it names, and from a named object back to the
/// rows naming it.
///
/// ```toml
/// [[views."karpenter.sh/v1/nodeclaims".refs]]
/// path = "/spec/nodeClassRef/name"   # JSON Pointer; `*` fans out over an array
/// kind = "ec2nodeclasses"
/// relation = "shaped by"             # row label; default "references"
/// reverse = "cluster"                # usages listed: namespace (default) | cluster | none
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RefConfig {
    pub path: String,
    pub kind: String,
    pub kind_path: Option<String>,
    pub kinds: Vec<String>,
    pub relation: Option<String>,
    pub reverse: Option<String>,
    /// Where the target's namespace lives when it isn't the row's own.
    pub namespace_path: Option<String>,
}

/// The `drill` of a [`ViewConfig`]: `enter` on a row of this kind opens
/// `kind`, filtered by `labels` and/or `fields` with `{name}` and
/// `{namespace}` filled in from the row.
///
/// ```toml
/// [views."karpenter.sh/v1/nodepools"]
/// drill = { kind = "nodeclaims", labels = "karpenter.sh/nodepool={name}" }
///
/// [views.externalsecrets]
/// drill = { kind = "secrets", fields = "metadata.name={name}" }
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct DrillConfig {
    /// Target kind (alias, plural, or kind), resolved against the cluster.
    pub kind: String,
    /// Label selector template; `{name}` and `{namespace}` come from the row.
    pub labels: Option<String>,
    /// Field selector template, same placeholders (e.g. `metadata.name={name}`).
    pub fields: Option<String>,
}

/// One column of a [`ViewConfig`]. Everything is optional at parse time so a
/// half-written column degrades to a validation warning instead of discarding
/// the whole config file.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ViewColumnConfig {
    /// Built-in metric source. Use exactly one of path, metric, or builtin.
    pub metric: Option<String>,
    /// Existing built-in column, such as READY or AGE.
    pub builtin: Option<String>,
    /// Column header (displayed uppercased).
    pub name: String,
    /// JSON Pointer to the cell value, e.g. `/status/phase`.
    pub path: String,
    /// Value type: `text` (default), `status`, `number`, `quantity`, `time`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Path display format: `image-tag` for text, `cpu` or `memory` for quantities.
    pub format: Option<String>,
    /// Only shown in wide mode.
    pub wide: bool,
    /// Fixed display width in columns (defaults to a flexible share).
    pub width: Option<u16>,
    /// Cell alignment: `left` (default), `center`, `right`.
    pub align: Option<String>,
}

/// Skin selection. `name` picks a built-in palette (see
/// [`crate::theme::BUILTIN_NAMES`]); leaving it unset auto-detects the
/// terminal's dark/light mode and picks `catppuccin-mocha`/`catppuccin-latte`
/// accordingly. `colors` overrides individual swatches by name with
/// `#rrggbb` hex values. Naming a skin in a per-cluster/per-context override
/// file swaps it in while that context is active — e.g. a light skin on prod
/// as a visual "careful now" cue. `background` fills every view with the
/// skin's own background swatch instead of leaving the terminal background
/// showing through; combined with a light per-context skin it makes the prod
/// context unmistakably bright.
///
/// ```toml
/// [skin]
/// name = "gruvbox"
/// background = true       # paint the skin's background (default: false)
///
/// [skin.colors]
/// red   = "#fb4934"
/// mauve = "#d3869b"
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Skin {
    pub name: Option<String>,
    pub colors: HashMap<String, String>,
    /// Fill views with the skin's `base` background swatch (default: false,
    /// i.e. transparent — inherit the terminal background).
    pub background: bool,
}

/// A shell-out plugin (k9s-style). Bound to `key` on resources matching
/// `scopes` (plural names; empty = all).
///
/// `key` is a [key chord](crate::keys::KeyChord): a single character (`"g"`),
/// or a modifier combination (`"ctrl-g"`, `"alt-x"`, `"shift-b"`), or a
/// function/named key (`"f5"`, `"ctrl-f2"`). A single lowercase char keeps the
/// original single-key behaviour, so existing configs are unchanged. Built-in
/// keys win over a plugin bound to the same chord.
///
/// Placeholders are substituted in `command`/`args`, each as one argument (no
/// implicit shell): `$NAME`, `$NAMESPACE`/`$NS`, `$CONTEXT`, `$CLUSTER`,
/// `$RESOURCE` (plural), `$GROUP`, `$VERSION`, `$KIND`, and `$FILTER` (the
/// active row filter).
///
/// ```toml
/// [[plugins]]
/// key = "ctrl-g"
/// name = "argocd-sync"
/// command = "argocd"
/// args = ["app", "sync", "$NAME"]
/// scopes = ["deployments"]
/// dangerous = true          # confirm (showing the command) before running
///
/// [[plugins]]
/// key = "shift-y"
/// name = "yaml-summary"
/// command = "kubectl"
/// args = ["get", "$RESOURCE", "$NAME", "-n", "$NAMESPACE", "-o", "yaml"]
/// mutating = false          # read-only: allowed even in --readonly mode
/// output = "popup"          # capture into a scrollable view (not the terminal)
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Plugin {
    /// Inline configuration ignores unknown fields. Package validation rejects them.
    #[serde(flatten)]
    pub(crate) unknown_fields: std::collections::BTreeMap<String, toml::Value>,
    /// Key chord that triggers the plugin (see [`crate::keys::KeyChord`]).
    #[serde(default)]
    pub key: String,
    /// Optional command-palette name, independent of the executable.
    pub palette: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    pub install: Option<String>,
    #[serde(default)]
    pub inputs: std::collections::BTreeMap<String, crate::plugins::Input>,
    /// `context` runs once without requiring a selected row; default `selection`.
    pub target: Option<String>,
    /// Declare traffic generation even when no Kubernetes objects are mutated.
    #[serde(default)]
    pub network_load: bool,
    /// Remote port (or an input placeholder) to forward for the selected pod/service.
    pub port_forward: Option<String>,
    #[serde(skip)]
    pub package_dir: Option<PathBuf>,
    /// Set for a package sofka ships, whose adapter is this binary. Such a
    /// plugin speaks the request/report protocol without a package directory.
    #[serde(skip)]
    pub bundled: bool,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Whether the plugin can modify the cluster. `None`/absent defaults to
    /// `true` — blocked in read-only mode. Set `false` for a known read-only
    /// plugin so it stays available with `--readonly`.
    pub mutating: Option<bool>,
    /// Confirm (showing the exact command) before running.
    #[serde(default)]
    pub confirm: bool,
    /// Mark the plugin dangerous: implies `confirm` and is highlighted red in
    /// the confirmation dialog.
    #[serde(default)]
    pub dangerous: bool,
    /// Run the command through `sh -c` instead of executing the argv directly.
    /// An explicit opt-in into shell interpretation; placeholders are still
    /// passed as separate positional arguments (`$1`, `$2`, …), never spliced
    /// into the script string.
    #[serde(default)]
    pub shell: bool,
    /// Output mode: `terminal` (default; interactive, inherits the terminal),
    /// `popup` (captured into a scrollable view), or `background` (detached,
    /// notifies on completion).
    pub output: Option<String>,
    /// Timeout for `popup`/`background` runs (`"30s"`, `"2m"`). Default 30s.
    /// Ignored for `terminal` (interactive) plugins.
    pub timeout: Option<String>,
}

/// A saved navigation command: jump to a resource (optionally in another
/// context/namespace) with a filter, sort, and view applied in one keystroke.
/// Bound to an optional key chord and always available in the command palette.
///
/// ```toml
/// [[bookmarks]]
/// key = "shift-1"
/// name = "Prod API failures"
/// resource = "pods"
/// context = "prod-eu"          # optional: switch context first
/// namespace = "checkout"       # optional; "all"/"*" = all namespaces
/// filter = "status!=Running -l app=api"   # optional: same syntax as `/`
/// sort = "RESTARTS:desc"       # optional: COLUMN[:asc|:desc]
/// view = "xray"                # optional: xray | pulse
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Bookmark {
    /// Optional key chord (see [`crate::keys::KeyChord`]) to trigger it.
    pub key: Option<String>,
    /// Display name, shown in the palette and help, and used by `:` selection.
    pub name: String,
    /// Resource to open (alias/plural/kind).
    pub resource: String,
    /// Namespace to scope to, or `all`/`*` for all namespaces. Absent keeps
    /// the current namespace.
    pub namespace: Option<String>,
    /// Kubeconfig context to switch to first. Absent stays on the current one.
    pub context: Option<String>,
    /// Row filter to apply, same syntax as the interactive `/` filter.
    pub filter: Option<String>,
    /// Initial sort: a column header, optionally suffixed `:asc`/`:desc`.
    pub sort: Option<String>,
    /// A view to open after landing: `xray` or `pulse`.
    pub view: Option<String>,
}

/// A task-oriented collection of views — checkout operations, a cluster
/// upgrade, certificate renewal — saved as plain config a team can share.
/// Opening a workspace switches to its (optional) context and lands on its
/// first view; `Tab`/`Shift-Tab` cycle through the rest without leaving it.
///
/// ```toml
/// [[workspaces]]
/// key = "ctrl-w"
/// name = "Checkout ops"
/// context = "prod-eu"          # optional: switched once on open
///
/// [[workspaces.views]]
/// name = "API pods"
/// resource = "pods"
/// namespace = "checkout"
/// filter = "-l app=api"
/// sort = "RESTARTS:desc"
///
/// [[workspaces.views]]
/// name = "Ingress"
/// resource = "ingresses"
/// namespace = "checkout"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Workspace {
    /// Optional key chord (see [`crate::keys::KeyChord`]) to open it.
    pub key: Option<String>,
    pub name: String,
    /// Kubeconfig context, switched to once when the workspace opens.
    pub context: Option<String>,
    pub views: Vec<WorkspaceView>,
}

/// One view within a [`Workspace`]: like a [`Bookmark`] without its own key or
/// context (the workspace owns those).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceView {
    pub name: String,
    pub resource: String,
    pub namespace: Option<String>,
    pub filter: Option<String>,
    pub sort: Option<String>,
    pub view: Option<String>,
}

impl WorkspaceView {
    /// The equivalent [`Bookmark`] (no key/context), so the app can apply a
    /// workspace view through the same path as a bookmark.
    pub fn as_bookmark(&self) -> Bookmark {
        Bookmark {
            key: None,
            name: self.name.clone(),
            resource: self.resource.clone(),
            namespace: self.namespace.clone(),
            context: None,
            filter: self.filter.clone(),
            sort: self.sort.clone(),
            view: self.view.clone(),
        }
    }
}

/// Validation warnings for workspaces: an unparseable key chord, a missing
/// name, no views, or a bad view (missing resource / unknown view kind).
pub fn workspace_warnings(workspaces: &[Workspace]) -> Vec<String> {
    let mut warns = Vec::new();
    for w in workspaces {
        let label = if w.name.is_empty() {
            "<unnamed>"
        } else {
            &w.name
        };
        if w.name.trim().is_empty() {
            warns.push("workspace: missing `name`".into());
        }
        if let Some(k) = &w.key
            && let Err(e) = crate::keys::KeyChord::parse(k)
        {
            warns.push(format!("workspace {label:?}: invalid key — {e}"));
        }
        if w.views.is_empty() {
            warns.push(format!("workspace {label:?}: no [[workspaces.views]]"));
        }
        for v in &w.views {
            if v.resource.trim().is_empty() {
                warns.push(format!("workspace {label:?}: a view is missing `resource`"));
            }
            if let Some(kind) = &v.view
                && !matches!(kind.as_str(), "xray" | "pulse")
            {
                warns.push(format!(
                    "workspace {label:?}: unknown view {kind:?} (expected xray/pulse) — ignored"
                ));
            }
        }
    }
    warns
}

/// A declarative safety policy: match dangerous actions by context, namespace,
/// resource, and action, then require extra confirmation, deny them, or cap a
/// bulk selection. Gates `delete`, `force-delete`, `drain`, `restart`,
/// `shell`, `debug`, `node-debug`, `transfer`, `pvc-explore`, and
/// `pvc-upload` today. Empty match lists mean "any"; glob `*` supported.
///
/// ```toml
/// [[guardrails]]
/// contexts = ["prod-*"]
/// namespaces = ["kube-system", "payments"]
/// actions = ["delete", "force-delete", "drain"]
/// confirmation = "type-resource-name"   # confirm | type-resource-name | type-context-name
///
/// [[guardrails]]
/// contexts = ["prod-*"]
/// actions = ["force-delete", "drain", "shell"]
/// deny = true
/// reason = "not allowed on prod — use a break-glass context"
///
/// [[guardrails]]
/// actions = ["delete"]
/// max_bulk = 10          # refuse a marked delete of more than 10 at once
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Guardrail {
    /// Kubeconfig contexts this applies to (globs). Empty = any.
    pub contexts: Vec<String>,
    /// Namespaces this applies to (globs). Empty = any.
    pub namespaces: Vec<String>,
    /// Resource plurals/kinds this applies to (globs). Empty = any.
    pub resources: Vec<String>,
    /// Actions this applies to: `delete`, `force-delete`, `drain`, `restart`,
    /// `shell`, `debug`, `node-debug`, `transfer`, `pvc-explore` (creating or
    /// sweeping a PVC-explore helper pod), `pvc-upload`. Empty = any.
    pub actions: Vec<String>,
    /// Block the action outright.
    pub deny: bool,
    /// Extra confirmation required: `confirm`, `type-resource-name`, or
    /// `type-context-name`.
    pub confirmation: Option<String>,
    /// Maximum number of targets a single (bulk) action may touch.
    pub max_bulk: Option<usize>,
    /// Human note shown when the guardrail blocks or confirms.
    pub reason: Option<String>,
}

/// Upper bound for `mouse_scroll_lines`; each step is a full input dispatch,
/// so a huge value would stall the UI on a single wheel event.
pub const MAX_MOUSE_SCROLL_LINES: u16 = 100;

/// Resolves `mouse_scroll_lines`: unset means 3, `0` is treated as 1 and values
/// above [`MAX_MOUSE_SCROLL_LINES`] are capped, each with a warning pushed onto
/// `warnings`.
pub fn mouse_scroll_lines(value: Option<u16>, warnings: &mut Vec<String>) -> u16 {
    match value {
        None => 3,
        Some(0) => {
            warnings.push("mouse_scroll_lines: 0 is not allowed — using 1".into());
            1
        }
        Some(n) if n > MAX_MOUSE_SCROLL_LINES => {
            warnings.push(format!(
                "mouse_scroll_lines: {n} is above the maximum of {MAX_MOUSE_SCROLL_LINES} — using {MAX_MOUSE_SCROLL_LINES}"
            ));
            MAX_MOUSE_SCROLL_LINES
        }
        Some(n) => n,
    }
}

/// Validation warnings for guardrails: an unknown `confirmation` mode.
pub fn guardrail_warnings(guardrails: &[Guardrail]) -> Vec<String> {
    let mut warns = Vec::new();
    for (i, g) in guardrails.iter().enumerate() {
        if let Some(c) = &g.confirmation
            && !matches!(
                c.as_str(),
                "confirm" | "type-resource-name" | "type-context-name"
            )
        {
            warns.push(format!(
                "guardrail #{}: unknown confirmation {c:?} (expected confirm/type-resource-name/type-context-name) — treated as confirm",
                i + 1
            ));
        }
    }
    warns
}

/// Validation warnings for bookmarks: an unparseable key chord, a missing
/// name/resource, or an unknown `view`. A broken bookmark is skipped, not fatal.
pub fn bookmark_warnings(bookmarks: &[Bookmark]) -> Vec<String> {
    let mut warns = Vec::new();
    for b in bookmarks {
        let label = if b.name.is_empty() {
            "<unnamed>"
        } else {
            &b.name
        };
        if b.name.trim().is_empty() {
            warns.push("bookmark: missing `name`".into());
        }
        if b.resource.trim().is_empty() {
            warns.push(format!("bookmark {label:?}: missing `resource`"));
        }
        if let Some(k) = &b.key
            && let Err(e) = crate::keys::KeyChord::parse(k)
        {
            warns.push(format!("bookmark {label:?}: invalid key — {e}"));
        }
        if let Some(v) = &b.view
            && !matches!(v.as_str(), "xray" | "pulse")
        {
            warns.push(format!(
                "bookmark {label:?}: unknown view {v:?} (expected xray/pulse) — ignored"
            ));
        }
    }
    warns
}

/// Validation warnings for plugins: an unparseable key chord (see
/// [`crate::keys::KeyChord::parse`]), an unknown `output` mode, or a malformed
/// `timeout`. A bad chord disables just that plugin; a bad output/timeout
/// falls back to the default. Reported, never fatal.
pub fn plugin_warnings(plugins: &[Plugin]) -> Vec<String> {
    let mut warns = Vec::new();
    for p in plugins {
        if !(p.key.is_empty() && p.palette.is_some())
            && let Err(e) = crate::keys::KeyChord::parse(&p.key)
        {
            warns.push(format!("plugin {:?}: invalid key — {e}", p.name));
        }
        if let Some(o) = &p.output
            && !matches!(o.as_str(), "terminal" | "popup" | "background" | "report")
        {
            warns.push(format!(
                "plugin {:?}: unknown output {o:?} (expected terminal/popup/background/report) — using terminal",
                p.name
            ));
        }
        if let Some(t) = &p.timeout
            && let Err(e) = crate::providers::parse_lookback(t)
        {
            warns.push(format!("plugin {:?}: timeout: {e} — using 30s", p.name));
        }
    }
    warns
}

/// Config resolved for one (cluster, context) pair: the base config with any
/// matching override files merged in.
pub struct Resolved {
    pub config: Config,
    /// `skin.name` when an override file (not the base config) set it. Wins
    /// over the session skin while the context is active, exactly so that a
    /// manual `:skin` choice still survives switches into contexts that don't
    /// pin their own skin.
    pub skin_override: Option<String>,
    /// Problems with override files (syntax errors, type mismatches). The
    /// offending layer is skipped, never fatal.
    pub warnings: Vec<String>,
}

/// Holds the parsed base config for the session and re-reads override
/// files on demand, so `:ctx` switches pick up freshly edited overrides
/// without a restart.
#[derive(Default)]
pub struct ConfigLoader {
    /// Base settings as a shared table (`None` when missing or invalid).
    base: Option<toml::Value>,
    base_file: Option<PathBuf>,
    /// The `sofka` directory that contains the base config and `clusters/`.
    dir: Option<PathBuf>,
}

impl ConfigLoader {
    /// Read the base config, warning on stderr (we're pre-TUI) and falling
    /// back to defaults if it's malformed — syntax or types. The validation
    /// errors are also returned so the TUI can keep showing them (`:config`).
    pub fn load() -> (Self, Vec<String>) {
        let dir = config_dir();
        let empty = Self {
            base: None,
            base_file: None,
            dir: dir.clone(),
        };
        match empty.reload() {
            Ok(loader) => (loader, Vec::new()),
            Err(e) => {
                eprintln!("warning: ignoring invalid {e}");
                (
                    Self {
                        base: None,
                        base_file: None,
                        dir,
                    },
                    vec![e],
                )
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn from_dir(dir: Option<PathBuf>) -> Self {
        let base_file = dir.as_ref().and_then(|d| document::select(d).ok());
        let base = base_file
            .as_ref()
            .and_then(|path| read_value(path).ok().flatten());
        Self {
            base,
            base_file,
            dir,
        }
    }

    /// Read and validate the base config. On error, the caller keeps the last valid loader.
    pub fn reload(&self) -> Result<Self, String> {
        let dir = self.dir.clone();
        let Some(directory) = &dir else {
            return Ok(Self::default());
        };
        let path = document::select(directory)?;
        let base = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", path.display())),
            Ok(text) => {
                Some(validate_file(&path, &text).map_err(|e| format!("{}: {e}", path.display()))?)
            }
        };
        Ok(Self {
            base,
            base_file: Some(path),
            dir,
        })
    }

    /// Path used by the cached base config, or the first candidate before loading.
    pub fn base_path(&self) -> Option<PathBuf> {
        self.base_file
            .clone()
            .or_else(|| self.base_paths().into_iter().next())
    }

    pub fn base_paths(&self) -> Vec<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| document::paths(d))
            .unwrap_or_default()
    }

    /// Whether a parsed base config is active (as opposed to a missing or
    /// invalid file, both of which fall back to defaults).
    pub fn has_base(&self) -> bool {
        self.base.is_some()
    }

    pub fn dropin_paths(&self) -> Vec<PathBuf> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir.join("conf.d")) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("toml" | "yaml" | "yml")
                ) && path.is_file()
            })
            .collect();
        paths.sort();
        paths
    }

    /// Override files consulted for the given kubeconfig cluster/context, in
    /// merge order (cluster level first, then context level). The files need
    /// not exist — this is the search path, for [`resolve`](Self::resolve)
    /// and the `:config` view.
    pub fn override_paths(&self, context: &str, cluster: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let (Some(dir), false) = (&self.dir, cluster.is_empty()) {
            let cluster_dir = dir.join("clusters").join(sanitize(cluster));
            paths.extend(document::paths(&cluster_dir));
            if !context.is_empty() {
                paths.extend(document::paths(&cluster_dir.join(sanitize(context))));
            }
        }
        paths
    }

    /// Merge override files for the given kubeconfig cluster/context over the
    /// base config. Either name may be empty (e.g. in-cluster, no kubeconfig
    /// context) — its level is simply skipped.
    pub fn resolve(&self, context: &str, cluster: &str) -> Resolved {
        let mut warnings = Vec::new();
        let mut merged = self
            .base
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        let mut overlay = toml::Value::Table(toml::map::Map::new());
        let mut migrations = Vec::new();
        if let Some(path) = self.base_path()
            && let Some(migration) = key_migration::prepare(&mut merged, &path, &mut warnings)
        {
            migrations.push(migration);
        }
        let base = merged.clone();

        for path in self.dropin_paths() {
            match read_dropin(&path) {
                Ok(Some(mut v)) => {
                    if let Some(migration) = key_migration::prepare(&mut v, &path, &mut warnings) {
                        migrations.push(migration);
                    }
                    merge(&mut merged, v);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("ignoring invalid {}: {e}", path.display())),
            }
        }

        for path in self.override_paths(context, cluster) {
            match read_value(&path) {
                Ok(Some(mut v)) => {
                    if let Some(migration) = key_migration::prepare(&mut v, &path, &mut warnings) {
                        migrations.push(migration);
                    }
                    merge(&mut merged, v.clone());
                    merge(&mut overlay, v);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("ignoring invalid {}: {e}", path.display())),
            }
        }

        // A type mismatch introduced by an override drops back to the base
        // config (validated at load time) rather than losing everything.
        let mut valid_config = true;
        let mut config: Config = merged.try_into().unwrap_or_else(|e| {
            valid_config = false;
            warnings.push(format!("ignoring drop-in and cluster overrides: {e}"));
            base.try_into().unwrap_or_default()
        });
        if !migrations.is_empty() {
            key_migration::finish(
                migrations,
                valid_config && crate::keymap::Keymap::compile(&config.keys).is_ok(),
                &mut warnings,
            );
        }
        if let Some(dir) = &self.dir {
            crate::plugins::load_packages(&dir.join("plugins"), &mut config.plugins, &mut warnings);
        }
        // Last, so an inline entry or a user package of the same name wins and
        // a user can replace a shipped plugin without editing sofka.
        for bundled in crate::plugins::bundled() {
            match bundled {
                Ok(p) => {
                    if !config.plugins.iter().any(|old| {
                        old.name == p.name || (p.palette.is_some() && old.palette == p.palette)
                    }) {
                        config.plugins.push(p);
                    }
                }
                Err(e) => warnings.push(e),
            }
        }
        let skin_override = overlay
            .get("skin")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from);
        Resolved {
            config,
            skin_override,
            warnings,
        }
    }
}

/// Recursively merge `overlay` into `base`: tables merge key-by-key, any
/// other value (scalar or array) replaces the base one wholesale.
fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) if slot.is_table() && v.is_table() => merge(slot, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// Validate a config document end-to-end — TOML syntax and the typed
/// [`Config`] shape — returning the raw table (kept raw for later override
/// merging) only when both pass. The typed pass is what turns e.g.
/// `readonly = "yes"` into a precise "expected a boolean" error pointing at
/// the offending line instead of silently loading defaults.
fn validate(text: &str) -> Result<toml::Value, toml::de::Error> {
    toml::from_str::<Config>(text)?;
    parse_doc(text)
}

/// Human-readable state of one config file, for the `:config` view:
/// `loaded` (present and parseable), `absent`, or `invalid` (present but
/// malformed config).
pub fn file_state(path: &Path) -> &'static str {
    state_of(read_value(path))
}

pub fn dropin_state(path: &Path) -> &'static str {
    state_of(read_dropin(path))
}

fn state_of(result: Result<Option<toml::Value>, String>) -> &'static str {
    match result {
        Ok(Some(_)) => "loaded",
        Ok(None) => "absent",
        Err(_) => "invalid - skipped",
    }
}

/// Read an optional override file. Report conflicts, read errors, and parse errors.
fn read_value(path: &Path) -> Result<Option<toml::Value>, String> {
    if let Some(dir) = path.parent() {
        document::select(dir)?;
    }
    read_file(path)
}

fn read_dropin(path: &Path) -> Result<Option<toml::Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => validate_file(path, &text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn read_file(path: &Path) -> Result<Option<toml::Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => document::parse(path, &text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn validate_file(path: &Path, text: &str) -> Result<toml::Value, String> {
    if !document::is_yaml(path) {
        return validate(text).map_err(|e| e.to_string());
    }
    let value = document::parse(path, text)?;
    let _: Config = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| e.to_string())?;
    Ok(value)
}

/// Parse a TOML *document* into a `Value::Table` (a bare `Value` parse would
/// expect a single TOML value, not a document).
fn parse_doc(text: &str) -> Result<toml::Value, toml::de::Error> {
    text.parse::<toml::Table>().map(toml::Value::Table)
}

/// Map a kubeconfig cluster/context name onto a safe directory name: any
/// character outside `[A-Za-z0-9._-]` becomes `-` (EKS ARNs contain `:` and
/// `/`). All-dot results (`.`, `..`) would be path navigation, not names.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.chars().all(|c| c == '.') {
        "-".repeat(s.len().max(1))
    } else {
        s
    }
}

fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sofka"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_config_format_combination_uses_the_same_merge_rules() {
        let dir = std::env::temp_dir().join(format!("sofka-format-matrix-{}", std::process::id()));
        for base in ["toml", "yaml", "yml"] {
            for cluster in ["toml", "yaml", "yml"] {
                for context in ["toml", "yaml", "yml"] {
                    let cluster_dir = dir.join("clusters/prod");
                    let context_dir = cluster_dir.join("admin");
                    std::fs::create_dir_all(&context_dir).unwrap();
                    for (directory, extension, text) in [
                        (
                            &dir,
                            base,
                            "readonly = true\nfavorite_namespaces = ['base']\n[aliases]\npo = 'pods'\n[skin]\nname = 'nord'\n[keys.command]\ndown = ['f7', 'f8']\n",
                        ),
                        (
                            &cluster_dir,
                            cluster,
                            "favorite_namespaces = ['cluster']\n[aliases]\ndep = 'deployments'\n[skin]\nbackground = true\n",
                        ),
                        (
                            &context_dir,
                            context,
                            "readonly = false\nfavorite_namespaces = []\n[keys.command]\ndown = ['f9']\n",
                        ),
                    ] {
                        let text = if extension == "toml" {
                            text.into()
                        } else {
                            serde_yaml::to_string(&parse_doc(text).unwrap()).unwrap()
                        };
                        std::fs::write(directory.join(format!("config.{extension}")), text)
                            .unwrap();
                    }
                    let loader = ConfigLoader {
                        dir: Some(dir.clone()),
                        ..Default::default()
                    }
                    .reload()
                    .unwrap();
                    let resolved = loader.resolve("admin", "prod");
                    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
                    assert!(!resolved.config.readonly);
                    assert!(resolved.config.favorite_namespaces.is_empty());
                    assert_eq!(resolved.config.aliases.len(), 2);
                    assert_eq!(resolved.config.skin.name.as_deref(), Some("nord"));
                    assert!(resolved.config.skin.background);
                    assert_eq!(
                        resolved.config.keys.0["command"]["down"]
                            .as_array()
                            .unwrap(),
                        &[toml::Value::String("f9".into())]
                    );
                    assert_eq!(loader.base_path(), Some(dir.join(format!("config.{base}"))));
                    std::fs::remove_dir_all(&dir).unwrap();
                }
            }
        }
    }

    #[test]
    fn yaml_conflicts_and_invalid_overrides_report_the_source() {
        let dir = std::env::temp_dir().join(format!("sofka-yaml-invalid-{}", std::process::id()));
        let cluster_dir = dir.join("clusters/prod");
        std::fs::create_dir_all(&cluster_dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "readonly: true\n").unwrap();
        let loader = ConfigLoader {
            dir: Some(dir.clone()),
            ..Default::default()
        }
        .reload()
        .unwrap();
        for text in ["readonly: [", "readonly: 'yes'"] {
            std::fs::write(cluster_dir.join("config.yml"), text).unwrap();
            let resolved = loader.resolve("admin", "prod");
            assert!(resolved.config.readonly);
            assert!(!resolved.warnings.is_empty());
        }
        std::fs::write(dir.join("config.yml"), "readonly: false\n").unwrap();
        let error = loader.reload().err().unwrap();
        assert!(error.contains("conflicting config files"));
        assert!(error.contains("config.yaml") && error.contains("config.yml"));
        assert!(loader.resolve("", "").config.readonly);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn journal_config_defaults_and_overrides() {
        let cfg = Config::default();
        assert!(!cfg.journal.enabled);
        assert_eq!(
            cfg.journal.path(),
            crate::diagnostics::state_dir().join("journal.jsonl")
        );
        assert_eq!(cfg.journal.max_bytes(), 8 * 1024 * 1024);
        let cfg: Config =
            toml::from_str("[journal]\nenabled = true\nfile = 'actions.jsonl'\nmax_size_mb = 0")
                .unwrap();
        assert!(cfg.journal.enabled);
        assert_eq!(cfg.journal.path(), PathBuf::from("actions.jsonl"));
        assert_eq!(cfg.journal.max_bytes(), 64 * 1024);
    }

    #[test]
    fn compact_mode_defaults_false_and_accepts_booleans() {
        assert!(!Config::default().compact_mode);
        for (text, expected) in [
            ("", false),
            ("compact_mode = false", false),
            ("compact_mode = true", true),
        ] {
            let cfg: Config = toml::from_str(text).unwrap();
            assert_eq!(cfg.compact_mode, expected);
        }
        assert!(toml::from_str::<Config>("compact_mode = 'true'").is_err());
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            default_namespace = "kube-system"
            default_resource  = "deployments"

            [aliases]
            dep = "deployments"

            [[plugins]]
            key = "g"
            name = "argocd-sync"
            command = "argocd"
            args = ["app", "sync", "$NAME"]
            scopes = ["deployments"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.default_namespace.as_deref(), Some("kube-system"));
        assert_eq!(
            cfg.aliases.get("dep").map(String::as_str),
            Some("deployments")
        );
        assert_eq!(cfg.plugins.len(), 1);
        let p = &cfg.plugins[0];
        assert_eq!(p.key, "g");
        assert_eq!(p.args, vec!["app", "sync", "$NAME"]);
        assert_eq!(p.scopes, vec!["deployments"]);
    }

    #[test]
    fn parses_rich_plugin_fields() {
        let toml = r#"
            [[plugins]]
            key = "shift-y"
            name = "yaml"
            command = "kubectl"
            args = ["get", "$RESOURCE", "$NAME"]
            mutating = false
            dangerous = true
            shell = true
            output = "popup"
            timeout = "45s"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let p = &cfg.plugins[0];
        assert_eq!(p.mutating, Some(false));
        assert!(p.dangerous && p.shell);
        assert_eq!(p.output.as_deref(), Some("popup"));
        assert_eq!(p.timeout.as_deref(), Some("45s"));
        assert!(plugin_warnings(&cfg.plugins).is_empty());

        // Legacy minimal plugin still parses; new fields default off.
        let cfg: Config =
            toml::from_str("[[plugins]]\nkey = \"g\"\nname = \"x\"\ncommand = \"true\"").unwrap();
        let p = &cfg.plugins[0];
        assert_eq!(p.mutating, None);
        assert!(!p.confirm && !p.dangerous && !p.shell);
        assert!(p.output.is_none());
    }

    #[test]
    fn parses_bookmarks_and_flags_problems() {
        let cfg: Config = toml::from_str(
            r#"
            [[bookmarks]]
            key = "shift-1"
            name = "Prod API failures"
            resource = "pods"
            context = "prod-eu"
            namespace = "checkout"
            filter = "status!=Running -l app=api"
            sort = "RESTARTS:desc"
            view = "xray"
        "#,
        )
        .unwrap();
        let b = &cfg.bookmarks[0];
        assert_eq!(b.name, "Prod API failures");
        assert_eq!(b.resource, "pods");
        assert_eq!(b.context.as_deref(), Some("prod-eu"));
        assert_eq!(b.namespace.as_deref(), Some("checkout"));
        assert_eq!(b.sort.as_deref(), Some("RESTARTS:desc"));
        assert_eq!(b.view.as_deref(), Some("xray"));
        assert!(bookmark_warnings(&cfg.bookmarks).is_empty());

        let bad: Config = toml::from_str(
            r#"
            [[bookmarks]]
            name = ""
            resource = ""
            key = "hyper-x"
            view = "sidebar"
        "#,
        )
        .unwrap();
        let w = bookmark_warnings(&bad.bookmarks);
        assert!(w.iter().any(|s| s.contains("missing `name`")), "{w:?}");
        assert!(w.iter().any(|s| s.contains("missing `resource`")), "{w:?}");
        assert!(w.iter().any(|s| s.contains("invalid key")), "{w:?}");
        assert!(w.iter().any(|s| s.contains("unknown view")), "{w:?}");
    }

    #[test]
    fn parses_workspaces_and_flags_problems() {
        let cfg: Config = toml::from_str(
            r#"
            [[workspaces]]
            key = "ctrl-w"
            name = "Checkout ops"
            context = "prod-eu"

            [[workspaces.views]]
            name = "API pods"
            resource = "pods"
            namespace = "checkout"
            filter = "-l app=api"
            sort = "RESTARTS:desc"

            [[workspaces.views]]
            name = "Ingress"
            resource = "ingresses"
        "#,
        )
        .unwrap();
        let w = &cfg.workspaces[0];
        assert_eq!(w.name, "Checkout ops");
        assert_eq!(w.context.as_deref(), Some("prod-eu"));
        assert_eq!(w.views.len(), 2);
        assert_eq!(w.views[0].resource, "pods");
        assert!(workspace_warnings(&cfg.workspaces).is_empty());

        let bad: Config = toml::from_str(
            r#"
            [[workspaces]]
            name = ""
            key = "hyper-x"
        "#,
        )
        .unwrap();
        let warns = workspace_warnings(&bad.workspaces);
        assert!(
            warns.iter().any(|s| s.contains("missing `name`")),
            "{warns:?}"
        );
        assert!(warns.iter().any(|s| s.contains("invalid key")), "{warns:?}");
        assert!(
            warns.iter().any(|s| s.contains("no [[workspaces.views]]")),
            "{warns:?}"
        );
    }

    #[test]
    fn plugin_warnings_flag_bad_output_and_timeout() {
        let cfg: Config = toml::from_str(
            r#"
            [[plugins]]
            key = "ctrl-x"
            name = "bad"
            command = "true"
            output = "sidebar"
            timeout = "soon"
        "#,
        )
        .unwrap();
        let w = plugin_warnings(&cfg.plugins);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w.iter().any(|s| s.contains("unknown output")));
        assert!(w.iter().any(|s| s.contains("timeout")));
    }

    #[test]
    fn parses_favorite_namespaces() {
        let cfg: Config =
            toml::from_str("favorite_namespaces = [\"kube-system\", \"monitoring\"]").unwrap();
        assert_eq!(cfg.favorite_namespaces, vec!["kube-system", "monitoring"]);
        let empty: Config = toml::from_str("").unwrap();
        assert!(empty.favorite_namespaces.is_empty());
    }

    #[test]
    fn empty_config_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.aliases.is_empty());
        assert!(cfg.plugins.is_empty());
        assert!(cfg.default_resource.is_none());
        assert!(cfg.providers.logs.is_none());
    }

    #[test]
    fn logging_defaults_to_off_under_the_state_dir() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.logging.level, "off");
        assert!(logging_warnings(&cfg.logging).is_empty());
        assert_eq!(cfg.logging.path(), crate::diagnostics::default_log_path());
        assert_eq!(cfg.logging.max_bytes(), 8 * 1024 * 1024);
    }

    #[test]
    fn parses_logging_section() {
        let toml = r#"
            [logging]
            level = "debug"
            file = "/tmp/sofka-test.log"
            max_size_mb = 2
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.logging.level, "debug");
        assert_eq!(cfg.logging.path(), PathBuf::from("/tmp/sofka-test.log"));
        assert_eq!(cfg.logging.max_bytes(), 2 * 1024 * 1024);
        assert!(logging_warnings(&cfg.logging).is_empty());
    }

    #[test]
    fn logging_bad_level_warns_and_empty_file_falls_back() {
        let cfg: Config = toml::from_str("[logging]\nlevel = \"chatty\"\nfile = \"\"\n").unwrap();
        let warnings = logging_warnings(&cfg.logging);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("chatty"), "{warnings:?}");
        assert!(warnings[1].contains("file is empty"), "{warnings:?}");
        assert_eq!(cfg.logging.path(), crate::diagnostics::default_log_path());
    }

    #[test]
    fn logging_rotation_floor_survives_a_zero() {
        let cfg: Config = toml::from_str("[logging]\nmax_size_mb = 0\n").unwrap();
        assert_eq!(cfg.logging.max_bytes(), 64 * 1024);
    }

    #[test]
    fn parses_providers_section() {
        let toml = r#"
            [providers.logs]
            type = "victorialogs"
            url = "https://vlogs.example.com"
            lookback = "2h"
            limit = 500

            [providers.logs.headers]
            Authorization = "Bearer token"

            [providers.logs.fields]
            pod = "pod_name"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let logs = cfg.providers.logs.unwrap();
        assert_eq!(logs.kind, "victorialogs");
        assert_eq!(logs.url, "https://vlogs.example.com");
        assert_eq!(logs.lookback.as_deref(), Some("2h"));
        assert_eq!(logs.limit, Some(500));
        assert_eq!(
            logs.headers.get("Authorization").map(String::as_str),
            Some("Bearer token")
        );
        assert_eq!(logs.fields.pod.as_deref(), Some("pod_name"));
        assert!(logs.fields.namespace.is_none());
    }

    #[test]
    fn parses_thresholds_section() {
        let toml = r#"
            [thresholds]
            restarts = { warn = 3, critical = 10 }
            cpu = { warn = "200m", critical = "1" }
            memory = { warn = "256Mi", critical = "1Gi" }
            utilization = { critical = 95 }

            [thresholds.resources.pods]
            restarts = { warn = 5, critical = 20 }
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let t = &cfg.thresholds;
        let d = &t.defaults;
        assert_eq!(d.restarts.unwrap().warn, Some(3));
        assert_eq!(d.restarts.unwrap().critical, Some(10));
        assert_eq!(d.cpu.as_ref().unwrap().warn.as_deref(), Some("200m"));
        assert_eq!(d.memory.as_ref().unwrap().critical.as_deref(), Some("1Gi"));
        assert_eq!(d.utilization.unwrap().warn, None);
        assert_eq!(d.utilization.unwrap().critical, Some(95));
        let pods = t.resources.get("pods").unwrap();
        assert_eq!(pods.restarts.unwrap().warn, Some(5));
        assert_eq!(pods.restarts.unwrap().critical, Some(20));
    }

    #[test]
    fn empty_config_has_default_thresholds() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.thresholds.defaults.restarts.is_none());
        assert!(cfg.thresholds.resources.is_empty());
    }

    fn val(s: &str) -> toml::Value {
        parse_doc(s).unwrap()
    }

    #[test]
    fn merge_tables_key_by_key_scalars_and_arrays_replace() {
        let mut base = val(r##"
            default_namespace = "default"
            default_resource = "pods"

            [aliases]
            dep = "deployments"

            [[plugins]]
            key = "g"
            name = "base-plugin"
            command = "true"

            [skin]
            name = "gruvbox-dark"
            [skin.colors]
            red = "#ff0000"
        "##);
        merge(
            &mut base,
            val(r##"
                default_namespace = "kube-system"

                [aliases]
                ks = "kustomizations"

                [[plugins]]
                key = "h"
                name = "override-plugin"
                command = "false"

                [skin]
                name = "catppuccin-latte"
                [skin.colors]
                blue = "#0000ff"
            "##),
        );
        let cfg: Config = base.try_into().unwrap();
        // scalars replace
        assert_eq!(cfg.default_namespace.as_deref(), Some("kube-system"));
        assert_eq!(cfg.skin.name.as_deref(), Some("catppuccin-latte"));
        // untouched base values survive
        assert_eq!(cfg.default_resource.as_deref(), Some("pods"));
        // tables merge
        assert_eq!(cfg.aliases.len(), 2);
        assert_eq!(cfg.skin.colors.len(), 2);
        // arrays replace wholesale
        assert_eq!(cfg.plugins.len(), 1);
        assert_eq!(cfg.plugins[0].name, "override-plugin");
    }

    #[test]
    fn sanitize_maps_unsafe_chars_to_dashes() {
        assert_eq!(sanitize("prod-cluster_1.io"), "prod-cluster_1.io");
        assert_eq!(
            sanitize("arn:aws:eks:eu-west-1:123:cluster/prod"),
            "arn-aws-eks-eu-west-1-123-cluster-prod"
        );
        assert_eq!(sanitize(".."), "--");
        assert_eq!(sanitize(""), "-");
    }

    /// End-to-end: base + cluster override + context override on disk.
    #[test]
    fn resolves_cluster_and_context_overrides() {
        let dir = std::env::temp_dir().join(format!("sofka-cfg-test-{}", std::process::id()));
        let ctx_dir = dir.join("clusters").join("prod-cluster").join("prod-ctx");
        std::fs::create_dir_all(&ctx_dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "default_namespace = \"default\"\n[aliases]\ndep = \"deployments\"\n[skin]\nname = \"gruvbox-dark\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("clusters")
                .join("prod-cluster")
                .join("config.toml"),
            "readonly = true\n[skin]\nname = \"catppuccin-latte\"\nbackground = true\n",
        )
        .unwrap();
        std::fs::write(
            ctx_dir.join("config.toml"),
            "default_namespace = \"prod\"\n[aliases]\nks = \"kustomizations\"\n",
        )
        .unwrap();

        let loader = ConfigLoader::from_dir(Some(dir.clone()));

        // Unknown context/cluster: base only, no skin override.
        let plain = loader.resolve("dev-ctx", "dev-cluster");
        assert!(plain.warnings.is_empty());
        assert_eq!(plain.config.default_namespace.as_deref(), Some("default"));
        assert_eq!(plain.config.skin.name.as_deref(), Some("gruvbox-dark"));
        assert_eq!(plain.skin_override, None);
        assert!(!plain.config.readonly);

        // Cluster + context overrides stack over the base.
        let prod = loader.resolve("prod-ctx", "prod-cluster");
        assert!(prod.warnings.is_empty());
        assert_eq!(prod.config.default_namespace.as_deref(), Some("prod"));
        assert!(prod.config.skin.background);
        assert_eq!(prod.config.aliases.len(), 2);
        assert_eq!(prod.skin_override.as_deref(), Some("catppuccin-latte"));
        assert!(prod.config.readonly);

        // Empty cluster name (in-cluster): overrides skipped entirely.
        let bare = loader.resolve("prod-ctx", "");
        assert_eq!(bare.config.default_namespace.as_deref(), Some("default"));
        assert_eq!(bare.skin_override, None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reload_swaps_base_and_rejects_invalid_with_precise_errors() {
        let dir =
            std::env::temp_dir().join(format!("sofka-cfg-reload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        std::fs::write(&path, "default_namespace = \"one\"\n").unwrap();
        let loader = ConfigLoader::from_dir(Some(dir.clone()));
        assert!(loader.has_base());
        assert_eq!(loader.base_path(), Some(path.clone()));

        // A valid edit on disk is picked up.
        std::fs::write(&path, "default_namespace = \"two\"\n").unwrap();
        let loader = loader.reload().unwrap();
        let r = loader.resolve("", "");
        assert_eq!(r.config.default_namespace.as_deref(), Some("two"));

        // A type error is rejected, naming the file and the offending key.
        std::fs::write(&path, "readonly = \"yes\"\n").unwrap();
        let err = loader.reload().err().unwrap();
        assert!(err.contains("config.toml"), "{err}");
        assert!(err.contains("readonly"), "{err}");
        assert!(err.contains("expected a boolean"), "{err}");

        // Malformed TOML syntax is rejected too.
        std::fs::write(&path, "not toml [[[").unwrap();
        assert!(loader.reload().err().unwrap().contains("config.toml"));
        assert_eq!(file_state(&path), "invalid - skipped");

        // A missing base file reloads as defaults, never an error.
        std::fs::remove_file(&path).unwrap();
        let loader = loader.reload().unwrap();
        assert!(!loader.has_base());
        assert_eq!(file_state(&path), "absent");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn override_paths_follow_cluster_then_context() {
        let dir = PathBuf::from("/tmp/sofka-nowhere");
        let loader = ConfigLoader::from_dir(Some(dir.clone()));
        assert_eq!(
            loader.override_paths("prod-ctx", "prod-cluster"),
            vec![
                dir.join("clusters")
                    .join("prod-cluster")
                    .join("config.toml"),
                dir.join("clusters")
                    .join("prod-cluster")
                    .join("prod-ctx")
                    .join("config.toml"),
            ]
        );
        // No cluster name (in-cluster) — no override levels at all.
        assert!(loader.override_paths("ctx", "").is_empty());
        // No context name — cluster level only.
        assert_eq!(loader.override_paths("", "c1").len(), 1);
    }

    #[test]
    fn malformed_override_warns_and_is_skipped() {
        let dir = std::env::temp_dir().join(format!("sofka-cfg-bad-test-{}", std::process::id()));
        let cluster_dir = dir.join("clusters").join("c1");
        std::fs::create_dir_all(&cluster_dir).unwrap();
        std::fs::write(dir.join("config.toml"), "default_namespace = \"base\"\n").unwrap();
        std::fs::write(cluster_dir.join("config.toml"), "not valid toml [[[").unwrap();

        let loader = ConfigLoader::from_dir(Some(dir.clone()));
        let r = loader.resolve("ctx", "c1");
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.config.default_namespace.as_deref(), Some("base"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropins_merge_in_name_order_before_cluster_overrides() {
        let dir =
            std::env::temp_dir().join(format!("sofka-cfg-dropin-test-{}", std::process::id()));
        let dropin_dir = dir.join("conf.d");
        let cluster_dir = dir.join("clusters").join("c1");
        std::fs::create_dir_all(&dropin_dir).unwrap();
        std::fs::create_dir_all(&cluster_dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "default_namespace = \"base\"\nreadonly = false\n[aliases]\npo = \"pods\"\n",
        )
        .unwrap();
        std::fs::write(
            dropin_dir.join("20-personal.toml"),
            "default_namespace = \"personal\"\n[aliases]\ndep = \"deployments\"\n",
        )
        .unwrap();
        std::fs::write(
            dropin_dir.join("10-team.yaml"),
            "default_namespace: team\nreadonly: true\naliases:\n  svc: services\n",
        )
        .unwrap();
        std::fs::write(dropin_dir.join("30-broken.yml"), "aliases: [unclosed\n").unwrap();
        std::fs::write(dropin_dir.join("40-typed.toml"), "readonly = \"yes\"\n").unwrap();
        std::fs::write(dropin_dir.join("README.md"), "ignored\n").unwrap();
        std::fs::write(cluster_dir.join("config.toml"), "readonly = false\n").unwrap();

        let loader = ConfigLoader::from_dir(Some(dir.clone()));
        assert_eq!(
            loader.dropin_paths(),
            vec![
                dropin_dir.join("10-team.yaml"),
                dropin_dir.join("20-personal.toml"),
                dropin_dir.join("30-broken.yml"),
                dropin_dir.join("40-typed.toml"),
            ]
        );
        assert_eq!(
            dropin_state(&dropin_dir.join("40-typed.toml")),
            "invalid - skipped"
        );

        let r = loader.resolve("", "");
        assert_eq!(r.config.default_namespace.as_deref(), Some("personal"));
        assert!(r.config.readonly);
        for (alias, kind) in [("po", "pods"), ("svc", "services"), ("dep", "deployments")] {
            assert_eq!(r.config.aliases.get(alias).map(String::as_str), Some(kind));
        }
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("30-broken.yml"), "{}", r.warnings[0]);
        assert!(r.warnings[1].contains("40-typed.toml"), "{}", r.warnings[1]);
        assert!(r.warnings[1].contains("readonly"), "{}", r.warnings[1]);

        let r = loader.resolve("ctx", "c1");
        assert!(!r.config.readonly, "cluster override wins over drop-ins");
        assert_eq!(r.config.default_namespace.as_deref(), Some("personal"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
