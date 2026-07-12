use super::*;

impl App {
    // ----- key handling --------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
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
            Mode::Containers => self.key_containers(key),
            Mode::SetImage => self.key_set_image(key),
            Mode::Confirm => self.key_confirm(key),
            Mode::Prompt => self.key_prompt(key),
            Mode::Pulse => self.key_pulse(key),
            Mode::Xray => self.key_xray(key),
            Mode::FluxMenu => self.key_flux_menu(key),
            Mode::PortForwards => self.key_port_forwards(key),
            Mode::Skins => self.key_skins(key),
        }
        Ok(())
    }

    pub(super) fn key_table(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
                self.ensure_namespace_cache();
                self.update_suggestions();
            }
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
                let len = self.rows().len();
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
            }
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
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
            KeyCode::Char('J') => self.jump_owner(),
            KeyCode::Char('C') => self.request_cordon(true),
            KeyCode::Char('U') => self.request_cordon(false),
            KeyCode::Char('D') => self.request_drain(),
            // Sorting: S cycles the column, I inverts the direction.
            KeyCode::Char('S') => self.cycle_sort(),
            KeyCode::Char('I') => self.toggle_sort_dir(),
            // `f`/Shift-F = port-forward.
            KeyCode::Char('f') | KeyCode::Char('F') => self.request_port_forward(),
            KeyCode::Char('n') => self.open_namespaces(),
            // Browser-style view history: [ back, ] forward.
            KeyCode::Char('[') => self.history_back(),
            KeyCode::Char(']') => self.history_forward(),
            // k9s: 0 = all namespaces.
            KeyCode::Char('0') => {
                self.namespace.clear();
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
            // Flux CD: toggle suspend/resume on the marked rows, or current.
            KeyCode::Char('t') => self.request_flux_menu(),
            KeyCode::Char('?') => {
                self.help_filter.clear();
                self.mode = Mode::Help;
            }
            // User-defined plugins fall through here (built-ins take priority).
            KeyCode::Char(c) => self.try_plugin(c),
            _ => {}
        }
    }

    /// Run a config-defined plugin bound to `c` if it applies to the current
    /// kind. Blocked in read-only mode: plugins shell out to arbitrary
    /// commands, so we can't know they won't mutate the cluster.
    pub(super) fn try_plugin(&mut self, c: char) {
        let Some(plugin) = self
            .plugins
            .iter()
            .find(|p| {
                p.key == c
                    && (p.scopes.is_empty() || p.scopes.iter().any(|s| s == &self.kind_plural))
            })
            .cloned()
        else {
            return;
        };
        if self.deny_readonly() {
            return;
        }
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no selection for plugin");
            return;
        };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let ctx = self.cluster.context.clone();
        let res = self.kind_plural.clone();
        let subst = |s: &str| {
            s.replace("$NAMESPACE", &ns)
                .replace("$NS", &ns)
                .replace("$NAME", &name)
                .replace("$CONTEXT", &ctx)
                .replace("$RESOURCE", &res)
        };
        let mut argv = vec![subst(&plugin.command)];
        argv.extend(plugin.args.iter().map(|a| subst(a)));
        self.flash = format!("plugin: {}", plugin.name);
        self.flash_err = false;
        self.pending = Some(Suspend::Shell(argv));
    }

    pub(super) fn key_command(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Table,
            KeyCode::Down | KeyCode::Tab => {
                if !self.cmd_suggestions.is_empty() {
                    self.cmd_sel = (self.cmd_sel + 1) % self.cmd_suggestions.len();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                if !self.cmd_suggestions.is_empty() {
                    self.cmd_sel = self
                        .cmd_sel
                        .checked_sub(1)
                        .unwrap_or(self.cmd_suggestions.len() - 1);
                }
            }
            KeyCode::Enter => {
                let typed = self.command.trim().to_string();
                let picked = self.cmd_suggestions.get(self.cmd_sel).cloned();
                self.mode = Mode::Table;
                self.command.clear();
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
                    // An exact typed built-in wins (stable muscle memory), then
                    // the highlighted suggestion, then the raw text as a resource.
                    _ => {
                        if self.run_palette_command(&typed) {
                            // handled
                        } else if let Some(s) = picked {
                            match s.kind {
                                SuggestKind::Command => {
                                    self.run_palette_command(&s.label);
                                }
                                SuggestKind::Resource => self.switch_kind_ns(&s.label, ns_arg),
                                SuggestKind::Namespace | SuggestKind::Context => {}
                            }
                        } else if !head.is_empty() {
                            self.switch_kind_ns(&head, ns_arg);
                        }
                    }
                }
            }
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

    /// Run a built-in palette action.
    pub(super) fn run_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Quit => self.should_quit = true,
            PaletteAction::Ctx => self.open_contexts(),
            PaletteAction::Pulse => self.open_pulse(),
            PaletteAction::Xray => self.open_xray(),
            PaletteAction::Diff => self.open_diff(),
            PaletteAction::Events => self.open_events(),
            PaletteAction::PortForwards => self.open_port_forwards(),
            PaletteAction::Skin => self.open_skins(),
            PaletteAction::Helm => self.open_helm_releases(),
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
        let action = PALETTE_COMMANDS
            .iter()
            .find(|c| c.names.contains(&cmd))
            .map(|c| c.action);
        match action {
            Some(a) => {
                self.run_action(a);
                true
            }
            None => false,
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
                    .filter_map(|n| self.matcher.fuzzy_match(n, q))
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

        // An exact alias/kind/plural hit (e.g. `hr` → helmreleases) outranks
        // every fuzzy match, so a shorthand lands on its target instead of an
        // alphabetically-earlier lookalike (hr → horizontalpodautoscalers).
        let alias_target = if q.is_empty() {
            None
        } else {
            self.cluster.resolve(q).map(|k| k.ar.plural.to_lowercase())
        };

        // Resource catalog (RBAC-filtered).
        for c in self.cluster.catalog.iter().filter(|c| self.rbac_visible(c)) {
            let score = if q.is_empty() {
                Some(0)
            } else if alias_target.as_deref() == Some(c.as_str()) {
                Some(i64::MAX)
            } else {
                self.matcher.fuzzy_match(c, q)
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

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
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
            } else if let Some(s) = self.matcher.fuzzy_match(&n, arg) {
                s
            } else {
                continue;
            };
            scored.push((score, n));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
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
            } else if let Some(s) = self.matcher.fuzzy_match(c, arg) {
                s
            } else {
                continue;
            };
            scored.push((score, c.clone()));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
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
            // Search within the document (k9s `/` in YAML/describe views).
            KeyCode::Char('/') if detail => {
                self.doc_filter_return = self.mode;
                self.mode = Mode::DocFilter;
            }
            // Copy the document to the clipboard (k9s `c`), same as the logs
            // view: an active search copies only the matching lines.
            KeyCode::Char('c') if detail => {
                self.copy_doc();
            }
            KeyCode::Char('j') | KeyCode::Down => target.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => target.scroll_by(-1),
            KeyCode::Char('h') | KeyCode::Left => target.scroll_h(-5),
            KeyCode::Char('l') | KeyCode::Right => target.scroll_h(5),
            KeyCode::PageDown | KeyCode::Char(' ') => target.scroll_by(20),
            KeyCode::PageUp => target.scroll_by(-20),
            KeyCode::Char('g') | KeyCode::Home => {
                target.scroll = 0;
                target.hscroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                target.scroll = target.lines.len().saturating_sub(1)
            }
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
                    let overflow = self.logs.view.lines.len().saturating_sub(MAX_LOG_LINES);
                    if overflow > 0 {
                        self.logs.view.lines.drain(0..overflow);
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
        match key.code {
            KeyCode::Esc => {
                self.logs.filter.clear();
                self.mode = Mode::Logs;
            }
            KeyCode::Enter => self.mode = Mode::Logs,
            KeyCode::Backspace => {
                self.logs.filter.pop();
            }
            KeyCode::Char(c) => self.logs.filter.push(c),
            _ => {}
        }
    }

    pub(super) fn key_help(&mut self, key: KeyEvent) {
        match key.code {
            // Esc backs out of an active search first, then closes help.
            KeyCode::Esc if !self.help_filter.is_empty() => self.help_filter.clear(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => self.mode = Mode::Table,
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
        match key.code {
            KeyCode::Esc => {
                self.doc_filter_mut().clear();
                self.reset_doc_scroll();
                self.mode = self.doc_filter_return;
            }
            KeyCode::Enter => self.mode = self.doc_filter_return,
            KeyCode::Backspace => {
                self.doc_filter_mut().pop();
                self.reset_doc_scroll();
            }
            KeyCode::Char(c) => {
                self.doc_filter_mut().push(c);
                self.reset_doc_scroll();
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

    /// Snap back to the top when the query changes, so the first match is
    /// visible instead of a stale offset past the (now shorter) filtered list.
    fn reset_doc_scroll(&mut self) {
        if self.doc_filter_return != Mode::Help {
            self.detail.scroll = 0;
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
