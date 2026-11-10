//! Progress for a background `kubectl cp`, measured at the destination:
//! locally for a download, through one long-lived exec for an upload.
//!
//! The copy itself is still `kubectl cp`. Nothing here changes what is
//! transferred, in which direction, or which guardrail had to allow it.
//!
//! Measuring the volume side is an exec of its own, gated no further than
//! the copy it belongs to; `docs/safety.md` carries that argument. How the
//! pod-side sampler is stopped, and why nothing simpler works, is
//! [`pvc::size_watch`]'s to explain — this module holds the other end.

use super::*;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

use crate::pvcexplore as pvc;

/// Ceiling on measuring a copy's source, whichever end it is on: a `du` in
/// the container for a download, a walk of this disk for an upload. A copy
/// whose source cannot be measured in that time still runs; it has no bar.
const SIZE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for a closed watcher to exit before letting
/// `kill_on_drop` take the local process. Generous, because killing it
/// before the API server has passed the stdin close through is the leak
/// this design exists to prevent, and nothing waits on the result.
const WATCH_STOP: Duration = Duration::from_secs(120);

/// How long to wait for an opening measurement before the copy starts
/// without one. Long enough for an exec handshake against a busy API
/// server, because the fallback — the first sample that arrives, taken
/// after the copy began — quietly writes off everything copied until then.
/// It costs nothing when there is nothing to wait for: a watcher that
/// cannot start ends its stream, and that is not waited on.
const BASELINE_WAIT: Duration = Duration::from_secs(3);

/// The rest a walk of `cost` earns before the next one. Three times over,
/// because a walk is a recursive `stat` of everything copied so far against
/// the disk `tar` is writing to, and a quarter duty cycle is plenty for a
/// bar. Bounded by [`WALK_LIMIT`] bounding the walk.
fn rest_after(cost: Duration) -> Duration {
    cost * 3
}

/// Ceiling on one walk of a download's destination. A destination that
/// cannot be measured inside it will not be measurable at this cadence at
/// all, so the sampler retires rather than starting another walk.
const WALK_LIMIT: Duration = Duration::from_secs(20);

/// How often a download's destination is re-measured: smooth enough to read
/// as motion, and one `stat` for the single large file this is mostly for.
const LOCAL_SAMPLE: Duration = Duration::from_millis(250);

/// Which row a copy's bar belongs on: a volume, a pane and a directory, not
/// just a name. By the time a sample lands the browser can be showing
/// another directory, or another claim entirely, and a same-named entry
/// there is a different file — `/data` and `/srv` are common enough mount
/// paths for that to be an ordinary Tuesday.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferAnchor {
    pub pane: Pane,
    /// The volume this row belongs to, as `namespace/claim`.
    pub claim: String,
    pub dir: String,
    pub name: String,
}

/// A copy in flight, and how far along it is.
pub struct TransferProgress {
    /// The claim the copy's result will present, and what a sample names to
    /// find its way back here.
    pub(crate) claim: StatusClaim,
    /// The status text this copy owns, without its progress suffix.
    label: String,
    /// The last status text this copy wrote, so a refresh can tell its own
    /// text from one something else borrowed the bar for. See
    /// [`App::set_claimed_progress`].
    shown: String,
    /// The watch generation the copy started under: a context switch cannot
    /// stop a copy, but the row it was drawn on is no longer on screen.
    generation: u64,
    /// Where the bar goes, when the copy started from a row at all. A
    /// transfer prompted by `t` with a typed path has no row to draw on.
    pub anchor: Option<TransferAnchor>,
    /// Bytes at the destination.
    pub done: u64,
    /// The source's total, when it could be measured. Without one there is
    /// no bar — only the byte count on the status bar.
    pub total: Option<u64>,
}

impl TransferProgress {
    /// One copy's progress, for a renderer test.
    #[cfg(test)]
    pub(crate) fn fake(generation: u64, anchor: TransferAnchor, done: u64, total: u64) -> Self {
        Self {
            claim: StatusClaim(0),
            label: String::new(),
            shown: String::new(),
            generation,
            anchor: Some(anchor),
            done,
            total: Some(total),
        }
    }
}

/// Everything one background copy needs, gathered on the UI thread because
/// the task that runs it cannot touch [`App`].
pub(super) struct TransferJob {
    /// The `kubectl cp` argv.
    pub cp: Vec<String>,
    /// `kubectl exec` up to and including `--`: a download sizes its source
    /// through it, an upload watches its destination (and so asks for `-i`).
    pub exec: Vec<String>,
    pub upload: bool,
    pub src: String,
    pub dest: String,
    /// The source's size, already known from the listing that showed it — so
    /// copying a single file costs no `du` at all.
    pub known: Option<u64>,
    pub claim: StatusClaim,
    pub generation: u64,
    pub tx: Sender<Msg>,
}

impl App {
    /// Register a copy so its samples have somewhere to land, and work out
    /// which row wears its bar.
    pub(super) fn register_transfer(
        &mut self,
        claim: StatusClaim,
        label: String,
        upload: bool,
        src: &str,
    ) -> Option<u64> {
        // A copy whose task died without reporting — a panic, a runtime
        // shutdown — would otherwise keep its row for the session. Nothing
        // older than the current watch can still be drawn, so nothing older
        // needs keeping.
        self.transfers.retain(|t| t.generation == self.generation);
        let anchor = self.transfer_anchor(upload, src);
        // Only the volume side is worth trusting a listing for. A local
        // source is a `stat` away in the task, which cannot be stale.
        let known = anchor
            .as_ref()
            .filter(|a| a.pane == Pane::Remote)
            .and_then(|a| self.anchored_size(a));
        self.transfers.push(TransferProgress {
            // What `claim_status` just put on the bar, so the first refresh
            // recognises the text as this copy's own.
            shown: format!("{label}…"),
            claim,
            label,
            generation: self.generation,
            anchor,
            done: 0,
            total: None,
        });
        known
    }

