use super::*;

// ----- free helpers ------------------------------------------------------

pub(super) fn restart_patch(restarted_at: &str) -> Value {
    json!({
        "spec": { "template": { "metadata": { "annotations": {
            "kubectl.kubernetes.io/restartedAt": restarted_at
        }}}}
    })
}

pub(super) fn set_image_patch(plural: &str, container: &str, image: &str) -> Value {
    let containers = json!([{ "name": container, "image": image }]);
    if plural == "pods" {
        json!({ "spec": { "containers": containers } })
    } else {
        json!({ "spec": { "template": { "spec": { "containers": containers } } } })
    }
}

pub(super) fn scale_patch(replicas: i32) -> Value {
    json!({ "spec": { "replicas": replicas } })
}

pub(super) fn suspend_patch(suspend: bool) -> Value {
    json!({ "spec": { "suspend": suspend } })
}

pub(super) fn reconcile_patch(requested_at: &str) -> Value {
    json!({
        "metadata": { "annotations": { "reconcile.fluxcd.io/requestedAt": requested_at } }
    })
}

/// Annotation key for stashing an Application's `automated` block on suspend.
const ARGOCD_AUTOMATED_STASH: &str = "sofka.io/argocd-automated";

/// Annotation key for stashing an ApplicationSet's `applicationsSync` mode.
const ARGOCD_APPSYNC_STASH: &str = "sofka.io/argocd-applications-sync";

/// ArgoCD Application suspend/resume patch, built from the live object.
///
/// **Suspend** base64-encodes the current `spec.syncPolicy.automated` object
/// into an annotation, then removes the field — so `prune`, `selfHeal`, and
/// `allowEmpty` survive the round-trip. **Resume** decodes the annotation and
/// restores the original object, then removes the annotation. When the
/// annotation is absent (the Application was suspended by someone else), resume
/// falls back to an empty `automated: {}`.
pub(super) fn argocd_suspend_patch(obj: &DynamicObject, suspend: bool) -> Value {
    use base64::Engine;
    if suspend {
        let stash = obj.data.pointer("/spec/syncPolicy/automated").map(|v| {
            base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_string(v).unwrap_or_default().as_bytes())
        });
        let mut patch = json!({"spec": {"syncPolicy": {"automated": null}}});
        if let Some(s) = stash {
            patch["metadata"]["annotations"][ARGOCD_AUTOMATED_STASH] = json!(s);
        }
        patch
    } else {
        let annotation = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(ARGOCD_AUTOMATED_STASH));
        match annotation {
            Some(s) => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                let mut patch =
                    json!({"metadata": {"annotations": {ARGOCD_AUTOMATED_STASH: null}}});
                patch["spec"]["syncPolicy"]["automated"] = decoded.unwrap_or_else(|| json!({}));
                patch
            }
            None => json!({"spec": {"syncPolicy": {"automated": {}}}}),
        }
    }
}

/// ArgoCD ApplicationSet suspend/resume patch, built from the live object.
///
/// ApplicationSet has no `automated` field — it uses `spec.syncPolicy.applicationsSync`
/// (a string: `sync`, `create-only`, `create-update`, `create-delete`). There is
/// no `none`/`disabled` mode, so suspend sets it to `create-only` (closest to
/// suspended — stops updates/deletes of existing child Applications). The
/// original value is base64-stashed into an annotation so resume restores it
/// exactly. When the annotation is absent, resume falls back to `"sync"`.
pub(super) fn argocd_appset_suspend_patch(obj: &DynamicObject, suspend: bool) -> Value {
    use base64::Engine;
    const SUSPEND_MODE: &str = "create-only";
    if suspend {
        let current = obj
            .data
            .pointer("/spec/syncPolicy/applicationsSync")
            .and_then(Value::as_str)
            .unwrap_or("sync");
        let stash = base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_string(current)
                .unwrap_or_default()
                .as_bytes(),
        );
        json!({
            "spec": {"syncPolicy": {"applicationsSync": SUSPEND_MODE}},
            "metadata": {"annotations": {ARGOCD_APPSYNC_STASH: stash}}
        })
    } else {
        let annotation = obj
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(ARGOCD_APPSYNC_STASH));
        match annotation {
            Some(s) => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| v.as_str().map(String::from));
                let restored = decoded.unwrap_or_else(|| "sync".into());
                json!({
                    "spec": {"syncPolicy": {"applicationsSync": restored}},
                    "metadata": {"annotations": {ARGOCD_APPSYNC_STASH: null}}
                })
            }
            None => json!({"spec": {"syncPolicy": {"applicationsSync": "sync"}}}),
        }
    }
}

/// ArgoCD sync patch. Setting the top-level `operation.sync` field triggers a
/// manual sync — the same mechanism the ArgoCD API server's `SyncApplication`
/// endpoint uses. The controller fills in the revision from `spec.source`.
pub(super) fn argocd_sync_patch() -> Value {
    json!({ "operation": { "sync": {} } })
}

pub(super) fn external_secret_refresh_patch(force_sync: &str) -> Value {
    json!({
        "metadata": { "annotations": { "force-sync": force_sync } }
    })
}

pub(super) fn node_unschedulable_patch(unschedulable: bool) -> Value {
    json!({ "spec": { "unschedulable": unschedulable } })
}

