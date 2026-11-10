# Changelog

## v0.35.1 - 2026-11-10

This release fixes the cursor colors in resource tables.

### Fixes

- The cursor row in resource tables uses the solid lavender bar again, so it matches pickers and lists. The `gruvbox-light` skin also gets its previous surface colors back.

## v0.35.0 - 2026-11-10

sofka 0.34.0 adds a context picker key, line copy in logs and documents, and `:config edit`. It also fixes actions that hit the wrong target, lost failure messages, and credential leaks in files and terminal output.

### New features

- Adds a `contexts` table action. It opens the context picker and is unbound by default. Set a key under `[keys.table]` to use it.
- Adds `V` to select a range of lines in logs, YAML, describe, diff, and events. Press `y` to copy the selected lines.
- Adds `:config edit` to open a config file in your editor. sofka reloads the config when the editor closes.
- Adds Terraform support in the Flux views for tofu-controller. Use `t` to reconcile, suspend, or resume a Terraform. The list shows the applied revision and the plan state.
- Keeps the status colors on the cursor row in resource tables. You can read the status of a pod while the cursor is on it.
- Publishes a Sigstore bundle with each release and signs `SHA256SUMS`. The release package guide shows how to verify a download.

### Fixes

- Keeps action menu targets when rows change. Enter acts on the object that the menu opened for. The saved targets clear when the menu closes.
- Keeps the delete confirmation resource kind stable. A namespace lookup no longer changes the view during a delete.
- Shows action failures after a refresh or view change. The status bar no longer hides them.
- Saves bundles as private files. Masks known credential patterns in pod logs and in literal environment values with credential names.
- Passes values in imported k9s shell scripts as arguments. A value can no longer run as a command. sofka warns when a script needs manual changes.
- Quits cleanly on SIGTERM and SIGHUP. The terminal is restored before sofka exits.
- Merges namespace and sort changes from each process. One process no longer erases the changes of another.
- Removes credentials and control characters from text printed to the terminal, such as startup errors and plugin output.
- Keeps the selected pod in the faults view during bursts of watch updates.
- Ends kubectl options before resource and context names. A name that starts with `-` no longer changes the active context.
- Uses the configured drill for Cluster API MachineDeployments when you press Enter. Machines stays the default when no drill is set.
- Opens the default layout when you select a resource that names no layout.
- Resolves HelmRelease `dependsOn` entries as HelmReleases in the GitOps view. Before, they showed as Kustomizations.

### Other changes

- Replaces the `fs2` dependency with standard library file locks. The minimum Rust version is now 1.89.
- Explains that `:events` follows the current namespace. Press `0` to show all namespaces.

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