    /// A sample landed: move the bar, and put the percentage on the status
    /// bar so a copy with no row to draw on still reports itself.
    pub(super) fn update_transfer(&mut self, claim: StatusClaim, done: u64, total: Option<u64>) {
        let Some(transfer) = self.transfers.iter_mut().find(|t| t.claim == claim) else {
            return;
        };
        transfer.done = done;
        transfer.total = total;
        let progress = match total {
            Some(total) => format!("{}%", pvc::progress_pct(done, total)),
            // No total, so no share of it — but the byte count still tells a
            // copy that is moving from one that has stalled.
            None => pvc::human_size(done),
        };
        // Still ending in `…`: an unfinished operation's status is what lets
        // a stale failure borrow the bar (see `set_claimed_status`), and a
        // copy that is 62% done is unfinished.
        let text = format!("{} ({progress})…", transfer.label);
        let shown = transfer.shown.clone();
        let shown = self.set_claimed_progress(claim, &shown, text);
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.claim == claim) {
            transfer.shown = shown;
        }
    }

    /// The copy is over, whichever way it went.
    pub(super) fn end_transfer(&mut self, claim: StatusClaim) {
        self.transfers.retain(|t| t.claim != claim);
    }

    /// The rows in `pane` being copied while it shows `dir`, as `(name,
    /// done, total)` — for this volume, this watch, and only for copies
    /// with a measured total: a bar against a total nobody knows would be
    /// an animation, not a report.
    pub fn pane_transfers(&self, pane: Pane, dir: &str) -> Vec<(&str, u64, u64)> {
        self.transfers
            .iter()
            .filter_map(|t| {
                let anchor = t.anchor.as_ref()?;
                let total = t.total?;
                (t.generation == self.generation
                    && anchor.claim == self.pvc_key()
                    && anchor.pane == pane
                    && anchor.dir == dir.trim_end_matches('/'))
                .then_some((anchor.name.as_str(), t.done, total))
            })
            .collect()
    }

    /// The row a copy started from, or `None` when it did not start from one.
    ///
    /// The path is checked against the pane it should have come from rather
    /// than trusted: `t` can transfer any path the user types, and a bar on
    /// the row that happens to share its name would be pointing at the wrong
    /// file.
    fn transfer_anchor(&self, upload: bool, src: &str) -> Option<TransferAnchor> {
        if !self.pvc.active {
            return None;
        }
        if upload {
            let path = Path::new(src);
            let name = path.file_name()?.to_string_lossy().into_owned();
            // Compared as paths, stored as the pane's own rendering of it:
            // that rendering is what the renderer looks the bar up by, so
            // deriving a second one here would be a second chance to differ.
            if path.parent() != Some(self.pvc.local_path.as_path()) {
                return None;
            }
            return Some(TransferAnchor {
                pane: Pane::Local,
                claim: self.pvc_key(),
                dir: self.pvc.local_path.to_string_lossy().into_owned(),
                name,
            });
        }
        let (dir, name) = src.rsplit_once('/')?;
        // Trimmed on both sides and stored trimmed, so a directory has
        // exactly one spelling. A `mountPath` written as `/srv/` is shown
        // verbatim at the mount root but comes back from `parent_path` as
        // `/srv`, so an anchor holding either spelling stops matching the
        // moment the browser walks down and back up.
        let shown = self.pvc.current_dir().trim_end_matches('/');
        (dir.trim_end_matches('/') == shown).then(|| TransferAnchor {
            pane: Pane::Remote,
            claim: self.pvc_key(),
            dir: shown.to_string(),
            name: name.to_string(),
        })
    }

    /// The volume the browser is showing, as `namespace/claim`.
    fn pvc_key(&self) -> String {
        format!("{}/{}", self.pvc.namespace, self.pvc.claim)
    }

    /// The size the pane's own listing already reported for an anchored entry.
    /// `None` for a directory, which is exactly when `du` has to run.
    fn anchored_size(&self, anchor: &TransferAnchor) -> Option<u64> {
        let entries = match anchor.pane {
            Pane::Local => &self.pvc.local,
            Pane::Remote => &self.pvc.remote,
        };
        // A directory has no size in either listing — `ls` reports its
        // inode's, not its tree's — so this is a file's size or nothing.
        entries.iter().find(|e| e.name == anchor.name)?.size
    }
}

