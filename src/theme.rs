//! Data-driven palette + semantic styles for the TUI.
//!
//! The palette is a 25-swatch [`Palette`] (Catppuccin's slot layout). A named
//! built-in skin — default `catppuccin-mocha` — is selected in config, with
//! optional per-swatch hex overrides:
//!
//! ```toml
//! [skin]
//! name = "gruvbox"
//!
//! [skin.colors]        # optional; keys are swatch names (see `Palette` fields)
//! red   = "#fb4934"
//! mauve = "#d3869b"
//! ```
//!
//! [`init`] installs the resolved palette once at startup; every color accessor
//! (`text()`, `peach()`, …) and semantic style (`title()`, `selected_row()`, …)
//! reads from it. Swatches not yet wired into a view are kept for completeness.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

/// A full 25-swatch palette. Field names double as the override keys accepted
/// under `[skin.colors]` in config.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub rosewater: Color,
    pub flamingo: Color,
    pub pink: Color,
    pub mauve: Color,
    pub red: Color,
    pub maroon: Color,
    pub peach: Color,
    pub yellow: Color,
    pub green: Color,
    pub teal: Color,
    pub sky: Color,
    pub sapphire: Color,
    pub blue: Color,
    pub lavender: Color,
    pub text: Color,
    pub subtext1: Color,
    pub subtext0: Color,
    pub overlay1: Color,
    pub overlay0: Color,
    pub surface2: Color,
    pub surface1: Color,
    pub surface0: Color,
    pub base: Color,
    pub mantle: Color,
    pub crust: Color,
}

/// Swatch names in the order the built-in tables list their hex values.
const FIELDS: [&str; 25] = [
    "rosewater",
    "flamingo",
    "pink",
    "mauve",
    "red",
    "maroon",
    "peach",
    "yellow",
    "green",
    "teal",
    "sky",
    "sapphire",
    "blue",
    "lavender",
    "text",
    "subtext1",
    "subtext0",
    "overlay1",
    "overlay0",
    "surface2",
    "surface1",
    "surface0",
    "base",
    "mantle",
    "crust",
];

impl Palette {
    /// Build a palette from 25 hex strings in [`FIELDS`] order. Panics on a
    /// malformed value — only ever called with the hardcoded built-in tables,
    /// so a bad hex here is a programmer error, not user input.
    fn from_hexes(h: &[&str; 25]) -> Palette {
        let c = |i: usize| parse_hex(h[i]).unwrap_or_else(|| panic!("bad built-in hex {}", h[i]));
        Palette {
            rosewater: c(0),
            flamingo: c(1),
            pink: c(2),
            mauve: c(3),
            red: c(4),
            maroon: c(5),
            peach: c(6),
            yellow: c(7),
            green: c(8),
            teal: c(9),
            sky: c(10),
            sapphire: c(11),
            blue: c(12),
            lavender: c(13),
            text: c(14),
            subtext1: c(15),
            subtext0: c(16),
            overlay1: c(17),
            overlay0: c(18),
            surface2: c(19),
            surface1: c(20),
            surface0: c(21),
            base: c(22),
            mantle: c(23),
            crust: c(24),
        }
    }

