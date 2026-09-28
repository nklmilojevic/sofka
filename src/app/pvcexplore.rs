use super::*;

use std::path::PathBuf;

use crate::app::actions::SHELL_FALLBACK;
use crate::pvcexplore::{
    self as pvc, Entry, HELPER_ANNOTATION, HELPER_MOUNT, HELPER_PREFIX, Mount,
};

/// Ceiling on one `kubectl exec … ls`. A directory on an unresponsive NFS
/// backend can hang the exec indefinitely; without this the pane would sit on
/// "loading…" with nothing to cancel.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for a helper pod to reach `Running` before giving up. An
/// image pull on a cold node is the slow case.
const HELPER_READY_TIMEOUT: Duration = Duration::from_secs(90);
const HELPER_POLL: Duration = Duration::from_millis(500);

/// What the user asked for before sofka knew which pod could serve it.
/// Resolving a claim is asynchronous (and may need a helper pod), so the
/// intent has to survive the round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvcIntent {
    /// Open the split-pane browser.
    Browse,
    /// Suspend the TUI and drop into a shell at the mount point.
    Shell,
}

/// Which pane has the cursor. The local side is on the left and the volume on
/// the right, so "copy" always means "into the other one".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Local,
    Remote,
}

/// State for the PVC browser (`x` on a PVC row).
pub struct PvcExplore {
    /// The browser owns the screen. Also what tells the confirm/transfer
    /// overlays to draw over it rather than over the table.
    pub active: bool,
    pub claim: String,
    pub namespace: String,
    /// The pod and path the volume is reachable through. `None` until the
    /// resolve round trip lands.
    pub mount: Option<Mount>,
    /// What the user asked for, kept until the target resolves.
    pub intent: PvcIntent,
    /// Directory the remote pane is showing, and the mount point it may not
    /// walk above.
    pub remote_path: String,
    pub remote: Vec<Entry>,
    pub remote_state: ListState,
    /// Why the remote pane is empty, when it is empty for a reason.
    pub remote_error: Option<String>,
    /// A listing is in flight — the pane shows the last one until it lands.
    pub loading: bool,
    /// The local pane hit [`crate::pvcexplore::MAX_ENTRIES`] too.
    pub local_truncated: bool,
    /// The current listing hit [`crate::pvcexplore::MAX_ENTRIES`] and there is
    /// more behind it.
    /// A count would be a lie — the cap is applied inside the container, so
    /// nothing ever counted the rest.
    pub truncated: bool,
    /// Directory whose entries are actually on screen. A failed navigation
    /// returns the title to it — tracking "the path before the last request"
    /// would name a directory that was itself never successfully listed once
    /// two navigations are in flight.
    displayed_path: String,
    pub local_path: PathBuf,
    pub local: Vec<Entry>,
    pub local_state: ListState,
    pub local_error: Option<String>,
    pub focus: Pane,
    /// A shell was queued straight from a PVC row (not from inside the
    /// browser), so its state — including any helper pod — is torn down once
    /// the suspended shell returns, not before it has started.
    pub(super) shell_pending: bool,
    /// Entry to put the cursor on when the next listing lands — the directory
    /// just stepped out of, so `⌫` returns you to where you were rather than
    /// to the top of the parent.
    want_select: Option<String>,
    /// Bumped on every navigation so a slow listing for a directory the user
    /// has already left is discarded instead of replacing the current one.
    pub run: u64,
    pub(super) recovery: Option<RecoveryState>,
    listed: bool,
}

pub(super) struct RecoveryState {
    pub error: String,
    pub original: Mount,
    candidates: std::collections::VecDeque<Mount>,
    helper: Option<Result<pvc::HelperOptions, String>>,
}

impl Default for PvcExplore {
    fn default() -> Self {
        Self {
            active: false,
            claim: String::new(),
            namespace: String::new(),
            mount: None,
            intent: PvcIntent::Browse,
            remote_path: String::new(),
            remote: Vec::new(),
            remote_state: ListState::default(),
            remote_error: None,
            loading: false,
            truncated: false,
            local_truncated: false,
            displayed_path: String::new(),
            want_select: None,
            local_path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            local: Vec::new(),
            local_state: ListState::default(),
            local_error: None,
            focus: Pane::Remote,
            shell_pending: false,
            run: 0,
            recovery: None,
            listed: false,
        }
    }
}

impl PvcExplore {
    /// The mount point, which is also the floor for `..`.
    pub fn root(&self) -> &str {
        self.mount.as_ref().map(|m| m.path.as_str()).unwrap_or("/")
    }

    /// The directory whose entries are on screen — which is not
    /// [`Self::remote_path`] while a listing is in flight, because the pane
    /// deliberately keeps showing the old one until the new one arrives.
    /// Every action builds its path from here, so what you are looking at is
    /// what `c`, `s` and `⏎` act on even when a slow exec is still running.
    pub fn current_dir(&self) -> &str {
        if self.displayed_path.is_empty() {
            &self.remote_path
        } else {
            &self.displayed_path
        }
    }

    pub fn selected_remote(&self) -> Option<&Entry> {
        self.remote.get(self.remote_state.selected()?)
    }

    pub fn selected_local(&self) -> Option<&Entry> {
        self.local.get(self.local_state.selected()?)
    }

    /// Put the cursor back on `previous` if that entry is still there, else on
    /// the first row. A refresh — or the pane reload after a copy — otherwise
    /// resets both cursors, which in a directory of any size means re-finding
    /// your place after every single file.
    fn reselect(state: &mut ListState, entries: &[Entry], previous: Option<&str>) {
        // The current index first, then the name: with two rows of the same
        // name (which a forged `ls` row can produce) this keeps the cursor
        // where it is instead of jumping to the other one.
        let at = state.selected().filter(|i| {
            entries
                .get(*i)
                .is_some_and(|e| Some(e.name.as_str()) == previous)
        });
        let index = at
            .or_else(|| previous.and_then(|name| entries.iter().position(|e| e.name == name)))
            .or(if entries.is_empty() { None } else { Some(0) });
        state.select(index);
    }
}

impl App {
    /// `x` on a PVC row, or `:pvc-explore`: open the split-pane browser for
    /// the selected claim. Read-only until you ask for a transfer, so it is
    /// available in read-only mode.
    pub(super) fn open_pvc_explore(&mut self) {
        self.start_pvc(PvcIntent::Browse);
    }

