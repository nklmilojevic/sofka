use super::*;

impl App {
    // ----- detail / describe --------------------------------------------

    /// Remember which view a transient sub-view (logs/detail/diff) was opened
    /// from, so `esc` returns there (e.g. back to the xray tree, not the table).
    pub(super) fn set_return_mode(&mut self) {
        self.stop_plugins();
        // A transient sub-view (logs/detail/events) opened from a list-style
        // view returns to that view, not the table underneath it.
        self.return_mode = match self.mode {
            Mode::Xray => Mode::Xray,
            Mode::Explain => Mode::Explain,
            _ => Mode::Table,
        };
        // Remember the selected row so we can land back on it.
        self.return_selection = self.selected_ref().map(row_key);
    }

    /// Re-select the row remembered by [`set_return_mode`], by identity, so the
    /// cursor returns to the same object even if the list shifted meanwhile.
    pub(super) fn restore_selection(&mut self) {
        let Some(key) = self.return_selection.take() else {
            return;
        };
        if let Some(i) = self.rows().iter().position(|o| row_key(o) == key) {
            self.table_state.select(Some(i));
        }
    }

    pub(super) fn open_detail(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_shared() else {
            return;
        };
        let target = document_target(&obj);
        // Helm rows are backed by the raw storage Secret — `y` should show
        // the rendered chart manifest, not that Secret's own YAML.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.spawn_document(target, "decoding Helm manifest…", move || {
                let rel = crate::helm::decode(&obj)
                    .ok_or_else(|| "could not decode this Helm release revision".to_string())?;
                Ok((
                    format!("{} v{} — manifest", rel.name, rel.revision),
                    rel.manifest.lines().map(String::from).collect(),
                ))
            });
            return;
        }
        let name = obj.metadata.name.clone().unwrap_or_else(|| "object".into());
        let kind = self.kind.clone();
        if document_is_expensive(&obj) {
            self.spawn_document(target, format!("rendering {name}…"), move || {
                Ok((format!("{name} — YAML"), stamped_yaml(&obj, kind.as_ref())))
            });
        } else {
            self.detail = Scrollable {
                title: format!("{name} — YAML"),
                lines: stamped_yaml(&obj, kind.as_ref()).into(),
                ..Default::default()
            };
            self.mode = Mode::Detail;
        }
    }

    /// Show the selected Secret with its `data` base64-decoded (k9s `x`), as
    /// it would appear in `stringData`. A plain [`Mode::Detail`] view, so `/`
    /// search and `c` copy work like every other single-document view.
    pub(super) fn open_decoded_secret(&mut self) {
        self.set_return_mode();
        self.show_decoded_secret();
    }

    /// The decoded-secret view itself, without touching the return mode — the
    /// in-document `x` binding lands here, so esc still returns to wherever
    /// the describe/YAML view was opened from.
    pub(super) fn show_decoded_secret(&mut self) {
        let Some(obj) = self.selected_shared() else {
            return;
        };
        let Some(data) = obj.data.get("data").and_then(Value::as_object) else {
            self.flash_warn("secret has no data");
            return;
        };
        if data.is_empty() {
            self.flash_warn("secret has no data");
            return;
        }
        let target = document_target(&obj);
        let name = obj.metadata.name.clone().unwrap_or_else(|| "secret".into());
        self.spawn_document(target, format!("decoding {name}…"), move || {
            let mut lines = Vec::new();
            if let Some(data) = obj.data.get("data").and_then(Value::as_object) {
                for (key, value) in data {
                    lines.extend(decoded_secret_entry(key, value));
                }
            }
            Ok((format!("{name} — decoded"), lines))
        });
    }

    /// Describe the selection via `kubectl describe`, off-thread so the UI loop
    /// keeps rendering. Falls back to the object's YAML if kubectl is missing
    /// or fails. The result arrives as `Msg::Detail`.
    pub(super) fn describe(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_shared() else {
            return;
        };
        let target = document_target(&obj);
        // No `kubectl describe` for a Helm release's storage Secret — decode
        // its NOTES.txt through the same bounded worker as the manifest.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.spawn_document(target, "decoding Helm notes…", move || {
                let rel = crate::helm::decode(&obj)
                    .ok_or_else(|| "could not decode this Helm release revision".to_string())?;
                let lines = if rel.notes.is_empty() {
                    vec!["<no notes>".to_string()]
                } else {
                    rel.notes.lines().map(String::from).collect()
                };
                Ok((format!("{} v{} — notes", rel.name, rel.revision), lines))
            });
            return;
        }
        let name = obj.metadata.name.clone().unwrap_or_default();
        let plural = self.kind_plural.clone();
        let ns = obj.metadata.namespace.clone();

        // The fallback needs the object as it is *now* — the selection may
        // change before the describe completes — but a describe that succeeds
        // never renders it, so carry the object and serialize only on failure.
        let fallback = obj;
        let fallback_kind = self.kind.clone();
        let yaml_title = format!("{name} — YAML");

        let tx = self.tx.clone();
        let workers = Arc::clone(&self.document_workers);
        let genr = self.generation;
        let mut argv = self.kubectl_base();
        argv.extend(["describe".to_string(), plural, name.clone()]);
        if let Some(ns) = &ns {
            argv.push("-n".into());
            argv.push(ns.clone());
        }
        let claim = self.claim_status(format!("describing {name}…"));
        tokio::spawn(async move {
            let msg = match tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .await
            {
                Ok(out) if out.status.success() => Msg::Detail {
                    generation: genr,
                    claim,
                    target: Some(target.clone()),
                    title: format!("{name} — describe"),
                    lines: String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .map(String::from)
                        .collect(),
                    warn: None,
                },
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    let _permit = match workers.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let lines = tokio::task::spawn_blocking(move || {
                        stamped_yaml(&fallback, fallback_kind.as_ref())
                    })
                    .await
                    .unwrap_or_else(|e| vec![format!("# document preparation failed: {e}")]);
                    Msg::Detail {
                        generation: genr,
                        claim,
                        target: Some(target.clone()),
                        title: yaml_title,
                        lines,
                        warn: Some(format!(
                            "kubectl describe failed ({}); showing YAML",
                            err.lines().next().unwrap_or("error")
                        )),
                    }
                }
                Err(_) => {
                    let _permit = match workers.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let lines = tokio::task::spawn_blocking(move || {
                        stamped_yaml(&fallback, fallback_kind.as_ref())
                    })
                    .await
                    .unwrap_or_else(|e| vec![format!("# document preparation failed: {e}")]);
                    Msg::Detail {
                        generation: genr,
                        claim,
                        target: Some(target),
                        title: yaml_title,
                        lines,
                        warn: Some("kubectl not found; showing YAML".into()),
                    }
                }
            };
            let _ = tx.send(msg).await;
        });
    }

    /// Render an object as YAML lines, stamping its type if missing.
    pub fn object_yaml(&self, obj: &DynamicObject) -> Vec<String> {
        stamped_yaml(obj, self.kind.as_ref())
    }

    /// Diff the live object against its `last-applied-configuration`
    /// (k9s-style), or — when that annotation is absent, as it is for every
    /// Flux/Helm-managed object — against the previous revision this session's
    /// watch saw, so "what just changed?" has an answer on GitOps clusters.
    pub fn open_diff(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_shared() else {
            return;
        };
        let target = document_target(&obj);
        let name = obj.metadata.name.clone().unwrap_or_default();

        let has_last_applied =
            obj.metadata.annotations.as_ref().is_some_and(|a| {
                a.contains_key("kubectl.kubernetes.io/last-applied-configuration")
            });

        // Carry shared snapshots into the worker. In particular, do not clone
        // a potentially multi-megabyte last-applied annotation on keypress.
        let (baseline, baseline_label) = if has_last_applied {
            (Baseline::LastApplied, "last-applied")
        } else {
            let key = row_key(&obj);
            let Some(prev) = self.prev_revisions.shared(&self.kind_plural, &key) else {
                self.flash_warn(
                    "nothing to diff: no last-applied annotation, \
                         and no change seen this session",
                );
                return;
            };
            (Baseline::Previous(prev), "session: previous")
        };
        let title = format!("{name} — diff ({baseline_label} → live)");

        // Two whole-document serializations plus the change walk. That is
        // sub-millisecond for an ordinary object and tens of milliseconds for
        // a large CRD, so the small case stays inline — where the document
        // appears in the same frame as the keypress — and only the large one
        // pays a round-trip to a blocking worker.
        if !diff_is_expensive(&baseline, &obj) {
            match render_diff(baseline, obj, baseline_label) {
                Ok(lines) => {
                    self.detail = Scrollable {
                        title,
                        lines: lines.into(),
                        ..Default::default()
                    };
                    self.mode = Mode::Diff;
                }
                Err(label) => {
                    self.flash = format!("no diff: live matches {label}");
                    self.flash_err = false; // nothing to show — stay put
                }
            }
            return;
        }

        let claim = self.claim_status(format!("diffing {name}…"));
        let tx = self.tx.clone();
        let workers = Arc::clone(&self.document_workers);
        let genr = self.generation;
        let label = baseline_label.to_string();
        tokio::spawn(async move {
            let Ok(_permit) = workers.acquire_owned().await else {
                return;
            };
            let Ok(result) =
                tokio::task::spawn_blocking(move || render_diff(baseline, obj, &label)).await
            else {
                return;
            };
            let _ = tx
                .send(Msg::Diff {
                    generation: genr,
                    claim,
                    target,
                    title,
                    result,
                })
                .await;
        });
    }

    /// Run document expansion in a bounded blocking worker and return it only
    /// if the same selected object and status claim are still current.
    pub(super) fn spawn_document<F>(&mut self, target: String, progress: impl Into<String>, job: F)
    where
        F: FnOnce() -> Result<(String, Vec<String>), String> + Send + 'static,
    {
        let claim = self.claim_status(progress);
        let generation = self.generation;
        let tx = self.tx.clone();
        let workers = Arc::clone(&self.document_workers);
        tokio::spawn(async move {
            let Ok(_permit) = workers.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(job).await;
            let msg = match result {
                Ok(Ok((title, lines))) => Msg::Detail {
                    generation,
                    claim,
                    target: Some(target),
                    title,
                    lines,
                    warn: None,
                },
                Ok(Err(message)) => Msg::Flash {
                    generation,
                    claim,
                    message,
                    err: true,
                },
                Err(e) => Msg::Flash {
                    generation,
                    claim,
                    message: format!("document preparation failed: {e}"),
                    err: true,
                },
            };
            let _ = tx.send(msg).await;
        });
    }

    pub(super) fn document_result_is_current(&self, claim: StatusClaim, target: &str) -> bool {
        self.owns_status(claim)
            && self
                .selected_ref()
                .is_some_and(|obj| document_target(obj) == target)
    }

    /// Live Events for the selected object, filtered by object UID when
    /// available. Uses the discovered `events` resource, so core/v1 Events are
    /// preferred but events.k8s.io clusters still work.
    pub(super) fn open_events(&mut self) {
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection for events");
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let uid = obj.metadata.uid.clone().filter(|u| !u.is_empty());
        self.open_events_for(name, ns, uid);
    }

    /// Live Events for an object identified by coordinates (rather than the
    /// current table selection), so the explain view can open the event stream
    /// for a blocking pod. `uid` scopes precisely when known; otherwise we fall
    /// back to a name(+namespace) selector.
    pub(super) fn open_events_for(&mut self, name: String, ns: String, uid: Option<String>) {
        self.set_return_mode();
        let Some(kind) = self.cluster.resolve("events") else {
            self.flash_warn("events kind unavailable");
            return;
        };

        let title = format!("{name} — events");
        let field = if kind.ar.group == "events.k8s.io" {
            "regarding"
        } else {
            "involvedObject"
        };
        let selector = uid
            .as_ref()
            .filter(|uid| !uid.is_empty())
            .map(|uid| format!("{field}.uid={uid}"))
            .unwrap_or_else(|| {
                let mut parts = vec![format!("{field}.name={name}")];
                if !ns.is_empty() {
                    parts.push(format!("{field}.namespace={ns}"));
                }
                parts.join(",")
            });

        self.stop_event_stream();
        let genr = self.event_gen;
        self.detail = Scrollable {
            title: title.clone(),
            lines: vec!["loading events…".into()].into(),
            ..Default::default()
        };
        self.flash = format!("events: {name}");
        self.flash_err = false;
        self.mode = Mode::Events;

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let ar = kind.ar.clone();
        let namespaced = kind.namespaced;
        let watch_ns = ns;
        let is_events_v1 = ar.group == "events.k8s.io";
        let handle = tokio::spawn(async move {
            let api: Api<DynamicObject> = if namespaced && !watch_ns.is_empty() {
                Api::namespaced_with(client, &watch_ns, &ar)
            } else {
                Api::all_with(client, &ar)
            };
            let cfg = watcher::Config::default().any_semantic().fields(&selector);
            let mut stream = watcher(api, cfg).boxed();
            let mut doc = EventDoc::new(is_events_v1);
            // The initial list arrives as a stream of `InitApply`s. Publishing
            // each one republished a growing document N times before the view
            // had drawn once; the document is only complete at `InitDone`.
            let mut synced = false;
            let mut dirty = false;
            let mut publish = tokio::time::interval(Duration::from_millis(EVENTS_PUBLISH_MS));
            publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut backoff = watcher::DefaultBackoff::default();

            loop {
                tokio::select! {
                    maybe_event = stream.next() => {
                        let Some(event) = maybe_event else { break };
                        // Progress, as opposed to another doomed list attempt:
                        // `Init` and its `InitApply`s replay on every attempt,
                        // so resetting on those never escalates the delay.
                        if matches!(
                            event,
                            Ok(watcher::Event::Apply(_)
                                | watcher::Event::Delete(_)
                                | watcher::Event::InitDone)
                        ) {
                            backoff.reset();
                        }
                        match event {
                            Ok(watcher::Event::Init) => {
                                doc.clear();
                                synced = false;
                            }
                            Ok(watcher::Event::Apply(obj))
                            | Ok(watcher::Event::InitApply(obj)) => {
                                doc.apply(&obj);
                                dirty = true;
                            }
                            Ok(watcher::Event::Delete(obj)) => {
                                doc.remove(&obj);
                                dirty = true;
                            }
                            Ok(watcher::Event::InitDone) => {
                                synced = true;
                                // The list is complete even when it is empty,
                                // which is the "(no events)" document.
                                dirty = true;
                            }
                            // Self-healing desync (410 Expired) — the watcher
                            // re-lists on its own; don't scribble an error line
                            // over the events document.
                            Err(e) if crate::k8s::watch_error_is_benign(&e) => continue,
                            // A watcher that fails before its first list would
                            // otherwise leave the view on "loading events…"
                            // forever, so this publishes directly rather than
                            // waiting for a tick that needs `synced`.
                            Err(e) => {
                                if tx
                                    .send(Msg::Events {
                                        generation: genr,
                                        title: title.clone(),
                                        lines: vec![format!("error: {e}")],
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                // An error that does not clear itself would
                                // otherwise re-list as fast as the stream is
                                // polled; pace the next attempt.
                                tokio::time::sleep(
                                    backoff.next().unwrap_or(crate::k8s::WATCH_BACKOFF_CEILING),
                                )
                                .await;
                            }
                        }
                    }
                    _ = publish.tick(), if dirty && synced => {
                        if !doc.publish(&tx, genr, &title).await {
                            break;
                        }
                        dirty = false;
                    }
                }
            }
            // A stream that ends between a change and the next publish tick
            // would otherwise leave the view one revision behind.
            if dirty && synced {
                let _ = doc.publish(&tx, genr, &title).await;
            }
        });
        self.event_task = Some(handle);
    }

    pub(super) fn stop_event_stream(&mut self) {
        self.event_gen += 1;
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
    }
}

/// A [`DynamicObject`] serialized from borrows, with the type header supplied
/// separately. Mirrors `DynamicObject`'s own field order and flattening, so it
/// produces byte-identical YAML without owning a copy of the object.
#[derive(serde::Serialize)]
struct TypedObject<'a> {
    #[serde(flatten)]
    types: Option<&'a TypeMeta>,
    metadata: &'a kube::core::ObjectMeta,
    #[serde(flatten)]
    data: &'a Value,
}

