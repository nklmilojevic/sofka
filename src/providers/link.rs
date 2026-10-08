//! Log providers that open the selection in a web log UI instead of querying
//! a backend from sofka.
//!
//! `type = "gcp"` builds a Cloud Logging Logs Explorer link for GKE clusters,
//! and is picked automatically when the kubeconfig cluster is named
//! `gke_<project>_<location>_<cluster>` and no other log provider is set.
//! `type = "link"` fills a URL template for any other log UI:
//!
//! ```toml
//! [providers.logs]
//! type = "link"
//! url = "https://grafana.example.com/explore?left=...{namespace}...{pod}..."
//! lookback = "1h"
//! ```

use serde_json::Value;

/// A compiled link provider. Cheap to clone.
#[derive(Clone, Debug, PartialEq)]
pub enum LogLink {
    /// Cloud Logging for GKE clusters.
    Gcp { lookback_secs: i64 },
    /// A URL template with `{placeholder}`s filled from the selection.
    Template { url: String, lookback: String },
}

/// One pod-selector requirement of a workload or service.
#[derive(Clone, Debug, PartialEq)]
pub enum Requirement {
    In(String, Vec<String>),
    NotIn(String, Vec<String>),
    Exists(String),
    DoesNotExist(String),
}

/// The selected row, as a link provider sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkTarget {
    pub context: String,
    /// Kubeconfig cluster name.
    pub cluster: String,
    /// Resource plural, e.g. `deployments`.
    pub resource: String,
    pub namespace: String,
    pub name: String,
    /// Set when the link is for one container of a pod.
    pub container: Option<String>,
    /// The pod selector of a workload or service. Empty for other kinds.
    pub selector: Vec<Requirement>,
}

impl LogLink {
    /// The provider for a cluster with no `[providers.logs]`: Cloud Logging
    /// when the kubeconfig cluster name is a GKE one.
    pub fn detect(cluster: &str) -> Option<Self> {
        gke_cluster(cluster).map(|_| LogLink::Gcp {
            lookback_secs: super::parse_lookback(super::DEFAULT_LOOKBACK).unwrap_or(3600),
        })
    }

    /// The log UI link for `target`, or why there is none.
    pub fn url(&self, target: &LinkTarget) -> Result<String, String> {
        match self {
            LogLink::Gcp { lookback_secs } => gcp_url(target, *lookback_secs),
            LogLink::Template { url, lookback } => Ok(fill_template(url, target, lookback)),
        }
    }
}

/// The pod selector of `resource`, read from the object JSON. `None` for
/// kinds without one; `Some(empty)` when the selector is empty.
pub fn selector(resource: &str, obj: &Value) -> Option<Vec<Requirement>> {
    let value = obj.pointer("/spec/selector")?;
    match resource {
        "services" => {
            let mut labels: Vec<(&String, &str)> = value
                .as_object()?
                .iter()
                .filter_map(|(key, value)| Some((key, value.as_str()?)))
                .collect();
            labels.sort();
            Some(
                labels
                    .into_iter()
                    .map(|(key, value)| Requirement::In(key.clone(), vec![value.into()]))
                    .collect(),
            )
        }
        "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
            let selector: k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector =
                serde_json::from_value(value.clone()).ok()?;
            let mut requirements: Vec<Requirement> = selector
                .match_labels
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| Requirement::In(key, vec![value]))
                .collect();
            for expression in selector.match_expressions.unwrap_or_default() {
                let values = expression.values.unwrap_or_default();
                requirements.push(match expression.operator.as_str() {
                    "In" => Requirement::In(expression.key, values),
                    "NotIn" => Requirement::NotIn(expression.key, values),
                    "Exists" => Requirement::Exists(expression.key),
                    "DoesNotExist" => Requirement::DoesNotExist(expression.key),
                    _ => return None,
                });
            }
            Some(requirements)
        }
        _ => None,
    }
}

