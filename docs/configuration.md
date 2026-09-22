# Configuration

sofka reads one config file under `$XDG_CONFIG_HOME/sofka` (or
`~/.config/sofka`). Use `config.toml`, `config.yaml`, or `config.yml`.
On Windows, Sofka uses `USERPROFILE` for the home directory when `HOME` is unset.
TOML remains the default. Both formats support the same settings, defaults,
validation, keybindings, and overrides. `:reload` reads the files again;
`:config` shows the sources and warnings. Settings that require a restart
have the same requirement in both formats.

Keep only one config file at each level: base, cluster, and context. If more
than one supported file exists at a level, sofka reports a conflict. At
startup, an invalid or conflicting base config uses defaults. A failed base
reload keeps the previous config. Conflicting or invalid override files are
skipped with warnings. A type error in the merged overrides uses the base
config, as with TOML.

Legacy palette keys are automatically moved to `keys.command`, with the
original file saved with a `.bak` suffix, such as `config.yaml.bak`. If the
file is read-only or managed through a symlink, sofka warns and uses the
converted settings in memory. See [palette migration](keybindings.md#legacy-palette-migration).

Everything below is optional. An empty config behaves like no config.

## Experimental native describe

Describe uses kubectl by default. Opt into the experimental Rust `deskribe`
backend in `config.toml`:

```toml
[experimental]
native_describe = true
```

Or in `config.yaml`:

```yaml
experimental:
  native_describe: true
```

Alternatively, `sofka --experimental-describe` enables it for that process,
regardless of config overrides. This changes the `d` action's backend, not other
kubectl-backed actions. The setting participates in normal cluster/context
overrides and `:reload`. Disabling it on reload cancels
an open native description's pending fetch/automatic refresh; reopen with `d` to
use kubectl. A CLI opt-in remains enabled across reloads and context switches.

Native descriptions reuse the current Kubernetes client and fetch fresh resource,
related-object, and event data. Custom resources use a generic native description.
If a native description is unsupported or the resource cannot be resolved, `d`
uses kubectl and labels the document **kubectl fallback**. Refresh retains that
backend. If this fallback fails, the error is shown, never cached YAML.
Native fetch and permission errors are shown directly without retrying kubectl.
As with kubectl,
service-account Secret descriptions can include tokens; treat copied output as
sensitive. Helm release notes are unchanged.

## Drop-in files

To split the configuration, put extra files in `conf.d/` next to the base
config. Every `*.toml`, `*.yaml`, and `*.yml` file in that directory is merged
over the base config, in file name order. This lets a team share one file and
keep personal settings in another:

```
~/.config/sofka/
├── config.toml
└── conf.d/
    ├── 10-team.yaml       # shared, for example CRD view settings
    └── 20-personal.toml   # applied after 10-team.yaml
```

TOML and YAML files can be mixed in `conf.d/`. Drop-in files merge before the
cluster and context overrides, so those keep the last word. The merge rules are
the same as for [overrides](#per-cluster-and-per-context-overrides): tables
merge key by key, and arrays like `[[plugins]]` replace the base value by default.
Use [inline plugin merging](#inline-plugin-merging) to combine plugins by name. An
invalid drop-in file is skipped with a warning. `:config` lists the drop-in
files and `:reload` reads them again.

The [Home Manager module](home-manager.md) can manage generated TOML settings
or existing TOML and YAML files, including cluster and context overrides.

## Inline plugin merging

By default, each file that contains `plugins` replaces the complete inherited
inline plugin list. To keep shared and personal plugins, set
`plugins_merge = "name"` at the top level of each file that must combine lists.

For example, keep personal plugins in `config.toml` and put this shared file in
`conf.d/10-team.toml`:

```toml
plugins_merge = "name"

[[plugins]]
name = "Team logs"
key = "ctrl-l"
command = "kubectl"
args = ["logs", "-f", "-n", "$NAMESPACE", "$NAME"]
scopes = ["pods"]
output = "terminal"
mutating = false
```

Names match exactly and are case-sensitive. A new name is added at the end of
the list. An existing name keeps its position, but the later entry replaces its
complete definition. Fields are not combined: include all required fields,
including `command`, in each replacement. This prevents old arguments or
permissions from carrying into a new command. If a name occurs more than once,
the last definition wins and the first position is kept.

Use `plugins_remove` to remove inherited inline entries by name. Removal runs
before the entries in the same file are applied. Unknown names have no effect.
A file can remove a name and then add a new definition with that name.

```toml
# conf.d/20-personal.toml
plugins_merge = "name"
plugins_remove = ["Team logs"]

[[plugins]]
name = "Personal logs"
palette = "personal-logs"
command = "kubectl"
args = ["logs", "--tail=100", "-n", "$NAMESPACE", "$NAME"]
scopes = ["pods"]
output = "popup"
mutating = false
```

Both controls apply only to the file that contains them. A later file uses
`plugins_merge = "replace"` unless it explicitly selects `"name"`. A file with
no `plugins` field keeps the inherited list, apart from requested removals.
In replace mode, `plugins = []` clears the inherited inline list. In name mode,
an empty list adds nothing. Other arrays still use replacement.

The controls work in TOML and YAML, in the base file, `conf.d/`, cluster files,
and context files. The order stays the same: base, drop-in files in file name
order, cluster, then context. `:reload` applies the same rules.

Package and bundled plugins load after inline configuration. These controls do
not disable those plugins. Removing an inline entry can expose a package or
bundled plugin that the entry previously replaced. Package terminal support is
unchanged.

## YAML format

Examples below use TOML unless marked as YAML. Use the same field names in
YAML. TOML sections become mappings; arrays of tables become lists of mappings.
For example, `[keys.table]` becomes `keys: {table: ...}`, and `[[plugins]]`
becomes a list under `plugins`.

```yaml
# config.yaml
default_namespace: kube-system
default_resource: deployments
readonly: false
favorite_namespaces:
  - kube-system
  - monitoring
aliases:
  dep: deployments
keys:
  table:
    page_down: f8
  command:
    down: [ctrl-n, down]
skin:
  name: gruvbox-dark
  colors:
    red: "#fb4934"
views:
  "*":
    sort: "AGE:desc"
plugins:
  - name: Get pods
    palette: get-pods
    command: kubectl
    args: [get, pods, --context, "$CONTEXT", -n, "$NAMESPACE"]
    target: context
    mutating: false
    output: popup
```

YAML rules:

- Use one document with a mapping at the root. Empty files, comment-only
  files, and `{}` use defaults.
- Use string mapping keys. Duplicate keys are errors.
- Omit a setting to use its default. Explicit `null` and `~` values are
  errors, including in lists and nested mappings.
- Anchors and aliases are supported. YAML merge keys (`<<`) and custom tags
  are not supported. Write explicit settings instead.
- Use `true` and `false` for booleans. Quote text that YAML could read as a
  different type. Quote colors such as `"#fb4934"` and wildcard keys such as
  `"*"`.
- Integers must fit in a signed 64-bit value, as in TOML.

## Namespace selection

The default order at startup and on context changes is: explicit destination,
saved namespace for the context, `default_namespace`, then the Kubernetes
namespace fallback.

Set `prefer_context_namespace = true` to use this order: explicit destination,
namespace explicitly set in the target kubeconfig context, saved namespace,
`default_namespace`, then the Kubernetes namespace fallback. An absent kubeconfig
namespace does not count as an explicit `default` namespace. Saved all-namespaces
selections remain valid history values.

Explicit CLI flags, resource commands, bookmarks, and workspaces keep their
priority. Namespace choices are still saved. The target context's effective
configuration controls the policy, including cluster and context overrides.
The setting also accepts YAML (`prefer_context_namespace: true`). `:reload`
updates the policy for later context changes and does not change the active
namespace.

## Base options

```toml
default_namespace = "kube-system"  # fallback only: the last namespace picked in a
                                   # context is remembered across restarts
prefer_context_namespace = false # true gives the kubeconfig namespace priority over history
default_resource  = "deployments"
readonly          = false  # true disables every mutating action (delete, edit,
                           # scale, shell, plugins, …); --readonly/--write win
hide_header = false # true hides the header and logo
compact_mode = false # true starts with a one-line header and no footer
detail_wrap = false # true starts with document line wrapping enabled
mouse             = false  # default; :mouse switches it for this session
                           # false keeps the terminal's native mouse behavior
                           # (text selection) instead of scroll/click/sort
mouse_scroll_lines = 3     # steps per wheel event in captured views; 1 for one step
terminal_title = true # false disables terminal title changes
remember_sort     = true   # save and restore sort choices per resource kind
                           # false makes sort changes temporary

# Namespaces pinned to the top of the `n` switcher (★); session recents (·)
# follow them. Keys 1 to 9 select the first nine entries in this fixed order.
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

Set `detail_wrap = true` to enable line wrapping at startup for document views,
including describe, YAML, diff, events, and plugin output. The default is `false`.
The `w` key changes wrapping for the current session. New documents, `:reload`,
and context switches keep this setting. The configuration is read at startup,
including cluster and context overrides. Changes made with `w` are not written
to the configuration file. Log wrapping is separate.

`mouse_scroll_lines` sets how many navigation steps one received mouse wheel
event triggers in views with mouse capture enabled (tables and pickers). The
default is `3`. Set it to `1` if your terminal or trackpad already sends several
wheel events per gesture and scrolling overshoots. `0` is treated as `1` and
values above `100` are capped at `100`, each reported as a config warning. Logs and document views release mouse capture and
receive terminal-generated arrow keys instead, so this setting does not control
their scroll speed. The option supports cluster and context overrides and
`:reload`.

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

The resource table actions `favorite_namespace_1` through
`favorite_namespace_9` select entries from `favorite_namespaces` in configuration
order. Recent namespace changes do not change these assignments. Extra entries
remain available through `n`. Empty or unconfigured slots do nothing. The same
keys work inside the namespace switcher while its filter is empty, and the
header shows the configured favourites next to the current namespace.

To change or disable a shortcut, use the existing key configuration:

```toml
[keys.table]
favorite_namespace_1 = "f1"
favorite_namespace_2 = []
```

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

Quantity path columns accept `format = "cpu"` or `format = "memory"` with
`type = "quantity"`. These formats use millicores or Mi/Gi for display and
the original values for sorting and numeric filters. See
[quantity formats](views.md#quantity-formats).

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

## Action journal files

The journal keeps the latest 500 entries in memory. To also save entries to disk:

```toml
[journal]
enabled = true
# file = "/path/to/journal.jsonl"
max_size_mb = 8
```

`enabled` defaults to `false`. No journal file is created when it is disabled.
`file` defaults to `<state-dir>/journal.jsonl`. An empty path uses the default.
Relative paths start at the working directory. The file is created on the first
recorded action. Restart sofka to apply changes to these settings.

`max_size_mb` limits each file to 8 MiB by default, with a minimum of 64 KiB.
The writer appends one JSON object per line. Before the next entry exceeds the
limit, it rotates the file to `<file>.1` and replaces the previous backup.
An entry larger than the limit is not saved and produces a warning.
A `<file>.lock` file coordinates writes from multiple sessions.
Use a separate path from the application log and other sofka state files.

Each entry has `at` (full UTC timestamp), `context`, `action`, and `target`.
Entries record actions started, not confirmed results. The journal view still
shows only the current session. It does not load saved entries.

File writes run on a background thread. Failed writes and a full queue produce
a status warning, also kept in `:info`. Failed entries are not retried; later
entries can still be saved. At exit, sofka waits up to 300 ms for pending writes
and reports a warning if they do not finish. Entries can be lost on a crash or
a stalled disk. This is local action history, not a complete audit record.

Secret inputs are excluded. File output uses the same credential and IP address
redaction as the application log. Resource names and contexts remain visible.
Application logging at `info` or above also records actions, independently of
`[journal]`.

## Node role labels

The node `ROLES` column always reads role names from the suffix of
`node-role.kubernetes.io/<role>` and the value of `kubernetes.io/role`.
Add custom sources with:

```toml
[node_roles]
label_prefixes = ["node-role.example.com/"]
label_keys = ["example.com/role"]
```

Each prefix selects matching label keys and uses the non-empty suffix as a role.
Each exact key uses its non-empty label value as a role. Empty prefixes are
ignored with a config warning. Roles from all sources are sorted and duplicates
are removed. If no role is found, the column shows `<none>`.

With no configuration, the output stays unchanged. Built-in sources always stay
active, including when configured arrays are empty. Listing a built-in source
again has no effect.

These settings support `:reload` and cluster/context overrides. Override arrays
replace the configured arrays from the previous level; built-in sources are then
added. Display, filtering, and sorting use the same role text. These settings do
not change Kubernetes objects or API requests.

A custom view column with `name = "ROLES"` replaces the built-in column and
uses its own source. A column with `builtin = "ROLES"` uses these settings.

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

Each file in this tree can instead be `config.yaml` or `config.yml`. Different
levels can use different formats. For example, a TOML base can have a YAML
cluster override and a TOML context override. Do not place two config formats
in the same directory.

Overrides merge over the base config, cluster level first, then context level.
Tables like `[aliases]` and `[skin.colors]` merge key by key. Everything else -
strings, booleans, and arrays like `[[plugins]]` - replaces the base value by
default. [Inline plugin merging](#inline-plugin-merging) can change this rule
for plugins in an individual file.

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

sofka reads packages from the `plugins/` directory next to the base config.
Each package directory contains a `plugin.toml` manifest.
Schema 2 uses `[[commands]]` entries, each with separate inputs and safety settings.
Sofka also reads schema 1 manifests with one `[plugin]` table.
Enter `:reload` to read package changes.
The `:config` view shows invalid packages and absent executables.

Inline `[[plugins]]` entries take priority over packages with the same name or palette command.
Packages load after cluster and context overrides.
An empty inline plugin list does not disable installed packages.

The [manifest reference](plugin-authoring.md#manifest) describes the package fields.
The [authoring guide](plugin-authoring.md) includes an adapter and tests without a cluster.

## Plugin catalog sources

Plugin commands read `catalogs.toml` from the Sofka configuration directory.
This file is separate from `config.toml` and `config.yaml`; cluster overrides
and `conf.d` do not apply to it.

```toml
official = false

[[catalogs]]
name = "team"
url = "/srv/sofka-plugins/index.json"
trusted = true
```

`official` defaults to `true`. Each entry in `catalogs` needs a unique `name`,
a local path or HTTP/HTTPS `url`, and explicit `trusted = true`. Relative paths
start at the Sofka configuration directory. An invalid configuration stops
catalog commands. See [Custom catalogs and offline installation](plugins.md#custom-catalogs-and-offline-installation)
for source selection, trust, update rules, and mirror preparation.
