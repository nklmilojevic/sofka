use super::*;

use crate::argocd::{self, Destination, Evidence};
use crate::explain::{Finding, Level, Target};

/// Lowercased `(kind, group)` -> lowercased plural, resolved up front because
/// the spawned gather has no access to the cluster registry.
type KindPlurals = HashMap<(String, String), String>;

/// Kubernetes caps a label value here, which is where Argo truncates a long
/// Application name.
const LABEL_VALUE_MAX: usize = 63;

/// Page size and page cap for the truncated-label fallback scan.
/// A remote managed resource on its way to being opened in the cluster that
/// holds it, with what `esc` should come back to.
pub(super) struct RemoteJump {
    pub resource: argocd::ManagedResource,
    pub back: ArgocdReturn,
}

/// The Argo CD view a remote jump left: the context it was on and the object
/// it was opened for (an Application, or something Argo manages), so the same
/// view can be reopened after switching back.
#[derive(Clone)]
pub(super) struct ArgocdReturn {
    pub context: String,
    pub source: DynamicObject,
    pub kind: crate::k8s::Kind,
}

const PREFIX_SCAN_PAGE: u32 = 500;
const PREFIX_SCAN_PAGES: usize = 20;

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
        self.show_argocd(obj);
    }

    /// Open the Argo CD view for `obj`, a row of the current kind.
    fn show_argocd(&mut self, obj: DynamicObject) {
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

    /// The kubeconfig as seen now, for resolving destinations.
    fn context_index(&self) -> crate::k8s::ContextIndex {
        #[cfg(test)]
        if let Some(index) = &self.context_index_override {
            return index.clone();
        }
        crate::k8s::ContextIndex::read()
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
        let self_is_appset = self.argocd_kind() && !self_is_app;
        // Resolved here: the spawned task has no cluster registry.
        let Some(app_kind) = self.cluster.resolve_in_group("Application", ARGOCD_GROUP) else {
            return;
        };

        let plurals: KindPlurals = self.cluster.kind_plurals();
        let current_server = self.cluster.cluster_url.clone();
        // Read here, not in the gather: parsing the kubeconfig is blocking file
        // I/O and has no business on a tokio worker.
        let contexts = self.context_index();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let claim = self.claim_status(format!("Argo CD: {}…", subject));

        // A re-gather rebuilds every finding, so nothing stays expanded.
        self.argocd_expanded.clear();
        self.cancel_argocd_children();
        if let Some(stale) = self.argocd_cause_claim.take() {
            self.clear_claimed_status(stale);
        }
        self.argocd_claim = Some(claim);
        self.argocd_request = self.argocd_request.wrapping_add(1);
        let request = self.argocd_request;
        tokio::spawn(async move {
            let gathered: Result<_, String> = async {
                // Re-read so the report reflects the cluster now, not whatever
                // the watch last delivered.
                let selection = report_source(&client, &kind.ar, kind.namespaced, &obj).await?;

                // An ApplicationSet has no owning Application and nothing
                // `describe` expects — it names generators and produced
                // Applications, not a sync/health rollup.
                if self_is_appset {
                    let mut resources = argocd::managed_resources(&selection);
                    for r in &mut resources {
                        let key = (r.kind.to_lowercase(), r.group.to_lowercase());
                        if let Some(plural) = plurals.get(&key) {
                            r.plural = plural.clone();
                        }
                    }
                    let findings =
                        argocd::describe_applicationset(&selection, &subject, &resources);
                    return Ok((selection, findings, Destination::Current, resources));
                }

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
                    .map(|a| resolve_destination(a, &current_server, &contexts))
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
                let findings = argocd::describe(&ev, crate::columns::now_secs());
                Ok((selection, findings, destination, ev.resources))
            }
            .await;
            // Not `report_result`: the destination travels with the findings so
            // the view can explain an absent jump target.
            let (source, findings, destination, resources) = match gathered {
                Ok((source, findings, destination, resources)) => {
                    (Some(Box::new(source)), findings, destination, resources)
                }
                Err(error) => {
                    let mut findings = Vec::new();
                    prepend_warn_finding(&mut findings, Some(error));
                    (None, findings, Destination::Current, Vec::new())
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
                    resources,
                })
                .await;
        });
    }

    pub(super) fn cancel_argocd_request(&mut self) {
        self.argocd_request = self.argocd_request.wrapping_add(1);
        if let Some(claim) = self.argocd_claim.take() {
            self.clear_claimed_status(claim);
        }
        // The cause search outlives the report it belongs to, and its result is
        // dropped by the request guard, so nothing else would take the status back.
        if let Some(claim) = self.argocd_cause_claim.take() {
            self.clear_claimed_status(claim);
        }
        self.cancel_argocd_children();
    }

    fn cancel_argocd_children(&mut self) {
        // The expanded set is cleared by the next gather, not here: leaving the
        // view to open a managed resource must not forget what was open.
        for claim in std::mem::take(&mut self.argocd_children_claims) {
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
            (Some(Action::DiscoverChildren), _) => self.toggle_argocd_children(),
            // Lines for an Application deploying elsewhere carry no target;
            // when a kubeconfig context serves that cluster the jump goes
            // through it, otherwise this reports where the object lives.
            (Some(Action::Accept), _) => {
                let selected = self
                    .argocd_state
                    .selected()
                    .and_then(|i| self.argocd_items.get(i));
                match selected.and_then(|f| f.target.clone()) {
                    Some(t) => self.navigate_to_target(&t),
                    None => self.jump_to_remote_managed_resource(),
                }
            }
            _ => {}
        }
        if self.mode != Mode::Argocd {
            self.cancel_argocd_request();
        }
    }

    /// `⏎` on a managed resource of an Application deploying to a cluster some
    /// kubeconfig context serves: switch to it and open the object once the
    /// connection lands. The kind is resolved then, against the cluster that
    /// holds it — this one may not know the CRD. Any other target-less line
    /// reports why there is nothing to jump to.
    fn jump_to_remote_managed_resource(&mut self) {
        let Destination::Context(context) = self.argocd_destination.clone() else {
            self.flash_warn(&self.no_jump_reason());
            return;
        };
        // Same name as the connected context, yet not classified local: the
        // kubeconfig now points that name at another server. A switch would
        // be a no-op, so say what happened instead of dropping the jump.
        if context == self.cluster.context && self.cluster.connected {
            self.flash_warn(&format!(
                "kubeconfig context {context} no longer points at the connected cluster; reconnect with :ctx {context}"
            ));
            return;
        }
        let resource = self
            .argocd_state
            .selected()
            .and_then(|i| self.managed_row_ordinal(i))
            .and_then(|o| self.argocd_resources.get(o).cloned());
        let Some(resource) = resource else {
            self.flash_warn(&self.no_jump_reason());
            return;
        };
        let back = match (self.argocd_source.clone(), self.kind.clone()) {
            (Some(source), Some(kind)) => ArgocdReturn {
                context: self.cluster.context.clone(),
                source,
                kind,
            },
            _ => {
                self.flash_warn(&self.no_jump_reason());
                return;
            }
        };
        // Leave the view first: it describes the cluster being left, and the
        // tail of `key_argocd` cancels its request on the mode change.
        self.mode = self.return_mode;
        self.switch_context(context);
        self.pending_resource_query = None;
        self.pending_bookmark = None;
        self.pending_workspace = None;
        self.pending_argocd_return = None;
        self.pending_argocd_target = Some(RemoteJump { resource, back });
    }

    /// `esc` at the root of the view a remote jump opened: switch back to the
    /// context the Argo CD view was on and reopen it there once the switch
    /// lands.
    pub(super) fn return_to_argocd(&mut self, back: ArgocdReturn) {
        self.switch_context(back.context.clone());
        self.pending_resource_query = None;
        self.pending_bookmark = None;
        self.pending_workspace = None;
        self.pending_argocd_target = None;
        self.pending_argocd_return = Some(back);
    }

    /// The landing half of [`Self::return_to_argocd`]: the table underneath
    /// is the kind the view was opened from, so `esc` out of the view lands
    /// somewhere sensible, and the view itself re-reads its object.
    pub(super) fn reopen_argocd(&mut self, back: ArgocdReturn) {
        let ArgocdReturn { source, kind, .. } = back;
        self.set_root_view(kind);
        if let Some(ns) = source.metadata.namespace.clone() {
            self.namespace = ns;
        }
        self.record_history();
        self.start_watch();
        self.show_argocd(source);
    }

    /// The landing half of [`Self::jump_to_remote_managed_resource`]: the
    /// switch to `context` is in, so open the object as a root view scoped to
    /// its name. A kind this cluster does not know falls back to pods, and
    /// says so.
    pub(super) fn open_remote_managed_resource(&mut self, jump: RemoteJump, context: &str) {
        let RemoteJump { resource, back } = jump;
        let Some(kind) = self
            .cluster
            .resolve_in_group(&resource.kind, &resource.group)
        else {
            if let Some(pods) = self.cluster.resolve("pods") {
                self.set_root_view(pods);
                self.record_history();
                self.start_watch();
            }
            // Still the jumped view: `esc` must find its way back from here
            // too, and `set_root_view` above just dropped it.
            self.argocd_return = Some(back);
            self.flash_warn(&format!("{context} has no {}; viewing pods", resource.kind));
            return;
        };
        let title = kind.title();
        self.set_root_view(kind);
        if !resource.namespace.is_empty() {
            self.namespace = resource.namespace.clone();
        }
        self.fields = Some(format!("metadata.name={}", resource.name));
        self.scope_label = Some(resource.name.clone());
        self.record_history();
        self.start_watch();
        // After `set_root_view`, which drops it: this root is the jumped view.
        self.argocd_return = Some(back);
        self.set_flash(format!(
            "Viewing {title}/{} in {context} · esc returns to the Application",
            resource.name
        ));
    }

    /// Whether `⏎` on row `index` goes somewhere: the row's own target, or a
    /// managed resource of an Application deploying to a cluster some
    /// kubeconfig context serves, which jumps through that context.
    pub fn argocd_row_jumps(&self, index: usize) -> bool {
        self.argocd_items
            .get(index)
            .is_some_and(|f| f.target.is_some())
            || (matches!(self.argocd_destination, Destination::Context(_))
                && self.managed_row_ordinal(index).is_some())
    }

    /// Why `⏎` did nothing. A remote destination is a different answer from a
    /// line that never named a resource.
    fn no_jump_reason(&self) -> String {
        match &self.argocd_destination {
            Destination::Context(ctx) => {
                format!("these resources live in {ctx}; ⏎ on a managed resource opens it there")
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
    // A label value stops at 63 characters, so a name of exactly that length may
    // be a truncated prefix and the exact-name selector above would never match.
    if owner.name.len() == LABEL_VALUE_MAX {
        return claimed_by_prefix(client, ar, &owner.name, subject).await;
    }
    Ok(None)
}

/// The Application whose name starts with `prefix` and that claims the subject.
///
/// Only for the truncated-label case, so it pages and stops at the first match
/// rather than materialising a collection that runs to thousands of objects.
async fn claimed_by_prefix(
    client: &Client,
    ar: &ApiResource,
    prefix: &str,
    subject: (&str, &str, &str, &str),
) -> Result<Option<DynamicObject>, String> {
    let (kind, group, namespace, name) = subject;
    let api: Api<DynamicObject> = Api::all_with(client.clone(), ar);
    let mut token: Option<String> = None;
    for _ in 0..PREFIX_SCAN_PAGES {
        let mut params = ListParams::default().limit(PREFIX_SCAN_PAGE);
        if let Some(t) = &token {
            params = params.continue_token(t);
        }
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(e) => return Err(format!("looking up Application {prefix}…: {e}")),
        };
        if let Some(found) = page.items.into_iter().find(|a| {
            a.metadata
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with(prefix))
                && crate::argocd::manages(a, kind, group, namespace, name)
        }) {
            return Ok(Some(found));
        }
        token = page.metadata.continue_.filter(|t| !t.is_empty());
        if token.is_none() {
            return Ok(None);
        }
    }
    // Ran out of pages with more to read. Saying "not managed by Argo CD" here
    // would report giving up as an answer.
    Err(format!(
        "looking up Application {prefix}…: more than {} pages",
        PREFIX_SCAN_PAGES
    ))
}

/// Match `spec.destination` against the connected cluster, then the kubeconfig.
fn resolve_destination(
    app: &DynamicObject,
    current_server: &str,
    contexts: &crate::k8s::ContextIndex,
) -> Destination {
    let (server, name) = argocd::destination_ref(app);
    classify_destination(
        server,
        name,
        current_server,
        |c| contexts.server_by_context.get(c).cloned(),
        |s| {
            contexts
                .by_server
                .get(&crate::k8s::normalize_server(s))
                .cloned()
        },
        |n| argocd::context_for_name(n, &contexts.clusters),
    )
}

/// The destination decision, with the kubeconfig lookups injected so it can be
/// tested without one.
pub(super) fn classify_destination(
    server: &str,
    name: &str,
    current_server: &str,
    server_for_context: impl Fn(&str) -> Option<String>,
    context_for_server: impl Fn(&str) -> Option<String>,
    context_for_name: impl Fn(&str) -> Option<String>,
) -> Destination {
    // A resolved context is local when it serves the cluster we are connected
    // to — by server, never by name: a context of the same name can point
    // elsewhere once the kubeconfig has changed under a running session.
    let here = crate::k8s::normalize_server(current_server);
    let resolved = |ctx: String| {
        if server_for_context(&ctx).is_some_and(|s| crate::k8s::normalize_server(&s) == here) {
            Destination::Current
        } else {
            Destination::Context(ctx)
        }
    };
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
            Some(ctx) => resolved(ctx),
            None => Destination::Unresolved(server.to_string()),
        };
    }

    if name.is_empty() {
        return Destination::Unresolved(String::new());
    }
    // Try the kubeconfig first. A context of that name is concrete, and taking
    // it over the `in-cluster` convention stops a cluster registered under that
    // name from passing as the local one.
    if let Some(ctx) = context_for_name(name) {
        return resolved(ctx);
    }
    if name == "in-cluster" {
        return Destination::Current;
    }
    Destination::Unresolved(name.to_string())
}

