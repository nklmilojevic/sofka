//! Session-local resource timeline.
//!
//! As the table watch streams object versions, [`Timeline::observe`] diffs each
//! new version against the previous one and records the meaningful state
//! changes — generation bumps, replica/readiness shifts, pod phase and restart
//! changes, and condition transitions — into a bounded, per-object history.
//! The explain/timeline view then presents them as a causal, chronological log.
//!
//! This is deliberately session-local: it observes only what happens while
//! sofka is watching, cannot reconstruct anything from before it started, and
//! keeps nothing on disk. The transition logic is pure and unit-tested; the app
//! layer feeds it watch events and renders the history.

use std::collections::{HashMap, HashSet, VecDeque};

use k8s_openapi::jiff::Timestamp;
use kube::core::DynamicObject;
use serde_json::Value;

/// How many entries to keep per object before dropping the oldest.
const MAX_PER_OBJECT: usize = 200;
/// How many distinct objects to keep history for before evicting the
/// least-recently-touched one.
const MAX_OBJECTS: usize = 2000;

/// Coloring/severity of a timeline entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Good,
    Warn,
    Bad,
}

/// One recorded state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Epoch seconds when sofka observed the change.
    pub at: i64,
    pub level: Level,
    pub text: String,
}

/// Per-object session-local history, keyed by `plural/namespace/name`.
pub struct Timeline {
    /// Epoch seconds when this timeline started observing.
    started: i64,
    history: HashMap<String, VecDeque<Entry>>,
    /// Touch order for [`MAX_OBJECTS`] eviction (front = oldest).
    order: VecDeque<String>,
    /// Keys observed at least once, so "created" fires only on genuine first
    /// sight (not on a watcher relist, which re-applies everything).
    seen: HashSet<String>,
}

impl Default for Timeline {
    fn default() -> Self {
        Timeline {
            started: Self::now(),
            history: HashMap::new(),
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }
}

impl Timeline {
    /// Wipe all history (e.g. on a context switch — a new cluster's objects are
    /// unrelated). Keeps `started` so creation detection stays anchored.
    pub fn clear(&mut self) {
        self.history.clear();
        self.order.clear();
        self.seen.clear();
    }

    /// Record the transitions from `prev` to `new` for one object. `prev` is
    /// `None` on first sight (initial list / post-relist): nothing is recorded
    /// except a genuine creation (first sight *and* born after we started
    /// watching), so a relisted fleet doesn't flood the log with phantom
    /// "created" lines.
    pub fn observe(
        &mut self,
        plural: &str,
        rk: &str,
        prev: Option<&DynamicObject>,
        new: &DynamicObject,
    ) {
        let now = Self::now();
        let key = tkey(plural, rk);
        let first_ever = self.seen.insert(key.clone());
        match prev {
            // First sight (initial list / post-relist). Record a creation only
            // for an object genuinely born after we started watching, and only
            // once — so a relisted fleet doesn't flood the log.
            None => {
                if first_ever && created_after(new, self.started) {
                    self.push(
                        &key,
                        now,
                        Level::Good,
                        format!("{} created", singular(plural)),
                    );
                }
            }
            Some(prev) => {
                for (level, text) in transitions(prev, new, plural) {
                    self.push(&key, now, level, text);
                }
            }
        }
    }

    /// Record a deletion.
    pub fn observe_delete(&mut self, plural: &str, rk: &str) {
        let now = Self::now();
        self.push(
            &tkey(plural, rk),
            now,
            Level::Bad,
            format!("{} deleted", singular(plural)),
        );
    }

    /// The recorded history for one object, oldest first.
    pub fn entries(&self, plural: &str, rk: &str) -> Option<&VecDeque<Entry>> {
        self.history.get(&tkey(plural, rk))
    }

