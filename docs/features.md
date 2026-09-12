# Features

The full list. For how sofka compares to k9s, see [vs k9s](vs-k9s.md).

## Node drain options

Press `D` on a node to set options for the current node or marked nodes. The form
shows the targets and requires confirmation before it sends mutation requests.
Read-only mode and configured confirmation rules still apply.

- **Ignore DaemonSets** starts on. DaemonSet pods stay on the node. Turn this off
  to block drain when an eligible DaemonSet pod is present.
- **Force** starts off. Turn it on to permit pods without a live controller.
  Pod replacement is not assured. Force does not change the grace period or
  bypass PodDisruptionBudgets. Without Force, sofka must be able to read each
  pod controller and verify its UID.
- **Delete emptyDir data** starts off. Turn it on to permit removal of pods with
  emptyDir volumes and loss of their local data.
- **Disable eviction** starts off. Turn it on to use pod deletion and bypass
  PodDisruptionBudgets. Failed eviction requests never cause an automatic switch
  to deletion. Eviction and deletion requests both use pod UID checks.
- **Grace period** is empty by default and uses each pod's setting. Enter a
  nonnegative whole number of seconds to override it. Zero requests immediate
  termination.
- **Timeout** is empty or `0` for unlimited waiting. Use durations such as `30s`,
  `5m`, or `1h30m`. One deadline covers all selected nodes, including requests,
  retries, and waiting. Individual API requests have a 30-second limit.

Use `Tab` or the arrow keys to select a field, `Space` to toggle a checkbox, and
text input to edit a duration. `Backspace` removes a character; `Ctrl-U` clears
the field. `Enter` reviews the options. `PgUp` and `PgDn` scroll the form,
confirmation, and progress. Risk options reset to off for each new operation.

Nodes are processed in name order, one at a time. For each node, sofka cordons
it, lists its pods, and checks all eligible pods before the first pod removal.
Mirror pods and completed pods are excluded. Pods already terminating are
included when checking completion. Temporary eviction failures, including
PodDisruptionBudget rejection, are retried with delays from 1 to 10 seconds.
Permanent API failures stop the operation. Progress shows the current node,
remaining pod count, and retry reason.

Sofka reports a node as drained only after its eligible pods are gone. On failure,
timeout, or cancellation, it stops before starting another node. The result lists
completed, incomplete, and unstarted nodes and nodes that remain cordoned.

During the operation, `Esc` or `Ctrl-C` cancels requests, retries, and waiting.
Accepted requests cannot be reversed. Nodes are not automatically uncordoned.
If cancellation or failure interrupts a cordon request, check that node's state:
the server may have accepted the request. Press `Enter` or `Esc` to close the
result. Navigation stays disabled until the operation ends.

Persistent defaults, saved profiles, arbitrary kubectl arguments, pod selectors,
concurrent drains, and full kubectl drain parity are outside this feature.

## Core navigation

- **Container details** show readiness, state or failure reason, and restart
  count beside the existing resource columns. The selected container's details
  show its type, full image reference, declared ports, and configured startup,
  readiness, and liveness probes. Images and ports wrap to fit the popup.
  Probe labels show configuration, not the current probe result.
  Regular, init, native sidecar, and ephemeral containers are included. Pod watch
  updates refresh the details and keep the selected container by name. Missing
  status values show `-` or `Unknown`. If the selected container disappears,
  its selection clears. Name, state, and restarts have priority on narrow
  terminals. Resource percentages hide first, then usage and readiness columns.
  The popup uses fixed columns.

- **Container trends** show CPU and memory for the selected container on wide
  screens. Each chart covers the last five minutes in five-second bins, with
  the newest sample on the right. The scale is zero to the largest visible
  value. A dot marks a missing sample; measured zero has an empty bar.
  History starts while the container is selected and keeps at most 60 samples.
  It clears on a selection or view change, context change, or pod replacement.
  It uses the existing metrics polls and is not saved between sessions.
  The table keeps the current values; the charts show their history.

- **Scroll position** appears on the borders of long resource tables, document
  views, logs, and pickers. Tables and unwrapped documents also show horizontal
  position. Wrapped views use display rows. Scrollbars use thin lines. They
  appear during keyboard or mouse scrolling and hide after 700 ms without
  scrolling. They stay hidden when content fits. Fullscreen logs and documents
  keep their borderless layout for text selection.

- **Fullscreen documents** - `F` toggles the full terminal area for YAML, decoded
  Secret, describe, diff, events, and plugin popup output. Borders, scrollbars,
  the application header, status line, and key hints are hidden. The title and
  active search or command prompt remain visible. Search, scrolling, wrapping,
  copying, and refresh still work. The document setting is kept for the current
  session, including new documents, and is separate from the Logs setting.

- **Terminal title** shows `sofka: <context>/<namespace>` and follows navigation.
  The namespace is `all` when all namespaces are selected. Set
  `terminal_title = false` to disable title changes. Sofka clears the title on exit.

- **Compact startup** - set `compact_mode = true` to start with a one-line
  header and no footer. `Ctrl-E` toggles the layout for the session; reloads and
  context switches preserve it.

- **Optional header** - set `hide_header = true` in the configuration to hide
  the header and logo, including the one-line header in compact mode.

- **Connect** to the current kubeconfig context, including exec credential
  plugins (GKE, EKS, and friends).
