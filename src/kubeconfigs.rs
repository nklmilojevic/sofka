//! Kubeconfig sources: which files sofka reads contexts from, and the
//! identity of a cluster inside them.
//!
//! kubectl merges every file in `$KUBECONFIG` into one namespace of context
//! names, and the first file to define a name wins — a context in a second
//! file with a name the first already used is simply invisible. sofka keeps
//! that merged view as one *source* ([`Source::Default`]) so nothing changes
//! for people who never add a file, and lets extra files be added at runtime
//! as their own sources. A context is then identified by [`ClusterId`], the
//! pair of the source it came from and its name, so two files that both call
//! a context `prod` stay distinguishable everywhere: in the switcher, in
//! remembered namespaces, in fleet membership, and in `kubectl` shell-outs.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use kube::config::Kubeconfig;
use serde::{Deserialize, Serialize};

/// Which kubeconfig a context was read from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// Whatever `kubectl` itself would read: `$KUBECONFIG` (a merged list when
    /// it names several files) or `~/.kube/config`. Resolved by kube-rs, so
    /// its merge rules — including which duplicate name wins — are kubectl's.
    #[default]
    Default,
    /// An extra file, added with `:kubeconfig` or listed in `[kubeconfigs]`.
    /// Read on its own, never merged, so its contexts cannot be shadowed by
    /// (or shadow) another file's.
    File(PathBuf),
}

impl Source {
    /// The file to pass to `kubectl --kubeconfig`, or `None` when the default
    /// resolution (no flag) is already correct.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Default => None,
            Self::File(p) => Some(p),
        }
    }

    /// Parse this source's kubeconfig. A file source is read alone; the
    /// default source goes through kube-rs so `$KUBECONFIG` merging applies.
    pub fn read(&self) -> Result<Kubeconfig, String> {
        match self {
            Self::Default => Kubeconfig::read().map_err(|e| format!("reading kubeconfig: {e}")),
            Self::File(p) => {
                Kubeconfig::read_from(p).map_err(|e| format!("reading {}: {e}", p.display()))
            }
        }
    }
}

/// A context, qualified by the kubeconfig it lives in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClusterId {
    pub source: Source,
    pub context: String,
}

impl ClusterId {
    /// A context in the default (kubectl-equivalent) kubeconfig.
    pub fn new(context: impl Into<String>) -> Self {
        Self {
            source: Source::Default,
            context: context.into(),
        }
    }

    /// A context in a specific file.
    pub fn in_file(path: impl Into<PathBuf>, context: impl Into<String>) -> Self {
        Self {
            source: Source::File(path.into()),
            context: context.into(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.source.path()
    }

    /// Key for persisted per-cluster state (remembered namespaces, fleet
    /// membership). A default-source context keys on its bare name, so state
    /// written before kubeconfig sources existed still resolves; anything else
    /// is qualified by absolute path, which no context name can collide with
    /// (`@` is legal in neither a kubeconfig context name nor a DNS label, and
    /// even an EKS ARN — full of `:` and `/` — has none).
    pub fn state_key(&self) -> String {
        match &self.source {
            Source::Default => self.context.clone(),
            Source::File(p) => format!("{}@{}", self.context, p.display()),
        }
    }

    /// Inverse of [`Self::state_key`], for reading persisted state back.
    pub fn from_state_key(key: &str) -> Self {
        match key.rsplit_once('@') {
            Some((context, path)) if !path.is_empty() => Self::in_file(path, context),
            _ => Self::new(key),
        }
    }
}

/// A bare context name means a context in the default kubeconfig — the same
/// reading `kubectl --context` gives it.
impl From<&str> for ClusterId {
    fn from(context: &str) -> Self {
        Self::new(context)
    }
}

impl From<String> for ClusterId {
    fn from(context: String) -> Self {
        Self::new(context)
    }
}

/// Extra kubeconfig files and directories added in the TUI (`a`/`d` in
/// `:kubeconfig`), persisted to `<state-dir>/kubeconfigs.toml`. A separate
/// state file for the same reason fleet marks are: sofka never rewrites the
/// user's config, so `[kubeconfigs] paths` stays the hand-edited base list and
/// these overlay it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceMarks {
    /// Paths added on top of `[kubeconfigs] paths`, in the order added.
    pub added: Vec<String>,
    /// Config-listed paths removed with `d`.
    pub removed: Vec<String>,
}

impl SourceMarks {
    /// Where marks live: `<state-dir>/kubeconfigs.toml`.
    pub fn default_path() -> PathBuf {
        crate::diagnostics::state_dir().join("kubeconfigs.toml")
    }

