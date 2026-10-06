use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, PoisonError};

use kube::core::DynamicObject;
use serde_json::Value;

use crate::columns::{
    fmt_cpu_sample, fmt_mem_sample, fmt_pct, parse_cpu_milli, parse_mem_bytes, usage_pct,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricColumn {
    Cpu,
    Memory,
    CpuRequest,
    CpuLimit,
    MemoryRequest,
    MemoryLimit,
    CpuRequestUtilization,
    CpuLimitUtilization,
    MemoryRequestUtilization,
    MemoryLimitUtilization,
    NodePods,
    NodeCpuUtilization,
    NodeMemoryUtilization,
    NodeCpuTrend,
    NodeMemoryTrend,
    NodeRequest(&'static str),
    NodeLimit(&'static str),
}

impl MetricColumn {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "cpu" => Self::Cpu,
            "memory" => Self::Memory,
            "cpu-request" => Self::CpuRequest,
            "cpu-limit" => Self::CpuLimit,
            "memory-request" => Self::MemoryRequest,
            "memory-limit" => Self::MemoryLimit,
            "cpu-request-utilization" => Self::CpuRequestUtilization,
            "cpu-limit-utilization" => Self::CpuLimitUtilization,
            "memory-request-utilization" => Self::MemoryRequestUtilization,
            "memory-limit-utilization" => Self::MemoryLimitUtilization,
            "node-pods" => Self::NodePods,
            "node-cpu-utilization" => Self::NodeCpuUtilization,
            "node-memory-utilization" => Self::NodeMemoryUtilization,
            "node-cpu-trend" => Self::NodeCpuTrend,
            "node-memory-trend" => Self::NodeMemoryTrend,
            "node-cpu-request" => Self::NodeRequest("cpu"),
            "node-memory-request" => Self::NodeRequest("memory"),
            "node-cpu-limit" => Self::NodeLimit("cpu"),
            "node-memory-limit" => Self::NodeLimit("memory"),
            _ => {
                let (kind, resource) = name.split_once(':')?;
                let resource = resource.trim();
                if resource.is_empty() || resource.contains(char::is_whitespace) {
                    return None;
                }
                match kind {
                    "node-request" => Self::NodeRequest(intern(resource)),
                    "node-limit" => Self::NodeLimit(intern(resource)),
                    _ => return None,
                }
            }
        })
    }

    pub fn supported(self, group: &str, plural: &str) -> bool {
        group.is_empty()
            && match self {
                Self::Cpu | Self::Memory => matches!(plural, "pods" | "nodes"),
                Self::NodePods
                | Self::NodeCpuUtilization
                | Self::NodeMemoryUtilization
                | Self::NodeCpuTrend
                | Self::NodeMemoryTrend
                | Self::NodeRequest(_)
                | Self::NodeLimit(_) => plural == "nodes",
                _ => plural == "pods",
            }
    }

    pub fn percentage(self) -> bool {
        matches!(
            self,
            Self::CpuRequestUtilization
                | Self::CpuLimitUtilization
                | Self::MemoryRequestUtilization
                | Self::MemoryLimitUtilization
                | Self::NodeCpuUtilization
                | Self::NodeMemoryUtilization
                | Self::NodeCpuTrend
                | Self::NodeMemoryTrend
                | Self::NodeRequest(_)
                | Self::NodeLimit(_)
        )
    }

    /// Sources computed from the pods scheduled on each node.
    pub fn node_load(self) -> bool {
        matches!(
            self,
            Self::NodePods | Self::NodeRequest(_) | Self::NodeLimit(_)
        )
    }

    pub fn trend(self) -> bool {
        matches!(self, Self::NodeCpuTrend | Self::NodeMemoryTrend)
    }

    pub fn cpu(self) -> bool {
        matches!(
            self,
            Self::Cpu
                | Self::CpuRequest
                | Self::CpuLimit
                | Self::CpuRequestUtilization
                | Self::CpuLimitUtilization
                | Self::NodeCpuUtilization
                | Self::NodeCpuTrend
                | Self::NodeRequest("cpu")
                | Self::NodeLimit("cpu")
        )
    }

    pub fn format(self, value: Option<i64>) -> String {
        if self.percentage() {
            fmt_pct(value)
        } else if self == Self::NodePods {
            value.map_or_else(|| "-".into(), |v| v.to_string())
        } else if self.cpu() {
            fmt_cpu_sample(value)
        } else {
            fmt_mem_sample(value)
        }
    }

    pub fn value(
        self,
        obj: &DynamicObject,
        usage: Option<(i64, i64)>,
        load: Option<&NodeLoad>,
    ) -> Option<i64> {
        match self {
            Self::Cpu => usage.map(|v| v.0),
            Self::Memory => usage.map(|v| v.1),
            Self::NodePods => load.map(|v| v.pods as i64),
            Self::NodeRequest(resource) => committed_pct(&load?.requests, obj, resource),
            Self::NodeLimit(resource) => committed_pct(&load?.limits, obj, resource),
            Self::NodeCpuUtilization | Self::NodeCpuTrend => {
                usage_pct(usage?.0, crate::columns::node_allocatable(obj).0)
            }
            Self::NodeMemoryUtilization | Self::NodeMemoryTrend => {
                usage_pct(usage?.1, crate::columns::node_allocatable(obj).1)
            }
            _ => {
                let limit = matches!(
                    self,
                    Self::CpuLimit
                        | Self::MemoryLimit
                        | Self::CpuLimitUtilization
                        | Self::MemoryLimitUtilization
                );
                let base = pod_resource(obj, self.cpu(), limit);
                if self.percentage() {
                    let usage = usage?;
                    usage_pct(if self.cpu() { usage.0 } else { usage.1 }, base)
                } else {
                    base
                }
            }
        }
    }
}

