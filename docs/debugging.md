# Debugging and incident workflow

## Explain unhealthy (`X`)

A deterministic, evidence-based answer to "why is this broken?" for the selected
object: rollout state, degraded conditions, the blocking pods and their container
failure reasons (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable,
failed probes), and recent Warning events. No AI, no external service.

`j`/`k` move, `⏎` goes to the resource behind a finding, `E` its events, `l` its
logs, `r` gathers again. A finding you can drill into has a trailing `→`.

## Timeline (`T`)

A per-object timestamped log of every state change the watch saw this session:
generation bumps, replica and readiness changes, pod phase, restarts, waiting
reasons, condition flips. Diffed from the watch stream, bounded in size, never
written to disk.

## Diff (`:diff`)

A unified diff of the live object against its `last-applied-configuration`. When
that annotation is missing - as it is for every Flux-, ArgoCD-, or Helm-managed object,
which nothing ever `kubectl apply`s - sofka diffs against the previous revision
this session's watch saw instead, so "what just changed?" has an answer on GitOps
clusters. The last revision of up to 256 changed objects is kept in memory.

## Notifications

`:notify` toggles a notification on the selected object. Sophie watches it so you
don't have to: every state change the watch sees (the same transitions the
timeline records - rollout progress, readiness, phase, restarts, waiting reasons,
conditions) flashes in the status line, rings the terminal bell, and fires a
**desktop notification**.

Each notify is its own bounded single-object watch, so it keeps firing while you
browse other views - "tell me when this rollout finishes" and keep working.
`:notify` on the same row turns it off. Everything is session-local.

Because each one holds a watch for the session, `max_watches` caps how many can
be active at once; turning one off always works, even at the cap. A successful
cluster-context switch stops the previous cluster's notification watches, so
they cannot consume the new context's budget or report stale changes. Delivery
is coalesced to one message per frame, so a rollout touching many notified
objects arrives as a single notification rather than a burst the sink would
rate-limit away. If all notifier subprocess slots are busy, one bounded
delivery is kept and later changes are counted into its summary until a slot is
free.

```toml
[notify]
bell = true         # ring the terminal bell
desktop = "osc777"  # "osc777" | "osc9" | "both" | "off"
max_watches = 25    # max objects watched at once (0 = no cap)
# command = ["notify-send", "sofka", "$MESSAGE"]     # Linux, inside tmux
# command = ["terminal-notifier", "-title", "sofka"] # macOS ($MESSAGE appended)
```

- `osc777` (default) - rxvt-style title+body, the form Ghostty recommends. Also
  kitty, WezTerm, foot, urxvt.
- `osc9` - iTerm2-style body-only, for iTerm2 and Windows Terminal, which speak
  only that.
- `both` and `off` are also valid. Terminals ignore protocols they don't speak.

Inside a **terminal multiplexer**, which swallows escape sequences from its panes,
set `command` to run a local notifier subprocess instead (`$MESSAGE` is
substituted as a whole argument, never through a shell).

In a **herdr** pane no config is needed at all: sofka detects the pane
environment and delivers through `herdr notification show`, so the toast follows
herdr's own `ui.toast` delivery (in-app, outer terminal, or system).

## Log controls

The kubelet logs view (`l`) keeps a bounded follow buffer. Tune the initial tail,
the buffer size, and an optional `since` lookback:

```toml
[logs]
tail = 300         # initial lines fetched per stream (kubectl --tail)
buffer = 5000      # max lines kept while following (oldest dropped)
buffer_bytes = 67108864 # max bytes kept while following (0 = no limit)
line_bytes = 16384 # max bytes kept per line (0 = keep whole lines)
max_streams = 50   # max concurrent streams an aggregate view opens (0 = no cap)
since = "1h"       # optional: only logs newer than this — replaces tail
fullscreen = false # open log views fullscreen (F toggles per session)
```

`buffer` and `buffer_bytes` both apply: a line count alone does not bound
memory, because one structured-log record can be megabytes on its own. A line
longer than `line_bytes` is cut and marked `…[N bytes truncated]`, so the loss
is visible in the buffer rather than silent. `buffer_bytes` also bounds batches
being assembled by producers or waiting in the UI channel; all streams in the
view share that queue budget. Multiline provider records stop at the per-batch
ceiling with an explicit omission marker.