/// `obj` as YAML lines, stamped with `kind`'s type when the watch stripped it.
///
/// Stamping used to mean cloning the whole object for the sake of two strings
/// — a cost that scales with the object, not with the header — so the object
/// is serialized from the borrow and the projection supplies the type.
fn stamped_yaml(obj: &DynamicObject, kind: Option<&Kind>) -> Vec<String> {
    let stamped = match (kind, &obj.types) {
        (Some(kind), None) => Some(TypeMeta {
            api_version: kind.ar.api_version.clone(),
            kind: kind.ar.kind.clone(),
        }),
        _ => None,
    };
    let doc = TypedObject {
        types: stamped.as_ref().or(obj.types.as_ref()),
        metadata: &obj.metadata,
        data: &obj.data,
    };
    serde_yaml::to_string(&doc)
        .unwrap_or_else(|e| format!("# error: {e}"))
        .lines()
        .map(String::from)
        .collect()
}

/// Stable identity for an asynchronous document request. Including the
/// resource version prevents an old snapshot replacing a document after the
/// selected row updates in place.
pub(super) fn document_target(obj: &DynamicObject) -> String {
    format!(
        "{}@{}",
        row_key(obj),
        obj.metadata.resource_version.as_deref().unwrap_or_default()
    )
}

/// What `d` compares the live object against, carried unrendered so the
/// rendering can happen wherever the diff itself runs.
pub(crate) enum Baseline {
    /// Read the `last-applied-configuration` annotation from the shared live
    /// object only once document work has reached its execution context.
    LastApplied,
    /// The previous revision this session's watch saw.
    Previous(Arc<DynamicObject>),
}

