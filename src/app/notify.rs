use super::*;

impl App {
    /// `:notify` — toggle a watch-notification on the selected object. While
    /// active, every state change the watch sees for it (the same transitions
    /// the timeline records: rollout progress, readiness, phase, restarts,
    /// waiting reasons, conditions) flashes, rings the terminal bell, and
    /// emits a desktop notification (`[notify]`). Each notify is its own bounded
    /// single-object watch, so it keeps firing no matter which view is open,
    /// until toggled off or the session ends. Nothing touches disk.
    pub(super) fn toggle_notify(&mut self) {
        if matches!(self.kind_plural.as_str(), "helm" | "helmhistory") {
            self.flash_warn("notify is not available for Helm views");
            return;
        }
        let Some(kind) = self.kind.clone() else {
            self.flash_warn("select a resource first");
            return;
        };
        let Some((key, name, ns)) = self.selected_ref().map(|obj| {
            (
                row_key(obj),
                obj.metadata.name.clone().unwrap_or_default(),
                obj.metadata.namespace.clone().unwrap_or_default(),
            )
        }) else {
            self.flash_warn("no selection to notify on");
            return;
        };
        let id = format!("{}/{key}", self.kind_plural);
        if let Some(handle) = self.notify_tasks.remove(&id) {
            handle.abort();
            self.flash = format!(
                "notify off: {key} ({} still active)",
                self.notify_tasks.len()
            );
            self.flash_err = false;
            return;
        }
        if name.is_empty() {
            self.flash_warn("object has no name to watch");
            return;
        }
        // Each notify is its own watch, held for the session; without a budget
        // a long session can accumulate an unbounded number of them. Toggling
        // one off is always allowed — the check is below that branch.
        let budget = self.notify_cfg.max_watches;
        if budget > 0 && self.notify_tasks.len() >= budget {
            self.flash_warn(&format!(
                "notify budget reached ({budget} watches) — toggle one off, \
                 or raise [notify] max_watches"
            ));
            return;
        }
        let label = format!("{}/{name}", trim_s(&self.kind_plural));
        let plural = self.kind_plural.clone();
        let client = self.cluster.client.clone();
        let ar = kind.ar.clone();
        let namespaced = kind.namespaced;
        let tx = self.tx.clone();

        let handle = tokio::spawn(async move {
            let api: Api<DynamicObject> = if namespaced && !ns.is_empty() {
                Api::namespaced_with(client, &ns, &ar)
            } else {
                Api::all_with(client, &ar)
            };
            let cfg = watcher::Config::default()
                .any_semantic()
                .fields(&format!("metadata.name={name}"));
            let mut stream = watcher(api, cfg).boxed();
            let mut prev: Option<DynamicObject> = None;
            // The initial list describes the state the user just looked at —
            // only changes after that are news.
            let mut synced = false;
            let mut backoff = watcher::DefaultBackoff::default();
            while let Some(event) = stream.next().await {
                // Progress, as opposed to another doomed list attempt: `Init`
                // and its `InitApply`s replay on every attempt, so resetting on
                // those would never let the delay escalate.
                if matches!(
                    event,
                    Ok(watcher::Event::Apply(_)
                        | watcher::Event::Delete(_)
                        | watcher::Event::InitDone)
                ) {
                    backoff.reset();
                }
                match event {
                    Ok(watcher::Event::Apply(o)) | Ok(watcher::Event::InitApply(o)) => {
                        // One watch event → ONE notification. A single change
                        // often carries several transitions (phase + Ready +
                        // restarts); separate notifications land microseconds
                        // apart, and notification sinks rate-limit such bursts
                        // (herdr drops all but the first), so joining is what
                        // keeps them deliverable — and more readable.
                        let items: Vec<String> = match &prev {
                            Some(p) => crate::timeline::transitions(p, &o, &plural)
                                .into_iter()
                                .map(|(_, text)| text)
                                .collect(),
                            None if synced => vec!["created".into()],
                            None => Vec::new(),
                        };
                        if !items.is_empty() {
                            let text = format!("{label}: {}", items.join(" · "));
                            if tx.send(Msg::Notify(text)).await.is_err() {
                                return;
                            }
                        }
                        prev = Some(o);
                    }
                    Ok(watcher::Event::Delete(_)) => {
                        prev = None;
                        if tx
                            .send(Msg::Notify(format!("{label}: deleted")))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(watcher::Event::Init) => {}
                    Ok(watcher::Event::InitDone) => synced = true,
                    // The watcher self-heals; a transient error is not a
                    // state change worth ringing a bell for. It does need
                    // pacing though: `watcher` re-lists as fast as the stream
                    // is polled, so an error that never clears would hammer
                    // the API server for the rest of the session.
                    Err(_) => {
                        tokio::time::sleep(
                            backoff.next().unwrap_or(crate::k8s::WATCH_BACKOFF_CEILING),
                        )
                        .await;
                    }
                }
            }
        });
        self.notify_tasks.insert(id, handle);
        self.flash = format!(
            "notify on: {key} — changes flash + bell ({} active)",
            self.notify_tasks.len()
        );
        self.flash_err = false;
    }

    /// The message the main loop should deliver (bell, escape sequence,
    /// notifier subprocess), if any arrived since the last frame. Multiple
    /// pending notifications join into one message — one delivery per frame,
    /// bounded, so bursts survive sink rate limiting.
    ///
    /// Anything the queue had to drop is reported as a count rather than
    /// silently lost.
    pub fn take_notification(&mut self) -> Option<String> {
        if self.pending_notify.is_empty() {
            return None;
        }
        let text = self.pending_notify.join(" · ");
        self.pending_notify.clear();
        let dropped = std::mem::take(&mut self.dropped_notify);
        Some(notification_summary(&text, dropped))
    }

    /// Deliver one `:notify` event through the notifier subprocess, when one
    /// applies (`[notify] command`, or the herdr auto-detection) — the route
    /// that survives terminal multiplexers, which swallow pane escape
    /// sequences. Fire-and-forget with nulled stdio; a notifier that can't
    /// even start is worth one warning, not a broken TUI.
    pub fn run_notify_command(&mut self, text: &str) {
        if notification_command(&self.notify_cfg, in_herdr_pane(), text).is_none() {
            return;
        }
        if self.pending_notifier.is_some() {
            self.merged_notifier = self.merged_notifier.saturating_add(1);
        } else {
            self.pending_notifier = Some(text.to_string());
        }
        self.retry_notify_command();
    }

    /// Reap notifier subprocesses and deliver the retained summary as soon as
    /// one slot is free. Called on quiet frames too, so a saturated burst does
    /// not need another state change to make progress.
    pub fn retry_notify_command(&mut self) {
        self.notifier_procs
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
        if self.notifier_procs.len() >= MAX_NOTIFIER_PROCS {
            return;
        }
        let Some(pending) = self.pending_notifier.as_deref() else {
            return;
        };
        let text = notification_summary(pending, self.merged_notifier);
        let Some(argv) = notification_command(&self.notify_cfg, in_herdr_pane(), &text) else {
            self.pending_notifier = None;
            self.merged_notifier = 0;
            return;
        };
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                self.notifier_procs.push(child);
                self.pending_notifier = None;
                self.merged_notifier = 0;
            }
            Err(e) => {
                self.pending_notifier = None;
                self.merged_notifier = 0;
                self.flash_warn(&format!("notify command '{}' failed: {e}", argv[0]));
            }
        }
    }
}

