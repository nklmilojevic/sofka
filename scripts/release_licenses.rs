use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use flate2::{Compression, GzBuilder, read::GzDecoder};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const REQUIRED: [&str; 3] = ["LICENSE-MIT", "LICENSE-APACHE", "THIRD-PARTY-LICENSES.txt"];
const LICENSE_NAMES: [&str; 5] = ["LICENSE", "LICENCE", "COPYING", "COPYRIGHT", "NOTICE"];

#[derive(Parser)]
#[command(about = "Collect release notices and package a binary")]
struct Args {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Generate {
        #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))]
        manifest: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "cargo-about")]
        cargo_about: PathBuf,
        #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/target/license-cache"))]
        cache: PathBuf,
    },
    Package {
        #[arg(long)]
        binary: PathBuf,
        #[arg(long)]
        notices: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        rust_notices: PathBuf,
    },
}

struct Temporary(PathBuf);

impl Temporary {
    fn new(parent: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(parent)?;
        loop {
            let path = parent.join(format!(
                ".release-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn download(url: &str, cache: &Path) -> Result<Option<Vec<u8>>> {
    download_with(
        url,
        cache,
        |destination| {
            Command::new("curl")
                .args([
                    "--silent",
                    "--show-error",
                    "--location",
                    "--connect-timeout",
                    "15",
                    "--max-time",
                    "60",
                    "--output",
                ])
                .arg(destination)
                .args(["--write-out", "%{http_code}", url])
                .output()
                .context("Cannot start curl")
        },
        std::thread::sleep,
    )
}

fn download_with(
    url: &str,
    cache: &Path,
    mut fetch: impl FnMut(&Path) -> Result<Output>,
    mut sleep: impl FnMut(Duration),
) -> Result<Option<Vec<u8>>> {
    let path = cache.join(digest(url.as_bytes()));
    let missing = path.with_extension("missing");
    if path.exists() {
        return Ok(Some(fs::read(path)?));
    }
    if missing.exists() {
        return Ok(None);
    }
    let temporary = Temporary::new(cache)?;
    let destination = temporary.0.join("download");
    for attempt in 0..4 {
        // A new file prevents a partial response from entering the cache.
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        let result = fetch(&destination).with_context(|| format!("Download failed: {url}"))?;
        let status = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        if result.status.success() {
            match status.as_str() {
                "200" => {
                    let content = fs::read(&destination)?;
                    fs::rename(&destination, &path)?;
                    return Ok(Some(content));
                }
                "404" => {
                    fs::write(missing, [])?;
                    return Ok(None);
                }
                _ => {}
            }
        }
        let message = format!(
            "Download failed: {url} (curl {}, HTTP {status}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        );
        let retry = !result.status.success()
            || matches!(status.as_str(), "408" | "429")
            || status.starts_with('5');
        if !retry || attempt == 3 {
            bail!("{message}");
        }
        eprintln!("{message}; retry {} of 3", attempt + 1);
        sleep(Duration::from_secs(1 << attempt));
    }
    unreachable!()
}

fn files_in(directory: &Path) -> Result<Vec<PathBuf>> {
    ensure!(
        fs::symlink_metadata(directory)?.file_type().is_dir(),
        "Expected a regular directory: {}",
        directory.display()
    );
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "A file must not be a symlink: {}",
            entry.path().display()
        );
        if kind.is_dir() {
            files.extend(files_in(&entry.path())?);
        } else {
            ensure!(
                kind.is_file(),
                "Expected a regular file: {}",
                entry.path().display()
            );
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    id: String,
    manifest_path: PathBuf,
    license: Option<String>,
    license_file: Option<PathBuf>,
    source: Option<String>,
    repository: Option<String>,
}

#[derive(Deserialize)]
struct CrateEntry {
    package: Package,
}

#[derive(Deserialize)]
struct UsedCrate {
    id: String,
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct UsedBy {
    #[serde(rename = "crate")]
    krate: UsedCrate,
}

#[derive(Deserialize)]
struct License {
    id: String,
    name: String,
    text: String,
    used_by: Vec<UsedBy>,
}

#[derive(Deserialize)]
struct Report {
    crates: Vec<CrateEntry>,
    licenses: Vec<License>,
}

#[derive(Deserialize)]
struct Config {
    targets: Vec<String>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    checksum: Option<String>,
}

#[derive(Deserialize)]
struct Lockfile {
    package: Vec<LockedPackage>,
}

fn license_files(package: &Package) -> Result<Vec<(String, String)>> {
    let root = package
        .manifest_path
        .parent()
        .context("Missing manifest directory")?;
    let mut files = BTreeSet::new();
    for path in files_in(root)? {
        let relative = path.strip_prefix(root)?;
        let name = path
            .file_name()
            .context("Missing file name")?
            .to_string_lossy()
            .to_uppercase();
        let in_license_dir = relative.parent().is_some_and(|parent| {
            parent.iter().any(|part| {
                matches!(
                    part.to_string_lossy()
                        .trim_start_matches('.')
                        .to_uppercase()
                        .as_str(),
                    "LICENSES" | "LICENCES"
                )
            })
        });
        if LICENSE_NAMES.iter().any(|prefix| name.starts_with(prefix)) || in_license_dir {
            files.insert(path);
        }
    }
    if let Some(file) = &package.license_file {
        files.insert(root.join(file));
    }
    files
        .into_iter()
        .map(|path| {
            Ok((
                path.strip_prefix(root)?.to_string_lossy().into_owned(),
                fs::read_to_string(&path)
                    .with_context(|| format!("Cannot read {}", path.display()))?,
            ))
        })
        .collect()
}

fn repository_notices(package: &Package, cache: &Path) -> Result<Vec<(String, String)>> {
    let root = package
        .manifest_path
        .parent()
        .context("Missing manifest directory")?;
    let vcs: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".cargo_vcs_info.json"))?)?;
    let mut commit = vcs["git"]["sha1"]
        .as_str()
        .context("Missing source commit")?;
    let repository = package
        .repository
        .as_deref()
        .unwrap_or("")
        .trim_end_matches(".git");
    // Kube 4.0.0 records an unavailable commit. Its source matches this release tag.
    if repository == "https://github.com/kube-rs/kube"
        && commit == "7b4e520b3ef9d6aae8214577fe5f8f4f4ddb4fab"
    {
        commit = "b4f0cc4d7b4ce00ac7fa2d85c0120bbd38fa6210";
    }
    ensure!(
        regex::Regex::new(r"^https://github.com/[\w.-]+/[\w.-]+$")?.is_match(repository),
        "Review the missing notices for {}",
        package.name
    );
    ensure!(
        commit.len() == 40
            && commit
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "Invalid source commit"
    );
    let base = repository.replacen(
        "https://github.com/",
        "https://raw.githubusercontent.com/",
        1,
    );
    let mut notices = Vec::new();
    for name in [
        "LICENSE",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "NOTICE",
        "COPYRIGHT",
    ] {
        let url = format!("{base}/{commit}/{name}");
        if let Some(content) = download(&url, cache)? {
            notices.push((url, String::from_utf8(content)?));
        }
    }
    ensure!(
        !notices.is_empty(),
        "No source notices found for {}",
        package.name
    );
    Ok(notices)
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("Cannot start {command:?}"))?;
    ensure!(status.success(), "Command failed ({status}): {command:?}");
    Ok(())
}

fn generate(manifest: &Path, output: &Path, cargo_about: &Path, cache: &Path) -> Result<()> {
    let manifest = fs::canonicalize(manifest)?;
    let lock = manifest.with_file_name("Cargo.lock");
    let before = fs::read_to_string(&lock)?;
    let config: Config = toml::from_str(&fs::read_to_string(Path::new(ROOT).join("about.toml"))?)?;
    ensure!(
        !config.targets.is_empty(),
        "The release target list is empty"
    );
    run(Command::new("cargo")
        .args(["fetch", "--locked", "--manifest-path"])
        .arg(&manifest))?;
    let temporary = Temporary::new(cache)?;
    let report_path = temporary.0.join("licenses.json");
    let mut command = Command::new(cargo_about);
    command
        .args([
            "generate",
            "--locked",
            "--offline",
            "--fail",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(&manifest)
        .arg("--config")
        .arg(Path::new(ROOT).join("about.toml"))
        .arg("--output-file")
        .arg(&report_path);
    for target in &config.targets {
        command.args(["--target", target]);
    }
    run(&mut command)?;
    ensure!(
        fs::read_to_string(&lock)? == before,
        "License collection changed Cargo.lock"
    );
    let report = serde_json::from_slice(&fs::read(report_path)?)?;
    let lockfile = toml::from_str(&before)?;
    write_report(report, &manifest, output, cache, &config.targets, lockfile)
}

fn write_report(
    mut report: Report,
    manifest: &Path,
    output: &Path,
    cache: &Path,
    targets: &[String],
    lock: Lockfile,
) -> Result<()> {
    let mut packages = Vec::new();
    for entry in report.crates.drain(..) {
        if fs::canonicalize(&entry.package.manifest_path)? != manifest {
            packages.push(entry.package);
        }
    }
    packages.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    ensure!(
        !packages.is_empty() && !report.licenses.is_empty(),
        "The dependency license report is empty"
    );
    let mut selected: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for license in &report.licenses {
        ensure!(
            !license.text.trim().is_empty(),
            "An empty license was returned"
        );
        for user in &license.used_by {
            selected
                .entry(&user.krate.id)
                .or_default()
                .insert(&license.id);
        }
    }
    let checksums: BTreeMap<_, _> = lock
        .package
        .into_iter()
        .map(|p| ((p.name, p.version), p.checksum))
        .collect();
    let mut text = format!(
        "Third-party licenses for Sofka\n\nThis report covers the default release features.\nTargets: {}\nIt includes dependency license texts and notices from bundled source trees.\nSome notices can apply to code that is not included in a particular binary.\nThe source links identify the exact published dependency versions.\nMPL 2.0 source packages are also included in THIRD-PARTY-SOURCES/.\nThese source packages retain their original licenses.\n\n",
        targets.join(", ")
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = Temporary::new(parent)?;
    let valid_name = regex::Regex::new(r"^[A-Za-z0-9_.+-]+$")?;
    for package in &packages {
        let Package { name, version, .. } = package;
        ensure!(
            valid_name.is_match(name) && valid_name.is_match(version),
            "Invalid package name or version"
        );
        let licenses = selected
            .get(package.id.as_str())
            .with_context(|| format!("No selected license for {name} {version}"))?;
        ensure!(
            package.source.as_deref()
                == Some("registry+https://github.com/rust-lang/crates.io-index"),
            "Review source distribution for {name} {version}"
        );
        let source = format!("https://static.crates.io/crates/{name}/{name}-{version}.crate");
        text.push_str(&format!(
            "{name} {version}\nDeclared license: {}\nSelected licenses: {}\nSource: {source}\n\n",
            package.license.as_deref().unwrap_or("None"),
            licenses.iter().copied().collect::<Vec<_>>().join(", ")
        ));
        if licenses.contains("MPL-2.0") {
            let archive = download(&source, cache)?
                .with_context(|| format!("Source package not found: {source}"))?;
            let expected = checksums
                .get(&(name.clone(), version.clone()))
                .and_then(Option::as_ref);
            ensure!(
                expected == Some(&digest(&archive)),
                "Source checksum mismatch for {name} {version}"
            );
            let sources = staging.0.join("THIRD-PARTY-SOURCES");
            fs::create_dir_all(&sources)?;
            fs::write(sources.join(format!("{name}-{version}.crate")), archive)?;
        }
    }
    for license in &report.licenses {
        let users: BTreeSet<_> = license
            .used_by
            .iter()
            .map(|u| format!("{} {}", u.krate.name, u.krate.version))
            .collect();
        text.push_str(&format!(
            "{}\n{}\nUsed by: {}\n\n{}\n\n",
            "=".repeat(72),
            license.name,
            users.into_iter().collect::<Vec<_>>().join(", "),
            license.text
        ));
    }
    for package in &packages {
        let mut notices = license_files(package)?;
        if notices.is_empty() {
            notices = repository_notices(package, cache)?;
        }
        for (path, notice) in notices {
            ensure!(
                !notice.trim().is_empty(),
                "Empty notice: {}/{path}",
                package.name
            );
            text.push_str(&format!(
                "{}\n{} {}: {path}\n\n{notice}\n\n",
                "=".repeat(72),
                package.name,
                package.version
            ));
        }
    }
    fs::write(staging.0.join("THIRD-PARTY-LICENSES.txt"), text)?;
    for name in &REQUIRED[..2] {
        fs::copy(manifest.with_file_name(name), staging.0.join(name))?;
    }
    validate_notices(&staging.0)?;
    // Use a fresh directory so old source packages cannot enter a later release.
    ensure!(
        !output.exists() || fs::read_dir(output)?.next().is_none(),
        "The output directory must be empty: {}",
        output.display()
    );
    fs::create_dir_all(output)?;
    for entry in fs::read_dir(&staging.0)? {
        let entry = entry?;
        fs::rename(entry.path(), output.join(entry.file_name()))?;
    }
    Ok(())
}

fn read_regular(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    fs::read(path).with_context(|| format!("Cannot read {}", path.display()))
}

fn validate_notices(directory: &Path) -> Result<()> {
    for name in REQUIRED {
        ensure!(
            !read_regular(&directory.join(name))?.is_empty(),
            "Missing or empty release notice: {name}"
        );
    }
    Ok(())
}

fn package(binary: &Path, notices: &Path, output: &Path, rust_notices: &Path) -> Result<()> {
    validate_notices(notices)?;
    let original = read_regular(binary)?;
    ensure!(!original.is_empty(), "The release binary is empty");
    let runtime = read_regular(rust_notices)?;
    ensure!(!runtime.is_empty(), "The Rust library notice is empty");
    let mut contents = BTreeMap::from([
        (PathBuf::from("sofka"), original),
        (PathBuf::from("RUST-LICENSES.html"), runtime),
    ]);
    for path in files_in(notices)? {
        let name = path.strip_prefix(notices)?.to_path_buf();
        ensure!(
            !contents.contains_key(&name),
            "Duplicate archive name: {}",
            name.display()
        );
        contents.insert(name, read_regular(&path)?);
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = Temporary::new(parent)?;
    let archive_path = staging.0.join("archive.tar.gz");
    let compressed = GzBuilder::new()
        .mtime(0)
        .write(File::create(&archive_path)?, Compression::default());
    let mut archive = tar::Builder::new(compressed);
    for (name, bytes) in &contents {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(if name == Path::new("sofka") {
            0o755
        } else {
            0o644
        });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, name, bytes.as_slice())?;
    }
    archive.into_inner()?.finish()?.flush()?;
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(&archive_path)?));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        let expected = contents
            .remove(&name)
            .with_context(|| format!("Unexpected archive entry: {}", name.display()))?;
        let mut actual = Vec::new();
        entry.read_to_end(&mut actual)?;
        ensure!(
            actual == expected,
            "Archive content mismatch: {}",
            name.display()
        );
    }
    ensure!(contents.is_empty(), "Archive entries are missing");
    fs::rename(archive_path, output)?;
    Ok(())
}

fn main() -> Result<()> {
    match Args::parse().command {
        Action::Generate {
            manifest,
            output,
            cargo_about,
            cache,
        } => generate(&manifest, &output, &cargo_about, &cache),
        Action::Package {
            binary,
            notices,
            output,
            rust_notices,
        } => package(&binary, &notices, &output, &rust_notices),
    }
}

#[cfg(test)]
#[path = "release_licenses_tests.rs"]
mod tests;
