use super::*;
use crate::rbac::{Bindings, ObjectRef, Subject, SubjectKind};
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub enum Query {
    Subjects(SubjectKind),
    Subject(Subject),
    Object(ObjectRef),
}

impl Query {
    fn title(&self) -> String {
        match self {
            Self::Subjects(SubjectKind::User) => "RBAC users (all namespaces)".into(),
            Self::Subjects(SubjectKind::Group) => "RBAC groups (all namespaces)".into(),
            Self::Subject(s) => format!("Directly bound RBAC rules: {}", s.label()),
            Self::Object(o) => format!("RBAC rules: {}", o.label()),
        }
    }
}

#[derive(Default)]
pub struct Report {
    pub subjects: Vec<(Subject, usize)>,
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Default)]
pub struct State {
    pub query: Option<Query>,
    pub subjects: Vec<(Subject, usize)>,
    pub selection: ListState,
    pub document: Scrollable,
    pub warnings: Vec<String>,
    pub pending: bool,
    history: Vec<(Query, Option<Subject>)>,
    selection_target: Option<Subject>,
    request: u64,
    task: Option<JoinHandle<()>>,
    claim: Option<StatusClaim>,
}

impl State {
    fn selected_subject(&self) -> Option<Subject> {
        self.selection
            .selected()
            .and_then(|i| self.subjects.get(i))
            .map(|(subject, _)| subject.clone())
            .or_else(|| self.selection_target.clone())
    }
}

impl App {
    pub(super) fn open_rbac_subjects(&mut self, kind: SubjectKind) {
        self.set_return_mode();
        self.rbac.history.clear();
        self.rbac.selection.select(None);
        self.load_rbac(Query::Subjects(kind), None);
    }

    pub(super) fn open_policy(&mut self, args: &str) {
        let subject = if args.is_empty() {
            if self
                .kind
                .as_ref()
                .is_some_and(|k| k.ar.group.is_empty() && k.ar.kind == "ServiceAccount")
            {
                self.selected_ref().and_then(|o| {
                    Some(Subject::ServiceAccount {
                        namespace: o.metadata.namespace.clone()?,
                        name: o.metadata.name.clone()?,
                    })
                })
            } else {
                None
            }
        } else {
            Subject::parse(args)
        };
        let Some(subject) = subject else {
            self.flash_warn("usage: :policy u:<name> | g:<name> | s:<namespace>/<name>; bare :policy needs a service account");
            return;
        };
        self.set_return_mode();
        self.rbac.history.clear();
        self.load_rbac(Query::Subject(subject), None);
    }

    pub(super) fn open_rbac_object(&mut self, obj: &DynamicObject) {
        let Some(kind) = self.kind.as_ref() else {
            return;
        };
        let target = ObjectRef {
            kind: kind.ar.kind.clone(),
            namespace: kind
                .namespaced
                .then(|| obj.metadata.namespace.clone().unwrap_or_default()),
            name: obj.metadata.name.clone().unwrap_or_default(),
        };
        self.set_return_mode();
        self.rbac.history.clear();
        self.load_rbac(Query::Object(target), None);
    }

    fn load_rbac(&mut self, query: Query, selection: Option<Subject>) {
        self.cancel_rbac();
        let request = self.rbac.request;
        let generation = self.generation;
        self.rbac.document = Scrollable {
            title: query.title(),
            wrap: true,
            ..Default::default()
        };
        self.rbac.selection_target = selection;
        self.rbac.selection.select(None);
        self.rbac.subjects.clear();
        self.rbac.warnings.clear();
        self.rbac.pending = true;
        self.rbac.query = Some(query.clone());
        self.mode = Mode::Rbac;
        let claim = self.claim_status("Reading RBAC bindings and rules...");
        self.rbac.claim = Some(claim);
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        self.rbac.task = Some(tokio::spawn(async move {
            let report = gather(client, query).await;
            let _ = tx
                .send(Msg::RbacReport {
                    generation,
                    request,
                    report,
                })
                .await;
        }));
    }

    pub(super) fn cancel_rbac(&mut self) {
        self.rbac.request = self.rbac.request.wrapping_add(1);
        if let Some(task) = self.rbac.task.take() {
            task.abort();
        }
        if let Some(claim) = self.rbac.claim.take() {
            self.clear_claimed_status(claim);
        }
        self.rbac.pending = false;
    }