impl Baseline {
    fn render(self, live: &DynamicObject) -> String {
        match self {
            Baseline::LastApplied => live
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
                .and_then(|json| serde_json::from_str::<Value>(json).ok())
                .and_then(|v| serde_yaml::to_string(&v).ok())
                .unwrap_or_else(|| {
                    live.metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
                        .cloned()
                        .unwrap_or_default()
                }),
            Baseline::Previous(obj) => diffable_yaml((*obj).clone()),
        }
    }
}

/// The whole diff document: clean both sides, then walk the change list.
/// `Err` carries `label` back when live matches the baseline, which shows a
/// status line instead of an empty document.
pub(crate) fn render_diff(
    baseline: Baseline,
    live: Arc<DynamicObject>,
    label: &str,
) -> Result<Vec<String>, String> {
    let baseline_yaml = baseline.render(&live);
    let lines = diff_document(&baseline_yaml, (*live).clone());
    if lines.iter().all(|l| l.starts_with(' ')) {
        return Err(label.to_string());
    }
    Ok(lines)
}

/// The whole diff document: clean both sides, then walk the change list.
/// Split out from `open_diff` so its cost can be measured, since it runs on
/// the UI thread in response to a keypress.
pub(crate) fn diff_document(baseline_yaml: &str, live: DynamicObject) -> Vec<String> {
    diff_lines(baseline_yaml, &diffable_yaml(live))
}

