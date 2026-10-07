use super::*;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::app::{App, Mode, Suspend};
use crate::config::Plugin;
use crate::k8s::Cluster;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

const CHILD_TEST: &str = "terminal::tests::terminal_plugin_child";
const COMMAND_TEST: &str = "terminal::tests::terminal_plugin_command";
const CASE_ENV: &str = "SOFKA_TERMINAL_TEST_CASE";
const AUTH_TEST: &str = "terminal::tests::exec_auth_child";

struct Session {
    child: Child,
    master: Option<File>,
    /// Output read past the last marker, which the next `expect` starts from:
    /// a child can print two markers in one write.
    unread: Vec<u8>,
}

impl Session {
    fn start(case: &str) -> Self {
        Self::start_test(CHILD_TEST, case, &[])
    }

    fn start_test(test: &str, case: &str, env: &[(&str, &std::path::Path)]) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes two file descriptors to valid pointers.
        // Null name and termios pointers request the default terminal setup.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        // SAFETY: Each descriptor is valid and receives one owner.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: F_SETFD accepts FD_CLOEXEC for these valid descriptors.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(CASE_ENV, case)
            .envs(env.iter().copied())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: The child only calls signal-safe system functions before exec.
        // Its new session confines terminal signals to this test and its child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_CORE, &limit) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master: Some(master),
            unread: Vec::new(),
        }
    }

    fn expect(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut output = std::mem::take(&mut self.unread);
        if self.consume(&mut output, marker) {
            return;
        }
        while Instant::now() < deadline {
            let mut fd = libc::pollfd {
                fd: self.master.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one valid pollfd and a bounded timeout.
            let ready = unsafe { libc::poll(&mut fd, 1, 100) };
            if ready > 0 {
                let mut buffer = [0; 4096];
                match self.master.as_mut().unwrap().read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
                if self.consume(&mut output, marker) {
                    return;
                }
            }
        }
        panic!("missing {marker}: {}", String::from_utf8_lossy(&output));
    }

    /// Whether `output` holds `marker`; what follows it is kept for later.
    fn consume(&mut self, output: &mut Vec<u8>, marker: &str) -> bool {
        let Some(at) = output
            .windows(marker.len())
            .position(|window| window == marker.as_bytes())
        else {
            return false;
        };
        self.unread = output.split_off(at + marker.len());
        true
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.as_mut().unwrap().write_all(bytes).unwrap();
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "test process failed: {status}");
                return;
            }
            let mut fd = libc::pollfd {
                fd: self.master.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one valid pollfd. Drain the final output
            // so that terminal cleanup can finish on macOS.
            if unsafe { libc::poll(&mut fd, 1, 10) } > 0 {
                let _ = self.master.as_mut().unwrap().read(&mut [0; 4096]);
            }
        }
        panic!("test process did not exit");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.master.take());
        // SAFETY: The test process owns this separate process group. Also stop
        // any command left in the group if an assertion fails.
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

#[test]
fn terminal_plugin_restores_tui_after_exit_interrupt_quit_and_spawn_error() {
    for case in [
        "exit",
        "failure",
        "descendant",
        "interrupt",
        "quit",
        "missing",
    ] {
        let mut session = Session::start(case);
        for _ in 0..2 {
            session.expect("TUI_READY");
            session.send(b"\x1bg");
            if case != "missing" {
                session.expect("PLUGIN_READY");
                session.send(match case {
                    "interrupt" => b"\x03",
                    "quit" => b"\x1c",
                    _ => b"done\n",
                });
            }
            session.expect("TUI_RESUMED");
            session.send(b"?");
            session.expect("INPUT_OK");
            session.send(b"\x1b");
        }
        session.expect("ALL_DONE");
        session.finish();
    }
}

fn signal_action(signal: libc::c_int) -> libc::sigaction {
    // SAFETY: A null action pointer queries the current action into valid memory.
    unsafe {
        let mut action = std::mem::zeroed();
        assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut action), 0);
        action
    }
}

fn read_key() -> KeyEvent {
    loop {
        assert!(crossterm::event::poll(Duration::from_secs(10)).unwrap());
        if let Event::Key(key) = crossterm::event::read().unwrap() {
            return key;
        }
    }
}

