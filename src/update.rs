//! Release update checks: is a newer sofka published, and how would this
//! install upgrade to it.
//!
//! sofka never downloads or replaces itself. Homebrew, Nix, Cargo, winget, and
//! the distro packages each track the installed binary, so the check only
//! reports the latest release and the upgrade command for the install method
//! the running executable came from. The background check runs at most once
//! per [`CHECK_INTERVAL`]; the result is cached under the state directory so
//! every session in between reads it without touching the network.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const LATEST_URL: &str = "https://api.github.com/repos/nklmilojevic/sofka/releases/latest";
pub const RELEASES_URL: &str = "https://github.com/nklmilojevic/sofka/releases";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const RESPONSE_LIMIT: usize = 1024 * 1024;

/// The latest published release, as GitHub reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Version without the leading `v`, e.g. `0.32.0`.
    pub version: String,
    /// The release notes page.
    pub url: String,
}

impl Release {
    /// Whether this release is newer than the running build.
    pub fn is_newer(&self) -> bool {
        is_newer(crate::diagnostics::VERSION, &self.version)
    }
}

/// The last network attempt and the last release any attempt found. A failed
/// attempt still updates `checked_at`, so an offline machine or a rate limit
/// does not turn every startup into another request.
#[derive(Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    release: Option<Release>,
}

/// Where the last check result lives: `<state-dir>/update-check.toml`.
pub fn cache_path() -> PathBuf {
    crate::diagnostics::state_dir().join("update-check.toml")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn read_cache(path: &Path) -> Option<Cache> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
}

/// The last release any check found, however old. `None` when no check has
/// succeeded yet or the file is unreadable.
pub fn cached(path: &Path) -> Option<Release> {
    read_cache(path).and_then(|cache| cache.release)
}

/// Whether the last attempt was within [`CHECK_INTERVAL`] of `now`. A
/// timestamp in the future (a clock that moved back) counts as stale.
fn is_fresh(cache: &Cache, now: u64) -> bool {
    now.checked_sub(cache.checked_at)
        .is_some_and(|age| age < CHECK_INTERVAL.as_secs())
}

fn store(path: &Path, now: u64, release: Option<&Release>) -> Result<(), String> {
    let cache = Cache {
        checked_at: now,
        release: release.cloned(),
    };
    let text = toml::to_string(&cache).map_err(|e| e.to_string())?;
    crate::atomicfile::write(path, &text)
}

/// `latest` > `current` as semantic versions. Anything that does not parse is never newer,
/// so a malformed tag cannot produce a notification.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(latest),
    ) {
        (Ok(current), Ok(latest)) => latest > current,
        _ => false,
    }
}

/// Read `tag_name` and `html_url` from a GitHub release response.
pub fn parse_release(bytes: &[u8]) -> Result<Release, String> {
    #[derive(Deserialize)]
    struct Response {
        tag_name: String,
        html_url: String,
    }
    let response: Response =
        serde_json::from_slice(bytes).map_err(|e| format!("reading release response: {e}"))?;
    let version = response
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&response.tag_name)
        .to_string();
    semver::Version::parse(&version)
        .map_err(|e| format!("release tag {:?} is not a version: {e}", response.tag_name))?;
    let url = if response.html_url.starts_with("https://github.com/") {
        response.html_url
    } else {
        format!("{RELEASES_URL}/tag/{}", response.tag_name)
    };
    Ok(Release { version, url })
}

/// The latest release: from the cache when it is fresh and `force` is false,
/// otherwise from GitHub, refreshing the cache.
pub async fn check(force: bool) -> Result<Release, String> {
    check_at(&cache_path(), now(), force, || {
        crate::plugin_catalog::get(LATEST_URL, RESPONSE_LIMIT)
    })
    .await
}

/// [`check`] against an explicit cache file, clock, and fetch. Within a day
/// of the last attempt an unforced check answers from the cache, and fails
/// without a request when that attempt found nothing.
async fn check_at<F, Fut>(path: &Path, now: u64, force: bool, fetch: F) -> Result<Release, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let previous = read_cache(path);
    if !force && let Some(cache) = previous.as_ref().filter(|cache| is_fresh(cache, now)) {
        return cache
            .release
            .clone()
            .ok_or_else(|| "the last update check failed; retrying within a day".to_string());
    }
    let result = fetch().await.and_then(|bytes| parse_release(&bytes));
    // Read again after a failure: a `:check-update` or another session may
    // have saved a release while this request was in flight.
    let known = match &result {
        Ok(release) => Some(release.clone()),
        Err(_) => read_cache(path)
            .and_then(|cache| cache.release)
            .or_else(|| previous.and_then(|cache| cache.release)),
    };
    if let Err(error) = store(path, now, known.as_ref()) {
        crate::log_warn!("update.cache", error = error);
    }
    result
}

