# How sofka differs from k9s

sofka and [k9s](https://github.com/derailed/k9s) are terminal interfaces for
Kubernetes. This comparison covers sofka 0.31.0 and k9s 0.51.0.
Neither the implementation language nor the source code size proves that one
program is faster. See the [benchmark](benchmark-k9s.md) for measured results
and test limits.

## Shared functions

Both programs support custom resources, filters, sorting, skins, multiple row
selection, and background port-forwards. These functions are not exclusive to
sofka.

- **Custom resources:** sofka uses a shared `DynamicObject` pipeline with
  built-in columns and fallback columns. k9s combines resource-specific code
  with a [generic Kubernetes Table path](https://github.com/derailed/k9s/blob/v0.51.0/internal/dao/table.go).
  k9s does not need a dedicated renderer for every custom resource.
- **Skins:** sofka supplies built-in palettes and per-context settings.
  k9s also supports [custom skins and per-context settings](https://k9scli.io/topics/skins/).
- **Selection:** sofka uses `space` to mark rows for bulk actions.
  k9s also has [multiple row selection](https://github.com/derailed/k9s/blob/v0.51.0/internal/ui/select_table.go).
  The available actions depend on the resource and program.
- **Port-forwards:** sofka lists active forwards with `:pf`.
  k9s also has a port-forward view and supports background forwards. See the
  [k9s documentation](https://github.com/derailed/k9s/blob/v0.51.0/README.md#benchmark-your-applications).

## sofka workflows

These are reasons to try sofka. They do not establish that k9s has no equivalent
workflow or extension.

- **Flux CD and Argo CD actions:** `t` opens the supported action menu.
  sofka sends Kubernetes API requests without a `flux` or `argocd` executable.
  See [features](features.md) for resource and action coverage.
- **Incident view:** `X` shows rollout state, conditions, pod failure reasons,
  and recent Warning events. Jobs, CronJobs, PersistentVolumeClaims, and Nodes
  have their own checks. A finding can open the related resource, events, or
  logs. This view uses rules and cluster data.
- **Session timeline:** `T` shows object changes observed by the watch during
  the session. It is not a durable audit log.
- **Committed node capacity:** the nodes view shows the CPU and memory that
  pods request as a percentage of allocatable, with limits in wide mode. The
  scheduler places pods by requests, so these columns show why a pod does not
  schedule on a node with low usage. They work without Metrics Server.
- **Decoded Secret edit:** `e` in the decoded Secret view opens the values as
  plain text in `$EDITOR`. sofka encodes the result and patches only the keys
  that you changed, after a confirmation that names them.
- **Status display:** row colors, status badges, and configurable CPU, memory,
  and restart thresholds help identify resources that need attention.

## Moving from k9s

`sofka import k9s` converts k9s aliases, plugins, hotkeys, views, skins, and
per-context settings into sofka configuration, and lists what it could not
convert. See [Import from k9s](features.md#import-from-k9s).

sofka has its own Pulse (`:pulse`) and Xray (`:xray`) views, and Popeye is
available as a package from the official plugin registry. These functions do
not work the same way as in k9s. Users who depend on them must compare their
workflows before they switch. See the [k9s commands](https://k9scli.io/topics/commands/) and
[k9s project documentation](https://github.com/derailed/k9s/blob/v0.51.0/README.md).

## Performance design

sofka batches watch messages, caches row calculations, and uses generation tags
to reject messages from old watches. Its built-in resource actions use the
Kubernetes client. See [the event loop](../src/main.rs) and
[row calculations](../src/app/rows.rs).

These choices can reduce work. They do not prove a performance advantage over
k9s. Rust has no tracing garbage collector, but this does not guarantee smooth
redraws. Allocation, sorting, rendering, network delay, and scheduling can affect
both programs. The benchmark does not isolate garbage collection or establish
its effect on response time.
