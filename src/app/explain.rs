use super::*;

impl App {
    /// Open the deterministic "why is this unhealthy?" view for the selection.
    /// Evidence (owned pods, recent events) is gathered off-thread and arrives
    /// as [`Msg::Explain`]; [`crate::explain`] turns it into ranked findings.
    pub(super) fn open_explain(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        // Helm rows are synthetic (backed by storage Secrets) — nothing to
        // diagnose against the Kubernetes health model.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("explain is not available for Helm releases");
            return;
        }
        let Some(obj) = self.selected() else {
            self.flash_warn("no selection to explain");
            return;
        };
        self.set_return_mode();
        self.explain_return = self.return_mode;
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.explain_title = format!("{name} — explain");
        self.explain_items.clear();
        self.explain_state.select(None);
        self.explain_selection_lost = false;
        self.cancel_gitops_request();
        self.cancel_argocd_request();
        self.explain_refresh_source = self.resource_source(
            &obj,
            refresh::RefreshView::Explain {
                pods: self.cluster.resolve("pods").map(Box::new),
                events: self.cluster.resolve("events").map(Box::new),
            },
        );
        self.explain_source = Some(obj);
        self.mode = Mode::Explain;
        self.spawn_explain();
    }

    /// `r` in the explain view — re-gather the evidence for the same object.
    pub(super) fn refresh_explain(&mut self) {
        if self.refresh_task.is_some() {
            self.stop_resource_refresh();
            self.toggle_resource_refresh();
        } else if self.explain_source.is_some() {
            self.spawn_explain();
        }
    }

    /// Gather owned pods and recent events for [`Self::explain_source`], run the
    /// deterministic analysis, and hand the findings back via [`Msg::Explain`].
    fn spawn_explain(&mut self) {
        let Some(source) = self.explain_refresh_source.clone() else {
            return;
        };
        let title = self.explain_title.clone();
        let obj = &source.object;
        let tx = self.tx.clone();
        let genr = self.generation;
        let claim = self.claim_status(format!(
            "explaining {}…",
            obj.metadata.name.clone().unwrap_or_default()
        ));

        if let Some(task) = self.explain_task.take() {
            task.abort();
        }
        self.explain_claim = Some(claim);
        self.explain_request = self.explain_request.wrapping_add(1);
        let request = self.explain_request;
        self.explain_task = Some(tokio::spawn(async move {
            let refresh::RefreshView::Explain { pods, events } = &source.view else {
                return;
            };
            let gathered = gather_explain(&source, pods.as_deref(), events.as_deref())
                .await
                .map(|(source, mut findings, warning)| {
                    prepend_warn_finding(&mut findings, warning);
                    (source, findings)
                });
            let (source, findings) = report_result(gathered);
            let _ = tx
                .send(Msg::Explain {
                    generation: genr,
                    request,
                    claim,
                    title,
                    source,
                    findings,
                })
                .await;
        }));
    }

    pub(super) fn cancel_explain_request(&mut self) {
        if let Some(task) = self.explain_task.take() {
            task.abort();
        }
        self.explain_request = self.explain_request.wrapping_add(1);
        if let Some(claim) = self.explain_claim.take() {
            self.clear_claimed_status(claim);
        }
    }

    pub(super) fn key_explain(&mut self, key: KeyInput) {
        if self.explain_selection_lost
            && self.explain_state.selected().is_none()
            && matches!(
                key.action,
                Some(Action::Accept | Action::Events | Action::Logs)
            )
        {
            self.flash_warn("select a finding before opening its resource, events, or logs");
            return;
        }
        let len = self.explain_items.len();
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                let destination = self.explain_return;
                self.mode = destination;
                self.explain_return = Mode::Table;
                if destination == Mode::Table {
                    self.restore_selection();
                }
            }
            (Some(Action::Down), _) => findings_step(
                &mut self.explain_state,
                &self.explain_items,
                &mut self.findings_scroll,
                true,
            ),
            (Some(Action::Up), _) => findings_step(
                &mut self.explain_state,
                &self.explain_items,
                &mut self.findings_scroll,
                false,
            ),
            (Some(Action::First), _) => {
                if len > 0 {
                    self.explain_state.select(Some(0));
                }
            }
            (Some(Action::Last), _) => {
                if len > 0 {
                    self.explain_state.select(Some(len - 1));
                }
            }
            (Some(Action::Refresh), _) => self.refresh_explain(),
            (Some(Action::AutoRefresh), _) => self.toggle_resource_refresh(),
            (Some(Action::Wrap), _) => self.toggle_findings_wrap(),
            // Direct evidence navigation: jump to the resource behind the
            // selected finding (⏎), or open its events (E) / logs (l). With no
            // target on the current line, E/l fall back to the object being
            // explained — so the whole evidence trail is one keystroke away.
            (Some(Action::Accept), _) => self.explain_goto(),
            (Some(Action::Events), _) => self.explain_events(),
            (Some(Action::Logs), _) => self.explain_logs(),
            _ => {}
        }
        if !matches!(self.mode, Mode::Explain | Mode::Events | Mode::Logs) {
            self.cancel_explain_request();
        }
    }

    /// `w` in a findings view: fold long findings onto more rows, or clip them
    /// at the right edge for a denser overview.
    pub(super) fn toggle_findings_wrap(&mut self) {
        self.findings_wrap = !self.findings_wrap;
        self.flash = format!("wrap: {}", if self.findings_wrap { "on" } else { "off" });
        self.flash_err = false;
    }

    /// The navigation target of the highlighted finding, if any.
    fn selected_target(&self) -> Option<crate::explain::Target> {
        self.explain_state
            .selected()
            .and_then(|i| self.explain_items.get(i))
            .and_then(|f| f.target.clone())
    }

    /// ⏎ — land on the resource behind the selected finding (a blocking pod),
    /// as a name-filtered table view. Everything you'd do next (logs, events,
    /// containers) is then a single keystroke away.
    fn explain_goto(&mut self) {
        let Some(t) = self.selected_target() else {
            self.flash_warn("no resource to jump to on this line");
            return;
        };
        if !matches!(t.plural.as_str(), "pods" | "jobs") {
            self.flash_warn("can only jump to pods and jobs here");
            return;
        }
        self.drill_to(
            &t.plural,
            t.namespace.unwrap_or_default(),
            None,
            Some(format!("metadata.name={}", t.name)),
            format!("{}/{}", trim_s(&t.plural), t.name),
        );
        self.mode = Mode::Table;
    }

    /// `E` — open the event stream for the selected finding's target, or (no
    /// target) the object being explained.
    fn explain_events(&mut self) {
        match self.selected_target() {
            Some(t) => self.open_events_for(t.name, t.namespace.unwrap_or_default(), None),
            None => self.open_events(),
        }
    }

    /// `l` — stream logs for the selected finding's target pod, or (no target)
    /// the object being explained.
    fn explain_logs(&mut self) {
        match self.selected_target() {
            Some(t) if t.plural == "pods" => self.launch_logs(
                LogSource::Pod {
                    ns: t.namespace.unwrap_or_default(),
                    name: t.name.clone(),
                    uid: None,
                    containers: vec![],
                },
                format!("{} — logs", t.name),
            ),
            _ => self.open_logs(),
        }
    }
}