/// Above this much JSON on either side, rendering and diffing takes long
/// enough to be worth a round-trip to a worker rather than a stalled frame.
/// Ordinary objects are one to two orders of magnitude below it.
const DIFF_INLINE_BUDGET: usize = 128 * 1024;

/// Whether this diff should leave the UI thread. Answers from the object's
/// structure and stops counting as soon as the budget is gone, so the gate
/// itself stays far cheaper than the work it is gating.
fn diff_is_expensive(baseline: &Baseline, live: &DynamicObject) -> bool {
    let mut budget = DIFF_INLINE_BUDGET;
    let baseline_fits = match baseline {
        Baseline::LastApplied => {
            let len = live
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
                .map_or(0, String::len);
            budget = budget.saturating_sub(len);
            budget > 0
        }
        Baseline::Previous(obj) => object_fits_in(obj, &mut budget),
    };
    !baseline_fits || !object_fits_in(live, &mut budget)
}

fn document_is_expensive(obj: &DynamicObject) -> bool {
    let mut budget = DIFF_INLINE_BUDGET;
    // `object_fits_in` models diff YAML, which deliberately strips these two
    // fields. A plain YAML view keeps both, so account for the annotation and
    // send any managed-fields document to the worker: one FieldsV1 tree can be
    // arbitrarily larger than the rest of the object.
    if obj
        .metadata
        .managed_fields
        .as_ref()
        .is_some_and(|fields| !fields.is_empty())
    {
        return true;
    }
    if let Some(last_applied) = obj
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
    {
        budget = budget.saturating_sub(last_applied.len());
        if budget == 0 {
            return true;
        }
    }
    !object_fits_in(obj, &mut budget)
}

