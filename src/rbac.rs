//! Subjects and rule sources from explicit RBAC bindings.

use std::collections::BTreeMap;

use k8s_openapi::api::rbac::v1::{ClusterRoleBinding, PolicyRule, RoleBinding};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    User(String),
    Group(String),
    ServiceAccount { namespace: String, name: String },
}

impl Subject {
    pub fn parse(input: &str) -> Option<Self> {
        let (kind, name) = input.split_once(':')?;
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return None;
        }
        match kind {
            "u" => Some(Self::User(name.into())),
            "g" => Some(Self::Group(name.into())),
            "s" => {
                let (namespace, name) = name.split_once('/')?;
                if namespace.is_empty() || name.is_empty() || name.contains('/') {
                    return None;
                }
                Some(Self::ServiceAccount {
                    namespace: namespace.into(),
                    name: name.into(),
                })
            }
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::User(name) => format!("User {name}"),
            Self::Group(name) => format!("Group {name}"),
            Self::ServiceAccount { namespace, name } => {
                format!("ServiceAccount {namespace}/{name}")
            }
        }
    }

    fn matches(
        &self,
        subject: &k8s_openapi::api::rbac::v1::Subject,
        binding_ns: Option<&str>,
    ) -> bool {
        match self {
            Self::User(name) => subject.kind == "User" && subject.name == *name,
            Self::Group(name) => subject.kind == "Group" && subject.name == *name,
            Self::ServiceAccount { namespace, name } => {
                subject.kind == "ServiceAccount"
                    && subject.name == *name
                    && subject
                        .namespace
                        .as_deref()
                        .filter(|ns| !ns.is_empty())
                        .or(binding_ns)
                        == Some(namespace.as_str())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectKind {
    User,
    Group,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectRef {
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

impl ObjectRef {
    pub fn label(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{} {ns}/{}", self.kind, self.name),
            None => format!("{} {}", self.kind, self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub binding: ObjectRef,
    pub role: ObjectRef,
}

#[derive(Default)]
pub struct Bindings {
    pub roles: Vec<RoleBinding>,
    pub cluster_roles: Vec<ClusterRoleBinding>,
}

impl Bindings {
    pub fn subjects(&self, kind: SubjectKind) -> Vec<(Subject, usize)> {
        let mut counts = BTreeMap::new();
        for subjects in self
            .roles
            .iter()
            .map(|rb| &rb.subjects)
            .chain(self.cluster_roles.iter().map(|rb| &rb.subjects))
        {
            let mut unique = std::collections::BTreeSet::new();
            for s in subjects.iter().flatten() {
                let subject = match (kind, s.kind.as_str()) {
                    (SubjectKind::User, "User") => Subject::User(s.name.clone()),
                    (SubjectKind::Group, "Group") => Subject::Group(s.name.clone()),
                    _ => continue,
                };
                unique.insert(subject);
            }
            for subject in unique {
                *counts.entry(subject).or_insert(0) += 1;
            }
        }
        counts.into_iter().collect()
    }

    pub fn grants(&self, subject: &Subject) -> Vec<Grant> {
        let mut grants = Vec::new();
        for rb in &self.roles {
            let ns = rb.metadata.namespace.as_deref();
            if rb.subjects.iter().flatten().any(|s| subject.matches(s, ns)) {
                grants.push(Grant {
                    binding: ObjectRef {
                        kind: "RoleBinding".into(),
                        namespace: rb.metadata.namespace.clone(),
                        name: rb.metadata.name.clone().unwrap_or_default(),
                    },
                    role: ObjectRef {
                        kind: rb.role_ref.kind.clone(),
                        namespace: (rb.role_ref.kind == "Role")
                            .then(|| ns.unwrap_or_default().into()),
                        name: rb.role_ref.name.clone(),
                    },
                });
            }
        }
        for rb in &self.cluster_roles {
            if rb
                .subjects
                .iter()
                .flatten()
                .any(|s| subject.matches(s, None))
            {
                grants.push(Grant {
                    binding: ObjectRef {
                        kind: "ClusterRoleBinding".into(),
                        namespace: None,
                        name: rb.metadata.name.clone().unwrap_or_default(),
                    },
                    role: ObjectRef {
                        kind: rb.role_ref.kind.clone(),
                        namespace: None,
                        name: rb.role_ref.name.clone(),
                    },
                });
            }
        }
        grants.sort_by(|a, b| a.binding.cmp(&b.binding));
        grants
    }
}

pub fn rule_lines(rules: &[PolicyRule], namespaced_binding: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        lines.push(format!("Rule {}", i + 1));
        lines.push(format!("  Verbs: {}", rule.verbs.join(", ")));
        if let Some(groups) = &rule.api_groups {
            lines.push(format!(
                "  API groups: {}",
                groups
                    .iter()
                    .map(|g| if g.is_empty() { "core" } else { g })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(resources) = &rule.resources {
            lines.push(format!("  Resources: {}", resources.join(", ")));
        }
        if let Some(names) = &rule.resource_names
            && !names.is_empty()
        {
            lines.push(format!(
                "  Resource names (restricted): {}",
                names.join(", ")
            ));
        }
        if let Some(urls) = &rule.non_resource_urls {
            lines.push(format!("  Non-resource URLs: {}", urls.join(", ")));
            if namespaced_binding {
                lines.push("  Not granted by this namespaced binding.".into());
            }
        }
    }
    if rules.is_empty() {
        lines.push("No rules in this role.".into());
    }
    lines
}

const MATRIX_VERBS: [(&str, &str); 8] = [
    ("get", "GET"),
    ("list", "LIST"),
    ("watch", "WATCH"),
    ("create", "CREATE"),
    ("update", "UPDATE"),
    ("patch", "PATCH"),
    ("delete", "DELETE"),
    ("deletecollection", "DEL-LIST"),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Resource { group: String, resource: String },
    Url(String),
}

/// Rules as one row per resource and verb columns. Rows with the same
/// resource and resource names merge their verbs across rules.
///
/// `scopes` maps discovered `(group, plural)` pairs to whether they are
/// namespaced. Under a namespaced scope, cluster-scoped resources and
/// non-resource URLs are not granted. A `*` group is checked against every
/// discovered group with that plural. Wildcard and undiscovered resources keep
/// their ticks.
pub fn rule_matrix(
    rules: &[PolicyRule],
    namespaced_binding: bool,
    scopes: &std::collections::HashMap<(String, String), bool>,
) -> Vec<String> {
    if rules.is_empty() {
        return vec!["No rules in this role.".into()];
    }
    let mut rows: BTreeMap<(Target, Vec<String>), std::collections::BTreeSet<&str>> =
        BTreeMap::new();
    for rule in rules {
        let verbs = rule.verbs.iter().map(String::as_str);
        let mut names = rule.resource_names.clone().unwrap_or_default();
        names.sort();
        for resource in rule.resources.iter().flatten() {
            for group in rule.api_groups.iter().flatten() {
                let target = Target::Resource {
                    group: group.clone(),
                    resource: resource.clone(),
                };
                rows.entry((target, names.clone()))
                    .or_default()
                    .extend(verbs.clone());
            }
        }
        for url in rule.non_resource_urls.iter().flatten() {
            rows.entry((Target::Url(url.clone()), Vec::new()))
                .or_default()
                .extend(verbs.clone());
        }
    }
    let with_names = rows.keys().any(|(_, names)| !names.is_empty());
    let mut header = vec!["RESOURCE".to_string(), "API GROUP".to_string()];
    header.extend(MATRIX_VERBS.iter().map(|(_, label)| label.to_string()));
    header.push("OTHER".into());
    if with_names {
        header.push("RESOURCE NAMES".into());
    }
    let mut table = vec![header];
    let mut any_ungranted = false;
    for ((target, names), verbs) in &rows {
        let (resource, group) = match target {
            Target::Resource { group, resource } if names.is_empty() => (
                resource.clone(),
                if group.is_empty() { "core" } else { group }.to_string(),
            ),
            Target::Resource { group, resource } => (
                format!("{resource} (named)"),
                if group.is_empty() { "core" } else { group }.to_string(),
            ),
            Target::Url(url) => (url.clone(), "non-resource URL".into()),
        };
        let (ungranted, namespaced_groups_only) = match target {
            _ if !namespaced_binding => (false, false),
            Target::Url(_) => (true, false),
            Target::Resource { group, resource } => {
                let plural = resource.split('/').next().unwrap_or_default();
                if group == "*" {
                    let namespaced: Vec<bool> = scopes
                        .iter()
                        .filter(|((_, candidate), _)| candidate == plural)
                        .map(|(_, namespaced)| *namespaced)
                        .collect();
                    let cluster = namespaced.iter().any(|namespaced| !namespaced);
                    (
                        cluster && !namespaced.contains(&true),
                        cluster && namespaced.contains(&true),
                    )
                } else {
                    let key = (group.clone(), plural.to_string());
                    (scopes.get(&key) == Some(&false), false)
                }
            }
        };
        any_ungranted |= ungranted;
        let all = verbs.contains("*");
        let mut row = vec![resource, group];
        row.extend(MATRIX_VERBS.iter().map(|(verb, _)| {
            if !ungranted && (all || verbs.contains(verb)) {
                "✓"
            } else {
                "·"
            }
            .to_string()
        }));
        let other: Vec<&str> = verbs
            .iter()
            .copied()
            .filter(|verb| !MATRIX_VERBS.iter().any(|(known, _)| known == verb))
            .collect();
        row.push(if ungranted {
            "not granted in a namespace".into()
        } else if namespaced_groups_only {
            other
                .into_iter()
                .chain(["namespaced API groups only"])
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            other.join(", ")
        });
        if with_names {
            row.push(names.join(", "));
        }
        table.push(row);
    }
    let widths: Vec<usize> = (0..table[0].len())
        .map(|col| {
            table
                .iter()
                .map(|row| unicode_width::UnicodeWidthStr::width(row[col].as_str()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut lines = Vec::new();
    if with_names {
        lines.push("Rows marked (named) apply only to the objects in RESOURCE NAMES.".into());
    }
    if any_ungranted {
        lines.push(
            "Cluster-scoped resources and non-resource URLs are not granted in a namespace.".into(),
        );
    }
    lines.extend(table.iter().map(|row| {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(&widths) {
            line.push_str(cell);
            let pad = width - unicode_width::UnicodeWidthStr::width(cell.as_str());
            line.push_str(&" ".repeat(pad + 2));
        }
        line.trim_end().to_string()
    }));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(groups: &[&str], resources: &[&str], names: &[&str], verbs: &[&str]) -> PolicyRule {
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        PolicyRule {
            api_groups: Some(strings(groups)),
            resources: Some(strings(resources)),
            resource_names: (!names.is_empty()).then(|| strings(names)),
            verbs: strings(verbs),
            non_resource_urls: None,
        }
    }

    #[test]
    fn matrix_merges_verbs_per_resource_and_aligns_columns() {
        let rules = [
            rule(
                &["", "apps"],
                &["pods", "deployments"],
                &[],
                &["get", "list"],
            ),
            rule(&["apps"], &["deployments"], &[], &["watch", "escalate"]),
            rule(&["batch"], &["jobs"], &[], &["*"]),
        ];
        let lines = rule_matrix(&rules, false, &Default::default());
        assert_eq!(
            lines,
            vec![
                "RESOURCE     API GROUP  GET  LIST  WATCH  CREATE  UPDATE  PATCH  DELETE  DEL-LIST  OTHER",
                "deployments  core       ✓    ✓     ·      ·       ·       ·      ·       ·",
                "pods         core       ✓    ✓     ·      ·       ·       ·      ·       ·",
                "deployments  apps       ✓    ✓     ✓      ·       ·       ·      ·       ·         escalate",
                "pods         apps       ✓    ✓     ·      ·       ·       ·      ·       ·",
                "jobs         batch      ✓    ✓     ✓      ✓       ✓       ✓      ✓       ✓         *",
            ]
        );
    }

    #[test]
    fn matrix_keeps_resource_names_and_non_resource_urls_apart() {
        let mut url = rule(&[], &[], &[], &["get", "post"]);
        url.api_groups = None;
        url.resources = None;
        url.non_resource_urls = Some(vec!["/healthz".into()]);
        let rules = [
            rule(&[""], &["secrets"], &["b", "a"], &["get"]),
            rule(&[""], &["secrets"], &[], &["list"]),
            url,
        ];
        let lines = rule_matrix(&rules, true, &Default::default());
        let text = lines.join("\n");
        assert_eq!(
            lines[..2],
            [
                "Rows marked (named) apply only to the objects in RESOURCE NAMES.",
                "Cluster-scoped resources and non-resource URLs are not granted in a namespace.",
            ],
            "{text}"
        );
        assert!(
            lines[2]
                .split_whitespace()
                .rev()
                .take(3)
                .eq(["NAMES", "RESOURCE", "OTHER"]),
            "{text}"
        );
        let tokens = |i: usize| lines[i].split_whitespace().collect::<Vec<_>>();
        assert_eq!(tokens(3)[..4], ["secrets", "core", "·", "✓"], "{text}");
        assert_eq!(
            tokens(4)[..5],
            ["secrets", "(named)", "core", "✓", "·"],
            "{text}"
        );
        assert!(lines[4].ends_with("a, b"), "{text}");
        let url = tokens(5);
        assert_eq!(url[..3], ["/healthz", "non-resource", "URL"], "{text}");
        assert!(url[3..11].iter().all(|cell| *cell == "·"), "{text}");
        assert!(lines[5].ends_with("not granted in a namespace"), "{text}");

        let cluster = rule_matrix(&rules, false, &Default::default());
        let text = cluster.join("\n");
        assert!(!text.contains("not granted"), "{text}");
        let url = cluster[4].split_whitespace().collect::<Vec<_>>();
        assert_eq!(url[3], "✓", "{text}");
        assert!(cluster[4].ends_with("post"), "{text}");
        assert_eq!(
            rule_matrix(&[], false, &Default::default()),
            vec!["No rules in this role."]
        );
    }

    #[test]
    fn namespaced_scope_does_not_tick_discovered_cluster_scoped_resources() {
        let rules = [
            rule(&[""], &["nodes", "nodes/status", "pods"], &[], &["get"]),
            rule(&["example.com"], &["widgets"], &[], &["get"]),
            rule(&["*"], &["*"], &[], &["list"]),
            rule(
                &["*"],
                &["nodes", "pods", "gadgets", "widgets"],
                &[],
                &["watch"],
            ),
        ];
        let scopes = [
            ("", "nodes", false),
            ("", "pods", true),
            ("metrics.k8s.io", "nodes", false),
            ("metrics.k8s.io", "pods", true),
            ("example.com", "widgets", true),
            ("other.io", "widgets", false),
        ]
        .into_iter()
        .map(|(group, plural, namespaced)| ((group.to_string(), plural.to_string()), namespaced))
        .collect();
        let row = |lines: &[String], name: &str, group: &str| {
            lines
                .iter()
                .find(|line| {
                    let cells: Vec<_> = line.split_whitespace().collect();
                    cells.len() > 1 && cells[0] == name && cells[1] == group
                })
                .unwrap_or_else(|| panic!("{name} {group}: {lines:#?}"))
                .clone()
        };
        let ticked = |line: String| {
            (
                line.contains('✓'),
                line.ends_with("not granted in a namespace"),
            )
        };
        let namespaced = rule_matrix(&rules, true, &scopes);
        assert!(
            namespaced[0].starts_with("Cluster-scoped resources"),
            "{namespaced:?}"
        );
        assert_eq!(ticked(row(&namespaced, "nodes", "core")), (false, true));
        assert_eq!(
            ticked(row(&namespaced, "nodes/status", "core")),
            (false, true)
        );
        assert_eq!(ticked(row(&namespaced, "pods", "core")), (true, false));
        assert_eq!(
            ticked(row(&namespaced, "widgets", "example.com")),
            (true, false)
        );
        assert_eq!(ticked(row(&namespaced, "*", "*")), (true, false));
        // A `*` group is cluster-scoped only when every discovered match is.
        assert_eq!(ticked(row(&namespaced, "nodes", "*")), (false, true));
        assert_eq!(ticked(row(&namespaced, "pods", "*")), (true, false));
        assert_eq!(ticked(row(&namespaced, "gadgets", "*")), (true, false));
        let mixed = row(&namespaced, "widgets", "*");
        assert!(mixed.contains('✓'), "{mixed}");
        assert!(mixed.ends_with("namespaced API groups only"), "{mixed}");

        let cluster = rule_matrix(&rules, false, &scopes);
        assert_eq!(ticked(row(&cluster, "nodes", "core")), (true, false));
        assert_eq!(ticked(row(&cluster, "nodes", "*")), (true, false));
        let text = cluster.join("\n");
        assert!(!text.contains("not granted"), "{text}");
        assert!(!text.contains("namespaced API groups only"), "{text}");
    }
}