fn intern(name: &str) -> &'static str {
    static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = names.get(name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

/// Any resource quantity in thousandths of its unit. Requests and allocatable
/// share the scale, so only their ratio matters.
/// `i128` keeps petabyte-scale memory from saturating once scaled and summed.
fn quantity_milli(s: &str) -> Option<i128> {
    crate::views::parse_quantity(s)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| (n * 1000.0).round() as i128)
}

fn allocatable(obj: &DynamicObject, resource: &str) -> Option<i128> {
    obj.data
        .pointer("/status/allocatable")?
        .get(resource)?
        .as_str()
        .and_then(quantity_milli)
        .filter(|v| *v > 0)
}

fn committed_pct(
    totals: &BTreeMap<String, i128>,
    obj: &DynamicObject,
    resource: &str,
) -> Option<i64> {
    let used = totals.get(resource).copied().unwrap_or(0);
    let base = allocatable(obj, resource)?;
    Some((used as f64 / base as f64 * 100.0).round() as i64)
}

/// What the pods bound to one node commit: the pod count and the summed
/// requests and limits per resource, in [`quantity_milli`] units.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeLoad {
    pub pods: usize,
    pub requests: BTreeMap<String, i128>,
    pub limits: BTreeMap<String, i128>,
}

pub(crate) static NO_LOAD: NodeLoad = NodeLoad {
    pods: 0,
    requests: BTreeMap::new(),
    limits: BTreeMap::new(),
};

