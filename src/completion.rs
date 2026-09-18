use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use clap::CommandFactory;
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::env::{CompleteEnv, Shells};
use sofka::{k8s, plugin_catalog, plugin_install};

#[cfg(windows)]
mod windows;

const ENV: &str = "SOFKA_COMPLETE";
const WORKER: &str = "SOFKA_COMPLETE_WORKER";
const TIMEOUT: Duration = Duration::from_secs(2);

pub fn write_registration(shell: clap_complete::Shell, out: &mut dyn Write) -> std::io::Result<()> {
    Shells::builtins()
        .completer(&shell.to_string())
        .ok_or_else(|| std::io::Error::other("unsupported completion shell"))?
        .write_registration(ENV, "sofka", "sofka", "sofka", out)
}

pub fn try_complete() -> bool {
    let Some(shell) = std::env::var_os(ENV).filter(|s| !s.is_empty() && s != "0") else {
        return false;
    };
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if std::env::var_os(WORKER).is_some() {
        #[cfg(windows)]
        if !windows::wait_for_parent() {
            return true;
        }
        let offset = args
            .iter()
            .position(|a| a == "--")
            .map(|i| i + 1)
            .unwrap_or(args.len());
        let mut index = match shell.to_str() {
            Some("fish" | "powershell") => args.len().saturating_sub(offset + 1),
            _ => std::env::var("_CLAP_COMPLETE_INDEX")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default(),
        };
        if shell == "bash" {
            let mut words = args.split_off(offset);
            normalize_bash_words(&mut words, &mut index);
            args.extend(words);
            // SAFETY: the completion worker has not started any threads.
            unsafe {
                std::env::set_var("_CLAP_COMPLETE_INDEX", index.to_string());
            }
        }
        let words = &args[offset..];
        let selection = Selection::from_words(&words[..index.min(words.len())]);
        let _ = CompleteEnv::with_factory(|| command(selection.clone()))
            .var(ENV)
            .try_complete(args.clone(), std::env::current_dir().ok().as_deref());
        return true;
    }
    // Isolate synchronous credential helpers and discard their diagnostics.
    // The parent bounds the whole request, including client construction.
    let result = (|| {
        let exe = std::env::current_exe().ok()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            let mut child = tokio::process::Command::new(exe);
            #[cfg(unix)]
            child.process_group(0);
            child
                .args(&args[1..])
                .env(WORKER, "1")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            child.stdout(Stdio::piped());
            #[cfg(windows)]
            child.stdin(Stdio::piped());
            let child = child.spawn().ok()?;
            #[cfg(windows)]
            let (_job, child) = windows::start_worker(child).await.ok()?;
            #[cfg(unix)]
            let pid = child.id();
            let result = tokio::time::timeout(TIMEOUT, child.wait_with_output()).await;
            #[cfg(unix)]
            if result.is_err()
                && let Some(pid) = pid
            {
                // SAFETY: the child starts a new process group with this PID.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            result.ok()?.ok()
        })
    })();
    if let Some(output) = result.filter(|o| o.status.success()) {
        let _ = std::io::stdout().lock().write_all(&output.stdout);
    }
    true
}

fn normalize_bash_words(words: &mut Vec<OsString>, index: &mut usize) {
    // Bash can split --option=value at '=' in COMP_WORDS.
    let command = super::Args::command();
    let mut i = 1;
    while i < words.len() && i <= *index {
        let previous = words[i - 1].to_string_lossy();
        let takes_value = previous.strip_prefix("--").is_some_and(|name| {
            command
                .get_arguments()
                .any(|a| a.get_long() == Some(name) && a.get_action().takes_values())
        });
        if words[i] == "=" && takes_value {
            if i == *index {
                words[i] = OsString::new();
            } else {
                words.remove(i);
                *index -= 1;
            }
        }
        i += 1;
    }
}

#[derive(Clone, Default, Debug, PartialEq)]
struct Selection {
    kubeconfig: Option<PathBuf>,
    context: Option<String>,
    catalog: Option<String>,
    allow_v1_client_cert: bool,
    no_tls_resumption: bool,
}

