use super::*;

pub enum NamespacePatternAction {
    Select,
    Resource(String),
    Query(crate::filter::ResourceQuery),
    Resume,
}

pub(super) fn is_pattern(value: &str) -> bool {
    value.contains(['*', '?'])
}

fn matcher(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let expression = format!(
        "^{}$",
        regex::escape(pattern)
            .replace(r"\*", ".*")
            .replace(r"\?", ".")
    );
    regex::Regex::new(&expression)
}

impl App {
    pub fn namespace_is_pattern(&self) -> bool {
        is_pattern(&self.namespace)
    }

    pub(super) fn watch_namespaces(&self) -> Vec<String> {
        self.namespace_patterns
            .get(&self.namespace)
            .cloned()
            .unwrap_or_else(|| {
                if self.namespace_is_pattern() {
                    Vec::new()
                } else {
                    vec![self.namespace.clone()]
                }
            })
    }

    pub(super) fn refresh_namespace_selection(&mut self) {
        if self.namespace_is_pattern() {
            self.resolve_namespace_pattern(self.namespace.clone(), NamespacePatternAction::Select);
        } else {
            self.start_watch();
        }
    }

    pub(super) fn resolve_namespace_pattern(
        &mut self,
        pattern: String,
        action: NamespacePatternAction,
    ) {
        let re = match matcher(&pattern) {
            Ok(re) => re,
            Err(error) => {
                self.flash_warn(&format!("invalid namespace pattern: {error}"));
                return;
            }
        };
        self.namespace_request += 1;
        let request = self.namespace_request;
        let generation = self.generation;
        let client = self.cluster.client.clone();
        let kind = self.cluster.resolve("namespaces");
        let tx = self.tx.clone();
        self.mode = Mode::Table;
        self.set_flash(format!("finding namespaces: {pattern}"));
        self.tasks.push(tokio::spawn(async move {
            let result = if let Some(kind) = kind {
                let api: Api<DynamicObject> = Api::all_with(client, &kind.ar);
                match tokio::time::timeout(
                    Duration::from_secs(15),
                    api.list(&ListParams::default()),
                )
                .await
                {
                    Ok(Ok(list)) => Ok(list
                        .items
                        .into_iter()
                        .filter_map(|o| o.metadata.name)
                        .filter(|name| re.is_match(name))
                        .collect()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("namespace discovery timed out".to_string()),
                }
            } else {
                Err("namespace discovery is unavailable".to_string())
            };
            let _ = tx
                .send(Msg::NamespacePattern {
                    generation,
                    request,
                    pattern,
                    action,
                    result,
                })
                .await;
        }));
    }

    pub(super) fn finish_namespace_pattern(
        &mut self,
        pattern: String,
        action: NamespacePatternAction,
        result: Result<Vec<String>, String>,
    ) {
        let mut names = match result {
            Ok(names) if !names.is_empty() => names,
            result => {
                let unresolved = self.namespace_is_pattern()
                    && !self.namespace_patterns.contains_key(&self.namespace);
                if unresolved {
                    self.store.clear();
                    self.watch_key = None;
                    self.namespace_errors.clear();
                    self.clear_rows_cache();
                }
                let state = if unresolved {
                    "pattern unresolved; no resources loaded"
                } else {
                    "selection unchanged"
                };
                self.flash_warn(&match result {
                    Ok(_) => format!("no namespaces match {pattern}; {state}"),
                    Err(error) => format!("cannot select {pattern}: {error}; {state}"),
                });
                return;
            }
        };
        names.sort();
        names.dedup();
        self.namespace_patterns.insert(pattern.clone(), names);
        match action {
            NamespacePatternAction::Resource(resource) => {
                let Some(kind) = self.cluster.resolve(&resource) else {
                    if workloads::NAMES.contains(&resource.trim().to_lowercase().as_str()) {
                        self.open_workloads(Some(&pattern));
                        self.set_flash(format!("namespace: {}", self.namespace_label()));
                    }
                    return;
                };
                if kind.ar.plural == "namespaces" {
                    if self.kind_plural == "namespaces" {
                        self.prepare_namespace_return();
                    }
                    self.apply_namespace_selection(pattern);
                } else {
                    self.save_history_filter();
                    self.namespace = pattern;
                    self.set_root_view(kind);
                    self.remember_namespace();
                    self.record_history();
                    self.start_watch();
                }
            }
            NamespacePatternAction::Query(query) => {
                if let Some(kind) = self.cluster.resolve(&query.resource) {
                    self.apply_resolved_resource_query(query, kind);
                }
            }
            NamespacePatternAction::Resume => self.start_watch(),
            NamespacePatternAction::Select => self.apply_namespace_selection(pattern),
        }
        self.set_flash(format!("namespace: {}", self.namespace_label()));
    }

    pub(super) fn start_namespace_watches(
        &mut self,
        kind: &Kind,
        labels: Option<String>,
        fields: Option<String>,
    ) {
        if !self.namespace_is_pattern() || !kind.namespaced {
            self.tasks.push(self.cluster.spawn_watch(
                kind,
                if kind.namespaced { &self.namespace } else { "" },
                labels,
                fields,
                self.generation,
                self.tx.clone(),
            ));
            return;
        }
        for namespace in self.watch_namespaces() {
            let (tx, mut rx) = tokio::sync::mpsc::channel(256);
            self.tasks.push(self.cluster.spawn_watch(
                kind,
                &namespace,
                labels.clone(),
                fields.clone(),
                self.generation,
                tx,
            ));
            let tx = self.tx.clone();
            let generation = self.generation;
            self.tasks.push(tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if tx
                        .send(Msg::NamespaceWatch {
                            generation,
                            namespace: namespace.clone(),
                            event: Box::new(event),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::matcher;

    #[test]
    fn wildcard_names_match_the_whole_name() {
        for (pattern, name, expected) in [
            ("*-crons", "team-crons", true),
            ("*-crons", "team-crons-old", false),
            ("team-?", "team-a", true),
            ("team-?", "team-ab", false),
            ("team-*", "team-", true),
            ("team-*", "Team-a", false),
            ("team.[ab]*", "team-a", false),
            ("**-crons", "team-crons", true),
        ] {
            assert_eq!(
                matcher(pattern).unwrap().is_match(name),
                expected,
                "{pattern}: {name}"
            );
        }
    }
}
