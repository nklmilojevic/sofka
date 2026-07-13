//! Historical right-sizing: Prometheus response parsing, the recommendation
//! math, and strategic-merge patch synthesis.
//!
//! Given P50/P95/P99 CPU and memory for a workload's containers (gathered from
//! a Prometheus-compatible backend), sofka suggests a request sized at P95 plus
//! a headroom margin, classifies over/under-provisioning, and previews the
//! patch that would apply it — but never mutates anything itself. All of the
//! logic here is pure and unit-tested; the HTTP queries live in the provider.

use serde_json::{Value, json};

/// Parse a Prometheus/VictoriaMetrics instant-query response body and return
/// the first sample's value. `None` for a non-success status, an empty result
/// vector, or a malformed body — callers treat that as "no data".
pub fn scalar_from_query(body: &str) -> Option<f64> {
    let v: Value = serde_json::from_str(body).ok()?;
    if v.get("status").and_then(Value::as_str) != Some("success") {
        return None;
    }
    let result = v.pointer("/data/result")?.as_array()?;
    let value = result.first()?.get("value")?.as_array()?.get(1)?.as_str()?;
    value.parse::<f64>().ok()
}

/// P50/P95/P99 of one resource over the window (CPU in millicores, memory in
/// bytes). Missing samples read as 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quantiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// A right-sizing verdict for one resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No current request set — can't compare.
    Unset,
    /// Request is well above observed peak — wasting reservation.
    Over,
    /// Request is close to observed peak — sized about right.
    Ok,
    /// Request is below observed peak (risk of eviction/throttling).
    Under,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Unset => "no request set",
            Verdict::Over => "over-provisioned",
            Verdict::Ok => "sized about right",
            Verdict::Under => "under-provisioned",
        }
    }
}

/// The suggested request: P95 plus `headroom_pct`% margin.
pub fn suggest(p95: f64, headroom_pct: u32) -> f64 {
    p95 * (1.0 + headroom_pct as f64 / 100.0)
}

/// Classify a current request against the observed P95. `Over` when the request
/// exceeds P95 by more than half again (lots of slack); `Under` when it's below
/// P95 (peak already breaches the request); `Ok` in between.
pub fn verdict(current_request: Option<f64>, p95: f64) -> Verdict {
    match current_request {
        None => Verdict::Unset,
        Some(req) if req <= 0.0 => Verdict::Unset,
        Some(req) if p95 > req => Verdict::Under,
        Some(req) if req > p95 * 1.5 => Verdict::Over,
        Some(_) => Verdict::Ok,
    }
}

/// A right-sizing recommendation for one container.
#[derive(Clone, Debug)]
pub struct ContainerRec {
    pub container: String,
    /// CPU quantiles in millicores.
    pub cpu: Quantiles,
    /// Memory quantiles in bytes.
    pub mem: Quantiles,
    /// Current requests (millicores / bytes), when set.
    pub cpu_request: Option<f64>,
    pub mem_request: Option<f64>,
    /// OOM kills and CPU-throttled periods observed over the window.
    pub oom: f64,
    pub throttle: f64,
    /// Suggested requests (millicores / bytes).
    pub suggested_cpu: f64,
    pub suggested_mem: f64,
}

impl ContainerRec {
    pub fn cpu_verdict(&self) -> Verdict {
        verdict(self.cpu_request, self.cpu.p95)
    }
    pub fn mem_verdict(&self) -> Verdict {
        // An OOM kill in the window forces an under-provisioned verdict even if
        // the sampled working set never appeared to breach the request.
        if self.oom > 0.0 {
            return Verdict::Under;
        }
        verdict(self.mem_request, self.mem.p95)
    }
}

/// Round CPU millicores up to a whole-millicore Kubernetes quantity (`"120m"`).
pub fn cpu_quantity(millicores: f64) -> String {
    format!("{}m", millicores.ceil().max(1.0) as i64)
}

/// Round memory bytes up to a whole-mebibyte Kubernetes quantity (`"128Mi"`).
pub fn mem_quantity(bytes: f64) -> String {
    let mi = (bytes / (1024.0 * 1024.0)).ceil().max(1.0) as i64;
    format!("{mi}Mi")
}