async fn list_with(
    client: &Client,
    ar: &ApiResource,
    namespaced: bool,
    ns: &str,
    params: &ListParams,
) -> Result<Vec<DynamicObject>, String> {
    let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    api.list(params)
        .await
        .map(|l| l.items)
        .map_err(|e| format!("listing {}: {e}", ar.plural))
}

/// What [`crate::explain`] reads besides the object itself.
pub(super) struct Gathered {
    pub(super) pods: Vec<DynamicObject>,
    pub(super) pods_listed: bool,
    pub(super) related: Vec<DynamicObject>,
    pub(super) related_listed: bool,
    pub(super) storage_classes: Option<Vec<DynamicObject>>,
    pub(super) events: Vec<DynamicObject>,
    pub(super) events_v1: bool,
}

impl Gathered {
    pub(super) fn evidence<'a>(
        &'a self,
        kind: &'a str,
        plural: &'a str,
        obj: &'a DynamicObject,
    ) -> crate::explain::Evidence<'a> {
        crate::explain::Evidence {
            kind,
            plural,
            obj,
            pods: &self.pods,
            pods_listed: self.pods_listed,
            related: &self.related,
            related_listed: self.related_listed,
            storage_classes: self.storage_classes.as_deref(),
            events: &self.events,
            events_v1: self.events_v1,
        }
    }
}