/// Child kinds per parent plural, resolved up front because the spawned gather
/// has no cluster registry. Comes from `adjacent`, so `[views."…"].children`
/// applies here too.
type ChildPlan = HashMap<String, Vec<crate::k8s::Kind>>;

/// Two levels covers Deployment to ReplicaSet to Pod and CronJob to Job to Pod.
/// Kubernetes workloads do not nest deeper.
const MAX_DEPTH: u8 = 2;

/// One object found by walking `ownerReferences` down from a managed resource.
struct Descendant {
    obj: DynamicObject,
    kind: String,
    plural: String,
    depth: u8,
}

impl App {
    /// A managed-resource row's identity, stable across re-gathers.
    /// Which entry of `status.resources[]` the row at `index` is, by position
    /// in the managed-resource block.
    ///
    /// Position rather than name: two managed resources can share a kind,
    /// namespace and name when their API groups differ, so no string built from
    /// the row identifies it.
    fn managed_row_ordinal(&self, index: usize) -> Option<usize> {
        let heading = self.argocd_items[..index]
            .iter()
            .rposition(|f| f.level == Level::Heading)?;
        if !self.argocd_items[heading]
            .text
            .starts_with("Managed resources")
        {
            return None;
        }
        if self.argocd_items[index].indent != 1 {
            return None;
        }
        let ordinal = self.argocd_items[heading + 1..index]
            .iter()
            .filter(|f| f.indent == 1)
            .count();
        // Past the cap sits the "… and N more" summary, another indent-1 row
        // that names no resource — it must not map to the first hidden one.
        (ordinal < argocd::MAX_LISTED).then_some(ordinal)
    }

