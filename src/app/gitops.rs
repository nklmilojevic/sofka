use super::*;

use crate::gitops::{self, FluxRef, Node};

/// Flux kinds resolved up front so the off-thread gather can map a
/// `sourceRef.kind` / `dependsOn` to an API resource without the cluster
/// registry (which isn't available in the spawned task).
type FluxKinds = HashMap<String, (ApiResource, bool, String)>;

impl App {
    /// Open the GitOps view for the selection: its Flux owner, source, the
    /// dependsOn chain, and reconciliation state. Gathered off-thread; the
    /// findings arrive as [`Msg::Gitops`].
    pub(super) fn open_gitops(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("GitOps view is not available for Helm releases");
            return;
        }
        let Some(obj) = self.selected() else {
            self.flash_warn("no selection for GitOps view");
            return;
        };
        self.set_return_mode();
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.gitops_title = format!("{name} — GitOps");
        self.gitops_items.clear();
        self.gitops_state.select(None);
        self.gitops_source = Some(obj);
        self.mode = Mode::Gitops;
        self.spawn_gitops();
    }

    /// `r` in the GitOps view — re-gather for the same object.
    pub(super) fn refresh_gitops(&mut self) {
        if self.gitops_source.is_some() {
            self.gitops_items.clear();
            self.spawn_gitops();
        }
    }

    fn spawn_gitops(&mut self) {
        let Some(obj) = self.gitops_source.clone() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let plural = self.kind_plural.clone();
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let subject = format!("{}/{name}", kind.ar.kind);
        let title = self.gitops_title.clone();

        // The selection is itself the owner (a Kustomization/HelmRelease), or
        // it's a managed object naming its owner via toolkit labels.
        let self_is_owner = gitops::is_owner_plural(&plural);
        let owner_ref = if self_is_owner {
            Some(FluxRef {
                kind: kind.ar.kind.clone(),
                name,
                namespace: ns,
            })
        } else {
            gitops::owner_ref(&obj)
        };
        let owner_inline = self_is_owner.then(|| obj.clone());
        let owner_plural_inline = self_is_owner.then(|| plural.clone());

        let flux = self.flux_kind_map();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = format!("GitOps: {}…", subject);
        self.flash_err = false;

        tokio::spawn(async move {
            // Owner: the selection itself, or fetched from its label reference.
            let owner = match owner_ref {
                None => None,
                Some(r) => {
                    let (plural, obj) = match (owner_inline, owner_plural_inline) {
                        (Some(o), Some(p)) => (p, Some(o)),
                        _ => fetch_flux(&client, &flux, &r).await,
                    };
                    Some(Node {
                        reference: r,
                        plural,
                        obj,
                    })
                }
            };

            // Source + dependencies, read from the owner object.
            let mut source = None;
            let mut deps = Vec::new();
            if let Some(owner_obj) = owner.as_ref().and_then(|n| n.obj.as_ref()) {
                if let Some(sr) = gitops::source_ref(owner_obj) {
                    let (plural, obj) = fetch_flux(&client, &flux, &sr).await;
                    source = Some(Node {
                        reference: sr,
                        plural,
                        obj,
                    });
                }
                for dr in gitops::depends_on(owner_obj) {
                    let (plural, obj) = fetch_flux(&client, &flux, &dr).await;
                    deps.push(Node {
                        reference: dr,
                        plural,
                        obj,
                    });
                }
            }

            let ev = gitops::Evidence {
                subject,
                self_is_owner,
                owner,
                source,
                deps,
            };
            let findings = gitops::describe(&ev);
            let _ = tx
                .send(Msg::Gitops {
                    generation: genr,
                    title,
                    findings,
                })
                .await;
        });
    }

    /// Resolve the Flux kinds the chain can reference (owner, sources,
    /// dependencies) into API resources, keyed by both lowercased kind and
    /// plural so a `sourceRef.kind` or plural both look up.
    fn flux_kind_map(&self) -> FluxKinds {
        let mut m = FluxKinds::new();
        for k in [
            "kustomizations",
            "helmreleases",
            "gitrepositories",
            "ocirepositories",
            "buckets",
            "helmrepositories",
            "helmcharts",
        ] {
            if let Some(kind) = self.cluster.resolve(k) {
                let plural = kind.ar.plural.to_lowercase();
                let entry = (kind.ar.clone(), kind.namespaced, plural.clone());
                m.insert(kind.ar.kind.to_lowercase(), entry.clone());
                m.insert(plural, entry);
            }
        }
        m
    }

    pub(super) fn key_gitops(&mut self, key: KeyEvent) {
        let len = self.gitops_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.gitops_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.gitops_state, len, false),
            KeyCode::Char('g') | KeyCode::Home if len > 0 => self.gitops_state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End if len > 0 => self.gitops_state.select(Some(len - 1)),
            KeyCode::Char('r') => self.refresh_gitops(),
            // Jump to the resource behind the selected chain node.
            KeyCode::Enter => {
                let target = self
                    .gitops_state
                    .selected()
                    .and_then(|i| self.gitops_items.get(i))
                    .and_then(|f| f.target.clone());
                match target {
                    Some(t) => self.navigate_to_target(&t),
                    None => self.flash_warn("no resource to jump to on this line"),
                }
            }
            _ => {}
        }
    }
}

/// Fetch one Flux object by reference, returning its resolved plural (empty if
/// the kind is unknown to the cluster) and the object (`None` if not found).
async fn fetch_flux(
    client: &Client,
    flux: &FluxKinds,
    r: &FluxRef,
) -> (String, Option<DynamicObject>) {
    let Some((ar, namespaced, plural)) = flux.get(&r.kind.to_lowercase()) else {
        return (String::new(), None);
    };
    let api: Api<DynamicObject> = if *namespaced && !r.namespace.is_empty() {
        Api::namespaced_with(client.clone(), &r.namespace, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    (plural.clone(), api.get(&r.name).await.ok())
}
