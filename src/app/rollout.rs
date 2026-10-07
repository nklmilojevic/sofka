use super::*;

use crate::rollout::{self, Workload};

impl App {
    /// Open the rollout history of the selected Deployment, StatefulSet, or
    /// DaemonSet (`kubectl rollout history`): one row per ReplicaSet or
    /// ControllerRevision it owns, newest first.
    pub(super) fn open_rollout_history(&mut self) {
        let Some(workload) = Workload::from_plural(&self.kind_plural) else {
            self.flash_warn("rollout history applies to deployments, statefulsets, and daemonsets");
            return;
        };
        let Some(obj) = self.selected() else {
            self.flash_warn("no selection for rollout history");
            return;
        };
        let Some(backing) = self.cluster.resolve(workload.backing_plural()) else {
            self.flash_warn(&format!("{} kind unavailable", workload.backing_plural()));
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.push_frame();
        self.kind = Some(backing);
        self.kind_plural = rollout::VIEW.into();
        self.namespace = ns;
        self.labels = label_selector(&obj, "matchLabels");
        self.fields = None;
        self.owner = Some(OwnerScope {
            kind: workload.kind().into(),
            name: name.clone(),
            uid: obj.metadata.uid.clone(),
        });
        self.rollout_managed = gitops_manager(&obj);
        self.scope_label = Some(format!("{}/{name}", workload.short()));
        // A workload filter (`-f metadata.name=web`, a workload-only label)
        // would hide revisions, or make an older one look current.
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ {name} rollout history");
        self.flash_err = false;
        self.start_watch();
        self.sort_origin = SortOrigin::Selected {
            header: "REVISION".into(),
            desc: true,
        };
        self.sort_column = self.display_headers().iter().position(|h| h == "REVISION");
        self.sort_desc = self.sort_column.is_some();
        self.invalidate_rows();
    }

    /// Whether `o` belongs in the table under the current owner scope. Rollout
    /// history only takes revisions whose owner reference carries the
    /// workload's UID: an orphaned ReplicaSet left by an earlier workload of
    /// the same name is not part of this one's history.
    pub(super) fn in_owner_scope(&self, o: &DynamicObject) -> bool {
        match &self.owner {
            None => true,
            Some(owner) if self.kind_plural == rollout::VIEW => owner.owns_strictly(o),
            Some(owner) => owner.owns(o),
        }
    }

    /// The highest revision of the workload whose history is open. Computed
    /// once per store change: every row's STATUS asks for it.
    pub(super) fn rollout_current(&self) -> Option<i64> {
        let key = (self.generation, self.store.version());
        if let Some((generation, version, current)) = self.rollout_current_cache.get()
            && (generation, version) == key
        {
            return current;
        }
        let current = rollout::current(
            self.store
                .iter()
                .map(|(_, o)| o)
                .filter(|o| self.in_owner_scope(o)),
        );
        self.rollout_current_cache
            .set(Some((key.0, key.1, current)));
        current
    }

    /// Refuse a direct change to a revision object. Deleting or editing a
    /// ReplicaSet or ControllerRevision from the history view would destroy
    /// or rewrite history, and guardrails written for those kinds would not
    /// match the history view's plural.
    pub(super) fn deny_revision_mutation(&mut self) -> bool {
        if self.kind_plural != rollout::VIEW {
            return false;
        }
        self.flash_warn("revisions belong to their workload; press r to roll back to one");
        true
    }

    /// Enter on a revision: what rolling back to it would change, as a diff of
    /// the live workload's pod template against this revision's. The live
    /// template is read rather than taken from the newest revision, which
    /// lags behind a paused Deployment's edits or a rollout just started.
    pub(super) fn open_rollout_diff(&mut self, obj: &DynamicObject) {
        let Some(owner) = self.owner.clone() else {
            return;
        };
        let Some(kind) = Workload::from_kind(&owner.kind)
            .and_then(|workload| self.cluster.resolve(workload.plural()))
        else {
            return;
        };
        let Some(target) = rollout::template(obj) else {
            self.flash_warn("this revision has no pod template");
            return;
        };
        self.set_return_mode();
        let revision = rollout::revision(obj).unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let name = owner.name;
        let claim = self.claim_status(format!("reading {name}…"));
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.rollout_preview = self.rollout_preview.wrapping_add(1);
        let request = self.rollout_preview;
        tokio::spawn(async move {
            let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &kind.ar);
            let yaml = |v: &Value| serde_yaml::to_string(v).unwrap_or_default();
            let title = format!("{name} — rollback preview (live → revision {revision})");
            let msg = match api.get(&name).await {
                Ok(live) => {
                    let live = live
                        .data
                        .pointer("/spec/template")
                        .map(yaml)
                        .unwrap_or_default();
                    let lines = details::diff_lines(&live, &yaml(&target));
                    let warn = lines
                        .iter()
                        .all(|l| l.starts_with(' '))
                        .then(|| format!("revision {revision} matches the live pod template"));
                    Msg::Diff {
                        generation: genr,
                        request,
                        claim,
                        title,
                        lines,
                        warn,
                    }
                }
                Err(e) => Msg::Flash {
                    generation: genr,
                    claim,
                    message: format!("reading {name} failed: {e}"),
                    err: true,
                },
            };
            let _ = tx.send(msg).await;
        });
    }