    /// The row showing the `ordinal`th managed resource, if it is still there.
    fn managed_row_at(&self, ordinal: usize) -> Option<usize> {
        let heading = self
            .argocd_items
            .iter()
            .position(|f| f.level == Level::Heading && f.text.starts_with("Managed resources"))?;
        self.argocd_items[heading + 1..]
            .iter()
            .enumerate()
            .filter(|(_, f)| f.indent == 1)
            .nth(ordinal)
            .map(|(offset, _)| heading + 1 + offset)
    }

    /// `c` in the Argo CD view: show or hide the selected resource's
    /// `ownerReferences` descendants.
    pub(super) fn toggle_argocd_children(&mut self) {
        let Some(index) = self.argocd_state.selected() else {
            return;
        };
        let Some(finding) = self.argocd_items.get(index) else {
            return;
        };
        // Only a row naming a resource in this cluster can be walked; remote
        // destinations and headings carry no target.
        if finding.target.is_none() {
            self.flash_warn(&self.no_jump_reason());
            return;
        }
        let Some(ordinal) = self.managed_row_ordinal(index) else {
            self.flash_warn("expand a managed resource, not one of its children");
            return;
        };
        if self.argocd_expanded.remove(&ordinal).is_some() {
            self.collapse_argocd_children(index);
            return;
        }
        self.spawn_argocd_children(index, ordinal);
    }

