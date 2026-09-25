//! Plugin catalog metadata, selection, caching, and downloads.

mod sources;
pub use sources::{Source, load_selected};
pub(crate) use sources::{configured_sources, load_sources};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

const REPOSITORY: &str = "nklmilojevic/sofka-plugins";
const COMMIT_URL: &str = "https://api.github.com/repos/nklmilojevic/sofka-plugins/commits/HEAD";
const RAW_ROOT: &str = "https://raw.githubusercontent.com/nklmilojevic/sofka-plugins";
pub(crate) const RELEASE_ROOT: &str =
    "https://github.com/nklmilojevic/sofka-plugins/releases/download/";
const CATALOG_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const ARTIFACT_MAX_BYTES: usize = 50 * 1024 * 1024;
const REDIRECT_LIMIT: usize = 5;

/// How long a download may run, and how long it may make no progress at all.
/// One budget cannot serve both callers: metadata is a few kilobytes of JSON,
/// while an artifact runs to `ARTIFACT_MAX_BYTES`, where a total budget tight
/// enough to catch a dead connection also strangles an ordinary slow link.
#[derive(Clone, Copy, Debug)]
struct Budget {
    total: Duration,
    stall: Duration,
}

const METADATA_BUDGET: Budget = Budget {
    total: Duration::from_secs(30),
    stall: Duration::from_secs(30),
};

