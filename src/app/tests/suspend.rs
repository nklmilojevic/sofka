use super::*;

/// The `ctrl-z` remap from docs/keybindings.md.
const CTRL_Z_RECIPE: &str = r#"
[keys.global]
suspend = "ctrl-z"

[keys.table]
faults = "Z"

[keys.logs]
log_warnings = "Z"

[keys.events]
faults = "Z"
"#;

fn keymap(text: &str) -> Result<Keymap, Vec<String>> {
    let cfg: crate::config::Config = toml::from_str(text).unwrap();
    Keymap::compile(&cfg.keys)
}

fn suspend_app() -> (App, Receiver<Msg>) {
    let (mut app, rx) = test_app();
    app.keymap = keymap(CTRL_Z_RECIPE).unwrap();
    app.job_control = true;
    app.switch_kind("pods");
    (app, rx)
}

fn assert_stop_queued(app: &mut App, mode: Mode) {
    app.handle_key(ctrl(KeyCode::Char('z'))).unwrap();
    assert!(
        matches!(app.pending.take(), Some(Suspend::Stop)),
        "{mode:?}: suspend did not queue a stop"
    );
    assert_eq!(app.mode, mode);
}

#[tokio::test]
async fn suspend_is_unbound_by_default_and_ctrl_z_keeps_faults() {
    let (mut app, _rx) = test_app();
    app.job_control = true;
    app.switch_kind("pods");
    for scope in ["table", "logs", "filter", "command", "help"] {
        assert_eq!(app.keymap.label(scope, Action::Suspend), "unbound");
    }
    app.handle_key(ctrl(KeyCode::Char('z'))).unwrap();
    assert!(app.pending.is_none());
    assert!(app.faults_only);
}

#[tokio::test]
async fn bound_suspend_queues_a_stop_from_every_mode() {
    let (mut app, _rx) = suspend_app();
    assert_stop_queued(&mut app, Mode::Table);
    assert!(!app.faults_only);

    app.handle_key(press(KeyCode::Char('/'))).unwrap();
    app.handle_key(press(KeyCode::Char('w'))).unwrap();
    assert_stop_queued(&mut app, Mode::Filter);
    assert_eq!(app.filter, "w");
    app.handle_key(press(KeyCode::Esc)).unwrap();

    app.handle_key(press(KeyCode::Char(':'))).unwrap();
    assert_stop_queued(&mut app, Mode::Command);
    app.handle_key(press(KeyCode::Esc)).unwrap();

    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    assert_stop_queued(&mut app, Mode::Help);
    app.handle_key(press(KeyCode::Esc)).unwrap();

    app.mode = Mode::Logs;
    assert_stop_queued(&mut app, Mode::Logs);
    assert!(!app.logs.warnings_only);
    app.mode = Mode::Table;

    open_pod_events(&mut app);
    assert_stop_queued(&mut app, Mode::Events);
    assert!(!app.events_faults_only);
    app.handle_key(press(KeyCode::Esc)).unwrap();

    app.handle_command_result(None, Err(std::io::Error::other("exit status: 1")), None);
    assert!(app.command_failure_visible());
    assert_stop_queued(&mut app, Mode::Table);
    assert!(app.command_failure_visible(), "suspend dismissed the error");
}

#[tokio::test]
async fn ctrl_z_recipe_moves_faults_and_log_warnings_to_shift_z() {
    let (mut app, _rx) = suspend_app();
    app.handle_key(press(KeyCode::Char('Z'))).unwrap();
    assert!(app.faults_only);
    assert!(app.pending.is_none());
    app.mode = Mode::Logs;
    app.handle_key(press(KeyCode::Char('Z'))).unwrap();
    assert!(app.logs.warnings_only);
    assert!(app.pending.is_none());
    app.mode = Mode::Table;
    // The faults filter would hide the healthy pod the events open from.
    app.handle_key(press(KeyCode::Char('Z'))).unwrap();
    assert!(!app.faults_only);
    open_pod_events(&mut app);
    app.handle_key(press(KeyCode::Char('Z'))).unwrap();
    assert!(app.events_faults_only);
    assert!(app.pending.is_none());
}

#[test]
fn suspend_on_ctrl_z_without_moving_faults_reports_conflicts() {
    let errors = keymap("[keys.global]\nsuspend = 'ctrl-z'").unwrap_err();
    for expected in [
        "keys.table: ctrl-z conflicts between faults and suspend",
        "keys.logs: ctrl-z conflicts between log_warnings and suspend",
        "keys.events: ctrl-z conflicts between faults and suspend",
    ] {
        assert!(errors.iter().any(|e| e == expected), "{errors:?}");
    }
}

#[tokio::test]
async fn suspend_without_job_control_only_flashes() {
    let (mut app, _rx) = suspend_app();
    app.job_control = false;
    app.handle_key(ctrl(KeyCode::Char('z'))).unwrap();
    assert!(app.pending.is_none());
    assert_eq!(app.mode, Mode::Table);
    assert_eq!(app.flash, "suspend is not supported on Windows");
    assert!(app.flash_err);
    assert!(!app.faults_only);
}

#[tokio::test]
async fn help_lists_suspend_with_its_binding() {
    use ratatui::{Terminal, backend::TestBackend};

    let (mut app, _rx) = suspend_app();
    app.handle_key(press(KeyCode::Char('?'))).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(200, 400)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();
    let row = rows
        .iter()
        .find(|row| row.contains(Action::Suspend.description()));
    assert!(row.is_some_and(|row| row.contains("ctrl-z")), "{rows:#?}");
}