/// `(project, location, cluster)` of a gcloud-written GKE kubeconfig cluster
/// name, `gke_<project>_<location>_<cluster>`. None of the parts can
/// contain `_`.
fn gke_cluster(name: &str) -> Option<(&str, &str, &str)> {
    let mut parts = name.strip_prefix("gke_")?.split('_');
    let (project, location, cluster) = (parts.next()?, parts.next()?, parts.next()?);
    let valid = parts.next().is_none()
        && !project.is_empty()
        && !location.is_empty()
        && !cluster.is_empty();
    valid.then_some((project, location, cluster))
}

fn gcp_url(target: &LinkTarget, lookback_secs: i64) -> Result<String, String> {
    let (project, location, cluster) = gke_cluster(&target.cluster).ok_or_else(|| {
        format!(
            "GCP logs need a gcloud cluster name like gke_<project>_<location>_<cluster>, not {:?}",
            target.cluster
        )
    })?;
    let mut query = vec![
        if target.resource == "nodes" {
            r#"resource.type="k8s_node""#.to_string()
        } else {
            r#"resource.type="k8s_container""#.to_string()
        },
        format!("resource.labels.project_id={}", quote(project)),
        format!("resource.labels.location={}", quote(location)),
        format!("resource.labels.cluster_name={}", quote(cluster)),
    ];
    match target.resource.as_str() {
        "nodes" => query.push(format!("resource.labels.node_name={}", quote(&target.name))),
        "namespaces" => query.push(format!(
            "resource.labels.namespace_name={}",
            quote(&target.name)
        )),
        resource => {
            query.push(format!(
                "resource.labels.namespace_name={}",
                quote(&target.namespace)
            ));
            match resource {
                "pods" => {
                    query.push(format!("resource.labels.pod_name={}", quote(&target.name)));
                    if let Some(container) = &target.container {
                        query.push(format!(
                            "resource.labels.container_name={}",
                            quote(container)
                        ));
                    }
                }
                // A CronJob has no selector, but GKE labels each of its pods'
                // log entries with the CronJob name.
                "cronjobs" => {
                    query.push(
                        r#"labels."logging.gke.io/top_level_controller_type"="CronJob""#.into(),
                    );
                    query.push(format!(
                        r#"labels."logging.gke.io/top_level_controller_name"={}"#,
                        quote(&target.name)
                    ));
                }
                "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs"
                | "services" => {
                    if target.selector.is_empty() {
                        return Err(format!("{resource}/{} has no pod selector", target.name));
                    }
                    query.extend(target.selector.iter().map(gcp_label_term));
                }
                _ => {
                    return Err(
                        "GCP logs cover pods, workloads, services, cronjobs, namespaces, and nodes"
                            .into(),
                    );
                }
            }
        }
    }
    Ok(format!(
        "https://console.cloud.google.com/logs/query;query={};duration={}?project={}",
        encode(&query.join("\n")),
        iso_duration(lookback_secs),
        encode(project)
    ))
}