/// Run one `kubectl cp`, reporting how far it has got until it lands.
pub(super) async fn run_transfer(job: TransferJob) {
    let TransferJob {
        cp,
        exec,
        upload,
        src,
        dest,
        known,
        claim,
        generation,
        tx,
    } = job;

    // The two ends as `cp` addresses them: its own last two arguments,
    // rather than a second copy of them carried alongside.
    let from = cp[cp.len() - 2].clone();
    let to = cp[cp.len() - 1].clone();

    let mut sampler = Sampler::start(upload, &exec, &dest).await;
    // What the destination holds before the copy can touch it — see
    // `Sampler::baseline` for why it is not just the first sample.
    let mut baseline = sampler.baseline().await;

    let mut command = Command::new(&cp[0]);
    // No `kill_on_drop`: a copy is the user's data moving, and quitting
    // sofka has always let an in-flight one finish rather than leaving a
    // truncated file. Null stdin, like every child here — sofka's own is a
    // terminal in raw mode and `output()` does not close it for the child.
    command.args(&cp[1..]).stdin(std::process::Stdio::null());
    let copy = command.output();
    tokio::pin!(copy);

    // Behind the copy, not ahead of it: sizing first would leave the child
    // spawned but unpolled for as long as a `du` over a whole tree takes,
    // its result unobserved and its stderr pipe filling unread.
    let sizing = resolve_total(&exec, upload, &src, known);
    tokio::pin!(sizing);
    let mut sized = false;

    let mut total: Option<u64> = None;
    let mut done = 0;
    // Nothing will measure this destination, and no later total can undo
    // that. Decided before the loop for the sampler that never started; a
    // watcher that starts and then ends its stream is caught inside it
    // instead, one sample later, which the run loop folds into the same
    // frame — so a bar is withdrawn before it can be drawn either way.
    let mut blind = matches!(sampler, Sampler::Idle);
    let report = |done: u64, total: Option<u64>| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Msg::TransferProgress {
                    generation,
                    claim,
                    done,
                    total,
                })
                .await;
        }
    };
    let out = loop {
        tokio::select! {
            finished = &mut copy => break finished,
            measured = &mut sizing, if !sized => {
                // Polled to the end even when the destination has already
                // proven unmeasurable: the result costs nothing to throw
                // away, and a future dropped mid-poll leaves its `du` to
                // the container-side `timeout` rather than to anything
                // here.
                sized = true;
                if !blind {
                    // The bar can only appear once something knows what it
                    // is a share of; until then the status bar counts bytes.
                    total = measured;
                    report(done, total).await;
                }
            }
            sampled = sampler.sample() => {
                if let Some(sampled) = sampled {
                    let moved = moved(sampled, &mut baseline);
                    // Never backwards: a destination is walked while it is
                    // being written, and a walk that raced a rename would
                    // make the bar retreat for no reason.
                    if moved > done {
                        done = moved;
                        report(done, total).await;
                    }
                } else if matches!(sampler, Sampler::Idle) && !blind {
                    // The sampler has retired, so nothing will move again.
                    // A bar left at the fraction it reached says the copy
                    // stopped there, which is a claim; dropping the total
                    // leaves the byte count, which is only a number that
                    // stopped. No total still on its way re-draws it.
                    blind = true;
                    report(done, None).await;
                }
            }
        }
    };
    // No closing measurement, and no waiting for a total still in flight.
    // The run loop drains its channel before drawing, so a report sent here
    // usually shares a frame with the row's removal and is never seen — and
    // what it would buy, at the cost of another exec, is one sample period
    // of accuracy on a bar that is about to disappear.
    // Closing its stdin is what the container can see; nothing waits on the
    // unwinding, which `reap` does after the result has gone out.
    sampler.close();

    let result = match out {
        Ok(o) if o.status.success() => Ok(format!("copied {from} → {to}")),
        Ok(o) => Err(cp_error(&String::from_utf8_lossy(&o.stderr), &o.status)),
        Err(e) => Err(format!("kubectl cp failed to start: {e}")),
    };
    let _ = tx
        .send(Msg::TransferDone {
            generation,
            claim,
            result,
        })
        .await;
    sampler.reap().await;
}

/// What a raw destination measurement says has *moved*, against what was at
/// the destination when the copy started.
///
/// The baseline is what [`Sampler::baseline`] measured before the copy
/// started, or the first sample when nothing could: whatever was already
/// there is not progress, or an overwrite would open the bar at 100%.
///
/// It ratchets down to the lowest sample seen and never below, so a file
/// `cp` truncates is measured from zero while a directory, which only dips
/// by the file being replaced at that moment, is measured from what
/// survived. A folder copied over a copy of itself therefore ends short —
/// under-reporting being the direction to err in.
fn moved(sampled: u64, baseline: &mut Option<u64>) -> u64 {
    let base = baseline.get_or_insert(sampled);
    *base = (*base).min(sampled);
    sampled - *base
}

/// The line of `kubectl cp`'s stderr worth flashing. The actual cause (a
/// tar/exec error) comes before kubectl's generic "command terminated with
/// exit code" trailer, so the trailer is only used when it is all there is.
fn cp_error(stderr: &str, status: &std::process::ExitStatus) -> String {
    let line = |skip_trailer: bool| {
        stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !(l.is_empty() || skip_trailer && l.starts_with("command terminated")))
            .unwrap_or_default()
            .to_string()
    };
    match line(true) {
        e if e.is_empty() => match line(false) {
            e if e.is_empty() => format!("kubectl cp exited with {status}"),
            e => e,
        },
        e => e,
    }
}

/// The source's byte total, or `None` when nothing could measure it — an
/// unreadable tree, a `du` the container does not have, a walk past
/// [`pvc::MAX_SIZE_ENTRIES`], or a source that measures zero, which is what a
/// tree nothing could read comes back as.
async fn resolve_total(
    exec: &[String],
    upload: bool,
    src: &str,
    known: Option<u64>,
) -> Option<u64> {
    if known.is_some() {
        return known.filter(|&bytes| bytes > 0);
    }
    let measured = if upload {
        // Timed out like the remote probe: `read_dir` on a hung network mount
        // never returns, and the copy is already running behind this.
        let walk = local_size(PathBuf::from(src), pvc::MAX_SIZE_ENTRIES);
        match tokio::time::timeout(SIZE_TIMEOUT, walk).await.ok()? {
            pvc::Measure::Bytes(bytes) => Some(bytes),
            pvc::Measure::Absent | pvc::Measure::TooBig => None,
        }
    } else {
        let argv = size_argv(exec, &pvc::size_probe(), src);
        let run = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(SIZE_TIMEOUT, run).await.ok()?.ok()?;
        if !out.status.success()
            && let Some(line) = String::from_utf8_lossy(&out.stderr)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        {
            // A denial, or a container without a shell, is otherwise a bar
            // that quietly never appears.
            crate::log_warn!("pvc.size", error = line);
        }
        pvc::parse_size(&String::from_utf8_lossy(&out.stdout))
    };
    // A zero-byte file has a size; a bar against a total of zero renders as
    // instantly complete, so neither kind of zero is a total.
    measured.filter(|&bytes| bytes > 0)
}

/// `sh -c <script> sh <path>` appended to an exec prefix. The path is a
/// positional parameter, never spliced into the script — the same shape the
/// listing uses, and for the same reason.
fn size_argv(exec: &[String], script: &str, path: &str) -> Vec<String> {
    let mut argv = exec.to_vec();
    argv.extend([
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        path.to_string(),
    ]);
    argv
}

/// Blocking work, so it runs off the runtime — and blocking work the
/// runtime waits for at shutdown, so giving up on the future is not enough.
/// The flag is how the thread is told, and it is raised whether this returns
/// or is dropped mid-flight.
async fn local_size(path: PathBuf, cap: usize) -> pvc::Measure {
    let stop = Arc::new(AtomicBool::new(false));
    let _raised = Stop(stop.clone());
    tokio::task::spawn_blocking(move || pvc::local_size_capped(&path, cap, &stop))
        .await
        // The blocking pool is gone (shutdown); nothing will measure again.
        .unwrap_or(pvc::Measure::TooBig)
}

