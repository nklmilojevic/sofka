# Features

The full list. For how sofka compares to k9s, see [vs k9s](vs-k9s.md).

## Core navigation

- **Connect** to the current kubeconfig context, including exec credential
  plugins (GKE, EKS, and friends).
- **API discovery** of every resource type on the cluster, with k9s-style short
  aliases (`po`, `dp`, `svc`, `no`, `cm`, `sts`, `ds`, `ks`, `hr`, …) and correct
  precedence - core `pods` wins over `pods.metrics.k8s.io`.
- **Live watch** of any kind through `kube::runtime::watcher`, streamed into an
  in-memory store.
- **Curated columns** for common kinds (pods, deployments, replicasets,
  statefulsets, daemonsets, services, nodes, namespaces, configmaps, secrets,
  jobs, cronjobs, PVC/PV, ingresses, endpoints, CustomResourceDefinitions), with
  a NAME/AGE fallback for everything else.
- **Custom views** - define columns for any resource in the config file. An
  unknown custom resource picks up its CRD `additionalPrinterColumns`
  automatically. `w` toggles wide-only columns (kubectl `-o wide`). See
  [Views and thresholds](views.md).
- **Drill-down navigation** with a breadcrumb stack: workload/service → pods,
  cronjob → its jobs, node → its pods, pod → containers, namespace → re-scope,
  CRD → its custom resources. `esc` goes back.
- **Command palette** (`:`) - fuzzy search over the full resource catalog, your
  saved bookmarks and workspaces, and the built-in commands (`ctx`, `helm`,
  `pulse`, `xray`, `explain`, `timeline`, `gitops`, `can-i`, `journal`, `debug`,
  `debug-clean`, `bundle`, `bundle-save`, `snapshot`, `snapshots`, `diff`,
  `events`, `pf`, `notify`, `find`, `vlogs`, `rightsize`, `fleet`, `skin`,
  `reload`, `config`, `info`).
- **Filtering** (`/`) with matched-character highlighting: fuzzy text, `!text`
  inverse match, `-l`/`-f` label and field selectors (evaluated server-side on
  ⏎), and typed column comparisons (`status=CrashLoopBackOff`, `cpu>500m`,
  `memory>1Gi`, `restarts>=5`, `age<2h`). Space-separated terms AND together.
- **Global fuzzy find** (`:find <text>`) - search object names across the common
  kinds (workloads, pods, services, config, ingresses, jobs, storage, nodes,
  namespaces, Flux objects) in every namespace at once, concurrently. Results
  rank by fuzzy score, `⏎` jumps to the object. When a kind can't be listed
  (RBAC), the result says it's incomplete instead of pretending otherwise.
- **Multiselect** (`space`) for bulk delete/kill/suspend/resume/reconcile.
- **Copy to clipboard** - `c` copies the selected resource's name; `Y` opens a
  field picker over the selected row's displayed columns (full values, never
  the width-truncated cell text) - type to match a column name or its value
  (an IP, an image, a node), `⏎` copies it. Falls back to OSC 52 on remote
  terminals without a local clipboard tool.
- **RBAC-aware palette browse** - the empty `:` list hides kinds you cannot
  `list`. An explicit search checks the full discovery catalog, because some
  delegated authorizers return incomplete rule reviews.
- **Namespace switcher** (`n`) with pinned favourites (★) and per-context
  session recents (·) above the rest, plus a context switcher (`:ctx`). The
  last namespace picked in each context is remembered across restarts
  (`<state-dir>/namespaces.toml`); `-n`/`-A` override it for a session.