    /// Override one swatch by name. Returns `false` for an unknown key.
    fn set(&mut self, key: &str, color: Color) -> bool {
        match key {
            "rosewater" => self.rosewater = color,
            "flamingo" => self.flamingo = color,
            "pink" => self.pink = color,
            "mauve" => self.mauve = color,
            "red" => self.red = color,
            "maroon" => self.maroon = color,
            "peach" => self.peach = color,
            "yellow" => self.yellow = color,
            "green" => self.green = color,
            "teal" => self.teal = color,
            "sky" => self.sky = color,
            "sapphire" => self.sapphire = color,
            "blue" => self.blue = color,
            "lavender" => self.lavender = color,
            "text" => self.text = color,
            "subtext1" => self.subtext1 = color,
            "subtext0" => self.subtext0 = color,
            "overlay1" => self.overlay1 = color,
            "overlay0" => self.overlay0 = color,
            "surface2" => self.surface2 = color,
            "surface1" => self.surface1 = color,
            "surface0" => self.surface0 = color,
            "base" => self.base = color,
            "mantle" => self.mantle = color,
            "crust" => self.crust = color,
            _ => return false,
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Built-in skins
// ---------------------------------------------------------------------------

/// Names accepted by [`builtin`] (canonical spellings), for help/error text.
pub const BUILTIN_NAMES: &[&str] = &[
    "catppuccin-mocha",
    "catppuccin-latte",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "gruvbox",
    "nord",
    "dracula",
];

/// Look up a built-in palette by name (case-insensitive; a few aliases). The
/// short Catppuccin flavor names (`mocha`, `latte`, …) are accepted too.
pub fn builtin(name: &str) -> Option<Palette> {
    // FIELDS order: rosewater, flamingo, pink, mauve, red, maroon, peach,
    // yellow, green, teal, sky, sapphire, blue, lavender, text, subtext1,
    // subtext0, overlay1, overlay0, surface2, surface1, surface0, base, mantle, crust
    let hex: [&str; 25] = match name {
        "catppuccin-mocha" | "mocha" => [
            "#f5e0dc", "#f2cdcd", "#f5c2e7", "#cba6f7", "#f38ba8", "#eba0ac", "#fab387", "#f9e2af",
            "#a6e3a1", "#94e2d5", "#89dceb", "#74c7ec", "#89b4fa", "#b4befe", "#cdd6f4", "#bac2de",
            "#a6adc8", "#7f849c", "#6c7086", "#585b70", "#45475a", "#313244", "#1e1e2e", "#181825",
            "#11111b",
        ],
        "catppuccin-latte" | "latte" => [
            "#dc8a78", "#dd7878", "#ea76cb", "#8839ef", "#d20f39", "#e64553", "#fe640b", "#df8e1d",
            "#40a02b", "#179299", "#04a5e5", "#209fb5", "#1e66f5", "#7287fd", "#4c4f69", "#5c5f77",
            "#6c6f85", "#8c8fa1", "#9ca0b0", "#acb0be", "#bcc0cc", "#ccd0da", "#eff1f5", "#e6e9ef",
            "#dce0e8",
        ],
        "catppuccin-frappe" | "frappe" => [
            "#f2d5cf", "#eebebe", "#f4b8e4", "#ca9ee6", "#e78284", "#ea999c", "#ef9f76", "#e5c890",
            "#a6d189", "#81c8be", "#99d1db", "#85c1dc", "#8caaee", "#babbf1", "#c6d0f5", "#b5bfe2",
            "#a5adce", "#949cbb", "#838ba7", "#626880", "#51576d", "#414559", "#303446", "#292c3c",
            "#232634",
        ],
        "catppuccin-macchiato" | "macchiato" => [
            "#f4dbd6", "#f0c6c6", "#f5bde6", "#c6a0f6", "#ed8796", "#ee99a0", "#f5a97f", "#eed49f",
            "#a6da95", "#8bd5ca", "#91d7e3", "#7dc4e4", "#8aadf4", "#b7bdf8", "#cad3f5", "#b8c0e0",
            "#a5adcb", "#8087a2", "#6e738d", "#5b6078", "#494d64", "#363a4f", "#24273a", "#1e2030",
            "#181926",
        ],
        "gruvbox" | "gruvbox-dark" => [
            "#ebdbb2", "#d5c4a1", "#d3869b", "#d3869b", "#fb4934", "#cc241d", "#fe8019", "#fabd2f",
            "#b8bb26", "#8ec07c", "#83a598", "#458588", "#83a598", "#d3869b", "#ebdbb2", "#d5c4a1",
            "#bdae93", "#a89984", "#928374", "#665c54", "#504945", "#3c3836", "#282828", "#1d2021",
            "#1d2021",
        ],
        "nord" => [
            "#d8dee9", "#e5e9f0", "#b48ead", "#b48ead", "#bf616a", "#bf616a", "#d08770", "#ebcb8b",
            "#a3be8c", "#8fbcbb", "#88c0d0", "#81a1c1", "#5e81ac", "#b48ead", "#eceff4", "#e5e9f0",
            "#d8dee9", "#616e88", "#4c566a", "#434c5e", "#3b4252", "#333a47", "#2e3440", "#2b303b",
            "#242933",
        ],
        "dracula" => [
            "#f8f8f2", "#ffb86c", "#ff79c6", "#bd93f9", "#ff5555", "#ff5555", "#ffb86c", "#f1fa8c",
            "#50fa7b", "#8be9fd", "#8be9fd", "#62d6e8", "#6272a4", "#bd93f9", "#f8f8f2", "#d8d8d2",
            "#b8b8b2", "#6272a4", "#565761", "#44475a", "#3a3c4e", "#343746", "#282a36", "#21222c",
            "#191a21",
        ],
        _ => return None,
    };
    Some(Palette::from_hexes(&hex))
}

fn catppuccin_mocha() -> Palette {
    builtin("catppuccin-mocha").expect("mocha built-in")
}

/// Resolve the active palette from config: start from the named built-in
/// (default `catppuccin-mocha`), then apply per-swatch hex overrides. Warns on
/// an unknown name, unknown swatch key, or malformed hex, and carries on.
pub fn resolve_skin(name: Option<&str>, colors: &HashMap<String, String>) -> Palette {
    let mut p = match name {
        None => catppuccin_mocha(),
        Some(n) => match builtin(&n.trim().to_ascii_lowercase()) {
            Some(p) => p,
            None => {
                eprintln!(
                    "warning: unknown skin '{n}' (known: {}); using catppuccin-mocha",
                    BUILTIN_NAMES.join(", ")
                );
                catppuccin_mocha()
            }
        },
    };
    for (k, v) in colors {
        let key = k.trim().to_ascii_lowercase();
        match parse_hex(v) {
            Some(c) if p.set(&key, c) => {}
            Some(_) => eprintln!("warning: unknown skin color '{k}' (ignored)"),
            None => eprintln!("warning: invalid hex '{v}' for skin color '{k}' (ignored)"),
        }
    }
    p
}

/// Parse `#rrggbb` (or `rrggbb`) into a `Color::Rgb`. Returns `None` for any
/// other length or non-hex content.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

// ---------------------------------------------------------------------------
// Active palette + accessors
// ---------------------------------------------------------------------------

static ACTIVE: OnceLock<Palette> = OnceLock::new();

/// Install the active palette. Call once at startup, before rendering; later
/// calls are ignored. If never called, accessors fall back to Catppuccin Mocha.
pub fn init(p: Palette) {
    let _ = ACTIVE.set(p);
}

fn palette() -> &'static Palette {
    ACTIVE.get_or_init(catppuccin_mocha)
}

pub fn rosewater() -> Color {
    palette().rosewater
}
pub fn flamingo() -> Color {
    palette().flamingo
}
pub fn pink() -> Color {
    palette().pink
}
pub fn mauve() -> Color {
    palette().mauve
}
pub fn red() -> Color {
    palette().red
}
pub fn maroon() -> Color {
    palette().maroon
}
pub fn peach() -> Color {
    palette().peach
}
pub fn yellow() -> Color {
    palette().yellow
}
pub fn green() -> Color {
    palette().green
}
pub fn teal() -> Color {
    palette().teal
}
pub fn sky() -> Color {
    palette().sky
}
pub fn sapphire() -> Color {
    palette().sapphire
}
pub fn blue() -> Color {
    palette().blue
}
pub fn lavender() -> Color {
    palette().lavender
}
pub fn text() -> Color {
    palette().text
}
pub fn subtext1() -> Color {
    palette().subtext1
}
pub fn subtext0() -> Color {
    palette().subtext0
}
pub fn overlay1() -> Color {
    palette().overlay1
}
pub fn overlay0() -> Color {
    palette().overlay0
}
pub fn surface2() -> Color {
    palette().surface2
}
pub fn surface1() -> Color {
    palette().surface1
}
pub fn surface0() -> Color {
    palette().surface0
}
pub fn base() -> Color {
    palette().base
}
pub fn mantle() -> Color {
    palette().mantle
}
pub fn crust() -> Color {
    palette().crust
}

// ---------------------------------------------------------------------------
// Semantic styles
// ---------------------------------------------------------------------------

// The frame styles below mirror k9s' catppuccin skin element-for-element
// (`frame.title`, `frame.border`, `views.table.*`), so sofka's chrome matches.

/// Border-title text: k9s `frame.title.fgColor` (teal), bold.
pub fn title() -> Style {
    Style::default().fg(teal()).add_modifier(Modifier::BOLD)
}

/// Unfocused border: k9s `frame.border.fgColor` (mauve).
pub fn border() -> Style {
    Style::default().fg(mauve())
}

/// Focused border: k9s `frame.border.focusColor` (lavender).
pub fn border_focused() -> Style {
    Style::default().fg(lavender())
}

/// Table header: k9s `views.table.header.fgColor` (yellow), rendered non-bold.
pub fn header_row() -> Style {
    Style::default().fg(yellow())
}

/// Cursor / selected row: a bright lavender bar with dark text, bold. Using
/// `lavender` (light in dark themes, mid-tone in light themes) as the bg and
/// `base` as the fg keeps the cursor high-contrast under every skin.
pub fn selected_row() -> Style {
    Style::default()
        .bg(lavender())
        .fg(base())
        .add_modifier(Modifier::BOLD)
}

/// Sort-indicator arrow in the header: k9s `header.sorterColor` (sky).
pub fn sorter() -> Color {
    sky()
}

/// Title `[count]` color: k9s `frame.title.counterColor` (yellow).
pub fn counter() -> Color {
    yellow()
}

/// Marked-row color: k9s `views.table.markColor` (rosewater).
pub fn mark() -> Color {
    rosewater()
}

pub fn dim() -> Style {
    Style::default().fg(overlay1())
}

pub fn accent() -> Style {
    Style::default().fg(teal())
}

/// Colorize a status-like string (pod phase, node condition, etc.) for the
/// STATUS badge itself. Terminal/faded states (Succeeded, Terminating) match
/// their [`row_color`] counterpart so the badge blends into the already-faded
/// row instead of popping green/yellow and contradicting it; active states
/// (healthy, pending, error) keep a distinct pop color so they stand out
/// against the row tint.
pub fn status_color(s: &str) -> Color {
    match s {
        "Running" | "Ready" | "Active" | "Bound" | "True" => green(),
        // Faded, not "healthy green" — a finished pod isn't running.
        "Succeeded" | "Completed" => overlay0(),
        "Pending" | "ContainerCreating" | "PodInitializing" | "Progressing" => yellow(),
        // Matches row_color's killColor — a distinct "on its way out" hue,
        // not the same bucket as Pending.
        "Terminating" => mauve(),
        "Failed" | "Error" | "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull"
        | "Evicted" | "OOMKilled" | "NotReady" | "False" => red(),
        "Unknown" | "" => overlay1(),
        _ => text(),
    }
}

/// k9s-style whole-row color: every table row is tinted a single color chosen
/// by its status, exactly like k9s' colorer (`StdColor`/`ErrColor`/
/// `PendingColor`/`CompletedColor`/`KillColor`).
///
/// - errors → red
/// - transitional (pending/creating/progressing) → peach
/// - completed/succeeded → dim gray
/// - terminating/deleting → dimmer gray
/// - everything healthy (Running/Ready/Bound/…) **or without a status** → the
///   standard row color (blue), so healthy rows read blue like k9s, not white.
pub fn row_color(s: &str) -> Color {
    match s {
        "Failed" | "Error" | "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull"
        | "Evicted" | "OOMKilled" | "NotReady" | "Unhealthy" | "False" => red(),
        "Pending" | "ContainerCreating" | "PodInitializing" | "Progressing" => peach(),
        "Completed" | "Succeeded" => overlay0(),
        // k9s killColor — terminating/deleting rows.
        "Terminating" => mauve(),
        _ => blue(),
    }
}

/// Restart-count severity: `None` for a clean 0 (falls back to the row
/// color), a warning tint for a handful, red for a pod that's actively
/// crash-looping. Absolute thresholds, mirroring k9s' restart colorer.
pub fn restarts_severity(count: i64) -> Option<Color> {
    if count >= 5 {
        Some(red())
    } else if count >= 1 {
        Some(peach())
    } else {
        None
    }
}

/// CPU-usage severity in millicores. `None` for unremarkable usage.
pub fn cpu_severity(millicores: i64) -> Option<Color> {
    if millicores >= 1000 {
        Some(red())
    } else if millicores >= 200 {
        Some(peach())
    } else {
        None
    }
}

/// Memory-usage severity in bytes. `None` for unremarkable usage.
pub fn mem_severity(bytes: i64) -> Option<Color> {
    let mi = bytes as f64 / (1024.0 * 1024.0);
    if mi >= 1024.0 {
        Some(red())
    } else if mi >= 256.0 {
        Some(peach())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtins_parse() {
        for name in BUILTIN_NAMES {
            assert!(builtin(name).is_some(), "missing built-in {name}");
        }
    }

    #[test]
    fn parse_hex_forms() {
        assert_eq!(parse_hex("#1e1e2e"), Some(Color::Rgb(30, 30, 46)));
        assert_eq!(parse_hex("1e1e2e"), Some(Color::Rgb(30, 30, 46)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#gggggg"), None);
    }

    #[test]
    fn resolve_applies_overrides_and_ignores_junk() {
        let mut colors = HashMap::new();
        colors.insert("red".to_string(), "#010203".to_string());
        colors.insert("nope".to_string(), "#040506".to_string()); // unknown key
        colors.insert("green".to_string(), "zzzzzz".to_string()); // bad hex
        let p = resolve_skin(Some("catppuccin-mocha"), &colors);
        assert_eq!(p.red, Color::Rgb(1, 2, 3));
        assert_eq!(p.green, builtin("catppuccin-mocha").unwrap().green); // unchanged
    }

    #[test]
    fn row_color_matches_k9s_model() {
        // Healthy and status-less rows read blue (k9s StdColor), not white.
        assert_eq!(row_color("Running"), blue());
        assert_eq!(row_color("Ready"), blue());
        assert_eq!(row_color(""), blue());
        // Special states mirror k9s' colorer.
        assert_eq!(row_color("CrashLoopBackOff"), red());
        assert_eq!(row_color("Pending"), peach());
        assert_eq!(row_color("Completed"), overlay0());
        assert_eq!(row_color("Terminating"), mauve());
    }

    #[test]
    fn status_color_fades_terminal_states_pops_active_ones() {
        // Healthy pops distinct from the row's blue.
        assert_eq!(status_color("Running"), green());
        // Terminal states fade to match their row_color, not a bright color.
        assert_eq!(status_color("Succeeded"), row_color("Succeeded"));
        assert_eq!(status_color("Completed"), row_color("Completed"));
        assert_eq!(status_color("Terminating"), row_color("Terminating"));
        // Errors are unambiguously red, still matching the row.
        assert_eq!(status_color("CrashLoopBackOff"), red());
        assert_eq!(
            status_color("CrashLoopBackOff"),
            row_color("CrashLoopBackOff")
        );
        // Pending pops distinct from the row's peach.
        assert_eq!(status_color("Pending"), yellow());
        assert_ne!(status_color("Pending"), row_color("Pending"));
    }

    #[test]
    fn resource_severity_thresholds() {
        assert_eq!(restarts_severity(0), None);
        assert_eq!(restarts_severity(1), Some(peach()));
        assert_eq!(restarts_severity(5), Some(red()));

        assert_eq!(cpu_severity(50), None);
        assert_eq!(cpu_severity(200), Some(peach()));
        assert_eq!(cpu_severity(1000), Some(red()));

        assert_eq!(mem_severity(100 * 1024 * 1024), None); // 100Mi
        assert_eq!(mem_severity(300 * 1024 * 1024), Some(peach())); // 300Mi
        assert_eq!(mem_severity(2048 * 1024 * 1024), Some(red())); // 2Gi
    }

    #[test]
    fn unknown_skin_falls_back_to_mocha() {
        let p = resolve_skin(Some("no-such-skin"), &HashMap::new());
        assert_eq!(p.base, catppuccin_mocha().base);
    }
}
