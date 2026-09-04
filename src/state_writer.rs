//! Serialized off-thread writes for small pieces of persistent UI state.
//!
//! Namespace, sort, and fleet choices are changed from input handlers on the
//! render/event-loop thread. Filesystems can stall unpredictably, so the live
//! app hands snapshots to one ordered worker instead of doing TOML encoding,
//! directory creation, and writes inline. A single queue prevents two rapid
//! changes to the same file from completing out of order and coalesces a
//! queued burst to its newest snapshot per destination.
//!
//! Teardown drains the queue but never waits forever: a state directory on a
//! stalled network filesystem must not hold the process open after the TUI has
//! handed the terminal back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc::Sender as UiSender;

use crate::store::Msg;

/// How long teardown waits for the worker to finish its queue before giving up
/// and letting the process exit without it.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

enum Write {
    Fleet(crate::fleet::FleetMarks, PathBuf),
    Namespace(crate::nsmem::NamespaceMemory, PathBuf),
    Sort(crate::sortmem::SortMemory, PathBuf),
    /// Test-only: a write of known duration, so teardown's grace period can be
    /// observed without a genuinely stalled filesystem.
    #[cfg(test)]
    Stall(Duration, PathBuf),
}

impl Write {
    fn path(&self) -> &Path {
        match self {
            Write::Fleet(_, path) | Write::Namespace(_, path) | Write::Sort(_, path) => path,
            #[cfg(test)]
            Write::Stall(_, path) => path,
        }
    }

    fn run(self) -> Result<(), String> {
        match self {
            #[cfg(test)]
            Write::Stall(duration, _) => {
                std::thread::sleep(duration);
                Ok(())
            }
            Write::Fleet(state, path) => state
                .save(&path)
                .map_err(|e| format!("{}: {e}", path.display())),
            Write::Namespace(state, path) => state
                .save(&path)
                .map_err(|e| format!("{}: {e}", path.display())),
            Write::Sort(state, path) => state
                .save(&path)
                .map_err(|e| format!("{}: {e}", path.display())),
        }
    }
}

/// Distinct failures the worker could not put in front of the user, for
/// [`StateWriter::drop`] to print once the terminal is back.
type Unreported = Arc<Mutex<BTreeSet<String>>>;

/// How the worker thread gets a failed write in front of the user.
struct Reporting {
    ui_tx: UiSender<Msg>,
    /// Set by [`StateWriter::drop`] before the queue closes: past that point
    /// nothing drains the UI channel, so `Msg` would go nowhere.
    draining: Arc<AtomicBool>,
    unreported: Unreported,
}

impl Reporting {
    /// Perform one write, making sure a failure is recorded somewhere.
    ///
    /// This thread deliberately never touches stderr while the app is live:
    /// the TUI owns the alternate screen, and ratatui only repaints cells it
    /// sees change, so a stray line would stay smeared across the table until
    /// something else happened to overwrite it. The status line is the one
    /// channel that reaches the user safely.
    ///
    /// It can fail to reach even that — a full event channel, or a write that
    /// only completes after teardown began — and losing the notice would leave
    /// a discarded namespace, sort, or fleet choice with no trace at all. Those
    /// are stashed for `Drop` to print after the TUI releases the terminal.
    fn report(&self, write: Write) {
        let Err(error) = write.run() else { return };
        // Relaxed: the flag guards nothing but itself. Reading it stale can
        // only lose the notice for a write that failed in the nanoseconds
        // between the check and teardown setting it, which the stderr summary
        // would not have printed any sooner.
        let delivered = !self.draining.load(Ordering::Relaxed)
            && self
                .ui_tx
                .try_send(Msg::StateWriteFailed(error.clone()))
                .is_ok();
        if !delivered && let Ok(mut unreported) = self.unreported.lock() {
            unreported.insert(error);
        }
    }
}

/// Handle to the ordered state-write worker.
pub struct StateWriter {
    tx: Option<Sender<Write>>,
    worker: Option<JoinHandle<()>>,
    /// Disconnects when the worker returns. Teardown waits on this rather than
    /// on [`JoinHandle::join`], which has no deadline.
    finished: Receiver<()>,
    draining: Arc<AtomicBool>,
    unreported: Unreported,
    grace: Duration,
}

impl StateWriter {
    pub fn new(ui_tx: UiSender<Msg>) -> Result<Self, String> {
        Self::with_grace(ui_tx, SHUTDOWN_GRACE)
    }