/// A Job manifest that runs `cj`'s jobTemplate immediately — what `kubectl
/// create job --from=cronjob/…` builds: the template's spec and labels, its
/// annotations plus `cronjob.kubernetes.io/instantiate: manual`, and a
/// non-controller owner reference back to the CronJob. `None` when `cj` has
/// no jobTemplate spec (not actually a CronJob).
pub(super) fn cronjob_manual_job(cj: &DynamicObject, suffix: &str) -> Option<Value> {
    let name = cj.metadata.name.clone()?;
    let spec = cj.data.pointer("/spec/jobTemplate/spec")?.clone();
    // Re-key non-object annotations (invalid, but cluster data is untrusted
    // and `Value`'s IndexMut panics on a type mismatch).
    let mut annotations = cj
        .data
        .pointer("/spec/jobTemplate/metadata/annotations")
        .filter(|a| a.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    annotations["cronjob.kubernetes.io/instantiate"] = json!("manual");
    let mut metadata = json!({
        "name": manual_job_name(&name, suffix),
        "annotations": annotations,
    });
    if let Some(ns) = &cj.metadata.namespace {
        metadata["namespace"] = json!(ns);
    }
    if let Some(labels) = cj.data.pointer("/spec/jobTemplate/metadata/labels") {
        metadata["labels"] = labels.clone();
    }
    if let Some(uid) = &cj.metadata.uid {
        metadata["ownerReferences"] = json!([{
            "apiVersion": "batch/v1",
            "kind": "CronJob",
            "name": name,
            "uid": uid,
        }]);
    }
    Some(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": metadata,
        "spec": spec,
    }))
}

/// `<cronjob>-manual-<suffix>`, with the CronJob name truncated so the Job
/// name stays well under the 63-char label-value limit its pods inherit
/// (k9s truncates at 42 for the same reason).
pub(super) fn manual_job_name(cronjob: &str, suffix: &str) -> String {
    let base: String = cronjob.chars().take(42).collect();
    format!("{base}-manual-{suffix}")
}

/// Readline-style line edits shared by every text input (command palette,
/// filters, prompts, pickers). These are what terminals send for the macOS
/// editing chords: cmd+delete arrives as ctrl-u (kill line) and
/// option+delete as alt-backspace or ctrl-w (kill word). Returns whether the
/// key was handled, so callers run their post-edit refresh and skip the
/// plain-key arms.
pub(super) fn edit_chord(key: &KeyEvent, buf: &mut String) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('u') if ctrl => buf.clear(),
        KeyCode::Char('w') if ctrl => pop_word(buf),
        KeyCode::Backspace if alt || ctrl => pop_word(buf),
        _ => return false,
    }
    true
}

/// Delete the trailing word: trailing whitespace first, then the run of
/// non-whitespace before it (readline's unix-word-rubout).
fn pop_word(buf: &mut String) {
    while buf.chars().next_back().is_some_and(char::is_whitespace) {
        buf.pop();
    }
    while buf.chars().next_back().is_some_and(|c| !c.is_whitespace()) {
        buf.pop();
    }
}

/// A compact journal label for a set of node names.
pub(super) fn node_targets_label(targets: &[String]) -> String {
    match targets {
        [] => "—".into(),
        [one] => one.clone(),
        many => format!("{} nodes", many.len()),
    }
}

/// What manages `obj`, if anything: its Flux owner (from the toolkit labels)
/// preferred, else its controller/owner reference. Used to warn that a delete
/// will be recreated.
pub(super) fn managed_by(obj: &DynamicObject) -> Option<String> {
    if let Some(f) = flux_managed_by(obj) {
        return Some(f);
    }
    let owners = obj.metadata.owner_references.as_ref()?;
    let owner = owners
        .iter()
        .find(|o| o.controller == Some(true))
        .or_else(|| owners.first())?;
    Some(format!("{}/{}", owner.kind, owner.name))
}

/// The Flux Kustomization/HelmRelease managing `obj`, from its toolkit labels.
/// Used to warn that an edit will be reverted on the next reconcile.
pub(super) fn flux_managed_by(obj: &DynamicObject) -> Option<String> {
    crate::gitops::owner_ref(obj).map(|r| format!("Flux {}/{}", r.kind, r.name))
}

pub(super) fn delete_confirm_label(
    kind_plural: &str,
    targets: &[(String, String)],
    force: bool,
    cascade: Cascade,
    managed: Option<&str>,
) -> String {
    let verb = if force { "Force delete" } else { "Delete" };
    // Background is the kubectl default, so only surface the unusual modes.
    let suffix = match cascade {
        Cascade::Background => "",
        Cascade::Foreground => " (cascade: foreground)",
        Cascade::Orphan => " (orphan dependents)",
    };
    // A managed target gets recreated straight after deletion — say so.
    let managed = managed.map(|m| format!("  {m}")).unwrap_or_default();
    if targets.len() == 1 {
        let (name, ns) = &targets[0];
        let where_ns = if ns.is_empty() {
            String::new()
        } else {
            format!(" in {ns}")
        };
        format!(
            "{verb} {} {name}{where_ns}{suffix}?{managed}",
            trim_s(kind_plural)
        )
    } else {
        format!("{verb} {} {}{suffix}?{managed}", targets.len(), kind_plural)
    }
}

pub(super) fn drainable_pod(pod: &Pod) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
    if pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
    {
        return false;
    }
    if pod
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner.kind.eq_ignore_ascii_case("DaemonSet"))
        })
    {
        return false;
    }
    !matches!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

pub(super) fn eviction_unsupported(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(api_err) if matches!(api_err.code, 404 | 405))
}