/// How the running executable was installed, judged from its resolved path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Nix,
    Cargo,
    Winget,
    Package,
    Unknown,
}

impl InstallMethod {
    /// The running executable's install method.
    pub fn current() -> Self {
        let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
        std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_or(Self::Unknown, |exe| {
                Self::detect(&exe, cargo_home.as_deref())
            })
    }

    pub fn detect(exe: &Path, cargo_home: Option<&Path>) -> Self {
        let path = exe.to_string_lossy().replace('\\', "/");
        let lower = path.to_ascii_lowercase();
        if path.starts_with("/nix/store/") {
            Self::Nix
        } else if path.contains("/Cellar/") || path.contains("/linuxbrew/") {
            Self::Homebrew
        } else if lower.contains("/microsoft/winget/") {
            Self::Winget
        } else if cargo_home.is_some_and(|home| exe.starts_with(home.join("bin")))
            || path.contains("/.cargo/bin/")
        {
            Self::Cargo
        } else if path == "/usr/bin/sofka" {
            Self::Package
        } else {
            Self::Unknown
        }
    }

    /// What to run (or do) to upgrade to `release`.
    pub fn upgrade_hint(self, release: &Release) -> String {
        match self {
            Self::Homebrew => "brew upgrade sofka".into(),
            Self::Nix => "update the sofka flake input or Nix profile".into(),
            Self::Cargo => "cargo install sofka --locked".into(),
            Self::Winget => "winget upgrade nklmilojevic.sofka".into(),
            Self::Package | Self::Unknown => format!("download it from {}", release.url),
        }
    }
}

/// One-line notice for the status bar.
pub fn notice(release: &Release, method: InstallMethod) -> String {
    format!(
        "sofka v{} is available (running v{}): {}",
        release.version,
        crate::diagnostics::VERSION,
        method.upgrade_hint(release)
    )
}