    /// `s` on a PVC row: a shell at the mount point instead of the browser.
    /// The same target resolution, a different thing done with it.
    pub(super) fn request_pvc_shell(&mut self) {
        if self.deny_readonly() {
            return;
        }
        self.start_pvc(PvcIntent::Shell);
    }

    fn start_pvc(&mut self, intent: PvcIntent) {
        if self.kind_plural != "persistentvolumeclaims" {
            self.flash_warn("PVC explore only works on persistentvolumeclaims");
            return;
        }
        let Some(obj) = self.selected_ref() else {
            self.flash_warn("no PVC selected");
            return;
        };
        let claim = obj.metadata.name.clone().unwrap_or_default();
        let ns = obj.metadata.namespace.clone().unwrap_or_default();
        if ns.is_empty() {
            self.flash_warn("PVC has no namespace");
            return;
        }
        // An unbound claim has no volume behind it yet — there is nothing to
        // mount, and a helper pod would sit Pending until it timed out.
        let phase = obj
            .data
            .get("status")
            .and_then(|s| s.get("phase"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !phase.is_empty() && phase != "Bound" {
            self.flash_warn(&format!(
                "{claim} is {phase}, not Bound — nothing to browse"
            ));
            return;
        }
        // A block claim is a raw device, not a filesystem: no pod mounts it at
        // a path, and a helper pod asking for one would sit in
        // ContainerCreating until it timed out.
        if obj
            .data
            .get("spec")
            .and_then(|s| s.get("volumeMode"))
            .and_then(Value::as_str)
            == Some("Block")
        {
            self.flash_warn(&format!(
                "{claim} is a block volume — it has no filesystem to browse"
            ));
            return;
        }
        // Opening a second claim abandons the first. Tear it down here, or its
        // helper pod is orphaned for the rest of its TTL.
        self.leave_pvc_explore();
        self.pvc.claim = claim.clone();
        self.pvc.namespace = ns.clone();
        self.pvc.intent = intent;
        self.pvc.mount = None;
        self.spawn_pvc_resolve(ns, claim);
    }

    /// Find a running pod that already mounts the claim. Nothing is created
    /// here: a `None` result comes back to the UI, which then asks before
    /// creating a helper pod.
    fn spawn_pvc_resolve(&mut self, ns: String, claim: String) {
        self.pvc.run += 1;
        let run = self.pvc.run;
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        let status = self.claim_status(format!("finding a pod that mounts {claim}…"));
        let context = self.cluster.context.clone();
        tokio::spawn(async move {
            let pods: Api<DynamicObject> = Api::namespaced_with(client, &ns, &pod_resource());
            let result = match pods.list(&ListParams::default()).await {
                Ok(list) => Ok(pvc::find_mount(&list.items, &claim)),
                Err(e) => Err(format!("listing pods in {ns}: {e}")),
            };
            let _ = tx
                .send(Msg::PvcTarget {
                    generation: genr,
                    run,
                    namespace: ns,
                    context,
                    claim: status,
                    result,
                })
                .await;
        });
    }

    /// The resolve round trip landed: open what was asked for, or offer a
    /// helper pod when nothing mounts the claim.
    pub(super) fn handle_pvc_target(
        &mut self,
        run: u64,
        namespace: String,
        status: StatusClaim,
        result: Result<Option<Mount>, String>,
    ) {
        // Superseded — the user asked for another claim, or gave up, while a
        // helper pod was still coming up. Creating it already succeeded, so
        // dropping the message here would leak it for its whole TTL.
        if run != self.pvc.run {
            if let Ok(Some(mount)) = result {
                self.delete_helper(namespace, mount);
            }
            // The dropped operation still owns the status bar, and a message
            // ending in "…" is never expired by the tick.
            self.clear_claimed_status(status);
            return;
        }
        match result {
            Err(e) => {
                self.set_claimed_status(status, e.clone(), true);
                if self.pvc.recovery.is_some() {
                    self.pvc_recovery_error(&e);
                }
            }
            Ok(Some(mount)) => {
                self.clear_claimed_status(status);
                self.enter_pvc_target(namespace, mount);
            }
            Ok(None) => {
                self.clear_claimed_status(status);
                self.offer_pvc_helper();
            }
        }
    }

    fn enter_pvc_target(&mut self, namespace: String, mount: Mount) {
        let intent = self.pvc.intent;
        // The resolve is slow enough (a helper pod can take a minute to pull)
        // that the user may have moved on — to a document view, or to another
        // kind entirely. Opening the browser on top of that would be a view
        // they didn't ask for, so hand the pod back and say why.
        // An overlay over the table — the palette, help, a filter — is not
        // moving on: it closes back to the same view. A dialog is different:
        // opening the browser over it would discard a decision the user has
        // not made yet, so treat it as moved on and hand the pod back.
        let over_table = matches!(
            self.mode,
            Mode::Table | Mode::Command | Mode::Help | Mode::Filter
        );
        let moved_on =
            !self.pvc.active && (!over_table || self.kind_plural != "persistentvolumeclaims");
        if moved_on {
            self.delete_helper(namespace, mount);
            self.pvc.shell_pending = false;
            self.flash_warn(&format!(
                "{} is ready, but you've moved on — press x on it again",
                self.pvc.claim
            ));
            return;
        }
        let path = mount.path.clone();
        self.pvc.mount = Some(mount);
        match intent {
            PvcIntent::Shell => {
                // A shell needs no browser state — resolve, suspend, done. The
                // teardown waits for `after_suspend`: deleting a helper pod
                // here would pull the volume out from under a shell the run
                // loop has not started yet. A guardrail that refuses the
                // shell means nothing will suspend, so tear down immediately.
                self.pvc.shell_pending = true;
                if !self.pvc_shell_at(&path) {
                    self.after_suspend();
                }
            }
            PvcIntent::Browse => {
                if !self.pvc.active {
                    self.set_return_mode();
                }
                self.pvc.active = true;
                self.pvc.focus = Pane::Remote;
                // Seeded, not left empty: a first listing that fails restores
                // the title from here, and an empty path would blank it and
                // make `⌫` and `r` both misbehave.
                self.pvc.displayed_path = path.clone();
                self.pvc.remote.clear();
                self.pvc.remote_error = self.pvc.recovery.as_ref().map(|r| r.error.clone());
                self.pvc.truncated = false;
                self.mode = Mode::PvcExplore;
                self.reload_local();
                self.open_remote_dir(path, None);
            }
        }
    }

    /// Nothing mounts the claim, so browsing it means creating a pod that
    /// does. That writes to the cluster: refused in read-only mode, gated by
    /// the `pvc-explore` guardrail, and always confirmed — the dialog names
    /// the image and the claim.
    fn offer_pvc_helper(&mut self) {
        if self.readonly {
            self.flash_warn(&format!(
                "browsing {} needs a helper pod, which read-only mode blocks",
                self.pvc.claim
            ));
            return;
        }
        let ns = self.pvc.namespace.clone();
        let claim = self.pvc.claim.clone();
        // A shell that a `shell` guardrail will refuse would otherwise create
        // a pod, wait for it to pull, and delete it again — all to reach a
        // refusal. The check is namespace-wide because the pod that would
        // serve the shell does not exist yet.
        if self.pvc.intent == PvcIntent::Shell
            && self.guard_denies("shell", "pods", &[(String::from("*"), ns.clone())])
        {
            self.flash_warn(&format!(
                "blocked by guardrail: shell pods — not creating a helper pod for {claim}"
            ));
            return;
        }
        let targets = [(claim.clone(), ns.clone())];
        let Some(level) = self.guard(
            "pvc-explore",
            "persistentvolumeclaims",
            &targets,
            ConfirmLevel::Plain,
        ) else {
            return;
        };
        let image = self.pvc_cfg.image.clone();
        let label = if let Some(recovery) = &self.pvc.recovery {
            format!(
                "{}\n\nBrowse with helper pod? Create a temporary {image} pod in {ns} for {claim}.",
                recovery.error
            )
        } else {
            format!("Nothing mounts {claim}. Create a temporary {image} pod in {ns} to mount it?")
        };
        self.begin_guarded(
            ConfirmAction::PvcHelper {
                ns,
                claim: claim.clone(),
                intent: self.pvc.intent,
            },
            label,
            level,
            claim,
        );
    }

    /// Create the helper pod and wait for it to run. `generateName` means two
    /// sessions browsing the same claim never collide on a name.
    pub(super) fn create_pvc_helper(&mut self, ns: String, claim: String, intent: PvcIntent) {
        if self.deny_readonly()
            || self
                .guard(
                    "pvc-explore",
                    "persistentvolumeclaims",
                    &[(claim.clone(), ns.clone())],
                    ConfirmLevel::None,
                )
                .is_none()
        {
            if self.pvc.recovery.is_some() {
                let reason = self.flash.clone();
                self.pvc_recovery_error(&reason);
            }
            return;
        }
        self.pvc.intent = intent;
        self.pvc.run += 1;
        let run = self.pvc.run;
        let ttl = self.pvc_ttl_secs();
        let mut manifest = pvc::helper_pod(&claim, &self.pvc_cfg.image, ttl);
        let original = self.pvc.recovery.as_ref().map(|r| r.original.clone());
        self.note_action("pvc-explore helper pod", format!("{claim} in {ns}"));
        let status = self.claim_status(format!("starting a helper pod for {claim}…"));
        let context = self.cluster.context.clone();
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let result = async {
                if let Some(original) = original {
                    let plan = load_recovery_plan(client.clone(), &ns, &claim, &original).await?;
                    pvc::apply_helper_options(&mut manifest, &plan.helper?);
                }
                start_helper(client, &ns, manifest).await
            }
            .await;
            let _ = tx
                .send(Msg::PvcTarget {
                    generation: genr,
                    run,
                    namespace: ns,
                    context,
                    claim: status,
                    result: result.map(Some),
                })
                .await;
        });
    }

