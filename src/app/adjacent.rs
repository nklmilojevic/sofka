use super::*;

use crate::adjacent::{
    Backward, Direction, Forward, KindRef, Kinds, dedup, names_source, owned_by,
};
use crate::store::AdjacentItem;
use kube::core::GroupVersionResource;
use std::collections::hash_map::Entry;

mod children;

type ListCache = HashMap<(GroupVersionResource, String), Vec<DynamicObject>>;

/// The cluster's registry, answering the plan's kind lookups.
struct ClusterKinds<'a>(&'a crate::k8s::Cluster);

impl Kinds for ClusterKinds<'_> {
    fn by_name(&self, name: &str) -> Option<KindRef> {
        self.0.resolve(name).map(kind_ref)
    }

    fn by_kind_in_group(&self, kind: &str, group: &str) -> Option<KindRef> {
        self.0.resolve_in_group(kind, group).map(kind_ref)
    }
}

fn kind_ref(k: crate::k8s::Kind) -> KindRef {
    KindRef {
        plural: k.ar.plural.to_lowercase(),
        ar: k.ar,
        namespaced: k.namespaced,
    }
}

impl App {
    /// `u` / `:adjacent` — list the objects directly connected to the
    /// selection: its owners, the objects it owns, the objects its spec names,
    /// and the objects whose specs name it. Gathered off-thread; arrives as
    /// [`Msg::Adjacent`]. `⏎` lands on one in its regular view.
    pub(super) fn open_adjacent(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        // Helm rows are synthetic (backed by storage Secrets): nothing in
        // the API references them, and their references are the chart's.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("adjacent is not available for Helm releases");
            return;
        }
        // A namespace's neighbourhood is everything in it — that's what
        // `enter` re-scopes to, not a list of one-hop connections.
        if self.kind_plural == "namespaces" {
            self.flash_warn("adjacent is not available for namespaces — enter re-scopes to one");
            return;
        }
        let Some(obj) = self.selected() else {
            self.flash_warn("no selection");
            return;
        };
        self.cancel_adjacent_request();
        self.child_status.clear();
        self.adjacent_warning = None;
        self.set_return_mode();
        self.adjacent_return = self.return_mode;
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.adjacent_title = format!("{name} — adjacent");
        self.adjacent_items.clear();
        self.adjacent_state.select(None);
        self.adjacent_source = Some(obj);
        self.mode = Mode::Adjacent;
        self.spawn_adjacent();
    }

    /// `r` in the adjacent view — gather again for the same object.
    pub(super) fn refresh_adjacent(&mut self) {
        if self.adjacent_source.is_some() {
            self.cancel_children();
            self.child_status.clear();
            self.adjacent_warning = None;
            self.spawn_adjacent();
        }
    }

    /// Whether a gather is still running (the view says "gathering…").
    pub fn adjacent_pending(&self) -> bool {
        self.adjacent_claim.is_some()
    }

    /// Read the current source before planning its connections.
    fn spawn_adjacent(&mut self) {
        let Some(obj) = self.adjacent_source.clone() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let generation = self.generation;
        let title = self.adjacent_title.clone();
        let claim = self.claim_status(format!(
            "gathering connections for {}...",
            obj.metadata.name.clone().unwrap_or_default()
        ));
        self.adjacent_claim = Some(claim);
        self.adjacent_request = self.adjacent_request.wrapping_add(1);
        let request = self.adjacent_request;
        tokio::spawn(async move {
            let msg = match report_source(&client, &kind.ar, kind.namespaced, &obj).await {
                Ok(source) => Msg::AdjacentSource {
                    generation,
                    request,
                    claim,
                    source: Box::new(source),
                },
                Err(warn) => Msg::Adjacent {
                    generation,
                    request,
                    claim,
                    title,
                    items: Vec::new(),
                    warn: Some(warn),
                },
            };
            let _ = tx.send(msg).await;
        });
    }

    /// Plan on the UI thread, then read the connected objects in a task.
    pub(super) fn gather_adjacent(&mut self, request: u64, claim: StatusClaim) {
        let Some(obj) = self.adjacent_source.clone() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let source = KindRef {
            plural: self.kind_plural.clone(),
            ar: kind.ar.clone(),
            namespaced: kind.namespaced,
        };
        let mut plan = crate::adjacent::plan(
            &self.user_views,
            &ClusterKinds(&self.cluster),
            &source,
            &obj,
            &self.namespace,
        );

        if self.namespace_is_pattern() {
            plan.backward = plan
                .backward
                .into_iter()
                .flat_map(|backward| {
                    if backward.scope == self.namespace {
                        self.watch_namespaces()
                            .into_iter()
                            .map(|scope| {
                                let mut scoped = backward.clone();
                                scoped.scope = scope;
                                scoped
                            })
                            .collect::<Vec<_>>()
                    } else {
                        vec![backward]
                    }
                })
                .collect();
        }

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let title = self.adjacent_title.clone();
        tokio::spawn(async move {
            let mut warn = (!plan.warns.is_empty()).then(|| plan.warns.join("; "));
            let mut items: Vec<AdjacentItem> = Vec::new();
            let mut lists = ListCache::new();
            let ns = obj.metadata.namespace.clone().unwrap_or_default();
            let source_uid = obj.metadata.uid.clone();
            let source_name = obj.metadata.name.clone().unwrap_or_default();
            let source_ns = source.namespaced.then_some(ns.as_str());

            for (r, name) in plan.owners {
                let owner_ns = if r.namespaced { ns.as_str() } else { "" };
                if let Some(o) = get_or_warn(&client, &r, owner_ns, &name, &mut warn).await {
                    items.push(item(Direction::Owner, "owned by", &r, o));
                }
            }
            for r in plan.children {
                let scope = if r.namespaced { ns.as_str() } else { "" };
                for o in cached_list(&mut lists, &client, &r, scope, &mut warn).await {
                    if owned_by(o, source_uid.as_deref()) {
                        items.push(item(Direction::Child, "owns", &r, o.clone()));
                    }
                }
            }
            for Forward { rule, target, refs } in plan.forward {
                for (name, target_ns) in refs {
                    let scope = if target.namespaced {
                        target_ns.as_str()
                    } else {
                        ""
                    };
                    if let Some(o) = get_or_warn(&client, &target, scope, &name, &mut warn).await {
                        items.push(item(Direction::Names, &rule.relation, &target, o));
                    }
                }
            }
            for Backward {
                rule,
                from,
                scope,
                default,
            } in plan.backward
            {
                for o in cached_list(&mut lists, &client, &from, &scope, &mut warn).await {
                    if names_source(o, &rule, default.as_ref(), &source, &source_name, source_ns) {
                        items.push(item(Direction::NamedBy, &rule.relation, &from, o.clone()));
                    }
                }
            }
            dedup(&mut items);
            let _ = tx
                .send(Msg::Adjacent {
                    generation: genr,
                    request,
                    claim,
                    title,
                    items,
                    warn,
                })
                .await;
        });
    }

    pub(super) fn cancel_adjacent_request(&mut self) {
        self.cancel_children();
        self.adjacent_request = self.adjacent_request.wrapping_add(1);
        if let Some(claim) = self.adjacent_claim.take() {
            self.clear_claimed_status(claim);
        }
    }

    pub(super) fn key_adjacent(&mut self, key: KeyInput) {
        let len = self.adjacent_items.len();
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                let destination = self.adjacent_return;
                self.mode = destination;
                self.adjacent_return = Mode::Table;
                if destination == Mode::Table {
                    self.restore_selection();
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.adjacent_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.adjacent_state, len, false),
            (Some(Action::First), _) => {
                if len > 0 {
                    self.adjacent_state.select(Some(0));
                }
            }
            (Some(Action::Last), _) => {
                if len > 0 {
                    self.adjacent_state.select(Some(len - 1));
                }
            }
            (Some(Action::Refresh), _) => self.refresh_adjacent(),
            (Some(Action::DiscoverChildren), _) => self.discover_children(),
            (Some(Action::Accept), _) => self.adjacent_goto(),
            (Some(Action::Yaml), _) => self.adjacent_yaml(),
            (Some(Action::Describe), _) => self.adjacent_describe(),
            _ => {}
        }
        if !matches!(self.mode, Mode::Adjacent | Mode::Detail) {
            self.cancel_adjacent_request();
        }
    }

    fn adjacent_selected(&self) -> Option<AdjacentItem> {
        self.adjacent_state
            .selected()
            .and_then(|i| self.adjacent_items.get(i))
            .cloned()
    }

    /// ⏎ — land on the object in its own kind's view, name-filtered, where
    /// every action applies to it.
    fn adjacent_goto(&mut self) {
        let Some(it) = self.adjacent_selected() else {
            self.flash_warn("no object selected");
            return;
        };
        self.navigate_to_target(&crate::explain::Target {
            plural: it.plural,
            namespace: it.namespace,
            name: it.name,
        });
    }

    /// `y` — the row's YAML, already fetched by the gather; `esc` returns here.
    fn adjacent_yaml(&mut self) {
        let Some(it) = self.adjacent_selected() else {
            self.flash_warn("no object selected");
            return;
        };
        self.set_return_mode();
        self.show_yaml(&it.object);
    }

    /// `d` — `kubectl describe` the row; `esc` returns here.
    fn adjacent_describe(&mut self) {
        let Some(it) = self.adjacent_selected() else {
            self.flash_warn("no object selected");
            return;
        };
        self.set_return_mode();
        self.describe_object(it.plural, &it.object);
    }
}

