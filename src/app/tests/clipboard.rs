use super::*;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "SOFKA_CLIPBOARD_TEST_CHILD";
const TEXT: &str = "a\né Ж 中 😀\n'\" $() `command` \\";

#[tokio::test]
async fn copy_key_selects_clipboard_tools_and_preserves_text() {
    if std::env::var_os(CHILD).is_some() {
        let (mut app, mut rx) = test_app();
        app.mode = Mode::Detail;
        app.detail = Scrollable {
            lines: TEXT.lines().map(str::to_owned).collect(),
            ..Default::default()
        };
        app.handle_key(press(KeyCode::Char('c'))).unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("clipboard timeout")
            .unwrap();
        assert!(matches!(&msg, Msg::ClipboardCopied { copied: true, .. }));
        app.handle_msg(msg);
        assert_eq!(app.flash, "copied 3 lines");
        assert!(!app.flash_err);
        return;
    }

    let directory = std::env::temp_dir().join(format!(
        "sofka-clipboard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let tools = ["clip.exe", "pbcopy", "wl-copy", "xclip", "xsel"];
    for (case, variable, clip_exists, succeeds, expected) in [
        (
            "interop",
            Some("WSL_INTEROP"),
            true,
            "clip.exe",
            "clip.exe\n",
        ),
        (
            "distro",
            Some("WSL_DISTRO_NAME"),
            true,
            "clip.exe",
            "clip.exe\n",
        ),
        ("native", None, true, "pbcopy", "pbcopy\n"),
        ("missing", Some("WSL_INTEROP"), false, "pbcopy", "pbcopy\n"),
        (
            "failed",
            Some("WSL_INTEROP"),
            true,
            "pbcopy",
            "clip.exe\npbcopy\n",
        ),
        (
            "wayland",
            Some("WSL_INTEROP"),
            true,
            "wl-copy",
            "clip.exe\npbcopy\nwl-copy\n",
        ),
        (
            "xclip",
            Some("WSL_INTEROP"),
            true,
            "xclip",
            "clip.exe\npbcopy\nwl-copy\nxclip\n",
        ),
        (
            "xsel",
            Some("WSL_INTEROP"),
            true,
            "xsel",
            "clip.exe\npbcopy\nwl-copy\nxclip\nxsel\n",
        ),
    ] {
        let path = directory.join(case);
        std::fs::create_dir(&path).unwrap();
        for tool in tools {
            if tool == "clip.exe" && !clip_exists {
                continue;
            }
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' '{tool}' >> \"$SOFKA_CLIPBOARD_TEST_DIR/order\"\nprintf '%s\\n' \"$@\" > \"$SOFKA_CLIPBOARD_TEST_DIR/{tool}.args\"\n/bin/cat > \"$SOFKA_CLIPBOARD_TEST_DIR/{tool}.input\"\nexit {}\n",
                if tool == succeeds { 0 } else { 1 }
            );
            let executable = path.join(tool);
            std::fs::write(&executable, script).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "app::tests::clipboard::copy_key_selects_clipboard_tools_and_preserves_text",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PATH", &path)
            .env("SOFKA_CLIPBOARD_TEST_DIR", &path)
            .env_remove("WSL_INTEROP")
            .env_remove("WSL_DISTRO_NAME")
            .kill_on_drop(true);
        if let Some(variable) = variable {
            command.env(variable, "test");
        }
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("child timeout")
            .unwrap();
        assert!(
            output.status.success(),
            "{case}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(path.join("order")).unwrap(),
            expected,
            "{case}"
        );
        for tool in expected.lines() {
            let input = std::fs::read(path.join(format!("{tool}.input"))).unwrap();
            if tool == "clip.exe" {
                assert_eq!(&input[..2], &[0xff, 0xfe]);
                assert_eq!(input.len() % 2, 0);
                let units: Vec<u16> = input[2..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .copied()
                    .map(u16::from_le_bytes)
                    .collect();
                assert_eq!(String::from_utf16(&units).unwrap(), TEXT);
            } else {
                assert_eq!(input, TEXT.as_bytes());
            }
            let args = std::fs::read_to_string(path.join(format!("{tool}.args"))).unwrap();
            assert_eq!(
                args,
                match tool {
                    "xclip" => "-selection\nclipboard\n",
                    "xsel" => "--clipboard\n--input\n",
                    _ => "\n",
                }
            );
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

const SELECTION_CHILD: &str = "SOFKA_SELECTION_COPY_TEST_CHILD";

/// Run the test `name` again in a child process whose `PATH` holds only a
/// `pbcopy` that records its input, so the copy reaches no real clipboard.
async fn run_with_recording_pbcopy(name: &str) {
    let directory = std::env::temp_dir().join(format!(
        "sofka-selection-copy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let pbcopy = directory.join("pbcopy");
    std::fs::write(
        &pbcopy,
        "#!/bin/sh\n/bin/cat > \"$SOFKA_CLIPBOARD_TEST_DIR/pbcopy.input\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&pbcopy, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(SELECTION_CHILD, "1")
        .env("PATH", &directory)
        .env("SOFKA_CLIPBOARD_TEST_DIR", &directory)
        .env_remove("WSL_INTEROP")
        .env_remove("WSL_DISTRO_NAME")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(15), output)
        .await
        .expect("child timeout")
        .unwrap();
    std::fs::remove_dir_all(&directory).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Press `code`, apply the clipboard result, and return the copied text.
async fn copy_with(app: &mut App, rx: &mut Receiver<Msg>, code: KeyCode) -> String {
    app.handle_key(press(code)).unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = rx.recv().await.expect("channel closed");
            if matches!(msg, Msg::ClipboardCopied { .. }) {
                break msg;
            }
        }
    })
    .await
    .expect("clipboard timeout");
    assert!(matches!(&msg, Msg::ClipboardCopied { copied: true, .. }));
    app.handle_msg(msg);
    let directory = std::env::var_os("SOFKA_CLIPBOARD_TEST_DIR").unwrap();
    std::fs::read_to_string(std::path::Path::new(&directory).join("pbcopy.input")).unwrap()
}

fn type_keys(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(press(KeyCode::Char(c))).unwrap();
    }
}

#[tokio::test]
async fn selected_log_lines_copy_raw_records_without_markers() {
    if std::env::var_os(SELECTION_CHILD).is_none() {
        run_with_recording_pbcopy(
            "app::tests::clipboard::selected_log_lines_copy_raw_records_without_markers",
        )
        .await;
        return;
    }
    let (mut app, mut rx) = test_app();
    app.mode = Mode::Logs;
    let long = format!("plain {}", "y".repeat(300));
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec![
            r#"{"level":"info","msg":"first"}"#.into(),
            r#"{"level":"error","msg":"second"}"#.into(),
        ],
    });
    type_keys(&mut app, "m");
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec![long.clone(), r#"{"msg":"fourth"}"#.into(), "after".into()],
    });
    type_keys(&mut app, "Jwg");
    assert_eq!(app.logs.json, JsonView::Record);

    type_keys(&mut app, "Vjjj");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('y')).await;
    assert_eq!(
        copied,
        format!(
            "{}\n{}\n{long}\n{}",
            r#"{"level":"info","msg":"first"}"#,
            r#"{"level":"error","msg":"second"}"#,
            r#"{"msg":"fourth"}"#
        )
    );
    assert_eq!(app.flash, "copied 4 log lines");
    assert!(!app.logs.selecting(), "copy ends the selection");
    assert_eq!(app.mode, Mode::Logs);

    type_keys(&mut app, "/!plain");
    app.handle_key(press(KeyCode::Enter)).unwrap();
    type_keys(&mut app, "gVG");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('c')).await;
    assert_eq!(
        copied,
        format!(
            "{}\n{}\n{}\nafter",
            r#"{"level":"info","msg":"first"}"#,
            r#"{"level":"error","msg":"second"}"#,
            r#"{"msg":"fourth"}"#
        ),
        "only the lines the filter shows"
    );
    assert_eq!(app.flash, "copied 4 log lines");
    assert!(!app.logs.selecting());
}