    fn start_pvc_recovery(&mut self, error: String) {
        let Some(original) = self.pvc.mount.clone() else {
            return;
        };
        self.pvc.remote_error = Some(error.clone());
        self.pvc.loading = true;
        self.pvc.recovery = Some(RecoveryState {
            error,
            original: original.clone(),
            candidates: Default::default(),
            helper: None,
        });
        self.pvc.run += 1;
        let run = self.pvc.run;
        let generation = self.generation;
        let client = self.cluster.client.clone();
        let namespace = self.pvc.namespace.clone();
        let claim = self.pvc.claim.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_recovery_plan(client, &namespace, &claim, &original).await;
            let _ = tx
                .send(Msg::PvcRecovery {
                    generation,
                    run,
                    result,
                })
                .await;
        });
    }

    pub(super) fn handle_pvc_recovery(
        &mut self,
        run: u64,
        result: Result<pvc::RecoveryPlan, String>,
    ) {
        if run != self.pvc.run || !self.pvc.active {
            return;
        }
        self.pvc.loading = false;
        if self.mode != Mode::PvcExplore {
            self.pvc_recovery_error("Recovery canceled because another view or dialog is open.");
            return;
        }
        match result {
            Ok(plan) => {
                let Some(recovery) = &mut self.pvc.recovery else {
                    return;
                };
                recovery.candidates = plan.candidates.into();
                recovery.helper = Some(plan.helper);
                self.next_pvc_candidate();
            }
            Err(error) => self.pvc_recovery_error(&error),
        }
    }

    fn next_pvc_candidate(&mut self) {
        let Some(recovery) = &mut self.pvc.recovery else {
            return;
        };
        if let Some(mount) = recovery.candidates.pop_front() {
            self.enter_pvc_target(self.pvc.namespace.clone(), mount);
            return;
        }
        let helper = recovery.helper.take();
        self.pvc.loading = false;
        match helper {
            Some(Ok(_)) => {
                self.offer_pvc_helper();
                if self.mode == Mode::PvcExplore {
                    let reason = self.flash.clone();
                    self.pvc_recovery_error(&reason);
                }
            }
            Some(Err(error)) => self.pvc_recovery_error(&error),
            None => self.pvc_recovery_error(
                "No further recovery targets are available. Reopen the claim to retry.",
            ),
        }
    }

    fn pvc_recovery_error(&mut self, reason: &str) {
        let message = match &self.pvc.recovery {
            Some(recovery) => format!("{}\n\n{reason}", recovery.error),
            None => reason.to_owned(),
        };
        self.pvc.loading = false;
        self.pvc.remote_error = Some(message.clone());
        self.flash_warn(&message);
    }

    /// `[pvc_explore] ttl`, already validated at load; a value that slipped
    /// through falls back to the default rather than creating a pod that never
    /// expires.
    fn pvc_ttl_secs(&self) -> u64 {
        crate::providers::parse_lookback(&self.pvc_cfg.ttl)
            .ok()
            .and_then(|s| u64::try_from(s).ok())
            .filter(|s| *s > 0)
            .unwrap_or(crate::config::PVC_DEFAULT_TTL_SECS)
    }

    // ----- browsing ------------------------------------------------------

    /// Show `path` in the remote pane. The listing runs off-thread; the pane
    /// keeps showing the previous directory until it arrives, so navigation
    /// never blanks the screen.
    fn open_remote_dir(&mut self, path: String, select: Option<String>) {
        self.pvc.run += 1;
        let run = self.pvc.run;
        // Per request, so a listing that fails cannot leave a wanted selection
        // behind to steer an unrelated later one.
        self.pvc.want_select = select;
        self.pvc.remote_path = path.clone();
        self.pvc.loading = true;
        let Some((argv, nonce)) = self.pvc_list_argv(&path) else {
            self.pvc.loading = false;
            return;
        };
        let tx = self.tx.clone();
        let genr = self.generation;
        tokio::spawn(async move {
            let result = run_listing(&argv, &nonce).await;
            let _ = tx
                .send(Msg::PvcListing {
                    generation: genr,
                    run,
                    path,
                    result,
                })
                .await;
        });
    }

    pub(super) fn handle_pvc_listing(
        &mut self,
        run: u64,
        path: String,
        result: crate::store::PvcListingResult,
    ) {
        if run != self.pvc.run || !self.pvc.active {
            return;
        }
        self.pvc.loading = false;
        self.pvc.remote_path = path.clone();
        match result {
            Ok((listing, warn)) => {
                self.pvc.listed = true;
                self.pvc.recovery = None;
                self.pvc.truncated = listing.truncated;
                // Re-listing the same directory keeps the cursor; stepping
                // into a new one starts at the top, unless we stepped *out* of
                // one, in which case it starts on the one we left.
                let keep = self.pvc.want_select.take().or_else(|| {
                    (self.pvc.displayed_path == self.pvc.remote_path)
                        .then(|| self.pvc.selected_remote().map(|e| e.name.clone()))
                        .flatten()
                });
                self.pvc.remote = listing.entries;
                self.pvc.remote_error = None;
                self.pvc.displayed_path = self.pvc.remote_path.clone();
                PvcExplore::reselect(
                    &mut self.pvc.remote_state,
                    &self.pvc.remote,
                    keep.as_deref(),
                );
                // Entries `ls` could not stat are still listed; say so rather
                // than let the pane quietly under-report what is there.
                if let Some(w) = warn {
                    self.flash_warn(&w);
                }
            }
            // Step back to where the pane was, the way the local side does on
            // a failed `cd` — the entries on screen still belong to that
            // directory, so leaving the title pointing somewhere else would
            // mislabel them.
            Err(e) => {
                if !self.pvc.listed
                    && self.mode == Mode::PvcExplore
                    && self
                        .pvc
                        .mount
                        .as_ref()
                        .is_some_and(|m| !m.helper && path == m.path)
                    && pvc::missing_listing_tools(&e)
                {
                    if self.pvc.recovery.is_some() {
                        self.next_pvc_candidate();
                    } else {
                        self.start_pvc_recovery(e);
                    }
                    return;
                }
                if self.pvc.recovery.is_some() {
                    self.pvc_recovery_error(&e);
                    return;
                }
                self.pvc.remote_path = self.pvc.displayed_path.clone();
                self.pvc.want_select = None;
                // Only when the failure is about the directory still on
                // screen: with nothing to step back to, the pane would
                // otherwise read "empty" — the one thing an unreadable volume
                // must never say, and the flash expires after eight seconds.
                // A failed step-out from an *empty* directory is a different
                // thing, and must not label that directory unreadable.
                if self.pvc.remote.is_empty() && self.pvc.displayed_path == path {
                    self.pvc.remote_error = Some(e.clone());
                }
                self.flash_warn(&e);
            }
        }
    }

    /// Re-read the local pane. Local directories are read on the UI thread:
    /// unlike a cluster call this is a syscall against the page cache, and the
    /// asynchrony would only buy a frame of staleness.
    fn reload_local(&mut self) {
        let keep = self.pvc.selected_local().map(|e| e.name.clone());
        self.reload_local_selecting(keep);
    }

    /// [`Self::reload_local`], choosing what the cursor lands on. Stepping
    /// *into* a directory passes `None`: the current selection names the
    /// directory being entered, and reselecting that name inside it would be a
    /// coincidence rather than a place the user has been.
    fn reload_local_selecting(&mut self, keep: Option<String>) {
        match pvc::read_local(&self.pvc.local_path) {
            Ok((entries, truncated)) => {
                self.pvc.local = entries;
                self.pvc.local_truncated = truncated;
                self.pvc.local_error = None;
                PvcExplore::reselect(&mut self.pvc.local_state, &self.pvc.local, keep.as_deref());
            }
            Err(e) => {
                self.pvc.local.clear();
                self.pvc.local_state.select(None);
                self.pvc.local_error = Some(e);
            }
        }
    }

    /// The listing argv, and the nonce its output has to carry back.
    fn pvc_list_argv(&self, path: &str) -> Option<(Vec<String>, String)> {
        let mount = self.pvc.mount.as_ref()?;
        let probe = pvc::list_probe(&mount.path);
        let mut argv = self.exec_prefix(
            &self.pvc.namespace,
            &mount.pod,
            Some(mount.container.as_str()),
            false,
        );
        argv.extend([
            "sh".into(),
            "-c".into(),
            probe.script,
            // `$0`, then the path as `$1`: never spliced into the script.
            "sh".into(),
            path.to_string(),
            probe.root,
        ]);
        Some((argv, probe.nonce))
    }

    /// Ask for a shell rooted at `path` inside the mount. Returns whether
    /// anything is still going to happen — `false` means a guardrail refused
    /// it and nothing will suspend.
    ///
    /// The exec lands in a real pod, so it passes the same `shell` guardrail
    /// as `s` on that pod's row. Without this, a rule denying shells in prod
    /// is defeated by shelling into a claim a prod pod mounts.
    fn pvc_shell_at(&mut self, path: &str) -> bool {
        let Some(mount) = self.pvc.mount.clone() else {
            return false;
        };
        let ns = self.pvc.namespace.clone();
        let targets = [(mount.pod.clone(), ns.clone())];
        let Some(level) = self.guard("shell", "pods", &targets, ConfirmLevel::None) else {
            return false;
        };
        let label = format!("Shell into {} via {}?", self.pvc.claim, mount.pod);
        let hint = mount.pod.clone();
        self.begin_guarded(
            ConfirmAction::PvcShell {
                ns,
                pod: mount.pod,
                container: mount.container,
                path: path.to_string(),
                claim: self.pvc.claim.clone(),
            },
            label,
            level,
            hint,
        );
        true
    }

    /// Queue the shell, once any guardrail confirmation is satisfied.
    pub(super) fn do_pvc_shell(
        &mut self,
        ns: String,
        pod: String,
        container: String,
        path: String,
        claim: String,
    ) {
        self.note_action("pvc shell", format!("{claim} in {ns}"));
        let mut argv = self.kubectl_base();
        argv.extend([
            "exec".into(),
            "-it".into(),
            "-n".into(),
            ns,
            pod,
            "-c".into(),
            container,
            "--".into(),
            "sh".into(),
            "-c".into(),
            // A path that has gone away silently drops the shell at the
            // container's WORKDIR, which is not where the user asked to be.
            // The message names no path: after the suspend this goes to a
            // real terminal, and both the path and `$PWD` are volume-derived
            // text that has not been through any escaping.
            format!(
                "cd -- \"$1\" 2>/dev/null || \
                 echo 'sofka: that directory is gone; starting where the container does' >&2\n\
                 {SHELL_FALLBACK}"
            ),
            "sh".into(),
            path,
        ]);
        self.pending = Some(Suspend::Shell(argv));
    }

    /// A guarded PVC shell was cancelled at the dialog: nothing will suspend,
    /// so run the teardown the suspend would have triggered.
    pub(super) fn pvc_shell_cancelled(&mut self) {
        self.after_suspend();
    }

    // ----- transfers -----------------------------------------------------

    /// `c`: copy the focused pane's selection into the other pane's directory.
    /// The direction is the focus, so one key does both and there is never a
    /// question about which way it went.
    fn copy_across(&mut self) {
        match self.pvc.focus {
            Pane::Remote => self.pvc_download(),
            Pane::Local => self.pvc_upload(),
        }
    }

    fn pvc_download(&mut self) {
        let Some(entry) = self.pvc.selected_remote().cloned() else {
            self.flash_warn("nothing selected to download");
            return;
        };
        let Some(mount) = self.pvc.mount.clone() else {
            return;
        };
        let src = pvc::join_path(self.pvc.current_dir(), &entry.name);
        let dest = self.pvc.local_path.join(&entry.name);
        let ns = self.pvc.namespace.clone();
        let dest = dest.to_string_lossy().into_owned();
        if !self.transferable(&entry, &src, &dest) {
            return;
        }
        // A download writes to the user's own disk, so no guardrail applies —
        // but overwriting a local file on one keystroke, with no undo, is not
        // something to do silently.
        if std::fs::symlink_metadata(&dest).is_ok() {
            self.confirm_return = Mode::PvcExplore;
            self.confirm_label = format!("{dest} already exists. Overwrite it?");
            self.confirm_action = Some(ConfirmAction::Transfer {
                ns,
                pod: mount.pod,
                container: Some(mount.container),
                // A download, not an upload: without this the confirmation
                // would run the copy backwards and write into the cluster,
                // past read-only mode and both upload guardrails.
                upload: false,
                src,
                dest,
            });
            self.mode = Mode::Confirm;
            return;
        }
        self.start_transfer(ns, mount.pod, Some(mount.container), false, src, dest);
    }

    fn pvc_upload(&mut self) {
        let Some(entry) = self.pvc.selected_local().cloned() else {
            self.flash_warn("nothing selected to upload");
            return;
        };
        let Some(mount) = self.pvc.mount.clone() else {
            return;
        };
        if mount.read_only {
            self.flash_warn(&format!(
                "{} is mounted read-only in {} — uploads would fail",
                self.pvc.claim, mount.pod
            ));
            return;
        }
        if self.deny_readonly() {
            return;
        }
        let src = self.pvc.local_path.join(&entry.name);
        let dest = pvc::join_path(self.pvc.current_dir(), &entry.name);
        let src = src.to_string_lossy().into_owned();
        if !self.transferable(&entry, &src, &dest) {
            return;
        }
        let claim = self.pvc.claim.clone();
        let ns = self.pvc.namespace.clone();
        let targets = [(claim.clone(), ns.clone())];
        // Gated twice, because it is two things at once. `transfer` against
        // the serving pod is the rule an operator already wrote to stop `t`
        // uploads into prod, and reaching the same pod through a claim it
        // mounts must not defeat it; `pvc-upload` against the claim is the
        // rule for protecting a volume whichever pod exposes it. The stronger
        // confirmation of the two wins.
        let Some(pod_level) = self.guard(
            "transfer",
            "pods",
            &[(mount.pod.clone(), ns.clone())],
            ConfirmLevel::None,
        ) else {
            return;
        };
        let Some(level) = self.guard("pvc-upload", "persistentvolumeclaims", &targets, pod_level)
        else {
            return;
        };
        let label = format!("Upload {src} into {claim}:{dest}?");
        self.begin_guarded(
            ConfirmAction::Transfer {
                ns,
                pod: mount.pod,
                container: Some(mount.container),
                upload: true,
                src,
                dest,
            },
            label,
            level,
            claim,
        );
    }

    /// Whether `kubectl cp` can address this entry at all, flashing why not.
    /// `cp` splits each argument on the first `:` to separate pod from path,
    /// so a name containing one is unaddressable however it is quoted — and a
    /// name that lost bytes to lossy UTF-8 decoding no longer names anything.
    fn transferable(&mut self, entry: &Entry, src: &str, dest: &str) -> bool {
        if !entry.addressable() {
            self.flash_warn(&format!(
                "{} is not valid UTF-8 — sofka cannot address it",
                entry.name
            ));
            return false;
        }
        // The whole path, not just the leaf: a parent directory named `a:b` —
        // or a local working directory with a colon in it — breaks `cp` just
        // as thoroughly as the file's own name would.
        if let Some(bad) = [src, dest].into_iter().find(|p| p.contains(':')) {
            self.flash_warn(&format!(
                "kubectl cp cannot address {bad:?} — a ':' separates pod from path"
            ));
            return false;
        }
        true
    }

    /// Both panes after a copy landed, so the new file shows up where it went
    /// without the user having to ask.
    pub(super) fn refresh_pvc_panes(&mut self) {
        if !self.pvc.active {
            return;
        }
        self.reload_local();
        let path = self.pvc.current_dir().to_string();
        self.open_remote_dir(path, None);
    }

    // ----- lifecycle -----------------------------------------------------

    /// An interactive command the run loop suspended for has finished. A shell
    /// launched from a PVC row owns its (possibly created) pod for exactly that
    /// long; one launched from inside the browser does not, because the browser
    /// is still using it.
    pub fn after_suspend(&mut self) {
        if !std::mem::take(&mut self.pvc.shell_pending) || self.pvc.active {
            return;
        }
        self.cleanup_pvc_helper();
        self.pvc.mount = None;
    }

    /// Leave the browser, taking any helper pod with it.
    pub(super) fn close_pvc_explore(&mut self) {
        self.leave_pvc_explore();
        self.mode = self.return_mode;
        if self.return_mode == Mode::Table {
            self.restore_selection();
        }
    }

    /// Tear the browser down without deciding where to go next. Called both by
    /// `esc` and by anything that navigates out from under the browser — a
    /// palette jump, a bookmark — so a helper pod is never orphaned and the
    /// overlays stop drawing panes that are no longer on screen.
    pub(super) fn leave_pvc_explore(&mut self) {
        self.cleanup_pvc_helper();
        self.pvc.active = false;
        self.pvc.mount = None;
        self.pvc.shell_pending = false;
        self.pvc.loading = false;
        self.pvc.remote.clear();
        self.pvc.local.clear();
        // Paths belong to the claim we just left; a failed first listing on
        // the next one would otherwise show this one's directory in the title.
        self.pvc.remote_path.clear();
        self.pvc.displayed_path.clear();
        self.pvc.want_select = None;
        self.pvc.recovery = None;
        self.pvc.listed = false;
    }

    /// Delete the helper pod, if this session created one. Best effort and
    /// fire-and-forget: `activeDeadlineSeconds` on the pod is the backstop for
    /// a sofka that never gets to run this (a crash, a `ctrl-c`).
    pub(super) fn cleanup_pvc_helper(&mut self) {
        let Some(mount) = self.pvc.mount.take() else {
            return;
        };
        if !mount.helper {
            self.pvc.mount = Some(mount);
            return;
        }
        let ns = self.pvc.namespace.clone();
        self.delete_helper(ns, mount);
    }

    /// A resolve arrived for a generation that is over — a dashboard opened, a
    /// context switched, the watch restarted. The pod it names may already
    /// have been created, so delete it rather than let it sit out its TTL
    /// holding the volume. Only when it is still this cluster's: after a
    /// `:ctx` switch the client points somewhere else, and a name that
    /// resolves in both clusters would be somebody else's pod.
    pub(super) fn discard_pvc_target(
        &mut self,
        namespace: String,
        context: String,
        result: Result<Option<Mount>, String>,
    ) {
        if context != self.cluster.context {
            // Deleting it would target the wrong cluster. Its own TTL will
            // reap it, but the user is the only one who can do it sooner.
            if matches!(&result, Ok(Some(m)) if m.helper) {
                self.flash_warn(&format!(
                    "a PVC helper pod was left in {context}/{namespace} — :ctx back and :pvc-clean to remove it now"
                ));
            }
            return;
        }
        if let Ok(Some(mount)) = result {
            self.delete_helper(namespace, mount);
        }
    }

    /// Delete one helper pod, fire and forget. Only ever called with a pod
    /// this session created, so a `helper: false` mount is a no-op rather than
    /// an accidental delete of somebody's workload.
    fn delete_helper(&mut self, ns: String, mount: Mount) {
        if !mount.helper {
            return;
        }
        // Deleting a pod is a mutation, and `:journal` is where the session's
        // mutations are accounted for — including the ones it undoes for you.
        self.note_action(
            "pvc-explore helper pod removed",
            format!("{} in {ns}", mount.pod),
        );
        let client = self.cluster.client.clone();
        tokio::spawn(async move {
            let pods: Api<Pod> = Api::namespaced(client, &ns);
            let _ = pods.delete(&mount.pod, &DeleteParams::default()).await;
        });
    }

    /// Await the helper pod's deletion instead of spawning it. The browser's
    /// own `esc` can spawn and forget; process exit cannot, because the
    /// runtime stops before a spawned task is polled.
    pub async fn shutdown_pvc_helper(&mut self) {
        let Some(mount) = self.pvc.mount.take() else {
            return;
        };
        if !mount.helper {
            return;
        }
        let pods: Api<Pod> = Api::namespaced(self.cluster.client.clone(), &self.pvc.namespace);
        let _ = pods.delete(&mount.pod, &DeleteParams::default()).await;
    }

    /// `:pvc-clean` — delete helper pods left behind by a session that exited
    /// without cleaning up. Deleting pods is a mutation like any other: it is
    /// blocked in read-only mode, matched by the `pvc-explore` guardrail, and
    /// always confirmed, because the sweep is by label and the user cannot see
    /// what it will hit first.
    pub(super) fn request_pvc_clean(&mut self) {
        if self.namespace_is_pattern() {
            self.flash_warn("select one namespace or all namespaces to clean PVC helpers");
            return;
        }
        if self.deny_readonly() {
            return;
        }
        let scope = self.pvc_clean_scope();
        let where_ = match &scope {
            Some(ns) => ns.clone(),
            None => "all namespaces".into(),
        };
        let targets = [(String::from("*"), scope.clone().unwrap_or_default())];
        // A cluster-wide sweep has no one namespace to match, so a
        // `namespaces = ["prod"]` rule would be skipped entirely unless the
        // guard is told the scope is every namespace (as `:sanitize` does).
        let Some(level) = self.guard_scope(
            "pvc-explore",
            "persistentvolumeclaims",
            &targets,
            ConfirmLevel::Plain,
            scope.is_none(),
        ) else {
            return;
        };
        self.begin_guarded(
            ConfirmAction::PvcClean { scope },
            // Honest about the blast radius: it cannot tell a leftover from a
            // pod another sofka session is browsing through right now.
            format!(
                "Delete every sofka PVC-explore helper pod in {where_}, including any another session is using?"
            ),
            level,
            where_,
        );
    }

    /// Namespace the sweep covers: the current one, or every namespace when
    /// the view is across all of them. A helper leaked in another namespace
    /// would otherwise be invisible to the command meant to find it.
    fn pvc_clean_scope(&self) -> Option<String> {
        (!self.namespace.is_empty()).then(|| self.namespace.clone())
    }

    pub(super) fn cleanup_pvc_helpers(&mut self, scope: Option<String>) {
        let where_ = scope.clone().unwrap_or_else(|| "all namespaces".into());
        self.note_action("pvc-clean", where_.clone());
        let status = self.claim_status(format!("cleaning up PVC helper pods in {where_}…"));
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let genr = self.generation;
        // The browser's own pod is not litter — sweeping it would break the
        // view the user is looking at.
        let keep = self
            .pvc
            .mount
            .as_ref()
            .filter(|m| m.helper)
            .map(|m| m.pod.clone());
        tokio::spawn(async move {
            let pods: Api<Pod> = match &scope {
                Some(ns) => Api::namespaced(client, ns),
                None => Api::all(client),
            };
            let selector = pvc::helper_selector();
            let mut deleted = 0usize;
            let mut failed = Vec::new();
            match pods.list(&ListParams::default().labels(&selector)).await {
                Ok(list) => {
                    for pod in list.items {
                        let Some(name) = pod.metadata.name.clone() else {
                            continue;
                        };
                        // The labels got it into this list; the name prefix
                        // and the claim annotation are the rest of the
                        // evidence. None of it is unforgeable — nothing sofka
                        // writes on creation is — but all four together make
                        // an accidental match essentially impossible, and a
                        // deliberate one still has to get past the
                        // confirmation and the `pvc-explore` guardrail.
                        let names_a_claim = pod
                            .metadata
                            .annotations
                            .as_ref()
                            .is_some_and(|a| a.contains_key(HELPER_ANNOTATION));
                        if !name.starts_with(HELPER_PREFIX)
                            || !names_a_claim
                            || keep.as_deref() == Some(name.as_str())
                        {
                            continue;
                        }
                        let ns = pod.metadata.namespace.clone().unwrap_or_default();
                        let api: Api<Pod> = Api::namespaced(pods.clone().into_client(), &ns);
                        match api.delete(&name, &DeleteParams::default()).await {
                            Ok(_) => deleted += 1,
                            Err(e) => failed.push(format!("{ns}/{name}: {e}")),
                        }
                    }
                }
                Err(e) => failed.push(format!("listing pods in {where_}: {e}")),
            }
            let _ = tx
                .send(Msg::PvcHelpersCleaned {
                    generation: genr,
                    claim: status,
                    deleted,
                    failed,
                })
                .await;
        });
    }

    // ----- input ---------------------------------------------------------

    pub(super) fn key_pvc_explore(&mut self, key: KeyInput) {
        match (key.action, key.code) {
            (Some(Action::Back), _) | (Some(Action::Close), _) => self.close_pvc_explore(),
            (Some(Action::SwitchPane), _) => self.pvc.focus = self.other_pane(),
            (Some(Action::Left), _) => self.pvc.focus = Pane::Local,
            (Some(Action::Right), _) => self.pvc.focus = Pane::Remote,
            (Some(Action::Down), _) => self.step_pane(true),
            (Some(Action::Up), _) => self.step_pane(false),
            (Some(Action::First), _) => self.jump_pane(true),
            (Some(Action::Last), _) => self.jump_pane(false),
            (Some(Action::Accept), _) => self.descend_pane(),
            (Some(Action::Parent), _) => self.ascend_pane(),
            (Some(Action::Copy), _) => self.copy_across(),
            (Some(Action::Refresh), _) => self.refresh_pvc_panes(),
            (Some(Action::Shell), _) => self.pvc_shell_here(),
            _ => {}
        }
    }

    /// `s` in the browser: a shell at the directory the remote pane is showing,
    /// not at the mount root — you have already navigated to where you want to
    /// be.
    fn pvc_shell_here(&mut self) {
        if self.deny_readonly() {
            return;
        }
        let path = self.pvc.current_dir().to_string();
        self.pvc_shell_at(&path);
    }

    fn other_pane(&self) -> Pane {
        match self.pvc.focus {
            Pane::Local => Pane::Remote,
            Pane::Remote => Pane::Local,
        }
    }

    fn step_pane(&mut self, down: bool) {
        match self.pvc.focus {
            Pane::Local => {
                let len = self.pvc.local.len();
                list_step(&mut self.pvc.local_state, len, down);
            }
            Pane::Remote => {
                let len = self.pvc.remote.len();
                list_step(&mut self.pvc.remote_state, len, down);
            }
        }
    }

    fn jump_pane(&mut self, top: bool) {
        let (state, len) = match self.pvc.focus {
            Pane::Local => (&mut self.pvc.local_state, self.pvc.local.len()),
            Pane::Remote => (&mut self.pvc.remote_state, self.pvc.remote.len()),
        };
        if len > 0 {
            state.select(Some(if top { 0 } else { len - 1 }));
        }
    }

    fn descend_pane(&mut self) {
        match self.pvc.focus {
            Pane::Local => {
                let Some(entry) = self.pvc.selected_local().cloned() else {
                    return;
                };
                // A symlink is worth trying: `read_local` reports the link, not
                // its target, so the only way to know is to look.
                if entry.kind == crate::pvcexplore::EntryKind::File {
                    return;
                }
                let next = self.pvc.local_path.join(&entry.name);
                let previous = std::mem::replace(&mut self.pvc.local_path, next);
                // The rollback re-read must land on the row we came from: the
                // failed read has already cleared the cursor, so the name has
                // to be carried across it by hand.
                let here = entry.name.clone();
                self.reload_local_selecting(None);
                // The rollback re-read succeeds and clears the error, so the
                // reason has to be kept — otherwise the keystroke is a silent
                // no-op, while the remote pane flashes on the same failure.
                if let Some(why) = self.pvc.local_error.clone() {
                    self.pvc.local_path = previous;
                    self.reload_local_selecting(Some(here));
                    self.flash_warn(&why);
                }
            }
            Pane::Remote => {
                let Some(entry) = self.pvc.selected_remote().cloned() else {
                    return;
                };
                if entry.kind == crate::pvcexplore::EntryKind::File {
                    return;
                }
                if !entry.addressable() {
                    self.flash_warn(&format!(
                        "{} is not valid UTF-8 — sofka cannot address it",
                        entry.name
                    ));
                    return;
                }
                let next = pvc::join_path(self.pvc.current_dir(), &entry.name);
                self.open_remote_dir(next, None);
            }
        }
    }

    fn ascend_pane(&mut self) {
        match self.pvc.focus {
            Pane::Local => {
                let Some(parent) = self.pvc.local_path.parent().map(PathBuf::from) else {
                    self.flash_warn("already at the filesystem root");
                    return;
                };
                // Stepping out lands on the directory just left, the way
                // every file manager does it — `reload_local` would otherwise
                // reselect whatever the *child's* cursor was named, which in
                // the parent is a coincidence at best.
                let left = self
                    .pvc
                    .local_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                let here = self.pvc.selected_local().map(|e| e.name.clone());
                let previous = std::mem::replace(&mut self.pvc.local_path, parent);
                self.reload_local_selecting(left);
                // Symmetric with descending, and with the remote pane: an
                // unreadable directory leaves the view — and the cursor —
                // where they were, and says why.
                if let Some(why) = self.pvc.local_error.clone() {
                    self.pvc.local_path = previous;
                    self.reload_local_selecting(here);
                    self.flash_warn(&why);
                }
            }
            Pane::Remote => {
                let root = self.pvc.root().to_string();
                let here = self.pvc.current_dir().to_string();
                match pvc::parent_path(&here, &root) {
                    Some(parent) => {
                        // Land back on the directory just left, not at the top.
                        let left = here
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .map(str::to_string)
                            .filter(|s| !s.is_empty());
                        self.open_remote_dir(parent, left);
                    }
                    None => self.flash_warn("already at the top of the volume"),
                }
            }
        }
    }
}

