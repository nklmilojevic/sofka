//! User configuration loaded from `$XDG_CONFIG_HOME/sofka/config.toml`
//! (falling back to `~/.config/sofka/config.toml`), with optional
//! per-cluster / per-context overrides, k9s-style:
//!
//! ```text
//! sofka/
//! ├── config.toml                  # base, applies everywhere
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
    /// Log-view options — see [`LogsConfig`].
    pub logs: LogsConfig,
}

/// Log-view controls (kubelet streams).
///
/// ```toml
/// [logs]
/// tail = 300       # initial lines fetched per stream
/// buffer = 5000    # max lines retained while following (bounded tail)
/// since = "1h"     # optional: only logs newer than this (overrides tail)
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
    /// When set it replaces `tail` (Kubernetes accepts one or the other).
    pub since: Option<String>,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            tail: 300,
            buffer: 5000,
            since: None,
        }
    }
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
    pub columns: Vec<ViewColumnConfig>,
}

/// One column of a [`ViewConfig`]. Everything is optional at parse time so a
/// half-written column degrades to a validation warning instead of discarding
/// the whole config file.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ViewColumnConfig {
    /// Column header (displayed uppercased).
    pub name: String,
    /// JSON Pointer to the cell value, e.g. `/status/phase`.
    pub path: String,
    /// Value type: `text` (default), `status`, `number`, `quantity`, `time`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
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
    /// Key chord that triggers the plugin (see [`crate::keys::KeyChord`]).
    pub key: String,
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
/// bulk selection. Gates `delete`, `force-delete`, `drain`, `shell`, `debug`,
/// and `node-debug` today. Empty match lists mean "any"; glob `*` supported.
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
    /// Actions this applies to: `delete`, `force-delete`, `drain`, `shell`,
    /// `debug`, `node-debug`. Empty = any.
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
        if let Err(e) = crate::keys::KeyChord::parse(&p.key) {
            warns.push(format!("plugin {:?}: invalid key — {e}", p.name));
        }
        if let Some(o) = &p.output
            && !matches!(o.as_str(), "terminal" | "popup" | "background")
        {
            warns.push(format!(
                "plugin {:?}: unknown output {o:?} (expected terminal/popup/background) — using terminal",
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
    /// Problems with override files (malformed TOML, type mismatches). The
    /// offending layer is skipped, never fatal.
    pub warnings: Vec<String>,
}

/// Holds the parsed base `config.toml` for the session and re-reads override
/// files on demand, so `:ctx` switches pick up freshly edited overrides
/// without a restart.
#[derive(Default)]
pub struct ConfigLoader {
    /// Base `config.toml` as a raw TOML table (`None` when missing/invalid).
    base: Option<toml::Value>,
    /// The `sofka` config directory containing `config.toml` and `clusters/`.
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
            dir: dir.clone(),
        };
        match empty.reload() {
            Ok(loader) => (loader, Vec::new()),
            Err(e) => {
                eprintln!("warning: ignoring invalid {e}");
                (Self { base: None, dir }, vec![e])
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn from_dir(dir: Option<PathBuf>) -> Self {
        let base = dir.as_ref().and_then(|d| {
            let text = std::fs::read_to_string(d.join("config.toml")).ok()?;
            parse_doc(&text).ok()
        });
        Self { base, dir }
    }

    /// Re-read the base `config.toml` from disk, validated end-to-end (TOML
    /// syntax *and* the typed [`Config`] shape). `Ok` carries a fresh loader
    /// ready to [`resolve`](Self::resolve); `Err` carries the precise error —
    /// file, offending key, and what's wrong — so the caller keeps the last
    /// known-good loader instead. A missing base file is not an error: it
    /// loads as defaults, like at startup.
    pub fn reload(&self) -> Result<Self, String> {
        let dir = self.dir.clone();
        let Some(path) = self.base_path() else {
            return Ok(Self { base: None, dir });
        };
        match std::fs::read_to_string(&path) {
            Err(_) => Ok(Self { base: None, dir }),
            Ok(text) => match validate(&text) {
                Ok(value) => Ok(Self {
                    base: Some(value),
                    dir,
                }),
                Err(e) => Err(format!("{}: {e}", path.display())),
            },
        }
    }

    /// The base `config.toml` path, when a config directory is known.
    pub fn base_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("config.toml"))
    }

    /// Whether a parsed base config is active (as opposed to a missing or
    /// invalid file, both of which fall back to defaults).
    pub fn has_base(&self) -> bool {
        self.base.is_some()
    }

    /// Override files consulted for the given kubeconfig cluster/context, in
    /// merge order (cluster level first, then context level). The files need
    /// not exist — this is the search path, for [`resolve`](Self::resolve)
    /// and the `:config` view.
    pub fn override_paths(&self, context: &str, cluster: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let (Some(dir), false) = (&self.dir, cluster.is_empty()) {
            let cluster_dir = dir.join("clusters").join(sanitize(cluster));
            paths.push(cluster_dir.join("config.toml"));
            if !context.is_empty() {
                paths.push(cluster_dir.join(sanitize(context)).join("config.toml"));
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

        for path in self.override_paths(context, cluster) {
            match read_value(&path) {
                Ok(Some(v)) => {
                    merge(&mut merged, v.clone());
                    merge(&mut overlay, v);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("ignoring invalid {}: {e}", path.display())),
            }
        }

        // A type mismatch introduced by an override drops back to the base
        // config (validated at load time) rather than losing everything.
        let config = merged.try_into().unwrap_or_else(|e| {
            warnings.push(format!("ignoring cluster overrides: {e}"));
            self.base
                .clone()
                .and_then(|b| b.try_into().ok())
                .unwrap_or_default()
        });
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
/// malformed TOML — it is being skipped).
pub fn file_state(path: &Path) -> &'static str {
    match read_value(path) {
        Ok(Some(_)) => "loaded",
        Ok(None) => "absent",
        Err(_) => "invalid — skipped",
    }
}

/// Parse an override file. Missing/unreadable -> `Ok(None)` (overrides are
/// optional); present but malformed -> `Err` so the caller can warn.
fn read_value(path: &Path) -> Result<Option<toml::Value>, toml::de::Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_doc(&text).map(Some),
        Err(_) => Ok(None),
    }
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
        assert_eq!(file_state(&path), "invalid — skipped");

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
}