    /// Load persisted marks. A missing or unparsable file is an empty set —
    /// the switcher must still open when state was never written or got
    /// hand-mangled.
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

    /// Add `path` — a kubeconfig file, or a directory of them — or report why
    /// it can't be used. Returns whether anything changed, so callers can skip
    /// the disk write.
    pub fn add(&mut self, path: &str) -> Result<bool, String> {
        let path = expand(path);
        let text = path.to_string_lossy().to_string();
        if text.is_empty() {
            return Err("empty path".into());
        }
        if path.is_dir() {
            if scan_dir(&path).is_empty() {
                return Err(format!("{text}: no kubeconfigs in this directory"));
            }
        } else if path.is_file() {
            Kubeconfig::read_from(&path).map_err(|e| format!("{text}: {e}"))?;
        } else {
            return Err(format!("{text}: no such file or directory"));
        }
        self.removed.retain(|p| *p != text);
        if self.added.contains(&text) {
            return Ok(false);
        }
        self.added.push(text);
        Ok(true)
    }

    /// Drop `path` from the active set: an added path is forgotten, a
    /// config-listed one is masked. Returns whether anything changed.
    pub fn remove(&mut self, path: &Path) -> bool {
        let text = path.to_string_lossy().to_string();
        let had = self.added.len();
        self.added.retain(|p| *p != text);
        if self.added.len() != had {
            return true;
        }
        if self.removed.contains(&text) {
            return false;
        }
        self.removed.push(text);
        true
    }
}

/// The extra sources in effect: `[kubeconfigs] paths` minus removals, plus
/// session additions, in that order, deduplicated. Entries may be files or
/// directories. Paths are expanded (`~`) but not required to exist — one that
/// vanished is reported when its contexts are listed, which is more useful
/// than silently dropping it from the picker.
pub fn active_paths(configured: &[String], marks: &SourceMarks) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let removed: Vec<PathBuf> = marks.removed.iter().map(|p| expand(p)).collect();
    for path in configured.iter().chain(marks.added.iter()) {
        let path = expand(path);
        if removed.contains(&path) || out.contains(&path) {
            continue;
        }
        out.push(path);
    }
    out
}

/// How deep a directory source is walked. Kubeconfig collections are usually
/// one or two levels (`~/.kube/configs/<provider>/<cluster>`); a deeper walk
/// would start costing real time on a mistyped path like `~`.
const MAX_DEPTH: usize = 3;
/// Ceiling on files taken from one directory source, so a wrong path cannot
/// turn the switcher into a filesystem crawl.
const MAX_DIR_FILES: usize = 500;

/// Every kubeconfig file a source contributes: the file itself, or the
/// kubeconfigs found under a directory (sorted, so the picker's order is
/// stable across runs). Files that don't parse as a kubeconfig are skipped
/// silently — a directory of mixed YAML is normal, and warning per stray file
/// would bury the real errors.
pub fn files_in(path: &Path) -> Vec<PathBuf> {
    if path.is_dir() {
        scan_dir(path)
    } else {
        vec![path.to_path_buf()]
    }
}

fn scan_dir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut queue = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
        children.sort();
        for child in children {
            if out.len() >= MAX_DIR_FILES {
                return out;
            }
            // Dotfiles are editor swap files and VCS metadata; backups are a
            // stale copy of a kubeconfig that is already listed, and would
            // otherwise appear as a second source for the same contexts.
            if child
                .file_name()
                .is_some_and(|n| is_noise(&n.to_string_lossy()))
            {
                continue;
            }
            if child.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    queue.push((child, depth + 1));
                }
            } else if Kubeconfig::read_from(&child).is_ok_and(|k| !k.contexts.is_empty()) {
                out.push(child);
            }
        }
    }
    out.sort();
    out
}

/// Names that are never a kubeconfig you meant to switch to: hidden files, and
/// the backup and swap copies editors and `cp` leave beside a real one. They
/// parse fine, so only the name can tell them apart.
fn is_noise(name: &str) -> bool {
    const BACKUP_MARKERS: &[&str] = &[".bak", ".backup", ".orig", ".old", ".save", ".swp", ".tmp"];
    name.starts_with('.')
        || name.ends_with('~')
        || BACKUP_MARKERS.iter().any(|marker| name.contains(marker))
}

/// Expand a leading `~` so config and typed paths behave like the shell.
fn expand(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(trimmed)
}

