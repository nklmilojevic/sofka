//! Restart the watch after the machine wakes from sleep.
//!
//! A watch connection rarely survives a sleep on battery: the peer drops it
//! while the laptop cannot answer, and the reconnect can block on a socket
//! that will never reply. One OS clock stops while the machine sleeps and
//! another keeps counting, so a tick where the second outruns the first marks
//! a wake. Neither follows the wall clock, so NTP steps and manual clock
//! changes never look like sleep.

use std::time::Duration;

use super::{App, Mode};

/// Sleep between two ticks that counts as a wake. Shorter naps rarely outlast
/// the server's idle timeouts, and keepalive covers the rest.
const SLEEP_GAP: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(super) struct Clock {
    /// Time the machine was awake.
    pub(super) awake: Duration,
    /// Time including sleep.
    pub(super) total: Duration,
}

impl Clock {
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    fn now() -> Option<Self> {
        fn read(id: libc::clockid_t) -> Option<Duration> {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if unsafe { libc::clock_gettime(id, &raw mut ts) } != 0 {
                return None;
            }
            Some(Duration::new(
                u64::try_from(ts.tv_sec).ok()?,
                u32::try_from(ts.tv_nsec).ok()?,
            ))
        }
        #[cfg(target_vendor = "apple")]
        let (awake, total) = (libc::CLOCK_UPTIME_RAW, libc::CLOCK_MONOTONIC);
        #[cfg(not(target_vendor = "apple"))]
        let (awake, total) = (libc::CLOCK_MONOTONIC, libc::CLOCK_BOOTTIME);
        Some(Self {
            awake: read(awake)?,
            total: read(total)?,
        })
    }

    /// No clock pair that tells sleep apart; keepalive alone recovers.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    fn now() -> Option<Self> {
        None
    }
}

impl App {
    pub fn detect_resume(&mut self) {
        if let Some(clock) = Clock::now() {
            self.detect_resume_at(clock);
        }
    }

    pub(super) fn detect_resume_at(&mut self, clock: Clock) {
        if let Some(last) = self.resume_clock.replace(clock) {
            let slept = clock
                .total
                .saturating_sub(last.total)
                .saturating_sub(clock.awake.saturating_sub(last.awake));
            if slept >= SLEEP_GAP {
                crate::log_info!("app.resume", slept_secs = slept.as_secs());
                self.resume_pending = true;
            }
        }
        // Restarting now would discard a context switch in flight, or what
        // the user is looking at in an overlay. A switch that lands clears
        // the wake; one that fails leaves it for the table it returns to.
        if !self.resume_pending
            || self.context_switch_target.is_some()
            || self.mode != Mode::Table
            || self.kind.is_none()
        {
            return;
        }
        self.resume_pending = false;
        // Reuse the namespaces a pattern already resolved to: a fresh lookup
        // would move the selection and could fail before restarting.
        self.start_watch();
        // The restart clears watch errors; any other warning, such as a
        // failed context switch, still applies.
        if !self.flash_err {
            self.set_flash("reconnected after sleep");
        }
    }
}
