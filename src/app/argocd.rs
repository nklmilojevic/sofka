use super::*;

use crate::argocd::{self, Destination, Evidence};

/// Lowercased `(kind, group)` -> lowercased plural, resolved up front because
/// the spawned gather has no access to the cluster registry.
type KindPlurals = HashMap<(String, String), String>;

impl App {
    /// Open the Argo CD view for the selection: sync and health state, source,
    /// the objects it manages, and what blocks a sync. Works on an Application
    /// directly, or on anything Argo manages, whose tracking metadata names the
    /// Application to look up. Findings arrive as [`Msg::Argocd`].
    pub fn open_argocd(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        if self
            .cluster
            .resolve_in_group("Application", ARGOCD_GROUP)
            .is_none()
        {
            self.flash_warn("this cluster has no Argo CD Application CRD");
            return;
        }
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("Argo CD view is not available for Helm releases");
            return;
        }
        let Some(obj) = self.selected() else {
            self.flash_warn("no selection for the Argo CD view");
            return;
        };
        self.set_return_mode();
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.argocd_title = format!("{name} — Argo CD");
        self.argocd_items.clear();
        self.argocd_state.select(None);
        // `⏎` during the load must not report the previous Application's
        // cluster.
        self.argocd_destination = Destination::Current;
        self.cancel_explain_request();
        self.argocd_source = Some(obj);
        self.mode = Mode::Argocd;
        self.spawn_argocd();
    }

    /// `r` in the Argo CD view: re-gather for the same selection.
    pub(super) fn refresh_argocd(&mut self) {
        if self.argocd_source.is_some() {
            self.spawn_argocd();
        }
    }

    fn spawn_argocd(&mut self) {
        let Some(obj) = self.argocd_source.clone() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let subject = format!("{}/{name}", kind.ar.kind);
        let title = self.argocd_title.clone();
        let self_is_app = self.argocd_app_kind();
        // Resolved here: the spawned task has no cluster registry.
        let Some(app_kind) = self.cluster.resolve_in_group("Application", ARGOCD_GROUP) else {
            return;
        };

        let plurals: KindPlurals = self.cluster.kind_plurals();
        let current_server = self.cluster.cluster_url.clone();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let claim = self.claim_status(format!("Argo CD: {}…", subject));

        self.argocd_claim = Some(claim);
        self.argocd_request = self.argocd_request.wrapping_add(1);
        let request = self.argocd_request;
        tokio::spawn(async move {
            let gathered: Result<_, String> = async {
                // Re-read so the report reflects the cluster now, not whatever
                // the watch last delivered.
                let selection = report_source(&client, &kind.ar, kind.namespaced, &obj).await?;

                // Either the selection is the Application, or its tracking
                // metadata names one to fetch.
                let (via, owner, app) = if self_is_app {
                    (None, None, Some(selection.clone()))
                } else {
                    let owner = argocd::owner_ref(&selection);
                    let app = match &owner {
                        Some(r) => {
                            let subject = (
                                kind.ar.kind.as_str(),
                                kind.ar.group.as_str(),
                                selection.metadata.namespace.as_deref().unwrap_or_default(),
                                selection.metadata.name.as_deref().unwrap_or_default(),
                            );
                            fetch_application(&client, &app_kind.ar, r, subject).await?
                        }
                        None => None,
                    };
                    (Some(subject.clone()), owner, app)
                };

                let subject = match &app {
                    Some(a) => format!(
                        "{}/{}",
                        app_kind.ar.kind,
                        a.metadata.name.clone().unwrap_or_default()
                    ),
                    None => subject,
                };
                let destination = app
                    .as_ref()
                    .map(|a| resolve_destination(a, &current_server))
                    .unwrap_or(Destination::Current);
                let mut resources = app
                    .as_ref()
                    .map(argocd::managed_resources)
                    .unwrap_or_default();
                for r in &mut resources {
                    let key = (r.kind.to_lowercase(), r.group.to_lowercase());
                    if let Some(plural) = plurals.get(&key) {
                        r.plural = plural.clone();
                    }
                }
                let ev = Evidence {
                    subject,
                    via,
                    owner,
                    destination_namespace: app
                        .as_ref()
                        .map(|a| argocd::destination_namespace(a).to_string())
                        .unwrap_or_default(),
                    destination: destination.clone(),
                    resources,
                    app,
                };
                Ok((selection, argocd::describe(&ev), destination))
            }
            .await;
            // Not `report_result`: the destination travels with the findings so
            // the view can explain an absent jump target.
            let (source, findings, destination) = match gathered {
                Ok((source, findings, destination)) => {
                    (Some(Box::new(source)), findings, destination)
                }
                Err(error) => {
                    let mut findings = Vec::new();
                    prepend_warn_finding(&mut findings, Some(error));
                    (None, findings, Destination::Current)
                }
            };
            let _ = tx
                .send(Msg::Argocd {
                    generation: genr,
                    request,
                    claim,
                    title,
                    source,
                    destination,
                    findings,
                })
                .await;
        });
    }

    pub(super) fn cancel_argocd_request(&mut self) {
        self.argocd_request = self.argocd_request.wrapping_add(1);
        if let Some(claim) = self.argocd_claim.take() {
            self.clear_claimed_status(claim);
        }
    }

    pub(super) fn key_argocd(&mut self, key: KeyInput) {
        let len = self.argocd_items.len();
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.argocd_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.argocd_state, len, false),
            (Some(Action::First), _) if len > 0 => self.argocd_state.select(Some(0)),
            (Some(Action::Last), _) if len > 0 => self.argocd_state.select(Some(len - 1)),
            (Some(Action::Refresh), _) => self.refresh_argocd(),
            // Lines for an Application deploying elsewhere carry no target, so
            // this reports where the object lives rather than searching here.
            (Some(Action::Accept), _) => {
                let selected = self
                    .argocd_state
                    .selected()
                    .and_then(|i| self.argocd_items.get(i));
                match selected.and_then(|f| f.target.clone()) {
                    Some(t) => self.navigate_to_target(&t),
                    None => self.flash_warn(&self.no_jump_reason()),
                }
            }
            _ => {}
        }
        if self.mode != Mode::Argocd {
            self.cancel_argocd_request();
        }
    }

    /// Why `⏎` did nothing. A remote destination is a different answer from a
    /// line that never named a resource.
    fn no_jump_reason(&self) -> String {
        match &self.argocd_destination {
            Destination::Context(ctx) => {
                format!("these resources live in {ctx}; switch with :ctx {ctx}")
            }
            Destination::Unresolved(server) if !server.is_empty() => {
                format!("these resources live in {server}, which no kubeconfig context serves")
            }
            _ => "no resource to jump to on this line".to_string(),
        }
    }
}