    /// Drop the run of deeper-indented findings that follows `index`.
    fn collapse_argocd_children(&mut self, index: usize) {
        let base = self.argocd_items[index].indent;
        let end = self.argocd_items[index + 1..]
            .iter()
            .position(|f| f.indent <= base)
            .map(|n| index + 1 + n)
            .unwrap_or(self.argocd_items.len());
        self.argocd_items.drain(index + 1..end);
    }

    fn spawn_argocd_children(&mut self, index: usize, ordinal: usize) {
        let Some(target) = self.argocd_items[index].target.clone() else {
            return;
        };
        let namespace = target.namespace.clone().unwrap_or_default();
        if namespace.is_empty() {
            self.flash_warn("cluster-scoped resources have no owned workloads to show");
            return;
        }
        // Resolve in the resource's own group. A plural on its own is
        // ambiguous (`services` is core and `serving.knative.dev`), and reading
        // the wrong object would build a tree for something else entirely.
        // The row's own entry, so a kind that collides with another group's
        // resolves to the object this row actually names.
        let Some(parent) = self
            .argocd_resources
            .get(ordinal)
            .and_then(|r| self.cluster.resolve_in_group(&r.kind, &r.group))
        else {
            self.flash_warn("this row's kind is not known to the cluster");
            return;
        };
        let parent_plural = parent.ar.plural.to_lowercase();
        let plan = self.children_plan(&parent, &namespace);

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let claim = self.claim_status(format!("Argo CD: children of {}…", target.name));
        // Claims are per request, not per view: two rows can be gathering at
        // once and each clears its own when it lands.
        self.argocd_children_claims.push(claim);
        // The counter identifies a request, it does not cancel other rows. Rows
        // expand independently, so the guard is per key: a result is taken only
        // if its row still expects that exact request.
        self.argocd_children_request = self.argocd_children_request.wrapping_add(1);
        let request = self.argocd_children_request;
        self.argocd_expanded.insert(ordinal, request);
        let name = target.name.clone();
        tokio::spawn(async move {
            let findings =
                gather_children(&client, parent, parent_plural, &namespace, &name, &plan).await;
            let _ = tx
                .send(Msg::ArgocdChildren {
                    generation: genr,
                    request,
                    claim,
                    ordinal,
                    findings,
                })
                .await;
        });
    }