/// Build the strategic-merge patch that would apply the suggested requests to a
/// workload's pod template, as pretty JSON. `path_root` is the pointer prefix
/// to the container list — `spec.template.spec` for Deployments/StatefulSets/
/// DaemonSets. Returns `None` when no container has a suggestion.
pub fn patch_preview(recs: &[ContainerRec]) -> Option<String> {
    let containers: Vec<Value> = recs
        .iter()
        .filter(|r| r.suggested_cpu > 0.0 || r.suggested_mem > 0.0)
        .map(|r| {
            let mut requests = serde_json::Map::new();
            if r.suggested_cpu > 0.0 {
                requests.insert("cpu".into(), json!(cpu_quantity(r.suggested_cpu)));
            }
            if r.suggested_mem > 0.0 {
                requests.insert("memory".into(), json!(mem_quantity(r.suggested_mem)));
            }
            json!({ "name": r.container, "resources": { "requests": requests } })
        })
        .collect();
    if containers.is_empty() {
        return None;
    }
    let patch = json!({
        "spec": { "template": { "spec": { "containers": containers } } }
    });
    serde_json::to_string_pretty(&patch).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_prometheus_vector_sample() {
        let body = r#"{"status":"success","data":{"resultType":"vector",
            "result":[{"metric":{},"value":[1700000000,"117743616"]}]}}"#;
        assert_eq!(scalar_from_query(body), Some(117743616.0));
    }

    #[test]
    fn empty_or_failed_query_is_none() {
        let empty = r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#;
        assert_eq!(scalar_from_query(empty), None);
        let failed = r#"{"status":"error","error":"boom"}"#;
        assert_eq!(scalar_from_query(failed), None);
        assert_eq!(scalar_from_query("not json"), None);
    }

    #[test]
    fn suggest_applies_headroom_over_p95() {
        assert!((suggest(100.0, 15) - 115.0).abs() < 1e-6);
        assert_eq!(suggest(200.0, 0), 200.0);
    }

    #[test]
    fn verdict_classifies_against_p95() {
        assert_eq!(verdict(None, 100.0), Verdict::Unset);
        assert_eq!(verdict(Some(0.0), 100.0), Verdict::Unset);
        assert_eq!(verdict(Some(50.0), 100.0), Verdict::Under); // peak > request
        assert_eq!(verdict(Some(120.0), 100.0), Verdict::Ok); // a little slack
        assert_eq!(verdict(Some(500.0), 100.0), Verdict::Over); // >1.5× P95
    }

    #[test]
    fn oom_forces_memory_under_provisioned() {
        let rec = ContainerRec {
            container: "app".into(),
            cpu: Quantiles::default(),
            mem: Quantiles {
                p50: 10.0,
                p95: 20.0,
                p99: 25.0,
            },
            cpu_request: Some(100.0),
            mem_request: Some(1_000_000.0), // request far above sampled p95…
            oom: 3.0,                       // …but it was OOM-killed
            throttle: 0.0,
            suggested_cpu: 0.0,
            suggested_mem: 30.0,
        };
        assert_eq!(rec.mem_verdict(), Verdict::Under);
    }

    #[test]
    fn quantities_round_up_to_k8s_units() {
        assert_eq!(cpu_quantity(4.5), "5m");
        assert_eq!(cpu_quantity(0.1), "1m"); // never zero
        assert_eq!(mem_quantity(117_743_616.0), "113Mi");
        assert_eq!(mem_quantity(1.0), "1Mi");
    }

    #[test]
    fn patch_preview_covers_containers_with_a_suggestion() {
        let recs = vec![
            ContainerRec {
                container: "app".into(),
                cpu: Quantiles::default(),
                mem: Quantiles::default(),
                cpu_request: None,
                mem_request: None,
                oom: 0.0,
                throttle: 0.0,
                suggested_cpu: 120.0,
                suggested_mem: 134_217_728.0,
            },
            ContainerRec {
                container: "sidecar".into(),
                cpu: Quantiles::default(),
                mem: Quantiles::default(),
                cpu_request: None,
                mem_request: None,
                oom: 0.0,
                throttle: 0.0,
                suggested_cpu: 0.0, // no data → excluded from the patch
                suggested_mem: 0.0,
            },
        ];
        let patch = patch_preview(&recs).unwrap();
        assert!(patch.contains("\"name\": \"app\""));
        assert!(patch.contains("\"cpu\": \"120m\""));
        assert!(patch.contains("\"memory\": \"128Mi\""));
        assert!(!patch.contains("sidecar"), "no-data container omitted");
        // Empty input → no patch at all.
        assert!(patch_preview(&[]).is_none());
    }
}