- **Mouse support** - the wheel scrolls every view (one notch is three steps of
  that view's own up/down), clicking a row selects it, clicking a column header
  sorts by it (click again to flip). Document views (YAML/describe, diff,
  events, logs, help) release the mouse automatically so click-drag selects
  text natively; the wheel still scrolls them in terminals that translate it to
  arrow keys in the alternate screen (kitty, Ghostty, iTerm2, ...). Set
  `mouse = false` to keep the terminal's native mouse behavior everywhere.
  sofka also releases the mouse while a suspended command (`kubectl exec`,
  `$EDITOR`) runs.
- **Compact mode** (`ctrl-e`) - collapse the seven-line header and the footer
  into one info line (kind · count · namespace · context, with a flash and the
  live indicator), so a tiled pane is almost all table.

## Metrics and health

- **Live CPU and MEM columns** for pods and nodes from the metrics API, colored
  on unusual values. Nodes also get **%CPU and %MEM of allocatable**
  (`status.allocatable` - the pool the scheduler hands out), colored by the
  `utilization` thresholds and sortable, so "which node is full" is one glance
  and one `S`. The container picker shows per-container CPU and memory, usage as
  a percent of request and of limit (`-` marks an unset one), and the pod QoS
  class. All of it degrades cleanly when metrics-server isn't installed.
- **Configurable thresholds** for the RESTARTS/CPU/MEM/request-limit coloring,
  globally and per resource and per context. See
  [Views and thresholds](views.md#thresholds).
- **Workload health at a glance** - Deployments, StatefulSets, DaemonSets, and
  ReplicaSets carry a STATUS column derived from their replica counts and
  conditions (`Ready`, `Progressing`, `Degraded`, `Unavailable`, `Stalled`,
  `ScaledDown`, `Terminating`), and the whole row is tinted by it - so a
  workload whose pods are crashing or whose desired replicas aren't met reads
  red/peach in the list, like k9s, instead of looking uniformly healthy.
- **Explain-unhealthy view** (`X` / `:explain`) - a deterministic, evidence-based
  explanation of why the selection is unhealthy: rollout state, degraded
  conditions, blocking pods and their container failure reasons
  (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable, failed probes),
  and recent Warning events. No AI, no external service. `⏎`, `E`, or `l` jumps
  from a finding to the pod, its events, or its logs.
- **Session-local timeline** (`T` / `:timeline`) - a per-object timestamped log
  of every state change the watch saw: generation bumps, replica and readiness
  changes, pod phase, restarts, waiting reasons, condition flips. Computed from
  the watch stream, bounded, never written to disk.
- **Pulse dashboard** (`:pulse`) - cluster-health tiles, refreshed every 5s.
- **Xray tree** (`:xray`) - a hierarchical view from the current kind down
  through owner references to pods and containers.
- **Watch notifications** (`:notify`) - toggle a notification on the selected
  object and Sophie watches it for you. See [Notifications](debugging.md#notifications).

## GitOps and Helm

- **Flux CD controls** (`t`) - a suspend/resume/reconcile-now menu built on
  native Kubernetes API patches, for Kustomizations, HelmReleases, git/helm/oci
  repositories, buckets, image automation, and notification alerts and
  receivers. No `flux` binary needed. Works with bulk multiselect. `⏎` on a
  **HelmRelease** opens the revision history of the Helm release it manages
  (resolved the way helm-controller composes `releaseName`/`storageNamespace`):
  `⏎` shows a revision's values, `y` the rendered manifest, `d` the NOTES, `r`
  rolls back.
- **Argo CD controls** (`t`) - a suspend/resume/sync-now menu for ArgoCD
  Applications, and a suspend/resume menu for ApplicationSets, built on native
  Kubernetes API patches. Suspend removes `spec.syncPolicy.automated` and
  stashes the original value (including `prune`/`selfHeal`/`allowEmpty`) as a
  base64 annotation so resume restores it exactly; ApplicationSet suspend sets
  `applicationsSync` to `create-only` (no `none` mode exists) and stashes the
  original value the same way. Sync-now patches the top-level `operation`
  field. No `argocd` binary needed. Works with bulk multiselect.
- **GitOps view** (`:gitops` / `:flux`) - the Flux ownership and reconciliation
  chain for the selection: the owning Kustomization/HelmRelease, its source
  (GitRepository/OCIRepository/HelmRepository) with applied and latest revision,
  the `dependsOn` edges, and ready status. Each item is a finding you can `⏎`
  into.
- **Native Helm inspector** (`:helm` / `:hm`) - sofka decodes Helm's release
  storage Secrets directly (double base64 → gunzip → JSON, same as Helm) and
  lists one row per release at its latest revision, like `helm list`. `⏎` opens
  the full revision history (`helm history`); on a revision, `⏎` shows
  user-supplied values, `y` the rendered manifest, `d` the NOTES.txt. `r` rolls
  back and `ctrl-d` uninstalls - those two shell out to the real `helm` binary,
  all the inspection is native.
- **Managed-resource mutation warnings** - before you edit, delete, scale, or
  otherwise change an object Flux (or another controller) owns, sofka tells you
  the next reconcile will revert it or recreate it. Fix the source instead of
  fighting the controller.

## Actions

- **CronJob controls** (`t`) - trigger now (creates a Job from the jobTemplate,
  like `kubectl create job --from`), suspend, resume.
- **Background port-forwards** (`f`/`F` to start, `:pf` to manage) plus **saved
  forwards** that show up in `:pf` even while stopped, with optional autostart.
  See [Saved forwards](plugins.md#saved-forwards).
- **File transfer** (`t` on a pod, or `t` in the container picker for one
  container) - download from or upload to a pod via `kubectl cp`, off-thread
  with a completion flash. Uploads are gated by the `transfer` guardrail and
  read-only mode.
- **Ephemeral debug containers** and **node debug pods** (`:debug`). See
  [Debug containers and pods](debugging.md#debug-containers-and-pods).
- **Logs** (`l`) - per-container on a pod, or aggregated across all matching
  pods on a workload/service, with filtering, previous-container logs, and
  configurable tail/buffer/lookback. sofka parses ANSI color from the source app
  and maps it onto the active skin instead of printing literal escapes. See
  [Log controls](debugging.md#log-controls).
- **VictoriaLogs integration** (`L` / `:vlogs`) - log history from a
  VictoriaLogs backend for a pod, container, workload, service, or whole
  namespace, covering restarted and deleted pods. Zero config: sofka finds the
  service in-cluster and reaches it through the API-server proxy. See
  [Providers](providers.md#log-provider-victorialogs).
- **Right-sizing** (`:rightsize`) - estimate right-sized requests from past
  usage in a Prometheus or VictoriaMetrics backend, with a patch preview. Never
  mutates. See [Providers](providers.md#right-sizing-metrics-provider).
- **Fleet dashboard** (`:fleet`) - an opt-in health summary across contexts,
  side by side. Contexts come from config or `space` in the `:ctx` switcher.
  See [Providers](providers.md#fleet-dashboard).
- **YAML view** (`y`), **describe** (`d`, via `kubectl`), **events**
  (`:events` / `E`, filtered by UID when available), and **diff** (`:diff`).
- **Diff on GitOps clusters** - `:diff` shows a unified diff of the live object
  against its `last-applied-configuration`. When that annotation is missing - as
  it is for every Flux- or Helm-managed object, which nothing ever
  `kubectl apply`s - sofka diffs against the previous revision this session's
  watch saw, so "what just changed?" has an answer. The last revision of up to
  256 changed objects is kept in memory.

## Safety

- **Read-only mode**, **declarative guardrails**, **action-aware authorization**
  (`:can-i`), and a session-local **action journal** (`:journal`). See
  [Safety](safety.md).

## Extensibility

- **Plugins** - shell-out commands bound to key chords, scoped per resource,
  with terminal/popup/background output modes, confirmation and dangerous flags,
  read-only declarations, rich placeholders, and bulk execution over marked
  rows. See [Plugins](plugins.md).
- **Bookmarks** - saved navigation commands on a chord and in the palette.
- **Workspaces** - a named set of views for one task, cycled with `Tab`.
- **Skins** - built-in Catppuccin, Gruvbox, Solarized, Nord, Dracula, Tokyo
  Night, One Dark, Rosé Pine, Rosé Pine Dawn, Monokai, and Flexoki palettes,
  auto dark/light detection,
  and per-swatch hex overrides. Every semantic color (row status, severity
  badges, headers, borders) is derived from the active palette, so one skin
  change lands everywhere at once.
- **Config file** (TOML) with per-cluster and per-context overrides and live
  `:reload`. See [Configuration](configuration.md).

## Diagnostics

- **Diagnostic bundles** (`:bundle`, `:bundle-save`) - a redacted incident
  bundle for the selection as one Markdown document. See
  [Diagnostic bundles](debugging.md#diagnostic-bundles).
- **Snapshots** (`:snapshot`, `:snapshots`) - capture the current table view to
  text, JSON, or YAML, then browse and open saved captures. See
  [Snapshots](debugging.md#snapshots).
- **Runtime diagnostics** (`:info`, or `sofka --info`) - version and build,
  config sources, live context/cluster/API server and Kubernetes revision,
  discovery and Metrics API status, watch error counts, and the
  state/snapshot/bundle directories. The connected Kubernetes revision also
  stays visible in the main header.
  Identifiers and counts only, never credentials, tokens, or Secret values.
