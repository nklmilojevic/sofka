//! Kubernetes connectivity: client bootstrap, resource discovery, alias
//! resolution, and async watch streams that feed the in-memory store.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use kube::api::{Api, ListParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::core::DynamicObject;
use kube::discovery::{ApiResource, Discovery, Scope};
use kube::runtime::{WatchStreamExt, utils::Backoff, watcher};
use kube::{Client, Config, ResourceExt};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use crate::store::{Msg, row_key};

/// A resolvable Kubernetes resource type.
#[derive(Clone)]
pub struct Kind {
    pub ar: ApiResource,
    pub namespaced: bool,
}

impl Kind {
    pub fn title(&self) -> String {
        if self.ar.group.is_empty() {
            self.ar.plural.clone()
        } else {
            format!("{}.{}", self.ar.plural, self.ar.group)
        }
    }

    /// Whether this kind comes from a custom API group (a CRD or third-party
    /// aggregated API) rather than one Kubernetes ships with. Custom kinds own
    /// their name in the command palette even when a built-in command shares
    /// it (`:snapshots` with a snapshots.* CRD installed); built-in Kubernetes
    /// API groups don't get that priority.
    pub fn is_custom(&self) -> bool {
        let group = self.ar.group.as_str();
        !group.is_empty()
            && !group.ends_with(".k8s.io")
            && !matches!(
                group,
                "apps" | "batch" | "policy" | "autoscaling" | "extensions"
            )
    }
}

/// Connection + discovery context for a cluster.
pub struct Cluster {
    pub client: Client,
    pub context: String,
    /// Kubeconfig cluster name referenced by `context` (empty when unknown,
    /// e.g. in-cluster). Keys per-cluster config overrides.
    pub cluster_name: String,
    pub cluster_url: String,
    /// Kubernetes API-server revision (`gitVersion` from `/version`). Empty
    /// when disconnected or when the optional version request fails.
    pub server_version: String,
    pub default_namespace: String,
    /// Context name to pass to `kubectl` shell-outs (`--context`). `None` when
    /// we connected without a named kubeconfig context (e.g. in-cluster), in
    /// which case kubectl falls back to its own default.
    cli_context: Option<String>,
    /// lookup key (alias/plural/kind, lowercased) -> Kind
    /// Every lookup key — bare plural, lowercased kind, group-qualified name,
    /// and each alias — pointing at one shared `Kind`. Storing a full copy per
    /// key meant four or more duplicates of every resource's group, version,
    /// api_version, kind and plural strings, which on a CRD-heavy cluster is
    /// the bulk of the registry.
    registry: HashMap<String, Arc<Kind>>,
    /// stable, de-duplicated list of plural names for completion
    pub catalog: Vec<String>,
    /// False for the placeholder built by [`Cluster::disconnected`] when the
    /// current context is unreachable at launch — the app then starts in the
    /// context picker instead of a resource view.
    pub connected: bool,
    /// Per-cluster support for Kubernetes streaming-list watch startup:
    /// unknown, supported, or unsupported. Shared by all view watches so one
    /// negotiation failure avoids retrying the extension on every switch.
    streaming_lists: Arc<AtomicU8>,
}

const STREAMING_UNKNOWN: u8 = 0;
const STREAMING_SUPPORTED: u8 = 1;
const STREAMING_UNSUPPORTED: u8 = 2;
#[cfg(not(test))]
const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const VERSION_TIMEOUT: Duration = Duration::from_millis(50);
const SERVER_VERSION_MAX_CHARS: usize = 128;

/// API metadata is remote input and reaches both the TUI and plain terminal
/// output. Drop terminal control characters once at ingestion, then bound the
/// value so every downstream sink can safely render the stored revision.
fn sanitize_server_version(version: &str) -> String {
    let visible: String = version.chars().filter(|c| !c.is_control()).collect();
    crate::text::ellipsize(&visible, SERVER_VERSION_MAX_CHARS)
}

impl Cluster {
    pub async fn connect() -> Result<Self> {
        let config = Config::infer()
            .await
            .context("loading kubeconfig (is KUBECONFIG / ~/.kube/config present?)")?;
        // One parse for both identity questions. `Config::infer` does not
        // surface the context name, so the file is read once more here — but
        // the current context and the cluster behind it come out of the same
        // snapshot instead of two further reads.
        let kubeconfig = Kubeconfig::read().ok();
        // The real kubeconfig current-context (if any) is what kubectl uses by
        // default; pass it explicitly so shell-outs can't drift from us.
        let cli_context = kubeconfig.as_ref().and_then(|k| k.current_context.clone());
        let context = cli_context.clone().unwrap_or_else(|| "default".into());
        let cluster_name = kubeconfig
            .as_ref()
            .and_then(|k| cluster_name_in(k, &context))
            .unwrap_or_default();
        Self::from_config(config, context, cli_context, cluster_name).await
    }

    /// Connect using a specific kubeconfig context (for the `:ctx` switcher).
    pub async fn connect_context(name: &str) -> Result<Self> {
        let kubeconfig = Kubeconfig::read().context("reading kubeconfig")?;
        let opts = KubeConfigOptions {
            context: Some(name.to_string()),
            cluster: None,
            user: None,
        };
        // Read off the snapshot before it is consumed below.
        let cluster_name = cluster_name_in(&kubeconfig, name).unwrap_or_default();
        let config = Config::from_custom_kubeconfig(kubeconfig, &opts)
            .await
            .with_context(|| format!("building config for context '{name}'"))?;
        Self::from_config(
            config,
            name.to_string(),
            Some(name.to_string()),
            cluster_name,
        )
        .await
    }

