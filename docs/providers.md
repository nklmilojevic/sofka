# Providers and fleet

Both providers autodiscover in-cluster by default. Configure them only to point
at an external endpoint or to change the defaults.

## Right-sizing (metrics provider)

`:rightsize` on a workload (or pod) estimates right-sized requests from past
usage in a **Prometheus-compatible** backend - Prometheus or VictoriaMetrics,
which share the query API. Per container it shows the current requests, P50/P95/P99
CPU and memory over the window, a suggested request (P95 plus headroom), OOM and
throttle evidence, and a **strategic-merge patch preview** (`c` copies it). It
**never mutates** - apply the patch with `kubectl patch` yourself if you agree.

Pod queries match the exact name. Workload queries match the standard pod-name
format for the selected controller: a template hash and random suffix for
Deployments, an ordinal for StatefulSets, and a random suffix for ReplicaSets
and DaemonSets. This excludes pods from workloads with longer name prefixes,
such as `api-worker` when `api` is selected, while retaining past rollout data.
Names are matched literally, including dots. Matching uses names, not verified
owner references; adopted pods with other names are not included, and a name
that follows the same format can still match. The backend must contain data
for the selected cluster and namespace.

The patch uses `spec.containers` for a Pod and `spec.template.spec.containers`
for a workload. Kubernetes can still reject a resource change that the cluster
does not support.

With no `[providers.metrics]` section, sofka finds a Prometheus or
VictoriaMetrics query `Service` in the cluster by well-known labels and reaches
it through the API-server proxy.

```toml
[providers.metrics]
type = "prometheus"        # or "victoriametrics" (same query API)
url = "https://prom.example.com"   # omit to autodiscover in-cluster
window = "7d"              # lookback for the P50/P95/P99 quantiles
step = "5m"                # subquery resolution for the CPU rate()
headroom = 15              # percent added over P95 for the suggestion

[providers.metrics.headers]        # optional
Authorization = "Bearer <token>"
```

It uses the standard cAdvisor metric names
(`container_cpu_usage_seconds_total`, `container_memory_working_set_bytes`,
`container_oom_events_total`, `container_cpu_cfs_throttled_periods_total`).
VictoriaMetrics **cluster** mode (vmselect) needs a tenant path in the `url`.
Single-node VM and Prometheus serve the API at the root and autodiscover fine.

## Log provider (VictoriaLogs)

`L` (or `:vlogs`) opens log history for the selection - pod, container, workload,
service, or whole namespace - from a VictoriaLogs backend instead of the kubelet:
a lookback query and a live tail, in the same logs view. It covers restarted and
deleted pods, because the backend still has what the kubelet dropped.

With no configuration, sofka finds the VictoriaLogs `Service` by its well-known
labels (Helm charts and the VictoriaMetrics operator), queries it through the
Kubernetes API-server service proxy, and reuses your kubeconfig credentials.

```toml
[providers.logs]
type = "victorialogs"
url = "https://vlogs.example.com"  # omit to autodiscover in-cluster
lookback = "1h"                    # initial query window (s/m/h/d)
limit = 300                        # lines fetched by the initial query

[providers.logs.headers]           # optional, sent with every request
Authorization = "Bearer <token>"

# Field names as ingested by your log shipper. Omit this section to let
# sofka detect the convention from the backend's stream fields — vector,
# fluentd, fluent-bit, OpenTelemetry, and bare namespace/pod/container
# names are recognized. Configure only for exotic pipelines.
[providers.logs.fields]
namespace = "kubernetes.pod_namespace"
pod = "kubernetes.pod_name"
container = "kubernetes.container_name"
```

## Log links (Cloud Logging and other log UIs)

Instead of showing logs in sofka, `L` can open the selection in the log UI you
already use, with the filter set. sofka doesn't query that backend; the browser
does.

On GKE this works with no configuration. When the kubeconfig cluster is named
`gke_<project>_<location>_<cluster>` (what `gcloud container clusters
get-credentials` writes) and there is no `[providers.logs]` section, `L` opens
the Cloud Logging Logs Explorer for the project, filtered to the selection:

- a pod by `pod_name`, or one container from the container picker
- a Deployment, StatefulSet, DaemonSet, ReplicaSet, Job, or Service by its pod
  selector, so it covers every pod the workload owned, including deleted ones
- a CronJob by GKE's `top_level_controller` labels
- a namespace or a node by its own field

The detection uses the kubeconfig cluster name, never the context name, so you
can rename contexts freely. To keep VictoriaLogs on a GKE cluster, set
`type = "victorialogs"`. To use Cloud Logging when the cluster name does not
follow the gcloud format, there is no workaround yet: sofka needs the project,
location, and cluster from it.

```toml
[providers.logs]
type = "gcp"
lookback = "1h"   # optional: the time range the Logs Explorer opens with
```

For any other log UI, `type = "link"` fills a URL template. Each placeholder is
URL-encoded:

| Placeholder   | Value                                           |
| ------------- | ----------------------------------------------- |
| `{context}`   | kubeconfig context name                         |
| `{cluster}`   | kubeconfig cluster name                         |
| `{namespace}` | namespace of the selection                      |
| `{kind}`      | resource of the selection, e.g. `deployments`   |
| `{name}`      | name of the selection                           |
| `{pod}`       | pod name for a pod, empty otherwise             |
| `{container}` | container from the container picker, else empty |
| `{lookback}`  | the `lookback` setting, `1h` by default         |

```toml
[providers.logs]
type = "link"
url = "https://kibana.example.com/app/discover#/?_g=(time:(from:now-{lookback}))&_a=(query:(language:kuery,query:'kubernetes.namespace:{namespace} and kubernetes.pod.name:{pod}'))"
lookback = "1h"
```

Check the generated link against your own log UI. URL formats differ between
versions and installs.

With no browser (over SSH, or no `open`/`xdg-open`), sofka copies the link to
the clipboard and shows it in the status bar.

Like every section, `[providers.logs]` can live in a per-cluster or per-context
override file, so each cluster can use its own backend.

## Fleet dashboard

`:fleet` summarizes several clusters side by side so you don't have to switch
through them. It's **opt-in** - sofka queries only the contexts you list.

```toml
[fleet]
contexts = ["prod-eu", "prod-us", "staging"]
```

You can also build or edit the fleet from inside the TUI: in the `:ctx`
switcher, `space` toggles the highlighted context in or out of the fleet
(members show a `✓`). These marks are saved to `<state-dir>/fleet.toml`
(`~/.local/state/sofka/fleet.toml` by default; see `sofka info`) and overlay
the `[fleet] contexts` list on every start - sofka never rewrites your config
file, so the config stays the hand-edited base list and marks can both add to
it and mask entries out.

Contexts are gathered concurrently with a per-context timeout, so an unreachable
or slow cluster shows an error on its own row instead of blocking the others.
Each row shows connectivity, Kubernetes version, node readiness, the unhealthy
pod count, Flux `Ready=False` failures, the count of Argo CD Applications that
are `OutOfSync` or whose health is `Degraded`, `Missing` or `Unknown`, and the
resolved read-only policy. A cluster without the Flux or Argo CD CRDs shows `—`
for that column rather than a zero. `Progressing` Applications are not counted,
a rollout in flight is not a fault. `⏎`
switches to the highlighted context (through the normal context-switch path), `r`
gathers again. Only these non-sensitive summaries are kept, in memory.
