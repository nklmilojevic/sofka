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
mouse             = true   # false keeps the terminal's native mouse behavior
                           # (text selection) instead of scroll/click/sort

# Namespaces pinned to the top of the `n` switcher (★); session recents (·)
# follow them.
favorite_namespaces = ["kube-system", "monitoring"]

[aliases]
dep = "deployments"
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

## Other sections

Each of these is documented where the feature itself is:

| Section               | What it does                                 | Docs                                                       |
| --------------------- | -------------------------------------------- | ---------------------------------------------------------- |
| `[views]`             | custom table columns per resource            | [Views and thresholds](views.md)                           |
| `[thresholds]`        | RESTARTS/CPU/MEM/utilization coloring bands  | [Views and thresholds](views.md#thresholds)                |
| `[[plugins]]`         | shell-out commands bound to key chords       | [Plugins](plugins.md)                                      |
| `[[bookmarks]]`       | saved navigation commands                    | [Plugins](plugins.md#bookmarks)                            |
| `[[workspaces]]`      | named sets of views for one task             | [Plugins](plugins.md#workspaces)                           |
| `[[forwards]]`        | saved port-forwards, optionally autostarted  | [Plugins](plugins.md#saved-forwards)                       |
| `[[guardrails]]`      | enforced rules on destructive actions        | [Safety](safety.md#guardrails)                             |
| `[logs]`              | byte, line, stream caps and `since` lookback | [Log controls](debugging.md#log-controls)                  |
| `[notify]`            | bell, command, desktop, and watch cap        | [Notifications](debugging.md#notifications)                |
| `[keys]`              | palette completion key rebinds               | [Key reference](keys.md#palette-completion-keys)           |
| `[debug]`             | ephemeral and node debug images              | [Debug containers](debugging.md#debug-containers-and-pods) |
| `[bundle]`            | redaction and size caps for `:bundle`        | [Diagnostic bundles](debugging.md#diagnostic-bundles)      |
| `[providers.metrics]` | Prometheus/VictoriaMetrics for `:rightsize`  | [Providers](providers.md#right-sizing-metrics-provider)    |
| `[providers.logs]`    | VictoriaLogs backend for `L`                 | [Providers](providers.md#log-provider-victorialogs)        |
| `[fleet]`             | contexts in the cross-cluster dashboard      | [Providers](providers.md#fleet-dashboard)                  |

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
