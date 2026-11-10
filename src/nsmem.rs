//! Remembered namespace per context, persisted across restarts.
//!
//! Picking a namespace (`n`, `0`, `:ns`, `:<kind> <ns>`) records the choice
//! for the active context in `<state-dir>/namespaces.toml`, so the next
//! launch — and the next `:ctx` switch back — lands where you left off. Like
//! sort memory, this is a separate state file: sofka never rewrites the
//! user's config, and `default_namespace` there stays the fallback for a
//! context with no remembered pick. `-n`/`-A` on the CLI always win.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// Spelling of "all namespaces" in the file, so it stays hand-editable
/// (the in-app representation is the empty string).
const ALL: &str = "all";

/// Per-context namespace memory. Keys are kubeconfig context names; values
/// are a namespace or `all`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NamespaceMemory {
    pub contexts: BTreeMap<String, String>,
    #[serde(skip)]
    // Each edit has its own completion flag, shared with queued snapshots.
    dirty: BTreeMap<String, Arc<AtomicBool>>,
}

impl PartialEq for NamespaceMemory {
    fn eq(&self, other: &Self) -> bool {
        self.contexts == other.contexts
    }
}

impl NamespaceMemory {
    /// Where remembered namespaces live: `<state-dir>/namespaces.toml`.
    pub fn default_path() -> PathBuf {
        crate::diagnostics::state_dir().join("namespaces.toml")
    }

