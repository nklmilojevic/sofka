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
            // Transfer files to/from this container (`kubectl cp -c`).
            KeyCode::Char('t') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                    && let Some((ns, name)) = self.container_pod.clone()
                {
                    self.open_transfer_menu(ns, name, Some(c));
                }
            }
            // Debug an ephemeral container targeting this container's namespace
            // (`kubectl debug --target`). The picker's pod is the selected row,
            // so request_debug reads it back from the table selection.
            KeyCode::Char('d') => {
                if let Some(i) = self.container_state.selected()
                    && let Some(c) = self.container_list.get(i).cloned()
                {
                    self.mode = Mode::Table;
                    self.request_debug(Some(c));
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
                // argv is `<kubectl> edit <kind> <name> [-n <ns>]`; recover the
                // name (and namespace) that follow `edit` for the journal entry.
                let label = argv
                    .iter()
                    .position(|a| a == "edit")
                    .and_then(|i| argv.get(i + 2))
                    .map(|name| match argv.iter().position(|a| a == "-n") {
                        Some(j) => match argv.get(j + 1) {
                            Some(ns) => format!("{name} in {ns}"),
                            None => name.clone(),
                        },
                        None => name.clone(),
                    })
                    .unwrap_or_default();
                self.note_action("edit", label);
                self.pending = Some(Suspend::Shell(argv));
            }
            ConfirmAction::Exec { ns, name } => {
                self.exec_into(ns, name, None);
            }
            ConfirmAction::Transfer {
                ns,
                pod,
                container,
                src,
                dest,
            } => {
                self.start_transfer(ns, pod, container, true, src, dest);
            }
            ConfirmAction::Drain { targets } => {
                self.do_drain_nodes(targets);
                self.marked.clear();
            }
            ConfirmAction::Restart { kind, name, ns } => {
                self.do_restart(kind, name, ns);
            }
            ConfirmAction::HelmRollback { ns, name, revision } => {
                self.do_helm_rollback(ns, name, revision);
            }
            ConfirmAction::HelmUninstall { targets } => {
                self.do_helm_uninstall(targets);
                self.marked.clear();
            }
            ConfirmAction::NodeDebug {
                node,
                image,
                namespace,
                profile,
            } => {
                self.do_node_debug(node, image, namespace, profile);
            }
            ConfirmAction::CleanupDebuggers => {
                self.do_cleanup_debuggers();
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
        if edit_chord(&key, &mut self.prompt_input) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                // Most prompts start at (and return to) the table; the
                // lookback and rename-context prompts return to the view
                // they were opened from.
                self.mode = if self.prompt_over_logs() {
                    Mode::Logs
                } else if self.prompt_over_contexts() {
                    Mode::Contexts
                } else {
                    Mode::Table
                };
                self.prompt_kind = None;
            }
            KeyCode::Enter => {
                let input = self.prompt_input.trim().to_string();
                self.mode = if self.prompt_over_logs() {
                    Mode::Logs
                } else if self.prompt_over_contexts() {
                    Mode::Contexts
                } else {
                    Mode::Table
                };
                match self.prompt_kind.take() {
                    Some(PromptKind::Scale { targets }) => match input.parse::<i32>() {
                        Ok(n) if n >= 0 => self.do_scale(targets, n),
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
                    Some(PromptKind::Debug { ns, pod, target }) => {
                        if input.is_empty() {
                            self.flash_warn("no debug image given");
                        } else {
                            self.do_debug(ns, pod, target, input);
                        }
                    }
                    // The transfer prompts chain: source path first, then the
                    // destination (prefilled with the source's file name on
                    // download, since it usually lands in the CWD as-is).
                    Some(PromptKind::Transfer {
                        ns,
                        pod,
                        container,
                        upload,
                        src: None,
                    }) => {
                        if input.is_empty() {
                            self.flash_warn("no path given — transfer cancelled");
                        } else {
                            self.prompt_label = if upload {
                                format!("Upload {input} to {pod} — remote path:")
                            } else {
                                format!("Download {pod}:{input} — local path:")
                            };
                            self.prompt_input = if upload {
                                String::new()
                            } else {
                                input.rsplit('/').next().unwrap_or_default().to_string()
                            };
                            self.prompt_kind = Some(PromptKind::Transfer {
                                ns,
                                pod,
                                container,
                                upload,
                                src: Some(input),
                            });
                            self.mode = Mode::Prompt;
                        }
                    }
                    Some(PromptKind::Transfer {
                        ns,
                        pod,
                        container,
                        upload,
                        src: Some(src),
                    }) => {
                        if input.is_empty() {
                            self.flash_warn("no path given — transfer cancelled");
                        } else {
                            self.do_transfer(ns, pod, container, upload, src, input);
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
                    // Empty input = cancel, keep the old name.
                    Some(PromptKind::RenameContext { old }) if !input.is_empty() => {
                        self.rename_context(old, input);
                    }
                    Some(PromptKind::RenameContext { .. }) => {}
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

    /// Port-forward picker (`f` on a pod/service): single-select over the
    /// object's declared ports, plus a "Custom…" entry that falls through to
    /// the typed prompt.
    pub(super) fn key_port_forward_picker(&mut self, key: KeyEvent) {
        let len = self.pf_picker_items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Table,
            KeyCode::Char('j') | KeyCode::Down => list_step(&mut self.pf_picker_state, len, true),
            KeyCode::Char('k') | KeyCode::Up => list_step(&mut self.pf_picker_state, len, false),
            KeyCode::Enter => {
                let Some(i) = self.pf_picker_state.selected() else {
                    return;
                };
                let Some(item) = self.pf_picker_items.get(i).cloned() else {
                    return;
                };
                let Some((ns, name)) = self.pf_picker_target.clone() else {
                    return;
                };
                if item == "Custom…" {
                    self.prompt_label =
                        format!("Port-forward {name} (LOCAL:REMOTE, e.g. 8080:80):");
                    self.prompt_input.clear();
                    self.prompt_kind = Some(PromptKind::PortForward { ns, name });
                    self.mode = Mode::Prompt;
                } else {
                    // Extract the "LOCAL:REMOTE" portion before any "  (name)" suffix.
                    let ports = item.split_whitespace().next().unwrap_or(&item).to_string();
                    let target = if self.kind_plural == "services" {
                        format!("svc/{name}")
                    } else {
                        name
                    };
                    self.start_port_forward(ns, target, ports);
                    self.mode = Mode::Table;
                }
            }
            _ => {}
        }
    }
}
