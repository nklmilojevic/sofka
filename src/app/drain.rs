use super::*;
use anyhow::{Context, bail};
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DrainOptions {
    pub ignore_daemonsets: bool,
    pub force: bool,
    pub delete_emptydir: bool,
    pub disable_eviction: bool,
    pub grace_period: Option<u32>,
    pub timeout: Duration,
}

impl Default for DrainOptions {
    fn default() -> Self {
        Self {
            ignore_daemonsets: true,
            force: false,
            delete_emptydir: false,
            disable_eviction: false,
            grace_period: None,
            timeout: Duration::ZERO,
        }
    }
}

#[derive(Default)]
pub struct DrainState {
    pub targets: Vec<String>,
    pub field: usize,
    pub scroll: u16,
    pub focus_field: bool,
    pub grace: String,
    pub timeout: String,
    pub error: String,
    pub message: String,
    pub done: bool,
    pub err: bool,
    pub(super) options: DrainOptions,
    pub(super) claim: Option<StatusClaim>,
    pub(super) cancel: Option<oneshot::Sender<()>>,
}

impl DrainState {
    pub fn started(&self) -> bool {
        self.claim.is_some()
    }

    pub fn rows(&self) -> Vec<String> {
        let mark = |v| if v { "[x]" } else { "[ ]" };
        vec![
            format!("{} Ignore DaemonSets", mark(self.options.ignore_daemonsets)),
            format!(
                "{} Force: permit pods with no live controller",
                mark(self.options.force)
            ),
            format!(
                "{} Delete emptyDir data: lose local data",
                mark(self.options.delete_emptydir)
            ),
            format!(
                "{} Disable eviction: bypass PodDisruptionBudgets",
                mark(self.options.disable_eviction)
            ),
            format!(
                "Grace period: {}",
                if self.grace.is_empty() {
                    "pod default"
                } else {
                    &self.grace
                }
            ),
            format!(
                "Timeout: {}",
                if self.timeout.is_empty() {
                    "0 (unlimited)"
                } else {
                    &self.timeout
                }
            ),
        ]
    }

    fn parse(&self) -> Result<DrainOptions> {
        let mut options = self.options.clone();
        options.grace_period =
            if self.grace.is_empty() {
                None
            } else {
                Some(self.grace.parse().context(
                    "Grace period must be whole seconds >= 0, or empty for the pod default",
                )?)
            };
        options.timeout = parse_timeout(&self.timeout)
            .context("Timeout must be 0 or a duration such as 30s, 5m, or 1h30m")?;
        if tokio::time::Instant::now()
            .checked_add(options.timeout)
            .is_none()
        {
            bail!("Timeout is too large");
        }
        Ok(options)
    }
}

fn parse_timeout(input: &str) -> Option<Duration> {
    if input.is_empty() || input == "0" {
        return Some(Duration::ZERO);
    }
    let mut rest = input;
    let mut total = 0u64;
    while !rest.is_empty() {
        let end = rest.find(|c: char| !c.is_ascii_digit())?;
        let number = rest[..end].parse::<u64>().ok()?;
        let unit = rest.as_bytes()[end];
        let factor = match unit {
            b's' => 1,
            b'm' => 60,
            b'h' => 3600,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(factor)?)?;
        rest = &rest[end + 1..];
    }
    Some(Duration::from_secs(total))
}

impl App {
    pub fn drain_confirmation(&self) -> bool {
        matches!(self.confirm_action, Some(ConfirmAction::Drain { .. }))
            && self.mode == Mode::Confirm
            || matches!(&self.prompt_kind, Some(PromptKind::GuardConfirm { action, .. }) if matches!(**action, ConfirmAction::Drain { .. }))
                && self.mode == Mode::Prompt
    }

    pub(super) fn request_drain(&mut self) {
        if self.deny_readonly() {
            return;
        }
        if self.kind_plural != "nodes" {
            self.flash_warn("drain applies to nodes");
            return;
        }
        let mut targets = self.node_action_targets();
        targets.sort();
        if targets.is_empty() {
            return;
        }
        let pairs = targets
            .iter()
            .map(|n| (n.clone(), String::new()))
            .collect::<Vec<_>>();
        if self
            .guard("drain", "nodes", &pairs, ConfirmLevel::Plain)
            .is_none()
        {
            return;
        }
        self.drain = DrainState {
            targets,
            focus_field: true,
            ..Default::default()
        };
        self.mode = Mode::Drain;
    }

