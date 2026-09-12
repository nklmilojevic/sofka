//! Built-in actions and their effective keyboard bindings.

use crate::{config::KeysConfig, keys::KeyChord};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;
use std::sync::OnceLock;

macro_rules! actions {
    ($($variant:ident => ($name:literal, $description:literal)),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub enum Action { $($variant),* }
        impl Action {
            pub fn name(self) -> &'static str {
                match self { $(Self::$variant => $name),* }
            }
            pub fn description(self) -> &'static str {
                match self { $(Self::$variant => $description),* }
            }
        }
    };
}

actions! {
    Accept => ("accept", "accept selection or input"),
    ActionMenu => ("action_menu", "resource action menu"),
    Adjacent => ("adjacent", "adjacent"),
    AllNamespaces => ("all_namespaces", "all namespaces"),
    Anchor0 => ("anchor_0", "tail logs"),
    Anchor1 => ("anchor_1", "logs from last minute"),
    Anchor2 => ("anchor_2", "logs from last 5 minutes"),
    Anchor3 => ("anchor_3", "logs from last 15 minutes"),
    Anchor4 => ("anchor_4", "logs from last 30 minutes"),
    Anchor5 => ("anchor_5", "logs from last hour"),
    Attach => ("attach", "attach"),
    AutoRefresh => ("auto_refresh", "toggle automatic refresh"),
    ResetBaseline => ("reset_baseline", "reset diff baseline to displayed resource"),
    Back => ("back", "back or clear search"),
    Backspace => ("backspace", "backspace"),
    Cascade => ("cascade", "cascade"),
    Clear => ("clear", "clear"),
    ClearLine => ("clear_line", "clear line"),
    Close => ("close", "close view"),
    Command => ("command", "command palette"),
    Compact => ("compact", "toggle compact mode"),
    Copy => ("copy", "copy"),
    CopyCell => ("copy_cell", "copy cell"),
    CopyName => ("copy_name", "copy name"),
    Cordon => ("cordon", "cordon"),
    Debug => ("debug", "debug"),
    DecodeSecret => ("decode_secret", "decode secret"),
    Delete => ("delete", "delete"),
    DeleteWord => ("delete_word", "delete word"),
    Describe => ("describe", "describe"),
    DiscoverChildren => ("discover_children", "discover children"),
    Down => ("down", "down"),
    Drain => ("drain", "drain"),
    Edit => ("edit", "edit"),
    Events => ("events", "events"),
    Exit => ("exit", "quit from table"),
    Explain => ("explain", "explain"),
    Faults => ("faults", "faults"),
    FavoriteNamespace1 => ("favorite_namespace_1", "select configured favourite namespace 1"),
    FavoriteNamespace2 => ("favorite_namespace_2", "select configured favourite namespace 2"),
    FavoriteNamespace3 => ("favorite_namespace_3", "select configured favourite namespace 3"),
    FavoriteNamespace4 => ("favorite_namespace_4", "select configured favourite namespace 4"),
    FavoriteNamespace5 => ("favorite_namespace_5", "select configured favourite namespace 5"),
    FavoriteNamespace6 => ("favorite_namespace_6", "select configured favourite namespace 6"),
    FavoriteNamespace7 => ("favorite_namespace_7", "select configured favourite namespace 7"),
    FavoriteNamespace8 => ("favorite_namespace_8", "select configured favourite namespace 8"),
    FavoriteNamespace9 => ("favorite_namespace_9", "select configured favourite namespace 9"),
    Filter => ("filter", "filter"),
    First => ("first", "first row"),
    FleetMark => ("fleet_mark", "fleet mark"),
    Follow => ("follow", "follow"),
    Force => ("force", "force"),
    ForceDelete => ("force_delete", "force delete"),
    Fullscreen => ("fullscreen", "fullscreen"),
    Help => ("help", "help"),
    HistoryBack => ("history_back", "history back"),
    HistoryForward => ("history_forward", "history forward"),
    Inspect => ("inspect", "decode secret or browse PVC"),
    InvertSort => ("invert_sort", "invert sort"),
    Last => ("last", "last row"),
    Left => ("left", "left"),
    Logs => ("logs", "logs"),
    LogWarnings => ("log_warnings", "toggle warning and error logs"),
    LogMarker => ("log_marker", "add visual log marker"),
    Lookback => ("lookback", "lookback"),
    Mark => ("mark", "mark"),
    RangeDown => ("range_down", "extend or reduce selection down"),
    RangeUp => ("range_up", "extend or reduce selection up"),
    Namespaces => ("namespaces", "namespaces"),
    NamespaceSelected => ("namespace_selected", "selected resource namespace"),
    NextMatch => ("next_match", "next match"),
    NextView => ("next_view", "next view"),
    Node => ("node", "node"),
    Open => ("open", "open selection"),
    Owner => ("owner", "owner"),
    PageDown => ("page_down", "page down"),
    PageUp => ("page_up", "page up"),
    Parent => ("parent", "parent"),
    PortForward => ("port_forward", "port forward"),
    PreviousLogs => ("previous_logs", "previous logs"),
    PreviousMatch => ("previous_match", "previous match"),
    PreviousView => ("previous_view", "previous view"),
    ProviderLogs => ("provider_logs", "provider logs"),
    Quit => ("quit", "quit"),
    Refresh => ("refresh", "refresh"),
    Rename => ("rename", "rename"),
    RestartOrRefresh => ("restart_or_refresh", "restart, sync, rollback, or refresh"),
    Right => ("right", "right"),
    Save => ("save", "save"),
    SetImage => ("set_image", "set image"),
    Shell => ("shell", "shell"),
    ShellOrScale => ("shell_or_scale", "shell or scale through /scale"),
    Sort => ("sort", "sort"),
    SortAge => ("sort_age", "sort by age; repeat to invert"),
    Start => ("start", "start"),
    Stream => ("stream", "stream"),
    SwitchPane => ("switch_pane", "switch pane"),
    Timeline => ("timeline", "timeline"),
    Timestamps => ("timestamps", "timestamps"),
    Toggle => ("toggle", "toggle"),
    Transfer => ("transfer", "transfer"),
    Uncordon => ("uncordon", "uncordon"),
    Up => ("up", "up"),
    Wide => ("wide", "wide"),
    Wrap => ("wrap", "wrap"),
    Yaml => ("yaml", "yaml"),
}

