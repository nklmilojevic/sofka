//! Versioned external plugin packages and their bounded process/report protocol.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use unicode_width::UnicodeWidthStr;

use crate::config::Plugin;

pub mod activity;

pub const MAX_BYTES: usize = 1 << 20;
pub const MAX_LINES: usize = 5_000;

#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    /// Inline configuration ignores unknown fields. Package validation rejects them.
    #[serde(flatten)]
    unknown_fields: BTreeMap<String, toml::Value>,
    #[serde(rename = "type")]
    pub kind: String,
    pub default: Option<String>,
    pub min: Option<u64>,
    pub max: Option<u64>,
    #[serde(default)]
    pub choices: Vec<String>,
}

impl Input {
    pub fn validate(&self, value: &str) -> Result<(), String> {
        let number = match self.kind.as_str() {
            "string" => None,
            "boolean" if matches!(value, "true" | "false") => None,
            "integer" => Some(
                value
                    .parse::<u64>()
                    .map_err(|_| "expected unsigned integer")?,
            ),
            "duration" => Some(crate::providers::parse_lookback(value)? as u64),
            _ => return Err(format!("invalid {} value {value:?}", self.kind)),
        };
        if let Some(n) = number
            && (self.min.is_some_and(|m| n < m) || self.max.is_some_and(|m| n > m))
        {
            return Err(format!(
                "value outside range {:?}..{:?}",
                self.min, self.max
            ));
        }
        if !self.choices.is_empty() && !self.choices.iter().any(|c| c == value) {
            return Err(format!("expected one of {}", self.choices.join(", ")));
        }
        Ok(())
    }
}

pub fn inputs(plugin: &Plugin, arguments: &str) -> Result<BTreeMap<String, String>, String> {
    let mut supplied = BTreeMap::new();
    for word in arguments.split_whitespace() {
        let (name, value) = word
            .split_once('=')
            .ok_or("use name=value plugin arguments")?;
        if !plugin.inputs.contains_key(name) {
            return Err(format!("unknown input {name:?}"));
        }
        if supplied
            .insert(name.to_string(), value.to_string())
            .is_some()
        {
            return Err(format!("duplicate input {name:?}"));
        }
    }
    for (name, spec) in &plugin.inputs {
        let value = supplied
            .get(name)
            .or(spec.default.as_ref())
            .ok_or_else(|| format!("missing input {name}=…"))?
            .clone();
        spec.validate(&value).map_err(|e| format!("{name}: {e}"))?;
        supplied.insert(name.clone(), value);
    }
    Ok(supplied)
}

/// Whether a run without arguments asks for input values first.
pub fn needs_form(plugin: &Plugin) -> bool {
    !plugin.inputs.is_empty()
        && (plugin.prompt.as_deref() == Some("always")
            || plugin.inputs.values().any(|spec| spec.default.is_none()))
}

pub fn input_arg(value: &str, inputs: &BTreeMap<String, String>) -> String {
    value
        .strip_prefix("${input.")
        .and_then(|s| s.strip_suffix('}'))
        .and_then(|key| inputs.get(key))
        .cloned()
        .unwrap_or_else(|| value.into())
}

pub fn available(plugin: &Plugin) -> Result<(), String> {
    let missing: Vec<_> = plugin
        .requires
        .iter()
        .filter(|name| executable(name).is_none())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing executables: {}{}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            plugin
                .install
                .as_ref()
                .map(|s| format!(" — {s}"))
                .unwrap_or_default()
        ))
    }
}

pub fn executable(name: &str) -> Option<PathBuf> {
    let candidates = if name.contains(std::path::MAIN_SEPARATOR) {
        vec![PathBuf::from(name)]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(name))
            .collect()
    };
    candidates
        .into_iter()
        .map(|path| executable_path(path, std::env::consts::EXE_SUFFIX))
        .find(|p| {
            p.metadata().is_ok_and(|m| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    m.is_file() && m.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    m.is_file()
                }
            })
        })
}

fn executable_path(path: PathBuf, suffix: &str) -> PathBuf {
    if !suffix.is_empty() && path.extension().is_none() && !path.is_file() {
        path.with_extension(suffix.trim_start_matches('.'))
    } else {
        path
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    /// Publication metadata, in Cargo's spelling. Absent from a manifest that is
    /// only ever installed by hand; required by the catalog, which generates its
    /// index entry from it.
    #[serde(default)]
    package: Option<Package>,
    plugin: Option<Plugin>,
    commands: Option<Vec<Plugin>>,
}

/// The `[package]` table: who publishes this package, under what licence, and
/// which sofka versions and platforms it supports. Sofka validates it and
/// otherwise leaves it alone — nothing here changes how a plugin runs.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub version: String,
    pub display_name: Option<String>,
    pub description: String,
    pub license: String,
    #[serde(default)]
    pub authors: Vec<String>,
    pub repository: Option<String>,
    pub readme: Option<String>,
    /// Semantic version requirement on sofka itself, like Cargo's
    /// `rust-version`. Its lower bound must include support for this manifest.
    pub sofka: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Catalog-only tool metadata for adapters that can use any of several
    /// executable names. Runtime discovery remains the adapter's job.
    #[serde(default)]
    pub requirements: Vec<PackageRequirement>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRequirement {
    pub name: String,
    #[serde(default)]
    pub alternatives: Vec<String>,
    pub install: String,
}

pub fn validate_package(package: &Package) -> Result<(), String> {
    if package
        .display_name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err("package display_name must not be empty".into());
    }
    if semver::Version::parse(&package.version).is_err() {
        return Err(format!(
            "package version {:?} is not a semantic version",
            package.version
        ));
    }
    if package.description.trim().is_empty() || package.license.trim().is_empty() {
        return Err("package description and license must not be empty".into());
    }
    if package
        .authors
        .iter()
        .any(|author| author.trim().is_empty())
    {
        return Err("package authors must not contain an empty entry".into());
    }
    let requirement = package
        .sofka
        .as_ref()
        .map(|sofka| {
            semver::VersionReq::parse(sofka)
                .map_err(|_| format!("package sofka {sofka:?} is not a version requirement"))
        })
        .transpose()?;
    if package.display_name.is_some()
        && !requirement.as_ref().is_some_and(|requirement| {
            requires_version(requirement, &semver::Version::new(0, 27, 1))
        })
    {
        return Err("package display_name requires a sofka requirement that excludes versions before 0.27.1".into());
    }
    for field in [&package.repository, &package.readme] {
        if field.as_ref().is_some_and(|value| value.trim().is_empty()) {
            return Err("package repository and readme must not be empty".into());
        }
    }
    if package
        .repository
        .as_ref()
        .is_some_and(|url| !url.starts_with("https://"))
    {
        return Err("package repository must use HTTPS".into());
    }
    // Not checked against the targets this build knows: `platforms` says what
    // the package publishes for, and rejecting an unfamiliar triple would make
    // every older sofka drop a working package the day the catalog adds a
    // target. What decides installability is the artifact list, which
    // `validate_artifact` still holds to the supported set.
    if package.platforms.iter().any(|platform| {
        platform.trim().is_empty()
            || package.platforms.iter().filter(|p| *p == platform).count() > 1
    }) {
        return Err("package platforms must be distinct and not empty".into());
    }
    if package.tags.iter().any(|tag| tag.trim().is_empty()) {
        return Err("package tags must not contain an empty entry".into());
    }
    for requirement in &package.requirements {
        if requirement.name.trim().is_empty()
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
        {
            return Err(
                "package requirements must name distinct executables and installation instructions"
                    .into(),
            );
        }
    }
    Ok(())
}

