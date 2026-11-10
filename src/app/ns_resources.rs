//! The `:ns-resources` view: one row per namespaced kind that has objects in
//! a namespace, with how many.
//!
//! The counts are a snapshot, not a watch: a fresh API discovery, then one
//! `limit=1` list per kind read off `remainingItemCount`, a few at a time.
//! Rows land in the store as the lists answer, so filter, sort, and view
//! history work as in any table. `⏎` opens the kind's own view in the same
//! namespace and `esc` comes back, which counts again.

use super::*;
use futures_util::stream;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use tokio::time::timeout;

/// The synthetic plural the view is known by.
pub(crate) const VIEW: &str = "ns-resources";

/// Palette names that open the view.
pub(super) const NAMES: &[&str] = &[VIEW];

/// Lists in flight at once.
const CONCURRENCY: usize = 8;
/// Page size when the server leaves `remainingItemCount` out and the count
/// has to page through the list instead.
const PAGE_SIZE: u32 = 500;
/// Pages read per kind before the count is shown as a lower bound.
const MAX_PAGES: usize = 20;
const REQUEST_TIME: Duration = Duration::from_secs(10);
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

/// How many objects of one kind a namespace holds, or why that isn't known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Count {
    /// `more` when paging stopped early and `n` is a lower bound.
    Objects {
        n: u64,
        more: bool,
    },
    NoAccess,
    Failed(String),
}

#[derive(Debug)]
pub enum Update {
    /// One kind's list answered. `row` is `None` for a kind with no objects.
    Counted {
        row: Option<Box<DynamicObject>>,
        done: usize,
        total: usize,
    },
    /// Every list answered. `warning` names a discovery problem.
    Done { warning: Option<String> },
}

#[derive(Default)]
pub(super) struct State {
    /// The progress text last put on the status bar.
    shown: String,
}

/// The kind the view itself is known by. Nothing is ever requested from it.
fn identity() -> Kind {
    Kind {
        ar: ApiResource {
            group: String::new(),
            version: "v1".into(),
            api_version: "v1".into(),
            kind: "NamespaceResource".into(),
            plural: VIEW.into(),
        },
        namespaced: false,
        scalable: false,
    }
}

/// What the view says when asked to count a namespace pattern.
const PATTERN_WARNING: &str = "ns-resources counts one namespace or all namespaces, not a pattern";

/// One summary line for every API group discovery could not read: their
/// kinds are missing from the counts.
pub(crate) fn discovery_warning(skipped: Vec<String>) -> Option<String> {
    match skipped.len() {
        0 => None,
        1 => skipped.into_iter().next(),
        n => Some(format!(
            "{n} API groups not counted: {}",
            skipped.join("; ")
        )),
    }
}

/// The row for one counted kind, or `None` for a kind with no objects.
pub fn row(kind: &Kind, count: &Count) -> Option<DynamicObject> {
    let cell = match count {
        Count::Objects { n: 0, .. } => return None,
        Count::Objects { n, more: false } => n.to_string(),
        Count::Objects { n, more: true } => format!("{n}+"),
        Count::NoAccess => "no access".into(),
        Count::Failed(error) => format!("error: {error}"),
    };
    Some(DynamicObject {
        types: None,
        metadata: ObjectMeta {
            name: Some(kind.title()),
            resource_version: Some(cell.clone()),
            ..Default::default()
        },
        data: json!({
            "kind": kind.ar.kind,
            "group": kind.ar.group,
            "version": kind.ar.version,
            "plural": kind.ar.plural,
            "count": cell,
        }),
    })
}

/// Count `kind`'s objects in `namespace` ("" = all namespaces).
pub(crate) async fn count(client: Client, kind: &Kind, namespace: &str) -> Count {
    let api = kind.api(client, namespace);
    let mut n = 0u64;
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut params = ListParams::default().limit(if token.is_some() { PAGE_SIZE } else { 1 });
        if let Some(token) = &token {
            params = params.continue_token(token);
        }
        let page = match timeout(REQUEST_TIME, api.list(&params)).await {
            Ok(Ok(page)) => page,
            Ok(Err(kube::Error::Api(status))) if status.code == 403 => return Count::NoAccess,
            Ok(Err(kube::Error::Api(status))) if !status.message.is_empty() => {
                return Count::Failed(status.message.clone());
            }
            Ok(Err(e)) => return Count::Failed(e.to_string()),
            Err(_) => return Count::Failed("request timed out".into()),
        };
        n += page.items.len() as u64;
        if let Some(remaining) = page.metadata.remaining_item_count {
            return Count::Objects {
                n: n + remaining.max(0) as u64,
                more: false,
            };
        }
        match page.metadata.continue_.filter(|t| !t.is_empty()) {
            Some(next) => token = Some(next),
            None => return Count::Objects { n, more: false },
        }
    }
    Count::Objects { n, more: true }
}

