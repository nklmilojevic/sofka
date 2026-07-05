//! Data-driven palette + semantic styles for the TUI.
//!
//! The palette is a 25-swatch [`Palette`] (Catppuccin's slot layout). A named
//! built-in skin is selected in config, with optional per-swatch hex
//! overrides:
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
//! Leaving out `[skin]`/`name` entirely defaults to an auto-detected
//! Catppuccin flavor: `catppuccin-latte` on a light terminal background,
//! `catppuccin-mocha` otherwise. Detection queries the terminal's background
//! color (OSC 11) and is best-effort — it silently falls back to mocha on
//! terminals that don't answer the query (e.g. `TERM=dumb`, piped output, an
//! SSH hop with high latency).
//!
//! [`init`] installs the resolved palette once at startup; every color accessor
//! (`text()`, `peach()`, …) and semantic style (`title()`, `selected_row()`, …)
//! reads from it. Swatches not yet wired into a view are kept for completeness.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use ratatui::style::{Color, Modifier, Style};
use terminal_colorsaurus::{QueryOptions, ThemeMode};

macro_rules! palette_swatches {
    ($($idx:expr => $field:ident),+ $(,)?) => {
        /// A full 25-swatch palette. Field names double as the override keys
        /// accepted under `[skin.colors]` in config.
        #[derive(Debug, Clone, Copy)]
        pub struct Palette {
            $(pub $field: Color,)+
        }

        /// Swatch names in the order the built-in tables list their hex values.
        const FIELDS: &[&str] = &[$(stringify!($field),)+];

        impl Palette {
            /// Build a palette from 25 hex strings in [`FIELDS`] order. Panics
            /// on a malformed value — only ever called with the hardcoded
            /// built-in tables, so a bad hex here is a programmer error, not
            /// user input.
            fn from_hexes(h: &[&str]) -> Palette {
                assert_eq!(
                    h.len(),
                    FIELDS.len(),
                    "built-in palette has {} colors but {} swatches are defined",
                    h.len(),
                    FIELDS.len()
                );
                let c = |i: usize| {
                    parse_hex(h[i]).unwrap_or_else(|| panic!("bad built-in hex {}", h[i]))
                };
                Palette {
                    $($field: c($idx),)+
                }
            }

            /// Override one swatch by name. Returns `false` for an unknown key.
            fn set(&mut self, key: &str, color: Color) -> bool {
                match key {
                    $(stringify!($field) => self.$field = color,)+
                    _ => return false,
                }
                true
            }
        }

        $(pub fn $field() -> Color {
            palette().$field
        })+
    };
}

palette_swatches! {
    0 => rosewater,
    1 => flamingo,
    2 => pink,
    3 => mauve,
    4 => red,
    5 => maroon,
    6 => peach,
    7 => yellow,
    8 => green,
    9 => teal,
    10 => sky,
    11 => sapphire,
    12 => blue,
    13 => lavender,
    14 => text,
    15 => subtext1,
    16 => subtext0,
    17 => overlay1,
    18 => overlay0,
    19 => surface2,
    20 => surface1,
    21 => surface0,
    22 => base,
    23 => mantle,
    24 => crust,
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
    "gruvbox-dark",
    "gruvbox-light",
    "nord",
    "dracula",
    "solarized-dark",
    "solarized-light",
    "tokyo-night",
    "one-dark",
    "rose-pine",
    "monokai",
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
        "gruvbox-light" => [
            "#ebdbb2", "#d5c4a1", "#b16286", "#b16286", "#cc241d", "#9d0006", "#d65d0e", "#d79921",
            "#98971a", "#689d6a", "#458588", "#076678", "#458588", "#b16286", "#282828", "#3c3836",
            "#504945", "#665c54", "#7c6f64", "#928374", "#a89984", "#bdae93", "#fbf1c7", "#ebdbb2",
            "#d5c4a1",
        ],
        "solarized-dark" | "solarized" => [
            "#d33682", "#d33682", "#d33682", "#6c71c4", "#dc322f", "#dc322f", "#cb4b16", "#b58900",
            "#859900", "#2aa198", "#2aa198", "#268bd2", "#268bd2", "#6c71c4", "#93a1a1", "#839496",
            "#657b83", "#586e75", "#586e75", "#586e75", "#073642", "#073642", "#002b36", "#002b36",
            "#001e26",
        ],
        "solarized-light" => [
            "#d33682", "#d33682", "#d33682", "#6c71c4", "#dc322f", "#dc322f", "#cb4b16", "#b58900",
            "#859900", "#2aa198", "#2aa198", "#268bd2", "#268bd2", "#6c71c4", "#002b36", "#073642",
            "#586e75", "#657b83", "#839496", "#93a1a1", "#eee8d5", "#eee8d5", "#fdf6e3", "#eee8d5",
            "#eee8d5",
        ],
        "tokyo-night" | "tokyonight" => [
            "#f7768e", "#f7768e", "#bb9af7", "#9d7cd8", "#f7768e", "#db4b4b", "#ff9e64", "#e0af68",
            "#9ece6a", "#73daca", "#7dcfff", "#0db9d7", "#7aa2f7", "#9d7cd8", "#c0caf5", "#a9b1d6",
            "#545c7e", "#545c7e", "#3b4261", "#3b4261", "#292e42", "#292e42", "#1a1b26", "#16161e",
            "#13131a",
        ],
        "one-dark" | "onedark" => [
            "#e06c75", "#e06c75", "#c678dd", "#c678dd", "#e06c75", "#be5046", "#d19a66", "#e5c07b",
            "#98c379", "#56b6c2", "#56b6c2", "#61afef", "#61afef", "#c678dd", "#abb2bf", "#828997",
            "#5c6370", "#5c6370", "#4b5263", "#4b5263", "#3b4048", "#323842", "#282c34", "#21252b",
            "#1b1d23",
        ],
        "rose-pine" | "rosepine" => [
            "#ebbcba", "#ebbcba", "#ebbcba", "#c4a7e7", "#eb6f92", "#eb6f92", "#f6c177", "#f6c177",
            "#31748f", "#31748f", "#9ccfd8", "#9ccfd8", "#9ccfd8", "#c4a7e7", "#e0def4", "#908caa",
            "#6e6a86", "#6e6a86", "#524f67", "#524f67", "#403d52", "#26233a", "#191724", "#191724",
            "#14121d",
        ],
        "monokai" => [
            "#f92672", "#f92672", "#f92672", "#ae81ff", "#f92672", "#f92672", "#fd971f", "#e6db74",
            "#a6e22e", "#66d9ef", "#66d9ef", "#66d9ef", "#66d9ef", "#ae81ff", "#f8f8f2", "#cfcfc2",
            "#75715e", "#75715e", "#49483e", "#49483e", "#3e3d32", "#3e3d32", "#272822", "#23241f",
            "#1e1f1c",
        ],
        _ => return None,
    };
    Some(Palette::from_hexes(&hex))
}

