use super::*;

impl App {
    // ----- detail / describe --------------------------------------------

    /// Remember which view a transient sub-view (logs/detail/diff) was opened
    /// from, so `esc` returns there (e.g. back to the xray tree, not the table).
    pub(super) fn set_return_mode(&mut self) {
        self.return_mode = if self.mode == Mode::Xray {
            Mode::Xray
        } else {
            Mode::Table
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
        let title = obj.metadata.name.clone().unwrap_or_else(|| "object".into());
        self.detail = Scrollable {
            title: format!("{title} — YAML"),
            lines: self.object_yaml(obj).into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }

    /// Describe the selection via `kubectl describe`, off-thread so the UI loop
    /// keeps rendering. Falls back to the object's YAML if kubectl is missing
    /// or fails. The result arrives as `Msg::Detail`.
    pub(super) fn describe(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_ref() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let plural = self.kind_plural.clone();
        let ns = obj.metadata.namespace.clone();

        // Compute the YAML fallback up front while we hold the object; the
        // selection may change before the describe completes.
        let yaml = self.object_yaml(obj);
        let yaml_title = format!("{name} — YAML");

        let tx = self.tx.clone();
        let genr = self.generation;
        let mut argv = self.kubectl_base();
        argv.extend(["describe".to_string(), plural, name.clone()]);
        if let Some(ns) = &ns {
            argv.push("-n".into());
            argv.push(ns.clone());
        }
        self.flash = format!("describing {name}…");
        self.flash_err = false;
        tokio::spawn(async move {
            let msg = match tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .await
            {
                Ok(out) if out.status.success() => Msg::Detail {
                    generation: genr,
                    title: format!("{name} — describe"),
                    lines: String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .map(String::from)
                        .collect(),
                    warn: None,
                },
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    Msg::Detail {
                        generation: genr,
                        title: yaml_title,
                        lines: yaml,
                        warn: Some(format!(
                            "kubectl describe failed ({}); showing YAML",
                            err.lines().next().unwrap_or("error")
                        )),
                    }
                }
                Err(_) => Msg::Detail {
                    generation: genr,
                    title: yaml_title,
                    lines: yaml,
                    warn: Some("kubectl not found; showing YAML".into()),
                },
            };
            let _ = tx.send(msg).await;
        });
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

    /// Diff the live object against its `last-applied-configuration` (k9s-style).
    pub fn open_diff(&mut self) {
        use similar::{ChangeTag, TextDiff};
        self.set_return_mode();
        let Some(mut obj) = self.selected() else {
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();

        let last = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("kubectl.kubernetes.io/last-applied-configuration"))
            .cloned();
        let Some(last_json) = last else {
            self.flash_warn("no last-applied-configuration (not applied via kubectl apply)");
            return;
        };
        let last_yaml = serde_json::from_str::<Value>(&last_json)
            .ok()
            .and_then(|v| serde_yaml::to_string(&v).ok())
            .unwrap_or(last_json);

        // Clean the live object for a readable comparison.
        if let Some(ann) = obj.metadata.annotations.as_mut() {
            ann.remove("kubectl.kubernetes.io/last-applied-configuration");
        }
        obj.metadata.managed_fields = None;
        let live_yaml = serde_yaml::to_string(&obj).unwrap_or_default();

        let diff = TextDiff::from_lines(&last_yaml, &live_yaml);
        let mut lines = Vec::new();
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            lines.push(format!("{sign}{}", change.value().trim_end_matches('\n')));
        }
        if lines.iter().all(|l| l.starts_with(' ')) {
            self.flash = "no diff: live matches last-applied".into();
            self.flash_err = false;
            return; // nothing to show — stay on the current view
        }
        self.detail = Scrollable {
            title: format!("{name} — diff (last-applied → live)"),
            lines: lines.into(),
            ..Default::default()
        };
        self.mode = Mode::Diff;
    }

    /// Live Events for the selected object, filtered by object UID when
    /// available. Uses the discovered `events` resource, so core/v1 Events are
    /// preferred but events.k8s.io clusters still work.
    pub(super) fn open_events(&mut self) {
        self.set_return_mode();
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection for events");
            return;
        };
        let Some(kind) = self.cluster.resolve("events") else {
            self.flash_warn("events kind unavailable");
            return;
        };

        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let title = format!("{name} — events");
        let field = if kind.ar.group == "events.k8s.io" {
            "regarding"
        } else {
            "involvedObject"
        };
        let selector = obj
            .metadata
            .uid
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
