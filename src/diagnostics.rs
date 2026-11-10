//! Runtime diagnostics: the facts `sofka info` and `:info` both report.
//!
//! Three things live here. The **build stamp and directories** — what sofka
//! is and where it keeps things. The **request-latency registry** — a
//! process-wide histogram every Kubernetes API request feeds through the tower
//! middleware in [`crate::k8s`], so "the cluster feels slow" becomes a number.
//! And the **shared report sections**, so the headless report and the in-app
//! view cannot drift apart.
//!
//! Nothing here touches the cluster, and nothing here emits a credential:
//! every value that could carry one (an API server URL with userinfo, an error
//! string echoing a request) goes through [`crate::redact::text`] first.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build target as `os/arch` plus the compile profile.
pub fn build_line() -> String {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "{}/{} ({profile})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Base state directory: `$XDG_STATE_HOME/sofka`, else `~/.local/state/sofka`,
/// else a temp fallback. Snapshots, logs, and the small UI-state files live
/// under it.
pub fn state_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_STATE_HOME")
        && !x.is_empty()
    {
        return PathBuf::from(x).join("sofka");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("sofka");
    }
    std::env::temp_dir().join("sofka")
}

/// Where the structured application log is written.
pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Default log file (`<state-dir>/logs/sofka.log`).
pub fn default_log_path() -> PathBuf {
    log_dir().join("sofka.log")
}

/// Where diagnostic bundles are written.
pub fn bundle_dir() -> PathBuf {
    std::env::temp_dir()
}

// ---------------------------------------------------------------------------
// Request latency
// ---------------------------------------------------------------------------

/// The classes of Kubernetes API request worth timing separately. A watch that
/// takes 400ms to establish and a list that takes 400ms mean different things,
/// so they never share a bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// API group/version discovery and `/version`.
    Discovery,
    /// A `GET` that is not a watch: list, get, CRD fetch.
    Read,
    /// Watch establishment (time to response headers — the stream that follows
    /// is open for as long as the view is).
    Watch,
    /// Anything mutating: create, patch, delete, eviction, subject access
    /// reviews.
    Write,
    /// `metrics.k8s.io` polls.
    Metrics,
    /// Pod log streams.
    Logs,
    /// Upgraded connections: exec, attach, port-forward.
    Exec,
}

impl Op {
    pub const ALL: [Op; 7] = [
        Op::Discovery,
        Op::Read,
        Op::Watch,
        Op::Write,
        Op::Metrics,
        Op::Logs,
        Op::Exec,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Discovery => "discovery",
            Op::Read => "read",
            Op::Watch => "watch",
            Op::Write => "write",
            Op::Metrics => "metrics",
            Op::Logs => "logs",
            Op::Exec => "exec",
        }
    }

    fn index(self) -> usize {
        match self {
            Op::Discovery => 0,
            Op::Read => 1,
            Op::Watch => 2,
            Op::Write => 3,
            Op::Metrics => 4,
            Op::Logs => 5,
            Op::Exec => 6,
        }
    }

    /// Classify a request from its method and URI alone — one pass over the
    /// path, no allocation, no header inspection. On the path of every API
    /// call, so it stays branches over borrowed segments.
    ///
    /// A discovery request *for* the metrics group counts as `metrics`: when
    /// metrics-server is wedged, the useful attribution is the API it belongs
    /// to, not the phase it happened in.
    pub fn classify(method: &http::Method, path: &str, query: Option<&str>) -> Op {
        let (mut first, mut second, mut last) = ("", "", "");
        let mut segments = 0usize;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            match segments {
                0 => first = segment,
                1 => second = segment,
                _ => {}
            }
            last = segment;
            segments += 1;
        }
        match last {
            "exec" | "attach" | "portforward" => return Op::Exec,
            "log" => return Op::Logs,
            _ => {}
        }
        if first == "apis" && second == "metrics.k8s.io" {
            return Op::Metrics;
        }
        if query.is_some_and(|q| q.split('&').any(|kv| kv == "watch=true")) {
            return Op::Watch;
        }
        // `/version`, `/api`, `/api/v1`, `/apis`, `/apis/apps/v1`, `/openapi/…`
        // are the shapes discovery asks for; one more segment is a resource.
        let discovery = match first {
            "version" | "openapi" => true,
            "api" => segments <= 2,
            "apis" => segments <= 3,
            _ => false,
        };
        if discovery {
            return Op::Discovery;
        }
        match *method {
            http::Method::GET | http::Method::HEAD | http::Method::OPTIONS => Op::Read,
            _ => Op::Write,
        }
    }
}