/// Both halves of an object as `diffable_yaml` will render it.
fn object_fits_in(obj: &DynamicObject, budget: &mut usize) -> bool {
    meta_fits_in(&obj.metadata, budget) && fits_in(&obj.data, budget)
}

/// Metadata's share of the estimate. Annotations alone can dominate an object,
/// so ignoring them would rate a huge object as cheap. The two things
/// `diffable_yaml` strips — managed fields and the `last-applied` annotation —
/// are left out here too, because they are never rendered.
fn meta_fits_in(meta: &kube::core::ObjectMeta, budget: &mut usize) -> bool {
    let mut charge = |n: usize| {
        *budget = budget.saturating_sub(n);
        *budget > 0
    };
    // Name, namespace, uid, timestamps and the like.
    if !charge(256) {
        return false;
    }
    if let Some(annotations) = &meta.annotations {
        for (k, v) in annotations {
            if k == "kubectl.kubernetes.io/last-applied-configuration" {
                continue;
            }
            if !charge(k.len() + v.len() + 6) {
                return false;
            }
        }
    }
    if let Some(labels) = &meta.labels {
        for (k, v) in labels {
            if !charge(k.len() + v.len() + 6) {
                return false;
            }
        }
    }
    if let Some(owners) = &meta.owner_references
        && !charge(owners.len() * 192)
    {
        return false;
    }
    if let Some(finalizers) = &meta.finalizers {
        for f in finalizers {
            if !charge(f.len() + 4) {
                return false;
            }
        }
    }
    true
}

