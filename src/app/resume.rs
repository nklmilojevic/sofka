//! Restart the watch after the machine wakes from sleep.
//!
//! A watch connection rarely survives a sleep on battery: the peer drops it
//! while the laptop cannot answer, and the reconnect can block on a socket
//! that will never reply. Monotonic time stops while the machine sleeps and
//! wall time does not, so a tick whose wall gap outruns its monotonic gap
//! marks a wake.

use std::time::{Duration, Instant, SystemTime};

use super::{App, Mode};

/// Wall time beyond monotonic time between two ticks that counts as a sleep.
/// Large enough that clock slews and NTP corrections never trip it.
const SLEEP_GAP: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(super) struct Clock {
    mono: Instant,
    wall: SystemTime,
}

impl App {
    pub fn detect_resume(&mut self) {
        self.detect_resume_at(Instant::now(), SystemTime::now());
    }

    pub(super) fn detect_resume_at(&mut self, mono: Instant, wall: SystemTime) {
        if let Some(last) = self.resume_clock.replace(Clock { mono, wall }) {
            let slept = wall
                .duration_since(last.wall)
                .unwrap_or_default()
                .saturating_sub(mono.saturating_duration_since(last.mono));
            if slept >= SLEEP_GAP {
                crate::log_info!("app.resume", slept_secs = slept.as_secs());
                self.resume_pending = true;
            }
        }
        // Restarting from an overlay would drop what the user is looking at,
        // so wait until they are back on the table.
        if !self.resume_pending || self.mode != Mode::Table || self.kind.is_none() {
            return;
        }
        self.resume_pending = false;
        self.refresh_namespace_selection();
        self.set_flash("reconnected after sleep");
    }
}