/// Pick a version name to query a CRD's custom resources: the storage version
/// if flagged, else the first served version, else the first listed.
pub(super) fn crd_served_version(d: &Value) -> Option<String> {
    let versions = d.pointer("/spec/versions")?.as_array()?;
    let pick = versions
        .iter()
        .find(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
        .or_else(|| {
            versions
                .iter()
                .find(|v| v.get("served").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| versions.first())?;
    pick.get("name").and_then(Value::as_str).map(String::from)
}

/// Build a `k=v,k2=v2` selector string from `spec/<field>` (matchLabels for
/// workloads, selector map for services).
pub(super) fn label_selector(obj: &DynamicObject, field: &str) -> Option<String> {
    let path = if field == "matchLabels" {
        vec!["spec", "selector", "matchLabels"]
    } else {
        vec!["spec", "selector"]
    };
    let mut cur = &obj.data;
    for p in path {
        cur = cur.get(p)?;
    }
    let map = cur.as_object()?;
    if map.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = map
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|vs| format!("{k}={vs}")))
        .collect();
    parts.sort();
    Some(parts.join(","))
}

pub(super) fn container_names(obj: &DynamicObject) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["containers", "initContainers", "ephemeralContainers"] {
        if let Some(arr) = obj
            .data
            .pointer(&format!("/spec/{key}"))
            .and_then(Value::as_array)
        {
            for c in arr {
                if let Some(n) = c.get("name").and_then(Value::as_str) {
                    names.push(n.to_string());
                }
            }
        }
    }
    names
}

/// Merge a drill-down selector with a filter selector into one comma-joined
/// Kubernetes selector (`None` when neither is set).
pub(super) fn join_selectors(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a},{b}")),
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (None, None) => None,
    }
}

/// Normalize a user-typed namespace argument: `all`, `*`, and `<all>` mean
/// "all namespaces" (the empty string internally).
pub(super) fn normalize_ns(ns: &str) -> String {
    let t = ns.trim();
    if t == "all" || t == "*" || t == "<all>" {
        String::new()
    } else {
        t.to_string()
    }
}

/// Trim a trailing plural "s" for breadcrumb labels (deployments -> deployment).
pub(super) fn trim_s(plural: &str) -> &str {
    plural.strip_suffix('s').unwrap_or(plural)
}

pub(super) fn xray_pool_plurals(root_kind: &str) -> &'static [&'static str] {
    match root_kind {
        "pod" => &[],
        "cronjob" => &["jobs", "pods"],
        "job" | "daemonset" | "replicaset" | "statefulset" => &["pods"],
        "deployment" => &["replicasets", "pods"],
        _ => &["replicasets", "pods"],
    }
}

impl App {
    /// Whether the current kind supports the Flux suspend/resume menu (`t`).
    pub fn flux_suspendable(&self) -> bool {
        FLUX_SUSPENDABLE_KINDS.contains(&self.kind_plural.as_str())
    }

    /// Whether the current kind is an ArgoCD CRD (Application or
    /// ApplicationSet, group `argoproj.io`). The plurals are generic, so the
    /// group is checked too — only the real ArgoCD CRDs get the `t` menu.
    pub fn argocd_kind(&self) -> bool {
        matches!(
            self.kind_plural.as_str(),
            "applications" | "applicationsets"
        ) && self
            .kind
            .as_ref()
            .is_some_and(|k| k.ar.group == ARGOCD_GROUP)
    }

    /// Whether the current kind is an ArgoCD Application (not ApplicationSet).
    /// Gates "Sync now", which patches the `operation` field — ApplicationSet
    /// has no such field.
    pub fn argocd_app_kind(&self) -> bool {
        self.kind_plural == "applications"
            && self
                .kind
                .as_ref()
                .is_some_and(|k| k.ar.group == ARGOCD_GROUP)
    }

    /// Whether the current kind is CronJobs, which get their own `t` menu
    /// (trigger/suspend/resume).
    pub fn cronjob_kind(&self) -> bool {
        self.kind_plural == "cronjobs"
    }

    /// The items shown in the `t` action menu for the current kind.
    pub fn action_menu_items(&self) -> &'static [&'static str] {
        if self.cronjob_kind() {
            CRONJOB_MENU_ITEMS
        } else if self.argocd_app_kind() {
            ARGOCD_MENU_ITEMS
        } else if self.argocd_kind() {
            ARGOCD_APPSET_MENU_ITEMS
        } else {
            FLUX_MENU_ITEMS
        }
    }

    /// Whether the current kind is an External Secrets resource that honours
    /// the force-sync annotation (`r`).
    pub fn external_secret_kind(&self) -> bool {
        EXTERNAL_SECRET_KINDS.contains(&self.kind_plural.as_str())
    }
}

/// Move a list selection one step, clamped to `[0, len)`. Shared by every
/// modal picker (namespaces, contexts, containers, set-image, xray).
pub(super) fn list_step(state: &mut ListState, len: usize, down: bool) {
    if len == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0);
    let next = if down {
        (i + 1).min(len - 1)
    } else {
        i.saturating_sub(1)
    };
    state.select(Some(next));
}

/// Copy text to the system clipboard via the first available OS tool, falling
/// back to OSC 52 for remote terminals where local clipboard tools are absent.
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // tool not installed — try the next one
        };
        // Write must finish (and the pipe close) before we wait, or the child
        // can block; report success only if the write and the process succeed.
        let wrote = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        if wrote && ok {
            return true;
        }
    }
    copy_to_clipboard_osc52(text)
}

pub(super) fn copy_to_clipboard_osc52(text: &str) -> bool {
    use std::fs::OpenOptions;
    use std::io::{Write, stdout};

    let sequence = osc52_sequence(text);
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        return tty
            .write_all(sequence.as_bytes())
            .and_then(|_| tty.flush())
            .is_ok();
    }

    let mut out = stdout();
    out.write_all(sequence.as_bytes())
        .and_then(|_| out.flush())
        .is_ok()
}

pub(super) fn osc52_sequence(text: &str) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// What a log stream shares with the view that opened it.
pub(super) struct LogStreamCtx {
    pub(super) generation: u64,
    pub(super) flag: Arc<AtomicU64>,
    /// Held only across a connection attempt: an aggregate view opens many
    /// streams, and letting every one of them dial at once is a burst of API
    /// load separate from how many end up staying open. Deliberately released
    /// while waiting for a container to start, so a pod stuck in `Pending`
    /// cannot hold a slot the other streams need. `None` for a single-pod
    /// view, which has nothing to queue behind.
    pub(super) setup: Option<Arc<tokio::sync::Semaphore>>,
}