/// Raises its flag when it goes, however it goes — the walk behind it then
/// stops at the next directory instead of running on unwatched.
struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The exec an upload's samples arrive on.
struct Watcher {
    /// The write end of the exec's stdin, held open for as long as the
    /// watcher is wanted and dropped to stop it — see [`Sampler::close`].
    stdin: Option<ChildStdin>,
    child: tokio::process::Child,
    lines: Lines<BufReader<ChildStdout>>,
}

/// Where a copy's samples come from.
enum Sampler {
    /// A download lands on the user's own disk, so sampling is a walk of the
    /// destination and costs the cluster nothing.
    Local {
        path: PathBuf,
        tick: tokio::time::Interval,
        /// Rest owed to the destination before the next walk, when the last
        /// one cost more than the interval.
        owed: Duration,
        /// Entry cap for one walk, so a test can reach the retirement branch
        /// without a tree of [`pvc::MAX_SIZE_ENTRIES`] files.
        cap: usize,
        /// Ceiling on one walk, an argument for the same reason.
        limit: Duration,
    },
    /// An upload lands in the pod, so the samples come from one exec that
    /// prints the destination's size once a second — not one exec per sample
    /// against somebody's production pod.
    Remote(Box<Watcher>),

    /// Nothing to sample from: the watcher would not start, or its stream
    /// has ended. A copy in this state has no bar rather than one pinned
    /// wherever it had got to.
    Idle,
}

