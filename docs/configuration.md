# Configuration

sofka reads `$XDG_CONFIG_HOME/sofka/config.toml` (or
`~/.config/sofka/config.toml`). `:reload` re-reads it live, `:config` shows the
sources and any warnings.

Everything below is optional. An empty config behaves exactly like no config.

## Base options

```toml
default_namespace = "kube-system"  # fallback only: the last namespace picked in a
                                   # context is remembered across restarts
default_resource  = "deployments"
readonly          = false  # true disables every mutating action (delete, edit,
                           # scale, shell, plugins, …); --readonly/--write win
hide_header = false # true hides the header and logo
mouse             = true   # false keeps the terminal's native mouse behavior
                           # (text selection) instead of scroll/click/sort
remember_sort     = true   # save and restore sort choices per resource kind
                           # false makes sort changes temporary
cluster_strip     = true   # show the quick-switch cluster strip under the header
                           # it hides itself until a second cluster has been visited

# Namespaces pinned to the top of the `n` switcher (★); session recents (·)
# follow them.
favorite_namespaces = ["kube-system", "monitoring"]

[aliases]
dep = "deployments"
```

Set `hide_header = true` to remove the header and logo and give more space to
the active view. The default is `false`. This option supports cluster and
context overrides and `:reload`. It also hides the one-line header in compact
mode (`Ctrl-E`). The footer and command input keep their existing behavior.

`remember_sort` is enabled by default. Set it to `false` to stop saving and
restoring sort choices from `S`, `I`, and column header clicks. Existing saved
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

## Kubeconfigs

sofka always reads the kubeconfig `kubectl` itself would use - `$KUBECONFIG`,
or `~/.kube/config` if that's unset. `[kubeconfigs] paths` lists extra
kubeconfigs to read contexts from as well. A path is either a kubeconfig file
or a directory of them:

```toml
[kubeconfigs]
paths = ["~/.kube/work.yaml", "~/.kube/configs"]
```

A directory contributes every kubeconfig found under it, up to three levels
deep, skipping dotfiles and anything that doesn't parse - which is how
collections managed by tools like kubeswitch are laid out. It is re-read every
time you open `:ctx` or `:kubeconfig`, so a file dropped into the directory
shows up without a restart or an index rebuild. Nothing outside the paths you
list is ever scanned.

Each extra file is read on its own rather than merged with the default
kubeconfig or with each other, so a context in it is never shadowed by a
same-named context elsewhere; it is shown and typed as `context@file`. A
leading `~/` is expanded.

`:kubeconfig` (also `:kubeconfigs`, `:kc`) adds and removes paths at runtime.
Those edits persist to `<state-dir>/kubeconfigs.toml` and overlay this list
on every start rather than rewriting `config.toml` - exactly like fleet
marks; see [Fleet dashboard](providers.md#fleet-dashboard).

`--kubeconfig` takes a file or a directory and may be repeated. Files replace
the session's own kubeconfig (they set `$KUBECONFIG`, so `kubectl` shell-outs
agree); directories are added as extra sources for that session only.

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
| `[keys]`              | palette completion key rebinds              | [Key reference](keys.md#palette-completion-keys)           |
| `[debug]`             | ephemeral and node debug images             | [Debug containers](debugging.md#debug-containers-and-pods) |
| `[bundle]`            | redaction and size caps for `:bundle`       | [Diagnostic bundles](debugging.md#diagnostic-bundles)      |
| `[logging]`           | sofka's own structured log file             | [Runtime diagnostics](debugging.md#runtime-diagnostics)    |
| `[pvc_explore]`       | helper pod image and TTL for PVC explore    | [PVC explore](features.md#pvc-explore)                     |
| `[providers.metrics]` | Prometheus/VictoriaMetrics for `:rightsize` | [Providers](providers.md#right-sizing-metrics-provider)    |
| `[providers.logs]`    | VictoriaLogs backend for `L`                | [Providers](providers.md#log-provider-victorialogs)        |
| `[fleet]`             | contexts in the cross-cluster dashboard     | [Providers](providers.md#fleet-dashboard)                  |
| `[kubeconfigs]`       | extra kubeconfig files and directories      | [Kubeconfigs](#kubeconfigs)                                |

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

Overrides key off the context and cluster names as reported by the
kubeconfig, not the file it came from, so a same-named context in a
different kubeconfig file resolves to the same override directory.

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