    fn push(&mut self, key: &str, at: i64, level: Level, text: String) {
        let entry = Entry { at, level, text };
        let dq = match self.history.get_mut(key) {
            Some(dq) => dq,
            None => {
                if self.order.len() >= MAX_OBJECTS
                    && let Some(evict) = self.order.pop_front()
                {
                    self.history.remove(&evict);
                }
                self.order.push_back(key.to_string());
                self.history.entry(key.to_string()).or_default()
            }
        };
        dq.push_back(entry);
        while dq.len() > MAX_PER_OBJECT {
            dq.pop_front();
        }
    }

    fn now() -> i64 {
        Timestamp::now().as_second()
    }
}

fn tkey(plural: &str, rk: &str) -> String {
    format!("{plural}/{rk}")
}

/// A rough singular for display ("deployments" -> "Deployment").
fn singular(plural: &str) -> String {
    let base = plural.strip_suffix('s').unwrap_or(plural);
    let mut c = base.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => base.to_string(),
    }
}

/// Whether the object was created at or after `since` (epoch seconds).
fn created_after(obj: &DynamicObject, since: i64) -> bool {
    obj.metadata
        .creation_timestamp
        .as_ref()
        .map(|ts| ts.0.as_second() >= since)
        .unwrap_or(false)
}

/// Format an entry timestamp as `HH:MM:SS` (UTC, matching the events view).
pub fn clock(at: i64) -> String {
    match Timestamp::from_second(at) {
        Ok(ts) => {
            let s = ts.to_string();
            s.split_once('T')
                .map(|(_, t)| t.split(['.', 'Z']).next().unwrap_or(t).to_string())
                .unwrap_or(s)
        }
        Err(_) => at.to_string(),
    }
}

// ----- transition detection (pure) ----------------------------------------

fn transitions(prev: &DynamicObject, new: &DynamicObject, plural: &str) -> Vec<(Level, String)> {
    let mut out = Vec::new();

    // Spec (generation) change — the rollout trigger.
    if let (Some(a), Some(b)) = (prev.metadata.generation, new.metadata.generation)
        && a != b
    {
        out.push((Level::Info, format!("spec changed: generation {a} → {b}")));
    }

    match plural {
        "pods" => pod_transitions(prev, new, &mut out),
        "deployments" | "statefulsets" | "daemonsets" | "replicasets" => {
            workload_transitions(prev, new, plural, &mut out)
        }
        _ => ready_transition(prev, new, &mut out),
    }
    out
}

fn pod_transitions(prev: &DynamicObject, new: &DynamicObject, out: &mut Vec<(Level, String)>) {
    // Phase.
    let (p0, p1) = (str_at(prev, "/status/phase"), str_at(new, "/status/phase"));
    if p0 != p1 && !p1.is_empty() {
        let level = match p1.as_str() {
            "Running" | "Succeeded" => Level::Good,
            "Failed" => Level::Bad,
            _ => Level::Info,
        };
        out.push((level, format!("phase {} → {}", or_none(&p0), p1)));
    }

    // Restarts (summed across containers).
    let (r0, r1) = (restarts_sum(prev), restarts_sum(new));
    if r1 > r0 {
        out.push((
            Level::Warn,
            format!("container restart ({r0} → {r1} total)"),
        ));
    }

    // Waiting reason (CrashLoopBackOff, ImagePullBackOff, …).
    let (w0, w1) = (waiting_reason(prev), waiting_reason(new));
    if w0 != w1 {
        match &w1 {
            Some(r) => out.push((Level::Bad, format!("container waiting: {r}"))),
            None => {
                if w0.is_some() {
                    out.push((Level::Good, "container no longer waiting".into()));
                }
            }
        }
    }

    ready_transition(prev, new, out);
}

fn workload_transitions(
    prev: &DynamicObject,
    new: &DynamicObject,
    plural: &str,
    out: &mut Vec<(Level, String)>,
) {
    let (ready_ptr, total_ptr) = if plural == "daemonsets" {
        ("/status/numberReady", "/status/desiredNumberScheduled")
    } else {
        ("/status/readyReplicas", "/status/replicas")
    };
    let (a, b) = (i_at(prev, ready_ptr), i_at(new, ready_ptr));
    if a != b {
        let total = i_at(new, total_ptr);
        let level = if b < a { Level::Warn } else { Level::Good };
        out.push((level, format!("ready replicas {a} → {b} (of {total})")));
    }

    // Available / Progressing conditions.
    for ty in ["Available", "Progressing"] {
        if let Some(t) = cond_transition(prev, new, ty) {
            out.push(t);
        }
    }
}

