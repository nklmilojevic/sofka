//! User configuration loaded from `$XDG_CONFIG_HOME/sofka/config.toml`
//! (falling back to `~/.config/sofka/config.toml`).
//!
//! Example:
//! ```toml
//! default_namespace = "kube-system"
//! default_resource  = "deployments"
//!
//! [aliases]
//! dep = "deployments"
//! ti  = "deployments"   # whatever shortcuts you like
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Namespace to start in when none is given on the CLI.
    pub default_namespace: Option<String>,
    /// Resource to open on launch when none is given on the CLI.
    pub default_resource: Option<String>,
    /// Custom alias -> canonical resource (plural/kind) mappings.
    pub aliases: HashMap<String, String>,
    /// User-defined shell-out plugins bound to keys.
    pub plugins: Vec<Plugin>,
    /// Color skin: a built-in palette name plus optional per-swatch overrides.
    pub skin: Skin,
}

/// Skin selection. `name` picks a built-in palette (see
/// [`crate::theme::BUILTIN_NAMES`]); leaving it unset auto-detects the
/// terminal's dark/light mode and picks `catppuccin-mocha`/`catppuccin-latte`
/// accordingly. `colors` overrides individual swatches by name with
/// `#rrggbb` hex values. `contexts` maps a kubeconfig context name to a skin
/// that overrides `name` while that context is active — e.g. a light skin on
/// prod as a visual "careful now" cue.
///
/// ```toml
/// [skin]
/// name = "gruvbox"
///
/// [skin.colors]
/// red   = "#fb4934"
/// mauve = "#d3869b"
///
/// [skin.contexts]
/// prod = "catppuccin-latte"
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Skin {
    pub name: Option<String>,
    pub colors: HashMap<String, String>,
    pub contexts: HashMap<String, String>,
}

/// A shell-out plugin (k9s-style). Bound to `key` on resources matching
/// `scopes` (plural names; empty = all). Placeholders `$NAME`, `$NAMESPACE`,
/// `$NS`, `$CONTEXT`, `$RESOURCE` are substituted in `command`/`args`.
///
/// ```toml
/// [[plugins]]
/// key = "g"
/// name = "argocd-sync"
/// command = "argocd"
/// args = ["app", "sync", "$NAME"]
/// scopes = ["deployments"]
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct Plugin {
    pub key: char,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl Config {
    /// Load config, returning defaults if the file is missing or malformed
    /// (a warning is printed for malformed files).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("warning: ignoring invalid {}: {e}", path.display());
                Self::default()
            }
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sofka").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let toml = r#"
            default_namespace = "kube-system"
            default_resource  = "deployments"

            [aliases]
            dep = "deployments"

            [[plugins]]
            key = "g"
            name = "argocd-sync"
            command = "argocd"
            args = ["app", "sync", "$NAME"]
            scopes = ["deployments"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.default_namespace.as_deref(), Some("kube-system"));
        assert_eq!(
            cfg.aliases.get("dep").map(String::as_str),
            Some("deployments")
        );
        assert_eq!(cfg.plugins.len(), 1);
        let p = &cfg.plugins[0];
        assert_eq!(p.key, 'g');
        assert_eq!(p.args, vec!["app", "sync", "$NAME"]);
        assert_eq!(p.scopes, vec!["deployments"]);
    }

    #[test]
    fn parses_per_context_skins() {
        let toml = r#"
            [skin]
            name = "gruvbox-dark"

            [skin.contexts]
            prod = "catppuccin-latte"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.skin.name.as_deref(), Some("gruvbox-dark"));
        assert_eq!(
            cfg.skin.contexts.get("prod").map(String::as_str),
            Some("catppuccin-latte")
        );
    }

    #[test]
    fn empty_config_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.aliases.is_empty());
        assert!(cfg.plugins.is_empty());
        assert!(cfg.default_resource.is_none());
    }
}