    pub(super) fn receive_rbac(&mut self, generation: u64, request: u64, report: Report) {
        if generation != self.generation || request != self.rbac.request {
            return;
        }
        self.rbac.pending = false;
        self.rbac.task = None;
        if let Some(claim) = self.rbac.claim.take() {
            self.clear_claimed_status(claim);
        }
        self.rbac.subjects = report.subjects;
        let selected = match &self.rbac.selection_target {
            Some(subject) => self
                .rbac
                .subjects
                .iter()
                .position(|(candidate, _)| candidate == subject),
            None => (!self.rbac.subjects.is_empty()).then_some(0),
        };
        self.rbac.selection.select(selected);
        let mut lines = Vec::new();
        for warning in &report.warnings {
            lines.push(format!("INCOMPLETE: {warning}"));
        }
        lines.extend(report.lines);
        self.rbac.document.lines = lines.into();
        self.rbac.warnings = report.warnings;
    }

    pub(super) fn key_rbac(&mut self, key: KeyInput) {
        let subjects = matches!(self.rbac.query, Some(Query::Subjects(_)));
        let len = self.rbac.subjects.len();
        match key.action {
            Some(Action::Back | Action::Close) => {
                self.cancel_rbac();
                if let Some((query, selection)) = self.rbac.history.pop() {
                    self.load_rbac(query, selection);
                } else {
                    self.mode = self.return_mode;
                    self.restore_selection();
                }
            }
            Some(Action::Refresh) => {
                if let Some(query) = self.rbac.query.clone() {
                    let selection = self.rbac.selected_subject();
                    self.load_rbac(query, selection);
                }
            }
            Some(Action::Accept) if subjects => {
                if let Some((subject, _)) = self
                    .rbac
                    .selection
                    .selected()
                    .and_then(|i| self.rbac.subjects.get(i))
                    .cloned()
                {
                    if let Some(query) = self.rbac.query.clone() {
                        self.rbac.history.push((query, Some(subject.clone())));
                    }
                    self.load_rbac(Query::Subject(subject), None);
                }
            }
            Some(Action::Down) if subjects => list_step(&mut self.rbac.selection, len, true),
            Some(Action::Up) if subjects => list_step(&mut self.rbac.selection, len, false),
            Some(Action::First) if subjects => self.rbac.selection.select((len > 0).then_some(0)),
            Some(Action::Last) if subjects => self.rbac.selection.select(len.checked_sub(1)),
            Some(Action::Down) => self.rbac.document.scroll_by(1),
            Some(Action::Up) => self.rbac.document.scroll_by(-1),
            Some(Action::PageDown) if subjects => {
                for _ in 0..10 {
                    list_step(&mut self.rbac.selection, len, true);
                }
            }
            Some(Action::PageUp) if subjects => {
                for _ in 0..10 {
                    list_step(&mut self.rbac.selection, len, false);
                }
            }
            Some(Action::PageDown) => self.rbac.document.scroll_by(10),
            Some(Action::PageUp) => self.rbac.document.scroll_by(-10),
            Some(Action::First) => self.rbac.document.scroll_by(i32::MIN),
            Some(Action::Last) => self.rbac.document.scroll_to_bottom(),
            _ => {}
        }
    }
}

async fn list_all<K>(api: &Api<K>) -> Result<Vec<K>, kube::Error>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let mut items = Vec::new();
    let mut params = kube::api::ListParams::default().limit(500);
    loop {
        let page = api.list(&params).await?;
        items.extend(page.items);
        match page.metadata.continue_.filter(|token| !token.is_empty()) {
            Some(token) => params = params.continue_token(&token),
            None => return Ok(items),
        }
    }
}

async fn bindings(client: &kube::Client, report: &mut Report) -> Bindings {
    let roles = Api::<RoleBinding>::all(client.clone());
    let cluster_roles = Api::<ClusterRoleBinding>::all(client.clone());
    let (roles, cluster_roles) = tokio::join!(list_all(&roles), list_all(&cluster_roles));
    Bindings {
        roles: roles.unwrap_or_else(|e| {
            report
                .warnings
                .push(format!("RoleBindings could not be read: {e}"));
            Vec::new()
        }),
        cluster_roles: cluster_roles.unwrap_or_else(|e| {
            report
                .warnings
                .push(format!("ClusterRoleBindings could not be read: {e}"));
            Vec::new()
        }),
    }
}