/// Log2 latency buckets: bucket `k` holds requests below `2^k` microseconds,
/// so the last one covers everything past ~34 seconds. 26 `u32`s per class is
/// small enough to keep percentiles without a sampling reservoir or a lock.
const BUCKETS: usize = 26;

struct Stats {
    count: AtomicU64,
    errors: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
    buckets: [AtomicU32; BUCKETS],
}

impl Stats {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            buckets: [const { AtomicU32::new(0) }; BUCKETS],
        }
    }
}

static STATS: [Stats; Op::ALL.len()] = [const { Stats::new() }; Op::ALL.len()];

#[cfg(test)]
pub(crate) static LATENCY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Record one completed request. Four relaxed atomic adds and a max — cheap
/// enough to run unconditionally, so the numbers are there when someone asks
/// for them rather than only after turning something on.
pub fn record(op: Op, elapsed: Duration, ok: bool) {
    let stats = &STATS[op.index()];
    let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
    stats.count.fetch_add(1, Ordering::Relaxed);
    if !ok {
        stats.errors.fetch_add(1, Ordering::Relaxed);
    }
    stats.total_us.fetch_add(micros, Ordering::Relaxed);
    stats.max_us.fetch_max(micros, Ordering::Relaxed);
    stats.buckets[bucket(micros)].fetch_add(1, Ordering::Relaxed);
}

fn bucket(micros: u64) -> usize {
    ((u64::BITS - micros.leading_zeros()) as usize).min(BUCKETS - 1)
}

/// One request class's latency, in milliseconds.
pub struct OpSummary {
    pub op: Op,
    pub count: u64,
    /// Requests canceled before response headers, transport failures, and 5xx
    /// responses. A 4xx is the API server answering correctly — a missing CRD,
    /// an RBAC denial — and the UI surfaces those where they happen, so counting
    /// them here would make a healthy session read as a broken one.
    pub errors: u64,
    pub avg_ms: f64,
    /// Bucket upper bounds, so these read as "at most": exact percentiles
    /// would need every sample kept.
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub max_ms: f64,
}

/// Latency per request class, skipping classes with no requests yet.
pub fn latency_summary() -> Vec<OpSummary> {
    Op::ALL
        .into_iter()
        .filter_map(|op| {
            let stats = &STATS[op.index()];
            let count = stats.count.load(Ordering::Relaxed);
            if count == 0 {
                return None;
            }
            let total_us = stats.total_us.load(Ordering::Relaxed);
            let max_ms = stats.max_us.load(Ordering::Relaxed) as f64 / 1000.0;
            let counts: Vec<u64> = stats
                .buckets
                .iter()
                .map(|b| u64::from(b.load(Ordering::Relaxed)))
                .collect();
            Some(OpSummary {
                op,
                count,
                errors: stats.errors.load(Ordering::Relaxed),
                avg_ms: total_us as f64 / count as f64 / 1000.0,
                // Clamped to the observed maximum: a bucket's upper bound can
                // exceed every sample in it, and a table where P90 reads
                // higher than MAX looks like a bug rather than a rounding
                // rule.
                p50_ms: percentile_ms(&counts, count, 50).min(max_ms),
                p90_ms: percentile_ms(&counts, count, 90).min(max_ms),
                max_ms,
            })
        })
        .collect()
}

/// The upper bound of the bucket the `p`th percentile falls in, in ms.
fn percentile_ms(counts: &[u64], total: u64, p: u64) -> f64 {
    let target = total.saturating_mul(p).div_ceil(100).max(1);
    let mut seen = 0u64;
    for (k, n) in counts.iter().enumerate() {
        seen += n;
        if seen >= target {
            return (1u64 << k) as f64 / 1000.0;
        }
    }
    0.0
}

