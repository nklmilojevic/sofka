//! Cluster-independent `sofka plugin` command output and orchestration.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use semver::Version;
use serde::Serialize;

use crate::plugin_catalog::{
    Catalog, CatalogPlugin, CatalogSnapshot, CatalogVersion, VersionStatus,
};
use crate::plugin_install::{Activation, InstallLock, InstalledPackage};

#[derive(clap::Args, Debug, Clone)]
pub struct PluginArgs {
    /// Select a configured plugin catalog.
    #[arg(long, global = true)]
    pub catalog: Option<String>,
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum PluginCommand {
    /// Search the configured plugin catalogs.
    Search {
        /// Match plugin IDs, names, descriptions, and tags.
        #[arg(default_value = "")]
        query: String,
        /// Use previously cached catalog metadata.
        #[arg(long)]
        offline: bool,
        /// Print stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one plugin version and its runtime behavior.
    Describe {
        /// Plugin ID, optionally followed by @VERSION.
        plugin: String,
        /// Use previously cached catalog metadata.
        #[arg(long)]
        offline: bool,
        /// Print stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install or explicitly roll back one or more plugins.
    Install {
        /// Plugin IDs, optionally followed by @VERSION.
        #[arg(required = true)]
        plugins: Vec<String>,
        /// Use cached catalog metadata and artifacts.
        #[arg(long)]
        offline: bool,
    },
    /// Update managed plugins to the latest compatible versions.
    Update {
        /// Managed plugin IDs. With none, update every managed plugin.
        plugins: Vec<String>,
        /// Use cached catalog metadata and artifacts.
        #[arg(long)]
        offline: bool,
    },
    /// List installed managed and manual plugin packages without network access.
    List {
        /// Print stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove one or more catalog-managed plugins.
    Remove {
        /// Managed plugin IDs.
        #[arg(required = true)]
        plugins: Vec<String>,
    },
}

#[derive(Serialize)]
struct SearchRow<'a> {
    catalog: &'a str,
    id: &'a str,
    display_name: &'a str,
    description: &'a str,
    tags: &'a [String],
    latest_version: Option<&'a str>,
    compatible: bool,
    installed: bool,
    installed_version: Option<&'a str>,
    withdrawal_reason: Option<&'a str>,
}

#[derive(Serialize)]
struct ListRow<'a> {
    catalog_source: &'a str,
    id: &'a str,
    version: Option<&'a str>,
    path: &'a Path,
    managed: bool,
    modified: bool,
    withdrawal_reason: Option<&'a str>,
}

#[derive(Serialize)]
struct Description<'a> {
    catalog: &'a str,
    id: &'a str,
    display_name: &'a str,
    description: &'a str,
    tags: &'a [String],
    publisher: &'a str,
    repository: &'a str,
    version: &'a str,
    status: &'a str,
    withdrawal_reason: Option<&'a str>,
    license: &'a str,
    readme: &'a str,
    sofka: &'a str,
    platforms: Vec<&'a str>,
    requirements: &'a [crate::plugin_catalog::RuntimeRequirement],
    #[serde(flatten)]
    execution: &'a crate::plugin_catalog::CatalogExecution,
    confirmation: bool,
    installed: bool,
    installed_version: Option<&'a str>,
    installed_withdrawal_reason: Option<&'a str>,
}

/// Whether sofka will ask before running a release, matching the rule in
/// `App::run_plugin`: confirmation, danger, and traffic generation each
/// require it.
fn confirms(release: &CatalogVersion) -> bool {
    release
        .execution
        .entries()
        .iter()
        .any(|command| command.confirm || command.dangerous || command.network_load)
}

pub async fn run(args: &PluginArgs) -> Result<(), String> {
    let catalog = args.catalog.as_deref();
    if catalog.is_some()
        && matches!(
            args.command,
            PluginCommand::List { .. } | PluginCommand::Remove { .. }
        )
    {
        return Err("--catalog applies to search, describe, install, and update".into());
    }
    match &args.command {
        PluginCommand::Search {
            query,
            offline,
            json,
        } => search(query, *offline, *json, catalog).await,
        PluginCommand::Describe {
            plugin,
            offline,
            json,
        } => describe(plugin, *offline, *json, catalog).await,
        PluginCommand::Install { plugins, offline } => install(plugins, *offline, catalog).await,
        PluginCommand::Update { plugins, offline } => update(plugins, *offline, catalog).await,
        PluginCommand::List { json } => list(*json),
        PluginCommand::Remove { plugins } => remove(plugins),
    }
}

async fn search(
    query: &str,
    offline: bool,
    json: bool,
    catalog: Option<&str>,
) -> Result<(), String> {
    let snapshot = crate::plugin_catalog::load_selected(offline, catalog).await?;
    offline_notice(&snapshot);
    let installed = crate::plugin_install::installed_versions()?;
    let rows = search_rows(&snapshot.catalog, &installed, query);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        );
    } else {
        for row in rows {
            let compatibility = row.latest_version.map_or("incompatible", |_| "compatible");
            let installed = row
                .installed_version
                .map_or(String::new(), |version| format!(" installed={version}"));
            let withdrawn = row
                .withdrawal_reason
                .map_or(String::new(), |reason| format!(" withdrawn: {reason}"));
            println!(
                "{}\t{}\t{}\t{}{}{}\t{}",
                row.catalog,
                row.id,
                row.latest_version.unwrap_or("-"),
                compatibility,
                installed,
                withdrawn,
                row.description
            );
        }
    }
    Ok(())
}

fn search_rows<'a>(
    catalog: &'a Catalog,
    installed: &'a BTreeMap<(String, String), String>,
    query: &str,
) -> Vec<SearchRow<'a>> {
    catalog
        .matching(query)
        .into_iter()
        .map(|plugin| {
            let latest = plugin.latest_compatible();
            let installed_version = installed
                .get(&(plugin.id.clone(), plugin.source.identity().into()))
                .map(String::as_str);
            SearchRow {
                catalog: &plugin.source.name,
                id: &plugin.id,
                display_name: &plugin.display_name,
                description: &plugin.description,
                tags: &plugin.tags,
                latest_version: latest.map(|release| release.version.as_str()),
                compatible: latest.is_some(),
                installed: installed_version.is_some(),
                installed_version,
                withdrawal_reason: withdrawal(plugin, installed_version, latest.is_some()),
            }
        })
        .collect()
}