    /// `cluster_name` is resolved by the caller from the kubeconfig it has
    /// already parsed. Reading it here made every connect parse the file again
    /// for one field it had just been holding.
    async fn from_config(
        config: Config,
        context: String,
        cli_context: Option<String>,
        cluster_name: String,
    ) -> Result<Self> {
        let cluster_url = config.cluster_url.to_string();
        let default_namespace = config.default_namespace.clone();
        let client = Client::try_from(config).context("building kube client")?;
        let version_client = client.clone();

        let mut cluster = Self {
            client,
            context,
            cluster_name,
            cluster_url,
            server_version: String::new(),
            default_namespace,
            cli_context,
            registry: HashMap::new(),
            catalog: Vec::new(),
            connected: true,
            streaming_lists: Arc::new(AtomicU8::new(STREAMING_UNKNOWN)),
        };
        // Version is useful metadata, not a connectivity prerequisite. Fetch
        // it alongside discovery so it adds no serial startup latency, and
        // keep the cluster usable if an unusual API proxy rejects `/version`.
        let version = tokio::time::timeout(VERSION_TIMEOUT, version_client.apiserver_version());
        let (discovery, version) = tokio::join!(cluster.discover(), version);
        discovery?;
        if let Ok(Ok(info)) = version {
            cluster.server_version = sanitize_server_version(&info.git_version);
        }
        Ok(cluster)
    }

    /// A placeholder for launching when the current context's API server is
    /// unreachable (k9s drops you into the context picker in this situation
    /// instead of exiting). Identity fields come straight from the kubeconfig
    /// so the header still names the broken context; the client points at the
    /// configured server but nothing uses it until a real context connects.
    /// `requested` is the `--context` flag when the failed connect targeted a
    /// named context, so the header names what the user asked for instead of
    /// the kubeconfig current-context.
    pub fn disconnected(requested: Option<&str>) -> Self {
        let kubeconfig = Kubeconfig::read().ok();
        let context = requested
            .map(str::to_owned)
            .or_else(|| kubeconfig.as_ref().and_then(|k| k.current_context.clone()))
            .unwrap_or_default();
        let cluster_name = kubeconfig
            .as_ref()
            .and_then(|k| {
                k.contexts
                    .iter()
                    .find(|c| c.name == context)?
                    .context
                    .as_ref()
                    .map(|c| c.cluster.clone())
            })
            .unwrap_or_default();
        let cluster_url = kubeconfig
            .as_ref()
            .and_then(|k| {
                k.clusters
                    .iter()
                    .find(|c| c.name == cluster_name)?
                    .cluster
                    .as_ref()?
                    .server
                    .clone()
            })
            .unwrap_or_default();
        let url = cluster_url
            .parse()
            .unwrap_or_else(|_| "http://127.0.0.1:8080".parse().expect("static url"));
        let client = Client::try_from(Config::new(url)).expect("building offline client");
        Self {
            client,
            cli_context: (!context.is_empty()).then(|| context.clone()),
            context,
            cluster_name,
            cluster_url,
            server_version: String::new(),
            default_namespace: "default".into(),
            registry: HashMap::new(),
            catalog: Vec::new(),
            connected: false,
            streaming_lists: Arc::new(AtomicU8::new(STREAMING_UNKNOWN)),
        }
    }

    /// Context name to pass to `kubectl` (`--context`), when known. Keeps
    /// shell-outs (edit/describe/exec/attach/port-forward) on the same cluster
    /// sofka is connected to, even after an in-app `:ctx` switch.
    pub fn kubectl_context(&self) -> Option<&str> {
        self.cli_context.as_deref()
    }

    /// All context names from the kubeconfig.
    /// Context names from the kubeconfig. A read/parse failure is an error,
    /// not an empty list — "no contexts" and "your kubeconfig is invalid"
    /// must not look the same in the picker.
    pub fn list_contexts() -> Result<Vec<String>, String> {
        Kubeconfig::read()
            .map(|k| k.contexts.into_iter().map(|c| c.name).collect())
            .map_err(|e| format!("reading kubeconfig: {e}"))
    }

    /// Merge user-defined aliases (alias -> canonical) into the registry.
    pub fn add_aliases(&mut self, aliases: &HashMap<String, String>) {
        for (alias, target) in aliases {
            if let Some(k) = self.registry.get(&target.to_lowercase()).map(Arc::clone) {
                self.registry.insert(alias.to_lowercase(), k);
            }
        }
    }