/// Discover the namespaced kinds, then count each, sending a row per kind as
/// its list answers.
async fn run(
    client: Client,
    known: Vec<Kind>,
    namespace: String,
    tx: Sender<Msg>,
    generation: u64,
    claim: StatusClaim,
) {
    let (kinds, warning) =
        match timeout(DISCOVERY_TIME, crate::k8s::namespaced_kinds(&client)).await {
            Ok(Ok((kinds, skipped))) => (kinds, discovery_warning(skipped)),
            Ok(Err(e)) => (known, Some(format!("API discovery failed: {e}"))),
            Err(_) => (known, Some("API discovery timed out".into())),
        };
    let total = kinds.len();
    let mut counts = stream::iter(kinds)
        .map(|kind| {
            let client = client.clone();
            let namespace = namespace.clone();
            async move {
                let count = count(client, &kind, &namespace).await;
                (kind, count)
            }
        })
        .buffer_unordered(CONCURRENCY);
    let mut done = 0;
    while let Some((kind, count)) = counts.next().await {
        done += 1;
        let update = Update::Counted {
            row: row(&kind, &count).map(Box::new),
            done,
            total,
        };
        let msg = Msg::NsResources {
            generation,
            claim,
            update,
        };
        if tx.send(msg).await.is_err() {
            return;
        }
    }
    let _ = tx
        .send(Msg::NsResources {
            generation,
            claim,
            update: Update::Done { warning },
        })
        .await;
}

impl App {
    /// Whether the synthetic view is open, not a custom resource that
    /// happens to share its plural.
    pub fn ns_resources_active(&self) -> bool {
        self.kind_plural == VIEW
            && self
                .kind
                .as_ref()
                .is_some_and(|k| k.ar.group.is_empty() && k.ar.kind == identity().ar.kind)
    }

    /// Refuse a namespace pattern before anything about the view changes.
    pub(super) fn ns_resources_refuses(&mut self, namespace: &str) -> bool {
        if namespace_patterns::is_pattern(&normalize_ns(namespace)) {
            self.flash_warn(PATTERN_WARNING);
            return true;
        }
        false
    }

    /// Open the view as a fresh root, in namespace `ns` or the current one.
    pub(super) fn open_ns_resources(&mut self, ns: Option<&str>) {
        let namespace = ns.map_or_else(|| self.namespace.clone(), str::to_string);
        if self.ns_resources_refuses(&namespace) {
            return;
        }
        self.save_history_filter();
        if let Some(ns) = ns {
            self.namespace = normalize_ns(ns);
            self.note_recent_namespace(ns);
            self.remember_namespace();
        }
        self.set_root_view(identity());
        self.set_flash(format!("Viewing resources in {}", self.namespace_label()));
        self.record_history();
        self.start_watch();
    }

    /// `R` on a namespace row: the view for that namespace, with `esc`
    /// back to the namespace list.
    pub(super) fn open_ns_resources_for_row(&mut self) {
        if self.kind_plural != "namespaces" {
            self.flash_warn("R lists resources from a namespace row; elsewhere use :ns-resources");
            return;
        }
        let Some(name) = self.selected_ref().and_then(|o| o.metadata.name.clone()) else {
            self.flash_warn("no namespace selected");
            return;
        };
        self.push_frame();
        self.kind = Some(identity());
        self.kind_plural = VIEW.into();
        self.labels = None;
        self.fields = None;
        self.owner = None;
        self.scope_label = None;
        self.filter.clear();
        self.reset_sort();
        self.set_namespace(name);
    }

    /// Replay the view from history or a bookmark-like entry.
    pub(super) fn set_ns_resources_root(&mut self) {
        self.set_root_view(identity());
    }