fn withdrawn_reason(release: &CatalogVersion) -> Option<&str> {
    match release.status {
        VersionStatus::Withdrawn => Some(
            release
                .withdrawal_reason
                .as_deref()
                .unwrap_or("no reason given"),
        ),
        VersionStatus::Active => None,
    }
}

/// The withdrawal of the exact version a user has installed, while the catalog
/// still lists it.
fn installed_withdrawal<'a>(plugin: &'a CatalogPlugin, installed: &str) -> Option<&'a str> {
    released(plugin, installed).and_then(withdrawn_reason)
}

/// The catalog entry for an exact installed version, when the index still
/// carries one. Absent is not the same as withdrawn: a withdrawal explains
/// itself, while a dropped entry leaves nothing to say and nothing to install.
fn released<'a>(plugin: &'a CatalogPlugin, version: &str) -> Option<&'a CatalogVersion> {
    plugin
        .versions
        .iter()
        .find(|release| release.version == version)
}

/// The withdrawal a searcher needs to see: the installed version's, or — when
/// nothing compatible is left to install — the newest version's.
fn withdrawal<'a>(
    plugin: &'a CatalogPlugin,
    installed: Option<&str>,
    compatible: bool,
) -> Option<&'a str> {
    let installed = installed.and_then(|version| installed_withdrawal(plugin, version));
    if installed.is_some() || compatible {
        return installed;
    }
    plugin
        .versions
        .iter()
        .filter_map(|release| Version::parse(&release.version).ok().map(|v| (v, release)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .and_then(|(_, release)| withdrawn_reason(release))
}

/// Everything `describe` reports for one release, in one place, so the text and
/// JSON views cannot drift apart.
fn description<'a>(
    plugin: &'a CatalogPlugin,
    release: &'a CatalogVersion,
    installed: Option<&'a str>,
) -> Description<'a> {
    // Only a *different* installed version needs its own line; the described
    // release's own withdrawal is already reported as `withdrawal_reason`.
    let installed_withdrawal_reason = installed
        .filter(|version| *version != release.version)
        .and_then(|version| installed_withdrawal(plugin, version));
    Description {
        catalog: &plugin.source.name,
        id: &plugin.id,
        display_name: &plugin.display_name,
        description: &plugin.description,
        tags: &plugin.tags,
        publisher: &plugin.publisher,
        repository: &plugin.repository,
        version: &release.version,
        status: match release.status {
            VersionStatus::Active => "active",
            VersionStatus::Withdrawn => "withdrawn",
        },
        withdrawal_reason: release.withdrawal_reason.as_deref(),
        license: &release.license,
        readme: &release.readme,
        sofka: &release.sofka,
        platforms: release
            .artifacts
            .iter()
            .map(|artifact| artifact.platform.as_str())
            .collect(),
        requirements: &release.requirements,
        execution: &release.execution,
        confirmation: confirms(release),
        installed: installed.is_some(),
        installed_version: installed,
        installed_withdrawal_reason,
    }
}

async fn describe(
    request: &str,
    offline: bool,
    json: bool,
    catalog: Option<&str>,
) -> Result<(), String> {
    let snapshot = crate::plugin_catalog::load_selected(offline, catalog).await?;
    offline_notice(&snapshot);
    let (plugin, release) = described_release(&snapshot, request)?;
    let installed = crate::plugin_install::installed_versions()?;
    let description = description(
        plugin,
        release,
        installed
            .get(&(plugin.id.clone(), plugin.source.identity().into()))
            .map(String::as_str),
    );
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&description).map_err(|e| e.to_string())?
        );
    } else {
        println!("{} ({})", description.display_name, description.id);
        println!("catalog: {}", description.catalog);
        println!("version: {} ({})", description.version, description.status);
        if let Some(reason) = description.withdrawal_reason {
            println!("withdrawal: {reason}");
        }
        if let Some(reason) = description.installed_withdrawal_reason {
            println!("installed version withdrawal: {reason}");
        }
        println!("description: {}", description.description);
        println!("publisher: {}", description.publisher);
        println!("repository: {}", description.repository);
        println!("license: {}", description.license);
        println!("requires sofka: {}", description.sofka);
        println!("platforms: {}", description.platforms.join(", "));
        match description.execution {
            crate::plugin_catalog::CatalogExecution::Legacy(execution) => {
                print_execution(execution)
            }
            crate::plugin_catalog::CatalogExecution::Commands { commands } => {
                for command in commands {
                    println!(
                        "{} ({}):",
                        command.name,
                        command.palette.as_deref().unwrap_or("key only")
                    );
                    println!("scopes: {}", command.scopes.join(", "));
                    print_execution(&command.execution);
                }
            }
        }
        println!(
            "installed: {}",
            description.installed_version.unwrap_or("no")
        );
        for requirement in description.requirements {
            println!(
                "requires: {} — {}",
                requirement_names(requirement),
                requirement.install
            );
        }
    }
    Ok(())
}

fn print_execution(command: &crate::plugin_catalog::Execution) {
    println!("command: {}", command.command);
    println!("target: {}", command.target);
    println!("output: {}", command.output);
    println!("mutating: {}", command.mutating);
    println!(
        "confirmation: {}",
        command.confirm || command.dangerous || command.network_load
    );
    println!("network load: {}", command.network_load);
}

async fn install(requests: &[String], offline: bool, catalog: Option<&str>) -> Result<(), String> {
    let snapshot = crate::plugin_catalog::load_selected(offline, catalog).await?;
    offline_notice(&snapshot);
    report_missing_requirements(&snapshot, requests)?;
    let config = crate::plugin_catalog::config_dir()?;
    let _lock = InstallLock::acquire(&config)?;
    let prepared = crate::plugin_install::prepare(&snapshot, requests, offline).await?;
    activate(prepared).report()
}