impl Sampler {
    async fn start(upload: bool, exec: &[String], dest: &str) -> Self {
        if !upload {
            let mut tick = tokio::time::interval(LOCAL_SAMPLE);
            // Skip, not the default Burst: a walk of a large destination can
            // take longer than the interval, and burst ticking would then run
            // walks back to back with no pause for the rest of the copy.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            return Sampler::Local {
                path: PathBuf::from(dest),
                tick,
                owed: Duration::ZERO,
                cap: pvc::MAX_SIZE_ENTRIES,
                limit: WALK_LIMIT,
            };
        }
        let argv = size_argv(exec, &pvc::size_watch(), dest);
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            // Piped and then left alone: the write end stays open for as long
            // as the child does, and closes with it. `null()` here would end
            // the loop before its first sample.
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Kept, not discarded: an RBAC denial, a container without a
            // shell and a quiet copy all look the same from out here, and
            // the only symptom is a bar that never moves.
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        match child {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    tokio::spawn(async move {
                        // The first line says why a watcher is not working —
                        // an RBAC denial, a container with no shell — and
                        // the rest is drained rather than read. Dropping the
                        // reader would close the pipe, and kubectl's next
                        // write to a closed stderr takes it down with
                        // SIGPIPE: a kubectl killed rather than closed
                        // abandons the exec instead of ending it, and the
                        // loop keeps running in the pod. `copy` keeps the
                        // pipe open to EOF whatever the bytes are, which
                        // `lines()` would not for invalid UTF-8.
                        let mut lines = BufReader::new(stderr).lines();
                        if let Ok(Some(line)) = lines.next_line().await
                            && !line.trim().is_empty()
                        {
                            crate::log_warn!("pvc.watch", error = line.trim());
                        }
                        let mut reader = lines.into_inner();
                        let mut sink = tokio::io::sink();
                        let _ = tokio::io::copy(&mut reader, &mut sink).await;
                    });
                }
                match child.stdout.take() {
                    Some(stdout) => Sampler::Remote(Box::new(Watcher {
                        stdin: child.stdin.take(),
                        child,
                        lines: BufReader::new(stdout).lines(),
                    })),
                    None => Sampler::Idle,
                }
            }
            // No watcher means no bar, not a failed copy.
            Err(e) => {
                crate::log_warn!("pvc.watch", error = e.to_string());
                Sampler::Idle
            }
        }
    }

    /// What the destination measures before the copy touches it.
    ///
    /// A download's destination is on this disk, so this is one walk and
    /// costs nothing. An upload's is in the pod, so its watcher's first
    /// sample is the measurement, waited for briefly — one exec's setup,
    /// spent before the copy starts, for a baseline that is exactly right
    /// rather than one that quietly subtracts the first second of it.
    ///
    /// `None` when nothing could measure it, and [`moved`] then falls back to
    /// the first sample. A destination that does not exist is not that case:
    /// it is a baseline of zero, and the most common one.
    async fn baseline(&mut self) -> Option<u64> {
        match self {
            Sampler::Local { path, cap, .. } => {
                // Bounded like everything else that runs before the copy can
                // report: a walk of an enormous existing destination must not
                // hold up the thing being measured.
                let walk = local_size(path.clone(), *cap);
                match tokio::time::timeout(BASELINE_WAIT, walk).await {
                    Ok(pvc::Measure::Bytes(bytes)) => Some(bytes),
                    Ok(pvc::Measure::Absent) => Some(0),
                    Ok(pvc::Measure::TooBig) | Err(_) => None,
                }
            }
            Sampler::Remote(watcher) => {
                // Read directly rather than through `sample`, which parks
                // for good at end-of-input: a container with no `du` exits
                // the script at once, and waiting out `BASELINE_WAIT` for a
                // sample that cannot come would delay every such copy.
                let lines = &mut watcher.lines;
                let first = async {
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(bytes) = pvc::parse_size_sample(&line) {
                            return Some(bytes);
                        }
                    }
                    None
                };
                tokio::time::timeout(BASELINE_WAIT, first)
                    .await
                    .unwrap_or_default()
            }
            Sampler::Idle => None,
        }
    }

    /// End the watcher the only way the container can see: by closing its
    /// stdin, which is the end-of-input [`pvc::size_watch`] explains. On the
    /// paths that never get here — a cancelled task, a killed session —
    /// nothing carries the close and the loop runs to its own bound.
    fn close(&mut self) {
        if let Sampler::Remote(watcher) = self {
            drop(watcher.stdin.take());
        }
    }

    /// Wait for a closed watcher to actually exit, so its `kubectl` is reaped
    /// rather than left to the runtime. Separate from [`Self::close`] and
    /// called after the copy's result has been sent: the close is what the
    /// container needs, and nothing should wait on the unwinding.
    async fn reap(&mut self) {
        if let Sampler::Remote(watcher) = self {
            let _ = tokio::time::timeout(WATCH_STOP, watcher.child.wait()).await;
        }
    }

    /// The destination's size, whenever it can next be read, and `None` when
    /// this attempt could not read it. `None` rather than zero, because zero
    /// is a measurement: fed to [`moved`] it would look like a destination
    /// that had just been truncated and take the baseline with it.
    ///
    /// Cancel-safe: `Lines::next_line` keeps its state in `self`, and an
    /// abandoned walk is a number nobody needs.
    async fn sample(&mut self) -> Option<u64> {
        match self {
            Sampler::Local {
                path,
                tick,
                owed,
                cap,
                limit,
            } => {
                // Paid at the top, not after measuring: sleeping the debt
                // off before the *next* walk keeps the number just measured
                // from ageing on the way out.
                if *owed > Duration::ZERO {
                    tokio::time::sleep(*owed).await;
                    // Cleared after the sleep, not before: a sample dropped
                    // mid-sleep would otherwise forget it owed anything.
                    *owed = Duration::ZERO;
                }
                tick.tick().await;
                let started = std::time::Instant::now();
                let walk = local_size(path.clone(), *cap);
                let measured = match tokio::time::timeout(*limit, walk).await {
                    Ok(measured) => measured,
                    // Too much destination to measure at this cadence,
                    // which is what the cap means as well.
                    Err(_) => pvc::Measure::TooBig,
                };
                *owed = rest_after(started.elapsed());
                match measured {
                    pvc::Measure::Bytes(bytes) => Some(bytes),
                    // Not there yet, or not readable this time round: one
                    // `lstat`, and asking again is the whole point.
                    pvc::Measure::Absent => None,
                    // Over the entry cap, which it will not come back under.
                    pvc::Measure::TooBig => {
                        *self = Sampler::Idle;
                        None
                    }
                }
            }
            Sampler::Remote(watcher) => loop {
                let lines = &mut watcher.lines;
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(bytes) = pvc::parse_size_sample(&line) {
                            return Some(bytes);
                        }
                    }
                    // The watcher is over — it ran out of samples, the
                    // container took it with it, or it never had a `du` to
                    // run. The copy is not, and the bar must not sit at
                    // whatever fraction it had reached.
                    _ => {
                        // Closed, not just dropped: `Idle` lets go of the
                        // `Watcher`, and `kill_on_drop` on a `kubectl` that
                        // has not been closed abandons the exec rather than
                        // ending it. Once, so the caller can take the bar
                        // away; after that there is nothing to wait for.
                        drop(watcher.stdin.take());
                        *self = Sampler::Idle;
                        return None;
                    }
                }
            },
            Sampler::Idle => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sofka-xfer-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A job whose "cluster" is this machine: an empty exec prefix leaves
    /// [`size_argv`] producing `sh -c <script> sh <path>`, so the real probe
    /// and watcher scripts run here against a real directory, and `cp` being
    /// an argv like any other, a script that writes slowly stands in for the
    /// copy.
    fn local_job(cp: &str, upload: bool, src: &Path, dest: &Path, tx: Sender<Msg>) -> TransferJob {
        TransferJob {
            cp: vec!["sh".into(), "-c".into(), cp.into()],
            exec: Vec::new(),
            upload,
            src: src.display().to_string(),
            dest: dest.display().to_string(),
            known: None,
            claim: StatusClaim(1),
            generation: 0,
            tx,
        }
    }

    /// Every progress report a run produced, and its result.
    async fn drain(mut rx: tokio::sync::mpsc::Receiver<Msg>) -> (Vec<(u64, Option<u64>)>, String) {
        let mut seen = Vec::new();
        let mut outcome = String::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::TransferProgress { done, total, .. } => seen.push((done, total)),
                Msg::TransferDone { result, .. } => {
                    outcome = match result {
                        Ok(summary) => summary,
                        Err(e) => format!("FAILED: {e}"),
                    };
                }
                _ => {}
            }
        }
        (seen, outcome)
    }

    #[tokio::test]
    async fn a_run_sizes_its_source_and_follows_its_destination() {
        let dir = scratch("run");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 8192]).unwrap();
        let dest = dir.join("dest.bin");
        // Eight visible steps, so the samples have something to follow.
        let cp = format!(
            "i=0; while [ $i -lt 8 ]; do printf '%1024s' '' >> '{}'; i=$((i+1)); sleep 0.1; done",
            dest.display()
        );
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        run_transfer(local_job(&cp, false, &src, &dest, tx)).await;
        let (seen, outcome) = drain(rx).await;

        assert!(outcome.starts_with("copied"), "{outcome}");
        // The source was measured by the probe script, running here.
        let total = seen
            .iter()
            .find_map(|(_, total)| *total)
            .expect("no total: the size probe produced nothing");
        assert!(total >= 8192, "{total} is less than the source");
        // Progress only rises, and reaches what landed. Which sample lands
        // first is a race with the copy's own first write and is not a
        // property to assert — that a copy opens at zero is
        // `what_was_already_at_the_destination_is_not_progress`'s to say,
        // where the destination is pre-filled and the fixture waits.
        assert!(
            seen.windows(2).all(|w| w[1].0 >= w[0].0),
            "progress went backwards: {seen:?}"
        );
        assert!(
            seen.iter().any(|(done, _)| *done >= 4096),
            "nothing was ever reported as moved: {seen:?}"
        );
        assert!(
            seen.last().is_some_and(|(done, _)| *done <= total),
            "more was reported than the source holds: {seen:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn what_was_already_at_the_destination_is_not_progress() {
        let dir = scratch("baseline");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        // Yesterday's copy, sixteen times the size of what is about to be
        // added to it — an upload merging into a directory behaves this way,
        // and absolute samples would call it 1600% done.
        let dest = dir.join("dest.bin");
        std::fs::write(&dest, vec![1u8; 65536]).unwrap();
        // The trailing rest is not padding: there is no closing measurement,
        // so the last sample is whatever the timer caught, and a copy that
        // ends the instant it writes leaves the last write unmeasured.
        // The quiet at the front is what makes the first assertion below a
        // property rather than a race: the destination is pre-filled, so a
        // sample taken before anything is appended must read zero moved.
        let cp = format!(
            "sleep 0.6; i=0; while [ $i -lt 4 ]; do printf '%1024s' '' >> '{}'; i=$((i+1)); sleep 0.1; done; sleep 0.4",
            dest.display()
        );
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        run_transfer(local_job(&cp, false, &src, &dest, tx)).await;
        let (seen, outcome) = drain(rx).await;

        assert!(outcome.starts_with("copied"), "{outcome}");
        let moved: Vec<u64> = seen.iter().map(|(done, _)| *done).collect();
        assert_eq!(moved[0], 0, "the copy opened part-done: {moved:?}");
        assert!(
            moved.iter().all(|done| *done <= 4096),
            "the file that was already there was counted as progress: {moved:?}"
        );
        assert_eq!(
            moved.last(),
            Some(&4096),
            "the bytes that were added were not all counted: {moved:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_copy_that_cannot_be_watched_draws_no_bar_at_all() {
        // The watcher could not start — denied, no shell, a pod that went
        // away. Without a word from the sampler the row would render an
        // empty bar and "(0%)…" for the whole copy, which is worse than the
        // line it replaced; and with nothing to measure against, the source
        // is not sized either.
        let dir = scratch("unwatched");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        let dest = dir.join("dest.bin");
        let cp = format!("sleep 0.2; printf '%2048s' '' > '{}'", dest.display());
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut job = local_job(&cp, true, &src, &dest, tx);
        // An upload, so the sampler is the watcher — pointed at a binary
        // that does not exist.
        job.exec = vec!["sofka-no-such-binary".to_string()];
        run_transfer(job).await;
        let (seen, outcome) = drain(rx).await;

        assert!(outcome.starts_with("copied"), "{outcome}");
        assert!(
            seen.iter().all(|(_, total)| total.is_none()),
            "a bar was drawn against a destination nothing can measure: {seen:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_bar_that_was_drawn_and_then_cannot_be_kept_is_taken_away() {
        // The sampler stops mid-copy — the watcher's exec dies, the pod
        // restarts. The bar has a total by then, so leaving it alone would
        // freeze it at the fraction it had reached, which reads as a copy
        // that stopped there. The last word is that there is no bar.
        let dir = scratch("collapse-run");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 8192]).unwrap();
        let dest = dir.join("dest.bin");
        let cp = format!("sleep 1.2; printf '%2048s' '' > '{}'", dest.display());
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut job = local_job(&cp, true, &src, &dest, tx);
        // A watcher that answers twice and then goes away: the first is the
        // baseline, the second moves the bar, and the end of its output is
        // what the sampler has to notice.
        job.exec = vec![
            "sh".into(),
            "-c".into(),
            "printf 'sofka-size:1:0\n'; sleep 0.4; printf 'sofka-size:1:2048\n'; sleep 0.2".into(),
        ];
        run_transfer(job).await;
        let (seen, outcome) = drain(rx).await;

        assert!(outcome.starts_with("copied"), "{outcome}");
        assert!(
            seen.iter().any(|(_, total)| total.is_some()),
            "the bar was never drawn, so this proves nothing: {seen:?}"
        );
        assert_eq!(
            seen.last().map(|(_, total)| *total),
            Some(None),
            "the bar was left frozen instead of taken away: {seen:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_run_does_not_end_before_it_has_closed_its_watcher() {
        // `reap` waits on a watcher that has not been told to stop, and the
        // real script only stops when its stdin closes. A run that reaped
        // before closing would sit here for `WATCH_STOP`.
        let dir = scratch("closed");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        let dest = dir.join("dest.bin");
        let cp = format!("sleep 0.2; printf '%2048s' '' > '{}'", dest.display());
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        // An upload with an empty exec prefix: the real watcher script runs
        // here, and runs until something closes its stdin.
        let job = local_job(&cp, true, &src, &dest, tx);
        tokio::time::timeout(Duration::from_secs(15), run_transfer(job))
            .await
            .expect("the run never returned: its watcher was not closed");
        let (_, outcome) = drain(rx).await;
        assert!(outcome.starts_with("copied"), "{outcome}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_upload_measures_its_destination_before_the_copy_touches_it() {
        // Through the real watcher script, against a destination that
        // already holds something: without this the copy would be measured
        // from whatever was there and open near its own total.
        let dir = scratch("remote-baseline");
        let dest = dir.join("dest.bin");
        std::fs::write(&dest, vec![1u8; 65536]).unwrap();
        let mut sampler = Sampler::start(true, &[], &dest.display().to_string()).await;
        let baseline = tokio::time::timeout(Duration::from_secs(10), sampler.baseline())
            .await
            .expect("the baseline never arrived");
        assert!(
            baseline.is_some_and(|held| held >= 65536),
            "{baseline:?} does not measure what was already there"
        );
        sampler.close();
        sampler.reap().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_destination_too_big_to_measure_collapses_the_bar() {
        // The other way to stop having a number: the destination outgrows
        // the entry cap mid-copy. Same answer — no bar rather than one
        // frozen at whatever fraction it had reached.
        let dir = scratch("collapse");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        for i in 0..8 {
            std::fs::write(dest.join(format!("f{i}")), b"x").unwrap();
        }
        let cp = format!("sleep 0.5; printf '%2048s' '' > '{}/new'", dest.display());
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut job = local_job(&cp, false, &src, &dest, tx);
        job.dest = dest.display().to_string();
        // Run it with a cap the destination is already over.
        let mut sampler = Sampler::Local {
            path: dest.clone(),
            tick: tokio::time::interval(Duration::from_millis(10)),
            owed: Duration::ZERO,
            cap: 3,
            limit: WALK_LIMIT,
        };
        assert_eq!(sampler.sample().await, None, "an over-cap walk gave a size");
        assert!(matches!(sampler, Sampler::Idle), "the sampler kept walking");
        drop(job);
        let _ = drain(rx).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_destination_too_slow_to_walk_loses_its_bar_too() {
        // The other way a destination stops being measurable. Left alone,
        // a walk that cannot finish would be started again every ceiling,
        // and the bar would sit at the fraction it reached.
        let dir = scratch("slow");
        // Enough entries that the walk is still running when its ceiling is
        // checked; a handful of files can finish before the first poll.
        for i in 0..3_000 {
            std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        let mut sampler = Sampler::Local {
            path: dir.clone(),
            tick: tokio::time::interval(Duration::from_millis(1)),
            owed: Duration::ZERO,
            cap: pvc::MAX_SIZE_ENTRIES,
            // No walk of three thousand entries finishes in a nanosecond.
            limit: Duration::from_nanos(1),
        };
        assert_eq!(sampler.sample().await, None);
        assert!(
            matches!(sampler, Sampler::Idle),
            "a walk that cannot finish was left to be started again"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_walk_that_cost_more_than_the_interval_is_slept_off_first() {
        // The only thing keeping a large destination from being walked
        // back to back against the disk `tar` is writing to.
        let dir = scratch("owed");
        let mut sampler = Sampler::Local {
            path: dir.clone(),
            tick: tokio::time::interval(Duration::from_millis(1)),
            owed: Duration::from_millis(300),
            cap: pvc::MAX_SIZE_ENTRIES,
            limit: WALK_LIMIT,
        };
        let started = std::time::Instant::now();
        assert!(sampler.sample().await.is_some());
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "the rest owed was skipped: {:?}",
            started.elapsed()
        );
        let Sampler::Local { owed, .. } = &sampler else {
            unreachable!()
        };
        // What is owed now is what this walk earned, not what it was
        // handed: a debt that outlived its payment would compound.
        assert!(
            *owed > Duration::ZERO && *owed < Duration::from_millis(300),
            "the rest owed is not this walk's own: {owed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_destination_that_shrinks_does_not_walk_the_bar_backwards() {
        // A destination is walked while it is being written, so a sample can
        // land mid-rename or mid-truncate and read short. The bar is a
        // report on a copy, not on the filesystem: it does not retreat.
        let dir = scratch("shrink");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 8192]).unwrap();
        let dest = dir.join("dest.bin");
        // Each state is held for several sample periods: the sampler runs
        // on a timer, and a window it can miss makes the test a coin toss
        // rather than a check.
        let cp = format!(
            "printf '%4096s' '' > '{0}'; sleep 1.2; printf '%1s' '' > '{0}'; sleep 1.2",
            dest.display()
        );
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        run_transfer(local_job(&cp, false, &src, &dest, tx)).await;
        let (seen, outcome) = drain(rx).await;
        assert!(outcome.starts_with("copied"), "{outcome}");
        let moved: Vec<u64> = seen.iter().map(|(done, _)| *done).collect();
        assert!(
            moved.iter().any(|done| *done >= 4096),
            "the first write was never measured: {moved:?}"
        );
        assert!(
            moved.windows(2).all(|w| w[1] >= w[0]),
            "the bar went backwards when the file shrank: {moved:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_watcher_script_samples_a_growing_destination_and_stops_on_close() {
        let dir = scratch("watch");
        let dest = dir.join("grows.bin");
        std::fs::write(&dest, vec![0u8; 4096]).unwrap();
        // The real `size_watch` script, run by a real `sh` — the exec prefix
        // is empty, so this is everything the pod side does but the pod.
        let mut sampler = Sampler::start(true, &[], &dest.display().to_string()).await;
        assert!(matches!(sampler, Sampler::Remote(_)), "no watcher started");

        let first = tokio::time::timeout(Duration::from_secs(10), sampler.sample())
            .await
            .expect("no sample arrived")
            .expect("the sample carried no size");
        assert!(first >= 4096, "{first} does not measure the destination");
        std::fs::write(&dest, vec![0u8; 4096 * 8]).unwrap();
        let grown = tokio::time::timeout(Duration::from_secs(10), sampler.sample())
            .await
            .expect("the watcher stopped sampling")
            .expect("the sample carried no size");
        assert!(
            grown > first,
            "{grown} did not follow the file: was {first}"
        );

        // Closing stdin is what ends the loop; nothing else can reach it.
        sampler.close();
        tokio::time::timeout(Duration::from_secs(10), sampler.reap())
            .await
            .expect("the watcher outlived its close");
        let Sampler::Remote(watcher) = &mut sampler else {
            unreachable!()
        };
        assert!(
            watcher.child.try_wait().unwrap().is_some(),
            "the watcher is still running after its stdin closed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_size_already_known_skips_the_probe_but_not_the_zero_check() {
        // A listing gave the size, so nothing runs. An unreachable exec
        // prefix proves it: a probe would fail against it.
        let nowhere = vec!["definitely-not-a-binary".to_string()];
        assert_eq!(
            resolve_total(&nowhere, false, "/srv/x", Some(4096)).await,
            Some(4096)
        );
        // Except zero, which is not a total any bar can be drawn against —
        // whether a listing said so or a walk of an empty file did.
        assert_eq!(
            resolve_total(&nowhere, false, "/srv/x", Some(0)).await,
            None
        );
        let dir = scratch("empty");
        let empty = dir.join("nothing");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            resolve_total(&[], true, &empty.display().to_string(), None).await,
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn progress_is_measured_from_what_was_already_there() {
        let step = |baseline: &mut Option<u64>, sampled| moved(sampled, baseline);

        // A destination that did not exist: everything that lands is progress.
        let mut fresh = None;
        assert_eq!(step(&mut fresh, 0), 0);
        assert_eq!(step(&mut fresh, 4096), 4096);

        // One file being overwritten: `cp` truncates it, the baseline goes
        // with it, and the new copy is measured from zero — including past
        // the size of the file it replaced.
        let mut overwrite = None;
        assert_eq!(step(&mut overwrite, 5_000), 0);
        assert_eq!(step(&mut overwrite, 0), 0);
        assert_eq!(step(&mut overwrite, 2_000), 2_000);
        assert_eq!(step(&mut overwrite, 9_000), 9_000);

        // A directory being re-copied only ever dips by the file `tar` is
        // replacing at that moment. The baseline follows the dip and no
        // further: handing over the whole existing tree at the first
        // truncation would read as ~100% for the rest of the copy.
        let mut retree = None;
        assert_eq!(step(&mut retree, 10_000), 0);
        assert_eq!(step(&mut retree, 9_900), 0);
        assert_eq!(step(&mut retree, 10_400), 500);
        assert_eq!(step(&mut retree, 9_800), 0);
        assert_eq!(step(&mut retree, 10_600), 800);
    }

    #[tokio::test]
    async fn a_destination_too_big_to_measure_stops_being_measured() {
        // Past the entry cap a walk can only ever fail, and it is the most
        // expensive failure in the module — so the sampler retires. Driven
        // through a cap of 3 rather than a tree of 200,000 files.
        let dir = scratch("cap");
        for i in 0..8 {
            std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        let go = AtomicBool::new(false);
        assert_eq!(pvc::local_size_capped(&dir, 3, &go), pvc::Measure::TooBig);

        let mut sampler = Sampler::Local {
            path: dir.clone(),
            tick: tokio::time::interval(Duration::from_millis(1)),
            owed: Duration::ZERO,
            cap: 3,
            limit: WALK_LIMIT,
        };
        assert_eq!(sampler.sample().await, None);
        assert!(
            matches!(sampler, Sampler::Idle),
            "a tree that will never fit kept being walked"
        );

        // A path that is not there is the other empty answer, and it must
        // not retire anything: `cp` has simply not created it yet.
        let mut waiting = Sampler::Local {
            path: dir.join("not-yet"),
            tick: tokio::time::interval(Duration::from_millis(1)),
            owed: Duration::ZERO,
            cap: pvc::MAX_SIZE_ENTRIES,
            limit: WALK_LIMIT,
        };
        assert_eq!(waiting.sample().await, None);
        assert!(
            matches!(waiting, Sampler::Local { .. }),
            "the sampler gave up on a destination that was merely late"
        );
        // And it is a baseline of zero, not an unknown one: this copy is
        // about to create it, so everything that lands is progress.
        assert_eq!(waiting.baseline().await, Some(0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn nothing_is_reported_after_the_result() {
        // `store.rs` promises a copy's progress never follows its result,
        // which is what lets `end_transfer` drop the row on `TransferDone`
        // without racing a sample that is still on its way.
        let dir = scratch("order");
        let src = dir.join("src.bin");
        std::fs::write(&src, vec![7u8; 4096]).unwrap();
        let dest = dir.join("dest.bin");
        let cp = format!(
            "i=0; while [ $i -lt 3 ]; do printf '%1024s' '' >> '{}'; i=$((i+1)); sleep 0.1; done",
            dest.display()
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        run_transfer(local_job(&cp, false, &src, &dest, tx)).await;

        let mut ended = false;
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::TransferDone { .. } => ended = true,
                Msg::TransferProgress { .. } => {
                    assert!(!ended, "progress arrived after the result");
                }
                _ => {}
            }
        }
        assert!(ended, "no result was ever sent");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_slow_walk_earns_a_rest_three_times_its_own_length() {
        // The accrual, not the payment: without it a large destination is
        // walked back to back against the disk `tar` is writing to.
        assert_eq!(
            rest_after(Duration::from_millis(200)),
            Duration::from_millis(600)
        );
        assert_eq!(rest_after(Duration::ZERO), Duration::ZERO);
        // And it cannot outrun the ceiling on a walk by more than that.
        assert_eq!(rest_after(WALK_LIMIT), WALK_LIMIT * 3);
    }

    #[test]
    fn a_walk_that_nobody_is_waiting_for_is_told_to_stop() {
        // The producer's half: a `spawn_blocking` walk cannot be cancelled,
        // so the flag is the only thing that reaches it — and it has to be
        // raised however the future ends, not only when it returns.
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _raised = Stop(flag.clone());
            assert!(!flag.load(Ordering::Relaxed));
        }
        assert!(
            flag.load(Ordering::Relaxed),
            "an abandoned walk was never told to stop"
        );
    }

    #[test]
    fn a_script_argv_keeps_the_path_out_of_the_script() {
        let exec = ["kubectl", "exec", "-i", "--"].map(String::from).to_vec();
        let argv = size_argv(&exec, "echo hi", "/pvc/a b'c\n");
        assert_eq!(&argv[..4], exec.as_slice());
        // `sh -c <script> sh <path>`: the path is $1, never text in $0's
        // script, so no name on a volume can close a quote in it.
        assert_eq!(&argv[4..7], ["sh", "-c", "echo hi"]);
        assert_eq!(argv[7], "sh");
        assert_eq!(argv[8], "/pvc/a b'c\n");
    }

    fn status() -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            std::os::unix::process::ExitStatusExt::from_raw(1 << 8)
        }
        #[cfg(windows)]
        {
            std::os::windows::process::ExitStatusExt::from_raw(1)
        }
    }

    #[test]
    fn a_copy_failure_reports_the_cause_not_kubectls_trailer() {
        // What kubectl actually prints when tar cannot write: the reason
        // first, then its own generic trailer.
        let stderr = "tar: can't open '/pvc/x': Read-only file system\n\
                      command terminated with exit code 2\n";
        assert_eq!(
            cp_error(stderr, &status()),
            "tar: can't open '/pvc/x': Read-only file system"
        );
        // The trailer alone is all there is: better than nothing.
        assert_eq!(
            cp_error("command terminated with exit code 2\n", &status()),
            "command terminated with exit code 2"
        );
        // Nothing at all — the exit status is the only fact available.
        assert!(cp_error("  \n\n", &status()).contains("exited with"));
    }
}