    /// Walk the discovery API and index every recommended resource by its
    /// plural and kind. Built-in aliases are layered on top.
    async fn discover(&mut self) -> Result<()> {
        // Prefer the Aggregated Discovery API (K8s ≥1.26): two requests total,
        // and the apiserver serves cached data for groups whose backing
        // APIService is down. The per-group walk instead 503s on the first
        // broken aggregated API (e.g. a dead metrics-server), which would make
        // the whole cluster unconnectable. Servers without aggregated
        // discovery answer the request with the legacy document, which
        // deserializes as *empty* rather than failing — so fall back to the
        // per-group walk on empty as well as on error.
        let discovery = match Discovery::new(self.client.clone()).run_aggregated().await {
            Ok(d) if d.groups().next().is_some() => d,
            _ => Discovery::new(self.client.clone())
                .run()
                .await
                .context("running API discovery")?,
        };

        // Collect everything first, then insert bare keys in priority order so
        // that e.g. core `pods` wins over `pods.metrics.k8s.io`.
        let mut entries: Vec<(Arc<Kind>, String, String)> = Vec::new(); // (kind, plural, kind_lc)
        let mut catalog = Vec::new();
        for group in discovery.groups() {
            // All served versions of the group, most stable version per kind.
            // NOT `recommended_resources()`: that only returns resources at
            // the group's *preferred* version, silently dropping kinds served
            // solely at other versions — e.g. a CRD group whose preferred
            // version is v1 while half its kinds only exist at v1alpha1
            // (netbird.io does this; kube-rs docs call it the "ApiGroup
            // Common Pitfall").
            for (ar, caps) in group.resources_by_stability() {
                let namespaced = matches!(caps.scope, Scope::Namespaced);
                let kind = Kind {
                    ar: ar.clone(),
                    namespaced,
                };
                let plural = ar.plural.to_lowercase();
                let kind_lc = ar.kind.to_lowercase();
                catalog.push(plural.clone());
                // Group-qualified keys are unambiguous; insert directly. They
                // join the catalog too, so completion can surface a kind whose
                // bare plural is shadowed (or find it by its group name).
                let kind = Arc::new(kind);
                if !ar.group.is_empty() {
                    let qualified = format!("{}.{}", plural, ar.group);
                    self.registry.insert(qualified.clone(), Arc::clone(&kind));
                    catalog.push(qualified);
                }
                entries.push((kind, plural, kind_lc));
            }
        }
        // Lowest priority first; later inserts overwrite, so the highest
        // priority group ends up owning each bare plural/kind key.
        // Deliberately a *stable* sort: groups tie on priority constantly, and
        // insertion order is what decides which kind wins a shared key. An
        // unstable sort would make `:` resolution vary between runs.
        entries.sort_by_key(|(k, _, _)| group_priority(&k.ar.group));
        for (kind, plural, kind_lc) in entries {
            self.registry.insert(plural, Arc::clone(&kind));
            self.registry.insert(kind_lc, kind);
        }
        catalog.sort();
        catalog.dedup();
        self.catalog = catalog;

        // Built-in short aliases (k9s-style), resolved against the registry.
        for (alias, target) in ALIASES {
            if let Some(k) = self.registry.get(*target).map(Arc::clone) {
                self.registry.entry((*alias).to_string()).or_insert(k);
            }
        }
        Ok(())
    }

    pub fn resolve(&self, input: &str) -> Option<Kind> {
        self.resolve_ref(input).cloned()
    }

    /// [`Self::resolve`] without the copy, for callers that only read the kind.
    pub fn resolve_ref(&self, input: &str) -> Option<&Kind> {
        let key = input.trim().trim_start_matches(':').to_lowercase();
        self.registry.get(&key).map(Arc::as_ref)
    }

    /// Spawn a watch task for `kind` in `namespace` ("" = all namespaces),
    /// optionally scoped by a label and/or field selector (used for drill-down,
    /// e.g. deployment -> its pods, or node -> pods on that node).
    /// Messages are tagged with `gen` so the UI can drop stale streams.
    pub fn spawn_watch(
        &self,
        kind: &Kind,
        namespace: &str,
        labels: Option<String>,
        fields: Option<String>,
        generation: u64,
        tx: Sender<Msg>,
    ) -> JoinHandle<()> {
        let client = self.client.clone();
        let ar = kind.ar.clone();
        let namespaced = kind.namespaced;
        let ns = namespace.to_string();
        let streaming_lists = Arc::clone(&self.streaming_lists);

        tokio::spawn(async move {
            let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
                Api::namespaced_with(client, &ns, &ar)
            } else {
                Api::all_with(client, &ar)
            };

            let mut cfg = watcher::Config::default().any_semantic();
            if let Some(l) = labels {
                cfg = cfg.labels(&l);
            }
            if let Some(f) = fields {
                cfg = cfg.fields(&f);
            }
            let mut using_streaming =
                streaming_lists.load(Ordering::Acquire) != STREAMING_UNSUPPORTED;
            let mut initializing = true;
            // `watcher` re-lists as fast as the stream is polled, so an error
            // that does not clear itself hammers the API server — the node
            // counter measured ~9,800 requests a second against a failing test
            // server before it was paced. This is the same client-go strategy
            // `spawn_node_pods_poll` uses, reset only on genuine progress:
            // `.default_backoff()` resets on any `Ok`, and `watcher` replays
            // `Ok(Event::Init)` before every list attempt, so a failing initial
            // list would cycle at the minimum delay forever.
            let mut backoff = watcher::DefaultBackoff::default();
            let mut stream = watcher(
                api.clone(),
                if using_streaming {
                    cfg.clone().streaming_lists()
                } else {
                    cfg.clone()
                },
            )
            .modify(|obj| obj.managed_fields_mut().clear())
            .boxed();
            if tx.send(Msg::Reset { generation }).await.is_err() {
                return;
            }

            while let Some(event) = stream.next().await {
                if using_streaming
                    && initializing
                    && event.as_ref().is_err_and(streaming_lists_unsupported)
                {
                    streaming_lists.store(STREAMING_UNSUPPORTED, Ordering::Release);
                    using_streaming = false;
                    stream = watcher(api.clone(), cfg.clone())
                        .modify(|obj| obj.managed_fields_mut().clear())
                        .boxed();
                    continue;
                }
                // Progress, as opposed to another doomed list attempt: the
                // init replay happens again on every retry, so it says nothing
                // about whether the watch is working.
                if matches!(
                    event,
                    Ok(watcher::Event::Apply(_)
                        | watcher::Event::Delete(_)
                        | watcher::Event::InitDone)
                ) {
                    backoff.reset();
                }
                let mut failed = false;
                let msg = match event {
                    Ok(watcher::Event::Apply(obj)) | Ok(watcher::Event::InitApply(obj)) => {
                        Msg::Applied {
                            generation,
                            key: row_key(&obj),
                            obj: Box::new(obj),
                        }
                    }
                    Ok(watcher::Event::Delete(obj)) => Msg::Deleted {
                        generation,
                        key: row_key(&obj),
                    },
                    Ok(watcher::Event::Init) => Msg::Reset { generation },
                    Ok(watcher::Event::InitDone) => {
                        initializing = false;
                        if using_streaming {
                            // Unsupported is sticky if two startup watches
                            // negotiate concurrently and only one endpoint
                            // rejects the extension.
                            let _ = streaming_lists.compare_exchange(
                                STREAMING_UNKNOWN,
                                STREAMING_SUPPORTED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                        }
                        Msg::Synced { generation }
                    }
                    // The watcher heals a desync by re-listing on its own
                    // (the stream continues with Init/…/InitDone), so the
                    // "too old resource version: Expired" error is routine —
                    // the sync dot already shows the re-list. No error flash.
                    Err(e) if watch_error_is_benign(&e) => continue,
                    Err(e) => {
                        failed = true;
                        Msg::Error {
                            generation,
                            error: e.to_string(),
                        }
                    }
                };
                if tx.send(msg).await.is_err() {
                    break; // UI gone
                }
                if failed {
                    tokio::time::sleep(backoff.next().unwrap_or(WATCH_BACKOFF_CEILING)).await;
                }
            }
        })
    }

