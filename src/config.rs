//! User configuration loaded from `$XDG_CONFIG_HOME/sofka/config.toml`
//! (falling back to `~/.config/sofka/config.toml`), with optional
//! per-cluster / per-context overrides, k9s-style:
//!
//! ```text
//! sofka/
//! ├── config.toml                  # base, applies everywhere
//! └── clusters/
//!     └── <cluster>/               # kubeconfig *cluster* name
//!         ├── config.toml          # every context on this cluster
//!         └── <context>/           # kubeconfig *context* name
//!             └── config.toml      # this context only
//! ```
//!
//! Override files are partial configs merged over the base (cluster level
//! first, then context level): tables like `[aliases]` and `[skin.colors]`
//! merge key-by-key, everything else — strings, booleans, arrays like
//! `[[plugins]]` — replaces the base value. Directory names are the
//! kubeconfig names sanitized for the filesystem: any character other than
//! ASCII letters, digits, `.`, `_` and `-` becomes `-`, so an EKS context
//! `arn:aws:eks:eu-west-1:123:cluster/prod` lives in
//! `arn-aws-eks-eu-west-1-123-cluster-prod/`.
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
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Namespace to start in when none is given on the CLI.
    pub default_namespace: Option<String>,
    /// Resource to open on launch when none is given on the CLI.
    pub default_resource: Option<String>,
    /// Disable every action that could modify the cluster (or run arbitrary
    /// commands): delete, edit, scale, restart, set-image, cordon/drain,
    /// Flux suspend/resume/reconcile, Helm rollback/uninstall, shell/attach,
    /// and plugins. Overridden by the `--readonly`/`--write` CLI flags. Set
    /// it in a per-cluster/per-context override file to lock down just prod.
    pub readonly: bool,
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
/// `#rrggbb` hex values. Naming a skin in a per-cluster/per-context override
/// file swaps it in while that context is active — e.g. a light skin on prod
/// as a visual "careful now" cue. `background` fills every view with the
/// skin's own background swatch instead of leaving the terminal background
/// showing through; combined with a light per-context skin it makes the prod
/// context unmistakably bright.
///
/// ```toml
/// [skin]
/// name = "gruvbox"
/// background = true       # paint the skin's background (default: false)
///
/// [skin.colors]
/// red   = "#fb4934"
/// mauve = "#d3869b"
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Skin {
    pub name: Option<String>,
    pub colors: HashMap<String, String>,
    /// Fill views with the skin's `base` background swatch (default: false,
    /// i.e. transparent — inherit the terminal background).
    pub background: bool,
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

/// Config resolved for one (cluster, context) pair: the base config with any
/// matching override files merged in.
pub struct Resolved {
    pub config: Config,
    /// `skin.name` when an override file (not the base config) set it. Wins
    /// over the session skin while the context is active, exactly so that a
    /// manual `:skin` choice still survives switches into contexts that don't
    /// pin their own skin.
    pub skin_override: Option<String>,
    /// Problems with override files (malformed TOML, type mismatches). The
    /// offending layer is skipped, never fatal.
    pub warnings: Vec<String>,
}

/// Holds the parsed base `config.toml` for the session and re-reads override
/// files on demand, so `:ctx` switches pick up freshly edited overrides
/// without a restart.
#[derive(Default)]
pub struct ConfigLoader {
    /// Base `config.toml` as a raw TOML table (`None` when missing/invalid).
    base: Option<toml::Value>,
    /// The `sofka` config directory containing `config.toml` and `clusters/`.
    dir: Option<PathBuf>,
}

impl ConfigLoader {
    /// Read the base config, warning on stderr (we're pre-TUI) and falling
    /// back to defaults if it's malformed — syntax or types.
    pub fn load() -> Self {
        let mut loader = Self::from_dir(config_dir());
        if let Some(dir) = &loader.dir
            && let Ok(text) = std::fs::read_to_string(dir.join("config.toml"))
            && let Err(e) = toml::from_str::<Config>(&text)
        {
            eprintln!(
                "warning: ignoring invalid {}: {e}",
                dir.join("config.toml").display()
            );
            loader.base = None;
        }
        loader
    }

    pub(crate) fn from_dir(dir: Option<PathBuf>) -> Self {
        let base = dir.as_ref().and_then(|d| {
            let text = std::fs::read_to_string(d.join("config.toml")).ok()?;
            parse_doc(&text).ok()
        });
        Self { base, dir }
    }

    /// Merge override files for the given kubeconfig cluster/context over the
    /// base config. Either name may be empty (e.g. in-cluster, no kubeconfig
    /// context) — its level is simply skipped.
    pub fn resolve(&self, context: &str, cluster: &str) -> Resolved {
        let mut warnings = Vec::new();
        let mut merged = self
            .base
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        let mut overlay = toml::Value::Table(toml::map::Map::new());

        if let (Some(dir), false) = (&self.dir, cluster.is_empty()) {
            let cluster_dir = dir.join("clusters").join(sanitize(cluster));
            let mut paths = vec![cluster_dir.join("config.toml")];
            if !context.is_empty() {
                paths.push(cluster_dir.join(sanitize(context)).join("config.toml"));
            }
            for path in paths {
                match read_value(&path) {
                    Ok(Some(v)) => {
                        merge(&mut merged, v.clone());
                        merge(&mut overlay, v);
                    }
                    Ok(None) => {}
                    Err(e) => warnings.push(format!("ignoring invalid {}: {e}", path.display())),
                }
            }
        }

        // A type mismatch introduced by an override drops back to the base
        // config (validated at load time) rather than losing everything.
        let config = merged.try_into().unwrap_or_else(|e| {
            warnings.push(format!("ignoring cluster overrides: {e}"));
            self.base
                .clone()
                .and_then(|b| b.try_into().ok())
                .unwrap_or_default()
        });
        let skin_override = overlay
            .get("skin")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from);
        Resolved {
            config,
            skin_override,
            warnings,
        }
    }
}