- **Optional TLS session resumption workaround** through `--no-tls-resumption`
  for clusters that reject resumed connections with HTTP 401. The default is
  unchanged. See [TLS session resumption](debugging.md#tls-session-resumption-and-http-401).
- **Optional v1 client certificates** through `--allow-v1-client-cert`, disabled
  by default. See [certificate compatibility](debugging.md#x509-v1-client-certificates).
- **Teleport local proxy certificates** work when the server certificate exactly
  matches a configured CA. Hostname, date, usage, and TLS signature checks remain
  enabled. See [proxy certificates](debugging.md#teleport-local-kubernetes-proxy-certificates).
- **API discovery** of every resource type on the cluster, with k9s-style short
  aliases (`po`, `dp`, `svc`, `no`, `cm`, `sts`, `ds`, `ks`, `hr`, …) and correct
  precedence - core `pods` wins over `pods.metrics.k8s.io`.
  Discovered short names also work for custom resources, such as `:md` for
  MachineDeployments. Exact aliases appear before fuzzy resource matches.
  Resource names and built-in aliases take priority over discovered short names.
  Shared short names use group priority, then alphabetical group and resource
  order. User aliases override discovered aliases.
  Sofka can connect when it cannot read one API group. Examples: the
  extension API server is down, or it sends an `apiVersion` that is not `v1`.
  Sofka does not load that group. It shows a warning at startup and a flash
  on the first screen. `:info` shows the group and the reason.
- **Namespace switcher** (`n`) opens with the cursor on the active namespace.
  `current` marks the active namespace, including `<all>`. `context default`
  marks the kubeconfig namespace. Both labels can apply to one row and stay
  visible when you filter the list. The active and context default namespaces
  are available even if namespace listing is restricted. Favourites, recent
  namespaces, and shortcuts keep their existing order. Startup rules do not change.
- **Namespace commands**: `:ns <name>` changes namespace and keeps the current
  resource view, as the namespace switcher does. `:namespace` and `:namespaces`
  accept the same argument; `all` and `*` select all namespaces. From the
  Namespaces list, the command returns to the previous view, or opens Pods if
  there is no previous view. Other cluster-scoped views stay open; the namespace
  selection applies to the next namespaced resource view. Filters follow the
  namespace switcher's rules, and resource ownership scope is cleared.
  Without an argument, `:ns` opens the Namespaces list.
- **Live watch** of any kind through `kube::runtime::watcher`, streamed into an
  in-memory store. Watch requests use uncompressed responses to avoid gzip
  stream errors. List requests retain gzip compression.
- **Curated columns** for common kinds (pods, deployments, replicasets,
  statefulsets, daemonsets, services, nodes, namespaces, configmaps, secrets,
  jobs, cronjobs, PVC/PV, ingresses, endpoints, CustomResourceDefinitions), with
  a NAME/AGE fallback for everything else. STATUS columns use a fixed width of
  26 characters, or 27 for Nodes, so status changes do not move adjacent
  columns. A configured column width takes priority. Column widths use the full filtered list so
  vertical scrolling does not move the columns. Node ROLES combines
  `node-role.kubernetes.io/` labels with the legacy `kubernetes.io/role` value
  and removes duplicate roles. Node STATUS adds `SchedulingDisabled` when
  the Node is cordoned and keeps its readiness color.
- **Event timing** - LAST-SEEN shows the most recent reported occurrence for
  core and events.k8s.io Events. It advances with time and sorts by occurrence
  timestamp. AGE continues to show object creation age.
- **Service endpoints** include ExternalName targets, configured external IPs,
  load balancer addresses, and NodePort values such as `80:30080/TCP`.
- **Pod health** shows init progress and failure reasons, Pod reasons such as
  `Evicted`, scheduling gates, and termination signals or exit codes.
  Init progress appears after the kubelet reports init state. Until then, the
  table keeps the Pod phase or reason, including `SchedulingGated`.
  Normal init containers count toward RESTARTS during initialization, but not READY.
  Native sidecars count toward READY and RESTARTS. Their failures remain
  visible after initialization, without adding restarts from completed normal init containers.
  Application waiting and termination reasons take precedence after initialization.
  A blocked readiness gate
  gives a Running pod warning colors even when all containers are ready.
  Failure reasons use red rows and status text, including init failures and
  the `Lost` PVC state.
- **Horizontal scrolling** - Left and Right move the table by five text positions.
  NAME and NAMESPACE stay fixed. Other columns keep their widths while you
  scroll. Arrows in the title show where more content is available. When all
  columns fit, Left and Right do nothing.
- **Custom views** - define columns for any resource in the config file.
  Select and order built-in columns, live CPU/MEM usage, pod request and limit
  totals, and utilization percentages. Metric columns support numeric sorting,
  structured filters, and threshold colors. Custom text path columns support
  `format = "image-tag"` to show image tags, with registry ports and digests
  handled separately. Quantity path columns support `format = "cpu"` and
  `format = "memory"` for millicore and Mi/Gi display. Sorting and numeric
  filters use values before display rounding. An
  unknown custom resource picks up its CRD `additionalPrinterColumns`
  automatically. If no usable CRD columns are available, server Table columns
  can supply the view. Explicit views and built-in columns keep their priority.
  Table cells use watch updates, periodic refresh, and a polling fallback.
  `w` toggles wide-only columns (kubectl `-o wide`), including
  node labels. Add `@<namespace>` to a view key to select columns for one
  namespace. See
  [Views and thresholds](views.md).
- **Drill-down navigation** with a breadcrumb stack: workload/service → pods,
  cronjob → its jobs, node → its pods, pod → containers, namespace → re-scope,
  CRD → its custom resources. `esc` goes back.
  Workload pod selection includes both `matchLabels` and `matchExpressions`.
  Drill-down, logs, Explain, and diagnostic bundles apply all requirements,
  including `In`, `NotIn`, `Exists`, and `DoesNotExist`. Services use their
  plain label map.
- **Resource cycling** (`Tab` / `Shift-Tab`) - browse pods → services →
  deployments → statefulsets → daemonsets → secrets → configmaps → ingresses →
  PVCs, wrapping in either direction without configuration. Keeps the current
  namespace (including all namespaces), skips kinds absent from API discovery,
  and follows the active workspace's views when one is open. `[` / `]` remain
  view history.
- **Command palette** (`:`) - fuzzy search over the full resource catalog, your
  saved bookmarks and workspaces, and the built-in commands (`ctx`, `helm`,
  `pulse`, `xray`, `explain`, `timeline`, `gitops`, `argocd`, `adjacent`, `can-i`, `journal`,
  `debug`,
  `debug-clean`, `bundle`, `bundle-save`, `snapshot`, `snapshots`, `diff`,
  `events`, `pf`, `notify`, `find`, `vlogs`, `rightsize`, `fleet`, `skin`,
  `reload`, `config`, `info`). `:` and `?` open the palette and help from every
  navigation screen, then close back to the screen where they were opened.
- **Cross-context resource navigation** - `:pods @production-cluster default`
  switches context, resource, and namespace together without changing kubeconfig's
  `current-context`. Context names after `@` fuzzy-complete: Tab/Shift-Tab or
  Down/Up fill the selected context in the command. Add a namespace if needed,
  then press Enter to run the query. Omit the namespace to use the target context's
  remembered or default namespace. Namespace completion after `@context` is not
  provided.
- **Help scrolling** (`?`) - browse all bindings, including plugins, bookmarks,
  and workspaces. `j` / `k` and `↑` / `↓` scroll one line. `ctrl-f`, `PgDn`,
  and `space` move forward one page; `ctrl-b` and `PgUp` move back one page.
  Each page uses the visible content height. `g` / `Home` go to the top;
  `G` / `End` go to the bottom. `/` filters the bindings and resets the scroll
  position. `esc` clears the filter first, then closes help. `q` or `?` closes
  help and returns to the previous screen.
- **Filtering** (`/`) with matched-character highlighting: fuzzy text, `"text"`
  contiguous match, `/re/` regular expression (both case-insensitive), `!text`
  inverse match (also `!"text"` and `!/re/`), local label key and value search
  (`label:text`, `label:"text"`, `label:/re/`, and `!label:text`),
  `-l`/`-f` label and field selectors (evaluated server-side on
  ⏎), and typed column comparisons (`status=CrashLoopBackOff`, `cpu>500m`,
  `memory>1Gi`, `restarts>=5`, `age<2h`). Structured terms AND together with
  spaces or `&&`; `||` combines alternatives, parentheses group expressions,
  and `!(...)` negates a group. Quote values containing spaces. Selectors
  survive refresh, namespace changes, drill-down, and view history. The title
  shows local, server-side, mixed, or pending evaluation; `/` edits and Esc
  clears. Palette queries combine scope and filtering:
  `:pods -n prod --context west /-l app=api status=Running`.
  See [filter grammar and selectors](filtering.md).
- **Toggle faults** (`Ctrl+Z`, pods only) shows pending, failed, unknown,
  terminating, and running pods that are not ready. Completed pods are hidden.
  The table title shows `[faults]` while the filter is on. It works with the
  text filter and current namespace or drill scope. Press `Ctrl+Z` again to
  turn it off. The setting stays on for pod views during the session and does
  not filter other resource types. Configured `Ctrl+Z` bookmark, workspace,
  and matching plugin actions take precedence. Live updates keep the selected
  pod selected. If it leaves the list or its UID changes, selection is cleared.
- **Global fuzzy find** (`:find <text>`) - search object names across the common
  kinds (workloads, pods, services, config, ingresses, jobs, storage, nodes,
  namespaces, Flux objects) in every namespace at once, concurrently. Results
  rank by fuzzy score, `⏎` jumps to the object. When a kind can't be listed
  (RBAC), the result says it's incomplete instead of pretending otherwise.
- **Multiselect** (`space`) for bulk delete/kill/suspend/resume/reconcile.
  `Shift+ArrowUp` / `Shift+ArrowDown` extend or reduce a range from a fixed
  starting row. Separate marks remain selected when the range contracts.
  Range marks also work with combined pod logs.
- **Copy to clipboard** - `c` copies the selected resource's name; `Y` opens a
  field picker over the selected row's displayed columns (full values, never
  the width-truncated cell text) - type to match a column name or its value
  (an IP, an image, a node), `⏎` copies it. Falls back to OSC 52 on remote
  terminals without a local clipboard tool.
- **RBAC-aware palette browse** - the empty `:` list hides kinds you cannot
  `list`. An explicit search checks the full discovery catalog, because some
  delegated authorizers return incomplete rule reviews.
- **Resource type on context switch** - `:ctx` keeps the current resource type
  when the target cluster supports it. If unavailable, sofka opens that context's
  configured default resource, or Pods, and shows a fallback message. Filters and
  object ownership scope are cleared. Namespace selection follows the existing
  per-context rules. Explicit resource queries, bookmarks, and workspaces keep
  their specified destination.
- **Context picker at launch** - `sofka ctx` and `sofka contexts` open the picker
  before connecting to a cluster. Enter connects to the selected context and
  opens its configured default resource, or pods. `--context NAME` selects the
  initial context in the picker. An unknown name returns an error. `-n` and `-A`
  apply to the first successful selection only. These commands require
  interactive mode.
- **Namespace switcher** (`n`) with pinned favourites (★) and per-context
  session recents (·) above the rest, plus a context switcher (`:ctx`). In the
  resource table, `1` to `9` select the first nine configured favourites in fixed
  configuration order. The picker lists these shortcuts in a column before the
  names, and while its filter is empty the same digits select the favourite
  directly; a digit without a favourite is a filter character. Unconfigured
  slots do nothing in the table. `0` selects all namespaces in both places. The
  header's `Namespace:` line shows the configured favourites with their keys
  when the terminal is wide enough for them. The
  last namespace picked in each context is remembered across restarts
  (`<state-dir>/namespaces.toml`); `-n`/`-A` override it for a session.
- **Selected namespace** (`W`) switches to the cursor row's namespace and
  keeps the resource kind. It uses normal namespace history and watch behavior.
  Rows without a namespace show a status message.
- **Sort by age** (`A`) selects `AGE` with the same direction as the sort picker.
  Press it again to invert. If `AGE` is absent, the current sort stays active.
- **Default sort** - `[views."*"].sort` sets a global initial sort, with
  resource-specific overrides. Sort choices are saved per kind by default.
  Set `remember_sort = false` to make user sort changes temporary.
- **Configurable key bindings** - change or disable built-in keyboard actions
  under `[keys]`. Shared navigation settings and mode overrides keep text input
  separate from navigation. Help and key hints show the effective bindings.
  Changes support `:reload` and cluster/context overrides. Invalid bindings keep
  the previous keymap. Legacy palette settings are migrated with a config
  backup; managed files produce a warning and use the converted keys in memory.
  See [Configure key bindings](keybindings.md).
- **Mouse support** - the wheel scrolls every view (one wheel event is three
  steps of that view's own up/down; `mouse_scroll_lines` tunes this in views
  with mouse capture), clicking a row selects it, clicking a column header
  sorts by it (click again to flip). Document views (YAML/describe, diff,
  events, logs, help) release the mouse automatically so click-drag selects
  text natively; the wheel still scrolls them in terminals that translate it to
  arrow keys in the alternate screen (kitty, Ghostty, iTerm2, ...), at the
  terminal's own speed, not `mouse_scroll_lines`. Set
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
  class. Memory quantities use Kubernetes units, including decimal `k`, `P`,
  and `E`, and binary `Pi` and `Ei`, in metrics and filters. Fractional bytes
  round up to the next whole byte.
  Missing samples show `-` and do not match numeric CPU or memory filters.
  Measured zero shows `0m`, `0Mi`, or `0%`. Missing metric values sort before
  measured values in ascending order and after them in descending order.
  All of it degrades cleanly when metrics-server isn't installed.
- **Configurable thresholds** for the RESTARTS/CPU/MEM/request-limit coloring,
  globally and per resource and per context. See
  [Views and thresholds](views.md#thresholds).
- **Workload health at a glance** - Deployments, StatefulSets, DaemonSets, and
  ReplicaSets carry a STATUS column derived from their replica counts and
  conditions (`Ready`, `Progressing`, `Degraded`, `Unavailable`, `Stalled`,
  `ScaledDown`, `Terminating`), and the whole row is tinted by it - so a
  workload whose pods are crashing or whose desired replicas aren't met reads
  red/peach in the list, like k9s, instead of looking uniformly healthy.
- **Job execution status** distinguishes pending, running, suspended, failed,
  completing, completed, and terminating jobs. Failed jobs use the error color even when
  no pod is active.
- **Storage deletion status** shows `Terminating` for PVs and PVCs after
  deletion starts, including when a storage protection finalizer keeps the
  object in the API.
- **Explain-unhealthy view** (`X` / `:explain`) - a deterministic, evidence-based
  explanation of why the selection is unhealthy: rollout state, degraded
  conditions, blocking pods and their container failure reasons
  (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable, failed probes),
  and recent Warning events. No AI, no external service. `⏎`, `E`, or `l` jumps
  from a finding to the pod, its events, or its logs. After opening evidence,
  `esc` returns to Explain before another `esc` returns to the table.
  Opening the view or pressing `r` reads the selected resource from the API
  before gathering its evidence. A failed read or a changed UID produces a
  warning instead of findings from an old snapshot. Only the latest requested
  report can update the findings. Closing the view with `esc` or `q` cancels
  pending results and clears the report progress message. Navigation to
  a target resource or a palette destination also cancels pending results.
  Temporary Events and Logs views keep the parent report active. New findings
  update that report without changing the evidence view. Refresh keeps the
  previous findings until new results arrive.
- **Session-local timeline** (`T` / `:timeline`) - a per-object timestamped log
  of every state change the watch saw: generation bumps, replica and readiness
  changes, pod phase, restarts, waiting reasons, condition flips. Computed from
  the watch stream, bounded, never written to disk.
- **Pulse dashboard** (`:pulse`) - cluster-health tiles, refreshed every 5s.
- **Xray tree** (`:xray`) - a hierarchical view from the current kind down
  through owner references to pods and containers.
- **Adjacent view** (`u` / `:adjacent`) - one hop in every direction from the
  selection: its owners, the objects it owns, the objects its spec names (a
  pod's node, claims, ConfigMaps, Secrets; a claim's classes and volume), and
  the objects whose specs name it (the pods mounting a claim, the claims using
  a class). `⏎` opens one in its regular view, `y`/`d` show its YAML or
  describe. Relations are data: a built-in table for core kinds, extended per
  CRD with `[[views."…".refs]]` and `children`. A ref whose target kind is a
  field of the object (an ExternalSecret's `secretStoreRef.kind`) reads it
  with `kind_path` from a list of candidate `kinds`. Reverse lookups stay in
  the row's namespace unless the rule says `cluster`.
  Press `c` in this view to discover direct children of a namespaced custom
  resource with a UID, including resources with no configured child kinds.
  Wait for the initial adjacent lookup to finish first. The search uses API
  discovery from the current cluster connection. It selects namespaced resources
  that support listing, excludes subresources, and selects one API version per
  resource. It searches only the source namespace and matches owner UIDs, not
  names or labels. Results are added to the view as pages arrive. `⏎` opens a
  result. The initial lookup and Enter action on the resource table stay the same.
  Each search permits four concurrent requests, 200 objects per page, at most
  200 list requests and 20,000 objects checked, five seconds per request, and
  30 seconds in total. Objects with other owners count towards the object limit.
  Access denial, request errors, timeouts, skipped API discovery, and search
  limits mark the search as incomplete. Results already found remain available.
  The search status stays above the results. Leaving the view, opening an
  overlay, refreshing, or changing the source or context cancels the search.
  Late replies are ignored. `c` starts another search; `r` repeats the initial
  adjacent lookup. This action does not search across namespaces, follow
  descendants recursively, or start background watches.
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
  into. Opening the view or pressing `r` reads the original resource again,
  then follows its current owner labels, source, and dependencies. A missing
  or replaced resource produces a warning. These reads require `get` access.
  Only the latest requested report can update the findings. Closing the view
  with `esc` or `q` cancels pending results and clears the report progress
  message. Navigation to a target resource or a palette destination also
  cancels pending results.
- **Argo CD view** (`:argocd` / `:argo`) - the state of the selected Application:
  sync and health, the project and destination, the source repository with the
  revision actually deployed, every object in `status.resources[]` with its own
  sync and health, and a summary of what is blocking - a suspended sync policy, a
  `ComparisonError`, a failed sync operation, degraded or missing objects, or
  drift. Each managed resource is a finding you can `⏎` into. Read entirely from
  the Application CRD: no Argo CD API server, no token, no `argocd` binary.
  Applications deploying to a **remote cluster** are handled honestly - the
  destination is resolved against your kubeconfig and shown by context name, and
  because those objects do not live in the cluster you are connected to, `⏎`
  reports where they are instead of searching here. Opened on any **other**
  object, the view follows Argo's tracking metadata the other way: the
  `argocd.argoproj.io/tracking-id` annotation (preferred, since it is exact) or
  the `app.kubernetes.io/instance` label (truncated at 63 characters) names the
  Application, which is looked up by name. An object Argo does not manage says
  so, and a tracking reference whose Application no longer exists is reported as
  a dangling reference rather than as unmanaged. Where several Argo CD instances
  share a cluster and each holds an Application of the same name, the one that
  actually lists the object among its managed resources wins. `r` re-reads the resource and
  follows its tracking metadata again. These reads require `get` and `list`
  access.
- **Native Helm inspector** (`:helm` / `:hm`) - sofka decodes Helm's release
  storage Secrets directly (double base64 → gunzip → JSON, same as Helm) and
  lists one row per release at its latest revision, like `helm list`. `⏎` opens
  the full revision history (`helm history`); on a revision, `⏎` shows
  user-supplied values, `y` the rendered manifest, `d` the NOTES.txt. `r` rolls
  back and `ctrl-d` uninstalls - those two shell out to the real `helm` binary,
  all the inspection is native. UPDATED advances with the clock in both the
  release list and revision history. The table keeps the deployment timestamp
  in its row cache, so clock updates do not decode the release again.
- **Scale discovered resources** (`s`) - scale built-in or custom resources when
  API discovery lists a `scale` subresource with PATCH support. Changes use
  `/scale`, including when a CRD stores replicas at a custom path. Marked rows
  can be scaled together. Custom resources do not show an assumed current count.
  The action requires patch permission on the scale subresource.
- **Managed-resource mutation warnings** - before you edit, delete, scale, or
  otherwise change an object Flux (or another controller) owns, sofka tells you
  the next reconcile will revert it or recreate it. Fix the source instead of
  fighting the controller.

## Actions

- **CronJob controls** (`t`) - trigger now (creates a Job from the jobTemplate,
  like `kubectl create job --from`), suspend, resume.
- **Background port-forwards** (`f`/`F` to start, `:pf` to manage) plus **saved
  forwards** that show up in `:pf` even while stopped, with optional autostart.
  Pressing `f` on a pod or service opens a picker listing the manifest's
  declared ports. Press `enter` to start the selected mapping, or `e` to edit
  only its local port. The edit prompt contains the current local port;
  `esc` returns to the same picker row. Choose "Custom…" for manual
  `LOCAL:REMOTE` input. If the local port cannot bind to either loopback address, the input stays open
  and shows an error so you can choose another port. Active forwards show a teal `●` in a dedicated
  indicator column next to the row name. See [Saved forwards](plugins.md#saved-forwards).
- **File transfer** (`t` on a pod, or `t` in the container picker for one
  container) - download from or upload to a pod via `kubectl cp`, off-thread
  with a completion flash. Uploads are gated by the `transfer` guardrail and
  read-only mode.
- **PVC explore** (`x` on a PVC, or `:pvc-explore`) - a two-pane browser over a
  volume's contents, with `s` for a shell inside it. See
  [PVC explore](#pvc-explore).
- **Ephemeral debug containers** and **node debug pods** (`:debug`). See
  [Debug containers and pods](debugging.md#debug-containers-and-pods).
- **Logs** (`l`) - combined logs for marked pods, per-container on a pod, or aggregated across all matching
  pods on a workload/service, with filtering, previous-container logs, and
  configurable tail/buffer/lookback. Lines with timestamps are sorted by time.
  Press `t` to show or hide timestamps without changing log order or restarting
  streams. If a container is waiting to start, sofka
  retries until its logs are available. sofka parses ANSI color from the source app
  and maps it onto the active skin instead of printing literal escapes. See
  [Log controls](debugging.md#log-controls).
- **Log severity filter** (`Ctrl+Z` in logs) shows detected warning and error
  lines, including fatal and panic levels. The title shows `[warn/error]`.
  It combines with `/` and uses the same severity rules as log colors.
  Detection depends on how applications format their logs. Lines with no
  recognized severity, including stack trace continuation lines, can be hidden.
  Press `Ctrl+Z` again to show all retained lines. The filter resets when a new
  log view opens. Copy and save use the filtered lines.
- **Log markers** (`m` in logs) add visual separators at the buffer tail.
  Markers stay visible through filters, do not move a paused viewport, and are
  excluded from sofka copy/save. Clearing or replacing the buffer removes them.
  Their storage is bounded by the active log buffer cap.
- **VictoriaLogs integration** (`L` / `:vlogs`) - log history from a
  VictoriaLogs backend for a pod, container, workload, service, or whole
  namespace, covering restarted and deleted pods. Zero config: sofka finds the
  service in-cluster and reaches it through the API-server proxy. See
  [Providers](providers.md#log-provider-victorialogs).
- **Right-sizing** (`:rightsize`) - estimate right-sized requests from past
  usage in a Prometheus or VictoriaMetrics backend, with a patch preview. Never
  mutates. See [Providers](providers.md#right-sizing-metrics-provider).
- **Fleet dashboard** (`:fleet`) - an opt-in health summary across contexts,
  side by side: node readiness, unhealthy pods, Flux failures, and Argo CD
  Applications that are `OutOfSync` or not `Healthy`. A cluster without the Flux
  or Argo CD CRDs shows `—` rather than a zero. Contexts come from config or
  `space` in the `:ctx` switcher. See
  [Providers](providers.md#fleet-dashboard).
- **YAML view** (`y`), **describe** (`d`, via `kubectl`), **events**
  (`:events` / `E`, filtered by UID when available), and **diff** (`:diff`), with
  `ctrl-f` / `ctrl-b` (or `PgDn` / `PgUp`) paging through each document.
- **Diff on GitOps clusters** - `:diff` shows a unified diff of the live object
  against its `last-applied-configuration`. When that annotation is missing - as
  it is for every Flux- or Helm-managed object, which nothing ever
  `kubectl apply`s - sofka diffs against the previous revision this session's
  watch saw, so "what just changed?" has an answer. The last revision of up to
  256 changed objects is kept in memory.

Automatic refresh is available in these resource views:

| View                           | Automatic refresh           | Other refresh controls                            |
| ------------------------------ | --------------------------- | ------------------------------------------------- |
| YAML, decoded Secret, describe | `r` turns refresh on or off | None                                              |
| Diff                           | `r` turns refresh on or off | `R` resets the baseline to the displayed resource |
| Explain                        | `R` turns refresh on or off | `r` refreshes immediately                         |

Automatic refresh is off when a view opens. It reads immediately, then waits
5 seconds after each result before the next read. Describe runs `kubectl describe`
and updates the full document, including events. The other views read the
resource through the Kubernetes API. YAML also supports custom resources.

Refresh keeps the original resource and context. It preserves document search,
scroll position where possible, and the selected Explain resource when findings
move or its status text changes. Findings without a resource target match by
content. If the selected finding disappears, the selection clears. Select another
finding before opening its resource, events, or logs.
A shorter document can reduce the scroll position. The status indicator shows
`refresh` while automatic refresh is on and `stopped` when it is off. Documents
without refresh support, such as saved snapshots and Helm manifests, show `static`.

Automatic refresh stops when you leave the view, open help or the command
palette, or a request fails. Document search keeps refresh active. A failed
request keeps the last result and shows the reason. A deleted resource, or a
resource recreated with the same name and a different UID, also stops refresh.
Opening events or logs from Explain stops its automatic refresh; returning does
not restart it. Its existing manual evidence request can still finish.

Diff keeps the baseline chosen when the view opens, even if the last-applied
annotation or session history changes. `R` makes the currently displayed object
the new baseline. A Diff view can stay open when both sides match, so automatic
refresh can show later changes. This does not add change highlighting to YAML.

## PVC explore

A PersistentVolumeClaim has no API that returns its contents: the only way to
see what is on a volume is from inside a pod that mounts it. `x` on a PVC row
(or `:pvc-explore`) does that for you and puts the result on screen as a
two-pane file browser - your local filesystem on the left, the volume on the
right - so a download or an upload is one keystroke rather than a hand-written
`kubectl cp` path.

- **It uses a pod that is already there.** sofka looks for a running pod in the
  claim's namespace that mounts it, preferring one with a writable mount, and
  execs into that container at its `mountPath`. Nothing is created, so this
  works in read-only mode.
- **Otherwise it offers a helper pod.** When nothing mounts the claim - the
  common case for a volume you are trying to inspect _because_ its workload is
  scaled to zero - sofka asks before creating a short-lived pod that mounts it
  at `/pvc`. That is a write: it is blocked in read-only mode, matches the
  `pvc-explore` guardrail action, and always confirms, naming the image and the
  namespace. The helper carries both a `sleep` and `activeDeadlineSeconds`, so
  it expires on its own even if sofka never gets to delete it, and closing the
  browser - or quitting sofka - deletes it immediately. `:pvc-clean` removes
  any a crashed session left behind: it sweeps the current namespace, or every
  namespace when the view is across all of them, requiring the name prefix,
  both of the labels sofka sets, and the annotation naming the claim, and
  skipping the pod your own open browser is using. None of that evidence is
  unforgeable - anything sofka writes on creation, anything else can write too
  - so it is there to make an accidental match essentially impossible, not as
    a permission check; the confirmation, the guardrail and read-only mode are
    what bound a deliberate one. It cannot tell a leftover from a pod _another_ session is browsing
    through right now, so the confirmation says so. Deleting pods is a mutation
    like any other: blocked in read-only mode, matched by the `pvc-explore`
    guardrail, recorded in `:journal`.
- **Navigation is confined to the mount.** `⌫` stops at the mount point, and
  every listing verifies with `pwd -P` that it actually landed inside the
  volume - so a symlink on the volume pointing at `/` is refused rather than
  quietly dropping you into the serving pod's root. sofka also treats the
  volume's contents as untrusted: GNU `ls` writes file names into a pipe
  unescaped, so a file whose name contains a newline can inject what looks
  like an extra row, and a symlink target can carry an absolute path. Such a
  row may still appear as a phantom entry - there is no way to tell it from a
  real one - but it is contained: entries naming `.`, `..`, or anything
  containing `/` are discarded, so a forged row can reach neither outside the
  mount nor outside the directory a download lands in. busybox `ls` - the
  default helper image - substitutes `?` for control characters instead, so
  there is nothing to forge; such a name lists looking ordinary and fails when
  you open or copy it.
- **`c` copies from the focused pane into the other one** - out of the volume
  when the right pane has the cursor, into it when the left one does. Uploads go
  through `kubectl cp`, are blocked in read-only mode, match the `pvc-upload`
  guardrail action, and are refused up front when the mount is `readOnly`. A
  download that would overwrite a local file confirms first. `kubectl cp`
  splits its arguments on the first `:`, so a name containing one is refused
  with an explanation rather than a `filespec must match the canonical format`
  from kubectl.
- **A copy in flight fills a bar** where the row's size was, so a 5 GB file or
  a directory of thousands of them shows how far it has got rather than one
  unchanging "copying" line for minutes. `kubectl cp` reports nothing while it
  runs, so what is measured is the destination: a download's bar comes from the
  local file (or tree) it is writing, an upload's from one `kubectl exec` that
  prints the destination's size once a second - one exec for the whole copy,
  not one per second. A folder is measured whole, so its bar is the recursive
  total and not one file at a time. The status bar carries the percentage as
  well, which is also where a copy started by `t` on a pod - with typed paths
  and no row to draw on - reports itself.

  Quitting sofka while an upload is running leaves that `du` loop in the pod
  until it times itself out - fifteen minutes where the container has a clock,
  and 300 passes, at least five minutes, where it has not - because the signal that stops it is the
  connection closing, and a killed process does not send one; the copy itself
  is left to finish either way.

  Measuring the volume side is an exec of its own - `du`, alongside the `tar`
  that `kubectl cp` already runs there - and it is gated no further than the
  copy it belongs to; see [safety](safety.md#guardrails) for why. A copy also
  starts a moment later than it used to, since the destination is measured
  before it is touched.

  The source's total comes from the listing for a single file and from `du`
  for a directory, so the pod needs `du` as well as the `ls` a listing needs
  and the `tar` a copy needs; without it the copy runs with no bar. The total
  is an estimate either way, and a bar can finish short of the end or reach it
  early and wait: a `du` that can only report whole disk blocks reads high, a
  subdirectory the serving pod cannot read is missing from the total, and both
  ends count directory entries at whatever their own filesystem charges for
  one - 4 KiB on ext4 against a couple of hundred bytes on APFS, which on a
  tree of many small directories is a visible fraction rather than a rounding
  error. What is already at the destination is not counted - an overwrite
  opens at zero rather than at yesterday's copy - but a whole folder copied
  over a copy of itself is the case this cannot measure: almost nothing new
  lands, so its bar ends well short even though the copy is complete.

- **`s` opens a shell** at the directory the remote pane is showing (or at the
  mount point, from the PVC row directly). The exec lands in a real pod, so it
  passes the same `shell` guardrail as `s` on that pod's row - a rule that
  denies shells in prod is not defeated by reaching the pod through a claim it
  mounts, and a denied shell is refused before a helper pod is created rather
  than after.

Listings are read with `ls -A -l` over `kubectl exec`, so the pod's image needs
a shell and `ls`; transfers additionally need `tar`, as `kubectl cp` always
does. An entry `ls` cannot stat still appears, with an unknown size and a
warning, rather than blanking the whole directory. The helper-pod image and
lifetime are configurable:

```toml
[pvc_explore]
image = "busybox:1.37"   # helper-pod image
ttl = "30m"              # how long it lives before deleting itself
```

Only a `Bound` filesystem claim can be browsed: an unbound one has no volume
behind it, and a `volumeMode: Block` one has no filesystem. A listing is a
point-in-time read, not a watch: `r` re-reads both panes. Both panes cap one
directory at 5,000 entries - on the volume side by `head` inside the pod, so a
spool directory is never streamed out in full. An entry nothing could stat
still lists, with `?` for its size.

The helper pod runs as whatever user its image defaults to, because reading a
volume's contents generally needs root. It drops all capabilities and sets
`allowPrivilegeEscalation: false` and `seccompProfile: RuntimeDefault`, which
satisfies the `baseline` Pod Security Standard - but not `restricted`, which
also requires `runAsNonRoot`. In a namespace enforcing `restricted` the helper
pod is rejected; browse through a pod that already mounts the claim instead.

## Safety

- **Read-only mode**, **declarative guardrails**, **action-aware authorization**
  (`:can-i`), and a session-local **action journal** (`:journal`) with optional
  [file persistence](configuration.md#action-journal-files). See
  [Safety](safety.md).

## Extensibility

- **Plugins** - shell-out commands bound to key chords, scoped per resource,
  with terminal/popup/background output modes, confirmation and dangerous flags,
  read-only declarations, rich placeholders, and bulk execution over marked
  rows. The official reviewed catalog supports cluster-independent search,
  description, checksum-verified installation, explicit update and rollback,
  offline listing and caching, withdrawal, and safe removal. See
  [Plugins](plugins.md).
- **Bookmarks** - saved navigation commands on a chord and in the palette.
- **Workspaces** - a named set of views for one task, cycled with `Tab`.
- **Skins** - built-in Catppuccin, Gruvbox, Solarized, Nord, Dracula, Tokyo
  Night, One Dark, Rosé Pine, Rosé Pine Dawn, Monokai, and Flexoki palettes,
  auto dark/light detection,
  and per-swatch hex overrides. Every semantic color (row status, severity
  badges, headers, borders) is derived from the active palette, so one skin
  change lands everywhere at once.
- **Config file** (TOML or YAML) with per-cluster and per-context overrides and live
  `:reload`. See [Configuration](configuration.md).

## Diagnostics

- **Diagnostic bundles** (`:bundle`, `:bundle-save`) - a redacted incident
  bundle for the selection as one Markdown document. See
  [Diagnostic bundles](debugging.md#diagnostic-bundles).
- **Snapshots** (`:snapshot`, `:snapshots`) - capture the current table view to
  text, JSON, or YAML, then browse and open saved captures. See
  [Snapshots](debugging.md#snapshots).
- **Runtime diagnostics** (`:info`, or `sofka info`) - version and build, config
  sources, live context/cluster/API server and Kubernetes revision, discovery
  with warnings for unread API groups, Metrics API status, watch error and reconnect counts, API request latency
  per class, active skin, loaded plugins and views, and the
  state/log/snapshot/bundle directories. The connected Kubernetes revision also
  stays visible in the main header.
  Identifiers, paths, and counts only, never credentials, tokens, or Secret
  values. See [Runtime diagnostics](debugging.md#runtime-diagnostics).
- **Structured logging** (`[logging]`, or `SOFKA_LOG=debug`) - sofka's own
  session log as logfmt lines under the state directory, with every value
  redacted on the way in and writes off the UI thread. Off by default. See
  [Structured logging](debugging.md#structured-logging).

## Bundled plugins

- **`:sanitize`** deletes the pods a namespace has finished with - completed
  jobs, failed and evicted pods, and optionally the wedged ones. It ships with
  sofka and needs no runtime on `PATH`; the adapter is the sofka binary.
  `states` selects `terminal` (the default), `stuck`, or `all`, based on
  application container state and Pod phase. Specific table reason labels do
  not add deletion categories. `dry_run=true` reports without deleting.
  It confirms before running, is blocked in read-only mode, and matches
  guardrails as `plugin:sanitize`. It never deletes a pod that is terminating,
  still has a running container, or was replaced since the scan.
  The scope is the current namespace - **all namespaces when the view is**.
  `-l`/`-f` filter terms narrow the scan server-side; a filter it cannot
  reproduce exactly makes it refuse rather than delete more than the table
  shows.
  See [Sanitize pods](../plugins/sanitize/README.md).

## External plugin packages

- **Package discovery** reads `plugins/*/plugin.toml` from the sofka configuration directory.
  Packages reload with `:reload`.
- **Named commands** and key chords start adapters without changes to sofka's source code.
- **Validated inputs** supply named arguments with types, defaults, choices, and limits.
- **JSON reports** show text sections and tables in a searchable document.
- **Shared execution** limits output and concurrency.
  It cancels processes on timeout, navigation, or `:plugin-cancel`.
- **Safety controls** apply read-only mode, confirmation, and guardrails to plugins.
  Load-test plugins require a network-load declaration.
- **Managed port-forwards** supply a local endpoint for a selected pod or service.
- **Local checks** validate package manifests and reports without a cluster.

See [Create a plugin package](plugin-authoring.md).

Workload STATUS shows `Progressing` until the controller observes the current
specification and an active rolling update reaches its target. StatefulSet
partitions and `OnDelete` strategies retain their update semantics. Deployment
READY compares ready replicas with the desired count from the specification.
An active rollout with some ready replicas shows `Progressing` even when
`Available=False`. A workload with no ready replicas shows `Unavailable`. A
failed rollout shows `Stalled`.