    /// List namespaces for the namespace switcher.
    pub async fn namespaces(&self) -> Result<Vec<String>> {
        if let Some(kind) = self.resolve("namespaces") {
            let api: Api<DynamicObject> = Api::all_with(self.client.clone(), &kind.ar);
            let list = api.list(&ListParams::default()).await?;
            let mut names: Vec<String> = list
                .items
                .into_iter()
                .filter_map(|o| o.metadata.name)
                .collect();
            names.sort();
            Ok(names)
        } else {
            Ok(vec![])
        }
    }
}

/// Fallback delay if the watcher backoff ever runs out of steps.
/// `DefaultBackoff` is unbounded in attempts, so this is belt and braces
/// rather than a real path.
const WATCH_BACKOFF_CEILING: Duration = Duration::from_secs(30);

/// A watch error the watcher recovers from by itself: the resourceVersion the
/// watch resumed from was already compacted away by etcd (HTTP 410 Gone,
/// reason `Expired` — "too old resource version"). Routine on quiet resources
/// with short compaction windows; the watcher re-lists and carries on.
pub fn watch_error_is_benign(e: &watcher::Error) -> bool {
    matches!(e, watcher::Error::WatchError(status)
        if status.code == 410 || status.reason == "Expired")
}

/// Errors that specifically mean the API server rejected streaming-list
/// watch parameters. Authentication, throttling, transport, and server errors
/// remain visible to the user instead of being disguised by a fallback.
fn streaming_lists_unsupported(e: &watcher::Error) -> bool {
    let unsupported_status =
        |status: &kube::core::Status| matches!(status.code, 400 | 404 | 405 | 422);
    match e {
        watcher::Error::WatchStartFailed(kube::Error::Api(status)) => unsupported_status(status),
        watcher::Error::WatchFailed(kube::Error::Api(status)) => unsupported_status(status),
        watcher::Error::WatchError(status) => unsupported_status(status),
        _ => false,
    }
}

/// Higher wins when two API groups expose the same bare plural/kind (e.g.
/// core `pods` should beat `pods.metrics.k8s.io`).
fn group_priority(group: &str) -> u8 {
    match group {
        "" => 100, // core/v1
        "apps" => 90,
        "batch" => 85,
        "networking.k8s.io" => 80,
        "rbac.authorization.k8s.io" | "storage.k8s.io" | "policy" => 75,
        "metrics.k8s.io" => 0, // virtual metrics API — never shadow real kinds
        _ => 50,
    }
}

/// The current kubeconfig context, its cluster name, and API-server URL, read
/// offline (no connection). For `--info`. `None` when there's no kubeconfig or
/// no current context. The server URL never carries credentials.
pub fn current_context_info() -> Option<(String, String, String)> {
    let kubeconfig = kube::config::Kubeconfig::read().ok()?;
    let context = kubeconfig.current_context.clone()?;
    let cluster_name = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)
        .and_then(|c| c.context.as_ref())
        .map(|c| c.cluster.clone())
        .unwrap_or_default();
    let server = kubeconfig
        .clusters
        .iter()
        .find(|c| c.name == cluster_name)
        .and_then(|c| c.cluster.as_ref())
        .and_then(|c| c.server.clone())
        .unwrap_or_default();
    Some((context, cluster_name, server))
}

/// Public wrapper over [`cluster_name_for`] for resolving per-context config
/// (fleet dashboard read-only policy) without a live connection.
pub fn cluster_name_for_context(context: &str) -> String {
    cluster_name_for(context).unwrap_or_default()
}

/// Kubeconfig cluster name a context points at, when the kubeconfig knows it.
fn cluster_name_for(context: &str) -> Option<String> {
    cluster_name_in(&kube::config::Kubeconfig::read().ok()?, context)
}

/// The same lookup against a kubeconfig the caller already parsed.
fn cluster_name_in(kubeconfig: &Kubeconfig, context: &str) -> Option<String> {
    kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)?
        .context
        .as_ref()
        .map(|c| c.cluster.clone())
}

