//! Safe installation records and filesystem activation for catalog plugins.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::plugin_catalog::{self, CatalogSnapshot};

fn official_source() -> String {
    "official".into()
}

const RECORD: &str = ".sofka-install.json";
const STAGE_MARKER: &str = ".sofka-install-stage";
const RECORD_SCHEMA: u32 = 1;
/// Directories under this name are committed for deletion: whatever state an
/// interruption left them in, recovery discards them.
const REMOVED_PREFIX: &str = ".plugin-removed-";
const TRASH: &str = ".plugin-trash";
const TRASH_MARKER: &str = ".sofka-trash";
const RECORD_MAX_BYTES: usize = 1024 * 1024;
const EXPANDED_MAX_BYTES: u64 = 200 * 1024 * 1024;
const FILE_MAX: usize = 2_000;
/// A package may hold thousands of files; an error message may not.
const MODIFIED_LISTED: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationRecord {
    pub schema_version: u32,
    pub id: String,
    pub package_version: String,
    pub catalog_commit: String,
    #[serde(default = "official_source")]
    pub catalog_source: String,
    pub source_commit: String,
    pub artifact_digest: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledPackage {
    pub catalog_source: String,
    pub id: String,
    pub version: Option<String>,
    pub path: PathBuf,
    pub managed: bool,
    pub modified: bool,
}

pub struct InstallLock {
    file: File,
}

impl InstallLock {
    pub fn acquire(config: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(config)
            .map_err(|e| format!("creating {}: {e}", config.display()))?;
        let path = config.join(".plugin-install.lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        file.try_lock_exclusive().map_err(|e| {
            format!(
                "another sofka plugin operation holds {}: {e}",
                path.display()
            )
        })?;
        recover(config)?;
        Ok(Self { file })
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug)]
pub struct PreparedPackage {
    pub id: String,
    pub version: String,
    pub previous_version: Option<String>,
    /// Package directories whose plugin name or palette command collides with
    /// this one. The loader keeps the directory it reads first, so a collision
    /// hides one of the two packages.
    pub conflicts: Vec<PathBuf>,
    stage: Option<PathBuf>,
    destination: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Installed,
    Updated,
    RolledBack,
    Unchanged,
}

impl PreparedPackage {
    /// Build one without running `prepare`, so the activation contract can be
    /// driven from tests in either module. A stage that does not exist models
    /// the filesystem failing partway through a batch.
    #[cfg(test)]
    pub(crate) fn staged(
        id: &str,
        version: &str,
        previous: Option<&str>,
        stage: PathBuf,
        destination: PathBuf,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            previous_version: previous.map(str::to_owned),
            conflicts: Vec::new(),
            stage: Some(stage),
            destination,
        }
    }

    /// The "already installed and intact" outcome, which preparation models by
    /// staging nothing at all.
    #[cfg(test)]
    pub(crate) fn unchanged(id: &str, version: &str, destination: PathBuf) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            previous_version: Some(version.into()),
            conflicts: Vec::new(),
            stage: None,
            destination,
        }
    }

    pub fn activate(mut self) -> Result<Activation, String> {
        // Cloned, not taken: every step below here can fail, and `Drop` only
        // clears the staging directory while this still owns it.
        let Some(stage) = self.stage.clone() else {
            return Ok(Activation::Unchanged);
        };
        let action = match self.previous_version.as_deref() {
            None => Activation::Installed,
            Some(old) => match (
                semver::Version::parse(old),
                semver::Version::parse(&self.version),
            ) {
                (Ok(old), Ok(new)) if new < old => Activation::RolledBack,
                _ => Activation::Updated,
            },
        };
        let config = self
            .destination
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "invalid plugin destination".to_string())?;
        let backup = unique_path(config, &format!(".plugin-backup-{}", self.id));
        // Preparation can be slow — a download, an extraction, a batch of other
        // packages — and the destination is not locked against its owner in
        // that window. Anything edited since must not be silently discarded.
        let had_destination = match inspect_destination(&self.destination, &self.id) {
            Ok(current) => current.is_some(),
            Err(error) => return Err(format!("refusing to replace {}: {error}", self.id)),
        };
        // Claim the cleanup directory before moving either package. Otherwise
        // discovering an unowned directory after the swap reports failure even
        // though the new package is already active and cannot be rolled back.
        let trash = had_destination
            .then(|| trash(config))
            .transpose()
            .map_err(|e| format!("refusing to replace {}: {e}", self.id))?;
        if had_destination {
            std::fs::rename(&self.destination, &backup).map_err(|e| {
                format!(
                    "staging previous {} as {}: {e}",
                    self.destination.display(),
                    backup.display()
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&stage, &self.destination) {
            if had_destination {
                let _ = std::fs::rename(&backup, &self.destination);
            }
            return Err(format!(
                "activating {} at {}: {error}",
                self.id,
                self.destination.display()
            ));
        }
        // The stage is the destination now, so there is nothing left to clean
        // up under its old name.
        self.stage = None;
        std::fs::remove_file(self.destination.join(STAGE_MARKER)).map_err(|e| {
            format!(
                "{} was activated, but removing its staging marker failed: {e}",
                self.id
            )
        })?;
        if had_destination {
            // Rename first: a half-deleted backup is unidentifiable, so recovery
            // must see it under a name that means "already replaced".
            let trash = trash.expect("cleanup was prepared before replacement");
            let discarded = unique_path(&trash, &format!("{REMOVED_PREFIX}{}", self.id));
            std::fs::rename(&backup, &discarded).map_err(|e| {
                format!(
                    "{} was activated, but retiring {} failed: {e}",
                    self.id,
                    backup.display()
                )
            })?;
            std::fs::remove_dir_all(&discarded).map_err(|e| {
                format!(
                    "{} was activated, but cleanup of {} failed: {e}",
                    self.id,
                    discarded.display()
                )
            })?;
        }
        Ok(action)
    }
}

impl Drop for PreparedPackage {
    fn drop(&mut self) {
        if let Some(stage) = self.stage.take() {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
}

pub async fn prepare(
    snapshot: &CatalogSnapshot,
    requests: &[String],
    offline: bool,
) -> Result<Vec<PreparedPackage>, String> {
    prepare_below(
        &plugin_catalog::config_dir()?,
        &plugin_catalog::cache_dir(),
        snapshot,
        requests,
        offline,
    )
    .await
}

async fn prepare_below(
    config: &Path,
    cache: &Path,
    snapshot: &CatalogSnapshot,
    requests: &[String],
    offline: bool,
) -> Result<Vec<PreparedPackage>, String> {
    let plugins = config.join("plugins");
    std::fs::create_dir_all(&plugins)
        .map_err(|e| format!("creating {}: {e}", plugins.display()))?;

    let mut requested: HashMap<String, String> = HashMap::new();
    let mut selections = Vec::new();
    for request in requests {
        let selection = snapshot.catalog.select(request)?;
        if let Some(previous) = requested.insert(
            selection.plugin.id.clone(),
            selection.version.version.clone(),
        ) && previous != selection.version.version
        {
            return Err(format!(
                "conflicting versions requested for {}: {previous} and {}",
                selection.plugin.id, selection.version.version
            ));
        }
        if selections
            .iter()
            .any(|(id, _, _, _, _): &(String, _, _, _, _)| id == &selection.plugin.id)
        {
            continue;
        }
        selections.push((
            selection.plugin.id.clone(),
            selection.version,
            selection.artifact,
            &selection.plugin.source,
            &selection.plugin.revision,
        ));
    }

    let mut prepared = Vec::new();
    // Manifests prepared so far in this batch, by package ID.
    let mut batch: Vec<(String, Vec<crate::config::Plugin>)> = Vec::new();
    for (id, release, artifact, source, revision) in selections {
        let version = release.version.clone();
        let destination = plugins.join(&id);
        let previous = inspect_destination(&destination, &id)?;
        if let Some(record) = previous.as_ref()
            && record.catalog_source != source.identity()
        {
            return Err(format!(
                "plugin {id} belongs to a different catalog; remove it before changing its source"
            ));
        }
        if let Some(record) = previous.as_ref()
            && record.package_version == version
        {
            if !record
                .artifact_digest
                .eq_ignore_ascii_case(&artifact.blake3)
            {
                return Err(format!(
                    "catalog digest for {id}@{version} differs from the installed immutable version"
                ));
            }
            prepared.push(PreparedPackage {
                id,
                version,
                previous_version: previous.map(|record| record.package_version),
                conflicts: Vec::new(),
                stage: None,
                destination,
            });
            continue;
        }
        let archive = plugin_catalog::artifact_from(cache, artifact, offline, source).await?;
        let stage = unique_path(config, &format!(".plugin-stage-{id}"));
        std::fs::create_dir(&stage).map_err(|e| format!("creating {}: {e}", stage.display()))?;
        std::fs::write(
            stage.join(STAGE_MARKER),
            b"sofka plugin installation staging\n",
        )
        .map_err(|e| format!("marking {}: {e}", stage.display()))?;
        // Extraction hashes every byte it writes, so the record below needs no
        // second pass over the package.
        let staged = extract(archive.path(), &stage).and_then(|files| {
            // The declared manifest, not the resolved one: reconciliation
            // compares the spellings the author published.
            let (declared, published) = crate::plugins::read_package_manifest(&stage)?;
            reconcile(&id, release, &declared, published.as_ref())?;
            crate::plugins::read_package(&stage).map(|package| (files, package))
        });
        let (files, package) = match staged {
            Ok(staged) => staged,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(format!("preparing {id}@{version}: {error}"));
            }
        };
        let mut conflicts = conflicts(&plugins, &destination, &package);
        // Two packages installed in one command never see each other on disk,
        // so compare the staged manifests directly.
        conflicts.extend(
            batch
                .iter()
                .filter(|(_, manifest)| packages_conflict(manifest, &package))
                .map(|(other, _)| plugins.join(other)),
        );
        batch.push((id.clone(), package.clone()));
        let record = InstallationRecord {
            schema_version: RECORD_SCHEMA,
            id: id.clone(),
            package_version: version.clone(),
            catalog_commit: if revision.is_empty() {
                snapshot.commit.clone()
            } else {
                revision.clone()
            },
            catalog_source: source.identity().into(),
            source_commit: release.source_commit.clone(),
            artifact_digest: artifact.blake3.to_ascii_lowercase(),
            files,
        };
        let written = serialize_record(&record)
            .and_then(|json| crate::atomicfile::write(&stage.join(RECORD), &json));
        if let Err(error) = written {
            // Nothing owns the stage until the PreparedPackage below exists, so
            // `Drop` cannot clean up after an early return here.
            let _ = std::fs::remove_dir_all(&stage);
            return Err(format!("preparing {id}@{version}: {error}"));
        }
        prepared.push(PreparedPackage {
            id,
            version,
            previous_version: previous.map(|record| record.package_version),
            conflicts,
            stage: Some(stage),
            destination,
        });
    }
    Ok(prepared)
}

/// The catalog's claims about a release and the package's own manifest have to
/// agree. `describe` reports the catalog while the loader runs the manifest, so
/// a publishing mistake could otherwise advertise a reviewed, non-mutating,
/// unprompted plugin and install one that writes to the cluster unasked.
fn reconcile(
    id: &str,
    release: &plugin_catalog::CatalogVersion,
    declared: &[crate::config::Plugin],
    published: Option<&crate::plugins::Package>,
) -> Result<(), String> {
    let published = published
        .ok_or_else(|| format!("catalog package {id} requires a [package] table in plugin.toml"))?;
    let mut differences = Vec::new();
    let mut compare = |field: &str, manifest: &str, catalog: &str| {
        if manifest != catalog {
            differences.push(format!("{field} {manifest:?}, catalog says {catalog:?}"));
        }
    };
    compare("version", &published.version, &release.version);
    compare(
        "sofka",
        published.sofka.as_deref().unwrap_or(""),
        &release.sofka,
    );
    let actual: Vec<plugin_catalog::CatalogCommand> = declared.iter().map(Into::into).collect();
    match &release.execution {
        plugin_catalog::CatalogExecution::Commands { commands } => {
            if &actual != commands {
                differences.push(
                    "commands differ in identity, arguments, scopes, or execution settings".into(),
                );
            }
        }
        plugin_catalog::CatalogExecution::Legacy(execution) => {
            if actual.len() != 1 || actual[0].execution != *execution {
                differences.push("command, target, output, mutating, confirm, dangerous, or network_load differs".into());
            }
        }
    }
    if differences.is_empty() {
        return Ok(());
    }
    Err(format!(
        "package {id} contradicts the catalog entry it was selected from ({}); \
         the published archive and the index disagree, so neither can be trusted",
        differences.join("; ")
    ))
}

/// The installed packages a freshly staged one would collide with. Package
/// directories load in sorted order and the first plugin name or palette
/// command wins, so either side of a collision can end up unreachable.
fn conflicts(
    plugins: &Path,
    destination: &Path,
    package: &[crate::config::Plugin],
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(plugins) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path != destination && path.is_dir())
        .collect();
    paths.sort();
    paths.retain(|path| {
        crate::plugins::read_package(path).is_ok_and(|other| packages_conflict(&other, package))
    });
    paths
}

fn packages_conflict(left: &[crate::config::Plugin], right: &[crate::config::Plugin]) -> bool {
    left.iter().any(|a| {
        right
            .iter()
            .any(|b| crate::plugins::command_conflicts(a, b))
    })
}

fn inspect_destination(path: &Path, id: &str) -> Result<Option<InstallationRecord>, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("inspecting {}: {e}", path.display())),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "refusing symlinked plugin destination {}",
                path.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "plugin destination {} is not a directory",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    let record = read_record(path).map_err(|error| {
        format!(
            "refusing unmanaged plugin directory {}: {error}; a package sofka did not \
             install is never reported as installed — move or remove it manually",
            path.display()
        )
    })?;
    if record.id != id {
        return Err(format!(
            "installation record in {} belongs to {}, not {id}",
            path.display(),
            record.id
        ));
    }
    verify_record(path, &record)?;
    Ok(Some(record))
}