#[test]
fn terminal_plugin_command() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    // Check the dispositions after exec, before the parent can send a signal.
    for signal in [libc::SIGINT, libc::SIGQUIT] {
        assert_eq!(signal_action(signal).sa_sigaction, libc::SIG_DFL);
    }
    println!("PLUGIN_READY");
    io::stdout().flush().unwrap();
    if matches!(case.as_str(), "exit" | "failure" | "descendant") {
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).unwrap();
        assert_eq!(answer.trim_end(), "done");
        if case == "descendant" {
            let _child = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            eprintln!("descendant still has stderr open");
            std::process::exit(7);
        }
        if case == "failure" {
            eprintln!("exec: \"sh\": executable file not found in $PATH");
            std::process::exit(7);
        }
    } else {
        std::thread::sleep(Duration::from_secs(30));
        panic!("terminal interrupt did not stop the command");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_plugin_child() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let mut app = App::new(Cluster::fake(), tx);
    app.plugins = vec![Plugin {
        key: "alt-g".into(),
        name: "terminal-test".into(),
        command: if case == "missing" {
            "/sofka-test-command-does-not-exist".into()
        } else {
            std::env::current_exe().unwrap().to_str().unwrap().into()
        },
        args: vec!["--exact".into(), COMMAND_TEST.into(), "--nocapture".into()],
        target: Some("context".into()),
        mutating: Some(false),
        output: Some("terminal".into()),
        ..Default::default()
    }];
    let mut terminal = ratatui::init();
    let before = [signal_action(libc::SIGINT), signal_action(libc::SIGQUIT)];
    for captured in [false, true] {
        if captured {
            crossterm::execute!(io::stdout(), EnableMouseCapture).unwrap();
        }
        println!("TUI_READY");
        let key = read_key();
        assert_eq!(key, KeyEvent::new(KeyCode::Char('g'), KeyModifiers::ALT));
        app.handle_key(key).unwrap();
        let Some(Suspend::Shell(argv)) = app.pending.take() else {
            panic!("plugin did not queue a terminal command: {}", app.flash);
        };
        let result = suspend_and_run(&mut terminal, &argv, captured);
        assert_eq!(result.is_err(), case != "exit");
        if case == "failure" {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("executable file not found"), "{error}");
            assert!(error.contains("7"), "{error}");
        }
        app.after_suspend();
        assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
        for (signal, previous) in [libc::SIGINT, libc::SIGQUIT].into_iter().zip(&before) {
            let restored = signal_action(signal);
            assert_eq!(restored.sa_sigaction, previous.sa_sigaction);
            // The kernel can add internal flags when an action is restored.
            let flags = libc::SA_RESTART
                | libc::SA_SIGINFO
                | libc::SA_NODEFER
                | libc::SA_RESETHAND
                | libc::SA_ONSTACK;
            assert_eq!(restored.sa_flags & flags, previous.sa_flags & flags);
            for member in [libc::SIGINT, libc::SIGQUIT] {
                // SAFETY: Both masks came from successful sigaction queries.
                unsafe {
                    assert_eq!(
                        libc::sigismember(&restored.sa_mask, member),
                        libc::sigismember(&previous.sa_mask, member),
                    );
                }
            }
        }
        println!("TUI_RESUMED");
        app.handle_key(read_key()).unwrap();
        assert_eq!(app.mode, Mode::Help);
        assert!(!app.should_quit);
        println!("INPUT_OK");
        app.handle_key(read_key()).unwrap();
        assert_eq!(app.mode, Mode::Table);
    }
    crossterm::execute!(io::stdout(), DisableMouseCapture).unwrap();
    ratatui::restore();
    println!("ALL_DONE");
}

#[tokio::test]
async fn stderr_drain_keeps_ready_tail_after_deadline() {
    let final_error = b"exec: \"sh\": executable file not found in $PATH\n";
    let mut bytes = vec![b'x'; ERROR_LIMIT * 4];
    bytes.extend_from_slice(final_error);
    let mut reader = bytes.as_slice();
    let mut tail = Vec::new();
    let mut output = Vec::new();
    drain_stderr(
        &mut reader,
        &mut tail,
        &mut output,
        tokio::time::Instant::now(),
    )
    .await;
    assert_eq!(output, bytes);
    assert_eq!(tail.len(), ERROR_LIMIT);
    assert!(tail.ends_with(final_error));
}

#[tokio::test]
async fn stderr_drain_stops_for_idle_or_continuously_writing_descendants() {
    let (mut reader, _held_open) = tokio::io::duplex(4096);
    let mut tail = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(1),
        drain_stderr(
            &mut reader,
            &mut tail,
            &mut io::sink(),
            tokio::time::Instant::now(),
        ),
    )
    .await
    .unwrap();
    assert!(tail.is_empty());
    let mut reader = tokio::io::repeat(b'x');
    let mut output = Vec::new();
    drain_stderr(
        &mut reader,
        &mut tail,
        &mut output,
        tokio::time::Instant::now(),
    )
    .await;
    assert_eq!(output.len(), 1024 * 1024);
    assert_eq!(tail.len(), ERROR_LIMIT);
}

