# Changelog

## v0.34.0 - 2026-11-01

This release shows Argo CD sync progress and lets you pin table columns. It also fixes proxy login, `NO_PROXY` handling, table watches on Kubernetes 1.37, and filter selection.

### New features

- Shows Argo CD sync progress in the list and the Argo CD view. The SYNC column shows `Syncing`, and the view lists the sync result and the failed objects.
- Lets you choose which columns stay fixed during horizontal scrolling. Set `pin` in a view to list the headers that stay in view.

### Fixes

- Fixes proxy login for user names and passwords with special characters, such as `@`. Before, the proxy rejected these credentials with `407`.
- Fixes `NO_PROXY` handling with entries that the Kubernetes client cannot read, such as a trailing comma. Before, sofka stopped. Now sofka skips the entry, warns, and keeps an explicit kubeconfig `proxy-url`.
- Fixes the table watch on Kubernetes 1.37 clusters. sofka now requests uncompressed watch events.
- Keeps the selected object when the filter changes. The cursor stays on the object, and before, it moved to the first row.

### Other changes

- Changes how sofka builds Kubernetes API calls for namespaced and cluster-scoped kinds. The request URLs stay the same, so users see no change.

## v0.33.0 - 2026-10-29

sofka adds shell completions for the Nix package, Scoop install on Windows, and per-context badges. It also works when RBAC allows list but not watch.

### New features

- The Nix package now installs shell completions for bash, fish, and zsh.
- CPU values of one core or more now show as cores, such as `64` instead of `64000m`. Values below one core stay in millicores.
- Flux views show SUSPENDED next to READY. A suspended object is visible even when the message is long.
- Windows users can install sofka with `scoop install sofka`. The update check suggests the matching Scoop upgrade command.
- Set `[context_badge]` in a context override file to label a context, such as `prod`. The label shows in the header and in the `:ctx` picker.
- sofka works when RBAC allows list but not watch, because views poll with list every 2 seconds. Before, the views stayed empty with a retry error.

### Fixes

- sofka exits with a short error when the `--kubeconfig` file does not exist. Before, it opened a disconnected TUI that you had to quit by hand.

## v0.32.0 - 2026-10-22

This release changes how keys look in status bar and popup hints. It also adds a way to set a HelmRelease chart version and fixes log scrolling.

### New features

- Keys in the status bar and popup footers now appear in bold, in the key color. You can see each key more easily.
- Press `i` on a Flux `HelmRelease` to set `spec.chart.spec.version`. You no longer need to open the editor to change one field.

### Fixes

- Long log lines now scroll sideways with the arrow keys, or with `h` and `l`. You can read the end of a long line without turning on wrap.
- The namespaces header now shows `R resources`. You can find the `R` key in the view itself.

## v0.31.14 - 2026-10-20

This release fixes owner navigation in Shift-J and `:bundle`. A pod owned by a custom resource now opens the correct kind.

### Fixes

- Shift-J and `:bundle` now find the owner of a pod in the correct API group. Before this fix, a custom resource with the same kind name as a built-in kind opened the wrong kind. If the cluster does not serve the owner group, sofka reports the owner as unresolved.
