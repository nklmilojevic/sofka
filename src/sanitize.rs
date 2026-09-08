//! The `:sanitize` core plugin: delete the pods a namespace has finished with.
//!
//! Runs as `sofka --plugin-adapter sanitize`, spawned by the plugin runner like
//! any other package. It speaks the same request/report protocol over stdin and
//! stdout, so guardrails, read-only mode, confirmation, and the report view all
//! apply unchanged — the only difference from an external package is that the
//! adapter ships inside the binary instead of needing a runtime on PATH.

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ApiResource, DeleteParams, ListParams, Preconditions};
use kube::core::DynamicObject;
use kube::{Client, Config};
use serde_json::{Value, json};

/// Statuses that mean the pod is finished and will not come back on its own.
const TERMINAL: &[&str] = &[
    "Succeeded",
    "Failed",
    "Error",
    "OOMKilled",
    "ContainerStatusUnknown",
];
/// Terminal, plus the pods that are wedged and will not recover unaided.
const STUCK: &[&str] = &["CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull"];
/// Everything above, plus pods that have not started. This is the k9s set;
/// `Pending` covers a pod that is merely waiting for a node, so it is opt-in.
const UNSTARTED: &[&str] = &["Pending"];

/// Rows per report table. The runner caps captured output at 1 MiB.
const MAX_ROWS: usize = 500;
/// Pods fetched per list request. Sanitizing all namespaces on a large cluster
/// is otherwise one unpaged response holding the entire pod inventory.
const PAGE: u32 = 500;

fn wanted(states: &str) -> Option<Vec<&'static str>> {
    let mut set = TERMINAL.to_vec();
    match states {
        "terminal" => {}
        "stuck" => set.extend_from_slice(STUCK),
        "all" => {
            set.extend_from_slice(STUCK);
            set.extend_from_slice(UNSTARTED);
        }
        _ => return None,
    }
    set.sort_unstable();
    Some(set)
}

/// Whether an application container is still running.
///
/// The STATUS column reports the reason of the last container that terminated,
/// so a multi-container pod reads `OOMKilled` while a sibling still serves. That
/// is right for a column and wrong for a delete. Readiness is deliberately not
/// part of this: a container failing its readiness probe, or still inside its
/// startup probe, is out of the load balancer and very much alive.
///
/// Restartable init containers (native sidecars) are deliberately not consulted.
/// They are infrastructure for the workload rather than the workload, and
/// counting them would make an identical crash-looping pod exempt from
/// `states = stuck` purely because something injected a proxy into it. The
/// cleanup rules keep this distinction even when table status becomes more detailed.
fn running(pod: &DynamicObject) -> bool {
    pod.data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .is_some_and(|cs| cs.iter().any(|c| c.pointer("/state/running").is_some()))
}

/// Cleanup uses application state and pod phase, not display-only reason labels.
/// Init failures and numeric exit labels must not widen the deletion categories.
fn matches_states(pod: &DynamicObject, wanted: &[&str]) -> bool {
    if pod.metadata.deletion_timestamp.is_some() || running(pod) {
        return false;
    }
    let mut waiting = None;
    let mut terminated = None;
    if let Some(statuses) = pod
        .data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
    {
        for status in statuses {
            if let Some(reason) = status
                .pointer("/state/waiting/reason")
                .and_then(Value::as_str)
                && (reason != "ContainerCreating" || waiting.is_none())
            {
                waiting = Some(reason);
            }
            if let Some(reason) = status
                .pointer("/state/terminated/reason")
                .and_then(Value::as_str)
                && reason != "Completed"
            {
                terminated = Some(reason);
            }
        }
    }
    let category = waiting
        .or(terminated)
        .or_else(|| pod.data.pointer("/status/phase").and_then(Value::as_str))
        .unwrap_or("Unknown");
    wanted.contains(&category)
}

struct Target {
    namespace: String,
    name: String,
    uid: String,
    status: String,
}

/// Read the request, sanitize, and write the report. Errors here are execution
/// errors: the runner shows them and no report is produced.
pub async fn run(allow_v1_client_cert: bool) -> Result<()> {
    let request: Value = serde_json::from_reader(std::io::stdin().lock())
        .context("reading the plugin request from stdin")?;
    if request.get("schema_version").and_then(Value::as_u64) != Some(1) {
        anyhow::bail!("unsupported request schema_version");
    }
    let client = client_for(
        request.get("context").and_then(Value::as_str),
        request.get("kubeconfig").and_then(Value::as_str),
        allow_v1_client_cert,
    )
    .await?;
    let report = sanitize(client, &request, &guardrails_for(&request)).await?;
    serde_json::to_writer(std::io::stdout().lock(), &report).context("writing the report")
}

/// The `[[guardrails]]` that apply to this run. The adapter is sofka, so it
/// reads the same configuration the session did rather than being told.
fn guardrails_for(request: &Value) -> Vec<crate::config::Guardrail> {
    let context = guardrail_context(request);
    let cluster = request
        .get("cluster")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (loader, _) = crate::config::ConfigLoader::load();
    loader.resolve(context, cluster).config.guardrails
}

