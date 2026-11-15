use std::io::{self, Write};
use std::process::Stdio;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Receive termination signals before the TUI starts to read input.
pub struct Termination {
    requested: bool,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
}

impl Termination {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            requested: false,
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(unix)]
            hup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
        })
    }

    pub fn requested(&self) -> bool {
        self.requested
    }

    pub async fn wait(&mut self) {
        if self.requested {
            return;
        }
        #[cfg(unix)]
        tokio::select! {
            _ = self.term.recv() => {},
            _ = self.hup.recv() => {},
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
        self.requested = true;
    }
}

/// Suspend the TUI for an interactive command, then restore its terminal modes.
/// Call this only from the main loop, with the current mouse capture state.
pub async fn suspend_and_run(
    terminal: &mut ratatui::DefaultTerminal,
    argv: &[String],
    captured: bool,
    termination: &mut Termination,
) -> io::Result<()> {
    if argv.is_empty() {
        return Ok(());
    }
    suspend_and_await(terminal, captured, run_command_async(argv, termination)).await?
}

/// Run `context`'s exec auth plugin with the TUI suspended so it can prompt,
/// then hand the outcome to the app, which retries the connection.
pub async fn authenticate(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut crate::app::App,
    context: String,
    switch: bool,
    captured: bool,
    termination: &mut Termination,
) {
    let result = suspend_and_await(
        terminal,
        captured,
        crate::k8s::authenticate_interactively_until(Some(&context), termination.wait()),
    )
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|e| format!("{e:#}"));
    if !termination.requested() {
        app.authenticated(context, switch, result);
    }
}

/// Suspend the TUI, give the terminal to whatever `future` runs, then restore
/// the terminal modes. Use the same rules as [`suspend_and_run`].
pub async fn suspend_and_await<T>(
    terminal: &mut ratatui::DefaultTerminal,
    captured: bool,
    future: impl std::future::Future<Output = T>,
) -> io::Result<T> {
    // Protect the parent before normal terminal input can generate signals.
    #[cfg(unix)]
    let _signals = SignalGuard::new()?;
    release(captured);
    let result = future.await;
    restore(terminal, captured);
    Ok(result)
}

/// How long [`stop`] waits for a stop that `kill` did not apply before it
/// returned. An orphaned process group discards `SIGTSTP`, and the TUI must
/// come back then too.
#[cfg(unix)]
const STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// How often a job resumed in the background checks whether it has the
/// terminal again. bash's `fg` sends no `SIGCONT` to a job that is running.
#[cfg(unix)]
const FOREGROUND_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Release the terminal and stop with `SIGTSTP`, as the terminal's suspend key
/// would, then take the terminal back once the process runs in the foreground
/// again. Use the same rules as [`suspend_and_run`].
///
/// The signal goes to the whole process group, like the one the terminal
/// sends: `kubectl port-forward` children pause too, and a wrapper such as
/// `cargo run` stops with sofka, so the shell sees the job stop.
#[cfg(unix)]
pub async fn stop(terminal: &mut ratatui::DefaultTerminal, captured: bool) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    if signal_action(libc::SIGTSTP)?.sa_sigaction == libc::SIG_IGN {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SIGTSTP is ignored, so this shell has no job control",
        ));
    }
    // Listen first: the process can stop and resume before `kill` returns.
    let mut resumed = signal(SignalKind::from_raw(libc::SIGCONT))?;
    // Job control hands out the controlling terminal, whichever standard
    // streams are redirected. Without one there is no `fg` to wait for.
    let controlling = std::fs::File::open("/dev/tty").ok();
    let signals = SignalGuard::new()?;
    release(captured);
    // SAFETY: kill has no memory preconditions. Process group 0 is this
    // process's own group.
    let sent = if unsafe { libc::kill(0, libc::SIGTSTP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    if sent.is_ok() {
        // Another thread can take the stop after `kill` returns.
        let _ = tokio::time::timeout(STOP_GRACE, resumed.recv()).await;
        // `bg` resumes the job while the shell keeps the terminal. Restoring
        // then would change the shell's terminal modes, so wait for `fg`.
        while controlling.as_ref().is_some_and(|tty| !foreground(tty)) {
            let _ = tokio::time::timeout(FOREGROUND_POLL, resumed.recv()).await;
        }
    }
    restore(terminal, captured);
    drop(signals);
    sent
}

/// Hand the terminal back to the shell: no mouse capture, cooked input, the
/// main screen, and a visible cursor.
fn release(captured: bool) {
    if captured {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Take the terminal back after [`release`] and repaint all of it on the next
/// draw.
fn restore(terminal: &mut ratatui::DefaultTerminal, captured: bool) {
    // Set the modes directly. ratatui::init would install another panic hook.
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), EnterAlternateScreen);
    if captured {
        let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    }
    let _ = terminal.clear();
}

const ERROR_LIMIT: usize = 16 * 1024;

async fn run_command_async(argv: &[String], termination: &mut Termination) -> io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut tail = Vec::new();
    let mut buffer = [0; 4096];
    let mut closed = false;
    let status = loop {
        tokio::select! {
            biased;
            _ = termination.wait() => {
                child.kill().await?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "Command stopped."));
            }
            result = child.wait() => break result?,
            result = stderr.read(&mut buffer), if !closed => {
                match result {
                    Ok(0) => closed = true,
                    Ok(n) => record_stderr(&mut tail, &mut io::stderr(), &buffer[..n]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
                    Err(_) => closed = true,
                }
            }
        }
    };
    if !closed {
        drain_stderr(
            &mut stderr,
            &mut tail,
            &mut io::stderr(),
            tokio::time::Instant::now() + std::time::Duration::from_millis(100),
        )
        .await;
    }
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Command failed ({status}).\n{}",
            String::from_utf8_lossy(&tail).trim()
        )))
    }
}

