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
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.explain_title = format!("{name} — explain");
        self.explain_items.clear();
        self.explain_state.select(None);
        self.explain_source = Some(obj);
        self.mode = Mode::Explain;
        self.spawn_explain();
    }

    /// `r` in the explain view — re-gather the evidence for the same object.
    pub(super) fn refresh_explain(&mut self) {
        if self.explain_source.is_some() {
            self.explain_items.clear();
            self.spawn_explain();
        }
    }

    /// Gather owned pods and recent events for [`Self::explain_source`], run the
    /// deterministic analysis, and hand the findings back via [`Msg::Explain`].
    fn spawn_explain(&mut self) {
        let Some(obj) = self.explain_source.clone() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let plural = self.kind_plural.clone();
        let kind_name = kind.ar.kind.clone();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let title = self.explain_title.clone();

        // A workload's pods are found by its own pod selector; a pod explains
        // itself; everything else has no owned pods to correlate.
        let selector = match plural.as_str() {
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" => {
                label_selector(&obj, "matchLabels")
            }
            _ => None,
        };
        let pods_kind = self.cluster.resolve("pods").map(|k| (k.ar, k.namespaced));
        let events_kind = self.cluster.resolve("events").map(|k| (k.ar, k.namespaced));

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = format!(
            "explaining {}…",
            obj.metadata.name.clone().unwrap_or_default()
        );
        self.flash_err = false;

        tokio::spawn(async move {
            let pods: Vec<DynamicObject> = if plural == "pods" {
                vec![obj.clone()]
            } else if let (Some((ar, nsd)), Some(sel)) = (&pods_kind, &selector) {
                list_selected(&client, ar, *nsd, &ns, sel).await
            } else {
                Vec::new()
            };

            let (events, events_v1) = match &events_kind {
                Some((ar, nsd)) => {
                    let v1 = ar.group == "events.k8s.io";
                    let all = list_kind(&client, ar, *nsd, &ns).await;
                    (filter_events(&all, &obj, &pods, v1), v1)
                }
                None => (Vec::new(), false),
            };

            let evidence = crate::explain::Evidence {
                kind: &kind_name,
                plural: &plural,
                obj: &obj,
                pods: &pods,
                events: &events,
                events_v1,
            };
            let findings = crate::explain::explain(&evidence);
            let _ = tx
                .send(Msg::Explain {
                    generation: genr,
                    title,
                    findings,
                })
                .await;
        });
    }

    pub(super) fn key_explain(&mut self, key: KeyEvent) {
        let len = self.explain_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.explain_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.explain_state, len, false),
            KeyCode::Char('g') | KeyCode::Home => {
                if len > 0 {
                    self.explain_state.select(Some(0));
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if len > 0 {
                    self.explain_state.select(Some(len - 1));
                }
            }
            KeyCode::Char('r') => self.refresh_explain(),
            _ => {}
        }
    }
}

/// List one kind, narrowed to a label selector (a workload's pods).
async fn list_selected(
    client: &Client,
    ar: &ApiResource,
    namespaced: bool,
    ns: &str,
    selector: &str,
) -> Vec<DynamicObject> {
    let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    api.list(&ListParams::default().labels(selector))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
}

/// Keep only events that regard the object or one of its pods, matching by UID
/// (falling back to name when an event carries no UID).
fn filter_events(
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
    let mut names: HashSet<&str> = HashSet::new();
    for o in std::iter::once(obj).chain(pods.iter()) {
        if let Some(u) = o.metadata.uid.as_deref() {
            uids.insert(u);
        }
        if let Some(n) = o.metadata.name.as_deref() {
            names.insert(n);
        }
    }
    all.iter()
        .filter(|e| {
            let inv = e.data.get(field);
            let uid = inv.and_then(|o| o.get("uid")).and_then(|v| v.as_str());
            let name = inv.and_then(|o| o.get("name")).and_then(|v| v.as_str());
            match uid {
                Some(u) => uids.contains(u),
                None => name.is_some_and(|n| names.contains(n)),
            }
        })
        .cloned()
        .collect()
}
