# Key reference

The full keymap. `?` inside sofka shows the same thing, including your own
plugin, bookmark, and workspace chords. `:` and `?` work from every navigation
screen; closing either returns to the screen where it was opened. Text-entry
pickers keep both characters available as input.

## Table views

Use `:resource -n namespace --context context /filter` to apply a complete query.
Scope options precede the slash. Structured filter terms combine with spaces or
`&&`, with `||` for OR and `!(...)` for group negation. `/` edits the active filter
and Esc clears it. See [filtering](filtering.md)
for the grammar and selector persistence rules.

| Key                                           | Action                                                                                                                                                                                    |
| --------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `:resource -n ns --context ctx /filter`       | query resource, namespace, context, and filter together                                                                                                                                   |
| `:<resource>`                                 | command palette - fuzzy over kinds and built-in commands                                                                                                                                  |
| `:<resource> <ns>`                            | switch kind and namespace at once (`:deploy social`; `all`/`*` = all namespaces; the namespace tab-completes)                                                                             |
| `[` / `]`                                     | view history - back / forward through visited kind+namespace views                                                                                                                        |
| `Tab` / `shift-Tab`                           | next / previous common resource in the current namespace; cycle workspace views when one is open                                                                                          |
| `enter`                                       | drill down (workload/svc → pods, machinedeployment → machines, cronjob → jobs, node → pods, pod → containers, ns → re-scope, CRD → resources, or [views](views.md))                       |
| `esc`                                         | go back / pop the view stack / clear filter / clear marks                                                                                                                                 |
| `j`/`k`, `↓`/`↑`, `g`/`G`                     | navigate                                                                                                                                                                                  |
| `ctrl-f` / `ctrl-b`, `PgDn` / `PgUp`          | page forward / back - one screenful at a time                                                                                                                                             |
| `S` / `I`                                     | sort-column picker (fuzzy; ⏎ on the active column inverts) / invert sort direction; saved per kind by default (`remember_sort = false` disables this)                                     |
| `ctrl-e`                                      | compact mode: collapse the header + footer (for tiled/multiplexed panes)                                                                                                                  |
| `space`                                       | mark/unmark row for bulk actions                                                                                                                                                          |
| `/`                                           | filter: fuzzy text · `"exact"` · `/regex/` · `!inverse` · `-l`/`-f` selectors (server-side on ⏎) · `status=X` `cpu>500m` `age<2h`                                                         |
| `Ctrl+Z`                                      | toggle faults filter in pod views; configured actions take precedence; combine with `/`; press again to turn off                                                                          |
| `n` / `0`                                     | namespace switcher / all namespaces                                                                                                                                                       |
| `shift-j`                                     | jump to owner/controller                                                                                                                                                                  |
| `o`                                           | show the node the selected row names (pods built in; other kinds via `[views."…"].node`)                                                                                                  |
| `ctrl-r`                                      | refresh the watch                                                                                                                                                                         |
| `y` / `d` / `E`                               | view YAML / describe (`kubectl`) / live events                                                                                                                                            |
| `x`                                           | secrets: show `data` base64-decoded (as `stringData`) · PVCs: browse the volume                                                                                                           |
| `X` / `T`                                     | explain why the selection is unhealthy / session-local state-change timeline                                                                                                              |
| `u` / `:adjacent`                             | adjacent view: owners, children, and the objects the selection names or is named by (`⏎` opens one)                                                                                       |
| `c` (adjacent view)                           | Discover direct children of a namespaced custom resource. Search starts on request. Results can be incomplete; see the limits in [Features](features.md).                                 |
| `:gitops` / `:flux`                           | Flux owner, source, revisions & reconciliation chain for the selection (`⏎` to jump)                                                                                                      |
| `:can-i` / `:can-i <verb> <resource> [ns]`    | what you can do here / check a single action (`SelfSubjectAccessReview`)                                                                                                                  |
| `:journal` / `:audit`                         | session-local log of the mutating actions you've taken                                                                                                                                    |
| `:rightsize`                                  | historical right-sizing: P50/P95/P99 usage → suggested requests + patch preview (needs a metrics backend)                                                                                 |
| `:ctx` / `:ctx <name>`                        | context switcher popup (type to filter, `r` renames a context in the default kubeconfig, `space` toggles fleet membership for any context) / switch directly (the name tab-completes)     |
| `shift-1`…`shift-9`                           | switch to the nth cluster in the header's cluster strip. `ctrl-1`…`ctrl-9` does the same where the terminal sends it, which macOS terminals mostly do not                                 |
| `:kubeconfig` / `:kc`                         | manage the kubeconfig files and directories contexts come from (also `:kubeconfigs`; `j`/`k` move, `a` adds a file or directory, `d` removes, `⏎` opens the context switcher, `esc` back) |
| `:helm`                                       | Helm releases (native storage-Secret decode): ⏎ history → values · `y` manifest · `d` notes · `r` rollback                                                                                |
| `:fleet`                                      | cross-context health dashboard (opt-in: `[fleet]` contexts or `space` in `:ctx`; `⏎` switches, `r` refreshes)                                                                             |
| `:skin`                                       | switch the color skin live (`:skin gruvbox-dark` applies directly)                                                                                                                        |
| `:reload` / `:config` / `:info`               | reload config from disk · config sources + warnings · runtime diagnostics                                                                                                                 |
| `l` / `p`                                     | logs (workload = all matching pods) / previous-container logs                                                                                                                             |
| `L` / `:vlogs`                                | VictoriaLogs history for the selection (pod, container, workload, service, namespace)                                                                                                     |
| `c`                                           | copy resource name to clipboard                                                                                                                                                           |
| `Y`                                           | copy any cell of the selected row: picker over the displayed columns (type to match a column name or value), `⏎` copies                                                                   |
| `e`                                           | edit in `$EDITOR` (`kubectl edit`)                                                                                                                                                        |
| `s`                                           | shell into pod / shell into a PVC's volume / scale a workload (context-dependent)                                                                                                         |
| `a`                                           | attach to pod                                                                                                                                                                             |
| `:debug`                                      | pod: ephemeral debug container (`d` in the picker targets one) · node: privileged debug pod (previewed + confirmed)                                                                       |
| `:debug-clean`                                | delete the node debugger pods launched this session                                                                                                                                       |
| `:pvc-explore` / `:pvc-clean`                 | browse the selected PVC (also `:pvc-browse`; see below) · delete helper pods a previous session left behind (also `:pvc-cleanup`)                                                         |
| `:bundle` / `:bundle-save`                    | assemble a redacted diagnostic bundle for the selection · write the previewed bundle to a file                                                                                            |
| `:snapshot [text\|json\|yaml]` / `:snapshots` | capture the current view to a file · browse, open, and delete saved snapshots                                                                                                             |
| `:notify`                                     | toggle watch notifications on the selected object                                                                                                                                         |
| `:find <text>`                                | global fuzzy find over object names across common kinds, all namespaces                                                                                                                   |
| `i`                                           | set container image                                                                                                                                                                       |
| `r`                                           | rollout restart (workloads) / force-sync (ExternalSecrets/PushSecrets) / refresh (elsewhere)                                                                                              |
| `f` / `shift-f`                               | port-forward (pods/services) — picker shows declared ports, or "Custom…" for manual entry; active forwards show `●` next to the name                                                      |
| `t`                                           | Flux: suspend/resume/reconcile menu · ArgoCD App/AppSet: suspend/resume (App: + sync) · CronJobs: trigger/suspend/resume · pods: file transfer (`kubectl cp`)                             |
| `C` / `U` / `D`                               | nodes: cordon / uncordon / drain                                                                                                                                                          |
| `ctrl-d` / `ctrl-k`                           | delete / force-delete (marked rows, or current); in confirm: `f` toggles force, `c` cycles cascade (background → foreground → orphan)                                                     |
| `w`                                           | toggle wide-only columns (kubectl `-o wide`), including node labels                                                                                                                       |
| `←` / `→`                                     | scroll sideways by 5 text positions; NAMESPACE/NAME stay fixed; arrows show more content                                                                                                  |
| `:q`, `ctrl-c`                                | quit                                                                                                                                                                                      |
| `?`                                           | help                                                                                                                                                                                      |
| _(config)_                                    | plugin / bookmark / workspace key chords — `ctrl-`/`alt-`/`shift-`/`fN`; listed in `?` help                                                                                               |

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
`t` timestamps · `x` stop/resume stream · `z` clear buffer · `c` copy buffer ·
`ctrl-s` save to file · `F` fullscreen (no chrome, clean text selection) ·
`0`–`5` time anchors (tail · 1m · 5m · 15m · 30m · 1h) · `T` provider lookback
(VictoriaLogs views) · `esc` back. The newest line anchors to the bottom of the
viewport.

## Document views (YAML, describe, diff, events)

`ctrl-f` / `ctrl-b` page forward or back, with `PgDn` / `PgUp` as aliases.
`/` searches like vim: the whole document stays on screen and every match is
highlighted. `n` / `N` go to the next or previous match. `w` wraps. `c` copies
the document. `esc` backs out - the first press clears an active search.

In the describe view, `r` turns automatic refresh on or off. Refresh is off
when the view opens. When on, it runs `kubectl describe` immediately and then
5 seconds after each result. This updates the full document, including events.
The resource, scroll position, and search stay the same. Refresh stops when
you leave the view or a request fails. A failed request keeps the last result.

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
list and `⏎` runs the highlighted one. Rebind them under `[keys]` in
`config.toml` — emacs/cmp style, for example:

```toml
[keys]
palette_next   = "ctrl-n"
palette_prev   = "ctrl-p"
palette_accept = ["ctrl-y", "enter"]
```

Each value is one [key chord](plugins.md) or a list of chords, and replaces
the default set for that action (`["tab", "down"]`, `["backtab", "up"]`,
`["enter"]`) — include a default in the list to keep it too. `ctrl-c` (quit)
and `ctrl-e` (compact mode) are reserved by built-ins and can't be bound.

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
