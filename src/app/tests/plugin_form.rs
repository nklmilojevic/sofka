use super::*;
use ratatui::{Terminal, backend::TestBackend};

fn form_plugin(inputs: &str) -> crate::config::Plugin {
    let mut plugin = named_plugin("echo", &["${input.port}", "${input.scan}"]);
    plugin.key = "ctrl-g".into();
    plugin.inputs = toml::from_str(inputs).unwrap();
    plugin
}

const INPUTS: &str = r#"
    [port]
    type = "integer"
    min = 1
    max = 65535
    [scan]
    type = "string"
    default = "all"
    choices = ["all", "misconfig"]
"#;

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
}

fn chord(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .unwrap();
}

fn shell_argv(app: &mut App) -> Vec<String> {
    match app.pending.take() {
        Some(Suspend::Shell(argv)) => argv,
        _ => panic!("plugin did not run"),
    }
}

#[tokio::test]
async fn key_chord_with_a_required_input_opens_the_form_and_runs_its_values() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![form_plugin(INPUTS)];

    chord(&mut app);
    assert_eq!(app.mode, Mode::PluginForm);
    assert!(
        app.pending.is_none(),
        "must not run before the form is sent"
    );
    let form = app.plugin_form.as_ref().unwrap();
    assert_eq!(form.fields[0].value, "", "no default");
    assert_eq!(form.fields[1].value, "all", "prefilled with the default");

    typed(&mut app, "80 80");
    assert_eq!(app.plugin_form.as_ref().unwrap().fields[0].value, "8080");
    app.handle_key(press(KeyCode::Tab)).unwrap();
    app.handle_key(press(KeyCode::Right)).unwrap();
    typed(&mut app, "x");
    assert_eq!(
        app.plugin_form.as_ref().unwrap().fields[1].value,
        "misconfig",
        "arrows cycle choices and typing leaves them alone"
    );
    app.handle_key(press(KeyCode::Enter)).unwrap();

    assert_eq!(app.mode, Mode::Table);
    assert!(app.plugin_form.is_none());
    assert_eq!(shell_argv(&mut app), ["echo", "8080", "misconfig"]);
}

#[tokio::test]
async fn plugin_form_shows_validation_errors_on_the_field_and_keeps_the_values() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![form_plugin(INPUTS)];
    plugin_command(&mut app, "example-plugin");
    assert_eq!(app.mode, Mode::PluginForm);

    app.handle_key(press(KeyCode::Tab)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(
        app.mode,
        Mode::PluginForm,
        "invalid values keep the form open"
    );
    assert!(app.pending.is_none());
    let form = app.plugin_form.as_ref().unwrap();
    assert_eq!(form.focus, 0, "focus moves to the first invalid field");
    assert!(
        form.fields[0]
            .error
            .as_deref()
            .is_some_and(|e| e == "required"),
        "{:?}",
        form.fields[0].error
    );
    assert!(form.fields[1].error.is_none());

    typed(&mut app, "70000");
    assert!(
        app.plugin_form.as_ref().unwrap().fields[0].error.is_none(),
        "editing clears it"
    );
    app.handle_key(press(KeyCode::Enter)).unwrap();
    let form = app.plugin_form.as_ref().unwrap();
    assert!(
        form.fields[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("range"))
    );

    app.handle_key(press(KeyCode::Backspace)).unwrap();
    app.handle_key(press(KeyCode::Backspace)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(shell_argv(&mut app), ["echo", "700", "all"]);
}

#[tokio::test]
async fn plugin_form_cancel_runs_nothing_and_arguments_skip_the_form() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![form_plugin(INPUTS)];
    chord(&mut app);
    typed(&mut app, "80");
    app.handle_key(press(KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Table);
    assert!(app.plugin_form.is_none());
    assert!(app.pending.is_none());

    plugin_command(&mut app, "example-plugin scan=misconfig");
    assert_ne!(app.mode, Mode::PluginForm, "arguments never open the form");
    assert!(app.pending.is_none());
    assert!(app.flash.contains("missing input port"), "{}", app.flash);

    plugin_command(&mut app, "example-plugin port=443");
    assert_eq!(shell_argv(&mut app), ["echo", "443", "all"]);
}

#[tokio::test]
async fn plugin_form_opens_for_defaults_only_when_the_command_asks_for_it() {
    let (mut app, _rx) = app_with_pod();
    let inputs = r#"
        [port]
        type = "integer"
        default = "80"
        [scan]
        type = "boolean"
        default = "false"
    "#;
    app.plugins = vec![form_plugin(inputs)];
    chord(&mut app);
    assert_eq!(
        shell_argv(&mut app),
        ["echo", "80", "false"],
        "defaults run at once"
    );

    app.plugins[0].prompt = Some("always".into());
    chord(&mut app);
    assert_eq!(app.mode, Mode::PluginForm);
    app.handle_key(press(KeyCode::BackTab)).unwrap();
    app.handle_key(press(KeyCode::Left)).unwrap();
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(
        shell_argv(&mut app),
        ["echo", "80", "true"],
        "booleans cycle"
    );
}

#[tokio::test]
async fn plugin_form_is_gated_before_it_opens_and_confirms_after_it_is_sent() {
    let (mut app, _rx) = app_with_pod();
    let mut plugin = form_plugin(INPUTS);
    plugin.mutating = None;
    app.plugins = vec![plugin];
    app.readonly = true;
    chord(&mut app);
    assert_ne!(
        app.mode,
        Mode::PluginForm,
        "read-only blocks before the form"
    );
    assert!(app.flash.contains("read-only"), "{}", app.flash);

    app.readonly = false;
    app.plugins[0].dangerous = true;
    chord(&mut app);
    typed(&mut app, "8080");
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::Confirm);
    assert!(app.pending.is_none(), "must not run before confirmation");
    assert!(
        app.confirm_label.contains("port=8080"),
        "{}",
        app.confirm_label
    );
    app.handle_key(press(KeyCode::Char('y'))).unwrap();
    assert_eq!(shell_argv(&mut app), ["echo", "8080", "all"]);
}