/// Ready-condition transition (pods, Flux objects, most condition-based CRDs).
fn ready_transition(prev: &DynamicObject, new: &DynamicObject, out: &mut Vec<(Level, String)>) {
    if let Some(t) = cond_transition(prev, new, "Ready") {
        out.push(t);
    }
}

/// A `(status, reason)` change on condition `ty`, rendered with a sensible
/// severity. `None` when the status didn't change.
fn cond_transition(prev: &DynamicObject, new: &DynamicObject, ty: &str) -> Option<(Level, String)> {
    let (s0, _) = condition(prev, ty)?;
    let (s1, reason) = condition(new, ty)?;
    if s0 == s1 {
        return None;
    }
    // For Progressing/Available/Ready, True is the healthy state.
    let level = match s1.as_str() {
        "True" => Level::Good,
        "False" => Level::Bad,
        _ => Level::Warn,
    };
    let suffix = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    };
    Some((level, format!("{ty}: {s0} → {s1}{suffix}")))
}

// ----- accessors -----------------------------------------------------------

fn str_at(obj: &DynamicObject, ptr: &str) -> String {
    obj.data
        .pointer(ptr)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn i_at(obj: &DynamicObject, ptr: &str) -> i64 {
    obj.data.pointer(ptr).and_then(Value::as_i64).unwrap_or(0)
}

fn restarts_sum(obj: &DynamicObject) -> i64 {
    obj.data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter_map(|c| c.get("restartCount").and_then(Value::as_i64))
                .sum()
        })
        .unwrap_or(0)
}

fn waiting_reason(obj: &DynamicObject) -> Option<String> {
    let cs = obj
        .data
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)?;
    cs.iter().find_map(|c| {
        c.pointer("/state/waiting/reason")
            .and_then(Value::as_str)
            .filter(|r| *r != "ContainerCreating" && *r != "PodInitializing")
            .map(String::from)
    })
}

