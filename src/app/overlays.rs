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
            _ => {}
        }
    }

    pub(super) fn key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Some(action) = self.confirm_action.take() {
                    match action {
                        ConfirmAction::Delete { targets, force } => {
                            self.do_delete(targets, force);
                            self.marked.clear();
                        }
                        ConfirmAction::Drain { targets } => {
                            self.do_drain_nodes(targets);
                            self.marked.clear();
                        }
                    }
                }
                self.mode = Mode::Table;
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                let update = match self.confirm_action.as_mut() {
                    Some(ConfirmAction::Delete { targets, force }) => {
                        *force = !*force;
                        Some((targets.clone(), *force))
                    }
                    _ => None,
                };
                if let Some((targets, force)) = update {
                    self.confirm_label = delete_confirm_label(&self.kind_plural, &targets, force);
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
                self.prompt_kind = None;
                self.mode = Mode::Table;
            }
            KeyCode::Enter => {
                let input = self.prompt_input.trim().to_string();
                self.mode = Mode::Table;
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