pub fn installed() -> Result<Vec<InstalledPackage>, String> {
    installed_in(&plugin_catalog::config_dir()?.join("plugins"))
}

/// Installed versions by ID, without hashing a single file. Search and describe
/// report what is installed, never whether it was edited, and verifying every
/// file of every package to answer that costs more than the rest of the command.
pub fn installed_versions() -> Result<BTreeMap<(String, String), String>, String> {
    Ok(scan(&plugin_catalog::config_dir()?.join("plugins"), false)?
        .into_iter()
        .filter(|package| package.managed)
        .filter_map(|package| Some(((package.id, package.catalog_source), package.version?)))
        .collect())
}

fn installed_in(plugins: &Path) -> Result<Vec<InstalledPackage>, String> {
    scan(plugins, true)
}

/// `verify` hashes each package's files to decide whether it still matches its
/// installation record. Callers that only report versions leave it off.
fn scan(plugins: &Path, verify: bool) -> Result<Vec<InstalledPackage>, String> {
    let entries = match std::fs::read_dir(plugins) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("reading {}: {e}", plugins.display())),
    };
    let mut packages = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", plugins.display()))?;
        let path = entry.path();
        let linked = entry
            .file_type()
            .map_err(|e| format!("inspecting {}: {e}", path.display()))?
            .is_symlink();
        // `is_dir` follows the link, as the loader does. Sofka runs a symlinked
        // package, so hiding it from `list` only made it look absent.
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if linked {
            // Reported, never owned: install, update, and removal all refuse a
            // symlinked path, so it can only ever be somebody else's package.
            packages.push(InstalledPackage {
                catalog_source: String::new(),
                id,
                version: None,
                path,
                managed: false,
                modified: false,
            });
            continue;
        }
        match read_record(&path) {
            Ok(record) => packages.push(InstalledPackage {
                catalog_source: record.catalog_source.clone(),
                modified: record.id != id || (verify && verify_record(&path, &record).is_err()),
                id,
                version: Some(record.package_version.clone()),
                path,
                managed: true,
            }),
            Err(_) => {
                let has_record = path.join(RECORD).exists();
                packages.push(InstalledPackage {
                    catalog_source: String::new(),
                    id,
                    version: None,
                    path,
                    managed: has_record,
                    modified: has_record,
                });
            }
        }
    }
    packages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(packages)
}

pub fn managed_ids() -> Result<Vec<String>, String> {
    managed_ids_in(&plugin_catalog::config_dir()?.join("plugins"))
}

fn managed_ids_in(plugins: &Path) -> Result<Vec<String>, String> {
    Ok(installed_in(plugins)?
        .into_iter()
        .filter(|package| package.managed)
        .map(|package| package.id)
        .collect())
}

/// What a refused removal leaves behind: the packages that did come out before
/// it stopped, and why it stopped.
#[derive(Debug)]
pub struct Removal {
    pub removed: Vec<(String, PathBuf)>,
    pub error: String,
}

impl std::fmt::Display for Removal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl From<String> for Removal {
    fn from(error: String) -> Self {
        Self {
            removed: Vec::new(),
            error,
        }
    }
}

pub fn remove(ids: &[String]) -> Result<Vec<(String, PathBuf)>, Removal> {
    remove_below(&plugin_catalog::config_dir()?, ids)
}

fn remove_below(config: &Path, ids: &[String]) -> Result<Vec<(String, PathBuf)>, Removal> {
    let plugins = config.join("plugins");
    let mut checked = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        plugin_catalog::parse_request(id).and_then(|(_, version)| {
            if version.is_some() {
                Err("remove accepts plugin IDs without versions".into())
            } else {
                Ok(())
            }
        })?;
        if !seen.insert(id) {
            continue;
        }
        let path = plugins.join(id);
        let record = inspect_destination(&path, id)?
            .ok_or_else(|| format!("plugin {id} is not installed at {}", path.display()))?;
        checked.push((id.clone(), path, record));
    }
    // A failure partway through must still report what is already gone: the
    // caller cannot tell the user to re-run something that already happened.
    let mut removed = Vec::new();
    let trash = trash(config)?;
    for (id, path, _) in checked {
        let staged = unique_path(&trash, &format!("{REMOVED_PREFIX}{id}"));
        if let Err(e) = std::fs::rename(&path, &staged) {
            return Err(Removal {
                removed,
                error: format!("removing {}: {e}", path.display()),
            });
        }
        // Past the rename the plugin is deactivated, so it counts as removed
        // even if the directory lingers; recovery discards it later.
        removed.push((id.clone(), path));
        if let Err(e) = std::fs::remove_dir_all(&staged) {
            return Err(Removal {
                removed,
                error: format!(
                    "plugin {id} was deactivated, but cleanup of {} failed: {e}",
                    staged.display()
                ),
            });
        }
    }
    Ok(removed)
}

fn read_record(dir: &Path) -> Result<InstallationRecord, String> {
    let path = dir.join(RECORD);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() > RECORD_MAX_BYTES {
        return Err(format!("{} exceeds 1 MiB", path.display()));
    }
    let record: InstallationRecord =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid {}: {e}", path.display()))?;
    if record.schema_version != RECORD_SCHEMA {
        return Err(format!(
            "unsupported installation record in {}",
            path.display()
        ));
    }
    Ok(record)
}

fn serialize_record(record: &InstallationRecord) -> Result<String, String> {
    let json = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    if json.len() > RECORD_MAX_BYTES {
        return Err("installation record exceeds 1 MiB".into());
    }
    Ok(json)
}

fn verify_record(dir: &Path, record: &InstallationRecord) -> Result<(), String> {
    let current = hash_files(dir)?;
    if current == record.files {
        return Ok(());
    }
    // The record stores a digest per file, so say which files moved rather than
    // leaving the user to diff an installation directory by hand.
    let mut changed: Vec<String> = Vec::new();
    for (path, digest) in &current {
        match record.files.get(path) {
            None => changed.push(format!("added {path}")),
            Some(expected) if expected != digest => changed.push(format!("changed {path}")),
            Some(_) => {}
        }
    }
    changed.extend(
        record
            .files
            .keys()
            .filter(|path| !current.contains_key(*path))
            .map(|path| format!("removed {path}")),
    );
    let listed = changed.len().min(MODIFIED_LISTED);
    let rest = changed.len() - listed;
    let mut detail = changed[..listed].join(", ");
    if rest > 0 {
        detail.push_str(&format!(", and {rest} more"));
    }
    Err(format!(
        "plugin {} at {} has local modifications ({detail}); restore it or manage the directory manually",
        record.id,
        dir.display()
    ))
}

/// Hash a file without reading it into memory: BLAKE3 maps it and hashes the
/// pages across every core, which is the whole reason verification is cheap
/// enough to run on every list, update, and removal.
fn digest_file(path: &Path) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(plugin_catalog::hex(hasher.finalize().as_bytes()))
}

fn hash_files(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut files = BTreeMap::new();
    hash_directory(root, root, &mut files)?;
    Ok(files)
}

fn hash_directory(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("inspecting {}: {e}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("plugin contains symlink {}", path.display()));
        }
        if metadata.is_dir() {
            hash_directory(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walk remains below root")
                .to_str()
                .ok_or_else(|| format!("plugin path is not UTF-8: {}", path.display()))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if relative != RECORD && relative != STAGE_MARKER {
                files.insert(relative, digest_file(&path)?);
            }
        } else {
            return Err(format!("plugin contains special file {}", path.display()));
        }
    }
    Ok(())
}