#[derive(Clone)]
pub(super) struct LogRun {
    pub(super) queue: LogQueue,
    pub(super) timestamps: bool,
}

pub(super) async fn forward_log_stream(
    api: Api<Pod>,
    pod: String,
    lp: LogParams,
    prefix: String,
    queue: LogQueue,
    ctx: LogStreamCtx,
) {
    use futures_util::{AsyncBufReadExt, TryStreamExt};
    use tokio::time::MissedTickBehavior;

    let LogStreamCtx {
        generation,
        flag,
        setup,
    } = ctx;

    let stream = loop {
        if flag.load(Ordering::SeqCst) != generation || queue.is_closed() {
            return;
        }
        let attempt = {
            let _permit = match &setup {
                Some(sem) => match sem.acquire().await {
                    Ok(permit) => Some(permit),
                    Err(_) => return, // semaphore closed: the view is gone
                },
                None => None,
            };
            api.log_stream(&pod, &lp).await
        };
        match attempt {
            Ok(stream) => break stream,
            Err(kube::Error::Api(e))
                if lp.follow
                    && !lp.previous
                    && e.code == 400
                    && e.message.contains("is waiting to start") =>
            {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = queue.closed() => return,
                }
            }
            Err(e) => {
                let _ = queue.send(vec![format!("[error] {e}")]).await;
                return;
            }
        }
    };

    let mut lines = stream.lines();
    let Some(mut batch) = queue.batch().await else {
        return;
    };
    let mut flush = tokio::time::interval(Duration::from_millis(LOG_BATCH_MS));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if flag.load(Ordering::SeqCst) != generation {
            break;
        }

        tokio::select! {
            next = lines.try_next() => {
                match next {
                    Ok(Some(line)) => {
                        batch.push(&prefix, line);
                        if batch.is_full() && !batch.flush_and_renew(&queue).await {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        batch.push_raw(format!("[error] {e}"));
                        break;
                    }
                }
            }
            _ = flush.tick(), if !batch.is_empty() => {
                if !batch.flush_and_renew(&queue).await {
                    break;
                }
            }
        }
    }

    if flag.load(Ordering::SeqCst) == generation {
        let _ = batch.flush(&queue).await;
    }
}

pub(super) async fn send_log_batch(queue: &LogQueue, batch: &mut Vec<String>) -> bool {
    if batch.is_empty() {
        return true;
    }
    queue.send(std::mem::take(batch)).await
}

/// Sender shared by every producer in one logs view. A semaphore reservation
/// travels inside each queued message, so channel depth can no longer multiply
/// large batches into an unaccounted memory spike.
#[derive(Clone)]
pub(super) struct LogQueue {
    tx: Sender<Msg>,
    generation: u64,
    line_bytes: usize,
    batch_bytes: usize,
    batch_reservation: usize,
    byte_budget: Option<Arc<tokio::sync::Semaphore>>,
}

impl LogQueue {
    pub(super) fn new(
        tx: Sender<Msg>,
        generation: u64,
        line_bytes: usize,
        queue_bytes: usize,
    ) -> Self {
        let budget_bytes = queue_bytes.min(u32::MAX as usize);
        let line_bytes = match (line_bytes, budget_bytes) {
            (0, 0) => 0,
            (0, queue) => queue,
            (line, 0) => line,
            (line, queue) => line.min(queue),
        };
        let batch_bytes = if budget_bytes == 0 {
            LOG_BATCH_BYTES
        } else {
            LOG_BATCH_BYTES.min(budget_bytes)
        };
        let batch_reservation = batch_bytes
            .saturating_add(line_bytes)
            .saturating_add(256)
            .min(budget_bytes)
            .max(usize::from(budget_bytes > 0));
        Self {
            tx,
            generation,
            line_bytes,
            batch_bytes,
            batch_reservation,
            byte_budget: (budget_bytes > 0)
                .then(|| Arc::new(tokio::sync::Semaphore::new(budget_bytes))),
        }
    }

    pub(super) async fn batch(&self) -> Option<LogBatch> {
        let permit = match &self.byte_budget {
            Some(budget) => Some(
                Arc::clone(budget)
                    .acquire_many_owned(self.batch_reservation as u32)
                    .await
                    .ok()?,
            ),
            None => None,
        };
        Some(LogBatch::with_permit(
            self.line_bytes,
            self.batch_bytes,
            permit,
        ))
    }

    pub(super) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub(super) async fn closed(&self) {
        self.tx.closed().await;
    }

    /// Send any number of input lines as bounded batches.
    pub(super) async fn send(&self, lines: Vec<String>) -> bool {
        let Some(mut batch) = self.batch().await else {
            return false;
        };
        let mut lines = lines.into_iter().peekable();
        while let Some(line) = lines.next() {
            batch.push_raw(line);
            if batch.is_full() {
                if !batch.flush(self).await {
                    return false;
                }
                if lines.peek().is_some() {
                    let Some(next) = self.batch().await else {
                        return false;
                    };
                    batch = next;
                }
            }
        }
        batch.flush(self).await
    }

