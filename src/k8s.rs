//! Kubernetes connectivity: client bootstrap, resource discovery, alias
//! resolution, and async watch streams that feed the in-memory store.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use kube::api::{Api, ListParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::core::DynamicObject;
use kube::discovery::{ApiResource, Discovery, Scope};
use kube::runtime::{WatchStreamExt, watcher};
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
}

/// Connection + discovery context for a cluster.
pub struct Cluster {
    pub client: Client,
    pub context: String,
    /// Kubeconfig cluster name referenced by `context` (empty when unknown,
    /// e.g. in-cluster). Keys per-cluster config overrides.
    pub cluster_name: String,
    pub cluster_url: String,
    pub default_namespace: String,
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
}

impl Cluster {
    pub async fn connect() -> Result<Self> {
        let config = Config::infer()
            .await
            .context("loading kubeconfig (is KUBECONFIG / ~/.kube/config present?)")?;
        // The real kubeconfig current-context (if any) is what kubectl uses by
        // default; pass it explicitly so shell-outs can't drift from us.
        let cli_context = current_context_name();
        let context = cli_context.clone().unwrap_or_else(|| "default".into());
        Self::from_config(config, context, cli_context).await
    }

    /// Connect using a specific kubeconfig context (for the `:ctx` switcher).
    pub async fn connect_context(name: &str) -> Result<Self> {
        let kubeconfig = Kubeconfig::read().context("reading kubeconfig")?;
        let opts = KubeConfigOptions {
            context: Some(name.to_string()),
            cluster: None,
            user: None,
        };
        let config = Config::from_custom_kubeconfig(kubeconfig, &opts)
            .await
            .with_context(|| format!("building config for context '{name}'"))?;
        Self::from_config(config, name.to_string(), Some(name.to_string())).await
    }

    async fn from_config(
        config: Config,
        context: String,
        cli_context: Option<String>,
    ) -> Result<Self> {
        let cluster_url = config.cluster_url.to_string();
        let default_namespace = config.default_namespace.clone();
        let client = Client::try_from(config).context("building kube client")?;

        let cluster_name = cluster_name_for(&context).unwrap_or_default();
        let mut cluster = Self {
            client,
            context,
            cluster_name,
            cluster_url,
            default_namespace,
            cli_context,
            registry: HashMap::new(),
            catalog: Vec::new(),
            connected: true,
        };
        cluster.discover().await?;
        Ok(cluster)
    }