#[cfg(any(test, feature = "bench"))]
/// Clear every counter. Tests only — the registry is process-wide.
pub fn reset_latency() {
    for stats in &STATS {
        stats.count.store(0, Ordering::Relaxed);
        stats.errors.store(0, Ordering::Relaxed);
        stats.total_us.store(0, Ordering::Relaxed);
        stats.max_us.store(0, Ordering::Relaxed);
        for b in &stats.buckets {
            b.store(0, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared report sections
// ---------------------------------------------------------------------------

/// Render a value that may carry a credential (an API server URL with
/// userinfo, an error string that echoed a request).
pub fn safe(value: &str) -> Cow<'_, str> {
    crate::redact::text(value)
}

/// Prepare text for terminal output. Redact credentials and replace control
/// characters and bidirectional formatting characters with spaces. Keep newlines.
pub fn printable(value: &str) -> String {
    safe(value)
        .chars()
        .map(|c| {
            if (c.is_control() && c != '\n')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// `value`, or `fallback` when it is empty — after redaction.
pub fn safe_or<'a>(value: &'a str, fallback: &'a str) -> Cow<'a, str> {
    if value.is_empty() {
        Cow::Borrowed(fallback)
    } else {
        crate::redact::text(value)
    }
}

pub fn version_lines() -> Vec<String> {
    vec![
        format!("sofka v{VERSION}"),
        format!("  build: {}", build_line()),
    ]
}

/// Config files consulted for `context`/`cluster`, in merge order, each with
/// whether it loaded.
pub fn config_source_lines(
    loader: &crate::config::ConfigLoader,
    context: &str,
    cluster: &str,
) -> Vec<String> {
    let mut lines = vec!["Config sources".to_string()];
    match loader.base_path() {
        Some(path) => {
            let state = if loader.has_base() {
                "loaded"
            } else if path.exists() {
                "invalid — using defaults"
            } else {
                "absent — using defaults"
            };
            lines.push(format!("  {} ({state})", path.display()));
        }
        None => lines.push("  no config directory — using defaults".into()),
    }
    for path in loader.dropin_paths() {
        lines.push(format!(
            "  {} ({})",
            path.display(),
            crate::config::dropin_state(&path)
        ));
    }
    for path in loader.override_paths(context, cluster) {
        lines.push(format!(
            "  {} ({})",
            path.display(),
            crate::config::file_state(&path)
        ));
    }
    lines
}

/// State, log, snapshot, and bundle directories.
pub fn directory_lines() -> Vec<String> {
    vec![
        "Directories".to_string(),
        format!("  state:     {}", state_dir().display()),
        format!("  logs:      {}", log_dir().display()),
        format!(
            "  snapshots: {}",
            crate::snapshot::snapshots_dir().display()
        ),
        format!("  bundles:   {}", bundle_dir().display()),
    ]
}

/// Logging level, destination, and whether anything was dropped.
pub fn logging_lines() -> Vec<String> {
    let status = crate::applog::status();
    let mut lines = vec![
        "Logging".to_string(),
        format!("  level: {}", status.level.as_str()),
    ];
    match &status.path {
        Some(path) => {
            lines.push(format!("  file:  {}", path.display()));
            lines.push(format!("  lines: {}", status.written));
            if status.dropped > 0 {
                lines.push(format!(
                    "  dropped: {} (writer fell behind)",
                    status.dropped
                ));
            }
        }
        None => lines.push(format!(
            "  file:  (disabled — set [logging] level or {}=debug)",
            crate::applog::LEVEL_ENV
        )),
    }
    lines
}

/// Request latency per class, as an aligned table. Empty when no request has
/// been made yet (a headless report that never connected).
pub fn latency_lines() -> Vec<String> {
    let summary = latency_summary();
    if summary.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        "API request latency".to_string(),
        format!(
            "  {:<10} {:>7} {:>7} {:>9} {:>9} {:>9} {:>9}",
            "CLASS", "COUNT", "ERRORS", "AVG", "P50", "P90", "MAX"
        ),
    ];
    for s in summary {
        lines.push(format!(
            "  {:<10} {:>7} {:>7} {:>9} {:>9} {:>9} {:>9}",
            s.op.as_str(),
            s.count,
            s.errors,
            ms(s.avg_ms),
            ms(s.p50_ms),
            ms(s.p90_ms),
            ms(s.max_ms),
        ));
    }
    lines
}

/// Milliseconds at a fixed, readable precision (`4.2ms`, `1204ms`).
fn ms(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}ms")
    } else {
        format!("{value:.1}ms")
    }
}

/// How long a headless watch probe waits for an initial sync. Long enough for
/// a slow list on a large cluster, short enough that `sofka info` stays a
/// command you run while someone is waiting.
pub const WATCH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Render a [`crate::k8s::WatchProbe`] as report lines.
pub fn watch_probe_lines(
    probe: &crate::k8s::WatchProbe,
    resource: &str,
    timeout: Duration,
) -> Vec<String> {
    let crate::k8s::WatchProbe::Ran {
        synced,
        objects,
        polling,
        errors,
        last_error,
    } = probe
    else {
        return vec![format!(
            "  established: no — '{resource}' is not in this cluster's discovery"
        )];
    };
    let mut lines = vec![match synced {
        Some(elapsed) if *polling => format!(
            "  established: polling every {} (watch forbidden), first list in {}ms ({objects} objects)",
            human_duration(crate::k8s::POLL_INTERVAL),
            elapsed.as_millis()
        ),
        Some(elapsed) => format!(
            "  established: yes, in {}ms ({objects} objects in the initial list)",
            elapsed.as_millis()
        ),
        None if *errors > 0 => "  established: no — see the error below".to_string(),
        None => format!(
            "  established: no — no initial sync within {}",
            human_duration(timeout)
        ),
    }];
    lines.push(format!("  errors:      {errors}"));
    if let Some(error) = last_error {
        lines.push(format!("  last error:  {}", safe(error)));
    }
    lines
}

fn human_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{}s", d.as_secs())
    }
}

/// How many named items (plugins, views, …) a report lists before it stops and
/// reports the remainder as a count.
const MAX_NAMED: usize = 12;

/// `  label:      <count>  (a, b, c)` — so a report answers "which plugins
/// loaded?" and not only "how many". Long lists are truncated; the count stays
/// exact either way.
pub fn named_line<'a>(label: &str, names: impl Iterator<Item = &'a str>, count: usize) -> String {
    let mut names: Vec<&str> = names.take(MAX_NAMED + 1).collect();
    names.sort_unstable();
    let overflow = count.saturating_sub(names.len().min(MAX_NAMED));
    names.truncate(MAX_NAMED);
    let detail = if names.is_empty() {
        String::new()
    } else if overflow > 0 {
        format!("  ({}, +{overflow} more)", names.join(", "))
    } else {
        format!("  ({})", names.join(", "))
    };
    format!("  {:<11} {count}{detail}", format!("{label}:"))
}