async fn update(requested: &[String], offline: bool, catalog: Option<&str>) -> Result<(), String> {
    // The lock finishes any interrupted operation, so hold it before reading
    // installations: a half-applied update has no state worth deciding from.
    let config = crate::plugin_catalog::config_dir()?;
    let _lock = InstallLock::acquire(&config)?;
    let installed = crate::plugin_install::installed()?;
    let mut sources = crate::plugin_catalog::configured_sources(catalog)?;
    let managed: HashMap<_, _> = installed
        .iter()
        .filter(|package| package.managed)
        .map(|package| (package.id.as_str(), package))
        .collect();
    let ids: Vec<String> = if requested.is_empty() {
        let mut ids: Vec<_> = managed
            .values()
            .filter(|package| {
                catalog.is_none()
                    || sources
                        .iter()
                        .any(|source| source.identity() == package.catalog_source)
            })
            .map(|package| package.id.clone())
            .collect();
        ids.sort();
        ids
    } else {
        requested.to_vec()
    };
    if ids.is_empty() {
        println!("No managed plugins are installed.");
        return Ok(());
    }
    // Triaged before the catalog is fetched: when nothing is updatable there is
    // no reason to reach the network at all.
    let (mut ready, refused) = triage(&managed, ids);
    let mut unresolved = Vec::new();
    for (id, reason) in refused {
        eprintln!("warning: {reason}");
        unresolved.push(id);
    }
    ready.retain(|id| {
        let package = managed[id.as_str()];
        if sources
            .iter()
            .any(|source| source.identity() == package.catalog_source)
        {
            true
        } else {
            eprintln!(
                "warning: {id}: its installed catalog is not enabled or does not match --catalog"
            );
            unresolved.push(id.clone());
            false
        }
    });
    if ready.is_empty() {
        return Err(format!("could not update: {}", unresolved.join(", ")));
    }
    sources.retain(|source| {
        ready
            .iter()
            .any(|id| managed[id.as_str()].catalog_source == source.identity())
    });
    let mut snapshot = crate::plugin_catalog::load_sources(
        sources,
        &crate::plugin_catalog::cache_dir(),
        offline,
        None,
    )
    .await?;
    snapshot.catalog.plugins.retain(|plugin| {
        managed
            .get(plugin.id.as_str())
            .is_some_and(|package| package.catalog_source == plugin.source.identity())
    });
    offline_notice(&snapshot);
    let mut updates = Vec::new();
    for id in ready {
        let Some(current) = managed
            .get(id.as_str())
            .and_then(|package| package.version.as_deref())
            .and_then(|version| Version::parse(version).ok())
        else {
            eprintln!("warning: {id} has an invalid installed version");
            unresolved.push(id);
            continue;
        };
        match update_plan(&snapshot.catalog, &id, &current) {
            UpdatePlan::Newer => updates.push(id),
            UpdatePlan::Current => println!("{id} is current at {current}"),
            UpdatePlan::Unlisted { newest } => {
                eprintln!(
                    "warning: {id} is installed at {current}, which the catalog no longer \
                     lists; the newest version sofka can install is {newest}"
                );
                unresolved.push(id);
            }
            // Nothing newer is not the same as nothing wrong: the version in
            // use may since have been withdrawn, and that is the one thing an
            // update run must not stay quiet about.
            UpdatePlan::Withdrawn { reason, newest } => eprintln!(
                "warning: {id} is installed at {current}, which was withdrawn: {reason}; \
                 the newest version sofka can install is {newest}"
            ),
            UpdatePlan::Unavailable { error, withdrawn } => {
                match withdrawn {
                    Some(reason) => eprintln!(
                        "warning: {id} is installed at {current}, which was withdrawn: \
                         {reason}; sofka has no version to replace it with: {error}"
                    ),
                    None => eprintln!("warning: {id}: {error}"),
                }
                unresolved.push(id);
            }
        }
    }
    if !updates.is_empty() {
        report_missing_requirements(&snapshot, &updates)?;
        let prepared = crate::plugin_install::prepare(&snapshot, &updates, offline).await?;
        activate(prepared).report()?;
    }
    if unresolved.is_empty() {
        Ok(())
    } else {
        Err(format!("could not update: {}", unresolved.join(", ")))
    }
}

/// Split the requested IDs into the installations an update run can work on and
/// the ones it can only report. One unusable installation is that
/// installation's problem: it used to leave every other plugin un-updated.
fn triage(
    managed: &HashMap<&str, &InstalledPackage>,
    ids: Vec<String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut ready = Vec::new();
    let mut refused = Vec::new();
    for id in ids {
        match managed.get(id.as_str()) {
            None => {
                let reason = format!("{id} is not a managed installation");
                refused.push((id, reason));
            }
            Some(package) if package.modified => {
                let reason = format!(
                    "{id} at {} has local modifications; update refused",
                    package.path.display()
                );
                refused.push((id, reason));
            }
            Some(_) => ready.push(id),
        }
    }
    (ready, refused)
}

/// What an update run should do about one installed plugin. A plugin the
/// catalog can no longer serve is that plugin's problem: reporting it as a
/// plan rather than an error is what keeps it from cancelling the whole run.
#[derive(Debug, PartialEq, Eq)]
enum UpdatePlan {
    Newer,
    Current,
    Unlisted {
        newest: String,
    },
    Withdrawn {
        reason: String,
        newest: String,
    },
    Unavailable {
        error: String,
        withdrawn: Option<String>,
    },
}

