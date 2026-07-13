//! Optional external provider integrations.
//!
//! Providers are small, config-driven interfaces to observability backends
//! that launch from the currently selected Kubernetes object. The core stays
//! fully usable without any provider configured — a provider only adds ways
//! to look at data Kubernetes itself doesn't keep (log history beyond a pod's
//! lifetime, logs of deleted pods, whole-namespace searches).
//!
//! The first (and so far only) provider kind is a log backend:
//! [VictoriaLogs](https://docs.victoriametrics.com/victorialogs/), queried
//! over its HTTP API with LogsQL. Loki-style backends can slot in later
//! behind the same [`LogProvider`] surface.
//!
//! Zero configuration is the default path: with no `[providers.logs]`
//! section, pressing `L` [`discover`]s a VictoriaLogs `Service` in the
//! cluster by its well-known labels and reaches it through the Kubernetes
//! API server's service proxy — the ClusterIP itself isn't routable from a
//! workstation, and the proxy reuses the session's authentication. An
//! explicit config takes precedence:
//!
//! ```toml
//! [providers.logs]
//! type = "victorialogs"
//! url = "https://vlogs.example.com"  # omit to autodiscover in-cluster
//! lookback = "1h"        # how far back the initial query reaches
//! limit = 300            # lines fetched by the initial query
//!
//! [providers.logs.headers]
//! Authorization = "Bearer <token>"
//!
//! # Field names depend on the log shipper. Without this section they are
//! # detected from the backend's stream fields (vector, fluentd, fluent-bit,
//! # OpenTelemetry, and bare conventions are recognized).
//! [providers.logs.fields]
//! namespace = "kubernetes.pod_namespace"
//! pod = "kubernetes.pod_name"
//! container = "kubernetes.container_name"
//! ```
//!
//! Like every other config section, `[providers.logs]` participates in
//! per-cluster/per-context override files, so each cluster can point at its
//! own backend.

use futures_util::{AsyncBufReadExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, ListParams};
use serde_json::Value;

use crate::config::LogProviderConfig;

const DEFAULT_NAMESPACE_FIELD: &str = "kubernetes.pod_namespace";
const DEFAULT_POD_FIELD: &str = "kubernetes.pod_name";
const DEFAULT_CONTAINER_FIELD: &str = "kubernetes.container_name";
pub const DEFAULT_LOOKBACK: &str = "1h";
/// Lines fetched by the initial (backfill) query — mirrors the 300-line tail
/// the kubectl-API log view starts with.
const DEFAULT_LIMIT: usize = 300;
/// One-shot queries that take longer than this are almost certainly scanning
/// far more than a scoped stream filter should.
const QUERY_TIMEOUT_SECS: u64 = 30;

/// A compiled, validated log-provider configuration. Cheap to clone into
/// background tasks.
#[derive(Clone)]
pub struct LogProvider {
    transport: Transport,
    headers: Vec<(String, String)>,
    fields: Fields,
    /// Whether `fields` came from explicit config. When they didn't, the
    /// first use runs [`LogProvider::detect_fields`] against the backend,
    /// because shippers disagree on names (vector's `kubernetes.pod_name`
    /// vs fluent-bit's `kubernetes_pod_name`, …).
    fields_configured: bool,
    lookback_secs: i64,
    /// The configured lookback verbatim (`"1h"`), for titles and messages.
    pub lookback_label: String,
    limit: usize,
}

impl Default for LogProvider {
    /// The zero-config provider: everything at its default, transport and
    /// field names to be discovered in the cluster.
    fn default() -> Self {
        Self {
            transport: Transport::Auto,
            headers: Vec::new(),
            fields: Fields::default(),
            fields_configured: false,
            lookback_secs: 3600,
            lookback_label: DEFAULT_LOOKBACK.into(),
            limit: DEFAULT_LIMIT,
        }
    }
}

/// How the backend is reached.
#[derive(Clone)]
enum Transport {
    /// Straight HTTP(S) to a configured base URL (no trailing slash) — an
    /// ingress, a load balancer, or a local port-forward.
    Direct { url: String },
    /// Through the Kubernetes API server's service proxy
    /// (`/api/v1/namespaces/<ns>/services/<name>:<port>/proxy`), using the
    /// session's already-authenticated client. How discovered in-cluster
    /// services are reached: their ClusterIP isn't routable from a laptop.
    ServiceProxy {
        client: kube::Client,
        ns: String,
        service: String,
        port: i32,
    },
    /// Not yet known — [`discover`] resolves it to [`Transport::ServiceProxy`]
    /// on first use.
    Auto,
}

/// Log-record field names as ingested by the user's log shipper. Stream
/// fields in VictoriaLogs terms; the defaults match the vector/Kubernetes
/// setup documented by VictoriaLogs.
#[derive(Clone)]
struct Fields {
    namespace: String,
    pod: String,
    container: String,
}

impl Default for Fields {
    fn default() -> Self {
        Self {
            namespace: DEFAULT_NAMESPACE_FIELD.into(),
            pod: DEFAULT_POD_FIELD.into(),
            container: DEFAULT_CONTAINER_FIELD.into(),
        }
    }
}

/// A provider-logs request as derived from the selected row, before any
/// Kubernetes lookups. [`LogRequest::Selector`] still names a label selector;
/// the streaming task resolves it to concrete pods ([`LogScope::Pods`])
/// because a log backend only knows the fields that were ingested, and pod
/// labels usually aren't.
#[derive(Clone, Debug, PartialEq)]
pub enum LogRequest {
    Pod {
        ns: String,
        pod: String,
        container: Option<String>,
        /// Whether the pod has several containers (tags lines per container).
        multi_container: bool,
    },
    /// Pods of one workload/service, matched by its label selector.
    Selector {
        ns: String,
        labels: String,
    },
    Namespace {
        ns: String,
    },
}

/// What to query the backend for: a [`LogRequest`] after selector resolution.
#[derive(Clone, Debug, PartialEq)]
pub enum LogScope {
    Pod {
        ns: String,
        pod: String,
        container: Option<String>,
    },
    /// Concrete pods of one workload/service (resolved from its selector).
    Pods {
        ns: String,
        pods: Vec<String>,
    },
    Namespace {
        ns: String,
    },
}