#[tokio::test]
async fn selected_document_lines_copy_whole_lines() {
    if std::env::var_os(SELECTION_CHILD).is_none() {
        run_with_recording_pbcopy(
            "app::tests::clipboard::selected_document_lines_copy_whole_lines",
        )
        .await;
        return;
    }
    let (mut app, mut rx) = test_app();
    let long = format!("b {}", "z".repeat(300));
    app.mode = Mode::Detail;
    app.detail = Scrollable {
        lines: vec!["a".into(), long.clone(), "c".into(), "d".into()].into(),
        ..Default::default()
    };
    type_keys(&mut app, "llj");
    assert!(app.detail.hscroll > 0);

    type_keys(&mut app, "Vj");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('y')).await;
    assert_eq!(copied, format!("{long}\nc"));
    assert_eq!(app.flash, "copied 2 lines");
    assert_eq!(app.detail.selection(), None, "copy ends the selection");
    assert_eq!(app.mode, Mode::Detail);

    type_keys(&mut app, "wgVj");
    assert!(app.detail.wrap);
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('y')).await;
    assert_eq!(copied, format!("a\n{long}"), "a wrapped line copies whole");
    assert_eq!(app.flash, "copied 2 lines");

    type_keys(&mut app, "/d");
    app.handle_key(press(KeyCode::Enter)).unwrap();
    type_keys(&mut app, "Vk");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('c')).await;
    assert_eq!(copied, "c\nd");
    assert_eq!(app.flash, "copied 2 lines");
    assert_eq!(app.detail.selection(), None);
}

#[tokio::test]
async fn selected_blank_lines_are_copied_and_counted() {
    if std::env::var_os(SELECTION_CHILD).is_none() {
        run_with_recording_pbcopy(
            "app::tests::clipboard::selected_blank_lines_are_copied_and_counted",
        )
        .await;
        return;
    }
    let (mut app, mut rx) = test_app();
    app.mode = Mode::Detail;
    app.detail = Scrollable {
        lines: vec!["first".into(), String::new(), "last".into()].into(),
        ..Default::default()
    };
    type_keys(&mut app, "Vj");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('y')).await;
    assert_eq!(copied, "first\n");
    assert_eq!(app.flash, "copied 2 lines");
    type_keys(&mut app, "Vjoj");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('y')).await;
    assert_eq!(copied, "");
    assert_eq!(app.flash, "copied 1 lines");

    app.mode = Mode::Logs;
    app.handle_msg(Msg::LogLines {
        generation: app.log_gen,
        lines: vec!["first".into(), String::new(), "last".into()],
    });
    type_keys(&mut app, "gVj");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('y')).await;
    assert_eq!(copied, "first\n");
    assert_eq!(app.flash, "copied 2 log lines");
    type_keys(&mut app, "Vjoj");
    let copied = copy_with(&mut app, &mut rx, KeyCode::Char('c')).await;
    assert_eq!(copied, "");
    assert_eq!(app.flash, "copied 1 log lines");
    assert!(!app.logs.selecting());
}