/// One selectable context, with the metadata the switcher shows so you can
/// tell same-named contexts in different files apart at a glance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: ClusterId,
    /// Kubeconfig `cluster:` name the context points at.
    pub cluster: String,
    /// API-server URL. Never carries credentials.
    pub server: String,
    /// Namespace the context pins, if any.
    pub namespace: Option<String>,
    /// Source label shown in the picker: empty for the default kubeconfig,
    /// else the shortest unique tail of the file's path.
    pub source_label: String,
    /// How the context is written in `:ctx <name>` and everywhere else it is
    /// named. The bare context name, unless another kubeconfig defines that
    /// name too — then it is qualified as `context@file`. Set by [`list`],
    /// which is the only place that can see the whole set.
    pub label: String,
    /// This context is its own file's `current-context`.
    pub current: bool,
}

impl Entry {
    /// A context in the default kubeconfig with nothing else known about it.
    /// The switcher renders the metadata columns empty rather than guessing.
    pub fn named(context: impl Into<String>) -> Self {
        let context = context.into();
        Self {
            id: ClusterId::new(context.clone()),
            cluster: String::new(),
            server: String::new(),
            namespace: None,
            source_label: String::new(),
            label: context,
            current: false,
        }
    }

    pub fn context(&self) -> &str {
        &self.id.context
    }

    /// What type-to-filter matches against: always the fully qualified form,
    /// so typing a kubeconfig's name narrows to its contexts even when their
    /// displayed labels are bare.
    pub fn search_key(&self) -> String {
        if self.source_label.is_empty() {
            self.id.context.clone()
        } else {
            format!("{}@{}", self.id.context, self.source_label)
        }
    }

    /// Whether this context needed qualifying to stay distinct — the picker
    /// shows the source column for exactly these, since for everything else
    /// it repeats what the name already says.
    pub fn qualified(&self) -> bool {
        self.label != self.id.context
    }
}

/// Resolve a user-typed context label (`prod`, or `prod@work` for an added
/// file) against the known contexts. A label nothing matches is taken as a
/// context in the default kubeconfig: that is what someone typing a name the
/// cache has not seen means, and connecting will report it properly if it
/// does not exist.
pub fn resolve_label(entries: &[Entry], label: &str) -> ClusterId {
    entries
        .iter()
        .find(|e| e.label == label)
        .map(|e| e.id.clone())
        .unwrap_or_else(|| ClusterId::new(label))
}

/// Every context in every active source, default kubeconfig first and the
/// extra sources in configured order, sorted by name within each. A directory
/// source contributes every kubeconfig under it. Also returns one warning per
/// source that could not be read — an unreadable file must say so, since an
/// empty picker over a parse error looks like "you have no contexts".
pub fn list(paths: &[PathBuf]) -> (Vec<Entry>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for path in paths {
        let found = files_in(path);
        if found.is_empty() {
            warnings.push(format!("{}: no kubeconfigs found", path.display()));
        }
        for file in found {
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    let labels = short_labels(&files);
    let mut sources: Vec<(Source, String)> = vec![(Source::Default, String::new())];
    sources.extend(
        files
            .iter()
            .map(|p| (Source::File(p.clone()), labels[p].clone())),
    );

    let mut entries = Vec::new();
    for (source, source_label) in sources {
        let config = match source.read() {
            Ok(c) => c,
            Err(e) => {
                warnings.push(e);
                continue;
            }
        };
        let servers: HashMap<&str, &str> = config
            .clusters
            .iter()
            .filter_map(|c| Some((c.name.as_str(), c.cluster.as_ref()?.server.as_deref()?)))
            .collect();
        let current = config.current_context.as_deref().unwrap_or_default();
        let mut from_source: Vec<Entry> = config
            .contexts
            .iter()
            .map(|named| {
                let ctx = named.context.as_ref();
                let cluster = ctx.map(|c| c.cluster.clone()).unwrap_or_default();
                Entry {
                    id: ClusterId {
                        source: source.clone(),
                        context: named.name.clone(),
                    },
                    server: servers
                        .get(cluster.as_str())
                        .copied()
                        .unwrap_or("")
                        .to_string(),
                    cluster,
                    namespace: ctx.and_then(|c| c.namespace.clone()),
                    source_label: source_label.clone(),
                    label: named.name.clone(),
                    current: named.name == current,
                }
            })
            .collect();
        from_source.sort_by(|a, b| a.id.context.cmp(&b.id.context));
        entries.extend(from_source);
    }
    // Qualify a name only where it is genuinely ambiguous. A directory of
    // per-cluster kubeconfigs — one context each, file named after it — would
    // otherwise read `prod@prod` on every row and, worse, `:ctx prod` would
    // match nothing. The bare name always means the default kubeconfig, as it
    // does for kubectl, so only the added sources ever grow a suffix.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for entry in &entries {
        *seen.entry(entry.id.context.as_str()).or_default() += 1;
    }
    let ambiguous: Vec<String> = seen
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name.to_string())
        .collect();
    for entry in &mut entries {
        if !entry.source_label.is_empty() && ambiguous.contains(&entry.id.context) {
            entry.label = format!("{}@{}", entry.id.context, entry.source_label);
        }
    }
    (entries, warnings)
}

