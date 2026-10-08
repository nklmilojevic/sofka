use std::cmp::Reverse;

use anyhow::{Context, Result, ensure};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::APIResourceList;
use kube::Client;
use kube::core::Version;
use kube::core::discovery::v2::{ACCEPT_AGGREGATED_DISCOVERY_V2, APIGroupDiscoveryList};

use super::{ApiResource, Kind};

pub(super) struct Resource {
    pub kind: Kind,
    pub short_names: Vec<String>,
    pub listable: bool,
}

pub(super) struct Discovered {
    pub resources: Vec<Resource>,
    pub child_kinds: Vec<Kind>,
    pub skipped: Vec<String>,
    pub fallback: Option<String>,
}

// Keep short names from the wire documents. kube's Discovery conversion drops them.
pub(super) async fn discover(client: &Client) -> Result<Discovered> {
    let mut skipped = Vec::new();
    let mut fallback = None;
    let mut resources = match aggregated(client).await {
        Ok(Some(resources)) => resources,
        Ok(None) => legacy(client, &mut skipped).await?,
        Err(e) => {
            fallback = Some(format!(
                "Aggregated API discovery failed: {e}. Sofka read each API group separately."
            ));
            legacy(client, &mut skipped).await?
        }
    };
    // Select the most stable served version of each kind, including kinds absent
    // from the group's preferred version.
    resources.sort_by_cached_key(|r| {
        (
            r.kind.ar.group.clone(),
            r.kind.ar.kind.clone(),
            Reverse(Version::parse(&r.kind.ar.version).priority()),
        )
    });
    let child_kinds = child_candidates(&resources);
    resources
        .dedup_by(|a, b| a.kind.ar.group == b.kind.ar.group && a.kind.ar.kind == b.kind.ar.kind);
    Ok(Discovered {
        resources,
        child_kinds,
        skipped,
        fallback,
    })
}

async fn aggregated(client: &Client) -> Result<Option<Vec<Resource>>> {
    let groups = aggregated_endpoint(client, "/apis", "APIGroupList").await?;
    let core = aggregated_endpoint(client, "/api", "APIVersions").await?;
    let (Some(groups), Some(core)) = (groups, core) else {
        return Ok(None);
    };
    let mut resources = Vec::new();
    append_aggregated(&mut resources, groups)?;
    append_aggregated(&mut resources, core)?;
    Ok(Some(resources))
}

async fn aggregated_endpoint(
    client: &Client,
    path: &str,
    legacy_kind: &str,
) -> Result<Option<APIGroupDiscoveryList>> {
    let document: serde_json::Value = client
        .request(
            http::Request::get(path)
                .header(http::header::ACCEPT, ACCEPT_AGGREGATED_DISCOVERY_V2)
                .body(Vec::new())?,
        )
        .await?;
    // The typed list discards the response kind and defaults missing items to [].
    // Check the format first so a valid empty list does not trigger legacy discovery.
    if document["kind"].as_str() == Some(legacy_kind) {
        return Ok(None);
    }
    ensure!(
        document["kind"].as_str() == Some("APIGroupDiscoveryList"),
        "unexpected discovery response kind at {path}: {}",
        document["kind"]
    );
    Ok(Some(parse_aggregated(document)?))
}

// Some aggregated APIs (KubeVirt's subresources.kubevirt.io) send `"verbs": null`.
// kube's types default a missing list but reject null, so drop only null verbs and
// let any other malformed field fail the parse.
fn parse_aggregated(mut document: serde_json::Value) -> Result<APIGroupDiscoveryList> {
    fn drop_null_verbs(object: &mut serde_json::Value) {
        if let Some(map) = object.as_object_mut()
            && map.get("verbs").is_some_and(serde_json::Value::is_null)
        {
            map.remove("verbs");
        }
    }
    let resources = document
        .get_mut("items")
        .and_then(serde_json::Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get_mut("versions")?.as_array_mut())
        .flatten()
        .filter_map(|version| version.get_mut("resources")?.as_array_mut())
        .flatten();
    for resource in resources {
        drop_null_verbs(resource);
        if let Some(subresources) = resource
            .get_mut("subresources")
            .and_then(serde_json::Value::as_array_mut)
        {
            subresources.iter_mut().for_each(drop_null_verbs);
        }
    }
    Ok(serde_json::from_value(document)?)
}