    async fn send_batch(
        &self,
        lines: Vec<String>,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> bool {
        self.tx
            .send(Msg::LogLines {
                generation: self.generation,
                lines: crate::store::QueuedLogLines::new(lines, permit),
            })
            .await
            .is_ok()
    }
}

/// Drive `lines` through the ingest batching, flushing whenever a batch fills
/// — the allocation shape of the stream loop, without the stream. Returns the
/// batch count so the work cannot be optimized away.
#[cfg(feature = "bench")]
pub(crate) fn ingest_lines(prefix: &str, lines: impl IntoIterator<Item = String>) -> usize {
    let mut batch = LogBatch::new(0, LOG_BATCH_BYTES);
    let mut batches = 0usize;
    for line in lines {
        batch.push(prefix, line);
        if batch.is_full() {
            std::hint::black_box(batch.take());
            batches += 1;
        }
    }
    std::hint::black_box(batch.take());
    batches + 1
}

/// The pending-line buffer both log ingest paths (kubelet streams and provider
/// tails) fill between flushes. Owning the mechanics in one place keeps the
/// prefixing and the hand-off consistent across the two.
pub(super) struct LogBatch {
    lines: Vec<String>,
    bytes: usize,
    line_bytes: usize,
    batch_bytes: usize,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl LogBatch {
    #[cfg(any(test, feature = "bench"))]
    pub(super) fn new(line_bytes: usize, batch_bytes: usize) -> Self {
        Self::with_permit(line_bytes, batch_bytes, None)
    }

    fn with_permit(
        line_bytes: usize,
        batch_bytes: usize,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        Self {
            lines: Vec::with_capacity(LOG_BATCH_LINES),
            bytes: 0,
            line_bytes,
            batch_bytes,
            permit,
        }
    }

    /// Add one stream line under `prefix` (empty for a single-pod stream).
    pub(super) fn push(&mut self, prefix: &str, line: String) {
        // A single-pod stream has nothing to prepend, which is the common
        // case: move the line the reader already allocated rather than
        // copying it into an identical new one.
        if prefix.is_empty() {
            self.push_raw(line);
            return;
        }
        let mut prefixed = String::with_capacity(prefix.len() + line.len());
        prefixed.push_str(prefix);
        prefixed.push_str(&line);
        self.push_raw(prefixed);
    }

    /// Add one line that is already complete (an error notice, a provider
    /// record that carried its own prefix).
    pub(super) fn push_raw(&mut self, line: String) {
        let line = bound_log_line(line, self.line_bytes);
        self.bytes += line.len();
        self.lines.push(line);
    }

    /// Append one provider record's display lines straight into the batch.
    pub(super) fn render_entry(
        &mut self,
        entry: &crate::providers::LogEntry,
        prefix: crate::providers::Prefix,
        timestamps: bool,
    ) {
        let omitted = entry.render_while(prefix, timestamps, |line| {
            if self.is_full() {
                return false;
            }
            self.push_raw(line);
            true
        });
        if omitted > 0 {
            self.push_raw(format!(
                "[truncated] {omitted} lines omitted from oversized provider record"
            ));
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether this batch has reached either limit and should be handed off.
    pub(super) fn is_full(&self) -> bool {
        self.lines.len() >= LOG_BATCH_LINES || self.bytes >= self.batch_bytes
    }

    /// Hand the pending lines off, leaving an empty batch behind. The
    /// replacement starts at the batch size, so a steady stream does not
    /// re-grow the same `Vec` from nothing between every flush.
    pub(super) fn take(&mut self) -> Vec<String> {
        self.bytes = 0;
        std::mem::replace(&mut self.lines, Vec::with_capacity(LOG_BATCH_LINES))
    }

    /// Send the pending lines. `false` once the UI is gone.
    pub(super) async fn flush(&mut self, queue: &LogQueue) -> bool {
        if self.lines.is_empty() {
            return true;
        }
        let lines = self.take();
        let permit = self.permit.take();
        queue.send_batch(lines, permit).await
    }

    /// Flush this reservation into the channel and obtain another before
    /// reading more source data. The same semaphore therefore accounts for
    /// both in-flight producer batches and queued messages.
    pub(super) async fn flush_and_renew(&mut self, queue: &LogQueue) -> bool {
        if !self.flush(queue).await {
            return false;
        }
        let Some(next) = queue.batch().await else {
            return false;
        };
        *self = next;
        true
    }
}

/// Normalize and cap a line before it can occupy a producer batch or channel
/// slot. The UI repeats this defensively for synthetic/test messages.
pub(super) fn bound_log_line(line: String, cap: usize) -> String {
    let line = clean_log_line(line);
    match cap {
        0 => line,
        cap if line.len() > cap => truncate_log_line(line, cap),
        _ => line,
    }
}

fn clean_log_line(line: String) -> String {
    if !line.contains('\r') && !line.contains('\t') {
        return line;
    }
    line.chars()
        .filter_map(|c| match c {
            '\r' => None,
            '\t' => Some(' '),
            c => Some(c),
        })
        .collect()
}

#[cold]
#[inline(never)]
fn truncate_log_line(mut line: String, cap: usize) -> String {
    let mut end = cap;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = line.len() - end;
    line.truncate(end);
    line.push_str(&format!("…[{dropped} bytes truncated]"));
    line
}

/// The events document, maintained incrementally.
///
/// A watch delivers one event at a time, and the whole document is republished
/// each time it changes. Re-deriving every row from its `DynamicObject` on each
/// publish made that quadratic over an initial list, so each row is rendered
/// once when its event arrives and a publish only re-sorts the rendered rows.
pub(crate) struct EventDoc {
    /// Row key → (sort key, rendered row).
    rows: crate::store::FastMap<String, (String, String)>,
    events_v1: bool,
}

impl EventDoc {
    pub(crate) fn new(events_v1: bool) -> Self {
        Self {
            rows: crate::store::FastMap::default(),
            events_v1,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.rows.clear();
    }

    pub(crate) fn apply(&mut self, event: &DynamicObject) {
        let seen = event_time(event, self.events_v1);
        let line = event_line(event, self.events_v1, &seen);
        self.rows.insert(row_key(event), (seen, line));
    }

    pub(crate) fn remove(&mut self, event: &DynamicObject) {
        self.rows.remove(&row_key(event));
    }

    /// The document as the view shows it: header, then rows newest first.
    pub(crate) fn render(&self) -> Vec<String> {
        let mut rows: Vec<&(String, String)> = self.rows.values().collect();
        // Unstable: the comparator orders on both tuple fields, i.e. the whole
        // element, so a tie means the two rows are indistinguishable.
        rows.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let mut lines = Vec::with_capacity(rows.len().max(1) + 1);
        lines.push(EVENT_HEADER.to_string());
        if rows.is_empty() {
            lines.push("(no events)".into());
        } else {
            lines.extend(rows.into_iter().map(|(_, line)| line.clone()));
        }
        lines
    }

    /// Publish the document. `false` once the UI is gone.
    pub(super) async fn publish(&self, tx: &Sender<Msg>, generation: u64, title: &str) -> bool {
        tx.send(Msg::Events {
            generation,
            title: title.to_string(),
            lines: self.render(),
        })
        .await
        .is_ok()
    }
}

const EVENT_HEADER: &str = concat!(
    "LAST SEEN            ",
    "TYPE     ",
    "REASON                   ",
    "COUNT ",
    "MESSAGE"
);

/// One-shot render of a fixed set of events (the diagnostic bundle), through
/// the same accumulator the live view uses.
pub(crate) fn format_event_lines<'a, I>(events: I, events_v1: bool) -> Vec<String>
where
    I: IntoIterator<Item = &'a DynamicObject>,
{
    let mut doc = EventDoc::new(events_v1);
    for event in events {
        doc.apply(event);
    }
    doc.render()
}

pub(super) fn event_line(event: &DynamicObject, events_v1: bool, seen: &str) -> String {
    let typ = svalue(&event.data, &["type"]).unwrap_or_default();
    let reason = svalue(&event.data, &["reason"]).unwrap_or_default();
    let count = event_count(event, events_v1);
    let message = if events_v1 {
        svalue(&event.data, &["note"])
            .or_else(|| svalue(&event.data, &["message"]))
            .unwrap_or_default()
    } else {
        svalue(&event.data, &["message"])
            .or_else(|| svalue(&event.data, &["note"]))
            .unwrap_or_default()
    };
    format!(
        "{:<20} {:<8} {:<24} {:>5} {}",
        compact_event_time(seen),
        typ,
        reason,
        count,
        message.replace('\n', " ")
    )
}

pub(super) fn event_count(event: &DynamicObject, events_v1: bool) -> i64 {
    if events_v1 {
        ivalue(&event.data, &["series", "count"])
            .or_else(|| ivalue(&event.data, &["deprecatedCount"]))
            .unwrap_or(1)
    } else {
        ivalue(&event.data, &["count"]).unwrap_or(1)
    }
}

pub(super) fn event_time(event: &DynamicObject, events_v1: bool) -> String {
    let data = &event.data;
    let value = if events_v1 {
        svalue(data, &["series", "lastObservedTime"])
            .or_else(|| svalue(data, &["eventTime"]))
            .or_else(|| svalue(data, &["deprecatedLastTimestamp"]))
            .or_else(|| svalue(data, &["deprecatedFirstTimestamp"]))
    } else {
        svalue(data, &["lastTimestamp"])
            .or_else(|| svalue(data, &["eventTime"]))
            .or_else(|| svalue(data, &["firstTimestamp"]))
    };
    value
        .or_else(|| {
            event
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0.to_string())
        })
        .unwrap_or_default()
}

pub(super) fn compact_event_time(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('Z');
    if let Some((date, time)) = trimmed.split_once('T') {
        let time = time.split('.').next().unwrap_or(time);
        format!("{date} {time}")
    } else {
        raw.to_string()
    }
}

pub(super) fn svalue(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(String::from)
}

pub(super) fn ivalue(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_i64()
}

/// Recursively flatten an object and its owned children into xray rows.
/// Deepest owner chain the tree walks. Kubernetes ownership is shallow —
/// cronjob → job → pod is the longest built-in chain — so anything beyond this
/// is a cycle in the owner references, and the walk stops there instead of
/// recursing until the stack runs out.
const XRAY_MAX_DEPTH: usize = 8;

/// Which pool entries each owner uid claims, by position. Positions, not
/// objects: the pool outlives the walk, so the tree is built from borrows
/// instead of one deep clone per owner reference.
type OwnerIndex<'a> = crate::store::FastMap<&'a str, Vec<u32>>;

/// Build the owner index over `pool` and flatten the whole tree — the CPU half
/// of one xray refresh, split out from the polling task so it can be tested and
/// benchmarked without a cluster.
pub(crate) fn xray_flatten(
    root_kind: &str,
    roots: &[DynamicObject],
    pool: &[(String, DynamicObject)],
) -> Vec<XrayItem> {
    let mut children: OwnerIndex<'_> = OwnerIndex::default();
    for (i, (_, o)) in pool.iter().enumerate() {
        if let Some(owners) = &o.metadata.owner_references {
            for owner in owners {
                children
                    .entry(owner.uid.as_str())
                    .or_default()
                    .push(i as u32);
            }
        }
    }
    // Each root contributes its own row and a populated pool contributes most
    // of the rest, so this is the right order of magnitude on the first push.
    let mut items = Vec::with_capacity(roots.len() + pool.len());
    for root in roots {
        emit_xray(root_kind, root, 0, pool, &children, &mut items);
    }
    items
}

pub(super) fn emit_xray(
    kind: &str,
    obj: &DynamicObject,
    depth: usize,
    pool: &[(String, DynamicObject)],
    children: &OwnerIndex<'_>,
    items: &mut Vec<XrayItem>,
) {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let ns = obj.metadata.namespace.clone().unwrap_or_default();
    items.push(XrayItem {
        depth,
        kind: kind.to_string(),
        name: name.clone(),
        ns: ns.clone(),
        status: xray_status(kind, obj),
        container: None,
    });

    if depth < XRAY_MAX_DEPTH
        && let Some(uid) = &obj.metadata.uid
        && let Some(kids) = children.get(uid.as_str())
    {
        for &i in kids {
            let (clabel, cobj) = &pool[i as usize];
            emit_xray(clabel, cobj, depth + 1, pool, children, items);
        }
    }

    // Pods expand into their containers as leaves.
    if kind == "pod" {
        for c in container_names(obj) {
            items.push(XrayItem {
                depth: depth + 1,
                kind: "container".into(),
                name: name.clone(),
                ns: ns.clone(),
                status: String::new(),
                container: Some(c),
            });
        }
    }
}

pub(super) fn xray_status(kind: &str, o: &DynamicObject) -> String {
    match kind {
        "pod" => phase(o),
        "job" => format!(
            "{}/{}",
            o.data
                .pointer("/status/succeeded")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/spec/completions")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1),
        ),
        "cronjob" => format!(
            "active {}",
            o.data
                .pointer("/status/active")
                .and_then(Value::as_array)
                .map_or(0, |items| items.len()),
        ),
        "deployment" | "replicaset" | "statefulset" => format!(
            "{}/{}",
            o.data
                .pointer("/status/readyReplicas")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/spec/replicas")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        "daemonset" => format!(
            "{}/{}",
            o.data
                .pointer("/status/numberReady")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            o.data
                .pointer("/status/desiredNumberScheduled")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ),
        _ => String::new(),
    }
}

/// List all objects of a kind (namespaced to `ns` when applicable).
pub(super) async fn list_kind(
    client: &Client,
    ar: &ApiResource,
    namespaced: bool,
    ns: &str,
) -> Result<Vec<DynamicObject>, String> {
    let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
        Api::namespaced_with(client.clone(), ns, ar)
    } else {
        Api::all_with(client.clone(), ar)
    };
    api.list(&ListParams::default())
        .await
        .map(|l| l.items)
        .map_err(|e| format!("listing {}: {e}", ar.plural))
}

/// [`list_kind`], degrading a failure to an empty list while recording why in
/// `warn` (first error wins). For read paths that aggregate several kinds:
/// a denied or failed list must surface as "couldn't look", never render as a
/// confident zero.
pub(super) async fn list_or_warn(
    client: &Client,
    ar: &ApiResource,
    namespaced: bool,
    ns: &str,
    warn: &mut Option<String>,
) -> Vec<DynamicObject> {
    match list_kind(client, ar, namespaced, ns).await {
        Ok(items) => items,
        Err(e) => {
            warn.get_or_insert(e);
            Vec::new()
        }
    }
}

/// How many dashboard lists may be in flight at once. The dashboards exist to
/// describe cluster health, so they fan out enough to stop paying for each
/// round-trip in series without themselves becoming a burst of API load.
const DASHBOARD_LIST_CONCURRENCY: usize = 4;

/// Whether two discovered kinds are the same resource.
pub(super) fn same_api_resource(a: &ApiResource, b: &ApiResource) -> bool {
    a.group == b.group && a.version == b.version && a.kind == b.kind
}

/// Split the xray pool into the kinds that duplicate the root kind and the
/// kinds that need a list of their own.
///
/// A kind can be both (a replicaset root is listed with replicasets in its
/// pool). The duplicates come back as bare labels because their objects are
/// already in the root list — asking the API server for the same inventory a
/// second time is the part worth avoiding.
pub(super) fn split_pool_kinds(
    pool: Vec<(String, ApiResource, bool)>,
    root: &ApiResource,
) -> (Vec<String>, Vec<(String, ApiResource, bool)>) {
    let mut aliases = Vec::new();
    let mut rest = Vec::new();
    for kind in pool {
        if same_api_resource(&kind.1, root) {
            aliases.push(kind.0);
        } else {
            rest.push(kind);
        }
    }
    (aliases, rest)
}

/// List every resolved kind with bounded concurrency, in the order given.
/// Unresolved kinds yield an empty list, exactly as skipping them did.
pub(super) async fn gather_lists<const N: usize>(
    client: &Client,
    kinds: &[Option<(ApiResource, bool)>; N],
    ns: &str,
) -> [(Vec<DynamicObject>, Option<String>); N] {
    let mut out = gather_list_vec(client, kinds, ns).await.into_iter();
    std::array::from_fn(|_| out.next().expect("one result per kind"))
}

/// [`gather_lists`] for a run-time-sized request list.
pub(super) async fn gather_list_vec(
    client: &Client,
    kinds: &[Option<(ApiResource, bool)>],
    ns: &str,
) -> Vec<(Vec<DynamicObject>, Option<String>)> {
    let pending: Vec<_> = kinds
        .iter()
        .map(|kind| {
            let (kind, client, ns) = (kind.clone(), client.clone(), ns.to_string());
            async move {
                let Some((ar, namespaced)) = kind else {
                    return (Vec::new(), None);
                };
                let mut warn = None;
                let items = list_or_warn(&client, &ar, namespaced, &ns, &mut warn).await;
                (items, warn)
            }
        })
        .collect();
    // `buffered` yields in input order, so this is the order of `kinds`.
    futures_util::stream::iter(pending)
        .buffered(DASHBOARD_LIST_CONCURRENCY)
        .collect()
        .await
}

/// Prepend an "evidence incomplete" warning to a findings list when one of
/// the gather reads failed — the analysis below it saw only partial data.
pub(super) fn prepend_warn_finding(
    findings: &mut Vec<crate::explain::Finding>,
    warn: Option<String>,
) {
    if let Some(w) = warn {
        findings.insert(
            0,
            crate::explain::Finding {
                indent: 0,
                level: crate::explain::Level::Warn,
                text: format!("evidence incomplete — {w}"),
                target: None,
            },
        );
    }
}

pub(super) fn phase(o: &DynamicObject) -> String {
    o.data
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub(super) fn node_ready(o: &DynamicObject) -> bool {
    o.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_str) == Some("True")
            })
        })
        .unwrap_or(false)
}