    /// Count again, keeping the rows on screen until the new counts are in.
    pub(super) fn start_ns_resources(&mut self) {
        if self.namespace_is_pattern() {
            self.store.finish_sync();
            self.flash_warn(PATTERN_WARNING);
            return;
        }
        if self.store.begin_reset() {
            self.clear_rows_cache();
        }
        let claim = self.claim_status(format!("counting resources in {}…", self.namespace_label()));
        self.ns_resources.shown = self.flash.clone();
        self.tasks.push(tokio::spawn(run(
            self.cluster.client.clone(),
            self.cluster.child_kinds.clone(),
            self.namespace.clone(),
            self.tx.clone(),
            self.generation,
            claim,
        )));
    }

    pub(super) fn receive_ns_resources(&mut self, claim: StatusClaim, update: Update) {
        match update {
            Update::Counted { row, done, total } => {
                if let Some(row) = row {
                    let key = row_key(&row);
                    self.apply_watched(None, key, *row);
                }
                let shown = std::mem::take(&mut self.ns_resources.shown);
                self.ns_resources.shown = self.set_claimed_progress(
                    claim,
                    &shown,
                    format!("counting resources: {done}/{total} kinds…"),
                );
            }
            Update::Done { warning } => {
                if self.store.finish_sync() {
                    self.clear_rows_cache();
                }
                let (mut kinds, mut denied, mut failed) = (0, 0, 0);
                for (_, obj) in self.store.iter() {
                    match obj.data["count"].as_str().unwrap_or("") {
                        "no access" => denied += 1,
                        c if c.starts_with("error: ") => failed += 1,
                        _ => kinds += 1,
                    }
                }
                let mut message = format!(
                    "{kinds} kind{} with objects in {}",
                    if kinds == 1 { "" } else { "s" },
                    self.namespace_label()
                );
                if denied > 0 {
                    message.push_str(&format!(" · {denied} no access"));
                }
                if failed > 0 {
                    message.push_str(&format!(" · {failed} failed"));
                }
                if let Some(warning) = &warning {
                    message.push_str(&format!(" · {warning}"));
                }
                self.set_claimed_status(claim, message, failed > 0 || warning.is_some());
            }
        }
    }

    /// Keys that would treat a row as an object say what the view does
    /// instead. Bookmarks and workspaces still get theirs.
    pub(super) fn ns_resources_blocks(&mut self, key: &KeyInput) -> bool {
        if !self.ns_resources_active() || key.action.is_none_or(|a| ALLOWED.contains(&a)) {
            return false;
        }
        if !self.try_bookmark_key(key.event()) && !self.try_workspace_key(key.event()) {
            self.flash_warn("ns-resources lists kinds: ⏎ opens one, r counts again");
        }
        true
    }