/// A pod-selector requirement as a Cloud Logging filter term. GKE records pod
/// label `a.b/c` as log label `k8s-pod/a_b/c`.
fn gcp_label_term(requirement: &Requirement) -> String {
    let field = |key: &str| format!(r#"labels."k8s-pod/{}""#, key.replace('.', "_"));
    let any = |key: &str, values: &[String]| {
        values
            .iter()
            .map(|value| format!("{}={}", field(key), quote(value)))
            .collect::<Vec<_>>()
            .join(" OR ")
    };
    match requirement {
        Requirement::In(key, values) if values.len() == 1 => any(key, values),
        Requirement::In(key, values) => format!("({})", any(key, values)),
        Requirement::NotIn(key, values) => format!("NOT ({})", any(key, values)),
        Requirement::Exists(key) => format!("{}:*", field(key)),
        Requirement::DoesNotExist(key) => format!("NOT {}:*", field(key)),
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An ISO 8601 duration in the largest whole unit, e.g. `PT1H`, `P2D`.
fn iso_duration(secs: i64) -> String {
    if secs % 86_400 == 0 {
        format!("P{}D", secs / 86_400)
    } else if secs % 3600 == 0 {
        format!("PT{}H", secs / 3600)
    } else if secs % 60 == 0 {
        format!("PT{}M", secs / 60)
    } else {
        format!("PT{secs}S")
    }
}

/// Replace each known `{placeholder}` with its URL-encoded value. Unknown
/// braces are left as they are, so templates can carry literal JSON.
fn fill_template(template: &str, target: &LinkTarget, lookback: &str) -> String {
    let pod = if target.resource == "pods" {
        target.name.as_str()
    } else {
        ""
    };
    let values = [
        ("{context}", target.context.as_str()),
        ("{cluster}", target.cluster.as_str()),
        ("{namespace}", target.namespace.as_str()),
        ("{kind}", target.resource.as_str()),
        ("{name}", target.name.as_str()),
        ("{pod}", pod),
        ("{container}", target.container.as_deref().unwrap_or("")),
        ("{lookback}", lookback),
    ];
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'scan: while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        for (placeholder, value) in values {
            if let Some(after) = rest.strip_prefix(placeholder) {
                out.push_str(&encode(value));
                rest = after;
                continue 'scan;
            }
        }
        out.push('{');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

/// Percent-encode every byte outside the RFC 3986 unreserved set.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// The decoded Cloud Logging query of a GCP link.
    fn gcp_query(target: &LinkTarget) -> String {
        let url = LogLink::Gcp {
            lookback_secs: 3600,
        }
        .url(target)
        .unwrap();
        let query = url
            .strip_prefix("https://console.cloud.google.com/logs/query;query=")
            .unwrap();
        decode(&query[..query.find(';').unwrap()])
    }

    fn gke(resource: &str, namespace: &str, name: &str) -> LinkTarget {
        LinkTarget {
            context: "prod".into(),
            cluster: "gke_my-project_asia-southeast1_main".into(),
            resource: resource.into(),
            namespace: namespace.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    const CLUSTER_TERMS: &str = "resource.labels.project_id=\"my-project\"\n\
        resource.labels.location=\"asia-southeast1\"\n\
        resource.labels.cluster_name=\"main\"";

    #[test]
    fn detects_only_gcloud_cluster_names() {
        assert!(LogLink::detect("gke_my-project_europe-west1-b_main").is_some());
        assert!(LogLink::detect("gke_p_l").is_none());
        assert!(LogLink::detect("gke_p_l_c_extra").is_none());
        assert!(LogLink::detect("gke__l_c").is_none());
        assert!(LogLink::detect("arn:aws:eks:eu-west-1:123:cluster/prod").is_none());
        assert!(LogLink::detect("").is_none());
    }

    #[test]
    fn gcp_pod_link_has_project_duration_and_pod_filter() {
        let url = LogLink::Gcp {
            lookback_secs: 3600,
        }
        .url(&gke("pods", "web", "api-7d9f"))
        .unwrap();
        assert!(url.ends_with(";duration=PT1H?project=my-project"), "{url}");
        assert_eq!(
            gcp_query(&gke("pods", "web", "api-7d9f")),
            format!(
                "resource.type=\"k8s_container\"\n{CLUSTER_TERMS}\n\
                 resource.labels.namespace_name=\"web\"\n\
                 resource.labels.pod_name=\"api-7d9f\""
            )
        );
    }

    #[test]
    fn gcp_container_link_adds_the_container() {
        let mut target = gke("pods", "web", "api-7d9f");
        target.container = Some("sidecar".into());
        assert!(gcp_query(&target).ends_with("\nresource.labels.container_name=\"sidecar\""));
    }

    #[test]
    fn gcp_node_and_namespace_links_filter_on_their_own_fields() {
        assert_eq!(
            gcp_query(&gke("nodes", "", "node-1")),
            format!(
                "resource.type=\"k8s_node\"\n{CLUSTER_TERMS}\n\
                 resource.labels.node_name=\"node-1\""
            )
        );
        assert_eq!(
            gcp_query(&gke("namespaces", "", "web")),
            format!(
                "resource.type=\"k8s_container\"\n{CLUSTER_TERMS}\n\
                 resource.labels.namespace_name=\"web\""
            )
        );
    }

    #[test]
    fn gcp_cronjob_link_uses_the_top_level_controller() {
        assert!(gcp_query(&gke("cronjobs", "batch", "nightly")).ends_with(
            "\nlabels.\"logging.gke.io/top_level_controller_type\"=\"CronJob\"\n\
             labels.\"logging.gke.io/top_level_controller_name\"=\"nightly\""
        ));
    }

    #[test]
    fn gcp_workload_link_translates_every_selector_operator() {
        let obj = json!({"spec": {"selector": {
            "matchLabels": {"app.kubernetes.io/name": "api"},
            "matchExpressions": [
                {"key": "tier", "operator": "In", "values": ["web", "edge"]},
                {"key": "track", "operator": "NotIn", "values": ["canary"]},
                {"key": "team", "operator": "Exists"},
                {"key": "legacy", "operator": "DoesNotExist"},
            ],
        }}});
        let mut target = gke("deployments", "web", "api");
        target.selector = selector("deployments", &obj).unwrap();
        assert!(gcp_query(&target).ends_with(
            "\nresource.labels.namespace_name=\"web\"\n\
             labels.\"k8s-pod/app_kubernetes_io/name\"=\"api\"\n\
             (labels.\"k8s-pod/tier\"=\"web\" OR labels.\"k8s-pod/tier\"=\"edge\")\n\
             NOT (labels.\"k8s-pod/track\"=\"canary\")\n\
             labels.\"k8s-pod/team\":*\n\
             NOT labels.\"k8s-pod/legacy\":*"
        ));
    }

    #[test]
    fn gcp_service_link_uses_the_service_selector() {
        let obj = json!({"spec": {"selector": {"app": "api", "tier": "web"}}});
        let mut target = gke("services", "web", "api");
        target.selector = selector("services", &obj).unwrap();
        assert!(
            gcp_query(&target)
                .ends_with("\nlabels.\"k8s-pod/app\"=\"api\"\nlabels.\"k8s-pod/tier\"=\"web\"")
        );
    }

    #[test]
    fn gcp_link_refuses_workloads_without_a_selector_and_other_kinds() {
        let link = LogLink::Gcp {
            lookback_secs: 3600,
        };
        assert!(link.url(&gke("deployments", "web", "api")).is_err());
        assert!(link.url(&gke("configmaps", "web", "settings")).is_err());
        let mut target = gke("pods", "web", "api");
        target.cluster = "prod".into();
        assert!(link.url(&target).unwrap_err().contains("gke_<project>"));
    }

    #[test]
    fn gcp_filter_values_are_quoted() {
        let query = gcp_query(&gke("pods", "web", r#"a"b\c"#));
        assert!(
            query.ends_with(r#"resource.labels.pod_name="a\"b\\c""#),
            "{query}"
        );
    }

    #[test]
    fn iso_duration_uses_the_largest_whole_unit() {
        assert_eq!(iso_duration(90), "PT90S");
        assert_eq!(iso_duration(900), "PT15M");
        assert_eq!(iso_duration(7200), "PT2H");
        assert_eq!(iso_duration(172_800), "P2D");
    }

    #[test]
    fn template_fills_and_encodes_placeholders() {
        let link = LogLink::Template {
            url: "https://logs.example.com/?q={\"ns\":\"{namespace}\"}&c={cluster}&k={kind}&n={name}&p={pod}&ct={container}&ctx={context}&since={lookback}&x={unknown}".into(),
            lookback: "1h".into(),
        };
        let mut target = gke("pods", "web", "api 1");
        target.container = Some("app".into());
        assert_eq!(
            link.url(&target).unwrap(),
            "https://logs.example.com/?q={\"ns\":\"web\"}&c=gke_my-project_asia-southeast1_main&k=pods&n=api%201&p=api%201&ct=app&ctx=prod&since=1h&x={unknown}"
        );
    }

    #[test]
    fn template_pod_is_empty_for_other_kinds() {
        let link = LogLink::Template {
            url: "{name}|{pod}|{container}".into(),
            lookback: "1h".into(),
        };
        assert_eq!(
            link.url(&gke("deployments", "web", "api")).unwrap(),
            "api||"
        );
    }

    #[test]
    fn selector_is_none_for_kinds_without_one() {
        let obj = json!({"spec": {"selector": {"app": "api"}}});
        assert_eq!(selector("pods", &obj), None);
        assert_eq!(selector("deployments", &json!({})), None);
    }
}