    /// Child kinds for the parent and for everything it can own, down to
    /// [`MAX_DEPTH`]. Resolved here because the spawned gather has no registry.
    fn children_plan(&self, parent: &crate::k8s::Kind, namespace: &str) -> ChildPlan {
        let mut plan = ChildPlan::new();
        let mut queue = vec![parent.clone()];
        for _ in 0..MAX_DEPTH {
            let mut next = Vec::new();
            for kind in queue.drain(..) {
                let plural = kind.ar.plural.to_lowercase();
                if plan.contains_key(&plural) {
                    continue;
                }
                let kids: Vec<crate::k8s::Kind> =
                    crate::adjacent::children_for(&self.user_views, &kind.ar, Some(namespace))
                        .iter()
                        .filter_map(|p| self.cluster.resolve(p))
                        .collect();
                next.extend(kids.iter().cloned());
                plan.insert(plural, kids);
            }
            queue = next;
        }
        plan
    }

    /// Insert descendants under the row that asked, if it is still expanded.
    pub(super) fn apply_argocd_children(
        &mut self,
        ordinal: usize,
        request: u64,
        findings: Vec<Finding>,
    ) {
        // Only the request this row is currently waiting on: a collapse or a
        // second expansion of the same row supersedes an earlier result.
        if self.argocd_expanded.get(&ordinal) != Some(&request) {
            return;
        }
        let Some(index) = self.managed_row_at(ordinal) else {
            self.argocd_expanded.remove(&ordinal);
            return;
        };
        let before = self.argocd_items.len();
        self.collapse_argocd_children(index);
        for (offset, finding) in findings.into_iter().enumerate() {
            self.argocd_items.insert(index + 1 + offset, finding);
        }
        // Rows landing above the cursor would otherwise slide the selection onto
        // a different line, which matters because these arrive while the user is
        // still moving around.
        let shift = self.argocd_items.len() as isize - before as isize;
        if let Some(selected) = self.argocd_state.selected()
            && selected > index
            && shift != 0
        {
            let moved = (selected as isize + shift).max(index as isize + 1) as usize;
            self.argocd_state
                .select(Some(moved.min(self.argocd_items.len().saturating_sub(1))));
        }
    }
}