/// Bounds a decompressed stream, so a small archive cannot expand without
/// limit however its bytes are declared.
struct Bounded<R> {
    inner: R,
    remaining: u64,
    limit: u64,
}

impl<R: std::io::Read> std::io::Read for Bounded<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.remaining = self
            .remaining
            .checked_sub(read as u64)
            .ok_or_else(|| std::io::Error::other(beyond_limit(self.limit)))?;
        Ok(read)
    }
}

fn beyond_limit(limit: u64) -> String {
    format!("archive expands beyond {} MiB", limit / (1024 * 1024))
}

/// Hashes what it writes, so extraction and the installation record cost one
/// pass over the package instead of two.
struct Hashing<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: std::io::Write> std::io::Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn extract(archive: &Path, destination: &Path) -> Result<BTreeMap<String, String>, String> {
    extract_bounded(archive, destination, EXPANDED_MAX_BYTES)
}

fn extract_bounded(
    archive: &Path,
    destination: &Path,
    limit: u64,
) -> Result<BTreeMap<String, String>, String> {
    let file = File::open(archive).map_err(|e| format!("opening {}: {e}", archive.display()))?;
    let zstd = zstd::stream::read::Decoder::new(file)
        .map_err(|e| format!("reading {}: {e}", archive.display()))?;
    // The reader budget covers payload, TAR headers, and padding together; the
    // per-entry tally below only turns an oversized archive into a clearer
    // error before its bytes are read.
    let mut archive = tar::Archive::new(Bounded {
        inner: zstd,
        remaining: limit,
        limit,
    });
    let entries = archive
        .entries()
        .map_err(|e| format!("reading archive: {e}"))?;
    let mut paths = HashSet::new();
    let mut files = BTreeMap::new();
    let mut count = 0usize;
    let mut expanded = 0u64;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("reading archive entry: {e}"))?;
        count += 1;
        if count > FILE_MAX {
            return Err(format!("archive contains more than {FILE_MAX} entries"));
        }
        let path = entry
            .path()
            .map_err(|e| format!("invalid archive path: {e}"))?
            .into_owned();
        let kind = entry.header().entry_type();
        let normalized = validate_relative_path(&path, kind.is_dir())?;
        if !paths.insert(normalized.clone()) {
            return Err(format!("duplicate archive path {}", path.display()));
        }
        // Every entry counts, whatever its type: a directory that declares a
        // payload expands the stream exactly as a regular file does.
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| "archive expanded size overflow".to_string())?;
        if expanded > limit {
            return Err(beyond_limit(limit));
        }
        let target = destination.join(&path);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("creating {}: {e}", target.display()))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!(
                "archive contains link or special file {}",
                path.display()
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let output = File::options()
            .create_new(true)
            .write(true)
            .open(&target)
            .map_err(|e| format!("creating {}: {e}", target.display()))?;
        let mut output = Hashing {
            inner: output,
            hasher: blake3::Hasher::new(),
        };
        std::io::copy(&mut entry, &mut output)
            .map_err(|e| format!("extracting {}: {e}", target.display()))?;
        // The validated spelling, not the archive's: one value decides both
        // what is written and what the record says about it.
        files.insert(
            normalized,
            plugin_catalog::hex(output.hasher.finalize().as_bytes()),
        );
        output
            .inner
            .sync_all()
            .map_err(|e| format!("flushing {}: {e}", target.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let source_mode = entry.header().mode().unwrap_or(0);
            let mode = if source_mode & 0o111 == 0 {
                0o644
            } else {
                0o755
            };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .map_err(|e| format!("setting permissions on {}: {e}", target.display()))?;
        }
    }
    if !destination.join("plugin.toml").is_file() {
        return Err("archive has no plugin.toml at its root".into());
    }
    Ok(files)
}

fn validate_relative_path(path: &Path, directory: bool) -> Result<String, String> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe archive path {}", path.display()));
    }
    let raw = path
        .to_str()
        .ok_or_else(|| "archive path is not UTF-8".to_string())?;
    // A backslash is a separator on Windows but a filename character on Unix.
    // Refusing it gives one installation record the same meaning everywhere.
    if raw.contains('\\') {
        return Err(format!(
            "archive path {} contains a backslash",
            path.display()
        ));
    }
    // `bin/./data` has only normal components — Rust drops the `.` while
    // iterating — but it is written to disk as `bin/data`. Recording the
    // spelling from the archive would then never match the file that was
    // created, and the package would look modified the moment it installed.
    // Compared as strings, not as paths: `Path`'s own equality is
    // component-wise, so it considers `bin/./data` equal to `bin/data`.
    let recorded = if directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    let plain = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if plain != recorded {
        return Err(format!(
            "archive path {} is not in its plain form",
            path.display()
        ));
    }
    // Checked here, not on the archive's spelling: a trailing slash made a
    // directory entry miss the name entirely, and on a case-insensitive
    // filesystem a difference in case lands on the same file while the record
    // keeps the archive's spelling, so the package reads as modified the
    // moment it installs.
    if plain.eq_ignore_ascii_case(RECORD) || plain.eq_ignore_ascii_case(STAGE_MARKER) {
        return Err(format!("archive path {plain} is reserved by sofka"));
    }
    Ok(plain)
}

fn ensure_directory_path(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("refusing symlinked directory {}", path.display()))
        }
        Ok(metadata) if !metadata.is_dir() => Err(format!("{} is not a directory", path.display())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspecting {}: {e}", path.display())),
    }
}

fn unique_path(parent: &Path, prefix: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    parent.join(format!("{prefix}-{}-{nanos:x}", std::process::id()))
}

/// Where a removed package waits to be deleted. A removal renames into here
/// and only then deletes, so everything inside is sofka's by construction:
/// recovery can clear it without matching a name or reading a record, and a
/// leftover that lost its record to an interrupted deletion is still cleaned
/// up. Nothing a user put in the config directory can end up in it.
fn trash(config: &Path) -> Result<PathBuf, String> {
    let trash = config.join(TRASH);
    ensure_directory_path(&trash)?;
    match std::fs::create_dir(&trash) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("creating {}: {e}", trash.display())),
    }
    ensure_directory_path(&trash)?;
    let marker = trash.join(TRASH_MARKER);
    if !marker.is_file() {
        // The name alone claims nothing. A directory that is already here and
        // already holds something was put here by someone else, and emptying it
        // would destroy their files; an empty one has nothing to lose.
        if !contents(&trash)?.is_empty() {
            return Err(format!(
                "refusing to use {} for removals: it holds files sofka did not put there; \
                 move or remove it manually",
                trash.display()
            ));
        }
        std::fs::write(&marker, b"sofka deletes everything in this directory\n")
            .map_err(|e| format!("claiming {}: {e}", trash.display()))?;
    }
    Ok(trash)
}

/// Everything in the trash directory except the marker that claims it. Every
/// read error is propagated: an entry this cannot see must not read as an empty
/// directory, because "empty" is what allows sofka to claim someone else's.
fn contents(trash: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    let mut rest = Vec::new();
    for entry in
        std::fs::read_dir(trash).map_err(|e| format!("reading {}: {e}", trash.display()))?
    {
        let entry = entry.map_err(|e| format!("reading {}: {e}", trash.display()))?;
        if entry.file_name() != std::ffi::OsStr::new(TRASH_MARKER) {
            rest.push(entry);
        }
    }
    Ok(rest)
}

fn empty_trash(trash: &Path) -> Result<(), String> {
    if !trash.join(TRASH_MARKER).is_file() {
        // Unclaimed, so not sofka's to empty. A removal refuses it out loud;
        // recovery has no business deleting anything here on its own.
        return Ok(());
    }
    for entry in contents(trash)? {
        let path = entry.path();
        // A symlink reports as neither a directory nor a file here: the link is
        // unlinked, never followed.
        let discarded = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        discarded.map_err(|e| format!("recovering {}: {e}", path.display()))?;
    }
    Ok(())
}