fn update_plan(catalog: &Catalog, id: &str, current: &Version) -> UpdatePlan {
    let installed = current.to_string();
    let unavailable = |error: String| UpdatePlan::Unavailable {
        withdrawn: catalog
            .find(id)
            .ok()
            .and_then(|plugin| installed_withdrawal(plugin, &installed))
            .map(str::to_owned),
        error,
    };
    let selected = match catalog.select(id) {
        Ok(selected) => selected,
        Err(error) => return unavailable(error),
    };
    let available = match Version::parse(&selected.version.version) {
        Ok(available) => available,
        Err(e) => return unavailable(format!("catalog version {e}")),
    };
    if available > *current {
        return UpdatePlan::Newer;
    }
    let Some(release) = released(selected.plugin, &installed) else {
        // Newer than anything installable and absent from the index: the entry
        // was dropped rather than withdrawn, so there is no reason to report
        // and nothing to update to. Calling that current said the opposite.
        return UpdatePlan::Unlisted {
            newest: available.to_string(),
        };
    };
    match withdrawn_reason(release) {
        Some(reason) => UpdatePlan::Withdrawn {
            reason: reason.to_owned(),
            newest: available.to_string(),
        },
        None => UpdatePlan::Current,
    }
}

/// What an activation batch left behind: whether anything reached the plugins
/// directory, and which packages did not. A package that fails after an earlier
/// one succeeded still leaves a running session holding stale packages, so the
/// two answers are kept apart.
struct Activated {
    any: bool,
    failed: Vec<String>,
}

impl Activated {
    fn report(self) -> Result<(), String> {
        // Ordered before the failure: what already went in has to be reloaded
        // whether or not a later package in the same batch made it.
        if self.any {
            println!("Run :reload in an existing sofka session to load the changes.");
        }
        if self.failed.is_empty() {
            Ok(())
        } else {
            Err(format!("failed to activate: {}", self.failed.join(", ")))
        }
    }
}

fn activate(prepared: Vec<crate::plugin_install::PreparedPackage>) -> Activated {
    let mut failed = Vec::new();
    let mut any = false;
    for package in prepared {
        let id = package.id.clone();
        let version = package.version.clone();
        for path in &package.conflicts {
            eprintln!(
                "warning: {id} shares a plugin name or palette command with the package at {}; \
                 sofka loads the first of the two and ignores the other",
                path.display()
            );
        }
        match package.activate() {
            Ok(action) => {
                any |= !matches!(action, Activation::Unchanged);
                println!(
                    "{} {id}@{version}",
                    match action {
                        Activation::Installed => "installed",
                        Activation::Updated => "updated",
                        Activation::RolledBack => "rolled back",
                        Activation::Unchanged => "already installed",
                    }
                );
            }
            Err(error) => {
                eprintln!("{id}: {error}");
                failed.push(id);
            }
        }
    }
    Activated { any, failed }
}

fn list(json: bool) -> Result<(), String> {
    let packages = crate::plugin_install::installed()?;
    // Withdrawals come from whatever was last fetched; list never reaches the
    // network, and having no cache at all is not an error.
    let cached = crate::plugin_catalog::cached();
    let rows = list_rows(&packages, cached.as_ref().map(|snapshot| &snapshot.catalog));
    if rows.iter().any(|row| row.withdrawal_reason.is_some())
        && let Some(snapshot) = &cached
    {
        offline_notice(snapshot);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
        );
    } else {
        for row in rows {
            let kind = if row.managed { "managed" } else { "manual" };
            let modified = if row.modified { " modified" } else { "" };
            let withdrawn = row
                .withdrawal_reason
                .map_or(String::new(), |reason| format!(" withdrawn: {reason}"));
            println!(
                "{}\t{}\t{}\t{kind}{modified}{withdrawn}\t{}",
                row.id,
                row.catalog_source,
                row.version.unwrap_or("-"),
                row.path.display()
            );
        }
    }
    Ok(())
}

fn list_rows<'a>(
    packages: &'a [InstalledPackage],
    catalog: Option<&'a Catalog>,
) -> Vec<ListRow<'a>> {
    packages
        .iter()
        .map(|package| ListRow {
            catalog_source: &package.catalog_source,
            id: &package.id,
            version: package.version.as_deref(),
            path: &package.path,
            managed: package.managed,
            modified: package.modified,
            withdrawal_reason: catalog
                .filter(|_| package.managed)
                .zip(package.version.as_deref())
                .and_then(|(catalog, version)| {
                    installed_withdrawal(
                        catalog.plugins.iter().find(|plugin| {
                            plugin.id == package.id
                                && plugin.source.identity() == package.catalog_source
                        })?,
                        version,
                    )
                }),
        })
        .collect()
}