/// Walk `ownerReferences` down from one managed resource.
///
/// Child kinds come from `adjacent`, which already knows Deployment to
/// ReplicaSet to Pod and honours `[views."…"].children`. Each kind is listed
/// once per namespace and reused at every depth.
async fn gather_children(
    client: &Client,
    parent: crate::k8s::Kind,
    parent_plural: String,
    namespace: &str,
    name: &str,
    plan: &ChildPlan,
) -> Vec<Finding> {
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &parent.ar);
    let root = match api.get(name).await {
        Ok(o) => o,
        Err(e) => {
            return vec![child_finding_text(
                2,
                Level::Warn,
                format!("reading {name}: {e}"),
            )];
        }
    };
    let Some(root_uid) = root.metadata.uid.clone() else {
        return Vec::new();
    };

    let mut lists: HashMap<(String, String), Vec<DynamicObject>> = HashMap::new();
    let mut warn = None;
    let mut found = Vec::new();
    walk(
        client,
        namespace,
        &parent_plural,
        &root_uid,
        1,
        plan,
        &mut lists,
        &mut warn,
        &mut found,
    )
    .await;

    let mut out: Vec<Finding> = found
        .iter()
        .map(|d| child_finding(&d.obj, &d.kind, &d.plural, namespace, d.depth))
        .collect();
    if let Some(w) = warn {
        out.insert(0, child_finding_text(2, Level::Warn, w));
    } else if out.is_empty() {
        out.push(child_finding_text(2, Level::Info, "owns nothing"));
    }
    out
}

