//! Serialized off-thread writes for small pieces of persistent UI state.
//!
//! Namespace, sort, and fleet choices are changed from input handlers on the
//! render/event-loop thread. Filesystems can stall unpredictably, so the live
//! app hands snapshots to one ordered worker instead of doing TOML encoding,
//! directory creation, and writes inline. A single queue also prevents two
//! rapid changes to the same file from completing out of order.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use tokio::sync::mpsc::Sender as UiSender;

use crate::store::Msg;

enum Write {
    Fleet(crate::fleet::FleetMarks, PathBuf),
    Namespace(crate::nsmem::NamespaceMemory, PathBuf),
    Sort(crate::sortmem::SortMemory, PathBuf),
}

impl Write {
    fn run(self) -> Result<(), String> {
        match self {
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

/// Handle to the ordered state-write worker.
pub struct StateWriter {
    tx: Option<Sender<Write>>,
    worker: Option<JoinHandle<()>>,
}

impl StateWriter {
    pub fn new(ui_tx: UiSender<Msg>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Write>();
        let worker = std::thread::Builder::new()
            .name("sofka-state-writer".into())
            .spawn(move || {
                for write in rx {
                    if let Err(error) = write.run() {
                        eprintln!("warning: state not saved: {error}");
                        let _ = ui_tx.try_send(Msg::StateWriteFailed(error));
                    }
                }
            })
            .map_err(|e| format!("failed to start state writer: {e}"))?;
        Ok(Self {
            tx: Some(tx),
            worker: Some(worker),
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
        // Closing the queue lets the worker drain every accepted snapshot.
        // Joining here makes the last UI choice durable before process exit.
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_writes_in_submission_order_on_shutdown() {
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
        drop(writer);

        match ui_rx.blocking_recv().unwrap() {
            Msg::StateWriteFailed(error) => assert!(error.contains("sort.toml")),
            _ => panic!("expected state-write failure"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