fn remove(ids: &[String]) -> Result<(), String> {
    let config = crate::plugin_catalog::config_dir()?;
    let _lock = InstallLock::acquire(&config)?;
    // Whatever came out before a failure is still out, and saying so is the
    // difference between "re-run it" and "some of that already happened".
    let (removed, failure) = match crate::plugin_install::remove(ids) {
        Ok(removed) => (removed, None),
        Err(refused) => (refused.removed, Some(refused.error)),
    };
    let any = !removed.is_empty();
    for (id, path) in removed {
        println!("removed {id} from {}", path.display());
    }
    if any {
        println!("Run :reload in an existing sofka session to load the changes.");
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

fn report_missing_requirements(
    snapshot: &CatalogSnapshot,
    requests: &[String],
) -> Result<(), String> {
    for warning in missing_requirements(snapshot, requests)? {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// The requirement's command and every alternative that satisfies it. Describe
/// and the missing-tool warning have to agree: a user who already has the
/// alternative installed needs both to say so.
fn requirement_names(requirement: &crate::plugin_catalog::RuntimeRequirement) -> String {
    std::iter::once(requirement.name.as_str())
        .chain(requirement.alternatives.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// External tools a request needs but this machine does not have. Reported, not
/// enforced: a missing tool never stops a valid package from being installed.
fn missing_requirements(
    snapshot: &CatalogSnapshot,
    requests: &[String],
) -> Result<Vec<String>, String> {
    let mut reported = HashSet::new();
    let mut warnings = Vec::new();
    for request in requests {
        let selection = snapshot.catalog.select(request)?;
        for requirement in &selection.version.requirements {
            if reported.insert((selection.plugin.id.clone(), requirement.name.clone()))
                && std::iter::once(&requirement.name)
                    .chain(&requirement.alternatives)
                    .all(|name| crate::plugins::executable(name).is_none())
            {
                warnings.push(format!(
                    "{} requires {} — {}",
                    selection.plugin.id,
                    requirement_names(requirement),
                    requirement.install
                ));
            }
        }
    }
    Ok(warnings)
}

fn described_release<'a>(
    snapshot: &'a CatalogSnapshot,
    request: &str,
) -> Result<(&'a CatalogPlugin, &'a CatalogVersion), String> {
    let (id, exact) = crate::plugin_catalog::parse_request(request)?;
    let plugin = snapshot.catalog.find(id)?;
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
            .latest_compatible()
            .or_else(|| {
                plugin
                    .versions
                    .iter()
                    .filter_map(|release| {
                        Version::parse(&release.version)
                            .ok()
                            .map(|version| (version, release))
                    })
                    .max_by(|(a, _), (b, _)| a.cmp(b))
                    .map(|(_, release)| release)
            })
            .ok_or_else(|| format!("plugin {id} has no published versions"))?
    };
    Ok((plugin, release))
}

fn offline_notice(snapshot: &CatalogSnapshot) {
    if snapshot.offline {
        eprintln!(
            "using cached catalog from {} ago; withdrawal information may be stale",
            crate::plugin_catalog::age(snapshot.fetched_at)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(versions: serde_json::Value) -> Catalog {
        let index = serde_json::json!({
            "schema_version": 1,
            "generated_at": "2026-09-11T00:00:00Z",
            "plugins": [{
                "id": "resource-summary",
                "display_name": "Resource summary",
                "description": "Summarize the selected resource.",
                "tags": ["example"],
                "publisher": "sofka maintainers",
                "repository": "https://github.com/nklmilojevic/sofka-plugins",
                "versions": versions,
            }],
        });
        Catalog::parse(&serde_json::to_vec(&index).unwrap()).unwrap()
    }

    fn release(version: &str, status: &str, reason: Option<&str>) -> serde_json::Value {
        let mut release = serde_json::json!({
            "version": version,
            "sofka": ">=0.0.1",
            "source_commit": "1".repeat(40),
            "license": "MIT",
            "readme": "https://example.invalid/readme",
            "requirements": [],
            "command": "./adapter",
            "target": "selection",
            "output": "report",
            "mutating": false,
            "confirm": false,
            "dangerous": false,
            "network_load": false,
            "status": status,
            "artifacts": [{
                "platform": "any",
                "url": format!(
                    "{}resource-summary-v{version}/resource-summary.tar.zst",
                    crate::plugin_catalog::RELEASE_ROOT
                ),
                "blake3": "2".repeat(64),
                "size": 1,
            }],
        });
        if let Some(reason) = reason {
            release["withdrawal_reason"] = reason.into();
        }
        release
    }

    fn package(version: &str) -> Vec<InstalledPackage> {
        vec![InstalledPackage {
            catalog_source: "official".into(),
            id: "resource-summary".into(),
            version: Some(version.into()),
            path: std::path::PathBuf::from("/config/plugins/resource-summary"),
            managed: true,
            modified: false,
        }]
    }

    fn snapshot(versions: serde_json::Value) -> CatalogSnapshot {
        CatalogSnapshot {
            catalog: catalog(versions),
            commit: "0".repeat(40),
            fetched_at: 0,
            offline: true,
        }
    }

    /// The documented field set of one JSON row, sorted: object key order is
    /// not part of the contract, the field set is.
    fn keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<_> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn description_lists_each_command_with_its_own_safety_flags() {
        let catalog = catalog(serde_json::json!([{
            "version": "1.0.0", "sofka": ">=0.0.1", "source_commit": "0".repeat(40),
            "license": "MIT", "readme": "https://example.invalid/readme", "requirements": [],
            "command": "adapter", "target": "selection", "output": "report", "mutating": false,
            "status": "active", "artifacts": [{"platform": "any", "url": format!("{}x/x.tar.zst", crate::plugin_catalog::RELEASE_ROOT), "blake3": "0".repeat(64), "size": 1}]
        }]));
        let plugin = &catalog.plugins[0];
        let mut release = plugin.versions[0].clone();
        let command = |name: &str, mutating: bool| crate::plugin_catalog::CatalogCommand {
            name: name.into(),
            palette: Some(format!("cert-manager-{name}")),
            key: None,
            args: vec![name.into()],
            scopes: vec!["certificates".into()],
            execution: crate::plugin_catalog::Execution {
                command: "./adapter".into(),
                target: "selection".into(),
                output: "report".into(),
                mutating,
                confirm: mutating,
                dangerous: false,
                network_load: false,
            },
        };
        release.execution = crate::plugin_catalog::CatalogExecution::Commands {
            commands: vec![command("status", false), command("renew", true)],
        };
        let described = serde_json::to_value(description(plugin, &release, None)).unwrap();
        assert_eq!(described["commands"][0]["mutating"], false);
        assert_eq!(described["commands"][1]["mutating"], true);
        assert_eq!(described["commands"][1]["confirm"], true);
        assert_eq!(described["confirmation"], true);
        assert!(described.get("mutating").is_none());
    }

    #[test]
    fn describe_selects_exact_versions_and_errors_on_anything_unknown() {
        let snapshot = snapshot(serde_json::json!([
            release("1.0.0", "active", None),
            release("2.0.0", "active", None),
        ]));
        let (plugin, release) = described_release(&snapshot, "resource-summary").unwrap();
        assert_eq!(plugin.id, "resource-summary");
        assert_eq!(release.version, "2.0.0");
        assert_eq!(
            described_release(&snapshot, "resource-summary@1.0.0")
                .unwrap()
                .1
                .version,
            "1.0.0"
        );
        assert!(
            described_release(&snapshot, "absent")
                .unwrap_err()
                .contains("unknown plugin")
        );
        assert!(
            described_release(&snapshot, "resource-summary@9.9.9")
                .unwrap_err()
                .contains("has no version 9.9.9")
        );
        assert!(described_release(&snapshot, "Bad Id").is_err());
    }

    #[test]
    fn describe_still_reports_a_plugin_whose_only_release_was_withdrawn() {
        let snapshot = snapshot(serde_json::json!([
            release("1.0.0", "withdrawn", Some("leaks secrets")),
            release("2.0.0", "withdrawn", Some("same defect")),
        ]));
        let (plugin, release) = described_release(&snapshot, "resource-summary").unwrap();
        assert_eq!(release.version, "2.0.0");
        let described = description(plugin, release, None);
        assert_eq!(described.status, "withdrawn");
        assert_eq!(described.withdrawal_reason, Some("same defect"));
        assert!(!described.installed);
    }

    #[test]
    fn a_withdrawn_installed_version_is_never_called_current() {
        let catalog = catalog(serde_json::json!([
            release("0.1.0", "active", None),
            release("0.2.0", "withdrawn", Some("corrupts reports")),
        ]));
        let plugin = catalog.find("resource-summary").unwrap();
        // Nothing newer is installable, so update has nothing to do — but the
        // version in use was withdrawn, which is the whole point of saying so.
        assert_eq!(
            installed_withdrawal(plugin, "0.2.0"),
            Some("corrupts reports")
        );
        assert_eq!(installed_withdrawal(plugin, "0.1.0"), None);
        // The newest installable version really is the older one.
        assert_eq!(plugin.latest_compatible().unwrap().version, "0.1.0");
    }

    #[test]
    fn describe_says_a_plugin_will_prompt_whenever_sofka_would() {
        let variants = [
            (false, false, false, false),
            (true, false, false, true),
            (false, true, false, true),
            // Traffic generation prompts too; describe used to say it did not.
            (false, false, true, true),
        ];
        for (confirm, dangerous, network_load, expected) in variants {
            let mut release = release("1.0.0", "active", None);
            release["confirm"] = confirm.into();
            release["dangerous"] = dangerous.into();
            release["network_load"] = network_load.into();
            let snapshot = snapshot(serde_json::json!([release]));
            let (plugin, release) = described_release(&snapshot, "resource-summary").unwrap();
            assert_eq!(
                description(plugin, release, None).confirmation,
                expected,
                "confirm={confirm} dangerous={dangerous} network_load={network_load}"
            );
        }
    }

    #[test]
    fn describe_reports_the_release_and_installed_state_as_documented_fields() {
        let snapshot = snapshot(serde_json::json!([release("1.0.0", "active", None)]));
        let (plugin, release) = described_release(&snapshot, "resource-summary").unwrap();
        let packages = package("1.0.0");
        let described = description(plugin, release, packages[0].version.as_deref());
        assert!(described.installed);
        assert_eq!(described.installed_version, Some("1.0.0"));
        assert_eq!(described.installed_withdrawal_reason, None);
        assert_eq!(described.platforms, ["any"]);
        assert_eq!(described.sofka, ">=0.0.1");
        assert_eq!(described.execution.entries()[0].target, "selection");
        assert_eq!(described.execution.entries()[0].output, "report");

        let value = serde_json::to_value(&described).unwrap();
        assert_eq!(
            keys(&value),
            [
                "catalog",
                "command",
                "confirm",
                "confirmation",
                "dangerous",
                "description",
                "display_name",
                "id",
                "installed",
                "installed_version",
                "installed_withdrawal_reason",
                "license",
                "mutating",
                "network_load",
                "output",
                "platforms",
                "publisher",
                "readme",
                "repository",
                "requirements",
                "sofka",
                "status",
                "tags",
                "target",
                "version",
                "withdrawal_reason",
            ]
        );
    }

    #[test]
    fn describe_reports_when_the_installed_version_was_withdrawn() {
        let snapshot = snapshot(serde_json::json!([
            release("0.1.0", "active", None),
            release("0.2.0", "withdrawn", Some("corrupts reports")),
        ]));
        let (plugin, selected) = described_release(&snapshot, "resource-summary").unwrap();
        assert_eq!(selected.version, "0.1.0");

        let described = description(plugin, selected, Some("0.2.0"));

        assert_eq!(described.status, "active");
        assert_eq!(described.withdrawal_reason, None);
        assert_eq!(
            described.installed_withdrawal_reason,
            Some("corrupts reports")
        );

        // Describing the withdrawn release itself must not say it twice.
        let withdrawn = plugin
            .versions
            .iter()
            .find(|release| release.version == "0.2.0")
            .unwrap();
        let described = description(plugin, withdrawn, Some("0.2.0"));
        assert_eq!(described.withdrawal_reason, Some("corrupts reports"));
        assert_eq!(described.installed_withdrawal_reason, None);
    }

    #[test]
    fn search_and_list_json_carry_their_documented_fields() {
        let catalog = catalog(serde_json::json!([release("1.0.0", "active", None)]));
        let installed = installed_rows(&package("1.0.0"));
        let rows = serde_json::to_value(search_rows(&catalog, &installed, "")).unwrap();
        assert_eq!(
            keys(&rows[0]),
            [
                "catalog",
                "compatible",
                "description",
                "display_name",
                "id",
                "installed",
                "installed_version",
                "latest_version",
                "tags",
                "withdrawal_reason",
            ]
        );
        assert_eq!(rows[0]["latest_version"], "1.0.0");
        assert_eq!(rows[0]["compatible"], true);
        assert_eq!(rows[0]["installed"], true);
        assert_eq!(rows[0]["withdrawal_reason"], serde_json::Value::Null);

        let packages = package("1.0.0");
        let rows = serde_json::to_value(list_rows(&packages, Some(&catalog))).unwrap();
        assert_eq!(
            keys(&rows[0]),
            [
                "catalog_source",
                "id",
                "managed",
                "modified",
                "path",
                "version",
                "withdrawal_reason"
            ]
        );
        assert_eq!(rows[0]["managed"], true);
        assert_eq!(rows[0]["modified"], false);
    }

    #[test]
    fn a_query_that_matches_nothing_is_an_empty_result_not_an_error() {
        let catalog = catalog(serde_json::json!([release("1.0.0", "active", None)]));
        let none = BTreeMap::new();
        assert!(search_rows(&catalog, &none, "absent").is_empty());
        assert_eq!(search_rows(&catalog, &none, "RESOURCE").len(), 1);
        assert!(list_rows(&[], Some(&catalog)).is_empty());
    }

    #[test]
    fn a_manual_package_is_never_labelled_with_catalog_withdrawal() {
        let catalog = catalog(serde_json::json!([release(
            "1.0.0",
            "withdrawn",
            Some("leaks secrets")
        )]));
        let mut packages = package("1.0.0");
        packages[0].managed = false;
        assert_eq!(
            list_rows(&packages, Some(&catalog))[0].withdrawal_reason,
            None
        );

        // A version the catalog never listed carries no withdrawal either.
        let unknown = package("9.9.9");
        assert_eq!(
            list_rows(&unknown, Some(&catalog))[0].withdrawal_reason,
            None
        );
    }

    #[test]
    fn missing_external_tools_are_reported_once_each_and_never_block_a_request() {
        let mut versions = serde_json::json!([release("1.0.0", "active", None)]);
        versions[0]["requirements"] = serde_json::json!([
            {
                "name": "sofka-absent-tool-40412",
                "alternatives": ["sofka-absent-tool-40413"],
                "install": "brew install absent"
            },
            {
                "name": "sofka-absent-primary-40414",
                "alternatives": ["sh"],
                "install": "already present under an alternative name"
            },
            {"name": "sh", "install": "already present"},
        ]);
        let snapshot = snapshot(versions);
        let requests = [
            "resource-summary".to_string(),
            "resource-summary".to_string(),
        ];
        let warnings = missing_requirements(&snapshot, &requests).unwrap();
        assert_eq!(
            warnings,
            [
                "resource-summary requires sofka-absent-tool-40412 or sofka-absent-tool-40413 — brew install absent"
            ]
        );
        // Reporting a requirement is not a failure, and an unknown ID still is.
        report_missing_requirements(&snapshot, &requests).unwrap();
        assert!(missing_requirements(&snapshot, &["absent".to_string()]).is_err());
    }

    #[test]
    fn a_batch_keeps_earlier_successes_and_preserves_the_package_that_failed() {
        let config =
            std::env::temp_dir().join(format!("sofka-plugin-cli-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        let plugins = config.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();

        let good = plugins.join("good");
        let stage = config.join(".plugin-stage-good");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join(".sofka-install-stage"), "stage").unwrap();
        std::fs::write(stage.join("plugin.toml"), "new").unwrap();

        let bad = plugins.join("bad");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("plugin.toml"), "previous").unwrap();

        let error = activate(vec![
            crate::plugin_install::PreparedPackage::staged(
                "good",
                "1.0.0",
                None,
                stage,
                good.clone(),
            ),
            crate::plugin_install::PreparedPackage::staged(
                "bad",
                "2.0.0",
                Some("1.0.0"),
                config.join(".plugin-stage-bad-absent"),
                bad.clone(),
            ),
        ]);

        // The success is reported apart from the failure, so the reload notice
        // still reaches a session holding the package that did land.
        assert!(error.any);
        let error = error.report().unwrap_err();
        assert_eq!(error, "failed to activate: bad");
        assert_eq!(
            std::fs::read_to_string(good.join("plugin.toml")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(bad.join("plugin.toml")).unwrap(),
            "previous"
        );
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn search_reports_why_a_withdrawn_plugin_cannot_be_installed() {
        let catalog = catalog(serde_json::json!([release(
            "1.0.0",
            "withdrawn",
            Some("unsafe")
        )]));
        let none = BTreeMap::new();
        let rows = search_rows(&catalog, &none, "");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].compatible);
        assert_eq!(rows[0].withdrawal_reason, Some("unsafe"));
    }

    #[test]
    fn search_reports_a_withdrawn_installed_version_beside_its_replacement() {
        let catalog = catalog(serde_json::json!([
            release("1.0.0", "withdrawn", Some("leaks secrets")),
            release("2.0.0", "active", None),
        ]));
        let installed = installed_rows(&package("1.0.0"));
        let rows = search_rows(&catalog, &installed, "");
        assert_eq!(rows[0].latest_version, Some("2.0.0"));
        assert!(rows[0].compatible);
        assert_eq!(rows[0].withdrawal_reason, Some("leaks secrets"));

        let installed = installed_rows(&package("2.0.0"));
        let rows = search_rows(&catalog, &installed, "");
        assert_eq!(rows[0].withdrawal_reason, None);
    }

    #[test]
    fn list_reports_a_withdrawn_installed_version_and_survives_an_empty_cache() {
        let catalog = catalog(serde_json::json!([
            release("1.0.0", "withdrawn", Some("leaks secrets")),
            release("2.0.0", "active", None),
        ]));
        let packages = package("1.0.0");
        let rows = list_rows(&packages, Some(&catalog));
        assert_eq!(rows[0].withdrawal_reason, Some("leaks secrets"));

        let current = package("2.0.0");
        let rows = list_rows(&current, Some(&catalog));
        assert_eq!(rows[0].withdrawal_reason, None);

        let rows = list_rows(&packages, None);
        assert_eq!(rows[0].version, Some("1.0.0"));
        assert_eq!(rows[0].withdrawal_reason, None);
    }

    /// What search and describe actually consume: installed versions by ID.
    fn installed_rows(packages: &[InstalledPackage]) -> BTreeMap<(String, String), String> {
        packages
            .iter()
            .filter_map(|package| {
                Some((
                    (package.id.clone(), package.catalog_source.clone()),
                    package.version.clone()?,
                ))
            })
            .collect()
    }

    #[test]
    fn update_plans_a_newer_release_a_current_one_and_a_recalled_one() {
        let version = |v: &str| Version::parse(v).unwrap();
        let active = catalog(serde_json::json!([
            release("1.0.0", "active", None),
            release("2.0.0", "active", None),
        ]));
        assert_eq!(
            update_plan(&active, "resource-summary", &version("1.0.0")),
            UpdatePlan::Newer
        );
        assert_eq!(
            update_plan(&active, "resource-summary", &version("2.0.0")),
            UpdatePlan::Current
        );

        let recalled = catalog(serde_json::json!([
            release("0.1.0", "active", None),
            release("0.2.0", "withdrawn", Some("corrupts reports")),
        ]));
        assert_eq!(
            update_plan(&recalled, "resource-summary", &version("0.2.0")),
            UpdatePlan::Withdrawn {
                reason: "corrupts reports".into(),
                newest: "0.1.0".into(),
            }
        );
    }

    #[test]
    fn one_plugin_the_catalog_cannot_serve_never_cancels_an_update_run() {
        let version = |v: &str| Version::parse(v).unwrap();
        // Every release recalled, so there is nothing to update to. This used
        // to abort the run and skip every other installed plugin with it.
        let catalog = catalog(serde_json::json!([release(
            "1.0.0",
            "withdrawn",
            Some("leaks secrets")
        )]));
        assert_eq!(
            update_plan(&catalog, "resource-summary", &version("1.0.0")),
            UpdatePlan::Unavailable {
                error: "plugin resource-summary has no compatible stable version".into(),
                // The reason the installed version is unusable is the part the
                // run exists to say, and it survives the failure to select.
                withdrawn: Some("leaks secrets".into()),
            }
        );
        // A plugin the catalog dropped is reported the same way, not fatally.
        assert_eq!(
            update_plan(&catalog, "departed", &version("1.0.0")),
            UpdatePlan::Unavailable {
                error: "unknown plugin \"departed\"".into(),
                withdrawn: None,
            }
        );
    }

    #[test]
    fn describe_names_every_command_that_satisfies_a_requirement() {
        let mut versions = serde_json::json!([release("1.0.0", "active", None)]);
        versions[0]["requirements"] = serde_json::json!([
            {
                "name": "popeye",
                "alternatives": ["kubectl-popeye"],
                "install": "brew install derailed/popeye/popeye"
            },
            {"name": "jq", "install": "brew install jq"},
        ]);
        let snapshot = snapshot(versions);
        let (plugin, release) = described_release(&snapshot, "resource-summary").unwrap();
        let lines: Vec<_> = description(plugin, release, None)
            .requirements
            .iter()
            .map(|requirement| {
                format!(
                    "requires: {} — {}",
                    requirement_names(requirement),
                    requirement.install
                )
            })
            .collect();
        // The text view printed the bare name, so a user who already had the
        // alternative installed could not tell that it counted.
        assert_eq!(
            lines,
            [
                "requires: popeye or kubectl-popeye — brew install derailed/popeye/popeye",
                "requires: jq — brew install jq",
            ]
        );
    }

    #[test]
    fn an_already_installed_package_reports_no_changes_to_reload() {
        let config =
            std::env::temp_dir().join(format!("sofka-plugin-cli-unchanged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(config.join("plugins")).unwrap();

        // A prepared package with no stage is the "already installed, intact"
        // outcome. Nothing reached the plugins directory, so nothing needs a
        // reload — saying otherwise sent the user to a session with no change.
        let outcome = activate(vec![crate::plugin_install::PreparedPackage::unchanged(
            "sample",
            "1.0.0",
            config.join("plugins").join("sample"),
        )]);
        assert!(!outcome.any);
        assert!(outcome.report().is_ok());
        let _ = std::fs::remove_dir_all(config);
    }

    #[test]
    fn a_modified_package_is_refused_without_holding_back_the_others() {
        let installed = [
            InstalledPackage {
                catalog_source: "official".into(),
                id: "clean".into(),
                version: Some("1.0.0".into()),
                path: "/config/plugins/clean".into(),
                managed: true,
                modified: false,
            },
            InstalledPackage {
                catalog_source: "official".into(),
                id: "edited".into(),
                version: Some("1.0.0".into()),
                path: "/config/plugins/edited".into(),
                managed: true,
                modified: true,
            },
        ];
        let managed: HashMap<_, _> = installed
            .iter()
            .map(|package| (package.id.as_str(), package))
            .collect();
        let ids = ["clean", "edited", "absent"].map(str::to_owned).to_vec();

        let (ready, refused) = triage(&managed, ids);
        // The edited package used to abort the run before the catalog was even
        // fetched, so "clean" never got its update.
        assert_eq!(ready, ["clean"]);
        assert_eq!(
            refused,
            [
                (
                    "edited".to_string(),
                    "edited at /config/plugins/edited has local modifications; update refused"
                        .to_string()
                ),
                (
                    "absent".to_string(),
                    "absent is not a managed installation".to_string()
                ),
            ]
        );
    }

    #[test]
    fn an_installed_version_the_catalog_dropped_is_never_called_current() {
        let version = |v: &str| Version::parse(v).unwrap();
        let catalog = catalog(serde_json::json!([
            release("1.0.0", "active", None),
            release("2.0.0", "withdrawn", Some("corrupts reports")),
        ]));

        // 3.0.0 was published once and the entry has since been dropped, not
        // withdrawn. Nothing newer is installable and there is no reason to
        // report, which used to read as "current at 3.0.0" and exit clean.
        assert_eq!(
            update_plan(&catalog, "resource-summary", &version("3.0.0")),
            UpdatePlan::Unlisted {
                newest: "1.0.0".into()
            }
        );
        // A version the catalog still lists keeps its own answer.
        assert_eq!(
            update_plan(&catalog, "resource-summary", &version("2.0.0")),
            UpdatePlan::Withdrawn {
                reason: "corrupts reports".into(),
                newest: "1.0.0".into(),
            }
        );
        assert_eq!(
            update_plan(&catalog, "resource-summary", &version("1.0.0")),
            UpdatePlan::Current
        );
    }
}