    fn with_grace(ui_tx: UiSender<Msg>, grace: Duration) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Write>();
        let (finished_tx, finished) = mpsc::channel::<()>();
        let draining = Arc::new(AtomicBool::new(false));
        let unreported: Unreported = Arc::new(Mutex::new(BTreeSet::new()));
        let reporting = Reporting {
            ui_tx,
            draining: Arc::clone(&draining),
            unreported: Arc::clone(&unreported),
        };
        let worker = std::thread::Builder::new()
            .name("sofka-state-writer".into())
            .spawn(move || {
                // Never sent on; dropping it with the thread is the signal.
                let _finished_tx = finished_tx;
                while let Ok(first) = rx.recv() {
                    let Ok(second) = rx.try_recv() else {
                        reporting.report(first);
                        continue;
                    };
                    let mut pending = vec![first];
                    // Only the newest snapshot for a file matters. Drain the
                    // burst already waiting behind `first` before touching
                    // disk, while retaining independent state files.
                    for next in std::iter::once(second).chain(rx.try_iter()) {
                        if let Some(slot) =
                            pending.iter_mut().find(|write| write.path() == next.path())
                        {
                            *slot = next;
                        } else {
                            pending.push(next);
                        }
                    }
                    for write in pending {
                        reporting.report(write);
                    }
                }
            })
            .map_err(|e| format!("failed to start state writer: {e}"))?;
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
            finished,
            draining,
            unreported,
            grace,
        })
    }

    pub fn save_fleet(&self, state: crate::fleet::FleetMarks, path: PathBuf) -> Result<(), String> {
        self.send(Write::Fleet(state, path))
    }

    pub fn save_namespace(
        &self,
        state: crate::nsmem::NamespaceMemory,
        path: PathBuf,
    ) -> Result<(), String> {
        self.send(Write::Namespace(state, path))
    }

    pub fn save_sort(
        &self,
        state: crate::sortmem::SortMemory,
        path: PathBuf,
    ) -> Result<(), String> {
        self.send(Write::Sort(state, path))
    }

    /// Shared view of the failures the worker could not put in front of the
    /// user. A handle rather than a snapshot so a test can still read it after
    /// teardown, which is when most of them are recorded.
    #[cfg(test)]
    fn unreported_handle(&self) -> Unreported {
        Arc::clone(&self.unreported)
    }

    #[cfg(test)]
    fn stall(&self, duration: Duration, path: PathBuf) -> Result<(), String> {
        self.send(Write::Stall(duration, path))
    }

    fn send(&self, write: Write) -> Result<(), String> {
        self.tx
            .as_ref()
            .ok_or_else(|| "state writer is shutting down".to_string())?
            .send(write)
            .map_err(|_| "state writer stopped".to_string())
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        // Nothing reads the UI channel from here on, so failures have to be
        // stashed for the summary below instead of sent as `Msg`.
        self.draining.store(true, Ordering::Relaxed);
        // Closing the queue lets the worker drain every accepted snapshot.
        // Waiting for it makes the last UI choice durable before process exit.
        self.tx.take();
        // But only for so long. `join` would wait forever, and a state
        // directory on a stalled network filesystem would then hang the
        // process after the terminal had already been handed back — a quit
        // that never finishes, with nothing on screen to explain it. Past the
        // grace period the worker is left detached and the process exits;
        // whatever it was writing is lost, which is what a kill would have
        // done anyway.
        match self.finished.recv_timeout(self.grace) {
            Err(RecvTimeoutError::Timeout) => {
                self.worker.take();
                eprintln!(
                    "warning: state writer still busy after {:?}; the last \
                     namespace, sort, or fleet change may not be saved",
                    self.grace
                );
            }
            _ => {
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
        }
        // stderr is safe here and only here: `App` is dropped after the TUI
        // has released the terminal, so this lands on the user's shell rather
        // than smearing across a live alternate screen.
        if let Ok(unreported) = self.unreported.lock() {
            for error in unreported.iter() {
                eprintln!("warning: state not saved: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn coalescing_keeps_the_latest_snapshot_on_shutdown() {
        let dir = std::env::temp_dir().join(format!("sofka-state-writer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("namespaces.toml");
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::channel(1);
        let writer = StateWriter::new(ui_tx).unwrap();

        let mut first = crate::nsmem::NamespaceMemory::default();
        first.set("prod", "one");
        writer.save_namespace(first, path.clone()).unwrap();

        let mut latest = crate::nsmem::NamespaceMemory::default();
        latest.set("prod", "two");
        writer.save_namespace(latest.clone(), path.clone()).unwrap();

        drop(writer);
        assert_eq!(crate::nsmem::NamespaceMemory::load(&path), latest);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn coalescing_retains_independent_destination_files() {
        let dir = std::env::temp_dir().join(format!("sofka-state-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let namespace_path = dir.join("namespaces.toml");
        let sort_path = dir.join("sort.toml");
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::channel(1);
        let writer = StateWriter::new(ui_tx).unwrap();

        let mut namespaces = crate::nsmem::NamespaceMemory::default();
        namespaces.set("prod", "payments");
        writer
            .save_namespace(namespaces.clone(), namespace_path.clone())
            .unwrap();

        let mut sorts = crate::sortmem::SortMemory::default();
        sorts.set("pods", "AGE", true);
        writer.save_sort(sorts.clone(), sort_path.clone()).unwrap();

        drop(writer);
        assert_eq!(
            crate::nsmem::NamespaceMemory::load(&namespace_path),
            namespaces
        );
        assert_eq!(crate::sortmem::SortMemory::load(&sort_path), sorts);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn teardown_gives_up_on_a_stalled_worker() {
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::channel(1);
        let writer = StateWriter::with_grace(ui_tx, Duration::from_millis(50)).unwrap();
        // Stands in for a state directory on an unresponsive network mount.
        writer
            .stall(Duration::from_secs(10), PathBuf::from("/stalled"))
            .unwrap();

        let start = Instant::now();
        drop(writer);
        let waited = start.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "teardown blocked for {waited:?} behind a stalled write"
        );
    }

    #[test]
    fn failures_the_ui_cannot_take_are_kept_for_the_exit_summary() {
        let dir = std::env::temp_dir().join(format!("sofka-state-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let parent_file = dir.join("not-a-directory");
        std::fs::write(&parent_file, "x").unwrap();
        let impossible_path = parent_file.join("sort.toml");
        // One slot, already occupied: the worker's `try_send` cannot land, so
        // the failure has nowhere to go but the exit summary.
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::channel(1);
        ui_tx
            .try_send(Msg::StateWriteFailed("filler".into()))
            .unwrap();
        let writer = StateWriter::new(ui_tx).unwrap();

        writer
            .save_sort(crate::sortmem::SortMemory::default(), impossible_path)
            .unwrap();

        let unreported = writer.unreported_handle();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(error) = unreported.lock().unwrap().first() {
                assert!(error.contains("sort.toml"), "{error}");
                break;
            }
            assert!(Instant::now() < deadline, "the failure was dropped");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The case the old `reports_background_write_failures_to_the_ui` was
    /// accidentally covering: a write that only fails once teardown has begun.
    /// Nothing drains the event channel by then, so the notice has to end up in
    /// the exit summary instead of being posted into a channel no one reads.
    #[test]
    fn failures_during_the_shutdown_drain_reach_the_exit_summary() {
        let dir = std::env::temp_dir().join(format!("sofka-state-drain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let parent_file = dir.join("not-a-directory");
        std::fs::write(&parent_file, "x").unwrap();
        let impossible_path = parent_file.join("sort.toml");
        // Roomy channel and a generous grace: neither a full channel nor an
        // expired deadline can be what redirects this failure.
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel(8);
        let writer = StateWriter::with_grace(ui_tx, Duration::from_secs(5)).unwrap();
        let unreported = writer.unreported_handle();

        // Hold the worker so the failing write is still queued when `drop`
        // runs, and therefore only reaches disk during the drain.
        writer
            .stall(Duration::from_millis(50), PathBuf::from("/stall"))
            .unwrap();
        writer
            .save_sort(crate::sortmem::SortMemory::default(), impossible_path)
            .unwrap();
        drop(writer);

        let failures = unreported.lock().unwrap().clone();
        assert_eq!(failures.len(), 1, "{failures:?}");
        let error = failures.first().expect("one recorded failure");
        assert!(error.contains("sort.toml"), "{error}");
        assert!(
            ui_rx.try_recv().is_err(),
            "posted a notice to a channel nothing is reading"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_background_write_failures_to_the_ui() {
        let dir = std::env::temp_dir().join(format!("sofka-state-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let parent_file = dir.join("not-a-directory");
        std::fs::write(&parent_file, "x").unwrap();
        let impossible_path = parent_file.join("sort.toml");
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel(1);
        let writer = StateWriter::new(ui_tx).unwrap();

        writer
            .save_sort(crate::sortmem::SortMemory::default(), impossible_path)
            .unwrap();

        // Read while the writer is still live. Dropping it first would test
        // the opposite path: teardown stops using the UI channel precisely
        // because nothing drains it once the event loop has stopped.
        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match ui_rx.try_recv() {
                Ok(Msg::StateWriteFailed(error)) => break error,
                Ok(_) => panic!("expected a state-write failure"),
                Err(_) => {
                    assert!(Instant::now() < deadline, "no failure reported");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        assert!(error.contains("sort.toml"), "{error}");

        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }
}