fn requires_version(requirement: &semver::VersionReq, minimum: &semver::Version) -> bool {
    use semver::{Op, Prerelease, Version};
    requirement.comparators.iter().any(|comparator| {
        let mut lower = Version::new(
            comparator.major,
            comparator.minor.unwrap_or(0),
            comparator.patch.unwrap_or(0),
        );
        lower.pre = comparator.pre.clone();
        match comparator.op {
            Op::Exact | Op::GreaterEq | Op::Caret | Op::Tilde | Op::Wildcard => lower >= *minimum,
            Op::Greater if lower >= *minimum => true,
            Op::Greater if comparator.pre.is_empty() => {
                let component = if comparator.patch.is_some() {
                    &mut lower.patch
                } else if comparator.minor.is_some() {
                    &mut lower.minor
                } else {
                    &mut lower.major
                };
                let Some(next) = component.checked_add(1) else {
                    return false;
                };
                *component = next;
                if requirement.comparators.iter().any(|other| {
                    !other.pre.is_empty()
                        && other.major == lower.major
                        && other.minor == Some(lower.minor)
                        && other.patch == Some(lower.patch)
                }) {
                    lower.pre = Prerelease::new("0").expect("valid prerelease");
                }
                lower >= *minimum
            }
            _ => false,
        }
    })
}

/// The manifest exactly as the package declares it, before `read_package`
/// resolves a relative command against the package directory. Checking a
/// package against the catalog entry it was selected from has to compare what
/// the author wrote, not the absolute path this process resolved it to.
pub fn read_package_manifest(dir: &Path) -> Result<(Vec<Plugin>, Option<Package>), String> {
    let path = dir.join("plugin.toml");
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(|e| e.to_string())?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("manifest exceeds 1 MiB".into());
    }
    read_manifest(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
}

pub fn read_package(dir: &Path) -> Result<Vec<Plugin>, String> {
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    let mut commands = read_package_manifest(&dir)?.0;
    for plugin in &mut commands {
        if plugin.command.starts_with("./") {
            let command = executable_path(dir.join(&plugin.command), std::env::consts::EXE_SUFFIX)
                .canonicalize()
                .map_err(|e| e.to_string())?;
            if !command.starts_with(&dir) {
                return Err("relative command escapes package directory".into());
            }
            plugin.command = command.to_string_lossy().into_owned();
        }
        for requirement in &mut plugin.requires {
            if requirement.starts_with("./") {
                *requirement = dir.join(&*requirement).to_string_lossy().into_owned();
            }
        }
        if !plugin.requires.contains(&plugin.command) {
            plugin.requires.push(plugin.command.clone());
        }
        plugin.package_dir = Some(dir.clone());
    }
    Ok(commands)
}

/// Parse and validate a `plugin.toml`, wherever it came from.
pub fn parse_manifest(text: &str) -> Result<Vec<Plugin>, String> {
    Ok(read_manifest(text)?.0)
}

/// The whole manifest: how the plugin runs, and the publication metadata when
/// the package declares any.
pub fn read_manifest(text: &str) -> Result<(Vec<Plugin>, Option<Package>), String> {
    let manifest: Manifest = toml::from_str(text).map_err(|e| e.to_string())?;
    let commands = match (manifest.schema_version, manifest.plugin, manifest.commands) {
        (1, Some(plugin), None) => vec![plugin],
        (2, None, Some(commands)) if !commands.is_empty() => commands,
        (1 | 2, _, _) => return Err("schema 1 requires [plugin]; schema 2 requires nonempty [[commands]]; do not mix the formats".into()),
        _ => return Err("unsupported schema_version (expected 1 or 2)".into()),
    };
    if let Some(package) = &manifest.package {
        validate_package(package)?;
    }
    for (index, command) in commands.iter().enumerate() {
        validate_plugin(command)?;
        if commands[..index]
            .iter()
            .any(|other| command_conflicts(command, other))
        {
            return Err(format!(
                "duplicate command name, palette, or key: {}",
                command.name
            ));
        }
    }
    Ok((commands, manifest.package))
}

/// The manifest of every package sofka ships. Kept as real files under
/// `plugins/` so `--validate-plugin` covers them like any other package.
const BUNDLED: &[(&str, &str)] = &[("sanitize", include_str!("../plugins/sanitize/plugin.toml"))];

