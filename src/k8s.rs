//! Kubernetes connectivity: client bootstrap, resource discovery, alias
//! resolution, and async watch streams that feed the in-memory store.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use kube::api::{Api, ListParams};
use kube::config::KubeConfigOptions;
use kube::core::{DynamicObject, GroupVersionResource};
use kube::discovery::ApiResource;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Config, ResourceExt};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use crate::diagnostics::Op;
use crate::store::{Msg, row_key};

mod discovery;

pub(crate) fn build_client(config: Config, allow_v1_client_cert: bool) -> Result<Client> {
    let builder = crate::legacy_tls::client_builder(config, allow_v1_client_cert)?;
    let layer =
        tower::util::MapRequestLayer::new(|mut request: http::Request<kube::client::Body>| {
            let watch = request.uri().query().is_some_and(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .any(|(key, value)| key == "watch" && value == "true")
            });
            if watch {
                // Watch responses can contain multiple gzip members, which the client cannot decode.
                request.headers_mut().insert(
                    http::header::ACCEPT_ENCODING,
                    http::HeaderValue::from_static("identity"),
                );
            }
            request
        });
    Ok(builder.with_layer(&layer).with_layer(&MeterLayer).build())
}

/// Times every Kubernetes API request into [`crate::diagnostics`] and, at
/// `debug`, logs one line per request.
///
/// Latency is measured to response *headers*, not to the end of the body: a
/// watch's body stays open for the life of the view, and a list's decode time
/// belongs to us rather than to the API server.
#[derive(Clone, Copy)]
struct MeterLayer;

impl<S> tower::Layer<S> for MeterLayer {
    type Service = Meter<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Meter { inner }
    }
}

#[derive(Clone)]
struct Meter<S> {
    inner: S,
}

impl<S, B> tower::Service<http::Request<kube::client::Body>> for Meter<S>
where
    S: tower::Service<http::Request<kube::client::Body>, Response = http::Response<B>>,
    S::Error: std::fmt::Display,
    // kube's default stack is a `BoxService`, whose future is already pinned on
    // the heap. Requiring `Unpin` inherits that and lets the wrapper project
    // safely, so metering adds no allocation of its own.
    S::Future: Unpin,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Metered<S::Future>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<kube::client::Body>) -> Self::Future {
        let uri = request.uri();
        let path = uri.path();
        let op = Op::classify(request.method(), path, uri.query());
        crate::log_debug!(
            "request.start",
            op = op.as_str(),
            method = request.method(),
            path = path
        );
        Metered::new(self.inner.call(request), op)
    }
}

/// The in-flight half of [`Meter`]: records the elapsed time once, either when
/// the response arrives or when its caller cancels the request.
struct Metered<F> {
    inner: F,
    op: Op,
    start: Instant,
    completed: bool,
}

impl<F> Metered<F> {
    fn new(inner: F, op: Op) -> Self {
        Self {
            inner,
            op,
            start: Instant::now(),
            completed: false,
        }
    }
}

impl<F, B, E> Future for Metered<F>
where
    F: Future<Output = Result<http::Response<B>, E>> + Unpin,
    E: std::fmt::Display,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // Safe projection: every field is `Unpin`, so `Self` is too.
        let this = self.get_mut();
        let result = match Pin::new(&mut this.inner).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        this.completed = true;
        let elapsed = this.start.elapsed();
        let ok = result
            .as_ref()
            .is_ok_and(|response| !response.status().is_server_error());
        crate::diagnostics::record(this.op, elapsed, ok);
        match &result {
            Ok(response) => crate::log_debug!(
                "request.done",
                op = this.op.as_str(),
                status = response.status().as_u16(),
                ms = elapsed.as_millis()
            ),
            Err(e) => crate::log_warn!(
                "request.failed",
                op = this.op.as_str(),
                ms = elapsed.as_millis(),
                error = e
            ),
        }
        Poll::Ready(result)
    }
}

impl<F> Drop for Metered<F> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        let elapsed = self.start.elapsed();
        crate::diagnostics::record(self.op, elapsed, false);
        crate::log_warn!(
            "request.cancelled",
            op = self.op.as_str(),
            ms = elapsed.as_millis()
        );
    }
}

/// Outcome of [`Cluster::probe_watch`].
pub enum WatchProbe {
    /// The resource is not in this cluster's discovery.
    Unresolved,
    Ran {
        /// Time until the initial sync and watch headers arrive; `None` on failure.
        synced: Option<Duration>,
        /// Objects in the initial list.
        objects: usize,
        errors: usize,
        last_error: Option<String>,
    },
}

/// A resolvable Kubernetes resource type.
#[derive(Clone)]
pub struct Kind {
    pub ar: ApiResource,
    pub namespaced: bool,
}