async fn role_rules(client: &kube::Client, role: &ObjectRef) -> Result<Vec<PolicyRule>, String> {
    match (role.kind.as_str(), role.namespace.as_deref()) {
        ("Role", Some(ns)) if !ns.is_empty() => Api::<Role>::namespaced(client.clone(), ns)
            .get(&role.name)
            .await
            .map(|r| r.rules.unwrap_or_default())
            .map_err(|e| e.to_string()),
        ("ClusterRole", None) => Api::<ClusterRole>::all(client.clone())
            .get(&role.name)
            .await
            .map(|r| r.rules.unwrap_or_default())
            .map_err(|e| e.to_string()),
        _ => Err(format!("invalid role reference: {}", role.label())),
    }
}

async fn append_grants(
    client: &kube::Client,
    grants: Vec<crate::rbac::Grant>,
    report: &mut Report,
) {
    let roles: BTreeSet<_> = grants.iter().map(|grant| grant.role.clone()).collect();
    let cache: BTreeMap<_, _> = futures_util::stream::iter(roles)
        .map(|role| async move {
            let rules = role_rules(client, &role).await;
            (role, rules)
        })
        .buffer_unordered(8)
        .collect()
        .await;
    for grant in grants {
        let rules = &cache[&grant.role];
        report.lines.push(String::new());
        report
            .lines
            .push(format!("Binding: {}", grant.binding.label()));
        report.lines.push(format!("Role: {}", grant.role.label()));
        report.lines.push(match &grant.binding.namespace {
            Some(ns) => format!("Scope: namespace {ns} (namespaced resources only)"),
            None => "Scope: cluster and all namespaces".into(),
        });
        match rules {
            Ok(rules) => report.lines.extend(crate::rbac::rule_lines(
                rules,
                grant.binding.namespace.is_some(),
            )),
            Err(e) => {
                report
                    .warnings
                    .push(format!("{} could not be read: {e}", grant.role.label()));
                report.lines.push("Rules unavailable.".into());
            }
        }
    }
}

async fn gather(client: kube::Client, query: Query) -> Report {
    let mut report = Report::default();
    match query {
        Query::Subjects(kind) => {
            report.subjects = bindings(&client, &mut report).await.subjects(kind);
        }
        Query::Subject(subject) => {
            let grants = bindings(&client, &mut report).await.grants(&subject);
            report
                .lines
                .push("Direct subject matches only. Group membership is not resolved.".into());
            report
                .lines
                .push("These are stored RBAC rules, not an API access check.".into());
            if grants.is_empty() {
                report
                    .lines
                    .push("No direct bindings found in the data read.".into());
            }
            append_grants(&client, grants, &mut report).await;
        }
        Query::Object(object) => match object.kind.as_str() {
            "Role" | "ClusterRole" => match role_rules(&client, &object).await {
                Ok(rules) => {
                    report
                        .lines
                        .push("Role definition. Access depends on its bindings.".into());
                    report.lines.extend(crate::rbac::rule_lines(&rules, false));
                }
                Err(e) => report
                    .warnings
                    .push(format!("{} could not be read: {e}", object.label())),
            },
            "RoleBinding" | "ClusterRoleBinding" => {
                let reference = if object.kind == "RoleBinding" {
                    if let Some(ns) = object.namespace.as_deref().filter(|ns| !ns.is_empty()) {
                        Api::<RoleBinding>::namespaced(client.clone(), ns)
                            .get(&object.name)
                            .await
                            .map(|rb| rb.role_ref)
                            .map_err(|e| e.to_string())
                    } else {
                        Err("binding namespace is missing".into())
                    }
                } else {
                    Api::<ClusterRoleBinding>::all(client.clone())
                        .get(&object.name)
                        .await
                        .map(|rb| rb.role_ref)
                        .map_err(|e| e.to_string())
                };
                match reference {
                    Ok(reference) => {
                        let role = ObjectRef {
                            namespace: (reference.kind == "Role")
                                .then(|| object.namespace.clone())
                                .flatten(),
                            kind: reference.kind,
                            name: reference.name,
                        };
                        append_grants(
                            &client,
                            vec![crate::rbac::Grant {
                                binding: object,
                                role,
                            }],
                            &mut report,
                        )
                        .await;
                    }
                    Err(e) => report
                        .warnings
                        .push(format!("{} could not be read: {e}", object.label())),
                }
            }
            _ => report.warnings.push("unsupported RBAC object".into()),
        },
    }
    report
}