/// 50 MiB needs roughly 14 Mbit/s to land inside the metadata budget, which is
/// more than a plugin install can assume. The stall budget, not the total, is
/// what ends a download that has actually died.
const ARTIFACT_BUDGET: Budget = Budget {
    total: Duration::from_secs(900),
    stall: Duration::from_secs(30),
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Catalog {
    pub schema_version: u32,
    pub generated_at: String,
    pub plugins: Vec<CatalogPlugin>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogPlugin {
    #[serde(skip)]
    pub source: Source,
    #[serde(skip)]
    pub revision: String,
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub publisher: String,
    pub repository: String,
    pub versions: Vec<CatalogVersion>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogVersion {
    pub version: String,
    pub sofka: String,
    pub source_commit: String,
    pub license: String,
    pub readme: String,
    #[serde(default)]
    pub requirements: Vec<RuntimeRequirement>,
    #[serde(flatten)]
    pub execution: CatalogExecution,
    pub status: VersionStatus,
    #[serde(default)]
    pub withdrawal_reason: Option<String>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum CatalogExecution {
    Commands { commands: Vec<CatalogCommand> },
    Legacy(Execution),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Execution {
    pub command: String,
    #[serde(default = "default_target")]
    pub target: String,
    pub output: String,
    pub mutating: bool,
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub dangerous: bool,
    #[serde(default)]
    pub network_load: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CatalogCommand {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(flatten)]
    pub execution: Execution,
}

impl From<&crate::config::Plugin> for CatalogCommand {
    fn from(command: &crate::config::Plugin) -> Self {
        Self {
            name: command.name.clone(),
            palette: command.palette.clone(),
            key: (!command.key.is_empty()).then(|| command.key.clone()),
            args: command.args.clone(),
            scopes: command.scopes.clone(),
            execution: Execution {
                command: command.command.clone(),
                target: command.target.clone().unwrap_or_else(default_target),
                output: command.output.clone().unwrap_or_else(|| "terminal".into()),
                mutating: command.mutating.unwrap_or(true),
                confirm: command.confirm,
                dangerous: command.dangerous,
                network_load: command.network_load,
            },
        }
    }
}

impl<'de> Deserialize<'de> for CatalogExecution {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(commands) = value.get("commands") {
            if [
                "command",
                "target",
                "output",
                "mutating",
                "confirm",
                "dangerous",
                "network_load",
            ]
            .iter()
            .any(|field| value.get(field).is_some())
            {
                return Err(D::Error::custom(
                    "catalog release mixes commands and legacy execution fields",
                ));
            }
            Ok(Self::Commands {
                commands: serde_json::from_value(commands.clone()).map_err(D::Error::custom)?,
            })
        } else {
            Ok(Self::Legacy(
                serde_json::from_value(value).map_err(D::Error::custom)?,
            ))
        }
    }
}

impl CatalogExecution {
    pub fn entries(&self) -> Vec<&Execution> {
        match self {
            Self::Legacy(execution) => vec![execution],
            Self::Commands { commands } => {
                commands.iter().map(|command| &command.execution).collect()
            }
        }
    }
}

fn default_target() -> String {
    "selection".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeRequirement {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
    pub install: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionStatus {
    Active,
    Withdrawn,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Artifact {
    pub platform: String,
    pub url: String,
    pub blake3: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedCatalog {
    schema_version: u32,
    commit: String,
    fetched_at: u64,
    catalog: Catalog,
}

#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub catalog: Catalog,
    pub commit: String,
    pub fetched_at: u64,
    pub offline: bool,
}

#[derive(Clone, Debug)]
pub struct Selection<'a> {
    pub plugin: &'a CatalogPlugin,
    pub version: &'a CatalogVersion,
    pub artifact: &'a Artifact,
}

impl Catalog {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        Self::parse_from(bytes, &Source::default())
    }

    fn parse_from(bytes: &[u8], source: &Source) -> Result<Self, String> {
        if bytes.len() > CATALOG_MAX_BYTES {
            return Err("catalog exceeds 10 MiB".into());
        }
        #[derive(Deserialize)]
        struct Schema {
            schema_version: u32,
        }
        let schema: Schema =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid catalog JSON: {e}"))?;
        if !matches!(schema.schema_version, 1 | 2) {
            return Err(format!(
                "unsupported catalog schema_version {} (expected 1 or 2)",
                schema.schema_version
            ));
        }
        let mut catalog: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid catalog JSON: {e}"))?;
        for plugin in &mut catalog.plugins {
            plugin.source = source.clone();
        }
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.schema_version, 1 | 2) {
            return Err(format!(
                "unsupported catalog schema_version {} (expected 1 or 2)",
                self.schema_version
            ));
        }
        let mut ids = HashSet::new();
        let mut folded_ids = HashSet::new();
        for plugin in &self.plugins {
            validate_id(&plugin.id)?;
            if !ids.insert(&plugin.id) || !folded_ids.insert(plugin.id.to_ascii_lowercase()) {
                return Err(format!("duplicate plugin ID {:?}", plugin.id));
            }
            if plugin.display_name.trim().is_empty() || plugin.description.trim().is_empty() {
                return Err(format!("plugin {} has empty display metadata", plugin.id));
            }
            if !metadata_url(&plugin.repository, &plugin.source) {
                return Err(format!("plugin {} repository must use HTTPS", plugin.id));
            }
            let mut versions = HashSet::new();
            for release in &plugin.versions {
                let version = Version::parse(&release.version)
                    .map_err(|e| format!("plugin {} version: {e}", plugin.id))?;
                if !versions.insert(version) {
                    return Err(format!(
                        "plugin {} has duplicate version {}",
                        plugin.id, release.version
                    ));
                }
                VersionReq::parse(&release.sofka).map_err(|e| {
                    format!(
                        "plugin {} version {} compatibility: {e}",
                        plugin.id, release.version
                    )
                })?;
                if release.source_commit.len() != 40
                    || !release.source_commit.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(format!(
                        "plugin {} version {} has an invalid source commit",
                        plugin.id, release.version
                    ));
                }
                if release.license.trim().is_empty()
                    || !metadata_url(&release.readme, &plugin.source)
                {
                    return Err(format!(
                        "plugin {} version {} has invalid license or README metadata",
                        plugin.id, release.version
                    ));
                }
                if let CatalogExecution::Commands { commands } = &release.execution {
                    if self.schema_version != 2 || commands.is_empty() {
                        return Err("command entries require catalog schema 2 and a nonempty commands array".into());
                    }
                    let mut names = HashSet::new();
                    let mut palettes = HashSet::new();
                    for (index, command) in commands.iter().enumerate() {
                        if command.name.trim().is_empty()
                            || !names.insert(&command.name)
                            || (command.palette.is_none() && command.key.is_none())
                            || command.palette.as_ref().is_some_and(|palette| {
                                palette.is_empty()
                                    || !palette.bytes().all(|b| {
                                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
                                    })
                                    || crate::app::plugin_command_reserved(palette)
                                    || !palettes.insert(palette)
                            })
                            || command.key.as_ref().is_some_and(|key| {
                                key.trim().is_empty()
                                    || commands[..index].iter().any(|other| {
                                        other.key.as_ref().is_some_and(|other_key| {
                                            crate::plugins::keys_conflict(
                                                key,
                                                &command.scopes,
                                                other_key,
                                                &other.scopes,
                                            )
                                        })
                                    })
                            })
                        {
                            return Err(
                                "invalid or duplicate catalog command name, palette, or key".into(),
                            );
                        }
                    }
                }
                if release.execution.entries().iter().any(|execution| {
                    execution.command.trim().is_empty()
                        || !matches!(execution.target.as_str(), "selection" | "context")
                        || !matches!(execution.output.as_str(), "popup" | "background" | "report")
                }) {
                    return Err(format!(
                        "plugin {} version {} has invalid execution metadata",
                        plugin.id, release.version
                    ));
                }
                if release.requirements.iter().any(|requirement| {
                    requirement.name.trim().is_empty()
                        || requirement.install.trim().is_empty()
                        || requirement
                            .alternatives
                            .iter()
                            .enumerate()
                            .any(|(index, name)| {
                                name.trim().is_empty()
                                    || name == &requirement.name
                                    || requirement.alternatives[..index].contains(name)
                            })
                }) {
                    return Err(format!(
                        "plugin {} version {} has an invalid runtime requirement",
                        plugin.id, release.version
                    ));
                }
                match release.status {
                    VersionStatus::Withdrawn
                        if release
                            .withdrawal_reason
                            .as_deref()
                            .is_none_or(|reason| reason.trim().is_empty()) =>
                    {
                        return Err(format!(
                            "plugin {} version {} is withdrawn without an explanation",
                            plugin.id, release.version
                        ));
                    }
                    VersionStatus::Active if release.withdrawal_reason.is_some() => {
                        return Err(format!(
                            "plugin {} version {} is active but has a withdrawal explanation",
                            plugin.id, release.version
                        ));
                    }
                    _ => {}
                }
                if release.artifacts.is_empty() {
                    return Err(format!(
                        "plugin {} version {} has no artifacts",
                        plugin.id, release.version
                    ));
                }
                let mut platforms = HashSet::new();
                for artifact in &release.artifacts {
                    if !platforms.insert(&artifact.platform) {
                        return Err(format!(
                            "plugin {} version {} has duplicate platform {}",
                            plugin.id, release.version, artifact.platform
                        ));
                    }
                    validate_artifact_from(artifact, &plugin.source).map_err(|e| {
                        format!("plugin {} version {}: {e}", plugin.id, release.version)
                    })?;
                }
            }
        }
        Ok(())
    }

    pub fn find(&self, id: &str) -> Result<&CatalogPlugin, String> {
        let mut matches = self.plugins.iter().filter(|plugin| plugin.id == id);
        let plugin = matches
            .next()
            .ok_or_else(|| format!("unknown plugin {id:?}"))?;
        if matches.next().is_some() {
            return Err(format!(
                "plugin {id:?} exists in multiple catalogs; use --catalog NAME"
            ));
        }
        Ok(plugin)
    }

    pub fn matching(&self, query: &str) -> Vec<&CatalogPlugin> {
        let query = query.to_ascii_lowercase();
        let mut plugins: Vec<_> = self
            .plugins
            .iter()
            .filter(|plugin| {
                query.is_empty()
                    || contains_folded(&plugin.id, &query)
                    || contains_folded(&plugin.display_name, &query)
                    || contains_folded(&plugin.description, &query)
                    || plugin.tags.iter().any(|tag| contains_folded(tag, &query))
            })
            .collect();
        plugins.sort_by(|a, b| a.id.cmp(&b.id));
        plugins
    }

    pub fn select(&self, request: &str) -> Result<Selection<'_>, String> {
        let (id, exact) = parse_request(request)?;
        let plugin = self.find(id)?;
        let current = current_version().ok_or_else(|| "invalid sofka build version".to_string())?;
        let platform = platform()?;
        let release = if let Some(exact) = exact {
            let wanted = Version::parse(exact)
                .map_err(|e| format!("invalid requested version {exact:?}: {e}"))?;
            plugin
                .versions
                .iter()
                .find(|release| Version::parse(&release.version).ok().as_ref() == Some(&wanted))
                .ok_or_else(|| format!("plugin {id} has no version {exact}"))?
        } else {
            plugin
                .versions
                .iter()
                .filter_map(|release| {
                    let version = Version::parse(&release.version).ok()?;
                    let compatible = VersionReq::parse(&release.sofka).ok()?.matches(current);
                    (version.pre.is_empty()
                        && compatible
                        && matches!(release.status, VersionStatus::Active)
                        && release.artifacts.iter().any(|artifact| {
                            artifact.platform == platform || artifact.platform == "any"
                        }))
                    .then_some((version, release))
                })
                .max_by(|(a, _), (b, _)| a.cmp(b))
                .map(|(_, release)| release)
                .ok_or_else(|| format!("plugin {id} has no compatible stable version"))?
        };
        if matches!(release.status, VersionStatus::Withdrawn) {
            return Err(format!(
                "plugin {id} version {} was withdrawn: {}",
                release.version,
                release
                    .withdrawal_reason
                    .as_deref()
                    .unwrap_or("no reason given")
            ));
        }
        // `Catalog` and its fields are public, so this cannot assume the value
        // came through `parse`, which is the only thing that validates it.
        let requirement = VersionReq::parse(&release.sofka).map_err(|e| {
            format!(
                "plugin {id} version {} has an invalid compatibility requirement: {e}",
                release.version
            )
        })?;
        if !requirement.matches(current) {
            return Err(format!(
                "plugin {id} version {} requires sofka {}, current version is {current}",
                release.version, release.sofka
            ));
        }
        let artifact = release
            .artifacts
            .iter()
            .find(|artifact| artifact.platform == platform)
            .or_else(|| {
                release
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.platform == "any")
            })
            .ok_or_else(|| {
                format!(
                    "plugin {id} version {} has no artifact for {platform}",
                    release.version
                )
            })?;
        Ok(Selection {
            plugin,
            version: release,
            artifact,
        })
    }
}

/// This build's own version and target, parsed once. Selection asks for both
/// once per release record, and a catalog holds many.
fn current_version() -> Option<&'static Version> {
    static CURRENT: std::sync::OnceLock<Option<Version>> = std::sync::OnceLock::new();
    CURRENT
        .get_or_init(|| Version::parse(env!("CARGO_PKG_VERSION")).ok())
        .as_ref()
}

