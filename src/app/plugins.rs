use super::*;

impl App {
    /// Run a config-defined plugin bound to `c` if it applies to the current
    /// kind. Blocked in read-only mode: plugins shell out to arbitrary
    /// commands, so we can't know they won't mutate the cluster. Returns
    /// whether a plugin matched (so the caller can stop treating the event as
    /// an unhandled key).
    pub(super) fn try_plugin_key(&mut self, key: KeyEvent) -> bool {
        let Some(plugin) = self
            .plugins
            .iter()
            .find(|p| {
                crate::keys::KeyChord::parse(&p.key).is_ok_and(|chord| chord.matches(&key))
                    && (p.scopes.is_empty() || p.scopes.iter().any(|s| s == &self.kind_plural))
            })
            .cloned()
        else {
            return false;
        };
        self.run_plugin(plugin, "");
        true
    }

    /// Resolve placeholders, then run the plugin — after a confirmation prompt
    /// when it's marked `confirm`/`dangerous`.
    pub(super) fn run_plugin(&mut self, plugin: crate::config::Plugin, arguments: &str) {
        if let Err(e) = crate::plugins::available(&plugin) {
            self.flash_warn(&e);
            return;
        }
        let inputs = match crate::plugins::inputs(&plugin, arguments) {
            Ok(inputs) => inputs,
            Err(e) => {
                self.flash_warn(&e);
                return;
            }
        };
        // A mutating plugin (the default) is blocked in read-only mode; one
        // explicitly declared read-only stays available.
        if (plugin.mutating.unwrap_or(true) || plugin.network_load) && self.readonly {
            let reason = if plugin.network_load {
                "generates network load"
            } else {
                "may mutate (set mutating = false to allow)"
            };
            self.flash_warn(&format!(
                "read-only mode — plugin '{}' {reason}",
                plugin.name
            ));
            return;
        }
        let mode = match plugin.output.as_deref() {
            Some("popup") => PluginMode::Popup,
            Some("report") => PluginMode::Report,
            Some("background") => PluginMode::Background,
            _ => PluginMode::Terminal,
        };
        let timeout = plugin
            .timeout
            .as_deref()
            .and_then(|t| crate::providers::parse_lookback(t).ok())
            .unwrap_or(30)
            .max(1) as u64;

        // Marked rows drive a bulk run; otherwise the single selection.
        let targets = if plugin.target.as_deref() == Some("context") {
            vec![(String::new(), self.namespace.clone())]
        } else {
            self.action_targets()
        };
        if targets.is_empty() {
            self.flash_warn("no selection for plugin");
            return;
        }
        // An interactive terminal command can't compose over a marked set:
        // refuse rather than surprise the user by running on just one row.
        if mode == PluginMode::Terminal && targets.len() > 1 {
            self.flash_warn(&format!(
                "'{}': a marked set needs output = popup or background",
                plugin.name
            ));
            return;
        }

        let ctx = self.cluster.context.clone();
        let cluster = self.cluster.cluster_name.clone();
        let res = self.kind_plural.clone();
        let filter = self.filter.clone();
        let (group, version, kind) = self
            .kind
            .as_ref()
            .map(|k| (k.ar.group.clone(), k.ar.version.clone(), k.ar.kind.clone()))
            .unwrap_or_default();

        // One (label, argv) job per target. Placeholders are substituted as
        // whole arguments — never spliced into a shell string. `$NAMESPACE`
        // before `$NS`/`$NAME` so the longer token wins.
        let remote = if let Some(port) = &plugin.port_forward {
            match crate::plugins::input_arg(port, &inputs).parse::<u16>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    self.flash_warn("port_forward must resolve to a port from 1 to 65535");
                    return;
                }
            }
        } else {
            None
        };
        if remote.is_some() && !matches!(res.as_str(), "pods" | "services") {
            self.flash_warn("plugin port-forward requires a pod or service");
            return;
        }
        let jobs: Vec<crate::plugins::Job> = targets
            .iter()
            .map(|(name, ns)| {
                let subst = |s: &str| {
                    // Inputs are literal argv values, never re-expanded as context placeholders.
                    if s.starts_with("${input.") {
                        return crate::plugins::input_arg(s, &inputs);
                    }
                    s.replace("$NAMESPACE", ns)
                        .replace("$NS", ns)
                        .replace("$NAME", name)
                        .replace("$CONTEXT", &ctx)
                        .replace("$CLUSTER", &cluster)
                        .replace("$RESOURCE", &res)
                        .replace("$GROUP", &group)
                        .replace("$VERSION", &version)
                        .replace("$KIND", &kind)
                        .replace("$FILTER", &filter)
                };
                let mut argv = if plugin.shell {
                    vec![
                        "sh".into(),
                        "-c".into(),
                        subst(&plugin.command),
                        "sofka".into(),
                    ]
                } else {
                    vec![if plugin.package_dir.is_some() || plugin.bundled {
                        plugin.command.clone()
                    } else {
                        subst(&plugin.command)
                    }]
                };
                argv.extend(plugin.args.iter().map(|a| subst(a)));
                if plugin.bundled && self.cluster.allow_v1_client_cert {
                    argv.push("--allow-v1-client-cert".into());
                }
                let object = if (plugin.package_dir.is_some() || plugin.bundled)
                    && plugin.target.as_deref() != Some("context")
                {
                    let key = if ns.is_empty() {
                        name.clone()
                    } else {
                        format!("{ns}/{name}")
                    };
                    self.store.shared(&key)
                } else {
                    None
                };
                let speaks_protocol = plugin.package_dir.is_some() || plugin.bundled;
                let request = speaks_protocol.then(|| {
                    let mut request = serde_json::json!({
                        "schema_version": 1, "context": self.cluster.kubectl_context(),
                        "kubeconfig": self.cluster.kubectl_kubeconfig()
                            .map(|p| p.to_string_lossy().into_owned()),
                        "cluster": cluster, "namespace": ns, "resource": res,
                        "name": name, "filter": filter, "inputs": inputs, "forward": null
                    });
                    if object.is_none() {
                        request["object"] = Value::Null;
                    }
                    request
                });
                let forward = remote.map(|remote| {
                    let target = format!("{}/{name}", if res == "pods" { "pod" } else { "svc" });
                    let local = self.port_forwards.iter().find_map(|pf| {
                        let same = pf.target == target || (res == "pods" && pf.target == *name);
                        if pf.ns != *ns || !same {
                            return None;
                        }
                        let (l, r) = pf.ports.split_once(':').unwrap_or((&pf.ports, &pf.ports));
                        (r.parse::<u16>().ok() == Some(remote))
                            .then(|| l.parse::<u16>().ok())
                            .flatten()
                            .filter(|p| *p > 0)
                    });
                    let mut argv = self.kubectl_base();
                    argv.extend(["port-forward".into(), "-n".into(), ns.clone(), target]);
                    crate::plugins::Forward {
                        argv,
                        remote,
                        local,
                    }
                });
                crate::plugins::Job {
                    label: name.clone(),
                    argv,
                    directory: plugin.package_dir.clone(),
                    request,
                    object,
                    forward,
                }
            })
            .collect();

        let base = if plugin.confirm || plugin.dangerous || plugin.network_load {
            guardrails::ConfirmLevel::Plain
        } else {
            guardrails::ConfirmLevel::None
        };
        let action_name = format!(
            "plugin:{}",
            plugin.palette.as_deref().unwrap_or(&plugin.name)
        );
        let all_namespaces =
            plugin.target.as_deref() == Some("context") && self.namespace.is_empty();
        let Some(level) = self.guard_scope(&action_name, &res, &targets, base, all_namespaces)
        else {
            return;
        };
        let cmd = truncate_cmd(&jobs[0].argv);
        let input_preview = inputs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        let scope = format!(
            "context={ctx} namespace={}",
            if self.namespace.is_empty() {
                "all"
            } else {
                &self.namespace
            }
        );
        let forward_preview = remote
            .map(|port| format!(" port-forward={res}/{}:{port}", targets[0].0))
            .unwrap_or_default();
        let cmd = format!("{scope}{forward_preview} {cmd} {input_preview}");
        let head = format!("Run plugin '{}' on {} target(s)?", plugin.name, jobs.len());
        let label = if plugin.dangerous || plugin.network_load {
            format!("⚠ {head} (dangerous)  {cmd}")
        } else {
            format!("{head}  {cmd}")
        };
        let hint = if plugin.target.as_deref() == Some("context") {
            ctx.clone()
        } else if targets.len() == 1 {
            targets[0].0.clone()
        } else {
            targets.len().to_string()
        };
        self.stop_plugins();
        self.begin_guarded(
            ConfirmAction::Plugin {
                jobs,
                name: plugin.name,
                mode,
                timeout,
            },
            label,
            level,
            hint,
        );
    }

    /// Dispatch resolved plugin jobs (one per target) by output mode.
    pub(super) fn launch_plugin(
        &mut self,
        jobs: Vec<crate::plugins::Job>,
        name: String,
        mode: PluginMode,
        timeout: u64,
    ) {
        if jobs.is_empty() {
            return;
        }
        let n = jobs.len();
        self.note_action(
            format!("plugin: {name}"),
            if n == 1 {
                "1 target".to_string()
            } else {
                format!("{n} targets")
            },
        );
        match mode {
            PluginMode::Terminal => {
                // Terminal runs are single (enforced in run_plugin).
                let argv = jobs
                    .into_iter()
                    .next()
                    .map(|job| job.argv)
                    .unwrap_or_default();
                if argv.is_empty() {
                    return;
                }
                self.flash = format!("plugin: {name}");
                self.flash_err = false;
                self.pending = Some(Suspend::Shell(argv));
            }
            PluginMode::Popup | PluginMode::Report => {
                // Mirror describe: stay put, swap to the doc view when output
                // lands, so a view switch mid-run cleanly drops the result.
                self.set_return_mode();
                let claim = self.claim_status(plugin_flash(&name, n, ""));
                self.spawn_plugin(jobs, format!("{name} — output"), mode, timeout, claim);
            }
            PluginMode::Background => {
                let claim = self.claim_status(plugin_flash(&name, n, " (background)"));
                self.spawn_plugin(jobs, name, mode, timeout, claim);
            }
        }
    }

    /// Run every job off-thread with a per-job timeout, bounded output capture,
    /// and bounded concurrency, then report back via [`Msg`]. A hung command
    /// can't freeze the UI — the timeout aborts it — and the aggregated result
    /// is generation-gated like every other stream. For a bulk run this is
    /// where partial failures are counted.
    fn spawn_plugin(
        &mut self,
        jobs: Vec<crate::plugins::Job>,
        title: String,
        mode: PluginMode,
        timeout: u64,
        claim: StatusClaim,
    ) {
        let tx = self.tx.clone();
        let genr = self.generation;
        self.stop_plugins();
        let run = self.plugin_run;
        self.plugin_claim = Some(claim);
        self.plugin_task = Some(crate::plugins::Task(tokio::spawn(async move {
            let dur = Duration::from_secs(timeout);
            // Bounded concurrency, results in the marked order.
            let total = jobs.len();
            let mut results = futures_util::stream::iter(jobs.into_iter().map(|job| async move {
                let label = job.label.clone();
                let out = tokio::time::timeout(dur, crate::plugins::execute(job)).await;
                (label, out)
            }))
            .buffered(8);
            let mut lines = crate::plugins::Lines::default();
            let mut failures = crate::plugins::Lines::default();
            let mut failed = 0;
            while let Some((label, outcome)) = results.next().await {
                let (ok, block, reason) =
                    reduce_plugin_outcome(timeout, outcome, mode == PluginMode::Report);
                if !ok {
                    failed += 1;
                    failures.push(format!("{label}: {reason}"));
                }
                if total > 1 {
                    lines.push(format!("== {label} =="));
                }
                lines.extend(block);
            }
            let msg = match mode {
                PluginMode::Popup | PluginMode::Report => Msg::PluginOutput {
                    run,
                    generation: genr,
                    claim,
                    title,
                    lines: lines.finish(),
                    warn: (failed > 0).then(|| format!("{failed} of {total} failed")),
                },
                _ => Msg::PluginBulkDone {
                    run,
                    generation: genr,
                    claim,
                    name: title,
                    ok: total - failed,
                    failed: failures.finish(),
                },
            };
            let _ = tx.send(msg).await;
        })));
    }

    pub(super) fn stop_plugins(&mut self) {
        self.plugin_task = None;
        if let Some(claim) = self.plugin_claim.take() {
            self.clear_claimed_status(claim);
        }
        self.plugin_run = self.plugin_run.wrapping_add(1);
    }
}