/// One level of the walk, depth first so each child is followed by its own.
#[allow(clippy::too_many_arguments)]
fn walk<'a>(
    client: &'a Client,
    namespace: &'a str,
    parent_plural: &'a str,
    parent_uid: &'a str,
    depth: u8,
    plan: &'a ChildPlan,
    // Listings already fetched, keyed by namespace and plural so one cache can
    // serve parents in different namespaces.
    lists: &'a mut HashMap<(String, String), Vec<DynamicObject>>,
    warn: &'a mut Option<String>,
    out: &'a mut Vec<Descendant>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        if depth > MAX_DEPTH {
            return;
        }
        let Some(kinds) = plan.get(parent_plural) else {
            return;
        };
        for kind in kinds {
            let plural = kind.ar.plural.to_lowercase();
            let key = (namespace.to_string(), plural.clone());
            if !lists.contains_key(&key) {
                let api: Api<DynamicObject> =
                    Api::namespaced_with(client.clone(), namespace, &kind.ar);
                // A failed listing is reported, not swallowed: an empty tree
                // and an unreadable one must not look the same.
                let items = match api.list(&ListParams::default()).await {
                    Ok(list) => list.items,
                    Err(e) => {
                        warn.get_or_insert(format!("listing {plural}: {e}"));
                        Vec::new()
                    }
                };
                lists.insert(key.clone(), items);
            }
            let owned: Vec<DynamicObject> = lists[&key]
                .iter()
                .filter(|o| crate::adjacent::owned_by(o, Some(parent_uid)))
                .cloned()
                .collect();
            for obj in owned {
                let uid = obj.metadata.uid.clone().unwrap_or_default();
                let at = out.len();
                out.push(Descendant {
                    kind: kind.ar.kind.clone(),
                    plural: plural.clone(),
                    obj,
                    depth,
                });
                walk(
                    client,
                    namespace,
                    &plural,
                    &uid,
                    depth + 1,
                    plan,
                    lists,
                    warn,
                    out,
                )
                .await;
                // `revisionHistoryLimit` keeps ten superseded ReplicaSets by
                // default. One scaled to zero that owns no pod is history, and
                // listing it buries the revision actually running. Narrow to
                // ReplicaSets: a Job with no pods yet is still worth showing.
                if out.len() == at + 1
                    && kind.ar.kind == "ReplicaSet"
                    && argocd::scaled_to_zero(&out[at].obj)
                {
                    out.remove(at);
                }
            }
        }
    })
}

/// One descendant row, indented under its parent and independently navigable.
fn child_finding(
    obj: &DynamicObject,
    kind: &str,
    plural: &str,
    namespace: &str,
    depth: u8,
) -> Finding {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let state = argocd::descendant_state(obj);
    let text = if state.is_empty() {
        format!("{kind}/{name}")
    } else {
        format!("{kind}/{name}: {state}")
    };
    let level = match state.as_ref() {
        "Running" | "Succeeded" | "Complete" => Level::Good,
        // The states `theme::status_color` paints yellow: on their way up, not
        // broken. A pod that is still creating is not a blocker.
        "Pending" | "ContainerCreating" | "PodInitializing" | "SchedulingGated" | "Completing"
        | "Progressing" => Level::Warn,
        "" => Level::Info,
        s if s.ends_with(" ready") => Level::Info,
        // Failed, Unknown, and the waiting reasons that mean something is wrong
        // (CrashLoopBackOff, ImagePullBackOff, …).
        _ => Level::Critical,
    };
    child_finding_text(depth + 1, level, text).with_target(Target {
        plural: plural.to_string(),
        namespace: Some(namespace.to_string()),
        name,
    })
}

fn child_finding_text(indent: u8, level: Level, text: impl Into<String>) -> Finding {
    Finding {
        indent,
        level,
        text: text.into(),
        target: None,
    }
}

/// How many causes to name before stopping.
const MAX_CAUSES: usize = 5;