/// Built-in short aliases -> canonical plural. Mirrors common k9s/kubectl ones.
pub const ALIASES: &[(&str, &str)] = &[
    ("po", "pods"),
    ("pod", "pods"),
    ("dp", "deployments"),
    ("deploy", "deployments"),
    ("svc", "services"),
    ("ns", "namespaces"),
    ("no", "nodes"),
    ("node", "nodes"),
    ("cm", "configmaps"),
    ("sec", "secrets"),
    ("secret", "secrets"),
    ("sts", "statefulsets"),
    ("ds", "daemonsets"),
    ("rs", "replicasets"),
    ("rc", "replicationcontrollers"),
    ("ing", "ingresses"),
    ("pv", "persistentvolumes"),
    ("pvc", "persistentvolumeclaims"),
    ("sa", "serviceaccounts"),
    ("jo", "jobs"),
    ("cj", "cronjobs"),
    ("ep", "endpoints"),
    ("ev", "events"),
    ("hpa", "horizontalpodautoscalers"),
    ("pc", "priorityclasses"),
    ("crd", "customresourcedefinitions"),
    ("cr", "clusterroles"),
    ("crb", "clusterrolebindings"),
    ("ro", "roles"),
    ("rb", "rolebindings"),
    ("np", "networkpolicies"),
    ("pdb", "poddisruptionbudgets"),
    ("sc", "storageclasses"),
    // Flux CD — the CRDs' own `shortNames`.
    ("ks", "kustomizations"),
    ("hr", "helmreleases"),
];

#[cfg(any(test, feature = "bench"))]
impl Cluster {
    /// A connectionless cluster for unit tests: the client points at a dummy
    /// URL (no I/O happens until a request is actually made) and the registry
    /// is a small hand-built set of common kinds.
    ///
    /// Also compiled under the `bench` feature, because `benches/` links the
    /// library without `cfg(test)` and needs the same offline fixture.
    pub fn fake() -> Self {
        let config = Config::new("https://127.0.0.1:6443".parse().unwrap());
        let client = Client::try_from(config).expect("build test client");
        let mut cluster = Self {
            client,
            context: "test".into(),
            cluster_name: "test-cluster".into(),
            cluster_url: "https://127.0.0.1:6443".into(),
            server_version: String::new(),
            default_namespace: "default".into(),
            cli_context: Some("test".into()),
            connected: true,
            registry: HashMap::new(),
            catalog: Vec::new(),
            streaming_lists: Arc::new(AtomicU8::new(STREAMING_UNKNOWN)),
        };
        cluster.register_kind("", "Pod", "pods", true);
        cluster.register_kind("apps", "Deployment", "deployments", true);
        cluster.register_kind("", "Service", "services", true);
        cluster.register_kind("", "Secret", "secrets", true);
        cluster.register_kind("", "Node", "nodes", false);
        cluster.register_kind("", "Namespace", "namespaces", false);
        cluster.register_kind("", "Event", "events", true);
        cluster.register_kind("batch", "Job", "jobs", true);
        cluster.register_kind("batch", "CronJob", "cronjobs", true);
        cluster.register_kind(
            "kustomize.toolkit.fluxcd.io",
            "Kustomization",
            "kustomizations",
            true,
        );
        // An alias/plural pair that collide on fuzzy matching (`hr` is a
        // subsequence of horizontalpodautoscalers), for suggestion-priority
        // tests.
        cluster.register_kind(
            "helm.toolkit.fluxcd.io",
            "HelmRelease",
            "helmreleases",
            true,
        );
        let hr = Arc::clone(&cluster.registry["helmreleases"]);
        cluster.registry.insert("hr".to_string(), hr);
        cluster.register_kind(
            "autoscaling",
            "HorizontalPodAutoscaler",
            "horizontalpodautoscalers",
            true,
        );
        cluster.register_kind(
            "external-secrets.io",
            "ExternalSecret",
            "externalsecrets",
            true,
        );
        // A CR without curated columns, for custom-view tests.
        cluster.register_kind("cert-manager.io", "Certificate", "certificates", true);
        // A CRD whose plural collides with the `:snapshots` built-in command,
        // for palette-priority tests (CRD names outrank built-ins).
        cluster.register_kind("kopiur.home-operations.com", "Snapshot", "snapshots", true);
        // ArgoCD CRDs, for the `t` suspend/resume/sync menu.
        cluster.register_kind("argoproj.io", "Application", "applications", true);
        cluster.register_kind("argoproj.io", "ApplicationSet", "applicationsets", true);
        // A second `events` kind, reachable only by its qualified name — the
        // bare plural stays with core, as `discover` would leave it.
        let events_k8s_io = Kind {
            ar: ApiResource {
                group: "events.k8s.io".to_string(),
                version: "v1".to_string(),
                api_version: "events.k8s.io/v1".to_string(),
                kind: "Event".to_string(),
                plural: "events".to_string(),
            },
            namespaced: true,
        };
        cluster
            .registry
            .insert("events.events.k8s.io".to_string(), Arc::new(events_k8s_io));
        cluster
    }