/// How each rendered line is tagged, decided by what the scope covers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Prefix {
    /// Single container — no tag.
    None,
    /// One pod, several containers — `[container]`.
    Container,
    /// Several pods — `[pod:container]`.
    PodContainer,
}

/// One parsed log record from the backend.
#[derive(Debug, PartialEq)]
pub struct LogEntry {
    /// Raw RFC3339 `_time` as returned by the backend.
    pub time: String,
    /// `_time` in Unix nanoseconds, when it parsed. Used to de-duplicate the
    /// seam between the backfill query and the live tail.
    pub nanos: Option<i128>,
    pub msg: String,
    pub pod: String,
    pub container: String,
}

impl LogEntry {
    /// Render the entry as display lines (a multi-line message becomes one
    /// buffer line per physical line, like a streamed log would).
    pub fn lines(&self, prefix: Prefix, timestamps: bool) -> Vec<String> {
        let tag = match prefix {
            Prefix::None => String::new(),
            Prefix::Container if self.container.is_empty() => String::new(),
            Prefix::Container => format!("[{}] ", self.container),
            Prefix::PodContainer => match (self.pod.is_empty(), self.container.is_empty()) {
                (true, _) => String::new(),
                (false, true) => format!("[{}] ", self.pod),
                (false, false) => format!("[{}:{}] ", self.pod, self.container),
            },
        };
        let ts = if timestamps && !self.time.is_empty() {
            format!("{} ", self.time)
        } else {
            String::new()
        };
        self.msg
            .split('\n')
            .map(|part| format!("{tag}{ts}{part}"))
            .collect()
    }
}

/// Compile the raw `[providers.logs]` config into a ready-to-use provider.
/// Follows the [`crate::views::compile`] contract: problems become warnings,
/// never errors, and a section broken enough to be unusable yields `None`.
pub fn compile(cfg: Option<&LogProviderConfig>) -> (Option<LogProvider>, Vec<String>) {
    let Some(cfg) = cfg else {
        return (None, Vec::new());
    };
    let mut warnings = Vec::new();

    match cfg.kind.as_str() {
        "victorialogs" => {}
        "" => {
            warnings.push("providers.logs: missing `type` (expected \"victorialogs\")".into());
            return (None, warnings);
        }
        other => {
            warnings.push(format!(
                "providers.logs: unsupported type {other:?} (expected \"victorialogs\")"
            ));
            return (None, warnings);
        }
    }

    // An omitted/empty url means "find it in the cluster": the transport is
    // discovered on first use and reached through the API-server proxy.
    let url = cfg.url.trim().trim_end_matches('/').to_string();
    let transport = if url.is_empty() {
        Transport::Auto
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Transport::Direct { url }
    } else {
        warnings.push(format!(
            "providers.logs: url {:?} must start with http:// or https:// (or be omitted for autodiscovery)",
            cfg.url
        ));
        return (None, warnings);
    };

    let lookback_label = cfg
        .lookback
        .clone()
        .unwrap_or_else(|| DEFAULT_LOOKBACK.into());
    let lookback_secs = match parse_lookback(&lookback_label) {
        Ok(secs) => secs,
        Err(e) => {
            warnings.push(format!("providers.logs: lookback: {e}"));
            return (None, warnings);
        }
    };

    let mut headers = Vec::new();
    for (name, value) in &cfg.headers {
        let ok = http::header::HeaderName::try_from(name.as_str()).is_ok()
            && http::header::HeaderValue::try_from(value.as_str()).is_ok();
        if ok {
            headers.push((name.clone(), value.clone()));
        } else {
            warnings.push(format!("providers.logs: invalid header {name:?} — skipped"));
        }
    }
    headers.sort();

    let mut field = |name: &str, value: &Option<String>, default: &str| -> String {
        match value {
            None => default.into(),
            Some(v) if valid_field_name(v) => v.clone(),
            Some(v) => {
                warnings.push(format!(
                    "providers.logs: fields.{name} {v:?} has unsupported characters — using {default:?}"
                ));
                default.into()
            }
        }
    };
    let fields = Fields {
        namespace: field("namespace", &cfg.fields.namespace, DEFAULT_NAMESPACE_FIELD),
        pod: field("pod", &cfg.fields.pod, DEFAULT_POD_FIELD),
        container: field("container", &cfg.fields.container, DEFAULT_CONTAINER_FIELD),
    };

    // Any explicitly named field pins the whole mapping (the others fall to
    // the vector defaults); with none named, the mapping is detected against
    // the backend on first use.
    let fields_configured = cfg.fields.namespace.is_some()
        || cfg.fields.pod.is_some()
        || cfg.fields.container.is_some();

    let provider = LogProvider {
        transport,
        headers,
        fields,
        fields_configured,
        lookback_secs,
        lookback_label,
        limit: cfg.limit.unwrap_or(DEFAULT_LIMIT).max(1),
    };
    (Some(provider), warnings)
}

/// Stream-field naming conventions of the common log shippers, as
/// `(namespace, pod, container)`. Ordered: the vector convention (the
/// VictoriaLogs-documented default) first, the bare fallback last.
const FIELD_CONVENTIONS: &[(&str, &str, &str)] = &[
    // vector (VictoriaLogs Kubernetes docs)
    (
        DEFAULT_NAMESPACE_FIELD,
        DEFAULT_POD_FIELD,
        DEFAULT_CONTAINER_FIELD,
    ),
    // fluentd kubernetes_metadata filter
    (
        "kubernetes.namespace_name",
        "kubernetes.pod_name",
        "kubernetes.container_name",
    ),
    // fluent-bit kubernetes filter, flattened
    (
        "kubernetes_namespace_name",
        "kubernetes_pod_name",
        "kubernetes_container_name",
    ),
    (
        "kubernetes_namespace",
        "kubernetes_pod_name",
        "kubernetes_container_name",
    ),
    // OpenTelemetry collector k8sattributes
    ("k8s.namespace.name", "k8s.pod.name", "k8s.container.name"),
    // promtail/alloy-style bare labels
    ("namespace", "pod", "container"),
];

/// Match the backend's stream-field names against the known shipper
/// conventions; all three fields must be present.
fn pick_fields(names: &std::collections::HashSet<String>) -> Option<Fields> {
    FIELD_CONVENTIONS
        .iter()
        .find(|(ns, pod, container)| {
            names.contains(*ns) && names.contains(*pod) && names.contains(*container)
        })
        .map(|(ns, pod, container)| Fields {
            namespace: (*ns).into(),
            pod: (*pod).into(),
            container: (*container).into(),
        })
}

