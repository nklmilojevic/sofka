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
            _ => return None,
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
                | Self::NodeMemoryTrend => plural == "nodes",
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
        pods: Option<usize>,
    ) -> Option<i64> {
        match self {
            Self::Cpu => usage.map(|v| v.0),
            Self::Memory => usage.map(|v| v.1),
            Self::NodePods => pods.map(|v| v as i64),
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