impl Kind {
    pub fn resource_key(&self) -> GroupVersionResource {
        GroupVersionResource::gvr(&self.ar.group, &self.ar.version, &self.ar.plural)
    }

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
    /// Kubeconfig this context was read from. Pins `kubectl --kubeconfig` for
    /// shell-outs and keys per-cluster state, so a context named `prod` in an
    /// added file never collides with a `prod` in the default kubeconfig.
    pub source: crate::kubeconfigs::Source,
    /// Context name to pass to `kubectl` shell-outs (`--context`). `None` when
    /// we connected without a named kubeconfig context (e.g. in-cluster), in
    /// which case kubectl falls back to its own default.
    cli_context: Option<String>,
    /// lookup key (alias/plural/kind, lowercased) -> Kind
    registry: HashMap<String, Kind>,
    /// stable, de-duplicated list of plural names for completion
    pub catalog: Vec<String>,
    /// False for the placeholder built by [`Cluster::disconnected`] when the
    /// current context is unreachable at launch — the app then starts in the
    /// context picker instead of a resource view.
    pub connected: bool,
    /// Explicit consent for v1 client certificates, retained for this run.
    pub allow_v1_client_cert: bool,
    /// Per-cluster support for Kubernetes streaming-list watch startup:
    /// unknown, supported, or unsupported. Shared by all view watches so one
    /// negotiation failure avoids retrying the extension on every switch.
    streaming_lists: Arc<AtomicU8>,
    pub child_kinds: Vec<Kind>,
    pub discovery_warnings: Vec<String>,
    pub discovery_fallback: Option<String>,
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
    pub async fn connect(allow_v1_client_cert: bool) -> Result<Self> {
        let config = Config::infer()
            .await
            .context("loading kubeconfig (is KUBECONFIG / ~/.kube/config present?)")?;
        // The real kubeconfig current-context (if any) is what kubectl uses by
        // default; pass it explicitly so shell-outs can't drift from us.
        let cli_context = current_context_name();
        let context = cli_context.clone().unwrap_or_else(|| "default".into());
        Self::from_config(
            config,
            crate::kubeconfigs::ClusterId::new(context),
            cli_context,
            allow_v1_client_cert,
        )
        .await
    }

    /// Connect using a specific kubeconfig context (for the `:ctx` switcher).
    /// A context from an added file is built from that file alone, so a
    /// same-named context in the default kubeconfig cannot shadow it.
    pub async fn connect_context(
        id: &crate::kubeconfigs::ClusterId,
        allow_v1_client_cert: bool,
    ) -> Result<Self> {
        let kubeconfig = id.source.read().map_err(anyhow::Error::msg)?;
        let opts = KubeConfigOptions {
            context: Some(id.context.clone()),
            cluster: None,
            user: None,
        };
        let config = Config::from_custom_kubeconfig(kubeconfig, &opts)
            .await
            .with_context(|| format!("building config for context '{}'", id.context))?;
        Self::from_config(
            config,
            id.clone(),
            Some(id.context.clone()),
            allow_v1_client_cert,
        )
        .await
    }

    async fn from_config(
        config: Config,
        id: crate::kubeconfigs::ClusterId,
        cli_context: Option<String>,
        allow_v1_client_cert: bool,
    ) -> Result<Self> {
        let cluster_url = config.cluster_url.to_string();
        let default_namespace = config.default_namespace.clone();
        let client = build_client(config, allow_v1_client_cert).context("building kube client")?;
        let version_client = client.clone();

        let cluster_name = cluster_name_for(&id).unwrap_or_default();
        let mut cluster = Self {
            client,
            context: id.context,
            cluster_name,
            cluster_url,
            server_version: String::new(),
            default_namespace,
            source: id.source,
            cli_context,
            registry: HashMap::new(),
            catalog: Vec::new(),
            connected: true,
            allow_v1_client_cert,
            streaming_lists: Arc::new(AtomicU8::new(STREAMING_UNKNOWN)),
            child_kinds: Vec::new(),
            discovery_warnings: Vec::new(),
            discovery_fallback: None,
        };
        // Version is useful metadata, not a connectivity prerequisite. Fetch
        // it alongside discovery so it adds no serial startup latency, and
        // keep the cluster usable if an unusual API proxy rejects `/version`.
        let version = tokio::time::timeout(VERSION_TIMEOUT, version_client.apiserver_version());
        let (discovery, version) = tokio::join!(cluster.discover(), version);
        if let Err(e) = &discovery {
            crate::log_error!(
                "cluster.connect.failed",
                context = cluster.context,
                error = format!("{e:#}")
            );
        }
        discovery?;
        if let Ok(Ok(info)) = version {
            cluster.server_version = sanitize_server_version(&info.git_version);
        }
        crate::log_info!(
            "cluster.connected",
            context = cluster.context,
            cluster = cluster.cluster_name,
            server = cluster.cluster_url,
            k8s = cluster.server_version,
            kinds = cluster.catalog.len()
        );
        Ok(cluster)
    }

