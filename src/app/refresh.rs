use super::*;
use crate::store::RefreshContent;
use kube::ResourceExt;

#[derive(Clone)]
pub(super) struct RefreshSource {
    pub client: Client,
    pub kind: Kind,
    pub object: DynamicObject,
    pub view: RefreshView,
}

#[derive(Clone)]
pub(super) enum RefreshView {
    Yaml,
    DecodedSecret,
    Describe(Vec<String>),
    NativeDescribe,
    Diff {
        baseline: String,
    },
    Explain {
        pods: Option<Box<Kind>>,
        events: Option<Box<Kind>>,
    },
}

impl RefreshSource {
    fn mode(&self) -> Mode {
        match self.view {
            RefreshView::Diff { .. } => Mode::Diff,
            RefreshView::Explain { .. } => Mode::Explain,
            _ => Mode::Detail,
        }
    }

    pub async fn read(&self) -> Result<DynamicObject, String> {
        report_source(
            &self.client,
            &self.kind.ar,
            self.kind.namespaced,
            &self.object,
        )
        .await
    }

    async fn refresh(&self) -> Result<RefreshContent, String> {
        if let RefreshView::Explain { pods, events } = &self.view {
            let (source, findings, warning) =
                explain::gather_explain(self, pods.as_deref(), events.as_deref()).await?;
            if let Some(warning) = warning {
                return Err(warning);
            }
            return Ok(RefreshContent::Explain {
                source: Box::new(source),
                findings,
            });
        }
        if matches!(self.view, RefreshView::NativeDescribe) {
            let (source, output) =
                deskribe::fetch(self.client.clone(), &self.kind.ar, &self.object).await?;
            return Ok(RefreshContent::Document {
                source: Box::new(source),
                lines: output.lines().map(String::from).collect(),
            });
        }
        let mut source = self.read().await?;
        source.managed_fields_mut().clear();
        let lines = match &self.view {
            RefreshView::Yaml => serde_yaml::to_string(&source)
                .map_err(|e| e.to_string())?
                .lines()
                .map(String::from)
                .collect(),
            RefreshView::DecodedSecret => details::decoded_secret_lines(&source),
            RefreshView::Diff { baseline } => {
                details::diff_lines(baseline, &details::diffable_yaml(source.clone()))
            }
            RefreshView::Describe(argv) => {
                let output = tokio::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|e| format!("kubectl describe failed: {e}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "kubectl describe failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                            .lines()
                            .next()
                            .unwrap_or("error")
                    ));
                }
                // Check again because kubectl reads by name, not UID.
                self.read().await?;
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(String::from)
                    .collect()
            }
            RefreshView::Explain { .. } | RefreshView::NativeDescribe => unreachable!(),
        };
        Ok(RefreshContent::Document {
            source: Box::new(source),
            lines,
        })
    }
}

impl App {
    pub(super) fn resource_source(
        &self,
        object: &DynamicObject,
        view: RefreshView,
    ) -> Option<RefreshSource> {
        let mut kind = match &object.types {
            Some(types) => {
                let group = types
                    .api_version
                    .split_once('/')
                    .map_or("", |(group, _)| group);
                let mut kind = self.cluster.resolve_in_group(&types.kind, group)?;
                kind.ar.api_version = types.api_version.clone();
                kind.ar.version = types.api_version.rsplit('/').next()?.into();
                kind
            }
            None => self.kind.clone()?,
        };
        // Preserve the selected API resource when it has the same type.
        if let Some(selected) = &self.kind
            && selected.ar.api_version == kind.ar.api_version
            && selected.ar.kind == kind.ar.kind
        {
            kind = selected.clone();
        }
        let mut object = object.clone();
        if object.types.is_none() {
            object.types = Some(TypeMeta {
                api_version: kind.ar.api_version.clone(),
                kind: kind.ar.kind.clone(),
            });
        }
        Some(RefreshSource {
            client: self.cluster.client.clone(),
            kind,
            object,
            view,
        })
    }

    fn refresh_source(&self) -> Option<&RefreshSource> {
        let mode = if self.mode == Mode::DocFilter {
            self.doc_filter_return
        } else {
            self.mode
        };
        let source = if mode == Mode::Explain {
            self.explain_refresh_source.as_ref()
        } else {
            self.document_source.as_ref()
        }?;
        (source.mode() == mode).then_some(source)
    }

    pub fn resource_refresh_available(&self) -> bool {
        self.refresh_source().is_some()
    }