    /// Load persisted namespaces. A missing or unparsable file is an empty
    /// set — startup must not fail over hand-mangled state.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Merge changed keys into the current file and replace it atomically.
    /// Simultaneous saves from separate processes can still lose changes
    /// between the read and the write. The file is always complete.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut merged = Self::load(path);
        merged.merge_pending(self);
        let text = toml::to_string(&merged).map_err(|e| e.to_string())?;
        crate::atomicfile::write(path, &text)?;
        for saved in self.dirty.values() {
            saved.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Apply newer pending changes without losing earlier pending keys.
    pub(crate) fn merge_pending(&mut self, newer: &Self) {
        for (key, saved) in &newer.dirty {
            if saved.load(Ordering::Relaxed) {
                continue;
            }
            match newer.contexts.get(key) {
                Some(value) => self.contexts.insert(key.clone(), value.clone()),
                None => self.contexts.remove(key),
            };
            self.dirty.insert(key.clone(), Arc::clone(saved));
        }
    }

    /// Copy pending changes. Keep each edit until its write succeeds.
    pub fn snapshot(&mut self) -> Self {
        self.dirty.retain(|_, saved| !saved.load(Ordering::Relaxed));
        self.clone()
    }

    /// Remember `namespace` (empty = all namespaces) for `context`. Returns
    /// whether anything changed, so callers can skip the disk write. An
    /// empty context name (no kubeconfig context) is never recorded.
    pub fn set(&mut self, context: &str, namespace: &str) -> bool {
        if context.is_empty() {
            return false;
        }
        let value = if namespace.is_empty() {
            ALL.to_string()
        } else {
            namespace.to_string()
        };
        if self.contexts.get(context) == Some(&value) {
            return false;
        }
        self.contexts.insert(context.to_string(), value);
        self.dirty
            .insert(context.to_string(), Arc::new(AtomicBool::new(false)));
        true
    }

    /// The remembered namespace for `context`, if any; the empty string means
    /// all namespaces. Accepts the same `all`/`*` spellings the palette does.
    pub fn get(&self, context: &str) -> Option<String> {
        let v = self.contexts.get(context)?.trim();
        Some(match v {
            "" | "all" | "*" | "<all>" => String::new(),
            ns => ns.to_string(),
        })
    }
}

/// Resolve namespace scope. An empty explicit or saved value means all namespaces.
pub fn resolve_namespace(
    explicit: Option<String>,
    prefer_context_namespace: bool,
    context_namespace: Option<&str>,
    saved: Option<String>,
    configured: Option<&str>,
    fallback: &str,
) -> String {
    explicit
        .or_else(|| {
            prefer_context_namespace
                .then_some(context_namespace)
                .flatten()
                .map(str::to_owned)
        })
        .or(saved)
        .or_else(|| configured.map(str::to_owned))
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sofka-nsmem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn two_processes_keep_each_others_contexts() {
        let dir = scratch("two-processes");
        let path = dir.join("namespaces.toml");
        let mut a = NamespaceMemory::load(&path);
        let mut b = NamespaceMemory::load(&path);
        a.set("prod", "payments");
        a.snapshot().save(&path).unwrap();
        b.set("staging", "web");
        b.snapshot().save(&path).unwrap();
        let saved = NamespaceMemory::load(&path);
        assert_eq!(saved.get("prod"), Some("payments".into()));
        assert_eq!(saved.get("staging"), Some("web".into()));
        assert!(saved.dirty.is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[contexts]\nprod = \"payments\"\nstaging = \"web\"\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_later_writer_wins_for_the_same_context() {
        let dir = scratch("same-context");
        let path = dir.join("namespaces.toml");
        let mut a = NamespaceMemory::load(&path);
        let mut b = NamespaceMemory::load(&path);
        a.set("prod", "payments");
        b.set("prod", "web");
        a.snapshot().save(&path).unwrap();
        b.snapshot().save(&path).unwrap();
        assert_eq!(NamespaceMemory::load(&path).get("prod"), Some("web".into()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn successful_save_clears_the_pending_changes() {
        let dir = scratch("snapshot");
        let path = dir.join("namespaces.toml");
        let mut a = NamespaceMemory::load(&path);
        a.set("prod", "payments");
        a.snapshot().save(&path).unwrap();
        let mut b = NamespaceMemory::load(&path);
        b.set("prod", "web");
        b.snapshot().save(&path).unwrap();
        assert!(!a.set("prod", "payments"));
        a.snapshot().save(&path).unwrap();
        assert_eq!(NamespaceMemory::load(&path).get("prod"), Some("web".into()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn older_save_does_not_acknowledge_a_newer_edit() {
        let dir = scratch("older-save");
        let path = dir.join("namespaces.toml");
        let mut memory = NamespaceMemory::default();
        memory.set("prod", "payments");
        let older = memory.snapshot();
        memory.set("prod", "web");
        memory.set("prod", "payments");
        older.save(&path).unwrap();

        let mut external = NamespaceMemory::load(&path);
        external.set("prod", "external");
        external.save(&path).unwrap();
        memory.snapshot().save(&path).unwrap();
        assert_eq!(
            NamespaceMemory::load(&path).get("prod"),
            Some("payments".into())
        );
        assert!(memory.snapshot().dirty.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn namespace_precedence_keeps_explicit_and_saved_all_namespaces() {
        for explicit in [Some("chosen"), Some("")] {
            assert_eq!(
                resolve_namespace(
                    explicit.map(str::to_owned),
                    true,
                    Some("Y"),
                    Some("X".into()),
                    Some("configured"),
                    "default"
                ),
                explicit.unwrap()
            );
        }
        assert_eq!(
            resolve_namespace(
                None,
                false,
                Some("Y"),
                Some("X".into()),
                Some("configured"),
                "default"
            ),
            "X"
        );
        assert_eq!(
            resolve_namespace(
                None,
                true,
                Some("Y"),
                Some("X".into()),
                Some("configured"),
                "default"
            ),
            "Y"
        );
        assert_eq!(
            resolve_namespace(
                None,
                true,
                None,
                Some(String::new()),
                Some("configured"),
                "default"
            ),
            ""
        );
        assert_eq!(
            resolve_namespace(None, true, None, None, None, "default"),
            "default"
        );
    }

    #[test]
    fn set_get_roundtrip() {
        let mut m = NamespaceMemory::default();
        assert!(m.set("prod", "payments"));
        assert!(m.set("staging", ""));
        assert!(
            !m.set("prod", "payments"),
            "unchanged pick reports no change"
        );
        assert!(m.set("prod", "checkout"));
        assert_eq!(m.get("prod"), Some("checkout".into()));
        assert_eq!(m.get("staging"), Some(String::new()));
        assert_eq!(m.get("dev"), None);
        assert_eq!(m.contexts.get("staging").map(String::as_str), Some("all"));
    }

    #[test]
    fn empty_context_is_not_recorded() {
        let mut m = NamespaceMemory::default();
        assert!(!m.set("", "payments"));
        assert!(m.contexts.is_empty());
    }

    #[test]
    fn get_parses_hand_edited_specs() {
        let mut m = NamespaceMemory::default();
        m.contexts.insert("a".into(), "*".into());
        m.contexts.insert("b".into(), " <all> ".into());
        m.contexts.insert("c".into(), " kube-system ".into());
        assert_eq!(m.get("a"), Some(String::new()));
        assert_eq!(m.get("b"), Some(String::new()));
        assert_eq!(m.get("c"), Some("kube-system".into()));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sofka-nsmem-{}", std::process::id()));
        let path = dir.join("namespaces.toml");
        let mut m = NamespaceMemory::default();
        m.set("prod", "payments");
        m.set("staging", "");
        m.save(&path).unwrap();
        assert_eq!(NamespaceMemory::load(&path), m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_or_garbage_is_empty() {
        assert_eq!(
            NamespaceMemory::load(Path::new("/nonexistent/namespaces.toml")),
            NamespaceMemory::default()
        );
    }
}
