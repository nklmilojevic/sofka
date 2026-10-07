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

/// An exec plugin that prompts on /dev/tty must fail instead of waiting for
/// input sofka can never give it. The child gets a pty as its controlling
/// terminal, so /dev/tty exists and the prompt would block without the fix.
#[cfg(unix)]
#[test]
fn exec_plugin_prompting_on_the_terminal_fails_fast() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    let dir = std::env::temp_dir().join(format!("sofka-exec-tty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kubeconfig");
    std::fs::write(
        &path,
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
      - 'printf "Enter MFA code for arn:aws:iam::1:mfa/me: " >&2; read code </dev/tty; exit 1'
"#,
    )
    .unwrap();

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

    let mut command = Command::new(env!("CARGO_BIN_EXE_sofka"));
    command
        .arg("--check")
        .env("KUBECONFIG", &path)
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", &dir)
        .env("XDG_CACHE_HOME", &dir)
        .env_remove("SOFKA_COMPLETE")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .stdin(std::process::Stdio::null())
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
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("sofka hung on an exec plugin waiting for terminal input");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = child.wait_with_output().unwrap();
    // SAFETY: closing the master the parent still owns.
    unsafe { libc::close(master) };

    assert_eq!(status.code(), Some(1));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("MFA code required"), "{error}");
    assert!(error.contains("read code </dev/tty"), "{error}");
    std::fs::remove_dir_all(dir).unwrap();
}