/// `(status, reason)` of condition `ty`, if present.
fn condition(obj: &DynamicObject, ty: &str) -> Option<(String, String)> {
    obj.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(ty))
        .map(|c| {
            (
                c.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                c.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
}

fn or_none(s: &str) -> String {
    if s.is_empty() {
        "<none>".into()
    } else {
        s.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    fn kinds(t: &[(Level, String)]) -> Vec<&str> {
        t.iter().map(|(_, s)| s.as_str()).collect()
    }

    #[test]
    fn deployment_generation_and_ready_changes() {
        let a = obj(json!({
            "apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","generation":17},
            "status":{"readyReplicas":3,"replicas":3}
        }));
        let b = obj(json!({
            "apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","generation":18},
            "status":{"readyReplicas":1,"replicas":3}
        }));
        let t = transitions(&a, &b, "deployments");
        let all = kinds(&t).join("|");
        assert!(all.contains("generation 17 → 18"), "{all}");
        assert!(all.contains("ready replicas 3 → 1 (of 3)"), "{all}");
        // A drop is a warning.
        assert!(
            t.iter()
                .any(|(l, s)| *l == Level::Warn && s.contains("ready replicas"))
        );
    }

    #[test]
    fn pod_phase_restart_and_waiting_transitions() {
        let a = obj(json!({
            "apiVersion":"v1","kind":"Pod","metadata":{"name":"p"},
            "status":{"phase":"Pending","containerStatuses":[
                {"name":"c","restartCount":2,"state":{"running":{}}}
            ]}
        }));
        let b = obj(json!({
            "apiVersion":"v1","kind":"Pod","metadata":{"name":"p"},
            "status":{"phase":"Running","containerStatuses":[
                {"name":"c","restartCount":3,"state":{"waiting":{"reason":"CrashLoopBackOff"}}}
            ]}
        }));
        let t = transitions(&a, &b, "pods");
        let all = kinds(&t).join("|");
        assert!(all.contains("phase Pending → Running"), "{all}");
        assert!(all.contains("container restart (2 → 3 total)"), "{all}");
        assert!(all.contains("container waiting: CrashLoopBackOff"), "{all}");
    }

    #[test]
    fn ready_condition_transition_records_reason() {
        let a = obj(json!({
            "apiVersion":"cert-manager.io/v1","kind":"Certificate","metadata":{"name":"tls"},
            "status":{"conditions":[{"type":"Ready","status":"True"}]}
        }));
        let b = obj(json!({
            "apiVersion":"cert-manager.io/v1","kind":"Certificate","metadata":{"name":"tls"},
            "status":{"conditions":[{"type":"Ready","status":"False","reason":"Expired"}]}
        }));
        let t = transitions(&a, &b, "certificates");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, Level::Bad);
        assert!(
            t[0].1.contains("Ready: True → False (Expired)"),
            "{}",
            t[0].1
        );
    }

    #[test]
    fn no_change_yields_nothing() {
        let a = obj(json!({
            "apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","generation":5},
            "status":{"readyReplicas":3,"replicas":3,
                "conditions":[{"type":"Available","status":"True"}]}
        }));
        assert!(transitions(&a, &a, "deployments").is_empty());
    }

    #[test]
    fn observe_baselines_first_sight_without_created_for_old_objects() {
        let mut tl = Timeline::default();
        // An object created long ago (epoch 1000s) — no "created" on first sight.
        let old = obj(json!({
            "apiVersion":"v1","kind":"Pod",
            "metadata":{"name":"p","creationTimestamp":"1970-01-01T00:16:40Z"},
            "status":{"phase":"Running"}
        }));
        tl.observe("pods", "ns/p", None, &old);
        assert!(tl.entries("pods", "ns/p").is_none());
        // A later phase change records.
        let changed = obj(json!({
            "apiVersion":"v1","kind":"Pod",
            "metadata":{"name":"p","creationTimestamp":"1970-01-01T00:16:40Z"},
            "status":{"phase":"Failed"}
        }));
        tl.observe("pods", "ns/p", Some(&old), &changed);
        let e = tl.entries("pods", "ns/p").unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].text.contains("phase Running → Failed"));
    }

    #[test]
    fn created_fires_once_for_session_born_objects_only() {
        let mut tl = Timeline::default();
        // Born far in the future → unambiguously after `started`.
        let born = obj(json!({
            "apiVersion":"v1","kind":"Pod",
            "metadata":{"name":"new","creationTimestamp":"2999-01-01T00:00:00Z"},
            "status":{"phase":"Pending"}
        }));
        tl.observe("pods", "ns/new", None, &born);
        let e = tl.entries("pods", "ns/new").unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].text.contains("Pod created"));
        // A relist (prev=None again) must not record a second creation.
        tl.observe("pods", "ns/new", None, &born);
        assert_eq!(tl.entries("pods", "ns/new").unwrap().len(), 1);
    }

    #[test]
    fn per_object_history_is_bounded() {
        let mut tl = Timeline::default();
        let mk = |n: i64| {
            obj(json!({
                "apiVersion":"apps/v1","kind":"Deployment",
                "metadata":{"name":"api","generation":n},"status":{}
            }))
        };
        let mut prev = mk(0);
        for g in 1..(MAX_PER_OBJECT as i64 + 50) {
            let cur = mk(g);
            tl.observe("deployments", "ns/api", Some(&prev), &cur);
            prev = cur;
        }
        assert_eq!(
            tl.entries("deployments", "ns/api").unwrap().len(),
            MAX_PER_OBJECT
        );
    }

    #[test]
    fn clock_formats_hms() {
        // 08:45:12 UTC on 2026-07-13 → 1_752_396_312.
        assert_eq!(clock(1_752_396_312), "08:45:12");
    }
}