    /// `r` on a revision: roll the workload back to it after confirmation
    /// (`kubectl rollout undo --to-revision`). Never bulk, like Helm rollback.
    pub(super) fn request_rollout_undo(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let Some(owner) = self.owner.clone() else {
            return;
        };
        let Some(workload) = Workload::from_kind(&owner.kind) else {
            return;
        };
        let Some(rev) = self.selected() else {
            return;
        };
        let Some(revision) = rollout::revision(&rev) else {
            self.flash_warn("could not determine this revision's number");
            return;
        };
        if self.rollout_current() == Some(revision) {
            self.flash_warn(&format!(
                "revision {revision} is already the current revision"
            ));
            return;
        }
        let Some(kind) = self.cluster.resolve(workload.plural()) else {
            self.flash_warn(&format!("{} kind unavailable", workload.plural()));
            return;
        };
        let name = owner.name;
        let ns = rev.metadata.namespace.clone().unwrap_or_default();
        let targets = vec![(name.clone(), ns.clone())];
        let Some(level) = self.guard("rollback", workload.plural(), &targets, ConfirmLevel::Plain)
        else {
            return;
        };
        let warning = self.rollout_managed.as_ref().map(|manager| {
            format!("⚠ Managed by {manager} — the rollback will be reverted on the next sync.")
        });
        let question = format!(
            "Roll back {}/{name} in {ns} to revision {revision}?",
            workload.short()
        );
        let label = match &warning {
            Some(warning) => format!("{warning} {question}"),
            None => question,
        };
        self.begin_guarded(
            ConfirmAction::RolloutUndo {
                kind,
                workload,
                name: name.clone(),
                uid: owner.uid,
                revision,
                rev: Box::new(rev),
            },
            label,
            level,
            name,
        );
        // A typed guardrail confirmation shows its own prompt, not the label.
        if self.mode == Mode::Prompt
            && let Some(warning) = warning
        {
            self.prompt_label = format!("{warning} {}", self.prompt_label);
        }
    }

    /// Patch the workload with the revision's template. The workload is read
    /// first, so a paused Deployment, a template that already matches, or a
    /// workload recreated since its history was opened is reported instead of
    /// patched. The patch carries the read's resourceVersion, so a change in
    /// between fails as a conflict.
    pub(super) fn do_rollout_undo(
        &mut self,
        kind: Kind,
        workload: Workload,
        name: String,
        uid: Option<String>,
        revision: i64,
        rev: DynamicObject,
    ) {
        let ns = rev.metadata.namespace.clone().unwrap_or_default();
        let Some(mut patch) = rollout::undo_patch(workload, &rev) else {
            self.flash_warn(&format!("revision {revision} has no pod template"));
            return;
        };
        self.note_action(
            format!("rollout undo to {revision}"),
            format!("{name} in {ns}"),
        );
        let claim = self.claim_status(format!("rolling back {name} to revision {revision}…"));
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &kind.ar);
            let result = match api.get(&name).await {
                Ok(live) => {
                    let blocker = if uid.is_some() && live.metadata.uid != uid {
                        Some(format!("{name} was recreated since its history was opened"))
                    } else {
                        rollout::undo_blocker(&live, &rev)
                    };
                    match blocker {
                        Some(reason) => Err(reason),
                        None => {
                            if let Some(rv) = live.metadata.resource_version {
                                patch["metadata"]["resourceVersion"] = Value::String(rv);
                            }
                            api.patch(&name, &PatchParams::default(), &Patch::Strategic(patch))
                                .await
                                .map(|_| ())
                                .map_err(|e| e.to_string())
                        }
                    }
                }
                Err(e) => Err(e.to_string()),
            };
            let (message, err) = match result {
                Ok(()) => (format!("rolled back {name} to revision {revision}"), false),
                Err(e) => (format!("rollback of {name} failed: {e}"), true),
            };
            let _ = tx
                .send(Msg::Flash {
                    generation: genr,
                    claim,
                    message,
                    err,
                })
                .await;
        });
    }
}

/// Who reverts a manual change to `obj`: its Flux owner or Argo CD Application.
/// Argo's `app.kubernetes.io/instance` label is skipped: every Helm chart sets
/// it, so it alone does not mean Argo CD manages the workload.
fn gitops_manager(obj: &DynamicObject) -> Option<String> {
    if let Some(flux) = flux_managed_by(obj) {
        return Some(flux);
    }
    let app = crate::argocd::owner_ref(obj)
        .filter(|r| r.exact)
        .map(|r| r.name)
        .or_else(|| {
            obj.metadata
                .labels
                .as_ref()?
                .get("argocd.argoproj.io/instance")
                .filter(|v| !v.is_empty())
                .cloned()
        })?;
    Some(format!("Argo CD Application {app}"))
}