/// Label selectors that identify a VictoriaLogs query endpoint across the
/// common install methods: the `victoria-logs-single` Helm chart, the cluster
/// chart (only its `vlselect` component serves queries), and the
/// VictoriaMetrics operator's log CRDs.
const DISCOVERY_SELECTORS: &[&str] = &[
    "app.kubernetes.io/name=victoria-logs-single",
    "app.kubernetes.io/name=victoria-logs-cluster,app.kubernetes.io/component=vlselect",
    "app.kubernetes.io/name=vlsingle",
    "app.kubernetes.io/name=vlselect",
    "app.kubernetes.io/name=vlogs",
];

/// Find a VictoriaLogs `Service` in the cluster and return `base` with its
/// transport resolved to the API-server proxy for that service. Tries each
/// well-known label selector in order; inside one selector the first service
/// by namespace/name wins (deterministic across runs).
pub async fn discover(client: kube::Client, base: &LogProvider) -> Result<LogProvider, String> {
    let api: Api<Service> = Api::all(client.clone());
    for selector in DISCOVERY_SELECTORS {
        let list = api
            .list(&ListParams::default().labels(selector))
            .await
            .map_err(|e| format!("discovering VictoriaLogs services: {e}"))?
            .items;
        if let Some((ns, service, port)) = pick_service(&list) {
            let mut provider = base.clone();
            provider.transport = Transport::ServiceProxy {
                client,
                ns,
                service,
                port,
            };
            return Ok(provider);
        }
    }
    Err(
        "no VictoriaLogs service found in the cluster — set [providers.logs] url in config.toml"
            .into(),
    )
}

/// The first (by namespace/name) service with a usable port. Ports named
/// `http` win, then literal 9428 (the VictoriaLogs default), then the first
/// declared port.
fn pick_service(services: &[Service]) -> Option<(String, String, i32)> {
    let mut candidates: Vec<(&Service, i32)> = services
        .iter()
        .filter_map(|s| {
            let ports = s.spec.as_ref()?.ports.as_ref()?;
            let port = ports
                .iter()
                .find(|p| p.name.as_deref() == Some("http"))
                .or_else(|| ports.iter().find(|p| p.port == 9428))
                .or_else(|| ports.first())?;
            Some((s, port.port))
        })
        .collect();
    candidates.sort_by_key(|(s, _)| {
        (
            s.metadata.namespace.clone().unwrap_or_default(),
            s.metadata.name.clone().unwrap_or_default(),
        )
    });
    let (svc, port) = candidates.first()?;
    Some((
        svc.metadata.namespace.clone().unwrap_or_default(),
        svc.metadata.name.clone().unwrap_or_default(),
        *port,
    ))
}

/// `"90s"` / `"15m"` / `"1h"` / `"2d"` (bare numbers are seconds).
pub(crate) fn parse_lookback(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like \"30m\" or \"1h\""))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return Err(format!("{s:?} has unknown unit {unit:?} (use s/m/h/d)")),
    };
    if secs <= 0 {
        return Err(format!("{s:?} must be a positive duration"));
    }
    Ok(secs)
}

/// LogsQL field names we emit unquoted into filters.
fn valid_field_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Quote a value for a LogsQL exact-match filter.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

impl LogProvider {
    /// The LogsQL filter expression selecting `scope`, without pipes. Stream
    /// filters (`{...}`) keep the query on the narrow set of per-pod streams
    /// instead of scanning the whole time range.
    fn filter_expr(&self, scope: &LogScope) -> String {
        let f = &self.fields;
        match scope {
            LogScope::Pod {
                ns,
                pod,
                container: None,
            } => format!("{{{}={},{}={}}}", f.namespace, quote(ns), f.pod, quote(pod)),
            LogScope::Pod {
                ns,
                pod,
                container: Some(c),
            } => format!(
                "{{{}={},{}={},{}={}}}",
                f.namespace,
                quote(ns),
                f.pod,
                quote(pod),
                f.container,
                quote(c)
            ),
            LogScope::Pods { ns, pods } => {
                let list = pods.iter().map(|p| quote(p)).collect::<Vec<_>>().join(",");
                format!("{{{}={}}} {}:in({})", f.namespace, quote(ns), f.pod, list)
            }
            LogScope::Namespace { ns } => format!("{{{}={}}}", f.namespace, quote(ns)),
        }
    }

    /// Whether the transport still has to be [`discover`]ed in the cluster.
    pub fn needs_discovery(&self) -> bool {
        matches!(self.transport, Transport::Auto)
    }

    /// Change the lookback window (already validated via [`parse_lookback`]).
    pub fn set_lookback(&mut self, secs: i64, label: String) {
        self.lookback_secs = secs;
        self.lookback_label = label;
    }

    /// Whether the field mapping should be [`detect_fields`](Self::detect_fields)-ed
    /// before the first query (no explicit `[providers.logs.fields]`).
    pub fn needs_field_detection(&self) -> bool {
        !self.fields_configured
    }

    /// The active `namespace/pod/container` field names, for messages.
    pub fn field_names(&self) -> String {
        format!(
            "{}/{}/{}",
            self.fields.namespace, self.fields.pod, self.fields.container
        )
    }