    pub(super) fn key_drain(&mut self, key: KeyInput) {
        if self.drain.started() {
            if matches!(key.action, Some(Action::Back | Action::Quit)) {
                if self.drain.done {
                    self.mode = Mode::Table;
                } else if let Some(cancel) = self.drain.cancel.take() {
                    let _ = cancel.send(());
                    self.drain.message =
                        "Cancel requested. Accepted requests cannot be reversed.".into();
                }
            } else if self.drain.done && key.action == Some(Action::Accept) {
                self.mode = Mode::Table;
            }
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Back | Action::Quit), _) => self.mode = Mode::Table,
            (Some(Action::Down), _) => {
                self.drain.field = (self.drain.field + 1) % 6;
                self.drain.focus_field = true;
            }
            (Some(Action::Up), _) => {
                self.drain.field = (self.drain.field + 5) % 6;
                self.drain.focus_field = true;
            }
            (Some(Action::Toggle), _) => match self.drain.field {
                0 => self.drain.options.ignore_daemonsets ^= true,
                1 => self.drain.options.force ^= true,
                2 => self.drain.options.delete_emptydir ^= true,
                3 => self.drain.options.disable_eviction ^= true,
                _ => {}
            },
            (Some(Action::Accept), _) => self.confirm_drain(),
            _ if self.drain.field >= 4 => {
                let text = if self.drain.field == 4 {
                    &mut self.drain.grace
                } else {
                    &mut self.drain.timeout
                };
                match (key.action, key.code) {
                    (Some(Action::ClearLine | Action::DeleteWord), _) => text.clear(),
                    (Some(Action::Backspace), _) => {
                        text.pop();
                    }
                    (_, KeyCode::Char(c))
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && text.len() < 32 =>
                    {
                        text.push(c)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn confirm_drain(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let options = match self.drain.parse() {
            Ok(options) => options,
            Err(e) => {
                self.drain.error = e.to_string();
                self.drain.scroll = 0;
                self.drain.focus_field = false;
                return;
            }
        };
        let targets = self.drain.targets.clone();
        let pairs = targets
            .iter()
            .map(|n| (n.clone(), String::new()))
            .collect::<Vec<_>>();
        let Some(level) = self.guard("drain", "nodes", &pairs, ConfirmLevel::Plain) else {
            return;
        };
        let hint = if targets.len() == 1 {
            targets[0].clone()
        } else {
            targets.len().to_string()
        };
        self.drain.error.clear();
        self.drain.scroll = 0;
        self.begin_guarded(
            ConfirmAction::Drain { targets, options },
            "Confirm node drain".into(),
            level,
            hint,
        );
        if let Some(PromptKind::GuardConfirm { expected, .. }) = &self.prompt_kind
            && self.mode == Mode::Prompt
        {
            self.prompt_label = format!("Type '{expected}' to confirm:");
        }
    }

    pub(super) fn do_drain_nodes(&mut self, targets: Vec<String>, options: DrainOptions) {
        if self.deny_readonly() {
            return;
        }
        self.note_action("drain", targets.join(", "));
        let claim = self.claim_status("draining nodes...");
        let (cancel, receiver) = oneshot::channel();
        self.drain.claim = Some(claim);
        self.drain.cancel = Some(cancel);
        self.drain.message = "Starting drain...".into();
        self.drain.scroll = 0;
        self.mode = Mode::Drain;
        let worker = DrainWorker {
            client: self.cluster.client.clone(),
            kinds: self
                .cluster
                .catalog
                .iter()
                .filter_map(|name| self.cluster.resolve(name))
                .collect(),
            options,
            targets,
            tx: self.tx.clone(),
            claim,
        };
        tokio::spawn(worker.run(receiver));
    }
}

struct DrainWorker {
    client: Client,
    kinds: Vec<Kind>,
    options: DrainOptions,
    targets: Vec<String>,
    tx: Sender<Msg>,
    claim: StatusClaim,
}

#[derive(Default)]
struct DrainReport {
    completed: Vec<String>,
    current: Option<String>,
    cordoned: Vec<String>,
    cordon_pending: bool,
}

impl DrainWorker {
    async fn send(&self, message: String, done: bool, err: bool) {
        let _ = self
            .tx
            .send(Msg::Drain {
                claim: self.claim,
                message,
                done,
                err,
            })
            .await;
    }

    async fn run(self, mut cancel: oneshot::Receiver<()>) {
        let mut report = DrainReport::default();
        let deadline = async {
            if self.options.timeout.is_zero() {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep(self.options.timeout).await;
            }
        };
        let result = tokio::select! {
            biased;
            _ = &mut cancel => Err(anyhow::anyhow!("canceled; accepted requests cannot be reversed")),
            _ = deadline => Err(anyhow::anyhow!("timed out")),
            result = self.process(&mut report) => result,
        };
        let list = |items: &[String]| {
            if items.is_empty() {
                "none".into()
            } else {
                items.join(", ")
            }
        };
        let started = report.completed.len() + usize::from(report.current.is_some());
        let status = match &result {
            Ok(()) => "drained".into(),
            Err(e) => format!("drain stopped: {e:#}"),
        };
        let mut message = format!(
            "{status}\nCompleted: {}\nIncomplete: {}\nUnstarted: {}\nRemain cordoned: {}",
            list(&report.completed),
            report.current.as_deref().unwrap_or("none"),
            list(&self.targets[started..]),
            list(&report.cordoned)
        );
        if report.cordon_pending {
            message.push_str(
                "\nThe last cordon request may have succeeded. Check the incomplete node.",
            );
        }
        self.send(message, true, result.is_err()).await;
    }

    async fn process(&self, report: &mut DrainReport) -> Result<()> {
        let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(self.client.clone());
        for node in &self.targets {
            report.current = Some(node.clone());
            report.cordon_pending = true;
            self.send(format!("Node: {node}\nCordoning node..."), false, false)
                .await;
            let cordon = api_call(nodes.patch(
                node,
                &PatchParams::default(),
                &Patch::Merge(node_unschedulable_patch(true)),
            ))
            .await;
            if cordon.as_ref().is_err_and(|e| {
                e.downcast_ref::<kube::Error>().is_some_and(
                    |e| matches!(e, kube::Error::Api(e) if (400..500).contains(&e.code)),
                )
            }) {
                report.cordon_pending = false;
            }
            cordon.context("cordon failed")?;
            report.cordon_pending = false;
            report.cordoned.push(node.clone());
            self.drain_node(node).await?;
            report.completed.push(node.clone());
            report.current = None;
        }
        Ok(())
    }

    async fn pod_list(&self, node: &str) -> Result<Vec<Pod>> {
        let pods: Api<Pod> = Api::all(self.client.clone());
        let mut params = ListParams::default()
            .fields(&format!("spec.nodeName={node}"))
            .limit(500);
        let mut result = Vec::new();
        loop {
            let page = api_call(pods.list(&params))
                .await
                .context("list pods failed")?;
            result.extend(page.items);
            match page.metadata.continue_.filter(|s| !s.is_empty()) {
                Some(token) => params = params.continue_token(&token),
                None => return Ok(result),
            }
        }
    }

    async fn validate(&self, pods: &[Pod], accepted: &HashSet<String>) -> Result<()> {
        let mut checked = HashSet::new();
        for pod in pods.iter().filter(|p| eligible(p, false)) {
            let label = pod_label(pod);
            if daemonset(pod) {
                if !self.options.ignore_daemonsets {
                    bail!(
                        "{label}: DaemonSet pod blocks drain; enable Ignore DaemonSets to leave it running"
                    );
                }
                continue;
            }
            if pod.metadata.uid.as_deref().is_none_or(str::is_empty) {
                bail!("{label}: pod UID is missing; cannot verify the target");
            }
            if pod.metadata.name.as_deref().is_none_or(str::is_empty) {
                bail!("{label}: pod name is missing");
            }
            if pod.metadata.deletion_timestamp.is_some()
                || pod
                    .metadata
                    .uid
                    .as_ref()
                    .is_some_and(|uid| accepted.contains(uid))
            {
                continue;
            }
            if !self.options.delete_emptydir
                && pod
                    .spec
                    .as_ref()
                    .and_then(|s| s.volumes.as_ref())
                    .is_some_and(|vs| vs.iter().any(|v| v.empty_dir.is_some()))
            {
                bail!("{label}: pod has emptyDir data that removal would delete");
            }
            if !self.options.force {
                let Some(owner) = pod
                    .metadata
                    .owner_references
                    .as_ref()
                    .and_then(|refs| refs.iter().find(|r| r.controller == Some(true)))
                else {
                    bail!("{label}: pod has no controller and will not be replaced");
                };
                let identity = (
                    pod.metadata.namespace.clone(),
                    owner.api_version.clone(),
                    owner.kind.clone(),
                    owner.name.clone(),
                    owner.uid.clone(),
                );
                if !checked.insert(identity) {
                    continue;
                }
                let Some(kind) = self
                    .kinds
                    .iter()
                    .find(|k| k.ar.api_version == owner.api_version && k.ar.kind == owner.kind)
                else {
                    bail!(
                        "{label}: cannot resolve controller {}/{}",
                        owner.api_version,
                        owner.kind
                    );
                };
                let api = kind.api(
                    self.client.clone(),
                    pod.metadata.namespace.as_deref().unwrap_or("default"),
                );
                let controller = api_call(api.get_opt(&owner.name))
                    .await
                    .with_context(|| format!("{label}: cannot check controller"))?;
                if controller.as_ref().and_then(|o| o.metadata.uid.as_deref())
                    != Some(owner.uid.as_str())
                {
                    bail!(
                        "{label}: controller is missing or has been replaced; replacement is not assured"
                    );
                }
            }
        }
        Ok(())
    }

    async fn drain_node(&self, node: &str) -> Result<()> {
        let mut accepted = HashSet::new();
        let mut backoff = Duration::from_secs(1);
        loop {
            let pods = self.pod_list(node).await?;
            self.validate(&pods, &accepted).await?;
            let remaining: Vec<_> = pods.iter().filter(|p| eligible(p, true)).collect();
            if remaining.is_empty() {
                return Ok(());
            }
            self.send(
                format!(
                    "Node: {node}\nRemaining pods: {}\nWaiting for pod termination or eviction...",
                    remaining.len()
                ),
                false,
                false,
            )
            .await;
            let mut blocked = Vec::new();
            for pod in &remaining {
                let uid = pod.metadata.uid.as_deref().unwrap_or_default();
                if pod.metadata.deletion_timestamp.is_some() || accepted.contains(uid) {
                    continue;
                }
                match self.remove(pod).await {
                    Ok(()) => {
                        accepted.insert(uid.to_owned());
                    }
                    Err(e) if retryable(&e) => blocked.push(format!("{}: {e}", pod_label(pod))),
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("{}: pod removal failed", pod_label(pod)));
                    }
                }
            }
            if !blocked.is_empty() {
                self.send(
                    format!(
                        "Node: {node}\nRemaining pods: {}\nRetry in {}s: {}",
                        remaining.len(),
                        backoff.as_secs(),
                        blocked.join("; ")
                    ),
                    false,
                    false,
                )
                .await;
            }
            tokio::time::sleep(backoff).await;
            backoff = if blocked.is_empty() {
                Duration::from_secs(1)
            } else {
                (backoff * 2).min(Duration::from_secs(10))
            };
        }
    }

    async fn remove(&self, pod: &Pod) -> Result<()> {
        let name = pod
            .metadata
            .name
            .as_deref()
            .context("pod name is missing")?;
        let api: Api<Pod> = Api::namespaced(
            self.client.clone(),
            pod.metadata.namespace.as_deref().unwrap_or("default"),
        );
        let mut delete_options = json!({"preconditions": {"uid": pod.metadata.uid}});
        if let Some(seconds) = self.options.grace_period {
            delete_options["gracePeriodSeconds"] = json!(seconds);
        }
        let result = if self.options.disable_eviction {
            let params = DeleteParams {
                grace_period_seconds: self.options.grace_period,
                preconditions: Some(kube::api::Preconditions {
                    uid: pod.metadata.uid.clone(),
                    resource_version: None,
                }),
                ..Default::default()
            };
            api_call(api.delete(name, &params)).await.map(|_| ())
        } else {
            let eviction = json!({"apiVersion":"policy/v1", "kind":"Eviction", "metadata":{"name":name,"namespace":pod.metadata.namespace}, "deleteOptions":delete_options});
            api_call(api.create_subresource::<_, kube::core::Status>(
                "eviction",
                name,
                &PostParams::default(),
                &eviction,
            ))
            .await
            .map(|_| ())
        };
        match result {
            Err(e) if e.downcast_ref::<kube::Error>().is_some_and(|e| matches!(e, kube::Error::Api(e) if e.code == 404 && e.details.as_ref().is_some_and(|d| d.kind == "pods" && d.name == name))) => Ok(()),
            other => other,
        }
    }
}

async fn api_call<T>(
    future: impl std::future::Future<Output = Result<T, kube::Error>>,
) -> Result<T> {
    Ok(tokio::time::timeout(Duration::from_secs(30), future).await??)
}

fn retryable(error: &anyhow::Error) -> bool {
    error.is::<tokio::time::error::Elapsed>()
        || error
            .downcast_ref::<kube::Error>()
            .is_some_and(|e| match e {
                kube::Error::Api(e) => matches!(e.code, 408 | 429 | 500 | 502 | 503 | 504),
                kube::Error::Service(_) | kube::Error::HyperError(_) => true,
                _ => false,
            })
}

fn daemonset(pod: &Pod) -> bool {
    pod.metadata
        .owner_references
        .as_ref()
        .is_some_and(|refs| refs.iter().any(|r| r.kind == "DaemonSet"))
}

fn eligible(pod: &Pod, skip_daemonsets: bool) -> bool {
    if skip_daemonsets {
        return drainable_pod(pod);
    }
    !pod.metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
        && !matches!(
            pod.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        )
        && !(skip_daemonsets && daemonset(pod))
}

fn pod_label(pod: &Pod) -> String {
    format!(
        "{}/{}",
        pod.metadata.namespace.as_deref().unwrap_or("default"),
        pod.metadata.name.as_deref().unwrap_or("<unknown>")
    )
}
