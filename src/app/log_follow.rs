//! Keep followed kubelet log streams alive.
//!
//! The API server ends a follow stream when the container exits, when the
//! connection drops, or when the kubelet gives up on it. Each stream resumes
//! from the last line it delivered until its pod is gone or finished.
//! Workload and service logs watch their selector, so pods that a rollout
//! creates join the view and the pods it deletes leave it.

use super::*;
use futures_util::{AsyncBufReadExt, TryStreamExt};
use k8s_openapi::api::core::v1::ContainerStatus;
use k8s_openapi::jiff::Timestamp;
use std::time::Instant;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(15);
/// A stream that stayed open this long was healthy, whatever ended it.
const HEALTHY_STREAM: Duration = Duration::from_secs(10);
/// A connection that slept with the laptop can block forever without these.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const SELECTOR_BACKOFF_CEILING: Duration = Duration::from_secs(30);

/// Where a stream got to, so a reconnect continues without repeating lines.
/// `sinceTime` has one-second resolution, so the replay starts up to a second
/// early and drops the lines that were already delivered.
#[derive(Default)]
pub(super) struct LogResume {
    last: Option<Timestamp>,
    at_last: usize,
    skip: usize,
    replaying: bool,
}

impl LogResume {
    pub(super) fn admit(&mut self, line: &str) -> bool {
        let Ok(time) = line.split(' ').next().unwrap_or("").parse::<Timestamp>() else {
            return true;
        };
        let Some(last) = self.last else {
            self.last = Some(time);
            self.at_last = 1;
            return true;
        };
        if self.replaying {
            if time < last {
                return false;
            }
            if time == last && self.skip > 0 {
                self.skip -= 1;
                return false;
            }
        }
        if time == last {
            self.at_last += 1;
        } else if time > last {
            self.last = Some(time);
            self.at_last = 1;
            self.replaying = false;
        }
        true
    }

    pub(super) fn rewind(&mut self) {
        self.replaying = true;
        self.skip = self.at_last;
    }

    pub(super) fn since(&self) -> Option<Timestamp> {
        Timestamp::from_second(self.last?.as_second()).ok()
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum StreamEnd {
    Gone,
    Finished(String),
    Restarted(i32),
    Recreated,
    Resume,
}

/// What the pod says about a stream that just ended. `known_uid` and
/// `known_restarts` carry what earlier checks saw; `pinned` streams belong to
/// one pod instance and end when a pod of the same name replaces it.
pub(super) fn stream_end(
    pod: Option<&Pod>,
    container: Option<&str>,
    pinned: bool,
    known_uid: &mut Option<String>,
    known_restarts: &mut Option<i32>,
    connected_at: Timestamp,
) -> StreamEnd {
    let Some(pod) = pod else {
        return StreamEnd::Gone;
    };
    let uid = pod.metadata.uid.clone();
    if known_uid.is_some() && uid.is_some() && *known_uid != uid {
        if pinned {
            return StreamEnd::Gone;
        }
        *known_uid = uid;
        *known_restarts = None;
        return StreamEnd::Recreated;
    }
    if known_uid.is_none() {
        *known_uid = uid;
    }

    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or_default();
    if matches!(phase, "Succeeded" | "Failed") {
        return StreamEnd::Finished(phase.to_ascii_lowercase());
    }
    let Some(status) = container_status(pod, container) else {
        return StreamEnd::Resume;
    };

    let restarted = match *known_restarts {
        Some(known) => status.restart_count > known,
        None => status
            .last_state
            .as_ref()
            .and_then(|s| s.terminated.as_ref())
            .and_then(|t| t.finished_at.as_ref())
            .is_some_and(|finished| finished.0.as_second() >= connected_at.as_second()),
    };
    *known_restarts = Some(status.restart_count);
    if restarted {
        return StreamEnd::Restarted(status.restart_count);
    }

    // A container that exited for good while the rest of the pod runs on.
    let policy = pod
        .spec
        .as_ref()
        .and_then(|s| s.restart_policy.as_deref())
        .unwrap_or("Always");
    if let Some(done) = status.state.as_ref().and_then(|s| s.terminated.as_ref())
        && (policy == "Never" || (policy == "OnFailure" && done.exit_code == 0))
    {
        return StreamEnd::Finished("container exited".into());
    }
    StreamEnd::Resume
}

fn container_status<'a>(pod: &'a Pod, container: Option<&str>) -> Option<&'a ContainerStatus> {
    let name = match container {
        Some(name) => name,
        None => pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("kubectl.kubernetes.io/default-container"))
            .map(String::as_str)
            .or_else(|| {
                pod.spec
                    .as_ref()?
                    .containers
                    .first()
                    .map(|c| c.name.as_str())
            })?,
    };
    let status = pod.status.as_ref()?;
    status
        .container_statuses
        .iter()
        .chain(&status.init_container_statuses)
        .chain(&status.ephemeral_container_statuses)
        .flatten()
        .find(|s| s.name == name)
}

