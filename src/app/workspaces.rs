use super::*;

impl App {
    /// Trigger a workspace bound to `key`. Returns whether one matched.
    pub(super) fn try_workspace_key(&mut self, key: KeyEvent) -> bool {
        let Some(ws) = self
            .workspaces
            .iter()
            .find(|w| {
                w.key
                    .as_deref()
                    .and_then(|k| crate::keys::KeyChord::parse(k).ok())
                    .is_some_and(|chord| chord.matches(&key))
            })
            .cloned()
        else {
            return false;
        };
        self.open_workspace(ws);
        true
    }

    /// Open a workspace by name (from the command palette).
    pub(super) fn open_workspace_named(&mut self, name: &str) -> bool {
        let Some(ws) = self.workspaces.iter().find(|w| w.name == name).cloned() else {
            return false;
        };
        self.open_workspace(ws);
        true
    }

    /// Open a workspace: switch its context first (deferred, if it differs),
    /// then land on the first view and start cycling.
    pub(super) fn open_workspace(&mut self, ws: crate::config::Workspace) {
        if ws.views.is_empty() {
            self.flash_warn(&format!("workspace '{}' has no views", ws.name));
            return;
        }
        if let Some(ctx) = ws.context.clone()
            && ctx != self.cluster.context
        {
            self.pending_workspace = Some(ws);
            self.switch_context(ctx);
            return;
        }
        self.start_workspace(ws);
    }

    /// Land on a workspace's first view and make it the active workspace.
    fn start_workspace(&mut self, ws: crate::config::Workspace) {
        self.active_workspace = Some(ActiveWorkspace {
            name: ws.name,
            views: ws.views,
            index: 0,
        });
        self.apply_active_view();
    }

    /// Open a workspace stashed across a context switch, once it lands.
    pub(super) fn apply_pending_workspace(&mut self) {
        if let Some(ws) = self.pending_workspace.take() {
            self.start_workspace(ws);
        }
    }

    /// `Tab`/`Shift-Tab`: move to the next/previous view of the active
    /// workspace (wrapping). No-op when no workspace is active.
    pub(super) fn cycle_workspace(&mut self, forward: bool) -> bool {
        let Some(ws) = &self.active_workspace else {
            return false;
        };
        let len = ws.views.len();
        if len == 0 {
            return false;
        }
        let next = if forward {
            (ws.index + 1) % len
        } else {
            (ws.index + len - 1) % len
        };
        if let Some(ws) = self.active_workspace.as_mut() {
            ws.index = next;
        }
        self.apply_active_view();
        true
    }

    /// Apply the active workspace's current view and set the status line.
    fn apply_active_view(&mut self) {
        let Some(ws) = &self.active_workspace else {
            return;
        };
        let Some(view) = ws.views.get(ws.index) else {
            return;
        };
        let (name, i, n, vname) = (
            ws.name.clone(),
            ws.index + 1,
            ws.views.len(),
            view.name.clone(),
        );
        let bookmark = view.as_bookmark();
        // Reuse the bookmark application path (resource/ns/filter/sort/view),
        // then relabel the status line for the workspace.
        self.apply_bookmark_local(bookmark);
        if self.kind.is_some() {
            self.flash = format!("workspace {name} [{i}/{n}]: {vname}");
            self.flash_err = false;
        }
    }
}
