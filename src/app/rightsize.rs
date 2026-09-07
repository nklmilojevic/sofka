use super::*;

use crate::columns::{fmt_cpu, fmt_mem, parse_cpu_milli, parse_mem_bytes};
use crate::rightsize::{ContainerRec, Quantiles, patch_preview, suggest};

/// One container's spec pulled off the selected object before the gather.
struct ContainerSpec {
    name: String,
    cpu_request: Option<f64>,
    mem_request: Option<f64>,
}

impl App {
    /// `:rightsize` — estimate right-sized requests for the selected workload
    /// (or pod) from historical usage in a Prometheus/VictoriaMetrics backend,
    /// and preview the patch. Never mutates.
    pub(super) fn open_rightsize(&mut self) {
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection to right-size");
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        // Container list + a PromQL `pod` matcher: a bare pod matches exactly;
        // a workload matches its replica pods by name prefix.
        let (path, pod_matcher) = match self.kind_plural.as_str() {
            "pods" => ("/spec/containers", format!("^{}$", regex_escape(&name))),
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" => (
                "/spec/template/spec/containers",
                format!("^{}-.*", regex_escape(&name)),
            ),
            _ => {
                self.flash_warn("right-size applies to workloads (deploy/sts/ds/rs) and pods");
                return;
            }
        };
        let containers = container_specs(obj, path);
        if containers.is_empty() {
            self.flash_warn("no containers found to right-size");
            return;
        }

        // Zero-config: with no [providers.metrics], autodiscover in-cluster.
        let provider = self.metrics_provider.clone().unwrap_or_default();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let kind_name = self
            .kind
            .as_ref()
            .map(|k| k.ar.kind.clone())
            .unwrap_or_else(|| self.kind_plural.clone());
        let claim = self.claim_status(format!("right-sizing {name} over {}…", provider.window));

        tokio::spawn(async move {
            let title = format!("right-size — {kind_name}/{name}");
            // Resolve the transport (autodiscover) on first use.
            let provider = if provider.needs_discovery() {
                match crate::providers::discover_metrics(client, &provider).await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx
                            .send(Msg::Detail {
                                generation: genr,
                                claim,
                                target: None,
                                title,
                                lines: vec![format!("no metrics backend: {e}")],
                                warn: Some("right-size needs Prometheus/VictoriaMetrics".into()),
                            })
                            .await;
                        return;
                    }
                }
            } else {
                provider
            };

            let window = provider.window.clone();
            let step = provider.step.clone();
            let headroom = provider.headroom;

            // All containers (and all eight queries within each) gathered
            // concurrently — serialized this was 8N round trips.
            let results = futures_util::future::join_all(containers.iter().map(|c| {
                gather_container(&provider, &ns, &pod_matcher, c, &window, &step, headroom)
            }))
            .await;
            let mut recs = Vec::new();
            let mut errors = Vec::new();
            for (rec, errs) in results {
                recs.push(rec);
                errors.extend(errs);
            }

            let lines = render_report(&provider.location(), &window, headroom, &recs, &errors);
            let warn =
                (!errors.is_empty()).then(|| format!("{} metrics queries failed", errors.len()));
            let _ = tx
                .send(Msg::Detail {
                    generation: genr,
                    claim,
                    target: None,
                    title,
                    lines,
                    warn,
                })
                .await;
        });
    }
}

/// Query the eight PromQL series for one container (concurrently) and
/// assemble its rec, plus any query errors. A failed query yields `None` —
/// never a zero the report would then present as measured usage.
async fn gather_container(
    provider: &crate::providers::MetricsProvider,
    ns: &str,
    pod_matcher: &str,
    spec: &ContainerSpec,
    window: &str,
    step: &str,
    headroom: u32,
) -> (ContainerRec, Vec<String>) {
    let sel = format!(
        "namespace=\"{ns}\",pod=~\"{pod_matcher}\",container=\"{}\"",
        spec.name
    );
    let cpu_q = |q: &str| {
        format!(
            "max(quantile_over_time({q}, rate(container_cpu_usage_seconds_total{{{sel}}}[{step}])[{window}:{step}]))*1000"
        )
    };
    let mem_q = |q: &str| {
        format!(
            "max(quantile_over_time({q}, container_memory_working_set_bytes{{{sel}}}[{window}]))"
        )
    };
    let q = |promql: String| async move { provider.query(&promql).await };

    let (c50, c95, c99, m50, m95, m99, oom, throttle) = tokio::join!(
        q(cpu_q("0.5")),
        q(cpu_q("0.95")),
        q(cpu_q("0.99")),
        q(mem_q("0.5")),
        q(mem_q("0.95")),
        q(mem_q("0.99")),
        q(format!(
            "sum(increase(container_oom_events_total{{{sel}}}[{window}]))"
        )),
        q(format!(
            "sum(increase(container_cpu_cfs_throttled_periods_total{{{sel}}}[{window}]))"
        )),
    );

    let mut errors = Vec::new();
    let mut take = |r: Result<Option<f64>, String>| match r {
        Ok(v) => v,
        Err(e) => {
            errors.push(e);
            None
        }
    };
    let cpu = Quantiles {
        p50: take(c50),
        p95: take(c95),
        p99: take(c99),
    };
    let mem = Quantiles {
        p50: take(m50),
        p95: take(m95),
        p99: take(m99),
    };
    let oom = take(oom);
    let throttle = take(throttle);

    let rec = ContainerRec {
        container: spec.name.clone(),
        suggested_cpu: cpu.p95.filter(|v| *v > 0.0).map(|p| suggest(p, headroom)),
        suggested_mem: mem.p95.filter(|v| *v > 0.0).map(|p| suggest(p, headroom)),
        cpu,
        mem,
        cpu_request: spec.cpu_request,
        mem_request: spec.mem_request,
        oom,
        throttle,
    };
    (rec, errors)
}