    /// Add one kind to a test cluster, indexed the way [`Cluster::discover`]
    /// indexes a discovered one: by bare plural, lowercased kind, and — for a
    /// grouped kind — `plural.group`, with the plural (and qualified name)
    /// joining the catalog. Lets a test that needs a specific CRD declare it
    /// itself, rather than parking every such kind in [`Cluster::fake`].
    ///
    /// Every kind is registered at version `v1`; the fixture has no need for
    /// anything else.
    pub fn register_kind(&mut self, group: &str, kind: &str, plural: &str, namespaced: bool) {
        let plural = plural.to_lowercase();
        let k = Kind {
            ar: ApiResource {
                group: group.to_string(),
                version: "v1".to_string(),
                api_version: if group.is_empty() {
                    "v1".to_string()
                } else {
                    format!("{group}/v1")
                },
                kind: kind.to_string(),
                plural: plural.clone(),
            },
            namespaced,
        };
        let k = Arc::new(k);
        self.registry.insert(kind.to_lowercase(), Arc::clone(&k));
        self.registry.insert(plural.clone(), Arc::clone(&k));
        self.catalog.push(plural.clone());
        if !group.is_empty() {
            let qualified = format!("{plural}.{group}");
            self.registry.insert(qualified.clone(), k);
            self.catalog.push(qualified);
        }
        self.catalog.sort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_watch_errors_are_benign() {
        let expired = watcher::Error::WatchError(Box::new(kube::core::Status {
            code: 410,
            reason: "Expired".into(),
            message: "too old resource version: 1 (2)".into(),
            ..Default::default()
        }));
        assert!(watch_error_is_benign(&expired));
        let forbidden = watcher::Error::WatchError(Box::new(kube::core::Status {
            code: 403,
            reason: "Forbidden".into(),
            ..Default::default()
        }));
        assert!(!watch_error_is_benign(&forbidden));
        assert!(!watch_error_is_benign(&watcher::Error::NoResourceVersion));
    }

    #[test]
    fn streaming_fallback_only_accepts_capability_errors() {
        let api_error = |code| {
            watcher::Error::WatchStartFailed(kube::Error::Api(Box::new(kube::core::Status {
                code,
                reason: "test".into(),
                ..Default::default()
            })))
        };
        assert!(streaming_lists_unsupported(&api_error(400)));
        assert!(streaming_lists_unsupported(&api_error(422)));
        assert!(!streaming_lists_unsupported(&api_error(401)));
        assert!(!streaming_lists_unsupported(&api_error(403)));
        assert!(!streaming_lists_unsupported(&api_error(429)));
        assert!(!streaming_lists_unsupported(&api_error(500)));
        assert!(!streaming_lists_unsupported(
            &watcher::Error::NoResourceVersion
        ));
    }

    async fn mock_watch_server(
        responses: Vec<(&'static str, String)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::collections::VecDeque;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock watch server");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut responses: VecDeque<_> = responses.into();
            while let Some((status, body)) = responses.pop_front() {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = sock.split();
                let mut reader = BufReader::new(r);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                seen.lock()
                    .unwrap()
                    .push(request_line.split_whitespace().nth(1).unwrap_or("").into());
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                w.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{addr}"), requests)
    }

    fn watch_cluster(url: &str) -> Cluster {
        let mut config = Config::new(url.parse().expect("mock URL"));
        config.default_retry = false;
        let mut cluster = Cluster::fake();
        cluster.client = Client::try_from(config).expect("mock client");
        cluster.cluster_url = url.into();
        cluster
    }

    async fn receive_watch_result(mut rx: tokio::sync::mpsc::Receiver<Msg>) -> (usize, bool) {
        let mut applied = 0;
        for _ in 0..8 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("watch message timeout")
                .expect("watch channel closed");
            match msg {
                Msg::Applied { .. } => applied += 1,
                Msg::Synced { .. } => return (applied, true),
                Msg::Error { .. } => return (applied, false),
                _ => {}
            }
        }
        (applied, false)
    }

    #[tokio::test]
    async fn streaming_watch_initializes_from_initial_events() {
        let body = concat!(
            "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"a\",\"namespace\":\"default\",\"resourceVersion\":\"10\"}}}\n",
            "{\"type\":\"BOOKMARK\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"resourceVersion\":\"10\",\"annotations\":{\"k8s.io/initial-events-end\":\"true\"}}}}\n"
        )
        .to_string();
        let (url, requests) = mock_watch_server(vec![("200 OK", body)]).await;
        let cluster = watch_cluster(&url);
        let kind = cluster.resolve("pods").unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let task = cluster.spawn_watch(&kind, "default", None, None, 1, tx);
        assert_eq!(receive_watch_result(rx).await, (1, true));
        task.abort();
        assert_eq!(
            cluster.streaming_lists.load(Ordering::Acquire),
            STREAMING_SUPPORTED
        );
        assert!(requests.lock().unwrap()[0].contains("sendInitialEvents=true"));
    }

    #[tokio::test]
    async fn unsupported_streaming_watch_falls_back_to_list_watch() {
        let rejected = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"sendInitialEvents is not supported","reason":"BadRequest","code":400}"#.to_string();
        let listed = r#"{"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"10"},"items":[{"apiVersion":"v1","kind":"Pod","metadata":{"name":"a","namespace":"default","resourceVersion":"10"}}]}"#.to_string();
        let (url, requests) =
            mock_watch_server(vec![("400 Bad Request", rejected), ("200 OK", listed)]).await;
        let cluster = watch_cluster(&url);
        let kind = cluster.resolve("pods").unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let task = cluster.spawn_watch(&kind, "default", None, None, 1, tx);
        assert_eq!(receive_watch_result(rx).await, (1, true));
        task.abort();
        assert_eq!(
            cluster.streaming_lists.load(Ordering::Acquire),
            STREAMING_UNSUPPORTED
        );
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("sendInitialEvents=true"));
        assert!(!requests[1].contains("sendInitialEvents=true"));
        assert!(!requests[1].contains("watch=true"));
    }

