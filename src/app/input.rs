use super::*;

impl App {
    pub fn scrollbars_visible(&self) -> bool {
        self.scrollbar_activity
            .is_some_and(|time| time.elapsed() < std::time::Duration::from_millis(700))
    }

    pub fn expire_scrollbars(&mut self) -> bool {
        if self.scrollbar_activity.is_some() && !self.scrollbars_visible() {
            self.scrollbar_activity = None;
            return true;
        }
        false
    }

    // ----- key handling --------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        let action = self.keymap.action(self.key_scope(), &key);
        self.handle_input(KeyInput::new(action, key))
    }

    pub(super) fn handle_input(&mut self, key: KeyInput) -> Result<()> {
        if !matches!(key.action, Some(Action::RangeUp | Action::RangeDown)) {
            self.range_selection = None;
        }
        let before = match self.mode {
            Mode::Command => self.palette_return,
            Mode::Help => self.help_return,
            // Where the overlay will return to: the table, except for a
            // dialog raised from the PVC browser, which returns there. Getting
            // this wrong reads as "left the view" and stops a running plugin.
            Mode::Confirm | Mode::Prompt => self.overlay_return(),
            Mode::Filter | Mode::SortPicker | Mode::CopyPicker => Mode::Table,
            other => other,
        };
        let run = self.plugin_run;
        let input_mode = self.mode;
        let scroll_action = matches!(
            key.action,
            Some(
                Action::Up
                    | Action::Down
                    | Action::Left
                    | Action::Right
                    | Action::First
                    | Action::Last
                    | Action::PageUp
                    | Action::PageDown
                    | Action::RangeUp
                    | Action::RangeDown
                    | Action::NextMatch
                    | Action::PreviousMatch
            )
        );
        let result = self.handle_key_inner(key);
        if self.mode != input_mode {
            self.scrollbar_activity = None;
        } else if scroll_action {
            self.scrollbar_activity = Some(std::time::Instant::now());
        }
        self.check_resource_refresh();
        self.sync_container_history();
        if self.should_quit || self.mode != Mode::Adjacent {
            self.cancel_children();
        }
        let overlay = matches!(
            self.mode,
            Mode::Command
                | Mode::Help
                | Mode::Filter
                | Mode::DocFilter
                | Mode::LogFilter
                | Mode::Confirm
                | Mode::Prompt
                | Mode::SortPicker
                | Mode::CopyPicker
        );
        if matches!(before, Mode::Detail | Mode::Diff)
            && !matches!(self.mode, Mode::Detail | Mode::Diff)
            && !overlay
        {
            self.clear_document_source();
        }
        if self.should_quit || (self.plugin_run == run && self.mode != before && !overlay) {
            self.stop_plugins();
        }
        // Anything that navigated out of the PVC browser — a palette jump, a
        // bookmark, a workspace — would otherwise leave it holding a helper
        // pod and two stale panes. `leave_pvc_explore` is idempotent, so the
        // `esc` path having already run it costs nothing.
        if self.pvc.active && self.mode != Mode::PvcExplore && !overlay {
            self.leave_pvc_explore();
        }
        // A PVC shell waiting on a dialog can also be walked away from — `:`
        // is accepted from `Mode::Confirm` and simply abandons the action.
        // Nothing else will ever run the suspend, so release what it holds.
        let awaiting = matches!(self.mode, Mode::Confirm | Mode::Prompt) || self.pending.is_some();
        if self.pvc.shell_pending && !self.pvc.active && !awaiting {
            if matches!(self.confirm_action, Some(ConfirmAction::PvcShell { .. })) {
                self.confirm_action = None;
            }
            self.after_suspend();
        }
        result
    }

    fn handle_key_inner(&mut self, key: KeyInput) -> Result<()> {
        match key.action {
            Some(Action::Quit) => {
                self.stop_plugins();
                self.should_quit = true;
                return Ok(());
            }
            Some(Action::Compact) => {
                self.compact = !self.compact;
                return Ok(());
            }
            Some(Action::Command) => {
                self.open_palette();
                return Ok(());
            }
            Some(Action::Help) => {
                self.open_help();
                return Ok(());
            }
            _ => {}
        }

        match self.mode {
            Mode::Table => self.key_table(key),
            Mode::Command => self.key_command(key),
            Mode::Filter => self.key_filter(key),
            Mode::Detail | Mode::Diff | Mode::Events => self.key_scroll(key, true),
            Mode::Logs => self.key_logs(key),
            Mode::LogFilter => self.key_log_filter(key),
            Mode::DocFilter => self.key_doc_filter(key),
            Mode::Help => self.key_help(key),
            Mode::Namespaces => self.key_namespaces(key),
            Mode::Contexts => self.key_contexts(key),
            Mode::SortPicker => self.key_sort_picker(key),
            Mode::CopyPicker => self.key_copy_picker(key),
            Mode::Containers => self.key_containers(key),
            Mode::SetImage => self.key_set_image(key),
            Mode::Confirm => self.key_confirm(key),
            Mode::Prompt => self.key_prompt(key),
            Mode::Pulse => self.key_pulse(key),
            Mode::Xray => self.key_xray(key),
            Mode::Explain => self.key_explain(key),
            Mode::Timeline => self.key_timeline(key),
            Mode::Gitops => self.key_gitops(key),
            Mode::Argocd => self.key_argocd(key),
            Mode::Adjacent => self.key_adjacent(key),
            Mode::FluxMenu => self.key_flux_menu(key),
            Mode::TransferMenu => self.key_transfer_menu(key),
            Mode::PortForwards => self.key_port_forwards(key),
            Mode::Skins => self.key_skins(key),
            Mode::Snapshots => self.key_snapshots(key),
            Mode::Fleet => self.key_fleet(key),
            Mode::Find => self.key_find(key),
            Mode::PvcExplore => self.key_pvc_explore(key),
            Mode::PortForwardPicker => self.key_port_forward_picker(key),
        }
        Ok(())
    }

    pub fn key_scope(&self) -> &'static str {
        match self.mode {
            Mode::Contexts if self.ctx_filtering => "context_filter",
            Mode::Table => "table",
            Mode::Command => "command",
            Mode::Filter => "filter",
            Mode::Detail => "detail",
            Mode::Diff => "diff",
            Mode::Events => "events",
            Mode::Logs => "logs",
            Mode::LogFilter => "log_filter",
            Mode::DocFilter => "doc_filter",
            Mode::Help => "help",
            Mode::Namespaces => "namespaces",
            Mode::Contexts => "contexts",
            Mode::SortPicker => "sort_picker",
            Mode::CopyPicker => "copy_picker",
            Mode::Containers => "containers",
            Mode::SetImage => "set_image",
            Mode::Confirm => "confirm",
            Mode::Prompt => "prompt",
            Mode::Pulse => "pulse",
            Mode::Xray => "xray",
            Mode::Explain => "explain",
            Mode::Timeline => "timeline",
            Mode::Gitops => "gitops",
            Mode::Argocd => "argocd",
            Mode::Adjacent => "adjacent",
            Mode::FluxMenu => "flux_menu",
            Mode::TransferMenu => "transfer_menu",
            Mode::PortForwards => "port_forwards",
            Mode::Skins => "skins",
            Mode::Snapshots => "snapshots",
            Mode::Fleet => "fleet",
            Mode::Find => "find",
            Mode::PvcExplore => "pvc_explore",
            Mode::PortForwardPicker => "port_forward_picker",
        }
    }

    pub fn configure_keys(&mut self, cfg: &crate::config::KeysConfig) -> Vec<String> {
        let paths = self
            .config
            .base_path()
            .into_iter()
            .chain(self.config.dropin_paths())
            .chain(
                self.config
                    .override_paths(&self.cluster.context, &self.cluster.cluster_name),
            )
            .filter(|p| p.exists())
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let warnings = match Keymap::compile(cfg) {
            Ok(map) => {
                let warnings = map.warnings().to_vec();
                self.keymap = map;
                warnings
            }
            Err(errors) => {
                let kept = if self.keymap.is_default() {
                    "default keymap kept"
                } else {
                    "previous keymap kept"
                };
                errors.into_iter().map(|e| format!("{e}; {kept}")).collect()
            }
        };
        let mut warnings: Vec<_> = warnings
            .into_iter()
            .map(|w| {
                if paths.is_empty() {
                    w
                } else {
                    format!("{paths}: {w}")
                }
            })
            .collect();
        for (kind, name, binding, resources) in self
            .plugins
            .iter()
            .map(|p| {
                (
                    "plugin",
                    p.name.as_str(),
                    Some(p.key.as_str()),
                    p.scopes.as_slice(),
                )
            })
            .chain(
                self.bookmarks
                    .iter()
                    .map(|b| ("bookmark", b.name.as_str(), b.key.as_deref(), &[][..])),
            )
            .chain(
                self.workspaces
                    .iter()
                    .map(|w| ("workspace", w.name.as_str(), w.key.as_deref(), &[][..])),
            )
        {
            if let Some(binding) = binding
                && let Ok(chord) = crate::keys::KeyChord::parse(binding)
            {
                for (scope, action, chords) in self.keymap.entries() {
                    if scope == "table"
                        && action != Action::Faults
                        && (action != Action::Inspect
                            || resources.is_empty()
                            || resources.iter().any(|s| {
                                matches!(s.as_str(), "secrets" | "persistentvolumeclaims")
                            }))
                        && chords
                            .iter()
                            .any(|other| crate::keymap::overlaps(&chord, other))
                    {
                        warnings.push(format!("{kind} {name:?}: {} is hidden by keys.table.{} when that action is available", chord.label(), action.name()));
                    }
                }
            }
        }
        warnings
    }

    fn open_help(&mut self) {
        self.help_return = self.mode;
        self.help_filter.clear();
        self.help_scroll = 0;
        self.mode = Mode::Help;
    }

    pub(super) fn key_table(&mut self, key: KeyInput) {
        if let Some(index) = Action::FAVORITE_NAMESPACES
            .iter()
            .position(|action| Some(*action) == key.action)
        {
            if let Some(namespace) = self.favorite_namespace(index) {
                self.set_namespace(namespace);
            }
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Delete), _) => self.request_delete(false),
            (Some(Action::ForceDelete), _) => self.request_delete(true),
            (Some(Action::Refresh), _) => self.start_watch(),
            (Some(Action::Faults), _) if self.kind_plural == "pods" => {
                if !self.try_bookmark_key(key.event())
                    && !self.try_workspace_key(key.event())
                    && !self.try_plugin_key(key.event())
                {
                    self.faults_only = !self.faults_only;
                    self.invalidate_rows();
                    self.table_state.select(Some(0));
                }
            }
            (Some(Action::Filter), _) => self.mode = Mode::Filter,
            (Some(Action::Exit), _) => self.should_quit = true,
            (Some(Action::Back), _) => {
                if !self.marked.is_empty() {
                    self.marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.invalidate_rows();
                    // Dropping the filter also drops its server-side
                    // selectors, so the watch must widen back out.
                    self.sync_filter_selectors();
                    self.save_history_filter();
                } else if !self.pop_frame() {
                    // at root, nothing to pop
                }
            }
            (Some(Action::RangeDown), _) => self.extend_selection(1),
            (Some(Action::RangeUp), _) => self.extend_selection(-1),
            (Some(Action::Down), _) => self.move_selection(1),
            (Some(Action::Up), _) => self.move_selection(-1),
            (Some(Action::First), _) => self.table_state.select(Some(0)),
            (Some(Action::Last), _) => {
                let len = self.row_count();
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
            }
            (Some(Action::PageDown), _) => self.move_page(1),
            (Some(Action::PageUp), _) => self.move_page(-1),
            // Move the viewport; keep NAMESPACE/NAME in place.
            (Some(Action::Right), _) => self.scroll_columns(1),
            (Some(Action::Left), _) => self.scroll_columns(-1),
            // k9s: SPACE marks/unmarks the current row for bulk actions, then
            // advances so a range can be marked with repeated taps.
            (Some(Action::Mark), _) => {
                self.toggle_mark();
                self.move_selection(1);
            }
            (Some(Action::Open), _) => self.drill(),
            (Some(Action::Yaml), _) => self.open_detail(),
            (Some(Action::Describe), _) => self.describe(),
            // k9s: `x` shows a secret's data base64-decoded. Elsewhere `x`
            // stays free for user plugins (the fallthrough arm below).
            (Some(Action::Inspect), _) if self.kind_plural == "secrets" => {
                self.open_decoded_secret()
            }
            // `x` on a PVC opens the split-pane volume browser.
            (Some(Action::Inspect), _) if self.kind_plural == "persistentvolumeclaims" => {
                self.open_pvc_explore()
            }
            (Some(Action::Events), _) => self.open_events(),
            (Some(Action::Logs), _) => self.open_logs(),
            // Logs from the configured external provider ([providers.logs]).
            (Some(Action::ProviderLogs), _) => self.open_provider_logs(),
            (Some(Action::PreviousLogs), _) => self.open_previous_logs(),
            (Some(Action::Edit), _) => self.request_edit(),
            // k9s: `s` = shell on pods, scale on scalable workloads.
            (Some(Action::ShellOrScale), _) => {
                if self.kind_plural == "pods" {
                    self.request_exec();
                } else if self.kind_plural == "persistentvolumeclaims" {
                    // A PVC can't scale; `s` shells into the volume instead,
                    // through whatever pod mounts it.
                    self.request_pvc_shell();
                } else {
                    self.request_scale();
                }
            }
            (Some(Action::Attach), _) => self.request_attach(),
            (Some(Action::SetImage), _) => self.request_set_image(),
            (Some(Action::Node), _) => self.show_node(),
            (Some(Action::CopyName), _) => self.copy_name(),
            // Copy any displayed cell of the row via a field picker (`c`
            // above copies just the name).
            (Some(Action::CopyCell), _) => self.open_copy_picker(),
            (Some(Action::Owner), _) => self.jump_owner(),
            // `X` — explain why the selection is unhealthy (evidence-backed).
            (Some(Action::Explain), _) => self.open_explain(),
            // `T` — session-local state-change timeline for the selection.
            (Some(Action::Timeline), _) => self.open_timeline(),
            // `u` — the objects directly connected to the selection.
            (Some(Action::Adjacent), _) => self.open_adjacent(),
            (Some(Action::Cordon), _) => self.request_cordon(true),
            (Some(Action::Uncordon), _) => self.request_cordon(false),
            (Some(Action::Drain), _) => self.request_drain(),
            // Sorting: S opens the column picker, I inverts the direction.
            (Some(Action::Sort), _) => self.open_sort_picker(),
            (Some(Action::SortAge), _) => self.sort_by_age(),
            (Some(Action::InvertSort), _) => self.toggle_sort_dir(),
            // Wide mode: show wide-only columns (kubectl `-o wide`).
            (Some(Action::Wide), _) => self.toggle_wide(),
            // `f`/Shift-F = port-forward.
            (Some(Action::PortForward), _) => self.request_port_forward(),
            (Some(Action::Namespaces), _) => self.open_namespaces(),
            (Some(Action::NamespaceSelected), _) => self.select_resource_namespace(),
            // Browser-style view history: [ back, ] forward.
            (Some(Action::HistoryBack), _) => self.history_back(),
            (Some(Action::HistoryForward), _) => self.history_forward(),
            // Cycle workspace views, or the default resources in this namespace.
            (Some(Action::NextView), _) => {
                self.cycle_views(true);
            }
            (Some(Action::PreviousView), _) => {
                self.cycle_views(false);
            }
            // k9s: 0 = all namespaces.
            (Some(Action::AllNamespaces), _) => {
                self.save_history_filter();
                self.namespace.clear();
                self.drop_owner_scope();
                self.remember_namespace();
                self.flash = "namespace: all namespaces".into();
                self.flash_err = false;
                self.table_state.select(Some(0));
                self.record_history();
                self.start_watch();
            }
            // k9s: `r` = rollout restart on workloads, force-sync on external
            // secrets, rollback on a Helm release's revision history, else
            // refresh the watch.
            (Some(Action::RestartOrRefresh), _) => {
                if matches!(
                    self.kind_plural.as_str(),
                    "deployments" | "statefulsets" | "daemonsets"
                ) {
                    self.request_restart();
                } else if self.external_secret_kind() {
                    self.request_refresh_es();
                } else if self.kind_plural == "helmhistory" {
                    self.request_helm_rollback();
                } else {
                    self.start_watch();
                }
            }
            // Action menu on the marked rows, or current: Flux
            // suspend/resume/reconcile, ArgoCD Application suspend/resume/sync,
            // ArgoCD ApplicationSet suspend/resume, CronJob trigger/suspend/resume.
            // On pods, `t` is the file-transfer menu (kubectl cp) instead.
            (Some(Action::ActionMenu), _) => {
                if self.kind_plural == "pods" {
                    self.request_transfer();
                } else {
                    self.request_flux_menu();
                }
            }
            // User-defined bindings fall through here (built-ins take
            // priority): bookmarks first, then plugins. Any unhandled key — a
            // bare char, a function key — is offered to them. Ctrl/alt combos
            // are routed earlier, in `handle_key`, before the plain-key
            // bindings above can claim them.
            _ => {
                if !self.try_bookmark_key(key.event()) && !self.try_workspace_key(key.event()) {
                    self.try_plugin_key(key.event());
                }
            }
        }
    }

    pub(super) fn key_command(&mut self, key: KeyInput) {
        if key.action == Some(Action::Down) {
            if !self.cmd_suggestions.is_empty() {
                self.cmd_sel = (self.cmd_sel + 1) % self.cmd_suggestions.len();
                self.fill_resource_context();
            }
            return;
        }
        if key.action == Some(Action::Up) {
            if !self.cmd_suggestions.is_empty() {
                self.cmd_sel = self
                    .cmd_sel
                    .checked_sub(1)
                    .unwrap_or(self.cmd_suggestions.len() - 1);
                self.fill_resource_context();
            }
            return;
        }
        if key.action == Some(Action::Accept) {
            self.palette_accept();
            return;
        }
        if edit_action(key.action, &mut self.command) {
            self.update_suggestions();
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Back), _) => self.mode = self.palette_return,
            (Some(Action::Backspace), _) => {
                self.command.pop();
                self.update_suggestions();
            }
            (None, KeyCode::Char(c)) => {
                self.command.push(c);
                self.update_suggestions();
            }
            _ => {}
        }
    }

    fn fill_resource_context(&mut self) {
        if let Some(suggestion) = self
            .cmd_suggestions
            .get(self.cmd_sel)
            .filter(|s| s.kind == SuggestKind::Context)
            && let Some((head, argument)) = self.command.split_once(char::is_whitespace)
            && !is_ctx_command(head)
            && argument.trim_start().starts_with('@')
        {
            self.command = format!("{head} @{}", suggestion.label);
            // Keep the matching list so the next key can select another context.
        }
    }

    /// Run the highlighted palette suggestion (or the raw typed text).
    fn palette_accept(&mut self) {
        let mut typed = self.command.trim().to_string();
        let picked = self.cmd_suggestions.get(self.cmd_sel).cloned();
        self.mode = Mode::Table;
        self.command.clear();
        // Dispatch leaves the view the palette was opened from behind,
        // so run the cleanup its own esc path would have done. Help can sit
        // between the palette and that view, so unwrap its return destination.
        let source = if self.palette_return == Mode::Help {
            self.help_return
        } else {
            self.palette_return
        };
        match source {
            Mode::Logs => self.stop_log_stream(),
            Mode::Events => self.stop_event_stream(),
            _ => {}
        }
        self.cancel_explain_request();
        self.cancel_gitops_request();
        self.cancel_argocd_request();
        self.cancel_adjacent_request();
        self.help_return = Mode::Table;
        self.palette_return = Mode::Table;
        let query_head = typed.split_whitespace().next().unwrap_or("");
        let owns_command = PALETTE_COMMANDS
            .iter()
            .any(|c| c.names.contains(&query_head))
            || self
                .plugins
                .iter()
                .any(|p| p.palette.as_deref() == Some(query_head));
        if (self.cluster.resolve(query_head).is_some() || !owns_command)
            && (typed.contains(" /")
                || typed
                    .split_whitespace()
                    .any(|s| s.starts_with('@') || matches!(s, "-n" | "--namespace" | "--context")))
        {
            if let Some(s) = picked.as_ref().filter(|s| s.kind == SuggestKind::Context)
                && let Some((head, rest)) = typed.split_once(char::is_whitespace)
                && rest.trim_start().starts_with('@')
            {
                typed = format!("{head} @{}", s.label);
            }
            match crate::filter::ResourceQuery::parse(&typed) {
                Ok(query) => self.apply_resource_query(query),
                Err(error) => self.flash_warn(&format!("query: {error}")),
            }
            return;
        }
        // `:kind namespace` switches both at once (`:deploy social`,
        // `:cephclusters all`); only the first word selects the kind.
        let (head, ns_arg) = match typed.split_once(char::is_whitespace) {
            Some((h, rest)) => (h.to_string(), rest.split_whitespace().next()),
            None => (typed.clone(), None),
        };
        match picked.as_ref().map(|s| s.kind) {
            // Argument completions act on the highlighted suggestion:
            // apply the completed namespace/context, not the partial
            // text still in the buffer.
            Some(SuggestKind::Namespace) => {
                if let Some(s) = picked {
                    self.switch_kind_ns(&head, Some(s.label.as_str()));
                }
            }
            Some(SuggestKind::Context) => {
                if let Some(s) = picked {
                    self.switch_context(s.label);
                }
            }
            Some(SuggestKind::Bookmark) => {
                if let Some(s) = picked {
                    self.apply_bookmark_named(&s.label);
                }
            }
            Some(SuggestKind::Workspace) => {
                if let Some(s) = picked {
                    self.open_workspace_named(&s.label);
                }
            }
            // A name owned by a CRD resolves to the CRD even when a
            // built-in command shares it — the cluster's vocabulary
            // outranks ours, and the suggestion list already ranks the
            // resource first. After that an exact typed built-in wins
            // (stable muscle memory), then the highlighted suggestion,
            // then the raw text as a resource.
            _ => {
                let crd_owned = self.cluster.resolve(&head).is_some_and(|k| k.is_custom());
                if crd_owned {
                    self.switch_kind_ns(&head, ns_arg);
                } else if self.run_palette_command(&typed) {
                    // handled
                } else if let Some(s) = picked {
                    match s.kind {
                        SuggestKind::Command => {
                            if self
                                .plugins
                                .iter()
                                .any(|p| p.palette.as_deref() == Some(&s.label))
                            {
                                let args = typed
                                    .split_once(char::is_whitespace)
                                    .map(|(_, args)| args)
                                    .unwrap_or("");
                                self.run_palette_command(&format!("{} {args}", s.label));
                            } else {
                                self.run_palette_command(&s.label);
                            }
                        }
                        SuggestKind::Resource => self.switch_kind_ns(&s.label, ns_arg),
                        // Handled by the outer match arms above.
                        SuggestKind::Namespace
                        | SuggestKind::Context
                        | SuggestKind::Bookmark
                        | SuggestKind::Workspace => {}
                    }
                } else if !head.is_empty() {
                    self.switch_kind_ns(&head, ns_arg);
                }
            }
        }
    }

    /// Open the `:` command palette. Bound in the table and the document
    /// views (detail/diff/events/logs); `palette_return` remembers where it
    /// was opened so esc goes back there.
    pub(super) fn open_palette(&mut self) {
        self.palette_return = self.mode;
        self.mode = Mode::Command;
        self.command.clear();
        self.ensure_namespace_cache();
        self.update_suggestions();
    }

    /// Run a built-in palette action.
    pub(super) fn run_action(&mut self, action: PaletteAction) {
        self.stop_plugins();
        match action {
            PaletteAction::Quit => self.should_quit = true,
            PaletteAction::Ctx => self.open_contexts(),
            PaletteAction::Pulse => self.open_pulse(),
            PaletteAction::Xray => self.open_xray(),
            PaletteAction::Explain => self.open_explain(),
            PaletteAction::Timeline => self.open_timeline(),
            PaletteAction::Gitops => self.open_gitops(),
            PaletteAction::Argocd => self.open_argocd(),
            PaletteAction::Adjacent => self.open_adjacent(),
            PaletteAction::CanI => self.open_can_i(),
            PaletteAction::Journal => self.open_journal(),
            PaletteAction::Debug => self.request_debug(None),
            PaletteAction::DebugClean => self.request_debug_cleanup(),
            PaletteAction::Bundle => self.open_bundle(),
            PaletteAction::BundleSave => self.save_bundle(),
            PaletteAction::Snapshot => self.take_snapshot(""),
            PaletteAction::Snapshots => self.open_snapshots(),
            PaletteAction::Info => self.open_info(),
            PaletteAction::Fleet => self.open_fleet(),
            PaletteAction::Rightsize => self.open_rightsize(),
            PaletteAction::PvcExplore => self.open_pvc_explore(),
            PaletteAction::PvcClean => self.request_pvc_clean(),
            PaletteAction::Find => self.flash_warn("usage: :find <text>"),
            PaletteAction::Diff => self.open_diff(),
            PaletteAction::Events => self.switch_kind("events.events.k8s.io"),
            PaletteAction::PortForwards => self.open_port_forwards(),
            PaletteAction::ProviderLogs => self.open_provider_logs(),
            PaletteAction::Skin => self.open_skins(),
            PaletteAction::Helm => self.open_helm_releases(),
            PaletteAction::Notify => self.toggle_notify(),
            PaletteAction::Reload => self.reload_config(),
            PaletteAction::ConfigInfo => self.open_config_info(),
        }
    }

    /// Run a built-in command by any of its names/aliases. Returns `false` for
    /// empty or unknown input (so the caller can fall back to a resource kind).
    pub(super) fn run_palette_command(&mut self, cmd: &str) -> bool {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return false;
        }
        let mut parts = cmd.split_whitespace();
        if let Some(first) = parts.next()
            && first.eq_ignore_ascii_case("skin")
        {
            let rest = parts.collect::<Vec<_>>().join(" ");
            if rest.is_empty() {
                self.open_skins();
            } else {
                self.apply_skin(&rest);
            }
            return true;
        }
        // `:snapshot [format]` captures the current view; the optional arg is
        // the output format (text/json/yaml).
        let mut parts = cmd.split_whitespace();
        if let Some(first) = parts.next()
            && matches!(
                first.to_ascii_lowercase().as_str(),
                "snapshot" | "snap" | "dump"
            )
        {
            let rest = parts.collect::<Vec<_>>().join(" ");
            self.take_snapshot(&rest);
            return true;
        }
        // `:find <text>` sweeps object names across the common kinds.
        let mut parts = cmd.split_whitespace();
        if let Some(first) = parts.next()
            && matches!(first.to_ascii_lowercase().as_str(), "find" | "fd")
        {
            let rest = parts.collect::<Vec<_>>().join(" ");
            if rest.is_empty() {
                self.flash_warn("usage: :find <text>");
            } else {
                self.start_find(&rest);
            }
            return true;
        }
        // `:can-i <verb> <resource> [ns]` checks one action; bare `:can-i`
        // opens the overview.
        let mut parts = cmd.split_whitespace();
        if let Some(first) = parts.next()
            && matches!(
                first.to_ascii_lowercase().as_str(),
                "can-i" | "cani" | "can"
            )
        {
            let rest = parts.collect::<Vec<_>>().join(" ");
            if rest.is_empty() {
                self.open_can_i();
            } else {
                self.check_can_i(&rest);
            }
            return true;
        }
        let action = PALETTE_COMMANDS
            .iter()
            .find(|c| c.names.contains(&cmd))
            .map(|c| c.action);
        match action {
            Some(a) => {
                self.run_action(a);
                true
            }
            None => {
                let (name, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
                if name == "plugin-cancel" {
                    self.stop_plugins();
                    self.flash_warn("plugin cancelled");
                    return true;
                }
                if self.cluster.resolve(name).is_some() || crate::app::plugin_command_reserved(name)
                {
                    return false;
                }
                let plugin = self
                    .plugins
                    .iter()
                    .find(|p| p.palette.as_deref() == Some(name))
                    .cloned();
                if let Some(plugin) = plugin {
                    if !plugin.scopes.is_empty() && !plugin.scopes.contains(&self.kind_plural) {
                        self.flash_warn("plugin does not apply to this resource kind");
                    } else {
                        self.run_plugin(plugin, args);
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Recompute the command-palette suggestions: built-in commands and resource
    /// kinds, fuzzy-matched together. An empty query lists the resource catalog
    /// only (the browse default), so pressing `:`⏎ never fires a command.
    /// Only the first word is matched — anything after it is the namespace
    /// argument of `:kind namespace` and must not perturb the kind match.
    pub(super) fn update_suggestions(&mut self) {
        // Once a second word begins (`:<head> <arg>`), complete the argument:
        // context names after `:ctx`, namespaces after a resource kind. Fall
        // through to first-word matching when the head isn't completable, so a
        // half-typed head still lists commands/resources.
        if let Some((head, arg)) = self.command.split_once(char::is_whitespace).map(|(h, r)| {
            (
                h.trim().to_string(),
                r.split_whitespace().next().unwrap_or("").to_string(),
            )
        }) {
            if is_ctx_command(&head) {
                self.suggest_contexts(&arg);
                return;
            }
            if let Some(context) = arg.strip_prefix('@')
                && (self.cluster.resolve(&head).is_some()
                    || (!PALETTE_COMMANDS
                        .iter()
                        .any(|c| c.names.contains(&head.as_str()))
                        && !self
                            .plugins
                            .iter()
                            .any(|p| p.palette.as_deref() == Some(&head))))
            {
                if self.command.split_whitespace().count() == 2
                    && !self.command.ends_with(char::is_whitespace)
                {
                    self.suggest_contexts(context);
                } else {
                    self.cmd_suggestions.clear();
                    self.cmd_sel = 0;
                }
                return;
            }
            if self.cluster.resolve(&head).is_some() {
                self.suggest_namespaces(&arg);
                return;
            }
        }

        let q = self.command.split_whitespace().next().unwrap_or("");
        let mut scored: Vec<(i64, Suggestion)> = Vec::new();

        // Built-in commands: fuzzy over all names, display the canonical one.
        // Skipped for an empty query so they don't pre-empt the resource list.
        if !q.is_empty() {
            for c in PALETTE_COMMANDS {
                let best = c
                    .names
                    .iter()
                    .filter_map(|n| self.matcher.score(n, q))
                    .max();
                if let Some(score) = best {
                    scored.push((
                        score,
                        Suggestion {
                            label: c.names[0].to_string(),
                            kind: SuggestKind::Command,
                        },
                    ));
                }
            }
        }

        if !q.is_empty() {
            for name in self
                .plugins
                .iter()
                .filter(|p| p.scopes.is_empty() || p.scopes.contains(&self.kind_plural))
                .filter_map(|p| p.palette.as_deref())
                .chain(std::iter::once("plugin-cancel"))
            {
                if PALETTE_COMMANDS.iter().any(|c| c.names.contains(&name))
                    || self.cluster.resolve(name).is_some()
                {
                    continue;
                }
                if let Some(score) = self.matcher.score(name, q) {
                    scored.push((
                        score,
                        Suggestion {
                            label: name.into(),
                            kind: SuggestKind::Command,
                        },
                    ));
                }
            }
        }

        // Saved bookmarks, matched by name. They rank above resources so a
        // curated jump wins over an incidental catalog match, and they show on
        // an empty `:` so they're discoverable.
        for b in &self.bookmarks {
            let score = if q.is_empty() {
                Some(i64::MAX)
            } else {
                self.matcher.score(&b.name, q).map(|s| s + 1_000)
            };
            if let Some(score) = score {
                scored.push((
                    score,
                    Suggestion {
                        label: b.name.clone(),
                        kind: SuggestKind::Bookmark,
                    },
                ));
            }
        }

        // Saved workspaces, matched by name, ranked alongside bookmarks.
        for w in &self.workspaces {
            let score = if q.is_empty() {
                Some(i64::MAX)
            } else {
                self.matcher.score(&w.name, q).map(|s| s + 1_000)
            };
            if let Some(score) = score {
                scored.push((
                    score,
                    Suggestion {
                        label: w.name.clone(),
                        kind: SuggestKind::Workspace,
                    },
                ));
            }
        }

        // An exact alias/kind/plural hit (e.g. `hr` → helmreleases) outranks
        // every fuzzy match, so a shorthand lands on its target instead of an
        // alphabetically-earlier lookalike (hr → horizontalpodautoscalers).
        // Compared by resolved identity so any of a kind's names hits.
        let alias_target = if q.is_empty() {
            None
        } else {
            self.cluster.resolve(q).map(|k| k.title().to_lowercase())
        };

        // Resource catalog. The empty browse list is RBAC-filtered, but an
        // explicit query searches every discovered kind. Authorizers can return
        // an incomplete SelfSubjectRulesReview without marking it incomplete,
        // so using that result for search can hide resources the user can open.
        // Qualified entries are checked by their bare plural when browsing.
        for c in &self.cluster.catalog {
            let (plural, group) = match c.split_once('.') {
                Some((p, g)) => (p, Some(g)),
                None => (c.as_str(), None),
            };
            if q.is_empty() && !self.rbac_visible(plural) {
                continue;
            }
            if let Some(group) = group {
                let bare_matches = q.is_empty() || self.matcher.score(plural, q).is_some();
                let same_kind = self
                    .cluster
                    .resolve(plural)
                    .is_some_and(|k| k.ar.group.eq_ignore_ascii_case(group));
                if bare_matches && same_kind {
                    continue;
                }
            }
            let score = if q.is_empty() {
                Some(0)
            } else if alias_target.is_some()
                && self.cluster.resolve(c).map(|k| k.title().to_lowercase()) == alias_target
            {
                Some(i64::MAX)
            } else {
                self.matcher.score(c, q)
            };
            if let Some(score) = score {
                scored.push((
                    score,
                    Suggestion {
                        label: c.clone(),
                        kind: SuggestKind::Resource,
                    },
                ));
            }
        }

        rank_completions(&mut scored, |s| s.label.as_str(), !q.is_empty());
        self.cmd_suggestions = scored.into_iter().take(100).map(|(_, s)| s).collect();
        self.cmd_sel = 0;
    }

    /// Palette completions for `:<kind> <ns>`: cached namespaces fuzzy-matched
    /// against the partial argument, with a literal `all` for all-namespaces.
    /// An empty argument lists everything. Falls back gracefully to just `all`
    /// when the namespace cache is empty (e.g. listing is RBAC-restricted) —
    /// the raw typed namespace is still accepted verbatim on Enter.
    fn suggest_namespaces(&mut self, arg: &str) {
        let mut names: Vec<String> = vec!["all".to_string()];
        names.extend(
            self.ns_list
                .iter()
                .filter(|n| n.as_str() != "<all>")
                .cloned(),
        );
        let mut scored: Vec<(i64, String)> = Vec::new();
        for n in names {
            let score = if arg.is_empty() {
                0
            } else if let Some(s) = self.matcher.score(&n, arg) {
                s
            } else {
                continue;
            };
            scored.push((score, n));
        }
        rank_completions(&mut scored, |s| s.as_str(), !arg.is_empty());
        self.cmd_suggestions = scored
            .into_iter()
            .take(100)
            .map(|(_, label)| Suggestion {
                label,
                kind: SuggestKind::Namespace,
            })
            .collect();
        self.cmd_sel = 0;
    }

    /// Palette completions for `:ctx <name>`: cached kubeconfig contexts
    /// fuzzy-matched against the partial argument (empty lists all).
    fn suggest_contexts(&mut self, arg: &str) {
        let mut scored: Vec<(i64, String)> = Vec::new();
        for c in &self.all_contexts {
            let score = if arg.is_empty() {
                0
            } else if let Some(s) = self.matcher.score(c, arg) {
                s
            } else {
                continue;
            };
            scored.push((score, c.clone()));
        }
        rank_completions(&mut scored, |s| s.as_str(), !arg.is_empty());
        self.cmd_suggestions = scored
            .into_iter()
            .take(100)
            .map(|(_, label)| Suggestion {
                label,
                kind: SuggestKind::Context,
            })
            .collect();
        self.cmd_sel = 0;
    }

    /// Type the row filter. Local terms (fuzzy/inverse/column comparisons)
    /// apply live per keystroke; `-l`/`-f` selectors are sent to the API on
    /// ⏎, since that restarts the watch (see `sync_filter_selectors`).
    pub(super) fn key_filter(&mut self, key: KeyInput) {
        if edit_action(key.action, &mut self.filter) {
            self.invalidate_rows();
            self.table_state.select(Some(0));
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                self.filter.clear();
                self.mode = Mode::Table;
                self.sync_filter_selectors();
                self.save_history_filter();
            }
            (Some(Action::Accept), _) => {
                if let Some(err) = self.filter_error() {
                    self.flash_warn(&format!("filter: {err}"));
                } else {
                    self.mode = Mode::Table;
                    self.sync_filter_selectors();
                    self.save_history_filter();
                }
            }
            (Some(Action::Backspace), _) => {
                self.filter.pop();
            }
            (None, KeyCode::Char(c)) => self.filter.push(c),
            _ => {}
        }
        self.invalidate_rows();
        self.table_state.select(Some(0));
    }

    pub(super) fn key_scroll(&mut self, key: KeyInput, detail: bool) {
        if detail && key.action == Some(Action::Fullscreen) {
            self.document_fullscreen = !self.document_fullscreen;
            self.set_flash(format!(
                "fullscreen: {}",
                if self.document_fullscreen {
                    "on"
                } else {
                    "off"
                }
            ));
            return;
        }
        let target = if detail {
            &mut self.detail
        } else {
            &mut self.logs.view
        };
        match (key.action, key.code) {
            // Esc backs out of an active search first (like the table view);
            // `q` always leaves.
            (Some(Action::Back), _) if detail && !target.filter.is_empty() => {
                target.filter.clear();
            }
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                // The underlying view (table/xray) watch kept running, so there
                // is nothing to restart — just stop the log streams and return,
                // landing back on the same row.
                if !detail {
                    self.stop_log_stream();
                } else if self.mode == Mode::Events {
                    self.stop_event_stream();
                }
                self.stop_resource_refresh();
                self.clear_document_source();
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            // Search within the document (k9s `/` in YAML/describe views):
            // matches are highlighted in place while the full document stays
            // rendered, vim-style, and `n`/`N` step between them.
            (Some(Action::Filter), _) if detail => {
                self.doc_filter_return = self.mode;
                self.mode = Mode::DocFilter;
            }
            // Jump to the next / previous search match (vim `n`/`N`). No-op
            // when no search is active.
            (Some(Action::NextMatch), _) if detail => target.step_match(true),
            (Some(Action::PreviousMatch), _) if detail => target.step_match(false),
            (Some(Action::AutoRefresh), _) if matches!(self.mode, Mode::Detail | Mode::Diff) => {
                self.toggle_resource_refresh()
            }
            (Some(Action::ResetBaseline), _) if self.mode == Mode::Diff => {
                self.reset_diff_baseline();
            }
            // Copy the document to the clipboard (k9s `c`), same as the logs
            // view: an active search copies only the matching lines.
            (Some(Action::Copy), _) if detail => {
                self.copy_doc();
            }
            // `x` decodes the secret's data from inside its describe/YAML
            // view too — no need to back out to the table first.
            (Some(Action::DecodeSecret), _)
                if self.mode == Mode::Detail
                    && self.document_source.as_ref().is_some_and(|source| {
                        source.kind.ar.group.is_empty() && source.kind.ar.kind == "Secret"
                    }) =>
            {
                self.show_decoded_secret();
            }
            (Some(Action::Down), _) => target.scroll_by(1),
            (Some(Action::Up), _) => target.scroll_by(-1),
            (Some(Action::Left), _) => target.scroll_h(-5),
            (Some(Action::Right), _) => target.scroll_h(5),
            (Some(Action::PageDown), _) => target.scroll_by(20),
            (Some(Action::PageUp), _) => target.scroll_by(-20),
            (Some(Action::First), _) => {
                target.scroll = 0;
                target.hscroll = 0;
            }
            (Some(Action::Last), _) => target.scroll_to_bottom(),
            // k9s: `w` toggles line wrap; folding long lines is the other way to
            // read content that runs past the right edge.
            (Some(Action::Wrap), _) => {
                let on = target.toggle_wrap();
                self.flash = format!("wrap: {}", if on { "on" } else { "off" });
                self.flash_err = false;
            }
            _ => {}
        }
    }

    pub(super) fn key_logs(&mut self, key: KeyInput) {
        if key.action == Some(Action::LogMarker) {
            self.logs.add_marker(self.log_buffer_cap());
            self.set_flash("log marker added");
            return;
        }
        if let Some(anchor) = key.action.and_then(Action::log_anchor) {
            self.apply_log_anchor(anchor);
            return;
        }
        // Ctrl-S saves the buffer to a file (k9s).
        if key.action == Some(Action::Save) {
            self.save_logs();
            return;
        }
        match (key.action, key.code) {
            // k9s: `s` toggles autoscroll/follow (we also accept `f`).
            (Some(Action::Follow), _) => {
                self.logs.follow = !self.logs.follow;
                if self.logs.follow {
                    self.trim_log_buffer();
                }
                self.flash = format!(
                    "autoscroll: {}",
                    if self.logs.follow { "on" } else { "off" }
                );
                self.flash_err = false;
                return;
            }
            // k9s: `w` toggles line wrap.
            (Some(Action::Wrap), _) => {
                self.logs.wrap = !self.logs.wrap;
                self.flash = format!("wrap: {}", if self.logs.wrap { "on" } else { "off" });
                self.flash_err = false;
                return;
            }
            // Fullscreen: whole frame, no borders, so terminal text selection
            // copies clean lines (k9s binds `f`, taken here by follow).
            (Some(Action::Fullscreen), _) => {
                self.logs.fullscreen = !self.logs.fullscreen;
                self.flash = format!(
                    "fullscreen: {}",
                    if self.logs.fullscreen { "on" } else { "off" }
                );
                self.flash_err = false;
                return;
            }
            // Provider logs: `T` changes the lookback period (re-queries).
            (Some(Action::Lookback), _) => {
                if self.provider_logs_active() {
                    self.prompt_label = format!(
                        "lookback period — e.g. 30m, 4h, 2d (current: {})",
                        self.provider_lookback_label()
                    );
                    self.prompt_input.clear();
                    self.prompt_kind = Some(PromptKind::ProviderLookback);
                    self.mode = Mode::Prompt;
                } else {
                    self.flash_warn("lookback period applies to provider logs (L)");
                }
                return;
            }
            // Show or hide timestamps in the current buffer.
            (Some(Action::Timestamps), _) => {
                self.logs.toggle_timestamps();
                self.flash = format!(
                    "timestamps: {}",
                    if self.logs.timestamps { "on" } else { "off" }
                );
                self.flash_err = false;
                return;
            }
            // Stop / resume the live stream.
            (Some(Action::Stream), _) => {
                if self.logs.stopped {
                    self.logs.stopped = false;
                    self.flash = "log stream resumed".into();
                    self.flash_err = false;
                    self.retail_logs();
                } else {
                    self.logs.stopped = true;
                    self.stop_log_stream(); // abort log tasks; view watch untouched
                    self.flash = "log stream stopped".into();
                    self.flash_err = false;
                }
                return;
            }
            // k9s: `c` copies the (filtered) buffer to the clipboard.
            (Some(Action::Copy), _) => {
                self.copy_logs();
                return;
            }
            // Clear the on-screen buffer (the live stream keeps appending).
            (Some(Action::Clear), _) => {
                self.logs.clear_lines();
                self.logs.view.scroll = 0;
                self.flash = "log buffer cleared".into();
                self.flash_err = false;
                return;
            }
            (Some(Action::Filter), _) => {
                self.mode = Mode::LogFilter;
                return;
            }
            _ => {}
        }
        // Navigation. Any manual upward/relative move drops autoscroll and
        // freezes the view; jumping to the bottom (G/End) re-arms it, like
        // k9s. Scroll is clamped in display-row units (`viewport_rows`) so a
        // wrapped buffer doesn't jump to a stale line index when paused.
        let page = self.logs.viewport_h.max(1);
        // Deepest useful offset: last full page pinned to the viewport bottom.
        let max = self.logs.viewport_rows.saturating_sub(self.logs.viewport_h);
        let cur = self.logs.view.scroll;
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                self.stop_log_stream();
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            (Some(Action::Down), _) => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_add(1).min(max);
            }
            (Some(Action::Up), _) => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_sub(1);
            }
            (Some(Action::PageDown), _) => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_add(page).min(max);
            }
            (Some(Action::PageUp), _) => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_sub(page);
            }
            (Some(Action::First), _) => {
                self.logs.follow = false;
                self.logs.view.scroll = 0;
            }
            (Some(Action::Last), _) => {
                // Resume autoscroll; the next draw anchors to the bottom.
                self.logs.follow = true;
                self.trim_log_buffer();
            }
            _ => {}
        }
    }

    pub(super) fn key_log_filter(&mut self, key: KeyInput) {
        let mut edited = self.logs.filter.clone();
        if edit_action(key.action, &mut edited) {
            self.logs.set_filter(edited);
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                self.logs.set_filter(String::new());
                self.mode = Mode::Logs;
            }
            (Some(Action::Accept), _) => self.mode = Mode::Logs,
            (Some(Action::Backspace), _) => {
                let mut f = self.logs.filter.clone();
                f.pop();
                self.logs.set_filter(f);
            }
            (None, KeyCode::Char(c)) => {
                let mut f = self.logs.filter.clone();
                f.push(c);
                self.logs.set_filter(f);
            }
            _ => {}
        }
    }

    pub(super) fn key_help(&mut self, key: KeyInput) {
        let page = self.help_viewport_h.max(1);
        match (key.action, key.code) {
            // Esc backs out of an active search first, then closes help.
            (Some(Action::Back), _) if !self.help_filter.is_empty() => {
                self.help_filter.clear();
                self.help_scroll = 0;
            }
            (Some(Action::Back), _) | (Some(Action::Close), _) => {
                self.mode = self.help_return;
                self.help_return = Mode::Table;
            }
            (Some(Action::Filter), _) => {
                self.help_scroll = 0;
                self.doc_filter_return = self.mode;
                self.mode = Mode::DocFilter;
            }
            (Some(Action::Down), _) => {
                self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll);
            }
            (Some(Action::Up), _) => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            (Some(Action::PageDown), _) => {
                self.help_scroll = self
                    .help_scroll
                    .saturating_add(page)
                    .min(self.help_max_scroll);
            }
            (Some(Action::PageUp), _) => self.help_scroll = self.help_scroll.saturating_sub(page),
            (Some(Action::First), _) => self.help_scroll = 0,
            (Some(Action::Last), _) => self.help_scroll = self.help_max_scroll,
            _ => {}
        }
    }

    /// Type the search query for a single-document view (YAML/describe, diff,
    /// events, help). Mirrors [`Self::key_log_filter`]: enter keeps the query,
    /// esc clears it; either returns to the view it was opened from.
    pub(super) fn key_doc_filter(&mut self, key: KeyInput) {
        if edit_action(key.action, self.doc_filter_mut()) {
            return;
        }
        match (key.action, key.code) {
            (Some(Action::Back), _) => {
                self.doc_filter_mut().clear();
                self.mode = self.doc_filter_return;
            }
            (Some(Action::Accept), _) => {
                // Finalize: on a document view, jump to the first match so the
                // hit is on screen; help filters in place and needs no jump.
                if self.doc_filter_return != Mode::Help {
                    self.detail.focus_first_match();
                }
                self.mode = self.doc_filter_return;
            }
            (Some(Action::Backspace), _) => {
                self.doc_filter_mut().pop();
            }
            (None, KeyCode::Char(c)) => {
                self.doc_filter_mut().push(c);
            }
            _ => {}
        }
    }

    /// The query the doc search edits: the help view has its own buffer; every
    /// other doc view is backed by `detail`.
    fn doc_filter_mut(&mut self) -> &mut String {
        if self.doc_filter_return == Mode::Help {
            &mut self.help_filter
        } else {
            &mut self.detail.filter
        }
    }
}

/// True when `head` is one of the `:ctx` command's names, i.e. the argument
/// after it should complete against kubeconfig contexts.
fn is_ctx_command(head: &str) -> bool {
    PALETTE_COMMANDS
        .iter()
        .any(|c| matches!(c.action, PaletteAction::Ctx) && c.names.contains(&head))
}

/// Order scored palette completions: score descending, then label length,
/// then alphabetical. Skim scores only the matched characters — unmatched
/// trailing ones cost nothing — so short queries tie constantly (`serv`
/// scores `services` and `serviceaccounts` identically; issue #164), and a
/// purely alphabetical tie-break buried the shorter, denser match. Browse
/// lists (empty query: everything ties at one score) skip the length
/// tie-break so they stay alphabetical.
fn rank_completions<T>(scored: &mut [(i64, T)], label: fn(&T) -> &str, by_len: bool) {
    // Stable on purpose: `T` is `Suggestion` at one call site, whose `kind` is
    // not part of the ordering, so two suggestions sharing a label compare
    // equal and an unstable sort could swap which one the list offers first.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| {
                if by_len {
                    label(&a.1).len().cmp(&label(&b.1).len())
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| label(&a.1).cmp(label(&b.1)))
    });
}