/// Fetch the Application a managed object's tracking metadata named.
///
/// Usually only a name is known, since Argo records the namespace only for
/// Applications outside its own. A cluster can hold thousands of Applications,
/// each several KiB, so the search is a `metadata.name` field selector rather
/// than a full list.
///
/// `Ok(None)` means it genuinely is not there; a read failure stays an error so
/// a denied read never renders as "not found". `subject` breaks ties between
/// same-named Applications in different Argo instances.
async fn fetch_application(
    client: &Client,
    ar: &ApiResource,
    owner: &crate::argocd::OwnerRef,
    subject: (&str, &str, &str, &str),
) -> Result<Option<DynamicObject>, String> {
    if let Some(ns) = &owner.namespace {
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, ar);
        return match api.get(&owner.name).await {
            Ok(o) => Ok(Some(o)),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
            Err(e) => Err(format!("reading Application {}: {e}", owner.name)),
        };
    }
    let api: Api<DynamicObject> = Api::all_with(client.clone(), ar);
    let params = ListParams::default().fields(&format!("metadata.name={}", owner.name));
    let mut items = match api.list(&params).await {
        Ok(list) => list.items,
        Err(e) => return Err(format!("looking up Application {}: {e}", owner.name)),
    };
    let (kind, group, namespace, name) = subject;
    if let Some(i) = items
        .iter()
        .position(|a| crate::argocd::manages(a, kind, group, namespace, name))
    {
        return Ok(Some(items.swap_remove(i)));
    }
    // Nothing claims the object. An exact annotation is Argo's own word that
    // this Application owns it, so trust it over a possibly stale status. A bare
    // instance label is not evidence, since Helm writes it too.
    if owner.exact {
        return Ok(items.into_iter().next());
    }
    Ok(None)
}

/// Match `spec.destination` against the connected cluster, then the kubeconfig.
fn resolve_destination(app: &DynamicObject, current_server: &str) -> Destination {
    let (server, name) = argocd::destination_ref(app);
    let contexts = crate::k8s::Cluster::list_contexts().unwrap_or_default();
    classify_destination(
        server,
        name,
        current_server,
        crate::k8s::Cluster::context_for_server,
        |n| contexts.iter().any(|c| c == n),
    )
}

/// The destination decision, with the kubeconfig lookups injected so it can be
/// tested without one.
pub(super) fn classify_destination(
    server: &str,
    name: &str,
    current_server: &str,
    context_for_server: impl Fn(&str) -> Option<String>,
    is_known_context: impl Fn(&str) -> bool,
) -> Destination {
    if !server.is_empty() {
        let normalized = crate::k8s::normalize_server(server);
        // Argo writes the in-cluster destination as the Kubernetes service
        // URL, with or without the implicit port.
        let in_cluster = matches!(
            normalized.as_str(),
            "https://kubernetes.default.svc" | "https://kubernetes.default.svc:443"
        );
        if in_cluster || normalized == crate::k8s::normalize_server(current_server) {
            return Destination::Current;
        }
        return match context_for_server(server) {
            Some(ctx) => Destination::Context(ctx),
            None => Destination::Unresolved(server.to_string()),
        };
    }

    if name.is_empty() {
        return Destination::Unresolved(String::new());
    }
    // Try the kubeconfig first. A context of that name is concrete, and taking
    // it over the `in-cluster` convention stops a cluster registered under that
    // name from passing as the local one.
    if is_known_context(name) {
        return Destination::Context(name.to_string());
    }
    if name == "in-cluster" {
        return Destination::Current;
    }
    Destination::Unresolved(name.to_string())
}