impl NodeLoad {
    pub fn with_pods(pods: usize) -> Self {
        Self {
            pods,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PodLoad {
    node: String,
    requests: BTreeMap<String, i128>,
    limits: BTreeMap<String, i128>,
}

impl PodLoad {
    fn of(pod: &DynamicObject) -> Option<Self> {
        let node = pod.data.pointer("/spec/nodeName")?.as_str()?.to_string();
        Some(Self {
            node,
            requests: pod_effective(pod, "requests"),
            limits: pod_effective(pod, "limits"),
        })
    }
}

/// Per-node totals kept incrementally from a pod watch, keyed by pod.
#[derive(Debug, Default)]
pub struct NodeLoads {
    pods: HashMap<String, PodLoad>,
    nodes: HashMap<String, NodeLoad>,
}

impl NodeLoads {
    pub fn clear(&mut self) {
        self.pods.clear();
        self.nodes.clear();
    }

    /// Record the latest version of `pod`. Returns whether any node changed.
    pub fn apply(&mut self, key: String, pod: &DynamicObject) -> bool {
        match PodLoad::of(pod) {
            Some(load) => {
                if self.pods.get(&key) == Some(&load) {
                    return false;
                }
                self.add(&load);
                if let Some(old) = self.pods.insert(key, load) {
                    self.retire(&old);
                }
                true
            }
            None => self.remove(&key),
        }
    }

    /// Forget a pod. Returns whether any node changed.
    pub fn remove(&mut self, key: &str) -> bool {
        match self.pods.remove(key) {
            Some(old) => {
                self.retire(&old);
                true
            }
            None => false,
        }
    }

    pub fn snapshot(&self) -> HashMap<String, NodeLoad> {
        self.nodes.clone()
    }

    fn add(&mut self, pod: &PodLoad) {
        let node = self.nodes.entry(pod.node.clone()).or_default();
        node.pods += 1;
        for (totals, pod) in [
            (&mut node.requests, &pod.requests),
            (&mut node.limits, &pod.limits),
        ] {
            for (name, v) in pod {
                let total = totals.entry(name.clone()).or_insert(0);
                *total = total.saturating_add(*v);
            }
        }
    }

    fn retire(&mut self, pod: &PodLoad) {
        let Some(node) = self.nodes.get_mut(&pod.node) else {
            return;
        };
        node.pods = node.pods.saturating_sub(1);
        if node.pods == 0 {
            self.nodes.remove(&pod.node);
            return;
        }
        let sub = |totals: &mut BTreeMap<String, i128>, pod: &BTreeMap<String, i128>| {
            for (name, v) in pod {
                if let Some(total) = totals.get_mut(name) {
                    *total -= v;
                    if *total <= 0 {
                        totals.remove(name);
                    }
                }
            }
        };
        sub(&mut node.requests, &pod.requests);
        sub(&mut node.limits, &pod.limits);
    }
}

/// A pod's requests or limits as the scheduler and `kubectl describe node`
/// count them: app containers and native sidecars summed, raised to the
/// largest init container step, plus overhead. Pod-level declarations win.
fn pod_effective(pod: &DynamicObject, section: &str) -> BTreeMap<String, i128> {
    let read = |resources: Option<&Value>| quantities(resources.and_then(|r| r.get(section)));
    let add = |into: &mut BTreeMap<String, i128>, from: &BTreeMap<String, i128>| {
        for (k, v) in from {
            let slot = into.entry(k.clone()).or_insert(0);
            *slot = slot.saturating_add(*v);
        }
    };
    let max = |into: &mut BTreeMap<String, i128>, from: &BTreeMap<String, i128>| {
        for (k, v) in from {
            let slot = into.entry(k.clone()).or_insert(0);
            *slot = (*slot).max(*v);
        }
    };
    let containers = |path: &str| {
        pod.data
            .pointer(path)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let mut total = BTreeMap::new();
    for c in containers("/spec/containers") {
        add(&mut total, &read(c.get("resources")));
    }
    let mut sidecars = BTreeMap::new();
    let mut init_peak = BTreeMap::new();
    for c in containers("/spec/initContainers") {
        let own = read(c.get("resources"));
        let mut step = sidecars.clone();
        add(&mut step, &own);
        if c.get("restartPolicy").and_then(Value::as_str) == Some("Always") {
            add(&mut total, &own);
            sidecars = step.clone();
        }
        max(&mut init_peak, &step);
    }
    max(&mut total, &init_peak);
    for (k, v) in read(pod.data.pointer("/spec/resources")) {
        if matches!(k.as_str(), "cpu" | "memory") || k.starts_with("hugepages-") {
            total.insert(k, v);
        }
    }
    for (k, v) in quantities(pod.data.pointer("/spec/overhead")) {
        if section == "requests" || total.contains_key(&k) {
            let slot = total.entry(k).or_insert(0);
            *slot = slot.saturating_add(v);
        }
    }
    total
}

fn quantities(list: Option<&Value>) -> BTreeMap<String, i128> {
    list.and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str().and_then(quantity_milli)?)))
        .collect()
}

// These totals describe application containers and native sidecars after startup.
// Pod-level declarations take precedence. Init peaks and overhead are excluded.
fn pod_resource(obj: &DynamicObject, cpu: bool, limit: bool) -> Option<i64> {
    let resource = if cpu { "cpu" } else { "memory" };
    let section = if limit { "limits" } else { "requests" };
    let parse = if cpu {
        parse_cpu_milli
    } else {
        parse_mem_bytes
    };
    let read = |resources: &Value| resources.get(section)?.get(resource)?.as_str().map(parse);
    if let Some(value) = obj.data.pointer("/spec/resources").and_then(read) {
        return Some(value);
    }
    let containers = obj.data.pointer("/spec/containers")?.as_array()?;
    let sidecars = obj
        .data
        .pointer("/spec/initContainers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("restartPolicy").and_then(Value::as_str) == Some("Always"));
    let mut total = 0i64;
    let mut any = false;
    for container in containers.iter().chain(sidecars) {
        match container.get("resources").and_then(read) {
            Some(value) => {
                total = total.checked_add(value)?;
                any = true;
            }
            None if limit => return None,
            None => {}
        }
    }
    any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_load_counts_pods_like_the_scheduler() {
        let pod = |spec: Value| {
            serde_json::from_value::<DynamicObject>(json!({
                "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"}, "spec": spec
            }))
            .unwrap()
        };
        let res = |cpu: &str| json!({"requests": {"cpu": cpu}, "limits": {"cpu": cpu}});
        // Sidecars add to the app containers; a plain init container only
        // raises the total when its step, plus earlier sidecars, is larger.
        let p = pod(json!({
            "nodeName": "n",
            "overhead": {"cpu": "100m", "memory": "64Mi"},
            "initContainers": [
                {"name": "side", "restartPolicy": "Always", "resources": res("200m")},
                {"name": "migrate", "resources": res("2")}
            ],
            "containers": [
                {"name": "app", "resources": res("500m")},
                {"name": "bare"}
            ]
        }));
        assert_eq!(pod_effective(&p, "requests").get("cpu"), Some(&2300));
        assert_eq!(
            pod_effective(&p, "requests").get("memory"),
            Some(&(64 * 1024 * 1024 * 1000))
        );
        assert_eq!(pod_effective(&p, "limits").get("memory"), None);
        let p = pod(json!({
            "resources": {"requests": {"cpu": "4"}},
            "containers": [{"resources": {"requests": {"cpu": "1", "nvidia.com/gpu": "1"}}}]
        }));
        let requests = pod_effective(&p, "requests");
        assert_eq!(requests.get("cpu"), Some(&4000));
        assert_eq!(requests.get("nvidia.com/gpu"), Some(&1000));
    }

    #[test]
    fn node_percentages_hold_for_petabyte_memory() {
        let node = serde_json::from_value::<DynamicObject>(json!({
            "apiVersion": "v1", "kind": "Node", "metadata": {"name": "n"},
            "status": {"allocatable": {"memory": "32Pi"}}
        }))
        .unwrap();
        let mut loads = NodeLoads::default();
        for name in ["a", "b"] {
            let pod = serde_json::from_value::<DynamicObject>(json!({
                "apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
                "spec": {"nodeName": "n", "containers": [{"resources": {
                    "requests": {"memory": "8Pi"}, "limits": {"memory": "12Pi"}
                }}]}
            }))
            .unwrap();
            loads.apply(name.into(), &pod);
        }
        let load = loads.snapshot().remove("n").unwrap();
        assert_eq!(
            MetricColumn::NodeRequest("memory").value(&node, None, Some(&load)),
            Some(50)
        );
        assert_eq!(
            MetricColumn::NodeLimit("memory").value(&node, None, Some(&load)),
            Some(75)
        );
    }

    #[test]
    fn node_metric_sources_parse() {
        assert_eq!(
            MetricColumn::parse("node-cpu-request"),
            Some(MetricColumn::NodeRequest("cpu"))
        );
        assert_eq!(
            MetricColumn::parse("node-limit:nvidia.com/gpu"),
            Some(MetricColumn::NodeLimit("nvidia.com/gpu"))
        );
        assert_eq!(MetricColumn::parse("node-request:"), None);
        assert_eq!(MetricColumn::parse("pod-request:cpu"), None);
        assert!(MetricColumn::NodeRequest("cpu").cpu());
        assert!(!MetricColumn::NodeRequest("cpu").supported("", "pods"));
    }

    #[test]
    fn resource_totals_preserve_missing_zero_and_pod_declarations() {
        let pod = |spec| {
            serde_json::from_value::<DynamicObject>(json!({
                "apiVersion": "v1", "kind": "Pod", "metadata": {"name": "test"}, "spec": spec
            }))
            .unwrap()
        };
        let missing = pod(json!({"containers": [{"name": "app"}]}));
        assert_eq!(MetricColumn::CpuRequest.value(&missing, None, None), None);
        let zero = pod(json!({"containers": [{"resources": {"requests": {"cpu": "0"}}}]}));
        assert_eq!(MetricColumn::CpuRequest.value(&zero, None, None), Some(0));
        assert_eq!(
            MetricColumn::CpuRequestUtilization.value(&zero, Some((0, 0)), None),
            None
        );
        let declared = pod(json!({
            "resources": {"requests": {"cpu": "2"}, "limits": {"memory": "1Gi"}},
            "overhead": {"cpu": "100m"},
            "containers": [{"resources": {"requests": {"cpu": "500m", "memory": "128Mi"}}}, {}]
        }));
        assert_eq!(
            MetricColumn::CpuRequest.value(&declared, None, None),
            Some(2000)
        );
        assert_eq!(
            MetricColumn::MemoryRequest.value(&declared, None, None),
            Some(128 * 1024 * 1024)
        );
        assert_eq!(
            MetricColumn::MemoryLimit.value(&declared, None, None),
            Some(1024 * 1024 * 1024)
        );
        assert_eq!(
            MetricColumn::MemoryLimitUtilization.value(&declared, Some((0, 0)), None),
            Some(0)
        );
        assert_eq!(MetricColumn::CpuLimit.value(&declared, None, None), None);
    }
}
