//! The `:workloads` view: Deployments, StatefulSets, DaemonSets, CronJobs,
//! Jobs and Pods in one table.
//!
//! Each kind runs its own watch, tagged as one store partition, and every row
//! carries its kind in `types`. The view's own identity is the synthetic
//! `workloads` kind. A key that acts on a row runs with the row's real kind
//! in `kind`/`kind_plural`, so delete, describe, logs, scale and the rest
//! behave exactly as in that kind's own view, guardrails and plugins
//! included.

use super::*;

/// The synthetic plural the view is known by.
pub(crate) const VIEW: &str = "workloads";

/// Palette names that open the view.
pub(super) const NAMES: &[&str] = &["workloads", "workload", "wk"];

/// The kinds the view lists, as (group, kind, plural).
const KINDS: &[(&str, &str, &str)] = &[
    ("apps", "Deployment", "deployments"),
    ("apps", "StatefulSet", "statefulsets"),
    ("apps", "DaemonSet", "daemonsets"),
    ("batch", "CronJob", "cronjobs"),
    ("batch", "Job", "jobs"),
    ("", "Pod", "pods"),
];

/// Controllers whose pods and jobs are one `enter` away from a row the view
/// already lists, and so hidden by default.
const LISTED_OWNERS: &[&str] = &["ReplicaSet", "StatefulSet", "DaemonSet", "Job", "CronJob"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkloadsView {
    /// Show pods and jobs that a listed workload owns.
    pub all: bool,
}

/// The kind the view itself is known by. Nothing is ever requested from it:
/// every API call goes through a row's own kind.
fn identity() -> Kind {
    Kind {
        ar: ApiResource {
            group: String::new(),
            version: "v1".into(),
            api_version: "v1".into(),
            kind: "Workload".into(),
            plural: VIEW.into(),
        },
        namespaced: true,
        scalable: false,
    }
}

/// A row's API group and plural, from the kind its watch stamped on it.
pub(super) fn group_plural(obj: &DynamicObject) -> Option<(&'static str, &'static str)> {
    let kind = obj.types.as_ref()?.kind.as_str();
    KINDS
        .iter()
        .find(|(_, k, _)| *k == kind)
        .map(|(group, _, plural)| (*group, *plural))
}

fn plural_of(obj: &DynamicObject) -> Option<&'static str> {
    group_plural(obj).map(|(_, plural)| plural)
}

/// The store key of a workloads row: its plural before the usual
/// `namespace/name`, so a Deployment and a Pod of the same name never
/// collide.
pub(crate) fn key(obj: &DynamicObject) -> String {
    match plural_of(obj) {
        Some(plural) => format!("{plural}/{}", row_key(obj)),
        None => row_key(obj),
    }
}

/// A Pod or Job whose controller is a workload the view also lists.
fn listed_elsewhere(obj: &DynamicObject) -> bool {
    obj.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|r| r.controller == Some(true) && LISTED_OWNERS.contains(&r.kind.as_str()))
}

impl App {
    pub fn workloads_active(&self) -> bool {
        self.workloads.is_some()
    }

    /// The plural the store, timeline, and session diff file rows under:
    /// the view's own, even while a key runs with a row's kind.
    pub(super) fn view_plural(&self) -> &str {
        if self.workloads.is_some() {
            VIEW
        } else {
            &self.kind_plural
        }
    }

    /// The kind a watched row belongs to: the view's own, or for a
    /// workloads row the plural its key starts with.
    pub(super) fn watched_plural<'a>(&'a self, key: &'a str) -> &'a str {
        if self.workloads.is_some() {
            key.split_once('/').map_or(key, |(plural, _)| plural)
        } else {
            &self.kind_plural
        }
    }

    /// The store key of `obj` in the current view.
    pub(crate) fn key_of(&self, obj: &DynamicObject) -> String {
        if self.workloads.is_some() {
            key(obj)
        } else {
            row_key(obj)
        }
    }