/// Config validation warnings, or a line saying there are none.
pub fn warning_lines(warnings: &[String]) -> Vec<String> {
    if warnings.is_empty() {
        return vec!["No config validation warnings.".to_string()];
    }
    let mut lines = vec![format!("Warnings [{}]", warnings.len())];
    for w in warnings {
        for (i, l) in w.lines().enumerate() {
            let bullet = if i == 0 { "• " } else { "  " };
            lines.push(format!("  {bullet}{}", safe(l)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    #[test]
    fn printable_drops_escape_sequences() {
        let output = printable("ok\x1b[2J\x1b[Hbad");
        assert!(!output.contains('\x1b'));
        assert_eq!(output, "ok [2J [Hbad");
        assert_eq!(printable("a\r\t\x07\x7f\u{009b}b"), "a     b");
    }

    #[test]
    fn printable_keeps_newlines_and_plain_text() {
        assert_eq!(printable("line one\nline two"), "line one\nline two");
    }

    #[test]
    fn printable_replaces_bidirectional_formatting_controls() {
        for c in ('\u{202a}'..='\u{202e}').chain('\u{2066}'..='\u{2069}') {
            assert_eq!(
                printable(&format!("plugin{c}publisher")),
                "plugin publisher"
            );
        }
        assert_eq!(printable("日本語 العربية\ntext"), "日本語 العربية\ntext");
    }

    #[test]
    fn printable_redacts_url_userinfo() {
        let output = printable("https://dummy-user:dummy-password@proxy.example:3128");
        assert!(!output.contains("dummy-password"));
        assert_eq!(
            output,
            format!("https://{}@proxy.example:3128", crate::redact::REDACTED)
        );
    }

    #[test]
    fn classifies_requests_by_shape() {
        let get = Method::GET;
        assert_eq!(Op::classify(&get, "/version", None), Op::Discovery);
        assert_eq!(Op::classify(&get, "/api", None), Op::Discovery);
        assert_eq!(Op::classify(&get, "/api/v1", None), Op::Discovery);
        assert_eq!(Op::classify(&get, "/apis/apps/v1", None), Op::Discovery);
        assert_eq!(
            Op::classify(&get, "/api/v1/pods", Some("limit=500")),
            Op::Read
        );
        assert_eq!(
            Op::classify(&get, "/api/v1/pods", Some("resourceVersion=1&watch=true")),
            Op::Watch
        );
        assert_eq!(
            Op::classify(&get, "/apis/metrics.k8s.io/v1beta1/pods", None),
            Op::Metrics
        );
        assert_eq!(
            Op::classify(&get, "/api/v1/namespaces/x/pods/y/log", Some("follow=true")),
            Op::Logs
        );
        assert_eq!(
            Op::classify(&get, "/api/v1/namespaces/x/pods/y/exec", None),
            Op::Exec
        );
        assert_eq!(
            Op::classify(&Method::DELETE, "/api/v1/pods/y", None),
            Op::Write
        );
        assert_eq!(
            Op::classify(&Method::PATCH, "/apis/apps/v1/x/y", None),
            Op::Write
        );
    }

    #[test]
    fn classifies_metrics_group_discovery_as_metrics() {
        // Group discovery for metrics.k8s.io is attributed to the API it is
        // about, so a wedged metrics-server shows up in one row.
        assert_eq!(
            Op::classify(&Method::GET, "/apis/metrics.k8s.io/v1beta1", None),
            Op::Metrics
        );
        // A group whose name merely starts the same way is not the metrics API.
        assert_eq!(
            Op::classify(&Method::GET, "/apis/metrics.k8s.io.example/v1/things", None),
            Op::Read
        );
    }

    #[test]
    fn classifies_edge_paths_without_panicking() {
        assert_eq!(Op::classify(&Method::GET, "/", None), Op::Read);
        assert_eq!(Op::classify(&Method::GET, "", None), Op::Read);
        assert_eq!(Op::classify(&Method::POST, "/", None), Op::Write);
        // A resource plural ending in "log" is not the log subresource.
        assert_eq!(
            Op::classify(&Method::GET, "/apis/g/v1/namespaces/n/logs", None),
            Op::Read
        );
    }

    #[test]
    fn a_field_selector_mentioning_watch_is_not_a_watch() {
        assert_eq!(
            Op::classify(
                &Method::GET,
                "/api/v1/pods",
                Some("fieldSelector=metadata.name%3Dwatch%3Dtrue")
            ),
            Op::Read
        );
    }

    // One test for the whole registry: it is process-wide, so two tests that
    // both reset it would race under the default parallel runner.
    #[test]
    fn summarises_recorded_latency() {
        let _guard = LATENCY_TEST_LOCK.lock().unwrap();
        reset_latency();
        assert!(latency_lines().is_empty(), "no requests, no table");
        for _ in 0..8 {
            record(Op::Read, Duration::from_millis(2), true);
        }
        for _ in 0..2 {
            record(Op::Read, Duration::from_millis(500), false);
        }

        let summary = latency_summary();
        let read = summary
            .iter()
            .find(|s| s.op == Op::Read)
            .expect("read class recorded");
        assert_eq!(read.count, 10);
        assert_eq!(read.errors, 2);
        assert!((read.avg_ms - 101.6).abs() < 1.0, "avg {}", read.avg_ms);
        // Eight of ten samples are ~2ms, so p50 sits in a small bucket and the
        // slow pair only shows up from p90 on.
        assert!(read.p50_ms <= 4.2, "p50 {}", read.p50_ms);
        assert!(
            read.p90_ms >= 500.0 && read.p90_ms <= read.max_ms,
            "p90 {}",
            read.p90_ms
        );
        assert!((read.max_ms - 500.0).abs() < 1.0, "max {}", read.max_ms);
        // Classes nothing touched stay out of the report entirely.
        assert!(summary.iter().all(|s| s.op != Op::Exec));

        let table = latency_lines().join("\n");
        assert!(table.contains("API request latency"), "{table}");
        assert!(table.contains("read"), "{table}");
        assert!(!table.contains("exec"), "{table}");
        reset_latency();
    }

    #[test]
    fn report_sections_redact_credentials() {
        let warnings = vec!["providers: bad header authorization: Bearer abc123".to_string()];
        let lines = warning_lines(&warnings).join("\n");
        assert!(!lines.contains("abc123"), "{lines}");
        assert!(lines.contains(crate::redact::REDACTED), "{lines}");
        assert_eq!(
            safe("https://admin:pw@api.example.com"),
            format!("https://{}@api.example.com", crate::redact::REDACTED)
        );
        assert_eq!(safe_or("", "(unknown)"), "(unknown)");
    }

    #[tokio::test]
    async fn watch_probe_reports_an_unresolvable_resource() {
        let cluster = crate::k8s::Cluster::fake();
        let probe = cluster
            .probe_watch("widgets", "default", Duration::from_millis(50))
            .await;
        assert!(matches!(probe, crate::k8s::WatchProbe::Unresolved));
        let lines = watch_probe_lines(&probe, "widgets", WATCH_PROBE_TIMEOUT).join("\n");
        assert!(lines.contains("not in this cluster's discovery"), "{lines}");
    }

    #[tokio::test]
    async fn watch_probe_reports_a_watch_that_never_syncs() {
        // The fake cluster points at a port nothing answers on, which is the
        // failure the probe exists to name.
        let cluster = crate::k8s::Cluster::fake();
        let probe = cluster
            .probe_watch("pods", "default", Duration::from_millis(150))
            .await;
        let lines = watch_probe_lines(&probe, "pods", Duration::from_millis(150)).join("\n");
        assert!(lines.contains("established: no"), "{lines}");
        assert!(lines.contains("errors:"), "{lines}");
    }

    #[test]
    fn watch_probe_renders_a_successful_sync() {
        let probe = crate::k8s::WatchProbe::Ran {
            synced: Some(Duration::from_millis(84)),
            objects: 312,
            polling: false,
            errors: 0,
            last_error: None,
        };
        let lines = watch_probe_lines(&probe, "pods", WATCH_PROBE_TIMEOUT).join("\n");
        assert!(
            lines.contains("established: yes, in 84ms (312 objects in the initial list)"),
            "{lines}"
        );
        assert!(lines.contains("errors:      0"), "{lines}");
    }

    #[test]
    fn watch_probe_reports_polling_instead_of_a_failure() {
        let probe = crate::k8s::WatchProbe::Ran {
            synced: Some(Duration::from_millis(31)),
            objects: 3,
            polling: true,
            errors: 0,
            last_error: None,
        };
        let lines = watch_probe_lines(&probe, "pods", WATCH_PROBE_TIMEOUT).join("\n");
        assert!(
            lines.contains(
                "established: polling every 2s (watch forbidden), first list in 31ms (3 objects)"
            ),
            "{lines}"
        );
        assert!(lines.contains("errors:      0"), "{lines}");
    }

    #[test]
    fn watch_probe_redacts_the_error_it_reports() {
        let probe = crate::k8s::WatchProbe::Ran {
            synced: None,
            objects: 0,
            polling: false,
            errors: 1,
            last_error: Some("401 for authorization: Bearer eyJhbGciOi.J9.sig".into()),
        };
        let lines = watch_probe_lines(&probe, "pods", WATCH_PROBE_TIMEOUT).join("\n");
        assert!(!lines.contains("eyJhbGciOi.J9.sig"), "{lines}");
        assert!(lines.contains(crate::redact::REDACTED), "{lines}");
    }

    #[test]
    fn named_line_lists_names_and_truncates() {
        assert_eq!(
            named_line("plugins", std::iter::empty(), 0),
            "  plugins:    0"
        );
        assert_eq!(
            named_line("views", ["b", "a"].into_iter(), 2),
            "  views:      2  (a, b)"
        );
        let many: Vec<String> = (0..20).map(|i| format!("v{i:02}")).collect();
        let line = named_line("views", many.iter().map(String::as_str), many.len());
        assert!(line.starts_with("  views:      20  (v00, "), "{line}");
        assert!(line.ends_with(", +8 more)"), "{line}");
    }

    #[test]
    fn directories_include_logs_and_bundles() {
        let lines = directory_lines().join("\n");
        assert!(lines.contains("logs:"), "{lines}");
        assert!(lines.contains("snapshots:"), "{lines}");
        assert!(lines.contains("bundles:"), "{lines}");
    }

    #[test]
    fn logging_section_names_the_env_override_when_off() {
        let lines = logging_lines().join("\n");
        assert!(lines.contains("level: off"), "{lines}");
        assert!(lines.contains("SOFKA_LOG"), "{lines}");
    }
}