/// A refusal that retrying will not change, such as a missing container or
/// RBAC that does not allow reading logs.
fn refused(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(e) if (400..500).contains(&e.code) && e.code != 429)
}

/// The container has not started yet, so the stream has nothing to give.
fn not_started(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(e) if e.code == 400
        && (e.message.contains("is waiting to start")
            || e.message.contains("does not have a host assigned")))
}

enum Pumped {
    Ended,
    Woke,
    Stale,
}

/// One container's log stream.
pub(super) struct LogStream {
    pub(super) api: Api<Pod>,
    pub(super) pod: String,
    /// Set for pods a selector found: a replacement with the same name gets
    /// its own stream.
    pub(super) uid: Option<String>,
    pub(super) params: LogParams,
    pub(super) prefix: String,
    pub(super) tx: Sender<Msg>,
    pub(super) generation: u64,
    pub(super) flag: Arc<AtomicU64>,
    pub(super) wake: watch::Receiver<u64>,
}

impl LogStream {
    fn stale(&self) -> bool {
        self.flag.load(Ordering::SeqCst) != self.generation || self.tx.is_closed()
    }

    async fn send(&self, text: String) -> bool {
        let mut lines = vec![format!("{}{text}", self.prefix)];
        send_log_batch(&self.tx, self.generation, &mut lines).await
    }

