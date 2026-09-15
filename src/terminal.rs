use std::io;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Suspend the TUI for an interactive command, then restore its terminal modes.
/// Call this only from the main loop, with the current mouse capture state.
pub fn suspend_and_run(
    terminal: &mut ratatui::DefaultTerminal,
    argv: &[String],
    captured: bool,
) -> io::Result<()> {
    if argv.is_empty() {
        return Ok(());
    }
    // Protect the parent before normal terminal input can generate signals.
    #[cfg(unix)]
    let _signals = SignalGuard::new()?;
    if captured {
        let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    let result = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status();
    // Set the modes directly. ratatui::init would install another panic hook.
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), EnterAlternateScreen);
    if captured {
        let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    }
    let _ = terminal.clear();
    result.map(|_| ())
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