    /// `⏎` on a row: the kind's own view in the same namespace.
    pub(super) fn drill_into_ns_resource(&mut self, obj: &DynamicObject) {
        let field = |name: &str| obj.data[name].as_str().unwrap_or("").to_string();
        let (group, version, kind, plural) = (
            field("group"),
            field("version"),
            field("kind"),
            field("plural"),
        );
        let target = self
            .cluster
            .resolve(&obj.metadata.name.clone().unwrap_or_default())
            .filter(|k| k.ar.group == group && k.ar.plural == plural && k.ar.version == version)
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
                namespaced: true,
                scalable: false,
            });
        self.push_frame();
        self.kind_plural = target.ar.plural.to_lowercase();
        self.kind = Some(target);
        self.labels = None;
        self.fields = None;
        self.owner = None;
        self.scope_label = None;
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.set_flash(format!("↳ {plural} in {}", self.namespace_label()));
        self.start_watch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn kind() -> Kind {
        Kind {
            ar: ApiResource {
                group: "apps".into(),
                version: "v1".into(),
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                plural: "deployments".into(),
            },
            namespaced: true,
            scalable: true,
        }
    }

    /// A client whose server answers each request with the next response,
    /// recording the query strings it was sent.
    fn mock(responses: Vec<(u16, Value)>) -> (Client, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queries = seen.clone();
        let responses = Arc::new(Mutex::new(responses.into_iter()));
        let client = Client::new(
            tower::service_fn(move |req: http::Request<kube::client::Body>| {
                assert!(req.uri().path().starts_with("/apis/apps/v1/"));
                queries.lock().unwrap().push(format!(
                    "{}?{}",
                    req.uri().path(),
                    req.uri().query().unwrap_or("")
                ));
                let (code, body) = responses
                    .lock()
                    .unwrap()
                    .next()
                    .expect("unexpected request");
                async move {
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .status(code)
                            .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                            .unwrap(),
                    )
                }
            }),
            "default",
        );
        (client, seen)
    }

    fn page(items: usize, remaining: Option<i64>, next: &str) -> (u16, Value) {
        let items: Vec<Value> = (0..items)
            .map(|i| json!({"metadata": {"name": format!("d{i}")}}))
            .collect();
        let mut metadata = json!({"continue": next});
        if let Some(remaining) = remaining {
            metadata["remainingItemCount"] = json!(remaining);
        }
        (
            200,
            json!({"apiVersion": "apps/v1", "kind": "DeploymentList", "metadata": metadata, "items": items}),
        )
    }

    #[tokio::test]
    async fn count_reads_remaining_item_count_from_one_object() {
        let (client, seen) = mock(vec![page(1, Some(41), "next")]);
        assert_eq!(
            count(client, &kind(), "prod").await,
            Count::Objects { n: 42, more: false }
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("/apis/apps/v1/namespaces/prod/deployments?"));
        assert!(seen[0].contains("limit=1"), "{}", seen[0]);
    }

    #[tokio::test]
    async fn count_across_all_namespaces_lists_cluster_wide() {
        let (client, seen) = mock(vec![page(0, None, "")]);
        assert_eq!(
            count(client, &kind(), "").await,
            Count::Objects { n: 0, more: false }
        );
        assert!(seen.lock().unwrap()[0].starts_with("/apis/apps/v1/deployments?"));
    }

    #[tokio::test]
    async fn count_pages_when_the_server_omits_remaining_item_count() {
        let (client, seen) = mock(vec![
            page(1, None, "a"),
            page(3, None, "b"),
            page(2, None, ""),
        ]);
        assert_eq!(
            count(client, &kind(), "prod").await,
            Count::Objects { n: 6, more: false }
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[1].contains("limit=500") && seen[1].contains("continue=a"));
        assert!(seen[2].contains("continue=b"));
    }

    #[tokio::test]
    async fn count_stops_paging_at_a_lower_bound() {
        let pages = (0..MAX_PAGES).map(|_| page(1, None, "more")).collect();
        let (client, _) = mock(pages);
        assert_eq!(
            count(client, &kind(), "prod").await,
            Count::Objects {
                n: MAX_PAGES as u64,
                more: true
            }
        );
    }

    #[tokio::test]
    async fn count_tells_forbidden_from_other_errors() {
        let status = |code: u16, message: &str| {
            (
                code,
                json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                       "message": message, "code": code}),
            )
        };
        let (client, _) = mock(vec![status(403, "deployments is forbidden")]);
        assert_eq!(count(client, &kind(), "prod").await, Count::NoAccess);
        let (client, _) = mock(vec![status(503, "the server is currently unable")]);
        assert_eq!(
            count(client, &kind(), "prod").await,
            Count::Failed("the server is currently unable".into())
        );
    }

    #[test]
    fn discovery_warning_names_every_skipped_group() {
        assert_eq!(discovery_warning(Vec::new()), None);
        assert_eq!(
            discovery_warning(vec!["a unreadable".into()]).as_deref(),
            Some("a unreadable")
        );
        assert_eq!(
            discovery_warning(vec!["a unreadable".into(), "b unreadable".into()]).as_deref(),
            Some("2 API groups not counted: a unreadable; b unreadable")
        );
    }

    #[test]
    fn rows_hide_empty_kinds_and_mark_the_rest() {
        assert!(row(&kind(), &Count::Objects { n: 0, more: false }).is_none());
        let cell = |count: Count| {
            row(&kind(), &count).unwrap().data["count"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(cell(Count::Objects { n: 3, more: false }), "3");
        assert_eq!(cell(Count::Objects { n: 3, more: true }), "3+");
        assert_eq!(cell(Count::NoAccess), "no access");
        assert_eq!(cell(Count::Failed("boom".into())), "error: boom");
        let row = row(&kind(), &Count::NoAccess).unwrap();
        assert_eq!(row.metadata.name.as_deref(), Some("deployments.apps"));
    }
}
