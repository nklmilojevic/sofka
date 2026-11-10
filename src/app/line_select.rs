//! `V` line selection in the logs and document views: a cursor that extends a
//! range of whole lines, copied with the same text `c` copies.

use super::*;
use std::ops::RangeInclusive;

/// The line a selection started on and the line its cursor is on. Document
/// views store source line indices; logs store stable buffer positions, so
/// new lines and trimming do not shift the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSelection {
    pub anchor: usize,
    pub cursor: usize,
}

impl LineSelection {
    fn at(line: usize) -> Self {
        Self {
            anchor: line,
            cursor: line,
        }
    }

    pub fn range(self) -> RangeInclusive<usize> {
        self.anchor.min(self.cursor)..=self.anchor.max(self.cursor)
    }

    fn swap(&mut self) {
        std::mem::swap(&mut self.anchor, &mut self.cursor);
    }

    fn map(&mut self, f: impl Fn(usize) -> usize) {
        self.anchor = f(self.anchor);
        self.cursor = f(self.cursor);
    }
}

#[derive(Clone, Copy)]
enum Step {
    By(isize),
    First,
    Last,
}

fn step_for(action: Action, page: usize) -> Option<Step> {
    let page = action.page_size(page) as isize;
    Some(match action {
        Action::Down => Step::By(1),
        Action::Up => Step::By(-1),
        Action::PageDown | Action::HalfPageDown => Step::By(page),
        Action::PageUp | Action::HalfPageUp => Step::By(-page),
        Action::First => Step::First,
        Action::Last => Step::Last,
        _ => return None,
    })
}

/// Where a step lands among `count` selectable lines. `rank` counts the
/// selectable lines before the cursor, and `on` says whether the cursor is on
/// one: a filter can hide it, and then a step down lands on the next shown line.
fn target(rank: usize, on: bool, count: usize, step: Step) -> usize {
    let last = count.saturating_sub(1);
    match step {
        Step::First => 0,
        Step::Last => last,
        Step::By(n) if n >= 0 => (rank + n.unsigned_abs()).saturating_sub(usize::from(!on)),
        Step::By(n) => rank.saturating_sub(n.unsigned_abs()),
    }
    .min(last)
}

/// Bring the cursor's rows `start..end` into a viewport of `height` rows.
fn reveal(scroll: &mut usize, start: usize, end: usize, height: usize) {
    if start < *scroll {
        *scroll = start;
    } else if end > *scroll + height {
        *scroll = end.saturating_sub(height).min(start);
    }
}

impl Scrollable {
    pub fn selection(&self) -> Option<LineSelection> {
        self.selection
    }

    fn start_selection(&mut self) -> bool {
        let Some(last) = self.lines.len().checked_sub(1) else {
            return false;
        };
        let matches = self.match_lines();
        let line = match matches.len() {
            0 => self.source_line_at_scroll(),
            n => matches[self.match_idx.min(n - 1)],
        };
        self.selection = Some(LineSelection::at(line.min(last)));
        self.reveal_cursor = true;
        true
    }

    fn move_selection(&mut self, step: Step) {
        let count = self.lines.len();
        if let Some(selection) = &mut self.selection {
            selection.cursor = target(selection.cursor, true, count, step);
            self.reveal_cursor = true;
        }
    }

    fn select_match(&mut self, forward: bool) {
        self.step_match(forward);
        let matches = self.match_lines();
        if let Some(selection) = &mut self.selection
            && let Some(&line) = matches.get(self.match_idx)
        {
            selection.cursor = line;
            self.reveal_cursor = true;
        }
    }

