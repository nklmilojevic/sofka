# Key reference

The full keymap. `?` inside sofka shows the same thing, including your own
plugin, bookmark, and workspace chords.

## Table views

| Key                                           | Action                                                                                                                                |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `:<resource>`                                 | command palette - fuzzy over kinds and built-in commands                                                                              |
| `:<resource> <ns>`                            | switch kind and namespace at once (`:deploy social`; `all`/`*` = all namespaces; the namespace tab-completes)                         |
| `[` / `]`                                     | view history - back / forward through visited kind+namespace views                                                                    |
| `Tab` / `shift-Tab`                           | cycle views of the active workspace (when one is open)                                                                                |
| `enter`                                       | drill down (workload/svc → pods, node → its pods, pod → containers, ns → re-scope, CRD → its resources)                               |
| `esc`                                         | go back / pop the view stack / clear filter / clear marks                                                                             |
| `j`/`k`, `↓`/`↑`, `g`/`G`                     | navigate                                                                                                                              |
| `ctrl-f` / `ctrl-b`, `PgDn` / `PgUp`          | page forward / back - one screenful at a time                                                                                         |
| `S` / `I`                                     | sort-column picker (fuzzy; ⏎ on the active column inverts) / invert sort direction — remembered per kind across views and restarts    |
| `ctrl-e`                                      | compact mode: collapse the header + footer (for tiled/multiplexed panes)                                                              |
| `space`                                       | mark/unmark row for bulk actions                                                                                                      |
| `/`                                           | filter: fuzzy text · `!inverse` · `-l`/`-f` selectors (server-side on ⏎) · `status=X` `cpu>500m` `age<2h`                             |
| `n` / `0`                                     | namespace switcher / all namespaces                                                                                                   |
| `shift-j`                                     | jump to owner/controller                                                                                                              |
| `o`                                           | show the node hosting the selected pod                                                                                                |
| `ctrl-r`                                      | refresh the watch                                                                                                                     |
| `y` / `d` / `E`                               | view YAML / describe (`kubectl`) / live events                                                                                        |
| `x`                                           | secrets: show `data` base64-decoded (as `stringData`)                                                                                 |
| `X` / `T`                                     | explain why the selection is unhealthy / session-local state-change timeline                                                          |
| `:gitops` / `:flux`                           | Flux owner, source, revisions & reconciliation chain for the selection (`⏎` to jump)                                                  |
| `:can-i` / `:can-i <verb> <resource> [ns]`    | what you can do here / check a single action (`SelfSubjectAccessReview`)                                                              |
| `:journal` / `:audit`                         | session-local log of the mutating actions you've taken                                                                                |
| `:rightsize`                                  | historical right-sizing: P50/P95/P99 usage → suggested requests + patch preview (needs a metrics backend)                             |
| `:ctx` / `:ctx <name>`                        | context switcher popup (type to filter, `r` renames, `space` toggles fleet membership) / switch directly (the name tab-completes)     |
| `:helm`                                       | Helm releases (native storage-Secret decode): ⏎ history → values · `y` manifest · `d` notes · `r` rollback                            |
| `:fleet`                                      | cross-context health dashboard (opt-in: `[fleet]` contexts or `space` in `:ctx`; `⏎` switches, `r` refreshes)                         |
| `:skin`                                       | switch the color skin live (`:skin gruvbox-dark` applies directly)                                                                    |
| `:reload` / `:config` / `:info`               | reload config from disk · config sources + warnings · runtime diagnostics                                                             |
| `l` / `p`                                     | logs (workload = all matching pods) / previous-container logs                                                                         |
| `L` / `:vlogs`                                | VictoriaLogs history for the selection (pod, container, workload, service, namespace)                                                 |
| `c`                                           | copy resource name to clipboard                                                                                                       |
| `Y`                                           | copy any cell of the selected row: picker over the displayed columns (type to match a column name or value), `⏎` copies               |
| `e`                                           | edit in `$EDITOR` (`kubectl edit`)                                                                                                    |
| `s`                                           | shell into pod / scale a workload (context-dependent)                                                                                 |
| `a`                                           | attach to pod                                                                                                                         |
| `:debug`                                      | pod: ephemeral debug container (`d` in the picker targets one) · node: privileged debug pod (previewed + confirmed)                   |
| `:debug-clean`                                | delete the node debugger pods launched this session                                                                                   |
| `:bundle` / `:bundle-save`                    | assemble a redacted diagnostic bundle for the selection · write the previewed bundle to a file                                        |
| `:snapshot [text\|json\|yaml]` / `:snapshots` | capture the current view to a file · browse, open, and delete saved snapshots                                                         |
| `:notify`                                     | toggle watch notifications on the selected object                                                                                     |
| `:find <text>`                                | global fuzzy find over object names across common kinds, all namespaces                                                               |
| `i`                                           | set container image                                                                                                                   |
| `r`                                           | rollout restart (workloads) / force-sync (ExternalSecrets/PushSecrets) / refresh (elsewhere)                                          |
| `f` / `shift-f`                               | port-forward (pods/services) — picker shows declared ports, or "Custom…" for manual entry; active forwards show `●` next to the name |
| `t`                                           | Flux: suspend/resume/reconcile menu · ArgoCD App/AppSet: suspend/resume (App: + sync) · CronJobs: trigger/suspend/resume · pods: file transfer (`kubectl cp`)                           |
| `C` / `U` / `D`                               | nodes: cordon / uncordon / drain                                                                                                      |
| `ctrl-d` / `ctrl-k`                           | delete / force-delete (marked rows, or current); in confirm: `f` toggles force, `c` cycles cascade (background → foreground → orphan) |
| `w`                                           | toggle wide-only columns (kubectl `-o wide`)                                                                                          |
| `←` / `→`                                     | scroll columns horizontally (NAMESPACE/NAME stay anchored) — for narrow panes                                                         |
| `:q`, `ctrl-c`                                | quit                                                                                                                                  |
| `?`                                           | help                                                                                                                                  |
| _(config)_                                    | plugin / bookmark / workspace key chords — `ctrl-`/`alt-`/`shift-`/`fN`; listed in `?` help                                           |

## Logs view

`/` filter (substring · `/regex/` · `!invert`) · `s`/`f` autoscroll · `w` wrap ·
`t` timestamps · `x` stop/resume stream · `z` clear buffer · `c` copy buffer ·
`ctrl-s` save to file · `F` fullscreen (no chrome, clean text selection) ·
`0`–`5` time anchors (tail · 1m · 5m · 15m · 30m · 1h) · `T` provider lookback
(VictoriaLogs views) · `esc` back. The newest line anchors to the bottom of the
viewport.

## Document views (YAML, describe, diff, events)

`/` searches like vim: the whole document stays on screen and every match is
highlighted. `n` / `N` go to the next or previous match. `w` wraps. `c` copies
the document. `esc` backs out - the first press clears an active search. In the
`?` help panel, `/` filters instead and narrows to matching keybinds.

## Explain view (`X`)

`j` / `k` move, `⏎` goes to the resource behind a finding (a blocking pod), `E`
its events, `l` its logs, `r` gathers again, `esc` goes back. A finding you can
drill into has a trailing `→`.

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