    /// Open the view as a fresh root, optionally in namespace `ns`.
    pub(super) fn open_workloads(&mut self, ns: Option<&str>) {
        self.save_history_filter();
        if let Some(ns) = ns {
            self.namespace = normalize_ns(ns);
            self.note_recent_namespace(ns);
            self.remember_namespace();
        }
        self.set_workloads_root(WorkloadsView::default());
        self.flash = if ns.is_some() {
            format!("Viewing workloads in {}", self.namespace_label())
        } else {
            "Viewing workloads".into()
        };
        self.flash_err = false;
        self.record_history();
        self.start_watch();
    }

    pub(super) fn set_workloads_root(&mut self, view: WorkloadsView) {
        self.set_root_view(identity());
        self.workloads = Some(view);
    }

    /// Put the view's own identity back after a key ran with a row's kind.
    pub(super) fn restore_workloads_identity(&mut self) {
        if self.workloads.is_some() {
            self.kind = Some(identity());
            self.kind_plural = VIEW.into();
        }
    }

    /// Whether `key` acts on the selected row rather than on the view.
    /// Navigation, marks, filters, sorting and view switches keep the
    /// view's own identity; everything else runs with the row's kind.
    pub(super) fn acts_on_row(&self, key: &KeyInput) -> bool {
        if self.workloads.is_none() {
            return false;
        }
        match self.mode {
            Mode::Table => !key.action.is_some_and(|action| {
                Action::FAVORITE_NAMESPACES.contains(&action)
                    || matches!(
                        action,
                        Action::Up
                            | Action::Down
                            | Action::First
                            | Action::Last
                            | Action::PageUp
                            | Action::PageDown
                            | Action::HalfPageUp
                            | Action::HalfPageDown
                            | Action::Left
                            | Action::Right
                            | Action::RangeUp
                            | Action::RangeDown
                            | Action::Mark
                            | Action::MarkRange
                            | Action::Filter
                            | Action::Faults
                            | Action::ToggleOwned
                            | Action::Back
                            | Action::Sort
                            | Action::SortAge
                            | Action::InvertSort
                            | Action::Wide
                            | Action::Namespaces
                            | Action::AllNamespaces
                            | Action::HistoryBack
                            | Action::HistoryForward
                            | Action::NextView
                            | Action::PreviousView
                            | Action::Refresh
                            | Action::CopyCell
                            | Action::Exit
                            | Action::Command
                            | Action::Help
                    )
            }),
            // Palette commands that act on the selected row focus it
            // themselves (`run_action`, palette plugins); the rest, such as
            // `:reload`, act on the view.
            Mode::Command
            | Mode::Filter
            | Mode::Help
            | Mode::Namespaces
            | Mode::Contexts
            | Mode::SortPicker
            | Mode::CopyPicker => false,
            _ => true,
        }
    }

    /// Run the next key with the kind of the row it acts on. A key in the
    /// table or palette starts a new action on the cursor row; a key in a dialog or
    /// prompt that action opened keeps the kind it started with, even if a
    /// watch update has since moved the cursor to a row of another kind.
    pub(super) fn focus_workload_row(&mut self) {
        if self.workloads.is_none() {
            return;
        }
        // The table and the palette start a new action on the cursor row.
        if matches!(self.mode, Mode::Table | Mode::Command) {
            self.workload_skipped = 0;
            self.workload_focus = self.selected_ref().and_then(|obj| {
                let plural = plural_of(obj)?;
                let (group, kind, _) = KINDS.iter().find(|(_, _, p)| *p == plural)?;
                self.cluster.resolve_in_group(kind, group)
            });
            if let Some(kind) = &self.workload_focus {
                let prefix = format!("{}/", kind.ar.plural);
                self.workload_skipped = self
                    .marked
                    .iter()
                    .filter(|key| !key.starts_with(&prefix))
                    .count();
            }
        }
        if let Some(kind) = self.workload_focus.clone() {
            self.kind_plural = kind.ar.plural.to_lowercase();
            self.kind = Some(kind);
        }
    }