fn item(direction: Direction, relation: &str, r: &KindRef, o: DynamicObject) -> AdjacentItem {
    AdjacentItem {
        direction,
        relation: relation.to_string(),
        kind: r.ar.kind.clone(),
        plural: if r.ar.group.is_empty() {
            r.plural.clone()
        } else {
            format!("{}.{}", r.plural, r.ar.group)
        },
        namespace: o.metadata.namespace.clone(),
        name: o.metadata.name.clone().unwrap_or_default(),
        object: Box::new(o),
    }
}

async fn cached_list<'a>(
    lists: &'a mut ListCache,
    client: &Client,
    kind: &KindRef,
    scope: &str,
    warn: &mut Option<String>,
) -> &'a [DynamicObject] {
    let scope = if kind.namespaced { scope } else { "" };
    let key = (
        GroupVersionResource::gvr(&kind.ar.group, &kind.ar.version, &kind.ar.plural),
        scope.to_string(),
    );
    match lists.entry(key) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            entry.insert(list_or_warn(client, &kind.ar, kind.namespaced, scope, warn).await)
        }
    }
}

/// Read one object by name. A 404 is "not there" (a reference to something
/// gone), any other failure is recorded in `warn`.
async fn get_or_warn(
    client: &Client,
    r: &KindRef,
    ns: &str,
    name: &str,
    warn: &mut Option<String>,
) -> Option<DynamicObject> {
    let api: Api<DynamicObject> = if r.namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, &r.ar)
    } else {
        Api::all_with(client.clone(), &r.ar)
    };
    match api.get(name).await {
        Ok(o) => Some(o),
        Err(kube::Error::Api(ae)) if ae.code == 404 => None,
        Err(e) => {
            warn.get_or_insert(format!("reading {}/{name}: {e}", r.plural));
            None
        }
    }
}