/// Collect the evidence for one object: the pods it owns, runs, or serves,
/// the Jobs of a CronJob, the StorageClasses a claim may use, and the events
/// regarding all of them. Failures degrade to a warning in `warn` and are
/// marked as unlisted, so the analysis never reads them as empty.
pub(super) async fn gather_evidence(
    client: &Client,
    plural: &str,
    obj: &DynamicObject,
    pods_kind: Option<(&ApiResource, bool)>,
    events_kind: Option<(&ApiResource, bool)>,
    warn: &mut Option<String>,
) -> Gathered {
    let ns = obj.metadata.namespace.as_deref().unwrap_or_default();
    let name = obj.metadata.name.as_deref().unwrap_or_default();
    let mut related = Ok(Vec::new());
    // Many users may not list cluster-scoped StorageClasses. The analysis
    // then says the class is unknown rather than missing.
    let storage_classes = if plural == "persistentvolumeclaims" {
        let classes = ApiResource::erase::<k8s_openapi::api::storage::v1::StorageClass>(&());
        list_with(client, &classes, false, "", &ListParams::default())
            .await
            .ok()
    } else {
        None
    };
    let labels = |selector: &str| ListParams::default().labels(selector);
    let pods: Result<Vec<DynamicObject>, String> = match (plural, pods_kind) {
        ("pods", _) => Ok(vec![obj.clone()]),
        ("deployments" | "statefulsets" | "daemonsets" | "replicasets", Some((ar, nsd))) => {
            match label_selector(obj, "matchLabels") {
                Some(selector) => list_with(client, ar, nsd, ns, &labels(&selector)).await,
                None => Ok(Vec::new()),
            }
        }
        // A Job's selector can be set by hand and match other pods, so keep
        // only the pods the Job created.
        ("jobs", Some((ar, nsd))) => match label_selector(obj, "matchLabels") {
            Some(selector) => list_with(client, ar, nsd, ns, &labels(&selector))
                .await
                .map(|pods| owned_by(pods, obj)),
            None => Ok(Vec::new()),
        },
        ("nodes", Some((ar, nsd))) => {
            let params = ListParams::default().fields(&format!("spec.nodeName={name}"));
            list_with(client, ar, nsd, "", &params).await
        }
        ("persistentvolumeclaims", Some((ar, nsd))) => {
            list_with(client, ar, nsd, ns, &ListParams::default())
                .await
                .map(|pods| pods.into_iter().filter(|p| mounts_claim(p, name)).collect())
        }
        ("cronjobs", pods_kind) => {
            let jobs = ApiResource::erase::<k8s_openapi::api::batch::v1::Job>(&());
            related = list_with(client, &jobs, true, ns, &ListParams::default())
                .await
                .map(|jobs| owned_by(jobs, obj));
            let latest = related.as_ref().ok().and_then(|jobs| {
                jobs.iter()
                    .max_by_key(|j| j.metadata.creation_timestamp.as_ref().map(|t| t.0))
            });
            match (pods_kind, latest) {
                (Some((ar, nsd)), Some(job)) => match label_selector(job, "matchLabels") {
                    Some(selector) => list_with(client, ar, nsd, ns, &labels(&selector))
                        .await
                        .map(|pods| owned_by(pods, job)),
                    None => Ok(Vec::new()),
                },
                _ => Ok(Vec::new()),
            }
        }
        _ => Ok(Vec::new()),
    };
    let mut unwrap = |listed: Result<Vec<DynamicObject>, String>| match listed {
        Ok(items) => (items, true),
        Err(e) => {
            warn.get_or_insert(e);
            (Vec::new(), false)
        }
    };
    let (pods, mut pods_listed) = unwrap(pods);
    // Without a pods kind in discovery nothing was listed, so the empty set
    // says nothing about which pods exist.
    if pods_kind.is_none() && plural != "pods" {
        pods_listed = false;
    }
    let (related, related_listed) = unwrap(related);
    let (events, events_v1) = match events_kind {
        Some((ar, nsd)) => {
            let v1 = ar.group == "events.k8s.io";
            let all = list_or_warn(client, ar, nsd, ns, warn).await;
            let regarding: Vec<DynamicObject> = pods.iter().chain(&related).cloned().collect();
            (filter_events(&all, obj, &regarding, v1), v1)
        }
        None => (Vec::new(), false),
    };
    Gathered {
        pods,
        pods_listed,
        related,
        related_listed,
        storage_classes,
        events,
        events_v1,
    }
}

