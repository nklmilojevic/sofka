//! Shared runtime-diagnostics helpers (`:info` / `--info`).
//!
//! These are the pieces both the in-app diagnostics view and the headless
//! `--info` report agree on: the version/build stamp and the directories sofka
//! reads and writes. Nothing here touches the cluster or emits credentials.

use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The build target as `os/arch` plus the compile profile.
pub fn build_line() -> String {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "{}/{} ({profile})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Base state directory: `$XDG_STATE_HOME/sofka`, else `~/.local/state/sofka`,
/// else a temp fallback. Snapshots and any future on-disk state live under it.
pub fn state_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_STATE_HOME")
        && !x.is_empty()
    {
        return PathBuf::from(x).join("sofka");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("sofka");
    }
    std::env::temp_dir().join("sofka")
}