fn catppuccin_mocha() -> Palette {
    builtin("catppuccin-mocha").expect("mocha built-in")
}

fn catppuccin_latte() -> Palette {
    builtin("catppuccin-latte").expect("latte built-in")
}

/// Best-effort dark/light detection via an OSC 11 background-color query.
/// `None` means the terminal didn't answer (unsupported, `TERM=dumb`, piped
/// output, or the query timed out) — callers should treat that as "assume
/// dark" rather than blocking or erroring.
fn detect_terminal_mode() -> Option<ThemeMode> {
    terminal_colorsaurus::theme_mode(QueryOptions::default()).ok()
}

/// The skin used when config names none: auto-detected from the terminal's
/// dark/light mode (OSC 11 query — run this before entering the alternate
/// screen).
pub fn auto_skin_name() -> &'static str {
    match detect_terminal_mode() {
        Some(ThemeMode::Light) => "catppuccin-latte",
        _ => "catppuccin-mocha",
    }
}

/// Resolve the active palette from config: start from the named built-in, or
/// — when no name is configured — auto-detect the terminal's dark/light mode
/// and pick `catppuccin-latte`/`catppuccin-mocha` accordingly. Then apply
/// per-swatch hex overrides. Warns on an unknown name, unknown swatch key, or
/// malformed hex, and carries on.
pub fn resolve_skin(name: Option<&str>, colors: &HashMap<String, String>) -> Palette {
    let mut p = match name {
        None => builtin(auto_skin_name()).expect("auto skin is a built-in"),
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

static ACTIVE: OnceLock<RwLock<Palette>> = OnceLock::new();

/// Install the active palette. If called after startup, replaces the active
/// palette so `:skin` can update colors without restarting the TUI.
pub fn init(p: Palette) {
    if ACTIVE.set(RwLock::new(p)).is_err() {
        set(p);
    }
}

pub fn set(p: Palette) {
    let lock = ACTIVE.get_or_init(|| RwLock::new(catppuccin_mocha()));
    match lock.write() {
        Ok(mut active) => *active = p,
        Err(poisoned) => *poisoned.into_inner() = p,
    }
}

fn palette() -> Palette {
    let lock = ACTIVE.get_or_init(|| RwLock::new(catppuccin_mocha()));
    match lock.read() {
        Ok(active) => *active,
        Err(poisoned) => *poisoned.into_inner(),
    }
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
    fn mocha_base_matches_golden_swatch() {
        let p = builtin("catppuccin-mocha").unwrap();
        assert_eq!(p.base, Color::Rgb(30, 30, 46));
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

    #[test]
    fn detect_terminal_mode_is_best_effort() {
        // No TTY in the test harness, so this should return `None` promptly
        // rather than panicking or blocking on the query timeout.
        let _ = detect_terminal_mode();
    }
}
