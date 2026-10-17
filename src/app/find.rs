use super::*;

/// The kinds `:find` sweeps. A curated set rather than the whole discovery
/// catalog: listing every CRD across all namespaces on each keypress would
/// hammer the API server for kinds nobody names things in.
const FIND_KINDS: &[&str] = &[
    "pods",
    "deployments",
    "statefulsets",
    "daemonsets",
    "services",
    "configmaps",
    "secrets",
    "ingresses",
    "jobs",
    "cronjobs",
    "persistentvolumeclaims",
    "nodes",
    "namespaces",
    "kustomizations",
    "helmreleases",
];

/// Bound on how many hits are kept (best scores first).
const FIND_MAX_RESULTS: usize = 200;

impl App {
    /// `:find <text>` — fuzzy-search object names across the common kinds,
    /// all namespaces, concurrently. Results open in a picker; `⏎` jumps to
    /// the object as a name-filtered view of its kind.
    pub(super) fn start_find(&mut self, query: &str) {
        let query = query.trim().to_string();
        if query.is_empty() {
            self.flash_warn("usage: :find <text>");
            return;
        }
        if !self.cluster.connected {
            self.flash_warn("not connected to a cluster");
            return;
        }
        self.find_query = query.clone();
        self.find_items.clear();
        self.find_state.select(None);
        self.mode = Mode::Find;
        let claim = self.claim_status(format!("finding '{query}'…"));
        self.find_claim = Some(claim);

        let kinds: Vec<(String, ApiResource)> = FIND_KINDS
            .iter()
            .filter_map(|p| {
                self.cluster
                    .resolve(p)
                    .map(|k| (k.ar.plural.to_lowercase(), k.ar))
            })
            .collect();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;

        tokio::spawn(async move {
            let lists = futures_util::future::join_all(kinds.into_iter().map(|(plural, ar)| {
                let client = client.clone();
                async move {
                    let api: Api<DynamicObject> = Api::all_with(client, &ar);
                    (plural, api.list(&ListParams::default()).await)
                }
            }))
            .await;

            // Score after gathering: the matcher isn't Send, so it must not
            // live across the await above.
            let matcher = crate::fuzzy::Fuzzy::new();
            let mut failed = 0usize;
            let mut scored: Vec<(i64, crate::store::FindItem)> = Vec::new();
            for (plural, res) in lists {
                match res {
                    Ok(list) => {
                        for o in list.items {
                            let name = o.metadata.name.unwrap_or_default();
                            if let Some(score) = matcher.score(&name, &query) {
                                scored.push((
                                    score,
                                    crate::store::FindItem {
                                        plural: plural.clone(),
                                        ns: o.metadata.namespace.unwrap_or_default(),
                                        name,
                                    },
                                ));
                            }
                        }
                    }
                    Err(_) => failed += 1,
                }
            }
            // Stable on purpose: `FindItem::ns` is not in the tie-break, so
            // two hits that differ only by namespace compare equal here and an
            // unstable sort would order them arbitrarily between runs.
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.name.cmp(&b.1.name))
                    .then_with(|| a.1.plural.cmp(&b.1.plural))
            });
            let items: Vec<crate::store::FindItem> = scored
                .into_iter()
                .take(FIND_MAX_RESULTS)
                .map(|(_, i)| i)
                .collect();
            let warn = (failed > 0).then(|| format!("{failed} kind(s) could not be listed"));
            let _ = tx
                .send(Msg::FindResults {
                    generation: genr,
                    claim,
                    query,
                    items,
                    warn,
                })
                .await;
        });
    }

    pub(super) fn key_find(&mut self, key: KeyInput) {
        let len = self.find_items.len();
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => self.mode = Mode::Table,
            (Some(Action::Down), _) => list_step(&mut self.find_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.find_state, len, false),
            (Some(Action::First), _) => {
                if len > 0 {
                    self.find_state.select(Some(0));
                }
            }
            (Some(Action::Last), _) => {
                if len > 0 {
                    self.find_state.select(Some(len - 1));
                }
            }
            (Some(Action::Accept), _) => {
                let Some(item) = self
                    .find_state
                    .selected()
                    .and_then(|i| self.find_items.get(i))
                else {
                    return;
                };
                let target = crate::explain::Target {
                    plural: item.plural.clone(),
                    namespace: (!item.ns.is_empty()).then(|| item.ns.clone()),
                    name: item.name.clone(),
                };
                self.navigate_to_target(&target);
            }
            _ => {}
        }
    }
}