/// True when the two integer pointers are equal and non-zero (e.g. ready == desired).
pub(super) fn ready_eq(o: &DynamicObject, ready_ptr: &str, want_ptr: &str) -> bool {
    let r = o
        .data
        .pointer(ready_ptr)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let w = o
        .data
        .pointer(want_ptr)
        .and_then(Value::as_i64)
        .unwrap_or(0);
    w > 0 && r >= w
}

/// Extract (cpu millicores, memory bytes) from a metrics-API object.
pub(super) fn usage_of(obj: &DynamicObject, is_node: bool) -> (i64, i64) {
    use crate::columns::{parse_cpu_milli, parse_mem_bytes};
    if is_node {
        let cpu = obj
            .data
            .pointer("/usage/cpu")
            .and_then(Value::as_str)
            .map(parse_cpu_milli)
            .unwrap_or(0);
        let mem = obj
            .data
            .pointer("/usage/memory")
            .and_then(Value::as_str)
            .map(parse_mem_bytes)
            .unwrap_or(0);
        (cpu, mem)
    } else {
        let mut cpu = 0;
        let mut mem = 0;
        if let Some(cs) = obj.data.pointer("/containers").and_then(Value::as_array) {
            for c in cs {
                if let Some(s) = c.pointer("/usage/cpu").and_then(Value::as_str) {
                    cpu += parse_cpu_milli(s);
                }
                if let Some(s) = c.pointer("/usage/memory").and_then(Value::as_str) {
                    mem += parse_mem_bytes(s);
                }
            }
        }
        (cpu, mem)
    }
}

