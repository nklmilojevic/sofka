# Safety

## Read-only mode

`readonly = true` in the config disables every mutating action - delete, edit,
scale, shell, mutating plugins, uploads. The `--readonly` and `--write` flags set
the mode for the whole session and win over the config value, including over
per-cluster and per-context overrides, on every `:ctx` switch.

With no flag, switching into a context whose config sets `readonly = true`
enables read-only mode (shown as `[read-only]` in the header). Switching away
restores write mode. A common pattern is a per-context override that makes prod
read-only and repaints it in a light skin - see
[per-context overrides](configuration.md#per-cluster-and-per-context-overrides).

## Guardrails

`[[guardrails]]` turn rules like "never delete in prod", "always confirm drains",
and "no more than 5 at a time" into enforced rules instead of things you have to
remember.

Each rule matches on `contexts`, `namespaces`, `resources`, and `actions` globs
(all optional - an omitted glob matches everything). Rules can set `deny`
(block the action), `confirmation` (require confirmation), and `max_bulk`
(a row limit for one action). The gated `actions` are the destructive verbs sofka
performs directly: `delete`, `force-delete`, `drain`, `restart`, `shell` (exec),
`debug`, `node-debug`, `transfer` (a file upload into a pod), `pvc-explore`
(creating a helper pod to mount an unmounted PVC, and the `:pvc-clean` sweep
that deletes them - both matched against `persistentvolumeclaims`), and
`pvc-upload` (a file upload into a volume, matched the same way). The PVC
shell and the PVC upload also pass the `shell` and `transfer` rules for the
pod they reach the volume through, exactly like `s` and `t` on that pod's row -
a rule that already blocks shells or uploads in prod is not defeated by
reaching the same pod through a claim it mounts.

Measuring a copy's progress runs `du` in the pod, and the two directions cost
very different things. A download of a single file from the browser adds no
exec at all - its size is already on screen, from the listing, though a `t`
download of a typed path has no listing and so runs the probe - and a download of a directory
adds one `du`, which carries its own `timeout` inside the container. An
upload adds more: one long-lived exec that measures the destination once a
second for as long as the copy runs, because nothing else can see a volume
being written to from outside. That exec ends when sofka closes it, but a
sofka that is killed cannot close anything, and the loop then runs on in the
pod until its own bound expires - see [PVC explore](features.md#pvc-explore).

All of it is gated no further than the copy it belongs to, and recorded as
that one action in `:journal`, on the reasoning that a `du` reaches nothing a
permitted copy could not already reach: `kubectl cp` has always run `tar`
there.

sofka combines the restrictions from all matching rules:

- Any matching rule with `deny = true` blocks the action.
- The smallest matching `max_bulk` limit applies. If the number of targets
  exceeds this limit, sofka blocks the action.
- The strongest matching `confirmation` level applies: `type-context-name`
  is stronger than `type-resource-name`, which is stronger than a plain y/N
  confirmation. A rule cannot reduce the action's default confirmation level.
  For a bulk action, `type-resource-name` requires the target count.

Denial takes priority over the bulk limit and confirmation. Rule order does not
change these restrictions. For a denial, sofka shows the `reason` from the last
matching rule with `deny = true`, if that rule has a non-empty reason. Bulk-limit
warnings and confirmation prompts do not show a rule's `reason`.

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

## Managed-resource mutation warnings

Before you edit, delete, scale, or otherwise change an object that Flux, ArgoCD,
(or another controller) owns, sofka warns you that the next reconcile will revert the
change or recreate the object. The point is to send you to the source instead of
fighting the controller.

## Action-aware authorization

`:can-i` runs a `SelfSubjectRulesReview` and shows what you can do in the current
namespace. `:can-i <verb> <resource> [ns]` checks a single action before you try
it. Same answer as `kubectl auth can-i`, inside the TUI.

The empty command-palette browse list hides kinds you cannot `list`. An explicit
search includes every discovered kind because delegated authorizers can return
incomplete rule reviews. The API still enforces access when you open the kind.

## Action journal

`:journal` (or `:audit`) is a session-local in-memory log of every mutating
action you took - the action, the target, the context, the time - newest first.
Entries record actions started, not confirmed results. They contain identifiers
only, never secret input or decoded values. Optional [journal file
settings](configuration.md#action-journal-files) save entries to disk. Application
logging at `info` or above also records these actions.

## Plugin actions

Guardrails match plugin actions with `plugin:<palette>`.
If a plugin has no palette command, use `plugin:<name>`.
The pattern `plugin:*` matches all plugins.

Read-only mode blocks mutating plugins and plugins with `network_load = true`.
Network-load plugins require confirmation, even when they do not change Kubernetes resources.
See [Plugin safety controls](plugin-authoring.md#safety-controls).

## Node drain

A drain first cordons the node, then checks its pods. It stops for that node
if an eligible pod is blocked by one of the options below, or has no UID. The
error identifies the pod and the reason. No pods on that node are evicted when
this check fails. The node remains cordoned. Other selected nodes can still be
processed.

Three options decide what a drain may evict, named after the `kubectl drain`
flags they mirror. The confirm dialog shows their current values and toggles
them for this drain only:

| Option                 | Dialog key | Off (default)                                      | On                                        |
| ---------------------- | ---------- | -------------------------------------------------- | ----------------------------------------- |
| `ignore_daemonsets`    | `i`        | a DaemonSet pod refuses the node, as `kubectl` does | DaemonSet pods are skipped (**default**)  |
| `delete_emptydir_data` | `e`        | a pod with an `emptyDir` volume refuses the node   | those pods are evicted and their data is lost |
| `force`                | `f`        | a pod no controller will recreate refuses the node | those pods are evicted and not replaced   |

`ignore_daemonsets` defaults to on, the others to off, which is what drain did
before the options existed. `[drain]` in `config.toml` sets the defaults the
dialog opens with:

```toml
[drain]
ignore_daemonsets    = true
delete_emptydir_data = false
force                = false
```

DaemonSet pods are never evicted whichever way `ignore_daemonsets` is set:
their controller would recreate them on the cordoned node. The option decides
only whether they are skipped or refuse the drain. A missing pod UID always
refuses, since the eviction could not be pinned to the pod that was listed.

Drain uses the eviction API with the listed pod's UID. It does not use direct
pod deletion if eviction fails. This preserves PodDisruptionBudget checks and
prevents eviction of a replacement pod with the same name. A pod-specific
not-found response means the original pod is already gone. Other API errors
are reported, including an unavailable eviction endpoint.