/// Shortest unique tail of each path, so the switcher shows `work` rather than
/// `/home/me/.kube/work.yaml`, and `dev/config` vs `prod/config` when the file
/// names alone would collide. The extension is dropped from the last component
/// because `.yaml`/`.yml`/none carries no information here.
fn short_labels(files: &[PathBuf]) -> BTreeMap<PathBuf, String> {
    let parts: Vec<Vec<String>> = files
        .iter()
        .map(|p| {
            let mut comps: Vec<String> = p
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect();
            if let Some(last) = comps.last_mut()
                && let Some(stem) = Path::new(last.as_str()).file_stem()
            {
                *last = stem.to_string_lossy().to_string();
            }
            comps
        })
        .collect();

    let mut out = BTreeMap::new();
    for (i, path) in files.iter().enumerate() {
        let mut depth = 1;
        let label = loop {
            let candidate = tail(&parts[i], depth);
            let unique = parts
                .iter()
                .enumerate()
                .all(|(j, other)| i == j || tail(other, depth) != candidate);
            if unique || depth >= parts[i].len() {
                break candidate;
            }
            depth += 1;
        };
        out.insert(path.clone(), label);
    }
    out
}

fn tail(parts: &[String], depth: usize) -> String {
    let start = parts.len().saturating_sub(depth);
    parts[start..].join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(added: &[&str], removed: &[&str]) -> SourceMarks {
        SourceMarks {
            added: added.iter().map(|s| s.to_string()).collect(),
            removed: removed.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sofka-kubeconfigs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_source_keys_on_the_bare_context_name() {
        // State written before kubeconfig sources existed keys on the context
        // name alone; that state must keep resolving.
        let id = ClusterId::new("prod");
        assert_eq!(id.state_key(), "prod");
        assert_eq!(ClusterId::from_state_key("prod"), id);
    }

    #[test]
    fn file_sourced_contexts_get_distinct_state_keys() {
        let a = ClusterId::in_file("/home/me/work.yaml", "prod");
        let b = ClusterId::in_file("/home/me/home.yaml", "prod");
        assert_ne!(a.state_key(), b.state_key());
        assert_eq!(ClusterId::from_state_key(&a.state_key()), a);
        assert_eq!(ClusterId::from_state_key(&b.state_key()), b);
    }

    #[test]
    fn context_names_containing_at_round_trip() {
        // Splitting on the last `@` keeps a context name that contains one.
        let id = ClusterId::in_file("/tmp/k.yaml", "user@cluster");
        assert_eq!(ClusterId::from_state_key(&id.state_key()), id);
        assert_eq!(
            ClusterId::from_state_key("user@cluster"),
            ClusterId::in_file("cluster", "user"),
        );
    }

    #[test]
    fn marks_overlay_the_configured_list() {
        let configured = vec!["/a.yaml".to_string(), "/b.yaml".to_string()];
        let files = active_paths(&configured, &marks(&["/c.yaml"], &["/b.yaml"]));
        assert_eq!(
            files,
            vec![PathBuf::from("/a.yaml"), PathBuf::from("/c.yaml")]
        );
    }

    #[test]
    fn duplicate_files_are_listed_once() {
        let configured = vec!["/a.yaml".to_string()];
        let files = active_paths(&configured, &marks(&["/a.yaml"], &[]));
        assert_eq!(files, vec![PathBuf::from("/a.yaml")]);
    }

    #[test]
    fn removing_a_session_added_file_forgets_it_instead_of_masking_it() {
        let mut m = marks(&["/a.yaml"], &[]);
        assert!(m.remove(Path::new("/a.yaml")));
        assert!(m.added.is_empty());
        // Never config-listed, so nothing to mask.
        assert!(m.removed.is_empty());
    }

    #[test]
    fn removing_a_configured_file_masks_it() {
        let mut m = SourceMarks::default();
        assert!(m.remove(Path::new("/a.yaml")));
        assert_eq!(m.removed, vec!["/a.yaml".to_string()]);
        // Idempotent: a second removal changes nothing.
        assert!(!m.remove(Path::new("/a.yaml")));
    }

    #[test]
    fn re_adding_a_masked_file_unmasks_it() {
        let path = scratch("unmask").join("k.yaml");
        std::fs::write(&path, "apiVersion: v1\nkind: Config\ncontexts: []\n").unwrap();
        let text = path.to_string_lossy().to_string();
        let mut m = marks(&[], &[&text]);
        assert!(m.add(&text).unwrap());
        assert!(m.removed.is_empty());
        assert_eq!(m.added, vec![text]);
    }

    #[test]
    fn adding_rejects_paths_that_are_not_kubeconfigs() {
        let dir = scratch("reject");
        let missing = dir.join("nope.yaml");
        let mut m = SourceMarks::default();
        assert!(m.add(&missing.to_string_lossy()).is_err());

        let junk = dir.join("junk.yaml");
        std::fs::write(&junk, "[not: valid: yaml").unwrap();
        assert!(m.add(&junk.to_string_lossy()).is_err());
        assert!(m.added.is_empty());
    }

    #[test]
    fn labels_shorten_to_the_file_stem_until_they_collide() {
        let files = vec![
            PathBuf::from("/home/me/.kube/work.yaml"),
            PathBuf::from("/home/me/.kube/home.yaml"),
        ];
        let labels = short_labels(&files);
        assert_eq!(labels[&files[0]], "work");
        assert_eq!(labels[&files[1]], "home");
    }

    #[test]
    fn colliding_file_names_grow_a_directory_of_context() {
        let files = vec![
            PathBuf::from("/clusters/dev/config"),
            PathBuf::from("/clusters/prod/config"),
        ];
        let labels = short_labels(&files);
        assert_eq!(labels[&files[0]], "dev/config");
        assert_eq!(labels[&files[1]], "prod/config");
    }

    #[test]
    fn a_directory_skips_backups_hidden_files_and_non_kubeconfigs() {
        let dir = scratch("scan");
        let config = "apiVersion: v1\nkind: Config\ncontexts:\n  - name: a\n    context:\n      cluster: c\n";
        for name in [
            "prod.config",
            "prod.config.bak-20260811",
            "prod.config~",
            ".hidden.config",
        ] {
            std::fs::write(dir.join(name), config).unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "not a kubeconfig").unwrap();
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested").join("dev.config"), config).unwrap();

        let found = files_in(&dir);
        assert_eq!(
            found,
            vec![
                dir.join("nested").join("dev.config"),
                dir.join("prod.config")
            ],
            "a backup copy is not a second cluster"
        );
    }

    #[test]
    fn lists_contexts_with_server_and_namespace_from_the_file() {
        let path = scratch("list").join("extra.yaml");
        std::fs::write(
            &path,
            r#"
apiVersion: v1
kind: Config
current-context: beta
clusters:
  - name: c1
    cluster:
      server: https://beta.example:6443
contexts:
  - name: beta
    context:
      cluster: c1
      namespace: apps
  - name: alpha
    context:
      cluster: c1
"#,
        )
        .unwrap();

        let (entries, warnings) = list(std::slice::from_ref(&path));
        assert!(warnings.is_empty(), "{warnings:?}");
        let from_file: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.id.path() == Some(path.as_path()))
            .collect();
        // Sorted by name within the source.
        assert_eq!(
            from_file.iter().map(|e| e.context()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        let beta = from_file[1];
        assert_eq!(beta.server, "https://beta.example:6443");
        assert_eq!(beta.namespace.as_deref(), Some("apps"));
        assert!(beta.current);
        assert!(!from_file[0].current);
        assert_eq!(beta.label, "beta");
    }

    #[test]
    fn an_unreadable_source_warns_instead_of_vanishing() {
        let path = scratch("missing").join("gone.yaml");
        let (entries, warnings) = list(std::slice::from_ref(&path));
        assert!(
            warnings.iter().any(|w| w.contains("gone.yaml")),
            "{warnings:?}"
        );
        assert!(
            !entries.iter().any(|e| e.id.path() == Some(path.as_path())),
            "a source that could not be read contributes no contexts"
        );
    }
}