    /// A placeholder for launching when the current context's API server is
    /// unreachable (k9s drops you into the context picker in this situation
    /// instead of exiting). Identity fields come straight from the kubeconfig
    /// so the header still names the broken context; the client points at the
    /// configured server but nothing uses it until a real context connects.
    pub fn disconnected() -> Self {
        let kubeconfig = Kubeconfig::read().ok();
        let context = kubeconfig
            .as_ref()
            .and_then(|k| k.current_context.clone())
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
            default_namespace: "default".into(),
            registry: HashMap::new(),
            catalog: Vec::new(),
            connected: false,
        }
    }

    /// Context name to pass to `kubectl` (`--context`), when known. Keeps
    /// shell-outs (edit/describe/exec/attach/port-forward) on the same cluster
    /// sofka is connected to, even after an in-app `:ctx` switch.
    pub fn kubectl_context(&self) -> Option<&str> {
        self.cli_context.as_deref()
    }

    /// All context names from the kubeconfig.
    pub fn list_contexts() -> Vec<String> {
        Kubeconfig::read()
            .map(|k| k.contexts.into_iter().map(|c| c.name).collect())
            .unwrap_or_default()
    }

    /// Merge user-defined aliases (alias -> canonical) into the registry.
    pub fn add_aliases(&mut self, aliases: &HashMap<String, String>) {
        for (alias, target) in aliases {
            if let Some(k) = self.registry.get(&target.to_lowercase()).cloned() {
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
        let mut entries: Vec<(Kind, String, String)> = Vec::new(); // (kind, plural, kind_lc)
        let mut catalog = Vec::new();
        for group in discovery.groups() {
            for (ar, caps) in group.recommended_resources() {
                let namespaced = matches!(caps.scope, Scope::Namespaced);
                let kind = Kind {
                    ar: ar.clone(),
                    namespaced,
                };
                let plural = ar.plural.to_lowercase();
                let kind_lc = ar.kind.to_lowercase();
                catalog.push(plural.clone());
                // Group-qualified keys are unambiguous; insert directly.
                if !ar.group.is_empty() {
                    self.registry
                        .insert(format!("{}.{}", plural, ar.group), kind.clone());
                }
                entries.push((kind, plural, kind_lc));
            }
        }
        // Lowest priority first; later inserts overwrite, so the highest
        // priority group ends up owning each bare plural/kind key.
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
        Ok(())
    }

    pub fn resolve(&self, input: &str) -> Option<Kind> {
        let key = input.trim().trim_start_matches(':').to_lowercase();
        self.registry.get(&key).cloned()
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
            let mut stream = watcher(api, cfg)
                .modify(|obj| obj.managed_fields_mut().clear())
                .boxed();
            if tx.send(Msg::Reset { generation }).await.is_err() {
                return;
            }

            while let Some(event) = stream.next().await {
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
                    Ok(watcher::Event::InitDone) => Msg::Synced { generation },
                    Err(e) => Msg::Error {
                        generation,
                        error: e.to_string(),
                    },
                };
                if tx.send(msg).await.is_err() {
                    break; // UI gone
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
    let kubeconfig = kube::config::Kubeconfig::read().ok()?;
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

#[cfg(test)]
impl Cluster {
    /// A connectionless cluster for unit tests: the client points at a dummy
    /// URL (no I/O happens until a request is actually made) and the registry
    /// is a small hand-built set of common kinds.
    pub(crate) fn fake() -> Self {
        let config = Config::new("https://127.0.0.1:6443".parse().unwrap());
        let client = Client::try_from(config).expect("build test client");
        let mk = |group: &str, kind: &str, plural: &str, namespaced: bool| Kind {
            ar: ApiResource {
                group: group.to_string(),
                version: "v1".to_string(),
                api_version: if group.is_empty() {
                    "v1".to_string()
                } else {
                    format!("{group}/v1")
                },
                kind: kind.to_string(),
                plural: plural.to_string(),
            },
            namespaced,
        };
        let mut registry = HashMap::new();
        registry.insert("pods".to_string(), mk("", "Pod", "pods", true));
        registry.insert(
            "deployments".to_string(),
            mk("apps", "Deployment", "deployments", true),
        );
        registry.insert("services".to_string(), mk("", "Service", "services", true));
        registry.insert("secrets".to_string(), mk("", "Secret", "secrets", true));
        registry.insert("nodes".to_string(), mk("", "Node", "nodes", false));
        registry.insert(
            "namespaces".to_string(),
            mk("", "Namespace", "namespaces", false),
        );
        registry.insert(
            "kustomizations".to_string(),
            mk(
                "kustomize.toolkit.fluxcd.io",
                "Kustomization",
                "kustomizations",
                true,
            ),
        );
        // An alias/plural pair that collide on fuzzy matching (`hr` is a
        // subsequence of horizontalpodautoscalers), for suggestion-priority
        // tests.
        let hr = mk(
            "helm.toolkit.fluxcd.io",
            "HelmRelease",
            "helmreleases",
            true,
        );
        registry.insert("helmreleases".to_string(), hr.clone());
        registry.insert("hr".to_string(), hr);
        registry.insert(
            "horizontalpodautoscalers".to_string(),
            mk(
                "autoscaling",
                "HorizontalPodAutoscaler",
                "horizontalpodautoscalers",
                true,
            ),
        );
        registry.insert(
            "externalsecrets".to_string(),
            mk(
                "external-secrets.io",
                "ExternalSecret",
                "externalsecrets",
                true,
            ),
        );
        // A CR without curated columns, for custom-view tests.
        registry.insert(
            "certificates".to_string(),
            mk("cert-manager.io", "Certificate", "certificates", true),
        );
        Self {
            client,
            context: "test".into(),
            cluster_name: "test-cluster".into(),
            cluster_url: "https://127.0.0.1:6443".into(),
            default_namespace: "default".into(),
            cli_context: Some("test".into()),
            connected: true,
            registry,
            catalog: vec![
                "certificates".to_string(),
                "deployments".to_string(),
                "helmreleases".to_string(),
                "horizontalpodautoscalers".to_string(),
                "kustomizations".to_string(),
                "namespaces".to_string(),
                "nodes".to_string(),
                "pods".to_string(),
                "services".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn mock_apiserver(supports_aggregated: bool, include_broken: bool) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock apiserver");
        let addr = listener.local_addr().expect("local addr");

        fn route(path: &str, aggregated: bool, include_broken: bool) -> (&'static str, String) {
            let broken_legacy = r#",{"name":"broken.example.com","versions":[{"groupVersion":"broken.example.com/v1beta1","version":"v1beta1"}],"preferredVersion":{"groupVersion":"broken.example.com/v1beta1","version":"v1beta1"}}"#;
            let broken_v2 = r#",{"metadata":{"name":"broken.example.com"},"versions":[{"version":"v1beta1","resources":[],"freshness":"Stale"}]}"#;
            match (path, aggregated) {
                ("/apis", true) => (
                    "200 OK",
                    format!(
                        r#"{{"kind":"APIGroupDiscoveryList","apiVersion":"apidiscovery.k8s.io/v2","metadata":{{}},"items":[{{"metadata":{{"name":"apps"}},"versions":[{{"version":"v1","resources":[{{"resource":"deployments","responseKind":{{"group":"apps","version":"v1","kind":"Deployment"}},"scope":"Namespaced","singularResource":"deployment","verbs":["get","list","watch"]}}],"freshness":"Current"}}]}}{}]}}"#,
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
                        r#"{{"kind":"APIGroupList","apiVersion":"v1","groups":[{{"name":"apps","versions":[{{"groupVersion":"apps/v1","version":"v1"}}],"preferredVersion":{{"groupVersion":"apps/v1","version":"v1"}}}}{}]}}"#,
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
        Cluster::from_config(config, "test".into(), None).await
    }

    #[tokio::test]
    async fn discovery_tolerates_broken_apiservice() {
        // A dead aggregated API backend (e.g. metrics-server) must not make
        // the whole cluster unconnectable: aggregated discovery serves the
        // broken group as stale instead of 503ing.
        let url = mock_apiserver(true, true).await;
        let cluster = connect_mock(url)
            .await
            .expect("connect with broken APIService");
        assert!(cluster.resolve("deployments").is_some());
        assert!(cluster.resolve("pods").is_some());
    }

    #[tokio::test]
    async fn discovery_falls_back_without_aggregated_support() {
        // Pre-1.26 servers answer the aggregated request with the legacy
        // document, which deserializes as an *empty* group list (not an
        // error) — discovery must detect that and take the per-group walk.
        let url = mock_apiserver(false, false).await;
        let cluster = connect_mock(url)
            .await
            .expect("connect via legacy discovery walk");
        assert!(cluster.resolve("deployments").is_some());
        assert!(cluster.resolve("pods").is_some());
    }

    #[tokio::test]
    async fn legacy_walk_still_fails_on_broken_apiservice() {
        // Documents the failure mode the aggregated path exists to avoid:
        // the per-group walk hits the broken group's 503 and discovery fails
        // (after ~4 minutes of client-side 503 retries with the default
        // config). If kube-rs ever makes run() tolerant, this starts failing
        // and the aggregated workaround can be simplified.
        let url = mock_apiserver(false, true).await;
        assert!(connect_mock(url).await.is_err());
    }
}