    /// Ask the backend which stream fields exist and match them against the
    /// known shipper conventions. `Ok(Some)` returns a provider with the
    /// detected mapping pinned (so it won't re-detect); `Ok(None)` means no
    /// convention matched.
    pub async fn detect_fields(&self) -> Result<Option<LogProvider>, String> {
        let now = Timestamp::now();
        let start = Timestamp::from_second(now.as_second() - self.lookback_secs)
            .unwrap_or(Timestamp::UNIX_EPOCH);
        let params = [("query", "*"), ("start", &start.to_string())];
        let fut = self.fetch_text("/select/logsql/stream_field_names", &params);
        let text = tokio::time::timeout(std::time::Duration::from_secs(QUERY_TIMEOUT_SECS), fut)
            .await
            .map_err(|_| format!("field detection timed out after {QUERY_TIMEOUT_SECS}s"))??;

        let names: std::collections::HashSet<String> = serde_json::from_str::<Value>(&text)
            .map_err(|e| format!("field detection: unexpected response: {e}"))?
            .get("values")
            .and_then(Value::as_array)
            .map(|vals| {
                vals.iter()
                    .filter_map(|v| v.get("value").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(pick_fields(&names).map(|fields| {
            let mut p = self.clone();
            p.fields = fields;
            p.fields_configured = true;
            p
        }))
    }

    /// Where requests go, for titles and messages — the configured URL or the
    /// discovered service.
    pub fn location(&self) -> String {
        match &self.transport {
            Transport::Direct { url } => url.clone(),
            Transport::ServiceProxy {
                ns, service, port, ..
            } => format!("{ns}/{service}:{port} (API-server proxy)"),
            Transport::Auto => "(undiscovered)".into(),
        }
    }

    /// Fetch the most recent `limit` entries for `scope` within the lookback
    /// window, oldest first.
    pub async fn query(&self, scope: &LogScope) -> Result<Vec<LogEntry>, String> {
        let now = Timestamp::now();
        let start = Timestamp::from_second(now.as_second() - self.lookback_secs)
            .unwrap_or(Timestamp::UNIX_EPOCH);
        // `sort desc | limit N` is VictoriaLogs' top-N shape: the newest N
        // entries, cheaply. Reversed below so the buffer reads oldest-first.
        let query = format!(
            "{} | sort by (_time) desc | limit {}",
            self.filter_expr(scope),
            self.limit
        );
        let params = [
            ("query", query.as_str()),
            ("start", &start.to_string()),
            ("end", &now.to_string()),
        ];

        let fut = self.fetch_text("/select/logsql/query", &params);
        let text = tokio::time::timeout(std::time::Duration::from_secs(QUERY_TIMEOUT_SECS), fut)
            .await
            .map_err(|_| format!("query timed out after {QUERY_TIMEOUT_SECS}s"))??;

        let mut entries: Vec<LogEntry> = text
            .lines()
            .filter_map(|l| parse_entry(l, &self.fields))
            .collect();
        entries.reverse();
        Ok(entries)
    }

    /// Open the live tail stream for `scope`. Entries arrive as the backend
    /// flushes them; poll with [`LogTail::next_entry`].
    pub async fn tail(&self, scope: &LogScope) -> Result<LogTail, String> {
        let query = self.filter_expr(scope);
        let params = [("query", query.as_str())];
        let source = match &self.transport {
            Transport::Direct { url } => {
                let resp = self
                    .post_direct(url, "/select/logsql/tail", &params)
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp
                        .into_body()
                        .collect()
                        .await
                        .map(|b| b.to_bytes())
                        .unwrap_or_default();
                    return Err(http_error(status, &body));
                }
                TailSource::Http {
                    body: resp.into_body(),
                    buf: Vec::new(),
                }
            }
            Transport::ServiceProxy { client, .. } => {
                let req = self.proxy_request("/select/logsql/tail", &params)?;
                let stream = client
                    .request_stream(req)
                    .await
                    .map_err(|e| format!("{}: {e}", self.location()))?;
                let boxed: std::pin::Pin<Box<dyn futures_util::io::AsyncBufRead + Send>> =
                    Box::pin(stream);
                TailSource::Proxy {
                    lines: boxed.lines(),
                }
            }
            Transport::Auto => return Err("provider not discovered yet".into()),
        };
        Ok(LogTail {
            source,
            pending: std::collections::VecDeque::new(),
            fields: self.fields.clone(),
        })
    }

    /// One-shot POST returning the whole response body as text.
    async fn fetch_text(&self, path: &str, params: &[(&str, &str)]) -> Result<String, String> {
        match &self.transport {
            Transport::Direct { url } => {
                let resp = self.post_direct(url, path, params).await?;
                let status = resp.status();
                let body = resp
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| format!("reading response: {e}"))?
                    .to_bytes();
                if !status.is_success() {
                    return Err(http_error(status, &body));
                }
                Ok(String::from_utf8_lossy(&body).into_owned())
            }
            Transport::ServiceProxy { client, .. } => {
                let req = self.proxy_request(path, params)?;
                client
                    .request_text(req)
                    .await
                    .map_err(|e| format!("{}: {e}", self.location()))
            }
            Transport::Auto => Err("provider not discovered yet".into()),
        }
    }

    /// Direct-transport POST via a one-off hyper client.
    async fn post_direct(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|e| format!("loading system TLS roots: {e}"))?
            .https_or_http()
            .enable_http1()
            .build();
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https);

        let url = format!("{base}{path}");
        let mut req = hyper::Request::post(&url).header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        );
        for (name, value) in &self.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        let req = req
            .body(Full::new(Bytes::from(form_body(params))))
            .map_err(|e| format!("building request: {e}"))?;
        client.request(req).await.map_err(|e| format!("{url}: {e}"))
    }

    /// Build a POST request for the API-server service proxy (a path-only
    /// URI: the kube client resolves it against the cluster's base URL and
    /// injects authentication).
    fn proxy_request(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<http::Request<Vec<u8>>, String> {
        let Transport::ServiceProxy {
            ns, service, port, ..
        } = &self.transport
        else {
            return Err("provider not discovered yet".into());
        };
        let uri = format!("/api/v1/namespaces/{ns}/services/{service}:{port}/proxy{path}");
        let mut req = http::Request::post(uri).header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        );
        for (name, value) in &self.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        req.body(form_body(params).into_bytes())
            .map_err(|e| format!("building request: {e}"))
    }
}

/// Form-urlencode `params`. Scoped helper: the serializer holds a non-`Send`
/// encoder, so it must never be held across an await.
fn form_body(params: &[(&str, &str)]) -> String {
    let mut form = form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        form.append_pair(k, v);
    }
    form.finish()
}

fn http_error(status: http::StatusCode, body: &[u8]) -> String {
    let detail: String = String::from_utf8_lossy(body)
        .trim()
        .chars()
        .take(200)
        .collect();
    if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    }
}

/// A live `/select/logsql/tail` stream: a long-lived HTTP body of JSON lines,
/// buffered so a record split across chunks stays whole.
pub struct LogTail {
    source: TailSource,
    pending: std::collections::VecDeque<LogEntry>,
    fields: Fields,
}