#[tokio::test]
async fn plugin_form_renders_every_field_with_its_type_and_error() {
    let (mut app, _rx) = app_with_pod();
    app.plugins = vec![form_plugin(INPUTS)];
    chord(&mut app);
    app.handle_key(press(KeyCode::Enter)).unwrap();

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen = (0..30)
        .map(|y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains(" Example "), "{screen}");
    assert!(screen.contains("integer 1..65535, required"), "{screen}");
    assert!(screen.contains("‹ all ›"), "{screen}");
    assert!(
        screen
            .lines()
            .any(|line| line.contains("required") && !line.contains("integer")),
        "the error has its own row: {screen}"
    );
    assert!(screen.contains("enter:run"), "{screen}");
}

#[tokio::test]
async fn plugin_form_rejects_an_untouched_required_string() {
    let (mut app, _rx) = app_with_pod();
    let mut plugin = named_plugin("echo", &["${input.target}"]);
    plugin.inputs = toml::from_str("[target]\ntype = \"string\"").unwrap();
    app.plugins = vec![plugin];
    plugin_command(&mut app, "example-plugin");
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(app.mode, Mode::PluginForm);
    assert!(
        app.pending.is_none(),
        "an empty required input must not run"
    );
    let form = app.plugin_form.as_ref().unwrap();
    assert_eq!(form.fields[0].error.as_deref(), Some("required"));

    typed(&mut app, "web");
    app.handle_key(press(KeyCode::Enter)).unwrap();
    assert_eq!(shell_argv(&mut app), ["echo", "web"]);
}

#[tokio::test]
async fn plugin_form_keeps_a_wrapped_focused_field_visible() {
    let (mut app, _rx) = app_with_pod();
    let mut plugin = named_plugin("echo", &[]);
    let inputs = (0..8)
        .map(|n| {
            format!(
                "[input_{n}]\ntype = \"string\"\ndefault = \"{}\"\n",
                "long-value-".repeat(8)
            )
        })
        .collect::<String>();
    plugin.inputs = toml::from_str(&inputs).unwrap();
    plugin.prompt = Some("always".into());
    app.plugins = vec![plugin];
    plugin_command(&mut app, "example-plugin");
    app.handle_key(press(KeyCode::BackTab)).unwrap();
    assert_eq!(app.plugin_form.as_ref().unwrap().focus, 7);

    let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
    terminal
        .draw(|frame| crate::ui::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen = (0..24)
        .map(|y| (0..70).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let focused = screen
        .lines()
        .position(|line| line.contains("▸ input_7"))
        .unwrap_or_else(|| panic!("focused field hidden: {screen}"));
    assert!(
        screen
            .lines()
            .skip(focused)
            .any(|line| line.contains("string")),
        "its wrapped hint stays visible: {screen}"
    );
    assert!(
        !screen.contains("input_0"),
        "earlier fields scroll away: {screen}"
    );
}

#[test]
fn plugin_prompt_accepts_missing_or_always() {
    let mut plugin = form_plugin(INPUTS);
    for value in ["missing", "always"] {
        plugin.prompt = Some(value.into());
        assert!(crate::config::plugin_warnings(std::slice::from_ref(&plugin)).is_empty());
    }
    plugin.prompt = Some("never".into());
    let warnings = crate::config::plugin_warnings(std::slice::from_ref(&plugin));
    assert!(warnings[0].contains("unknown prompt"), "{warnings:?}");
}