impl Action {
    pub(crate) fn log_anchor(self) -> Option<char> {
        match self {
            Self::Anchor0 => Some('0'),
            Self::Anchor1 => Some('1'),
            Self::Anchor2 => Some('2'),
            Self::Anchor3 => Some('3'),
            Self::Anchor4 => Some('4'),
            Self::Anchor5 => Some('5'),
            _ => None,
        }
    }

    pub const FAVORITE_NAMESPACES: [Self; 9] = [
        Self::FavoriteNamespace1,
        Self::FavoriteNamespace2,
        Self::FavoriteNamespace3,
        Self::FavoriteNamespace4,
        Self::FavoriteNamespace5,
        Self::FavoriteNamespace6,
        Self::FavoriteNamespace7,
        Self::FavoriteNamespace8,
        Self::FavoriteNamespace9,
    ];
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyInput {
    pub action: Option<Action>,
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyInput {
    pub fn new(action: Option<Action>, key: KeyEvent) -> Self {
        Self {
            action,
            code: key.code,
            modifiers: key.modifiers,
        }
    }
    pub fn event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    bindings: BTreeMap<&'static str, BTreeMap<Action, Vec<KeyChord>>>,
    labels: BTreeMap<&'static str, BTreeMap<Action, KeyLabels>>,
    cancel_any: bool,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyLabels {
    first: String,
    all: String,
}

pub(crate) const LEGACY_PALETTE_KEYS: &[(&str, Action)] = &[
    ("palette_next", Action::Down),
    ("palette_prev", Action::Up),
    ("palette_accept", Action::Accept),
];

const GLOBAL: &[(Action, &[&str])] = &[(Action::Quit, &["ctrl-c"]), (Action::Compact, &["ctrl-e"])];
const INPUT: &[(Action, &[&str])] = &[
    (Action::Back, &["esc"]),
    (Action::Backspace, &["backspace"]),
    (Action::Accept, &["enter"]),
    (Action::ClearLine, &["ctrl-u"]),
    (
        Action::DeleteWord,
        &["ctrl-w", "alt-backspace", "ctrl-backspace"],
    ),
];
const VIEW_SCOPES: &[&str] = &[
    "table",
    "detail",
    "logs",
    "help",
    "containers",
    "confirm",
    "pulse",
    "xray",
    "explain",
    "timeline",
    "gitops",
    "argocd",
    "adjacent",
    "diff",
    "events",
    "flux_menu",
    "transfer_menu",
    "port_forwards",
    "skins",
    "snapshots",
    "fleet",
    "find",
    "pvc_explore",
];

const DEFAULTS: &[(&str, Action, &[&str])] = &[
    ("adjacent", Action::Accept, &["enter"]),
    ("adjacent", Action::Back, &["esc"]),
    ("adjacent", Action::Close, &["q"]),
    ("adjacent", Action::Describe, &["d"]),
    ("adjacent", Action::DiscoverChildren, &["c"]),
    ("adjacent", Action::Down, &["j", "down"]),
    ("adjacent", Action::First, &["g", "home"]),
    ("adjacent", Action::Last, &["G", "end"]),
    ("adjacent", Action::Refresh, &["r"]),
    ("adjacent", Action::Up, &["k", "up"]),
    ("adjacent", Action::Yaml, &["y"]),
    ("command", Action::Down, &["tab", "down"]),
    ("command", Action::Up, &["backtab", "up"]),
    ("confirm", Action::Accept, &["y", "Y", "enter"]),
    ("confirm", Action::Back, &["esc", "n", "N", "q"]),
    ("confirm", Action::Cascade, &["c", "C"]),
    ("confirm", Action::Force, &["f", "F"]),
    ("containers", Action::Back, &["esc"]),
    ("containers", Action::Close, &["q"]),
    ("containers", Action::Debug, &["d"]),
    ("containers", Action::Down, &["j", "down"]),
    ("containers", Action::Logs, &["enter", "l"]),
    ("containers", Action::PreviousLogs, &["p"]),
    ("containers", Action::ProviderLogs, &["L"]),
    ("containers", Action::Shell, &["s"]),
    ("containers", Action::Transfer, &["t"]),
    ("containers", Action::Up, &["k", "up"]),
    ("context_filter", Action::Down, &["down"]),
    ("context_filter", Action::Up, &["up"]),
    ("contexts", Action::Accept, &["enter"]),
    ("contexts", Action::Back, &["esc"]),
    ("contexts", Action::Down, &["down", "j"]),
    ("contexts", Action::Filter, &["/"]),
    ("contexts", Action::FleetMark, &["space"]),
    ("contexts", Action::Rename, &["r", "R"]),
    ("contexts", Action::Up, &["up", "k"]),
    ("copy_picker", Action::Down, &["down", "ctrl-n"]),
    ("copy_picker", Action::Up, &["up", "ctrl-p"]),
    ("detail", Action::AutoRefresh, &["r"]),
    ("detail", Action::Back, &["esc"]),
    ("detail", Action::Close, &["q"]),
    ("detail", Action::Copy, &["c"]),
    ("detail", Action::DecodeSecret, &["x"]),
    ("detail", Action::Down, &["j", "down"]),
    ("detail", Action::Filter, &["/"]),
    ("detail", Action::First, &["g", "home"]),
    ("detail", Action::Fullscreen, &["F"]),
    ("detail", Action::Last, &["G", "end"]),
    ("detail", Action::Left, &["h", "left"]),
    ("detail", Action::NextMatch, &["n"]),
    ("detail", Action::PageDown, &["pagedown", "space", "ctrl-f"]),
    ("detail", Action::PageUp, &["pageup", "ctrl-b"]),
    ("detail", Action::PreviousMatch, &["N"]),
    ("detail", Action::Right, &["l", "right"]),
    ("detail", Action::Up, &["k", "up"]),
    ("detail", Action::Wrap, &["w"]),
    ("diff", Action::AutoRefresh, &["r"]),
    ("diff", Action::ResetBaseline, &["R"]),
    ("diff", Action::Back, &["esc"]),
    ("diff", Action::Close, &["q"]),
    ("diff", Action::Copy, &["c"]),
    ("diff", Action::Down, &["j", "down"]),
    ("diff", Action::Filter, &["/"]),
    ("diff", Action::First, &["g", "home"]),
    ("diff", Action::Fullscreen, &["F"]),
    ("diff", Action::Last, &["G", "end"]),
    ("diff", Action::Left, &["h", "left"]),
    ("diff", Action::NextMatch, &["n"]),
    ("diff", Action::PageDown, &["pagedown", "space", "ctrl-f"]),
    ("diff", Action::PageUp, &["pageup", "ctrl-b"]),
    ("diff", Action::PreviousMatch, &["N"]),
    ("diff", Action::Right, &["l", "right"]),
    ("diff", Action::Up, &["k", "up"]),
    ("diff", Action::Wrap, &["w"]),
    ("drain", Action::Down, &["tab", "down"]),
    ("drain", Action::Up, &["backtab", "up"]),
    ("drain", Action::Toggle, &["space"]),
    ("drain", Action::PageUp, &["pageup"]),
    ("drain", Action::PageDown, &["pagedown"]),
    ("events", Action::Back, &["esc"]),
    ("events", Action::Close, &["q"]),
    ("events", Action::Copy, &["c"]),
    ("events", Action::Down, &["j", "down"]),
    ("events", Action::Filter, &["/"]),
    ("events", Action::First, &["g", "home"]),
    ("events", Action::Fullscreen, &["F"]),
    ("events", Action::Last, &["G", "end"]),
    ("events", Action::Left, &["h", "left"]),
    ("events", Action::NextMatch, &["n"]),
    ("events", Action::PageDown, &["pagedown", "space", "ctrl-f"]),
    ("events", Action::PageUp, &["pageup", "ctrl-b"]),
    ("events", Action::PreviousMatch, &["N"]),
    ("events", Action::Right, &["l", "right"]),
    ("events", Action::Up, &["k", "up"]),
    ("events", Action::Wrap, &["w"]),
    ("explain", Action::Accept, &["enter"]),
    ("explain", Action::AutoRefresh, &["R"]),
    ("explain", Action::Back, &["esc"]),
    ("explain", Action::Close, &["q"]),
    ("explain", Action::Down, &["j", "down"]),
    ("explain", Action::Events, &["E"]),
    ("explain", Action::First, &["g", "home"]),
    ("explain", Action::Last, &["G", "end"]),
    ("explain", Action::Logs, &["l"]),
    ("explain", Action::Refresh, &["r"]),
    ("explain", Action::Up, &["k", "up"]),
    ("find", Action::Accept, &["enter"]),
    ("find", Action::Back, &["esc"]),
    ("find", Action::Close, &["q"]),
    ("find", Action::Down, &["j", "down"]),
    ("find", Action::First, &["g", "home"]),
    ("find", Action::Last, &["G", "end"]),
    ("find", Action::Up, &["k", "up"]),
    ("fleet", Action::Accept, &["enter"]),
    ("fleet", Action::Back, &["esc"]),
    ("fleet", Action::Close, &["q"]),
    ("fleet", Action::Down, &["j", "down"]),
    ("fleet", Action::Refresh, &["r"]),
    ("fleet", Action::Up, &["k", "up"]),
    ("flux_menu", Action::Accept, &["enter"]),
    ("flux_menu", Action::Back, &["esc"]),
    ("flux_menu", Action::Close, &["q"]),
    ("flux_menu", Action::Down, &["j", "down"]),
    ("flux_menu", Action::Up, &["k", "up"]),
    ("argocd", Action::Accept, &["enter"]),
    ("argocd", Action::Back, &["esc"]),
    ("argocd", Action::Close, &["q"]),
    ("argocd", Action::DiscoverChildren, &["c"]),
    ("argocd", Action::Down, &["j", "down"]),
    ("argocd", Action::First, &["g", "home"]),
    ("argocd", Action::Last, &["G", "end"]),
    ("argocd", Action::Refresh, &["r"]),
    ("argocd", Action::Up, &["k", "up"]),
    ("gitops", Action::Accept, &["enter"]),
    ("gitops", Action::Back, &["esc"]),
    ("gitops", Action::Close, &["q"]),
    ("gitops", Action::Down, &["j", "down"]),
    ("gitops", Action::First, &["g", "home"]),
    ("gitops", Action::Last, &["G", "end"]),
    ("gitops", Action::Refresh, &["r"]),
    ("gitops", Action::Up, &["k", "up"]),
    ("help", Action::Back, &["esc"]),
    ("help", Action::Close, &["q", "?"]),
    ("help", Action::Down, &["j", "down"]),
    ("help", Action::Filter, &["/"]),
    ("help", Action::First, &["g", "home"]),
    ("help", Action::Last, &["G", "end"]),
    ("help", Action::PageDown, &["pagedown", "space", "ctrl-f"]),
    ("help", Action::PageUp, &["pageup", "ctrl-b"]),
    ("help", Action::Up, &["k", "up"]),
    ("logs", Action::Anchor0, &["0"]),
    ("logs", Action::Anchor1, &["1"]),
    ("logs", Action::Anchor2, &["2"]),
    ("logs", Action::Anchor3, &["3"]),
    ("logs", Action::Anchor4, &["4"]),
    ("logs", Action::Anchor5, &["5"]),
    ("logs", Action::Back, &["esc"]),
    ("logs", Action::Clear, &["z"]),
    ("logs", Action::Close, &["q"]),
    ("logs", Action::Copy, &["c"]),
    ("logs", Action::Down, &["j", "down"]),
    ("logs", Action::Filter, &["/"]),
    ("logs", Action::First, &["g", "home"]),
    ("logs", Action::Follow, &["s", "f"]),
    ("logs", Action::Fullscreen, &["F"]),
    ("logs", Action::Last, &["G", "end"]),
    ("logs", Action::Lookback, &["T"]),
    ("logs", Action::LogMarker, &["m"]),
    ("logs", Action::LogWarnings, &["ctrl-z"]),
    ("logs", Action::PageDown, &["pagedown", "space"]),
    ("logs", Action::PageUp, &["pageup"]),
    ("logs", Action::Save, &["ctrl-s"]),
    ("logs", Action::Stream, &["x"]),
    ("logs", Action::Timestamps, &["t"]),
    ("logs", Action::Up, &["k", "up"]),
    ("logs", Action::Wrap, &["w"]),
    ("namespaces", Action::Down, &["down"]),
    ("namespaces", Action::Up, &["up"]),
    ("port_forward_picker", Action::Accept, &["enter"]),
    ("port_forward_picker", Action::Back, &["esc"]),
    ("port_forward_picker", Action::Close, &["q"]),
    ("port_forward_picker", Action::Down, &["j", "down"]),
    ("port_forward_picker", Action::Edit, &["e"]),
    ("port_forward_picker", Action::Up, &["k", "up"]),
    ("port_forwards", Action::Back, &["esc"]),
    ("port_forwards", Action::Close, &["q"]),
    ("port_forwards", Action::Down, &["j", "down"]),
    ("port_forwards", Action::Start, &["enter"]),
    ("port_forwards", Action::Toggle, &["x", "s"]),
    ("port_forwards", Action::Up, &["k", "up"]),
    ("pulse", Action::Back, &["esc"]),
    ("pulse", Action::Close, &["q"]),
    ("pulse", Action::Refresh, &["r"]),
    ("pvc_explore", Action::Accept, &["enter"]),
    ("pvc_explore", Action::Back, &["esc"]),
    ("pvc_explore", Action::Close, &["q"]),
    ("pvc_explore", Action::Copy, &["c"]),
    ("pvc_explore", Action::Down, &["j", "down"]),
    ("pvc_explore", Action::First, &["g", "home"]),
    ("pvc_explore", Action::Last, &["G", "end"]),
    ("pvc_explore", Action::Left, &["left"]),
    ("pvc_explore", Action::Parent, &["backspace", "-"]),
    ("pvc_explore", Action::Refresh, &["r"]),
    ("pvc_explore", Action::Right, &["right"]),
    ("pvc_explore", Action::Shell, &["s"]),
    ("pvc_explore", Action::SwitchPane, &["tab", "backtab"]),
    ("pvc_explore", Action::Up, &["k", "up"]),
    ("set_image", Action::Accept, &["enter"]),
    ("set_image", Action::Back, &["esc"]),
    ("set_image", Action::Close, &["q"]),
    ("set_image", Action::Down, &["j", "down"]),
    ("set_image", Action::Up, &["k", "up"]),
    ("skins", Action::Accept, &["enter"]),
    ("skins", Action::Back, &["esc"]),
    ("skins", Action::Close, &["q"]),
    ("skins", Action::Down, &["j", "down"]),
    ("skins", Action::Up, &["k", "up"]),
    ("snapshots", Action::Accept, &["enter"]),
    ("snapshots", Action::Back, &["esc"]),
    ("snapshots", Action::Close, &["q"]),
    ("snapshots", Action::Delete, &["d"]),
    ("snapshots", Action::Down, &["j", "down"]),
    ("snapshots", Action::Up, &["k", "up"]),
    ("sort_picker", Action::Down, &["down", "ctrl-n"]),
    ("sort_picker", Action::Up, &["up", "ctrl-p"]),
    ("table", Action::ActionMenu, &["t"]),
    ("table", Action::Adjacent, &["u"]),
    ("table", Action::AllNamespaces, &["0"]),
    ("table", Action::FavoriteNamespace1, &["1"]),
    ("table", Action::FavoriteNamespace2, &["2"]),
    ("table", Action::FavoriteNamespace3, &["3"]),
    ("table", Action::FavoriteNamespace4, &["4"]),
    ("table", Action::FavoriteNamespace5, &["5"]),
    ("table", Action::FavoriteNamespace6, &["6"]),
    ("table", Action::FavoriteNamespace7, &["7"]),
    ("table", Action::FavoriteNamespace8, &["8"]),
    ("table", Action::FavoriteNamespace9, &["9"]),
    ("table", Action::Attach, &["a"]),
    ("table", Action::Back, &["esc"]),
    ("table", Action::CopyCell, &["Y"]),
    ("table", Action::CopyName, &["c"]),
    ("table", Action::Cordon, &["C"]),
    ("table", Action::Delete, &["ctrl-d"]),
    ("table", Action::Describe, &["d"]),
    ("table", Action::Down, &["j", "down"]),
    ("table", Action::Drain, &["D"]),
    ("table", Action::Edit, &["e"]),
    ("table", Action::Events, &["E"]),
    ("table", Action::Exit, &["q"]),
    ("table", Action::Explain, &["X"]),
    ("table", Action::Faults, &["ctrl-z"]),
    ("table", Action::Filter, &["/"]),
    ("table", Action::First, &["g", "home"]),
    ("table", Action::ForceDelete, &["ctrl-k"]),
    ("table", Action::HistoryBack, &["["]),
    ("table", Action::HistoryForward, &["]"]),
    ("table", Action::Inspect, &["x"]),
    ("table", Action::InvertSort, &["I"]),
    ("table", Action::Last, &["G", "end"]),
    ("table", Action::Left, &["left"]),
    ("table", Action::Logs, &["l"]),
    ("table", Action::Mark, &["space"]),
    ("table", Action::RangeDown, &["shift-down"]),
    ("table", Action::RangeUp, &["shift-up"]),
    ("table", Action::Namespaces, &["n"]),
    ("table", Action::NamespaceSelected, &["W"]),
    ("table", Action::NextView, &["tab"]),
    ("table", Action::Node, &["o"]),
    ("table", Action::Open, &["enter"]),
    ("table", Action::Owner, &["J"]),
    ("table", Action::PageDown, &["pagedown", "ctrl-f"]),
    ("table", Action::PageUp, &["pageup", "ctrl-b"]),
    ("table", Action::PortForward, &["f", "F"]),
    ("table", Action::PreviousLogs, &["p"]),
    ("table", Action::PreviousView, &["backtab"]),
    ("table", Action::ProviderLogs, &["L"]),
    ("table", Action::Refresh, &["ctrl-r"]),
    ("table", Action::RestartOrRefresh, &["r"]),
    ("table", Action::Right, &["right"]),
    ("table", Action::SetImage, &["i"]),
    ("table", Action::ShellOrScale, &["s"]),
    ("table", Action::Sort, &["S"]),
    ("table", Action::SortAge, &["A"]),
    ("table", Action::Timeline, &["T"]),
    ("table", Action::Uncordon, &["U"]),
    ("table", Action::Up, &["k", "up"]),
    ("table", Action::Wide, &["w"]),
    ("table", Action::Yaml, &["y"]),
    ("timeline", Action::Back, &["esc"]),
    ("timeline", Action::Close, &["q"]),
    ("timeline", Action::Down, &["j", "down"]),
    ("timeline", Action::First, &["g", "home"]),
    ("timeline", Action::Last, &["G", "end"]),
    ("timeline", Action::Up, &["k", "up"]),
    ("transfer_menu", Action::Accept, &["enter"]),
    ("transfer_menu", Action::Back, &["esc"]),
    ("transfer_menu", Action::Close, &["q"]),
    ("transfer_menu", Action::Down, &["j", "down"]),
    ("transfer_menu", Action::Up, &["k", "up"]),
    ("xray", Action::Back, &["esc"]),
    ("xray", Action::Close, &["q"]),
    ("xray", Action::Down, &["j", "down"]),
    ("xray", Action::First, &["g", "home"]),
    ("xray", Action::Last, &["G", "end"]),
    ("xray", Action::Logs, &["enter", "l"]),
    ("xray", Action::Refresh, &["r"]),
    ("xray", Action::Up, &["k", "up"]),
];
const TEXT_SCOPES: &[&str] = &[
    "drain",
    "command",
    "filter",
    "log_filter",
    "doc_filter",
    "prompt",
    "sort_picker",
    "copy_picker",
    "namespaces",
    "context_filter",
];
const NAVIGATION_ACTIONS: &[Action] = &[
    Action::Up,
    Action::Down,
    Action::First,
    Action::Last,
    Action::PageUp,
    Action::PageDown,
    Action::Left,
    Action::Right,
    Action::Back,
    Action::Close,
    Action::Command,
    Action::Help,
];
impl Default for Keymap {
    fn default() -> Self {
        static DEFAULT: OnceLock<Keymap> = OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                let mut bindings: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
                for &(scope, action, chords) in DEFAULTS {
                    bindings.entry(scope).or_default().insert(
                        action,
                        chords
                            .iter()
                            .map(|s| KeyChord::parse(s).expect("valid default key"))
                            .collect(),
                    );
                }
                for &scope in TEXT_SCOPES {
                    bindings.entry(scope).or_default();
                }
                for (&scope, actions) in &mut bindings {
                    for &(action, chords) in GLOBAL
                        .iter()
                        .chain(INPUT.iter().filter(|_| TEXT_SCOPES.contains(&scope)))
                    {
                        actions.insert(
                            action,
                            chords
                                .iter()
                                .map(|s| KeyChord::parse(s).expect("valid default key"))
                                .collect(),
                        );
                    }
                    if VIEW_SCOPES.contains(&scope) {
                        actions.insert(Action::Command, vec![KeyChord::parse(":").unwrap()]);
                        if scope != "help" {
                            actions.insert(Action::Help, vec![KeyChord::parse("?").unwrap()]);
                        }
                    }
                }
                // Raw navigation matches previously accepted Shift in these modes.
                // Table Shift+Up/Down select ranges. Other aliases stay unchanged.
                for (&scope, actions) in &mut bindings {
                    if scope == "command" {
                        continue;
                    }
                    for chords in actions.values_mut() {
                        let shifted: Vec<_> = chords
                            .iter()
                            .filter(|c| {
                                !c.ctrl
                                    && !c.alt
                                    && !c.shift
                                    && !(scope == "table"
                                        && matches!(c.code, KeyCode::Up | KeyCode::Down))
                                    && matches!(
                                        c.code,
                                        KeyCode::Up
                                            | KeyCode::Down
                                            | KeyCode::Left
                                            | KeyCode::Right
                                            | KeyCode::PageUp
                                            | KeyCode::PageDown
                                            | KeyCode::Home
                                            | KeyCode::End
                                    )
                            })
                            .map(|c| KeyChord { shift: true, ..*c })
                            .collect();
                        chords.extend(shifted);
                    }
                }
                let mut map = Keymap {
                    bindings,
                    labels: BTreeMap::new(),
                    cancel_any: true,
                    warnings: Vec::new(),
                };
                map.cache_labels();
                map
            })
            .clone()
    }
}

impl Keymap {
    pub fn compile(cfg: &KeysConfig) -> Result<Self, Vec<String>> {
        let Some(settings) = cfg.0.as_table() else {
            return Err(vec!["keys: expected a table".into()]);
        };
        let mut map = Self::default();
        let mut errors = Vec::new();
        let mut overrides = BTreeMap::new();
        for (scope, value) in settings {
            if let Some((_, action)) = LEGACY_PALETTE_KEYS.iter().find(|(name, _)| scope == name) {
                map.warnings.push(format!(
                    "keys.{scope}: legacy setting ignored; move its value to [keys.command] {}",
                    action.name()
                ));
                continue;
            }
            if !matches!(scope.as_str(), "global" | "navigation" | "input")
                && !map.bindings.contains_key(scope.as_str())
            {
                errors.push(format!("keys.{scope}: unknown scope"));
                continue;
            }
            let Some(actions) = value.as_table() else {
                errors.push(format!("keys.{scope}: expected a table of actions"));
                continue;
            };
            for (name, spec) in actions {
                let targets = map.targets(scope, name);
                if targets.is_empty() {
                    errors.push(format!("keys.{scope}.{name}: unsupported action"));
                    continue;
                }
                let chords = parse_chords(spec, &format!("keys.{scope}.{name}"), &mut errors);
                overrides.insert((scope.as_str(), name.as_str()), (targets, chords));
            }
        }
        // Apply groups first, then explicit mode settings, regardless of file order.
        for shared in [true, false] {
            for (&(scope, _), (targets, chords)) in &overrides {
                if matches!(scope, "global" | "navigation" | "input") != shared {
                    continue;
                }
                for &(target, action) in targets {
                    map.bindings
                        .get_mut(target)
                        .unwrap()
                        .insert(action, chords.clone());
                }
            }
        }
        let command_settings = settings.get("command").and_then(toml::Value::as_table);
        let scoped =
            |action: Action| command_settings.is_some_and(|c| c.contains_key(action.name()));
        // Explicit completion keys take priority over text editing and cancellation.
        for &(_, action) in LEGACY_PALETTE_KEYS {
            if !scoped(action) {
                continue;
            }
            let chords = map.chords("command", action).to_vec();
            for &(edit, _) in INPUT {
                if LEGACY_PALETTE_KEYS.iter().any(|&(_, a)| a == edit) {
                    continue;
                }
                map.bindings
                    .get_mut("command")
                    .unwrap()
                    .get_mut(&edit)
                    .unwrap()
                    .retain(|c| !chords.iter().any(|other| overlaps(c, other)));
            }
        }
        map.cancel_any = !settings
            .get("confirm")
            .and_then(toml::Value::as_table)
            .is_some_and(|s| s.contains_key("back"));
        for (scope, bindings) in &map.bindings {
            let entries: Vec<_> = bindings.iter().collect();
            for (i, (action, chords)) in entries.iter().enumerate() {
                for (other, other_chords) in &entries[i + 1..] {
                    if let Some(chord) = chords
                        .iter()
                        .find(|c| other_chords.iter().any(|o| overlaps(c, o)))
                    {
                        errors.push(format!(
                            "keys.{scope}: {} conflicts between {} and {}",
                            chord.label(),
                            action.name(),
                            other.name()
                        ));
                    }
                }
            }
        }
        if errors.is_empty() {
            map.cache_labels();
            Ok(map)
        } else {
            Err(errors)
        }
    }