/// Bound the number of captured output lines and total bytes from a
/// popup/background plugin, so a chatty command can't balloon memory or the
/// redraw. Mirrors the log view's tail-buffer discipline.
const PLUGIN_MAX_LINES: usize = 5_000;
const PLUGIN_MAX_BYTES: usize = 1 << 20; // 1 MiB

/// A short, single-line preview of an argv for the confirmation dialog.
fn truncate_cmd(argv: &[String]) -> String {
    let joined = argv.join(" ");
    if joined.chars().count() > 120 {
        let mut s: String = joined.chars().take(119).collect();
        s.push('…');
        s
    } else {
        joined
    }
}

type SpawnOutcome = Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed>;

/// Split captured bytes into bounded display lines.
fn bounded_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(PLUGIN_MAX_BYTES)]);
    let mut lines: Vec<String> = text
        .lines()
        .take(PLUGIN_MAX_LINES)
        .map(str::to_string)
        .collect();
    if text.lines().count() > PLUGIN_MAX_LINES {
        lines.push(format!("… output truncated at {PLUGIN_MAX_LINES} lines"));
    }
    lines
}

/// First non-empty line of stderr, for a compact failure summary.
fn stderr_summary(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("failed")
        .chars()
        .take(160)
        .collect()
}

/// Reduce one job's outcome to `(ok, stdout lines, short failure reason)`.
fn reduce_outcome(timeout: u64, outcome: SpawnOutcome) -> (bool, Vec<String>, String) {
    match outcome {
        Err(_) => {
            let r = format!("timed out after {timeout}s");
            (false, vec![r.clone()], r)
        }
        Ok(Err(e)) => {
            let r = format!("failed to start: {e}");
            (false, vec![format!("failed to run: {e}")], r)
        }
        Ok(Ok(out)) => {
            let mut lines = bounded_lines(&out.stdout);
            if out.status.success() {
                if lines.is_empty() {
                    lines.push("(no output)".into());
                }
                (true, lines, String::new())
            } else {
                let err = stderr_summary(&out.stderr);
                let reason = if err.is_empty() {
                    format!("exited {}", exit_label(&out.status))
                } else {
                    lines.push(String::new());
                    lines.push(format!("[stderr] {err}"));
                    format!("exited {} — {err}", exit_label(&out.status))
                };
                (false, lines, reason)
            }
        }
    }
}

fn reduce_plugin_outcome(
    timeout: u64,
    outcome: SpawnOutcome,
    report: bool,
) -> (bool, Vec<String>, String) {
    if report {
        match outcome {
            Ok(Ok(out)) if out.status.success() => match crate::plugins::render_report(&out.stdout)
            {
                Ok(lines) => (true, lines, String::new()),
                Err(e) => (false, vec![e.clone()], e),
            },
            other => reduce_outcome(timeout, other),
        }
    } else {
        reduce_outcome(timeout, outcome)
    }
}

fn exit_label(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("code {c}"),
        None => "by signal".into(),
    }
}

/// Flash text for a launched popup/background run, noting the count on a bulk
/// run.
fn plugin_flash(name: &str, n: usize, suffix: &str) -> String {
    if n > 1 {
        format!("plugin: {name} ×{n}{suffix}…")
    } else {
        format!("plugin: {name}{suffix}…")
    }
}
