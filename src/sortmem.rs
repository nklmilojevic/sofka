//! Remembered sort choices, persisted across restarts.
//!
//! Picking a sort column (`S`, a header click) or flipping direction (`I`)
//! records the choice per resource kind in `<state-dir>/sort.toml`, so the
//! next visit to that kind — in this session or the next — comes back sorted
//! the same way. Picking the pinned default ordering forgets the entry. Like
//! fleet marks, this is deliberately a separate state file: sofka never
//! rewrites the user's config, and a `[views] sort` there stays the
//! hand-edited fallback these choices overlay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-kind sort memory. Keys are the kind plural (or a synthetic view name
/// like `helm`); values are `HEADER` or `HEADER:desc`, the same spec format
/// bookmarks use, so the file stays hand-editable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SortMemory {
    pub kinds: BTreeMap<String, String>,
    #[serde(skip)]
    dirty: BTreeSet<String>,
}

impl PartialEq for SortMemory {
    fn eq(&self, other: &Self) -> bool {
        self.kinds == other.kinds
    }
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

    /// Merge changed keys into the current file and replace it atomically.
    /// Simultaneous saves from separate processes can still lose changes
    /// between the read and the write. The file is always complete.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut merged = Self::load(path);
        merged.merge_pending(self);
        let text = toml::to_string(&merged).map_err(|e| e.to_string())?;
        crate::atomicfile::write(path, &text)
    }

    /// Apply newer pending changes without losing earlier pending keys.
    pub(crate) fn merge_pending(&mut self, newer: &Self) {
        for key in &newer.dirty {
            match newer.kinds.get(key) {
                Some(value) => self.kinds.insert(key.clone(), value.clone()),
                None => self.kinds.remove(key),
            };
        }
        self.dirty.extend(newer.dirty.iter().cloned());
    }

    /// Take the pending changes for one save.
    pub fn snapshot(&mut self) -> Self {
        let snapshot = self.clone();
        self.dirty.clear();
        snapshot
    }

    /// Remember `header` (+ direction) for `kind`.
    pub fn set(&mut self, kind: &str, header: &str, desc: bool) {
        let spec = if desc {
            format!("{header}:desc")
        } else {
            header.to_string()
        };
        self.kinds.insert(kind.to_string(), spec);
        self.dirty.insert(kind.to_string());
    }

    /// Forget `kind`'s entry. Returns whether one existed.
    pub fn clear(&mut self, kind: &str) -> bool {
        let removed = self.kinds.remove(kind).is_some();
        if removed {
            self.dirty.insert(kind.to_string());
        }
        removed
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

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sofka-sortmem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn two_processes_keep_each_others_sorts() {
        let dir = scratch("two-processes");
        let path = dir.join("sort.toml");
        let mut a = SortMemory::load(&path);
        let mut b = SortMemory::load(&path);
        a.set("pods", "AGE", true);
        a.snapshot().save(&path).unwrap();
        b.set("deployments", "READY", false);
        b.snapshot().save(&path).unwrap();
        let saved = SortMemory::load(&path);
        assert_eq!(saved.get("pods"), Some(("AGE".into(), true)));
        assert_eq!(saved.get("deployments"), Some(("READY".into(), false)));
        assert!(saved.dirty.is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[kinds]\ndeployments = \"READY\"\npods = \"AGE:desc\"\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clearing_a_sort_removes_it_from_the_shared_file() {
        let dir = scratch("clear");
        let path = dir.join("sort.toml");
        let mut a = SortMemory::load(&path);
        a.set("pods", "AGE", true);
        a.snapshot().save(&path).unwrap();
        let mut b = SortMemory::load(&path);
        a.set("deployments", "READY", false);
        a.snapshot().save(&path).unwrap();
        assert!(b.clear("pods"));
        b.snapshot().save(&path).unwrap();
        let saved = SortMemory::load(&path);
        assert_eq!(saved.get("pods"), None);
        assert_eq!(saved.get("deployments"), Some(("READY".into(), false)));
        a.set("pods", "NAME", false);
        a.snapshot().save(&path).unwrap();
        b.snapshot().save(&path).unwrap();
        assert_eq!(
            SortMemory::load(&path).get("pods"),
            Some(("NAME".into(), false))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_clears_the_pending_changes() {
        let dir = scratch("snapshot");
        let path = dir.join("sort.toml");
        let mut a = SortMemory::load(&path);
        a.set("pods", "AGE", true);
        a.snapshot().save(&path).unwrap();
        let mut b = SortMemory::load(&path);
        b.set("pods", "NAME", false);
        b.snapshot().save(&path).unwrap();
        a.snapshot().save(&path).unwrap();
        assert_eq!(
            SortMemory::load(&path).get("pods"),
            Some(("NAME".into(), false))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

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