    /// Wait out `delay`, or less if the machine wakes. False once stale.
    async fn pause(&mut self, delay: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            Ok(()) = self.wake.changed() => {}
            _ = self.tx.closed() => return false,
        }
        !self.stale()
    }

    pub(super) async fn run(mut self) {
        let follow = self.params.follow && !self.params.previous;
        let pinned = self.uid.is_some();
        let mut known_uid = self.uid.clone();
        let mut known_restarts = None;
        let mut resume = LogResume::default();
        let mut delay = RECONNECT_MIN;
        let mut reported = false;

        loop {
            if self.stale() {
                return;
            }
            let connected_at = Timestamp::now();
            let opened = Instant::now();
            let opening =
                tokio::time::timeout(OPEN_TIMEOUT, self.api.log_stream(&self.pod, &self.params))
                    .await;
            let (pumped, delivered) = match opening {
                Ok(Ok(stream)) => {
                    reported = false;
                    self.pump(stream, &mut resume).await
                }
                Ok(Err(error)) if follow && not_started(&error) => {
                    if !self.pause(RECONNECT_MIN).await {
                        return;
                    }
                    continue;
                }
                failed => {
                    let (error, permanent) = match failed {
                        Ok(Err(error)) => (error.to_string(), refused(&error)),
                        _ => ("timed out opening the log stream".into(), false),
                    };
                    if !reported && !self.send(format!("[error] {error}")).await {
                        return;
                    }
                    if !follow || permanent {
                        return;
                    }
                    reported = true;
                    (Pumped::Ended, 0)
                }
            };
            let woke = match pumped {
                Pumped::Stale => return,
                Pumped::Woke => true,
                Pumped::Ended => false,
            };
            if !follow {
                return;
            }

            let pod = tokio::time::timeout(STATUS_TIMEOUT, self.api.get_opt(&self.pod)).await;
            if let Ok(Ok(pod)) = pod {
                let container = self.params.container.as_deref();
                match stream_end(
                    pod.as_ref(),
                    container,
                    pinned,
                    &mut known_uid,
                    &mut known_restarts,
                    connected_at,
                ) {
                    StreamEnd::Gone => {
                        self.send("[sofka] pod deleted; stream ended".into()).await;
                        return;
                    }
                    StreamEnd::Finished(how) => {
                        self.send(format!("[sofka] {how}; stream ended")).await;
                        return;
                    }
                    StreamEnd::Restarted(restarts) => {
                        if !self
                            .send(format!(
                                "[sofka] container restarted (restarts: {restarts})"
                            ))
                            .await
                        {
                            return;
                        }
                        delay = RECONNECT_MIN;
                    }
                    StreamEnd::Recreated => {
                        if !self.send("[sofka] pod recreated".into()).await {
                            return;
                        }
                        delay = RECONNECT_MIN;
                    }
                    StreamEnd::Resume => {}
                }
            }

            if delivered > 0 || opened.elapsed() >= HEALTHY_STREAM || woke {
                delay = RECONNECT_MIN;
            }
            if !woke {
                if !self.pause(delay).await {
                    return;
                }
                delay = (delay * 2).min(RECONNECT_MAX);
            }

            resume.rewind();
            if let Some(since) = resume.since() {
                self.params.tail_lines = None;
                self.params.since_seconds = None;
                self.params.since_time = Some(since);
            }
        }
    }

    /// Forward lines until the stream ends. Returns how it ended and how many
    /// new lines it delivered.
    async fn pump(
        &mut self,
        stream: impl futures_util::AsyncBufRead + Unpin,
        resume: &mut LogResume,
    ) -> (Pumped, usize) {
        let mut lines = stream.lines();
        let mut batch = Vec::with_capacity(LOG_BATCH_LINES);
        let mut delivered = 0;
        let mut flush = tokio::time::interval(Duration::from_millis(LOG_BATCH_MS));
        flush.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let pumped = loop {
            if self.flag.load(Ordering::SeqCst) != self.generation {
                break Pumped::Stale;
            }
            tokio::select! {
                next = lines.try_next() => match next {
                    Ok(Some(line)) => {
                        if !resume.admit(&line) {
                            continue;
                        }
                        delivered += 1;
                        batch.push(format!("{}{line}", self.prefix));
                        if batch.len() >= LOG_BATCH_LINES
                            && !send_log_batch(&self.tx, self.generation, &mut batch).await
                        {
                            break Pumped::Stale;
                        }
                    }
                    // Errors mid-stream resume like a clean end. Any gap is
                    // replayed from the last delivered line.
                    Ok(None) | Err(_) => break Pumped::Ended,
                },
                _ = flush.tick(), if !batch.is_empty() => {
                    if !send_log_batch(&self.tx, self.generation, &mut batch).await {
                        break Pumped::Stale;
                    }
                }
                Ok(()) = self.wake.changed() => break Pumped::Woke,
            }
        };
        if matches!(pumped, Pumped::Stale) || self.stale() {
            return (Pumped::Stale, delivered);
        }
        if !send_log_batch(&self.tx, self.generation, &mut batch).await {
            return (Pumped::Stale, delivered);
        }
        (pumped, delivered)
    }
}

/// A pod instance: its UID, or its name where the API left the UID out.
fn pod_key(pod: &Pod) -> String {
    pod.metadata.uid.clone().unwrap_or_else(|| {
        format!(
            "{}/{}",
            pod.metadata.namespace.as_deref().unwrap_or_default(),
            pod.metadata.name.as_deref().unwrap_or_default()
        )
    })
}

/// Logs for every pod a label selector matches, now and later.
pub(super) struct SelectorLogs {
    pub(super) client: Client,
    pub(super) ns: String,
    pub(super) labels: String,
    /// Lines per container for the pods that already exist. Pods that start
    /// later are shown from their first line.
    pub(super) tail: i64,
    pub(super) since: Option<i64>,
    pub(super) tx: Sender<Msg>,
    pub(super) generation: u64,
    pub(super) flag: Arc<AtomicU64>,
    pub(super) wake: watch::Receiver<u64>,
}

impl SelectorLogs {
    fn stale(&self) -> bool {
        self.flag.load(Ordering::SeqCst) != self.generation || self.tx.is_closed()
    }

    async fn send(&self, line: String) -> bool {
        send_log_batch(&self.tx, self.generation, &mut vec![line]).await
    }

