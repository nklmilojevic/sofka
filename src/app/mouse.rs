use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// Where the table landed on screen last frame, for mouse hit-testing.
/// Recorded by `draw_table`; coordinates are terminal cells.
#[derive(Clone, Default)]
pub(crate) struct TableHit {
    /// The header row's y.
    pub header_y: u16,
    /// First data row's y and how many rows are visible.
    pub rows_y: u16,
    pub rows_h: u16,
    /// Inner x extent (inside the borders).
    pub x_min: u16,
    pub x_max: u16,
    /// Per-column `[start, end)` x ranges plus the `display_headers()` index
    /// each range shows (columns can be scrolled out of view horizontally).
    pub cols: Vec<(u16, u16, usize)>,
}

impl App {
    /// Record the table geometry the renderer just used, so clicks can be
    /// mapped back to rows and header columns. `cols` are `[start, end)` x
    /// ranges tagged with the `display_headers()` index they show.
    pub fn record_table_hit(
        &self,
        header_y: u16,
        rows_y: u16,
        rows_h: u16,
        x_min: u16,
        x_max: u16,
        cols: Vec<(u16, u16, usize)>,
    ) {
        *self.table_hit.borrow_mut() = Some(TableHit {
            header_y,
            rows_y,
            rows_h,
            x_min,
            x_max,
            cols,
        });
    }

    /// Whether the current view redraws identically until something actually
    /// happens — a full-screen document, whose text and title carry no elapsed
    /// time. Every other view can be showing an age or a duration, and has to
    /// be redrawn on the 1s tick for that to stay true.
    ///
    /// Deliberately a small allowlist rather than a "not a table" test: a view
    /// wrongly listed here would freeze a clock the user is reading, which is
    /// worse than the idle frame it saves.
    pub fn static_between_events(&self) -> bool {
        matches!(
            self.mode,
            Mode::Detail | Mode::Diff | Mode::Events | Mode::Help
        )
    }

    /// Whether the run loop should keep terminal mouse capture on right now.
    /// Document-style views (YAML/describe, diff, events, logs, help) release
    /// capture so click-drag uses the terminal's native text selection —
    /// nothing in them uses clicks, and scrolling still works because with
    /// reporting off terminals translate the wheel into arrow keys in the
    /// alternate screen ("alternate scroll"), which these views all handle.
    /// The filter-typing overlays keep the same document on screen, so they
    /// release too. A wheel burst can split one of those escape sequences
    /// mid-read; `crate::altscroll` reassembles them before they reach us.
    pub fn wants_mouse_capture(&self) -> bool {
        !matches!(
            self.mode,
            Mode::Detail
                | Mode::Diff
                | Mode::Events
                | Mode::Logs
                | Mode::Help
                | Mode::DocFilter
                | Mode::LogFilter
        )
    }

    /// Route a mouse event. The wheel is synthesized into the mode's own
    /// up/down keys, so scrolling works identically in every view (table,
    /// logs, documents, pickers) without a second navigation code path;
    /// clicks are table-specific (select a row, sort by a header).
    pub fn handle_mouse(&mut self, m: MouseEvent) -> Result<()> {
        match m.kind {
            MouseEventKind::ScrollUp => self.wheel(KeyCode::Up),
            MouseEventKind::ScrollDown => self.wheel(KeyCode::Down),
            MouseEventKind::Down(MouseButton::Left) if self.mode == Mode::Table => {
                self.table_click(m.column, m.row);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// One wheel notch = three steps, like most list UIs.
    fn wheel(&mut self, code: KeyCode) -> Result<()> {
        for _ in 0..3 {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))?;
        }
        Ok(())
    }

    fn table_click(&mut self, x: u16, y: u16) {
        let Some(hit) = self.table_hit.borrow().clone() else {
            return;
        };
        if x < hit.x_min || x >= hit.x_max {
            return;
        }
        // Header row: sort by the clicked column (again = flip direction).
        if y == hit.header_y {
            let Some(&(_, _, idx)) = hit.cols.iter().find(|(s, e, _)| x >= *s && x < *e) else {
                return;
            };
            if self.sort_column == Some(idx) {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort_column = Some(idx);
                self.sort_desc = false;
            }
            self.invalidate_rows();
            self.remember_sort();
            let label = self.display_headers().get(idx).cloned().unwrap_or_default();
            self.flash = format!(
                "sort by {label} {}",
                if self.sort_desc {
                    "↓ desc"
                } else {
                    "↑ asc"
                }
            );
            self.flash_err = false;
            return;
        }
        // Data rows: select what was clicked.
        if y >= hit.rows_y && y < hit.rows_y + hit.rows_h {
            let idx = self.table_state.offset() + (y - hit.rows_y) as usize;
            if idx < self.row_count() {
                self.table_state.select(Some(idx));
            }
        }
    }
}
