use super::*;

use crate::snapshot::{Format, Snapshot, age, clock, snapshots_dir};

impl App {
    /// `:snapshot [format]` — capture the current table view (columns + visible
    /// rows) and write it to the snapshots directory. `format` is `text`
    /// (default), `json`, or `yaml`.
    pub(super) fn take_snapshot(&mut self, arg: &str) {
        let Some(format) = Format::parse(arg) else {
            self.flash_warn(&format!("unknown snapshot format '{arg}' (text/json/yaml)"));
            return;
        };
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        let (columns, rows) = self.snapshot_table();
        let snap = Snapshot {
            captured_at: k8s_openapi::jiff::Timestamp::now().as_second(),
            context: self.cluster.context.clone(),
            cluster: self.cluster.cluster_name.clone(),
            namespace: self.namespace.clone(),
            resource: self.kind_plural.clone(),
            filter: self.filter.clone(),
            columns,
            rows,
        };
        let text = snap.render(format);
        let path = snapshots_dir().join(snap.filename(format));
        let tx = self.tx.clone();
        let genr = self.generation;
        self.flash = format!("saving snapshot ({} rows)…", snap.rows.len());
        self.flash_err = false;
        tokio::spawn(async move {
            let result = write_snapshot(&path, &text).await;
            let _ = tx
                .send(Msg::SnapshotSaved {
                    generation: genr,
                    result,
                })
                .await;
        });
    }

    /// The current table as plain (unstyled) columns + rows — the same layout
    /// the table renders (NAMESPACE prepended across namespaces, CPU/MEM
    /// appended for pods/nodes, volatile cells resolved), minus the coloring.
    pub(super) fn snapshot_table(&self) -> (Vec<String>, Vec<Vec<String>>) {
        let headers = self.display_headers();
        let show_ns = self.show_namespace_column();
        let metrics_cols = self.metrics_columns();
        let pods_view = self.kind_plural == "pods";

        let objs = self.rows();
        self.ensure_table_cell_cache(&objs);
        let cache = self.table_cell_cache();
        let spec = self.view_spec();

        let rows = objs
            .iter()
            .map(|obj| {
                let rk = row_key(obj);
                let mut cells: Vec<String> = Vec::with_capacity(headers.len());
                if show_ns {
                    cells.push(obj.metadata.namespace.clone().unwrap_or_default());
                }
                if let Some((base_cells, _)) = cache.get(&rk) {
                    for (i, cell) in base_cells.iter().enumerate() {
                        match spec.volatile(obj, &self.kind_plural, i) {
                            Some(v) => cells.push(v),
                            None => cells.push(cell.clone()),
                        }
                    }
                }
                if metrics_cols {
                    let name = obj.metadata.name.as_deref().unwrap_or_default();
                    let key = if pods_view {
                        format!(
                            "{}/{}",
                            obj.metadata.namespace.as_deref().unwrap_or_default(),
                            name
                        )
                    } else {
                        name.to_string()
                    };
                    let (cpu, mem) = self.metrics.get(&key).copied().unwrap_or((0, 0));
                    cells.push(crate::columns::fmt_cpu(cpu));
                    cells.push(crate::columns::fmt_mem(mem));
                }
                cells
            })
            .collect();
        (headers, rows)
    }

    /// `:snapshots` — open the saved-snapshot browser.
    pub(super) fn open_snapshots(&mut self) {
        self.reload_snapshot_list();
        if self.snapshot_list.is_empty() {
            self.flash_warn("no snapshots yet — capture one with :snapshot");
            return;
        }
        self.snapshot_state.select(Some(0));
        self.mode = Mode::Snapshots;
    }

    /// Rebuild [`Self::snapshot_list`] from the snapshots directory, newest
    /// first, labelling each with its age and size.
    fn reload_snapshot_list(&mut self) {
        let now = k8s_openapi::jiff::Timestamp::now().as_second();
        let mut entries: Vec<(std::path::PathBuf, i64, String)> = Vec::new();
        if let Ok(dir) = std::fs::read_dir(snapshots_dir()) {
            for entry in dir.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.push((path, modified, name));
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.1));
        self.snapshot_list = entries
            .into_iter()
            .map(|(path, modified, name)| {
                let label = format!("{name}   ({})", age(modified, now));
                (path, label)
            })
            .collect();
    }

    pub(super) fn key_snapshots(&mut self, key: KeyEvent) {
        let len = self.snapshot_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.snapshot_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.snapshot_state, len, false),
            KeyCode::Enter => self.open_selected_snapshot(),
            KeyCode::Char('d') => self.delete_selected_snapshot(),
            _ => {}
        }
    }

    /// Load the highlighted snapshot into the detail view, marked stale.
    fn open_selected_snapshot(&mut self) {
        let Some(path) = self
            .snapshot_state
            .selected()
            .and_then(|i| self.snapshot_list.get(i))
            .map(|(p, _)| p.clone())
        else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(body) => {
                let now = k8s_openapi::jiff::Timestamp::now().as_second();
                let modified = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(now);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut lines = vec![
                    format!(
                        "⚠ snapshot captured {} ({}) — a point-in-time capture, likely stale",
                        clock(modified),
                        age(modified, now)
                    ),
                    String::new(),
                ];
                lines.extend(body.lines().map(String::from));
                self.detail = Scrollable {
                    title: name,
                    lines: lines.into(),
                    ..Default::default()
                };
                self.return_mode = Mode::Snapshots;
                self.mode = Mode::Detail;
            }
            Err(e) => self.flash_warn(&format!("read snapshot failed: {e}")),
        }
    }

    /// `d` in the browser — delete the highlighted snapshot file.
    fn delete_selected_snapshot(&mut self) {
        let Some(i) = self.snapshot_state.selected() else {
            return;
        };
        let Some((path, _)) = self.snapshot_list.get(i).cloned() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                self.flash = "snapshot deleted".into();
                self.flash_err = false;
                self.reload_snapshot_list();
                if self.snapshot_list.is_empty() {
                    self.mode = Mode::Table;
                } else {
                    let n = self.snapshot_list.len();
                    self.snapshot_state.select(Some(i.min(n - 1)));
                }
            }
            Err(e) => self.flash_warn(&format!("delete failed: {e}")),
        }
    }
}

/// Create the snapshots directory (if needed) and write `text` to `path`.
async fn write_snapshot(path: &std::path::Path, text: &str) -> Result<std::path::PathBuf, String> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(path, text)
        .await
        .map(|_| path.to_path_buf())
        .map_err(|e| e.to_string())
}