/// Ellipsize the detail while reserving room for the overflow count. Appending
/// the suffix after a full 300-character message made the count itself the
/// first thing truncation removed.
pub(super) fn notification_summary(text: &str, merged: usize) -> String {
    const LIMIT: usize = 300;
    if merged == 0 {
        return crate::text::ellipsize(text, LIMIT);
    }
    let suffix = format!(" · (+{merged} more)");
    let detail_limit = LIMIT.saturating_sub(suffix.chars().count());
    format!("{}{}", crate::text::ellipsize(text, detail_limit), suffix)
}

/// Whether this process runs inside a herdr pane (herdr exports its socket
/// path into every pane's environment). Always false under test, so a suite
/// run inside a herdr pane doesn't pop real toasts.
fn in_herdr_pane() -> bool {
    !cfg!(test) && std::env::var_os("HERDR_SOCKET_PATH").is_some()
}

/// The notifier argv for one notification, if any applies. An explicit
/// `[notify] command` wins: `$MESSAGE` substitutes as whole arguments (never
/// spliced into a shell string), and with no placeholder the message is
/// appended. With no command configured, a herdr pane auto-routes through
/// `herdr notification show`, which delivers via herdr's own `ui.toast`
/// setting — pane escape sequences would be swallowed there anyway.
pub fn notification_command(
    cfg: &crate::config::NotifyConfig,
    in_herdr: bool,
    text: &str,
) -> Option<Vec<String>> {
    let explicit = cfg
        .command
        .first()
        .is_some_and(|exe| !exe.trim().is_empty());
    if explicit {
        let mut argv: Vec<String> = cfg.command.clone();
        let mut substituted = false;
        for arg in argv.iter_mut().skip(1) {
            if arg.contains("$MESSAGE") {
                *arg = arg.replace("$MESSAGE", text);
                substituted = true;
            }
        }
        if !substituted {
            argv.push(text.to_string());
        }
        return Some(argv);
    }
    if in_herdr {
        return Some(vec![
            "herdr".into(),
            "notification".into(),
            "show".into(),
            "sofka".into(),
            "--body".into(),
            text.to_string(),
        ]);
    }
    None
}

/// The terminal escape sequences that deliver one `:notify` event, per the
/// `[notify]` config: an optional BEL, then the chosen desktop-notification
/// protocol(s). Control characters are stripped so object content can't
/// smuggle sequences; BEL-terminated OSC matches every terminal's documented
/// examples.
///
/// - `osc9` — iTerm2-style, body only: iTerm2, Ghostty, kitty, WezTerm,
///   foot, Windows Terminal.
/// - `osc777` — rxvt-style `notify;title;body`: Ghostty (which recommends
///   it), kitty, WezTerm, foot, urxvt.
///
/// An unknown `desktop` value behaves as the default `osc777` (it warned at
/// config load).
pub fn notification_sequence(text: &str, cfg: &crate::config::NotifyConfig) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut seq = String::new();
    if cfg.bell {
        seq.push('\x07');
    }
    let desktop = match cfg.desktop.as_str() {
        d @ ("osc9" | "osc777" | "both" | "off") => d,
        _ => "osc777",
    };
    if matches!(desktop, "osc9" | "both") {
        seq.push_str(&format!("\x1b]9;sofka: {clean}\x07"));
    }
    if matches!(desktop, "osc777" | "both") {
        seq.push_str(&format!("\x1b]777;notify;sofka;{clean}\x07"));
    }
    seq
}