/// The packages sofka ships, with `command` pointed at the running executable.
/// The adapters live in this binary, so a normal install has nothing to copy
/// and needs no extra language runtime on PATH.
pub fn bundled() -> Vec<Result<Plugin, String>> {
    let exe = std::env::current_exe();
    BUNDLED
        .iter()
        .flat_map(|(name, text)| {
            let commands = parse_manifest(text).and_then(|mut commands| {
                let exe = exe
                    .as_ref()
                    .map_err(|e| format!("locating the sofka binary: {e}"))?;
                for command in &mut commands {
                    command.command = exe.to_string_lossy().into_owned();
                    command.requires = vec![command.command.clone()];
                    command.bundled = true;
                }
                Ok(commands)
            });
            match commands {
                Ok(commands) => commands.into_iter().map(Ok).collect(),
                Err(error) => vec![Err(format!("bundled {name}: {error}"))],
            }
        })
        .collect()
}

pub(crate) fn command_conflicts(left: &Plugin, right: &Plugin) -> bool {
    left.name == right.name
        || (left.palette.is_some() && left.palette == right.palette)
        || keys_conflict(&left.key, &left.scopes, &right.key, &right.scopes)
}

/// Whether two key bindings would fire on the same keypress in some view.
pub(crate) fn keys_conflict(
    left: &str,
    left_scopes: &[String],
    right: &str,
    right_scopes: &[String],
) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let same = match (
        crate::keys::KeyChord::parse(left),
        crate::keys::KeyChord::parse(right),
    ) {
        (Ok(left), Ok(right)) => crate::keymap::overlaps(&left, &right),
        _ => left == right,
    };
    same && (left_scopes.is_empty()
        || right_scopes.is_empty()
        || left_scopes.iter().any(|scope| right_scopes.contains(scope)))
}

pub fn validate_plugin(plugin: &Plugin) -> Result<(), String> {
    if let Some(field) = plugin.unknown_fields.keys().next() {
        return Err(format!("unknown field {field:?} in plugin manifest"));
    }
    if plugin.name.trim().is_empty() || plugin.command.trim().is_empty() {
        return Err("name and command must not be empty".into());
    }
    if let Some(name) = &plugin.palette
        && (name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    {
        return Err("palette must contain lowercase letters, digits or hyphens".into());
    }
    if plugin
        .palette
        .as_deref()
        .is_some_and(crate::app::plugin_command_reserved)
    {
        return Err("palette command is reserved by sofka".into());
    }
    if plugin
        .prompt
        .as_deref()
        .is_some_and(|prompt| !matches!(prompt, "missing" | "always"))
    {
        return Err("prompt must be missing or always".into());
    }
    let warnings = crate::config::plugin_warnings(std::slice::from_ref(plugin));
    if !warnings.is_empty() {
        return Err(warnings.join("; "));
    }
    if !matches!(
        plugin.target.as_deref(),
        None | Some("selection" | "context")
    ) {
        return Err("target must be selection or context".into());
    }
    if plugin.port_forward.is_some()
        && (plugin.target.as_deref() == Some("context")
            || plugin.output.as_deref() != Some("report"))
    {
        return Err("port_forward requires target = selection and output = report".into());
    }
    if !matches!(
        plugin.output.as_deref(),
        Some("popup" | "background" | "report")
    ) {
        return Err("packages require captured output: popup, background or report".into());
    }
    if plugin.shell {
        return Err("packages must use an executable adapter, not shell = true".into());
    }
    for (name, spec) in &plugin.inputs {
        if let Some(field) = spec.unknown_fields.keys().next() {
            return Err(format!("unknown field {field:?} in plugin input {name:?}"));
        }
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(format!("invalid input name {name:?}"));
        }
        if !matches!(
            spec.kind.as_str(),
            "string" | "integer" | "boolean" | "duration"
        ) {
            return Err(format!("{name}: unsupported input type"));
        }
        if spec.min.zip(spec.max).is_some_and(|(min, max)| min > max) {
            return Err(format!("{name}: min exceeds max"));
        }
        if (spec.min.is_some() || spec.max.is_some())
            && !matches!(spec.kind.as_str(), "integer" | "duration")
        {
            return Err(format!("{name}: min/max require integer or duration"));
        }
        if let Some(value) = &spec.default {
            spec.validate(value).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    for argument in plugin.args.iter().chain(plugin.port_forward.iter()) {
        if argument.contains("${input.") {
            let name = argument.strip_prefix("${input.").and_then(|a| a.strip_suffix('}'))
                .filter(|name| plugin.inputs.contains_key(*name))
                .ok_or_else(|| format!("invalid input placeholder {argument:?}; use a declared input as a whole argument"))?;
            if name.is_empty() {
                return Err("empty input placeholder".into());
            }
        }
    }
    Ok(())
}

pub fn load_packages(dir: &Path, plugins: &mut Vec<Plugin>, warnings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            warnings.push(format!("{}: {e}", dir.display()));
            return;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) if e.path().is_dir() => paths.push(e.path()),
            Ok(_) => {}
            Err(e) => warnings.push(format!("{}: {e}", dir.display())),
        }
    }
    paths.sort();
    for path in paths {
        match read_package(&path) {
            Ok(commands) => {
                for p in commands {
                    if plugins.iter().any(|old| command_conflicts(old, &p)) {
                        warnings.push(format!(
                            "{}: duplicate plugin name/command; earlier configuration wins",
                            path.display()
                        ));
                    } else {
                        if let Err(e) = available(&p) {
                            warnings.push(format!("plugin {}: {e}", p.name));
                        }
                        plugins.push(p);
                    }
                }
            }
            Err(e) => warnings.push(format!("ignoring {}: {e}", path.display())),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    schema_version: u32,
    title: String,
    #[serde(default)]
    sections: Vec<Section>,
    #[serde(default)]
    action: Option<ReportAction>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportAction {
    ReloadKubeconfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    title: String,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<String>>,
}

pub fn render_report(bytes: &[u8]) -> Result<Vec<String>, String> {
    render_report_with_action(bytes).map(|(lines, _)| lines)
}

pub(crate) fn render_report_with_action(
    bytes: &[u8],
) -> Result<(Vec<String>, Option<ReportAction>), String> {
    if bytes.len() > MAX_BYTES {
        return Err("report exceeds 1 MiB".into());
    }
    let report: Report =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid plugin report: {e}"))?;
    if report.schema_version != 1 {
        return Err("unsupported report schema_version (expected 1)".into());
    }
    let mut lines = Lines::default();
    lines.push(clean(&report.title));
    for section in report.sections {
        if section
            .rows
            .iter()
            .any(|row| row.len() != section.columns.len())
        {
            return Err("report row length does not match columns".into());
        }
        if lines.truncated {
            continue;
        }
        lines.push(String::new());
        lines.push(clean(&section.title));
        lines.extend(section.lines.iter().map(|s| clean(s)));
        let columns: Vec<String> = section.columns.iter().map(|s| clean(s)).collect();
        let rows: Vec<Vec<String>> = section
            .rows
            .iter()
            .map(|row| row.iter().map(|s| clean(s)).collect())
            .collect();
        let mut widths: Vec<usize> = columns.iter().map(|s| s.width()).collect();
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.width());
            }
        }
        if !columns.is_empty() {
            lines.push(report_row(&columns, &widths));
            lines.push(
                widths
                    .iter()
                    .map(|width| "─".repeat(*width))
                    .collect::<Vec<_>>()
                    .join("─┼─"),
            );
        }
        for row in rows {
            if lines.truncated {
                break;
            }
            lines.push(report_row(&row, &widths));
        }
    }
    Ok((lines.finish(), report.action))
}

