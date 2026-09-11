# Key reference

The default keymap. All built-in keyboard actions are configurable.
See [Configure key bindings](keybindings.md) for scopes and action names.
`?` inside sofka shows the effective bindings, including your own
plugin, bookmark, and workspace chords. `:` and `?` work from every navigation
screen; closing either returns to the screen where it was opened. Text-entry
pickers keep both characters available as input.

## Table views

At launch, `sofka ctx` or `sofka contexts` opens the context picker before
connecting. Press Enter to connect to the selected context and open its default
resource, or pods if none is set.

Use `:resource -n namespace --context context /filter` to apply a complete query.
Use `:resource @context [namespace]` for cross-context navigation. Context names
fuzzy-complete after `@`; Tab/Shift-Tab or Down/Up fill the selected context in
the command. Add a namespace if needed, then press Enter to run the query.
Without a namespace, a context switch uses its remembered or default namespace.
This does not change kubeconfig's `current-context`.
Scope options precede the slash. Structured filter terms combine with spaces or
`&&`, with `||` for OR and `!(...)` for group negation. `/` edits the active filter
and Esc clears it. See [filtering](filtering.md)
for the grammar and selector persistence rules.

| Key                                           | Action                                                                                                                                                              |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `:resource -n ns --context ctx /filter`       | query resource, namespace, context, and filter together                                                                                                             |
| `:resource @context [namespace]`              | switch context and resource; context names fuzzy-complete                                                                                                           |
| `:<resource>`                                 | command palette - fuzzy over kinds and built-in commands                                                                                                            |
| `:<resource> <ns>`                            | switch kind and namespace at once (`:deploy social`; `all`/`*` = all namespaces; the namespace tab-completes)                                                       |
| `:ns <name>`                                  | change namespace and keep the resource view; from Namespaces, return to the previous view or Pods (`all`/`*` = all namespaces)                                      |
| `[` / `]`                                     | view history - back / forward through visited kind+namespace views                                                                                                  |
| `Tab` / `shift-Tab`                           | next / previous common resource in the current namespace; cycle workspace views when one is open                                                                    |
| `enter`                                       | drill down (workload/svc → pods, machinedeployment → machines, cronjob → jobs, node → pods, pod → containers, ns → re-scope, CRD → resources, or [views](views.md)) |
| `esc`                                         | go back / pop the view stack / clear filter / clear marks                                                                                                           |
| `j`/`k`, `↓`/`↑`, `g`/`G`                     | navigate                                                                                                                                                            |
| `ctrl-f` / `ctrl-b`, `PgDn` / `PgUp`          | page forward / back - one screenful at a time                                                                                                                       |
| `S` / `I`                                     | sort-column picker (fuzzy; ⏎ on the active column inverts) / invert sort direction; saved per kind by default (`remember_sort = false` disables this)               |
| `A`                                           | sort by `AGE`; press again to invert; uses the sort memory setting                                                                                                  |
| `ctrl-e`                                      | compact mode: collapse the header + footer (for tiled/multiplexed panes)                                                                                            |
| `space`                                       | mark/unmark row for bulk actions                                                                                                                                    |
| `shift-up` / `shift-down`                     | extend or reduce the marked range from the starting row                                                                                                             |
| `/`                                           | filter: fuzzy text · `"exact"` · `/regex/` · `!inverse` · `label:text` local label search · `-l`/`-f` selectors (server-side on ⏎) · `status=X` `cpu>500m` `age<2h` |
| `Ctrl+Z`                                      | toggle faults filter in pod views; configured actions take precedence; combine with `/`; press again to turn off                                                    |
| `n` / `0`                                     | namespace switcher / all namespaces; `0` also selects all namespaces inside the switcher while the filter is empty                                                  |
| `W`                                           | switch to the selected resource's namespace and keep the resource kind                                                                                              |
| `1` to `9`                                    | select a configured favourite namespace in fixed configuration order; also inside the switcher while the filter is empty                                            |
| `shift-j`                                     | jump to owner/controller                                                                                                                                            |
| `o`                                           | show the node the selected row names (pods built in; other kinds via `[views."…"].node`)                                                                            |
| `ctrl-r`                                      | refresh the watch                                                                                                                                                   |
| `y` / `d` / `E`                               | view YAML / describe (`kubectl`) / live events                                                                                                                      |
| `x`                                           | secrets: show `data` base64-decoded (as `stringData`) · PVCs: browse the volume                                                                                     |
| `X` / `T`                                     | explain why the selection is unhealthy / session-local state-change timeline                                                                                        |
| `u` / `:adjacent`                             | adjacent view: owners, children, and the objects the selection names or is named by (`⏎` opens one)                                                                 |
| `c` (adjacent view)                           | Discover direct children of a namespaced custom resource. Search starts on request. Results can be incomplete; see the limits in [Features](features.md).           |
| `:gitops` / `:flux`                           | Flux owner, source, revisions & reconciliation chain for the selection (`⏎` to jump)                                                                                |
| `:argocd` / `:argo`                           | Argo CD Application sync/health, source, managed resources & what's blocking, for the selection or the Application that manages it (`⏎` to jump, `c` to expand what a resource owns) |
| `:can-i` / `:can-i <verb> <resource> [ns]`    | what you can do here / check a single action (`SelfSubjectAccessReview`)                                                                                            |
| `:journal` / `:audit`                         | session-local log of the mutating actions you've taken                                                                                                              |
| `:rightsize`                                  | historical right-sizing: P50/P95/P99 usage → suggested requests + patch preview (needs a metrics backend)                                                           |
| `:ctx` / `:ctx <name>`                        | context switcher popup (type to filter, `r` renames, `space` toggles fleet membership) / switch directly (the name tab-completes)                                   |
| `:helm`                                       | Helm releases (native storage-Secret decode): ⏎ history → values · `y` manifest · `d` notes · `r` rollback                                                          |
| `:fleet`                                      | cross-context health dashboard (opt-in: `[fleet]` contexts or `space` in `:ctx`; `⏎` switches, `r` refreshes)                                                       |
| `:skin`                                       | switch the color skin live (`:skin gruvbox-dark` applies directly)                                                                                                  |
| `:reload` / `:config` / `:info`               | reload config from disk · config sources + warnings · runtime diagnostics                                                                                           |
| `l` / `p`                                     | logs (marked pods, or current row; workload = all matching pods) / previous-container logs                                                                          |
| `L` / `:vlogs`                                | VictoriaLogs history for the selection (pod, container, workload, service, namespace)                                                                               |
| `c`                                           | copy resource name to clipboard                                                                                                                                     |
| `Y`                                           | copy any cell of the selected row: picker over the displayed columns (type to match a column name or value), `⏎` copies                                             |
| `e`                                           | edit in `$EDITOR` (`kubectl edit`)                                                                                                                                  |
| `s`                                           | shell into pod / shell into a PVC's volume / scale a workload (context-dependent)                                                                                   |
| `a`                                           | attach to pod                                                                                                                                                       |
| `:debug`                                      | pod: ephemeral debug container (`d` in the picker targets one) · node: privileged debug pod (previewed + confirmed)                                                 |
| `:debug-clean`                                | delete the node debugger pods launched this session                                                                                                                 |
| `:pvc-explore` / `:pvc-clean`                 | browse the selected PVC (also `:pvc-browse`; see below) · delete helper pods a previous session left behind (also `:pvc-cleanup`)                                   |
| `:bundle` / `:bundle-save`                    | assemble a redacted diagnostic bundle for the selection · write the previewed bundle to a file                                                                      |
| `:snapshot [text\|json\|yaml]` / `:snapshots` | capture the current view to a file · browse, open, and delete saved snapshots                                                                                       |
| `:notify`                                     | toggle watch notifications on the selected object                                                                                                                   |
| `:find <text>`                                | global fuzzy find over object names across common kinds, all namespaces                                                                                             |
| `i`                                           | set container image                                                                                                                                                 |
| `r`                                           | rollout restart (workloads) / force-sync (ExternalSecrets/PushSecrets) / refresh (elsewhere)                                                                        |
| `f` / `shift-f`                               | port-forward (pods/services) — picker shows declared ports, or "Custom…" for manual entry; active forwards show `●` next to the name                                |
| `t`                                           | Flux: suspend/resume/reconcile menu · ArgoCD App/AppSet: suspend/resume (App: + sync) · CronJobs: trigger/suspend/resume · pods: file transfer (`kubectl cp`)       |
| `C` / `U` / `D`                               | nodes: cordon / uncordon / drain                                                                                                                                    |
| `ctrl-d` / `ctrl-k`                           | delete / force-delete (marked rows, or current); in confirm: `f` toggles force, `c` cycles cascade (background → foreground → orphan)                               |
| `w`                                           | toggle wide-only columns (kubectl `-o wide`), including node labels                                                                                                 |
| `←` / `→`                                     | scroll sideways by 5 text positions; NAMESPACE/NAME stay fixed; arrows show more content                                                                            |
| `:q`, `ctrl-c`                                | quit                                                                                                                                                                |
| `?`                                           | help                                                                                                                                                                |
| _(config)_                                    | plugin / bookmark / workspace key chords — `ctrl-`/`alt-`/`shift-`/`fN`; listed in `?` help                                                                         |

