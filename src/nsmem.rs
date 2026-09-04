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

use serde::{Deserialize, Serialize};

/// Spelling of "all namespaces" in the file, so it stays hand-editable
/// (the in-app representation is the empty string).
const ALL: &str = "all";

/// Per-context namespace memory. Keys are kubeconfig context names; values
/// are a namespace or `all`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamespaceMemory {
    pub contexts: BTreeMap<String, String>,
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

    /// Persist to `path`. The file is replaced atomically, so a crash or a
    /// second sofka writing at the same moment cannot leave a torn file that
    /// [`Self::load`] would quietly read as empty.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        crate::atomicfile::write(path, &text)
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

#[cfg(test)]
mod tests {
    use super::*;

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