fn report_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        line.push_str(cell);
        if index + 1 < cells.len() {
            line.extend(std::iter::repeat_n(' ', width - cell.width()));
            line.push_str(" │ ");
        }
    }
    line
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[derive(Default)]
pub struct Lines {
    lines: Vec<String>,
    bytes: usize,
    truncated: bool,
}

impl Lines {
    pub fn push(&mut self, line: String) {
        if self.truncated {
            return;
        }
        if self.lines.len() >= MAX_LINES || self.bytes + line.len() > MAX_BYTES {
            self.truncated = true;
        } else {
            self.bytes += line.len();
            self.lines.push(line);
        }
    }

    pub fn extend(&mut self, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            self.push(line);
        }
    }

    pub fn finish(mut self) -> Vec<String> {
        if self.truncated {
            self.lines.push("… output truncated".into());
        }
        self.lines
    }
}

pub fn bound_lines(lines: Vec<String>) -> Vec<String> {
    let mut output = Lines::default();
    output.extend(lines);
    output.finish()
}

pub struct Job {
    pub label: String,
    pub namespace: String,
    pub argv: Vec<String>,
    pub directory: Option<PathBuf>,
    pub request: Option<Value>,
    pub object: Option<std::sync::Arc<kube::core::DynamicObject>>,
    pub forward: Option<Forward>,
}

pub struct Forward {
    pub argv: Vec<String>,
    pub remote: u16,
    pub local: Option<u16>,
}

/// Dropping an owner cancels its task instead of detaching it.
pub struct Task(pub tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Process {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: u32,
}
impl Process {
    fn spawn(command: &mut tokio::process::Command) -> std::io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn()?;
        Ok(Self {
            #[cfg(unix)]
            group: child.id().unwrap_or_default(),
            child,
        })
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Adapters may launch scanners; cancel the whole private process group.
        #[cfg(unix)]
        if self.group > 0 {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.group)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.start_kill();
    }
}

