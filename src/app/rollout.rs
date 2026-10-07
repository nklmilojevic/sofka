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
        self.retain_filter_selectors();
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

    /// The revision in effect for the workload whose history is open.
    pub(super) fn rollout_current(&self) -> Option<i64> {
        rollout::current(
            self.store
                .iter()
                .map(|(_, o)| o)
                .filter(|o| self.owner.as_ref().is_none_or(|w| w.owns(o))),
        )
    }

    /// Enter on a revision: what rolling back to it would change, as a diff of
    /// the current revision's pod template against this one's.
    pub(super) fn open_rollout_diff(&mut self, obj: &DynamicObject) {
        self.set_return_mode();
        let Some(target) = rollout::template(obj) else {
            self.flash_warn("this revision has no pod template");
            return;
        };
        let revision = rollout::revision(obj).unwrap_or_default();
        let current_rev = self.rollout_current();
        let current = self
            .store
            .iter()
            .map(|(_, o)| o)
            .filter(|o| self.owner.as_ref().is_none_or(|w| w.owns(o)))
            .find(|o| rollout::revision(o).is_some() && rollout::revision(o) == current_rev)
            .and_then(rollout::template);
        let yaml = |v: &Value| serde_yaml::to_string(v).unwrap_or_default();
        let target_yaml = yaml(&target);
        let current_yaml = current.as_ref().map(yaml).unwrap_or_default();
        let workload = self
            .owner
            .as_ref()
            .map_or_else(String::new, |o| o.name.clone());
        let current_label = current_rev.map_or_else(|| "?".into(), |r| r.to_string());
        let lines = details::diff_lines(&current_yaml, &target_yaml);
        if current_rev == Some(revision) {
            self.set_flash(format!("revision {revision} is the current revision"));
        } else if lines.iter().all(|l| l.starts_with(' ')) {
            self.set_flash(format!(
                "revision {revision} has the same pod template as the current one"
            ));
        }
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: format!(
                "{workload} — pod template diff (revision {current_label} → {revision})"
            ),
            lines: lines.into(),
            ..Default::default()
        };
        self.mode = Mode::Diff;
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
        let question = format!(
            "Roll back {}/{name} in {ns} to revision {revision}?",
            workload.short()
        );
        let label = match &self.rollout_managed {
            Some(manager) => format!(
                "⚠ Managed by {manager} — the rollback will be reverted on the next sync. {question}"
            ),
            None => question,
        };
        self.begin_guarded(
            ConfirmAction::RolloutUndo {
                kind,
                workload,
                name: name.clone(),
                ns,
                revision,
                rev: Box::new(rev),
            },
            label,
            level,
            name,
        );
    }

    /// Patch the workload with the revision's template. The workload is read
    /// first, so a paused Deployment or a template that already matches is
    /// reported instead of patched, as `kubectl rollout undo` does.
    pub(super) fn do_rollout_undo(
        &mut self,
        kind: Kind,
        workload: Workload,
        name: String,
        ns: String,
        revision: i64,
        rev: DynamicObject,
    ) {
        self.note_action(
            format!("rollout undo to {revision}"),
            format!("{name} in {ns}"),
        );
        let Some(patch) = rollout::undo_patch(workload, &rev) else {
            self.flash_warn(&format!("revision {revision} has no pod template"));
            return;
        };
        let claim = self.claim_status(format!("rolling back {name} to revision {revision}…"));
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &kind.ar);
            let result = match api.get(&name).await {
                Ok(live) => match rollout::undo_blocker(&live, &rev) {
                    Some(reason) => Err(reason),
                    None => api
                        .patch(&name, &PatchParams::default(), &Patch::Strategic(patch))
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                },
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
