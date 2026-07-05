use super::*;

impl App {
    // ----- drill-down ----------------------------------------------------

    pub(super) fn drill(&mut self) {
        let Some(obj) = self.selected() else { return };
        let name = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();

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
            _ => self.open_detail(),
        }
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
        let Some(pods) = self.cluster.resolve("pods") else {
            self.flash_warn("pods kind unavailable");
            return;
        };
        self.push_frame();
        self.kind = Some(pods);
        self.kind_plural = "pods".into();
        self.namespace = ns;
        self.labels = labels;
        self.fields = fields;
        self.scope_label = Some(scope);
        self.filter.clear();
        self.reset_sort();
        self.table_state.select(Some(0));
        self.flash = "↳ drilled into pods".into();
        self.flash_err = false;
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
        self.flash = format!("namespace: {}", self.namespace_label());
        self.flash_err = false;
        self.record_history();
        self.start_watch();
    }
}
