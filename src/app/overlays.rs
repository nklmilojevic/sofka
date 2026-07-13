use super::*;

impl App {
    pub(super) fn key_containers(&mut self, key: KeyEvent) {
        let len = self.container_list.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.container_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.container_state, len, false),
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.launch_logs(
                        LogSource::Single {
                            ns,
                            pod: name.clone(),
                            container: Some(c.clone()),
                            previous: false,
                        },
                        format!("{name}:{c} — logs"),
                    );
                }
            }
            KeyCode::Char('p') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.launch_logs(
                        LogSource::Single {
                            ns,
                            pod: name.clone(),
                            container: Some(c.clone()),
                            previous: true,
                        },
                        format!("{name}:{c} — previous logs"),
                    );
                }
            }
            KeyCode::Char('s') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.exec_into(ns, name, Some(c));
                }
            }
            KeyCode::Char('L') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.launch_provider_container_logs(ns, name, c);
                }
            }
            _ => {}
        }
    }

    /// Execute a confirmed action. Shared by the y/n confirm dialog and the
    /// guardrail typed-confirmation prompt.
    pub(super) fn run_confirm_action(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Delete {
                targets,
                force,
                cascade,
                ..
            } => {
                self.do_delete(targets, force, cascade);
                self.marked.clear();
            }
            ConfirmAction::Edit { argv } => {
                self.pending = Some(Suspend::Shell(argv));
            }
            ConfirmAction::Exec { ns, name } => {
                self.exec_into(ns, name, None);
            }
            ConfirmAction::Drain { targets } => {
                self.do_drain_nodes(targets);
                self.marked.clear();
            }
            ConfirmAction::HelmRollback { ns, name, revision } => {
                self.do_helm_rollback(ns, name, revision);
            }
            ConfirmAction::HelmUninstall { targets } => {
                self.do_helm_uninstall(targets);
                self.marked.clear();
            }
            ConfirmAction::Plugin {
                jobs,
                name,
                mode,
                timeout,
            } => {
                self.launch_plugin(jobs, name, mode, timeout);
            }
        }
    }

    pub(super) fn key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(action) = self.confirm_action.take() {
                    self.run_confirm_action(action);
                }
                self.mode = Mode::Table;
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                let update = match self.confirm_action.as_mut() {
                    Some(ConfirmAction::Delete {
                        targets,
                        force,
                        cascade,
                        managed,
                    }) => {
                        *force = !*force;
                        Some((targets.clone(), *force, *cascade, managed.clone()))
                    }
                    _ => None,
                };
                if let Some((targets, force, cascade, managed)) = update {
                    self.confirm_label = delete_confirm_label(
                        &self.kind_plural,
                        &targets,
                        force,
                        cascade,
                        managed.as_deref(),
                    );
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                let update = match self.confirm_action.as_mut() {
                    Some(ConfirmAction::Delete {
                        targets,
                        force,
                        cascade,
                        managed,
                    }) => {
                        *cascade = cascade.next();
                        Some((targets.clone(), *force, *cascade, managed.clone()))
                    }
                    _ => None,
                };
                if let Some((targets, force, cascade, managed)) = update {
                    self.confirm_label = delete_confirm_label(
                        &self.kind_plural,
                        &targets,
                        force,
                        cascade,
                        managed.as_deref(),
                    );
                }
            }
            _ => {
                self.confirm_action = None;
                self.mode = Mode::Table;
            }
        }
    }

    pub(super) fn key_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // The lookback prompt is the one prompt opened from the logs
                // view; every other prompt starts at (and returns to) the table.
                self.mode = if self.prompt_over_logs() {
                    Mode::Logs
                } else {
                    Mode::Table
                };
                self.prompt_kind = None;
            }
            KeyCode::Enter => {
                let input = self.prompt_input.trim().to_string();
                self.mode = if self.prompt_over_logs() {
                    Mode::Logs
                } else {
                    Mode::Table
                };
                match self.prompt_kind.take() {
                    Some(PromptKind::Scale { ns, name }) => match input.parse::<i32>() {
                        Ok(n) if n >= 0 => self.do_scale(ns, name, n),
                        _ => self.flash_warn("invalid replica count"),
                    },
                    Some(PromptKind::PortForward { ns, name }) => {
                        if input.is_empty() {
                            self.flash_warn("no ports given");
                        } else {
                            let target = if self.kind_plural == "services" {
                                format!("svc/{name}")
                            } else {
                                name
                            };
                            self.start_port_forward(ns, target, input);
                        }
                    }
                    Some(PromptKind::SetImage {
                        ns,
                        name,
                        plural,
                        container,
                    }) => {
                        if input.is_empty() {
                            self.flash_warn("no image given");
                        } else {
                            self.do_set_image(ns, name, plural, container, input);
                        }
                    }
                    // Empty input = cancel, keep the current period.
                    Some(PromptKind::ProviderLookback) if !input.is_empty() => {
                        self.apply_provider_lookback(&input)
                    }
                    Some(PromptKind::ProviderLookback) => {}
                    Some(PromptKind::GuardConfirm { expected, action }) => {
                        if input == expected {
                            self.run_confirm_action(*action);
                        } else {
                            self.flash_warn("guardrail: input did not match — cancelled");
                        }
                    }
                    None => {}
                }
            }
            KeyCode::Backspace => {
                self.prompt_input.pop();
            }
            KeyCode::Char(c) => self.prompt_input.push(c),
            _ => {}
        }
    }
}