/// Extract each container's (CPU millicores, memory bytes) from a PodMetrics
/// object. Malformed or missing quantities degrade to zero through the shared
/// quantity parsers, matching the existing pod-total behavior.
pub(super) fn container_usage_of(obj: &DynamicObject) -> Vec<(String, (i64, i64))> {
    use crate::columns::{parse_cpu_milli, parse_mem_bytes};
    obj.data
        .pointer("/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|container| {
            let name = container.get("name")?.as_str()?.to_string();
            let cpu = container
                .pointer("/usage/cpu")
                .and_then(Value::as_str)
                .map(parse_cpu_milli)
                .unwrap_or(0);
            let memory = container
                .pointer("/usage/memory")
                .and_then(Value::as_str)
                .map(parse_mem_bytes)
                .unwrap_or(0);
            Some((name, (cpu, memory)))
        })
        .collect()
}

/// Extract each container's declared CPU/memory requests and limits from a Pod
/// spec, covering regular, init, and ephemeral containers so the map matches
/// [`container_names`]. Missing quantities stay `None` to keep "unset" distinct
/// from a real zero.
pub(super) fn container_resources_of(
    obj: &DynamicObject,
) -> Vec<(String, crate::columns::ContainerResources)> {
    let mut out = Vec::new();
    for key in ["containers", "initContainers", "ephemeralContainers"] {
        let Some(arr) = obj
            .data
            .pointer(&format!("/spec/{key}"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for c in arr {
            if let Some(name) = c.get("name").and_then(Value::as_str) {
                out.push((name.to_string(), single_container_resources(c)));
            }
        }
    }
    out
}

/// Kubernetes QoS class for a pod. Prefers the authoritative
/// `status.qosClass` set by the API server; when absent (e.g. a not-yet-
/// scheduled pod), derives it from regular-container requests and limits.
/// Returns an empty string only when there is no pod spec to reason about.
pub(super) fn qos_class(obj: &DynamicObject) -> String {
    if let Some(q) = obj
        .data
        .pointer("/status/qosClass")
        .and_then(Value::as_str)
        .filter(|q| !q.is_empty())
    {
        return q.to_string();
    }

    let Some(containers) = obj
        .data
        .pointer("/spec/containers")
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    if containers.is_empty() {
        return String::new();
    }

    let mut any_set = false;
    let mut guaranteed = true;
    for c in containers {
        use crate::columns::ContainerResources;
        let ContainerResources {
            cpu_request,
            cpu_limit,
            mem_request,
            mem_limit,
        } = single_container_resources(c);
        if cpu_request.is_some()
            || cpu_limit.is_some()
            || mem_request.is_some()
            || mem_limit.is_some()
        {
            any_set = true;
        }
        // Guaranteed requires every resource to have request == limit > 0.
        let matched = |req: Option<i64>, lim: Option<i64>| matches!((req, lim), (Some(r), Some(l)) if r == l && r > 0);
        if !(matched(cpu_request, cpu_limit) && matched(mem_request, mem_limit)) {
            guaranteed = false;
        }
    }

    if !any_set {
        "BestEffort".into()
    } else if guaranteed {
        "Guaranteed".into()
    } else {
        "Burstable".into()
    }
}

/// Parse one container's `resources` block. Shared by [`qos_class`]'s fallback
/// computation.
fn single_container_resources(c: &Value) -> crate::columns::ContainerResources {
    use crate::columns::{ContainerResources, parse_cpu_milli, parse_mem_bytes};
    let q = |section: &str, resource: &str, parse: fn(&str) -> i64| {
        c.pointer(&format!("/resources/{section}/{resource}"))
            .and_then(Value::as_str)
            .map(parse)
    };
    ContainerResources {
        cpu_request: q("requests", "cpu", parse_cpu_milli),
        cpu_limit: q("limits", "cpu", parse_cpu_milli),
        mem_request: q("requests", "memory", parse_mem_bytes),
        mem_limit: q("limits", "memory", parse_mem_bytes),
    }
}