    pub(super) async fn run(mut self) {
        let api: Api<Pod> = if self.ns.is_empty() {
            Api::all(self.client.clone())
        } else {
            Api::namespaced(self.client.clone(), &self.ns)
        };
        let config = watcher::Config::default().labels(&self.labels);
        let mut events = watcher(api.clone(), config.clone()).boxed();
        let mut backoff = watcher::DefaultBackoff::default();
        let mut followed: HashSet<String> = HashSet::new();
        let mut listed: HashSet<String> = HashSet::new();
        let mut synced = false;
        let mut reported = false;
        let mut streams = tokio::task::JoinSet::new();

        loop {
            if self.stale() {
                return;
            }
            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else { return };
                    // `Init` and the `InitApply`s behind it repeat on every
                    // list attempt, so only these show the watch got somewhere.
                    if matches!(
                        event,
                        Ok(watcher::Event::Apply(_)
                            | watcher::Event::Delete(_)
                            | watcher::Event::InitDone)
                    ) {
                        backoff.reset();
                    }
                    // A re-list succeeds even while the watch keeps failing.
                    if matches!(event, Ok(watcher::Event::Apply(_) | watcher::Event::Delete(_))) {
                        reported = false;
                    }
                    match event {
                        Ok(watcher::Event::Init) => listed.clear(),
                        Ok(watcher::Event::InitApply(pod) | watcher::Event::Apply(pod)) => {
                            let key = pod_key(&pod);
                            listed.insert(key.clone());
                            if !followed.insert(key) {
                                continue;
                            }
                            if synced {
                                let name = pod.metadata.name.as_deref().unwrap_or_default();
                                if !self.send(format!("[{name}] [sofka] following new pod")).await {
                                    return;
                                }
                            }
                            self.follow(&mut streams, &pod, !synced);
                        }
                        Ok(watcher::Event::Delete(pod)) => {
                            followed.remove(&pod_key(&pod));
                        }
                        Ok(watcher::Event::InitDone) => {
                            followed.retain(|uid| listed.contains(uid));
                            if !synced && followed.is_empty() {
                                let line = "(no matching pods; new pods will be followed)";
                                if !self.send(line.into()).await {
                                    return;
                                }
                            }
                            synced = true;
                        }
                        Err(error) => {
                            if !synced
                                && let watcher::Error::InitialListFailed(kube::Error::Api(_)) = &error
                            {
                                self.send(format!("[error] {error}")).await;
                                return;
                            }
                            if !reported && synced {
                                reported = true;
                                let line = format!("[sofka] pod watch failed, retrying: {error}");
                                if !self.send(line).await {
                                    return;
                                }
                            }
                            let delay = backoff.next().unwrap_or(SELECTOR_BACKOFF_CEILING);
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                Ok(()) = self.wake.changed() => {}
                                _ = self.tx.closed() => return,
                            }
                        }
                    }
                }
                Some(_) = streams.join_next(), if !streams.is_empty() => {}
                Ok(()) = self.wake.changed() => {
                    events = watcher(api.clone(), config.clone()).boxed();
                }
                _ = self.tx.closed() => return,
            }
        }
    }

    fn follow(&self, streams: &mut tokio::task::JoinSet<()>, pod: &Pod, existing: bool) {
        let name = pod.metadata.name.clone().unwrap_or_default();
        let ns = pod
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| self.ns.clone());
        let containers: Vec<String> = pod
            .spec
            .as_ref()
            .map(|s| s.containers.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        let multi = containers.len() > 1;
        for container in containers {
            let prefix = if multi {
                format!("[{name}:{container}] ")
            } else {
                format!("[{name}] ")
            };
            let (tail_lines, since_seconds) = if existing {
                (Some(self.tail), self.since)
            } else {
                (None, None)
            };
            let stream = LogStream {
                api: Api::namespaced(self.client.clone(), &ns),
                pod: name.clone(),
                uid: pod.metadata.uid.clone(),
                params: LogParams {
                    follow: true,
                    container: Some(container),
                    timestamps: true,
                    tail_lines,
                    since_seconds,
                    ..Default::default()
                },
                prefix,
                tx: self.tx.clone(),
                generation: self.generation,
                flag: self.flag.clone(),
                wake: self.wake.clone(),
            };
            streams.spawn(stream.run());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(v: serde_json::Value) -> Pod {
        serde_json::from_value(v).unwrap()
    }

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn resume_skips_the_replayed_second_but_keeps_new_lines() {
        let mut r = LogResume::default();
        for line in [
            "2026-10-06T10:00:00.5Z a",
            "2026-10-06T10:00:01.2Z b",
            "2026-10-06T10:00:01.2Z c",
        ] {
            assert!(r.admit(line));
        }
        r.rewind();
        assert_eq!(r.since(), Some(at("2026-10-06T10:00:01Z")));
        assert!(!r.admit("2026-10-06T10:00:01.1Z older"));
        assert!(!r.admit("2026-10-06T10:00:01.2Z b"));
        assert!(!r.admit("2026-10-06T10:00:01.2Z c"));
        assert!(r.admit("2026-10-06T10:00:01.2Z d"));
        assert!(r.admit("2026-10-06T10:00:01.3Z e"));
        assert!(r.admit("2026-10-06T10:00:01.1Z late but live"));
        assert!(r.admit("no timestamp"));
    }

    #[test]
    fn resume_without_lines_has_no_since() {
        let mut r = LogResume::default();
        r.rewind();
        assert_eq!(r.since(), None);
        assert!(r.admit("2026-10-06T10:00:00Z first"));
    }

    #[test]
    fn stream_end_reads_the_pod() {
        let connected = at("2026-10-06T10:00:00Z");
        let check = |p: Option<&Pod>, pinned: bool, uid: &mut Option<String>| {
            stream_end(p, Some("app"), pinned, uid, &mut None, connected)
        };
        assert_eq!(check(None, false, &mut None), StreamEnd::Gone);

        let running = pod(json!({"metadata": {"name": "web", "uid": "2"},
            "status": {"phase": "Running",
                "containerStatuses": [{"name": "app", "restartCount": 0, "image": "", "imageID": "", "ready": true}]}}));
        assert_eq!(
            check(Some(&running), true, &mut Some("1".into())),
            StreamEnd::Gone
        );
        let mut uid = Some("1".into());
        assert_eq!(check(Some(&running), false, &mut uid), StreamEnd::Recreated);
        assert_eq!(uid.as_deref(), Some("2"));
        assert_eq!(check(Some(&running), false, &mut uid), StreamEnd::Resume);

        let done = pod(json!({"metadata": {"name": "web"}, "status": {"phase": "Succeeded"}}));
        assert_eq!(
            check(Some(&done), false, &mut None),
            StreamEnd::Finished("succeeded".into())
        );
    }

    #[test]
    fn stream_end_notices_restarts() {
        let connected = at("2026-10-06T10:00:00Z");
        let restarted = |count: i32, finished: &str| {
            pod(
                json!({"metadata": {"name": "web"}, "spec": {"containers": [{"name": "app"}]},
                "status": {"phase": "Running", "containerStatuses": [{
                    "name": "app", "restartCount": count, "image": "", "imageID": "", "ready": true,
                    "lastState": {"terminated": {"exitCode": 1, "finishedAt": finished}}}]}}),
            )
        };
        let mut known = None;
        let first = restarted(3, "2026-10-06T10:00:04Z");
        assert_eq!(
            stream_end(Some(&first), None, false, &mut None, &mut known, connected),
            StreamEnd::Restarted(3)
        );
        assert_eq!(
            stream_end(Some(&first), None, false, &mut None, &mut known, connected),
            StreamEnd::Resume
        );
        let again = restarted(4, "2026-10-06T10:01:00Z");
        assert_eq!(
            stream_end(Some(&again), None, false, &mut None, &mut known, connected),
            StreamEnd::Restarted(4)
        );

        let before = restarted(3, "2026-10-06T09:59:00Z");
        assert_eq!(
            stream_end(Some(&before), None, false, &mut None, &mut None, connected),
            StreamEnd::Resume
        );
    }

    #[test]
    fn stream_end_stops_on_a_container_that_will_not_restart() {
        let exited = |policy: &str, code: i32| {
            pod(json!({"metadata": {"name": "web"},
                "spec": {"restartPolicy": policy, "containers": [{"name": "app"}, {"name": "side"}]},
                "status": {"phase": "Running", "containerStatuses": [{
                    "name": "app", "restartCount": 0, "image": "", "imageID": "", "ready": false,
                    "state": {"terminated": {"exitCode": code}}}]}}))
        };
        let end = |p: &Pod| {
            stream_end(
                Some(p),
                Some("app"),
                false,
                &mut None,
                &mut None,
                at("2026-10-06T10:00:00Z"),
            )
        };
        let finished = StreamEnd::Finished("container exited".into());
        assert_eq!(end(&exited("Never", 1)), finished);
        assert_eq!(end(&exited("OnFailure", 0)), finished);
        assert_eq!(end(&exited("OnFailure", 1)), StreamEnd::Resume);
        assert_eq!(end(&exited("Always", 0)), StreamEnd::Resume);
    }
}
