use super::*;

impl App {
    // ----- detail / describe --------------------------------------------

    /// Remember which view a transient sub-view (logs/detail/diff) was opened
    /// from, so `esc` returns there (e.g. back to the xray tree, not the table).
    pub(super) fn set_return_mode(&mut self) {
        self.stop_resource_refresh();
        self.clear_document_source();
        self.stop_plugins();
        // A transient sub-view (logs/detail/events) opened from a list-style
        // view returns to that view, not the table underneath it.
        self.return_mode = match self.mode {
            Mode::Xray => Mode::Xray,
            Mode::Explain => Mode::Explain,
            Mode::Adjacent => Mode::Adjacent,
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
        let Some(obj) = self.selected_ref() else {
            return;
        };
        // Helm rows are backed by the raw storage Secret — `y` should show
        // the rendered chart manifest, not that Secret's own YAML.
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            match crate::helm::decode(obj) {
                Some(rel) => {
                    self.detail = Scrollable {
                        wrap: self.detail.wrap,
                        title: format!("{} v{} — manifest", rel.name, rel.revision),
                        lines: rel.manifest.lines().map(String::from).collect(),
                        ..Default::default()
                    };
                    self.mode = Mode::Detail;
                }
                None => self.flash_warn("could not decode this Helm release revision"),
            }
            return;
        }
        let obj = obj.clone();
        self.show_yaml(&obj);
    }

    /// The YAML view of one object — the selected row, or a row of a list
    /// view that already holds the object.
    pub(super) fn show_yaml(&mut self, obj: &DynamicObject) {
        self.stop_resource_refresh();
        self.clear_document_source();
        self.document_source = self.resource_source(obj, refresh::RefreshView::Yaml);
        let title = obj.metadata.name.clone().unwrap_or_else(|| "object".into());
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: format!("{title} — YAML"),
            lines: self.object_yaml(obj).into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
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
        let obj = self
            .document_source
            .as_ref()
            .map(|source| source.object.clone())
            .or_else(|| self.selected());
        let Some(obj) = obj else { return };
        if obj
            .data
            .get("data")
            .and_then(Value::as_object)
            .is_none_or(|data| data.is_empty())
        {
            self.flash_warn("secret has no data");
            return;
        }
        self.stop_resource_refresh();
        self.clear_document_source();
        self.document_source = self.resource_source(&obj, refresh::RefreshView::DecodedSecret);
        let title = obj.metadata.name.as_deref().unwrap_or("secret");
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: format!("{title} - decoded"),
            lines: decoded_secret_lines(&obj).into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }

    /// Describe off-thread using native specialized or generic renderers.
    /// Only the compatibility kubectl path falls back to cached YAML.
    pub(super) fn describe(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_ref() else {
            return;
        };
        // No `kubectl describe` for a Helm release's storage Secret — show
        // the rendered NOTES.txt instead, decoded synchronously (cheap, no
        // subprocess needed).
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            match crate::helm::decode(obj) {
                Some(rel) => {
                    let lines = if rel.notes.is_empty() {
                        vec!["<no notes>".to_string()]
                    } else {
                        rel.notes.lines().map(String::from).collect()
                    };
                    self.detail = Scrollable {
                        wrap: self.detail.wrap,
                        title: format!("{} v{} — notes", rel.name, rel.revision),
                        lines: lines.into(),
                        ..Default::default()
                    };
                    self.mode = Mode::Detail;
                }
                None => self.flash_warn("could not decode this Helm release revision"),
            }
            return;
        }
        let resource = self.kubectl_resource();
        let obj = obj.clone();
        self.describe_object(resource, &obj);
    }

    /// Describe one object, retaining the backend and identity for refresh.
    pub(super) fn describe_object(&mut self, resource: String, obj: &DynamicObject) {
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone();

        // Cached YAML remains a compatibility fallback only in kubectl-default mode.
        let yaml = (!self.native_describe).then(|| self.object_yaml(obj));

        let tx = self.tx.clone();
        let genr = self.generation;
        let mut argv = self.kubectl_base();
        argv.extend(["describe".to_string(), resource, name.clone()]);
        if let Some(ns) = &ns {
            argv.push("-n".into());
            argv.push(ns.clone());
        }
        let claim = self.claim_status(format!("describing {name}…"));
        self.stop_resource_refresh();
        self.clear_document_source();
        self.document_source =
            self.resource_source(obj, refresh::RefreshView::Describe(argv.clone()));
        self.describe_source = Some((claim, argv.clone()));
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: format!("{name} - describe"),
            lines: vec!["loading describe...".into()].into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
        if self.native_describe
            && let Some(source) = self.document_source.as_mut()
            && deskribe::supports(&source.kind.ar)
        {
            source.view = refresh::RefreshView::NativeDescribe;
            let source = source.clone();
            self.describe_task = Some(tokio::spawn(async move {
                let result = deskribe::fetch(source.client, &source.kind.ar, &source.object)
                    .await
                    .map(|(object, output)| (Box::new(object), output));
                let _ = tx
                    .send(Msg::NativeDescribeReady {
                        generation: genr,
                        claim,
                        result,
                    })
                    .await;
            }));
            return;
        }
        if self.native_describe {
            self.detail.title = format!("{name} — describe (kubectl fallback)");
            self.detail.replace_lines(
                vec!["native description unavailable; loading kubectl fallback...".into()].into(),
            );
        }
        self.describe_task = Some(tokio::spawn(async move {
            let output = tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .kill_on_drop(true)
                .output()
                .await;
            let msg = kubectl_describe_message(genr, claim, &name, yaml, output);
            let _ = tx.send(msg).await;
        }));
    }

    /// Render an object as YAML lines, stamping its type if missing.
    pub(super) fn object_yaml(&self, obj: &DynamicObject) -> Vec<String> {
        let mut obj = obj.clone();
        if let Some(kind) = &self.kind
            && obj.types.is_none()
        {
            obj.types = Some(TypeMeta {
                api_version: kind.ar.api_version.clone(),
                kind: kind.ar.kind.clone(),
            });
        }
        serde_yaml::to_string(&obj)
            .unwrap_or_else(|e| format!("# error: {e}"))
            .lines()
            .map(String::from)
            .collect()
    }

    /// Diff the live object against its `last-applied-configuration`
    /// (k9s-style), or — when that annotation is absent, as it is for every
    /// Flux/Helm-managed object — against the previous revision this session's
    /// watch saw, so "what just changed?" has an answer on GitOps clusters.
    pub fn open_diff(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();

        let last = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
            .cloned();

        let (baseline_yaml, baseline_label) = match last {
            Some(last_json) => {
                let yaml = serde_json::from_str::<Value>(&last_json)
                    .ok()
                    .and_then(|v| serde_yaml::to_string(&v).ok())
                    .unwrap_or(last_json);
                (yaml, "last-applied")
            }
            None => {
                let key = row_key(&obj);
                let Some(prev) = self.prev_revisions.get(&self.kind_plural, &key) else {
                    self.flash_warn(
                        "nothing to diff: no last-applied annotation, \
                         and no change seen this session",
                    );
                    return;
                };
                (diffable_yaml(prev.clone()), "session: previous")
            }
        };

        let source = self.resource_source(
            &obj,
            refresh::RefreshView::Diff {
                baseline: baseline_yaml.clone(),
            },
        );
        let live_yaml = diffable_yaml(obj);
        let lines = diff_lines(&baseline_yaml, &live_yaml);
        if lines.iter().all(|l| l.starts_with(' ')) {
            self.flash = format!("no diff: live matches {baseline_label}");
            self.flash_err = false;
        }
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title: format!("{name} — diff ({baseline_label} → live)"),
            lines: lines.into(),
            ..Default::default()
        };
        self.document_source = source;
        self.mode = Mode::Diff;
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
            wrap: self.detail.wrap,
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
            let mut items: HashMap<String, DynamicObject> = HashMap::new();

            while let Some(event) = stream.next().await {
                match event {
                    Ok(watcher::Event::Init) => items.clear(),
                    Ok(watcher::Event::Apply(obj)) | Ok(watcher::Event::InitApply(obj)) => {
                        items.insert(row_key(&obj), obj);
                    }
                    Ok(watcher::Event::Delete(obj)) => {
                        items.remove(&row_key(&obj));
                    }
                    Ok(watcher::Event::InitDone) => {}
                    // Self-healing desync (410 Expired) — the watcher
                    // re-lists on its own; don't scribble an error line
                    // over the events document.
                    Err(e) if crate::k8s::watch_error_is_benign(&e) => continue,
                    Err(e) => {
                        let _ = tx
                            .send(Msg::Events {
                                generation: genr,
                                title: title.clone(),
                                lines: vec![format!("error: {e}")],
                            })
                            .await;
                        continue;
                    }
                }

                if !send_event_snapshot(&tx, genr, &title, &items, is_events_v1).await {
                    break;
                }
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

/// Render an object as YAML cleaned for a readable side-by-side: no
/// managedFields, no last-applied annotation (it *is* one of the sides), and
/// no resourceVersion (it differs on every change — pure noise in a diff).
pub(super) fn diffable_yaml(mut obj: DynamicObject) -> String {
    if let Some(ann) = obj.metadata.annotations.as_mut() {
        ann.remove("kubectl.kubernetes.io/last-applied-configuration");
    }
    obj.metadata.managed_fields = None;
    obj.metadata.resource_version = None;
    serde_yaml::to_string(&obj).unwrap_or_default()
}

/// Unified-diff lines (`-`/`+`/` ` prefixed) between two documents.
pub(super) fn diff_lines(before: &str, after: &str) -> Vec<String> {
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

pub(super) fn kubectl_describe_message(
    generation: u64,
    claim: crate::store::StatusClaim,
    name: &str,
    yaml: Option<Vec<String>>,
    output: std::io::Result<std::process::Output>,
) -> Msg {
    let title = if yaml.is_none() {
        format!("{name} — describe (kubectl fallback)")
    } else {
        format!("{name} — describe")
    };
    let error = match output {
        Ok(out) if out.status.success() => {
            return Msg::DescribeReady {
                generation,
                claim,
                title,
                lines: String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(String::from)
                    .collect(),
                warn: None,
            };
        }
        Ok(out) => format!(
            "kubectl describe failed ({})",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("error")
        ),
        Err(error) => format!("kubectl describe could not start: {error}"),
    };
    match yaml {
        Some(lines) => Msg::DescribeReady {
            generation,
            claim,
            title: format!("{name} — YAML"),
            lines,
            warn: Some(format!("{error}; showing YAML")),
        },
        None => Msg::DescribeReady {
            generation,
            claim,
            title,
            lines: vec![format!("Describe failed: {error}")],
            warn: Some(error),
        },
    }
}

pub(super) fn decoded_secret_lines(obj: &DynamicObject) -> Vec<String> {
    let Some(data) = obj
        .data
        .get("data")
        .and_then(Value::as_object)
        .filter(|data| !data.is_empty())
    else {
        return vec!["secret has no data".into()];
    };
    data.iter()
        .flat_map(|(key, value)| decoded_secret_entry(key, value))
        .collect()
}