    fn swap_selection(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.swap();
            self.reveal_cursor = true;
        }
    }

    /// Keep the selection on lines that still exist after the document was
    /// replaced in place.
    pub(super) fn clamp_selection(&mut self) {
        match self.lines.len().checked_sub(1) {
            Some(last) => {
                if let Some(selection) = &mut self.selection {
                    selection.map(|line| line.min(last));
                }
            }
            None => self.selection = None,
        }
    }

    /// Scroll a moved cursor into view. Runs at draw time, after the layout
    /// for the current width is known.
    pub(crate) fn reveal_selection(&mut self) {
        if !std::mem::take(&mut self.reveal_cursor) {
            return;
        }
        let (Some(selection), Some(viewport)) = (self.selection, self.viewport.as_ref()) else {
            return;
        };
        let start = viewport.line_start(selection.cursor);
        let end = viewport
            .ends
            .get(selection.cursor)
            .copied()
            .unwrap_or(start + 1);
        let height = viewport.height;
        reveal(&mut self.scroll, start, end, height);
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub(super) fn selected_text(&self) -> String {
        let Some(selection) = self.selection else {
            return String::new();
        };
        let range = selection.range();
        self.lines
            .iter()
            .skip(*range.start())
            .take(range.end() - range.start() + 1)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl LogsView {
    pub fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    /// Whether buffer line `i` is selected, and if so whether the cursor is on
    /// it.
    pub fn line_selected(&self, i: usize) -> Option<bool> {
        let selection = self.selection?;
        let position = self.line_offset + i;
        selection
            .range()
            .contains(&position)
            .then_some(position == selection.cursor)
    }

    /// Selected lines the filter shows, counted on the index as it stands.
    pub fn selected_lines(&self) -> usize {
        let Some(selection) = self.selection else {
            return 0;
        };
        let range = selection.range();
        let Some(end) = range.end().checked_sub(self.line_offset) else {
            return 0;
        };
        let start = range.start().saturating_sub(self.line_offset);
        self.index.rank(end + 1) - self.index.rank(start)
    }

    /// Start on the first visible line and pause autoscroll.
    fn start_selection(&mut self) -> bool {
        let (follow, height, scroll) = (self.follow, self.viewport_h, self.view.scroll);
        let index = self.refresh_index(self.last_wrap_width);
        let scroll = if follow {
            index.total_rows().saturating_sub(height)
        } else {
            scroll
        };
        let first = index.first_at_row(scroll);
        let rank = index.lines.partition_point(|&slot| (slot as usize) < first);
        let Some(line) = index
            .line(rank)
            .or_else(|| rank.checked_sub(1).and_then(|rank| index.line(rank)))
        else {
            return false;
        };
        self.view.scroll = scroll;
        self.selection = Some(LineSelection::at(self.line_offset + line));
        self.selection_follow = follow;
        self.follow = false;
        self.reveal_cursor = true;
        true
    }

    fn move_selection(&mut self, step: Step) {
        let offset = self.line_offset;
        let Some(mut selection) = self.selection else {
            return;
        };
        let index = self.refresh_index(self.last_wrap_width);
        let cursor = selection.cursor.saturating_sub(offset);
        let rank = index.rank(cursor);
        let on = index.line(rank) == Some(cursor);
        let Some(line) = index.line(target(rank, on, index.lines.len(), step)) else {
            return;
        };
        selection.cursor = offset + line;
        self.selection = Some(selection);
        self.reveal_cursor = true;
    }

    fn swap_selection(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.swap();
            self.reveal_cursor = true;
        }
    }

    /// A line was inserted at buffer index `index`, before newer lines.
    pub(super) fn shift_selection(&mut self, index: usize) {
        let at = self.line_offset + index;
        if let Some(selection) = &mut self.selection {
            selection.map(|position| position + usize::from(position >= at));
        }
    }

    /// Trimming moved the oldest kept line to [`Self::line_offset`].
    pub(super) fn clamp_selection(&mut self) {
        let first = self.line_offset;
        if let Some(selection) = &mut self.selection {
            selection.map(|position| position.max(first));
        }
    }

    /// Drop the selection and restore the autoscroll it paused.
    pub(super) fn end_selection(&mut self) {
        if self.selection.take().is_some() {
            self.follow = self.selection_follow;
        }
    }

    /// Scroll a moved cursor into view. Runs at draw time with the index
    /// refreshed for the current wrap width.
    pub(crate) fn reveal_selection(&mut self) {
        if !std::mem::take(&mut self.reveal_cursor) {
            return;
        }
        let Some(cursor) = self
            .selection
            .and_then(|selection| selection.cursor.checked_sub(self.line_offset))
        else {
            return;
        };
        let index = &self.index;
        let Some(&slot) = index.lines.get(index.rank(cursor)) else {
            return;
        };
        let start = index.start_row(slot as usize);
        let end = start + index.height_at(slot as usize);
        reveal(&mut self.view.scroll, start, end, self.viewport_h);
    }

    /// Raw lines the filter shows within buffer indices `lines`, as `c`
    /// copies them, and how many there are.
    pub(super) fn shown_text(&self, lines: RangeInclusive<usize>) -> (String, usize) {
        let shown: Vec<_> = self
            .view
            .lines
            .iter()
            .enumerate()
            .skip(*lines.start())
            .take_while(|(i, _)| i <= lines.end())
            .filter(|(i, line)| self.matches(line, self.line_meta.get(*i)))
            .map(|(_, line)| line.as_str())
            .collect();
        (shown.join("\n"), shown.len())
    }

    pub(super) fn selected_text(&self) -> (String, usize) {
        let Some(selection) = self.selection else {
            return (String::new(), 0);
        };
        let range = selection.range();
        let start = range.start().saturating_sub(self.line_offset);
        let Some(end) = range.end().checked_sub(self.line_offset) else {
            return (String::new(), 0);
        };
        self.shown_text(start..=end)
    }
}

impl LogIndex {
    /// Log lines before buffer index `line`.
    fn rank(&self, line: usize) -> usize {
        self.lines
            .partition_point(|&slot| self.shown[slot as usize].is_some_and(|i| (i as usize) < line))
    }

    /// Buffer index of the log line at `rank`.
    fn line(&self, rank: usize) -> Option<usize> {
        self.lines
            .get(rank)
            .and_then(|&slot| self.line_at(slot as usize))
    }
}

impl App {
    /// `V` and the keys of an open selection in the logs view. Returns whether
    /// the key was used.
    pub(super) fn key_log_selection(&mut self, key: KeyInput) -> bool {
        let Some(action) = key.action else {
            return false;
        };
        if !self.logs.selecting() {
            if action != Action::SelectLines {
                return false;
            }
            if !self.logs.start_selection() {
                self.flash_warn("no log lines to select");
            }
            return true;
        }
        match action {
            Action::SelectLines | Action::Back => self.end_log_selection(),
            Action::Copy | Action::CopyLines => self.copy_logs(),
            Action::SelectionOtherEnd => self.logs.swap_selection(),
            // Autoscroll was paused for the selection; the follow key turns it
            // back on, so the selection ends without restoring it first.
            Action::Follow => {
                self.logs.selection = None;
                return false;
            }
            Action::Close => {
                self.end_log_selection();
                return false;
            }
            _ => match step_for(action, self.logs.viewport_h.max(1)) {
                Some(step) => self.logs.move_selection(step),
                None => return false,
            },
        }
        true
    }

    pub(super) fn end_log_selection(&mut self) {
        self.logs.end_selection();
        if self.logs.follow {
            self.trim_log_buffer();
        }
    }

    /// `V` and the keys of an open selection in a document view. Returns
    /// whether the key was used.
    pub(super) fn key_doc_selection(&mut self, key: KeyInput) -> bool {
        let Some(action) = key.action else {
            return false;
        };
        if self.detail.selection.is_none() {
            if action != Action::SelectLines {
                return false;
            }
            if !self.detail.start_selection() {
                self.flash_warn("nothing to select");
            }
            return true;
        }
        match action {
            Action::SelectLines | Action::Back => self.detail.selection = None,
            Action::Copy | Action::CopyLines => self.copy_doc(),
            Action::SelectionOtherEnd => self.detail.swap_selection(),
            Action::NextMatch => self.detail.select_match(true),
            Action::PreviousMatch => self.detail.select_match(false),
            Action::Close => {
                self.detail.selection = None;
                return false;
            }
            _ => match step_for(action, DOCUMENT_PAGE) {
                Some(step) => self.detail.move_selection(step),
                None => return false,
            },
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_clamp_and_skip_to_the_next_shown_line() {
        assert_eq!(target(3, true, 10, Step::By(1)), 4);
        assert_eq!(target(3, true, 10, Step::By(-5)), 0);
        assert_eq!(target(3, true, 10, Step::By(20)), 9);
        assert_eq!(target(3, false, 10, Step::By(1)), 3);
        assert_eq!(target(3, false, 10, Step::By(-1)), 2);
        assert_eq!(target(10, false, 10, Step::By(1)), 9);
        assert_eq!(target(3, true, 10, Step::First), 0);
        assert_eq!(target(3, true, 10, Step::Last), 9);
    }
}