An aggregate view — a label selector, or a workload with many pods — opens one
stream per container, each a live connection. `max_streams` caps how many run at
once; when the match is larger, the view says so with a `[partial] streaming N of
M containers` line rather than quietly showing part of the match. Containers are
covered in namespace/pod/container order, so the same selector always covers the
same set.

In the view, `/` filters with a case-insensitive substring, a `/regex/`, or a
leading `!` to invert (keep lines that don't match). A malformed regex is flagged
instead of hiding everything. `z` clears the on-screen buffer while the live
stream keeps appending. A pod streams every container's logs at once. Full keymap:
[Logs view](keys.md#logs-view).

For history that outlives the pod, use [VictoriaLogs](providers.md#log-provider-victorialogs).

## Debug containers and pods

`:debug` on a **pod** attaches a temporary ephemeral debug container with
`kubectl debug`. sofka prompts for the image (prefilled from `[debug]`). An empty
`command` starts an interactive shell (bash if the image has it, else sh), like
the pod shell. `d` in the container picker sets `--target=<container>` so the
debug container shares that container's process namespace. The ephemeral
container stays on the pod until the pod is recreated - Kubernetes can't remove
it, so there's nothing for sofka to clean up.

`:debug` on a **node** starts a privileged diagnostic pod on it
(`kubectl debug node/<node>`, image `node_image` in `node_namespace`, optional
`node_profile`). That pod mounts the host filesystem at `/host` and joins the host
PID, network, and IPC namespaces, so sofka previews exactly that access and makes
you confirm before creating it. sofka records the node debuggers it started this
session and `:debug-clean` deletes them (matched by the `node-debugger-*` name and
the node). kubectl leaves the pod behind after you exit, so clean up when you're
done.

```toml
[debug]
image = "nicolaka/netshoot:latest"       # ephemeral (in-pod) debug image
command = ["bash"]                       # entrypoint; omit for an interactive shell
node_image = "nicolaka/netshoot:latest"  # node debug pod image
node_namespace = "default"               # namespace the node debugger lands in
node_profile = "sysadmin"                # kubectl debug --profile (optional)
```

Read-only mode and [guardrails](safety.md#guardrails) gate both actions: the
`debug` action for pods, `node-debug` for nodes. Both are recorded in the
[journal](safety.md#action-journal).

## Diagnostic bundles

`:bundle` assembles a redacted incident bundle for the selected object - its YAML,
the owner, the incident explanation, recent events, the session timeline, bounded
recent logs, and a metrics snapshot - into one Markdown document. It's for handing
an incident between application and platform teams. sofka gathers it off-thread
and shows a preview, then `:bundle-save` writes it to a temp file.

Always redacted: Secret `data`/`stringData` values, any credential-like
annotation (a key containing `token`, `password`, `secret`, `apikey`,
`credential`, and similar), and `last-applied-configuration`, all replaced with a
placeholder. `managedFields` is dropped. Env vars sourced from Secrets are flagged
(their values are references, not literals). Every bundle carries a manifest of
exactly what it includes and what it withholds.

```toml
[bundle]
anonymize = false   # replace context/cluster identity with placeholders
log_lines = 200     # max recent log lines per pod
max_pods = 3        # cap how many pods contribute logs
```

## Snapshots

`:snapshot` captures the current table view - its columns and visible rows, plus
metadata (context, cluster, namespace, resource, filter, timestamp) - to a file.
An optional argument sets the format: `text` (default, an aligned table with a
header block), `json`, or `yaml`. Files land in
`$XDG_STATE_HOME/sofka/snapshots` (or `~/.local/state/sofka/snapshots`).

`:snapshots` browses saved captures, newest first with their age. `⏎` opens one in
a viewer with a staleness banner (it's a point-in-time capture), `d` deletes the
highlighted file.

This is not the one-frame `--snapshot` CI flag - this is an interactive
capture-and-review workflow.

## Runtime diagnostics

`:info` shows the version and build, config sources, live context/cluster/API
server and Kubernetes revision, discovery and Metrics API status, watch error
counts, and the state/snapshot/bundle directories. `sofka --info` prints the
static subset without connecting to a cluster. Identifiers and counts only,
never credentials, tokens, or Secret values.