fn recover(config: &Path) -> Result<(), String> {
    let entries = match std::fs::read_dir(config) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("reading {}: {e}", config.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", config.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.starts_with(".plugin-stage-") {
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && path.join(STAGE_MARKER).is_file()
            {
                std::fs::remove_dir_all(&path)
                    .map_err(|e| format!("recovering {}: {e}", path.display()))?;
            }
        } else if name == TRASH {
            // The rename into here committed the removal, so nothing inside is
            // worth keeping — and the marker is what says the directory is
            // sofka's, rather than its name.
            ensure_directory_path(&path)?;
            empty_trash(&path)?;
        } else if name.starts_with(".plugin-backup-") {
            let record = read_record(&path).map_err(|e| {
                format!(
                    "cannot identify interrupted backup {}: {e}; inspect it manually",
                    path.display()
                )
            })?;
            crate::plugin_catalog::parse_request(&record.id)?;
            let destination = config.join("plugins").join(&record.id);
            if destination.exists() {
                read_record(&destination).map_err(|e| {
                    format!(
                        "refusing to discard backup {} because destination {} is unmanaged: {e}",
                        path.display(),
                        destination.display()
                    )
                })?;
                // Rename first, for the same reason activation does: a
                // half-deleted backup is unidentifiable, and recovery would
                // then refuse every later operation.
                let discarded =
                    unique_path(&trash(config)?, &format!("{REMOVED_PREFIX}{}", record.id));
                std::fs::rename(&path, &discarded)
                    .map_err(|e| format!("retiring {}: {e}", path.display()))?;
                std::fs::remove_dir_all(&discarded)
                    .map_err(|e| format!("recovering {}: {e}", discarded.display()))?;
            } else {
                std::fs::create_dir_all(config.join("plugins"))
                    .map_err(|e| format!("recovering plugins directory: {e}"))?;
                std::fs::rename(&path, &destination).map_err(|e| {
                    format!(
                        "restoring {} to {}: {e}",
                        path.display(),
                        destination.display()
                    )
                })?;
            }
        }
    }
    let plugins = config.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins) {
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {}: {e}", plugins.display()))?;
            let path = entry.path();
            let marker = path.join(STAGE_MARKER);
            if marker.is_file() && read_record(&path).is_ok() {
                std::fs::remove_file(&marker)
                    .map_err(|e| format!("recovering {}: {e}", marker.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(path: &Path, entries: &[(&str, &[u8], tar::EntryType)]) {
        let file = File::create(path).unwrap();
        let zstd = zstd::stream::write::Encoder::new(file, 19)
            .unwrap()
            .auto_finish();
        let mut builder = tar::Builder::new(zstd);
        for (name, bytes, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(if *name == "adapter" { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap();
    }

    /// Transient state a finished operation must not leave behind. The trash
    /// directory survives its first use; an empty one is not a leftover.
    fn leftovers(config: &Path) -> Vec<String> {
        std::fs::read_dir(config)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".plugin-"))
            .filter(|name| {
                name != TRASH || contents(&config.join(TRASH)).is_ok_and(|rest| !rest.is_empty())
            })
            .collect()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sofka-plugin-install-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const MANIFEST: &str = concat!(
        "schema_version = 1\n",
        "[package]\n",
        "version = \"1.0.0\"\n",
        "description = \"Summarize a resource.\"\n",
        "license = \"MIT\"\n",
        "sofka = \">=0.0.1\"\n",
        "[plugin]\n",
        "name = \"Resource summary\"\n",
        "palette = \"resource-summary\"\n",
        "command = \"/bin/echo\"\n",
        "output = \"report\"\n",
        // Declared, not omitted: the catalog entry says false, and an omitted
        // `mutating` means true, which is exactly the divergence `reconcile`
        // exists to refuse.
        "mutating = false\n",
    );

    /// A catalog whose single artifact is a real archive in `cache`, so the
    /// whole offline install path runs without a network or a cluster.
    fn published(cache: &Path, id: &str, version: &str, manifest: &str) -> CatalogSnapshot {
        let bytes = {
            let path = cache.join(format!("{id}-{version}.tar.zst"));
            archive(
                &path,
                &[("plugin.toml", manifest.as_bytes(), tar::EntryType::Regular)],
            );
            std::fs::read(&path).unwrap()
        };
        let digest = plugin_catalog::digest(&bytes);
        let stored = cache.join("artifacts").join(format!("{digest}.tar.zst"));
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, &bytes).unwrap();
        let index = serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-09-11T00:00:00Z",
            "plugins": [{
                "id": id,
                "display_name": "Resource summary",
                "description": "Summarize a resource.",
                "tags": [],
                "publisher": "sofka",
                "repository": "https://github.com/nklmilojevic/sofka-plugins",
                "versions": [{
                    "version": version,
                    "sofka": ">=0.0.1",
                    "source_commit": "1".repeat(40),
                    "license": "MIT",
                    "readme": "https://example.invalid/readme",
                    "requirements": [],
                    "command": "/bin/echo",
                    "target": "selection",
                    "output": "report",
                    "mutating": false,
                    "confirm": false,
                    "dangerous": false,
                    "network_load": false,
                    "status": "active",
                    "artifacts": [{
                        "platform": "any",
                        "url": format!(
                            "{}{id}-v{version}/{id}.tar.zst",
                            plugin_catalog::RELEASE_ROOT
                        ),
                        "blake3": digest,
                        "size": bytes.len(),
                    }],
                }],
            }],
        });
        CatalogSnapshot {
            catalog: plugin_catalog::Catalog::parse(&serde_json::to_vec(&index).unwrap()).unwrap(),
            commit: "0".repeat(40),
            fetched_at: 0,
            offline: true,
        }
    }

    #[tokio::test]
    async fn local_install_and_update_keep_the_source_and_reject_source_changes() {
        let config = scratch("local-source");
        let mirror = config.join("mirror");
        let cache = config.join("empty-cache");
        std::fs::create_dir_all(&mirror).unwrap();
        let mut snapshot = published(&mirror, "resource-summary", "1.0.0", MANIFEST);
        let plugin = &mut snapshot.catalog.plugins[0];
        plugin.source = plugin_catalog::Source {
            name: "team".into(),
            url: format!("file://{}/index.json", mirror.display()),
            trusted: true,
        };
        plugin.revision = "a".repeat(64);
        plugin.versions[0].artifacts[0].url = "resource-summary-1.0.0.tar.zst".into();
        let requests = ["resource-summary".into()];
        let prepared = prepare_below(&config, &cache, &snapshot, &requests, true)
            .await
            .unwrap();
        prepared.into_iter().next().unwrap().activate().unwrap();
        let destination = config.join("plugins/resource-summary");
        let record = read_record(&destination).unwrap();
        assert_eq!(
            record.catalog_source,
            snapshot.catalog.plugins[0].source.identity()
        );
        assert_eq!(record.catalog_commit, "a".repeat(64));
        let mut changed = snapshot.clone();
        changed.catalog.plugins[0].source.url = format!("file://{}/other.json", mirror.display());
        let error = prepare_below(&config, &cache, &changed, &requests, true)
            .await
            .unwrap_err();
        assert!(error.contains("different catalog"));
        assert_eq!(read_record(&destination).unwrap().package_version, "1.0.0");

        let mut newer = published(
            &mirror,
            "resource-summary",
            "2.0.0",
            &MANIFEST.replace("1.0.0", "2.0.0"),
        );
        newer.catalog.plugins[0].source = snapshot.catalog.plugins[0].source.clone();
        newer.catalog.plugins[0].versions[0].artifacts[0].url =
            "resource-summary-2.0.0.tar.zst".into();
        let prepared = prepare_below(&config, &cache, &newer, &requests, true)
            .await
            .unwrap();
        prepared.into_iter().next().unwrap().activate().unwrap();
        assert_eq!(read_record(&destination).unwrap().package_version, "2.0.0");
        assert_eq!(
            read_record(&destination).unwrap().catalog_source,
            record.catalog_source
        );
        std::fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn legacy_installation_records_belong_to_the_official_catalog() {
        let record = record_for("resource-summary", "1.0.0");
        let mut json = serde_json::to_value(&record).unwrap();
        json.as_object_mut().unwrap().remove("catalog_source");
        let record: InstallationRecord = serde_json::from_value(json).unwrap();
        assert_eq!(record.catalog_source, "official");
    }

    fn multi_manifest() -> String {
        format!(
            "{}{}",
            MANIFEST
                .replace("schema_version = 1", "schema_version = 2")
                .replace("[plugin]", "[[commands]]"),
            r#"
[[commands]]
name = "Renew"
palette = "cert-manager-renew"
command = "/bin/echo"
args = ["renew"]
scopes = ["certificates"]
output = "report"
mutating = true
confirm = true
"#
        )
    }

    fn published_commands(cache: &Path, version: &str, manifest: &str) -> CatalogSnapshot {
        let mut snapshot = published(cache, "cert-manager", version, manifest);
        snapshot.catalog.schema_version = 2;
        let (commands, _) = crate::plugins::read_manifest(manifest).unwrap();
        snapshot.catalog.plugins[0].versions[0].execution =
            plugin_catalog::CatalogExecution::Commands {
                commands: commands.iter().map(Into::into).collect(),
            };
        snapshot.catalog =
            plugin_catalog::Catalog::parse(&serde_json::to_vec(&snapshot.catalog).unwrap())
                .unwrap();
        snapshot
    }

    #[tokio::test]
    async fn command_package_installs_updates_and_removes_as_one_unit() {
        let config = scratch("command-lifecycle");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let manifest = multi_manifest();
        let snapshot = published_commands(&cache, "1.0.0", &manifest);
        let requests = ["cert-manager".to_string()];
        let prepared = prepare_below(&config, &cache, &snapshot, &requests, true)
            .await
            .unwrap();
        assert_eq!(prepared.len(), 1);
        prepared.into_iter().next().unwrap().activate().unwrap();
        let destination = config.join("plugins/cert-manager");
        let commands = crate::plugins::read_package(&destination).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].args, ["renew"]);
        let next = manifest
            .replace("version = \"1.0.0\"", "version = \"2.0.0\"")
            .replace("cert-manager-renew", "cert-manager-renew-v2");
        let snapshot = published_commands(&cache, "2.0.0", &next);
        let prepared = prepare_below(&config, &cache, &snapshot, &requests, true)
            .await
            .unwrap();
        assert_eq!(prepared[0].previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(
            prepared.into_iter().next().unwrap().activate().unwrap(),
            Activation::Updated
        );
        let commands = crate::plugins::read_package(&destination).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            commands[1].palette.as_deref(),
            Some("cert-manager-renew-v2")
        );
        remove_below(&config, &requests).unwrap();
        assert!(!destination.exists());
        std::fs::remove_dir_all(config).unwrap();
    }

    #[tokio::test]
    async fn a_second_command_cannot_disagree_with_catalog_safety_or_identity() {
        let config = scratch("command-reconcile");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published_commands(&cache, "1.0.0", &multi_manifest());
        for field in [
            "mutating", "confirm", "args", "scopes", "palette", "missing",
        ] {
            let mut modified = snapshot.clone();
            let plugin_catalog::CatalogExecution::Commands { commands } =
                &mut modified.catalog.plugins[0].versions[0].execution
            else {
                unreachable!()
            };
            match field {
                "mutating" => commands[1].execution.mutating = false,
                "confirm" => commands[1].execution.confirm = false,
                "args" => commands[1].args.clear(),
                "scopes" => commands[1].scopes.clear(),
                "palette" => commands[1].palette = Some("different".into()),
                _ => {
                    commands.pop();
                }
            }
            let error = prepare_below(&config, &cache, &modified, &["cert-manager".into()], true)
                .await
                .err()
                .unwrap();
            assert!(
                error.contains("contradicts the catalog"),
                "{field}: {error}"
            );
            assert!(!config.join("plugins/cert-manager").exists());
        }
        std::fs::remove_dir_all(config).unwrap();
    }

    #[tokio::test]
    async fn conflicts_include_later_commands_in_installed_and_staged_packages() {
        let config = scratch("command-conflicts");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let mut snapshot = published_commands(&cache, "1.0.0", &multi_manifest());
        let other_manifest = MANIFEST
            .replace("Resource summary", "Other")
            .replace("resource-summary", "cert-manager-renew");
        let other = published(&cache, "other", "1.0.0", &other_manifest);
        snapshot.catalog.plugins.extend(other.catalog.plugins);
        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["cert-manager".into(), "other".into()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared[1].conflicts, [config.join("plugins/cert-manager")]);
        let mut prepared = prepared.into_iter();
        prepared.next().unwrap().activate().unwrap();
        drop(prepared);
        let prepared = prepare_below(&config, &cache, &snapshot, &["other".into()], true)
            .await
            .unwrap();
        assert_eq!(prepared[0].conflicts, [config.join("plugins/cert-manager")]);
        drop(prepared);
        std::fs::remove_dir_all(config).unwrap();
    }

    fn record_for(id: &str, version: &str) -> InstallationRecord {
        InstallationRecord {
            schema_version: 1,
            id: id.into(),
            package_version: version.into(),
            catalog_commit: "0".repeat(40),
            catalog_source: official_source(),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        }
    }

    fn write_record(dir: &Path, record: &InstallationRecord) {
        std::fs::create_dir_all(dir).unwrap();
        let mut record = record.clone();
        record.files = hash_files(dir).unwrap();
        std::fs::write(dir.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn an_offline_install_stages_records_and_activates_one_package() {
        let config = scratch("install-offline");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].previous_version, None);
        assert!(prepared[0].conflicts.is_empty());
        // Nothing is visible to the loader until activation.
        let destination = config.join("plugins").join("resource-summary");
        assert!(!destination.exists());

        assert_eq!(
            prepared.into_iter().next().unwrap().activate().unwrap(),
            Activation::Installed
        );
        assert!(destination.join("plugin.toml").is_file());
        assert!(!destination.join(STAGE_MARKER).exists());
        let record = read_record(&destination).unwrap();
        assert_eq!(record.package_version, "1.0.0");
        assert_eq!(record.catalog_commit, "0".repeat(40));
        assert_eq!(record.source_commit, "1".repeat(40));
        assert!(record.files.contains_key("plugin.toml"));
        verify_record(&destination, &record).unwrap();

        // Reinstalling the same intact version changes nothing.
        let again = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary@1.0.0".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(again[0].previous_version.as_deref(), Some("1.0.0"));
        let before = std::fs::read(destination.join("plugin.toml")).unwrap();
        assert_eq!(
            again.into_iter().next().unwrap().activate().unwrap(),
            Activation::Unchanged
        );
        assert_eq!(
            std::fs::read(destination.join("plugin.toml")).unwrap(),
            before
        );

        let packages = installed_in(&config.join("plugins")).unwrap();
        assert_eq!(packages.len(), 1);
        assert!(packages[0].managed && !packages[0].modified);
        assert_eq!(packages[0].version.as_deref(), Some("1.0.0"));
        assert_eq!(
            managed_ids_in(&config.join("plugins")).unwrap(),
            ["resource-summary"]
        );

        let removed = remove_below(&config, &["resource-summary".to_string()]).unwrap();
        assert_eq!(
            removed,
            vec![("resource-summary".to_string(), destination.clone())]
        );
        assert!(!destination.exists());
        assert!(installed_in(&config.join("plugins")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn preparation_refuses_a_batch_before_touching_any_installed_package() {
        let config = scratch("install-batch");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let mut snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);
        let mut newer = snapshot.catalog.plugins[0].versions[0].clone();
        newer.version = "2.0.0".into();
        snapshot.catalog.plugins[0].versions.push(newer);
        snapshot.catalog.validate().unwrap();
        let plugins = config.join("plugins");

        // Two versions of one ID in a single request is a conflict, not a pick.
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &[
                "resource-summary@1.0.0".to_string(),
                "resource-summary@2.0.0".to_string(),
            ],
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("conflicting versions"), "{error}");

        // An unmanaged directory is never taken over.
        let destination = plugins.join("resource-summary");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), "local").unwrap();
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("refusing unmanaged plugin directory"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("plugin.toml")).unwrap(),
            "local"
        );

        // A managed package the user edited is refused just as firmly.
        write_record(&destination, &record_for("resource-summary", "1.0.0"));
        std::fs::write(destination.join("extra"), "mine").unwrap();
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("local modifications"), "{error}");

        // No stage survives a refused batch.
        let leftovers = leftovers(&config);
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn a_repeated_id_resolves_once_and_a_changed_digest_is_refused() {
        let config = scratch("install-digest");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &[
                "resource-summary".to_string(),
                "resource-summary@1.0.0".to_string(),
            ],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared.len(), 1);
        for package in prepared {
            package.activate().unwrap();
        }

        // The catalog may not repoint an immutable version at other bytes.
        let mut repointed = published(&cache, "resource-summary", "1.0.0", MANIFEST);
        repointed.catalog.plugins[0].versions[0].artifacts[0].blake3 = "9".repeat(64);
        let error = prepare_below(
            &config,
            &cache,
            &repointed,
            &["resource-summary@1.0.0".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("differs from the installed immutable version"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn an_install_names_the_package_it_would_be_hidden_by() {
        let config = scratch("install-conflict");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(&cache, "resource-summary", "1.0.0", MANIFEST);
        let manual = config.join("plugins").join("aaa-manual");
        std::fs::create_dir_all(&manual).unwrap();
        std::fs::write(manual.join("plugin.toml"), MANIFEST).unwrap();

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(prepared[0].conflicts, vec![manual]);
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn an_unbuildable_package_never_reaches_the_loader() {
        let config = scratch("install-invalid");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let snapshot = published(
            &cache,
            "resource-summary",
            "1.0.0",
            "schema_version = 1\n[plugin]\nname = \"X\"\ncommand = \"/bin/echo\"\nshell = true\n",
        );
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("preparing resource-summary@1.0.0"),
            "{error}"
        );
        assert!(!config.join("plugins").join("resource-summary").exists());
        let leftovers = std::fs::read_dir(&config).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".plugin-stage-")
        });
        assert!(!leftovers);
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn a_failed_activation_keeps_the_package_it_was_replacing() {
        let config = scratch("activation-failure");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), "previous").unwrap();
        write_record(&destination, &record_for("sample", "1.0.0"));

        // A stage that is gone models the filesystem failing mid-batch.
        let error = PreparedPackage::staged(
            "sample",
            "2.0.0",
            Some("1.0.0"),
            config.join(".plugin-stage-sample-absent"),
            destination.clone(),
        )
        .activate()
        .unwrap_err();

        assert!(error.contains("activating sample"), "{error}");
        assert_eq!(
            std::fs::read_to_string(destination.join("plugin.toml")).unwrap(),
            "previous"
        );
        let record = read_record(&destination).unwrap();
        assert_eq!(record.package_version, "1.0.0");
        verify_record(&destination, &record).unwrap();
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn replacement_refuses_unowned_trash_before_changing_the_active_package() {
        let config = scratch("activation-unowned-trash");
        let destination = config.join("plugins").join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), "previous").unwrap();
        write_record(&destination, &record_for("sample", "1.0.0"));

        let stage = unique_path(&config, ".plugin-stage-sample");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
        std::fs::write(stage.join("plugin.toml"), "replacement").unwrap();
        write_record(&stage, &record_for("sample", "2.0.0"));
        let user_file = config.join(TRASH).join("user-file");
        std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
        std::fs::write(&user_file, "mine").unwrap();

        let error = PreparedPackage::staged(
            "sample",
            "2.0.0",
            Some("1.0.0"),
            stage.clone(),
            destination.clone(),
        )
        .activate()
        .unwrap_err();

        assert!(error.contains("did not put there"), "{error}");
        assert!(!stage.exists(), "the refused stage was not cleaned up");
        assert_eq!(
            std::fs::read_to_string(destination.join("plugin.toml")).unwrap(),
            "previous"
        );
        assert_eq!(read_record(&destination).unwrap().package_version, "1.0.0");
        assert!(user_file.is_file());
        assert!(!config.join(TRASH).join(TRASH_MARKER).exists());
        assert!(!std::fs::read_dir(&config).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".plugin-backup-")
        }));
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn extraction_digests_match_verification_for_nested_and_empty_files() {
        let dir = scratch("digest-agreement");
        let source = dir.join("package.tar.zst");
        archive(
            &source,
            &[
                (
                    "plugin.toml",
                    b"schema_version = 1\n",
                    tar::EntryType::Regular,
                ),
                // An empty file cannot be memory-mapped on every platform, and a
                // nested path is spelled differently by the two hashers.
                ("empty", b"", tar::EntryType::Regular),
                ("bin/adapter", b"binary", tar::EntryType::Regular),
            ],
        );
        let destination = dir.join("out");
        std::fs::create_dir(&destination).unwrap();

        let extracted = extract(&source, &destination).unwrap();
        let walked = hash_files(&destination).unwrap();

        assert_eq!(
            extracted, walked,
            "extraction and verification disagree, so every install would look modified"
        );
        assert!(walked.contains_key("bin/adapter"));
        assert_eq!(
            walked["empty"],
            digest_file(&destination.join("empty")).unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn modification_reports_name_the_files_that_moved() {
        let dir = scratch("modified-detail");
        std::fs::write(dir.join("plugin.toml"), "one").unwrap();
        std::fs::write(dir.join("adapter"), "binary").unwrap();
        let record = InstallationRecord {
            files: hash_files(&dir).unwrap(),
            ..record_for("sample", "1.0.0")
        };

        std::fs::write(dir.join("adapter"), "edited").unwrap();
        std::fs::write(dir.join("notes"), "mine").unwrap();
        std::fs::remove_file(dir.join("plugin.toml")).unwrap();
        let error = verify_record(&dir, &record).unwrap_err();
        assert!(error.contains("changed adapter"), "{error}");
        assert!(error.contains("added notes"), "{error}");
        assert!(error.contains("removed plugin.toml"), "{error}");

        // A package that lost everything reports a bounded list, not a wall.
        let many: BTreeMap<String, String> = (0..50)
            .map(|i| (format!("file-{i}"), "0".repeat(64)))
            .collect();
        let error = verify_record(
            &dir,
            &InstallationRecord {
                files: many,
                ..record_for("sample", "1.0.0")
            },
        )
        .unwrap_err();
        assert!(error.contains("and 47 more"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn activation_refuses_a_destination_edited_since_preparation() {
        let config = scratch("edited-after-prepare");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), "mine").unwrap();
        write_record(&destination, &record_for("sample", "1.0.0"));
        let stage = unique_path(&config, ".plugin-stage-sample");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
        std::fs::write(stage.join("plugin.toml"), "theirs").unwrap();
        write_record(&stage, &record_for("sample", "2.0.0"));

        // Preparation verified the destination; the user edits it before the
        // swap, which can be a long download later.
        std::fs::write(destination.join("plugin.toml"), "edited since").unwrap();

        let error = PreparedPackage {
            id: "sample".into(),
            version: "2.0.0".into(),
            previous_version: Some("1.0.0".into()),
            conflicts: Vec::new(),
            stage: Some(stage.clone()),
            destination: destination.clone(),
        }
        .activate()
        .unwrap_err();

        assert!(error.contains("local modifications"), "{error}");
        // A refusal owes nothing to the staging directory: it used to be
        // disowned before the check and left behind until the next command.
        assert!(!stage.exists(), "staging directory outlived the refusal");
        assert_eq!(
            std::fs::read_to_string(destination.join("plugin.toml")).unwrap(),
            "edited since"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn recovery_discards_a_backup_whose_own_cleanup_was_interrupted() {
        let config = scratch("interrupted-backup-cleanup");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        write_record(&destination, &record_for("sample", "2.0.0"));
        // A backup whose destination is already in place: recovery discards it,
        // and must survive being interrupted while doing so.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record_for("sample", "1.0.0"));

        recover(&config).unwrap();

        assert!(!backup.exists());
        assert!(destination.join(RECORD).is_file());
        // Nothing is left behind that a later run would refuse to identify.
        let leftovers = leftovers(&config);
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn an_archive_path_that_is_not_its_own_plain_form_is_refused() {
        // `bin/./data` has only normal components, but lands on disk as
        // `bin/data`, so the recorded path would never match the file.
        for spelling in ["bin/./data", "./plugin.toml", "a/b/./c"] {
            assert!(
                validate_relative_path(Path::new(spelling), false).is_err(),
                "accepted {spelling}"
            );
        }
        validate_relative_path(Path::new("bin/data"), false).unwrap();
    }

    #[tokio::test]
    async fn a_batch_reports_two_packages_claiming_the_same_command() {
        let config = scratch("batch-conflict");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let mut snapshot = published(&cache, "first", "1.0.0", MANIFEST);
        // A second package with a different ID but the same palette command.
        let second = published(&cache, "second", "1.0.0", MANIFEST);
        snapshot
            .catalog
            .plugins
            .extend(second.catalog.plugins.clone());
        snapshot.catalog.validate().unwrap();

        let prepared = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["first".to_string(), "second".to_string()],
            true,
        )
        .await
        .unwrap();

        assert_eq!(prepared.len(), 2);
        assert!(prepared[0].conflicts.is_empty());
        // Neither is installed yet, so only the manifests can reveal it.
        assert_eq!(
            prepared[1].conflicts,
            vec![config.join("plugins").join("first")]
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn a_refused_removal_still_names_what_it_already_removed() {
        let config = scratch("partial-removal");
        let plugins = config.join("plugins");
        for id in ["first", "second"] {
            let package = plugins.join(id);
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("plugin.toml"), "body").unwrap();
            write_record(&package, &record_for(id, "1.0.0"));
        }
        // The second package cannot be renamed out of a read-only parent.
        let refused = plugins.join("second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let outcome = remove_below(&config, &["first".to_string(), "second".to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(config);
        // Whatever it decided about the second, the first is gone and says so.
        if let Err(refusal) = outcome {
            assert_eq!(
                refusal
                    .removed
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["first"]
            );
        }
    }

    #[test]
    fn destinations_are_inspected_before_anything_is_written() {
        let dir = scratch("inspect");
        let missing = dir.join("absent");
        assert!(inspect_destination(&missing, "sample").unwrap().is_none());

        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert!(
            inspect_destination(&file, "sample")
                .unwrap_err()
                .contains("is not a directory")
        );

        let unmanaged = dir.join("unmanaged");
        std::fs::create_dir(&unmanaged).unwrap();
        assert!(
            inspect_destination(&unmanaged, "sample")
                .unwrap_err()
                .contains("refusing unmanaged plugin directory")
        );

        let foreign = dir.join("foreign");
        write_record(&foreign, &record_for("other", "1.0.0"));
        assert!(
            inspect_destination(&foreign, "sample")
                .unwrap_err()
                .contains("belongs to other")
        );

        let managed = dir.join("sample");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("plugin.toml"), "body").unwrap();
        write_record(&managed, &record_for("sample", "1.0.0"));
        assert_eq!(
            inspect_destination(&managed, "sample")
                .unwrap()
                .unwrap()
                .package_version,
            "1.0.0"
        );
        std::fs::write(managed.join("plugin.toml"), "edited").unwrap();
        assert!(
            inspect_destination(&managed, "sample")
                .unwrap_err()
                .contains("local modifications")
        );

        #[cfg(unix)]
        {
            let linked = dir.join("linked");
            std::os::unix::fs::symlink(&managed, &linked).unwrap();
            assert!(
                inspect_destination(&linked, "sample")
                    .unwrap_err()
                    .contains("refusing symlinked plugin destination")
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listing_separates_managed_manual_and_edited_packages() {
        let plugins = scratch("listing").join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        assert!(installed_in(&plugins.join("absent")).unwrap().is_empty());

        let managed = plugins.join("managed");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("plugin.toml"), "body").unwrap();
        write_record(&managed, &record_for("managed", "1.0.0"));

        let edited = plugins.join("edited");
        std::fs::create_dir(&edited).unwrap();
        std::fs::write(edited.join("plugin.toml"), "body").unwrap();
        write_record(&edited, &record_for("edited", "2.0.0"));
        std::fs::write(edited.join("extra"), "mine").unwrap();

        let manual = plugins.join("manual");
        std::fs::create_dir(&manual).unwrap();
        std::fs::write(manual.join("plugin.toml"), "body").unwrap();

        let broken = plugins.join("broken");
        std::fs::create_dir(&broken).unwrap();
        std::fs::write(broken.join(RECORD), "not json").unwrap();

        std::fs::write(plugins.join("loose-file"), "ignored").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&managed, plugins.join("linked")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&managed, plugins.join("linked")).unwrap();

        let packages = installed_in(&plugins).unwrap();
        let rows: Vec<_> = packages
            .iter()
            .map(|p| (p.id.as_str(), p.version.as_deref(), p.managed, p.modified))
            .collect();
        assert_eq!(
            rows,
            [
                ("broken", None, true, true),
                ("edited", Some("2.0.0"), true, true),
                // The loader follows the link and runs the package behind it,
                // so listing nothing made a live package look absent. Manual:
                // sofka reports it and never takes ownership of it.
                ("linked", None, false, false),
                ("managed", Some("1.0.0"), true, false),
                ("manual", None, false, false),
            ]
        );
        assert_eq!(
            managed_ids_in(&plugins).unwrap(),
            ["broken", "edited", "managed"]
        );
        let _ = std::fs::remove_dir_all(plugins.parent().unwrap());
    }

    #[test]
    fn removal_rejects_versions_unknown_ids_and_edited_packages() {
        let config = scratch("removal-guards");
        let plugins = config.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();

        assert!(
            remove_below(&config, &["sample@1.0.0".to_string()])
                .unwrap_err()
                .error
                .contains("without versions")
        );
        assert!(
            remove_below(&config, &["sample".to_string()])
                .unwrap_err()
                .error
                .contains("is not installed")
        );

        let managed = plugins.join("sample");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(managed.join("plugin.toml"), "body").unwrap();
        write_record(&managed, &record_for("sample", "1.0.0"));
        std::fs::write(managed.join("extra"), "mine").unwrap();
        assert!(
            remove_below(&config, &["sample".to_string()])
                .unwrap_err()
                .error
                .contains("local modifications")
        );
        assert!(managed.is_dir());

        // One bad ID in a batch removes nothing at all.
        std::fs::remove_file(managed.join("extra")).unwrap();
        let other = plugins.join("other");
        std::fs::create_dir(&other).unwrap();
        write_record(&other, &record_for("other", "1.0.0"));
        assert!(
            remove_below(&config, &["sample".to_string(), "absent".to_string()])
                .unwrap_err()
                .error
                .contains("is not installed")
        );
        assert!(managed.is_dir() && other.is_dir());

        // A repeated ID is removed once.
        let removed = remove_below(&config, &["sample".to_string(), "sample".to_string()]).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(!managed.exists() && other.is_dir());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn recovery_finishes_an_interruption_at_every_filesystem_step() {
        let config = scratch("recovery-steps");
        let plugins = config.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let destination = plugins.join("sample");
        let record = record_for("sample", "1.0.0");

        // 1. Interrupted while staging: the marker identifies a partial stage.
        let stage = unique_path(&config, ".plugin-stage-sample");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
        recover(&config).unwrap();
        assert!(!stage.exists());

        // 2. Interrupted between the two renames: the backup is restored.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record);
        recover(&config).unwrap();
        assert!(!backup.exists());
        assert_eq!(read_record(&destination).unwrap().package_version, "1.0.0");

        // 3. Interrupted after the second rename: the backup is now redundant.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record);
        recover(&config).unwrap();
        assert!(!backup.exists());
        assert!(destination.is_dir());

        // 4. Interrupted before the staging marker was cleared.
        std::fs::write(destination.join(STAGE_MARKER), "stage").unwrap();
        recover(&config).unwrap();
        assert!(!destination.join(STAGE_MARKER).exists());

        // 5. Interrupted while deleting a committed removal, in any state.
        for leftover in ["with-record", "without-record"] {
            let removed = unique_path(
                &trash(&config).unwrap(),
                &format!("{REMOVED_PREFIX}{leftover}"),
            );
            std::fs::create_dir(&removed).unwrap();
            if leftover == "with-record" {
                std::fs::write(removed.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
            }
            recover(&config).unwrap();
            assert!(!removed.exists(), "{leftover}");
        }

        // 6. A backup that cannot be told apart from user data stops the world
        //    rather than guessing.
        let opaque = unique_path(&config, ".plugin-backup-sample");
        std::fs::create_dir(&opaque).unwrap();
        std::fs::write(opaque.join("data"), "unknown").unwrap();
        let error = recover(&config).unwrap_err();
        assert!(
            error.contains("cannot identify interrupted backup"),
            "{error}"
        );
        std::fs::remove_dir_all(&opaque).unwrap();

        // 7. A backup whose destination was replaced by something unmanaged.
        let backup = unique_path(&config, ".plugin-backup-sample");
        write_record(&backup, &record);
        std::fs::remove_file(destination.join(RECORD)).unwrap();
        let error = recover(&config).unwrap_err();
        assert!(error.contains("is unmanaged"), "{error}");
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn hashing_refuses_anything_that_is_not_a_plain_file_or_directory() {
        let dir = scratch("hashing");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("plugin.toml"), "a").unwrap();
        std::fs::write(dir.join("nested/adapter"), "b").unwrap();
        std::fs::write(dir.join(RECORD), "ignored").unwrap();
        std::fs::write(dir.join(STAGE_MARKER), "ignored").unwrap();
        let files = hash_files(&dir).unwrap();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["nested/adapter", "plugin.toml"]
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("plugin.toml"), dir.join("linked")).unwrap();
            assert!(hash_files(&dir).unwrap_err().contains("symlink"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_package_with_directories_installs_and_verifies_clean() {
        let config = scratch("explicit-directories");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // An explicit directory entry, the kind every ordinary tar writes.
        let bytes = {
            let path = cache.join("built.tar.zst");
            archive(
                &path,
                &[
                    ("plugin.toml", MANIFEST.as_bytes(), tar::EntryType::Regular),
                    ("bin/", b"", tar::EntryType::Directory),
                    ("bin/adapter", b"binary", tar::EntryType::Regular),
                ],
            );
            std::fs::read(&path).unwrap()
        };
        let digest = plugin_catalog::digest(&bytes);
        let stored = cache.join("artifacts").join(format!("{digest}.tar.zst"));
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, &bytes).unwrap();
        let mut snapshot = published(&cache, "sample", "1.0.0", MANIFEST);
        let artifact = &mut snapshot.catalog.plugins[0].versions[0].artifacts[0];
        artifact.blake3 = digest;
        artifact.size = bytes.len() as u64;

        let prepared = prepare_below(&config, &cache, &snapshot, &["sample".to_string()], true)
            .await
            .unwrap();
        prepared.into_iter().next().unwrap().activate().unwrap();

        // The whole point: it must not be modified the instant it installs.
        let destination = config.join("plugins").join("sample");
        let record = read_record(&destination).unwrap();
        assert!(record.files.contains_key("bin/adapter"));
        verify_record(&destination, &record).unwrap();
        assert!(!installed_in(&config.join("plugins")).unwrap()[0].modified);
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn extraction_requires_a_manifest_at_the_root_and_caps_the_entry_count() {
        let dir = scratch("extract-shape");
        let nested = dir.join("nested.tar.zst");
        archive(
            &nested,
            &[(
                "inner/plugin.toml",
                b"schema_version = 1\n",
                tar::EntryType::Regular,
            )],
        );
        let out = dir.join("nested-out");
        std::fs::create_dir(&out).unwrap();
        assert!(
            extract(&nested, &out)
                .unwrap_err()
                .contains("no plugin.toml at its root")
        );

        let many = dir.join("many.tar.zst");
        let names: Vec<String> = (0..=FILE_MAX).map(|i| format!("file-{i}")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), b"x".as_slice(), tar::EntryType::Regular))
            .collect();
        archive(&many, &entries);
        let out = dir.join("many-out");
        std::fs::create_dir(&out).unwrap();
        assert!(
            extract(&many, &out)
                .unwrap_err()
                .contains("more than 2000 entries")
        );

        // The builder refuses to write a traversing name, so the header is
        // filled in the way an attacker would have to.
        let traversal = dir.join("traversal.tar.zst");
        let file = File::create(&traversal).unwrap();
        let zstd = zstd::stream::write::Encoder::new(file, 19)
            .unwrap()
            .auto_finish();
        let mut builder = tar::Builder::new(zstd);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(1);
        let name = b"../escape.toml";
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        builder.into_inner().unwrap();
        let out = dir.join("traversal-out");
        std::fs::create_dir(&out).unwrap();
        assert!(extract(&traversal, &out).unwrap_err().contains("unsafe"));
        assert!(!dir.join("escape.toml").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_detect_changed_and_extra_files() {
        let dir = scratch("modified");
        std::fs::write(dir.join("plugin.toml"), "one").unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "sample".into(),
            package_version: "1.0.0".into(),
            catalog_commit: "0".repeat(40),
            catalog_source: official_source(),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: hash_files(&dir).unwrap(),
        };
        assert!(verify_record(&dir, &record).is_ok());
        std::fs::write(dir.join("extra"), "local").unwrap();
        assert!(verify_record(&dir, &record).is_err());
        std::fs::remove_file(dir.join("extra")).unwrap();
        std::fs::write(dir.join("plugin.toml"), "two").unwrap();
        assert!(verify_record(&dir, &record).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn installation_records_are_bounded_before_they_reach_disk() {
        let files = (0..FILE_MAX)
            .map(|index| {
                (
                    format!("{}file-{index}", "long-directory/".repeat(40)),
                    "0".repeat(64),
                )
            })
            .collect();
        let record = InstallationRecord {
            files,
            ..record_for("sample", "1.0.0")
        };

        let error = serialize_record(&record).unwrap_err();

        assert!(error.contains("exceeds 1 MiB"), "{error}");
    }

    #[test]
    fn archive_paths_must_be_plain_relative_components() {
        for bad in ["", ".", "../escape", "/absolute", "dir/../escape"] {
            assert!(
                validate_relative_path(Path::new(bad), false).is_err(),
                "accepted {bad}"
            );
        }
        assert!(validate_relative_path(Path::new("bin/adapter"), false).is_ok());
        assert!(validate_relative_path(Path::new("bin\\adapter"), false).is_err());
        assert!(validate_relative_path(Path::new(RECORD), false).is_err());
        assert!(validate_relative_path(Path::new(STAGE_MARKER), false).is_err());
        // A trailing slash used to carry a directory entry past the reserved
        // check, and a difference in case carried a file past it — which on a
        // case-insensitive filesystem lands on the record itself.
        for (raw, directory) in [
            (".sofka-install-stage/", true),
            (".sofka-install.json/", true),
            (".SOFKA-INSTALL.JSON", false),
            (".Sofka-Install-Stage", false),
        ] {
            let error = validate_relative_path(Path::new(raw), directory).unwrap_err();
            assert!(error.contains("reserved by sofka"), "{raw}: {error}");
        }
        assert_eq!(
            validate_relative_path(Path::new("bin/"), true).unwrap(),
            "bin"
        );
        assert!(validate_relative_path(Path::new("bin/"), false).is_err());
        assert!(validate_relative_path(Path::new("bin//"), true).is_err());
    }

    #[test]
    fn extraction_preserves_only_required_executable_bits() {
        let dir = scratch("extract");
        let source = dir.join("package.tar.zst");
        archive(
            &source,
            &[
                (
                    "plugin.toml",
                    b"schema_version = 1\n",
                    tar::EntryType::Regular,
                ),
                ("adapter", b"binary", tar::EntryType::Regular),
            ],
        );
        let destination = dir.join("out");
        std::fs::create_dir(&destination).unwrap();
        extract(&source, &destination).unwrap();
        assert_eq!(
            std::fs::read(destination.join("adapter")).unwrap(),
            b"binary"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(destination.join("adapter"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::metadata(destination.join("plugin.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extraction_rejects_links_and_duplicate_paths() {
        let dir = scratch("archive-safety");
        let duplicate = dir.join("duplicate.tar.zst");
        archive(
            &duplicate,
            &[
                ("plugin.toml", b"first", tar::EntryType::Regular),
                ("plugin.toml", b"second", tar::EntryType::Regular),
            ],
        );
        let output = dir.join("duplicate");
        std::fs::create_dir(&output).unwrap();
        assert!(
            extract(&duplicate, &output)
                .unwrap_err()
                .contains("duplicate")
        );

        let linked = dir.join("link.tar.zst");
        archive(
            &linked,
            &[
                ("plugin.toml", b"manifest", tar::EntryType::Regular),
                ("adapter", b"target", tar::EntryType::Symlink),
            ],
        );
        let output = dir.join("linked");
        std::fs::create_dir(&output).unwrap();
        assert!(extract(&linked, &output).unwrap_err().contains("link"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extraction_accepts_directory_entries_and_refuses_unrecordable_names() {
        let dir = scratch("archive-record-paths");
        let directories = dir.join("directories.tar.zst");
        archive(
            &directories,
            &[
                ("plugin.toml", b"manifest", tar::EntryType::Regular),
                ("bin/", b"", tar::EntryType::Directory),
                ("bin/adapter", b"binary", tar::EntryType::Regular),
            ],
        );
        let output = dir.join("directories");
        std::fs::create_dir(&output).unwrap();
        let files = extract(&directories, &output).unwrap();
        assert!(files.contains_key("bin/adapter"));

        for (tag, name) in [("backslash", "bin\\adapter"), ("record", RECORD)] {
            let source = dir.join(format!("{tag}.tar.zst"));
            archive(
                &source,
                &[
                    ("plugin.toml", b"manifest", tar::EntryType::Regular),
                    (name, b"contents", tar::EntryType::Regular),
                ],
            );
            let output = dir.join(tag);
            std::fs::create_dir(&output).unwrap();
            assert!(extract(&source, &output).is_err(), "accepted {name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extraction_bounds_the_whole_decompressed_stream() {
        let dir = scratch("expansion");
        let source = dir.join("bomb.tar.zst");
        let payload = vec![0u8; 2 * 1024 * 1024];
        archive(
            &source,
            &[
                (
                    "plugin.toml",
                    b"schema_version = 1\n",
                    tar::EntryType::Regular,
                ),
                // A directory entry is skipped rather than written, so only a
                // bound on the stream itself can catch its declared payload.
                ("payload", payload.as_slice(), tar::EntryType::Directory),
            ],
        );
        let destination = dir.join("out");
        std::fs::create_dir(&destination).unwrap();
        let error = extract_bounded(&source, &destination, 1024 * 1024).unwrap_err();
        assert!(error.contains("expands beyond 1 MiB"), "{error}");

        // TAR metadata is consumed before an entry is ever yielded, so no
        // per-entry accounting can see it.
        let metadata = dir.join("metadata.tar.zst");
        let file = File::create(&metadata).unwrap();
        let zstd = zstd::stream::write::Encoder::new(file, 19)
            .unwrap()
            .auto_finish();
        let mut builder = tar::Builder::new(zstd);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::GNULongName);
        header.set_mode(0o644);
        header.set_size(payload.len() as u64);
        header.set_cksum();
        builder
            .append(&header, std::io::Cursor::new(payload))
            .unwrap();
        builder.into_inner().unwrap();
        let destination = dir.join("metadata");
        std::fs::create_dir(&destination).unwrap();
        let error = extract_bounded(&metadata, &destination, 1024 * 1024).unwrap_err();
        assert!(error.contains("expands beyond 1 MiB"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_discards_a_backup_left_without_its_record() {
        let config = scratch("interrupted-cleanup");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "sample".into(),
            package_version: "2.0.0".into(),
            catalog_commit: "0".repeat(40),
            catalog_source: official_source(),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        };
        std::fs::write(
            destination.join(RECORD),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        // Cleanup deletes the record before the rest of the directory, which
        // used to leave a backup nothing could identify.
        let orphan = trash(&config).unwrap().join(".plugin-removed-sample-1-1");
        std::fs::create_dir(&orphan).unwrap();
        std::fs::write(orphan.join("leftover"), "old").unwrap();

        recover(&config).unwrap();

        assert!(!orphan.exists());
        assert!(destination.join(RECORD).is_file());
        assert!(InstallLock::acquire(&config).is_ok());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn activation_retires_the_backup_through_a_recoverable_name() {
        let config = scratch("retired-backup");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();
        write_record(&destination, &record_for("sample", "1.0.0"));
        let stage = unique_path(&config, ".plugin-stage-sample");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
        std::fs::write(stage.join("new"), "new").unwrap();
        write_record(&stage, &record_for("sample", "2.0.0"));

        PreparedPackage {
            id: "sample".into(),
            version: "2.0.0".into(),
            previous_version: Some("1.0.0".into()),
            conflicts: Vec::new(),
            stage: Some(stage),
            destination: destination.clone(),
        }
        .activate()
        .unwrap();

        let leftovers = leftovers(&config);
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(destination.join("new").is_file());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    #[cfg(unix)]
    fn operations_allow_symlinked_config_and_plugins_parents() {
        let root = scratch("symlinked-parent");
        let real_config = root.join("real-config");
        let config = root.join("linked-config");
        let elsewhere = root.join("elsewhere");
        let package = elsewhere.join("sample");
        std::fs::create_dir_all(&package).unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "sample".into(),
            package_version: "1.0.0".into(),
            catalog_commit: "0".repeat(40),
            catalog_source: official_source(),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        };
        std::fs::write(package.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        std::fs::create_dir(&real_config).unwrap();
        std::os::unix::fs::symlink(&elsewhere, real_config.join("plugins")).unwrap();
        std::os::unix::fs::symlink(&real_config, &config).unwrap();

        drop(InstallLock::acquire(&config).unwrap());
        let removed = remove_below(&config, &["sample".to_string()]).unwrap();

        assert_eq!(removed[0].0, "sample");
        assert!(!package.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conflicts_name_the_package_that_shadows_a_staged_one() {
        let dir = scratch("conflicts");
        let plugins = dir.join("plugins");
        let manual = plugins.join("aaa-manual");
        std::fs::create_dir_all(&manual).unwrap();
        let manifest = concat!(
            "schema_version = 1\n",
            "[plugin]\n",
            "name = \"Resource summary\"\n",
            "palette = \"resource-summary\"\n",
            "command = \"/bin/echo\"\n",
            "output = \"report\"\n",
        );
        std::fs::write(manual.join("plugin.toml"), manifest).unwrap();
        let unrelated = plugins.join("zzz-unrelated");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(
            unrelated.join("plugin.toml"),
            manifest
                .replace("Resource summary", "Something else")
                .replace("resource-summary", "something-else"),
        )
        .unwrap();
        let destination = plugins.join("resource-summary");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("plugin.toml"), manifest).unwrap();

        let staged = crate::plugins::read_package(&destination).unwrap();
        let found = conflicts(&plugins, &destination, &staged);

        assert_eq!(found, vec![manual]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_restores_hyphenated_ids_and_ignores_unmarked_directories() {
        let config = scratch("recovery");
        let backup = config.join(".plugin-backup-resource-summary-1-1");
        std::fs::create_dir(&backup).unwrap();
        let record = InstallationRecord {
            schema_version: 1,
            id: "resource-summary".into(),
            package_version: "1.0.0".into(),
            catalog_commit: "0".repeat(40),
            catalog_source: official_source(),
            source_commit: "1".repeat(40),
            artifact_digest: "2".repeat(64),
            files: BTreeMap::new(),
        };
        std::fs::write(backup.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        let unrelated = config.join(".plugin-stage-user-data");
        std::fs::create_dir(&unrelated).unwrap();

        recover(&config).unwrap();

        assert!(config.join("plugins").join("resource-summary").is_dir());
        assert!(unrelated.is_dir());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn activation_replaces_nonempty_directories_and_identifies_rollbacks() {
        let config = scratch("activation");
        let plugins = config.join("plugins");
        let destination = plugins.join("sample");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("old"), "old").unwrap();
        write_record(&destination, &record_for("sample", "1.0.0"));

        // Activation re-verifies the destination, so each stage carries the
        // record `prepare` would have written into it.
        let activate = |version: &str, previous: &str, contents: &str| {
            let stage = unique_path(&config, ".plugin-stage-sample");
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(stage.join(STAGE_MARKER), "stage").unwrap();
            std::fs::write(stage.join("current"), contents).unwrap();
            write_record(&stage, &record_for("sample", version));
            PreparedPackage {
                id: "sample".into(),
                version: version.into(),
                previous_version: Some(previous.into()),
                conflicts: Vec::new(),
                stage: Some(stage),
                destination: destination.clone(),
            }
            .activate()
            .unwrap()
        };

        assert_eq!(activate("2.0.0", "1.0.0", "new"), Activation::Updated);
        assert!(!destination.join("old").exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("current")).unwrap(),
            "new"
        );
        assert!(!destination.join(STAGE_MARKER).exists());
        assert_eq!(activate("1.5.0", "2.0.0", "older"), Activation::RolledBack);
        assert_eq!(
            std::fs::read_to_string(destination.join("current")).unwrap(),
            "older"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn install_lock_serializes_writers() {
        let config = scratch("lock");
        let first = InstallLock::acquire(&config).unwrap();
        assert!(InstallLock::acquire(&config).is_err());
        drop(first);
        assert!(InstallLock::acquire(&config).is_ok());
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn a_package_that_contradicts_its_catalog_entry_is_refused() {
        // `describe` reports the catalog while the loader runs the manifest, so
        // a package that claims less than it does must never reach the disk.
        for (label, field) in [
            ("mutating", "mutating = true\n"),
            ("confirm", "confirm = true\n"),
            ("dangerous", "dangerous = true\n"),
            ("network_load", "network_load = true\n"),
            ("target", "target = \"context\"\n"),
        ] {
            let config = scratch(&format!("contradicts-{label}"));
            let cache = config.join("cache");
            std::fs::create_dir_all(&cache).unwrap();
            let manifest = MANIFEST.replace("mutating = false\n", field);
            let snapshot = published(&cache, "resource-summary", "1.0.0", &manifest);
            let error = prepare_below(
                &config,
                &cache,
                &snapshot,
                &["resource-summary".to_string()],
                true,
            )
            .await
            .unwrap_err();
            assert!(error.contains("contradicts the catalog entry"), "{error}");
            assert!(error.contains(label), "{label}: {error}");
            // The refusal takes its staging directory with it.
            let staged: Vec<_> = std::fs::read_dir(&config)
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".plugin-stage-"))
                .collect();
            assert!(staged.is_empty(), "{label}: {staged:?}");
            let _ = std::fs::remove_dir_all(config);
        }

        let config = scratch("contradicts-sofka");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let manifest = MANIFEST.replace("sofka = \">=0.0.1\"", "sofka = \">=99.0.0\"");
        let snapshot = published(&cache, "resource-summary", "1.0.0", &manifest);
        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();
        assert!(error.contains("contradicts the catalog entry"), "{error}");
        assert!(error.contains("sofka"), "{error}");
        assert!(!config.join("plugins").join("resource-summary").exists());
        let _ = std::fs::remove_dir_all(config);
    }

    #[tokio::test]
    async fn a_catalog_package_without_publication_metadata_is_refused() {
        let config = scratch("missing-package-table");
        let cache = config.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let manifest = concat!(
            "schema_version = 1\n",
            "[plugin]\n",
            "name = \"Resource summary\"\n",
            "palette = \"resource-summary\"\n",
            "command = \"/bin/echo\"\n",
            "output = \"report\"\n",
            "mutating = false\n",
        );
        let snapshot = published(&cache, "resource-summary", "1.0.0", manifest);

        let error = prepare_below(
            &config,
            &cache,
            &snapshot,
            &["resource-summary".to_string()],
            true,
        )
        .await
        .unwrap_err();

        assert!(error.contains("requires a [package] table"), "{error}");
        assert!(!config.join("plugins").join("resource-summary").exists());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn recovery_clears_its_own_trash_and_nothing_else() {
        let config = scratch("removed-ownership");
        std::fs::create_dir_all(config.join("plugins")).unwrap();

        // A removal renames into the trash and then deletes, so an interrupted
        // one leaves a directory here — with or without its record, since
        // deletion can take the record first.
        let trash = trash(&config).unwrap();
        let with_record = unique_path(&trash, &format!("{REMOVED_PREFIX}sample"));
        std::fs::create_dir(&with_record).unwrap();
        write_record(&with_record, &record_for("sample", "1.0.0"));
        let without_record = unique_path(&trash, &format!("{REMOVED_PREFIX}other"));
        std::fs::create_dir(&without_record).unwrap();
        std::fs::write(without_record.join("leftover"), "old").unwrap();

        // Nothing a user put in the config directory is sofka's to delete, and
        // a name alone never made it so — not even one shaped like sofka's own.
        let theirs = config.join(".plugin-removed-notes-4321-17b2c9f0");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("keep.txt"), "mine").unwrap();

        recover(&config).unwrap();

        assert!(!with_record.exists());
        assert!(!without_record.exists(), "a leftover that lost its record");
        assert!(trash.join(TRASH_MARKER).is_file(), "the claim is kept");
        assert!(
            theirs.join("keep.txt").is_file(),
            "recovery deleted a directory it did not create"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn a_trash_directory_sofka_did_not_create_is_never_emptied() {
        let config = scratch("unowned-trash");
        // Someone else's directory that happens to have the name sofka uses.
        let theirs = config.join(TRASH);
        std::fs::create_dir_all(theirs.join("notes")).unwrap();
        std::fs::write(theirs.join("notes").join("keep.txt"), "mine").unwrap();

        // Recovery runs on every plugin command and must leave it alone.
        recover(&config).unwrap();
        assert!(theirs.join("notes").join("keep.txt").is_file());

        // A removal refuses it out loud rather than claiming it silently.
        let error = trash(&config).unwrap_err();
        assert!(error.contains("did not put there"), "{error}");
        assert!(theirs.join("notes").join("keep.txt").is_file());

        // An empty one has nothing to lose, so it is claimed and marked.
        std::fs::remove_dir_all(theirs.join("notes")).unwrap();
        let claimed = trash(&config).unwrap();
        assert!(claimed.join(TRASH_MARKER).is_file());
        // And claiming is idempotent.
        assert_eq!(trash(&config).unwrap(), claimed);
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn a_trash_directory_sofka_cannot_inspect_is_never_claimed() {
        // A stray file under the name: enumeration fails outright, and the
        // failure must not read as "empty, therefore free to take".
        let config = scratch("uninspectable-trash");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join(TRASH), "not a directory").unwrap();
        assert!(trash(&config).is_err());
        let _ = std::fs::remove_dir_all(&config);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let config = scratch("unreadable-trash");
            let theirs = config.join(TRASH);
            std::fs::create_dir_all(&theirs).unwrap();
            std::fs::write(theirs.join("keep.txt"), "mine").unwrap();
            std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root ignores the mode, so only assert when it actually bites.
            let unreadable = std::fs::read_dir(&theirs).is_err();
            let claimed = trash(&config);
            std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();
            if unreadable {
                assert!(claimed.is_err(), "claimed a directory it could not read");
                assert!(!theirs.join(TRASH_MARKER).exists());
            }
            assert!(theirs.join("keep.txt").is_file());
            let _ = std::fs::remove_dir_all(config);
        }
    }
}
