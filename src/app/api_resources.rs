//! The `:api-resources` (alias `:aliases`) view: a searchable table of all
//! discovered listable Kubernetes API resources (core, aggregated APIs, and
//! CRDs) with their working aliases, API version, scope, and kind.
//!
//! Populated directly from the cluster's cached API discovery with zero extra
//! requests on open; `r` (or `ctrl-r`) re-runs API discovery in the background
//! so newly installed CRDs appear without restarting. `⏎` opens the selected
//! resource's view (using its exact group/version/plural) and `esc` returns.

use super::*;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use tokio::time::timeout;

/// The synthetic plural the view is known by.
pub(crate) const VIEW: &str = "api-resources";

/// Palette names that open the view.
pub(super) const NAMES: &[&str] = &[VIEW, "aliases"];

const DISCOVERY_TIME: Duration = Duration::from_secs(30);

/// Keys that keep working on the view's rows. Everything else would act on
/// a synthetic row as if it were an object.
const ALLOWED: &[Action] = &[
    Action::Up,
    Action::Down,
    Action::First,
    Action::Last,
    Action::PageUp,
    Action::PageDown,
    Action::HalfPageUp,
    Action::HalfPageDown,
    Action::Left,
    Action::Right,
    Action::Filter,
    Action::Back,
    Action::Exit,
    Action::Open,
    Action::Sort,
    Action::InvertSort,
    Action::Wide,
    Action::Namespaces,
    Action::Contexts,
    Action::AllNamespaces,
    Action::ApiResources,
    Action::HistoryBack,
    Action::HistoryForward,
    Action::NextView,
    Action::PreviousView,
    Action::Refresh,
    Action::RestartOrRefresh,
    Action::CopyName,
    Action::CopyCell,
    Action::Command,
    Action::Help,
];

/// The kind the view itself is known by. Nothing is ever requested from it.
fn identity() -> Kind {
    Kind {
        ar: ApiResource {
            group: String::new(),
            version: "v1".into(),
            api_version: "v1".into(),
            kind: "ApiResource".into(),
            plural: VIEW.into(),
        },
        namespaced: false,
        scalable: false,
    }
}

/// One synthetic table row for a discovered API resource.
pub fn row(resource: &crate::k8s::DiscoveredResource, aliases: &[String]) -> DynamicObject {
    let kind = &resource.kind;
    let scope = if kind.namespaced {
        "Namespaced"
    } else {
        "Cluster"
    };
    DynamicObject {
        types: None,
        metadata: ObjectMeta {
            name: Some(kind.title()),
            ..Default::default()
        },
        data: json!({
            "plural": kind.ar.plural,
            "aliases": aliases.join(","),
            "apiVersion": kind.ar.api_version,
            "group": kind.ar.group,
            "version": kind.ar.version,
            "scope": scope,
            "namespaced": kind.namespaced,
            "kind": kind.ar.kind,
            "scalable": kind.scalable,
        }),
    }
}

fn summary_message(count: usize, warning: Option<String>) -> (String, bool) {
    let mut message = format!("{count} API resource{}", if count == 1 { "" } else { "s" });
    let has_warning = warning.is_some();
    if let Some(warning) = warning {
        message.push_str(&format!(" · {warning}"));
    }
    (message, has_warning)
}

impl App {
    /// Whether the synthetic resource browser view is open, not a custom
    /// resource that happens to share its plural.
    pub fn api_resources_active(&self) -> bool {
        self.kind_plural == VIEW
            && self
                .kind
                .as_ref()
                .is_some_and(|k| k.ar.group.is_empty() && k.ar.kind == identity().ar.kind)
    }

    /// Open the resource browser as a fresh root view.
    pub(super) fn open_api_resources(&mut self) {
        if self.api_resources_active() {
            return;
        }
        self.save_history_filter();
        self.set_root_view(identity());
        self.record_history();
        self.start_watch();
    }

    /// Replay the view from history.
    pub(super) fn set_api_resources_root(&mut self) {
        self.set_root_view(identity());
    }

    /// Fill the store directly from the cluster's cached API discovery.
    /// Returns the number of listable resources populated.
    pub(super) fn populate_api_resources_store(&mut self) -> usize {
        if self.store.begin_reset() {
            self.clear_rows_cache();
        }
        let rows: Vec<DynamicObject> = self
            .cluster
            .discovered_resources()
            .iter()
            .filter(|r| r.listable)
            .map(|r| {
                let aliases = self.cluster.aliases_for(&r.kind);
                row(r, &aliases)
            })
            .collect();
        let count = rows.len();
        for obj in rows {
            let key = row_key(&obj);
            self.apply_watched(None, key, obj);
        }
        if self.store.finish_sync() {
            self.clear_rows_cache();
        }
        count
    }