Shift+Arrow selects a range in the visible row order. Reversing direction reduces
the range and keeps separate marks made with `space`. Other keys end the range
operation. Normal movement keeps marked rows. Filtering, sorting, view changes,
and changes to the row order reset the range before the next Shift+Arrow press.

## Port-forward picker (`f` on a pod or service)

| Key                  | Action                                                         |
| -------------------- | -------------------------------------------------------------- |
| `enter`              | start the selected mapping, or open manual input for "Custom…" |
| `e`                  | edit only the local port of a declared mapping                 |
| `j` / `k`, `↓` / `↑` | select a mapping                                               |
| `esc` / `q`          | close the picker                                               |

The local-port prompt contains the current value. `enter` starts the forward;
`esc` returns to the same picker row. Invalid or unavailable ports keep the
prompt open for correction. If the forward process cannot start, the prompt
keeps the edited value so you can retry.

## PVC explore (`x` on a PVC)

A two-pane file browser over a PersistentVolumeClaim: your local filesystem on
the left, the volume on the right. `s` on a PVC row opens a shell at the mount
point instead. See [PVC explore](features.md#pvc-explore).

| Key              | Action                                                                      |
| ---------------- | --------------------------------------------------------------------------- |
| `tab`, `←` / `→` | switch pane (the focused pane has the bright border)                        |
| `j`/`k`, `g`/`G` | move within the focused pane                                                |
| `enter`          | open the selected directory                                                 |
| `⌫` or `-`       | go up one directory - stops at the mount point, never above it              |
| `c`              | copy the selection into the other pane: download from the volume, or upload |
| `s`              | shell into the volume at the directory the remote pane is showing           |
| `r`              | re-read both panes                                                          |
| `esc` / `q`      | close (and delete the helper pod, if one was created)                       |

## Logs view

`/` filter (substring · `/regex/` · `!invert`) · `s`/`f` autoscroll · `w` wrap ·
`m` visual marker · `t` timestamps · `x` stop/resume stream · `z` clear buffer · `c` copy buffer ·
`ctrl-s` save to file · `F` fullscreen (no chrome, clean text selection) ·
`0`–`5` time anchors (tail · 1m · 5m · 15m · 30m · 1h) · `T` provider lookback
(VictoriaLogs views) · `esc` back. The newest line anchors to the bottom of the
viewport.

`t` shows or hides timestamps in the current buffer without restarting the
streams. Log lines keep their timestamp order in both display settings.

`m` adds a separator after the latest received log line. Repeated presses add
separate markers. Markers stay visible through filters and are excluded from
sofka copy/save. While paused, a marker is added at the buffer tail without
moving the viewport. Markers have no timestamp. They are removed when the
buffer is cleared or replaced, or when their position is trimmed. Marker count
is limited to the active log buffer cap. Manual terminal selection can include
visible markers.

## Document views (YAML, describe, diff, events)

`ctrl-f` / `ctrl-b` page forward or back, with `PgDn` / `PgUp` as aliases.
`/` searches like vim: the whole document stays on screen and every match is
highlighted. `n` / `N` go to the next or previous match. `w` wraps. `c` copies
the document. `esc` backs out - the first press clears an active search.

`F` toggles fullscreen for document views and plugin popup output. Fullscreen
uses the full terminal area without the application header, status line, key
hints, borders, or scrollbars. The title and active search or command prompt
remain visible. Search, scrolling, wrapping, copying, and refresh still work.
The setting stays on across refreshes and new documents for the current session.
It starts off in a new session and is separate from the Logs fullscreen setting.
`F` restores the normal layout. `esc` and `q` keep their existing behavior.

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

## Help panel (`?`)

`j` / `k` and `↑` / `↓` scroll one line. `ctrl-f`, `PgDn`, and `space` move
forward one page. `ctrl-b` and `PgUp` move back one page. The page size is the
number of visible content rows. `g` / `Home` go to the top; `G` / `End` go to
the bottom.

`/` filters the bindings. Opening help, starting a filter, and clearing a filter
reset the scroll position to the top. `esc` clears an active filter first, then
closes help. `q` or `?` closes help and returns to the previous screen.

## Explain view (`X`)

`j` / `k` move, `⏎` goes to the resource behind a finding (a blocking pod), `E`
its events, `l` its logs, `r` gathers again, `esc` goes back. After opening
logs or events, one `esc` returns to Explain and another returns to the table. A
finding you can drill into has a trailing `→`.

## Adjacent view (`u`)

Every object directly connected to the selection: what owns it, what it owns,
what its spec names (a pod's node, claims, ConfigMaps, Secrets), and what names
it (the pods mounting a claim, the claims using a class). `j` / `k` move, `⏎`
opens the object in its own view - name-filtered, so every action there applies
to it - `y` shows its YAML, `d` describes it, `r` gathers again, `esc` goes back.
Not offered on namespaces (`enter` re-scopes to one) or Helm rows.
See [Views](views.md#navigating-between-kinds) for adding CRD relations.

## Text inputs (palette, filters, prompts)

`ctrl-u` clears the line, `ctrl-w` / `alt-⌫` delete the previous word.

## Palette completion keys

In the `:` palette, `tab`/`↓` and `shift-tab`/`↑` move through the suggestion
list and `⏎` runs the highlighted one. Rebind them under `[keys.command]` in
`config.toml`, for example:

```toml
[keys.command]
down = "ctrl-n"
up = "ctrl-p"
accept = ["ctrl-y", "enter"]
```

Each value is one [key combination](plugins.md) or a list of combinations.
It replaces the default set for that action: `["tab", "down"]`,
`["backtab", "up"]`, or `["enter"]`. Include a default in the list to keep it.
An empty list disables the action. Invalid values report errors. Explicit
completion bindings take priority over text editing and cancellation.
To assign `ctrl-c` or `ctrl-e`, first move or disable `quit` or `compact` under
`[keys.global]`.

Legacy `palette_next`, `palette_prev`, and `palette_accept` settings are
automatically migrated to this format, with a `config.toml.bak` backup.
If the file cannot be updated, sofka warns and uses the converted settings in
memory. See [migration](keybindings.md#legacy-palette-migration) for managed
configs, conflicts, and invalid values.

## What suspends the TUI

Interactive actions (`e`, `s` for shell, `a`) suspend the TUI and shell out to
`kubectl`. Delete, scale, restart, set-image, suspend, resume, reconcile, and
port-forward go through the kube API (or a backgrounded process) directly.

## Plugin commands

| Command                            | Action                                                       |
| ---------------------------------- | ------------------------------------------------------------ |
| `:<plugin> [name=value ...]`       | Run a plugin with validated inputs.                          |
| `:plugin-cancel`                   | Stop the active plugin run and its temporary forward.        |
| `:sanitize [states=…] [dry_run=…]` | Delete the pods the namespace has finished with (pods view). |

`:sanitize` ships with sofka; see [Sanitize pods](../plugins/sanitize/README.md).
Installed packages add their commands and key chords to `?` help.
Use `:reload` after a package change.
See [Create a plugin package](plugin-authoring.md).
