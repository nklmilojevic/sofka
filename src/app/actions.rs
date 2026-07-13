use super::*;

impl App {
    // ----- actions -------------------------------------------------------

    pub(super) fn request_delete(&mut self, force: bool) {
        if self.deny_readonly() {
            return;
        }
        // A Helm release row's underlying object is its storage Secret —
        // deleting that secret directly would corrupt Helm's own bookkeeping.
        // The actual semantic action is `helm uninstall`.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.request_helm_uninstall();
            return;
        }
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        let cascade = Cascade::Background;
        self.confirm_label = delete_confirm_label(&self.kind_plural, &targets, force, cascade);
        self.confirm_action = Some(ConfirmAction::Delete {
            targets,
            force,
            cascade,
        });
        self.mode = Mode::Confirm;
    }

    pub(super) fn spawn_patch_action<F>(
        &self,
        kind: Kind,
        targets: Vec<(String, String)>,
        patch: Patch<Value>,
        error_message: F,
    ) where
        F: Fn(&str, kube::Error) -> String + Send + 'static,
    {
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            for (name, ns) in targets {
                let api: Api<DynamicObject> = if kind.namespaced && !ns.is_empty() {
                    Api::namespaced_with(client.clone(), &ns, &kind.ar)
                } else {
                    Api::all_with(client.clone(), &kind.ar)
                };
                if let Err(e) = api.patch(&name, &PatchParams::default(), &patch).await {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: error_message(&name, e),
                        })
                        .await;
                }
            }
        });
    }

    pub(super) fn do_delete(
        &mut self,
        targets: Vec<(String, String)>,
        force: bool,
        cascade: Cascade,
    ) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = if targets.len() == 1 {
            format!("deleting {}…", targets[0].0)
        } else {
            format!("deleting {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        tokio::spawn(async move {
            let mut dp = DeleteParams {
                propagation_policy: Some(cascade.policy()),
                ..DeleteParams::default()
            };
            if force {
                dp = dp.grace_period(0);
            }
            for (name, ns) in targets {
                let api: Api<DynamicObject> = if kind.namespaced && !ns.is_empty() {
                    Api::namespaced_with(client.clone(), &ns, &kind.ar)
                } else {
                    Api::all_with(client.clone(), &kind.ar)
                };
                if let Err(e) = api.delete(&name, &dp).await {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: format!("delete {name} failed: {e}"),
                        })
                        .await;
                }
            }
        });
    }

    pub(super) fn request_cordon(&mut self, unschedulable: bool) {
        if self.deny_readonly() {
            return;
        }
        if self.kind_plural != "nodes" {
            self.flash_warn("cordon/uncordon applies to nodes");
            return;
        }
        let targets = self.node_action_targets();
        if targets.is_empty() {
            return;
        }
        self.do_cordon_nodes(targets, unschedulable);
    }

    pub(super) fn do_cordon_nodes(&mut self, targets: Vec<String>, unschedulable: bool) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let verb = if unschedulable {
            "cordoning"
        } else {
            "uncordoning"
        };
        self.flash = if targets.len() == 1 {
            format!("{verb} {}…", targets[0])
        } else {
            format!("{verb} {} nodes…", targets.len())
        };
        self.flash_err = false;
        let targets = targets
            .into_iter()
            .map(|name| (name, String::new()))
            .collect();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(node_unschedulable_patch(unschedulable)),
            move |name, e| format!("{verb} {name} failed: {e}"),
        );
    }

    pub(super) fn request_drain(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if self.kind_plural != "nodes" {
            self.flash_warn("drain applies to nodes");
            return;
        }
        let targets = self.node_action_targets();
        if targets.is_empty() {
            return;
        }
        self.confirm_label = if targets.len() == 1 {
            format!("Drain node {}? Cordon and evict eligible pods.", targets[0])
        } else {
            format!(
                "Drain {} nodes? Cordon and evict eligible pods.",
                targets.len()
            )
        };
        self.confirm_action = Some(ConfirmAction::Drain { targets });
        self.mode = Mode::Confirm;
    }

    pub(super) fn do_drain_nodes(&mut self, targets: Vec<String>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = if targets.len() == 1 {
            format!("draining {}…", targets[0])
        } else {
            format!("draining {} nodes…", targets.len())
        };
        self.flash_err = false;
        tokio::spawn(async move {
            let nodes: Api<DynamicObject> = Api::all_with(client.clone(), &kind.ar);
            let node_patch = Patch::Merge(node_unschedulable_patch(true));
            let pods: Api<Pod> = Api::all(client.clone());
            for node in targets {
                if let Err(e) = nodes
                    .patch(&node, &PatchParams::default(), &node_patch)
                    .await
                {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: format!("drain {node}: cordon failed: {e}"),
                        })
                        .await;
                    continue;
                }

                let listed = pods
                    .list(&ListParams::default().fields(&format!("spec.nodeName={node}")))
                    .await;
                let pod_list = match listed {
                    Ok(list) => list,
                    Err(e) => {
                        let _ = tx
                            .send(Msg::Error {
                                generation: genr,
                                error: format!("drain {node}: list pods failed: {e}"),
                            })
                            .await;
                        continue;
                    }
                };

                for pod in pod_list.items.iter().filter(|pod| drainable_pod(pod)) {
                    let Some(name) = pod.metadata.name.as_deref() else {
                        continue;
                    };
                    let ns = pod.metadata.namespace.as_deref().unwrap_or("default");
                    let pod_api: Api<Pod> = Api::namespaced(client.clone(), ns);
                    let evict = EvictParams {
                        delete_options: Some(DeleteParams::default()),
                        ..Default::default()
                    };
                    match pod_api.evict(name, &evict).await {
                        Ok(_) => {}
                        Err(e) if eviction_unsupported(&e) => {
                            if let Err(delete_err) =
                                pod_api.delete(name, &DeleteParams::default()).await
                            {
                                let _ = tx
                                    .send(Msg::Error {
                                        generation: genr,
                                        error: format!(
                                            "drain {node}: delete {ns}/{name} failed after eviction fallback: {delete_err}"
                                        ),
                                    })
                                    .await;
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Msg::Error {
                                    generation: genr,
                                    error: format!("drain {node}: evict {ns}/{name} failed: {e}"),
                                })
                                .await;
                        }
                    }
                }
            }
        });
    }

    pub(super) fn request_attach(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if self.kind_plural != "pods" {
            self.flash_warn("attach is only available for pods");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let mut argv = self.kubectl_base();
        argv.extend(["attach".into(), "-it".into(), "-n".into(), ns, name]);
        self.pending = Some(Suspend::Shell(argv));
    }

    /// Navigate to the node hosting the selected pod (k9s `o`).
    pub(super) fn show_node(&mut self) {
        if self.kind_plural != "pods" {
            self.flash_warn("'o' shows the node for a pod");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let Some(node) = obj.data.pointer("/spec/nodeName").and_then(Value::as_str) else {
            self.flash_warn("pod has no node assigned");
            return;
        };
        let node = node.to_string();
        let pod_name = obj.metadata.name.clone().unwrap_or_default();
        let Some(nodes) = self.cluster.resolve("nodes") else {
            self.flash_warn("nodes kind unavailable");
            return;
        };
        self.push_frame();
        self.kind = Some(nodes);
        self.kind_plural = "nodes".into();
        self.namespace = String::new();
        self.labels = None;
        self.fields = Some(format!("metadata.name={node}"));
        self.scope_label = Some(format!("host of {pod_name}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.start_watch();
    }

    /// Jump to the selected object's controller/owner (k9s Shift-J).
    pub(super) fn jump_owner(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let owners = obj
            .metadata
            .owner_references
            .as_ref()
            .filter(|o| !o.is_empty());
        let Some(owner) = owners.and_then(|o| o.first()) else {
            self.flash_warn("no owner reference");
            return;
        };
        let Some(kind) = self.cluster.resolve(&owner.kind.to_lowercase()) else {
            self.flash_warn(&format!("owner kind {} unresolved", owner.kind));
            return;
        };
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let owner_name = owner.name.clone();
        let child_name = obj.metadata.name.clone().unwrap_or_default();
        self.push_frame();
        self.kind_plural = kind.ar.plural.to_lowercase();
        self.kind = Some(kind);
        self.namespace = ns;
        self.labels = None;
        self.fields = Some(format!("metadata.name={owner_name}"));
        self.scope_label = Some(format!("owner of {child_name}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.start_watch();
    }

    /// Copy the (filtered) log buffer to the clipboard (k9s `c` in logs).
    pub(super) fn copy_logs(&mut self) {
        let text = self.filtered_log_text();
        if text.is_empty() {
            self.flash_warn("no log lines to copy");
            return;
        }
        let n = text.lines().count();
        self.copy_to_clipboard_async(
            text,
            format!("copied {n} log lines"),
            "no clipboard target found (pbcopy/xclip/wl-copy/OSC 52)",
        );
    }

    /// Save the filtered log buffer to a temp file (k9s Ctrl-S).
    pub(super) fn save_logs(&mut self) {
        let text = self.filtered_log_text();
        if text.is_empty() {
            self.flash_warn("no log lines to save");
            return;
        }
        let genr = self.log_gen;
        let tx = self.tx.clone();
        let ts = k8s_openapi::jiff::Timestamp::now().as_second();
        let safe: String = self
            .logs
            .view
            .title
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let path = std::env::temp_dir().join(format!("sofka-{safe}-{ts}.log"));
        tokio::spawn(async move {
            let result = tokio::fs::write(&path, text)
                .await
                .map(|_| path)
                .map_err(|e| e.to_string());
            let _ = tx
                .send(Msg::LogsSaved {
                    generation: genr,
                    result,
                })
                .await;
        });
    }

    pub(super) fn filtered_log_text(&self) -> String {
        let f = self.logs.filter.to_lowercase();
        self.logs
            .view
            .lines
            .iter()
            .filter(|l| f.is_empty() || l.to_lowercase().contains(&f))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Copy the current single-document view (YAML/describe/diff/events) to
    /// the clipboard (k9s `c` in those views). Mirrors [`Self::copy_logs`]:
    /// with a `/` search active, only the matching lines are copied.
    pub(super) fn copy_doc(&mut self) {
        let text = self.filtered_doc_text();
        if text.is_empty() {
            self.flash_warn("nothing to copy");
            return;
        }
        let n = text.lines().count();
        self.copy_to_clipboard_async(
            text,
            format!("copied {n} lines"),
            "no clipboard target found (pbcopy/xclip/wl-copy/OSC 52)",
        );
    }

    pub(super) fn filtered_doc_text(&self) -> String {
        let f = self.detail.filter.to_lowercase();
        self.detail
            .lines
            .iter()
            .filter(|l| f.is_empty() || l.to_lowercase().contains(&f))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Copy the selected resource's name to the system clipboard (k9s `c`).
    pub(super) fn copy_name(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        self.copy_to_clipboard_async(
            name.clone(),
            format!("copied: {name}"),
            "no clipboard target found (pbcopy/xclip/wl-copy/OSC 52)",
        );
    }

    pub(super) fn copy_to_clipboard_async(&self, text: String, success: String, failure: &str) {
        let tx = self.tx.clone();
        let genr = self.generation;
        let failure = failure.to_string();
        tokio::spawn(async move {
            let copied = tokio::task::spawn_blocking(move || copy_to_clipboard(&text))
                .await
                .unwrap_or(false);
            let _ = tx
                .send(Msg::ClipboardCopied {
                    generation: genr,
                    copied,
                    success,
                    failure,
                })
                .await;
        });
    }

    /// Previous-container logs for the selected pod (k9s `p` on a pod row).
    pub(super) fn open_previous_logs(&mut self) {
        if self.kind_plural != "pods" {
            self.flash_warn("previous logs are for pods (use the container picker elsewhere)");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let containers = container_names(obj);
        let container = containers.into_iter().next();
        self.launch_logs(
            LogSource::Single {
                ns,
                pod: name.clone(),
                container,
                previous: true,
            },
            format!("{name} — previous logs"),
        );
    }

    /// Rollout-restart a workload by stamping the template annotation (k9s `r`).
    pub(super) fn request_restart(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let now = k8s_openapi::jiff::Timestamp::now().to_string();
        self.flash = format!("restarting {name}…");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Strategic(restart_patch(&now)),
            |_, e| format!("restart failed: {e}"),
        );
    }

    /// Open the Set-Image picker for the selected workload/pod (k9s `i`).
    pub(super) fn request_set_image(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let is_pod = self.kind_plural == "pods";
        let workload = matches!(
            self.kind_plural.as_str(),
            "deployments"
                | "statefulsets"
                | "daemonsets"
                | "replicasets"
                | "replicationcontrollers"
        );
        if !is_pod && !workload {
            self.flash_warn("set image applies to pods and workload controllers");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let ptr = if is_pod {
            "/spec/containers"
        } else {
            "/spec/template/spec/containers"
        };
        let Some(cs) = obj.data.pointer(ptr).and_then(Value::as_array) else {
            self.flash_warn("no containers found");
            return;
        };
        let mut names = Vec::new();
        let mut images = Vec::new();
        for c in cs {
            names.push(
                c.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            );
            images.push(
                c.get("image")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
        if names.is_empty() {
            self.flash_warn("no containers found");
            return;
        }
        let target = (
            obj.metadata.namespace.clone().unwrap_or_default(),
            obj.metadata.name.clone().unwrap_or_default(),
            self.kind_plural.clone(),
        );
        self.container_list = names;
        self.image_values = images;
        self.image_target = Some(target);
        self.container_state.select(Some(0));
        self.mode = Mode::SetImage;
    }

    pub(super) fn key_set_image(&mut self, key: KeyEvent) {
        let len = self.container_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.container_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.container_state, len, false),
            KeyCode::Enter => {
                if let Some(i) = self.container_state.selected()
                    && let Some(container) = self.container_list.get(i).cloned()
                    && let Some((ns, name, plural)) = self.image_target.clone()
                {
                    self.prompt_label = format!("New image for {container}:");
                    self.prompt_input = self.image_values.get(i).cloned().unwrap_or_default();
                    self.prompt_kind = Some(PromptKind::SetImage {
                        ns,
                        name,
                        plural,
                        container,
                    });
                    self.mode = Mode::Prompt;
                }
            }
            _ => {}
        }
    }

    pub(super) fn do_set_image(
        &mut self,
        ns: String,
        name: String,
        plural: String,
        container: String,
        image: String,
    ) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        self.flash = format!("setting image: {container} → {image}");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Strategic(set_image_patch(&plural, &container, &image)),
            |_, e| format!("set image failed: {e}"),
        );
    }

    pub(super) fn request_edit(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let mut argv = self.kubectl_base();
        argv.extend(["edit".into(), self.kind_plural.clone(), name]);
        if let Some(ns) = &obj.metadata.namespace {
            argv.push("-n".into());
            argv.push(ns.clone());
        }
        self.pending = Some(Suspend::Shell(argv));
    }

    pub(super) fn request_exec(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if self.kind_plural != "pods" {
            self.flash_warn("shell is only available for pods");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.exec_into(ns, name, None);
    }

    /// Shell into `pod`, optionally pinned to `container` (k9s-style `-c`).
    /// Shared by the plain pod-row shell (`s`) and the per-container picker.
    pub(super) fn exec_into(&mut self, ns: String, pod: String, container: Option<String>) {
        if self.deny_readonly() {
            return;
        }
        let mut argv = self.kubectl_base();
        argv.extend(["exec".into(), "-it".into(), "-n".into(), ns, pod]);
        if let Some(c) = container {
            argv.push("-c".into());
            argv.push(c);
        }
        argv.extend([
            "--".into(),
            "sh".into(),
            "-c".into(),
            "command -v bash >/dev/null 2>&1 && exec bash || exec sh".into(),
        ]);
        self.pending = Some(Suspend::Shell(argv));
    }

    pub(super) fn request_scale(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if !matches!(
            self.kind_plural.as_str(),
            "deployments" | "statefulsets" | "replicasets"
        ) {
            self.flash_warn("scale applies to deployments/statefulsets/replicasets");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let cur = obj
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.prompt_label = format!("Scale {name} to replicas (current {cur}):");
        self.prompt_input.clear();
        self.prompt_kind = Some(PromptKind::Scale { ns, name });
        self.mode = Mode::Prompt;
    }

    pub(super) fn request_port_forward(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        if !matches!(self.kind_plural.as_str(), "pods" | "services") {
            self.flash_warn("port-forward applies to pods/services");
            return;
        }
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.prompt_label = format!("Port-forward {name} (LOCAL:REMOTE, e.g. 8080:80):");
        self.prompt_input.clear();
        self.prompt_kind = Some(PromptKind::PortForward { ns, name });
        self.mode = Mode::Prompt;
    }

    /// Start `kubectl port-forward` in the background (not a foreground
    /// `Suspend::Shell` — a forward should keep running while you keep
    /// browsing). stdio is nulled since the TUI still owns the terminal.
    pub(super) fn start_port_forward(&mut self, ns: String, target: String, ports: String) {
        let mut argv = self.kubectl_base();
        argv.extend([
            "port-forward".into(),
            "-n".into(),
            ns.clone(),
            target.clone(),
            ports.clone(),
        ]);
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let pf = PortForward {
                    ns,
                    target,
                    ports,
                    child,
                };
                self.flash = format!("port-forwarding {} (:pf to view/stop)", pf.label());
                self.flash_err = false;
                self.port_forwards.push(pf);
            }
            Err(e) => self.flash_warn(&format!("port-forward failed to start: {e}")),
        }
    }

    /// Drop any forward whose `kubectl` process has already exited (pod
    /// restarted, connection dropped, port in use, …), flashing a heads-up.
    /// Called on every tick, so a dead forward doesn't linger in the list.
    pub fn reap_port_forwards(&mut self) {
        let mut i = 0;
        while i < self.port_forwards.len() {
            match self.port_forwards[i].child.try_wait() {
                Ok(Some(_)) => {
                    let pf = self.port_forwards.remove(i);
                    self.flash_warn(&format!("port-forward {} exited", pf.label()));
                }
                _ => i += 1,
            }
        }
    }

    pub(super) fn open_port_forwards(&mut self) {
        self.pf_state.select(if self.port_forwards.is_empty() {
            None
        } else {
            Some(0)
        });
        self.mode = Mode::PortForwards;
    }

    pub(super) fn key_port_forwards(&mut self, key: KeyEvent) {
        let len = self.port_forwards.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.pf_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.pf_state, len, false),
            KeyCode::Char('x') | KeyCode::Char('s') => self.stop_selected_port_forward(),
            _ => {}
        }
    }

    pub(super) fn open_skins(&mut self) {
        self.skin_state.select(if self.skin_list.is_empty() {
            None
        } else {
            Some(0)
        });
        self.mode = Mode::Skins;
    }

    pub(super) fn key_skins(&mut self, key: KeyEvent) {
        let len = self.skin_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.skin_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.skin_state, len, false),
            KeyCode::Enter => {
                if let Some(name) = self
                    .skin_state
                    .selected()
                    .and_then(|i| self.skin_list.get(i).cloned())
                {
                    self.apply_skin(&name);
                }
                self.mode = Mode::Table;
            }
            _ => {}
        }
    }

    pub(super) fn apply_skin(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.open_skins();
            return;
        }
        if crate::theme::builtin(&name.to_ascii_lowercase()).is_none() {
            self.flash_warn(&format!("unknown skin: {name}"));
            return;
        }
        let palette = crate::theme::resolve_skin(Some(name), &self.skin_colors);
        crate::theme::set(palette);
        // A manual choice becomes the session skin, so it survives context
        // switches into contexts without a config skin override.
        self.session_skin = Some(name.to_string());
        self.active_skin = Some(name.to_string());
        self.flash = format!("skin: {name}");
        self.flash_err = false;
    }

    /// Re-resolve the skin when the context changes: a skin named by a
    /// cluster/context override file wins, otherwise the session skin (config
    /// `skin.name`, the auto-detected default, or the last `:skin` choice).
    pub(super) fn apply_context_skin(&mut self, override_skin: Option<String>) {
        let Some(name) = override_skin.or_else(|| self.session_skin.clone()) else {
            return;
        };
        if crate::theme::builtin(&name.trim().to_ascii_lowercase()).is_none() {
            self.flash_warn(&format!("unknown skin '{name}' in config"));
            return;
        }
        let palette = crate::theme::resolve_skin(Some(&name), &self.skin_colors);
        crate::theme::set(palette);
        self.active_skin = Some(name);
    }

    /// `:reload` — re-read the configuration from disk and apply it live:
    /// aliases, plugins, the log provider, skin (+ per-swatch overrides),
    /// background fill, and read-only mode. Launch defaults (`default_namespace`/`default_resource`)
    /// are deliberately not re-applied — a reload must never yank the current
    /// view. A reload that fails validation keeps the last known-good config
    /// active and reports the precise error (file, key, what's wrong) instead.
    pub(super) fn reload_config(&mut self) {
        let loader = match self.config.reload() {
            Ok(l) => l,
            Err(e) => {
                self.config_warnings = vec![e];
                self.flash_warn(
                    "config reload failed — previous config kept (:config for details)",
                );
                return;
            }
        };
        self.config = loader;
        let resolved = self
            .config
            .resolve(&self.cluster.context, &self.cluster.cluster_name);
        let mut warnings = resolved.warnings;
        self.user_aliases = resolved.config.aliases;
        self.plugins = resolved.config.plugins;
        self.bookmarks = resolved.config.bookmarks;
        self.workspaces = resolved.config.workspaces;
        warnings.extend(crate::config::plugin_warnings(&self.plugins));
        warnings.extend(crate::config::bookmark_warnings(&self.bookmarks));
        warnings.extend(crate::config::workspace_warnings(&self.workspaces));
        // Thresholds only change cell coloring (never the column layout), so —
        // unlike custom views — they're safe to re-apply live without yanking
        // the current view.
        let (thresholds, threshold_warnings) =
            crate::thresholds::compile(&resolved.config.thresholds);
        self.thresholds = thresholds;
        warnings.extend(threshold_warnings);
        let (log_provider, provider_warnings) =
            crate::providers::compile(resolved.config.providers.logs.as_ref());
        self.log_provider = log_provider;
        warnings.extend(provider_warnings);
        self.skin_colors = resolved.config.skin.colors;
        self.readonly = self.readonly_override.unwrap_or(resolved.config.readonly);
        self.cluster.add_aliases(&self.user_aliases);
        crate::theme::set_background(resolved.config.skin.background);
        // The skin named by the base config (if any) becomes the session skin
        // again — a reload is an explicit "use what the files say now". With
        // none configured, the current session skin (auto-detected or the last
        // `:skin` choice) stays put.
        if let Some(name) = self.config.resolve("", "").config.skin.name {
            self.session_skin = Some(name);
        }
        warnings.extend(crate::theme::validate_skin(
            resolved
                .skin_override
                .as_deref()
                .or(self.session_skin.as_deref()),
            &self.skin_colors,
        ));
        self.apply_context_skin(resolved.skin_override);
        self.config_warnings = warnings;
        if self.config_warnings.is_empty() {
            self.flash = "config reloaded".into();
            self.flash_err = false;
        } else {
            self.flash_warn(&format!(
                "config reloaded with {} warning(s) — :config for details",
                self.config_warnings.len()
            ));
        }
    }

    /// `:config` — a document view of the configuration currently in effect:
    /// every source path (and whether it loaded), the active skin and mode,
    /// and any validation errors/warnings from the last (re)load.
    pub(super) fn open_config_info(&mut self) {
        self.set_return_mode();
        let mut lines: Vec<String> = vec!["Sources".into()];
        match self.config.base_path() {
            Some(path) => {
                let state = if self.config.has_base() {
                    "loaded"
                } else if path.exists() {
                    "invalid — using defaults"
                } else {
                    "absent — using defaults"
                };
                lines.push(format!("  {} ({state})", path.display()));
            }
            None => lines.push("  no config directory — using defaults".into()),
        }
        for path in self
            .config
            .override_paths(&self.cluster.context, &self.cluster.cluster_name)
        {
            lines.push(format!(
                "  {} ({})",
                path.display(),
                crate::config::file_state(&path)
            ));
        }
        lines.push(String::new());
        lines.push("Active".into());
        lines.push(format!(
            "  skin: {}",
            self.active_skin.as_deref().unwrap_or("auto")
        ));
        lines.push(format!("  readonly: {}", self.readonly));
        lines.push(format!("  aliases: {}", self.user_aliases.len()));
        lines.push(format!("  plugins: {}", self.plugins.len()));
        lines.push(String::new());
        if self.config_warnings.is_empty() {
            lines.push("No validation warnings.".into());
        } else {
            lines.push(format!("Warnings [{}]", self.config_warnings.len()));
            for w in &self.config_warnings {
                for (i, l) in w.lines().enumerate() {
                    let bullet = if i == 0 { "• " } else { "  " };
                    lines.push(format!("  {bullet}{l}"));
                }
            }
        }
        self.detail = Scrollable {
            title: "config — :reload to re-read".into(),
            lines: lines.into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }

    /// Stop (kill) the selected forward. Others keep running.
    pub(super) fn stop_selected_port_forward(&mut self) {
        let Some(i) = self.pf_state.selected() else {
            return;
        };
        if i >= self.port_forwards.len() {
            return;
        }
        let pf = self.port_forwards.remove(i); // dropped -> Drop kills the child
        self.flash = format!("stopped port-forward {}", pf.label());
        self.flash_err = false;
        self.pf_state.select(if self.port_forwards.is_empty() {
            None
        } else {
            Some(i.min(self.port_forwards.len() - 1))
        });
    }

    pub(super) fn do_scale(&mut self, ns: String, name: String, replicas: i32) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        self.flash = format!("scaling {name} → {replicas}");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Merge(scale_patch(replicas)),
            |_, e| format!("scale failed: {e}"),
        );
    }

    /// Open the Flux suspend/resume menu (`t`) for the marked rows, or the
    /// current selection if none are marked. A menu, not a single-key
    /// toggle — suspending something always takes an explicit, visible
    /// choice (`j`/`k` + Enter) rather than one accidental keystroke.
    pub(super) fn request_flux_menu(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if !self.flux_suspendable() {
            self.flash_warn("suspend/resume only applies to Flux resources (ks/hr/git-, helm-, oci-repos, buckets, image automation, alerts, receivers)");
            return;
        }
        if self.action_targets().is_empty() {
            return;
        }
        self.flux_menu_state.select(Some(0));
        self.mode = Mode::FluxMenu;
    }

    pub(super) fn key_flux_menu(&mut self, key: KeyEvent) {
        let len = FLUX_MENU_ITEMS.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.flux_menu_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.flux_menu_state, len, false),
            KeyCode::Enter => {
                let choice = self
                    .flux_menu_state
                    .selected()
                    .and_then(|i| FLUX_MENU_ITEMS.get(i))
                    .copied();
                self.mode = Mode::Table;
                match choice {
                    Some("Suspend") => {
                        let targets = self.action_targets();
                        self.do_set_suspend(targets, true);
                    }
                    Some("Resume") => {
                        let targets = self.action_targets();
                        self.do_set_suspend(targets, false);
                    }
                    Some("Reconcile now") => {
                        let targets = self.action_targets();
                        self.do_reconcile(targets);
                    }
                    _ => {} // "Cancel" or nothing selected — do nothing.
                }
            }
            _ => {}
        }
    }

    pub(super) fn do_set_suspend(&mut self, targets: Vec<(String, String)>, suspend: bool) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let verb = if suspend { "suspending" } else { "resuming" };
        self.flash = if targets.len() == 1 {
            format!("{verb} {}…", targets[0].0)
        } else {
            format!("{verb} {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(suspend_patch(suspend)),
            move |name, e| format!("{verb} {name} failed: {e}"),
        );
    }

    /// Force an immediate Flux reconciliation, bypassing the normal interval —
    /// patches `reconcile.fluxcd.io/requestedAt`, the same annotation `flux
    /// reconcile` sets, watched by every toolkit controller.
    pub(super) fn do_reconcile(&mut self, targets: Vec<(String, String)>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let now = k8s_openapi::jiff::Timestamp::now().to_string();
        self.flash = if targets.len() == 1 {
            format!("reconciling {}…", targets[0].0)
        } else {
            format!("reconciling {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(reconcile_patch(&now)),
            |name, e| format!("reconcile {name} failed: {e}"),
        );
    }

    /// Force an immediate External Secrets Operator refresh on the marked rows
    /// (or the current selection), matching the k9s external-secrets plugin.
    pub(super) fn request_refresh_es(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if !EXTERNAL_SECRET_KINDS.contains(&self.kind_plural.as_str()) {
            self.flash_warn(
                "refresh only applies to external secrets (externalsecrets, pushsecrets)",
            );
            return;
        }
        let targets = self.action_targets();
        if targets.is_empty() {
            return;
        }
        self.do_refresh_es(targets);
    }

    /// Stamp the `force-sync` annotation ESO watches to reconcile a secret out
    /// of band — the same annotation the k9s plugin overwrites. The value only
    /// has to change to trigger a sync; a unix timestamp mirrors k9s' `date +%s`.
    pub(super) fn do_refresh_es(&mut self, targets: Vec<(String, String)>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        let now = k8s_openapi::jiff::Timestamp::now().as_second().to_string();
        self.flash = if targets.len() == 1 {
            format!("refreshing {}…", targets[0].0)
        } else {
            format!("refreshing {} {}…", targets.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        self.spawn_patch_action(
            kind,
            targets,
            Patch::Merge(external_secret_refresh_patch(&now)),
            |name, e| format!("refresh {name} failed: {e}"),
        );
    }

    /// Refuse a mutating action in read-only mode: flashes a warning and
    /// returns true when the caller must bail out.
    pub(super) fn deny_readonly(&mut self) -> bool {
        if self.readonly {
            self.flash_warn("read-only mode — action disabled");
        }
        self.readonly
    }

    pub(super) fn flash_warn(&mut self, msg: &str) {
        self.flash = msg.to_string();
        self.flash_err = true;
    }

    /// Base argv for a `kubectl` shell-out, pinned to the active context so it
    /// can't target a different cluster than the one we're viewing.
    pub(super) fn kubectl_base(&self) -> Vec<String> {
        let mut argv = vec!["kubectl".to_string()];
        if let Some(ctx) = self.cluster.kubectl_context() {
            argv.push("--context".to_string());
            argv.push(ctx.to_string());
        }
        argv
    }

    /// Base argv for a `helm` shell-out, pinned to the active context exactly
    /// like [`Self::kubectl_base`]. Rollback and uninstall are the only two
    /// Helm actions sofka can't do natively (see `crate::helm`) — Helm's own
    /// three-way-merge apply/delete logic is delegated to the real `helm`
    /// binary rather than reimplemented.
    pub(super) fn helm_base(&self) -> Vec<String> {
        let mut argv = vec!["helm".to_string()];
        if let Some(ctx) = self.cluster.kubectl_context() {
            argv.push("--kube-context".to_string());
            argv.push(ctx.to_string());
        }
        argv
    }

    /// Roll back the selected revision's release to it (k9s: `r` in the
    /// History view). Only ever acts on the single selected row — rolling
    /// back is not a bulk action.
    pub(super) fn request_helm_rollback(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if self.kind_plural != "helmhistory" {
            self.flash_warn("rollback applies to a Helm release's revision history");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let Some(name) = crate::helm::release_name(obj).map(str::to_string) else {
            self.flash_warn("not a Helm release secret");
            return;
        };
        let Some(revision) = crate::helm::revision(obj) else {
            self.flash_warn("could not determine this revision's number");
            return;
        };
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.confirm_label = format!("Roll back {name} in {ns} to revision {revision}?");
        self.confirm_action = Some(ConfirmAction::HelmRollback {
            ns,
            name,
            revision: revision.to_string(),
        });
        self.mode = Mode::Confirm;
    }

    pub(super) fn do_helm_rollback(&mut self, ns: String, name: String, revision: String) {
        let mut argv = self.helm_base();
        argv.extend([
            "rollback".to_string(),
            name.clone(),
            revision.clone(),
            "-n".to_string(),
            ns,
        ]);
        self.flash = format!("rolling back {name} to revision {revision}…");
        self.flash_err = false;
        let tx = self.tx.clone();
        let genr = self.generation;
        // No manual re-watch on success: the live `secrets` watch backing the
        // helm/helmhistory view picks up the new revision Secret on its own.
        tokio::spawn(async move {
            if let Err(e) = run_helm(&argv).await {
                let _ = tx
                    .send(Msg::Error {
                        generation: genr,
                        error: format!("helm rollback {name} failed: {e}"),
                    })
                    .await;
            }
        });
    }

    /// Uninstall the selected (or marked) Helm release(s) (k9s: `ctrl-d` /
    /// Delete on the release list or its history).
    pub(super) fn request_helm_uninstall(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let targets = self.helm_action_targets();
        if targets.is_empty() {
            return;
        }
        self.confirm_label = if targets.len() == 1 {
            format!(
                "Uninstall Helm release {} in {}? This deletes all its resources.",
                targets[0].0, targets[0].1
            )
        } else {
            format!(
                "Uninstall {} Helm releases? This deletes all their resources.",
                targets.len()
            )
        };
        self.confirm_action = Some(ConfirmAction::HelmUninstall { targets });
        self.mode = Mode::Confirm;
    }

    pub(super) fn do_helm_uninstall(&mut self, targets: Vec<(String, String)>) {
        self.flash = if targets.len() == 1 {
            format!("uninstalling {}…", targets[0].0)
        } else {
            format!("uninstalling {} Helm releases…", targets.len())
        };
        self.flash_err = false;
        let helm_base = self.helm_base();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            for (name, ns) in targets {
                let mut argv = helm_base.clone();
                argv.extend(["uninstall".to_string(), name.clone(), "-n".to_string(), ns]);
                if let Err(e) = run_helm(&argv).await {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: format!("helm uninstall {name} failed: {e}"),
                        })
                        .await;
                }
            }
        });
    }
}

/// Run a `helm` subprocess to completion, following the same
/// missing-binary/non-zero-exit handling as `describe()`'s `kubectl` shell-out.
async fn run_helm(argv: &[String]) -> std::result::Result<(), String> {
    match tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .await
    {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(err.lines().next().unwrap_or("error").to_string())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("helm not found on PATH".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}
