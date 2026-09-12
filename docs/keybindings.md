# Configure key bindings

All built-in keyboard actions can be changed in TOML or YAML config files.
See [YAML format](configuration.md#yaml-format). With no `[keys]`
settings, the default bindings remain active. `?` shows the effective bindings
for each mode. The header and footer show the first binding for each action.
An action with no binding is shown as `unbound`.

## Example: Ctrl+U and Ctrl+D for paging

```toml
[keys.navigation]
page_up = ["pageup", "ctrl-b", "ctrl-u"]
page_down = ["pagedown", "ctrl-f", "ctrl-d"]

[keys.table]
delete = "alt-d"
```

The example retains the listed paging keys and adds `ctrl-u` and `ctrl-d`.
The page size stays the same as before in each view. `ctrl-d` normally starts
deletion in tables, so the example moves deletion to `alt-d`. Deletion still
uses confirmation and read-only checks. In text inputs, `ctrl-u` still clears
the line.

## Values and scopes

- A string sets one key combination. An array sets multiple combinations.
- A configured value replaces the inherited bindings for that action. Include
  the default keys in the array if you want to keep them.
- An omitted action keeps its inherited bindings. An empty array (`[]`)
  disables all keyboard bindings for that action.
- `[keys.global]` sets `quit` and `compact` in every mode.
- `[keys.navigation]` sets shared navigation actions in modes that support them.
  It does not affect text input modes. The shared actions are `up`, `down`,
  `first`, `last`, `page_up`, `page_down`, `left`, `right`, `back`, `close`,
  `command`, and `help`. Shared `back` settings do not change confirmation
  dialogs. Shared `close` settings do not change the table's `exit` action or
  the global `quit` action; configure those separately.
- `[keys.input]` sets `clear_line`, `delete_word`, `backspace`, `back`, and
  `accept` in text input modes that support them.
- A mode table, such as `[keys.logs]` or `[keys.table]`, replaces the shared
  setting for that mode. File order does not affect this precedence.
- Explicit completion keys under `[keys.command]` take priority over text
  editing and cancellation. For example, use `down = "ctrl-w"` for the next
  suggestion or `accept = "esc"` to accept it.

Text input modes are `command`, `filter`, `log_filter`, `doc_filter`, `prompt`,
`sort_picker`, `copy_picker`, `namespaces`, and `context_filter`.
`contexts` is the context browser; `context_filter` is its text entry state.
Ordinary text and required confirmation text remain input. External editors
and shells use their own key bindings. Mouse wheel events still move rows,
even if the keyboard bindings for row movement change or are disabled.

For example, use a shared paging key, disable it in logs, and set two different
keys for the table:

```toml
[keys.navigation]
page_down = "alt-d"

[keys.logs]
page_down = []

[keys.table]
page_down = ["f8", "f9"]
```

Changes apply at startup, on `:reload`, and when a context switch resolves the
configuration. Cluster and context files use the normal configuration merge
rules: tables merge by key; strings and arrays replace earlier values.

## Conflicts and errors

Scoped settings reject conflicting actions in the same mode, except for the
palette completion priority described above. The same key can be used in
separate modes. Mode settings first replace shared settings, then validation
checks the resulting bindings.

Changing `page_down` to `ctrl-d` without moving or disabling `table.delete`
reports a conflict. Unknown scopes, unknown actions, invalid key combinations,
and incorrect value types report errors. Equivalent spellings,
such as `shift-tab` and `backtab`, count as the same binding.

At startup, invalid bindings leave the default keymap active. On reload or a
context switch, they leave the previous keymap active. Other valid configuration
settings still apply, including plugins, bookmarks, skins, and views. Custom
views apply at startup or on a context switch. Key value
errors do not reject the rest of the config file. Invalid TOML or YAML syntax can still
prevent a file from loading. `:config` shows the source paths and key errors.

Built-in bindings keep their current priority over bookmarks, workspaces, and
plugins. A released key becomes available to them. `:config` reports keys hidden
by a built-in action when that action is available. The existing exception stays:
bookmarks, workspaces, and matching plugins take priority over the table's
`faults` action.

The default confirmation dialog cancels on any unhandled key. Configuring
`[keys.confirm].back` replaces this fallback with the explicit bindings.
Shared navigation `back` settings leave the dialog's `n`, `q`, and other
cancellation keys unchanged. The mouse wheel also cancels a default dialog.
Set `[keys.confirm] back = []` to disable keyboard and wheel cancellation.
Disabling or changing the accept binding never changes required confirmation text.

Quit and compact mode have no reserved keys. If you disable all ways to open
help, open the command palette, or quit, edit the config outside sofka and
restart it to restore access.

## Legacy palette migration

When sofka loads a config with legacy palette fields, it moves the values to
`[keys.command]`:

| Old field under `[keys]` | New field under `[keys.command]` |
| ------------------------ | -------------------------------- |
| `palette_next`           | `down`                           |
| `palette_prev`           | `up`                             |
| `palette_accept`         | `accept`                         |

Migration preserves unrelated settings in both formats. TOML migration also
preserves comments. YAML migration writes the complete document again, so
formatting and key order can change, comments are removed, and aliases become
explicit values. Before replacing a file, sofka saves the exact original with
a `.bak` suffix in the same directory, such as `config.toml.bak`,
`config.yaml.bak`, or `config.yml.bak`. An existing
backup is never overwritten. Each base, cluster, or context file is converted
before the settings are merged, so override order stays the same.

If a file cannot be updated, sofka shows a warning and uses the converted keys
in memory. Symlinks and read-only files are left intact. For a Nix-managed
config, update the Nix source to use `[keys.command]`. The warning includes the
field mapping. After a manual change, use `:reload`.

If legacy and scoped keys define the same action in one file, or a legacy value is
invalid, sofka leaves that file unchanged and reports the problem. It does not
save migration results when the effective keymap has conflicts or errors.
Correct these settings and reload, or move them to the new format manually.

The old per-key fallback is removed. Values move without substitution, and an
empty list disables the action. Migrated keys use the same validation rules as
other scoped settings. To assign `ctrl-c` or `ctrl-e`, first move or disable
`quit` or `compact` under `[keys.global]`.

## Key syntax

Use the same syntax as [plugin bindings](plugins.md):

- Characters: `j`, `G`, `?`, `/`, `-`.
- Modifiers: `ctrl-u`, `alt-d`, `ctrl-alt-delete`, `shift-f5`.
- Named keys: `enter`, `tab`, `backtab`, `esc`, `space`, `backspace`, `delete`,
  `insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`.
- Function keys: `f1` through `f24`.

Letters without Ctrl are case-sensitive. `shift-g` and `G` are equivalent.
Ctrl-letter matching ignores letter case because terminals commonly send a
lowercase letter. `shift-tab` and `backtab` are equivalent. For shifted
punctuation, bind the resulting character, such as `_`, rather than `shift--`.

Some terminals send the same event for different physical key combinations.
For example, Ctrl+I may arrive as Tab and Ctrl+M as Enter. Bind the event that
the terminal sends. A terminal can also intercept a key before sofka receives
it. The keymap does not add a new terminal input protocol.

Default list navigation bindings also accept Shift+arrow, Shift+PageUp/Down,
and Shift+Home/End. These are explicit default aliases, shown in help. The
command palette retains its previous exact completion matching. User settings
replace the aliases, so use `["down", "shift-down"]` to accept both, or assign
the two combinations to separate actions.

Key sequences such as `gg`, macros, and new movement actions are not supported.

## Action reference

The tables below list local defaults without the shifted navigation aliases. Every mode also has `quit = "ctrl-c"` and
`compact = "ctrl-e"`. Each text input mode also has `clear_line = "ctrl-u"` and
`delete_word = ["ctrl-w", "alt-backspace", "ctrl-backspace"]`.

Navigation screens also have `command = ":"` and `help = "?"`. These are the
table, documents, logs, help, containers, confirm, dashboards, explain, timeline,
gitops, argocd, adjacent, action menus, port-forwards, skins, snapshots, fleet,
find, and PVC browser. In help, `?` closes the view instead.

`back` clears a search or returns to the previous view, depending on the mode.
`close` leaves the view directly. `accept` uses the selected item or input.
Table actions such as `shell_or_scale`, `restart_or_refresh`, and `action_menu`
keep their current resource-specific behavior. See the [default key reference](keys.md)
for those operations.

### `[keys.adjacent]`

| Action              | Default bindings |
| ------------------- | ---------------- |
| `accept`            | `enter`          |
| `back`              | `esc`            |
| `close`             | `q`              |
| `describe`          | `d`              |
| `discover_children` | `c`              |
| `down`              | `j`, `down`      |
| `first`             | `g`, `home`      |
| `last`              | `G`, `end`       |
| `refresh`           | `r`              |
| `up`                | `k`, `up`        |
| `yaml`              | `y`              |

### `[keys.command]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |
| `down`      | `tab`, `down`    |
| `up`        | `backtab`, `up`  |

### `[keys.confirm]`

| Action    | Default bindings     |
| --------- | -------------------- |
| `accept`  | `y`, `Y`, `enter`    |
| `back`    | `esc`, `n`, `N`, `q` |
| `cascade` | `c`, `C`             |
| `force`   | `f`, `F`             |

### `[keys.containers]`

| Action          | Default bindings |
| --------------- | ---------------- |
| `back`          | `esc`            |
| `close`         | `q`              |
| `debug`         | `d`              |
| `down`          | `j`, `down`      |
| `logs`          | `enter`, `l`     |
| `previous_logs` | `p`              |
| `provider_logs` | `L`              |
| `shell`         | `s`              |
| `transfer`      | `t`              |
| `up`            | `k`, `up`        |

### `[keys.context_filter]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |
| `down`      | `down`           |
| `up`        | `up`             |

### `[keys.contexts]`

| Action       | Default bindings |
| ------------ | ---------------- |
| `accept`     | `enter`          |
| `back`       | `esc`            |
| `down`       | `down`, `j`      |
| `filter`     | `/`              |
| `fleet_mark` | `space`          |
| `rename`     | `r`, `R`         |
| `up`         | `up`, `k`        |

### `[keys.copy_picker]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |
| `down`      | `down`, `ctrl-n` |
| `up`        | `up`, `ctrl-p`   |

### `[keys.detail]`

| Action           | Default bindings              |
| ---------------- | ----------------------------- |
| `auto_refresh`   | `r`                           |
| `back`           | `esc`                         |
| `close`          | `q`                           |
| `copy`           | `c`                           |
| `decode_secret`  | `x`                           |
| `down`           | `j`, `down`                   |
| `filter`         | `/`                           |
| `first`          | `g`, `home`                   |
| `last`           | `G`, `end`                    |
| `left`           | `h`, `left`                   |
| `next_match`     | `n`                           |
| `page_down`      | `pagedown`, `space`, `ctrl-f` |
| `page_up`        | `pageup`, `ctrl-b`            |
| `previous_match` | `N`                           |
| `right`          | `l`, `right`                  |
| `up`             | `k`, `up`                     |
| `wrap`           | `w`                           |

### `[keys.diff]`

| Action           | Default bindings              |
| ---------------- | ----------------------------- |
| `back`           | `esc`                         |
| `close`          | `q`                           |
| `copy`           | `c`                           |
| `down`           | `j`, `down`                   |
| `filter`         | `/`                           |
| `first`          | `g`, `home`                   |
| `last`           | `G`, `end`                    |
| `left`           | `h`, `left`                   |
| `next_match`     | `n`                           |
| `page_down`      | `pagedown`, `space`, `ctrl-f` |
| `page_up`        | `pageup`, `ctrl-b`            |
| `previous_match` | `N`                           |
| `right`          | `l`, `right`                  |
| `up`             | `k`, `up`                     |
| `wrap`           | `w`                           |

### `[keys.doc_filter]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |

### `[keys.events]`

| Action           | Default bindings              |
| ---------------- | ----------------------------- |
| `back`           | `esc`                         |
| `close`          | `q`                           |
| `copy`           | `c`                           |
| `down`           | `j`, `down`                   |
| `filter`         | `/`                           |
| `first`          | `g`, `home`                   |
| `last`           | `G`, `end`                    |
| `left`           | `h`, `left`                   |
| `next_match`     | `n`                           |
| `page_down`      | `pagedown`, `space`, `ctrl-f` |
| `page_up`        | `pageup`, `ctrl-b`            |
| `previous_match` | `N`                           |
| `right`          | `l`, `right`                  |
| `up`             | `k`, `up`                     |
| `wrap`           | `w`                           |

### `[keys.explain]`

| Action    | Default bindings |
| --------- | ---------------- |
| `accept`  | `enter`          |
| `back`    | `esc`            |
| `close`   | `q`              |
| `down`    | `j`, `down`      |
| `events`  | `E`              |
| `first`   | `g`, `home`      |
| `last`    | `G`, `end`       |
| `logs`    | `l`              |
| `refresh` | `r`              |
| `up`      | `k`, `up`        |

### `[keys.filter]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |

### `[keys.find]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `first`  | `g`, `home`      |
| `last`   | `G`, `end`       |
| `up`     | `k`, `up`        |

### `[keys.fleet]`

| Action    | Default bindings |
| --------- | ---------------- |
| `accept`  | `enter`          |
| `back`    | `esc`            |
| `close`   | `q`              |
| `down`    | `j`, `down`      |
| `refresh` | `r`              |
| `up`      | `k`, `up`        |

### `[keys.flux_menu]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `up`     | `k`, `up`        |

### `[keys.argocd]`

| Action              | Default bindings |
| ------------------- | ---------------- |
| `accept`            | `enter`          |
| `back`              | `esc`            |
| `close`             | `q`              |
| `discover_children` | `c`              |
| `down`              | `j`, `down`      |
| `first`             | `g`, `home`      |
| `last`              | `G`, `end`       |
| `refresh`           | `r`              |
| `up`                | `k`, `up`        |

### `[keys.gitops]`

| Action    | Default bindings |
| --------- | ---------------- |
| `accept`  | `enter`          |
| `back`    | `esc`            |
| `close`   | `q`              |
| `down`    | `j`, `down`      |
| `first`   | `g`, `home`      |
| `last`    | `G`, `end`       |
| `refresh` | `r`              |
| `up`      | `k`, `up`        |

### `[keys.help]`

| Action      | Default bindings              |
| ----------- | ----------------------------- |
| `back`      | `esc`                         |
| `close`     | `q`, `?`                      |
| `down`      | `j`, `down`                   |
| `filter`    | `/`                           |
| `first`     | `g`, `home`                   |
| `last`      | `G`, `end`                    |
| `page_down` | `pagedown`, `space`, `ctrl-f` |
| `page_up`   | `pageup`, `ctrl-b`            |
| `up`        | `k`, `up`                     |

### `[keys.log_filter]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |

### `[keys.logs]`

| Action       | Default bindings    |
| ------------ | ------------------- |
| `anchor_0`   | `0`                 |
| `anchor_1`   | `1`                 |
| `anchor_2`   | `2`                 |
| `anchor_3`   | `3`                 |
| `anchor_4`   | `4`                 |
| `anchor_5`   | `5`                 |
| `back`       | `esc`               |
| `clear`      | `z`                 |
| `close`      | `q`                 |
| `copy`       | `c`                 |
| `down`       | `j`, `down`         |
| `filter`     | `/`                 |
| `first`      | `g`, `home`         |
| `follow`     | `s`, `f`            |
| `fullscreen` | `F`                 |
| `last`       | `G`, `end`          |
| `lookback`   | `T`                 |
| `page_down`  | `pagedown`, `space` |
| `page_up`    | `pageup`            |
| `save`       | `ctrl-s`            |
| `stream`     | `x`                 |
| `timestamps` | `t`                 |
| `up`         | `k`, `up`           |
| `wrap`       | `w`                 |

### `[keys.namespaces]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |
| `down`      | `down`           |
| `up`        | `up`             |

### `[keys.port_forward_picker]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `up`     | `k`, `up`        |

### `[keys.port_forwards]`

| Action   | Default bindings |
| -------- | ---------------- |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `start`  | `enter`          |
| `toggle` | `x`, `s`         |
| `up`     | `k`, `up`        |

### `[keys.prompt]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |

### `[keys.pulse]`

| Action    | Default bindings |
| --------- | ---------------- |
| `back`    | `esc`            |
| `close`   | `q`              |
| `refresh` | `r`              |

### `[keys.pvc_explore]`

| Action        | Default bindings |
| ------------- | ---------------- |
| `accept`      | `enter`          |
| `back`        | `esc`            |
| `close`       | `q`              |
| `copy`        | `c`              |
| `down`        | `j`, `down`      |
| `first`       | `g`, `home`      |
| `last`        | `G`, `end`       |
| `left`        | `left`           |
| `parent`      | `backspace`, `-` |
| `refresh`     | `r`              |
| `right`       | `right`          |
| `shell`       | `s`              |
| `switch_pane` | `tab`, `backtab` |
| `up`          | `k`, `up`        |

### `[keys.set_image]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `up`     | `k`, `up`        |

### `[keys.skins]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `up`     | `k`, `up`        |

### `[keys.snapshots]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `delete` | `d`              |
| `down`   | `j`, `down`      |
| `up`     | `k`, `up`        |

### `[keys.sort_picker]`

| Action      | Default bindings |
| ----------- | ---------------- |
| `accept`    | `enter`          |
| `back`      | `esc`            |
| `backspace` | `backspace`      |
| `down`      | `down`, `ctrl-n` |
| `up`        | `up`, `ctrl-p`   |

### `[keys.table]`

| Action               | Default bindings     |
| -------------------- | -------------------- |
| `action_menu`        | `t`                  |
| `adjacent`           | `u`                  |
| `all_namespaces`     | `0`                  |
| `attach`             | `a`                  |
| `back`               | `esc`                |
| `copy_cell`          | `Y`                  |
| `copy_name`          | `c`                  |
| `cordon`             | `C`                  |
| `delete`             | `ctrl-d`             |
| `describe`           | `d`                  |
| `down`               | `j`, `down`          |
| `drain`              | `D`                  |
| `edit`               | `e`                  |
| `events`             | `E`                  |
| `exit`               | `q`                  |
| `explain`            | `X`                  |
| `faults`             | `ctrl-z`             |
| `filter`             | `/`                  |
| `first`              | `g`, `home`          |
| `force_delete`       | `ctrl-k`             |
| `history_back`       | `[`                  |
| `history_forward`    | `]`                  |
| `inspect`            | `x`                  |
| `invert_sort`        | `I`                  |
| `last`               | `G`, `end`           |
| `left`               | `left`               |
| `logs`               | `l`                  |
| `mark`               | `space`              |
| `namespaces`         | `n`                  |
| `next_view`          | `tab`                |
| `node`               | `o`                  |
| `open`               | `enter`              |
| `owner`              | `J`                  |
| `page_down`          | `pagedown`, `ctrl-f` |
| `page_up`            | `pageup`, `ctrl-b`   |
| `port_forward`       | `f`, `F`             |
| `previous_logs`      | `p`                  |
| `previous_view`      | `backtab`            |
| `provider_logs`      | `L`                  |
| `refresh`            | `ctrl-r`             |
| `restart_or_refresh` | `r`                  |
| `right`              | `right`              |
| `set_image`          | `i`                  |
| `shell_or_scale`     | `s`                  |
| `sort`               | `S`                  |
| `timeline`           | `T`                  |
| `uncordon`           | `U`                  |
| `up`                 | `k`, `up`            |
| `wide`               | `w`                  |
| `yaml`               | `y`                  |

### `[keys.timeline]`

| Action  | Default bindings |
| ------- | ---------------- |
| `back`  | `esc`            |
| `close` | `q`              |
| `down`  | `j`, `down`      |
| `first` | `g`, `home`      |
| `last`  | `G`, `end`       |
| `up`    | `k`, `up`        |

### `[keys.transfer_menu]`

| Action   | Default bindings |
| -------- | ---------------- |
| `accept` | `enter`          |
| `back`   | `esc`            |
| `close`  | `q`              |
| `down`   | `j`, `down`      |
| `up`     | `k`, `up`        |

### `[keys.xray]`

| Action    | Default bindings |
| --------- | ---------------- |
| `back`    | `esc`            |
| `close`   | `q`              |
| `down`    | `j`, `down`      |
| `first`   | `g`, `home`      |
| `last`    | `G`, `end`       |
| `logs`    | `enter`, `l`     |
| `refresh` | `r`              |
| `up`      | `k`, `up`        |