fn record_stderr(tail: &mut Vec<u8>, output: &mut impl Write, bytes: &[u8]) {
    let _ = output.write_all(bytes);
    tail.extend_from_slice(bytes);
    if tail.len() > ERROR_LIMIT {
        tail.drain(..tail.len() - ERROR_LIMIT);
    }
}

async fn drain_stderr(
    stderr: &mut (impl tokio::io::AsyncRead + Unpin),
    tail: &mut Vec<u8>,
    output: &mut impl Write,
    deadline: tokio::time::Instant,
) {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0; 4096];
    // Also bound descendants that keep writing after the command exits.
    let mut remaining: usize = 1024 * 1024;
    while remaining > 0 {
        let capacity = remaining.min(buffer.len());
        tokio::select! {
            biased;
            // Read available bytes before an expired timer. Cooperative task
            // budgets must not make a ready pipe appear empty during this drain.
            result = tokio::task::unconstrained(stderr.read(&mut buffer[..capacity])) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        record_stderr(tail, output, &buffer[..n]);
                        remaining -= n;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
}

/// Whether this process group owns the terminal `tty`. A terminal that
/// reports no foreground group cannot be waited for, so that also counts.
#[cfg(unix)]
fn foreground(tty: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;

    // SAFETY: tcgetpgrp takes a valid open descriptor; getpgrp has no
    // preconditions.
    let group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    group == -1 || group == unsafe { libc::getpgrp() }
}

#[cfg(unix)]
fn signal_action(signal: libc::c_int) -> io::Result<libc::sigaction> {
    // SAFETY: A null action pointer queries the current action into valid memory.
    unsafe {
        let mut action = std::mem::zeroed();
        if libc::sigaction(signal, std::ptr::null(), &mut action) == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(action)
    }
}

#[cfg(unix)]
struct SignalGuard {
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
impl SignalGuard {
    fn new() -> io::Result<Self> {
        // A caught handler resets to the default on exec. SIG_IGN would also
        // make the child ignore Ctrl-C. Keep both processes on the terminal.
        extern "C" fn catch_signal(_: libc::c_int) {}

        let mut guard = Self {
            saved: Vec::with_capacity(2),
        };
        for signal in [libc::SIGINT, libc::SIGQUIT] {
            // SAFETY: Both structures are initialized before use. The handler
            // has the required ABI and does not access memory or call functions.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = catch_signal as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, &mut previous) == -1 {
                    return Err(io::Error::last_os_error());
                }
                guard.saved.push((signal, previous));
            }
        }
        Ok(guard)
    }
}

#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, previous) in self.saved.iter().rev() {
            // SAFETY: Restore the valid action saved for this signal.
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