fn append_aggregated(out: &mut Vec<Resource>, list: APIGroupDiscoveryList) -> Result<()> {
    for group in list.items {
        let name = group.metadata.and_then(|m| m.name).unwrap_or_default();
        ensure!(!group.versions.is_empty(), "empty API group: {name}");
        for version in group.versions {
            let version_name = version.version.unwrap_or_default();
            for resource in version.resources {
                let plural = resource.resource.unwrap_or_default();
                // A group that only serves subresources (subresources.kubevirt.io) lists
                // the parent with no verbs. Legacy discovery has no such entry, and it
                // must not claim the bare name from the group that serves the resource.
                if plural.contains('/') || resource.verbs.is_empty() {
                    continue;
                }
                out.push(Resource {
                    kind: Kind {
                        ar: ApiResource {
                            group: name.clone(),
                            version: version_name.clone(),
                            api_version: api_version(&name, &version_name),
                            kind: resource
                                .response_kind
                                .and_then(|k| k.kind)
                                .unwrap_or_default(),
                            plural,
                        },
                        scalable: resource.subresources.iter().any(|s| {
                            s.subresource.as_deref() == Some("scale")
                                && s.verbs.iter().any(|v| v == "patch")
                        }),
                        namespaced: resource.scope.as_deref() == Some("Namespaced"),
                    },
                    listable: resource.verbs.iter().any(|v| v == "list"),
                    short_names: resource.short_names,
                });
            }
        }
    }
    Ok(())
}

