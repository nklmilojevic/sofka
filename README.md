# sofka

A Kubernetes TUI, reimagined in Rust - built on [`kube-rs`](https://kube.rs) and
[`ratatui`](https://ratatui.rs), async-first from the ground up.

## Screenshots

| Pod list + command palette                                                | Namespace switcher                                                          | Flux suspend/resume/reconcile menu                                                      |
| ------------------------------------------------------------------------- | --------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| ![Pod list with the fuzzy command palette open](docs/screenshot-pods.png) | ![Namespace switcher popup over a pod list](docs/screenshot-namespaces.png) | ![Flux Kustomizations with the suspend/resume/reconcile menu](docs/screenshot-flux.png) |

### Why "sofka"

<img src="docs/sophie.png" alt="Sophie, a Russian Blue, watching the screen with visible suspicion" align="right" width="220">

That's Sophie. She sits behind the monitor and watches it - not occasionally,
_constantly_, with the specific narrowed-eye expression of someone who has
noticed a pod in `CrashLoopBackOff` and is judging you for it. She doesn't
miss a state change. She doesn't get distracted. She is, functionally, a
cluster watchman who happens to be a cat.

`sofka` is the Serbian diminutive of Sophia - "wisdom," fittingly, since
watching things closely and knowing when something's wrong is more or less
the whole job description of both a good cluster TUI and a good cat.

This is a from-scratch reimagining of [k9s](https://github.com/derailed/k9s)
(originally ~51k lines of Go), not a line-for-line port. It keeps the spirit -
a fast, keyboard-driven cluster navigator - but rethinks the architecture
around a single generic object pipeline instead of one hand-written renderer
per resource kind.

## How it differs from k9s

- **One generic render pipeline, not one file per kind.** k9s ships a
  dedicated Go file (struct + `ColorerFunc`) per resource type it knows about.
  sofka has one `DynamicObject → cells` function with curated columns for the
  common kinds and a NAME/AGE fallback for everything else - so a CRD nobody's
  written a renderer for still lists, sorts, and filters correctly on day one.
- **Flux CD is a first-class citizen, not a plugin.** `t` opens a
  Suspend/Resume/Reconcile-now menu for Kustomizations, HelmReleases,
  git/helm/oci repositories, buckets, image automation, and notification
  alerts/receivers - patching `spec.suspend` and the
  `reconcile.fluxcd.io/requestedAt` annotation directly via the k8s API. No
  `flux` binary required, and it composes with bulk multiselect.
- **Port-forwards run in the background.** Starting one doesn't freeze the
  TUI for its whole lifetime; `:pf` lists active forwards and stops them
  individually while others keep running. They're killed automatically on
  quit rather than left orphaned.
- **Bulk actions via multiselect.** `space` marks rows for delete, kill, or
  Flux suspend/resume/reconcile across many resources at once - not
  one-row-at-a-time.
- **CRD rows drill into their custom resources**, not their YAML - `enter` on
  a CustomResourceDefinition resolves its served version and lists the actual
  objects.
- **Skins, not a single fixed palette.** Built-in Catppuccin, Gruvbox,
  Solarized, Nord, Dracula, Tokyo Night, One Dark, Rosé Pine, and Monokai
  palettes selectable in config, with per-swatch hex overrides. Auto-detects
  a light or dark terminal background when no skin is configured. Every
  semantic color (row status, severity badges, headers, borders) is derived
  from the active palette, so a skin change is consistent everywhere at once.
  Opt into `background = true` to paint the skin's own background instead of
  the terminal's — pair it with a light per-context skin to make prod glow.
- **A combined row colorer.** Whole-row status tinting like k9s (healthy
  rows, errors, pending, completed all read as one color), _plus_ a
  distinct STATUS badge and outlier coloring on RESTARTS/CPU/MEM so a
  crash-looping or resource-hungry pod still pops out of an otherwise
  uniform row. The RESTARTS/CPU/MEM (and container request/limit) warning
  and critical thresholds are **configurable** per resource and per context.
- **It explains _why_ something is broken.** `X` opens a deterministic,
  evidence-backed incident view for the selection: rollout state, degraded
  conditions, the blocking pods and their container failure reasons
  (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable, failing
  probes), and recent Warning events — no AI, no external service. `⏎`/`E`/`l`
  jump straight from a finding to the offending pod, its events, or its logs.
- **A session-local timeline.** `T` shows the state changes sofka has
  observed for an object while watching — generation bumps, replica/readiness
  shifts, pod phase/restart/waiting-reason changes, condition transitions —
  as a causal, timestamped log, derived from the watch stream with nothing
  stored on disk.

## Why it's faster

Not a marketing number - these are specific, checkable design choices:

- **No GC.** Rust's ownership model means zero garbage-collector pauses.
  Watching thousands of pods/CRs across a large cluster grows the in-memory
  store, but redraw latency doesn't get jittery as that store grows the way a
  GC'd runtime's can under sustained allocation pressure.
- **Batched redraws.** The event loop drains every pending watch message
  before triggering one redraw (`while let Ok(m) = rx.try_recv()`). A rollout
  touching 50 pods costs one render pass, not fifty.
- **Cached row computation.** Sorting and fuzzy-filtering the visible rows
  only reruns when the underlying data or the filter text actually changed (a
  dirty-flag-guarded cache), not on every frame or every keystroke against the
  full object set.
- **No subprocess overhead for the hot paths.** Delete, scale, suspend/
  resume/reconcile, and CRD drill-down are direct kube API calls (JSON
  merge-patches over the existing client), not a `kubectl`/`flux` process
  fork+exec per action.
- **Generation-tagged streams.** Switching views doesn't wait for an old
  watcher to tear down - stale messages are dropped by generation tag the
  instant a newer watch takes over, so navigation never stalls behind a
  slow-to-cancel stream.

## Features

- **Connect** to the current kubeconfig context, including exec credential
  plugins (e.g. GKE).
- **API discovery** of every resource type on the cluster, with k9s-style
  short aliases (`po`, `dp`, `svc`, `no`, `cm`, `sts`, `ds`, `ks`, `hr`, …) and
  correct precedence (core `pods` beats `pods.metrics.k8s.io`).
- **Live watch** of any kind via `kube::runtime::watcher`, streamed into an
  in-memory store.
- **Curated columns** for common kinds (pods, deployments, replicasets,
  statefulsets, daemonsets, services, nodes, namespaces, configmaps, secrets,
  jobs, cronjobs, PVC/PV, ingresses, endpoints, CustomResourceDefinitions)
  with a NAME/AGE fallback for everything else.
- **Custom views** - user-defined columns for any resource in config
  (`[views]`), extracted via JSON Pointer with typed sorting (quantities,
  numbers, timestamps sort by value). Unknown custom resources automatically
  pick up their CRD's `additionalPrinterColumns`. `w` toggles wide-only
  columns (kubectl `-o wide`).
- **Live CPU/MEM columns** for pods and nodes from the metrics API, with
  outlier coloring. The pod container picker also shows per-container CPU and
  memory, each container's usage as a percentage of its request and limit
  (`-` marks an unset request/limit), and the pod's QoS class; all of it
  degrades gracefully when metrics-server is absent.
- **Configurable thresholds** (`[thresholds]`) — the warning/critical cutoffs
  for RESTARTS, CPU, memory, and container request/limit utilization coloring,
  with global defaults plus per-resource and per-context overrides.
- **Explain-unhealthy view** (`X` / `:explain`) — a deterministic,
  evidence-backed explanation of why the selected object is unhealthy
  (rollout state, degraded conditions, blocking pods and their container
  failure reasons, recent Warning events), with `⏎`/`E`/`l` to jump to the
  resource, its events, or its logs.
- **Session-local timeline** (`T` / `:timeline`) — a per-object, timestamped
  log of the state changes sofka has observed while watching (generation
  bumps, readiness shifts, phase/restart/condition changes), bounded and
  kept only in memory.
- **Drill-down navigation** with a breadcrumb stack: workload/service →
  pods, node → its pods, pod → containers, namespace → re-scope, CRD → its
  custom resources. `esc` pops back.
- **Command palette** (`:`) - fuzzy over the full resource catalog, built-in
  commands (`ctx`, `pulse`, `xray`, `explain`, `timeline`, `gitops`, `can-i`,
  `journal`, `diff`, `events`, `pf`), and saved bookmarks/workspaces together,
  plus
  row **filtering** (`/`) with matched-character highlighting: fuzzy text,
  `!text` inverse match, `-l`/`-f` label & field selectors (evaluated by the
  API server on ⏎), and typed column comparisons (`status=CrashLoopBackOff`,
  `cpu>500m`, `memory>1Gi`, `restarts>=5`, `age<2h`) — space-separated terms
  AND together.
- **Multiselect** (`space`) for bulk delete/kill/suspend/resume/reconcile.
- **Pulse dashboard** (`:pulse`) - cluster-health tiles, refreshed every 5s.
- **Xray tree** (`:xray`) - hierarchical view from the current kind down
  through owner references to pods and containers.
- **Flux CD controls** (`t`) - suspend/resume/reconcile menu, native k8s API
  patches.
- **Background port-forwards** (`f`/`F` to start, `:pf` to manage).
- **Plugins** - config-defined shell-out commands bound to keys, scoped per
  resource. Keys are full **chords** (`ctrl-g`, `alt-x`, `shift-b`, `f5`);
  commands run in the **terminal**, a captured **popup**, or the **background**
  (with a timeout and bounded output); can require **confirmation** or be
  flagged **dangerous**; declare themselves read-only (`mutating = false`) to
  stay usable in `--readonly`; substitute rich placeholders as separate
  arguments (`$NAME`/`$NAMESPACE`/`$CONTEXT`/`$CLUSTER`/`$RESOURCE`/`$GROUP`/
  `$VERSION`/`$KIND`/`$FILTER`); and run over **every marked row** at once,
  reporting partial failures.
- **Bookmarks** (`[[bookmarks]]`) - saved navigation commands bound to a chord
  and the command palette: jump to a resource, optionally in another
  context/namespace, with a filter, sort, and view applied in one keystroke.
- **Workspaces** (`[[workspaces]]`) - a named, task-oriented collection of
  views; open it and cycle its views with `Tab`/`Shift-Tab` without leaving
  the workspace.
- **Diff** (`:diff`) - unified diff of the live object vs its
  `last-applied-configuration`.
- **Events** (`:events` / `E`) - live Kubernetes Events for the selected
  object, filtered by UID when available.
- **GitOps view** (`:gitops` / `:flux`) - for the selected object, the Flux
  ownership and reconciliation chain: owning Kustomization/HelmRelease, its
  source (GitRepository/OCIRepository/HelmRepository) and applied vs latest
  revision, `dependsOn` edges, and ready status - each an evidence-backed
  finding you can `⏎` to jump straight to.
- **Managed-resource mutation warnings** - editing, deleting, scaling, or
  otherwise mutating an object owned by Flux (or another controller) warns
  first that your change will be reverted or the object recreated on the next
  reconcile, so you fix the source instead of fighting the controller.
- **Action-aware authorization** (`:can-i`) - a `SelfSubjectRulesReview`
  overview of what you can do in the current namespace, plus
  `:can-i <verb> <resource> [ns]` to check a single action before you attempt
  it - the same answer `kubectl auth can-i` gives, without leaving the TUI.
- **Declarative guardrails** (`[[guardrails]]`) - config-defined rules that
  match on context/namespace/resource/action globs to **deny** a destructive
  action outright (`delete`/`force-delete`/`drain`/`shell`/`debug`), force
  **type-to-confirm** (resource name or `context/name`), or cap **bulk**
  operations - so "never delete in prod", "always confirm a prod shell", and
  "no more than one at once" are enforced, not remembered.
- **Action journal** (`:journal` / `:audit`) - a session-local, in-memory log
  of every mutating action you've taken (what, target, context, when),
  newest-first. Identifiers only - never secret input or decoded values - and
  never written to disk.
- **Ephemeral debug containers** (`:debug`) - attach a throwaway debug
  container to the selected pod via `kubectl debug`, prompting for the image
  (prefilled with the `[debug]` default). `d` in the container picker targets
  a specific container's process namespace (`--target`). Gated by read-only
  mode and the `debug` guardrail; recorded in the journal.
- **RBAC-aware palette** - hides resource kinds you can't `list`.
- **Namespace switcher** (`n`) with pinned **favourites** (`favorite_namespaces`,
  ★) and per-context session **recents** (·) above the rest, and **context
  switcher** (`:ctx`).
- **YAML view** (`y`) and **describe** (`d`, via `kubectl`).
- **Logs** (`l`) - per-container on a pod, aggregated across all matching
  pods on a workload/service. In-logs **search** (`/`) with highlighting;
  `p` for previous-container logs. ANSI color codes from the source app are
  parsed and mapped onto the active skin, not printed as literal escapes.
- **VictoriaLogs integration** (`L` / `:vlogs`) - log history for the
  selected pod, container, workload, service, or whole namespace from a
  VictoriaLogs backend: a lookback query plus live tail, in the same logs
  view. Zero-config: sofka autodiscovers the VictoriaLogs service in the
  cluster and reaches it through the API-server proxy; or point
  `[providers.logs]` at an external URL. Covers restarted and deleted pods —
  the backend remembers what the kubelet no longer has.
- **Skinnable** - built-in Catppuccin, Gruvbox, Solarized, Nord, Dracula,
  Tokyo Night, One Dark, Rosé Pine, and Monokai palettes, auto-detected
  dark/light default, plus per-swatch overrides in config.
- **Config file** (TOML): aliases, default namespace/resource, favourite
  namespaces, plugins, bookmarks, workspaces, views, thresholds, log provider,
  skin — with per-cluster/per-context overrides and live `:reload`.

## Installation

### Download from Github

Prebuilt binaries for macOS (aarch64/x86_64) and Linux (aarch64/x86_64) are
attached to each [GitHub release](https://github.com/nklmilojevic/sofka/releases).

### Nix

Nix users can run it directly without installing anything:

```sh
nix run github:nklmilojevic/sofka
```

### Cargo

```sh
cargo install sofka
```

or build from source (see [Development](#development)).

### macOS: "cannot be opened because the developer cannot be verified"

The release binaries aren't signed/notarized yet, so if you download a
tarball through a browser and extract it, Gatekeeper will refuse to run it -
this is expected, not a broken build. Clear the quarantine flag once:

```
xattr -d com.apple.quarantine sofka
```

(or right-click the binary in Finder → Open, and confirm through the dialog
once). **Signing and notarization are planned for the next release**, at
which point this step won't be necessary.

## Configuration

`$XDG_CONFIG_HOME/sofka/config.toml` (or `~/.config/sofka/config.toml`):

```toml
default_namespace = "kube-system"
default_resource  = "deployments"
readonly          = false  # true disables every mutating action (delete, edit,
                           # scale, shell, plugins, …); --readonly/--write win

# Namespaces pinned to the top of the `n` switcher (★); session recents (·)
# follow them.
favorite_namespaces = ["kube-system", "monitoring"]

[aliases]
dep = "deployments"

[skin]
# name omitted: auto-detects dark/light and picks catppuccin-mocha/-latte.
# Or pick one explicitly: catppuccin-mocha, -latte, -frappe, -macchiato,
# gruvbox-dark, gruvbox-light, nord, dracula, solarized-dark, solarized-light,
# tokyo-night, one-dark, rose-pine, monokai.
name = "gruvbox-dark"
background = true        # fill views with the skin's own background swatch
                        # (default: false = inherit the terminal background)

[skin.colors]            # optional per-swatch overrides
red = "#fb4934"

[[plugins]]
key = "ctrl-g"             # a key chord: ctrl-/alt-/shift-, function keys, …
name = "argocd-sync"
command = "argocd"
args = ["app", "sync", "$NAME"]
scopes = ["deployments"]   # omit for all resources
dangerous = true           # confirm (showing the command) before running
# mutating = false         # allow in --readonly mode (declares it read-only)
# output = "popup"         # "terminal" (default) / "popup" / "background"
# shell = true             # run via `sh -c` (args stay positional $1, $2, …)
```

See [Plugins](#plugins) below for the full plugin surface (output modes,
placeholders, bulk invocation over marked rows).

### Custom views

Define table columns for any resource — most usefully for custom resources
that would otherwise fall back to NAME/AGE. Views are keyed by
apiVersion/plural (`"cert-manager.io/v1/certificates"`, `"v1/pods"`),
group/plural, bare plural, or lowercased kind; the most specific key wins.

```toml
[views."cert-manager.io/v1/certificates"]
sort = "EXPIRES:desc"     # initial sort column, ":asc" (default) or ":desc"
# replace = true          # replace the curated columns instead of overlaying

[[views."cert-manager.io/v1/certificates".columns]]
name = "READY"
path = "/status/conditions/0/status"
type = "status"           # colors the row like other status columns

[[views."cert-manager.io/v1/certificates".columns]]
name = "EXPIRES"
path = "/status/notAfter"
type = "time"             # rendered as elapsed ("3d4h") / "in 30d"

[[views."cert-manager.io/v1/certificates".columns]]
name = "ISSUER"
path = "/spec/issuerRef/name"
wide = true               # only shown in wide mode (`w`)
```

`path` is a JSON Pointer (RFC 6901) into the object as served by the API —
`/metadata/…`, `/spec/…`, `/status/…`, array indices like
`/status/conditions/0/status`. Column `type` is `text` (default), `status`,
`number`, `quantity` (`500m`, `1Gi`), or `time`; typed columns sort by value,
not lexically. Optional `width` (fixed columns) and `align`
(`left`/`center`/`right`) tune the layout. By default columns overlay the
curated ones: a matching header replaces it in place, new columns land before
AGE. Invalid entries are skipped with a warning shown in-app — they never
take the TUI down.

Custom resources without an explicit view automatically use their CRD's
`additionalPrinterColumns` (columns with `priority > 0` become wide-only),
so most CRs get useful columns with zero configuration.

### Thresholds

The warning/critical cutoffs behind RESTARTS/CPU/MEM cell coloring (and the
container picker's request/limit utilization) are configurable. Anything left
unset keeps sofka's built-in defaults, so an empty config colors exactly as
before. Global `[thresholds]` apply everywhere; `[thresholds.resources.<key>]`
overrides them per resource (keyed like `[views]`), and — like every section —
a per-cluster/per-context override file can retune them for one context.
Thresholds also re-apply live on `:reload`.

```toml
[thresholds]
restarts    = { warn = 3, critical = 10 }      # count
cpu         = { warn = "200m", critical = "1" } # absolute usage
memory      = { warn = "256Mi", critical = "1Gi" }
utilization = { warn = 75, critical = 90 }     # percent of request/limit

[thresholds.resources.pods]                    # per-kind override
restarts = { warn = 5, critical = 20 }
```

Either bound of a band may be omitted to disable that level; `warn` is peach,
`critical` is red.

### Plugins

`[[plugins]]` bind a shell-out command to a key. `key` is a **chord**: a single
character (`"g"`), a modifier combination (`"ctrl-g"`, `"alt-x"`, `"shift-b"`),
or a function/named key (`"f5"`, `"ctrl-f2"`). Built-in keys win over a plugin
bound to the same chord.

```toml
[[plugins]]
key = "shift-y"
name = "yaml-summary"
command = "kubectl"
args = ["get", "$RESOURCE", "$NAME", "-n", "$NAMESPACE", "-o", "yaml"]
scopes = ["pods", "deployments"]   # omit for all resources
mutating = false          # read-only: still runs under --readonly
output = "popup"          # captured into a scrollable view (see below)

[[plugins]]
key = "ctrl-x"
name = "restart-rollout"
command = "kubectl"
args = ["rollout", "restart", "$RESOURCE/$NAME", "-n", "$NAMESPACE"]
scopes = ["deployments"]
dangerous = true          # confirm (showing the exact command) first
```

- **Placeholders** are substituted as whole arguments (never spliced into a
  shell string): `$NAME`, `$NAMESPACE`/`$NS`, `$CONTEXT`, `$CLUSTER`,
  `$RESOURCE` (plural), `$GROUP`, `$VERSION`, `$KIND`, `$FILTER`.
- **`output`**: `terminal` (default, interactive — suspends the TUI), `popup`
  (captured off-thread into a scrollable view), or `background` (detached, a
  notification flashes on completion). `popup`/`background` honour `timeout`
  (`"30s"`, default) and bound their captured output.
- **`mutating`** (default `true`): a mutating plugin is blocked in read-only
  mode; set `false` to allow a known read-only one.
- **`confirm`**/**`dangerous`**: prompt before running, showing the exact
  executable and arguments; `dangerous` is flagged ⚠.
- **`shell = true`**: opt into `sh -c`; placeholders are still passed as
  positional parameters (`$1`, `$2`, …), never interpolated into the script.
- **Bulk**: with rows marked (`space`), a `popup`/`background` plugin runs over
  every marked row, reporting partial failures. (Interactive `terminal`
  plugins can't compose over a set and refuse a marked run.)

Invalid values (bad chord, unknown `output`, malformed `timeout`) disable just
that plugin / fall back to the default, with a warning shown in `:config`.
Plugins appear in `?` help with their chord and scope.

### Bookmarks

`[[bookmarks]]` are saved navigation commands — jump to a resource, optionally
in another context/namespace, with a filter, sort, and view applied in one
keystroke. They're triggered by an optional key chord and always available in
the command palette (`★`, ranked above resources).

```toml
[[bookmarks]]
key = "shift-1"                          # optional
name = "Prod API failures"
resource = "pods"
context = "prod-eu"                      # optional: switched first
namespace = "checkout"                   # optional; all/* = all namespaces
filter = "status!=Running -l app=api"    # optional, same syntax as `/`
sort = "RESTARTS:desc"                   # optional: COLUMN[:asc|:desc]
view = "xray"                            # optional: xray | pulse
```

### Workspaces

`[[workspaces]]` group several views into a named, task-oriented set (checkout
ops, a cluster upgrade, cert renewal). Opening one (chord or palette, `▦`)
switches its optional context once and lands on the first view; **`Tab`** /
**`Shift-Tab`** cycle the rest without leaving the workspace.

```toml
[[workspaces]]
key = "ctrl-w"
name = "Checkout ops"
context = "prod-eu"          # optional: switched once on open

[[workspaces.views]]
name = "API pods"
resource = "pods"
namespace = "checkout"
filter = "-l app=api"
sort = "RESTARTS:desc"

[[workspaces.views]]
name = "Ingress"
resource = "ingresses"
namespace = "checkout"
```

### Guardrails

`[[guardrails]]` turn "never delete in prod", "always confirm drains", and
"no more than 5 at once" into enforced rules instead of things you have to
remember. Each rule matches on `contexts`, `namespaces`, `resources`, and
`actions` globs (all optional; omitted = matches everything), then applies
the strictest of: `deny` (block outright), `confirmation` (type to confirm),
and `max_bulk` (cap how many rows one action may touch). The gated `actions`
are the destructive verbs sofka takes directly — `delete`, `force-delete`,
`drain`, `shell` (exec), and `debug`. The first matching rule wins; `reason`
is shown when it fires.

```toml
[[guardrails]]
contexts = ["*prod*"]
actions = ["delete", "force-delete", "drain"]
deny = true
reason = "Destructive actions on prod go through GitOps, not the TUI."

[[guardrails]]
contexts = ["*prod*"]
actions = ["shell"]
# "type-resource-name" | "type-context-name"; any other value = a plain y/N
confirmation = "type-context-name"
reason = "Confirm the exact pod before shelling into prod."

[[guardrails]]
namespaces = ["kube-system"]
actions = ["delete"]
max_bulk = 1                     # no bulk deletes in kube-system
```

### Ephemeral debug containers

`:debug` attaches a throwaway debug container to the selected pod through
`kubectl debug`, prompting for the image (prefilled with the default below).
Leaving `command` empty launches an interactive shell (bash if the image ships
it, else sh), mirroring the pod shell. `d` in the container picker pins
`--target=<container>` so the debug container shares that container's process
namespace. The ephemeral container persists on the pod until it's recreated —
Kubernetes can't remove it — so there's nothing for sofka to clean up.

```toml
[debug]
image = "nicolaka/netshoot:latest"   # default image the prompt is prefilled with
command = ["bash"]                   # entrypoint; omit for an interactive shell
```

Debug creation is disabled in read-only mode and gated by the `debug`
guardrail action.

### Log provider (VictoriaLogs)

`L` (or `:vlogs`) opens log history for the selection from a VictoriaLogs
backend instead of the kubelet. With no configuration at all, sofka finds the
VictoriaLogs `Service` in the cluster by its well-known labels (Helm charts
and the VictoriaMetrics operator) and queries it through the Kubernetes
API-server service proxy, reusing your kubeconfig credentials. Configure it
only to point at an external endpoint or to adjust the defaults:

```toml
[providers.logs]
type = "victorialogs"
url = "https://vlogs.example.com"  # omit to autodiscover in-cluster
lookback = "1h"                    # initial query window (s/m/h/d)
limit = 300                        # lines fetched by the initial query

[providers.logs.headers]           # optional, sent with every request
Authorization = "Bearer <token>"

# Field names as ingested by your log shipper. Omit this section to let
# sofka detect the convention from the backend's stream fields — vector,
# fluentd, fluent-bit, OpenTelemetry, and bare namespace/pod/container
# names are recognized. Configure only for exotic pipelines.
[providers.logs.fields]
namespace = "kubernetes.pod_namespace"
pod = "kubernetes.pod_name"
container = "kubernetes.container_name"
```

Like every section, `[providers.logs]` can live in a per-cluster or
per-context override file, so each cluster can use its own backend.

### Per-cluster / per-context overrides

Any option can be overridden for a specific cluster or kubeconfig context,
k9s-style. Drop partial config files under `clusters/`:

```
~/.config/sofka/
├── config.toml                # base, applies everywhere
└── clusters/
    └── prod-cluster/          # kubeconfig *cluster* name
        ├── config.toml        # every context on prod-cluster
        └── prod-admin/        # kubeconfig *context* name
            └── config.toml    # that context only
```

Overrides merge over the base config (cluster level first, then context
level): tables like `[aliases]` and `[skin.colors]` merge key-by-key,
everything else — strings, booleans, arrays like `[[plugins]]` — replaces the
base value. Directory names are the kubeconfig names with any character other
than letters, digits, `.`, `_`, `-` replaced by `-`, so an EKS context
`arn:aws:eks:eu-west-1:123456789:cluster/prod` becomes the directory
`arn-aws-eks-eu-west-1-123456789-cluster-prod`.

```toml
# clusters/prod-cluster/config.toml — make prod unmistakable and hands-off
readonly = true

[skin]
name = "catppuccin-latte"
background = true
```

A skin named in an override pins that context's colors; contexts without one
keep the session skin (config `skin.name`, the auto-detected default, or your
last `:skin` choice). Overrides are re-read on every `:ctx` switch, so edits
apply without restarting.

### Headless modes (no TTY required)

```
sofka --check                # connect, run discovery, print a summary, exit
sofka pods --snapshot        # render one frame of a resource view to stdout
sofka dp -A --snapshot       # deployments, all namespaces
```

These double as CI smoke tests.

## Usage

```
sofka [RESOURCE] [-n NAMESPACE] [-A] [--readonly | --write]

  RESOURCE          resource to open (alias/plural/kind), default: pods
  -n, --namespace   namespace to start in
  -A, --all-namespaces
  --readonly        disable every mutating action for the session
  --write           force write mode, overriding any config `readonly`
```

`--readonly`/`--write` pin the mode for the whole session, winning over the
config `readonly` option — including per-cluster/per-context overrides — on
every `:ctx` switch. Without a flag, switching into a context whose config
sets `readonly = true` enables read-only mode (shown as `[read-only]` in the
header) and switching away restores write mode.

### Keys

| Key                                        | Action                                                                                                                                |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------- |
| `:<resource>`                              | command palette - fuzzy over kinds and built-in commands                                                                              |
| `:<resource> <ns>`                         | switch kind and namespace at once (`:deploy social`; `all`/`*` = all namespaces; the namespace tab-completes)                         |
| `[` / `]`                                  | view history - back / forward through visited kind+namespace views                                                                    |
| `Tab` / `shift-Tab`                        | cycle views of the active workspace (when one is open)                                                                                |
| `enter`                                    | drill down (workload/svc → pods, node → its pods, pod → containers, ns → re-scope, CRD → its resources)                               |
| `esc`                                      | go back / pop the view stack / clear filter / clear marks                                                                             |
| `j`/`k`, `↓`/`↑`, `g`/`G`                  | navigate                                                                                                                              |
| `S` / `I`                                  | cycle sort column / invert sort direction                                                                                             |
| `space`                                    | mark/unmark row for bulk actions                                                                                                      |
| `/`                                        | filter: fuzzy text · `!inverse` · `-l`/`-f` selectors (server-side on ⏎) · `status=X` `cpu>500m` `age<2h`                             |
| `n` / `0`                                  | namespace switcher / all namespaces                                                                                                   |
| `shift-j`                                  | jump to owner/controller                                                                                                              |
| `o`                                        | show the node hosting the selected pod                                                                                                |
| `ctrl-r`                                   | refresh the watch                                                                                                                     |
| `y` / `d` / `E`                            | view YAML / describe (`kubectl`) / live events                                                                                        |
| `X` / `T`                                  | explain why the selection is unhealthy / session-local state-change timeline                                                          |
| `:gitops` / `:flux`                        | Flux owner, source, revisions & reconciliation chain for the selection (`⏎` to jump)                                                  |
| `:can-i` / `:can-i <verb> <resource> [ns]` | what you can do here / check a single action (`SelfSubjectAccessReview`)                                                              |
| `:journal` / `:audit`                      | session-local log of the mutating actions you've taken                                                                                |
| `:ctx` / `:ctx <name>`                     | context switcher popup / switch directly (the name tab-completes)                                                                     |
| `:skin`                                    | switch the color skin live (`:skin gruvbox-dark` applies directly)                                                                    |
| `l` / `p`                                  | logs (workload = all matching pods) / previous-container logs                                                                         |
| `c`                                        | copy resource name to clipboard                                                                                                       |
| `e`                                        | edit in `$EDITOR` (`kubectl edit`)                                                                                                    |
| `s`                                        | shell into pod / scale a workload (context-dependent)                                                                                 |
| `a`                                        | attach to pod                                                                                                                         |
| `:debug`                                   | attach an ephemeral debug container to the pod (`kubectl debug`; `d` in the container picker targets one)                             |
| `i`                                        | set container image                                                                                                                   |
| `r`                                        | rollout restart (workloads) / refresh (elsewhere)                                                                                     |
| `f` / `shift-f`                            | port-forward (pods/services) - runs in the background                                                                                 |
| `t`                                        | Flux: suspend/resume/reconcile menu                                                                                                   |
| `C` / `U` / `D`                            | nodes: cordon / uncordon / drain                                                                                                      |
| `ctrl-d` / `ctrl-k`                        | delete / force-delete (marked rows, or current); in confirm: `f` toggles force, `c` cycles cascade (background → foreground → orphan) |
| `:q`, `ctrl-c`                             | quit                                                                                                                                  |
| `?`                                        | help                                                                                                                                  |
| _(config)_                                 | plugin / bookmark / workspace key chords — `ctrl-`/`alt-`/`shift-`/`fN`; listed in `?` help                                           |

**Logs view:** `/` search+highlight · `s` autoscroll · `w` wrap · `t`
timestamps · `x` stop/resume stream · `c` copy buffer · `ctrl-s` save to file
· `esc` back. The newest line anchors to the bottom of the viewport.

**Document views** (YAML, describe, diff, events): `/` searches vim-style —
the whole document stays on screen with every match highlighted, and `n` / `N`
jump to the next / previous match. `w` wraps, `c` copies the document, `esc`
backs out (first press clears an active search). The `?` help panel's `/`
filters instead, narrowing to matching keybinds.

**Explain view** (`X`): `j`/`k` move · `⏎` jump to the resource behind a
finding (a blocking pod) · `E` its events · `l` its logs · `r` re-gather ·
`esc` back. Findings that can be jumped into are marked with a trailing `→`.

Interactive actions (`e`, `s`-shell, `a`) suspend the TUI and shell out to
`kubectl`; delete/scale/restart/set-image/suspend/resume/reconcile/
port-forward go through the kube API (or a backgrounded process) directly.

## Architecture

```
main.rs      CLI (clap), terminal lifecycle, the async select! event loop,
             and the --check / --snapshot headless modes.
app.rs       All application state + input handling (a mode state machine:
             Table / Command / Filter / Detail / Logs / FluxMenu /
             PortForwards / Help / Namespaces / …), split into app/*.rs
             (plugins, bookmarks, workspaces, navigation, …). Spawns watch/
             log/port-forward tasks.
k8s.rs       Cluster connect, API discovery, alias registry + group-priority
             resolution, watch-task spawning, namespace listing.
keys.rs      Key-chord parsing + matching (ctrl-/alt-/shift-, function keys)
             for plugin, bookmark, and workspace bindings, with unit tests.
store.rs     In-memory resource store + the Msg enum that watch tasks send to
             the UI (generation-tagged so stale streams are dropped).
columns.rs   Per-kind column definitions and cell extraction from
             DynamicObjects (the "render" layer), with unit tests.
thresholds.rs Configurable RESTARTS/CPU/MEM/utilization coloring bands
             (global + per-resource), compiled from config, with unit tests.
explain.rs   Deterministic "why is this unhealthy?" analysis — pure, turns
             an object + its pods + events into ranked findings, unit-tested.
timeline.rs  Session-local per-object state-change history diffed from the
             watch stream (pure transition logic, unit-tested).
ui.rs        All ratatui rendering: header, table, scrollable views, popups,
             status bar.
theme.rs     Palette + semantic styles, skin resolution.
```

Data flow: `watcher` tasks push generation-tagged `Msg`s over an
`mpsc::UnboundedSender`; the main `tokio::select!` loop folds them into the
`Store`, batches any other queued updates before redrawing, and shares that
same loop with terminal input and a 1s tick (age columns, dead port-forward
reaping) - so the UI never blocks on the network.

## Development

```
cargo run -- pods            # run against current context
cargo test                   # unit tests (no cluster required)
cargo clippy --all-targets   # lints (clean)
```

## Release

After merging the release-ready changes to `main`, run one of:

```
just release-patch
just release-minor
just release-major
```

The recipe switches to a clean, up-to-date `main`, bumps `Cargo.toml` /
`Cargo.lock`, commits and pushes the version bump, then creates the GitHub
Release. The release workflow runs from that published release and uploads
platform binaries, publishes crates.io, and warms the Nix cache.

## Future roadmap

### Milestone 1: power-user foundation

- [x] Custom columns and view overlays.
- [x] CRD `additionalPrinterColumns` support.
- [x] Structured filter parser.
- [x] Server-side label and field selectors.
- [x] Config reload and validation view.
- [x] Wide/narrow column visibility.

### Milestone 2: actionable health

- [x] Container metrics.
- [x] Request/limit percentages and QoS.
- [x] Configurable thresholds.
- [x] Explain-unhealthy view.
- [x] Direct evidence navigation.
- [x] Initial session-local timeline.

### Milestone 3: extensibility and repeatable workflows

- [x] Modifier-aware hotkeys.
- [x] Rich plugin execution and output modes.
- [x] Bulk plugins.
- [x] Bookmarks and saved queries.
- [x] Operational workspaces.
- [x] Namespace favourites and recents.

### Milestone 4: GitOps and safety

- [x] Flux ownership and dependency navigation.
- [x] Revision and reconciliation-chain visibility.
- [x] Managed-resource mutation warnings.
- [x] Action-aware authorization checks.
- [x] Declarative guardrails.
- [x] Local action journal.

### Milestone 5: debugging and collaboration

- [ ] Ephemeral container workflow.
- [ ] Node debug pod workflow.
- [ ] Redacted diagnostic bundles.
- [ ] Screen dumps and structured snapshots.
- [ ] Runtime diagnostics and structured logs.
- [ ] Richer log controls.

### Milestone 6: fleet and integrations

- [ ] Opt-in cross-context health dashboard.
- [ ] Historical metrics provider interface.
- [x] Log provider interface (VictoriaLogs: autodiscovery or configured URL).
- [ ] Trace provider interface.
- [ ] Extended relationship graph.
- [ ] Vulnerability scanner integration.
- [ ] Historical right-sizing recommendations.

### Milestone 7: distribution polish

- [ ] Signed and notarized macOS releases.
- [ ] Homebrew distribution.
- [ ] Checksums, attestations, and SBOM.
- [ ] Evaluate Windows support.
- [ ] Document a Kubernetes compatibility matrix.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option - the Rust ecosystem standard.
