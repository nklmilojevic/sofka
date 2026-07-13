use super::*;

use super::explain::{filter_events, list_selected};
use crate::bundle::{Doc, redact_to_yaml};

const VERSION: &str = env!("CARGO_PKG_VERSION");

impl App {
    /// `:bundle` — assemble a redacted incident bundle for the selected object
    /// (its YAML, owner, conditions, events, explanation, timeline, bounded
    /// logs, and a metrics snapshot), gathered off-thread and previewed before
    /// it can be saved with `:bundle-save`.
    pub(super) fn open_bundle(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("bundle is not available for Helm releases");
            return;
        }
        let Some(obj) = self.selected_ref().cloned() else {
            self.flash_warn("no selection for bundle");
            return;
        };

        // In-memory pieces gathered on the main thread.
        let rk = row_key(&obj);
        let plural = self.kind_plural.clone();
        let timeline: Vec<String> = self
            .timeline
            .entries(&plural, &rk)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| format!("{}  {}", crate::timeline::clock(e.at), e.text))
                    .collect()
            })
            .unwrap_or_default();
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let metric_key = if ns.is_empty() {
            name.clone()
        } else {
            format!("{ns}/{name}")
        };
        let metrics = self.metrics.get(&metric_key).copied();

        let kind = self.kind.clone().unwrap();
        let kind_name = kind.ar.kind.clone();
        let context = self.cluster.context.clone();
        let cluster = self.cluster.cluster_name.clone();
        let anonymize = self.bundle_cfg.anonymize;
        let log_lines = self.bundle_cfg.log_lines.max(0);
        let max_pods = self.bundle_cfg.max_pods;

        // Resolve the kinds the gather task needs up front (needs cluster state).
        let pods_kind = self.cluster.resolve("pods").map(|k| (k.ar, k.namespaced));
        let events_kind = self.cluster.resolve("events").map(|k| (k.ar, k.namespaced));
        let owner_ref = obj
            .metadata
            .owner_references
            .as_ref()
            .and_then(|o| o.first())
            .cloned();
        let owner_kind = owner_ref
            .as_ref()
            .and_then(|o| self.cluster.resolve(&o.kind.to_lowercase()))
            .map(|k| (k.ar, k.namespaced));

        let selector = match plural.as_str() {
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" => {
                label_selector(&obj, "matchLabels")
            }
            _ => None,
        };

        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = format!("assembling bundle for {name}…");
        self.flash_err = false;

        tokio::spawn(async move {
            let ts = k8s_openapi::jiff::Timestamp::now();

            // ---- gather -----------------------------------------------------
            let pods: Vec<DynamicObject> = if plural == "pods" {
                vec![obj.clone()]
            } else if let (Some((ar, nsd)), Some(sel)) = (&pods_kind, &selector) {
                list_selected(&client, ar, *nsd, &ns, sel).await
            } else {
                Vec::new()
            };

            let (events, events_v1) = match &events_kind {
                Some((ar, nsd)) => {
                    let v1 = ar.group == "events.k8s.io";
                    let all = list_kind(&client, ar, *nsd, &ns).await;
                    (filter_events(&all, &obj, &pods, v1), v1)
                }
                None => (Vec::new(), false),
            };

            let evidence = crate::explain::Evidence {
                kind: &kind_name,
                plural: &plural,
                obj: &obj,
                pods: &pods,
                events: &events,
                events_v1,
            };
            let findings = crate::explain::explain(&evidence);

            let owner = match (&owner_ref, &owner_kind) {
                (Some(oref), Some((ar, nsd))) => {
                    let api: Api<DynamicObject> = if *nsd && !ns.is_empty() {
                        Api::namespaced_with(client.clone(), &ns, ar)
                    } else {
                        Api::all_with(client.clone(), ar)
                    };
                    api.get(&oref.name).await.ok().map(|mut o| {
                        o.types = Some(TypeMeta {
                            api_version: ar.api_version.clone(),
                            kind: oref.kind.clone(),
                        });
                        (oref.kind.clone(), o)
                    })
                }
                _ => None,
            };

            // Bounded logs for the first container of up to `max_pods` pods.
            let mut log_blocks: Vec<(String, Vec<String>)> = Vec::new();
            if log_lines > 0 {
                for pod in pods.iter().take(max_pods) {
                    let pname = pod.metadata.name.clone().unwrap_or_default();
                    let pns = pod.metadata.namespace.clone().unwrap_or(ns.clone());
                    let container = first_container(pod);
                    let api: Api<k8s_openapi::api::core::v1::Pod> =
                        Api::namespaced(client.clone(), &pns);
                    let lp = LogParams {
                        container,
                        tail_lines: Some(log_lines),
                        timestamps: true,
                        ..Default::default()
                    };
                    let lines = match api.logs(&pname, &lp).await {
                        Ok(text) => text.lines().map(String::from).collect(),
                        Err(e) => vec![format!("(logs unavailable: {e})")],
                    };
                    log_blocks.push((pname, lines));
                }
            }

            // ---- render -----------------------------------------------------
            let title = format!("Diagnostic bundle — {kind_name}/{name}");
            let (ctx_shown, cluster_shown) = if anonymize {
                ("«anonymized»".to_string(), "«anonymized»".to_string())
            } else {
                (context, cluster)
            };
            let mut doc = Doc::new(&title);
            doc.field("sofka", &format!("v{VERSION}"));
            doc.field("collected", &ts.to_string());
            doc.field("context", &ctx_shown);
            doc.field("cluster", &cluster_shown);
            doc.field(
                "namespace",
                if ns.is_empty() {
                    "(cluster-scoped)"
                } else {
                    &ns
                },
            );
            doc.field("resource", &format!("{kind_name}/{name}"));

            // Redact the primary object and (if any) its owner, collecting notes.
            let mut typed = obj.clone();
            typed.types = Some(TypeMeta {
                api_version: kind.ar.api_version.clone(),
                kind: kind_name.clone(),
            });
            let (obj_yaml, mut redactions) = redact_to_yaml(&typed, &kind_name);
            let owner_rendered = owner.as_ref().map(|(ok, oobj)| {
                let (y, notes) = redact_to_yaml(oobj, ok);
                redactions.extend(notes.into_iter().map(|n| format!("owner: {n}")));
                (
                    ok.clone(),
                    oobj.metadata.name.clone().unwrap_or_default(),
                    y,
                )
            });

            // Manifest: what's included, then what was redacted.
            let mut manifest = vec![
                format!("resource YAML ({kind_name}/{name})"),
                match &owner_rendered {
                    Some((ok, oname, _)) => format!("owner ({ok}/{oname})"),
                    None => "owner: none".into(),
                },
                format!("{} related pod(s)", pods.len()),
                format!("{} event(s)", events.len()),
                format!("{} explanation finding(s)", findings.len()),
                format!("{} timeline entry(ies)", timeline.len()),
                format!(
                    "logs from {} pod(s) (≤{log_lines} lines each)",
                    log_blocks.len()
                ),
                match metrics {
                    Some(_) => "metrics snapshot".into(),
                    None => "metrics: none".into(),
                },
            ];
            if anonymize {
                manifest.push("context/cluster anonymized".into());
            }

            doc.heading("Manifest — included");
            doc.bullets(&manifest);
            doc.heading("Manifest — redacted");
            doc.bullets(&if redactions.is_empty() {
                vec!["nothing sensitive found to redact".into()]
            } else {
                redactions
            });

            doc.heading("Explanation");
            doc.code("text", &render_findings(&findings));

            if let Some((_, _, y)) = &owner_rendered {
                doc.heading("Owner");
                doc.code("yaml", y);
            }

            doc.heading("Resource");
            doc.code("yaml", &obj_yaml);

            doc.heading("Events");
            doc.code("text", &format_event_lines(&events, events_v1));

            doc.heading("Timeline (session-local)");
            doc.code("text", &timeline);

            doc.heading("Metrics");
            doc.code(
                "text",
                &match metrics {
                    Some((cpu, mem)) => {
                        vec![format!("cpu: {cpu}m"), format!("memory: {mem} bytes")]
                    }
                    None => Vec::new(),
                },
            );

            for (pod, lines) in &log_blocks {
                doc.heading(&format!("Logs — {pod}"));
                doc.code("text", lines);
            }

            let text = doc.finish();
            let safe: String = format!("{kind_name}-{name}")
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect();
            let filename = format!("sofka-bundle-{}-{}.md", safe, ts.as_second());

            let _ = tx
                .send(Msg::Bundle {
                    generation: genr,
                    title,
                    text,
                    filename,
                })
                .await;
        });
    }

    /// `:bundle-save` — write the previewed bundle to a temp file.
    pub(super) fn save_bundle(&mut self) {
        let Some((filename, text)) = self.pending_bundle.clone() else {
            self.flash_warn("no bundle to save — run :bundle first");
            return;
        };
        let path = std::env::temp_dir().join(filename);
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let result = tokio::fs::write(&path, text)
                .await
                .map(|_| path)
                .map_err(|e| e.to_string());
            let _ = tx
                .send(Msg::BundleSaved {
                    generation: genr,
                    result,
                })
                .await;
        });
    }
}

/// Render explanation findings as indented plain-text lines for the bundle.
fn render_findings(findings: &[crate::explain::Finding]) -> Vec<String> {
    if findings.is_empty() {
        return vec!["(healthy — nothing to explain)".into()];
    }
    findings
        .iter()
        .map(|f| format!("{}{}", "  ".repeat(f.indent as usize), f.text))
        .collect()
}

/// The name of a pod's first (non-init) container, for a default log target.
fn first_container(pod: &DynamicObject) -> Option<String> {
    pod.data
        .pointer("/spec/containers/0/name")
        .and_then(Value::as_str)
        .map(String::from)
}
