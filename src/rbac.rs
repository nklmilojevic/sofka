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
