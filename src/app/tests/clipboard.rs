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