    /// Populate `:api-resources` from cached discovery and set the status line.
    pub(super) fn start_api_resources(&mut self) {
        let count = self.populate_api_resources_store();
        let (message, warn) = summary_message(count, self.discovery_warning());
        if warn {
            self.flash_warn(&message);
        } else {
            self.set_flash(message);
        }
    }

    /// Re-run API discovery in the background (`r` / `ctrl-r`), keeping the
    /// current rows visible until the fresh discovery lands.
    pub(super) fn refresh_api_resources(&mut self) {
        if !self.cluster.connected {
            self.flash_warn("not connected to a cluster");
            return;
        }
        self.store.synced = false;
        let claim = self.claim_status("refreshing API resources…");
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let epoch = self.context_epoch;
        let generation = self.generation;
        self.tasks.push(tokio::spawn(async move {
            let result = match timeout(DISCOVERY_TIME, crate::k8s::rediscover(&client)).await {
                Ok(Ok(discovery)) => Ok(Box::new(discovery)),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(_) => Err("API discovery timed out".into()),
            };
            let _ = tx
                .send(Msg::ApiResourcesRefreshed {
                    epoch,
                    generation,
                    claim,
                    result,
                })
                .await;
        }));
    }

    pub(super) fn receive_api_resources_refresh(
        &mut self,
        generation: u64,
        claim: StatusClaim,
        result: Result<Box<crate::k8s::Discovery>, String>,
    ) {
        match result {
            Ok(discovery) => {
                self.cluster.apply_discovery(*discovery);
                self.cluster.add_aliases(&self.user_aliases);
                self.rbac_allowed = None;
                self.refresh_rbac();
                if self.mode == Mode::Command {
                    self.update_suggestions();
                }
                if generation == self.generation && self.api_resources_active() {
                    let count = self.populate_api_resources_store();
                    let (message, warn) = summary_message(count, self.discovery_warning());
                    self.set_claimed_status(claim, message, warn);
                }
            }
            Err(error) => {
                if generation == self.generation && self.api_resources_active() {
                    self.store.synced = true;
                    self.set_claimed_status(claim, format!("API discovery failed: {error}"), true);
                }
            }
        }
    }

    /// Keys that would treat a row as an object say what the view does
    /// instead. Bookmarks and workspaces still get theirs.
    pub(super) fn api_resources_blocks(&mut self, key: &KeyInput) -> bool {
        if !self.api_resources_active() || key.action.is_none_or(|a| ALLOWED.contains(&a)) {
            return false;
        }
        if !self.try_bookmark_key(key.event()) && !self.try_workspace_key(key.event()) {
            self.flash_warn("api-resources lists kinds: ⏎ opens one, r refreshes discovery");
        }
        true
    }

    /// `⏎` on a row: open that resource's own view using its exact group,
    /// version, and plural.
    pub(super) fn drill_into_api_resource(&mut self, obj: &DynamicObject) {
        let field = |name: &str| obj.data[name].as_str().unwrap_or("").to_string();
        let (group, version, kind, plural) = (
            field("group"),
            field("version"),
            field("kind"),
            field("plural"),
        );
        let namespaced = obj.data["namespaced"].as_bool().unwrap_or(true);
        let scalable = obj.data["scalable"].as_bool().unwrap_or(false);
        let target = self
            .cluster
            .discovered_resources()
            .iter()
            .find(|r| {
                r.kind.ar.group == group
                    && r.kind.ar.version == version
                    && r.kind.ar.plural == plural
            })
            .map(|r| r.kind.clone())
            .unwrap_or_else(|| Kind {
                ar: ApiResource {
                    api_version: if group.is_empty() {
                        version.clone()
                    } else {
                        format!("{group}/{version}")
                    },
                    group,
                    version,
                    kind,
                    plural: plural.clone(),
                },
                namespaced,
                scalable,
            });
        let title = target.title();
        self.push_frame();
        self.kind_plural = target.ar.plural.to_lowercase();
        let is_namespaced = target.namespaced;
        self.kind = Some(target);
        self.labels = None;
        self.fields = None;
        self.owner = None;
        self.scope_label = None;
        self.filter.clear();
        self.reset_sort();
        self.reset_layout();
        self.table_state.select(Some(0));
        if is_namespaced {
            self.set_flash(format!("↳ {title} in {}", self.namespace_label()));
        } else {
            self.set_flash(format!("↳ {title}"));
        }
        self.start_watch();
    }
}
