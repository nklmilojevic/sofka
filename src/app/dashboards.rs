use super::*;

impl App {
    /// Open the pulse / cluster-health dashboard (k9s `:pulse`).
    pub fn open_pulse(&mut self) {
        self.bump_generation();
        self.pulse = Pulse::default();
        self.mode = Mode::Pulse;
        self.spawn_pulse();
    }

    pub(super) fn spawn_pulse(&mut self) {
        let claim = self.claim_status("pulse — cluster health…");
        let resolve = |n: &str| self.cluster.resolve(n).map(|k| (k.ar, k.namespaced));
        let nodes = resolve("nodes");
        let pods = resolve("pods");
        let deploys = resolve("deployments");
        let sts = resolve("statefulsets");
        let ds = resolve("daemonsets");
        let jobs = resolve("jobs");
        let pvc = resolve("persistentvolumeclaims");

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        // Cluster-health snapshot: always spans every namespace, regardless
        // of whatever namespace filter was active in the table view.
        let ns = String::new();

        // Index-addressed so the gather below can hand the results back in a
        // fixed order regardless of which finishes first.
        let kinds = [nodes, pods, deploys, sts, ds, jobs, pvc];

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                let mut p = Pulse::default();

                // Seven independent lists that used to run one after another,
                // so the dashboard took the sum of seven round-trips on every
                // refresh. They are bounded rather than unbounded: a health
                // snapshot must not itself be a burst of load.
                let mut lists = gather_lists(&client, &kinds, &ns).await;

                // A denied/failed list must not render as "0 healthy" tiles —
                // record it so the view can say the numbers are incomplete.
                // Folded in kind order, so the reported failure does not
                // depend on which list lost the race.
                let mut warn = None;
                for (_, w) in lists.iter_mut() {
                    if let Some(w) = w.take() {
                        warn.get_or_insert(w);
                    }
                }

                let [nodes, pods, deploys, sts, ds, jobs, pvc] = &lists;
                p.nodes_total = nodes.0.len();
                p.nodes_ready = nodes.0.iter().filter(|o| node_ready(o)).count();

                p.pods_total = pods.0.len();
                for o in &pods.0 {
                    match phase(o).as_str() {
                        "Running" => p.pods_running += 1,
                        "Pending" => p.pods_pending += 1,
                        "Failed" => p.pods_failed += 1,
                        "Succeeded" => p.pods_succeeded += 1,
                        _ => {}
                    }
                }

                p.deploys_total = deploys.0.len();
                p.deploys_ready = deploys
                    .0
                    .iter()
                    .filter(|o| ready_eq(o, "/status/readyReplicas", "/spec/replicas"))
                    .count();

                p.sts_total = sts.0.len();
                p.sts_ready = sts
                    .0
                    .iter()
                    .filter(|o| ready_eq(o, "/status/readyReplicas", "/spec/replicas"))
                    .count();

                p.ds_total = ds.0.len();
                p.ds_ready =
                    ds.0.iter()
                        .filter(|o| {
                            ready_eq(o, "/status/numberReady", "/status/desiredNumberScheduled")
                        })
                        .count();

                p.jobs_total = jobs.0.len();

                p.pvc_total = pvc.0.len();
                p.pvc_bound = pvc.0.iter().filter(|o| phase(o) == "Bound").count();

                p.warn = warn;

                if tx
                    .send(Msg::PulseData {
                        generation: genr,
                        claim,
                        data: p,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    pub(super) fn key_pulse(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Table;
                self.start_watch();
            }
            KeyCode::Char('r') => {
                self.bump_generation();
                self.spawn_pulse();
            }
            _ => {}
        }
    }

    /// Open the xray tree for the current kind (owner → children → containers).
    pub fn open_xray(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        self.bump_generation();
        self.xray_items.clear();
        self.xray_state.select(Some(0));
        self.mode = Mode::Xray;
        self.spawn_xray();
    }

    pub(super) fn spawn_xray(&mut self) {
        let claim = self.claim_status(format!("xray: {}…", self.kind_plural));
        let Some((root_ar, root_nsd)) = self.kind.as_ref().map(|k| (k.ar.clone(), k.namespaced))
        else {
            return;
        };
        let root_kind = trim_s(&self.kind_plural).to_string();
        let all_pool_kinds: Vec<(String, ApiResource, bool)> = xray_pool_plurals(&root_kind)
            .iter()
            .filter_map(|plural| {
                self.cluster
                    .resolve(plural)
                    .map(|k| (trim_s(plural).to_string(), k.ar, k.namespaced))
            })
            .collect();
        let (pool_aliases_of_root, pool_kinds) = split_pool_kinds(all_pool_kinds, &root_ar);
        let requests: Vec<Option<(ApiResource, bool)>> =
            std::iter::once(Some((root_ar.clone(), root_nsd)))
                .chain(
                    pool_kinds
                        .iter()
                        .map(|(_, ar, nsd)| Some((ar.clone(), *nsd))),
                )
                .collect();

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let flag = self.gen_flag.clone();
        let ns = self.namespace.clone();

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                // The roots and every pool kind are independent lists that
                // used to run one after another; the tree cannot be built
                // until all of them are in, so serialising them only added
                // round-trips.
                let mut lists = gather_list_vec(&client, &requests, &ns).await;
                let mut warn = None;
                for (_, w) in lists.iter_mut() {
                    if let Some(w) = w.take() {
                        warn.get_or_insert(w);
                    }
                }
                let mut lists = lists.into_iter();
                let (roots, _) = lists.next().expect("the root list is always requested");

                let mut pool: Vec<(String, DynamicObject)> =
                    Vec::with_capacity(lists.len() * roots.len());
                for ((label, _, _), (items, _)) in pool_kinds.iter().zip(lists) {
                    pool.extend(items.into_iter().map(|o| (label.clone(), o)));
                }
                // A pool kind that *is* the root kind was fetched once, above.
                for label in &pool_aliases_of_root {
                    pool.extend(roots.iter().map(|o| (label.clone(), o.clone())));
                }

                let items = xray_flatten(&root_kind, &roots, &pool);

                if tx
                    .send(Msg::XrayData {
                        generation: genr,
                        claim,
                        items,
                        warn,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        self.tasks.push(handle);
    }

    pub(super) fn key_xray(&mut self, key: KeyEvent) {
        let len = self.xray_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Table;
                self.start_watch();
            }
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.xray_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.xray_state, len, false),
            KeyCode::Char('g') | KeyCode::Home => {
                if len > 0 {
                    self.xray_state.select(Some(0));
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if len > 0 {
                    self.xray_state.select(Some(len - 1));
                }
            }
            // Enter on a pod/container streams logs.
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(i) = self.xray_state.selected()
                    && let Some(item) = self.xray_items.get(i).cloned()
                {
                    match item.kind.as_str() {
                        "container" => self.launch_logs(
                            LogSource::Single {
                                ns: item.ns,
                                pod: item.name.clone(),
                                container: item.container.clone(),
                                previous: false,
                            },
                            format!(
                                "{}:{} — logs",
                                item.name,
                                item.container.unwrap_or_default()
                            ),
                        ),
                        "pod" => self.launch_logs(
                            LogSource::Pod {
                                ns: item.ns,
                                name: item.name.clone(),
                                containers: vec![],
                            },
                            format!("{} — logs", item.name),
                        ),
                        _ => self.flash_warn("logs available on pods/containers"),
                    }
                }
            }
            KeyCode::Char('r') => {
                self.bump_generation();
                self.spawn_xray();
            }
            _ => {}
        }
    }
}
