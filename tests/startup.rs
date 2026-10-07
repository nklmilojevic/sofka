use std::process::Command;

#[test]
fn headless_start_without_current_context_reports_selection_instructions() {
    let dir = std::env::temp_dir().join(format!("sofka-startup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kubeconfig");
    for current in ["", "current-context: ''\n"] {
        std::fs::write(
            &path,
            format!(
                "{current}apiVersion: v1\nkind: Config\ncontexts:\n- name: prod\n  context:\n    cluster: prod\n- name: staging\n  context:\n    cluster: prod\nclusters:\n- name: prod\n  cluster:\n    server: https://127.0.0.1:1\n"
            ),
        )
        .unwrap();
        for mode in ["--check", "--snapshot"] {
            let output = Command::new(env!("CARGO_BIN_EXE_sofka"))
                .arg(mode)
                .env("KUBECONFIG", &path)
                .env("HOME", &dir)
                .env("XDG_CONFIG_HOME", &dir)
                .env("XDG_CACHE_HOME", &dir)
                .env_remove("SOFKA_COMPLETE")
                .env_remove("KUBERNETES_SERVICE_HOST")
                .env_remove("KUBERNETES_SERVICE_PORT")
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(
                error.contains(&format!("no current-context in {}", path.display())),
                "{error}"
            );
            assert!(error.contains("--context <name>"), "{error}");
            assert!(error.contains("sofka ctx"), "{error}");
            assert!(!error.contains("in-cluster"), "{error}");
            assert!(!error.contains("is KUBECONFIG"), "{error}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A kubeconfig whose only user runs `script` through `sh` as its exec plugin.
#[cfg(unix)]
fn exec_kubeconfig(script: &str, env: &[(&str, &str)]) -> String {
    let env: String = env
        .iter()
        .map(|(name, value)| format!("      - name: {name}\n        value: '{value}'\n"))
        .collect();
    format!(
        r#"apiVersion: v1
kind: Config
current-context: eks
contexts:
- name: eks
  context:
    cluster: eks
    user: mfa
clusters:
- name: eks
  cluster:
    server: https://127.0.0.1:1
users:
- name: mfa
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: sh
      args:
      - -c
      - '{script}'
      env:
{env}"#
    )
}

/// Run `sofka --check` with a pty as its controlling terminal, so /dev/tty
/// exists. `input` is typed into the terminal up front, and stdin is the
/// terminal when `tty_stdin` is set. Returns the exit status and stderr.
#[cfg(unix)]
fn check_on_pty(
    dir: &std::path::Path,
    kubeconfig: &str,
    input: &[u8],
    tty_stdin: bool,
) -> (std::process::ExitStatus, String) {
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    let path = dir.join("kubeconfig");
    std::fs::write(&path, kubeconfig).unwrap();
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes the two descriptors; the null pointers skip the
    // optional name, termios and window size.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    if !input.is_empty() {
        // SAFETY: writing a live buffer to the master this test owns. The
        // line discipline holds it until something reads the terminal.
        let written = unsafe { libc::write(master, input.as_ptr().cast(), input.len()) };
        assert_eq!(written, input.len() as isize);
    }

    let stdin = if tty_stdin {
        // SAFETY: dup gives the Stdio its own descriptor to close.
        std::process::Stdio::from(unsafe { std::fs::File::from_raw_fd(libc::dup(slave)) })
    } else {
        std::process::Stdio::null()
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_sofka"));
    command
        .arg("--check")
        .env("KUBECONFIG", &path)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_CACHE_HOME", dir)
        .env_remove("SOFKA_COMPLETE")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .stdin(stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // SAFETY: the child has its own copy; the parent keeps only the master.
    unsafe { libc::close(slave) };
    // Read what the child writes to the terminal (the echoed input). A
    // session leader's exit waits for its terminal's output to drain, so an
    // unread master would hold sofka in exit forever. The read fails once the
    // last slave descriptor closes.
    // SAFETY: dup gives the reader its own descriptor to close.
    let mut echo = unsafe { std::fs::File::from_raw_fd(libc::dup(master)) };
    std::thread::spawn(move || std::io::copy(&mut echo, &mut std::io::sink()));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            // pre_exec made sofka a session and process group leader, so this
            // also stops a plugin still blocked on the terminal.
            // SAFETY: plain syscalls on ids and a descriptor this test owns.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                libc::close(master);
            }
            let output = child.wait_with_output().unwrap();
            let _ = std::fs::remove_dir_all(dir);
            panic!(
                "sofka hung on an exec plugin waiting for terminal input:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = child.wait_with_output().unwrap();
    // SAFETY: closing the master the parent still owns.
    unsafe { libc::close(master) };
    (status, String::from_utf8(output.stderr).unwrap())
}

/// An exec plugin that prompts on /dev/tty must fail instead of waiting for
/// input sofka can never give it. The child gets a pty as its controlling
/// terminal, so /dev/tty exists and the prompt would block without the fix.
/// Stdin is not a terminal, so sofka does not offer to run it interactively.
#[cfg(unix)]
#[test]
fn exec_plugin_prompting_on_the_terminal_fails_fast() {
    let dir = std::env::temp_dir().join(format!("sofka-exec-tty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let kubeconfig = exec_kubeconfig(
        r#"printf "Enter MFA code for arn:aws:iam::1:mfa/me: " >&2; read code </dev/tty; exit 1"#,
        &[],
    );
    let (status, error) = check_on_pty(&dir, &kubeconfig, b"", false);
    assert_eq!(status.code(), Some(1));
    assert!(error.contains("MFA code required"), "{error}");
    assert!(error.contains("read code </dev/tty"), "{error}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// At startup the terminal is still sofka's, so a plugin that wants an MFA
/// code runs attached to it, reads the code, and caches its credentials; the
/// detached run that follows then gets through authentication.
#[cfg(unix)]
#[test]
fn startup_runs_an_exec_plugin_that_needs_input_on_the_terminal() {
    let dir = std::env::temp_dir().join(format!("sofka-exec-auth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cache = dir.join("cache");
    let kubeconfig = exec_kubeconfig(
        r#"if [ -f "$CACHE" ]; then printf "{\"apiVersion\":\"client.authentication.k8s.io/v1beta1\",\"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\"}}"; exit 0; fi; printf "Enter MFA code for arn:aws:iam::1:mfa/me: " >&2; read code </dev/tty || exit 1; printf "%s" "$code" > "$CACHE"; printf "{\"apiVersion\":\"client.authentication.k8s.io/v1beta1\",\"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\"}}""#,
        &[("CACHE", cache.to_str().unwrap())],
    );
    let (status, error) = check_on_pty(&dir, &kubeconfig, b"123456\n", true);
    assert_eq!(status.code(), Some(1), "{error}");
    assert_eq!(std::fs::read_to_string(&cache).unwrap(), "123456");
    assert!(error.contains("needs input; running it"), "{error}");
    // Authenticated: what stops it now is the unreachable server.
    assert!(!error.contains("MFA code required"), "{error}");
    std::fs::remove_dir_all(dir).unwrap();
}
