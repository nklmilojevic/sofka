#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Session {
    child: Child,
    master: File,
    original: libc::termios,
    output: Vec<u8>,
    dir: PathBuf,
}

impl Session {
    fn start(signal: i32, case: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "sofka-termination-{}-{signal}-{case}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("sofka")).unwrap();
        // No command can return credentials, so no API request can start.
        let script = r#"case "$KUBERNETES_EXEC_INFO" in *'"interactive":false'*) echo 'Enter MFA code' >&2; exit 1;; esac; echo $$ > "$PID_FILE"; echo COMMAND_READY >/dev/tty; read answer </dev/tty; exit 1"#;
        let kubeconfig = serde_json::json!({
            "apiVersion": "v1", "kind": "Config",
            "contexts": [{"name": "signal-test", "context": {"cluster": "test", "user": "test"}}],
            "clusters": [{"name": "test", "cluster": {"server": "https://127.0.0.1:1"}}],
            "users": [{"name": "test", "user": {"exec": {
                "apiVersion": "client.authentication.k8s.io/v1beta1",
                "command": "sh", "args": ["-c", script]
            }}}]
        });
        std::fs::write(dir.join("kubeconfig"), kubeconfig.to_string()).unwrap();
        std::fs::write(
            dir.join("sofka/config.toml"),
            format!(
                r#"mouse = true
update_check = false
[[plugins]]
key = "alt-g"
name = "signal-test"
command = "sh"
args = ["-c", {}]
target = "context"
mutating = false
output = "terminal"
"#,
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 140,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: The output pointers are valid. The size is initialized.
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
        // SAFETY: Each valid descriptor receives one owner.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: Both descriptors are valid.
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let original = termios(&master);
        let mut command = Command::new(env!("CARGO_BIN_EXE_sofka"));
        command
            .arg("contexts")
            .env("HOME", &dir)
            .env("XDG_CONFIG_HOME", &dir)
            .env("XDG_CACHE_HOME", &dir)
            .env("XDG_STATE_HOME", &dir)
            .env("KUBECONFIG", dir.join("kubeconfig"))
            .env("PID_FILE", dir.join("command.pid"))
            .env("TERM", "xterm-256color")
            .env_remove("SOFKA_COMPLETE")
            .env_remove("KUBERNETES_SERVICE_HOST")
            .env_remove("KUBERNETES_SERVICE_PORT")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: Only signal-safe system calls run before exec. The separate
        // session keeps all test signals away from the test runner.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master,
            original,
            output: Vec::new(),
            dir,
        }
    }

    fn read(&mut self) {
        let mut fd = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one valid descriptor and a bounded timeout.
        if unsafe { libc::poll(&mut fd, 1, 20) } > 0 {
            let mut buffer = [0; 8192];
            if let Ok(n) = self.master.read(&mut buffer) {
                self.output.extend_from_slice(&buffer[..n]);
            }
        }
    }

    fn expect(&mut self, marker: &[u8]) {
        let start = Instant::now();
        while !self
            .output
            .windows(marker.len())
            .any(|bytes| bytes == marker)
        {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "Missing {}: {}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&self.output)
            );
            self.read();
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn terminate(&mut self, signal: i32) {
        self.output.clear();
        // SAFETY: The signal is sent only to the child owned by this test.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) },
            0
        );
        let start = Instant::now();
        loop {
            self.read();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "Shutdown failed: {status}");
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "Shutdown did not finish: {}",
                String::from_utf8_lossy(&self.output)
            );
        }
        // These sequences must follow the signal, including during suspension.
        for marker in [b"\x1b[?1000l".as_slice(), b"\x1b[?1049l", b"\x1b[?25h"] {
            self.expect(marker);
        }
        for (enabled, disabled) in [
            (b"\x1b[?1000h".as_slice(), b"\x1b[?1000l".as_slice()),
            (b"\x1b[?1049h", b"\x1b[?1049l"),
            (b"\x1b[?25l", b"\x1b[?25h"),
        ] {
            let last = |marker: &[u8]| {
                self.output
                    .windows(marker.len())
                    .rposition(|bytes| bytes == marker)
            };
            assert!(
                last(disabled) > last(enabled),
                "Terminal mode was not restored"
            );
        }
        let restored = termios(&self.master);
        assert_eq!(restored.c_iflag, self.original.c_iflag);
        assert_eq!(restored.c_oflag, self.original.c_oflag);
        assert_eq!(restored.c_cflag, self.original.c_cflag);
        assert_eq!(restored.c_lflag, self.original.c_lflag);
        assert_eq!(restored.c_cc, self.original.c_cc);
        if let Ok(pid) = std::fs::read_to_string(self.dir.join("command.pid")) {
            let pid: libc::pid_t = pid.trim().parse().unwrap();
            // SAFETY: Signal zero checks whether the recorded child still exists.
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "Command still exists");
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
    }
}

fn termios(file: &File) -> libc::termios {
    // SAFETY: tcgetattr writes a valid termios structure for this terminal.
    unsafe {
        let mut value = std::mem::zeroed();
        assert_eq!(
            libc::tcgetattr(file.as_raw_fd(), &mut value),
            0,
            "{}",
            io::Error::last_os_error()
        );
        value
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: This process group contains only sofka and its test commands.
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn signals_restore_terminal_and_stop_suspended_commands() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        for case in ["tui", "plugin", "auth"] {
            let mut session = Session::start(signal, case);
            session.expect(b"signal-test");
            session.expect(b"\x1b[?1000h");
            assert_eq!(termios(&session.master).c_lflag & libc::ICANON, 0);
            match case {
                "plugin" => {
                    session.send(b"\x1b");
                    // Let the input reader finish the single Escape key.
                    std::thread::sleep(Duration::from_millis(100));
                    session.send(b"\x1bg");
                    session.expect(b"COMMAND_READY");
                }
                "auth" => {
                    session.send(b"\r");
                    session.expect(b"Run its auth plugin now?");
                    session.send(b"y");
                    session.expect(b"COMMAND_READY");
                }
                _ => {}
            }
            if case != "tui" {
                assert!(session.dir.join("command.pid").is_file());
            }
            session.terminate(signal);
        }
    }
}
