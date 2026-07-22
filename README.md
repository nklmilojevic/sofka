# sofka

sofka is a Kubernetes text user interface (TUI), written in Rust. It is built on
[`kube-rs`](https://kube.rs) and [`ratatui`](https://ratatui.rs). It uses async
operations for all tasks.

## Screenshots

| Pod list + command palette                                                | Namespace switcher                                                          | Flux suspend/resume/reconcile menu                                                      |
| ------------------------------------------------------------------------- | --------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| ![Pod list with the fuzzy command palette open](docs/screenshot-pods.png) | ![Namespace switcher popup over a pod list](docs/screenshot-namespaces.png) | ![Flux Kustomizations with the suspend/resume/reconcile menu](docs/screenshot-flux.png) |

### Why "sofka"

<img src="docs/sophie.png" alt="Sophie, a Russian Blue, watching the screen with visible suspicion" align="right" width="220">

That is Sophie. Sophie is a Russian Blue cat. She sits behind the monitor and
watches the screen. She watches it constantly, not sometimes. She has the
narrow-eyed look of someone who has seen a pod in `CrashLoopBackOff`. She sees
each state change. She does not get distracted. She is, in effect, a cluster
watchman that is a cat.

`sofka` is the Serbian short form of Sophia. Sophia means "wisdom". A good
cluster TUI and a good cat both watch things closely. Both know when something
is wrong.

sofka is a new version of [k9s](https://github.com/derailed/k9s) (originally
approximately 51,000 lines of Go). sofka is not a line-by-line copy. It keeps
the same purpose: a fast cluster navigator that you control with the keyboard.
But it uses a different architecture. It uses one generic object pipeline
instead of one renderer for each resource kind.

## How it differs from k9s

- **One generic render pipeline, not one file for each kind.** k9s has one Go
  file (a struct and a `ColorerFunc`) for each resource type that it knows.
  sofka has one function that changes a `DynamicObject` into cells. It has
  selected columns for the common kinds and a NAME/AGE fallback for all other
  kinds. So a CRD that has no renderer still lists, sorts, and filters correctly
  immediately.
- **Flux CD has built-in support, not a plugin.** Press `t` to open a
  Suspend/Resume/Reconcile-now menu. It works for Kustomizations, HelmReleases,
  git/helm/oci repositories, buckets, image automation, and notification
  alerts and receivers. sofka changes `spec.suspend` and the
  `reconcile.fluxcd.io/requestedAt` annotation directly with the Kubernetes API.
  You do not need the `flux` binary. This function also works with bulk
  multiselect.
- **Port-forwards run in the background.** A new port-forward does not stop the
  TUI for its whole lifetime. Type `:pf` to list the active forwards and stop
  each one while the others continue. sofka stops all forwards automatically on
  quit and does not leave them orphaned.
- **Bulk actions with multiselect.** Press `space` to mark rows for delete,
  kill, or Flux suspend/resume/reconcile across many resources at the same time,
  not one row at a time.
- **CRD rows drill into their custom resources**, not their YAML. Press `enter`
  on a CustomResourceDefinition. sofka finds its served version and lists the
  actual objects.
- **Skins, not one fixed palette.** sofka has built-in Catppuccin, Gruvbox,
  Solarized, Nord, Dracula, Tokyo Night, One Dark, Rosé Pine, and Monokai
  palettes. You select one in the config file, with a hex override for each
  swatch. If you configure no skin, sofka detects a light or dark terminal
  background automatically. sofka calculates every semantic color (row status,
  severity badges, headers, and borders) from the active palette. So one skin
  change is consistent everywhere at the same time. Set `background = true` to
  fill the view with the skin background instead of the terminal background.
  Use a light per-context skin to make production easy to identify.
- **A combined row colorer.** sofka tints the whole row by status, like k9s
  (healthy rows, errors, pending, and completed each read as one color). It
  also shows a separate STATUS badge and colors unusual values in the
  RESTARTS, CPU, and MEM columns. So a pod that crash-loops or uses too many
  resources is easy to see in an otherwise uniform row. The warning and
  critical thresholds for RESTARTS, CPU, MEM (and container request/limit) are
  **configurable** for each resource and each context.
- **It explains _why_ something is broken.** Press `X` to open a deterministic
  incident view for the selection that uses evidence: the rollout state, the
  degraded conditions, the blocking pods and their container failure reasons
  (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable pods, failed
  probes), and recent Warning events. It uses no AI and no external service.
  Press `⏎`, `E`, or `l` to go from a finding to the pod, its events, or its
  logs.
- **A session-local timeline.** Press `T` to see the state changes that sofka
  saw for an object during the watch — generation increases, replica and
  readiness changes, pod phase, restart, and waiting-reason changes, and
  condition changes — as a timestamped log. sofka calculates it from the watch
  stream and stores nothing on disk.

## Why it is faster

These are specific design choices that you can check. They are not marketing
numbers.

- **No garbage collector.** The Rust ownership model has no garbage-collector
  pauses. When you watch thousands of pods or custom resources across a large
  cluster, the in-memory store gets larger. But the redraw latency stays
  smooth. A runtime with a garbage collector can get unsteady under constant
  allocation load.
- **Batched redraws.** The event loop reads every pending watch message before
  it triggers one redraw (`while let Ok(m) = rx.try_recv()`). A rollout that
  touches 50 pods costs one render pass, not fifty.
- **Cached row computation.** sofka sorts and fuzzy-filters the visible rows
  again only when the data or the filter text changes. A dirty flag guards the
  cache. sofka does not recompute on every frame or every keystroke across the
  full object set.
- **No subprocess overhead on the hot paths.** Delete, scale,
  suspend/resume/reconcile, and CRD drill-down are direct kube API calls (JSON
  merge-patches over the existing client). They do not fork and run a
  `kubectl` or `flux` process for each action.
- **Generation-tagged streams.** When you change views, sofka does not wait for
  the old watcher to tear down. A generation tag identifies stale messages, and
  sofka drops them the instant a newer watch takes over. So navigation never
  stalls behind a slow stream.

## Features

- **Connect** to the current kubeconfig context. This includes exec credential
  plugins (for example, GKE).
- **API discovery** of every resource type on the cluster, with k9s-style
  short aliases (`po`, `dp`, `svc`, `no`, `cm`, `sts`, `ds`, `ks`, `hr`, and
  more) and correct precedence (the core `pods` wins over `pods.metrics.k8s.io`).
- **Live watch** of any kind through `kube::runtime::watcher`, streamed into an
  in-memory store.
- **Selected columns** for common kinds (pods, deployments, replicasets,
  statefulsets, daemonsets, services, nodes, namespaces, configmaps, secrets,
  jobs, cronjobs, PVC/PV, ingresses, endpoints, CustomResourceDefinitions),
  with a NAME/AGE fallback for all other kinds.
- **Custom views** — you define columns for any resource in the config file
  (`[views]`). sofka extracts each column with a JSON Pointer and sorts by type
  (quantities, numbers, and timestamps sort by value). An unknown custom
  resource uses its CRD `additionalPrinterColumns` automatically. Press `w` to
  show or hide the wide-only columns (kubectl `-o wide`).
- **Live CPU and MEM columns** for pods and nodes from the metrics API, with
  color for unusual values. The pod container picker also shows CPU and memory
  for each container, each container usage as a percent of its request and its
  limit (`-` marks an unset request or limit), and the pod QoS class. All of it
  works correctly when metrics-server is not present.
- **Configurable thresholds** (`[thresholds]`) — the warning and critical
  values for RESTARTS, CPU, memory, and container request/limit color. There
  are global defaults, plus per-resource and per-context overrides.
- **Explain-unhealthy view** (`X` / `:explain`) — a deterministic explanation,
  with evidence, of why the selected object is unhealthy (the rollout state,
  degraded conditions, blocking pods and their container failure reasons, and
  recent Warning events). Press `⏎`, `E`, or `l` to go to the resource, its
  events, or its logs.
- **Session-local timeline** (`T` / `:timeline`) — a per-object timestamped log
  of the state changes that sofka saw during the watch (generation increases,
  readiness changes, and phase, restart, and condition changes). It has a
  maximum size and stays only in memory.
- **Drill-down navigation** with a breadcrumb stack: workload/service → pods,
  node → its pods, pod → containers, namespace → re-scope, CRD → its custom
  resources. Press `esc` to go back.
- **Command palette** (`:`) — fuzzy search over the full resource catalog, the
  built-in commands (`ctx`, `pulse`, `xray`, `explain`, `timeline`, `gitops`,
  `can-i`, `journal`, `diff`, `events`, `pf`), and your saved
  bookmarks/workspaces together. It also does row **filtering** (`/`) with
  matched-character highlighting: fuzzy text, `!text` inverse match, `-l`/`-f`
  label and field selectors (the API server evaluates them on ⏎), and typed
  column comparisons (`status=CrashLoopBackOff`, `cpu>500m`, `memory>1Gi`,
  `restarts>=5`, `age<2h`). Terms with a space between them use AND.
- **Multiselect** (`space`) for bulk delete/kill/suspend/resume/reconcile.
- **Pulse dashboard** (`:pulse`) — cluster-health tiles. sofka refreshes them
  every 5 seconds.
- **Xray tree** (`:xray`) — a hierarchical view from the current kind down
  through owner references to pods and containers.
- **Flux CD controls** (`t`) — a suspend/resume/reconcile menu that uses native
  Kubernetes API patches.
- **CronJob controls** (`t`) — trigger now (creates a Job from the jobTemplate,
  like `kubectl create job --from`), suspend, and resume.
- **Background port-forwards** (`f`/`F` to start, `:pf` to manage).
- **Plugins** — shell-out commands that you define in the config file and bind
  to keys, scoped for each resource. Keys are full **chords** (`ctrl-g`,
  `alt-x`, `shift-b`, `f5`). Commands run in the **terminal**, in a captured
  **popup**, or in the **background** (with a timeout and a maximum output
  size). A command can need **confirmation** or carry a **dangerous** flag. A
  command can declare itself read-only (`mutating = false`) to stay usable in
  `--readonly`. A command can substitute rich placeholders as separate
  arguments (`$NAME`/`$NAMESPACE`/`$CONTEXT`/`$CLUSTER`/`$RESOURCE`/`$GROUP`/
  `$VERSION`/`$KIND`/`$FILTER`). A command can run over every marked row at the
  same time and report partial failures.
- **Bookmarks** (`[[bookmarks]]`) — saved navigation commands that you bind to
  a chord and the command palette. One keystroke goes to a resource, and can
  also change the context or namespace and apply a filter, a sort, and a view.
- **Workspaces** (`[[workspaces]]`) — a named collection of views for one task.
  Open it, then press `Tab` or `Shift-Tab` to cycle its views. You stay in the
  workspace.
- **Diff** (`:diff`) — a unified diff of the live object and its
  `last-applied-configuration`.
- **Events** (`:events` / `E`) — live Kubernetes Events for the selected
  object. sofka filters by UID when the UID is available.
- **GitOps view** (`:gitops` / `:flux`) — for the selected object, the Flux
  ownership and reconciliation chain: the owning Kustomization/HelmRelease, its
  source (GitRepository/OCIRepository/HelmRepository) and its applied revision
  and latest revision, the `dependsOn` edges, and the ready status. Each item is
  a finding with evidence that you can `⏎` to go to.
- **Managed-resource mutation warnings** — sofka warns you first before you
  edit, delete, scale, or otherwise change an object that Flux (or another
  controller) owns. The warning says that the next reconcile will revert your
  change or recreate the object. So you fix the source instead of fighting the
  controller.
- **Action-aware authorization** (`:can-i`) — a `SelfSubjectRulesReview`
  overview of what you can do in the current namespace, plus
  `:can-i <verb> <resource> [ns]` to check a single action before you try it.
  This is the same answer that `kubectl auth can-i` gives, inside the TUI.
- **Declarative guardrails** (`[[guardrails]]`) — rules in the config file that
  match on context/namespace/resource/action globs. A rule can **deny** a
  destructive action outright
  (`delete`/`force-delete`/`drain`/`restart`/`shell`/`debug`/`node-debug`),
  force **type-to-confirm** (the resource name or `context/name`), or cap
  **bulk** operations. So rules like "never delete in prod", "always confirm a
  prod shell", and "no more than one at a time" are enforced, not remembered.
- **Action journal** (`:journal` / `:audit`) — a session-local log in memory of
  every mutating action you did (the action, the target, the context, and the
  time), newest first. It records identifiers only, never secret input or
  decoded values, and never writes to disk.
- **Ephemeral debug containers** (`:debug`) — attach a temporary debug
  container to the selected pod with `kubectl debug`. sofka prompts for the
  image (prefilled with the `[debug]` default). Press `d` in the container
  picker to target the process namespace of one container (`--target`).
  Read-only mode and the `debug` guardrail control this action. sofka records
  it in the journal.
- **Node debug pods** (`:debug` on a node) — start a privileged diagnostic pod
  on the selected node (`kubectl debug node/…`) that mounts the host filesystem
  at `/host` and joins the host namespaces. sofka shows a preview of exactly
  that access and needs your confirmation before it creates the pod. Read-only
  mode and the `node-debug` guardrail control this action. sofka records what it
  started, so `:debug-clean` can remove the debugger pods later.
- **Diagnostic bundles** (`:bundle`) — sofka assembles a redacted incident
  bundle for the selected object into one Markdown document: its (redacted)
  YAML, the owner, the incident explanation, recent events, the session
  timeline, bounded recent logs, and a metrics snapshot. sofka always strips
  Secret `data`/`stringData`, credential-like annotations, and
  `last-applied-configuration`. A manifest lists what the bundle includes and
  what it withholds. Review the bundle in a preview, then use `:bundle-save` to
  write it to a file.
- **Snapshots** (`:snapshot`) — capture the current table view (its columns and
  visible rows, with metadata) to a file as an aligned text table, JSON, or
  YAML. Use `:snapshots` to browse the saved captures (newest first, with age),
  open one in a viewer that is marked stale, and delete one with `d`. This is
  different from the one-frame `--snapshot` CI mode. This is an interactive
  capture-and-review workflow.
- **RBAC-aware palette** — sofka hides the resource kinds that you cannot
  `list`.
- **Namespace switcher** (`n`) with pinned **favourites** (`favorite_namespaces`,
  ★) and per-context session **recents** (·) above the rest, and a **context
  switcher** (`:ctx`).
- **YAML view** (`y`) and **describe** (`d`, through `kubectl`).
- **Logs** (`l`) — per-container on a pod, or aggregated across all matching
  pods on a workload/service. In the logs view, `/` filters: a case-insensitive
  substring (highlighted), a `/regex/`, or `!` to invert. Press `z` to clear
  the buffer. Press `p` to show previous-container logs. The initial tail, the
  follow buffer, and the `since` lookback are configurable (`[logs]`). sofka
  parses ANSI color codes from the source app and maps them onto the active
  skin. It does not print them as literal escapes.
- **VictoriaLogs integration** (`L` / `:vlogs`) — log history for the selected
  pod, container, workload, service, or whole namespace from a VictoriaLogs
  backend: a lookback query and a live tail, in the same logs view. It needs no
  config: sofka finds the VictoriaLogs service in the cluster automatically and
  reaches it through the API-server proxy. Or point `[providers.logs]` at an
  external URL. It covers restarted and deleted pods, because the backend keeps
  what the kubelet no longer has.
- **Right-sizing** (`:rightsize`) — when a Prometheus or VictoriaMetrics backend
  is reachable, sofka estimates right-sized requests for the selected workload
  from past usage: for each container, the current requests, the P50/P95/P99 CPU
  and memory over a window, a suggested request (P95 plus headroom), OOM and
  throttle evidence, and a **patch preview**. It never mutates. sofka finds the
  backend in the cluster automatically (or set `[providers.metrics]`).
- **Fleet dashboard** (`:fleet`) — an opt-in health summary across contexts:
  connectivity, Kubernetes version, node readiness, unhealthy pods, Flux
  failures, and the read-only policy for each configured context, side by side.
  sofka queries only the contexts in `[fleet]`, and gathers each one at the same
  time with its own timeout, so one slow cluster never blocks the rest. Press
  `⏎` to switch to a context. Press `r` to refresh.
- **Compact mode** (`ctrl-e`) — collapse the seven-line header and the footer
  into one info line (kind · count · namespace · context, with a flash and the
  live indicator). So a tiled or multiplexed pane is almost all table.
- **Skinnable** — built-in Catppuccin, Gruvbox, Solarized, Nord, Dracula,
  Tokyo Night, One Dark, Rosé Pine, and Monokai palettes, an auto-detected
  dark/light default, and per-swatch overrides in the config file.
- **Config file** (TOML): aliases, default namespace/resource, favourite
  namespaces, plugins, bookmarks, workspaces, views, thresholds, log provider,
  and skin — with per-cluster and per-context overrides and live `:reload`.
- **Runtime diagnostics** (`:info`, or `sofka --info`) — the version and build,
  the config sources, the live context/cluster/API server, the discovery and
  Metrics API status, the watch error counts, and the state/snapshot/bundle
  directories. It prints only identifiers and counts, never credentials,
  tokens, or Secret values.

## Installation

### Download from Github

Each [GitHub release](https://github.com/nklmilojevic/sofka/releases) has
prebuilt binaries for macOS (aarch64/x86_64) and Linux (aarch64/x86_64).

### Nix

Nix users can run sofka directly. You do not need to install anything:

```sh
nix run github:nklmilojevic/sofka
```

### Cargo

```sh
cargo install sofka
```

Or build from source (see [Development](#development)).

### macOS: "cannot be opened because the developer cannot be verified"

The release binaries are not signed or notarized yet. So if you download a
tarball in a browser and extract it, Gatekeeper refuses to run it. This is
expected. It is not a broken build. Clear the quarantine flag one time:

```
xattr -d com.apple.quarantine sofka
```

(Or right-click the binary in Finder, select Open, and confirm in the dialog
one time.) **Signing and notarization are planned for the next release.** Then
this step is not necessary.

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

See [Plugins](#plugins) below for all plugin options: output modes,
placeholders, and bulk operation over marked rows.

### Custom views

Define table columns for any resource. This is most useful for custom resources,
because they use the NAME/AGE fallback. sofka keys views by apiVersion/plural
(`"cert-manager.io/v1/certificates"`, `"v1/pods"`), group/plural, bare plural,
or lowercase kind. The most specific key wins.

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

`path` is a JSON Pointer (RFC 6901) into the object as the API serves it:
`/metadata/…`, `/spec/…`, `/status/…`, and array indices like
`/status/conditions/0/status`. The column `type` is `text` (default), `status`,
`number`, `quantity` (`500m`, `1Gi`), or `time`. Typed columns sort by value,
not by text. The optional `width` (for fixed columns) and `align`
(`left`/`center`/`right`) tune the layout. By default, columns overlay the
selected ones: a header that matches replaces it in place, and new columns go
before AGE. sofka skips invalid entries and shows a warning in the app. They
never stop the TUI.

A custom resource that has no explicit view uses its CRD
`additionalPrinterColumns` automatically (columns with `priority > 0` become
wide-only). So most custom resources get useful columns with no configuration.

### Thresholds

The warning and critical values behind the RESTARTS/CPU/MEM cell color (and the
request/limit utilization in the container picker) are configurable. Any value
that you do not set keeps the sofka default, so an empty config colors exactly
as before. Global `[thresholds]` apply everywhere. `[thresholds.resources.<key>]`
overrides them for each resource (keyed like `[views]`). Like every section, a
per-cluster or per-context override file can retune them for one context.
Thresholds also apply again live on `:reload`.

```toml
[thresholds]
restarts    = { warn = 3, critical = 10 }      # count
cpu         = { warn = "200m", critical = "1" } # absolute usage
memory      = { warn = "256Mi", critical = "1Gi" }
utilization = { warn = 75, critical = 90 }     # percent of request/limit

[thresholds.resources.pods]                    # per-kind override
restarts = { warn = 5, critical = 20 }
```

You can omit either bound of a band to disable that level. `warn` is peach.
`critical` is red.

### Plugins

`[[plugins]]` binds a shell-out command to a key. `key` is a **chord**: a single
character (`"g"`), a modifier combination (`"ctrl-g"`, `"alt-x"`, `"shift-b"`),
or a function or named key (`"f5"`, `"ctrl-f2"`). A built-in key wins over a
plugin on the same chord.

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

- **Placeholders** are substituted as whole arguments. sofka never splices them
  into a shell string. They are `$NAME`, `$NAMESPACE`/`$NS`, `$CONTEXT`,
  `$CLUSTER`, `$RESOURCE` (plural), `$GROUP`, `$VERSION`, `$KIND`, `$FILTER`.
- **`output`**: `terminal` (default, interactive — it suspends the TUI), `popup`
  (captured off-thread into a scrollable view), or `background` (detached — a
  notification flashes when it completes). `popup` and `background` obey
  `timeout` (`"30s"`, default) and bound the captured output.
- **`mutating`** (default `true`): read-only mode blocks a mutating plugin. Set
  it to `false` to allow a known read-only one.
- **`confirm`**/**`dangerous`**: prompt before the command runs, and show the
  exact executable and arguments. `dangerous` also shows ⚠.
- **`shell = true`**: opt into `sh -c`. sofka still passes placeholders as
  positional parameters (`$1`, `$2`, …). It never interpolates them into the
  script.
- **Bulk**: with rows marked (`space`), a `popup` or `background` plugin runs
  over every marked row and reports partial failures. An interactive `terminal`
  plugin cannot run over a set and refuses a marked run.

For an invalid value (a bad chord, an unknown `output`, or a malformed
`timeout`), sofka disables just that plugin or uses the default, and shows a
warning in `:config`. Plugins appear in `?` help with their chord and scope.

### Bookmarks

`[[bookmarks]]` are saved navigation commands. One keystroke goes to a resource,
and can also change the context or namespace and apply a filter, a sort, and a
view. An optional key chord triggers a bookmark. Bookmarks are always in the
command palette (`★`, ranked above resources).

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

`[[workspaces]]` group several views into a named set for one task (checkout
ops, a cluster upgrade, or cert renewal). Open one with a chord or the palette
(`▦`). sofka changes its optional context one time and shows the first view.
Press **`Tab`** or **`Shift-Tab`** to cycle the other views. You stay in the
workspace.

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

`[[guardrails]]` make rules like "never delete in prod", "always confirm
drains", and "no more than 5 at a time" into enforced rules. You do not have to
remember them. Each rule matches on `contexts`, `namespaces`, `resources`, and
`actions` globs (all optional; an omitted glob matches everything). Then it
applies the strictest of these: `deny` (block the action), `confirmation` (type
to confirm), and `max_bulk` (a maximum number of rows for one action). The
gated `actions` are the destructive verbs that sofka does directly — `delete`,
`force-delete`, `drain`, `restart`, `shell` (exec), `debug`, and `node-debug`.
The first rule that matches wins. sofka shows the `reason` when a rule fires.

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

### Debug containers and pods

`:debug` on a **pod** attaches a temporary ephemeral debug container with
`kubectl debug`. sofka prompts for the image (prefilled with `image` below). An
empty `command` starts an interactive shell (bash if the image has it, or else
sh), like the pod shell. Press `d` in the container picker to set
`--target=<container>`, so the debug container shares the process namespace of
that container. The ephemeral container stays on the pod until the pod is
recreated. Kubernetes cannot remove it, so sofka has nothing to clean up.

`:debug` on a **node** starts a privileged diagnostic pod on it
(`kubectl debug node/<node>`, image `node_image` in `node_namespace`, optional
`node_profile`). This pod mounts the host filesystem at `/host` and joins the
host PID, network, and IPC namespaces. So sofka shows a preview of exactly that
access and makes you confirm before it creates the pod. sofka records the node
debuggers that it starts this session. `:debug-clean` deletes them (matched by
the `node-debugger-*` name and the node). kubectl leaves the pod after you exit,
so clean up when you finish.

```toml
[debug]
image = "nicolaka/netshoot:latest"       # ephemeral (in-pod) debug image
command = ["bash"]                       # entrypoint; omit for an interactive shell
node_image = "nicolaka/netshoot:latest"  # node debug pod image
node_namespace = "default"               # namespace the node debugger lands in
node_profile = "sysadmin"                # kubectl debug --profile (optional)
```

Read-only mode and guardrails disable both actions: the `debug` action for
pods, and the `node-debug` action for nodes.

### Diagnostic bundles

`:bundle` assembles a redacted incident bundle for the selected object — its
YAML, the owner, the incident explanation, recent events, the session timeline,
bounded recent logs, and a metrics snapshot — into one Markdown document. It
helps you hand off an incident between application and platform teams. sofka
gathers it off-thread and shows it in a preview. Then `:bundle-save` writes it
to a temp file.

sofka always redacts these items: Secret `data`/`stringData` values, any
credential-like annotation (a key that contains `token`, `password`, `secret`,
`apikey`, `credential`, and similar), and `last-applied-configuration`. It
replaces them with a placeholder. It drops `managedFields`. It flags env vars
that come from Secrets (their values are references, not literals). Every
bundle has a manifest of exactly what it includes and what it withholds.

```toml
[bundle]
anonymize = false   # replace context/cluster identity with placeholders
log_lines = 200     # max recent log lines per pod
max_pods = 3        # cap how many pods contribute logs
```

### Snapshots

`:snapshot` captures the current table view — its columns and visible rows, plus
metadata (context, cluster, namespace, resource, filter, and timestamp) — to a
file. An optional argument sets the format: `text` (default; an aligned table
with a header block), `json`, or `yaml`. sofka writes the files to
`$XDG_STATE_HOME/sofka/snapshots` (or `~/.local/state/sofka/snapshots`).

`:snapshots` browses the saved captures, newest first with their age. Press `⏎`
to open one in a viewer with a staleness banner (it is a point-in-time
capture). Press `d` to delete the highlighted file. This is different from the
one-frame `--snapshot` CI flag. This is an interactive capture-and-review
workflow.

### Log controls

The kubelet logs view (`l`) keeps a bounded follow buffer. Tune the initial
tail, the buffer size, and an optional `since` lookback:

```toml
[logs]
tail = 300       # initial lines fetched per stream (kubectl --tail)
buffer = 5000    # max lines kept while following (oldest dropped)
since = "1h"     # optional: only logs newer than this — replaces tail
```

In the view, `/` filters with a case-insensitive substring, a `/regex/`, or a
leading `!` to invert (keep the lines that do not match). sofka flags a
malformed regex instead of hiding everything. Press `z` to clear the on-screen
buffer (the live stream continues to append). A pod streams the logs of every
container at the same time.

### Fleet dashboard

`:fleet` summarizes several clusters side by side. You do not switch through
them. It is **opt-in**: sofka queries only the kubeconfig contexts that you
list.

```toml
[fleet]
contexts = ["prod-eu", "prod-us", "staging"]
```

sofka gathers each context at the same time (bounded, with a per-context
timeout). So an unreachable or slow cluster shows an error on its own row and
does not block the others. Each row shows connectivity, Kubernetes version,
node readiness, the unhealthy pod count, the Flux `Ready=False` failures, and
the resolved read-only policy. Press `⏎` to switch to the highlighted context
(through the normal context-switch path). Press `r` to gather again. sofka
keeps only these non-sensitive summaries in memory.

### Right-sizing (metrics provider)

`:rightsize` on a workload (or pod) estimates right-sized requests from past
usage in a **Prometheus-compatible** backend — Prometheus or VictoriaMetrics,
which share the query API. For each container it shows the current requests, the
P50/P95/P99 CPU and memory over the window, a suggested request (P95 plus
headroom), OOM and throttle evidence, and a **strategic-merge patch preview**
(press `c` to copy). It **never mutates**. Apply the patch yourself with
`kubectl patch` if you agree.

It needs no config by default: with no `[providers.metrics]` section, sofka
finds a Prometheus or VictoriaMetrics query `Service` in the cluster
automatically (by well-known labels) and reaches it through the API-server
proxy, like the log provider. Configure it only to point at an external
endpoint or to tune the window or headroom:

```toml
[providers.metrics]
type = "prometheus"        # or "victoriametrics" (same query API)
url = "https://prom.example.com"   # omit to autodiscover in-cluster
window = "7d"              # lookback for the P50/P95/P99 quantiles
step = "5m"                # subquery resolution for the CPU rate()
headroom = 15              # percent added over P95 for the suggestion

[providers.metrics.headers]        # optional
Authorization = "Bearer <token>"
```

It uses the standard cAdvisor metric names
(`container_cpu_usage_seconds_total`, `container_memory_working_set_bytes`,
`container_oom_events_total`, `container_cpu_cfs_throttled_periods_total`).
VictoriaMetrics **cluster** mode (vmselect) needs a tenant path in the `url`.
Single-node VM and Prometheus serve the API at the root and autodiscover
correctly.

### Log provider (VictoriaLogs)

`L` (or `:vlogs`) opens log history for the selection from a VictoriaLogs
backend instead of the kubelet. With no configuration, sofka finds the
VictoriaLogs `Service` in the cluster by its well-known labels (Helm charts and
the VictoriaMetrics operator). It queries the service through the Kubernetes
API-server service proxy and reuses your kubeconfig credentials. Configure it
only to point at an external endpoint or to change the defaults:

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

Like every section, `[providers.logs]` can live in a per-cluster or per-context
override file. So each cluster can use its own backend.

### Per-cluster / per-context overrides

You can override any option for a specific cluster or kubeconfig context, like
k9s. Put partial config files under `clusters/`:

```
~/.config/sofka/
├── config.toml                # base, applies everywhere
└── clusters/
    └── prod-cluster/          # kubeconfig *cluster* name
        ├── config.toml        # every context on prod-cluster
        └── prod-admin/        # kubeconfig *context* name
            └── config.toml    # that context only
```

Overrides merge over the base config (the cluster level first, then the context
level). Tables like `[aliases]` and `[skin.colors]` merge key by key. Everything
else — strings, booleans, and arrays like `[[plugins]]` — replaces the base
value. Directory names are the kubeconfig names. sofka replaces any character
that is not a letter, a digit, `.`, `_`, or `-` with `-`. So an EKS context
`arn:aws:eks:eu-west-1:123456789:cluster/prod` becomes the directory
`arn-aws-eks-eu-west-1-123456789-cluster-prod`.

```toml
# clusters/prod-cluster/config.toml — make prod unmistakable and hands-off
readonly = true

[skin]
name = "catppuccin-latte"
background = true
```

A skin in an override sets the colors for that context. A context that has no
skin keeps the session skin (the config `skin.name`, the auto-detected default,
or your last `:skin` choice). sofka reads the overrides again on every `:ctx`
switch. So edits apply without a restart.

### Headless modes (no TTY required)

```
sofka --check                # connect, run discovery, print a summary, exit
sofka pods --snapshot        # render one frame of a resource view to stdout
sofka dp -A --snapshot       # deployments, all namespaces
sofka --info                 # version/build, config sources, dirs, kubeconfig context (no connection)
```

These also work as CI smoke tests. `--info` prints only identifiers and paths,
never credentials, tokens, or Secret values.

## Usage

```
sofka [RESOURCE] [-n NAMESPACE] [-A] [--readonly | --write]

  RESOURCE          resource to open (alias/plural/kind), default: pods
  -n, --namespace   namespace to start in
  -A, --all-namespaces
  --readonly        disable every mutating action for the session
  --write           force write mode, overriding any config `readonly`
```

`--readonly` and `--write` set the mode for the whole session. They win over the
config `readonly` option — and over per-cluster and per-context overrides — on
every `:ctx` switch. With no flag, a switch into a context whose config sets
`readonly = true` enables read-only mode (shown as `[read-only]` in the header).
A switch away from it restores write mode.

### Keys

| Key                                           | Action                                                                                                                                |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `:<resource>`                                 | command palette - fuzzy over kinds and built-in commands                                                                              |
| `:<resource> <ns>`                            | switch kind and namespace at once (`:deploy social`; `all`/`*` = all namespaces; the namespace tab-completes)                         |
| `[` / `]`                                     | view history - back / forward through visited kind+namespace views                                                                    |
| `Tab` / `shift-Tab`                           | cycle views of the active workspace (when one is open)                                                                                |
| `enter`                                       | drill down (workload/svc → pods, node → its pods, pod → containers, ns → re-scope, CRD → its resources)                               |
| `esc`                                         | go back / pop the view stack / clear filter / clear marks                                                                             |
| `j`/`k`, `↓`/`↑`, `g`/`G`                     | navigate                                                                                                                              |
| `S` / `I`                                     | sort-column picker (fuzzy; ⏎ on the active column inverts) / invert sort direction                                                    |
| `ctrl-e`                                      | compact mode: collapse the header + footer (for tiled/multiplexed panes)                                                              |
| `space`                                       | mark/unmark row for bulk actions                                                                                                      |
| `/`                                           | filter: fuzzy text · `!inverse` · `-l`/`-f` selectors (server-side on ⏎) · `status=X` `cpu>500m` `age<2h`                             |
| `n` / `0`                                     | namespace switcher / all namespaces                                                                                                   |
| `shift-j`                                     | jump to owner/controller                                                                                                              |
| `o`                                           | show the node hosting the selected pod                                                                                                |
| `ctrl-r`                                      | refresh the watch                                                                                                                     |
| `y` / `d` / `E`                               | view YAML / describe (`kubectl`) / live events                                                                                        |
| `X` / `T`                                     | explain why the selection is unhealthy / session-local state-change timeline                                                          |
| `:gitops` / `:flux`                           | Flux owner, source, revisions & reconciliation chain for the selection (`⏎` to jump)                                                  |
| `:can-i` / `:can-i <verb> <resource> [ns]`    | what you can do here / check a single action (`SelfSubjectAccessReview`)                                                              |
| `:journal` / `:audit`                         | session-local log of the mutating actions you've taken                                                                                |
| `:rightsize`                                  | historical right-sizing: P50/P95/P99 usage → suggested requests + patch preview (needs a metrics backend)                             |
| `:ctx` / `:ctx <name>`                        | context switcher popup / switch directly (the name tab-completes)                                                                     |
| `:fleet`                                      | cross-context health dashboard (opt-in `[fleet]` contexts; `⏎` switches, `r` refreshes)                                               |
| `:skin`                                       | switch the color skin live (`:skin gruvbox-dark` applies directly)                                                                    |
| `:reload` / `:config` / `:info`               | reload config from disk · config sources + warnings · runtime diagnostics                                                             |
| `l` / `p`                                     | logs (workload = all matching pods) / previous-container logs                                                                         |
| `c`                                           | copy resource name to clipboard                                                                                                       |
| `e`                                           | edit in `$EDITOR` (`kubectl edit`)                                                                                                    |
| `s`                                           | shell into pod / scale a workload (context-dependent)                                                                                 |
| `a`                                           | attach to pod                                                                                                                         |
| `:debug`                                      | pod: ephemeral debug container (`d` in the picker targets one) · node: privileged debug pod (previewed + confirmed)                   |
| `:debug-clean`                                | delete the node debugger pods launched this session                                                                                   |
| `:bundle` / `:bundle-save`                    | assemble a redacted diagnostic bundle for the selection · write the previewed bundle to a file                                        |
| `:snapshot [text\|json\|yaml]` / `:snapshots` | capture the current view to a file · browse, open, and delete saved snapshots                                                         |
| `i`                                           | set container image                                                                                                                   |
| `r`                                           | rollout restart (workloads) / refresh (elsewhere)                                                                                     |
| `f` / `shift-f`                               | port-forward (pods/services) - runs in the background                                                                                 |
| `t`                                           | Flux: suspend/resume/reconcile menu · CronJobs: trigger/suspend/resume menu                                                           |
| `C` / `U` / `D`                               | nodes: cordon / uncordon / drain                                                                                                      |
| `ctrl-d` / `ctrl-k`                           | delete / force-delete (marked rows, or current); in confirm: `f` toggles force, `c` cycles cascade (background → foreground → orphan) |
| `:q`, `ctrl-c`                                | quit                                                                                                                                  |
| `?`                                           | help                                                                                                                                  |
| _(config)_                                    | plugin / bookmark / workspace key chords — `ctrl-`/`alt-`/`shift-`/`fN`; listed in `?` help                                           |

**Logs view:** `/` filter (substring · `/regex/` · `!invert`) · `s` autoscroll
· `w` wrap · `t` timestamps · `x` stop/resume stream · `z` clear buffer · `c`
copy buffer · `ctrl-s` save to file · `esc` back. The newest line anchors to
the bottom of the viewport.

**Document views** (YAML, describe, diff, events): `/` searches like vim. The
whole document stays on screen, and sofka highlights every match. Press `n` or
`N` to go to the next or previous match. Press `w` to wrap. Press `c` to copy
the document. Press `esc` to back out (the first press clears an active search).
In the `?` help panel, `/` filters instead and narrows to the matching
keybinds.

**Explain view** (`X`): press `j` or `k` to move. Press `⏎` to go to the
resource behind a finding (a blocking pod). Press `E` for its events. Press `l`
for its logs. Press `r` to gather again. Press `esc` to go back. A finding that
you can go into has a trailing `→`.

Interactive actions (`e`, `s` for shell, `a`) suspend the TUI and shell out to
`kubectl`. Delete, scale, restart, set-image, suspend, resume, reconcile, and
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
`mpsc::UnboundedSender`. The main `tokio::select!` loop folds them into the
`Store`. It batches any other queued updates before it redraws. It shares that
same loop with terminal input and a 1s tick (age columns and dead port-forward
reaping). So the UI never blocks on the network.

## Development

```
cargo run -- pods            # run against current context
cargo test                   # unit tests (no cluster required)
cargo clippy --all-targets   # lints (clean)
```

## Release

After you merge the release-ready changes to `main`, run one of:

```
just release-patch
just release-minor
just release-major
```

The recipe switches to a clean, up-to-date `main`. It bumps `Cargo.toml` and
`Cargo.lock`. It commits and pushes the version bump. Then it creates the GitHub
Release. The release workflow runs from that published release. It uploads the
platform binaries, publishes to crates.io, and warms the Nix cache.

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

- [x] Ephemeral container workflow.
- [x] Node debug pod workflow.
- [x] Redacted diagnostic bundles.
- [x] Screen dumps and structured snapshots.
- [x] Runtime diagnostics. _(Structured application logging is still to come.)_
- [x] Richer log controls.

### Milestone 6: fleet and integrations

- [x] Opt-in cross-context health dashboard (`:fleet`).
- [x] Historical metrics provider interface (Prometheus/VictoriaMetrics:
      autodiscovery or configured URL).
- [x] Log provider interface (VictoriaLogs: autodiscovery or configured URL).
- [ ] Trace provider interface.
- [ ] Extended relationship graph.
- [ ] Vulnerability scanner integration.
- [x] Historical right-sizing recommendations (`:rightsize`).

### Milestone 7: distribution polish

- [ ] Signed and notarized macOS releases.
- [ ] Homebrew distribution.
- [ ] Checksums, attestations, and SBOM.
- [ ] Evaluate Windows support.
- [ ] Document a Kubernetes compatibility matrix.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. This is the standard for the Rust ecosystem.