enum TailSource {
    /// Direct transport: raw hyper body, split into lines here.
    Http {
        body: hyper::body::Incoming,
        buf: Vec<u8>,
    },
    /// API-server proxy transport: the kube client already yields a buffered
    /// reader over the response body.
    Proxy {
        lines:
            futures_util::io::Lines<std::pin::Pin<Box<dyn futures_util::io::AsyncBufRead + Send>>>,
    },
}

impl LogTail {
    /// The next entry, `Ok(None)` when the backend closes the stream.
    /// Cancel-safe (usable in `select!`): dropping the future mid-poll only
    /// leaves partial data buffered for the next call.
    pub async fn next_entry(&mut self) -> Result<Option<LogEntry>, String> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Ok(Some(e));
            }
            match &mut self.source {
                TailSource::Http { body, buf } => match body.frame().await {
                    None => {
                        let rest = std::mem::take(buf);
                        return Ok(std::str::from_utf8(&rest)
                            .ok()
                            .and_then(|l| parse_entry(l.trim(), &self.fields)));
                    }
                    Some(Err(e)) => return Err(e.to_string()),
                    Some(Ok(frame)) => {
                        if let Some(data) = frame.data_ref() {
                            buf.extend_from_slice(data);
                            self.pending.extend(
                                drain_lines(buf)
                                    .iter()
                                    .filter_map(|l| parse_entry(l, &self.fields)),
                            );
                        }
                    }
                },
                TailSource::Proxy { lines } => match lines.next().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(e.to_string()),
                    Some(Ok(line)) => {
                        if let Some(e) = parse_entry(&line, &self.fields) {
                            self.pending.push_back(e);
                        }
                    }
                },
            }
        }
    }
}

/// Split complete `\n`-terminated lines out of `buf`, leaving any trailing
/// partial line in place for the next chunk.
fn drain_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let complete: Vec<u8> = buf.drain(..=last_nl).collect();
    String::from_utf8_lossy(&complete)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Parse one JSON-line record into a [`LogEntry`] using the configured field