    pub(super) fn stop_resource_refresh(&mut self) {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        if let Some(task) = self.refresh_task.take() {
            task.abort();
        }
    }

    /// Apply the config opt-in while retaining an explicit CLI opt-in.
    pub fn configure_native_describe(&mut self, configured: bool) {
        self.native_describe = self.native_describe_override || configured;
        if !self.native_describe
            && self
                .document_source
                .as_ref()
                .is_some_and(|source| matches!(source.view, RefreshView::NativeDescribe))
        {
            self.stop_resource_refresh();
            self.clear_document_source();
        }
    }

    pub(super) fn clear_document_source(&mut self) {
        self.document_source = None;
        if let Some(task) = self.describe_task.take() {
            task.abort();
        }
        if let Some((claim, _)) = self.describe_source.take() {
            self.clear_claimed_status(claim);
        }
    }

    pub(super) fn check_resource_refresh(&mut self) {
        if self.should_quit {
            self.clear_document_source();
            self.cancel_explain_request();
        }
        if self.should_quit || !self.resource_refresh_available() {
            self.stop_resource_refresh();
        }
    }

    pub(super) fn toggle_resource_refresh(&mut self) {
        if self.refresh_task.is_some() {
            self.stop_resource_refresh();
            self.flash = "automatic refresh: off".into();
            self.flash_err = false;
            return;
        }
        let Some(mut source) = self.refresh_source().cloned() else {
            self.flash_warn("refresh is unavailable for this view");
            return;
        };
        self.stop_resource_refresh();
        if source.mode() == Mode::Explain {
            self.cancel_explain_request();
        }
        if let Some(task) = self.describe_task.take() {
            task.abort();
        }
        if let Some((claim, _)) = self.describe_source.take() {
            self.clear_claimed_status(claim);
        }
        let generation = self.refresh_generation;
        let tx = self.tx.clone();
        self.refresh_task = Some(tokio::spawn(async move {
            loop {
                let result = source.refresh().await;
                let failed = result.is_err();
                if let Ok(
                    RefreshContent::Document { source: fresh, .. }
                    | RefreshContent::Explain { source: fresh, .. },
                ) = &result
                {
                    source.object = *fresh.clone();
                }
                if tx
                    .send(Msg::ResourceRefresh { generation, result })
                    .await
                    .is_err()
                    || failed
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }));
        self.flash = "automatic refresh: on (5s)".into();
        self.flash_err = false;
    }

    pub(super) fn apply_resource_refresh(&mut self, content: RefreshContent) {
        match content {
            RefreshContent::Document { source, lines } => {
                if let Some(document) = self.document_source.as_mut() {
                    document.object = *source;
                }
                self.detail.replace_lines(lines.into());
            }
            RefreshContent::Explain { source, findings } => {
                if let Some(report) = self.explain_refresh_source.as_mut() {
                    report.object = *source.clone();
                }
                self.explain_source = Some(*source);
                let selected = self
                    .explain_state
                    .selected()
                    .and_then(|index| self.explain_items.get(index));
                let keep = if let Some(selected) = selected {
                    findings.iter().position(|finding| match &selected.target {
                        Some(target) => finding.target.as_ref() == Some(target),
                        None => finding == selected,
                    })
                } else if self.explain_items.is_empty() && !self.explain_selection_lost {
                    findings
                        .iter()
                        .position(|finding| finding.target.is_some())
                        .or_else(|| (!findings.is_empty()).then_some(0))
                } else {
                    None
                };
                if selected.is_some() && keep.is_none() {
                    self.explain_selection_lost = true;
                    self.flash_warn(
                        "selected finding is no longer available; select another finding",
                    );
                } else if keep.is_some() {
                    self.explain_selection_lost = false;
                }
                self.explain_state.select(keep);
                self.explain_items = findings;
            }
        }
    }

    pub(super) fn reset_diff_baseline(&mut self) {
        let Some(source) = self.document_source.as_mut() else {
            return;
        };
        let RefreshView::Diff { baseline } = &mut source.view else {
            return;
        };
        *baseline = details::diffable_yaml(source.object.clone());
        let lines = details::diff_lines(baseline, baseline);
        let name = source.object.metadata.name.as_deref().unwrap_or("object");
        self.detail.title = format!("{name} - diff (reset baseline → live)");
        self.detail.replace_lines(lines.into());
        let running = self.refresh_task.is_some();
        self.stop_resource_refresh();
        if running {
            self.toggle_resource_refresh();
        }
        self.flash = "diff baseline reset to the displayed resource".into();
        self.flash_err = false;
    }
}