/// The pods `ApiResource`, built without discovery. The resolve task runs
/// outside the UI thread and so has no access to the cluster registry, and
/// `v1/Pod` is not a kind whose group could differ between clusters.
fn pod_resource() -> kube::discovery::ApiResource {
    kube::discovery::ApiResource::erase::<Pod>(&())
}

async fn load_recovery_plan(
    client: Client,
    ns: &str,
    claim: &str,
    original: &Mount,
) -> Result<pvc::RecoveryPlan, String> {
    let read = async {
        let pods: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &pod_resource());
        let pods = pods
            .list(&ListParams::default())
            .await
            .map_err(|e| format!("Cannot list recovery pods: {e}"))?;
        let resource = kube::discovery::ApiResource::erase::<
            k8s_openapi::api::core::v1::PersistentVolumeClaim,
        >(&());
        let claims: Api<DynamicObject> = Api::namespaced_with(client, ns, &resource);
        let claim = claims
            .get(claim)
            .await
            .map_err(|e| format!("Cannot check volume access modes: {e}"))?;
        Ok(pvc::recovery_plan(&pods.items, &claim, original))
    };
    tokio::time::timeout(LIST_TIMEOUT, read)
        .await
        .map_err(|_| "Recovery checks timed out.".to_owned())?
}