impl Selection {
    fn from_words(words: &[OsString]) -> Self {
        let mut selection = Self::default();
        let mut command = super::Args::command();
        command.build();
        let mut words = words.iter().skip(1);
        while let Some(word) = words.next() {
            if word == "--" {
                break;
            }
            let text = word.to_string_lossy();
            if let Some(long) = text.strip_prefix("--") {
                let (name, inline) = long
                    .split_once('=')
                    .map_or((long, None), |(n, v)| (n, Some(v)));
                let Some(arg) = command.get_arguments().find(|a| a.get_long() == Some(name)) else {
                    continue;
                };
                let value = if arg.get_action().takes_values() {
                    inline.map(OsString::from).or_else(|| words.next().cloned())
                } else {
                    None
                };
                match name {
                    "kubeconfig" => selection.kubeconfig = value.map(PathBuf::from),
                    "context" => selection.context = value.and_then(|v| v.into_string().ok()),
                    "allow-v1-client-cert" => selection.allow_v1_client_cert = true,
                    "no-tls-resumption" => selection.no_tls_resumption = true,
                    "catalog" => selection.catalog = value.and_then(|v| v.into_string().ok()),
                    _ => (),
                }
            } else if let Some(shorts) = text.strip_prefix('-') {
                for (i, short) in shorts.char_indices() {
                    if command
                        .get_arguments()
                        .any(|a| a.get_short() == Some(short) && a.get_action().takes_values())
                    {
                        if i + short.len_utf8() == shorts.len() {
                            words.next();
                        }
                        break;
                    }
                }
            } else {
                let subcommand = command
                    .get_subcommands()
                    .find(|c| c.get_name() == text)
                    .cloned();
                if let Some(subcommand) = subcommand {
                    command = subcommand;
                }
            }
        }
        selection
    }

    fn kubeconfig(&self) -> Option<kube::config::Kubeconfig> {
        match &self.kubeconfig {
            Some(path) => kube::config::Kubeconfig::read_from(path),
            None => kube::config::Kubeconfig::read(),
        }
        .ok()
    }

    fn cluster_values(&self, namespaces: bool) -> Vec<String> {
        let Some(config) = self.kubeconfig() else {
            return vec![];
        };
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return vec![];
        };
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(1200),
                k8s::completion::values(
                    config,
                    self.context.as_deref(),
                    namespaces,
                    self.allow_v1_client_cert,
                    self.no_tls_resumption,
                ),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
        })
    }
}

fn candidates(values: Vec<String>, current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let mut values: Vec<_> = values
        .into_iter()
        .filter(|value| value.starts_with(prefix.as_ref()) && !value.chars().any(char::is_control))
        .collect();
    values.sort();
    values.dedup();
    values.into_iter().map(CompletionCandidate::new).collect()
}