fn guardrail_context(request: &Value) -> &str {
    // Cluster::connect uses this name for guardrails without a named context.
    // Keep the request context null so client_for still uses Config::infer.
    request
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or("default")
}

async fn sanitize(
    client: Client,
    request: &Value,
    guardrails: &[crate::config::Guardrail],
) -> Result<Value> {
    let inputs = request.get("inputs").cloned().unwrap_or_else(|| json!({}));
    let states = inputs
        .get("states")
        .and_then(Value::as_str)
        .unwrap_or("terminal");
    let wanted = wanted(states).with_context(|| format!("unknown states value '{states}'"))?;
    let dry_run = inputs.get("dry_run").and_then(Value::as_str) == Some("true");

    let namespace = request
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let pods = ApiResource::erase::<Pod>(&());
    let api: Api<DynamicObject> = if namespace.is_empty() {
        Api::all_with(client.clone(), &pods)
    } else {
        Api::namespaced_with(client.clone(), &namespace, &pods)
    };

    let mut params = ListParams::default().limit(PAGE);
    let (labels, fields) = selectors(request.get("filter").and_then(Value::as_str))?;
    if let Some(l) = &labels {
        params = params.labels(l);
    }
    if let Some(f) = &fields {
        params = params.fields(f);
    }

    // Paged, and only the few strings each target needs are kept. Listing every
    // pod in a large cluster in one response and holding the objects is how this
    // shape falls over.
    let mut targets = Vec::new();
    loop {
        let page = api.list(&params).await.context("listing pods")?;
        let next = page.metadata.continue_.clone();
        for pod in page {
            let status = crate::columns::pod_status(&pod);
            if !matches_states(&pod, &wanted) {
                continue;
            }
            targets.push(Target {
                namespace: pod.metadata.namespace.clone().unwrap_or_default(),
                name: pod.metadata.name.clone().unwrap_or_default(),
                uid: pod.metadata.uid.clone().unwrap_or_default(),
                status,
            });
        }
        match next {
            Some(token) if !token.is_empty() => params = params.continue_token(&token),
            _ => break,
        }
    }
    targets.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    let matched = targets.len();
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut changed = 0usize;
    let mut gone = 0usize;

    // The runner guards a context plugin before it knows what will match, so it
    // can only weigh one placeholder target against `max_bulk`. Re-check here,
    // where the real count is known, before anything is deleted.
    let scoped: Vec<(String, String)> = targets
        .iter()
        .map(|t| (t.name.clone(), t.namespace.clone()))
        .collect();
    let limits = crate::app::guardrails::restrictions(
        guardrails,
        guardrail_context(request),
        "plugin:sanitize",
        "pods",
        &scoped,
        namespace.is_empty(),
    );
    let blocked = match limits.max_bulk {
        Some(max) if matched > max => Some(max),
        _ => None,
    };

    if !dry_run && blocked.is_none() {
        for target in &targets {
            let api: Api<DynamicObject> =
                Api::namespaced_with(client.clone(), &target.namespace, &pods);

            // Re-read and re-run the predicate rather than trusting the list. A
            // pod selected while crash-looping can have recovered and gone ready
            // since, and its UID would not have changed, so identity alone does
            // not catch it. Pinning the *scanned* resourceVersion instead would:
            // the statuses that can recover — CrashLoopBackOff, ImagePullBackOff,
            // Error, OOMKilled — are exactly the ones whose resourceVersion churns
            // on every restart, backoff tick and probe, so it would refuse crowds
            // of pods that were never anything but dead. Asking the question we
            // actually mean ignores that noise.
            let fresh = match api.get(&target.name).await {
                Ok(pod) => pod,
                Err(kube::Error::Api(e)) if e.code == 404 => {
                    gone += 1;
                    continue;
                }
                Err(e) => {
                    failed.push(vec![
                        target.namespace.clone(),
                        target.name.clone(),
                        e.to_string(),
                    ]);
                    continue;
                }
            };
            let still_selected = fresh.metadata.uid.as_deref() == Some(target.uid.as_str())
                && matches_states(&fresh, &wanted);
            if !still_selected {
                changed += 1;
                continue;
            }

            // The version comes from the read just made, not from the scan, so
            // the precondition is tight enough to be meaningful and short-lived
            // enough not to be noise. With the UID it covers both a replacement
            // wearing the same name and this object moving on.
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(target.uid.clone()),
                    resource_version: fresh.metadata.resource_version.clone(),
                }),
                ..DeleteParams::default()
            };
            match api.delete(&target.name, &params).await {
                Ok(_) => deleted.push(target),
                // A precondition can still fail in the gap between the read and
                // this call; 404 means someone got there first. Neither is an
                // error, and neither is a pod this run should claim to have
                // deleted.
                Err(kube::Error::Api(e)) if e.code == 409 => changed += 1,
                Err(kube::Error::Api(e)) if e.code == 404 => gone += 1,
                Err(e) => failed.push(vec![
                    target.namespace.clone(),
                    target.name.clone(),
                    e.to_string(),
                ]),
            }
        }
    }

    let scope = if namespace.is_empty() {
        "all namespaces".to_string()
    } else {
        format!("namespace {namespace}")
    };
    let mut summary = vec![
        format!("Scanned {scope}."),
        format!("Matching statuses: {}.", wanted.join(", ")),
    ];
    if let Some(max) = blocked {
        summary.push(format!(
            "{matched} matched, which exceeds the guardrail limit of {max} for \
             plugin:sanitize. Nothing deleted."
        ));
    } else if dry_run {
        summary.push(format!("{matched} matched; nothing deleted (dry run)."));
    } else {
        summary.push(format!(
            "{matched} matched, {} deleted, {} failed.",
            deleted.len(),
            failed.len()
        ));
        if changed > 0 {
            summary.push(format!("{changed} changed since the scan and left alone."));
        }
        if gone > 0 {
            summary.push(format!("{gone} already gone."));
        }
    }

    let mut sections = vec![json!({"title": "Summary", "lines": summary})];
    if !failed.is_empty() {
        sections.push(table("Failed", &["Namespace", "Pod", "Error"], failed));
    }
    let listed: Vec<&Target> = if dry_run || blocked.is_some() {
        targets.iter().collect()
    } else {
        deleted
    };
    let rows: Vec<Vec<String>> = listed
        .iter()
        .map(|t| vec![t.namespace.clone(), t.name.clone(), t.status.clone()])
        .collect();
    if !rows.is_empty() {
        let title = match (blocked.is_some(), dry_run) {
            (true, _) => "Blocked",
            (_, true) => "Would delete",
            _ => "Deleted",
        };
        sections.push(table(title, &["Namespace", "Pod", "Status"], rows));
    }

    Ok(json!({"schema_version": 1, "title": "Sanitize pods", "sections": sections}))
}