/// The objects `owner` owns.
fn owned_by(objects: Vec<DynamicObject>, owner: &DynamicObject) -> Vec<DynamicObject> {
    let Some(uid) = owner.metadata.uid.as_deref() else {
        return objects;
    };
    objects
        .into_iter()
        .filter(|o| {
            o.metadata
                .owner_references
                .iter()
                .flatten()
                .any(|r| r.uid == uid)
        })
        .collect()
}

/// Whether `pod` mounts the PersistentVolumeClaim `claim`.
fn mounts_claim(pod: &DynamicObject, claim: &str) -> bool {
    pod.data
        .pointer("/spec/volumes")
        .and_then(Value::as_array)
        .is_some_and(|volumes| {
            volumes.iter().any(|v| {
                v.pointer("/persistentVolumeClaim/claimName")
                    .and_then(Value::as_str)
                    == Some(claim)
            })
        })
}

/// Keep only events that regard the object or one of its pods, matching by UID
/// (falling back to name when an event carries no UID).
pub(super) fn filter_events(
    all: &[DynamicObject],
    obj: &DynamicObject,
    pods: &[DynamicObject],
    events_v1: bool,
) -> Vec<DynamicObject> {
    let field = if events_v1 {
        "regarding"
    } else {
        "involvedObject"
    };
    let mut uids: HashSet<&str> = HashSet::new();
    // Without a UID an event is matched by namespace and name, so a pod of the
    // same name in another namespace does not count.
    let mut names: HashSet<(&str, &str)> = HashSet::new();
    for o in std::iter::once(obj).chain(pods.iter()) {
        if let Some(u) = o.metadata.uid.as_deref() {
            uids.insert(u);
        }
        if let Some(n) = o.metadata.name.as_deref() {
            names.insert((o.metadata.namespace.as_deref().unwrap_or_default(), n));
        }
    }
    all.iter()
        .filter(|e| {
            let inv = e.data.get(field);
            let uid = inv.and_then(|o| o.get("uid")).and_then(|v| v.as_str());
            let name = inv.and_then(|o| o.get("name")).and_then(|v| v.as_str());
            let namespace = inv
                .and_then(|o| o.get("namespace"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            match uid {
                Some(u) => uids.contains(u),
                None => name.is_some_and(|n| names.contains(&(namespace, n))),
            }
        })
        .cloned()
        .collect()
}

pub(super) async fn gather_explain(
    source: &refresh::RefreshSource,
    pods_kind: Option<&Kind>,
    events_kind: Option<&Kind>,
) -> Result<(DynamicObject, Vec<crate::explain::Finding>, Option<String>), String> {
    let obj = source.read().await?;
    let plural = source.kind.ar.plural.as_str();
    let mut warning = None;
    let gathered = gather_evidence(
        &source.client,
        plural,
        &obj,
        pods_kind.map(|k| (&k.ar, k.namespaced)),
        events_kind.map(|k| (&k.ar, k.namespaced)),
        &mut warning,
    )
    .await;
    let findings = crate::explain::explain(&gathered.evidence(&source.kind.ar.kind, plural, &obj));
    Ok((obj, findings, warning))
}
