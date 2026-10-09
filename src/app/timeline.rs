use super::*;

impl App {
    /// Open the session-local timeline for the selected object: the state
    /// changes sofka has observed while watching this kind. The history keeps
    /// accruing underneath (the table watch stays live), so the view updates as
    /// new changes arrive.
    pub(super) fn open_timeline(&mut self) {
        if self.kind.is_none() {
            self.flash_warn("select a resource first");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection for timeline");
            return;
        };
        let rk = self.key_of(obj);
        let plural = self.view_plural().to_string();
        self.set_return_mode();
        let n = self
            .timeline
            .entries(&plural, &rk)
            .map(|e| e.len())
            .unwrap_or(0);
        // Land on the newest entry (bottom).
        self.timeline_state.select(n.checked_sub(1));
        self.timeline_target = Some((plural, rk));
        self.mode = Mode::Timeline;
    }

    pub(super) fn key_timeline(&mut self, key: KeyInput) {
        let len = self
            .timeline_target
            .as_ref()
            .and_then(|(p, rk)| self.timeline.entries(p, rk))
            .map(|e| e.len())
            .unwrap_or(0);
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            (Some(Action::Down), _) => list_step(&mut self.timeline_state, len, true),
            (Some(Action::Up), _) => list_step(&mut self.timeline_state, len, false),
            (Some(Action::First), _) if len > 0 => self.timeline_state.select(Some(0)),
            (Some(Action::Last), _) if len > 0 => self.timeline_state.select(Some(len - 1)),
            _ => {}
        }
    }
}
