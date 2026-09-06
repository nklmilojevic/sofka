use super::*;

use crate::gitops::{self, Engine, Managed, Node, ObjRef};

/// GitOps kinds resolved up front so the off-thread gather can map a
/// `sourceRef.kind` / `dependsOn` / Argo reference to an API resource without
/// the cluster registry (which isn't available in the spawned task).
type GitopsKinds = HashMap<String, (ApiResource, bool, String)>;

impl App {
    /// Open the GitOps view for the selection: its Flux or Argo CD owner, the
    /// source it reconciles from, the objects that gate it, and reconciliation
    /// state. Gathered off-thread; the findings arrive as [`Msg::Gitops`].
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

        // The selection is itself the owner (a Kustomization/HelmRelease, an
        // Application/ApplicationSet), or it's a managed object naming its
        // owner through the toolkit labels / Argo tracking stamp.
        let self_engine = gitops::owner_engine(&plural, &kind.ar.group);
        let owner_ref = match self_engine {
            Some(engine) => Some(gitops::Owner {
                engine,
                reference: ObjRef {
                    kind: kind.ar.kind.clone(),
                    name,
                    namespace: ns,
                },
                inferred: false,
            }),
            None => gitops::owner_ref(&obj),
        };
        let self_is_owner = self_engine.is_some();
        let owner_inline = self_is_owner.then(|| obj.clone());
        let owner_plural_inline = self_is_owner.then(|| plural.clone());

        let kinds = self.gitops_kind_map();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let claim = self.claim_status(format!("GitOps: {}…", subject));

        tokio::spawn(async move {
            let mut warn = None;
            // Owner: the selection itself, or fetched from its reference.
            let owner = match owner_ref {
                None => None,
                Some(r) => {
                    let (plural, obj) = match (owner_inline, owner_plural_inline) {
                        (Some(o), Some(p)) => (p, Some(o)),
                        _ => fetch_ref(&client, &kinds, &r.reference, &mut warn).await,
                    };
                    let mut reference = r.reference;
                    // A namespace-less Argo stamp resolves to wherever the
                    // Application actually lives, so the jump target works.
                    if let Some(found) = obj.as_ref().and_then(|o| o.metadata.namespace.clone()) {
                        reference.namespace = found;
                    }
                    Some(Managed {
                        engine: r.engine,
                        node: Node {
                            reference,
                            plural,
                            obj,
                        },
                        inferred: r.inferred,
                    })
                }
            };

            // The rest of the chain, read from the owner object.
            let mut source = None;
            let mut deps = Vec::new();
            if let Some(m) = owner.as_ref()
                && let Some(owner_obj) = m.node.obj.as_ref()
            {
                match m.engine {
                    Engine::Flux => {
                        if let Some(sr) = gitops::source_ref(owner_obj) {
                            let (plural, obj) = fetch_ref(&client, &kinds, &sr, &mut warn).await;
                            source = Some(Node {
                                reference: sr,
                                plural,
                                obj,
                            });
                        }
                        for dr in gitops::depends_on(owner_obj) {
                            let (plural, obj) = fetch_ref(&client, &kinds, &dr, &mut warn).await;
                            deps.push(Node {
                                reference: dr,
                                plural,
                                obj,
                            });
                        }
                    }
                    Engine::Argo => {
                        let refs = gitops::argo_project_ref(owner_obj)
                            .into_iter()
                            .chain(gitops::argo_parent_ref(owner_obj));
                        for r in refs {
                            let (plural, obj) = fetch_ref(&client, &kinds, &r, &mut warn).await;
                            deps.push(Node {
                                reference: r,
                                plural,
                                obj,
                            });
                        }
                    }
                }
            }

            let ev = gitops::Evidence {
                subject,
                self_is_owner,
                owner,
                source,
                deps,
            };
            let mut findings = gitops::describe(&ev);
            prepend_warn_finding(&mut findings, warn);
            let _ = tx
                .send(Msg::Gitops {
                    generation: genr,
                    claim,
                    title,
                    findings,
                })
                .await;
        });
    }

    /// Resolve the kinds a chain can reference (owners, sources, dependencies,
    /// Argo projects and generators) into API resources, keyed by both
    /// lowercased kind and plural so a `sourceRef.kind` or plural both look up.
    fn gitops_kind_map(&self) -> GitopsKinds {
        let mut m = GitopsKinds::new();
        for k in [
            "kustomizations",
            "helmreleases",
            "gitrepositories",
            "ocirepositories",
            "buckets",
            "helmrepositories",
            "helmcharts",
            // Argo's plurals are generic, so its kinds are looked up by their
            // group-qualified names.
            "applications.argoproj.io",
            "applicationsets.argoproj.io",
            "appprojects.argoproj.io",
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

/// Fetch one GitOps object by reference, returning its resolved plural (empty
/// if the kind is unknown to the cluster) and the object (`None` if not
/// found). A reference with no namespace — an Argo instance stamp naming an
/// Application in the controller's own namespace — is searched for across all
/// of them. A read *failure* (a 403, a timeout) is recorded in `warn`; folding
/// it into `None` would make the chain view assert the object doesn't exist.
async fn fetch_ref(
    client: &Client,
    kinds: &GitopsKinds,
    r: &ObjRef,
    warn: &mut Option<String>,
) -> (String, Option<DynamicObject>) {
    let Some((ar, namespaced, plural)) = kinds.get(&r.kind.to_lowercase()) else {
        return (String::new(), None);
    };
    if *namespaced && r.namespace.is_empty() {
        return (
            plural.clone(),
            search_all(client, ar, plural, r, warn).await,
        );
    }
    let api: Api<DynamicObject> = if *namespaced {
        Api::namespaced_with(client.clone(), &r.namespace, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    let obj = match api.get(&r.name).await {
        Ok(o) => Some(o),
        Err(kube::Error::Api(ae)) if ae.code == 404 => None,
        Err(e) => {
            warn.get_or_insert(format!("reading {}/{}: {e}", plural, r.name));
            None
        }
    };
    (plural.clone(), obj)
}

/// Find a namespaced object by name across every namespace. The apiserver
/// supports `metadata.name` as a field selector for any kind, so this stays
/// one request instead of a full list plus a client-side scan.
async fn search_all(
    client: &Client,
    ar: &ApiResource,
    plural: &str,
    r: &ObjRef,
    warn: &mut Option<String>,
) -> Option<DynamicObject> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), ar);
    let lp = ListParams::default().fields(&format!("metadata.name={}", r.name));
    match api.list(&lp).await {
        Ok(list) => list.items.into_iter().next(),
        Err(e) => {
            warn.get_or_insert(format!("searching {plural} for {}: {e}", r.name));
            None
        }
    }
}
