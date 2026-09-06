# How sofka differs from k9s

sofka is a reimagining of [k9s](https://github.com/derailed/k9s) (~51k lines of
Go), not a line-by-line port. Same purpose - a fast, keyboard-driven cluster
navigator - different architecture: one generic object pipeline instead of one
renderer per resource kind.

For measured start time, memory use, command start time, and binary size, see the
[performance benchmark](benchmark-k9s.md).

## Design differences

- **One generic render pipeline, not one file per kind.** k9s has a Go file (a
  struct and a `ColorerFunc`) for every resource type it knows. sofka has one
  function that turns a `DynamicObject` into cells, with curated columns for
  common kinds and a NAME/AGE fallback for everything else. A CRD with no
  renderer still lists, sorts, and filters correctly on day one.
- **Flux CD and Argo CD are built in, not plugins.** `t` opens a
  suspend/resume/reconcile-now menu for Flux Kustomizations, HelmReleases,
  git/helm/oci repositories, buckets, image automation, and notification alerts
  and receivers, and a suspend/resume/sync-now menu for ArgoCD Applications
  (suspend/resume only for ApplicationSets). sofka patches `spec.suspend`,
  `spec.syncPolicy.automated`, the `reconcile.fluxcd.io/requestedAt` annotation,
  and the ArgoCD `operation` field through the Kubernetes API - no `flux` or
  `argocd` binary. ArgoCD suspend/resume stashes the original sync policy as a
  base64 annotation so `prune`, `selfHeal`, and `allowEmpty` survive the
  round-trip. Works with bulk multiselect too. `:gitops` goes the other way for
  either controller: from any object, to the Kustomization or Application that
  applied it, the source and revision behind it, and what is blocking the
  reconcile.
- **Port-forwards run in the background.** Starting one doesn't freeze the TUI
  for its lifetime. `:pf` lists the active forwards and stops them individually
  while the others keep running. sofka tears all of them down on quit instead of
  orphaning them.
- **Bulk actions with multiselect.** `space` marks rows for delete, kill, or
  Flux/ArgoCD suspend/resume/reconcile/sync across many resources at once.
- **CRD rows drill into their custom resources**, not their YAML. `enter` on a
  CustomResourceDefinition resolves its served version and lists the actual
  objects.
- **Skins, not one fixed palette.** Built-in Catppuccin, Gruvbox, Solarized,
  Nord, Dracula, Tokyo Night, One Dark, Rosé Pine, Rosé Pine Dawn, Monokai,
  and Flexoki, picked in the
  config with a per-swatch hex override. With no skin configured, sofka detects a
  light or dark terminal background. Every semantic color (row status, severity
  badges, headers, borders) is derived from the active palette, so one skin
  change is consistent everywhere. `background = true` fills views with the
  skin's own background. A light per-context skin makes prod unmistakable.
- **A combined row colorer.** sofka tints the whole row by status like k9s
  (healthy, error, pending, completed each read as one color), _and_ shows a
  separate STATUS badge and colors outlier values in RESTARTS, CPU, and MEM. So
  a crash-looping or resource-hungry pod stands out inside an otherwise uniform
  row. Warning and critical bands are configurable per resource and per context.
- **It explains _why_ something is broken.** `X` opens a deterministic
  evidence-based incident view: rollout state, degraded conditions, blocking pods
  and their container failure reasons (ImagePullBackOff, CrashLoopBackOff,
  OOMKilled, unschedulable, failed probes), and recent Warning events. No AI, no
  external service. `⏎`, `E`, or `l` jumps from a finding to the pod, its
  events, or its logs.
- **A session-local timeline.** `T` shows every state change the watch saw for an
  object - generation bumps, replica and readiness changes, pod phase, restarts,
  waiting reasons, condition flips - as a timestamped log. Computed from the
  watch stream, stored nowhere on disk.

## Why it's faster

Design choices you can verify in the source, not marketing numbers.

- **No garbage collector.** Rust's ownership model means no GC pauses. Watching
  thousands of pods or custom resources grows the in-memory store, but redraw
  latency stays smooth. A GC runtime gets jittery under constant allocation load.
- **Batched redraws.** The event loop drains every pending watch message before
  triggering one redraw (`while let Ok(m) = rx.try_recv()`). A rollout touching
  50 pods costs one render pass, not fifty.
- **Cached row computation.** Sorting and fuzzy filtering recompute only when the
  data or the filter text changes, guarded by a dirty flag - not on every frame
  or every keystroke across the full object set.
- **No subprocess overhead on hot paths.** Delete, scale,
  suspend/resume/reconcile/sync, and CRD drill-down are direct kube API calls
  (JSON merge-patches over the existing client). No forking `kubectl`, `flux`,
  or `argocd` per action.
- **Generation-tagged streams.** Changing views doesn't wait for the old watcher
  to tear down. A generation tag identifies stale messages and sofka drops them
  the instant a newer watch takes over, so navigation never stalls behind a slow
  stream.
