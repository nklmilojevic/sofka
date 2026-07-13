//! Session-local action journal.
//!
//! A bounded, in-memory log of the mutating actions taken this session — what,
//! against which target, in which context, and when — for a quick audit trail
//! (`:journal`). It records identifiers only (names, verbs), never secret
//! inputs or decoded Secret values, and is never written to disk.

use std::collections::VecDeque;

use k8s_openapi::jiff::Timestamp;

/// How many entries to keep before dropping the oldest.
const MAX_ENTRIES: usize = 500;

/// One recorded action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Epoch seconds when the action was taken.
    pub at: i64,
    pub context: String,
    /// The verb (`delete`, `scale to 3`, `shell`, `plugin: argocd-sync`, …).
    pub action: String,
    /// The object(s) acted on (`pods/api in prod`, `3 deployments`, …).
    pub target: String,
}

/// The session's action log, newest last.
#[derive(Default)]
pub struct Journal {
    entries: VecDeque<Entry>,
}

impl Journal {
    /// Record an action. Callers pass only identifiers — never secret input or
    /// decoded Secret values.
    pub fn record(&mut self, context: &str, action: impl Into<String>, target: impl Into<String>) {
        self.entries.push_back(Entry {
            at: Timestamp::now().as_second(),
            context: context.to_string(),
            action: action.into(),
            target: target.into(),
        });
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The journal as display lines, newest first, with a header.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{:<19}  {:<24}  {:<18}  TARGET",
            "WHEN", "ACTION", "CONTEXT"
        )];
        if self.entries.is_empty() {
            out.push("(no actions recorded this session)".into());
            return out;
        }
        for e in self.entries.iter().rev() {
            out.push(format!(
                "{:<19}  {:<24}  {:<18}  {}",
                clock(e.at),
                e.action,
                e.context,
                e.target
            ));
        }
        out
    }
}

/// Format an epoch second as `MM-DD HH:MM:SS` (UTC, like the events view).
fn clock(at: i64) -> String {
    match Timestamp::from_second(at) {
        Ok(ts) => {
            let s = ts.to_string(); // 2026-07-13T08:45:12Z
            let date = s.split('T').next().unwrap_or("");
            let day = date.get(5..).unwrap_or(date); // MM-DD
            let time = s
                .split_once('T')
                .map(|(_, t)| t.split(['.', 'Z']).next().unwrap_or(t))
                .unwrap_or("");
            format!("{day} {time}")
        }
        Err(_) => at.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_formats_newest_first() {
        let mut j = Journal::default();
        assert!(j.is_empty());
        j.record("prod", "delete", "pods/a in default");
        j.record("prod", "scale to 3", "deployments/api in prod");
        assert_eq!(j.len(), 2);
        let lines = j.lines();
        // Header, then newest first.
        assert!(lines[0].contains("ACTION"));
        assert!(lines[1].contains("scale to 3"), "{:?}", lines);
        assert!(lines[2].contains("delete"), "{:?}", lines);
    }

    #[test]
    fn bounded_to_the_cap() {
        let mut j = Journal::default();
        for i in 0..(MAX_ENTRIES + 25) {
            j.record("c", "delete", format!("pods/p{i}"));
        }
        assert_eq!(j.len(), MAX_ENTRIES);
        // Oldest dropped: the newest is p<max+24>.
        assert!(j.lines()[1].contains(&format!("p{}", MAX_ENTRIES + 24)));
    }

    #[test]
    fn empty_journal_says_so() {
        let j = Journal::default();
        assert!(j.lines().iter().any(|l| l.contains("no actions recorded")));
    }
}
