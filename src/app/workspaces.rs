use super::*;

const DEFAULT_RESOURCES: [&str; 9] = [
    "pods",
    "services",
    "deployments",
    "statefulsets",
    "daemonsets",
    "secrets",
    "configmaps",
    "ingresses",
    "persistentvolumeclaims",
];

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
            self.switch_context_labeled(&ctx);
            self.pending_resource_query = None;
            self.pending_bookmark = None;
            self.pending_workspace = Some(ws);
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

    /// `Tab`/`Shift-Tab`: cycle the active workspace, or common resources in
    /// the current namespace when no workspace is open.
    pub(super) fn cycle_views(&mut self, forward: bool) -> bool {
        let Some(ws) = &self.active_workspace else {
            return self.cycle_default_resources(forward);
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

    fn cycle_default_resources(&mut self, forward: bool) -> bool {
        let len = DEFAULT_RESOURCES.len();
        let current = DEFAULT_RESOURCES
            .iter()
            .position(|resource| *resource == self.kind_plural)
            .unwrap_or(if forward { len - 1 } else { 0 });
        for offset in 1..=len {
            let index = if forward {
                (current + offset) % len
            } else {
                (current + len - offset) % len
            };
            let resource = DEFAULT_RESOURCES[index];
            if self.cluster.resolve(resource).is_some() {
                self.switch_kind(resource);
                return true;
            }
        }
        false
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
        if self.apply_bookmark_local(bookmark) {
            self.flash = format!("workspace {name} [{i}/{n}]: {vname}");
            self.flash_err = false;
        }
    }
}