fn table(title: &str, columns: &[&str], rows: Vec<Vec<String>>) -> Value {
    let total = rows.len();
    let mut section = json!({
        "title": title,
        "columns": columns,
        "rows": rows.into_iter().take(MAX_ROWS).collect::<Vec<_>>(),
    });
    if total > MAX_ROWS {
        section["lines"] = json!([format!("{} more rows not shown.", total - MAX_ROWS)]);
    }
    section
}

/// The label and field selectors the view filter implies.
///
/// `-l` and `-f` terms are already Kubernetes selectors, so they are sent with
/// the list and narrow the scan exactly. The rest of the grammar — text terms
/// and typed comparisons like `restarts>=5` — is evaluated against rendered
/// table cells inside the app, and this adapter cannot reproduce it. Rather
/// than silently sanitize a wider set than the table is showing, refuse: for a
/// bulk delete, acting on a different scope than the one on screen is the one
/// outcome worth failing over.
fn selectors(filter: Option<&str>) -> Result<(Option<String>, Option<String>)> {
    use crate::filter::ParsedFilter;
    let filter = filter.unwrap_or_default().trim();
    if filter.is_empty() {
        return Ok((None, None));
    }
    match crate::filter::parse(filter) {
        ParsedFilter::Structured(s) if s.terms.is_empty() => Ok((s.labels, s.fields)),
        _ => anyhow::bail!(
            "the view filter '{filter}' narrows the table in ways this command cannot \
             reproduce, and sanitizing the whole namespace instead would delete more \
             than you can see. Clear the filter, or narrow it with -l/-f selectors."
        ),
    }
}

