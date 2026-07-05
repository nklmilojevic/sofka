use super::*;

impl App {
    /// Open the pulse / cluster-health dashboard (k9s `:pulse`).
    pub fn open_pulse(&mut self) {
        self.bump_generation();
        self.pulse = Pulse::default();
        self.flash = "pulse — cluster health".into();
        self.flash_err = false;
        self.mode = Mode::Pulse;
        self.spawn_pulse();
    }

    pub(super) fn spawn_pulse(&mut self) {
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

        let handle = tokio::spawn(async move {
            loop {
                if flag.load(Ordering::SeqCst) != genr {
                    break;
                }
                let mut p = Pulse::default();

                if let Some((ar, _)) = &nodes {
                    let items = list_kind(&client, ar, false, "").await;
                    p.nodes_total = items.len();
                    p.nodes_ready = items.iter().filter(|o| node_ready(o)).count();
                }
                if let Some((ar, nsd)) = &pods {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.pods_total = items.len();
                    for o in &items {
                        match phase(o).as_str() {
                            "Running" => p.pods_running += 1,
                            "Pending" => p.pods_pending += 1,
                            "Failed" => p.pods_failed += 1,
                            "Succeeded" => p.pods_succeeded += 1,
                            _ => {}
                        }
                    }
                }
                if let Some((ar, nsd)) = &deploys {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.deploys_total = items.len();
                    p.deploys_ready = items
                        .iter()
                        .filter(|o| ready_eq(o, "/status/readyReplicas", "/spec/replicas"))
                        .count();
                }
                if let Some((ar, nsd)) = &sts {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.sts_total = items.len();
                    p.sts_ready = items
                        .iter()
                        .filter(|o| ready_eq(o, "/status/readyReplicas", "/spec/replicas"))
                        .count();
                }
                if let Some((ar, nsd)) = &ds {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.ds_total = items.len();
                    p.ds_ready = items
                        .iter()
                        .filter(|o| {
                            ready_eq(o, "/status/numberReady", "/status/desiredNumberScheduled")
                        })
                        .count();
                }
                if let Some((ar, nsd)) = &jobs {
                    p.jobs_total = list_kind(&client, ar, *nsd, &ns).await.len();
                }
                if let Some((ar, nsd)) = &pvc {
                    let items = list_kind(&client, ar, *nsd, &ns).await;
                    p.pvc_total = items.len();
                    p.pvc_bound = items.iter().filter(|o| phase(o) == "Bound").count();
                }

                if tx
                    .send(Msg::PulseData {
                        generation: genr,
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
        self.flash = format!("xray: {}", self.kind_plural);
        self.flash_err = false;
        self.mode = Mode::Xray;
        self.spawn_xray();
    }

    pub(super) fn spawn_xray(&mut self) {
        let Some((root_ar, root_nsd)) = self.kind.as_ref().map(|k| (k.ar.clone(), k.namespaced))
        else {
            return;
        };
        let root_kind = trim_s(&self.kind_plural).to_string();
        let pool_kinds: Vec<(String, ApiResource, bool)> = xray_pool_plurals(&root_kind)
            .iter()
            .filter_map(|plural| {
                self.cluster
                    .resolve(plural)
                    .map(|k| (trim_s(plural).to_string(), k.ar, k.namespaced))
            })
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
                let roots = list_kind(&client, &root_ar, root_nsd, &ns).await;
                let mut pool: Vec<(String, DynamicObject)> = Vec::new();
                for (label, ar, namespaced) in &pool_kinds {
                    for o in list_kind(&client, ar, *namespaced, &ns).await {
                        pool.push((label.clone(), o));
                    }
                }

                // Index children by owner uid.
                let mut children: HashMap<String, Vec<(String, DynamicObject)>> = HashMap::new();
                for (label, o) in &pool {
                    if let Some(owners) = &o.metadata.owner_references {
                        for owner in owners {
                            children
                                .entry(owner.uid.clone())
                                .or_default()
                                .push((label.clone(), o.clone()));
                        }
                    }
                }

                let mut items = Vec::new();
                for root in &roots {
                    emit_xray(&root_kind, root, 0, &children, &mut items);
                }

                if tx
                    .send(Msg::XrayData {
                        generation: genr,
                        items,
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
            KeyCode::Char('g') | KeyCode::Home => self.xray_state.select(Some(0)),
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
