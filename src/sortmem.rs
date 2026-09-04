//! Remembered sort choices, persisted across restarts.
//!
//! Picking a sort column (`S`, a header click) or flipping direction (`I`)
//! records the choice per resource kind in `<state-dir>/sort.toml`, so the
//! next visit to that kind — in this session or the next — comes back sorted
//! the same way. Picking the pinned default ordering forgets the entry. Like
//! fleet marks, this is deliberately a separate state file: sofka never
//! rewrites the user's config, and a `[views] sort` there stays the
//! hand-edited fallback these choices overlay.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-kind sort memory. Keys are the kind plural (or a synthetic view name
/// like `helm`); values are `HEADER` or `HEADER:desc`, the same spec format
/// bookmarks use, so the file stays hand-editable.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SortMemory {
    pub kinds: BTreeMap<String, String>,
}

impl SortMemory {
    /// Where remembered sorts live: `<state-dir>/sort.toml`.
    pub fn default_path() -> PathBuf {
        crate::diagnostics::state_dir().join("sort.toml")
    }

    /// Load persisted sorts. A missing or unparsable file is an empty set —
    /// startup must not fail over hand-mangled state.
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

    /// Remember `header` (+ direction) for `kind`.
    pub fn set(&mut self, kind: &str, header: &str, desc: bool) {
        let spec = if desc {
            format!("{header}:desc")
        } else {
            header.to_string()
        };
        self.kinds.insert(kind.to_string(), spec);
    }

    /// Forget `kind`'s entry. Returns whether one existed.
    pub fn clear(&mut self, kind: &str) -> bool {
        self.kinds.remove(kind).is_some()
    }

    /// The remembered `(HEADER, desc)` for `kind`, if any. Parses the same
    /// `COLUMN[:asc|:desc]` spec bookmarks accept, uppercased to match
    /// display headers.
    pub fn get(&self, kind: &str) -> Option<(String, bool)> {
        let spec = self.kinds.get(kind)?;
        let (name, desc) = match spec.rsplit_once(':') {
            Some((col, "desc")) => (col.trim(), true),
            Some((col, "asc")) => (col.trim(), false),
            _ => (spec.trim(), false),
        };
        Some((name.to_uppercase(), desc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip_and_clear() {
        let mut m = SortMemory::default();
        m.set("pods", "RESTARTS", true);
        m.set("deployments", "READY", false);
        assert_eq!(m.get("pods"), Some(("RESTARTS".into(), true)));
        assert_eq!(m.get("deployments"), Some(("READY".into(), false)));
        assert_eq!(m.get("nodes"), None);
        assert!(m.clear("pods"));
        assert!(!m.clear("pods"));
        assert_eq!(m.get("pods"), None);
    }

    #[test]
    fn get_parses_hand_edited_specs() {
        let mut m = SortMemory::default();
        m.kinds.insert("pods".into(), "age:desc".into());
        m.kinds.insert("nodes".into(), "cpu:asc".into());
        m.kinds.insert("jobs".into(), "completions".into());
        assert_eq!(m.get("pods"), Some(("AGE".into(), true)));
        assert_eq!(m.get("nodes"), Some(("CPU".into(), false)));
        assert_eq!(m.get("jobs"), Some(("COMPLETIONS".into(), false)));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sofka-sortmem-{}", std::process::id()));
        let path = dir.join("sort.toml");
        let mut m = SortMemory::default();
        m.set("pods", "RESTARTS", true);
        m.save(&path).unwrap();
        assert_eq!(SortMemory::load(&path), m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_or_garbage_is_empty() {
        assert_eq!(
            SortMemory::load(Path::new("/nonexistent/sort.toml")),
            SortMemory::default()
        );
    }
}
