use super::*;

/// How much confirmation an action needs, ordered weakest → strongest so the
/// strongest matching guardrail wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ConfirmLevel {
    /// Run immediately, no confirmation (an action's default when nothing
    /// forces a prompt).
    None,
    /// The ordinary y/n confirm dialog.
    Plain,
    /// Type the resource name (or the target count for a bulk action).
    TypeName,
    /// Type the current context name.
    TypeContext,
}

impl App {
    /// Evaluate the guardrails matching `action` on `targets` (a `(name, ns)`
    /// list). Returns the confirmation level to require — escalated from
    /// `base` — or `None` when a guardrail blocks the action outright (a
    /// `deny`, or a `max_bulk` the selection exceeds), having flashed why.
    pub(super) fn guard(
        &mut self,
        action: &str,
        plural: &str,
        targets: &[(String, String)],
        base: ConfirmLevel,
    ) -> Option<ConfirmLevel> {
        let ctx = self.cluster.context.clone();
        let mut level = base;
        let mut deny: Option<String> = None;
        let mut cap: Option<usize> = None;

        for g in &self.guardrails {
            if !list_matches(&g.contexts, &ctx)
                || !list_matches(&g.actions, action)
                || !list_matches(&g.resources, plural)
            {
                continue;
            }
            // A namespace filter matches if any target is in a listed namespace.
            if !g.namespaces.is_empty()
                && !targets
                    .iter()
                    .any(|(_, ns)| list_matches(&g.namespaces, ns))
            {
                continue;
            }
            if g.deny {
                deny = Some(g.reason.clone().unwrap_or_default());
            }
            if let Some(m) = g.max_bulk {
                cap = Some(cap.map_or(m, |c| c.min(m)));
            }
            if let Some(c) = &g.confirmation {
                level = level.max(parse_level(c));
            }
        }

        if let Some(reason) = deny {
            let tail = if reason.is_empty() {
                String::new()
            } else {
                format!(" — {reason}")
            };
            self.flash_warn(&format!("blocked by guardrail: {action} {plural}{tail}"));
            return None;
        }
        if let Some(max) = cap
            && targets.len() > max
        {
            self.flash_warn(&format!(
                "guardrail: {} exceeds the max of {max} for {action} — mark fewer",
                targets.len()
            ));
            return None;
        }
        Some(level)
    }

    /// Route an about-to-run action through its required confirmation. `Plain`
    /// uses the y/n dialog; the typed levels use a prompt that must match a
    /// resource name (`name_hint`) or the context; `None` runs immediately.
    pub(super) fn begin_guarded(
        &mut self,
        action: ConfirmAction,
        label: String,
        level: ConfirmLevel,
        name_hint: String,
    ) {
        match level {
            ConfirmLevel::None => self.run_confirm_action(action),
            ConfirmLevel::Plain => {
                self.confirm_label = label;
                self.confirm_action = Some(action);
                self.mode = Mode::Confirm;
            }
            ConfirmLevel::TypeName | ConfirmLevel::TypeContext => {
                let expected = if level == ConfirmLevel::TypeContext {
                    self.cluster.context.clone()
                } else {
                    name_hint
                };
                self.prompt_label = format!("⚠ guardrail — type '{expected}' to confirm:");
                self.prompt_input.clear();
                self.prompt_kind = Some(PromptKind::GuardConfirm {
                    expected,
                    action: Box::new(action),
                });
                self.mode = Mode::Prompt;
            }
        }
    }
}

fn parse_level(s: &str) -> ConfirmLevel {
    match s {
        "type-resource-name" => ConfirmLevel::TypeName,
        "type-context-name" => ConfirmLevel::TypeContext,
        _ => ConfirmLevel::Plain,
    }
}

/// Whether `value` matches any pattern (glob), or the list is empty ("any").
fn list_matches(patterns: &[String], value: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| glob(p, value))
}

/// Minimal glob: `*` matches any run of characters; every other char is
/// literal. Supports the `prod-*`, `*-system`, `*payments*` shapes guardrails
/// use.
fn glob(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // Leading segment must anchor at the start.
            if !value[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            // Trailing segment must anchor at the end.
            return value[pos..].ends_with(part);
        } else {
            match value[pos..].find(part) {
                Some(at) => pos += at + part.len(),
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_shapes() {
        assert!(glob("prod-*", "prod-eu"));
        assert!(!glob("prod-*", "staging-eu"));
        assert!(glob("*-system", "kube-system"));
        assert!(glob("*payments*", "acme-payments-prod"));
        assert!(glob("*", "anything"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
    }

    #[test]
    fn list_matches_treats_empty_as_any() {
        assert!(list_matches(&[], "whatever"));
        assert!(list_matches(&["a".into(), "b*".into()], "bee"));
        assert!(!list_matches(&["a".into(), "b*".into()], "cee"));
    }

    #[test]
    fn parse_level_maps_confirmation_strings() {
        assert_eq!(parse_level("confirm"), ConfirmLevel::Plain);
        assert_eq!(parse_level("type-resource-name"), ConfirmLevel::TypeName);
        assert_eq!(parse_level("type-context-name"), ConfirmLevel::TypeContext);
        assert_eq!(parse_level("bogus"), ConfirmLevel::Plain);
        // Ordering: context is the strongest.
        assert!(ConfirmLevel::TypeContext > ConfirmLevel::TypeName);
        assert!(ConfirmLevel::TypeName > ConfirmLevel::Plain);
    }
}