async fn read_limited(
    mut stream: impl AsyncRead + Unpin,
    activity: Option<(activity::Activity, String)>,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut sanitizer = activity::Sanitizer::default();
    loop {
        let n = stream.read(&mut chunk).await?;
        if let Some((activity, label)) = &activity {
            let text = sanitizer.feed(&chunk[..n], n == 0);
            if !text.is_empty() {
                activity.append(&clean(label), &text);
            }
        }
        if n == 0 {
            return Ok(bytes);
        }
        if bytes.len() + n > MAX_BYTES {
            return Err(std::io::Error::other("plugin output exceeds 1 MiB"));
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

async fn forward(spec: &Forward) -> std::io::Result<(u16, Option<(Process, Task)>)> {
    if let Some(port) = spec.local {
        return Ok((port, None));
    }
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut cmd = tokio::process::Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .arg(format!(":{}", spec.remote))
        .arg("--address=127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = Process::spawn(&mut cmd)?;
    let mut stdout = BufReader::new(child.child.stdout.take().unwrap());
    let mut line = Vec::new();
    loop {
        let n = (&mut stdout)
            .take(4096)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 || line.len() >= 4096 {
            return Err(std::io::Error::other(
                "port-forward failed before becoming ready",
            ));
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(port) = text
            .strip_prefix("Forwarding from 127.0.0.1:")
            .and_then(|s| s.split_whitespace().next())
            .and_then(|p| p.parse().ok())
        {
            let drain = Task(tokio::spawn(async move {
                let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
            }));
            return Ok((port, Some((child, drain))));
        }
        line.clear();
    }
}

struct RequestBuffer(Vec<u8>);

impl std::io::Write for RequestBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + bytes.len() > MAX_BYTES {
            return Err(std::io::Error::other("plugin request exceeds 1 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub async fn execute(job: Job) -> std::io::Result<std::process::Output> {
    execute_with_activity(job, None).await
}

pub async fn execute_with_activity(
    mut job: Job,
    activity: Option<(activity::Activity, String)>,
) -> std::io::Result<std::process::Output> {
    let mut forward_owner = None;
    if let Some(spec) = &job.forward {
        let (port, owner) = forward(spec).await?;
        forward_owner = owner;
        if let Some(request) = &mut job.request {
            request["forward"] = serde_json::json!({"host": "127.0.0.1", "local_port": port, "remote_port": spec.remote});
        }
    }
    let input = job
        .request
        .as_ref()
        .map(|request| {
            let mut writer = RequestBuffer(Vec::new());
            if let Some(object) = &job.object {
                #[derive(serde::Serialize)]
                struct Request<'a> {
                    #[serde(flatten)]
                    fields: &'a Value,
                    object: &'a kube::core::DynamicObject,
                }
                serde_json::to_writer(
                    &mut writer,
                    &Request {
                        fields: request,
                        object,
                    },
                )?;
            } else {
                serde_json::to_writer(&mut writer, request)?;
            }
            Ok::<_, std::io::Error>(writer.0)
        })
        .transpose()?;
    let mut command = tokio::process::Command::new(&job.argv[0]);
    command
        .args(&job.argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &job.directory {
        command.current_dir(dir);
    }
    let mut process = Process::spawn(&mut command)?;
    let mut stdin = process.child.stdin.take().unwrap();
    let stdout = process.child.stdout.take().unwrap();
    let stderr = process.child.stderr.take().unwrap();

    let write = async {
        if let Some(input) = input {
            stdin.write_all(&input).await?;
        }
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    let (_, stdout, stderr, status) = tokio::try_join!(
        write,
        read_limited(stdout, None),
        read_limited(stderr, activity),
        process.child.wait()
    )?;
    drop(forward_owner);
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_executable_lookup_adds_a_suffix_only_when_needed() {
        let dir = std::env::temp_dir().join(format!("sofka-exe-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("adapter");
        let exe = dir.join("adapter.exe");
        std::fs::write(&exe, b"binary").unwrap();
        assert_eq!(executable_path(path.clone(), ".exe"), exe);
        assert_eq!(executable_path(exe.clone(), ".exe"), exe);
        assert_eq!(executable_path(path.clone(), ""), path);
        std::fs::write(&path, b"native binary").unwrap();
        assert_eq!(executable_path(path.clone(), ".exe"), path);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn job(command: &str, args: &[&str]) -> Job {
        Job {
            label: "test".into(),
            namespace: String::new(),
            argv: std::iter::once(command)
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect(),
            directory: None,
            request: None,
            object: None,
            forward: None,
        }
    }

    fn package(text: &str) -> Result<Plugin, String> {
        let dir = std::env::temp_dir().join(format!(
            "sofka-package-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), text).unwrap();
        let result = read_package(&dir).map(|mut commands| commands.remove(0));
        std::fs::remove_dir_all(dir).unwrap();
        result
    }

    const PACKAGED: &str = concat!(
        "schema_version = 1\n",
        "\n",
        "[package]\n",
        "version = \"0.1.0\"\n",
        "description = \"Scan the active context.\"\n",
        "license = \"MIT OR Apache-2.0\"\n",
        "authors = [\"sofka maintainers\"]\n",
        "repository = \"https://github.com/nklmilojevic/sofka-plugins\"\n",
        "readme = \"README.md\"\n",
        "sofka = \">=0.26.0\"\n",
        "platforms = [\"x86_64-apple-darwin\"]\n",
        "tags = [\"diagnostics\"]\n",
        "requirements = [{ name = \"popeye\", alternatives = [\"kubectl-popeye\"], install = \"Install Popeye\" }]\n",
        "\n",
        "[plugin]\n",
        "name = \"Popeye scan\"\n",
        "palette = \"popeye\"\n",
        "command = \"./adapter\"\n",
        "output = \"report\"\n",
        "target = \"context\"\n",
        "mutating = false\n",
    );

    #[test]
    fn command_manifests_validate_all_entries_and_reject_ambiguous_formats() {
        let first = PACKAGED
            .replace("schema_version = 1", "schema_version = 2")
            .replace("[plugin]", "[[commands]]");
        let second = r#"
[[commands]]
name = "Renew"
palette = "cert-manager-renew"
key = "ctrl-r"
command = "/bin/echo"
args = ["renew"]
scopes = ["certificates"]
output = "report"
mutating = true
confirm = true
[commands.inputs.force]
type = "boolean"
default = "false"
"#;
        let text = format!("{first}{second}");
        let (commands, _) = read_manifest(&text).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].mutating, Some(false));
        assert_eq!(commands[1].mutating, Some(true));
        assert!(commands[1].confirm);
        assert!(commands[0].inputs.is_empty());
        assert!(commands[1].inputs.contains_key("force"));
        for invalid in [
            text.replace("cert-manager-renew", "popeye"),
            text.replace("name = \"Renew\"", "name = \"Popeye scan\""),
            text.replace("output = \"report\"", "output = \"terminal\""),
            text.replace("schema_version = 2", "schema_version = 1"),
            format!("{first}\n[plugin]\nname = \"Legacy\"\ncommand = \"echo\""),
            "schema_version = 2\ncommands = []".into(),
            "schema_version = 2".into(),
        ] {
            assert!(read_manifest(&invalid).is_err(), "accepted {invalid}");
        }
        let duplicated_key = text.replace(
            "palette = \"popeye\"",
            "palette = \"popeye\"\nkey = \"ctrl-r\"",
        );
        assert!(read_manifest(&duplicated_key).is_err());
    }

    fn scoped_manifest(first: &str, second: &str) -> String {
        format!(
            "schema_version = 2\n\
             [[commands]]\nname = \"Pod x\"\ncommand = \"cat\"\noutput = \"background\"\nkey = \"x\"\n{first}\n\
             [[commands]]\nname = \"Deployment x\"\ncommand = \"cat\"\noutput = \"background\"\n{second}\n"
        )
    }

    #[test]
    fn shared_keys_conflict_only_when_scopes_overlap() {
        let disjoint = scoped_manifest(
            "scopes = [\"pods\"]",
            "key = \"x\"\nscopes = [\"deployments\"]",
        );
        assert_eq!(read_manifest(&disjoint).unwrap().0.len(), 2);
        for invalid in [
            scoped_manifest(
                "scopes = [\"pods\", \"services\"]",
                "key = \"x\"\nscopes = [\"services\"]",
            ),
            scoped_manifest("scopes = [\"pods\"]", "key = \"x\""),
            scoped_manifest("", "key = \"x\"\nscopes = [\"deployments\"]"),
            scoped_manifest("scopes = [\"pods\"]", "key = \"X\"\nscopes = [\"pods\"]")
                .replace("key = \"x\"", "key = \"shift-x\""),
            scoped_manifest(
                "scopes = [\"pods\"]",
                "key = \"control-X\"\nscopes = [\"pods\"]",
            )
            .replace("key = \"x\"", "key = \"ctrl-x\""),
        ] {
            let error = read_manifest(&invalid).unwrap_err();
            assert!(error.contains("duplicate"), "{error}: {invalid}");
        }
    }

    #[test]
    fn packages_reuse_a_key_across_disjoint_scopes() {
        let dir = std::env::temp_dir().join(format!("sofka-scoped-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (package, name, scope) in [
            ("a-pod-x", "Pod x", "pods"),
            ("b-deployment-x", "Deployment x", "deployments"),
            ("c-pod-x-again", "Pod x again", "pods"),
        ] {
            std::fs::create_dir_all(dir.join(package)).unwrap();
            std::fs::write(
                dir.join(package).join("plugin.toml"),
                format!(
                    "schema_version = 2\n[[commands]]\nname = \"{name}\"\ncommand = \"cat\"\noutput = \"background\"\n\
                     key = \"x\"\nscopes = [\"{scope}\"]\nmutating = false\n"
                ),
            )
            .unwrap();
        }
        let (mut plugins, mut warnings) = (Vec::new(), Vec::new());
        load_packages(&dir, &mut plugins, &mut warnings);
        std::fs::remove_dir_all(&dir).unwrap();
        let names: Vec<_> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Pod x", "Deployment x"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("c-pod-x-again"), "{warnings:?}");
    }

    #[test]
    fn package_display_name_is_independent_of_command_names() {
        let manifest = PACKAGED
            .replace(
                "[package]",
                "[package]\ndisplay_name = \"Certificate tools\"",
            )
            .replace(">=0.26.0", ">=0.27.1");
        let (commands, package) = read_manifest(&manifest).unwrap();
        assert_eq!(
            package.unwrap().display_name.as_deref(),
            Some("Certificate tools")
        );
        assert_eq!(commands[0].name, "Popeye scan");
        for invalid in ["\"\"", "\"   \"", "false", "42"] {
            let manifest =
                PACKAGED.replace("[package]", &format!("[package]\ndisplay_name = {invalid}"));
            assert!(read_manifest(&manifest).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn package_titles_require_compatible_sofka_ranges() {
        let named = PACKAGED.replace(
            "[package]",
            "[package]\ndisplay_name = \"Certificate tools\"",
        );
        for range in [
            ">=0.27.1",
            "=0.27.1",
            "^0.27.1",
            "~0.27.1",
            ">0.27.0",
            ">0.27",
            ">=0.28",
            "1.*",
            ">=1",
            ">=0.27.1, <1",
            ">=0.27.1-alpha, >=0.27.1",
        ] {
            read_manifest(&named.replace(">=0.26.0", range))
                .unwrap_or_else(|error| panic!("{range}: {error}"));
        }
        for range in [
            "*",
            ">=0.27.0",
            "=0.26.0",
            "^0.27",
            "0.27.*",
            "~0.27.0",
            "<1",
            "<=0.27.1",
            ">0.26",
            ">=0.27.1-alpha",
            ">0.27.0, <0.27.1-beta",
            "latest",
        ] {
            assert!(
                read_manifest(&named.replace(">=0.26.0", range)).is_err(),
                "accepted {range}"
            );
        }
        assert!(read_manifest(&named.replace("sofka = \">=0.26.0\"\n", "")).is_err());
        read_manifest(&PACKAGED.replace("sofka = \">=0.26.0\"\n", "")).unwrap();
    }

    #[test]
    fn a_package_table_is_read_beside_the_execution_fields() {
        let (commands, package) = read_manifest(PACKAGED).unwrap();
        let plugin = &commands[0];
        // The execution half is untouched by the new table.
        assert_eq!(plugin.name, "Popeye scan");
        assert_eq!(plugin.palette.as_deref(), Some("popeye"));
        assert_eq!(plugin.command, "./adapter");
        let package = package.unwrap();
        assert!(package.display_name.is_none());
        assert_eq!(package.version, "0.1.0");
        assert_eq!(package.authors, ["sofka maintainers"]);
        assert_eq!(package.license, "MIT OR Apache-2.0");
        assert_eq!(package.sofka.as_deref(), Some(">=0.26.0"));
        assert_eq!(package.platforms, ["x86_64-apple-darwin"]);
        assert_eq!(package.tags, ["diagnostics"]);
        assert_eq!(package.requirements[0].name, "popeye");
        assert_eq!(package.requirements[0].alternatives, ["kubectl-popeye"]);
    }

    #[test]
    fn a_manifest_without_a_package_table_stays_valid() {
        let (plugin, package) = read_manifest(
            "schema_version = 1\n[plugin]\nname = \"Local\"\npalette = \"local\"\ncommand = \"/bin/echo\"\noutput = \"popup\"\n",
        )
        .unwrap();
        assert_eq!(plugin[0].name, "Local");
        assert!(package.is_none());
    }

    #[test]
    fn package_prompt_errors_do_not_promise_a_fallback() {
        let mut plugin = Plugin {
            name: "Scan".into(),
            palette: Some("scan".into()),
            command: "./adapter".into(),
            output: Some("report".into()),
            prompt: Some("always".into()),
            ..Default::default()
        };
        assert_eq!(validate_plugin(&plugin), Ok(()));
        plugin.prompt = Some("never".into());
        assert_eq!(
            validate_plugin(&plugin),
            Err("prompt must be missing or always".into())
        );
    }

    #[test]
    fn package_metadata_is_validated_field_by_field() {
        for (label, from, to) in [
            ("version", "version = \"0.1.0\"", "version = \"one\""),
            (
                "description",
                "description = \"Scan the active context.\"",
                "description = \"  \"",
            ),
            (
                "license",
                "license = \"MIT OR Apache-2.0\"",
                "license = \"\"",
            ),
            (
                "author",
                "authors = [\"sofka maintainers\"]",
                "authors = [\"\"]",
            ),
            ("sofka range", "sofka = \">=0.26.0\"", "sofka = \"latest\""),
            (
                "platform",
                "platforms = [\"x86_64-apple-darwin\"]",
                "platforms = [\"\"]",
            ),
            (
                "duplicate platform",
                "platforms = [\"x86_64-apple-darwin\"]",
                "platforms = [\"x86_64-apple-darwin\", \"x86_64-apple-darwin\"]",
            ),
            (
                "repository",
                "repository = \"https://github.com/nklmilojevic/sofka-plugins\"",
                "repository = \"http://insecure\"",
            ),
            ("tag", "tags = [\"diagnostics\"]", "tags = [\"\"]"),
            (
                "requirement",
                "alternatives = [\"kubectl-popeye\"]",
                "alternatives = [\"popeye\"]",
            ),
        ] {
            let manifest = PACKAGED.replace(from, to);
            assert!(read_manifest(&manifest).is_err(), "accepted {label}");
        }
        // A target this build has never heard of is still a valid declaration.
        // Refusing it would drop a working package from every older sofka the
        // day the catalog publishes for a new triple; what gates installation
        // is the artifact list, which the catalog validates separately.
        assert!(
            read_manifest(&PACKAGED.replace(
                "platforms = [\"x86_64-apple-darwin\"]",
                "platforms = [\"x86_64-unknown-linux-musl\"]",
            ))
            .is_ok()
        );
        // An unknown key in the new table is refused like any other.
        assert!(
            read_manifest(&PACKAGED.replace("[package]", "[package]\npublisher = \"x\"")).is_err()
        );
        read_manifest(PACKAGED).unwrap();
    }

    #[test]
    fn optional_package_fields_may_be_absent() {
        let minimal = concat!(
            "schema_version = 1\n",
            "[package]\n",
            "version = \"0.1.0\"\n",
            "description = \"Scan.\"\n",
            "license = \"MIT\"\n",
            "[plugin]\n",
            "name = \"Scan\"\n",
            "palette = \"scan\"\n",
            "command = \"/bin/echo\"\n",
            "output = \"popup\"\n",
        );
        let package = read_manifest(minimal).unwrap().1.unwrap();
        assert!(package.authors.is_empty());
        assert!(package.repository.is_none());
        assert!(package.sofka.is_none());
        assert!(package.platforms.is_empty());
        assert!(package.requirements.is_empty());
    }

    #[test]
    fn manifests_reject_unknown_versions_fields_commands_and_invalid_inputs() {
        let valid = "schema_version = 1\n[plugin]\nname = 'Demo'\npalette = 'demo'\ncommand = '/bin/cat'\noutput = 'report'\n";
        assert!(package(valid).is_ok());
        assert!(
            package(&valid.replace("schema_version = 1", "schema_version = 3"))
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(
            package(&format!("{valid}typo = true\n"))
                .unwrap_err()
                .contains("unknown field")
        );
        let nested_error = package(&format!(
            "{valid}[plugin.inputs.count]\ntype = 'integer'\ndefault = '2'\nobsolete_option = true\n"
        ))
        .unwrap_err();
        assert!(nested_error.contains("unknown field \"obsolete_option\""));
        assert!(nested_error.contains("plugin input \"count\""));
        assert!(
            package(&valid.replace("palette = 'demo'", "palette = 'ctx'"))
                .unwrap_err()
                .contains("reserved")
        );
        assert!(
            package(&valid.replace("output = 'report'", "output = 'terminal'"))
                .unwrap_err()
                .contains("captured output")
        );
        assert!(
            package(&format!(
                "{valid}[plugin.inputs.count]\ntype = 'integer'\ndefault = '99'\nmax = 5\n"
            ))
            .unwrap_err()
            .contains("outside range")
        );
        assert!(
            package(&format!("{valid}args = ['${{input.missing}}']\n"))
                .unwrap_err()
                .contains("invalid input placeholder")
        );
    }

    #[test]
    fn report_aligns_cleaned_unicode_cells_and_sizes_each_section() {
        let report = serde_json::json!({
            "schema_version": 1,
            "title": "Report",
            "sections": [
                {"title": "Summary", "lines": ["Plain text stays unchanged."]},
                {"title": "Rows", "columns": ["Name", "State", "Value"],
                 "rows": [["界", "ok", "1"], ["e\u{301}", "ready", ""], ["a\tb", "", "3"]]},
                {"title": "Empty", "columns": ["ID", "Count"]}
            ]
        });
        let lines = render_report(&serde_json::to_vec(&report).unwrap()).unwrap();
        assert_eq!(
            lines,
            [
                "Report",
                "",
                "Summary",
                "Plain text stays unchanged.",
                "",
                "Rows",
                "Name │ State │ Value",
                "─────┼───────┼──────",
                "界   │ ok    │ 1",
                "e\u{301}    │ ready │ ",
                "a b  │       │ 3",
                "",
                "Empty",
                "ID │ Count",
                "───┼──────"
            ]
        );
    }

    #[test]
    fn report_bounds_expanded_tables_and_validates_after_truncation() {
        let mut rows = vec![vec!["x".repeat(100_000), "1".into()]];
        rows.extend(vec![vec!["short".into(), "2".into()]; 200]);
        let mut report = serde_json::json!({
            "schema_version": 1, "title": "Report",
            "sections": [{"title": "Rows", "columns": ["Name", "Count"], "rows": rows}]
        });
        let bytes = serde_json::to_vec(&report).unwrap();
        assert!(bytes.len() < MAX_BYTES);
        let lines = render_report(&bytes).unwrap();
        assert!(lines.iter().map(String::len).sum::<usize>() <= MAX_BYTES + 32);
        assert!(lines.last().unwrap().contains("truncated"));
        report["sections"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "title": "Invalid", "columns": ["Name"], "rows": [["a", "b"]]
            }));
        assert!(
            render_report(&serde_json::to_vec(&report).unwrap())
                .unwrap_err()
                .contains("row length")
        );
    }

    #[test]
    fn report_rejects_wrong_row_shapes_and_bounds_rendered_lines() {
        let bad = br#"{"schema_version":1,"title":"Scan","sections":[{"title":"Rows","columns":["One"],"rows":[["a","b"]]}]}"#;
        assert!(render_report(bad).unwrap_err().contains("row length"));
        let report = serde_json::json!({"schema_version": 1, "title": "Scan", "sections": [{"title": "Rows", "lines": vec!["test"; MAX_LINES + 1]}]});
        let lines = render_report(&serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(lines.len() <= MAX_LINES + 1);
        assert!(lines.last().unwrap().contains("truncated"));
        assert!(
            render_report(&vec![b' '; MAX_BYTES + 1])
                .unwrap_err()
                .contains("exceeds")
        );
    }

    #[test]
    fn aggregate_output_budget_applies_across_jobs() {
        let mut lines = Lines::default();
        for _ in 0..100 {
            lines.push("x".repeat(50_000));
        }
        let output = lines.finish();
        assert!(output.iter().map(String::len).sum::<usize>() <= MAX_BYTES + 32);
        assert!(output.last().unwrap().contains("truncated"));
    }

    #[tokio::test]
    async fn capture_stops_chatty_processes_before_unbounded_allocation() {
        let error = execute(job("/bin/sh", &["-c", "head -c 1100000 /dev/zero"]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 1 MiB"));
    }

    #[tokio::test]
    async fn activity_drains_both_pipes_without_ui_consumption_and_preserves_exact_caps() {
        let (activity, receiver) = activity::Activity::new();
        let command = job(
            "/bin/sh",
            &[
                "-c",
                r"head -c 1048576 /dev/zero & head -c 1048576 /dev/zero | tr '\000' x >&2; wait",
            ],
        );
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            execute_with_activity(command, Some((activity, String::new()))),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_BYTES);
        assert_eq!(output.stderr.len(), MAX_BYTES);
        let snapshot = receiver.borrow().clone();
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].len(), 2048);
        assert!(snapshot.lines.iter().map(String::len).sum::<usize>() <= 64 * 1024);
        for redirection in ["", ">&2"] {
            let script = format!("head -c 1048577 /dev/zero {redirection}");
            let error = execute(job("/bin/sh", &["-c", &script])).await.unwrap_err();
            assert!(error.to_string().contains("exceeds 1 MiB"));
        }
    }

    #[tokio::test]
    async fn activity_streams_before_adapter_exit_without_altering_stdout() {
        let (activity, mut receiver) = activity::Activity::new();
        let command = job(
            "/bin/sh",
            &["-c", "printf 'phase one\\n' >&2; sleep 0.2; printf report"],
        );
        let task = tokio::spawn(execute_with_activity(
            command,
            Some((activity, String::new())),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(3), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            receiver
                .borrow()
                .lines
                .iter()
                .any(|line| line == "phase one")
        );
        assert!(!task.is_finished());
        let output = task.await.unwrap().unwrap();
        assert_eq!(output.stdout, b"report");
        assert_eq!(output.stderr, b"phase one\n");
    }

    #[tokio::test]
    async fn adapters_receive_stdin_and_run_in_their_package_directory() {
        let mut command = job("/bin/sh", &["-c", "pwd; cat"]);
        let dir = std::env::temp_dir().canonicalize().unwrap();
        command.directory = Some(dir.clone());
        command.request =
            Some(serde_json::json!({"schema_version":1,"object":{"metadata":{"name":"pod"}}}));
        let output = execute(command).await.unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let (cwd, json) = text.split_once('\n').unwrap();
        assert_eq!(Path::new(cwd), dir);
        assert_eq!(
            serde_json::from_str::<Value>(json).unwrap()["object"]["metadata"]["name"],
            "pod"
        );
    }

    #[tokio::test]
    async fn managed_forward_waits_for_readiness_and_supplies_local_port() {
        let mut command = job("/bin/cat", &[]);
        command.request = Some(serde_json::json!({"schema_version":1}));
        command.forward = Some(Forward {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'Forwarding from 127.0.0.1:32123 -> 80\\n'; sleep 30".into(),
                "forward".into(),
            ],
            remote: 80,
            local: None,
        });
        let output = tokio::time::timeout(std::time::Duration::from_secs(3), execute(command))
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        let request: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(request["forward"]["local_port"], 32123);
        assert_eq!(request["forward"]["remote_port"], 80);
    }

    #[tokio::test]
    async fn existing_forward_does_not_spawn_another_process() {
        let mut command = job("/bin/cat", &[]);
        command.request = Some(serde_json::json!({}));
        command.forward = Some(Forward {
            argv: vec!["does-not-exist".into()],
            remote: 443,
            local: Some(32124),
        });
        let output = execute(command).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["forward"]["local_port"],
            32124
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_adapter_descendants() {
        let dir = std::env::temp_dir().join(format!("sofka-plugin-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("orphan");
        let command = job(
            "/bin/sh",
            &[
                "-c",
                "(sleep 1; printf orphan > \"$1\") & wait",
                "test",
                marker.to_str().unwrap(),
            ],
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), execute(command))
                .await
                .is_err()
        );
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(!marker.exists(), "a descendant survived cancellation");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