    /// A placeholder for launching when the requested context's API server is
    /// unreachable (k9s drops you into the context picker in this situation
    /// instead of exiting). Identity fields come straight from the kubeconfig
    /// so the header still names the broken context; the client points at the
    /// configured server but nothing uses it until a real context connects.
    /// `requested` is the `--context` flag when the failed connect targeted a
    /// named context, so the header names what the user asked for instead of
    /// the kubeconfig current-context.
    pub fn disconnected(requested: Option<&crate::kubeconfigs::ClusterId>) -> Self {
        let source = requested.map(|id| id.source.clone()).unwrap_or_default();
        let kubeconfig = source.read().ok();
        let context = requested
            .map(|id| id.context.clone())
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
            source,
            registry: HashMap::new(),
            catalog: Vec::new(),
            connected: false,
            allow_v1_client_cert: false,
            streaming_lists: Arc::new(AtomicU8::new(STREAMING_UNKNOWN)),
            child_kinds: Vec::new(),
            discovery_warnings: Vec::new(),
            discovery_fallback: None,
        }
    }

    /// Which context and kubeconfig this connection belongs to.
    pub fn id(&self) -> crate::kubeconfigs::ClusterId {
        crate::kubeconfigs::ClusterId {
            source: self.source.clone(),
            context: self.context.clone(),
        }
    }

    /// Context name to pass to `kubectl` (`--context`), when known. Keeps
    /// shell-outs (edit/describe/exec/attach/port-forward) on the same cluster
    /// sofka is connected to, even after an in-app `:ctx` switch.
    pub fn kubectl_context(&self) -> Option<&str> {
        self.cli_context.as_deref()
    }

    /// File to pass to `kubectl --kubeconfig`, when the active context came
    /// from one sofka added rather than from kubectl's own resolution. Without
    /// it a shell-out would look up `--context` in the wrong kubeconfig.
    pub fn kubectl_kubeconfig(&self) -> Option<&std::path::Path> {
        self.source.path()
    }

    /// Merge user-defined aliases (alias -> canonical) into the registry.
    pub fn add_aliases(&mut self, aliases: &HashMap<String, String>) {
        for (alias, target) in aliases {
            if let Some(k) = self.registry.get(&target.to_lowercase()).cloned() {
                self.registry.insert(alias.to_lowercase(), k);
            }
        }
    }

    /// Index resource names and short names from API discovery.
    async fn discover(&mut self) -> Result<()> {
        // Aggregated discovery needs two requests and tolerates stale APIService
        // entries. Legacy discovery is used when negotiation fails.
        let discovered = discovery::discover(&self.client).await?;
        self.child_kinds = discovered.child_kinds;
        self.discovery_warnings = discovered.skipped;
        self.discovery_fallback = discovered.fallback;
        self.register_resources(discovered.resources);
        Ok(())
    }

    fn register_resources(&mut self, mut resources: Vec<discovery::Resource>) {
        // Higher priority groups claim short names first. Alphabetical group
        // order makes collisions between equal-priority groups deterministic.
        resources.sort_by(|a, b| {
            group_priority(&b.kind.ar.group)
                .cmp(&group_priority(&a.kind.ar.group))
                .then_with(|| a.kind.ar.group.cmp(&b.kind.ar.group))
                .then_with(|| a.kind.ar.plural.cmp(&b.kind.ar.plural))
        });
        let mut entries = Vec::new();
        let mut catalog = Vec::new();
        for resource in &resources {
            let kind = &resource.kind;
            let ar = &kind.ar;
            let plural = ar.plural.to_lowercase();
            let kind_lc = ar.kind.to_lowercase();
            catalog.push(plural.clone());
            if !ar.group.is_empty() {
                let qualified = format!("{}.{}", plural, ar.group);
                self.registry.insert(qualified.clone(), kind.clone());
                catalog.push(qualified);
            }
            entries.push((kind.clone(), plural, kind_lc));
        }
        // Lowest priority first; later inserts overwrite, so the highest
        // priority group ends up owning each bare plural/kind key.
        // Deliberately a *stable* sort: groups tie on priority constantly, and
        // insertion order is what decides which kind wins a shared key. An
        // unstable sort would make `:` resolution vary between runs.
        entries.sort_by_key(|(k, _, _)| group_priority(&k.ar.group));
        for (kind, plural, kind_lc) in entries {
            self.registry.insert(plural, kind.clone());
            self.registry.insert(kind_lc, kind);
        }
        catalog.sort();
        catalog.dedup();
        self.catalog = catalog;

        // Built-in short aliases (k9s-style), resolved against the registry.
        for (alias, target) in ALIASES {
            if let Some(k) = self.registry.get(*target).cloned() {
                self.registry.entry((*alias).to_string()).or_insert(k);
            }
        }
        for resource in resources {
            for alias in resource.short_names {
                if !alias.is_empty() {
                    self.registry
                        .entry(alias.to_lowercase())
                        .or_insert_with(|| resource.kind.clone());
                }
            }
        }
    }

    pub fn resolve(&self, input: &str) -> Option<Kind> {
        let key = input.trim().trim_start_matches(':').to_lowercase();
        self.registry.get(&key).cloned()
    }

    /// Resolve a kind within an API group, as an `ownerReference` names it
    /// (`kind` + the group of its `apiVersion`), so a kind name shared by
    /// several groups lands on the right one. "" is the core group.
    pub fn resolve_in_group(&self, kind: &str, group: &str) -> Option<Kind> {
        let mut found: Vec<&Kind> = self
            .registry
            .values()
            .filter(|k| {
                k.ar.kind.eq_ignore_ascii_case(kind) && k.ar.group.eq_ignore_ascii_case(group)
            })
            .collect();
        found.sort_by(|a, b| a.ar.api_version.cmp(&b.ar.api_version));
        found.first().cloned().cloned()
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
        let api = watch_api(self.client.clone(), kind, namespace);
        let mut cfg = watcher::Config::default().any_semantic();
        if let Some(l) = labels {
            cfg = cfg.labels(&l);
        }
        if let Some(f) = fields {
            cfg = cfg.fields(&f);
        }
        spawn_watch_task(
            api,
            kind.ar.plural.clone(),
            namespace.to_string(),
            cfg,
            Arc::clone(&self.streaming_lists),
            generation,
            tx,
        )
    }

    /// Open the watch a launch would open, wait for its initial sync, and
    /// report what happened. For `sofka info`, which has no session to count
    /// watches over and so runs one instead.
    ///
    /// Discovery working says nothing about whether watches do: a proxy or
    /// load balancer that closes long-lived connections passes every other
    /// check and still leaves the TUI with an empty, never-syncing table.
    pub async fn probe_watch(
        &self,
        resource: &str,
        namespace: &str,
        timeout: Duration,
    ) -> WatchProbe {
        let Some(kind) = self.resolve(resource) else {
            return WatchProbe::Unresolved;
        };
        let ns = if kind.namespaced { namespace } else { "" };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let started = std::time::Instant::now();
        let (headers_tx, mut headers_rx) = tokio::sync::watch::channel(false);
        let client = self.client.clone();
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let client = client.clone();
            let headers_tx = headers_tx.clone();
            async move {
                // A list clears evidence from an earlier streaming-list attempt.
                headers_tx.send_replace(false);
                let watch = request.uri().query().is_some_and(|q| {
                    form_urlencoded::parse(q.as_bytes())
                        .any(|(key, value)| key == "watch" && value == "true")
                });
                let response = client.send(request).await?;
                if watch && response.status().is_success() {
                    headers_tx.send_replace(true);
                }
                Ok::<_, kube::Error>(response)
            }
        });
        let client = Client::new(service, self.default_namespace.clone());
        let task = spawn_watch_task(
            watch_api(client, &kind, ns),
            kind.ar.plural.clone(),
            ns.to_string(),
            watcher::Config::default().any_semantic(),
            Arc::clone(&self.streaming_lists),
            1,
            tx,
        );

        let mut probe = WatchProbe::Ran {
            synced: None,
            objects: 0,
            errors: 0,
            last_error: None,
        };
        let WatchProbe::Ran {
            synced,
            objects,
            errors,
            last_error,
        } = &mut probe
        else {
            unreachable!("just constructed")
        };
        let deadline = tokio::time::Instant::now() + timeout;
        while synced.is_none() || !*headers_rx.borrow() {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                changed = headers_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                msg = rx.recv() => match msg {
                    Some(Msg::Applied { .. }) => *objects += 1,
                    Some(Msg::Synced { .. }) => *synced = Some(started.elapsed()),
                    Some(Msg::Reset { .. }) => {
                        *objects = 0;
                        *synced = None;
                    }
                    Some(Msg::Error { error, .. }) => {
                        *errors += 1;
                        *last_error = Some(error);
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        if synced.is_some() && *headers_rx.borrow() && *errors == 0 {
            *synced = Some(started.elapsed());
        } else {
            *synced = None;
        }
        task.abort();
        let _ = task.await;
        probe
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

fn watch_api(client: Client, kind: &Kind, namespace: &str) -> Api<DynamicObject> {
    if kind.namespaced && !namespace.is_empty() {
        Api::namespaced_with(client, namespace, &kind.ar)
    } else {
        Api::all_with(client, &kind.ar)
    }
}

fn spawn_watch_task(
    api: Api<DynamicObject>,
    kind: String,
    ns: String,
    cfg: watcher::Config,
    streaming_lists: Arc<AtomicU8>,
    generation: u64,
    tx: Sender<Msg>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut using_streaming = streaming_lists.load(Ordering::Acquire) != STREAMING_UNSUPPORTED;
        let mut initializing = true;
        // Distinct from `initializing`, which drives the streaming-list
        // fallback and must keep its current lifetime: this only records
        // that a full sync has happened, so a later re-list is a reconnect.
        let mut synced_once = false;
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
        crate::log_info!(
            "watch.start",
            kind = kind,
            ns = if ns.is_empty() { "*" } else { ns.as_str() },
            generation = generation
        );
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
                Ok(watcher::Event::Init) => {
                    if synced_once {
                        // The watcher healed a desync by re-listing. The
                        // UI counts this as a reconnect off the same
                        // message, so nothing extra crosses the channel.
                        crate::log_info!("watch.relist", kind = kind, generation = generation);
                    }
                    Msg::Reset { generation }
                }
                Ok(watcher::Event::InitDone) => {
                    initializing = false;
                    synced_once = true;
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
                Err(e) if watch_error_is_benign(&e) => {
                    crate::log_debug!("watch.desync", kind = kind, error = e);
                    continue;
                }
                Err(e) => {
                    crate::log_warn!("watch.error", kind = kind, error = e);
                    Msg::Error {
                        generation,
                        error: e.to_string(),
                    }
                }
            };
            if tx.send(msg).await.is_err() {
                break; // UI gone
            }
        }
    })
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

fn current_context_name() -> Option<String> {
    // Config::infer() doesn't surface the context name, so read it directly.
    let kubeconfig = kube::config::Kubeconfig::read().ok()?;
    kubeconfig.current_context
}

/// A requested context (or the default kubeconfig's current one when none was
/// requested), its cluster name, and API-server URL, read offline. For
/// `sofka info` when no live connection is available. The server URL never
/// carries credentials.
pub fn context_info(
    requested: Option<&crate::kubeconfigs::ClusterId>,
) -> Option<(String, String, String)> {
    let source = requested.map(|id| id.source.clone()).unwrap_or_default();
    let kubeconfig = source.read().ok();
    context_info_from(kubeconfig.as_ref(), requested.map(|id| id.context.as_str()))
}

fn context_info_from(
    kubeconfig: Option<&kube::config::Kubeconfig>,
    requested: Option<&str>,
) -> Option<(String, String, String)> {
    let context = requested
        .map(str::to_owned)
        .or_else(|| kubeconfig.and_then(|config| config.current_context.clone()))?;
    let cluster_name = kubeconfig
        .and_then(|config| config.contexts.iter().find(|c| c.name == context))
        .and_then(|c| c.context.as_ref())
        .map(|c| c.cluster.clone())
        .unwrap_or_default();
    let server = kubeconfig
        .and_then(|config| config.clusters.iter().find(|c| c.name == cluster_name))
        .and_then(|c| c.cluster.as_ref())
        .and_then(|c| c.server.clone())
        .unwrap_or_default();
    Some((context, cluster_name, server))
}

/// The namespace a context pins, if any. For the offline diagnostics report,
/// which has no live client to ask.
pub fn context_namespace(id: &crate::kubeconfigs::ClusterId) -> Option<String> {
    id.source
        .read()
        .ok()?
        .contexts
        .iter()
        .find(|c| c.name == id.context)?
        .context
        .as_ref()?
        .namespace
        .clone()
        .filter(|ns| !ns.is_empty())
}

/// Public wrapper over [`cluster_name_for`] for resolving per-context config
/// (fleet dashboard read-only policy) without a live connection.
pub fn cluster_name_for_context(id: &crate::kubeconfigs::ClusterId) -> String {
    cluster_name_for(id).unwrap_or_default()
}

/// Kubeconfig cluster name a context points at, when its kubeconfig knows it.
fn cluster_name_for(id: &crate::kubeconfigs::ClusterId) -> Option<String> {
    id.source
        .read()
        .ok()?
        .contexts
        .iter()
        .find(|c| c.name == id.context)?
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
            source: crate::kubeconfigs::Source::Default,
            cli_context: Some("test".into()),
            connected: true,
            allow_v1_client_cert: false,
            registry: HashMap::new(),
            catalog: Vec::new(),
            streaming_lists: Arc::new(AtomicU8::new(STREAMING_UNKNOWN)),
            child_kinds: Vec::new(),
            discovery_warnings: Vec::new(),
            discovery_fallback: None,
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
        let hr = cluster.registry["helmreleases"].clone();
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
            .insert("events.events.k8s.io".to_string(), events_k8s_io);
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
        if namespaced {
            self.child_kinds.push(k.clone());
        }
        self.registry.insert(kind.to_lowercase(), k.clone());
        self.registry.insert(plural.clone(), k.clone());
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
pub(crate) mod tests {
    use super::*;

    fn probe_cluster(streaming: bool, status: u16, delay: Duration) -> Cluster {
        let mut cluster = Cluster::fake();
        cluster.client = Client::new(
            tower::service_fn(move |req: http::Request<kube::client::Body>| async move {
                let query = req.uri().query().unwrap_or("");
                let watch = query.contains("watch=true");
                let initial = query.contains("sendInitialEvents=true");
                let (code, body) = if initial && streaming {
                    (200, concat!(
                    "{\"type\":\"ADDED\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"one\",\"resourceVersion\":\"1\"}}}\n",
                    "{\"type\":\"BOOKMARK\",\"object\":{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"resourceVersion\":\"1\",\"annotations\":{\"k8s.io/initial-events-end\":\"true\"}}}}\n"
                ).to_string())
                } else if initial {
                    (400, r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"BadRequest","message":"streaming lists unsupported","code":400}"#.into())
                } else if watch {
                    tokio::time::sleep(delay).await;
                    (
                        status,
                        if status == 200 {
                            String::new()
                        } else {
                            serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Forbidden","message":"watch forbidden","code":status}).to_string()
                        },
                    )
                } else {
                    (200, r#"{"kind":"PodList","apiVersion":"v1","metadata":{"resourceVersion":"1"},"items":[{"apiVersion":"v1","kind":"Pod","metadata":{"name":"one","resourceVersion":"1"}}]}"#.into())
                };
                let keep_open = watch && code == 200;
                let frames = futures_util::stream::iter([Ok::<_, std::io::Error>(
                    hyper::body::Frame::data(hyper::body::Bytes::from(body)),
                )])
                .chain(futures_util::stream::poll_fn(move |_| {
                    if keep_open {
                        Poll::Pending
                    } else {
                        Poll::Ready(None)
                    }
                }));
                Ok::<_, std::io::Error>(
                    http::Response::builder()
                        .status(code)
                        .body(http_body_util::StreamBody::new(frames))
                        .unwrap(),
                )
            }),
            "default",
        );
        cluster
    }

    #[tokio::test]
    async fn probe_requires_watch_headers_after_a_successful_list() {
        let cluster = probe_cluster(false, 403, Duration::from_millis(20));
        let probe = cluster
            .probe_watch("pods", "default", Duration::from_secs(1))
            .await;
        assert!(
            matches!(&probe, WatchProbe::Ran { synced: None, objects: 1, errors: 1, last_error: Some(error) } if error.contains("watch forbidden"))
        );
    }

    #[tokio::test]
    async fn probe_bounds_the_wait_for_watch_headers() {
        let cluster = probe_cluster(false, 200, Duration::from_secs(1));
        let probe = cluster
            .probe_watch("pods", "default", Duration::from_millis(30))
            .await;
        assert!(matches!(
            probe,
            WatchProbe::Ran {
                synced: None,
                objects: 1,
                errors: 0,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn probe_accepts_a_quiet_watch_after_the_fallback_list() {
        let cluster = probe_cluster(false, 200, Duration::from_millis(20));
        let probe = cluster
            .probe_watch("pods", "default", Duration::from_secs(1))
            .await;
        assert!(
            matches!(probe, WatchProbe::Ran { synced: Some(elapsed), objects: 1, errors: 0, .. } if elapsed >= Duration::from_millis(20))
        );
    }

    #[tokio::test]
    async fn probe_accepts_a_complete_streaming_list() {
        let cluster = probe_cluster(true, 403, Duration::ZERO);
        let probe = cluster
            .probe_watch("pods", "default", Duration::from_secs(1))
            .await;
        assert!(matches!(
            probe,
            WatchProbe::Ran {
                synced: Some(_),
                objects: 1,
                errors: 0,
                ..
            }
        ));
    }

    fn exec_latency() -> (u64, u64, f64) {
        crate::diagnostics::latency_summary()
            .into_iter()
            .find(|summary| summary.op == Op::Exec)
            .map(|summary| (summary.count, summary.errors, summary.max_ms))
            .unwrap_or_default()
    }

    #[test]
    fn meter_records_completed_and_cancelled_requests_once() {
        let _guard = crate::diagnostics::LATENCY_TEST_LOCK.lock().unwrap();
        let before = exec_latency();
        let waker = futures_util::task::noop_waker_ref();
        let mut cx = TaskContext::from_waker(waker);

        let mut completed = Metered::new(
            std::future::ready(Ok::<_, std::io::Error>(http::Response::new(()))),
            Op::Exec,
        );
        assert!(matches!(
            Pin::new(&mut completed).poll(&mut cx),
            Poll::Ready(Ok(_))
        ));
        drop(completed);
        let after_completed = exec_latency();
        assert_eq!(after_completed.0, before.0 + 1);
        assert_eq!(after_completed.1, before.1);

        let mut cancelled = Metered::new(
            std::future::pending::<Result<http::Response<()>, std::io::Error>>(),
            Op::Exec,
        );
        assert!(Pin::new(&mut cancelled).poll(&mut cx).is_pending());
        std::thread::sleep(Duration::from_millis(2));
        drop(cancelled);
        let after_cancelled = exec_latency();
        assert_eq!(after_cancelled.0, after_completed.0 + 1);
        assert_eq!(after_cancelled.1, after_completed.1 + 1);
        assert!(after_cancelled.2 >= 1.0, "max {}", after_cancelled.2);
    }

    #[test]
    fn offline_context_info_prefers_the_requested_context() {
        let kubeconfig: kube::config::Kubeconfig = serde_yaml::from_str(
            r#"
current-context: dev
contexts:
  - name: dev
    context: { cluster: dev-cluster }
  - name: prod
    context: { cluster: prod-cluster }
clusters:
  - name: dev-cluster
    cluster: { server: https://dev.example }
  - name: prod-cluster
    cluster: { server: https://prod.example }
"#,
        )
        .unwrap();

        assert_eq!(
            context_info_from(Some(&kubeconfig), Some("prod")),
            Some((
                "prod".into(),
                "prod-cluster".into(),
                "https://prod.example".into()
            ))
        );
        assert_eq!(
            context_info_from(Some(&kubeconfig), None),
            Some((
                "dev".into(),
                "dev-cluster".into(),
                "https://dev.example".into()
            ))
        );
        assert_eq!(
            context_info_from(None, Some("prod")),
            Some(("prod".into(), String::new(), String::new()))
        );
    }

    #[tokio::test]
    async fn client_accepts_socks5_proxy() {
        let mut config = Config::new("https://127.0.0.1:6443".parse().unwrap());
        config.proxy_url = Some("socks5://127.0.0.1:9090".parse().unwrap());

        Client::try_from(config).expect("build client with a SOCKS5 proxy");
    }

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
    pub(crate) async fn mock_apiserver(
        supports_aggregated: bool,
        include_broken: bool,
        serve_version: bool,
    ) -> String {
        mock_apiserver_with_requests(MockOptions {
            supports_aggregated,
            include_broken,
            serve_version,
            ..MockOptions::default()
        })
        .await
        .0
    }

    #[derive(Clone, Copy, Default)]
    pub(crate) struct MockOptions {
        pub supports_aggregated: bool,
        pub include_broken: bool,
        pub serve_version: bool,
        pub core_unreadable: bool,
        pub aggregated_unreadable: bool,
    }

    pub(crate) async fn mock_apiserver_opts(opts: MockOptions) -> String {
        mock_apiserver_with_requests(opts).await.0
    }

    async fn mock_apiserver_with_requests(
        opts: MockOptions,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let MockOptions {
            supports_aggregated,
            serve_version,
            ..
        } = opts;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock apiserver");
        let addr = listener.local_addr().expect("local addr");

        fn route(path: &str, aggregated: bool, opts: MockOptions) -> (&'static str, String) {
            let include_broken = opts.include_broken;
            let broken_legacy = r#",{"name":"broken.example.com","versions":[{"groupVersion":"broken.example.com/v1beta1","version":"v1beta1"}],"preferredVersion":{"groupVersion":"broken.example.com/v1beta1","version":"v1beta1"}},{"name":"odd.example.com","versions":[{"groupVersion":"odd.example.com/v1alpha3","version":"v1alpha3"}],"preferredVersion":{"groupVersion":"odd.example.com/v1alpha3","version":"v1alpha3"}}"#;
            let broken_v2 = r#",{"metadata":{"name":"broken.example.com"},"versions":[{"version":"v1beta1","resources":[],"freshness":"Stale"}]}"#;
            // A mixed-version group modeled on the netbird.io operator: the
            // preferred version (v1) serves `widgets`, while `gadgets` is
            // only served at v1alpha1 — a preferred-version-only walk never
            // sees gadgets.
            let mixed_v2 = r#",{"metadata":{"name":"mixed.example.com"},"versions":[{"version":"v1","resources":[{"resource":"widgets","responseKind":{"group":"mixed.example.com","version":"v1","kind":"Widget"},"scope":"Namespaced","singularResource":"widget","shortNames":["zz","po","pods","shared","native","cross"],"verbs":["get","list","watch"]}],"freshness":"Current"},{"version":"v1alpha1","resources":[{"resource":"gadgets","responseKind":{"group":"mixed.example.com","version":"v1alpha1","kind":"Gadget"},"scope":"Namespaced","singularResource":"gadget","shortNames":["gd","shared"],"verbs":["get","list","watch"]},{"resource":"widgets","responseKind":{"kind":"Widget"},"scope":"Namespaced","shortNames":["oldwidget"],"verbs":["get","list","watch"]}],"freshness":"Current"}]}"#;
            let mixed_legacy = r#",{"name":"mixed.example.com","versions":[{"groupVersion":"mixed.example.com/v1","version":"v1"},{"groupVersion":"mixed.example.com/v1alpha1","version":"v1alpha1"}],"preferredVersion":{"groupVersion":"mixed.example.com/v1","version":"v1"}}"#;
            let capi_legacy = r#",{"name":"cluster.x-k8s.io","versions":[{"groupVersion":"cluster.x-k8s.io/v1beta1","version":"v1beta1"}],"preferredVersion":{"groupVersion":"cluster.x-k8s.io/v1beta1","version":"v1beta1"}}"#;
            let capi_v2 = r#",{"metadata":{"name":"cluster.x-k8s.io"},"versions":[{"version":"v1beta1","freshness":"Current","resources":[{"resource":"machinedeployments","responseKind":{"kind":"MachineDeployment"},"scope":"Namespaced","shortNames":["md","cross"],"verbs":["get","list","watch"]},{"resource":"machinedrainrules","responseKind":{"kind":"MachineDrainRule"},"scope":"Namespaced","verbs":["get","list","watch"]}]}]}"#;
            match (path, aggregated) {
                ("/apis", true) if opts.aggregated_unreadable => (
                    "200 OK",
                    r#"{"kind":"APIGroupDiscoveryList","apiVersion":"apidiscovery.k8s.io/v2","metadata":{},"items":[{"metadata":{"name":"apps"},"versions":"not-a-list"}]}"#.into(),
                ),
                ("/api/v1", _) if opts.core_unreadable => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1alpha3","groupVersion":"v1","resources":[]}"#.into(),
                ),
                ("/version", _) => (
                    "200 OK",
                    r#"{"major":"1","minor":"36","gitVersion":"v1.36.2-eks-bca9cf6","gitCommit":"abc123","gitTreeState":"clean","buildDate":"2026-08-20T00:00:00Z","goVersion":"go1.25.0","compiler":"gc","platform":"linux/amd64"}"#.into(),
                ),
                ("/apis", true) => (
                    "200 OK",
                    format!(
                        r#"{{"kind":"APIGroupDiscoveryList","apiVersion":"apidiscovery.k8s.io/v2","metadata":{{}},"items":[{{"metadata":{{"name":"apps"}},"versions":[{{"version":"v1","resources":[{{"resource":"deployments","responseKind":{{"group":"apps","version":"v1","kind":"Deployment"}},"scope":"Namespaced","singularResource":"deployment","verbs":["get","list","watch"]}}],"freshness":"Current"}}]}}{mixed_v2}{capi_v2}{}]}}"#,
                        if include_broken { broken_v2 } else { "" }
                    ),
                ),
                ("/api", true) => (
                    "200 OK",
                    r#"{"kind":"APIGroupDiscoveryList","apiVersion":"apidiscovery.k8s.io/v2","metadata":{},"items":[{"metadata":{"name":""},"versions":[{"version":"v1","resources":[{"resource":"pods","responseKind":{"group":"","version":"v1","kind":"Pod"},"scope":"Namespaced","singularResource":"pod","shortNames":["native"],"verbs":["get","list","watch"]}],"freshness":"Current"}]}]}"#.into(),
                ),
                ("/apis", false) => (
                    "200 OK",
                    format!(
                        r#"{{"kind":"APIGroupList","apiVersion":"v1","groups":[{{"name":"apps","versions":[{{"groupVersion":"apps/v1","version":"v1"}}],"preferredVersion":{{"groupVersion":"apps/v1","version":"v1"}}}}{mixed_legacy}{capi_legacy}{}]}}"#,
                        if include_broken { broken_legacy } else { "" }
                    ),
                ),
                ("/api", false) => (
                    "200 OK",
                    r#"{"kind":"APIVersions","versions":["v1"],"serverAddressByClientCIDRs":[]}"#.into(),
                ),
                ("/apis/cluster.x-k8s.io/v1beta1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","groupVersion":"cluster.x-k8s.io/v1beta1","resources":[{"name":"machinedeployments","kind":"MachineDeployment","singularName":"machinedeployment","namespaced":true,"shortNames":["md","cross"],"verbs":["get","list","watch"]},{"name":"machinedrainrules","kind":"MachineDrainRule","singularName":"machinedrainrule","namespaced":true,"verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/apps/v1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"apps/v1","resources":[{"name":"deployments","singularName":"deployment","namespaced":true,"kind":"Deployment","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/api/v1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"v1","resources":[{"name":"pods","singularName":"pod","shortNames":["native"],"namespaced":true,"kind":"Pod","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/mixed.example.com/v1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"mixed.example.com/v1","resources":[{"name":"widgets","singularName":"widget","shortNames":["zz","po","pods","shared","native","cross"],"namespaced":true,"kind":"Widget","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/mixed.example.com/v1alpha1", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1","groupVersion":"mixed.example.com/v1alpha1","resources":[{"name":"gadgets","singularName":"gadget","shortNames":["gd","shared"],"namespaced":true,"kind":"Gadget","verbs":["get","list","watch"]}]}"#.into(),
                ),
                ("/apis/odd.example.com/v1alpha3", _) => (
                    "200 OK",
                    r#"{"kind":"APIResourceList","apiVersion":"v1alpha3","groupVersion":"odd.example.com/v1alpha3","resources":[{"name":"oddities","singularName":"oddity","namespaced":true,"kind":"Oddity","verbs":["get","list","watch"]}]}"#.into(),
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
                let recorded = Arc::clone(&recorded);
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
                        recorded.lock().unwrap().push(path.clone());
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
                        let (status, body) =
                            route(&path, wants_aggregated && supports_aggregated, opts);
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
        (format!("http://{addr}"), requests)
    }

    pub(crate) async fn connect_mock(url: String) -> Result<Cluster> {
        let mut config = Config::new(url.parse().expect("mock url"));
        // The client's default retry policy (15 attempts, exponential
        // backoff) turns the mock's deliberate 503 into a ~4-minute stall;
        // retrying is not what these tests exercise.
        config.default_retry = false;
        Cluster::from_config(config, "test".into(), None, false).await
    }

    #[tokio::test]
    async fn short_names_do_not_need_extra_discovery_requests() {
        for aggregated in [true, false] {
            let (url, requests) = mock_apiserver_with_requests(MockOptions {
                supports_aggregated: aggregated,
                serve_version: true,
                ..MockOptions::default()
            })
            .await;
            let cluster = connect_mock(url).await.unwrap();
            assert_eq!(cluster.resolve("md").unwrap().ar.kind, "MachineDeployment");
            let mut requests = requests.lock().unwrap().clone();
            requests.sort();
            let mut expected = vec!["/api", "/apis", "/version"];
            if !aggregated {
                expected.extend([
                    "/api",
                    "/apis",
                    "/api/v1",
                    "/apis/apps/v1",
                    "/apis/cluster.x-k8s.io/v1beta1",
                    "/apis/mixed.example.com/v1",
                    "/apis/mixed.example.com/v1alpha1",
                ]);
            }
            expected.sort();
            assert_eq!(requests, expected);
        }
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
        assert!(cluster.discovery_warnings.is_empty());
        assert!(cluster.discovery_fallback.is_none());
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
        assert!(cluster.discovery_warnings.is_empty());
        assert!(cluster.discovery_fallback.is_none());
    }

    /// Asserts every kind of the mixed-version group resolved: `widgets` at
    /// the preferred v1, `gadgets` only served at v1alpha1. A discovery walk
    /// limited to each group's preferred version loses gadgets entirely
    /// (the netbird.io bug: `:sidecarprofiles.netbird.io` -> no match).
    fn assert_mixed_group(cluster: &Cluster) {
        assert!(cluster.resolve("oldwidget").is_none());
        assert!(cluster.resolve("statusalias").is_none());
        assert_eq!(cluster.resolve("zz").unwrap().ar.version, "v1");
        assert_eq!(cluster.resolve("gd").unwrap().ar.version, "v1alpha1");
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
    async fn legacy_walk_skips_unreadable_groups_with_warnings() {
        let url = mock_apiserver(false, true, true).await;
        let cluster = connect_mock(url)
            .await
            .expect("connect despite unreadable groups");
        assert!(cluster.resolve("deployments").is_some());
        assert!(cluster.resolve("pods").is_some());
        assert!(cluster.resolve("oddities").is_none());
        let warnings = &cluster.discovery_warnings;
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("API discovery could not read broken.example.com/v1beta1: ")),
            "{warnings:?}"
        );
        let odd = warnings
            .iter()
            .find(|w| w.starts_with("API discovery could not read odd.example.com/v1alpha3: "))
            .expect("v1alpha3 group is named");
        assert!(odd.contains("expected v1"), "{odd}");
        assert_eq!(odd.matches("expected v1").count(), 1, "{odd}");
        assert!(cluster.discovery_fallback.is_none());
    }

    #[tokio::test]
    async fn legacy_walk_fails_when_the_core_group_is_unreadable() {
        let url = mock_apiserver_opts(MockOptions {
            supports_aggregated: false,
            serve_version: true,
            core_unreadable: true,
            ..MockOptions::default()
        })
        .await;
        let err = connect_mock(url)
            .await
            .err()
            .expect("a cluster without a readable core group is unusable");
        let text = format!("{err:#}");
        assert!(text.contains("reading core API group v1"), "{text}");
        assert!(text.contains("expected v1"), "{text}");
    }

    #[tokio::test]
    async fn failed_aggregated_discovery_is_reported_but_not_counted_as_skipped() {
        let url = mock_apiserver_opts(MockOptions {
            supports_aggregated: true,
            serve_version: true,
            aggregated_unreadable: true,
            ..MockOptions::default()
        })
        .await;
        let cluster = connect_mock(url).await.expect("legacy walk still connects");
        assert!(cluster.resolve("deployments").is_some());
        assert!(cluster.resolve("pods").is_some());
        assert!(cluster.discovery_warnings.is_empty());
        let note = cluster
            .discovery_fallback
            .as_deref()
            .expect("the fallback reason is kept");
        assert!(
            note.starts_with("Aggregated API discovery failed: "),
            "{note}"
        );
        assert!(
            note.ends_with("Sofka read each API group separately."),
            "{note}"
        );
    }
}