fn command(selection: Selection) -> clap::Command {
    let catalog = selection.catalog.clone();
    let contexts = selection.clone();
    let namespaces = selection.clone();
    let resources = move |current: &OsStr| {
        if current.to_string_lossy().starts_with('-') {
            return Vec::new();
        }
        let mut values = vec!["ctx".into(), "contexts".into()];
        values.extend(
            k8s::ALIASES
                .iter()
                .flat_map(|(alias, target)| [alias.to_string(), target.to_string()]),
        );
        let config = selection.kubeconfig();
        let context = selection
            .context
            .as_deref()
            .or_else(|| config.as_ref().and_then(|c| c.current_context.as_deref()))
            .unwrap_or_default();
        let cluster = config
            .as_ref()
            .and_then(|c| c.contexts.iter().find(|c| c.name == context))
            .and_then(|c| c.context.as_ref())
            .map(|c| c.cluster.as_str())
            .unwrap_or_default();
        let (loader, _) = sofka::config::ConfigLoader::load();
        values.extend(
            loader
                .resolve_read_only(context, cluster)
                .config
                .aliases
                .into_keys(),
        );
        values.extend(selection.cluster_values(false));
        candidates(values, current)
    };
    super::Args::command()
        .mut_arg("context", |a| {
            a.add(ArgValueCompleter::new(move |current: &OsStr| {
                candidates(
                    contexts
                        .kubeconfig()
                        .map(|c| c.contexts.into_iter().map(|c| c.name).collect())
                        .unwrap_or_default(),
                    current,
                )
            }))
        })
        .mut_arg("namespace", |a| {
            a.add(ArgValueCompleter::new(move |current: &OsStr| {
                candidates(namespaces.cluster_values(true), current)
            }))
        })
        .mut_arg("resource", |a| {
            a.add(ArgValueCompleter::new(resources.clone()))
        })
        .mut_arg("explicit_resource", |a| {
            a.add(ArgValueCompleter::new(resources))
        })
        .mut_subcommand("plugin", |mut cmd| {
            cmd = cmd.mut_subcommand("search", |sub| {
                sub.mut_arg("query", |arg| arg.value_hint(clap::ValueHint::Other))
            });
            for name in ["describe", "install", "update", "remove"] {
                let catalog = catalog.clone();
                cmd = cmd.mut_subcommand(name, |sub| {
                    sub.mut_arg(
                        if name == "describe" {
                            "plugin"
                        } else {
                            "plugins"
                        },
                        |arg| {
                            arg.add(ArgValueCompleter::new(move |current: &OsStr| {
                                candidates(
                                    plugin_values(name, current, catalog.as_deref()),
                                    current,
                                )
                            }))
                        },
                    )
                });
            }
            cmd
        })
}

fn plugin_values(command: &str, current: &OsStr, catalog: Option<&str>) -> Vec<String> {
    if matches!(command, "update" | "remove") {
        return plugin_install::managed_ids().unwrap_or_default();
    }
    let mut values = Vec::new();
    if let Some(snapshot) = plugin_catalog::cached_selected(catalog) {
        for plugin in &snapshot.catalog.plugins {
            if current.to_string_lossy().contains('@') {
                values.extend(
                    plugin
                        .versions
                        .iter()
                        .map(|v| format!("{}@{}", plugin.id, v.version))
                        .filter(|request| {
                            command != "install" || snapshot.catalog.select(request).is_ok()
                        }),
                );
            } else if command != "install" || snapshot.catalog.select(&plugin.id).is_ok() {
                values.push(plugin.id.clone());
            }
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_consumes_option_values_and_stops_at_subcommands() {
        for (words, context, path) in [
            (
                vec![
                    "sofka",
                    "--context",
                    "prod",
                    "--kubeconfig",
                    "space dir/config",
                    "-n",
                    "dev",
                ],
                Some("prod"),
                Some("space dir/config"),
            ),
            (
                vec!["sofka", "--context=prod", "--kubeconfig=file", "-A"],
                Some("prod"),
                Some("file"),
            ),
            (
                vec![
                    "sofka",
                    "--validate-plugin",
                    "--context=wrong",
                    "-n",
                    "--context=wrong",
                ],
                None,
                None,
            ),
            (
                vec!["sofka", "-Anprod", "--context=prod"],
                Some("prod"),
                None,
            ),
            (
                vec![
                    "sofka",
                    "--context",
                    "prod",
                    "plugin",
                    "search",
                    "--context=wrong",
                ],
                Some("prod"),
                None,
            ),
            (vec!["sofka", "--", "--context=wrong"], None, None),
        ] {
            let words = words.into_iter().map(OsString::from).collect::<Vec<_>>();
            let actual = Selection::from_words(&words);
            assert_eq!(actual.context.as_deref(), context, "{words:?}");
            assert_eq!(actual.kubeconfig, path.map(PathBuf::from), "{words:?}");
        }
    }

    #[test]
    fn candidate_values_are_filtered_sorted_and_not_shell_code() {
        let actual = candidates(
            vec![
                "prod two".into(),
                "prod\nbad".into(),
                "prod one".into(),
                "dev".into(),
                "prod one".into(),
            ],
            OsStr::new("prod"),
        );
        let values: Vec<_> = actual
            .iter()
            .map(|c| c.get_value().to_str().unwrap())
            .collect();
        assert_eq!(values, ["prod one", "prod two"]);
    }
}