/// Create the helper pod and poll until it is `Running` (or fails), returning
/// the mount to browse it through.
async fn start_helper(client: Client, ns: &str, manifest: Value) -> Result<Mount, String> {
    let spec: Pod = serde_json::from_value(manifest).map_err(|e| e.to_string())?;
    let volume_mount = spec
        .spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .and_then(|c| c.volume_mounts.as_ref())
        .and_then(|m| m.first());
    let read_only = volume_mount.and_then(|m| m.read_only).unwrap_or(false);
    let sub_path = volume_mount
        .and_then(|m| m.sub_path.clone())
        .unwrap_or_default();
    let pods: Api<Pod> = Api::namespaced(client, ns);
    let created = pods
        .create(&PostParams::default(), &spec)
        .await
        .map_err(|e| format!("creating helper pod: {e}"))?;
    let name = created.metadata.name.clone().unwrap_or_default();

    let deadline = tokio::time::Instant::now() + HELPER_READY_TIMEOUT;
    loop {
        match pods.get(&name).await {
            Ok(pod) => match pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
                Some("Running") => {
                    return Ok(Mount {
                        pod: name,
                        container: "explore".into(),
                        path: HELPER_MOUNT.into(),
                        sub_path: Some(sub_path),
                        read_only,
                        helper: true,
                    });
                }
                // A pod that reaches a terminal phase is never coming up; say
                // why rather than waiting out the timeout.
                Some(phase @ ("Failed" | "Succeeded")) => {
                    let reason = pod
                        .status
                        .as_ref()
                        .and_then(|s| s.reason.clone())
                        .unwrap_or_else(|| phase.to_string());
                    let _ = pods.delete(&name, &DeleteParams::default()).await;
                    return Err(format!("helper pod {name}: {reason}"));
                }
                _ => {}
            },
            Err(e) => {
                let _ = pods.delete(&name, &DeleteParams::default()).await;
                return Err(format!("waiting for helper pod {name}: {e}"));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = pods.delete(&name, &DeleteParams::default()).await;
            return Err(format!(
                "helper pod {name} did not start within {}s (image pull? unschedulable claim?)",
                HELPER_READY_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(HELPER_POLL).await;
    }
}

/// Run one `kubectl exec … ls` and turn it into entries. Exit code
/// [`EXIT_NOT_A_DIRECTORY`] is the script's own signal, not a tool failure, so
/// it gets a sentence rather than kubectl's stderr.
async fn run_listing(argv: &[String], nonce: &str) -> crate::store::PvcListingResult {
    let run = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(LIST_TIMEOUT, run).await {
        Err(_) => {
            return Err(format!(
                "listing timed out after {}s",
                LIST_TIMEOUT.as_secs()
            ));
        }
        Ok(r) => r.map_err(|e| format!("kubectl exec failed to start: {e}"))?,
    };
    pvc::interpret_listing(
        nonce,
        out.status.code(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )
}
