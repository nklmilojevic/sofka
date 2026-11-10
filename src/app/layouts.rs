use super::*;

impl App {
    /// The kind the table lists, unless it's a synthetic view backed by an
    /// unrelated kind (helm releases on `secrets`).
    fn layout_kind(&self) -> Option<&Kind> {
        self.kind
            .as_ref()
            .filter(|kind| kind.ar.plural.to_lowercase() == self.kind_plural)
    }

    /// The named layout active for the current kind, if it isn't the default.
    pub fn active_layout(&self) -> Option<&str> {
        let kind = self.layout_kind()?;
        self.layouts.get(&kind.title()).map(String::as_str)
    }

    /// The named layouts configured for the current kind.
    pub(super) fn kind_layouts(&self) -> Vec<String> {
        self.layout_kind()
            .map(|kind| crate::views::layouts_for(&self.user_views, &kind.ar))
            .unwrap_or_default()
    }

    /// Make `name` the current kind's layout without rebuilding the table:
    /// `default` drops back to the default layout and `wide` turns wide mode
    /// on. Returns the warning to show when the kind has no such layout.
    pub(super) fn select_layout(&mut self, name: &str) -> Result<(), String> {
        let name = name.to_lowercase();
        let name = name.as_str();
        let Some(kind) = self.layout_kind().cloned() else {
            return Err("this view has no layouts".into());
        };
        match name {
            "default" => {
                self.layouts.remove(&kind.title());
            }
            "wide" => self.wide = true,
            _ if crate::views::layouts_for(&self.user_views, &kind.ar)
                .iter()
                .any(|layout| layout == name) =>
            {
                self.layouts.insert(kind.title(), name.to_string());
            }
            _ => return Err(format!("{} has no layout '{name}'", kind.ar.plural)),
        }
        Ok(())
    }

    /// Drop back to the current kind's default layout. An explicit resource
    /// selection that names no layout (`:pods`, a bookmark without `layout`)
    /// opens the default one instead of the layout remembered for the kind.
    pub(super) fn reset_layout(&mut self) {
        if let Some(title) = self.layout_kind().map(Kind::title) {
            self.layouts.remove(&title);
        }
    }

    /// `:layout <name>`: switch the current kind's layout and rebuild the
    /// table. A sort the old layout configured gives way to the new one's.
    pub(super) fn set_layout(&mut self, name: &str) {
        if let Err(warning) = self.select_layout(name) {
            self.flash_warn(&warning);
            return;
        }
        if self.sort_origin == SortOrigin::Configured {
            self.reset_sort();
        }
        self.refresh_view_spec();
        self.apply_remembered_sort();
        self.apply_view_sort();
        if let Some(kind) = self.kind.clone() {
            self.maybe_fetch_printer_columns(&kind);
        }
        self.save_history_filter();
        self.flash = match (name.to_lowercase().as_str(), self.active_layout()) {
            ("wide", _) => "wide columns: on".to_string(),
            (_, Some(layout)) => format!("layout: {}:{layout}", self.list_title()),
            (_, None) => format!("layout: {} default", self.list_title()),
        };
        self.flash_err = false;
    }

    /// `:layout` without a name: the palette, listing the current kind's
    /// layouts with `default` first. A kind without named layouts lists only
    /// `default`.
    pub(super) fn open_layout_picker(&mut self) {
        if self.layout_kind().is_none() {
            self.flash_warn("this view has no layouts");
            return;
        }
        self.open_palette();
        self.command = "layout ".into();
        self.update_suggestions();
    }

    /// Forget remembered layouts the cluster's kinds and the config no longer
    /// define. Each kind is resolved again, so a context switch checks the
    /// destination's API version. Returns a warning when the current kind's
    /// layout was dropped.
    pub(super) fn prune_layouts(&mut self) -> Option<String> {
        let current = self.layout_kind().map(Kind::title);
        let mut warning = None;
        for (title, name) in std::mem::take(&mut self.layouts) {
            let exists = self
                .cluster
                .resolve(&title)
                .filter(|kind| kind.title() == title)
                .is_some_and(|kind| {
                    crate::views::layouts_for(&self.user_views, &kind.ar).contains(&name)
                });
            if exists {
                self.layouts.insert(title, name);
            } else if current.as_deref() == Some(title.as_str()) {
                let plural = title.split('.').next().unwrap_or(&title);
                warning = Some(format!(
                    "layout '{name}' no longer exists for {plural}; using the default"
                ));
            }
        }
        warning
    }

    /// The kind a view key names, held to the API group its prefix names, as
    /// `[views."…"]` lookup matches it.
    fn resolve_view_key(&self, key: &str) -> Option<Kind> {
        let base = key.split_once('@').map_or(key, |(base, _)| base);
        let base = base.split_once(':').map_or(base, |(base, _)| base);
        let plural = crate::views::key_plural(base);
        let Some((prefix, _)) = base.rsplit_once('/') else {
            return self.cluster.resolve(plural);
        };
        let group = prefix.split_once('/').map_or(prefix, |(group, _)| group);
        if group == "v1" {
            self.cluster
                .resolve(plural)
                .filter(|kind| kind.ar.group.is_empty())
        } else {
            self.cluster.resolve(&format!("{plural}.{group}"))
        }
    }

    /// Palette entries for every configured layout, as `plural:layout`.
    pub(super) fn layout_entries(&self) -> Vec<String> {
        let mut entries: Vec<String> = self
            .user_views
            .keys()
            .filter_map(|key| {
                let layout = crate::views::key_layout(key)?;
                let kind = self.resolve_view_key(key)?;
                crate::views::layouts_for(&self.user_views, &kind.ar)
                    .iter()
                    .any(|name| name == layout)
                    .then(|| format!("{}:{layout}", kind.title()))
            })
            .collect();
        entries.sort();
        entries.dedup();
        entries
    }
}