    fn targets(&self, scope: &str, name: &str) -> Vec<(&'static str, Action)> {
        self.bindings
            .iter()
            .filter_map(|(&target, bindings)| {
                let action = bindings.keys().find(|a| a.name() == name).copied()?;
                let applies = match scope {
                    "global" => GLOBAL.iter().any(|&(a, _)| a == action),
                    "input" => {
                        TEXT_SCOPES.contains(&target) && INPUT.iter().any(|&(a, _)| a == action)
                    }
                    "navigation" => {
                        !TEXT_SCOPES.contains(&target)
                            && NAVIGATION_ACTIONS.contains(&action)
                            && !(target == "confirm" && action == Action::Back)
                    }
                    _ => scope == target,
                };
                applies.then_some((target, action))
            })
            .collect()
    }

    pub fn is_default(&self) -> bool {
        let defaults = Self::default();
        self.bindings == defaults.bindings && self.cancel_any == defaults.cancel_any
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn wheel_action(&self, scope: &str, down: bool) -> Option<Action> {
        if scope == "confirm" {
            self.cancel_any.then_some(Action::Back)
        } else {
            Some(if down { Action::Down } else { Action::Up })
        }
    }

    pub fn action(&self, scope: &str, key: &KeyEvent) -> Option<Action> {
        self.bindings
            .get(scope)?
            .iter()
            .find_map(|(&action, chords)| chords.iter().any(|c| c.matches(key)).then_some(action))
            .or_else(|| (scope == "confirm" && self.cancel_any).then_some(Action::Back))
    }

    pub fn chords(&self, scope: &str, action: Action) -> &[KeyChord] {
        self.bindings
            .get(scope)
            .and_then(|b| b.get(&action))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn cache_labels(&mut self) {
        self.labels = self
            .bindings
            .iter()
            .map(|(&scope, bindings)| {
                let labels = bindings
                    .iter()
                    .map(|(&action, chords)| {
                        let labels: Vec<_> = chords.iter().map(KeyChord::label).collect();
                        let first = labels.first().cloned().unwrap_or_else(|| "unbound".into());
                        let all = if labels.is_empty() {
                            first.clone()
                        } else {
                            labels.join(" / ")
                        };
                        (action, KeyLabels { first, all })
                    })
                    .collect();
                (scope, labels)
            })
            .collect();
    }

    pub fn label(&self, scope: &str, action: Action) -> &str {
        self.labels
            .get(scope)
            .and_then(|labels| labels.get(&action))
            .map_or("unbound", |labels| labels.all.as_str())
    }

    pub fn first_label(&self, scope: &str, action: Action) -> &str {
        self.labels
            .get(scope)
            .and_then(|labels| labels.get(&action))
            .map_or("unbound", |labels| labels.first.as_str())
    }

    pub fn entries(&self) -> impl Iterator<Item = (&'static str, Action, &[KeyChord])> {
        self.bindings.iter().flat_map(|(&scope, bindings)| {
            bindings
                .iter()
                .map(move |(&action, chords)| (scope, action, chords.as_slice()))
        })
    }
}

pub(crate) fn parse_chords(
    spec: &toml::Value,
    path: &str,
    errors: &mut Vec<String>,
) -> Vec<KeyChord> {
    let values = match spec {
        toml::Value::String(_) => std::slice::from_ref(spec),
        toml::Value::Array(values) => values,
        _ => {
            errors.push(format!(
                "{path}: expected a key string or an array of key strings"
            ));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for value in values {
        let Some(value) = value.as_str() else {
            errors.push(format!(
                "{path}: expected a key string, got {}",
                value.type_str()
            ));
            continue;
        };
        match KeyChord::parse(value) {
            Ok(chord) => {
                if !out.iter().any(|other| overlaps(&chord, other)) {
                    out.push(chord);
                }
            }
            Err(error) => errors.push(format!("{path}: {error}")),
        }
    }
    out
}

pub(crate) fn overlaps(a: &KeyChord, b: &KeyChord) -> bool {
    let event = |c: &KeyChord| {
        let mut modifiers = KeyModifiers::NONE;
        modifiers.set(KeyModifiers::CONTROL, c.ctrl);
        modifiers.set(KeyModifiers::ALT, c.alt);
        modifiers.set(KeyModifiers::SHIFT, c.shift);
        KeyEvent::new(c.code, modifiers)
    };
    a.matches(&event(b)) || b.matches(&event(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(text: &str) -> Result<Keymap, Vec<String>> {
        let cfg: crate::config::Config = toml::from_str(text).unwrap();
        Keymap::compile(&cfg.keys)
    }

    #[test]
    fn defaults_have_no_conflicts() {
        assert_eq!(compile("").unwrap(), Keymap::default());
    }

    #[test]
    fn paging_requires_releasing_delete_and_keeps_text_editing() {
        let text = r#"
            [keys.navigation]
            page_up = ["pageup", "ctrl-b", "ctrl-u"]
            page_down = ["pagedown", "ctrl-f", "ctrl-d"]
        "#;
        let errors = compile(text).unwrap_err().join("\n");
        assert!(errors.contains("keys.table"), "{errors}");
        assert!(errors.contains("ctrl-d"), "{errors}");
        assert!(errors.contains("delete"), "{errors}");
        assert!(errors.contains("page_down"), "{errors}");
        let map = compile(&format!("{text}\n[keys.table]\ndelete = 'alt-d'\n")).unwrap();
        for scope in ["table", "detail", "diff", "events", "help", "logs"] {
            assert_eq!(
                map.action(
                    scope,
                    &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
                ),
                Some(Action::PageDown)
            );
        }
        assert_eq!(
            map.action(
                "prompt",
                &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
            ),
            Some(Action::ClearLine)
        );
    }

    #[test]
    fn resource_and_log_shortcuts_support_overrides_and_conflicts() {
        for (scope, action, default, custom) in [
            ("table", Action::SortAge, 'A', "f8"),
            ("table", Action::NamespaceSelected, 'W', "f9"),
            ("logs", Action::LogMarker, 'm', "f10"),
        ] {
            let key = KeyEvent::new(KeyCode::Char(default), KeyModifiers::NONE);
            assert_eq!(Keymap::default().action(scope, &key), Some(action));
            let setting = format!("[keys.{scope}]\n{} = '{custom}'", action.name());
            let map = compile(&setting).unwrap();
            assert_eq!(map.label(scope, action), custom);
            assert_eq!(map.action(scope, &key), None);
            let disabled = compile(&format!("[keys.{scope}]\n{} = []", action.name())).unwrap();
            assert_eq!(disabled.action(scope, &key), None);
            let conflict = format!("[keys.{scope}]\n{} = 'w'", action.name());
            assert!(
                compile(&conflict)
                    .unwrap_err()
                    .iter()
                    .any(|e| e.contains("conflicts"))
            );
        }
        let map = Keymap::default();
        for (scope, key, action) in [
            ("table", 'a', Action::Attach),
            ("table", 'w', Action::Wide),
            ("logs", 'w', Action::Wrap),
        ] {
            assert_eq!(
                map.action(
                    scope,
                    &KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)
                ),
                Some(action)
            );
        }
    }

    #[test]
    fn local_override_replaces_shared_keys_and_empty_array_disables() {
        let map = compile(
            r#"
            [keys.navigation]
            page_down = "alt-d"
            [keys.logs]
            page_down = []
            [keys.table]
            page_down = ["f8", "f9"]
        "#,
        )
        .unwrap();
        assert_eq!(map.label("table", Action::PageDown), "f8 / f9");
        assert_eq!(map.label("logs", Action::PageDown), "unbound");
        assert_eq!(map.label("help", Action::PageDown), "alt-d");
        assert_eq!(
            map.action(
                "table",
                &KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
            ),
            None
        );
    }

    #[test]
    fn equivalent_key_spellings_cannot_bypass_conflict_checks() {
        for text in [
            "[keys.table]\npage_up = 'ctrl-D'",
            "[keys.table]\npage_up = 'shift-tab'",
            "[keys.table]\npage_up = 'shift-g'",
        ] {
            assert!(
                compile(text)
                    .unwrap_err()
                    .iter()
                    .any(|e| e.contains("conflicts")),
                "{text}"
            );
        }
        let map = compile("[keys.table]\npage_up = ['ctrl-u', 'control-U']").unwrap();
        assert_eq!(map.chords("table", Action::PageUp).len(), 1);
    }

    #[test]
    fn invalid_scopes_actions_and_chords_report_the_path() {
        for (text, path) in [
            ("[keys.table]\npaeg_up = 'f8'", "keys.table.paeg_up"),
            ("[keys.unknown]", "keys.unknown"),
            ("[keys.unknown]\npage_up = 'f8'", "keys.unknown"),
            ("[keys.input]\npage_up = 'f8'", "keys.input.page_up"),
            ("[keys.table]\npage_up = 'hyper-u'", "keys.table.page_up"),
            (
                "[keys.table]\npage_up = ['ctrl-u', 'shift--']",
                "keys.table.page_up",
            ),
        ] {
            assert!(
                compile(text).unwrap_err().iter().any(|e| e.contains(path)),
                "{text}"
            );
        }
    }

    #[test]
    fn unmigrated_palette_fields_warn_and_keep_scoped_settings() {
        let map = compile("[keys]\npalette_next = 'ctrl-n'\n[keys.command]\ndown = 'f8'").unwrap();
        assert_eq!(map.label("command", Action::Down), "f8");
        assert!(map.warnings()[0].contains("[keys.command] down"));
    }

    #[test]
    fn global_quit_and_compact_are_reassignable() {
        let map = compile(
            r#"
            [keys.global]
            quit = "alt-q"
            compact = []
            [keys.command]
            accept = "ctrl-c"
        "#,
        )
        .unwrap();
        assert_eq!(map.label("table", Action::Quit), "alt-q");
        assert_eq!(map.label("prompt", Action::Compact), "unbound");
        assert_eq!(map.label("command", Action::Accept), "ctrl-c");
    }
}