async fn legacy(client: &Client, skipped: &mut Vec<String>) -> Result<Vec<Resource>> {
    let mut resources = Vec::new();
    for group in client
        .list_api_groups()
        .await
        .context("running API discovery")?
        .groups
    {
        if group.versions.is_empty() {
            skipped.push(format!(
                "API discovery could not read {}: the group has no versions",
                group.name
            ));
            continue;
        }
        for version in group.versions {
            let gv = version.group_version;
            let result = match client.list_api_group_resources(&gv).await {
                Ok(list) => append_legacy(&mut resources, list),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = result {
                skipped.push(format!("API discovery could not read {gv}: {e}"));
            }
        }
    }
    let core = client
        .list_core_api_versions()
        .await
        .context("running API discovery")?;
    ensure!(!core.versions.is_empty(), "empty core API group");
    for version in core.versions {
        let list = client
            .list_core_api_resources(&version)
            .await
            .with_context(|| format!("running API discovery: reading core API group {version}"))?;
        append_legacy(&mut resources, list)
            .with_context(|| format!("running API discovery: reading core API group {version}"))?;
    }
    Ok(resources)
}

fn append_legacy(out: &mut Vec<Resource>, list: APIResourceList) -> Result<()> {
    let gv: kube::core::GroupVersion = list.group_version.parse()?;
    let scalable: std::collections::HashSet<_> = list
        .resources
        .iter()
        .filter(|r| r.verbs.iter().any(|v| v == "patch"))
        .filter_map(|r| r.name.strip_suffix("/scale").map(str::to_owned))
        .collect();
    for resource in list.resources {
        if resource.name.contains('/') {
            continue;
        }
        out.push(Resource {
            kind: Kind {
                ar: ApiResource {
                    group: resource.group.unwrap_or_else(|| gv.group.clone()),
                    version: resource.version.unwrap_or_else(|| gv.version.clone()),
                    api_version: gv.api_version(),
                    kind: resource.kind,
                    plural: resource.name.clone(),
                },
                namespaced: resource.namespaced,
                scalable: scalable.contains(&resource.name),
            },
            listable: resource.verbs.iter().any(|v| v == "list"),
            short_names: resource.short_names.unwrap_or_default(),
        });
    }
    Ok(())
}

fn api_version(group: &str, version: &str) -> String {
    if group.is_empty() {
        version.to_string()
    } else {
        format!("{group}/{version}")
    }
}

fn child_candidates(resources: &[Resource]) -> Vec<Kind> {
    let mut kinds: Vec<_> = resources
        .iter()
        .filter(|r| r.listable && r.kind.namespaced && !r.kind.ar.plural.contains('/'))
        .map(|r| r.kind.clone())
        .collect();
    kinds.sort_by_cached_key(|k| {
        (
            k.ar.group.clone(),
            k.ar.plural.clone(),
            Reverse(Version::parse(&k.ar.version).priority()),
        )
    });
    kinds.dedup_by(|a, b| a.ar.group == b.ar.group && a.ar.plural == b.ar.plural);
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scale_capability_requires_patch_in_each_discovery_version() {
        for aggregated in [false, true] {
            let mut resources = Vec::new();
            for (version, verbs, expected) in [
                ("v1", vec!["get", "patch"], true),
                ("v2", vec!["get", "update"], false),
            ] {
                if aggregated {
                    append_aggregated(&mut resources, serde_json::from_value(json!({
                        "items":[{"metadata":{"name":"example.com"},"versions":[{
                            "version":version,"resources":[
                                {"resource":"todoapps","responseKind":{"kind":"TodoApp"},"scope":"Namespaced","verbs":["list"],
                                 "subresources":[{"subresource":"scale","verbs":verbs}]},
                                {"resource":"others","responseKind":{"kind":"Other"},"scope":"Cluster","verbs":["list","patch"]}
                            ]
                        }]}]
                    })).unwrap()).unwrap();
                } else {
                    append_legacy(&mut resources, serde_json::from_value(json!({
                        "groupVersion":format!("example.com/{version}"),"resources":[
                            {"name":"todoapps/scale","kind":"Scale","namespaced":true,"verbs":verbs},
                            {"name":"todoapps","kind":"TodoApp","namespaced":true,"verbs":["list"]},
                            {"name":"others","kind":"Other","namespaced":false,"verbs":["list","patch"]}
                        ]
                    })).unwrap()).unwrap();
                }
                let kind = &resources[resources.len() - 2].kind;
                assert_eq!(kind.ar.version, version);
                assert_eq!(kind.scalable, expected);
                assert!(!resources.last().unwrap().kind.scalable);
            }
            assert_eq!(resources.len(), 4);
        }
    }

    #[test]
    fn children_use_listable_namespaced_resources_and_one_version() {
        let mut resources = Vec::new();
        for version in ["v1beta1", "v1"] {
            append_legacy(&mut resources, serde_json::from_value(json!({
                "groupVersion": format!("example.io/{version}"),
                "resources": [
                    {"name":"widgets", "kind":"Widget", "namespaced":true, "verbs":["list"]},
                    {"name":"widgets/status", "kind":"Widget", "namespaced":true, "verbs":["list"]},
                    {"name":"global", "kind":"Global", "namespaced":false, "verbs":["list"]},
                    {"name":"writeonly", "kind":"WriteOnly", "namespaced":true, "verbs":["create"]}
                ]
            })).unwrap()).unwrap();
        }
        let kinds = child_candidates(&resources);
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].ar.plural, "widgets");
        assert_eq!(kinds[0].ar.version, "v1");
    }

    #[test]
    fn aggregated_children_require_the_list_verb() {
        let mut resources = Vec::new();
        append_aggregated(&mut resources, serde_json::from_value(json!({
            "items": [{"metadata":{"name":"example.io"}, "versions":[{
                "version":"v1", "resources":[
                    {"resource":"widgets", "responseKind":{"kind":"Widget"}, "scope":"Namespaced", "verbs":["get","list"]},
                    {"resource":"writeonly", "responseKind":{"kind":"WriteOnly"}, "scope":"Namespaced", "verbs":["create"]},
                    {"resource":"globals", "responseKind":{"kind":"Global"}, "scope":"Cluster", "verbs":["list"]}
                ]
            }]}]
        })).unwrap()).unwrap();
        let kinds = child_candidates(&resources);
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].ar.plural, "widgets");
    }

    #[test]
    fn aggregated_discovery_skips_resources_with_null_verbs() {
        let mut resources = Vec::new();
        append_aggregated(&mut resources, parse_aggregated(json!({
            "kind": "APIGroupDiscoveryList",
            "items": [{"metadata":{"name":"subresources.kubevirt.io"}, "versions":[{
                "version":"v1", "resources":[
                    {"resource":"virtualmachineinstances", "responseKind":{"kind":"VirtualMachineInstance"}, "scope":"Namespaced", "verbs":null,
                     "subresources":[{"subresource":"console", "verbs":null}]},
                    {"resource":"expand-vm-spec", "responseKind":{"kind":"VirtualMachine"}, "scope":"Namespaced", "verbs":null},
                    {"resource":"guestfs", "responseKind":{"kind":"Guestfs"}, "scope":"Namespaced", "verbs":["get"]}
                ]
            }]}]
        })).unwrap()).unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].kind.ar.plural, "guestfs");
    }

    #[test]
    fn aggregated_discovery_rejects_other_null_lists() {
        for document in [
            json!({"kind": "APIGroupDiscoveryList", "items": null}),
            json!({"kind": "APIGroupDiscoveryList", "items": [
                {"metadata": {"name": "example.io"}, "versions": [{"version": "v1", "resources": null}]}
            ]}),
        ] {
            assert!(parse_aggregated(document).is_err());
        }
    }
}
