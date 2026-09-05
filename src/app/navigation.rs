use super::*;

impl App {
    // ----- drill-down ----------------------------------------------------

    pub(super) fn drill(&mut self) {
        let Some(obj) = self.selected() else { return };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();

        // The literal arms are listed in `views::BUILTIN_DRILLS`, which is
        // how `compile` knows to warn about a `drill` configured on one of
        // them. Add a kind here, add it there.
        match self.kind_plural.as_str() {
            "namespaces" => self.set_namespace_and_return(&name),
            "nodes" => self.drill_to_pods(
                String::new(),
                None,
                Some(format!("spec.nodeName={name}")),
                format!("node/{name}"),
            ),
            "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
                match label_selector(&obj, "matchLabels") {
                    Some(sel) => self.drill_to_pods(
                        ns,
                        Some(sel),
                        None,
                        format!("{}/{name}", trim_s(&self.kind_plural)),
                    ),
                    None => self.flash_warn("no pod selector on this object"),
                }
            }
            "services" => match label_selector(&obj, "selector") {
                Some(sel) => self.drill_to_pods(ns, Some(sel), None, format!("svc/{name}")),
                None => self.flash_warn("service has no selector"),
            },
            "pods" => self.open_containers(&obj),
            // enter on a CRD lists its custom resources, not its YAML.
            "customresourcedefinitions" => self.drill_into_crd(&obj),
            // Helm: release -> every revision, revision -> its values.
            "helm" => self.drill_into_helm_history(&obj),
            "helmhistory" => self.open_helm_values(&obj),
            // A Flux HelmRelease bridges into the same native inspector:
            // enter opens the history of the Helm release it manages.
            "helmreleases" => self.drill_into_helmrelease(&obj),
            // Everything else is configuration: a `[views."…"].drill` opens
            // another kind scoped to this row; failing that, anything that
            // names a node (`[views."…"].node`) drills into it. Pods name one
            // too, but they drill into containers above.
            _ => {
                if let Some(drill) = self.configured_drill() {
                    self.drill_configured(&obj, &drill);
                } else if let Some(pointer) = self.node_pointer() {
                    self.show_node_at(&pointer);
                } else {
                    self.open_detail();
                }
            }
        }
    }

    /// The JSON Pointer holding the current kind's node name, if it has one.
    pub(super) fn node_pointer(&self) -> Option<String> {
        let ar = &self.kind.as_ref()?.ar;
        crate::views::node_pointer(&self.user_views, ar).map(str::to_string)
    }

    /// The `[views."…"].drill` for the current kind, if one is configured.
    fn configured_drill(&self) -> Option<crate::views::Drill> {
        let ar = &self.kind.as_ref()?.ar;
        crate::views::drill_for(&self.user_views, ar).cloned()
    }

    /// Drill from a row into the kind its view's `drill` names, scoped by the
    /// selector the row fills in — a NodePool into its NodeClaims, say.
    fn drill_configured(&mut self, obj: &DynamicObject, drill: &crate::views::Drill) {
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        let scope = format!("{}/{name}", trim_s(&self.kind_plural));
        self.drill_to(
            &drill.kind,
            ns,
            drill.labels_for(obj),
            drill.fields_for(obj),
            scope,
        );
    }

    /// Drill from a Flux `HelmRelease` row into the revision history of the
    /// Helm release it manages — the same view `:helm` → enter reaches, so
    /// values (`⏎`), manifest (`y`), notes (`d`), and rollback (`r`) all work
    /// without leaving the Flux object. Resolves the storage coordinates the
    /// way helm-controller composes them (releaseName/storageNamespace).
    pub(super) fn drill_into_helmrelease(&mut self, obj: &DynamicObject) {
        let Some(secrets) = self.cluster.resolve("secrets") else {
            self.flash_warn("secrets kind unavailable");
            return;
        };
        let (release, ns) = crate::helm::helmrelease_storage(obj);
        if release.is_empty() {
            self.flash_warn("HelmRelease has no resolvable release name");
            return;
        }
        self.push_frame();
        self.kind = Some(secrets);
        self.kind_plural = "helmhistory".into();
        self.namespace = ns;
        self.labels = Some(format!("owner=helm,name={release}"));
        self.fields = Some("type=helm.sh/release.v1".into());
        self.scope_label = Some(format!("helm/{release}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ {release} history");
        self.flash_err = false;
        self.start_watch();
    }

    /// Drill from an aggregated Helm release row into every revision of that
    /// release (`helm history`) — re-scopes the same underlying `secrets`
    /// watch with an added `name=<release>` label filter, narrowed to the
    /// release's own namespace.
    pub(super) fn drill_into_helm_history(&mut self, obj: &DynamicObject) {
        let Some(release) = crate::helm::release_name(obj) else {
            self.flash_warn("not a Helm release secret");
            return;
        };
        let release = release.to_string();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        self.push_frame();
        self.kind_plural = "helmhistory".into();
        self.namespace = ns;
        self.labels = Some(format!("owner=helm,name={release}"));
        self.fields = Some("type=helm.sh/release.v1".into());
        self.scope_label = Some(format!("helm/{release}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ {release} history");
        self.flash_err = false;
        self.start_watch();
    }

    /// Enter on a single revision (k9s: History view Enter -> Values): show
    /// the user-supplied value overrides for that revision.
    pub(super) fn open_helm_values(&mut self, obj: &DynamicObject) {
        self.set_return_mode();
        let Some(rel) = crate::helm::decode(obj) else {
            self.flash_warn("could not decode this Helm release revision");
            return;
        };
        let yaml = serde_yaml::to_string(&rel.config).unwrap_or_else(|e| format!("# error: {e}"));
        self.detail = Scrollable {
            title: format!("{} v{} — values", rel.name, rel.revision),
            lines: yaml.lines().map(String::from).collect(),
            ..Default::default()
        };
        self.mode = Mode::Detail;
    }

    /// Drill from a CustomResourceDefinition row into a listing of that CRD's
    /// custom resources. Resolves the target kind from discovery (the unambiguous
    /// group-qualified key), falling back to building it straight from the CRD
    /// spec if discovery didn't surface it.
    pub(super) fn drill_into_crd(&mut self, obj: &DynamicObject) {
        let d = &obj.data;
        let group = d
            .pointer("/spec/group")
            .and_then(Value::as_str)
            .unwrap_or("");
        let plural = d
            .pointer("/spec/names/plural")
            .and_then(Value::as_str)
            .unwrap_or("");
        let ckind = d
            .pointer("/spec/names/kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let scope = d
            .pointer("/spec/scope")
            .and_then(Value::as_str)
            .unwrap_or("Namespaced");
        if plural.is_empty() {
            self.flash_warn("CRD has no plural name");
            return;
        }

        let key = if group.is_empty() {
            plural.to_string()
        } else {
            format!("{plural}.{group}")
        };
        let kind = self.cluster.resolve(&key).or_else(|| {
            let version = crd_served_version(d)?;
            Some(Kind {
                ar: ApiResource {
                    api_version: if group.is_empty() {
                        version.clone()
                    } else {
                        format!("{group}/{version}")
                    },
                    group: group.to_string(),
                    version,
                    kind: ckind.to_string(),
                    plural: plural.to_string(),
                },
                namespaced: scope.eq_ignore_ascii_case("Namespaced"),
            })
        });
        let Some(kind) = kind else {
            self.flash_warn("could not resolve CRD's resource (no served version?)");
            return;
        };

        let crd_name = obj.metadata.name.clone().unwrap_or_default();
        // We already hold the CRD, so seed its printer-column fallback here
        // instead of re-fetching it when the watch starts.
        self.crd_views
            .entry(kind.ar.plural.to_lowercase())
            .or_insert_with(|| crate::views::printer_columns_view(d, &kind.ar.version));
        self.push_frame();
        self.kind_plural = kind.ar.plural.to_lowercase();
        self.kind = Some(kind);
        self.namespace = String::new(); // list across all namespaces
        self.labels = None;
        self.fields = None;
        self.scope_label = Some(format!("crd/{crd_name}"));
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ {plural}");
        self.flash_err = false;
        self.start_watch();
    }

    pub(super) fn drill_to_pods(
        &mut self,
        ns: String,
        labels: Option<String>,
        fields: Option<String>,
        scope: String,
    ) {
        self.drill_to("pods", ns, labels, fields, scope);
    }

    /// Push the current view and open `kind` (alias, plural, or kind name)
    /// under the given selectors — the shared tail of every drill that lands
    /// on a list. A cluster-scoped target ignores `ns`.
    pub(super) fn drill_to(
        &mut self,
        kind: &str,
        ns: String,
        labels: Option<String>,
        fields: Option<String>,
        scope: String,
    ) {
        let Some(target) = self.cluster.resolve(kind) else {
            self.flash_warn(&format!("{kind} kind unavailable"));
            return;
        };
        let plural = target.ar.plural.to_lowercase();
        self.push_frame();
        self.namespace = if target.namespaced { ns } else { String::new() };
        self.kind = Some(target);
        self.kind_plural = plural.clone();
        self.labels = labels;
        self.fields = fields;
        self.scope_label = Some(scope);
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = format!("↳ drilled into {plural}");
        self.flash_err = false;
        self.start_watch();
    }

    /// Scope the nodes list to one node by name — the shared tail of every
    /// jump to a node, and the same drill as any other so `o` and `enter`
    /// give the same feedback whichever kind they came from. The name is
    /// what we scope the watch by because `metadata.name` is the only field
    /// selector the apiserver indexes for nodes — a resource that pairs with
    /// its node by some other identifier (Karpenter's `status.providerID`,
    /// say) can't be selected on that.
    pub(super) fn goto_node(&mut self, node: &str, scope: String) {
        let by_name = Some(format!("metadata.name={node}"));
        self.drill_to("nodes", String::new(), None, by_name, scope);
    }

    /// Navigate to a specific object by (plural, namespace, name) — a
    /// name-filtered table view. Used to jump to the resource behind a GitOps
    /// chain node (owner/source/dependency).
    pub(super) fn navigate_to_target(&mut self, t: &crate::explain::Target) {
        let Some(kind) = self.cluster.resolve(&t.plural) else {
            self.flash_warn(&format!("cannot resolve '{}'", t.plural));
            return;
        };
        self.push_frame();
        self.kind_plural = kind.ar.plural.to_lowercase();
        self.kind = Some(kind);
        self.namespace = t.namespace.clone().unwrap_or_default();
        self.labels = None;
        self.fields = Some(format!("metadata.name={}", t.name));
        self.scope_label = Some(t.name.clone());
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.mode = Mode::Table;
        self.start_watch();
    }

    pub(super) fn set_namespace_and_return(&mut self, name: &str) {
        let ns = if name == "<all>" {
            String::new()
        } else {
            name.to_string()
        };
        // Return to the view we came from if there is one; otherwise (a `:ns`
        // root switch clears the stack) drop into pods scoped to the chosen
        // namespace — namespaces aren't namespaced, so staying on the list would
        // just reload it.
        if let Some(f) = self.stack.pop() {
            self.restore(f);
        } else if let Some(pods) = self.cluster.resolve("pods") {
            self.kind = Some(pods);
            self.kind_plural = "pods".into();
            self.labels = None;
            self.fields = None;
            self.scope_label = None;
            self.filter.clear();
            self.reset_sort();
            self.table_state.select(Some(0));
        }
        self.namespace = ns;
        self.note_recent_namespace(name);
        self.remember_namespace();
        self.set_flash(format!("namespace: {}", self.namespace_label()));
        self.record_history();
        self.start_watch();
    }
}