impl CatalogPlugin {
    pub fn latest_compatible(&self) -> Option<&CatalogVersion> {
        let current = current_version()?;
        let platform = platform().ok()?;
        self.versions
            .iter()
            .filter_map(|release| {
                let version = Version::parse(&release.version).ok()?;
                let compatible = VersionReq::parse(&release.sofka).ok()?.matches(current);
                (version.pre.is_empty()
                    && compatible
                    && matches!(release.status, VersionStatus::Active))
                .then_some(release)
                .filter(|release| {
                    release
                        .artifacts
                        .iter()
                        .any(|artifact| artifact.platform == platform || artifact.platform == "any")
                })
                .map(|release| (version, release))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, release)| release)
    }
}

/// Case-insensitive substring search that allocates nothing. `needle` is already
/// lowercase; every plugin field would otherwise be copied once per search.
fn contains_folded(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    let Some(last) = haystack.len().checked_sub(needle.len()) else {
        return false;
    };
    (0..=last).any(|start| {
        haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

fn validate_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.as_bytes()[0].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid plugin ID {id:?}; use lowercase ASCII letters, digits, and hyphens"
        ))
    }
}

/// The targets the catalog builds for. A package declares the subset it
/// supports; an artifact names exactly one of them, or `any`.
pub const SUPPORTED_PLATFORMS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

fn metadata_url(url: &str, source: &Source) -> bool {
    url.starts_with("https://")
        || (!source.is_official() && (url.starts_with("http://") || url.starts_with("file://")))
}

fn validate_artifact_from(artifact: &Artifact, source: &Source) -> Result<(), String> {
    if artifact.platform != "any" && !SUPPORTED_PLATFORMS.contains(&artifact.platform.as_str()) {
        return Err(format!("unsupported platform {:?}", artifact.platform));
    }
    if source.is_official() && !artifact.url.starts_with(RELEASE_ROOT) {
        return Err(format!(
            "artifact URL must be a release asset of {REPOSITORY}"
        ));
    }
    if !source.is_official() {
        source.artifact_location(&artifact.url)?;
    }
    if artifact.blake3.len() != 64 || !artifact.blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("artifact BLAKE3 must contain 64 hexadecimal characters".into());
    }
    if artifact.size == 0 || artifact.size > ARTIFACT_MAX_BYTES as u64 {
        return Err("artifact compressed size must be between 1 byte and 50 MiB".into());
    }
    Ok(())
}

pub fn parse_request(request: &str) -> Result<(&str, Option<&str>), String> {
    let (id, version) = request
        .split_once('@')
        .map_or((request, None), |(id, version)| (id, Some(version)));
    validate_id(id)?;
    if version.is_some_and(str::is_empty) || version.is_some_and(|v| v.contains('@')) {
        return Err(format!("invalid plugin request {request:?}"));
    }
    Ok((id, version))
}

pub fn platform() -> Result<&'static str, String> {
    platform_for(std::env::consts::ARCH, std::env::consts::OS)
}

fn platform_for(arch: &str, os: &str) -> Result<&'static str, String> {
    match (arch, os) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "windows") => Ok("x86_64-pc-windows-msvc"),
        ("aarch64", "windows") => Ok("aarch64-pc-windows-msvc"),
        (arch, os) => Err(format!("unsupported plugin platform {arch}-{os}")),
    }
}

pub fn cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(path).join("sofka").join("plugins");
    }
    if let Some(home) = crate::config::home_dir() {
        return PathBuf::from(home)
            .join(".cache")
            .join("sofka")
            .join("plugins");
    }
    std::env::temp_dir().join("sofka").join("plugins")
}

pub fn config_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::config::home_dir().map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            "cannot determine config directory: set XDG_CONFIG_HOME or HOME".to_string()
        })?;
    Ok(base.join("sofka"))
}

async fn load(offline: bool) -> Result<CatalogSnapshot, String> {
    let cache_path = cache_dir().join("catalog-cache.json");
    if offline {
        return load_cached(&cache_path);
    }
    let commit_bytes = get(COMMIT_URL, CATALOG_MAX_BYTES)
        .await
        .map_err(|error| catalog_fetch_error(error, &cache_path))?;
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    let commit: Commit = serde_json::from_slice(&commit_bytes)
        .map_err(|e| format!("invalid GitHub commit response: {e}"))?;
    if commit.sha.len() != 40 || !commit.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("GitHub returned an invalid catalog commit".into());
    }
    let url = format!("{RAW_ROOT}/{}/index.json", commit.sha);
    let bytes = get(&url, CATALOG_MAX_BYTES)
        .await
        .map_err(|error| catalog_fetch_error(error, &cache_path))?;
    let catalog = Catalog::parse(&bytes)?;
    finish_load(commit.sha, catalog, &cache_path)
}

fn catalog_fetch_error(error: String, cache_path: &Path) -> String {
    if cache_path.is_file() {
        format!("{error}; check the network and retry, or pass --offline to use the cached catalog")
    } else {
        format!("{error}; check the network and retry")
    }
}

fn finish_load(
    commit: String,
    catalog: Catalog,
    cache_path: &Path,
) -> Result<CatalogSnapshot, String> {
    let fetched_at = now();
    let cached = CachedCatalog {
        schema_version: 1,
        commit: commit.clone(),
        fetched_at,
        catalog: catalog.clone(),
    };
    let cache = serde_json::to_string(&cached).map_err(|e| e.to_string())?;
    if let Err(error) = crate::atomicfile::write(cache_path, &cache) {
        eprintln!(
            "warning: could not cache catalog at {}: {error}",
            cache_path.display()
        );
    }
    Ok(CatalogSnapshot {
        catalog,
        commit,
        fetched_at,
        offline: false,
    })
}

/// The cached catalog when one is readable, for commands that must work
/// without the network and without failing when nothing has been fetched yet.
pub fn cached() -> Option<CatalogSnapshot> {
    cached_selected(None)
}

pub fn cached_selected(selected: Option<&str>) -> Option<CatalogSnapshot> {
    sources::cached_sources(selected)
}

fn load_cached(path: &Path) -> Result<CatalogSnapshot, String> {
    let bytes = std::fs::read(path).map_err(|e| {
        format!(
            "offline catalog is unavailable at {}: {e}; run without --offline once",
            path.display()
        )
    })?;
    if bytes.len() > CATALOG_MAX_BYTES + 1024 * 1024 {
        return Err("cached catalog exceeds its size limit".into());
    }
    let cached: CachedCatalog =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid cached catalog: {e}"))?;
    if cached.schema_version != 1 {
        return Err("unsupported catalog cache version".into());
    }
    if cached.commit.len() != 40 || !cached.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("cached catalog has an invalid commit".into());
    }
    cached.catalog.validate()?;
    Ok(CatalogSnapshot {
        catalog: cached.catalog,
        commit: cached.commit,
        fetched_at: cached.fetched_at,
        offline: true,
    })
}