/// Build a client for the request's context. A null context means sofka had no
/// explicit kubeconfig context name, so let the usual inference apply.
/// `kubeconfig` is the file that context lives in when it is not the one
/// kubectl resolves on its own — without it the context would not be found.
async fn client_for(
    context: Option<&str>,
    kubeconfig: Option<&str>,
    allow_v1_client_cert: bool,
) -> Result<Client> {
    let config = match context {
        Some(name) => {
            let options = kube::config::KubeConfigOptions {
                context: Some(name.to_string()),
                cluster: None,
                user: None,
            };
            let kubeconfig = match kubeconfig {
                Some(path) => kube::config::Kubeconfig::read_from(path)
                    .with_context(|| format!("reading {path}"))?,
                None => kube::config::Kubeconfig::read().context("reading kubeconfig")?,
            };
            Config::from_custom_kubeconfig(kubeconfig, &options)
                .await
                .with_context(|| format!("building config for context '{name}'"))?
        }
        None => Config::infer().await.context("loading kubeconfig")?,
    };
    crate::k8s::build_client(config, allow_v1_client_cert).context("building a Kubernetes client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(value: Value) -> DynamicObject {
        serde_json::from_value(value).expect("pod fixture")
    }

    /// A mock apiserver that answers each request in order and records the
    /// method, path and body it saw. Enough to assert what the adapter sent,
    /// which is the part that decides whether a live pod survives.
    async fn mock_api(
        responses: Vec<(&'static str, String)>,
    ) -> (
        Client,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    ) {
        use std::collections::VecDeque;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock apiserver");
        let addr = listener.local_addr().expect("local addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            let mut responses: VecDeque<_> = responses.into();
            while let Some((status, body)) = responses.pop_front() {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = sock.split();
                let mut reader = BufReader::new(r);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut payload = vec![0u8; length];
                if length > 0 {
                    let _ = reader.read_exact(&mut payload).await;
                }
                recorder.lock().unwrap().push((
                    method,
                    path,
                    String::from_utf8_lossy(&payload).into_owned(),
                ));
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = w.write_all(response.as_bytes()).await;
            }
        });
        let mut config = Config::new(format!("http://{addr}").parse().expect("mock URL"));
        config.default_retry = false;
        (Client::try_from(config).expect("mock client"), seen)
    }

    fn pod_json(name: &str, uid: &str, version: &str, state: Value) -> Value {
        json!({
            "metadata": {"name": name, "namespace": "default",
                         "uid": uid, "resourceVersion": version},
            "status": {"phase": "Running", "containerStatuses": [
                {"name": "app", "ready": false, "restartCount": 0, "state": state}]}
        })
    }

    fn list_of(pods: Vec<Value>) -> String {
        json!({"apiVersion": "v1", "kind": "PodList",
               "metadata": {"resourceVersion": "1"}, "items": pods})
        .to_string()
    }

    fn crashing(name: &str, uid: &str, version: &str) -> Value {
        pod_json(
            name,
            uid,
            version,
            json!({"waiting": {"reason": "CrashLoopBackOff"}}),
        )
    }

    /// The same object, recovered: same UID, running and ready again.
    fn recovered(name: &str, uid: &str, version: &str) -> Value {
        let mut pod = pod_json(name, uid, version, json!({"running": {}}));
        pod["status"]["containerStatuses"][0]["ready"] = json!(true);
        pod
    }

    fn failed(name: &str, uid: &str, version: &str) -> Value {
        pod_json(
            name,
            uid,
            version,
            json!({"terminated": {"reason": "Error", "exitCode": 1}}),
        )
    }

    fn request_filtered(states: &str, dry_run: bool, filter: &str) -> Value {
        let mut r = request(states, dry_run);
        r["filter"] = json!(filter);
        r
    }

    fn request(states: &str, dry_run: bool) -> Value {
        json!({"schema_version": 1, "context": null, "cluster": "test",
               "namespace": "default", "resource": "pods", "name": "", "filter": "",
               "inputs": {"states": states, "dry_run": dry_run.to_string()},
               "object": null, "forward": null})
    }

    fn summary_of(report: &Value) -> String {
        report["sections"][0]["lines"]
            .as_array()
            .expect("summary lines")
            .iter()
            .map(|l| l.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[tokio::test]
    async fn delete_pins_the_identity_and_the_version_just_read() {
        let (client, seen) = mock_api(vec![
            ("200 OK", list_of(vec![failed("job-1", "uid-1", "rv-10")])),
            ("200 OK", failed("job-1", "uid-1", "rv-11").to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .expect("report");

        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "list, re-read, delete: {calls:?}");
        assert_eq!(calls[1].0, "GET");
        assert_eq!(calls[2].0, "DELETE");
        assert!(calls[2].1.contains("/pods/job-1"), "{}", calls[2].1);
        let body: Value = serde_json::from_str(&calls[2].2).expect("delete body");
        assert_eq!(body["preconditions"]["uid"], "uid-1");
        assert_eq!(
            body["preconditions"]["resourceVersion"], "rv-11",
            "the precondition must pin the version just verified, not the scan"
        );
        assert!(summary_of(&report).contains("1 deleted"));
    }

    #[tokio::test]
    async fn a_pod_that_recovers_between_list_and_delete_survives() {
        // Selected while crash-looping, ready again by the time its turn came.
        // Same UID throughout, so only re-running the predicate catches it.
        let (client, seen) = mock_api(vec![
            ("200 OK", list_of(vec![crashing("web-0", "uid-1", "rv-10")])),
            ("200 OK", recovered("web-0", "uid-1", "rv-11").to_string()),
        ])
        .await;
        let report = sanitize(client, &request("stuck", false), &[])
            .await
            .expect("report");

        let calls = seen.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "a recovered pod must never reach DELETE: {calls:?}"
        );
        assert!(calls.iter().all(|(method, ..)| method != "DELETE"));
        let summary = summary_of(&report);
        assert!(summary.contains("0 deleted"), "{summary}");
        assert!(summary.contains("1 changed since the scan"), "{summary}");
        assert!(
            !report["sections"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["title"] == "Deleted"),
            "a recovered pod must not be reported as deleted"
        );
    }

    #[tokio::test]
    async fn a_replacement_wearing_the_same_name_survives() {
        // A StatefulSet recreated `db-0`: the name matches, the identity does not.
        let (client, seen) = mock_api(vec![
            ("200 OK", list_of(vec![failed("db-0", "uid-old", "rv-10")])),
            ("200 OK", failed("db-0", "uid-new", "rv-1").to_string()),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .expect("report");
        assert_eq!(seen.lock().unwrap().len(), 2, "must not reach DELETE");
        assert!(summary_of(&report).contains("1 changed since the scan"));
    }

    #[tokio::test]
    async fn churn_that_does_not_change_the_verdict_still_deletes() {
        // The restart count moved and the version bumped, but the pod is just as
        // dead. Pinning the scanned version would have skipped this one.
        let mut churned = failed("job-1", "uid-1", "rv-99");
        churned["status"]["containerStatuses"][0]["restartCount"] = json!(12);
        let (client, seen) = mock_api(vec![
            ("200 OK", list_of(vec![failed("job-1", "uid-1", "rv-10")])),
            ("200 OK", churned.to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .expect("report");
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert!(
            summary_of(&report).contains("1 deleted"),
            "{}",
            summary_of(&report)
        );
    }

    #[tokio::test]
    async fn a_pod_already_gone_is_not_counted_as_deleted() {
        let missing = json!({"kind": "Status", "status": "Failure", "code": 404,
                             "reason": "NotFound", "message": "pods 'job-1' not found"});
        let (client, _) = mock_api(vec![
            ("200 OK", list_of(vec![failed("job-1", "uid-1", "rv-10")])),
            ("404 Not Found", missing.to_string()),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .expect("report");
        let summary = summary_of(&report);
        assert!(
            summary.contains("0 deleted") && summary.contains("1 already gone"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn one_refused_delete_does_not_hide_the_others() {
        let denied = json!({"kind": "Status", "status": "Failure", "code": 403,
                            "reason": "Forbidden", "message": "pods 'job-2' is forbidden"});
        let (client, _) = mock_api(vec![
            (
                "200 OK",
                list_of(vec![
                    failed("job-1", "uid-1", "rv-10"),
                    failed("job-2", "uid-2", "rv-11"),
                ]),
            ),
            ("200 OK", failed("job-1", "uid-1", "rv-10").to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
            ("200 OK", failed("job-2", "uid-2", "rv-11").to_string()),
            ("403 Forbidden", denied.to_string()),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .expect("report");

        let summary = summary_of(&report);
        assert!(summary.contains("1 deleted, 1 failed"), "{summary}");
        let titles: Vec<&str> = report["sections"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["title"].as_str())
            .collect();
        assert!(
            titles.contains(&"Failed") && titles.contains(&"Deleted"),
            "{titles:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_list_is_an_execution_error_not_an_empty_report() {
        let (client, _) = mock_api(vec![(
            "500 Internal Server Error",
            json!({"kind": "Status", "status": "Failure", "code": 500}).to_string(),
        )])
        .await;
        let error = sanitize(client, &request("terminal", false), &[])
            .await
            .expect_err("a broken list must not report success");
        assert!(format!("{error:#}").contains("listing pods"), "{error:#}");
    }

    #[tokio::test]
    async fn dry_run_sends_no_delete_at_all() {
        let (client, seen) = mock_api(vec![(
            "200 OK",
            list_of(vec![failed("job-1", "uid-1", "rv-10")]),
        )])
        .await;
        let report = sanitize(client, &request("terminal", true), &[])
            .await
            .expect("report");

        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "dry run must only list: {calls:?}");
        assert_eq!(calls[0].0, "GET");
        assert!(summary_of(&report).contains("nothing deleted (dry run)"));
        assert_eq!(report["sections"][1]["title"], "Would delete");
    }

    #[tokio::test]
    async fn a_bulk_guardrail_is_measured_against_the_pods_to_delete() {
        // The runner can only weigh one placeholder target for a context
        // plugin, so without this check max_bulk would not restrain anything.
        let guardrail = crate::config::Guardrail {
            actions: vec!["plugin:sanitize".into()],
            max_bulk: Some(1),
            ..Default::default()
        };
        let (client, seen) = mock_api(vec![(
            "200 OK",
            list_of(vec![
                failed("job-1", "uid-1", "rv-10"),
                failed("job-2", "uid-2", "rv-11"),
            ]),
        )])
        .await;
        let report = sanitize(client, &request("terminal", false), &[guardrail])
            .await
            .expect("report");

        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "an over-limit set must not delete anything"
        );
        let summary = summary_of(&report);
        assert!(
            summary.contains("exceeds the guardrail limit of 1"),
            "{summary}"
        );
        assert_eq!(report["sections"][1]["title"], "Blocked");
    }

    #[tokio::test]
    async fn a_context_bulk_limit_applies_without_a_named_context() {
        for context in [Value::Null, json!("default"), json!("production")] {
            let guardrail = crate::config::Guardrail {
                contexts: vec![context.as_str().unwrap_or("default").into()],
                actions: vec!["plugin:sanitize".into()],
                max_bulk: Some(1),
                ..Default::default()
            };
            let (client, seen) = mock_api(vec![(
                "200 OK",
                list_of(vec![
                    failed("job-1", "uid-1", "rv-10"),
                    failed("job-2", "uid-2", "rv-11"),
                ]),
            )])
            .await;
            let mut request = request("terminal", false);
            request["context"] = context;
            let report = sanitize(client, &request, &[guardrail]).await.unwrap();
            assert_eq!(seen.lock().unwrap().len(), 1);
            assert!(summary_of(&report).contains("exceeds the guardrail limit of 1"));
            assert_eq!(report["sections"][1]["title"], "Blocked");
        }
    }

    #[tokio::test]
    async fn a_delete_conflict_or_missing_pod_is_not_counted_as_deleted() {
        for (status, code, expected) in [
            ("409 Conflict", 409, "1 changed since the scan"),
            ("404 Not Found", 404, "1 already gone"),
        ] {
            let (client, seen) = mock_api(vec![
                ("200 OK", list_of(vec![failed("job-1", "uid-1", "rv-10")])),
                ("200 OK", failed("job-1", "uid-1", "rv-11").to_string()),
                (
                    status,
                    json!({"kind": "Status", "status": "Failure", "code": code}).to_string(),
                ),
            ])
            .await;
            let report = sanitize(client, &request("terminal", false), &[])
                .await
                .unwrap();
            let calls = seen.lock().unwrap().clone();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[2].0, "DELETE");
            let summary = summary_of(&report);
            assert!(summary.contains("0 deleted, 0 failed"), "{summary}");
            assert!(summary.contains(expected), "{summary}");
            assert_eq!(report["sections"].as_array().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn a_failed_pre_delete_read_skips_the_pod_and_continues() {
        let (client, seen) = mock_api(vec![
            (
                "200 OK",
                list_of(vec![
                    failed("job-1", "uid-1", "rv-10"),
                    failed("job-2", "uid-2", "rv-11"),
                ]),
            ),
            (
                "403 Forbidden",
                json!({"kind": "Status", "status": "Failure", "code": 403,
                "reason": "Forbidden", "message": "cannot read job-1"})
                .to_string(),
            ),
            ("200 OK", failed("job-2", "uid-2", "rv-11").to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .unwrap();
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 4);
        let deletes: Vec<_> = calls
            .iter()
            .filter(|(method, ..)| method == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(
            deletes[0].1.split('?').next(),
            Some("/api/v1/namespaces/default/pods/job-2")
        );
        assert!(summary_of(&report).contains("1 deleted, 1 failed"));
        assert_eq!(report["sections"][1]["title"], "Failed");
        assert_eq!(report["sections"][1]["rows"][0][1], "job-1");
        assert_eq!(report["sections"][2]["title"], "Deleted");
        assert_eq!(report["sections"][2]["rows"][0][1], "job-2");
    }

    #[tokio::test]
    async fn a_bulk_guardrail_allows_a_set_within_the_limit() {
        let guardrail = crate::config::Guardrail {
            actions: vec!["plugin:sanitize".into()],
            max_bulk: Some(2),
            ..Default::default()
        };
        let (client, seen) = mock_api(vec![
            ("200 OK", list_of(vec![failed("job-1", "uid-1", "rv-10")])),
            ("200 OK", failed("job-1", "uid-1", "rv-10").to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[guardrail])
            .await
            .expect("report");
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert!(summary_of(&report).contains("1 deleted"));
    }

    #[tokio::test]
    async fn the_scan_pages_instead_of_pulling_every_pod_at_once() {
        let mut first = json!({"apiVersion": "v1", "kind": "PodList",
            "metadata": {"resourceVersion": "1", "continue": "token-2"},
            "items": [failed("job-1", "uid-1", "rv-10")]});
        first["items"] = json!([failed("job-1", "uid-1", "rv-10")]);
        let (client, seen) = mock_api(vec![
            ("200 OK", first.to_string()),
            ("200 OK", list_of(vec![failed("job-2", "uid-2", "rv-11")])),
            ("200 OK", failed("job-1", "uid-1", "rv-10").to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
            ("200 OK", failed("job-2", "uid-2", "rv-11").to_string()),
            (
                "200 OK",
                json!({"kind": "Status", "status": "Success"}).to_string(),
            ),
        ])
        .await;
        let report = sanitize(client, &request("terminal", false), &[])
            .await
            .expect("report");

        let calls = seen.lock().unwrap().clone();
        assert!(
            calls[0].1.contains("limit="),
            "the list must be paged: {}",
            calls[0].1
        );
        assert!(
            calls[1].1.contains("continue=token-2"),
            "the second page must follow the continue token: {}",
            calls[1].1
        );
        assert!(summary_of(&report).contains("2 matched, 2 deleted"));
    }

    #[tokio::test]
    async fn a_selector_filter_narrows_the_scan_server_side() {
        let (client, seen) = mock_api(vec![(
            "200 OK",
            list_of(vec![failed("job-1", "uid-1", "rv-10")]),
        )])
        .await;
        sanitize(
            client,
            &request_filtered("terminal", true, "-l app=api"),
            &[],
        )
        .await
        .expect("report");
        let calls = seen.lock().unwrap().clone();
        assert!(
            calls[0].1.contains("labelSelector=app%3Dapi"),
            "the label selector must reach the API: {}",
            calls[0].1
        );
    }

    #[tokio::test]
    async fn a_filter_this_command_cannot_reproduce_refuses_to_run() {
        // Typing `/web-` then `:sanitize` must not quietly sanitize everything.
        let (client, seen) = mock_api(vec![(
            "200 OK",
            list_of(vec![failed("job-1", "uid-1", "rv-10")]),
        )])
        .await;
        let error = sanitize(client, &request_filtered("terminal", false, "web-"), &[])
            .await
            .expect_err("a fuzzy filter must not be silently ignored");
        assert!(format!("{error:#}").contains("cannot"), "{error:#}");
        assert!(seen.lock().unwrap().is_empty(), "nothing should be listed");
    }

    #[test]
    fn selectors_pass_through_only_what_the_api_can_enforce() {
        assert_eq!(selectors(None).unwrap(), (None, None));
        assert_eq!(selectors(Some("   ")).unwrap(), (None, None));
        let (labels, fields) = selectors(Some("-l app=api -f spec.nodeName=n1")).unwrap();
        assert_eq!(labels.as_deref(), Some("app=api"));
        assert_eq!(fields.as_deref(), Some("spec.nodeName=n1"));
        // Anything evaluated against rendered cells is refused, not ignored.
        assert!(selectors(Some("web-")).is_err());
        assert!(selectors(Some("restarts>=5")).is_err());
        assert!(selectors(Some("-l app=api web-")).is_err());
    }

    #[test]
    fn state_sets_widen_in_order() {
        let terminal = wanted("terminal").unwrap();
        let stuck = wanted("stuck").unwrap();
        let all = wanted("all").unwrap();
        assert!(terminal.contains(&"Succeeded") && terminal.contains(&"Failed"));
        assert!(!terminal.contains(&"CrashLoopBackOff"));
        assert!(stuck.contains(&"CrashLoopBackOff") && !stuck.contains(&"Pending"));
        assert!(all.contains(&"Pending"));
        assert!(terminal.len() < stuck.len() && stuck.len() < all.len());
        assert!(wanted("bogus").is_none());
    }

    #[test]
    fn status_matches_the_pods_view() {
        // The sets name what the STATUS column shows, not what k9s would print:
        // a finished pod reads Succeeded here, never Completed.
        let finished = pod(json!({
            "metadata": {"name": "job", "namespace": "default"},
            "status": {"phase": "Succeeded", "containerStatuses": [
                {"name": "c", "ready": false, "restartCount": 0,
                 "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}]}
        }));
        assert_eq!(crate::columns::pod_status(&finished), "Succeeded");
        assert!(wanted("terminal").unwrap().contains(&"Succeeded"));

        let evicted = pod(json!({
            "metadata": {"name": "gone", "namespace": "default"},
            "status": {"phase": "Failed", "reason": "Evicted"}
        }));
        assert_eq!(crate::columns::pod_status(&evicted), "Evicted");
        assert!(matches_states(&evicted, &wanted("terminal").unwrap()));
    }

    #[test]
    fn a_terminating_pod_is_never_a_target() {
        let dying = pod(json!({
            "metadata": {"name": "dying", "namespace": "default",
                         "deletionTimestamp": "2024-01-01T00:00:00Z"},
            "status": {"phase": "Failed", "containerStatuses": [
                {"name": "c", "ready": false, "restartCount": 0,
                 "state": {"terminated": {"reason": "Error", "exitCode": 1}}}]}
        }));
        assert_eq!(crate::columns::pod_status(&dying), "Terminating");
        assert!(!wanted("all").unwrap().contains(&"Terminating"));
    }

    #[test]
    fn a_running_container_protects_the_pod() {
        // One container OOMKilled, one still running: STATUS reads OOMKilled and
        // the set matches, so only the running check keeps the pod alive.
        let mixed = pod(json!({
            "metadata": {"name": "multi", "namespace": "default"},
            "status": {"phase": "Running", "containerStatuses": [
                {"name": "app", "ready": true, "restartCount": 0, "state": {"running": {}}},
                {"name": "batch", "ready": false, "restartCount": 0,
                 "state": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}}]}
        }));
        assert_eq!(crate::columns::pod_status(&mixed), "OOMKilled");
        assert!(wanted("terminal").unwrap().contains(&"OOMKilled"));
        assert!(running(&mixed), "a running sibling must protect the pod");

        // Readiness must not matter: an unready but running container is alive.
        let unready = pod(json!({
            "metadata": {"name": "starting", "namespace": "default"},
            "status": {"phase": "Running", "containerStatuses": [
                {"name": "app", "ready": false, "restartCount": 0, "state": {"running": {}}},
                {"name": "batch", "ready": false, "restartCount": 0,
                 "state": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}}]}
        }));
        assert!(running(&unready));
    }

    #[test]
    fn a_native_sidecar_does_not_exempt_a_dead_workload() {
        // A restartable init container keeps running for the pod's lifetime and
        // lands in initContainerStatuses, not containerStatuses. Counting it as
        // "running" would spare a crash-looping pod that an identical pod
        // without a proxy would not be spared, so it is left out on purpose.
        let sidecar = json!({"name": "proxy", "restartPolicy": "Always"});
        let crashing = pod(json!({
            "metadata": {"name": "wedged", "namespace": "default"},
            "spec": {"initContainers": [sidecar]},
            "status": {"phase": "Running",
                "initContainerStatuses": [
                    {"name": "proxy", "ready": true, "restartCount": 0,
                     "state": {"running": {}}}],
                "containerStatuses": [
                    {"name": "app", "ready": false, "restartCount": 7,
                     "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}
        }));
        assert_eq!(crate::columns::pod_status(&crashing), "CrashLoopBackOff");
        assert!(
            !running(&crashing),
            "a proxy must not exempt a dead workload"
        );

        // The benign case needs no special handling: once the application
        // container finishes cleanly the status is no longer in any set.
        let finished = pod(json!({
            "metadata": {"name": "done", "namespace": "default"},
            "spec": {"initContainers": [sidecar]},
            "status": {"phase": "Running",
                "initContainerStatuses": [
                    {"name": "proxy", "ready": true, "restartCount": 0,
                     "state": {"running": {}}}],
                "containerStatuses": [
                    {"name": "app", "ready": false, "restartCount": 0,
                     "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}]}
        }));
        assert_eq!(crate::columns::pod_status(&finished), "Running");
        assert!(!wanted("all").unwrap().contains(&"Running"));
    }

    #[test]
    fn a_finished_pod_has_nothing_running() {
        let finished = pod(json!({
            "metadata": {"name": "job", "namespace": "default"},
            "status": {"phase": "Succeeded", "containerStatuses": [
                {"name": "c", "ready": false, "restartCount": 0,
                 "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}]}
        }));
        assert!(!running(&finished));

        let crashing = pod(json!({
            "metadata": {"name": "crash", "namespace": "default"},
            "status": {"phase": "Running", "containerStatuses": [
                {"name": "c", "ready": false, "restartCount": 9,
                 "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}
        }));
        assert!(!running(&crashing));
        assert_eq!(crate::columns::pod_status(&crashing), "CrashLoopBackOff");
    }

    #[test]
    fn tables_cap_their_rows() {
        let rows: Vec<Vec<String>> = (0..MAX_ROWS + 20)
            .map(|i| vec!["default".into(), format!("pod-{i}"), "Failed".into()])
            .collect();
        let section = table("Deleted", &["Namespace", "Pod", "Status"], rows);
        assert_eq!(section["rows"].as_array().unwrap().len(), MAX_ROWS);
        assert!(
            section["lines"][0]
                .as_str()
                .unwrap()
                .starts_with("20 more rows")
        );
    }

    #[test]
    fn display_reason_changes_do_not_expand_cleanup_categories() {
        let mut p = pod(json!({"metadata": {"name": "p", "namespace": "default"},
            "status": {"phase": "Failed", "reason": "Evicted"}}));
        assert!(matches_states(&p, &wanted("terminal").unwrap()));
        p.data["status"]["reason"] = json!("DeadlineExceeded");
        assert!(matches_states(&p, &wanted("terminal").unwrap()));
        p.data["status"] = json!({"phase": "Pending", "conditions": [
            {"type": "PodScheduled", "status": "False", "reason": "SchedulingGated"}]});
        assert!(!matches_states(&p, &wanted("terminal").unwrap()));
        assert!(!matches_states(&p, &wanted("stuck").unwrap()));
        assert!(matches_states(&p, &wanted("all").unwrap()));
        p.data["spec"] = json!({"initContainers": [{"name": "init"}]});
        p.data["status"] = json!({"phase": "Pending", "initContainerStatuses": [
            {"name": "init", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}],
            "containerStatuses": [{"state": {"waiting": {"reason": "PodInitializing"}}}]});
        assert_eq!(crate::columns::pod_status(&p), "Init:CrashLoopBackOff");
        assert!(!matches_states(&p, &wanted("all").unwrap()));
        p.data["spec"] = json!({});
        p.data["status"] = json!({"phase": "Running", "containerStatuses": [
            {"state": {"terminated": {"exitCode": 42}}}]});
        assert_eq!(crate::columns::pod_status(&p), "ExitCode:42");
        assert!(!matches_states(&p, &wanted("all").unwrap()));
        p.data["status"] = json!({"phase": "Failed", "reason": "Evicted", "containerStatuses": [
            {"ready": false, "state": {"running": {}}}]});
        assert!(!matches_states(&p, &wanted("all").unwrap()));
        p.data["status"]["containerStatuses"] = json!([]);
        p.metadata.deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            "2026-09-07T00:00:00Z".parse().unwrap(),
        ));
        assert!(!matches_states(&p, &wanted("all").unwrap()));
    }
}