    #[tokio::test]
    async fn authorization_error_does_not_trigger_streaming_fallback() {
        let forbidden = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"forbidden","reason":"Forbidden","code":403}"#.to_string();
        let (url, requests) = mock_watch_server(vec![("403 Forbidden", forbidden)]).await;
        let cluster = watch_cluster(&url);
        let kind = cluster.resolve("pods").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let task = cluster.spawn_watch(&kind, "default", None, None, 1, tx);
        let mut saw_error = false;
        for _ in 0..4 {
            if let Msg::Error { .. } =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                    .await
                    .expect("watch message timeout")
                    .expect("watch channel closed")
            {
                saw_error = true;
                break;
            }
        }
        task.abort();
        assert!(saw_error);
        assert_eq!(
            cluster.streaming_lists.load(Ordering::Acquire),
            STREAMING_UNKNOWN
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn core_group_outranks_metrics() {
        // The fix for `pods` resolving to pods.metrics.k8s.io.
        assert!(group_priority("") > group_priority("metrics.k8s.io"));
        assert!(group_priority("apps") > group_priority("metrics.k8s.io"));
        assert!(group_priority("") > group_priority("apps"));
    }

    #[test]
    fn aliases_point_at_plurals() {
        // Every alias target should be non-empty and distinct from its short form.
        for (alias, target) in ALIASES {
            assert!(!target.is_empty());
            assert_ne!(alias, target);
        }
    }

    /// A minimal mock apiserver for discovery tests: `apps` (healthy, serves
    /// deployments), `broken.example.com` (its APIService backend is down —
    /// the per-group walk gets a 503, aggregated discovery gets a stale
    /// entry), and the core group (pods). When `supports_aggregated` is
    /// false it behaves like a pre-1.26 server and answers the aggregated
    /// request with the legacy document.
    async fn mock_apiserver(
        supports_aggregated: bool,
        include_broken: bool,
        serve_version: bool,
    ) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock apiserver");
        let addr = listener.local_addr().expect("local addr");

        fn route(path: &str, aggregated: bool, include_broken: bool) -> (&'static str, String) {
            let broken_legacy = r#",{"name":"broken.example.com","versions":[{"groupVersion":"broken.example.com/v1beta1","version":"v1beta1"}],"preferredVersion":{"groupVersion":"broken.example.com/v1beta1","version":"v1beta1"}}"#;
            let broken_v2 = r#",{"metadata":{"name":"broken.example.com"},"versions":[{"version":"v1beta1","resources":[],"freshness":"Stale"}]}"#;
            // A mixed-version group modeled on the netbird.io operator: the
            // preferred version (v1) serves `widgets`, while `gadgets` is
            // only served at v1alpha1 — a preferred-version-only walk never
            // sees gadgets.
            let mixed_v2 = r#",{"metadata":{"name":"mixed.example.com"},"versions":[{"version":"v1","resources":[{"resource":"widgets","responseKind":{"group":"mixed.example.com","version":"v1","kind":"Widget"},"scope":"Namespaced","singularResource":"widget","verbs":["get","list","watch"]}],"freshness":"Current"},{"version":"v1alpha1","resources":[{"resource":"gadgets","responseKind":{"group":"mixed.example.com","version":"v1alpha1","kind":"Gadget"},"scope":"Namespaced","singularResource":"gadget","verbs":["get","list","watch"]}],"freshness":"Current"}]}"#;
            let mixed_legacy = r#",{"name":"mixed.example.com","versions":[{"groupVersion":"mixed.example.com/v1","version":"v1"},{"groupVersion":"mixed.example.com/v1alpha1","version":"v1alpha1"}],"preferredVersion":{"groupVersion":"mixed.example.com/v1","version":"v1"}}"#;
            match (path, aggregated) {
                ("/version", _) => (
                    "200 OK",
                    r#"{"major":"1","minor":"36","gitVersion":"v1.36.2-eks-bca9cf6","gitCommit":"abc123","gitTreeState":"clean","buildDate":"2026-08-20T00:00:00Z","goVersion":"go1.25.0","compiler":"gc","platform":"linux/amd64"}"#.into(),
                ),
                ("/apis", true) => (
                    "200 OK",
                    format!(
                        r#"{{"kind":"APIGroupDiscoveryList","apiVersion":"apidiscovery.k8s.io/v2","metadata":{{}},"items":[{{"metadata":{{"name":"apps"}},"versions":[{{"version":"v1","resources":[{{"resource":"deployments","responseKind":{{"group":"apps","version":"v1","kind":"Deployment"}},"scope":"Namespaced","singularResource":"deployment","verbs":["get","list","watch"]}}],"freshness":"Current"}}]}}{mixed_v2}{}]}}"#,
                        if include_broken { broken_v2 } else { "" }
                    ),
                ),
                ("/api", true) => (
                    "200 OK",
                    r#"{"kind":"APIGroupDiscoveryList","apiVersion":"apidiscovery.k8s.io/v2","metadata":{},"items":[{"metadata":{"name":""},"versions":[{"version":"v1","resources":[{"resource":"pods","responseKind":{"group":"","version":"v1","kind":"Pod"},"scope":"Namespaced","singularResource":"pod","verbs":["get","list","watch"]}],"freshness":"Current"}]}]}"#.into(),
                ),
                ("/apis", false) => (
                    "200 OK",
                    format!(
                        r#"{{"kind":"APIGroupList","apiVersion":"v1","groups":[{{"name":"apps","versions":[{{"groupVersion":"apps/v1","version":"v1"}}],"preferredVersion":{{"groupVersion":"apps/v1","version":"v1"}}}}{mixed_legacy}{}]}}"#,
                        if include_broken { broken_legacy } else { "" }
                    ),
                ),
                ("/api", false) => (
                    "200 OK",
                    r#"{"kind":"APIVersions","versions":["v1"],"serverAddressByClientCIDRs":[]}"#.into(),
                ),
                ("/apis/apps/v1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"apps/v1","resources":[{"name":"deployments","singularName":"deployment","namespaced":true,"kind":"Deployment","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/api/v1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"v1","resources":[{"name":"pods","singularName":"pod","namespaced":true,"kind":"Pod","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/mixed.example.com/v1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"mixed.example.com/v1","resources":[{"name":"widgets","singularName":"widget","namespaced":true,"kind":"Widget","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/mixed.example.com/v1alpha1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"mixed.example.com/v1alpha1","resources":[{"name":"gadgets","singularName":"gadget","namespaced":true,"kind":"Gadget","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/broken.example.com/v1beta1", _) => (
                    "503 Service Unavailable",
                    r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"service unavailable","reason":"ServiceUnavailable","code":503}"#.into(),
                ),
                _ => ("404 Not Found", r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#.into()),
            }
        }

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (r, mut w) = sock.split();
                    let mut reader = BufReader::new(r);
                    // Serve sequential keep-alive requests on the connection.
                    loop {
                        let mut request_line = String::new();
                        if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("")
                            .to_string();
                        let mut wants_aggregated = false;
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if header == "\r\n" {
                                break;
                            }
                            let header = header.to_ascii_lowercase();
                            if header.starts_with("accept:")
                                && header.contains("apidiscovery.k8s.io")
                            {
                                wants_aggregated = true;
                            }
                        }
                        // Leave the version request unanswered to prove the
                        // optional lookup has its own deadline and cannot
                        // hold an otherwise healthy connection open.
                        if path == "/version" && !serve_version {
                            continue;
                        }
                        let (status, body) = route(
                            &path,
                            wants_aggregated && supports_aggregated,
                            include_broken,
                        );
                        let response = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if w.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    async fn connect_mock(url: String) -> Result<Cluster> {
        let mut config = Config::new(url.parse().expect("mock url"));
        // The client's default retry policy (15 attempts, exponential
        // backoff) turns the mock's deliberate 503 into a ~4-minute stall;
        // retrying is not what these tests exercise.
        config.default_retry = false;
        Cluster::from_config(config, "test".into(), None, "test-cluster".into()).await
    }

    #[tokio::test]
    async fn discovery_tolerates_broken_apiservice() {
        // A dead aggregated API backend (e.g. metrics-server) must not make
        // the whole cluster unconnectable: aggregated discovery serves the
        // broken group as stale instead of 503ing.
        let url = mock_apiserver(true, true, true).await;
        let cluster = connect_mock(url)
            .await
            .expect("connect with broken APIService");
        assert!(cluster.resolve("deployments").is_some());
        assert!(cluster.resolve("pods").is_some());
    }

    #[test]
    fn server_version_is_terminal_safe_and_bounded() {
        assert_eq!(
            sanitize_server_version("v1.36.2\u{1b}[31m\nforged\u{7}"),
            "v1.36.2[31mforged"
        );
        let long = "v".repeat(SERVER_VERSION_MAX_CHARS + 20);
        let clean = sanitize_server_version(&long);
        assert_eq!(clean.chars().count(), SERVER_VERSION_MAX_CHARS);
        assert!(clean.ends_with('…'));
    }

    #[tokio::test]
    async fn connection_captures_apiserver_version() {
        let url = mock_apiserver(true, false, true).await;
        let cluster = connect_mock(url)
            .await
            .expect("connect to versioned server");
        assert_eq!(cluster.server_version, "v1.36.2-eks-bca9cf6");
    }

    #[tokio::test]
    async fn version_timeout_does_not_block_an_otherwise_healthy_connection() {
        let url = mock_apiserver(true, false, false).await;
        let cluster = tokio::time::timeout(Duration::from_secs(1), connect_mock(url))
            .await
            .expect("optional version lookup must be bounded")
            .expect("discovery still succeeds");
        assert!(cluster.server_version.is_empty());
        assert!(cluster.resolve("pods").is_some());
    }

    #[tokio::test]
    async fn discovery_falls_back_without_aggregated_support() {
        // Pre-1.26 servers answer the aggregated request with the legacy
        // document, which deserializes as an *empty* group list (not an
        // error) — discovery must detect that and take the per-group walk.
        let url = mock_apiserver(false, false, true).await;
        let cluster = connect_mock(url)
            .await
            .expect("connect via legacy discovery walk");
        assert!(cluster.resolve("deployments").is_some());
        assert!(cluster.resolve("pods").is_some());
    }

    /// Asserts every kind of the mixed-version group resolved: `widgets` at
    /// the preferred v1, `gadgets` only served at v1alpha1. A discovery walk
    /// limited to each group's preferred version loses gadgets entirely
    /// (the netbird.io bug: `:sidecarprofiles.netbird.io` -> no match).
    fn assert_mixed_group(cluster: &Cluster) {
        let widgets = cluster.resolve("widgets").expect("widgets resolves");
        assert_eq!(widgets.ar.version, "v1");
        let gadgets = cluster.resolve("gadgets").expect("gadgets resolves");
        assert_eq!(gadgets.ar.version, "v1alpha1");
        assert!(cluster.resolve("gadgets.mixed.example.com").is_some());
        assert!(
            cluster
                .catalog
                .contains(&"gadgets.mixed.example.com".into())
        );
    }

    #[tokio::test]
    async fn aggregated_discovery_includes_non_preferred_versions() {
        let url = mock_apiserver(true, false, true).await;
        let cluster = connect_mock(url).await.expect("connect aggregated");
        assert_mixed_group(&cluster);
    }

    #[tokio::test]
    async fn legacy_discovery_includes_non_preferred_versions() {
        let url = mock_apiserver(false, false, true).await;
        let cluster = connect_mock(url).await.expect("connect legacy");
        assert_mixed_group(&cluster);
    }

    #[tokio::test]
    async fn legacy_walk_still_fails_on_broken_apiservice() {
        // Documents the failure mode the aggregated path exists to avoid:
        // the per-group walk hits the broken group's 503 and discovery fails
        // (after ~4 minutes of client-side 503 retries with the default
        // config). If kube-rs ever makes run() tolerant, this starts failing
        // and the aggregated workaround can be simplified.
        let url = mock_apiserver(false, true, true).await;
        assert!(connect_mock(url).await.is_err());
    }
}
