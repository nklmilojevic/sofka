# Configuration

sofka reads `$XDG_CONFIG_HOME/sofka/config.toml` (or
`~/.config/sofka/config.toml`). `:reload` re-reads it live, `:config` shows the
sources and any warnings.

Legacy palette keys are automatically moved to `[keys.command]`, with the
original file saved as `config.toml.bak`. If the file is read-only or managed
through a symlink, sofka warns and uses the converted settings in memory.
See [palette migration](keybindings.md#legacy-palette-migration).

Everything below is optional. An empty config behaves exactly like no config.

## Base options

```toml
default_namespace = "kube-system"  # fallback only: the last namespace picked in a
                                   # context is remembered across restarts
default_resource  = "deployments"
readonly          = false  # true disables every mutating action (delete, edit,
                           # scale, shell, plugins, …); --readonly/--write win
hide_header = false # true hides the header and logo
compact_mode = false # true starts with a one-line header and no footer
mouse             = true   # false keeps the terminal's native mouse behavior
                           # (text selection) instead of scroll/click/sort
terminal_title = true # false disables terminal title changes
remember_sort     = true   # save and restore sort choices per resource kind
                           # false makes sort changes temporary

# Namespaces pinned to the top of the `n` switcher (★); session recents (·)
# follow them. Keys 1 to 9 select the first nine entries in this fixed order;
# slots left open go to the namespaces you visit. The keys are rebindable —
# see "Namespace shortcuts" below.
favorite_namespaces = ["kube-system", "monitoring"]

[aliases]
dep = "deployments"
```

`terminal_title` is enabled by default. The title is `sofka: <context>/<namespace>`;
`all` means all namespaces. It follows context and namespace changes, context
overrides, and `:reload`. Sofka sets the title again after an external command
returns. It clears the title when the setting is disabled, on normal exit, and
on a fatal main-thread panic. It does not restore the title from before startup.
Set `terminal_title = false` to keep your shell or terminal in control of the
title. Headless modes do not change the title.

Set `hide_header = true` to remove the header and logo and give more space to
the active view. The default is `false`. This option supports cluster and
context overrides and `:reload`. It also hides the one-line header in compact
mode (`Ctrl-E`). The footer and command input keep their existing behavior.

Set `compact_mode = true` to start with a one-line header containing resource,
namespace, context, and live status, with the footer hidden. The default is
`false`. `Ctrl-E` toggles compact mode during the session. Cluster and context
overrides are resolved at startup; `:reload` and subsequent context switches do
not reset the current compact mode. With `hide_header = true`, the compact
header is hidden too. Command and filter input still appears when needed.

`remember_sort` is enabled by default. Set it to `false` to stop saving and
restoring sort choices from `S`, `A`, `I`, and column header clicks. Existing saved
choices stay on disk and become available again when you enable the option.
The option supports cluster and context overrides and `:reload`. A reload
keeps the active sort; the option controls later sort changes and view starts.

To use an initial sort for all resource tables, set a global default:

```toml
remember_sort = false

[views."*"]
sort = "AGE:desc"
```

A resource-specific sort has priority over this default. Tables without the
specified column ignore the global sort. See [Views](views.md) for details.

CRD short names are discovered automatically. To override a short name, add it
under `[aliases]`. Use a group-qualified target when resource names overlap:

```toml
[aliases]
md = "machinedeployments.cluster.x-k8s.io"
```

### Namespace shortcuts

The resource table actions `favorite_namespace_1` through
`favorite_namespace_9` select entries from `favorite_namespaces` in
configuration order. Slots that list leaves empty go to the namespaces you
visit in the current context, in the order they were given one. An assignment
does not move while you switch namespaces: only a namespace that has no slot,
with none left free, claims one, and it takes the least recently used. A
configured entry never loses its slot. Extra entries remain available through
`n`. Slots with neither a favourite nor a visited namespace do nothing. The header lists the resulting mapping while
the terminal is wide enough for it.

To change or disable a shortcut, use the existing key configuration:

```toml
[keys.table]
favorite_namespace_1 = "f1"
favorite_namespace_2 = []
```

### Other key overrides

The age sort, selected namespace, and log marker actions can also be changed:

```toml
[keys.table]
sort_age = "A"
namespace_selected = "W"

[keys.logs]
log_marker = "m"
```

Use `[]` to disable an action. Custom keys use the normal conflict checks.

## Skins

```toml
[skin]
# name omitted: auto-detects dark/light and picks catppuccin-mocha/-latte.
# Or pick one explicitly: catppuccin-mocha, -latte, -frappe, -macchiato,
# gruvbox-dark, gruvbox-light, nord, dracula, solarized-dark, solarized-light,
# tokyo-night, one-dark, rose-pine, rose-pine-dawn, monokai, flexoki-dark,
# flexoki-light.
name = "gruvbox-dark"
background = true        # fill views with the skin's own background swatch
                         # (default: false = inherit the terminal background)

[skin.colors]            # optional per-swatch overrides
red = "#fb4934"
```

Every semantic color - row status, severity badges, headers, borders - is derived
from the active palette, so one skin change lands everywhere at once. `:skin`
switches live, `:skin gruvbox-dark` applies directly.

Custom text path columns accept `format = "image-tag"` to show only the image
tag. See the [image tag example](views.md#image-tags) for configuration and
validation rules.

## Other sections

Each of these is documented where the feature itself is:

| Section               | What it does                                | Docs                                                       |
| --------------------- | ------------------------------------------- | ---------------------------------------------------------- |
| `[views]`             | columns and navigation per view             | [Views and thresholds](views.md)                           |
| `[thresholds]`        | RESTARTS/CPU/MEM/utilization coloring bands | [Views and thresholds](views.md#thresholds)                |
| `[[plugins]]`         | shell-out commands bound to key chords      | [Plugins](plugins.md)                                      |
| `[[bookmarks]]`       | saved navigation commands                   | [Plugins](plugins.md#bookmarks)                            |
| `[[workspaces]]`      | named sets of views for one task            | [Plugins](plugins.md#workspaces)                           |
| `[[forwards]]`        | saved port-forwards, optionally autostarted | [Plugins](plugins.md#saved-forwards)                       |
| `[[guardrails]]`      | enforced rules on destructive actions       | [Safety](safety.md#guardrails)                             |
| `[logs]`              | log tail, follow buffer, `since` lookback   | [Log controls](debugging.md#log-controls)                  |
| `[notify]`            | bell and desktop notification delivery      | [Notifications](debugging.md#notifications)                |
| `[keys]`              | built-in keyboard bindings                  | [Key bindings](keybindings.md)                             |
| `[debug]`             | ephemeral and node debug images             | [Debug containers](debugging.md#debug-containers-and-pods) |
| `[bundle]`            | redaction and size caps for `:bundle`       | [Diagnostic bundles](debugging.md#diagnostic-bundles)      |
| `[logging]`           | sofka's own structured log file             | [Runtime diagnostics](debugging.md#runtime-diagnostics)    |
| `[pvc_explore]`       | helper pod image and TTL for PVC explore    | [PVC explore](features.md#pvc-explore)                     |
| `[providers.metrics]` | Prometheus/VictoriaMetrics for `:rightsize` | [Providers](providers.md#right-sizing-metrics-provider)    |
| `[providers.logs]`    | VictoriaLogs backend for `L`                | [Providers](providers.md#log-provider-victorialogs)        |
| `[fleet]`             | contexts in the cross-cluster dashboard     | [Providers](providers.md#fleet-dashboard)                  |

## Per-cluster and per-context overrides

Any option can be overridden for a specific cluster or kubeconfig context, like
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

Overrides merge over the base config, cluster level first, then context level.
Tables like `[aliases]` and `[skin.colors]` merge key by key. Everything else -
strings, booleans, and arrays like `[[plugins]]` - replaces the base value.

Directory names are the kubeconfig names, with any character that isn't a
letter, digit, `.`, `_`, or `-` replaced by `-`. So the EKS context
`arn:aws:eks:eu-west-1:123456789:cluster/prod` becomes
`arn-aws-eks-eu-west-1-123456789-cluster-prod`.

```toml
# clusters/prod-cluster/config.toml — make prod unmistakable and hands-off
readonly = true

[skin]
name = "catppuccin-latte"
background = true
```

A skin in an override sets the colors for that context. A context with no skin
keeps the session skin (config `skin.name`, the auto-detected default, or your
last `:skin` choice). Overrides are re-read on every `:ctx` switch, so edits
apply without a restart.

## Plugin packages

sofka reads packages from the `plugins/` directory next to `config.toml`.
Each package directory contains a `plugin.toml` manifest.
Enter `:reload` to read package changes.
The `:config` view shows invalid packages and absent executables.

Inline `[[plugins]]` entries take priority over packages with the same name or palette command.
Packages load after cluster and context overrides.
An empty inline plugin list does not disable installed packages.

The [manifest reference](plugin-authoring.md#manifest) describes the package fields.
The [authoring guide](plugin-authoring.md) includes an adapter and tests without a cluster.
