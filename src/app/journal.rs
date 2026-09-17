use super::*;

impl App {
    /// Record a mutating action in the session journal. Pass identifiers only
    /// (verbs, names) — never secret input or decoded Secret values.
    pub(super) fn note_action(&mut self, action: impl Into<String>, target: impl Into<String>) {
        let ctx = self.cluster.context.clone();
        self.journal.record(&ctx, action, target);
    }

    pub fn check_journal_error(&mut self) {
        if let Some(error) = self.journal.take_error() {
            self.borrow_status(error.clone(), true);
            if !self.config_warnings.contains(&error) {
                self.config_warnings.push(error);
            }
        }
    }

    /// A compact "name" / "N kind" target label for a bulk-or-single action.
    pub(super) fn action_label(&self, targets: &[(String, String)]) -> String {
        match targets {
            [] => "—".into(),
            [(name, ns)] if ns.is_empty() => name.clone(),
            [(name, ns)] => format!("{name} in {ns}"),
            many => format!("{} {}", many.len(), self.kind_plural),
        }
    }

    /// `:journal` — the session's action log as a scrollable document.
    pub(super) fn open_journal(&mut self) {
        self.set_return_mode();
        let title = if self.journal.is_empty() {
            "action journal (empty)".to_string()
        } else {
            format!("action journal ({} entries)", self.journal.len())
        };
        self.detail = Scrollable {
            wrap: self.detail.wrap,
            title,
            lines: self.journal.lines().into(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }
}
