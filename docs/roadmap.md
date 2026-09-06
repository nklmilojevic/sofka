# Roadmap

Milestone status. Longer-form thinking on direction lives in
[`future.md`](../future.md).

## Milestone 1: power-user foundation

- [x] Custom columns and view overlays.
- [x] CRD `additionalPrinterColumns` support.
- [x] Structured filter parser.
- [x] Server-side label and field selectors.
- [x] Config reload and validation view.
- [x] Wide/narrow column visibility.

## Milestone 2: actionable health

- [x] Container metrics.
- [x] Request/limit percentages and QoS.
- [x] Configurable thresholds.
- [x] Explain-unhealthy view.
- [x] Direct evidence navigation.
- [x] Initial session-local timeline.

## Milestone 3: extensibility and repeatable workflows

- [x] Modifier-aware hotkeys.
- [x] Rich plugin execution and output modes.
- [x] Bulk plugins.
- [x] Bookmarks and saved queries.
- [x] Operational workspaces.
- [x] Namespace favourites and recents.

## Milestone 4: GitOps and safety

- [x] Flux and Argo CD ownership and dependency navigation.
- [x] Revision and reconciliation-chain visibility.
- [x] Managed-resource mutation warnings.
- [x] Action-aware authorization checks.
- [x] Declarative guardrails.
- [x] Local action journal.
- [x] ArgoCD Application suspend/resume/sync controls.
- [x] ArgoCD Application/ApplicationSet/AppProject columns and sync chain.

## Milestone 5: debugging and collaboration

- [x] Ephemeral container workflow.
- [x] Node debug pod workflow.
- [x] Redacted diagnostic bundles.
- [x] Screen dumps and structured snapshots.
- [x] Runtime diagnostics. _(Structured application logging is still to come.)_
- [x] Richer log controls.

## Milestone 6: fleet and integrations

- [x] Opt-in cross-context health dashboard (`:fleet`).
- [x] Historical metrics provider interface (Prometheus/VictoriaMetrics:
      autodiscovery or configured URL).
- [x] Log provider interface (VictoriaLogs: autodiscovery or configured URL).
- [ ] Trace provider interface.
- [ ] Extended relationship graph.
- [ ] Vulnerability scanner integration.
- [x] Historical right-sizing recommendations (`:rightsize`).

## Milestone 7: distribution polish

- [ ] Signed and notarized macOS releases.
- [ ] Homebrew distribution.
- [ ] Checksums, attestations, and SBOM.
- [ ] Evaluate Windows support.
- [ ] Document a Kubernetes compatibility matrix.