    /// Tell a confirmation or prompt raised over mixed marks which marked
    /// rows it leaves out.
    pub(super) fn note_skipped_marks(&mut self, before: Mode) {
        let skipped = std::mem::take(&mut self.workload_skipped);
        if before != Mode::Table {
            return;
        }
        let note = if skipped == 0 {
            String::new()
        } else {
            format!(
                " · skips {skipped} marked row{} of other kinds",
                if skipped == 1 { "" } else { "s" }
            )
        };
        match self.mode {
            Mode::Confirm => {
                self.confirm_label.push_str(&note);
                // Kept for the dialog's lifetime: toggling force or cascade
                // rebuilds the label.
                self.confirm_note = note;
            }
            Mode::Prompt => self.prompt_label.push_str(&note),
            _ => {}
        }
    }

    /// Whether a marked row is one the running action can act on: in the
    /// workloads view, a row of the cursor row's kind.
    pub(super) fn in_row_focus(&self, obj: &DynamicObject) -> bool {
        self.workloads.is_none()
            || group_plural(obj).is_some_and(|(_, plural)| plural == self.kind_plural)
    }

    /// Whether a row passes the view's default top-level filter.
    pub(super) fn workload_row_visible(&self, obj: &DynamicObject) -> bool {
        self.workloads
            .is_none_or(|view| view.all || !listed_elsewhere(obj))
    }

    pub(super) fn toggle_workloads_owned(&mut self) {
        let Some(view) = self.workloads.as_mut() else {
            return;
        };
        view.all = !view.all;
        let all = view.all;
        self.invalidate_rows();
        self.table_state.select(Some(0));
        self.set_flash(if all {
            "showing owned pods and jobs"
        } else {
            "showing top-level workloads"
        });
    }

    /// One watch per kind (per namespace for a namespace pattern), each a
    /// store partition so its relist replaces only its own rows.
    pub(super) fn start_workload_watches(
        &mut self,
        labels: Option<String>,
        fields: Option<String>,
    ) {
        let pattern = self.namespace_is_pattern();
        let namespaces = if pattern {
            self.watch_namespaces()
        } else {
            vec![self.namespace.clone()]
        };
        let mut partitions = Vec::new();
        for (group, kind_name, plural) in KINDS {
            let Some(kind) = self.cluster.resolve_in_group(kind_name, group) else {
                continue;
            };
            let types = TypeMeta {
                api_version: kind.ar.api_version.clone(),
                kind: kind.ar.kind.clone(),
            };
            for namespace in &namespaces {
                let partition = if pattern {
                    format!("{plural}/{namespace}")
                } else {
                    (*plural).to_string()
                };
                partitions.push(partition.clone());
                let (tx, mut rx) = tokio::sync::mpsc::channel(256);
                self.tasks.push(self.cluster.spawn_watch(
                    &kind,
                    namespace,
                    labels.clone(),
                    fields.clone(),
                    self.generation,
                    tx,
                ));
                let out = self.tx.clone();
                let generation = self.generation;
                let types = types.clone();
                self.tasks.push(tokio::spawn(async move {
                    while let Some(event) = rx.recv().await {
                        let event = match event {
                            Msg::Applied {
                                generation,
                                key,
                                mut obj,
                            } => {
                                obj.types = Some(types.clone());
                                Msg::Applied {
                                    generation,
                                    key: format!("{plural}/{key}"),
                                    obj,
                                }
                            }
                            Msg::Deleted { generation, key } => Msg::Deleted {
                                generation,
                                key: format!("{plural}/{key}"),
                            },
                            event => event,
                        };
                        let msg = Msg::NamespaceWatch {
                            generation,
                            namespace: partition.clone(),
                            event: Box::new(event),
                        };
                        if out.send(msg).await.is_err() {
                            break;
                        }
                    }
                }));
            }
        }
        self.store.set_partitions(&partitions, |_, obj| {
            let plural = plural_of(obj)?;
            Some(if pattern {
                format!("{plural}/{}", obj.metadata.namespace.as_deref()?)
            } else {
                plural.to_string()
            })
        });
    }
}