pub fn age(fetched_at: u64) -> String {
    let seconds = now().saturating_sub(fetched_at);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[derive(Debug)]
pub struct ArtifactArchive {
    path: PathBuf,
    temporary: bool,
}

impl ArtifactArchive {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ArtifactArchive {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub async fn artifact(artifact: &Artifact, offline: bool) -> Result<ArtifactArchive, String> {
    artifact_in(&cache_dir(), artifact, offline).await
}

pub async fn artifact_in(
    cache: &Path,
    artifact: &Artifact,
    offline: bool,
) -> Result<ArtifactArchive, String> {
    artifact_from(cache, artifact, offline, &Source::default()).await
}

pub async fn artifact_from(
    cache: &Path,
    artifact: &Artifact,
    offline: bool,
    source: &Source,
) -> Result<ArtifactArchive, String> {
    validate_artifact_from(artifact, source)?;
    let path = cache
        .join("artifacts")
        .join(format!("{}.tar.zst", artifact.blake3.to_ascii_lowercase()));
    if path.is_file() {
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if verify_artifact(artifact, &bytes).is_ok() {
            return Ok(ArtifactArchive {
                path,
                temporary: false,
            });
        }
        std::fs::remove_file(&path)
            .map_err(|e| format!("removing corrupt cached artifact {}: {e}", path.display()))?;
    }
    if !source.is_official() {
        let location = source.artifact_location(&artifact.url)?;
        let bytes = source
            .read(&location, ARTIFACT_MAX_BYTES, ARTIFACT_BUDGET, offline)
            .await?;
        verify_artifact(artifact, &bytes)?;
        return store_artifact(&path, &bytes);
    }
    if offline {
        return Err(format!(
            "artifact {} is not cached; run install without --offline once",
            artifact.blake3
        ));
    }
    let bytes = get_with(&artifact.url, ARTIFACT_MAX_BYTES, ARTIFACT_BUDGET, false).await?;
    verify_artifact(artifact, &bytes)?;
    store_artifact(&path, &bytes)
}

fn store_artifact(path: &Path, bytes: &[u8]) -> Result<ArtifactArchive, String> {
    if let Err(cache_error) = write_bytes(path, bytes) {
        let temporary = temporary_artifact_path();
        write_bytes(&temporary, bytes).map_err(|temporary_error| {
            format!(
                "caching artifact at {} failed: {cache_error}; storing it temporarily at {} \
                 failed: {temporary_error}",
                path.display(),
                temporary.display()
            )
        })?;
        eprintln!(
            "warning: could not cache artifact at {}: {cache_error}; using temporary file {}",
            path.display(),
            temporary.display()
        );
        return Ok(ArtifactArchive {
            path: temporary,
            temporary: true,
        });
    }
    Ok(ArtifactArchive {
        path: path.to_path_buf(),
        temporary: false,
    })
}

fn temporary_artifact_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "sofka-plugin-artifact-{}-{nonce:x}.tar.zst",
        std::process::id()
    ))
}

/// Published bytes must match the catalog exactly. A truncated or interrupted
/// download fails the length check before its digest is ever considered.
fn verify_artifact(artifact: &Artifact, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 != artifact.size {
        return Err(format!(
            "artifact size mismatch: catalog says {}, downloaded {}",
            artifact.size,
            bytes.len()
        ));
    }
    let actual = digest(bytes);
    if actual != artifact.blake3.to_ascii_lowercase() {
        return Err(format!(
            "artifact BLAKE3 mismatch: expected {}, received {actual}",
            artifact.blake3
        ));
    }
    Ok(())
}

/// BLAKE3 over the whole slice, using every core. Artifacts run to tens of
/// megabytes and this is the only work between download and install.
pub fn digest(bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update_rayon(bytes);
    hex(hasher.finalize().as_bytes())
}

/// Lowercase hex. A `format!` per byte costs twenty times as much, and this runs
/// once per file of every package sofka verifies.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = path.with_extension(format!("tmp-{}-{nonce:x}", std::process::id()));
    let mut created = false;
    let result = (|| {
        let mut file = std::fs::File::options()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        created = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(temp);
    }
    result
}

type HttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// One client for the whole command. Building it parses the system trust store,
/// which costs more than the request that follows, and every command makes at
/// least two requests — the commit, then the index, then any artifacts.
fn client(allow_http: bool) -> Result<HttpClient, String> {
    if allow_http {
        // Custom HTTP catalogs and the loopback test server use this client.
        return build_client(true);
    }
    static CLIENT: std::sync::OnceLock<Result<HttpClient, String>> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| build_client(false)).clone()
}

fn build_client(allow_http: bool) -> Result<HttpClient, String> {
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|e| format!("loading system TLS roots: {e}"))?;
    let https = if allow_http {
        builder.https_or_http().enable_http1().build()
    } else {
        builder.https_only().enable_http1().build()
    };
    Ok(
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(https),
    )
}

async fn get(url: &str, limit: usize) -> Result<Vec<u8>, String> {
    get_with(url, limit, METADATA_BUDGET, false).await
}

async fn get_with(
    url: &str,
    limit: usize,
    budget: Budget,
    allow_test_http: bool,
) -> Result<Vec<u8>, String> {
    tokio::time::timeout(
        budget.total,
        get_inner(url, limit, budget, allow_test_http, None),
    )
    .await
    .map_err(|_| {
        format!(
            "request timed out after {}ms: {url}",
            budget.total.as_millis()
        )
    })?
}

async fn get_inner(
    url: &str,
    limit: usize,
    budget: Budget,
    allow_test_http: bool,
    source: Option<&Source>,
) -> Result<Vec<u8>, String> {
    let client =
        client(allow_test_http || source.is_some_and(|source| source.url.starts_with("http://")))?;
    let mut current = url.to_string();
    for redirects in 0..=REDIRECT_LIMIT {
        let uri: http::Uri = current
            .parse()
            .map_err(|e| format!("invalid catalog URL: {e}"))?;
        if let Some(source) = source {
            source.validate_url(&current)?;
        } else {
            validate_http_uri(&uri, allow_test_http)?;
        }
        let base = uri.clone();
        let request = http::Request::get(uri)
            .header(
                http::header::USER_AGENT,
                format!("sofka/{}", env!("CARGO_PKG_VERSION")),
            )
            .header(http::header::ACCEPT, "application/vnd.github+json")
            .body(Full::new(Bytes::new()))
            .map_err(|e| format!("building request: {e}"))?;
        let response = tokio::time::timeout(budget.stall, client.request(request))
            .await
            .map_err(|_| {
                format!(
                    "request timed out after {}ms with no response: {current}",
                    budget.stall.as_millis()
                )
            })?
            .map_err(|e| format!("requesting {current}: {e}"))?;
        if response.status().is_redirection() {
            if redirects == REDIRECT_LIMIT {
                return Err("too many GitHub download redirects".into());
            }
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "GitHub redirect has no valid Location".to_string())?;
            current = resolve_redirect(&base, location)?;
            continue;
        }
        let status = response.status();
        let rate_limited = status == http::StatusCode::FORBIDDEN
            && response.headers().contains_key("x-ratelimit-remaining");
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = tokio::time::timeout(budget.stall, body.frame())
            .await
            .map_err(|_| {
                format!(
                    "download timed out after {}ms with no data: {current}",
                    budget.stall.as_millis()
                )
            })?
        {
            let frame = frame.map_err(|e| format!("reading response: {e}"))?;
            if let Ok(data) = frame.into_data() {
                if bytes.len().saturating_add(data.len()) > limit {
                    return Err(format!("download exceeds {} MiB", limit / 1024 / 1024));
                }
                bytes.extend_from_slice(&data);
            }
        }
        if !status.is_success() {
            if rate_limited {
                return Err("GitHub API rate limit reached; retry later".into());
            }
            let detail: String = String::from_utf8_lossy(&bytes)
                .trim()
                .chars()
                .take(200)
                .collect();
            return Err(if detail.is_empty() {
                format!("HTTP {status} from {current}")
            } else {
                format!("HTTP {status} from {current}: {detail}")
            });
        }
        return Ok(bytes);
    }
    unreachable!()
}