/// names. Records without a `_msg` (e.g. keep-alives) are dropped.
fn parse_entry(line: &str, fields: &Fields) -> Option<LogEntry> {
    let v: Value = serde_json::from_str(line).ok()?;
    let msg = v.get("_msg")?.as_str()?.trim_end_matches('\n').to_string();
    let time = v
        .get("_time")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let nanos = time.parse::<Timestamp>().ok().map(|t| t.as_nanosecond());
    let field = |name: &str| {
        v.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(LogEntry {
        nanos,
        msg,
        pod: field(&fields.pod),
        container: field(&fields.container),
        time,
    })
}

// ===== metrics provider (right-sizing) =====================================

/// Label selectors that identify a Prometheus-compatible query service across
/// common installs: the VictoriaMetrics k8s-stack / operator (`vmsingle`), the
/// single-node Helm chart, and kube-prometheus-stack Prometheus.
const METRICS_DISCOVERY_SELECTORS: &[&str] = &[
    "app.kubernetes.io/name=vmsingle",
    "app.kubernetes.io/name=victoria-metrics-single",
    "app.kubernetes.io/name=prometheus",
    "operated-prometheus=true",
];

pub const DEFAULT_METRICS_WINDOW: &str = "7d";
const DEFAULT_METRICS_STEP: &str = "5m";
const DEFAULT_HEADROOM: u32 = 15;

/// A compiled Prometheus-compatible metrics backend for `:rightsize`. Reaches
/// the query API at `/api/v1/query` over the same transports as the log
/// provider (direct URL or the API-server service proxy).
#[derive(Clone)]
pub struct MetricsProvider {
    transport: Transport,
    headers: Vec<(String, String)>,
    /// Quantile lookback (`"7d"`), verbatim for the PromQL and titles.
    pub window: String,
    /// Subquery resolution for the CPU `rate()` (`"5m"`).
    pub step: String,
    /// Percent headroom added over P95 for the suggested request.
    pub headroom: u32,
}

impl Default for MetricsProvider {
    fn default() -> Self {
        Self {
            transport: Transport::Auto,
            headers: Vec::new(),
            window: DEFAULT_METRICS_WINDOW.into(),
            step: DEFAULT_METRICS_STEP.into(),
            headroom: DEFAULT_HEADROOM,
        }
    }
}

/// Validate `[providers.metrics]` into a [`MetricsProvider`]. An omitted/empty
/// url means "autodiscover in-cluster" (resolved on first use). Accepts
/// `prometheus` and `victoriametrics` (the same query API).
pub fn compile_metrics(
    cfg: Option<&crate::config::MetricsProviderConfig>,
) -> (Option<MetricsProvider>, Vec<String>) {
    let Some(cfg) = cfg else {
        return (None, Vec::new());
    };
    let mut warnings = Vec::new();
    match cfg.kind.as_str() {
        "prometheus" | "victoriametrics" => {}
        "" => {
            warnings.push(
                "providers.metrics: missing `type` (expected \"prometheus\" or \"victoriametrics\")"
                    .into(),
            );
            return (None, warnings);
        }
        other => {
            warnings.push(format!(
                "providers.metrics: unsupported type {other:?} (expected \"prometheus\"/\"victoriametrics\")"
            ));
            return (None, warnings);
        }
    }

    let url = cfg.url.trim().trim_end_matches('/').to_string();
    let transport = if url.is_empty() {
        Transport::Auto
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Transport::Direct { url }
    } else {
        warnings.push(format!(
            "providers.metrics: url {:?} must start with http:// or https:// (or be omitted for autodiscovery)",
            cfg.url
        ));
        return (None, warnings);
    };

    let window = cfg
        .window
        .clone()
        .unwrap_or_else(|| DEFAULT_METRICS_WINDOW.into());
    if let Err(e) = parse_lookback(&window) {
        warnings.push(format!("providers.metrics: window: {e}"));
        return (None, warnings);
    }
    let step = cfg
        .step
        .clone()
        .unwrap_or_else(|| DEFAULT_METRICS_STEP.into());
    if let Err(e) = parse_lookback(&step) {
        warnings.push(format!("providers.metrics: step: {e}"));
        return (None, warnings);
    }

    (
        Some(MetricsProvider {
            transport,
            headers: cfg
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            window,
            step,
            headroom: cfg.headroom.unwrap_or(DEFAULT_HEADROOM),
        }),
        warnings,
    )
}

/// Find a Prometheus/VictoriaMetrics query `Service` and resolve `base`'s
/// transport to the API-server proxy for it.
pub async fn discover_metrics(
    client: kube::Client,
    base: &MetricsProvider,
) -> Result<MetricsProvider, String> {
    let api: Api<Service> = Api::all(client.clone());
    for selector in METRICS_DISCOVERY_SELECTORS {
        let list = api
            .list(&ListParams::default().labels(selector))
            .await
            .map_err(|e| format!("discovering metrics services: {e}"))?
            .items;
        if let Some((ns, service, port)) = pick_metrics_service(&list) {
            let mut provider = base.clone();
            provider.transport = Transport::ServiceProxy {
                client,
                ns,
                service,
                port,
            };
            return Ok(provider);
        }
    }
    Err(
        "no Prometheus/VictoriaMetrics service found — set [providers.metrics] url in config.toml"
            .into(),
    )
}

/// First (by namespace/name) service with a usable port, preferring `http`,
/// then the well-known query ports (Prometheus 9090, VM single 8428/8429).
fn pick_metrics_service(services: &[Service]) -> Option<(String, String, i32)> {
    let mut candidates: Vec<(&Service, i32)> = services
        .iter()
        .filter_map(|s| {
            let ports = s.spec.as_ref()?.ports.as_ref()?;
            let port = ports
                .iter()
                .find(|p| p.name.as_deref() == Some("http"))
                .or_else(|| ports.iter().find(|p| matches!(p.port, 9090 | 8428 | 8429)))
                .or_else(|| ports.first())?;
            Some((s, port.port))
        })
        .collect();
    candidates.sort_by_key(|(s, _)| {
        (
            s.metadata.namespace.clone().unwrap_or_default(),
            s.metadata.name.clone().unwrap_or_default(),
        )
    });
    let (svc, port) = candidates.first()?;
    Some((
        svc.metadata.namespace.clone().unwrap_or_default(),
        svc.metadata.name.clone().unwrap_or_default(),
        *port,
    ))
}

impl MetricsProvider {
    /// Whether the transport still needs [`discover_metrics`].
    pub fn needs_discovery(&self) -> bool {
        matches!(self.transport, Transport::Auto)
    }

    /// A human-readable description of where queries go, for messages.
    pub fn location(&self) -> String {
        match &self.transport {
            Transport::Direct { url } => url.clone(),
            Transport::ServiceProxy {
                ns, service, port, ..
            } => format!("{ns}/{service}:{port} (API-server proxy)"),
            Transport::Auto => "autodiscover".into(),
        }
    }

    /// Run one instant query, returning the first sample value (`None` = no
    /// data). Bounded by [`QUERY_TIMEOUT_SECS`].
    pub async fn query(&self, promql: &str) -> Result<Option<f64>, String> {
        let fut = self.fetch_query(promql);
        let body = tokio::time::timeout(std::time::Duration::from_secs(QUERY_TIMEOUT_SECS), fut)
            .await
            .map_err(|_| format!("query timed out after {QUERY_TIMEOUT_SECS}s"))??;
        Ok(crate::rightsize::scalar_from_query(&body))
    }

    /// POST `query=<promql>` to `/api/v1/query` over the active transport.
    async fn fetch_query(&self, promql: &str) -> Result<String, String> {
        let params = [("query", promql)];
        match &self.transport {
            Transport::ServiceProxy {
                client,
                ns,
                service,
                port,
            } => {
                let uri =
                    format!("/api/v1/namespaces/{ns}/services/{service}:{port}/proxy/api/v1/query");
                let mut req = http::Request::post(uri).header(
                    http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                );
                for (name, value) in &self.headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                let req = req
                    .body(form_body(&params).into_bytes())
                    .map_err(|e| format!("building request: {e}"))?;
                client
                    .request_text(req)
                    .await
                    .map_err(|e| format!("{}: {e}", self.location()))
            }
            Transport::Direct { url } => {
                let https = hyper_rustls::HttpsConnectorBuilder::new()
                    .with_native_roots()
                    .map_err(|e| format!("loading system TLS roots: {e}"))?
                    .https_or_http()
                    .enable_http1()
                    .build();
                let http_client = hyper_util::client::legacy::Client::builder(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .build(https);
                let full = format!("{url}/api/v1/query");
                let mut req = hyper::Request::post(&full).header(
                    http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                );
                for (name, value) in &self.headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                let req = req
                    .body(Full::new(Bytes::from(form_body(&params))))
                    .map_err(|e| format!("building request: {e}"))?;
                let resp = http_client
                    .request(req)
                    .await
                    .map_err(|e| format!("{full}: {e}"))?;
                let status = resp.status();
                let body = resp
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| format!("reading response: {e}"))?
                    .to_bytes();
                if !status.is_success() {
                    return Err(http_error(status, &body));
                }
                Ok(String::from_utf8_lossy(&body).into_owned())
            }
            Transport::Auto => Err("metrics provider not discovered yet".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogProviderConfig;

    fn cfg(toml: &str) -> LogProviderConfig {
        toml::from_str(toml).unwrap()
    }

    fn provider() -> LogProvider {
        let (p, w) = compile(Some(&cfg(
            "type = \"victorialogs\"\nurl = \"https://vlogs.example.com\"",
        )));
        assert!(w.is_empty(), "{w:?}");
        p.unwrap()
    }

    #[test]
    fn compile_defaults() {
        let p = provider();
        assert_eq!(p.location(), "https://vlogs.example.com");
        assert!(!p.needs_discovery());
        assert_eq!(p.fields.namespace, DEFAULT_NAMESPACE_FIELD);
        assert_eq!(p.fields.pod, DEFAULT_POD_FIELD);
        assert_eq!(p.fields.container, DEFAULT_CONTAINER_FIELD);
        assert_eq!(p.lookback_secs, 3600);
        assert_eq!(p.lookback_label, "1h");
        assert_eq!(p.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn compile_without_url_wants_discovery() {
        let (p, w) = compile(Some(&cfg("type = \"victorialogs\"\nlookback = \"4h\"")));
        assert!(w.is_empty(), "{w:?}");
        let p = p.unwrap();
        assert!(p.needs_discovery());
        // Custom settings survive into the discovered provider.
        assert_eq!(p.lookback_secs, 4 * 3600);
    }

    #[test]
    fn explicit_fields_pin_the_mapping_and_defaults_detect() {
        // No [fields]: detect against the backend on first use.
        let p = provider();
        assert!(p.needs_field_detection());

        // Any explicit field pins the whole mapping.
        let (p, w) = compile(Some(&cfg(
            "type = \"victorialogs\"\nurl = \"https://x\"\n[fields]\npod = \"pod\"",
        )));
        assert!(w.is_empty(), "{w:?}");
        assert!(!p.unwrap().needs_field_detection());
    }

    #[test]
    fn pick_fields_matches_shipper_conventions() {
        let set = |names: &[&str]| -> std::collections::HashSet<String> {
            names.iter().map(|s| s.to_string()).collect()
        };

        // vector (dotted) — the home-cluster shape.
        let f = pick_fields(&set(&[
            "kubernetes.pod_namespace",
            "kubernetes.pod_name",
            "kubernetes.container_name",
            "stream",
        ]))
        .unwrap();
        assert_eq!(f.namespace, "kubernetes.pod_namespace");

        // fluent-bit flattened underscores — the mailerlite-dev shape.
        let f = pick_fields(&set(&[
            "kubernetes_namespace",
            "kubernetes_pod_name",
            "kubernetes_container_name",
            "log_source_ident",
            "queue",
        ]))
        .unwrap();
        assert_eq!(f.namespace, "kubernetes_namespace");
        assert_eq!(f.pod, "kubernetes_pod_name");
        assert_eq!(f.container, "kubernetes_container_name");

        // OpenTelemetry semantic conventions.
        let f = pick_fields(&set(&[
            "k8s.namespace.name",
            "k8s.pod.name",
            "k8s.container.name",
        ]))
        .unwrap();
        assert_eq!(f.pod, "k8s.pod.name");

        // All three fields must exist; partial sets don't match.
        assert!(pick_fields(&set(&["kubernetes_pod_name", "stream"])).is_none());
        assert!(pick_fields(&set(&[])).is_none());
    }

    #[test]
    fn compile_full_config() {
        let (p, w) = compile(Some(&cfg(r#"
            type = "victorialogs"
            url = "http://localhost:9428/"
            lookback = "30m"
            limit = 1000
            [headers]
            Authorization = "Bearer abc"
            [fields]
            namespace = "namespace"
            pod = "pod"
            container = "container"
        "#)));
        assert!(w.is_empty(), "{w:?}");
        let p = p.unwrap();
        assert_eq!(p.location(), "http://localhost:9428"); // trailing slash stripped
        assert_eq!(p.lookback_secs, 1800);
        assert_eq!(p.limit, 1000);
        assert_eq!(
            p.headers,
            vec![("Authorization".into(), "Bearer abc".into())]
        );
        assert_eq!(p.fields.pod, "pod");
    }

    #[test]
    fn compile_rejects_unknown_type_and_bad_url() {
        let (p, w) = compile(Some(&cfg("type = \"loki\"\nurl = \"https://x\"")));
        assert!(p.is_none());
        assert!(w[0].contains("unsupported type"), "{w:?}");

        let (p, w) = compile(Some(&cfg(
            "type = \"victorialogs\"\nurl = \"vlogs.example.com\"",
        )));
        assert!(p.is_none());
        assert!(w[0].contains("http://"), "{w:?}");

        let (p, w) = compile(Some(&cfg("url = \"https://x\"")));
        assert!(p.is_none());
        assert!(w[0].contains("missing `type`"), "{w:?}");
    }

    fn svc(ns: &str, name: &str, ports: serde_json::Value) -> Service {
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "namespace": ns},
            "spec": {"ports": ports}
        }))
        .unwrap()
    }

    #[test]
    fn pick_service_prefers_http_port_then_9428() {
        // Named "http" wins over other ports.
        let s = svc(
            "monitoring",
            "vlogs",
            serde_json::json!([
                {"name": "metrics", "port": 8080},
                {"name": "http", "port": 9428}
            ]),
        );
        assert_eq!(
            pick_service(&[s]),
            Some(("monitoring".into(), "vlogs".into(), 9428))
        );

        // No named port: literal 9428 wins, else the first declared.
        let s = svc(
            "logs",
            "vl",
            serde_json::json!([{"port": 8080}, {"port": 9428}]),
        );
        assert_eq!(pick_service(&[s]), Some(("logs".into(), "vl".into(), 9428)));
        let s = svc("logs", "vl", serde_json::json!([{"port": 8080}]));
        assert_eq!(pick_service(&[s]), Some(("logs".into(), "vl".into(), 8080)));

        // Deterministic: first by namespace/name.
        let a = svc("b-ns", "svc", serde_json::json!([{"port": 9428}]));
        let b = svc("a-ns", "svc", serde_json::json!([{"port": 9428}]));
        assert_eq!(
            pick_service(&[a, b]),
            Some(("a-ns".into(), "svc".into(), 9428))
        );

        // A service without ports is unusable.
        let bare: Service = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "x", "namespace": "y"}
        }))
        .unwrap();
        assert_eq!(pick_service(&[bare]), None);
        assert_eq!(pick_service(&[]), None);
    }

    #[tokio::test]
    async fn proxy_request_targets_the_service_proxy() {
        let mut p = provider();
        p.transport = Transport::ServiceProxy {
            client: crate::k8s::Cluster::fake().client,
            ns: "monitoring".into(),
            service: "vlogs".into(),
            port: 9428,
        };
        p.headers = vec![("X-Scope".into(), "tenant-1".into())];
        assert_eq!(p.location(), "monitoring/vlogs:9428 (API-server proxy)");

        let req = p
            .proxy_request("/select/logsql/query", &[("query", "{a=\"b\"}")])
            .unwrap();
        assert_eq!(
            req.uri(),
            "/api/v1/namespaces/monitoring/services/vlogs:9428/proxy/select/logsql/query"
        );
        assert_eq!(
            req.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(req.headers().get("X-Scope").unwrap(), "tenant-1");
        assert_eq!(
            String::from_utf8(req.body().clone()).unwrap(),
            "query=%7Ba%3D%22b%22%7D"
        );
    }

    #[test]
    fn compile_warns_on_bad_lookback_header_and_field() {
        let (p, w) = compile(Some(&cfg(
            "type = \"victorialogs\"\nurl = \"https://x\"\nlookback = \"soon\"",
        )));
        assert!(p.is_none());
        assert!(w[0].contains("lookback"), "{w:?}");

        let (p, w) = compile(Some(&cfg(r#"
            type = "victorialogs"
            url = "https://x"
            [headers]
            "bad header" = "v"
            [fields]
            pod = "has space"
        "#)));
        let p = p.unwrap(); // degraded, still usable
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(p.headers.is_empty());
        assert_eq!(p.fields.pod, DEFAULT_POD_FIELD);
    }

    #[test]
    fn parse_lookback_units() {
        assert_eq!(parse_lookback("90s"), Ok(90));
        assert_eq!(parse_lookback("15m"), Ok(900));
        assert_eq!(parse_lookback("2h"), Ok(7200));
        assert_eq!(parse_lookback("1d"), Ok(86_400));
        assert_eq!(parse_lookback("45"), Ok(45)); // bare seconds
        assert!(parse_lookback("0m").is_err());
        assert!(parse_lookback("1w").is_err());
        assert!(parse_lookback("h").is_err());
    }

    #[test]
    fn filter_expr_per_scope() {
        let p = provider();
        assert_eq!(
            p.filter_expr(&LogScope::Pod {
                ns: "prod".into(),
                pod: "api-1".into(),
                container: None,
            }),
            r#"{kubernetes.pod_namespace="prod",kubernetes.pod_name="api-1"}"#
        );
        assert_eq!(
            p.filter_expr(&LogScope::Pod {
                ns: "prod".into(),
                pod: "api-1".into(),
                container: Some("istio".into()),
            }),
            r#"{kubernetes.pod_namespace="prod",kubernetes.pod_name="api-1",kubernetes.container_name="istio"}"#
        );
        assert_eq!(
            p.filter_expr(&LogScope::Pods {
                ns: "prod".into(),
                pods: vec!["api-1".into(), "api-2".into()],
            }),
            r#"{kubernetes.pod_namespace="prod"} kubernetes.pod_name:in("api-1","api-2")"#
        );
        assert_eq!(
            p.filter_expr(&LogScope::Namespace { ns: "prod".into() }),
            r#"{kubernetes.pod_namespace="prod"}"#
        );
    }

    #[test]
    fn quote_escapes_logsql_values() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r"a\b"), r#""a\\b""#);
    }

    fn fields() -> Fields {
        Fields {
            namespace: DEFAULT_NAMESPACE_FIELD.into(),
            pod: DEFAULT_POD_FIELD.into(),
            container: DEFAULT_CONTAINER_FIELD.into(),
        }
    }

    #[test]
    fn parse_entry_extracts_mapped_fields() {
        let line = r#"{"_time":"2026-07-13T06:41:47.879985929Z","_msg":"hello","kubernetes.pod_name":"api-1","kubernetes.container_name":"app"}"#;
        let e = parse_entry(line, &fields()).unwrap();
        assert_eq!(e.msg, "hello");
        assert_eq!(e.pod, "api-1");
        assert_eq!(e.container, "app");
        assert_eq!(e.time, "2026-07-13T06:41:47.879985929Z");
        assert!(e.nanos.is_some());

        // Records without _msg (keep-alives) and garbage are dropped.
        assert!(parse_entry(r#"{"_time":"x"}"#, &fields()).is_none());
        assert!(parse_entry("not json", &fields()).is_none());
    }

    #[test]
    fn entry_lines_prefix_and_timestamps() {
        let e = LogEntry {
            time: "2026-07-13T06:41:47Z".into(),
            nanos: None,
            msg: "one\ntwo".into(),
            pod: "api-1".into(),
            container: "app".into(),
        };
        assert_eq!(e.lines(Prefix::None, false), vec!["one", "two"]);
        assert_eq!(
            e.lines(Prefix::Container, false),
            vec!["[app] one", "[app] two"]
        );
        assert_eq!(
            e.lines(Prefix::PodContainer, true),
            vec![
                "[api-1:app] 2026-07-13T06:41:47Z one",
                "[api-1:app] 2026-07-13T06:41:47Z two"
            ]
        );
        // Missing tag fields degrade instead of rendering "[]".
        let bare = LogEntry {
            container: String::new(),
            ..e
        };
        assert_eq!(bare.lines(Prefix::Container, false), vec!["one", "two"]);
        assert_eq!(
            bare.lines(Prefix::PodContainer, false),
            vec!["[api-1] one", "[api-1] two"]
        );
    }

    #[test]
    fn drain_lines_keeps_partial_tail() {
        let mut buf = b"{\"a\":1}\n{\"b\":2}\n{\"part".to_vec();
        assert_eq!(drain_lines(&mut buf), vec!["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(buf, b"{\"part");
        // No newline yet — nothing complete.
        assert!(drain_lines(&mut buf).is_empty());
        buf.extend_from_slice(b"\":3}\n");
        assert_eq!(drain_lines(&mut buf), vec!["{\"part\":3}"]);
        assert!(buf.is_empty());
    }

    /// Live test against a real VictoriaLogs — opt-in:
    /// `SOFKA_TEST_VLOGS_URL=https://vlogs.example.com cargo test -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn e2e_victorialogs_query_and_tail() {
        let url = std::env::var("SOFKA_TEST_VLOGS_URL").expect("SOFKA_TEST_VLOGS_URL not set");
        let ns = std::env::var("SOFKA_TEST_VLOGS_NS").unwrap_or_else(|_| "monitoring".into());
        let (p, w) = compile(Some(&cfg(&format!(
            "type = \"victorialogs\"\nurl = \"{url}\"\nlookback = \"15m\""
        ))));
        assert!(w.is_empty(), "{w:?}");
        let p = p.unwrap();
        let scope = LogScope::Namespace { ns };

        let entries = p.query(&scope).await.unwrap();
        assert!(!entries.is_empty(), "no log entries in the last 15m");
        assert!(entries.iter().all(|e| e.nanos.is_some()));
        // Oldest first.
        let times: Vec<i128> = entries.iter().filter_map(|e| e.nanos).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));

        // Generous window: the tail only yields once something in the
        // namespace actually logs.
        let mut tail = p.tail(&scope).await.unwrap();
        let entry = tokio::time::timeout(std::time::Duration::from_secs(120), tail.next_entry())
            .await
            .expect("no tail data within 120s")
            .unwrap()
            .expect("tail closed immediately");
        assert!(!entry.msg.is_empty());
    }
}