impl App {
    /// Search the cluster for what is making an Application unhealthy when its
    /// own status does not say.
    ///
    /// Only worth doing when [`argocd::health_unexplained`] holds, which with
    /// Argo CD's default `controller.resource.health.persist` is every unhealthy
    /// Application: `status.resources[]` carries no health at all, so the only
    /// place the reason exists is the objects themselves.
    pub(super) fn spawn_argocd_cause(&mut self) {
        // Objects of another cluster are not here, and a plural resolved against
        // this one would open something else of the same name.
        if self.argocd_destination != Destination::Current {
            return;
        }
        // Only resources that can own something, so a ConfigMap costs nothing.
        // Each keeps its own namespace: one Application can span several, and a
        // parent fetched from the wrong one is either missing or somebody else's.
        let parents: Vec<(crate::k8s::Kind, String, String, ChildPlan)> = self
            .argocd_resources
            .iter()
            .filter(|r| !r.namespace.is_empty())
            .filter_map(|r| {
                let kind = self.cluster.resolve_in_group(&r.kind, &r.group)?;
                let plan = self.children_plan(&kind, &r.namespace);
                let plural = kind.ar.plural.to_lowercase();
                plan.get(&plural)
                    .is_some_and(|kids| !kids.is_empty())
                    .then(|| (kind, r.namespace.clone(), r.name.clone(), plan))
            })
            .collect();
        if parents.is_empty() {
            return;
        }

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let request = self.argocd_request;
        let claim = self.claim_status("Argo CD: looking for the cause…".to_string());
        self.argocd_cause_claim = Some(claim);
        tokio::spawn(async move {
            let mut findings = Vec::new();
            // Shared across parents: several workloads in one namespace would
            // otherwise re-list that namespace's pods once each.
            let mut lists: HashMap<(String, String), Vec<DynamicObject>> = HashMap::new();
            for (kind, namespace, name, plan) in parents {
                if findings.len() >= MAX_CAUSES {
                    break;
                }
                let plural = kind.ar.plural.to_lowercase();
                findings.extend(
                    unhealthy_descendants(
                        &client, kind, plural, &namespace, &name, &plan, &mut lists,
                    )
                    .await,
                );
            }
            findings.truncate(MAX_CAUSES);
            let _ = tx
                .send(Msg::ArgocdCause {
                    generation: genr,
                    request,
                    claim,
                    findings,
                })
                .await;
        });
    }

    /// Put the causes under the line that said none were known.
    pub(super) fn apply_argocd_cause(&mut self, findings: Vec<Finding>) {
        self.argocd_cause_claim = None;
        if findings.is_empty() {
            return;
        }
        let Some(at) = self
            .argocd_items
            .iter()
            .position(|f| f.text.contains("but no managed resource reports it"))
        else {
            return;
        };
        let before = self.argocd_items.len();
        for (offset, finding) in findings.into_iter().enumerate() {
            self.argocd_items.insert(at + 1 + offset, finding);
        }
        // Drift lines sit below this point, so a cursor parked on one would
        // otherwise slide onto a different row when the search lands.
        let shift = self.argocd_items.len() as isize - before as isize;
        if let Some(selected) = self.argocd_state.selected()
            && selected > at
            && shift != 0
        {
            let moved = (selected as isize + shift).max(at as isize + 1) as usize;
            self.argocd_state
                .select(Some(moved.min(self.argocd_items.len().saturating_sub(1))));
        }
    }
}

/// Walk one managed resource and report only the descendants in a bad state.
async fn unhealthy_descendants(
    client: &Client,
    parent: crate::k8s::Kind,
    parent_plural: String,
    namespace: &str,
    name: &str,
    plan: &ChildPlan,
    lists: &mut HashMap<(String, String), Vec<DynamicObject>>,
) -> Vec<Finding> {
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &parent.ar);
    let Ok(root) = api.get(name).await else {
        return Vec::new();
    };
    let Some(uid) = root.metadata.uid.clone() else {
        return Vec::new();
    };
    let mut warn = None;
    let mut found = Vec::new();
    walk(
        client,
        namespace,
        &parent_plural,
        &uid,
        1,
        plan,
        lists,
        &mut warn,
        &mut found,
    )
    .await;

    found
        .iter()
        .filter_map(|d| {
            let state = argocd::descendant_state(&d.obj);
            // Only states that mean something is wrong. A ReplicaSet's ready
            // count is context, not a cause, and the pod under it says more.
            // `ErrImagePull` is the reason a pod carries before the kubelet
            // starts backing off, so matching only the BackOff form would miss
            // the first minute of every broken image.
            let bad = matches!(
                state.as_ref(),
                "Failed" | "Unknown" | "Pending" | "Evicted" | "OOMKilled"
            ) || state.ends_with("BackOff")
                || state.ends_with("Error")
                || state.starts_with("Err");
            bad.then(|| {
                let name = d.obj.metadata.name.clone().unwrap_or_default();
                child_finding_text(2, Level::Critical, format!("{}/{name}: {state}", d.kind))
                    .with_target(Target {
                        plural: d.plural.clone(),
                        namespace: Some(namespace.to_string()),
                        name,
                    })
            })
        })
        .collect()
}