/// Render the recommendation report as document lines.
fn render_report(
    location: &str,
    window: &str,
    headroom: u32,
    recs: &[ContainerRec],
    errors: &[String],
) -> Vec<String> {
    let opt_cpu = |v: Option<f64>| v.map(|n| fmt_cpu(n as i64)).unwrap_or_else(|| "—".into());
    let opt_mem = |v: Option<f64>| v.map(|n| fmt_mem(n as i64)).unwrap_or_else(|| "—".into());

    let mut lines = vec![
        format!("source:  {location}"),
        format!("window:  {window}   headroom: +{headroom}% over P95"),
        String::new(),
    ];
    if !errors.is_empty() {
        lines.push(format!(
            "⚠ {} metrics quer{} failed — '—' below means unknown, not zero",
            errors.len(),
            if errors.len() == 1 { "y" } else { "ies" },
        ));
        lines.push(format!("  first error: {}", errors[0]));
        lines.push(String::new());
    }
    for r in recs {
        lines.push(format!("container: {}", r.container));
        lines.push(format!(
            "  cpu   request {:<8} P50 {:<8} P95 {:<8} P99 {:<8} → suggest {}   [{}]",
            opt_cpu(r.cpu_request),
            opt_cpu(r.cpu.p50),
            opt_cpu(r.cpu.p95),
            opt_cpu(r.cpu.p99),
            opt_cpu(r.suggested_cpu),
            r.cpu_verdict().label(),
        ));
        lines.push(format!(
            "  mem   request {:<8} P50 {:<8} P95 {:<8} P99 {:<8} → suggest {}   [{}]",
            opt_mem(r.mem_request),
            opt_mem(r.mem.p50),
            opt_mem(r.mem.p95),
            opt_mem(r.mem.p99),
            opt_mem(r.suggested_mem),
            r.mem_verdict().label(),
        ));
        if r.oom.unwrap_or(0.0) > 0.0 || r.throttle.unwrap_or(0.0) > 0.0 {
            lines.push(format!(
                "  evidence: {} OOM kill(s) · {} throttled CPU period(s) over {window}",
                r.oom.unwrap_or(0.0).round() as i64,
                r.throttle.unwrap_or(0.0).round() as i64,
            ));
        }
        lines.push(String::new());
    }

    match patch_preview(recs) {
        Some(patch) => {
            lines.push(
                "suggested patch (preview only — copy with c, apply with kubectl patch):".into(),
            );
            lines.push(String::new());
            lines.extend(patch.lines().map(String::from));
        }
        None => lines
            .push("no usage data found — is the workload scraped by the metrics backend?".into()),
    }
    lines
}

/// Extract each container's name and current CPU/memory requests from the pod
/// spec at `path` (`/spec/containers` for pods, `/spec/template/spec/containers`
/// for workloads).
fn container_specs(obj: &DynamicObject, path: &str) -> Vec<ContainerSpec> {
    obj.data
        .pointer(path)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let name = c.get("name").and_then(Value::as_str)?.to_string();
                    let req = c.pointer("/resources/requests");
                    let cpu_request = req
                        .and_then(|r| r.get("cpu"))
                        .and_then(Value::as_str)
                        .map(|s| parse_cpu_milli(s) as f64);
                    let mem_request = req
                        .and_then(|r| r.get("memory"))
                        .and_then(Value::as_str)
                        .map(|s| parse_mem_bytes(s) as f64);
                    Some(ContainerSpec {
                        name,
                        cpu_request,
                        mem_request,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Escape the regex metacharacters that can appear in a Kubernetes object name
/// (really just `.`), so the PromQL `pod=~` matcher is anchored to the literal.
fn regex_escape(name: &str) -> String {
    name.replace('.', "\\.")
}