/// The exec plugin the auth test runs: it prompts on the terminal like the
/// AWS CLI does for an MFA code, and caches the code it accepts.
const AUTH_PLUGIN: &str = r#"printf "Enter MFA code: " >/dev/tty; read code </dev/tty || exit 1; [ "$code" = 123456 ] || { echo "wrong code" >&2; exit 1; }; printf "%s" "$code" > "$CACHE"; printf "{\"apiVersion\":\"client.authentication.k8s.io/v1beta1\",\"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\"}}""#;

/// Accepting the offer a watch error makes suspends the TUI, runs the exec
/// plugin on the real terminal where it reads the code, and restores the TUI
/// whether the plugin succeeds, rejects the code, or is interrupted.
#[test]
fn exec_auth_runs_the_plugin_on_the_terminal_and_restores_the_tui() {
    for case in ["ok", "wrong", "interrupt"] {
        let dir = std::env::temp_dir().join(format!("sofka-auth-{case}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let kubeconfig = dir.join("kubeconfig");
        let config = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Config",
            "current-context": "test",
            "contexts": [{"name": "test", "context": {"cluster": "test", "user": "mfa"}}],
            "clusters": [{"name": "test", "cluster": {"server": "https://127.0.0.1:1"}}],
            "users": [{"name": "mfa", "user": {"exec": {
                "apiVersion": "client.authentication.k8s.io/v1beta1",
                "command": "sh",
                "args": ["-c", AUTH_PLUGIN],
                "env": [{"name": "CACHE", "value": dir.join("cache")}]
            }}}]
        });
        std::fs::write(&kubeconfig, serde_yaml::to_string(&config).unwrap()).unwrap();
        let mut session = Session::start_test(AUTH_TEST, case, &[("KUBECONFIG", &kubeconfig)]);
        session.expect("TUI_READY");
        session.send(b"y");
        session.expect("Enter MFA code");
        session.send(match case {
            "ok" => b"123456\n",
            "wrong" => b"000000\n",
            _ => b"\x03",
        });
        session.expect("TUI_RESUMED");
        session.send(b"?");
        session.expect("INPUT_OK");
        session.expect("ALL_DONE");
        session.finish();
        let cache = std::fs::read_to_string(dir.join("cache")).ok();
        assert_eq!(
            cache.as_deref(),
            (case == "ok").then_some("123456"),
            "{case}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_auth_child() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let mut app = App::new(Cluster::fake(), tx);
    app.switch_kind("pods");
    let mut terminal = ratatui::init();
    let before = [signal_action(libc::SIGINT), signal_action(libc::SIGQUIT)];
    let generation = app.generation;
    app.handle_msg(crate::store::Msg::WatchError {
        generation: app.generation,
        error: "MFA code required".into(),
        failure: crate::store::WatchFailure::NeedsInput,
    });
    assert_eq!(app.mode, Mode::Confirm);
    println!("TUI_READY");
    app.handle_key(read_key()).unwrap();
    let Some(Suspend::Authenticate { context, switch }) = app.pending.take() else {
        panic!("accepting did not queue authentication: {}", app.flash);
    };
    authenticate(&mut terminal, &mut app, context, switch, false);

    assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
    for (signal, previous) in [libc::SIGINT, libc::SIGQUIT].into_iter().zip(&before) {
        assert_eq!(signal_action(signal).sa_sigaction, previous.sa_sigaction);
    }
    assert_eq!(app.mode, Mode::Table);
    match case.as_str() {
        "ok" => {
            assert_eq!(app.flash, "authenticated");
            assert!(app.generation > generation, "watch not restarted");
        }
        "wrong" => assert!(
            app.flash.contains("authentication failed") && app.flash.contains("exit status: 1"),
            "{}",
            app.flash
        ),
        _ => assert!(
            app.flash.contains("authentication failed") && app.flash.contains("signal"),
            "{}",
            app.flash
        ),
    }
    println!("TUI_RESUMED");
    app.handle_key(read_key()).unwrap();
    assert_eq!(app.mode, Mode::Help);
    println!("INPUT_OK");
    ratatui::restore();
    println!("ALL_DONE");
}
