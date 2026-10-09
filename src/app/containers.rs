use super::*;

pub(crate) struct ContainerDetails {
    pub kind: &'static str,
    pub ready: Option<bool>,
    pub state: String,
    pub restarts: Option<u64>,
    pub image: String,
    pub ports: String,
    pub probes: [bool; 3],
}

fn container_details_of(obj: &DynamicObject) -> HashMap<String, ContainerDetails> {
    let mut details = HashMap::new();
    for (spec, status, kind) in [
        ("containers", "containerStatuses", "regular"),
        ("initContainers", "initContainerStatuses", "init"),
        (
            "ephemeralContainers",
            "ephemeralContainerStatuses",
            "ephemeral",
        ),
    ] {
        let statuses = obj.data.pointer(&format!("/status/{status}"));
        for container in obj
            .data
            .pointer(&format!("/spec/{spec}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = container.get("name").and_then(Value::as_str) else {
                continue;
            };
            let status = statuses
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|s| s.get("name").and_then(Value::as_str) == Some(name));
            let state = status
                .and_then(|s| s.get("state"))
                .map(container_state)
                .unwrap_or_else(|| "Unknown".into());
            let ports: Vec<_> = container
                .get("ports")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|port| {
                    let number = port.get("containerPort")?.as_u64()?;
                    let protocol = port
                        .get("protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("TCP");
                    let name = port.get("name").and_then(Value::as_str).unwrap_or("");
                    Some(if name.is_empty() {
                        format!("{number}/{protocol}")
                    } else {
                        format!("{name}:{number}/{protocol}")
                    })
                })
                .collect();
            details.insert(
                name.into(),
                ContainerDetails {
                    kind: if kind == "init"
                        && container.get("restartPolicy").and_then(Value::as_str) == Some("Always")
                    {
                        "sidecar"
                    } else {
                        kind
                    },
                    ready: status.and_then(|s| s.get("ready")).and_then(Value::as_bool),
                    state,
                    restarts: status
                        .and_then(|s| s.get("restartCount"))
                        .and_then(Value::as_u64),
                    image: container
                        .get("image")
                        .and_then(Value::as_str)
                        .unwrap_or("-")
                        .into(),
                    ports: if ports.is_empty() {
                        "none declared".into()
                    } else {
                        ports.join(", ")
                    },
                    probes: ["startupProbe", "readinessProbe", "livenessProbe"]
                        .map(|probe| container.get(probe).is_some_and(Value::is_object)),
                },
            );
        }
    }
    details
}

fn container_state(state: &Value) -> String {
    for (key, fallback) in [
        ("waiting", "Waiting"),
        ("terminated", "Terminated"),
        ("running", "Running"),
    ] {
        let Some(value) = state.get(key).filter(|v| v.is_object()) else {
            continue;
        };
        if let Some(reason) = value
            .get("reason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return reason.into();
        }
        if key == "terminated" {
            if let Some(signal) = value
                .get("signal")
                .and_then(Value::as_i64)
                .filter(|&s| s != 0)
            {
                return format!("Signal:{signal}");
            }
            if let Some(code) = value.get("exitCode").and_then(Value::as_i64) {
                return if code == 0 {
                    "Completed".into()
                } else {
                    format!("ExitCode:{code}")
                };
            }
        }
        return fallback.into();
    }
    "Unknown".into()
}

impl App {
    pub(super) fn open_containers(&mut self, obj: &DynamicObject) {
        if container_names(obj).is_empty() {
            self.flash_warn("no containers found");
            return;
        }
        self.container_pod = Some((
            obj.metadata.namespace.clone().unwrap_or_default(),
            obj.metadata.name.clone().unwrap_or_default(),
        ));
        self.update_containers(obj);
        self.container_state.select(Some(0));
        self.mode = Mode::Containers;
    }

    fn update_containers(&mut self, obj: &DynamicObject) {
        let selected = self
            .container_state
            .selected()
            .and_then(|i| self.container_list.get(i))
            .cloned();
        self.container_details = container_details_of(obj);
        self.container_list = self.container_details.keys().cloned().collect();
        self.container_list.sort();
        if let Some(default) = default_container(obj)
            && let Some(i) = self.container_list.iter().position(|n| *n == default)
        {
            self.container_list[..=i].rotate_right(1);
        }
        self.container_resources = container_resources_of(obj).into_iter().collect();
        self.container_qos = qos_class(obj);
        self.container_state
            .select(selected.and_then(|name| self.container_list.iter().position(|n| n == &name)));
    }

    pub(super) fn sync_container_picker(&mut self) {
        if self.mode != Mode::Containers
            && !(self.mode == Mode::Command && self.palette_return == Mode::Containers)
        {
            return;
        }
        let Some((ns, pod)) = &self.container_pod else {
            return;
        };
        if let Some(obj) = self.store.shared(&format!("{ns}/{pod}")) {
            self.update_containers(&obj);
        } else {
            self.container_list.clear();
            self.container_details.clear();
            self.container_resources.clear();
            self.container_qos.clear();
            self.container_state.select(None);
        }
        self.sync_container_history();
    }

    /// Get the latest metrics for a container in the open popup.
    /// `None` means that Metrics Server data is not available.
    pub fn selected_pod_container_metrics(&self, container: &str) -> Option<(i64, i64)> {
        let (namespace, pod) = self.container_pod.as_ref()?;
        self.container_metrics
            .get(&format!("{namespace}/{pod}/{container}"))
            .copied()
    }
}
