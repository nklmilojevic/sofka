use super::*;

/// Interactive-shell entrypoint for `exec`/`debug`: prefer bash when the image
/// ships it, otherwise fall back to sh, in a single `sh -c` invocation.
const SHELL_FALLBACK: &str = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";

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
        let action = if force { "force-delete" } else { "delete" };
        let plural = self.kind_plural.clone();
        let Some(level) = self.guard(action, &plural, &targets, ConfirmLevel::Plain) else {
            return;
        };
        let cascade = Cascade::Background;
        let managed = self.recreated_note();
        let label = delete_confirm_label(&plural, &targets, force, cascade, managed.as_deref());
        let name_hint = if targets.len() == 1 {
            targets[0].0.clone()
        } else {
            targets.len().to_string()
        };
        self.begin_guarded(
            ConfirmAction::Delete {
                targets,
                force,
                cascade,
                managed,
            },
            label,
            level,
            name_hint,
        );
    }

    /// A short "managed by X — recreated on delete" warning when any object in
    /// the current delete selection is owned by Flux or a controller (so the
    /// user knows deletion won't stick). `None` when nothing is managed.
    fn recreated_note(&self) -> Option<String> {
        let objs = self.action_target_objects();
        let managed: Vec<String> = objs.iter().filter_map(managed_by).collect();
        let (mine, total) = (managed.len(), objs.len());
        let first = managed.into_iter().next()?;
        Some(if total == 1 {
            format!("⚠ managed by {first} — recreated on delete")
        } else {
            format!("⚠ {mine}/{total} managed (e.g. {first}) — recreated on delete")
        })
    }

    /// The objects targeted by a bulk-or-single action (marked rows, else the
    /// selection), as owned clones.
    fn action_target_objects(&self) -> Vec<DynamicObject> {
        if self.marked.is_empty() {
            self.selected_ref().cloned().into_iter().collect()
        } else {
            self.rows()
                .into_iter()
                .filter(|o| self.marked.contains(&row_key(o)))
                .cloned()
                .collect()
        }
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
        let label = self.action_label(&targets);
        self.note_action(if force { "force-delete" } else { "delete" }, label);
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
        self.note_action(
            if unschedulable { "cordon" } else { "uncordon" },
            node_targets_label(&targets),
        );
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
        // Guardrails match on (name, namespace); nodes are cluster-scoped.
        let pairs: Vec<(String, String)> =
            targets.iter().map(|n| (n.clone(), String::new())).collect();
        let Some(level) = self.guard("drain", "nodes", &pairs, ConfirmLevel::Plain) else {
            return;
        };
        let label = if targets.len() == 1 {
            format!("Drain node {}? Cordon and evict eligible pods.", targets[0])
        } else {
            format!(
                "Drain {} nodes? Cordon and evict eligible pods.",
                targets.len()
            )
        };
        let name_hint = if targets.len() == 1 {
            targets[0].clone()
        } else {
            targets.len().to_string()
        };
        self.begin_guarded(ConfirmAction::Drain { targets }, label, level, name_hint);
    }

    pub(super) fn do_drain_nodes(&mut self, targets: Vec<String>) {
        let Some(kind) = self.kind.clone() else {
            return;
        };
        self.note_action("drain", node_targets_label(&targets));
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
        self.logs
            .view
            .lines
            .iter()
            .filter(|l| self.logs.matches(l))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Copy the current single-document view (YAML/describe/diff/events) to
    /// the clipboard (k9s `c` in those views). Copies the whole document — the
    /// `/` search highlights in place, it doesn't filter, so there is no
    /// "matching subset" to copy.
    pub(super) fn copy_doc(&mut self) {
        let text = self.doc_text();
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

    pub(super) fn doc_text(&self) -> String {
        self.detail
            .lines
            .iter()
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
        self.note_action("restart", format!("{name} in {ns}"));
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
        self.note_action(
            format!("set-image {container}={image}"),
            format!("{name} in {ns}"),
        );
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
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let edit_label = if ns.is_empty() {
            name.clone()
        } else {
            format!("{name} in {ns}")
        };
        // A Flux-managed object gets its spec reverted on the next reconcile —
        // warn (and confirm) before opening the editor.
        let flux = flux_managed_by(obj);
        let mut argv = self.kubectl_base();
        argv.extend(["edit".into(), self.kind_plural.clone(), name]);
        if !ns.is_empty() {
            argv.push("-n".into());
            argv.push(ns);
        }
        match flux {
            Some(owner) => {
                self.confirm_label = format!(
                    "⚠ Managed by {owner} — your edit will be reverted on the next reconcile. Edit anyway?"
                );
                self.confirm_action = Some(ConfirmAction::Edit { argv });
                self.mode = Mode::Confirm;
            }
            None => {
                self.note_action("edit", edit_label);
                self.pending = Some(Suspend::Shell(argv));
            }
        }
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
        // Shell is gated by guardrails (deny / confirm) but has no default
        // confirmation, so an unguarded shell opens straight away.
        let targets = [(name.clone(), ns.clone())];
        let Some(level) = self.guard("shell", "pods", &targets, ConfirmLevel::None) else {
            return;
        };
        self.begin_guarded(
            ConfirmAction::Exec {
                ns,
                name: name.clone(),
            },
            format!("Shell into {name}?"),
            level,
            name,
        );
    }

    /// Shell into `pod`, optionally pinned to `container` (k9s-style `-c`).
    /// Shared by the plain pod-row shell (`s`) and the per-container picker.
    pub(super) fn exec_into(&mut self, ns: String, pod: String, container: Option<String>) {
        if self.deny_readonly() {
            return;
        }
        self.note_action("shell", format!("{pod} in {ns}"));
        let mut argv = self.kubectl_base();
        argv.extend(["exec".into(), "-it".into(), "-n".into(), ns, pod]);
        if let Some(c) = container {
            argv.push("-c".into());
            argv.push(c);
        }
        argv.extend(["--".into(), "sh".into(), "-c".into(), SHELL_FALLBACK.into()]);
        self.pending = Some(Suspend::Shell(argv));
    }

    /// `:debug` — attach an ephemeral debug container to the selected pod via
    /// `kubectl debug`, prompting for the image (prefilled with the configured
    /// default). `target` pins `--target=<container>` when launched from the
    /// container picker. Gated by read-only mode and the `debug` guardrail.
    pub(super) fn request_debug(&mut self, target: Option<String>) {
        if self.deny_readonly() {
            return;
        }
        // On a node, `:debug` launches a privileged node debug pod instead of
        // an in-pod ephemeral container.
        if self.kind_plural == "nodes" {
            self.request_node_debug();
            return;
        }
        if self.kind_plural != "pods" {
            self.flash_warn("debug: select a pod (ephemeral container) or node (debug pod)");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        // A debug container is a mutation of the pod — let guardrails gate it,
        // with no default confirmation (like shell).
        let targets = [(name.clone(), ns.clone())];
        if self
            .guard("debug", "pods", &targets, ConfirmLevel::None)
            .is_none()
        {
            return;
        }
        self.prompt_label = match &target {
            Some(c) => format!("Debug image for {name} (--target {c}, ⏎ to accept):"),
            None => format!("Debug image for {name} (⏎ to accept):"),
        };
        self.prompt_input = self.debug.image.clone();
        self.prompt_kind = Some(PromptKind::Debug {
            ns,
            pod: name,
            target,
        });
        self.mode = Mode::Prompt;
    }

    /// Launch `kubectl debug` for the ephemeral container: suspends the TUI and
    /// shells out interactively, exactly like exec/attach. The ephemeral
    /// container persists on the pod (Kubernetes can't remove it) until the pod
    /// is recreated — so there's nothing for sofka to clean up afterwards.
    pub(super) fn do_debug(
        &mut self,
        ns: String,
        pod: String,
        target: Option<String>,
        image: String,
    ) {
        let tgt = target
            .as_deref()
            .map(|c| format!(" --target {c}"))
            .unwrap_or_default();
        self.note_action(format!("debug ({image}){tgt}"), format!("{pod} in {ns}"));
        let mut argv = self.kubectl_base();
        argv.extend([
            "debug".into(),
            "-it".into(),
            "-n".into(),
            ns,
            pod,
            format!("--image={image}"),
        ]);
        if let Some(c) = target {
            argv.push(format!("--target={c}"));
        }
        // No configured command = an interactive shell (bash if the image has
        // it, else sh), mirroring the pod shell; otherwise the configured argv.
        if self.debug.command.is_empty() {
            argv.extend(["--".into(), "sh".into(), "-c".into(), SHELL_FALLBACK.into()]);
        } else {
            argv.push("--".into());
            argv.extend(self.debug.command.clone());
        }
        self.pending = Some(Suspend::Shell(argv));
    }

    /// `:debug` on a node — preview and confirm the host access a node debug
    /// pod grants, then launch it. A node debugger is privileged by design
    /// (host filesystem at `/host`, host PID/network/IPC), so this always
    /// confirms, on top of any `node-debug` guardrail.
    pub(super) fn request_node_debug(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let node = obj.metadata.name.clone().unwrap_or_default();
        let targets = [(node.clone(), String::new())];
        let Some(level) = self.guard("node-debug", "nodes", &targets, ConfirmLevel::Plain) else {
            return;
        };
        let image = self.debug.node_image.clone();
        let namespace = self.debug.node_namespace.clone();
        let profile = self.debug.node_profile.clone();
        let profile_note = profile
            .as_deref()
            .map(|p| format!(", profile {p}"))
            .unwrap_or_default();
        let label = format!(
            "⚠ Node debug pod on {node} (image {image} in {namespace}{profile_note}) — \
             grants the host filesystem (/host) and host PID/network/IPC namespaces. Launch?"
        );
        self.begin_guarded(
            ConfirmAction::NodeDebug {
                node: node.clone(),
                image,
                namespace,
                profile,
            },
            label,
            level,
            node,
        );
    }

    /// Launch `kubectl debug node/<node>` and suspend into it, tracking the
    /// `(namespace, node)` so `:debug-clean` can delete the debugger pod later
    /// (kubectl leaves it running after the session).
    pub(super) fn do_node_debug(
        &mut self,
        node: String,
        image: String,
        namespace: String,
        profile: Option<String>,
    ) {
        self.note_action(format!("node-debug ({image})"), format!("node/{node}"));
        let entry = (namespace.clone(), node.clone());
        if !self.launched_node_debuggers.contains(&entry) {
            self.launched_node_debuggers.push(entry);
        }
        let mut argv = self.kubectl_base();
        argv.extend([
            "debug".into(),
            format!("node/{node}"),
            "-it".into(),
            "-n".into(),
            namespace,
            format!("--image={image}"),
        ]);
        if let Some(p) = profile {
            argv.push(format!("--profile={p}"));
        }
        argv.extend(["--".into(), "sh".into(), "-c".into(), SHELL_FALLBACK.into()]);
        self.pending = Some(Suspend::Shell(argv));
    }

    /// `:debug-clean` — delete the node debugger pods sofka launched this
    /// session (after a confirm). No-op with a flash when none were launched.
    pub(super) fn request_debug_cleanup(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if self.launched_node_debuggers.is_empty() {
            self.flash_warn("no node debuggers launched this session");
            return;
        }
        let nodes: Vec<&str> = self
            .launched_node_debuggers
            .iter()
            .map(|(_, node)| node.as_str())
            .collect();
        self.confirm_label = format!(
            "Delete node debugger pod(s) on {} launched this session?",
            nodes.join(", ")
        );
        self.confirm_action = Some(ConfirmAction::CleanupDebuggers);
        self.mode = Mode::Confirm;
    }

    /// Delete every `node-debugger-*` pod scheduled on a tracked node, in its
    /// tracked namespace — the pods `kubectl debug node` creates. Matching on
    /// the name prefix *and* `spec.nodeName` avoids touching unrelated pods.
    pub(super) fn do_cleanup_debuggers(&mut self) {
        let targets = std::mem::take(&mut self.launched_node_debuggers);
        self.flash = format!("cleaning up {} node debugger(s)…", targets.len());
        self.flash_err = false;
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let mut deleted = 0usize;
            let mut failed: Vec<String> = Vec::new();
            for (ns, node) in targets {
                let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
                let listed = pods
                    .list(&ListParams::default().fields(&format!("spec.nodeName={node}")))
                    .await;
                let list = match listed {
                    Ok(l) => l,
                    Err(e) => {
                        failed.push(format!("{ns} (node {node}): list failed: {e}"));
                        continue;
                    }
                };
                for pod in list.items {
                    let Some(name) = pod.metadata.name.as_deref() else {
                        continue;
                    };
                    if !name.starts_with("node-debugger-") {
                        continue;
                    }
                    match pods.delete(name, &DeleteParams::default()).await {
                        Ok(_) => deleted += 1,
                        Err(e) => failed.push(format!("{ns}/{name}: {e}")),
                    }
                }
            }
            let _ = tx
                .send(Msg::DebuggersCleaned {
                    generation: genr,
                    deleted,
                    failed,
                })
                .await;
        });
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
        self.namespace_favorites = resolved.config.favorite_namespaces;
        self.plugins = resolved.config.plugins;
        self.bookmarks = resolved.config.bookmarks;
        self.workspaces = resolved.config.workspaces;
        self.guardrails = resolved.config.guardrails;
        self.debug = resolved.config.debug;
        self.bundle_cfg = resolved.config.bundle;
        self.logs_cfg = resolved.config.logs;
        self.fleet_cfg = resolved.config.fleet;
        warnings.extend(crate::config::plugin_warnings(&self.plugins));
        warnings.extend(crate::config::bookmark_warnings(&self.bookmarks));
        warnings.extend(crate::config::workspace_warnings(&self.workspaces));
        warnings.extend(crate::config::guardrail_warnings(&self.guardrails));
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
        let (metrics_provider, metrics_warnings) =
            crate::providers::compile_metrics(resolved.config.providers.metrics.as_ref());
        self.metrics_provider = metrics_provider;
        warnings.extend(metrics_warnings);
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
        self.note_action(format!("scale to {replicas}"), format!("{name} in {ns}"));
        self.flash = format!("scaling {name} → {replicas}");
        self.flash_err = false;
        self.spawn_patch_action(
            kind,
            vec![(name, ns)],
            Patch::Merge(scale_patch(replicas)),
            |_, e| format!("scale failed: {e}"),
        );
    }

    /// `t` on a pod row: open the file-transfer menu for the selected pod.
    /// No container pin — `kubectl cp` targets the pod's default container;
    /// the container picker's `t` transfers to/from a specific one.
    pub(super) fn request_transfer(&mut self) {
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.open_transfer_menu(ns, name, None);
    }

    /// Open the download/upload choice for `pod`. A menu rather than a prompt
    /// straight away, so the direction is always an explicit, visible choice.
    pub(super) fn open_transfer_menu(
        &mut self,
        ns: String,
        pod: String,
        container: Option<String>,
    ) {
        self.transfer_target = Some((ns, pod, container));
        self.transfer_menu_state.select(Some(0));
        self.mode = Mode::TransferMenu;
    }

    pub(super) fn key_transfer_menu(&mut self, key: KeyEvent) {
        let len = TRANSFER_MENU_ITEMS.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.transfer_target = None;
                self.mode = Mode::Table;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                list_step(&mut self.transfer_menu_state, len, true)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                list_step(&mut self.transfer_menu_state, len, false)
            }
            KeyCode::Enter => {
                let choice = self
                    .transfer_menu_state
                    .selected()
                    .and_then(|i| TRANSFER_MENU_ITEMS.get(i))
                    .copied();
                self.mode = Mode::Table;
                let Some((ns, pod, container)) = self.transfer_target.take() else {
                    return;
                };
                match choice {
                    Some("Download from pod") => self.prompt_transfer(ns, pod, container, false),
                    // Upload writes into the container's filesystem — a
                    // mutation, unlike download.
                    Some("Upload to pod") if !self.deny_readonly() => {
                        self.prompt_transfer(ns, pod, container, true);
                    }
                    _ => {} // "Cancel" or nothing selected — do nothing.
                }
            }
            _ => {}
        }
    }

    /// First of the two transfer prompts: the source path (remote for a
    /// download, local for an upload). `key_prompt` chains into the
    /// destination prompt with the answer.
    pub(super) fn prompt_transfer(
        &mut self,
        ns: String,
        pod: String,
        container: Option<String>,
        upload: bool,
    ) {
        let target = match &container {
            Some(c) => format!("{pod}:{c}"),
            None => pod.clone(),
        };
        self.prompt_label = if upload {
            format!("Upload to {target} — local file:")
        } else {
            format!("Download from {target} — remote path:")
        };
        self.prompt_input.clear();
        self.prompt_kind = Some(PromptKind::Transfer {
            ns,
            pod,
            container,
            upload,
            src: None,
        });
        self.mode = Mode::Prompt;
    }

    /// Run the fully-specified transfer. Downloads start straight away; an
    /// upload writes into the pod, so it passes through the `transfer`
    /// guardrail first (no default confirmation, like shell).
    pub(super) fn do_transfer(
        &mut self,
        ns: String,
        pod: String,
        container: Option<String>,
        upload: bool,
        src: String,
        dest: String,
    ) {
        if !upload {
            self.start_transfer(ns, pod, container, false, src, dest);
            return;
        }
        let targets = [(pod.clone(), ns.clone())];
        let Some(level) = self.guard("transfer", "pods", &targets, ConfirmLevel::None) else {
            return;
        };
        let label = format!("Upload {src} to {pod}:{dest}?");
        let hint = pod.clone();
        self.begin_guarded(
            ConfirmAction::Transfer {
                ns,
                pod,
                container,
                src,
                dest,
            },
            label,
            level,
            hint,
        );
    }

    /// The `kubectl cp` argv for a transfer, pinned to the active context.
    pub(super) fn cp_argv(
        &self,
        ns: &str,
        pod: &str,
        container: Option<&str>,
        upload: bool,
        src: &str,
        dest: &str,
    ) -> Vec<String> {
        let (from, to) = if upload {
            (src.to_string(), format!("{pod}:{dest}"))
        } else {
            (format!("{pod}:{src}"), dest.to_string())
        };
        let mut argv = self.kubectl_base();
        argv.extend(["cp".into(), "-n".into(), ns.to_string()]);
        if let Some(c) = container {
            argv.extend(["-c".into(), c.to_string()]);
        }
        argv.push(from);
        argv.push(to);
        argv
    }

    /// Run `kubectl cp` off-thread and flash the outcome. Not a foreground
    /// `Suspend::Shell` — cp is non-interactive (and silent on success), and
    /// a large copy would otherwise freeze the UI for its whole duration.
    pub(super) fn start_transfer(
        &mut self,
        ns: String,
        pod: String,
        container: Option<String>,
        upload: bool,
        src: String,
        dest: String,
    ) {
        let argv = self.cp_argv(&ns, &pod, container.as_deref(), upload, &src, &dest);
        let from = argv[argv.len() - 2].clone();
        let to = argv[argv.len() - 1].clone();
        self.note_action(
            if upload { "cp upload" } else { "cp download" },
            format!("{pod} in {ns}"),
        );
        self.flash = format!("copying {from} → {to}…");
        self.flash_err = false;
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let out = tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .await;
            let result = match out {
                Ok(o) if o.status.success() => Ok(format!("copied {from} → {to}")),
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    // The actual cause (a tar/exec error) comes before
                    // kubectl's generic "command terminated with exit code"
                    // trailer — flash the last line that says something.
                    let line = |skip_trailer: bool| {
                        stderr
                            .lines()
                            .rev()
                            .map(str::trim)
                            .find(|l| {
                                !l.is_empty()
                                    && !(skip_trailer && l.starts_with("command terminated"))
                            })
                            .unwrap_or_default()
                            .to_string()
                    };
                    let err = match line(true) {
                        e if e.is_empty() => line(false),
                        e => e,
                    };
                    Err(if err.is_empty() {
                        format!("kubectl cp exited with {}", o.status)
                    } else {
                        err
                    })
                }
                Err(e) => Err(format!("kubectl cp failed to start: {e}")),
            };
            let _ = tx
                .send(Msg::TransferDone {
                    generation: genr,
                    result,
                })
                .await;
        });
    }

    /// Open the `t` action menu (Flux suspend/resume, CronJob
    /// trigger/suspend/resume) for the marked rows, or the current selection
    /// if none are marked. A menu, not a single-key toggle — suspending
    /// something always takes an explicit, visible choice (`j`/`k` + Enter)
    /// rather than one accidental keystroke.
    pub(super) fn request_flux_menu(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if !self.flux_suspendable() && !self.cronjob_kind() {
            self.flash_warn("suspend/resume only applies to CronJobs and Flux resources (ks/hr/git-, helm-, oci-repos, buckets, image automation, alerts, receivers)");
            return;
        }
        if self.action_targets().is_empty() {
            return;
        }
        self.flux_menu_state.select(Some(0));
        self.mode = Mode::FluxMenu;
    }

    pub(super) fn key_flux_menu(&mut self, key: KeyEvent) {
        let items = self.action_menu_items();
        let len = items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.flux_menu_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.flux_menu_state, len, false),
            KeyCode::Enter => {
                let choice = self
                    .flux_menu_state
                    .selected()
                    .and_then(|i| items.get(i))
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
                    Some("Trigger now") => self.do_trigger_cronjobs(),
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
        let label = self.action_label(&targets);
        self.note_action(if suspend { "suspend" } else { "resume" }, label);
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
        let label = self.action_label(&targets);
        self.note_action("reconcile", label);
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

    /// Run the marked CronJobs (or the current selection) immediately by
    /// creating a Job from each one's jobTemplate — what `kubectl create job
    /// --from=cronjob/…` does, manual-instantiate annotation and owner
    /// reference included. Works on suspended CronJobs too (the suspend flag
    /// only gates the schedule, not manually created Jobs).
    pub(super) fn do_trigger_cronjobs(&mut self) {
        let objs = self.action_target_objects();
        // Seconds-resolution suffix: unique enough for a manual action, and a
        // repeat trigger of the same CronJob within a second fails loudly
        // with AlreadyExists rather than silently double-running.
        let suffix = format!("{:x}", k8s_openapi::jiff::Timestamp::now().as_second());
        let jobs: Vec<(String, String, Value)> = objs
            .iter()
            .filter_map(|o| {
                let name = o.metadata.name.clone()?;
                let ns = o.metadata.namespace.clone().unwrap_or_default();
                let job = cronjob_manual_job(o, &suffix)?;
                Some((name, ns, job))
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let targets: Vec<(String, String)> = jobs
            .iter()
            .map(|(name, ns, _)| (name.clone(), ns.clone()))
            .collect();
        let label = self.action_label(&targets);
        self.note_action("trigger", label);
        self.flash = if jobs.len() == 1 {
            format!("triggering {}…", jobs[0].0)
        } else {
            format!("triggering {} {}…", jobs.len(), self.kind_plural)
        };
        self.flash_err = false;
        self.marked.clear();
        let job_ar = ApiResource {
            group: "batch".into(),
            version: "v1".into(),
            api_version: "batch/v1".into(),
            kind: "Job".into(),
            plural: "jobs".into(),
        };
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            for (name, ns, job) in jobs {
                let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &ns, &job_ar);
                let job: DynamicObject = match serde_json::from_value(job) {
                    Ok(j) => j,
                    Err(e) => {
                        let _ = tx
                            .send(Msg::Error {
                                generation: genr,
                                error: format!("trigger {name} failed: {e}"),
                            })
                            .await;
                        continue;
                    }
                };
                if let Err(e) = api.create(&PostParams::default(), &job).await {
                    let _ = tx
                        .send(Msg::Error {
                            generation: genr,
                            error: format!("trigger {name} failed: {e}"),
                        })
                        .await;
                }
            }
        });
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
        let label = self.action_label(&targets);
        self.note_action("refresh", label);
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
        self.note_action(
            format!("helm rollback to {revision}"),
            format!("{name} in {ns}"),
        );
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
        let label = match targets.as_slice() {
            [(name, ns)] => format!("{name} in {ns}"),
            many => format!("{} Helm releases", many.len()),
        };
        self.note_action("helm uninstall", label);
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