/// The `Updates` section of `sofka info` and `:info`.
pub fn report_lines(latest: Option<&Release>, enabled: bool, method: InstallMethod) -> Vec<String> {
    let mut lines = vec!["Updates".to_string()];
    match latest {
        Some(release) if release.is_newer() => {
            lines.push(format!("  latest:   v{} (newer)", release.version));
            lines.push(format!("  notes:    {}", release.url));
            lines.push(format!("  upgrade:  {}", method.upgrade_hint(release)));
        }
        Some(release) => {
            lines.push(format!("  latest:   v{} (up to date)", release.version));
            lines.push(format!("  notes:    {}", release.url));
        }
        None => lines.push("  latest:   not checked".into()),
    }
    lines.push(format!(
        "  checks:   {}",
        if enabled {
            "daily (update_check = true)"
        } else {
            "off (update_check = false)"
        }
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sofka-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn release(version: &str) -> Release {
        Release {
            version: version.into(),
            url: format!("{RELEASES_URL}/tag/v{version}"),
        }
    }

    #[test]
    fn newer_compares_semantic_versions() {
        assert!(is_newer("0.31.1", "0.32.0"));
        assert!(is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.31.1", "0.31.1"));
        assert!(!is_newer("0.32.0", "0.31.1"));
        assert!(!is_newer("0.31.1", "not-a-version"));
        assert!(is_newer("0.32.0-rc.1", "0.32.0"));
    }

    #[test]
    fn parse_release_strips_the_tag_prefix() {
        let body = br#"{"tag_name":"v0.32.0","html_url":"https://github.com/nklmilojevic/sofka/releases/tag/v0.32.0","assets":[]}"#;
        assert_eq!(
            parse_release(body).unwrap(),
            Release {
                version: "0.32.0".into(),
                url: "https://github.com/nklmilojevic/sofka/releases/tag/v0.32.0".into(),
            }
        );
    }

    #[test]
    fn parse_release_rejects_a_tag_that_is_not_a_version() {
        let body = br#"{"tag_name":"nightly","html_url":"https://github.com/x"}"#;
        assert!(parse_release(body).unwrap_err().contains("nightly"));
        assert!(parse_release(b"{}").is_err());
    }

    #[test]
    fn parse_release_ignores_a_notes_link_off_github() {
        let body = br#"{"tag_name":"v1.0.0","html_url":"https://example.com/evil"}"#;
        assert_eq!(
            parse_release(body).unwrap().url,
            format!("{RELEASES_URL}/tag/v1.0.0")
        );
    }

    fn body(version: &str) -> Vec<u8> {
        format!(r#"{{"tag_name":"v{version}","html_url":"{RELEASES_URL}/tag/v{version}"}}"#)
            .into_bytes()
    }

    #[tokio::test]
    async fn unforced_checks_reach_the_network_once_a_day() {
        let dir = scratch("fresh");
        let path = dir.join("update-check.toml");
        let ok = |version: &'static str| move || async move { Ok(body(version)) };
        let unreachable = || async { panic!("a fresh cache must not fetch") };

        assert_eq!(
            check_at(&path, 1_000, false, ok("1.2.3")).await,
            Ok(release("1.2.3"))
        );
        assert_eq!(
            check_at(&path, 1_000 + 86_399, false, unreachable).await,
            Ok(release("1.2.3"))
        );
        assert_eq!(
            check_at(&path, 1_000 + 86_400, false, ok("1.3.0")).await,
            Ok(release("1.3.0"))
        );
        assert_eq!(
            check_at(&path, 500, false, ok("1.3.1")).await,
            Ok(release("1.3.1")),
            "a clock that moved back is stale"
        );
        assert_eq!(
            check_at(&path, 501, true, ok("1.4.0")).await,
            Ok(release("1.4.0")),
            "a forced check ignores the cache"
        );
        assert_eq!(cached(&path), Some(release("1.4.0")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failure_keeps_a_release_saved_while_it_was_in_flight() {
        let dir = scratch("overlap");
        let path = dir.join("update-check.toml");
        let concurrent = path.clone();
        let fail_after_another_check = move || async move {
            store(&concurrent, 1_500, Some(&release("1.2.3"))).unwrap();
            Err("timed out".to_string())
        };
        assert!(
            check_at(&path, 1_000, false, fail_after_another_check)
                .await
                .is_err()
        );
        assert_eq!(cached(&path), Some(release("1.2.3")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_check_still_waits_a_day_and_keeps_the_known_release() {
        let dir = scratch("failed");
        let path = dir.join("update-check.toml");
        let fail = || async { Err("rate limited".to_string()) };
        let unreachable = || async { panic!("a failed attempt must throttle the next one") };

        assert_eq!(
            check_at(&path, 1_000, false, fail).await,
            Err("rate limited".into())
        );
        assert!(
            check_at(&path, 2_000, false, unreachable)
                .await
                .unwrap_err()
                .contains("retrying within a day")
        );
        assert_eq!(cached(&path), None);

        let ok = || async { Ok(body("1.2.3")) };
        assert_eq!(check_at(&path, 3_000, true, ok).await, Ok(release("1.2.3")));
        assert!(check_at(&path, 3_000 + 86_400, false, fail).await.is_err());
        assert_eq!(
            cached(&path),
            Some(release("1.2.3")),
            "a failure keeps the last release"
        );
        assert_eq!(
            check_at(&path, 3_000 + 86_401, false, unreachable).await,
            Ok(release("1.2.3"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_cache_is_no_cache() {
        let dir = scratch("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update-check.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        assert_eq!(cached(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_method_follows_the_executable_path() {
        let detect = |path: &str| InstallMethod::detect(Path::new(path), None);
        assert_eq!(
            detect("/opt/homebrew/Cellar/sofka/0.31.1/bin/sofka"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            detect("/home/linuxbrew/.linuxbrew/bin/sofka"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            detect("/nix/store/abc-sofka-0.31.1/bin/sofka"),
            InstallMethod::Nix
        );
        assert_eq!(detect("/home/me/.cargo/bin/sofka"), InstallMethod::Cargo);
        assert_eq!(
            detect(
                r"C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\nklmilojevic.sofka\sofka.exe"
            ),
            InstallMethod::Winget
        );
        assert_eq!(detect("/usr/bin/sofka"), InstallMethod::Package);
        assert_eq!(detect("/usr/local/bin/sofka"), InstallMethod::Unknown);
        assert_eq!(
            InstallMethod::detect(
                Path::new("/opt/cargo/bin/sofka"),
                Some(Path::new("/opt/cargo"))
            ),
            InstallMethod::Cargo
        );
    }

    #[test]
    fn unknown_installs_link_the_release() {
        assert_eq!(
            InstallMethod::Unknown.upgrade_hint(&release("9.0.0")),
            format!("download it from {RELEASES_URL}/tag/v9.0.0")
        );
    }

    #[test]
    fn report_shows_the_upgrade_only_for_a_newer_release() {
        let newer = report_lines(Some(&release("999.0.0")), true, InstallMethod::Homebrew);
        assert!(newer.contains(&"  latest:   v999.0.0 (newer)".to_string()));
        assert!(newer.contains(&"  upgrade:  brew upgrade sofka".to_string()));
        let current = report_lines(
            Some(&release(crate::diagnostics::VERSION)),
            false,
            InstallMethod::Homebrew,
        );
        assert!(current.iter().any(|line| line.contains("(up to date)")));
        assert!(current.contains(&format!(
            "  notes:    {RELEASES_URL}/tag/v{}",
            crate::diagnostics::VERSION
        )));
        assert!(!current.iter().any(|line| line.contains("upgrade:")));
        assert!(current.contains(&"  checks:   off (update_check = false)".to_string()));
        let none = report_lines(None, true, InstallMethod::Unknown);
        assert!(none.contains(&"  latest:   not checked".to_string()));
    }
}
