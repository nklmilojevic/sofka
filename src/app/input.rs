use super::*;

impl App {
    // ----- key handling --------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        let before = match self.mode {
            Mode::Command => self.palette_return,
            Mode::Help => self.help_return,
            Mode::Filter | Mode::Confirm | Mode::Prompt | Mode::SortPicker | Mode::CopyPicker => {
                Mode::Table
            }
            other => other,
        };
        let run = self.plugin_run;
        let result = self.handle_key_inner(key);
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
        if self.should_quit || (self.plugin_run == run && self.mode != before && !overlay) {
            self.stop_plugins();
        }
        result
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.stop_plugins();
                    self.should_quit = true;
                    return Ok(());
                }
                // Compact mode: collapse the header to one line and hide the
                // footer (k9s ctrl-e/ctrl-g, folded into one toggle). Works in
                // every mode, so it never reaches the plain-key bindings.
                KeyCode::Char('e') => {
                    self.compact = !self.compact;
                    return Ok(());
                }
                KeyCode::Char('d') if self.mode == Mode::Table => {
                    self.request_delete(false);
                    return Ok(());
                }
                KeyCode::Char('k') if self.mode == Mode::Table => {
                    self.request_delete(true); // kill = force delete
                    return Ok(());
                }
                KeyCode::Char('r') if self.mode == Mode::Table => {
                    self.start_watch();
                    return Ok(());
                }
                KeyCode::Char('z')
                    if self.mode == Mode::Table
                        && self.kind_plural == "pods"
                        && key.modifiers == KeyModifiers::CONTROL =>
                {
                    if self.try_bookmark_key(key)
                        || self.try_workspace_key(key)
                        || self.try_plugin_key(key)
                    {
                        return Ok(());
                    }
                    self.faults_only = !self.faults_only;
                    self.invalidate_rows();
                    self.table_state.select(Some(0));
                    return Ok(());
                }
                KeyCode::Char('f')
                    if self.mode == Mode::Table && key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.move_page(1);
                    return Ok(());
                }
                KeyCode::Char('b')
                    if self.mode == Mode::Table && key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.move_page(-1);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Ctrl/alt combos in the table are user plugin chords (the reserved
        // built-in ctrl keys above already returned). Route them here so they
        // never fall through to the plain-key table bindings — `ctrl-g` must
        // not trigger `g`. Unmatched combos are swallowed rather than misfiring.
        if self.mode == Mode::Table
            && (key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT))
        {
            if !self.try_bookmark_key(key) && !self.try_workspace_key(key) {
                self.try_plugin_key(key);
            }
            return Ok(());
        }

        // Navigation screens share the command-palette and help bindings.
        // Text-entry pickers deliberately stay out of this path so `:` and `?`
        // remain ordinary input while filtering or filling a prompt.
        let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        if plain && self.has_global_view_shortcuts() {
            match key.code {
                KeyCode::Char(':') => {
                    self.open_palette();
                    return Ok(());
                }
                KeyCode::Char('?') if self.mode != Mode::Help => {
                    self.open_help();
                    return Ok(());
                }
                _ => {}
            }
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
            Mode::FluxMenu => self.key_flux_menu(key),
            Mode::TransferMenu => self.key_transfer_menu(key),
            Mode::PortForwards => self.key_port_forwards(key),
            Mode::Skins => self.key_skins(key),
            Mode::Snapshots => self.key_snapshots(key),
            Mode::Fleet => self.key_fleet(key),
            Mode::Find => self.key_find(key),
        }
        Ok(())
    }

    fn has_global_view_shortcuts(&self) -> bool {
        matches!(
            self.mode,
            Mode::Table
                | Mode::Detail
                | Mode::Logs
                | Mode::Help
                | Mode::Containers
                | Mode::Confirm
                | Mode::Pulse
                | Mode::Xray
                | Mode::Explain
                | Mode::Timeline
                | Mode::Gitops
                | Mode::Diff
                | Mode::Events
                | Mode::FluxMenu
                | Mode::TransferMenu
                | Mode::PortForwards
                | Mode::Skins
                | Mode::Snapshots
                | Mode::Fleet
                | Mode::Find
        )
    }

    fn open_help(&mut self) {
        self.help_return = self.mode;
        self.help_filter.clear();
        self.mode = Mode::Help;
    }

    pub(super) fn key_table(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if !self.marked.is_empty() {
                    self.marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.invalidate_rows();
                    // Dropping the filter also drops its server-side
                    // selectors, so the watch must widen back out.
                    self.sync_filter_selectors();
                } else if !self.pop_frame() {
                    // at root, nothing to pop
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.table_state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.row_count();
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
            }
            KeyCode::PageDown => self.move_page(1),
            KeyCode::PageUp => self.move_page(-1),
            // Move the viewport; keep NAMESPACE/NAME in place.
            KeyCode::Right => self.scroll_columns(1),
            KeyCode::Left => self.scroll_columns(-1),
            // k9s: SPACE marks/unmarks the current row for bulk actions, then
            // advances so a range can be marked with repeated taps.
            KeyCode::Char(' ') => {
                self.toggle_mark();
                self.move_selection(1);
            }
            KeyCode::Enter => self.drill(),
            KeyCode::Char('y') => self.open_detail(),
            KeyCode::Char('d') => self.describe(),
            // k9s: `x` shows a secret's data base64-decoded. Elsewhere `x`
            // stays free for user plugins (the fallthrough arm below).
            KeyCode::Char('x') if self.kind_plural == "secrets" => self.open_decoded_secret(),
            KeyCode::Char('E') => self.open_events(),
            KeyCode::Char('l') => self.open_logs(),
            // Logs from the configured external provider ([providers.logs]).
            KeyCode::Char('L') => self.open_provider_logs(),
            KeyCode::Char('p') => self.open_previous_logs(),
            KeyCode::Char('e') => self.request_edit(),
            // k9s: `s` = shell on pods, scale on scalable workloads.
            KeyCode::Char('s') => {
                if self.kind_plural == "pods" {
                    self.request_exec();
                } else {
                    self.request_scale();
                }
            }
            KeyCode::Char('a') => self.request_attach(),
            KeyCode::Char('i') => self.request_set_image(),
            KeyCode::Char('o') => self.show_node(),
            KeyCode::Char('c') => self.copy_name(),
            // Copy any displayed cell of the row via a field picker (`c`
            // above copies just the name).
            KeyCode::Char('Y') => self.open_copy_picker(),
            KeyCode::Char('J') => self.jump_owner(),
            // `X` — explain why the selection is unhealthy (evidence-backed).
            KeyCode::Char('X') => self.open_explain(),
            // `T` — session-local state-change timeline for the selection.
            KeyCode::Char('T') => self.open_timeline(),
            KeyCode::Char('C') => self.request_cordon(true),
            KeyCode::Char('U') => self.request_cordon(false),
            KeyCode::Char('D') => self.request_drain(),
            // Sorting: S opens the column picker, I inverts the direction.
            KeyCode::Char('S') => self.open_sort_picker(),
            KeyCode::Char('I') => self.toggle_sort_dir(),
            // Wide mode: show wide-only columns (kubectl `-o wide`).
            KeyCode::Char('w') => self.toggle_wide(),
            // `f`/Shift-F = port-forward.
            KeyCode::Char('f') | KeyCode::Char('F') => self.request_port_forward(),
            KeyCode::Char('n') => self.open_namespaces(),
            // Browser-style view history: [ back, ] forward.
            KeyCode::Char('[') => self.history_back(),
            KeyCode::Char(']') => self.history_forward(),
            // Cycle workspace views, or the default resources in this namespace.
            KeyCode::Tab => {
                self.cycle_views(true);
            }
            KeyCode::BackTab => {
                self.cycle_views(false);
            }
            // k9s: 0 = all namespaces.
            KeyCode::Char('0') => {
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
            KeyCode::Char('r') => {
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
            KeyCode::Char('t') => {
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
                if !self.try_bookmark_key(key) && !self.try_workspace_key(key) {
                    self.try_plugin_key(key);
                }
            }
        }
    }

    pub(super) fn key_command(&mut self, key: KeyEvent) {
        // Configured completion chords win over everything else (including
        // the shared line-editing chords), so a `[keys]` rebind like ctrl-w
        // always does what the user asked. Defaults: tab/down, backtab/up,
        // enter — see [`crate::config::PaletteKeys`].
        if self.palette_keys.next.iter().any(|c| c.matches(&key)) {
            if !self.cmd_suggestions.is_empty() {
                self.cmd_sel = (self.cmd_sel + 1) % self.cmd_suggestions.len();
            }
            return;
        }
        if self.palette_keys.prev.iter().any(|c| c.matches(&key)) {
            if !self.cmd_suggestions.is_empty() {
                self.cmd_sel = self
                    .cmd_sel
                    .checked_sub(1)
                    .unwrap_or(self.cmd_suggestions.len() - 1);
            }
            return;
        }
        if self.palette_keys.accept.iter().any(|c| c.matches(&key)) {
            self.palette_accept();
            return;
        }
        if edit_chord(&key, &mut self.command) {
            self.update_suggestions();
            return;
        }
        match key.code {
            KeyCode::Esc => self.mode = self.palette_return,
            KeyCode::Backspace => {
                self.command.pop();
                self.update_suggestions();
            }
            KeyCode::Char(c) => {
                self.command.push(c);
                self.update_suggestions();
            }
            _ => {}
        }
    }

    /// Run the highlighted palette suggestion (or the raw typed text).
    fn palette_accept(&mut self) {
        let typed = self.command.trim().to_string();
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
        self.help_return = Mode::Table;
        self.palette_return = Mode::Table;
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
    pub(super) fn key_filter(&mut self, key: KeyEvent) {
        if edit_chord(&key, &mut self.filter) {
            self.invalidate_rows();
            self.table_state.select(Some(0));
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Table;
                self.sync_filter_selectors();
            }
            KeyCode::Enter => {
                self.mode = Mode::Table;
                if let Some(err) = self.filter_error() {
                    self.flash_warn(&format!("filter: {err}"));
                } else {
                    self.sync_filter_selectors();
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
        self.invalidate_rows();
        self.table_state.select(Some(0));
    }

    pub(super) fn key_scroll(&mut self, key: KeyEvent, detail: bool) {
        let target = if detail {
            &mut self.detail
        } else {
            &mut self.logs.view
        };
        match key.code {
            // Esc backs out of an active search first (like the table view);
            // `q` always leaves.
            KeyCode::Esc if detail && !target.filter.is_empty() => {
                target.filter.clear();
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                // The underlying view (table/xray) watch kept running, so there
                // is nothing to restart — just stop the log streams and return,
                // landing back on the same row.
                if !detail {
                    self.stop_log_stream();
                } else if self.mode == Mode::Events {
                    self.stop_event_stream();
                }
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            // Search within the document (k9s `/` in YAML/describe views):
            // matches are highlighted in place while the full document stays
            // rendered, vim-style, and `n`/`N` step between them.
            KeyCode::Char('/') if detail => {
                self.doc_filter_return = self.mode;
                self.mode = Mode::DocFilter;
            }
            // Jump to the next / previous search match (vim `n`/`N`). No-op
            // when no search is active.
            KeyCode::Char('n') if detail => target.step_match(true),
            KeyCode::Char('N') if detail => target.step_match(false),
            // Copy the document to the clipboard (k9s `c`), same as the logs
            // view: an active search copies only the matching lines.
            KeyCode::Char('c') if detail => {
                self.copy_doc();
            }
            // `x` decodes the secret's data from inside its describe/YAML
            // view too — no need to back out to the table first.
            KeyCode::Char('x') if self.mode == Mode::Detail && self.kind_plural == "secrets" => {
                self.show_decoded_secret();
            }
            KeyCode::Char('j') | KeyCode::Down => target.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => target.scroll_by(-1),
            KeyCode::Char('h') | KeyCode::Left => target.scroll_h(-5),
            KeyCode::Char('l') | KeyCode::Right => target.scroll_h(5),
            KeyCode::PageDown | KeyCode::Char(' ') => target.scroll_by(20),
            KeyCode::PageUp => target.scroll_by(-20),
            KeyCode::Char('f') if key.modifiers == KeyModifiers::CONTROL => target.scroll_by(20),
            KeyCode::Char('b') if key.modifiers == KeyModifiers::CONTROL => target.scroll_by(-20),
            KeyCode::Char('g') | KeyCode::Home => {
                target.scroll = 0;
                target.hscroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => target.scroll_to_bottom(),
            // k9s: `w` toggles line wrap; folding long lines is the other way to
            // read content that runs past the right edge.
            KeyCode::Char('w') => {
                let on = target.toggle_wrap();
                self.flash = format!("wrap: {}", if on { "on" } else { "off" });
                self.flash_err = false;
            }
            _ => {}
        }
    }

    pub(super) fn key_logs(&mut self, key: KeyEvent) {
        // Ctrl-S saves the buffer to a file (k9s).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.save_logs();
            return;
        }
        match key.code {
            // k9s: `s` toggles autoscroll/follow (we also accept `f`).
            KeyCode::Char('s') | KeyCode::Char('f') => {
                self.logs.follow = !self.logs.follow;
                if self.logs.follow {
                    // Resumed tailing — trim the backlog accumulated while paused.
                    let overflow = self
                        .logs
                        .view
                        .lines
                        .len()
                        .saturating_sub(self.logs_cfg.buffer.max(1));
                    if overflow > 0 {
                        let dropped = self.logs.view.drain_front(overflow);
                        self.logs.retained_bytes = self.logs.retained_bytes.saturating_sub(dropped);
                    }
                }
                self.flash = format!(
                    "autoscroll: {}",
                    if self.logs.follow { "on" } else { "off" }
                );
                self.flash_err = false;
                return;
            }
            // k9s: `w` toggles line wrap.
            KeyCode::Char('w') => {
                self.logs.wrap = !self.logs.wrap;
                self.flash = format!("wrap: {}", if self.logs.wrap { "on" } else { "off" });
                self.flash_err = false;
                return;
            }
            // Fullscreen: whole frame, no borders, so terminal text selection
            // copies clean lines (k9s binds `f`, taken here by follow).
            KeyCode::Char('F') => {
                self.logs.fullscreen = !self.logs.fullscreen;
                self.flash = format!(
                    "fullscreen: {}",
                    if self.logs.fullscreen { "on" } else { "off" }
                );
                self.flash_err = false;
                return;
            }
            // k9s time anchors: `0` re-tails, `1`-`5` re-stream a window.
            KeyCode::Char(c @ '0'..='5') => {
                self.apply_log_anchor(c);
                return;
            }
            // Provider logs: `T` changes the lookback period (re-queries).
            KeyCode::Char('T') => {
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
            // k9s: `t` toggles timestamps (re-streams).
            KeyCode::Char('t') => {
                self.logs.timestamps = !self.logs.timestamps;
                self.flash = format!(
                    "timestamps: {}",
                    if self.logs.timestamps { "on" } else { "off" }
                );
                self.flash_err = false;
                if !self.logs.stopped {
                    self.retail_logs();
                }
                return;
            }
            // Stop / resume the live stream.
            KeyCode::Char('x') => {
                if self.logs.stopped {
                    self.logs.stopped = false;
                    self.flash = "log stream resumed".into();
                    self.flash_err = false;
                    self.retail_logs();
                } else {
                    self.logs.stopped = true;
                    self.stop_log_stream(); // abort log tasks; view watch untouched
                    self.flash = "log stream stopped (x to resume)".into();
                    self.flash_err = false;
                }
                return;
            }
            // k9s: `c` copies the (filtered) buffer to the clipboard.
            KeyCode::Char('c') => {
                self.copy_logs();
                return;
            }
            // Clear the on-screen buffer (the live stream keeps appending).
            KeyCode::Char('z') => {
                self.logs.clear_buffer();
                self.logs.view.scroll = 0;
                self.flash = "log buffer cleared".into();
                self.flash_err = false;
                return;
            }
            KeyCode::Char('/') => {
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
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.stop_log_stream();
                self.mode = self.return_mode;
                if self.return_mode == Mode::Table {
                    self.restore_selection();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_add(1).min(max);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_sub(1);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_add(page).min(max);
            }
            KeyCode::PageUp => {
                self.logs.follow = false;
                self.logs.view.scroll = cur.saturating_sub(page);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.logs.follow = false;
                self.logs.view.scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                // Resume autoscroll; the next draw anchors to the bottom.
                self.logs.follow = true;
            }
            _ => {}
        }
    }

    pub(super) fn key_log_filter(&mut self, key: KeyEvent) {
        let mut edited = self.logs.filter.clone();
        if edit_chord(&key, &mut edited) {
            self.logs.set_filter(edited);
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.logs.set_filter(String::new());
                self.mode = Mode::Logs;
            }
            KeyCode::Enter => self.mode = Mode::Logs,
            KeyCode::Backspace => {
                let mut f = self.logs.filter.clone();
                f.pop();
                self.logs.set_filter(f);
            }
            KeyCode::Char(c) => {
                let mut f = self.logs.filter.clone();
                f.push(c);
                self.logs.set_filter(f);
            }
            _ => {}
        }
    }

    pub(super) fn key_help(&mut self, key: KeyEvent) {
        match key.code {
            // Esc backs out of an active search first, then closes help.
            KeyCode::Esc if !self.help_filter.is_empty() => self.help_filter.clear(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                self.mode = self.help_return;
                self.help_return = Mode::Table;
            }
            KeyCode::Char('/') => {
                self.doc_filter_return = self.mode;
                self.mode = Mode::DocFilter;
            }
            _ => {}
        }
    }

    /// Type the search query for a single-document view (YAML/describe, diff,
    /// events, help). Mirrors [`Self::key_log_filter`]: enter keeps the query,
    /// esc clears it; either returns to the view it was opened from.
    pub(super) fn key_doc_filter(&mut self, key: KeyEvent) {
        if edit_chord(&key, self.doc_filter_mut()) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.doc_filter_mut().clear();
                self.mode = self.doc_filter_return;
            }
            KeyCode::Enter => {
                // Finalize: on a document view, jump to the first match so the
                // hit is on screen; help filters in place and needs no jump.
                if self.doc_filter_return != Mode::Help {
                    self.detail.focus_first_match();
                }
                self.mode = self.doc_filter_return;
            }
            KeyCode::Backspace => {
                self.doc_filter_mut().pop();
            }
            KeyCode::Char(c) => {
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