/// Recursively merge `overlay` into `base`: tables merge key-by-key, any
/// other value (scalar or array) replaces the base one wholesale.
fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) if slot.is_table() && v.is_table() => merge(slot, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// Parse an override file. Missing/unreadable -> `Ok(None)` (overrides are
/// optional); present but malformed -> `Err` so the caller can warn.
fn read_value(path: &Path) -> Result<Option<toml::Value>, toml::de::Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_doc(&text).map(Some),
        Err(_) => Ok(None),
    }
}

/// Parse a TOML *document* into a `Value::Table` (a bare `Value` parse would
/// expect a single TOML value, not a document).
fn parse_doc(text: &str) -> Result<toml::Value, toml::de::Error> {
    text.parse::<toml::Table>().map(toml::Value::Table)
}

/// Map a kubeconfig cluster/context name onto a safe directory name: any
/// character outside `[A-Za-z0-9._-]` becomes `-` (EKS ARNs contain `:` and
/// `/`). All-dot results (`.`, `..`) would be path navigation, not names.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.chars().all(|c| c == '.') {
        "-".repeat(s.len().max(1))
    } else {
        s
    }
}

fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sofka"))
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
    fn empty_config_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.aliases.is_empty());
        assert!(cfg.plugins.is_empty());
        assert!(cfg.default_resource.is_none());
    }

    fn val(s: &str) -> toml::Value {
        parse_doc(s).unwrap()
    }

    #[test]
    fn merge_tables_key_by_key_scalars_and_arrays_replace() {
        let mut base = val(r##"
            default_namespace = "default"
            default_resource = "pods"

            [aliases]
            dep = "deployments"

            [[plugins]]
            key = "g"
            name = "base-plugin"
            command = "true"

            [skin]
            name = "gruvbox-dark"
            [skin.colors]
            red = "#ff0000"
        "##);
        merge(
            &mut base,
            val(r##"
                default_namespace = "kube-system"

                [aliases]
                ks = "kustomizations"

                [[plugins]]
                key = "h"
                name = "override-plugin"
                command = "false"

                [skin]
                name = "catppuccin-latte"
                [skin.colors]
                blue = "#0000ff"
            "##),
        );
        let cfg: Config = base.try_into().unwrap();
        // scalars replace
        assert_eq!(cfg.default_namespace.as_deref(), Some("kube-system"));
        assert_eq!(cfg.skin.name.as_deref(), Some("catppuccin-latte"));
        // untouched base values survive
        assert_eq!(cfg.default_resource.as_deref(), Some("pods"));
        // tables merge
        assert_eq!(cfg.aliases.len(), 2);
        assert_eq!(cfg.skin.colors.len(), 2);
        // arrays replace wholesale
        assert_eq!(cfg.plugins.len(), 1);
        assert_eq!(cfg.plugins[0].name, "override-plugin");
    }

    #[test]
    fn sanitize_maps_unsafe_chars_to_dashes() {
        assert_eq!(sanitize("prod-cluster_1.io"), "prod-cluster_1.io");
        assert_eq!(
            sanitize("arn:aws:eks:eu-west-1:123:cluster/prod"),
            "arn-aws-eks-eu-west-1-123-cluster-prod"
        );
        assert_eq!(sanitize(".."), "--");
        assert_eq!(sanitize(""), "-");
    }

    /// End-to-end: base + cluster override + context override on disk.
    #[test]
    fn resolves_cluster_and_context_overrides() {
        let dir = std::env::temp_dir().join(format!("sofka-cfg-test-{}", std::process::id()));
        let ctx_dir = dir.join("clusters").join("prod-cluster").join("prod-ctx");
        std::fs::create_dir_all(&ctx_dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "default_namespace = \"default\"\n[aliases]\ndep = \"deployments\"\n[skin]\nname = \"gruvbox-dark\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("clusters")
                .join("prod-cluster")
                .join("config.toml"),
            "readonly = true\n[skin]\nname = \"catppuccin-latte\"\nbackground = true\n",
        )
        .unwrap();
        std::fs::write(
            ctx_dir.join("config.toml"),
            "default_namespace = \"prod\"\n[aliases]\nks = \"kustomizations\"\n",
        )
        .unwrap();

        let loader = ConfigLoader::from_dir(Some(dir.clone()));

        // Unknown context/cluster: base only, no skin override.
        let plain = loader.resolve("dev-ctx", "dev-cluster");
        assert!(plain.warnings.is_empty());
        assert_eq!(plain.config.default_namespace.as_deref(), Some("default"));
        assert_eq!(plain.config.skin.name.as_deref(), Some("gruvbox-dark"));
        assert_eq!(plain.skin_override, None);
        assert!(!plain.config.readonly);

        // Cluster + context overrides stack over the base.
        let prod = loader.resolve("prod-ctx", "prod-cluster");
        assert!(prod.warnings.is_empty());
        assert_eq!(prod.config.default_namespace.as_deref(), Some("prod"));
        assert!(prod.config.skin.background);
        assert_eq!(prod.config.aliases.len(), 2);
        assert_eq!(prod.skin_override.as_deref(), Some("catppuccin-latte"));
        assert!(prod.config.readonly);

        // Empty cluster name (in-cluster): overrides skipped entirely.
        let bare = loader.resolve("prod-ctx", "");
        assert_eq!(bare.config.default_namespace.as_deref(), Some("default"));
        assert_eq!(bare.skin_override, None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_override_warns_and_is_skipped() {
        let dir = std::env::temp_dir().join(format!("sofka-cfg-bad-test-{}", std::process::id()));
        let cluster_dir = dir.join("clusters").join("c1");
        std::fs::create_dir_all(&cluster_dir).unwrap();
        std::fs::write(dir.join("config.toml"), "default_namespace = \"base\"\n").unwrap();
        std::fs::write(cluster_dir.join("config.toml"), "not valid toml [[[").unwrap();

        let loader = ConfigLoader::from_dir(Some(dir.clone()));
        let r = loader.resolve("ctx", "c1");
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.config.default_namespace.as_deref(), Some("base"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