/// Resolve a `Location` against the URL it came from. RFC 7231 allows a
/// relative reference, and one used to fail the next scheme check as "catalog
/// downloads require HTTPS" — a message about the wrong problem. Whatever this
/// produces still goes through `validate_http_uri`, so a redirect can no more
/// leave the allowed hosts than an absolute one can.
fn resolve_redirect(base: &http::Uri, location: &str) -> Result<String, String> {
    let location = location.trim();
    if location.is_empty() {
        return Err("GitHub redirect has an empty Location".into());
    }
    let absolute = location.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    });
    if absolute {
        return Ok(location.to_string());
    }
    let scheme = base.scheme_str().unwrap_or("https");
    // Scheme-relative: the target names its own authority.
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{rest}"));
    }
    let authority = base
        .authority()
        .ok_or_else(|| format!("cannot resolve redirect {location:?} against {base}"))?;
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    // Relative to the base's directory, as a browser would read it.
    let path = base.path();
    let directory = &path[..path.rfind('/').map_or(0, |slash| slash + 1)];
    Ok(format!("{scheme}://{authority}{directory}{location}"))
}

fn validate_http_uri(uri: &http::Uri, allow_test_http: bool) -> Result<(), String> {
    let local_test = allow_test_http
        && uri.scheme_str() == Some("http")
        && matches!(uri.host(), Some("127.0.0.1" | "localhost"));
    if uri.scheme_str() != Some("https") && !local_test {
        return Err("catalog downloads require HTTPS".into());
    }
    let host = uri.host().unwrap_or_default();
    if local_test
        || matches!(
            host,
            "api.github.com"
                | "raw.githubusercontent.com"
                | "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
        )
    {
        Ok(())
    } else {
        Err(format!("refusing download from untrusted host {host:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_targets_are_valid_catalog_platforms() {
        for (arch, target) in [
            ("x86_64", "x86_64-pc-windows-msvc"),
            ("aarch64", "aarch64-pc-windows-msvc"),
        ] {
            assert_eq!(platform_for(arch, "windows").unwrap(), target);
            let mut value = catalog();
            value.plugins[0].versions[0].artifacts[0].platform = target.into();
            value.validate().unwrap();
        }
        assert!(platform_for("riscv64", "windows").is_err());
    }

    fn server(response: Vec<u8>, delay: Duration) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            std::thread::sleep(delay);
            let _ = stream.write_all(&response);
        });
        format!("http://{address}/fixture")
    }

    /// A budget whose total and stall are the same, so a test that wants one
    /// deadline does not have to reason about two.
    fn budget(total: Duration) -> Budget {
        Budget {
            total,
            stall: total,
        }
    }

    /// Sends the head and then stops. Only a stall budget ends a download whose
    /// body never arrives.
    fn silent_after_head(head: Vec<u8>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&head);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(30));
        });
        format!("http://{address}/fixture")
    }

    pub(super) fn catalog() -> Catalog {
        Catalog {
            schema_version: 1,
            generated_at: "2026-09-11T00:00:00Z".into(),
            plugins: vec![CatalogPlugin {
                source: Source::default(),
                revision: String::new(),
                id: "resource-summary".into(),
                display_name: "Resource summary".into(),
                description: "Summarize a resource".into(),
                tags: vec!["report".into()],
                publisher: "sofka".into(),
                repository: "https://github.com/nklmilojevic/sofka-plugins".into(),
                versions: vec![CatalogVersion {
                    version: "0.1.0".into(),
                    sofka: ">=0.1.0, <1.0.0".into(),
                    source_commit: "0".repeat(40),
                    license: "MIT OR Apache-2.0".into(),
                    readme: "https://example.com/readme".into(),
                    requirements: vec![],
                    execution: CatalogExecution::Legacy(Execution {
                        command: "./resource-summary".into(),
                        target: "selection".into(),
                        output: "report".into(),
                        mutating: false,
                        confirm: false,
                        dangerous: false,
                        network_load: false,
                    }),
                    status: VersionStatus::Active,
                    withdrawal_reason: None,
                    artifacts: vec![Artifact {
                        platform: "any".into(),
                        url: format!(
                            "{RELEASE_ROOT}resource-summary-v0.1.0/resource-summary.tar.zst"
                        ),
                        blake3: "0".repeat(64),
                        size: 10,
                    }],
                }],
            }],
        }
    }

    fn command_catalog() -> serde_json::Value {
        let mut value = serde_json::to_value(catalog()).unwrap();
        value["schema_version"] = 2.into();
        let release = value["plugins"][0]["versions"][0].as_object_mut().unwrap();
        let mut command = serde_json::Map::new();
        for field in [
            "command",
            "target",
            "output",
            "mutating",
            "confirm",
            "dangerous",
            "network_load",
        ] {
            command.insert(field.into(), release.remove(field).unwrap());
        }
        command.insert("name".into(), "Status".into());
        command.insert("palette".into(), "cert-manager-status".into());
        command.insert("args".into(), serde_json::json!(["status"]));
        command.insert("scopes".into(), serde_json::json!(["certificates"]));
        release.insert("commands".into(), serde_json::json!([command]));
        value
    }

    #[test]
    fn command_catalogs_reject_mixed_empty_and_duplicate_definitions() {
        let value = command_catalog();
        Catalog::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        for case in [
            "schema",
            "mixed",
            "empty",
            "duplicate",
            "mutation",
            "missing_mutation",
            "palette",
        ] {
            let mut invalid = value.clone();
            let release = &mut invalid["plugins"][0]["versions"][0];
            match case {
                "schema" => invalid["schema_version"] = 1.into(),
                "mixed" => release["mutating"] = false.into(),
                "empty" => release["commands"] = serde_json::json!([]),
                "duplicate" => {
                    let command = release["commands"][0].clone();
                    release["commands"].as_array_mut().unwrap().push(command);
                }
                "mutation" => release["commands"][0]["mutating"] = "false".into(),
                "missing_mutation" => {
                    release["commands"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("mutating");
                }
                _ => release["commands"][0]["palette"] = "xray".into(),
            }
            assert!(
                Catalog::parse(&serde_json::to_vec(&invalid).unwrap()).is_err(),
                "accepted {case}"
            );
        }
        let mut mixed = value.clone();
        mixed["plugins"][0]["versions"][0]["mutating"] = false.into();
        assert!(
            serde_json::from_value::<Catalog>(mixed).is_err(),
            "cached catalogs must reject mixed forms too"
        );
    }

    #[test]
    fn command_catalogs_share_keys_only_across_disjoint_scopes() {
        let with_keys = |first: (&str, serde_json::Value), second: (&str, serde_json::Value)| {
            let mut value = command_catalog();
            let commands = value["plugins"][0]["versions"][0]["commands"]
                .as_array_mut()
                .unwrap();
            let mut other = commands[0].clone();
            other["name"] = "Renew".into();
            other["palette"] = "cert-manager-renew".into();
            commands.push(other);
            for (command, (key, scopes)) in commands.iter_mut().zip([first, second]) {
                command["key"] = key.into();
                command["scopes"] = scopes;
            }
            Catalog::parse(&serde_json::to_vec(&value).unwrap())
        };
        let pods = || serde_json::json!(["pods"]);
        let deployments = || serde_json::json!(["deployments"]);
        with_keys(("x", pods()), ("x", deployments())).unwrap();
        for (first, second) in [
            (
                ("x", pods()),
                ("x", serde_json::json!(["pods", "services"])),
            ),
            (("x", pods()), ("x", serde_json::json!([]))),
            (("ctrl-x", pods()), ("ctrl-X", pods())),
            (("X", pods()), ("shift-x", pods())),
        ] {
            let case = format!("{first:?} {second:?}");
            assert!(with_keys(first, second).is_err(), "accepted {case}");
        }
    }

    #[test]
    fn validates_and_selects_catalog_entries() {
        let catalog = catalog();
        catalog.validate().unwrap();
        let selected = catalog.select("resource-summary").unwrap();
        assert_eq!(selected.version.version, "0.1.0");
        assert_eq!(selected.artifact.platform, "any");
        assert!(catalog.select("missing").is_err());
    }

    #[test]
    fn parsing_accepts_additive_fields_and_reports_future_schemas_first() {
        let mut value = serde_json::to_value(catalog()).unwrap();
        value["future_catalog_field"] = true.into();
        value["plugins"][0]["future_plugin_field"] = true.into();
        value["plugins"][0]["versions"][0]["future_version_field"] = true.into();
        value["plugins"][0]["versions"][0]["requirements"] = serde_json::json!([{
            "name": "jq",
            "install": "install jq",
            "future_requirement_field": true,
        }]);
        value["plugins"][0]["versions"][0]["artifacts"][0]["future_artifact_field"] = true.into();

        Catalog::parse(&serde_json::to_vec(&value).unwrap()).unwrap();

        let future = serde_json::json!({
            "schema_version": 3,
            "future_layout": {},
        });
        let error = Catalog::parse(&serde_json::to_vec(&future).unwrap()).unwrap_err();
        assert!(
            error.contains("unsupported catalog schema_version 3"),
            "{error}"
        );
    }

    #[test]
    fn search_is_case_insensitive_sorted_and_matches_tags() {
        let mut catalog = catalog();
        let mut second = catalog.plugins[0].clone();
        second.id = "alpha".into();
        second.display_name = "Other".into();
        second.description = "Something else".into();
        catalog.plugins.push(second);
        let ids: Vec<_> = catalog
            .matching("REPORT")
            .into_iter()
            .map(|plugin| plugin.id.as_str())
            .collect();
        assert_eq!(ids, ["alpha", "resource-summary"]);
        assert!(catalog.matching("absent").is_empty());
    }

    #[test]
    fn rejects_unsafe_ids_urls_hashes_and_withdrawals() {
        for id in ["../bad", "Bad", ".", "bad/name"] {
            let mut catalog = catalog();
            catalog.plugins[0].id = id.into();
            assert!(catalog.validate().is_err(), "accepted {id}");
        }
        let mut invalid_url = catalog();
        invalid_url.plugins[0].versions[0].artifacts[0].url = "https://example.com/a".into();
        assert!(invalid_url.validate().is_err());
        let mut withdrawn = catalog();
        withdrawn.plugins[0].versions[0].status = VersionStatus::Withdrawn;
        assert!(withdrawn.validate().is_err());
    }

    #[test]
    fn exact_withdrawn_versions_are_rejected() {
        let mut catalog = catalog();
        catalog.plugins[0].versions[0].status = VersionStatus::Withdrawn;
        catalog.plugins[0].versions[0].withdrawal_reason = Some("unsafe output".into());
        let error = catalog.select("resource-summary@0.1.0").unwrap_err();
        assert!(error.contains("withdrawn"));
    }

    #[test]
    fn selection_skips_prerelease_incompatible_and_unsupported_versions() {
        let mut catalog = catalog();
        let base = catalog.plugins[0].versions[0].clone();
        let mut prerelease = base.clone();
        prerelease.version = "9.0.0-beta.1".into();
        let mut incompatible = base.clone();
        incompatible.version = "8.0.0".into();
        incompatible.sofka = ">=2.0.0".into();
        let mut wrong_platform = base;
        wrong_platform.version = "7.0.0".into();
        wrong_platform.artifacts[0].platform = if platform().unwrap().contains("linux") {
            "aarch64-apple-darwin".into()
        } else {
            "aarch64-unknown-linux-gnu".into()
        };
        catalog.plugins[0]
            .versions
            .extend([prerelease, incompatible, wrong_platform]);

        assert_eq!(
            catalog.select("resource-summary").unwrap().version.version,
            "0.1.0"
        );
        assert!(catalog.select("resource-summary@8.0.0").is_err());
    }

    #[test]
    fn cached_catalog_is_validated_before_offline_use() {
        let path =
            std::env::temp_dir().join(format!("sofka-catalog-cache-{}.json", std::process::id()));
        let cached = CachedCatalog {
            schema_version: 1,
            commit: "a".repeat(40),
            fetched_at: 1,
            catalog: catalog(),
        };
        std::fs::write(&path, serde_json::to_vec(&cached).unwrap()).unwrap();
        let snapshot = load_cached(&path).unwrap();
        assert!(snapshot.offline);
        assert_eq!(snapshot.commit, "a".repeat(40));

        std::fs::write(&path, b"{}").unwrap();
        assert!(load_cached(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn network_errors_offer_offline_mode_only_when_a_cache_exists() {
        let dir = scratch("network-guidance");
        let cache = dir.join("catalog-cache.json");
        let without = catalog_fetch_error("network failed".into(), &cache);
        assert!(without.contains("check the network and retry"));
        assert!(!without.contains("--offline"));

        std::fs::write(&cache, "cached").unwrap();
        let with = catalog_fetch_error("network failed".into(), &cache);
        assert!(with.contains("check the network and retry"));
        assert!(with.contains("--offline"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_catalog_cache_write_failure_does_not_discard_the_snapshot() {
        let dir = scratch("catalog-cache-write");
        let blocked = dir.join("not-a-directory");
        std::fs::write(&blocked, "file").unwrap();

        let snapshot = finish_load(
            "a".repeat(40),
            catalog(),
            &blocked.join("catalog-cache.json"),
        )
        .unwrap();

        assert_eq!(snapshot.commit, "a".repeat(40));
        assert!(!snapshot.offline);
        let _ = std::fs::remove_dir_all(dir);
    }

    type Mutation = (&'static str, Box<dyn Fn(&mut Catalog)>);

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sofka-catalog-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parsed_requests_split_only_on_a_single_version_suffix() {
        assert_eq!(parse_request("summary").unwrap(), ("summary", None));
        assert_eq!(
            parse_request("summary@1.2.3").unwrap(),
            ("summary", Some("1.2.3"))
        );
        for bad in [
            "",
            "summary@",
            "summary@1@2",
            "Summary",
            "-summary",
            "a/b",
            "..",
        ] {
            assert!(parse_request(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn validation_rejects_every_malformed_release_field() {
        let mutate: Vec<Mutation> = vec![
            ("schema", Box::new(|c: &mut Catalog| c.schema_version = 3)),
            (
                "display name",
                Box::new(|c: &mut Catalog| c.plugins[0].display_name = "  ".into()),
            ),
            (
                "repository",
                Box::new(|c: &mut Catalog| c.plugins[0].repository = "http://insecure".into()),
            ),
            (
                "version",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].version = "one".into()),
            ),
            (
                "sofka range",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].sofka = "latest".into()),
            ),
            (
                "source commit",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].source_commit = "abc".into()),
            ),
            (
                "license",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].license = " ".into()),
            ),
            (
                "readme",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].readme = "ftp://x".into()),
            ),
            (
                "command",
                Box::new(|c: &mut Catalog| {
                    if let CatalogExecution::Legacy(execution) =
                        &mut c.plugins[0].versions[0].execution
                    {
                        execution.command = " ".into()
                    }
                }),
            ),
            (
                "target",
                Box::new(|c: &mut Catalog| {
                    if let CatalogExecution::Legacy(execution) =
                        &mut c.plugins[0].versions[0].execution
                    {
                        execution.target = "cluster".into()
                    }
                }),
            ),
            (
                "output",
                Box::new(|c: &mut Catalog| {
                    if let CatalogExecution::Legacy(execution) =
                        &mut c.plugins[0].versions[0].execution
                    {
                        execution.output = "terminal".into()
                    }
                }),
            ),
            (
                "requirement",
                Box::new(|c: &mut Catalog| {
                    c.plugins[0].versions[0].requirements = vec![RuntimeRequirement {
                        name: " ".into(),
                        alternatives: Vec::new(),
                        install: "brew install".into(),
                    }];
                }),
            ),
            (
                "artifacts",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].artifacts.clear()),
            ),
            (
                "digest",
                Box::new(|c: &mut Catalog| {
                    c.plugins[0].versions[0].artifacts[0].blake3 = "zz".into();
                }),
            ),
            (
                "size",
                Box::new(|c: &mut Catalog| c.plugins[0].versions[0].artifacts[0].size = 0),
            ),
            (
                "oversize",
                Box::new(|c: &mut Catalog| {
                    c.plugins[0].versions[0].artifacts[0].size = ARTIFACT_MAX_BYTES as u64 + 1;
                }),
            ),
            (
                "platform",
                Box::new(|c: &mut Catalog| {
                    c.plugins[0].versions[0].artifacts[0].platform = "risc".into();
                }),
            ),
            (
                "withdrawal without a reason",
                Box::new(|c: &mut Catalog| {
                    c.plugins[0].versions[0].status = VersionStatus::Withdrawn;
                }),
            ),
            (
                "active with a reason",
                Box::new(|c: &mut Catalog| {
                    c.plugins[0].versions[0].withdrawal_reason = Some("why".into());
                }),
            ),
        ];
        for (label, change) in mutate {
            let mut catalog = catalog();
            change(&mut catalog);
            assert!(catalog.validate().is_err(), "accepted {label}");
        }
        catalog().validate().unwrap();
    }

    #[test]
    fn validation_rejects_colliding_and_repeated_entries() {
        let mut folded = catalog();
        let mut twin = folded.plugins[0].clone();
        twin.id = "resource-summary".into();
        folded.plugins.push(twin);
        assert!(folded.validate().is_err());

        let mut repeated = catalog();
        let twin = repeated.plugins[0].versions[0].clone();
        repeated.plugins[0].versions.push(twin);
        assert!(repeated.validate().is_err());

        let mut platforms = catalog();
        let twin = platforms.plugins[0].versions[0].artifacts[0].clone();
        platforms.plugins[0].versions[0].artifacts.push(twin);
        assert!(platforms.validate().is_err());
    }

    #[test]
    fn an_empty_query_lists_every_plugin_and_an_unknown_id_is_an_error() {
        let catalog = catalog();
        assert_eq!(catalog.matching("").len(), catalog.plugins.len());
        assert!(catalog.find("resource-summary").is_ok());
        assert!(catalog.find("absent").unwrap_err().contains("unknown"));
    }

    #[test]
    fn latest_compatible_ignores_withdrawn_and_prerelease_releases() {
        let mut catalog = catalog();
        assert!(catalog.plugins[0].latest_compatible().is_some());
        let base = catalog.plugins[0].versions[0].clone();
        let mut newer = base.clone();
        newer.version = "0.2.0".into();
        newer.status = VersionStatus::Withdrawn;
        newer.withdrawal_reason = Some("unsafe".into());
        let mut prerelease = base;
        prerelease.version = "0.3.0-rc.1".into();
        catalog.plugins[0].versions.extend([newer, prerelease]);
        assert_eq!(
            catalog.plugins[0].latest_compatible().unwrap().version,
            "0.1.0"
        );

        catalog.plugins[0].versions[0].status = VersionStatus::Withdrawn;
        catalog.plugins[0].versions[0].withdrawal_reason = Some("unsafe".into());
        assert!(catalog.plugins[0].latest_compatible().is_none());
    }

    #[test]
    fn published_bytes_must_match_the_recorded_size_and_digest() {
        let bytes = b"package bytes";
        let mut artifact = catalog().plugins[0].versions[0].artifacts[0].clone();
        artifact.size = bytes.len() as u64;
        artifact.blake3 = digest(bytes);
        verify_artifact(&artifact, bytes).unwrap();
        // An upper-case digest in the catalog is the same digest.
        let mut folded = artifact.clone();
        folded.blake3 = artifact.blake3.to_ascii_uppercase();
        verify_artifact(&folded, bytes).unwrap();

        let truncated = &bytes[..4];
        assert!(
            verify_artifact(&artifact, truncated)
                .unwrap_err()
                .contains("size mismatch")
        );
        let mut swapped = artifact;
        swapped.blake3 = "9".repeat(64);
        assert!(
            verify_artifact(&swapped, bytes)
                .unwrap_err()
                .contains("BLAKE3 mismatch")
        );
    }

    #[tokio::test]
    async fn cached_artifacts_are_reused_and_corrupt_ones_are_discarded() {
        let cache = scratch("artifacts");
        let bytes = b"package bytes";
        let mut artifact = catalog().plugins[0].versions[0].artifacts[0].clone();
        artifact.size = bytes.len() as u64;
        artifact.blake3 = digest(bytes);
        let path = cache
            .join("artifacts")
            .join(format!("{}.tar.zst", artifact.blake3));

        // Nothing cached: offline is an error that says how to recover.
        let error = artifact_in(&cache, &artifact, true).await.unwrap_err();
        assert!(error.contains("not cached"), "{error}");

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let cached = artifact_in(&cache, &artifact, true).await.unwrap();
        assert_eq!(cached.path(), path);

        // A half-written cache entry is removed rather than trusted.
        std::fs::write(&path, b"interrupted").unwrap();
        let error = artifact_in(&cache, &artifact, true).await.unwrap_err();
        assert!(error.contains("not cached"), "{error}");
        assert!(!path.exists());

        // The URL allowlist applies before anything is read or fetched.
        let mut foreign = artifact;
        foreign.url = "https://example.com/package.tar.zst".into();
        assert!(artifact_in(&cache, &foreign, true).await.is_err());
        let _ = std::fs::remove_dir_all(cache);
    }

    #[test]
    fn a_failed_artifact_cache_write_uses_a_temporary_file() {
        let dir = scratch("artifact-cache-write");
        let blocked = dir.join("not-a-directory");
        std::fs::write(&blocked, "file").unwrap();
        let cache_path = blocked.join("artifact.tar.zst");

        let archive = store_artifact(&cache_path, b"verified archive").unwrap();
        let temporary = archive.path().to_path_buf();
        assert_ne!(temporary, cache_path);
        assert_eq!(std::fs::read(&temporary).unwrap(), b"verified archive");

        drop(archive);
        assert!(!temporary.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cache_is_rejected_when_it_is_oversized_or_internally_inconsistent() {
        let dir = scratch("cache-guards");
        let path = dir.join("catalog-cache.json");
        let cached = CachedCatalog {
            schema_version: 1,
            commit: "a".repeat(40),
            fetched_at: 1,
            catalog: catalog(),
        };

        let mut future = cached.clone();
        future.schema_version = 3;
        std::fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(
            load_cached(&path)
                .unwrap_err()
                .contains("unsupported catalog cache version")
        );

        let mut commit = cached.clone();
        commit.commit = "not-a-commit".into();
        std::fs::write(&path, serde_json::to_vec(&commit).unwrap()).unwrap();
        assert!(load_cached(&path).unwrap_err().contains("invalid commit"));

        let mut invalid = cached;
        invalid.catalog.plugins[0].versions[0].sofka = "latest".into();
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(load_cached(&path).is_err());

        std::fs::write(&path, vec![b' '; CATALOG_MAX_BYTES + 1024 * 1024 + 1]).unwrap();
        assert!(load_cached(&path).unwrap_err().contains("size limit"));

        assert!(load_cached(&dir.join("absent.json")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn untrusted_hosts_and_schemes_are_refused_before_any_request() {
        for url in [
            "http://api.github.com/x",
            "https://example.com/x",
            "https://githubusercontent.com.evil.test/x",
        ] {
            let uri: http::Uri = url.parse().unwrap();
            assert!(validate_http_uri(&uri, false).is_err(), "accepted {url}");
        }
        // Non-HTTP schemes are refused by the parser or the allowlist; either
        // way they never become a request.
        for url in ["file:///etc/passwd", "ftp://example.com/x", "not a url"] {
            let refused = url
                .parse::<http::Uri>()
                .map_or(true, |uri| validate_http_uri(&uri, false).is_err());
            assert!(refused, "accepted {url}");
        }
        for url in [
            "https://api.github.com/x",
            "https://raw.githubusercontent.com/x",
            "https://github.com/x",
            "https://objects.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com/x",
        ] {
            let uri: http::Uri = url.parse().unwrap();
            validate_http_uri(&uri, false).unwrap();
        }
        // The test escape hatch stays pinned to loopback.
        let loopback: http::Uri = "http://127.0.0.1:1/x".parse().unwrap();
        validate_http_uri(&loopback, true).unwrap();
        let remote: http::Uri = "http://10.0.0.1/x".parse().unwrap();
        assert!(validate_http_uri(&remote, true).is_err());
    }

    #[test]
    fn cache_age_is_reported_in_the_largest_whole_unit() {
        let now = now();
        assert_eq!(age(now), "0s");
        assert_eq!(age(now - 90), "1m");
        assert_eq!(age(now - 3 * 3600), "3h");
        assert_eq!(age(now - 5 * 86_400), "5d");
        // A clock that moved backwards reports no age rather than panicking.
        assert_eq!(age(now + 600), "0s");
    }

    #[test]
    fn catalog_parser_enforces_the_download_limit() {
        assert!(Catalog::parse(&vec![b' '; CATALOG_MAX_BYTES + 1]).is_err());
    }

    #[tokio::test]
    async fn http_reader_rejects_failures_oversize_and_timeouts() {
        let url = server(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\ndown".to_vec(),
            Duration::ZERO,
        );
        let error = get_with(&url, 100, budget(Duration::from_secs(1)), true)
            .await
            .unwrap_err();
        assert!(error.contains("503") && error.contains("down"));

        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world".to_vec(),
            Duration::ZERO,
        );
        let error = get_with(&url, 5, budget(Duration::from_secs(1)), true)
            .await
            .unwrap_err();
        assert!(error.contains("exceeds"));

        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort".to_vec(),
            Duration::ZERO,
        );
        assert!(
            get_with(&url, 100, budget(Duration::from_secs(1)), true)
                .await
                .is_err()
        );

        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            Duration::from_millis(100),
        );
        let error = get_with(&url, 100, budget(Duration::from_millis(10)), true)
            .await
            .unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[tokio::test]
    async fn http_reader_reports_public_rate_limits() {
        let url = server(
            b"HTTP/1.1 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
            Duration::ZERO,
        );
        let error = get_with(&url, 100, budget(Duration::from_secs(1)), true)
            .await
            .unwrap_err();
        assert!(error.contains("rate limit"));
        assert!(!error.contains("--offline"));
    }

    #[test]
    fn a_relative_redirect_resolves_against_the_url_it_came_from() {
        let base: http::Uri = "https://github.com/owner/repo/releases/download/v1/pkg.tar.zst"
            .parse()
            .unwrap();
        // Absolute, and left exactly as sent.
        assert_eq!(
            resolve_redirect(&base, "https://objects.githubusercontent.com/x?t=1").unwrap(),
            "https://objects.githubusercontent.com/x?t=1"
        );
        // Root-relative and scheme-relative both used to fail the next scheme
        // check as "catalog downloads require HTTPS".
        assert_eq!(
            resolve_redirect(&base, "/moved?t=1").unwrap(),
            "https://github.com/moved?t=1"
        );
        assert_eq!(
            resolve_redirect(&base, "//objects.githubusercontent.com/x").unwrap(),
            "https://objects.githubusercontent.com/x"
        );
        // Relative to the base's directory, not its file.
        assert_eq!(
            resolve_redirect(&base, "other.tar.zst").unwrap(),
            "https://github.com/owner/repo/releases/download/v1/other.tar.zst"
        );
        assert!(resolve_redirect(&base, "   ").is_err());
        // Resolution never widens the allowlist: the host is still checked.
        let resolved = resolve_redirect(&base, "//evil.example/x").unwrap();
        let error = validate_http_uri(&resolved.parse().unwrap(), false).unwrap_err();
        assert!(error.contains("untrusted host"), "{error}");
    }

    #[tokio::test]
    async fn a_download_that_stops_delivering_ends_on_the_stall_budget() {
        // A generous total budget is what lets an artifact take its time; the
        // stall budget is what still ends a connection that has died. Both
        // halves of a request need it.
        let stalling = |stall| Budget {
            total: Duration::from_secs(30),
            stall,
        };

        // Nothing at all comes back.
        let url = server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            Duration::from_secs(5),
        );
        let error = get_with(&url, 100, stalling(Duration::from_millis(50)), true)
            .await
            .unwrap_err();
        assert!(error.contains("no response"), "{error}");

        // The head arrives and the body never does. The budget is wide enough
        // that a loaded machine still gets the head inside it, so the failure
        // this asserts can only come from the body.
        let url = silent_after_head(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n".to_vec());
        let error = get_with(&url, 100, stalling(Duration::from_millis(500)), true)
            .await
            .unwrap_err();
        assert!(error.contains("no data"), "{error}");
    }

    #[test]
    fn selecting_from_an_unvalidated_catalog_is_an_error_not_a_panic() {
        // `Catalog` and its fields are public, so a caller can hand `select` a
        // value that never went through `parse`.
        let mut catalog = catalog();
        catalog.plugins[0].versions[0].sofka = "not a requirement".into();
        let error = catalog.select("resource-summary@0.1.0").unwrap_err();
        assert!(
            error.contains("invalid compatibility requirement"),
            "{error}"
        );
    }
}