/// Draw this value's rough serialized size from `budget`. `false` once the
/// budget is exhausted, at which point the walk stops early.
fn fits_in(v: &Value, budget: &mut usize) -> bool {
    let charge = |budget: &mut usize, n: usize| {
        *budget = budget.saturating_sub(n);
        *budget > 0
    };
    match v {
        Value::Null => charge(budget, 4),
        Value::Bool(_) => charge(budget, 5),
        Value::Number(_) => charge(budget, 8),
        Value::String(s) => charge(budget, s.len() + 2),
        Value::Array(items) => {
            if !charge(budget, 2) {
                return false;
            }
            items.iter().all(|item| fits_in(item, budget))
        }
        Value::Object(map) => {
            if !charge(budget, 2) {
                return false;
            }
            map.iter()
                .all(|(k, v)| charge(budget, k.len() + 4) && fits_in(v, budget))
        }
    }
}

/// Render an object as YAML cleaned for a readable side-by-side: no
/// managedFields, no last-applied annotation (it *is* one of the sides), and
/// no resourceVersion (it differs on every change — pure noise in a diff).
fn diffable_yaml(mut obj: DynamicObject) -> String {
    if let Some(ann) = obj.metadata.annotations.as_mut() {
        ann.remove("kubectl.kubernetes.io/last-applied-configuration");
    }
    obj.metadata.managed_fields = None;
    obj.metadata.resource_version = None;
    serde_yaml::to_string(&obj).unwrap_or_default()
}

/// Unified-diff lines (`-`/`+`/` ` prefixed) between two documents.
fn diff_lines(before: &str, after: &str) -> Vec<String> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(before, after);
    diff.iter_all_changes()
        .map(|change| {
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            format!("{sign}{}", change.value().trim_end_matches('\n'))
        })
        .collect()
}

/// Render one Secret `data` entry as stringData-style YAML lines: single-line
/// values inline (`key: value`), multiline ones as a literal block (`key: |`).
/// Values that aren't valid base64 or don't decode to UTF-8 text (TLS certs
/// in DER, random binary) get a placeholder instead of mojibake.
fn decoded_secret_entry(key: &str, value: &Value) -> Vec<String> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let Some(b64) = value.as_str() else {
        return vec![format!("{key}: <not a string>")];
    };
    let Ok(bytes) = BASE64.decode(b64) else {
        return vec![format!("{key}: <invalid base64>")];
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => return vec![format!("{key}: <binary: {} bytes>", e.as_bytes().len())],
    };
    let text = text.trim_end_matches('\n');
    if text.contains('\n') {
        let mut lines = vec![format!("{key}: |")];
        lines.extend(text.lines().map(|l| format!("  {l}")));
        lines
    } else {
        vec![format!("{key}: {text}")]
    }
}
