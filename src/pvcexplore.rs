//! PVC explore — reading and transferring the contents of a
//! PersistentVolumeClaim.
//!
//! A PVC has no API of its own to read: the only way to see what is on a
//! volume is from inside a pod that mounts it. This module is the pure half of
//! that — picking the pod, describing the helper pod for a claim nothing
//! mounts, parsing the directory listing that comes back, and reading the
//! local side of the split view. Everything that talks to a cluster or the UI
//! lives in `app/pvcexplore.rs`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use kube::core::DynamicObject;
use serde_json::{Value, json};

/// Where the helper pod mounts the claim. Fixed rather than configurable: it
/// only ever exists inside a pod sofka created, and a knob here would only
/// change a path nobody types.
pub const HELPER_MOUNT: &str = "/pvc";

/// `metadata.generateName` for helper pods. `:pvc-clean` requires this prefix,
/// both of [`HELPER_LABELS`], and the [`HELPER_ANNOTATION`] naming the claim
/// before it deletes anything.
///
/// None of that is unforgeable — every label and annotation sofka writes on
/// creation, anything else can write too — so it is not a permission check.
/// It is there to make an *accidental* match essentially impossible; a
/// deliberate one is bounded instead by the sweep being confirmed, journalled,
/// blocked in read-only mode, and gated by the `pvc-explore` guardrail.
pub const HELPER_PREFIX: &str = "sofka-pvc-explore-";

/// Label every helper pod carries, so a leftover is identifiable as sofka's
/// even after the annotation naming the claim is gone.
pub const HELPER_LABELS: [(&str, &str); 2] = [
    ("app.kubernetes.io/managed-by", "sofka"),
    ("sofka.dev/component", "pvc-explore"),
];

/// Annotation naming the claim a helper pod was created for. Also part of the
/// evidence [`HELPER_PREFIX`] describes.
pub const HELPER_ANNOTATION: &str = "sofka.dev/pvc";

/// The label selector `:pvc-clean` lists with.
pub fn helper_selector() -> String {
    HELPER_LABELS
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Cap on entries read from one directory. A volume with a million files in
/// one directory is a real thing (a cache, a spool), and the cap is applied by
/// `head` inside the container rather than after the fact, so neither the pipe
/// nor this process ever holds the whole listing.
pub const MAX_ENTRIES: usize = 5_000;

/// Prefix of the line `ls`'s own exit status is reported on, since piping
/// through `head` replaces the pipeline's status with `head`'s.
///
/// The prefix alone is forgeable: GNU `ls` prints file names raw when its
/// output is a pipe, so a file called `x\nsofka-ls-status:0` puts a line
/// through that looks exactly like the real marker — and, arriving before the
/// real one, would let a truncated listing pass itself off as complete. Every
/// run therefore mints a nonce (see [`ListingProbe`]) that a name on the
/// volume cannot predict.
const STATUS_MARKER: &str = "sofka-ls-status:";

/// Exit code the listing script uses for a path it could not enter.
pub const EXIT_NOT_A_DIRECTORY: i32 = 3;

/// Exit code the listing script uses for a path that resolved outside the
/// mount. Only a symlink can do that — the browser never builds such a path
/// itself — but a volume's contents are not sofka's to trust.
pub const EXIT_OUTSIDE_MOUNT: i32 = 4;

/// One listing command and the nonce needed to read its output back.
#[derive(Debug, Clone)]
pub struct ListingProbe {
    pub script: String,
    pub nonce: String,
    /// The mount root the listing may not leave, passed as `$2`. The script
    /// resolves it itself, so it is the raw mount path.
    pub root: String,
}

/// A value a file name on the volume cannot guess: the nanosecond clock plus a
/// per-process counter, hex-encoded so it is safe to embed in the script
/// unquoted. It never leaves the exec, so it needs to be unpredictable, not
/// cryptographically random.
fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    format!("{t:016x}{n:x}")
}

/// The listing command, run as `sh -c <script> sh <path>` so the path arrives
/// as a positional parameter and is never spliced into the script.
///
/// `-A` includes dotfiles but not `.`/`..`; `-l` is the only long format both
/// GNU coreutils and busybox agree on. `LC_ALL=C` pins the month names — not
/// that [`parse_listing`] reads them, but a stable width is one less thing to
/// go wrong.
///
/// The `unset` line matters more than it looks: GNU `ls` reshapes its output
/// from the environment, and the container's environment is not sofka's to
/// choose. `TIME_STYLE=long-iso` prints a two-field date instead of three and
/// shifts the name column, leaving nothing parseable; `QUOTING_STYLE=shell`
/// wraps every name in quotes, so each one lists but none of them resolves;
/// `BLOCK_SIZE`/`LS_BLOCK_SIZE` scale the size column, which would report a
/// 100 kB file as `1B`. busybox ignores all four, and `unset` on an unset name
/// is free, so this is unconditional.
///
/// A failed `cd` exits 3 so the caller can say "not a directory" rather than
/// surface a shell error.
///
/// The cap is applied by `head` inside the container, so a spool directory
/// with a million files is never streamed out in full. That costs the
/// pipeline's exit status — it becomes `head`'s — which matters because `ls`
/// distinguishes three outcomes the pane must not confuse: everything listed
/// (0), listed but some entry could not be stat'd (non-zero, output still
/// good), and could not read the directory at all (non-zero, no output).
/// Hence the trailing marker: it carries `ls`'s real status, and its *absence*
/// means `head` cut the output short, which is exactly the truncation signal.
/// `$1` is the directory to list and `$2` the mount root it may not leave;
/// both arrive as positional parameters, never spliced into the script.
///
/// The `pwd -P` check is what makes "confined to the mount" true rather than
/// aspirational. Path arithmetic alone cannot enforce it: `cd` follows
/// symlinks, so a link on the volume pointing at `/` would land the browser in
/// the serving pod's root with every path still looking like it was under the
/// mount. Comparing the *resolved* directory is the only check that sees it.
///
/// Both sides get a trailing slash before they are compared, which makes the
/// boundary explicit — `/pvcx` is not inside `/pvc` — and keeps a root of `/`
/// working without depending on a glob subtlety to say so.
pub fn list_probe(root: &str) -> ListingProbe {
    let nonce = nonce();
    ListingProbe {
        // Both sides are resolved with `pwd -P` before they are compared: a
        // mount path can itself sit behind a symlink (as `/tmp` does on
        // macOS), and comparing the raw strings would then refuse the mount's
        // own root. Trimming a trailing slash keeps the `"$root"/*` pattern
        // meaningful when the root is `/` — not a legal mountPath, but the
        // script must not misbehave if one ever arrives.
        script: format!(
            r#"[ -n "$1" ] && [ -n "$2" ] || exit {EXIT_NOT_A_DIRECTORY}
unset TIME_STYLE QUOTING_STYLE BLOCK_SIZE LS_BLOCK_SIZE
for tool in ls head; do
    command -v "$tool" >/dev/null 2>&1 || {{ echo "missing browsing tool: $tool" >&2; exit 127; }}
done
root=$(cd -- "$2" 2>/dev/null && pwd -P) || exit {EXIT_NOT_A_DIRECTORY}
cd -- "$1" 2>/dev/null || exit {EXIT_NOT_A_DIRECTORY}
case "$(pwd -P)/" in "${{root%/}}/"*) ;; *) exit {EXIT_OUTSIDE_MOUNT} ;; esac
{{ LC_ALL=C ls -A -l; echo "{STATUS_MARKER}{nonce}:$?"; }} | head -n {}"#,
            MAX_ENTRIES + 2
        ),
        nonce,
        root: root.to_string(),
    }
}

/// What one [`list_probe`] run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Body lines that produced no entry. A few are ordinary — a forged row,
    /// a device node in an unfamiliar shape. *All* of them means the output
    /// was not the format we parse, and calling that an empty directory would
    /// tell the user their volume holds nothing.
    pub unparsed: usize,
    /// Rows `ls` produced with no name of their own. A file named `"\nfoo"`
    /// prints its metadata, then the newline ends the row before the name
    /// starts — so this row is the *head*, and `foo` arrives on the next line
    /// as an unparseable one. They are real files sofka cannot name, which is
    /// neither an unreadable listing nor an empty directory.
    pub unnameable: usize,
    /// `head` cut the output short: there are more entries than [`MAX_ENTRIES`].
    pub truncated: bool,
    /// `ls`'s own exit status. `None` when the marker never arrived — either
    /// the output was truncated, or the command never ran at all.
    pub status: Option<i32>,
}

/// Turn one run of [`list_probe`] into either a listing or the reason there
/// isn't one. Pure, so every branch is testable without a cluster: `exit_code`
/// is the process's, `stdout`/`stderr` its output.
pub fn interpret_listing(
    nonce: &str,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<(Listing, Option<String>), String> {
    if exit_code == Some(EXIT_NOT_A_DIRECTORY) {
        return Err("not a directory, or permission denied".into());
    }
    if exit_code == Some(EXIT_OUTSIDE_MOUNT) {
        return Err(
            "that link points outside the volume — the browser stays inside the mount".into(),
        );
    }
    let listing = parse_output(nonce, stdout);
    let message = last_error_line(stderr);
    let fail = |m: String| {
        Err(if m.is_empty() {
            match exit_code {
                Some(c) => format!("listing failed (exit {c})"),
                None => "listing failed".into(),
            }
        } else {
            m
        })
    };
    match listing.status {
        // Truncated: `ls` was still producing output when `head` closed the
        // pipe, so it plainly read the directory — but "plenty of output, none
        // of it parseable" is still unreadable, however much of it there was.
        None if listing.truncated && !listing.entries.is_empty() => Ok((listing, None)),
        None if listing.truncated => Err(unreadable(listing.unparsed)),
        // No marker and no truncation: the command never got as far as
        // reporting a status — a missing shell, a pod that isn't running, a
        // denied exec. Whatever stderr says is the real answer.
        None => fail(message),
        // `ls` succeeded and had something to say, but none of it parsed:
        // some other `ls`, or one whose columns the environment reshaped.
        // Files whose names contain a newline: they are there, and no path
        // sofka builds could reach them. Saying so beats an empty pane — and
        // beats "unreadable output", because each such name also leaves its
        // remainder behind as an unparseable line. That accounting is what
        // tells the two apart: at most one leftover per unnameable row means
        // these are newlines in names, more than that means the format itself
        // is one we do not read.
        Some(0) if listing.unnameable > 0 && listing.unparsed <= listing.unnameable => {
            let n = listing.unnameable;
            Ok((listing, Some(unnameable_note(n))))
        }
        Some(0) if listing.entries.is_empty() && listing.unparsed > 0 => {
            Err(unreadable(listing.unparsed))
        }
        Some(0) => Ok((listing, None)),
        // `ls` failed but still listed entries: it could not stat some of
        // them. Show what there is and say why it is incomplete.
        Some(_) if !listing.entries.is_empty() => {
            Ok((listing, (!message.is_empty()).then_some(message)))
        }
        // `ls` failed with nothing to show — an unreadable directory. This is
        // the case that must never render as "empty".
        Some(_) => fail(message),
    }
}

fn unnameable_note(n: usize) -> String {
    format!(
        "{n} entr{} here contain a newline in the name and cannot be opened or copied",
        if n == 1 { "y" } else { "ies" }
    )
}

fn unreadable(lines: usize) -> String {
    format!("could not read this listing — {lines} lines of unexpected `ls` output")
}

/// The last line of stderr that says something. The real cause comes before
/// kubectl's generic "command terminated with exit code" trailer.
fn last_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .rev()
        .find(|l| !l.is_empty() && !l.starts_with("command terminated"))
        .unwrap_or_default()
        .to_string()
}

fn parse_output(nonce: &str, stdout: &str) -> Listing {
    let marker = format!("{STATUS_MARKER}{nonce}:");
    let mut status = None;
    let mut body = String::with_capacity(stdout.len());
    let mut lines = 0usize;
    for line in stdout.lines() {
        lines += 1;
        match line.strip_prefix(marker.as_str()) {
            Some(code) => status = Some(code.trim().parse().unwrap_or(-1)),
            None => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    // Counted in *lines*, not entries: a file name containing a newline is
    // several lines and one entry (or none), so counting entries would call a
    // truncated listing complete. `head` emits at most MAX_ENTRIES + 2, so
    // reaching that with no marker means it cut the output short.
    let truncated = status.is_none() && lines >= MAX_ENTRIES + 2;
    let (mut entries, unparsed, unnameable) = parse_body(&body);
    entries.truncate(MAX_ENTRIES);
    Listing {
        entries,
        unparsed,
        unnameable,
        truncated,
        status,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// A symlink. Kept distinct from the two above because `ls -l` reports the
    /// link's own size and type, not the target's — descending into one is
    /// something we try, not something we can promise.
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// `None` when nothing could stat the entry — distinct from an empty
    /// file, which the pane would otherwise render identically.
    pub size: Option<u64>,
    /// The `-> target` tail of a symlink line, for display. Empty otherwise.
    pub link_target: String,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    /// Whether the name survived the round trip intact. `ls` output is decoded
    /// lossily, so a name that is not valid UTF-8 comes back with replacement
    /// characters — it lists, but no path built from it would resolve, so
    /// descending into it or copying it can only fail confusingly.
    pub fn addressable(&self) -> bool {
        !self.name.contains('\u{FFFD}')
    }
}

/// Whether `ls` could plausibly have produced this name. A directory entry can
/// never contain `/` and is never `.` or `..`, so anything that does is a
/// forgery: GNU `ls` writes names raw into a pipe, which lets a file called
/// `x\ndrwxr-xr-x 2 root root 4096 Jan 1 00:00 ..` inject a whole extra row —
/// and a symlink *target* is arbitrary bytes, so it can carry an absolute
/// path. Both would otherwise escape the mount on `enter` and, on download,
/// escape the destination directory (`Path::join` with an absolute path
/// replaces rather than appends).
fn plausible_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && name != "." && name != ".."
}

/// The container to exec into, and where the claim is mounted inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub pod: String,
    pub container: String,
    pub path: String,
    /// None means that subPathExpr cannot be resolved from the pod specification.
    pub sub_path: Option<String>,
    /// The mount is `readOnly` in the pod spec: writes will fail, so an upload
    /// is refused up front instead of failing halfway through a `kubectl cp`.
    pub read_only: bool,
    /// sofka created this pod and owns deleting it.
    pub helper: bool,
}

/// Pick the pod to browse `claim` through: a running pod that already mounts
/// it. Prefers a writable mount over a read-only one, so uploads work whenever
/// any consumer could do them at all.
///
/// `None` means nothing running mounts the claim — the caller's cue to offer a
/// helper pod ([`helper_pod`]).
pub fn find_mount(pods: &[DynamicObject], claim: &str) -> Option<Mount> {
    find_mounts(pods, claim).into_iter().next()
}

pub fn find_mounts(pods: &[DynamicObject], claim: &str) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for pod in pods {
        if phase(pod) == "Running" && pod.metadata.deletion_timestamp.is_none() {
            mounts.extend(mounts_in(pod, claim));
        }
    }
    mounts.sort_by_key(|m| m.read_only);
    mounts
}

/// Names of the pod's containers that are in the `running` state right now,
/// across all three kinds. Anything else cannot be exec'd into.
fn running_containers(pod: &DynamicObject) -> std::collections::HashSet<&str> {
    let Some(status) = pod.data.get("status") else {
        return std::collections::HashSet::new();
    };
    [
        "containerStatuses",
        "initContainerStatuses",
        "ephemeralContainerStatuses",
    ]
    .iter()
    .filter_map(|k| status.get(k))
    .filter_map(Value::as_array)
    .flatten()
    .filter(|c| c.get("state").is_some_and(|s| s.get("running").is_some()))
    .filter_map(|c| c.get("name").and_then(Value::as_str))
    .collect()
}

fn phase(pod: &DynamicObject) -> &str {
    pod.data
        .get("status")
        .and_then(|s| s.get("phase"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn mounts_in(pod: &DynamicObject, claim: &str) -> Vec<Mount> {
    let Some(spec) = pod.data.get("spec") else {
        return Vec::new();
    };
    let volumes: Vec<_> = spec
        .get("volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|v| {
            v.pointer("/persistentVolumeClaim/claimName")
                .and_then(Value::as_str)
                == Some(claim)
        })
        .collect();
    let running = running_containers(pod);
    let mut mounts = Vec::new();
    for key in ["containers", "initContainers", "ephemeralContainers"] {
        for c in spec
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = c.get("name").and_then(Value::as_str) else {
                continue;
            };
            if !running.contains(name)
                || (key == "initContainers"
                    && c.get("restartPolicy").and_then(Value::as_str) != Some("Always"))
            {
                continue;
            }
            for m in c
                .get("volumeMounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(volume) = volumes.iter().find(|v| v.get("name") == m.get("name")) else {
                    continue;
                };
                let Some(path) = m.get("mountPath").and_then(Value::as_str) else {
                    continue;
                };
                mounts.push(Mount {
                    pod: pod.metadata.name.clone().unwrap_or_default(),
                    container: name.to_owned(),
                    path: path.to_owned(),
                    sub_path: if m
                        .get("subPathExpr")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
                    {
                        None
                    } else {
                        Some(
                            m.get("subPath")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        )
                    },
                    read_only: m.get("readOnly").and_then(Value::as_bool).unwrap_or(false)
                        || volume
                            .pointer("/persistentVolumeClaim/readOnly")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    helper: false,
                });
            }
        }
    }
    mounts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperOptions {
    pub node: Option<String>,
    pub sub_path: String,
    pub read_only: bool,
}

#[derive(Debug)]
pub struct RecoveryPlan {
    pub candidates: Vec<Mount>,
    pub helper: Result<HelperOptions, String>,
}

pub fn recovery_plan(
    pods: &[DynamicObject],
    claim: &DynamicObject,
    original: &Mount,
) -> RecoveryPlan {
    let name = claim.metadata.name.as_deref().unwrap_or_default();
    let candidates = find_mounts(pods, name)
        .into_iter()
        .filter(|m| {
            (m.pod != original.pod || m.container != original.container || m.path != original.path)
                && original.sub_path.is_some()
                && m.sub_path == original.sub_path
        })
        .take(16)
        .map(|mut m| {
            m.read_only |= original.read_only;
            m
        })
        .collect();
    RecoveryPlan {
        candidates,
        helper: helper_options(pods, claim, original),
    }
}

pub fn helper_options(
    pods: &[DynamicObject],
    claim: &DynamicObject,
    original: &Mount,
) -> Result<HelperOptions, String> {
    let Some(sub_path) = &original.sub_path else {
        return Err("Cannot recover a subPathExpr mount without changing its boundaries.".into());
    };
    let name = claim.metadata.name.as_deref().unwrap_or_default();
    let consumers: Vec<_> = pods
        .iter()
        .filter(|p| !matches!(phase(p), "Succeeded" | "Failed"))
        .filter(|p| {
            p.data
                .pointer("/spec/volumes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|v| {
                    v.pointer("/persistentVolumeClaim/claimName")
                        .and_then(Value::as_str)
                        == Some(name)
                })
        })
        .collect();
    let modes: Vec<_> = claim
        .data
        .pointer("/spec/accessModes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if modes.is_empty() {
        return Err("Cannot determine the volume access modes. No helper was created.".into());
    }
    if modes.contains(&"ReadWriteOncePod") && !consumers.is_empty() {
        return Err(
            "ReadWriteOncePod is already in use. A second pod cannot mount this claim.".into(),
        );
    }
    let mut node = None;
    if modes.contains(&"ReadWriteOnce") && !consumers.is_empty() {
        let nodes: std::collections::BTreeSet<_> = consumers
            .iter()
            .filter_map(|p| p.data.pointer("/spec/nodeName").and_then(Value::as_str))
            .filter(|n| !n.is_empty())
            .collect();
        if nodes.len() != 1 {
            return Err("Cannot select one consumer node for this ReadWriteOnce claim.".into());
        }
        node = nodes.first().map(|s| (*s).to_owned());
    }
    Ok(HelperOptions {
        node,
        sub_path: sub_path.clone(),
        read_only: original.read_only || modes == ["ReadOnlyMany"],
    })
}

pub fn apply_helper_options(manifest: &mut Value, options: &HelperOptions) {
    if let Some(node) = &options.node {
        manifest["spec"]["affinity"] = json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {
            "nodeSelectorTerms": [{"matchFields": [{"key":"metadata.name", "operator":"In", "values":[node]}]}]
        }}});
    }
    manifest["spec"]["containers"][0]["volumeMounts"][0]["readOnly"] = json!(options.read_only);
    manifest["spec"]["volumes"][0]["persistentVolumeClaim"]["readOnly"] = json!(options.read_only);
    if !options.sub_path.is_empty() {
        manifest["spec"]["containers"][0]["volumeMounts"][0]["subPath"] = json!(options.sub_path);
    }
}

pub fn missing_listing_tools(error: &str) -> bool {
    error.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        (["sh", "/bin/sh"]
            .iter()
            .any(|tool| line.contains(&format!("exec: \"{tool}\"")))
            && (line.contains("executable file not found")
                || line.contains("no such file or directory")))
            || line.starts_with("missing browsing tool: ")
    })
}

/// The helper pod sofka creates when nothing mounts the claim: one sleeping
/// container with the volume at [`HELPER_MOUNT`], `generateName` so two
/// sessions never collide, and two independent expiries — the shell's `sleep`
/// and `activeDeadlineSeconds` — so the pod goes away even if sofka is killed
/// before it can delete it.
pub fn helper_pod(claim: &str, image: &str, ttl_secs: u64, resources: Value) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "generateName": HELPER_PREFIX,
            "labels": {
                HELPER_LABELS[0].0: HELPER_LABELS[0].1,
                HELPER_LABELS[1].0: HELPER_LABELS[1].1,
            },
            // A label value can't hold every legal claim name (63 chars, and
            // claims may be longer), so the claim goes in an annotation.
            "annotations": { HELPER_ANNOTATION: claim },
        },
        "spec": {
            "restartPolicy": "Never",
            "activeDeadlineSeconds": ttl_secs,
            "terminationGracePeriodSeconds": 0,
            "automountServiceAccountToken": false,
            "securityContext": { "seccompProfile": { "type": "RuntimeDefault" } },
            "containers": [{
                "name": "explore",
                "image": image,
                "command": ["sh", "-c", format!("sleep {ttl_secs}")],
                "volumeMounts": [{ "name": "pvc", "mountPath": HELPER_MOUNT }],
                "resources": resources,
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "capabilities": { "drop": ["ALL"] },
                },
            }],
            "volumes": [{
                "name": "pvc",
                "persistentVolumeClaim": { "claimName": claim },
            }],
        },
    })
}

/// Parse `ls -A -l` output into entries, sorted directories-first then by name.
///
/// Both GNU coreutils and busybox lay the line out as mode, links, owner,
/// group, size, then three date fields, then the name — so the name is
/// everything past the eighth field, spaces and all. Device nodes replace the
/// single size field with `major, minor`, which shifts the name by one; the
/// mode's leading character says when that happens.
pub fn parse_listing(stdout: &str) -> Vec<Entry> {
    parse_body(stdout).0
}

/// [`parse_listing`], plus a count of body lines that produced no entry —
/// the signal that separates "this directory is empty" from "this is not the
/// `ls` output we know how to read".
fn parse_body(stdout: &str) -> (Vec<Entry>, usize, usize) {
    let mut out = Vec::new();
    let mut unparsed = 0usize;
    let mut unnameable = 0usize;
    for line in stdout.lines() {
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        // From here, anything that bails out is a line we could not read.
        let Some(kind) = line.chars().next().and_then(entry_kind) else {
            unparsed += 1;
            continue;
        };
        // `ls` could not stat this entry, so it printed placeholders: the mode
        // is all `?` and the owner/size columns collapse to one `?` each with
        // a single `?` for the whole timestamp — six fields, not eight.
        // Dropping the line would hide the file entirely; it exists, we just
        // can't size it.
        let unknown = unstattable(line);
        let fields = match () {
            _ if unknown => 6,
            // Device nodes print "major, minor" where a file prints its size.
            _ if matches!(line.as_bytes()[0], b'b' | b'c') => 9,
            _ => 8,
        };
        let Some((head, rest)) = split_fields(line, fields) else {
            unparsed += 1;
            continue;
        };
        // A row whose name field is empty is a file whose name *begins* with
        // a newline: a real file, just not one that can be named. Counted
        // apart from `unparsed`, so a single such name neither makes a
        // readable directory look unreadable nor lets one holding only such
        // files read as empty.
        if rest.is_empty() {
            unnameable += 1;
            continue;
        }
        let (name, link_target) = match kind {
            EntryKind::Link => match rest.split_once(" -> ") {
                Some((n, t)) => (n, t),
                None => (rest, ""),
            },
            _ => (rest, ""),
        };
        if !plausible_name(name) {
            unparsed += 1;
            continue;
        }
        out.push(Entry {
            name: name.to_string(),
            kind,
            // A directory's `ls` size is its own inode's, not the tree's;
            // reporting it would be worse than reporting nothing.
            size: if kind == EntryKind::Dir || unknown {
                None
            } else {
                head[4].parse().ok()
            },
            link_target: link_target.to_string(),
        });
    }
    sort_entries(&mut out);
    (out, unparsed, unnameable)
}

/// Whether `ls` printed this row's metadata as `?` placeholders. The type
/// character is still real (`ls` gets it from the directory entry), so only
/// the permission bits after it are checked.
fn unstattable(line: &str) -> bool {
    let mode = line.split_whitespace().next().unwrap_or_default();
    mode.len() > 1 && mode[1..].bytes().all(|b| b == b'?')
}

fn entry_kind(mode: char) -> Option<EntryKind> {
    match mode {
        'd' => Some(EntryKind::Dir),
        'l' => Some(EntryKind::Link),
        // A filesystem without `d_type` (NFS, XFS without ftype) makes `ls`
        // print `?` for the type as well when it could not stat the entry.
        // It is still a thing that exists on the volume.
        '-' | 'b' | 'c' | 'p' | 's' | '?' => Some(EntryKind::File),
        _ => None,
    }
}

/// Split the first `n` whitespace-separated fields off `line`, returning them
/// with the untouched remainder — which keeps any run of spaces inside a file
/// name that `split_whitespace` would have eaten.
fn split_fields(line: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut rest = line;
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    // Both implementations pad *between* columns but put exactly one space
    // before the name, so only that one is a separator: trimming the run would
    // rename a file called " report.txt" and make one called " " vanish.
    let mut chars = rest.chars();
    if !chars.next()?.is_whitespace() {
        return None;
    }
    Some((fields, chars.as_str()))
}

/// Directories first, then case-insensitive by name — the ordering every file
/// manager uses, and the one that makes a deep tree navigable by eye.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Read one local directory for the left pane. Sizes come from the metadata we
/// already have; symlinks are reported as links without following them, so a
/// dangling one lists instead of erroring.
pub fn read_local(dir: &Path) -> Result<(Vec<Entry>, bool), String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    let mut truncated = false;
    for entry in read {
        // Capped like the remote pane, and for the same reason: a spool or
        // cache directory with a million files would otherwise be read and
        // sorted in full, on the UI thread, before the first frame.
        if out.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        // An entry we cannot stat still exists — the remote pane goes out of
        // its way to keep those visible, so this one must not drop them.
        let meta = entry.metadata().ok();
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match meta.as_ref() {
            Some(m) if m.file_type().is_symlink() => EntryKind::Link,
            Some(m) if m.is_dir() => EntryKind::Dir,
            Some(_) => EntryKind::File,
            // `read_dir` gave us the name but not the metadata; treat it as a
            // file of unknown size rather than pretending it isn't there.
            None => EntryKind::File,
        };
        // The remote pane gets link targets from `ls -l` for free; read them
        // here too, so both sides describe a symlink the same way.
        let link_target = match kind {
            EntryKind::Link => std::fs::read_link(entry.path())
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        out.push(Entry {
            name,
            kind,
            size: match (kind, meta) {
                (EntryKind::Dir, _) => None,
                (_, Some(m)) => Some(m.len()),
                (_, None) => None,
            },
            link_target,
        });
    }
    sort_entries(&mut out);
    Ok((out, truncated))
}

/// Append `name` to `base` as a POSIX path. `base` is always absolute here —
/// it starts at a mount path and only ever grows by one component at a time.
pub fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The parent of `path`, or `None` at `root`. The browser is confined to the
/// mount: there is nothing above it worth showing, and the rest of the
/// container's filesystem is not what the user asked to look at.
pub fn parent_path(path: &str, root: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let root_trimmed = root.trim_end_matches('/');
    if trimmed == root_trimmed || trimmed.is_empty() {
        return None;
    }
    let parent = match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    };
    // A mount path deeper than "/" means the root itself has a parent we must
    // not walk into. Compared on a path boundary, not as a bare prefix, or
    // "/pvcx" would pass for a root of "/pvc".
    let inside = parent == root_trimmed
        || parent
            .strip_prefix(root_trimmed)
            .is_some_and(|tail| tail.starts_with('/'));
    if !root_trimmed.is_empty() && !inside {
        return Some(root.to_string());
    }
    Some(parent)
}

/// Bytes as a short human-readable size: at most three significant figures,
/// no space before the unit. Binary units, like `ls -h` — deliberately not the
/// PVC table's CAPACITY cell, which passes the Kubernetes quantity string
/// (`10Gi`) through untouched.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

/// Marker every size sample is reported on, carrying the multiplier that
/// turns `du`'s number into bytes.
///
/// Unlike the listing's status line this needs no nonce, because a name on the
/// volume cannot put a line through that looks like one: `du` prints the size
/// *and the path*, and the scripts below echo only the leading number, so the
/// path never reaches stdout at all.
const SIZE_MARKER: &str = "sofka-size:";

/// Cap on entries walked to size a local path: past it, a cache directory of
/// a million files is being walked in full for a number only a bar reads.
pub const MAX_SIZE_ENTRIES: usize = 200_000;

/// How many samples [`size_watch`] takes, and how many seconds it may spend
/// taking them, before it gives up and the script exits. An upload longer
/// than that keeps copying; only its bar stops moving. The bound is the
/// backstop for a stdin that never closes — the ordinary end is the reader
/// seeing end-of-input.
///
/// Both, because neither alone is a duration: a `du` over a large tree can
/// take tens of seconds, so a sample count is a lower bound on the time it
/// buys. Fifteen minutes is already a long time to leave a `du` loop in
/// somebody's pod for a bar nobody is watching any more.
pub const SIZE_SAMPLES: u32 = 900;

/// Passes a destination may stay missing before the watcher gives up on it
/// too. `cp` creates its destination almost at once, so a path still absent
/// after this is one the container cannot see — a parent it may not search
/// answers "not there" exactly like a path that is not there — and
/// reporting zero at a bar for the length of a copy is what that would
/// otherwise look like.
pub const ABSENT_PASSES: u32 = 60;

/// Consecutive passes whose `du` answers nothing before the watcher gives
/// up and exits. A path that is merely not there yet answers zero and does
/// not count; this is the container where `du` exists but cannot run — an
/// applet that rejects the `timeout` in front of it, a permission it lacks
/// — and streaming zeros at a bar for the length of a copy says the copy
/// has stalled when it has not. Ending the stream instead is what tells the
/// sampler to drop the bar.
pub const GIVE_UP: u32 = 3;

/// Passes a watcher gets when the container has no `date` to bound itself
/// by. Much smaller, because a pass is then bounded only by its own `du`
/// and the second it sleeps. An upload that outlasts it keeps copying;
/// only its bar stops.
pub const BLIND_SAMPLES: u32 = 300;

/// The `du` invocation both size scripts are built from, reporting `$p`'s
/// recursive total on one [`SIZE_MARKER`] line.
///
/// `du` is the only recursive sizer GNU coreutils and busybox both have, and
/// `-b` is the one spelling of "apparent size in bytes" they agree on: GNU's
/// shorthand for `--apparent-size --block-size=1`, and a plain option on
/// busybox, which rejects both long forms — including in the image this
/// browser builds its own helper pods from, so the long pair would have left
/// the common case on the fallback. That fallback, `-k` in KiB of disk usage,
/// is why the marker carries the multiplier that was used rather than leaving
/// the reader to guess which `du` answered.
///
/// The branch is on whether a *number* came back, not on `du`'s exit status:
/// a directory the serving pod cannot read is ordinary on a volume, and `du`
/// prints the total it did reach and *then* exits non-zero. Keying off the
/// status would throw that away and fall through to a fallback about to fail
/// the same way, leaving no bar at all.
///
/// `set -- $(…)` is what keeps the path off stdout: it word-splits `du`'s
/// `size<TAB>path` and takes `$1`, the number. A name containing a newline
/// therefore cannot forge a sample, only make one unparseable. `set -f`
/// first, because that same split would otherwise glob a destination path
/// containing `*` against the working directory, once a second.
fn size_sample_body() -> String {
    format!(
        r#"u=1
set -- $($dl du -s -b -- "$p" 2>/dev/null)
case "${{1:-}}" in
''|*[!0-9]*)
u=1024
set -- $($dl du -s -k -- "$p" 2>/dev/null)
;;
esac
case "${{1:-}}" in
''|*[!0-9]*)
if [ -e "$p" ]; then
bad=$((bad+1))
set -- ""
else
gone=$((gone+1))
[ "$gone" -lt {ABSENT_PASSES} ] || bad={GIVE_UP}
set -- 0
fi
;;
*) bad=0; gone=0 ;;
esac
[ -z "$1" ] || printf '{SIZE_MARKER}%s:%s\n' "$u" "$1""#
    )
}

/// One recursive byte total for `$1`, for sizing a copy's source while the
/// copy is already running. Run as `sh -c <script> sh <path>`, like the listing: the
/// path arrives as a positional parameter and is never spliced into the
/// script.
pub fn size_probe() -> String {
    format!(
        "p=$1\n{PASS_SETUP}\nbad=0\ngone=0\n{}\n{}",
        du_deadline(),
        size_sample_body()
    )
}

/// What every sample needs and nothing changes between them, so the watcher
/// runs it once rather than eighteen hundred times. `set -f` is for the
/// word-splitting below: only `du`'s leading number is ever read, but
/// without it a destination path containing a `*` would be globbed against
/// the working directory once a second for nothing.
const PASS_SETUP: &str = "set -f";

/// A `timeout` in front of `du`, when the container has one.
///
/// The container's half of every bound here: killing the local `kubectl`
/// does not reach a `du` already running in the pod, so a tree on a wedged
/// mount would sit there long after sofka gave up on it. Empty when the
/// container has no `timeout`, which is the one case where only the local
/// half is bounded.
fn du_deadline() -> String {
    format!(
        "dl=\"\"\ncommand -v timeout >/dev/null 2>&1 && dl=\"timeout {}\"",
        DU_TIMEOUT
    )
}

/// Seconds a single `du` may run inside the container.
pub const DU_TIMEOUT: u32 = 20;

/// The same total, once a second, for as long as an upload can reasonably run.
/// One exec for the whole copy rather than one per sample — the alternative is
/// an exec per second against somebody's production pod.
///
/// A destination that does not exist yet — every upload, until `cp` creates
/// it — reports the zero that is the truth about it. A `du` that answers
/// nothing about a path that *is* there is a different thing, counted
/// towards [`GIVE_UP`]: nothing is printed, and after a few such passes the
/// watcher exits, so the bar is dropped rather than pinned at zero. That zero is what gives such
/// a copy a baseline of nothing rather than no baseline at all. A container
/// with no `du` is the other case and a different answer: nothing can be
/// measured there, so the script exits and the copy runs without a bar.
///
/// The `cat` is what ends the loop, and the arrangement is load-bearing in
/// three ways. A `printf` into an exec stream nobody reads keeps succeeding,
/// so the loop cannot notice its reader on its own — stdin closing is the
/// only signal that reaches the container, which is why the watcher's exec
/// needs `-i`. The reader must take an explicit dup of stdin, because a
/// shell gives a background command `/dev/null` otherwise and it would read
/// end-of-input at once. And the loop runs in the foreground with the reader
/// behind it, each ending the other, so that neither the sample bound nor a
/// closed stdin can leave the other half sitting in somebody's pod.
pub fn size_watch() -> String {
    format!(
        r#"p=$1
command -v du >/dev/null 2>&1 || exit 0
{PASS_SETUP}
bad=0
gone=0
{}
n=0
end=$(($(date +%s 2>/dev/null || echo 0)+{SIZE_SAMPLES}))
lim={SIZE_SAMPLES}
[ "$end" -gt {SIZE_SAMPLES} ] || lim={BLIND_SAMPLES}
exec 3<&0
cat <&3 > /dev/null &
reader=$!
while [ "$n" -lt "$lim" ]; do
kill -0 "$reader" 2>/dev/null || break
if [ "$end" -gt {SIZE_SAMPLES} ] && [ "$(date +%s 2>/dev/null || echo 0)" -ge "$end" ]; then
break
fi
t0=$(date +%s 2>/dev/null || echo 0)
{}
[ "$bad" -lt {GIVE_UP} ] || break
n=$((n+1))
rest=$(($(date +%s 2>/dev/null || echo 0)-t0))
[ "$rest" -gt 1 ] || rest=1
[ "$rest" -lt 30 ] || rest=30
sleep "$rest"
done
kill "$reader" 2>/dev/null
exit 0"#,
        du_deadline(),
        size_sample_body()
    )
}

/// Bytes from one [`size_probe`]/[`size_watch`] sample line, or `None` for a
/// line that is not one — including the sample a `du` that could not read the
/// path prints.
///
/// Zero is a reading like any other: it is what a destination measures
/// before its copy has created it, and knowing that is what lets the copy be
/// measured from zero rather than from whatever had landed by the time
/// something first looked. Zero as a *total* is filtered where totals are
/// decided, not here.
pub fn parse_size_sample(line: &str) -> Option<u64> {
    let (mult, count) = line.trim().strip_prefix(SIZE_MARKER)?.split_once(':')?;
    mult.parse::<u64>()
        .ok()?
        .checked_mul(count.trim().parse::<u64>().ok()?)
}

/// The last sample in a one-shot probe's output — the last, not the first,
/// since a run that printed a partial line has nothing to lose by it.
pub fn parse_size(out: &str) -> Option<u64> {
    out.lines().filter_map(parse_size_sample).next_back()
}

/// What one attempt to size a local path produced. Three answers rather
/// than an `Option`, because the two empty ones want opposite treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    Bytes(u64),
    /// Not there, or not readable — for a copy's destination, not yet.
    Absent,
    /// Past [`MAX_SIZE_ENTRIES`], and it will not come back under.
    TooBig,
}

/// The recursive apparent size of a local path: the byte total a copy of it
/// has to move, giving up past `cap` entries.
///
/// The cap is an argument rather than [`MAX_SIZE_ENTRIES`] itself so a test
/// can reach [`Measure::TooBig`] without building a tree of 200,000 files.
///
/// `stop` is how a caller that has given up says so: this runs on a blocking
/// thread that no timeout can cancel, and a thread still walking is one the
/// runtime waits for at shutdown. Checking it bounds every case except a
/// `read_dir` the kernel has not returned from, which nothing in userspace
/// can bound. A stopped walk reports [`Measure::Absent`] — no reading, ask
/// again — rather than pretending to a number.
///
/// Every entry's own size, directories included, which is the sum
/// `du --apparent-size` produces on the volume side. Counting only files
/// would measure a tree by one definition here and another one there, and a
/// directory of 200 subdirectories would finish at half a bar.
///
/// Entries that cannot be read are skipped rather than fatal — a mode-000
/// cache directory is the ordinary case on a volume, and `du` keeps its
/// partial total for the same reason. Symlinks are counted at their own size
/// and never followed, which is what `tar` puts on the wire and the only way
/// a cycle terminates.
///
/// Two things it cannot make identical, both absorbed by the clamp: what two
/// filesystems charge for a directory inode (4 KiB on ext4, a couple of
/// hundred bytes on APFS), and hard links, counted once per link here and
/// once per inode by `du`.
pub fn local_size_capped(path: &Path, cap: usize, stop: &AtomicBool) -> Measure {
    let Ok(root) = std::fs::symlink_metadata(path) else {
        return Measure::Absent;
    };
    let mut total: u64 = root.len();
    let mut walked: usize = 1;
    // Only directories are stacked, and every entry is counted as it is
    // seen: one flat directory of a few million files would otherwise sit on
    // the stack in full, rebuilt on every sample.
    let mut stack = if root.is_dir() {
        vec![path.to_path_buf()]
    } else {
        Vec::new()
    };
    while let Some(next) = stack.pop() {
        // Asked to stop — the caller timed out, or is gone. Checked per
        // directory rather than per entry: it is the `read_dir` that is slow
        // on the mounts this protects against, not the arithmetic.
        if stop.load(Ordering::Relaxed) {
            return Measure::Absent;
        }
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            walked += 1;
            if walked > cap {
                return Measure::TooBig;
            }
            // Also mid-directory: one flat directory can hold the whole cap,
            // and a walk nobody is waiting for should not run to the end of
            // it before noticing.
            if walked.is_multiple_of(4_096) && stop.load(Ordering::Relaxed) {
                return Measure::Absent;
            }
            // `metadata()` on a `DirEntry` does not follow symlinks, which
            // is what makes a link to a parent finite and a link to a
            // sibling count once.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            total = total.saturating_add(meta.len());
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Measure::Bytes(total)
}

/// Whole percent of `total` that `done` is.
///
/// Clamped, because a total can be an under-estimate: a `du` that could not
/// read every subdirectory, or a local walk that skipped one, reports less
/// than the copy then moves. (An over-estimate — the whole-block `-k`
/// fallback — needs no clamp; it only keeps the bar short of the end.)
pub fn progress_pct(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    // Floored, not rounded: a copy with 4 MB of a 1 GB file still to move is
    // not finished, and a status bar that says 100% while the bar is still
    // filling is the one number a reader would call a bug.
    let pct = (done as f64 / total as f64 * 100.0).floor();
    pct.clamp(0.0, 100.0) as u8
}

/// The two halves of a `width`-cell progress bar: the cells `done` of `total`
/// bytes has filled, and the track behind the rest. Two pieces because they
/// are drawn in different colors; together they are always exactly `width`
/// columns, so a row wearing one stays aligned with the rows that are not.
///
/// Eighth-blocks, not whole cells: the size column is 8 wide, and a 5 GB copy
/// that redrew once every 640 MB would look stuck.
pub fn progress_bar(done: u64, total: u64, width: usize) -> (String, String) {
    const PARTIALS: [&str; 8] = [
        "", "\u{258f}", "\u{258e}", "\u{258d}", "\u{258c}", "\u{258b}", "\u{258a}", "\u{2589}",
    ];
    // Zero says "nothing of it has moved", which is the truthful reading of
    // a total nobody could measure. Totals are refused before they get here,
    // but by accident this would divide to `NaN` and render the same.
    if total == 0 {
        return (String::new(), "\u{2591}".repeat(width));
    }
    let ratio = (done as f64 / total as f64).clamp(0.0, 1.0);
    // Floored, for the reason [`progress_pct`] floors: a bar that fills its
    // last eighth while bytes are still moving says the copy is done.
    let eighths = (ratio * (width * 8) as f64).floor() as usize;
    let full = eighths / 8;
    let partial = PARTIALS[eighths % 8];
    let mut fill = "\u{2588}".repeat(full);
    fill.push_str(partial);
    let cells = full + usize::from(!partial.is_empty());
    let track = "\u{2591}".repeat(width.saturating_sub(cells));
    (fill, track)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn parses_gnu_and_busybox_long_listings() {
        // First block is GNU coreutils, second is busybox — different column
        // widths, same field order.
        let out = "total 12\n\
             drwxr-xr-x 2 root root 4096 Jan  1 00:00 subdir\n\
             -rw-r--r-- 1 root root  128 Jan  1 00:00 data.json\n\
             lrwxrwxrwx 1 root root    9 Jan  1 00:00 current -> data.json\n\
             -rw-r--r--    1 root     root            12 Jan  1 00:00 busybox.txt\n";
        let entries = parse_listing(out);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["subdir", "busybox.txt", "current", "data.json"]
        );
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[1].size, Some(12));
        let link = entries.iter().find(|e| e.name == "current").unwrap();
        assert_eq!(link.kind, EntryKind::Link);
        assert_eq!(link.link_target, "data.json");
    }

    #[test]
    fn entries_ls_could_not_stat_still_list() {
        // GNU prints placeholders for an entry it cannot stat (an NFS
        // root_squash mount) and exits non-zero, but the rest of the listing
        // is good — dropping the line would hide the file entirely.
        let out = "total 0\n\
             -????????? ? ? ? ?            ? secret.dat\n\
             -rw-r--r-- 1 root root 12 Jan  1 00:00 readable.txt\n";
        let entries = parse_listing(out);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["readable.txt", "secret.dat"]
        );
        assert_eq!(entries[1].size, None, "an unstattable entry has no size");
    }

    #[test]
    fn a_link_target_containing_an_arrow_splits_at_the_first_one() {
        let entries = parse_listing("lrwxrwxrwx 1 root root 9 Jan  1 00:00 a -> b -> c\n");
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[0].link_target, "b -> c");
    }

    #[test]
    fn every_run_mints_a_fresh_nonce() {
        assert_ne!(list_probe("/srv").nonce, list_probe("/srv").nonce);
    }

    #[test]
    fn the_listing_script_caps_output_and_carries_ls_status() {
        let probe = list_probe("/srv");
        let script = probe.script;
        assert!(
            script.contains(&format!("exit {EXIT_NOT_A_DIRECTORY}")),
            "{script}"
        );
        // The cap is applied in the pod, not after the fact…
        assert!(
            script.contains(&format!("head -n {}", MAX_ENTRIES + 2)),
            "{script}"
        );
        // …which costs the pipeline's status, so `ls`'s own is echoed, keyed
        // by a nonce a file name on the volume cannot predict.
        assert!(
            script.contains(&format!("{STATUS_MARKER}{}:", probe.nonce)),
            "{script}"
        );
        // The path is only ever "$1" — never interpolated into the script.
        assert!(script.contains(r#"cd -- "$1""#), "{script}");
    }

    #[test]
    fn leading_spaces_belong_to_the_file_name() {
        // `ls` pads *between* columns but puts exactly one space before the
        // name, so a name that starts with a space is data, not padding.
        // Trimming the run renamed " report.txt" and made " " vanish while the
        // local pane still showed it.
        let entries = parse_listing(
            "-rw-r--r-- 1 root root 0 Jan  1 00:00  report.txt\n\
             -rw-r--r-- 1 root root 0 Jan  1 00:00   two\n\
             -rw-r--r-- 1 root root 0 Jan  1 00:00  \n",
        );
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            [" ", "  two", " report.txt"]
        );
    }

    #[test]
    fn read_local_sorts_keeps_and_caps_like_the_remote_pane() {
        let dir = std::env::temp_dir().join(format!("sofka-read-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Beta")).unwrap();
        std::fs::create_dir_all(dir.join("alpha")).unwrap();
        std::fs::write(dir.join("z.txt"), b"hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("z.txt", dir.join("link")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();

        let (entries, truncated) = read_local(&dir).expect("readable");
        assert!(!truncated);
        // Directories first, then case-insensitive by name.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(&names[..2], ["alpha", "Beta"]);
        assert!(entries[0].is_dir() && entries[1].is_dir());
        assert_eq!(entries[0].size, None, "a directory reports no size");

        let file = entries.iter().find(|e| e.name == "z.txt").unwrap();
        assert_eq!(file.size, Some(5));

        #[cfg(unix)]
        {
            let link = entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, EntryKind::Link);
            assert_eq!(link.link_target, "z.txt", "both panes describe links alike");
            // A dangling link still lists — `DirEntry::metadata` is `lstat`
            // on Unix, so it describes the link itself, exactly as `ls -l`
            // does on the other side rather than following it to nowhere.
            let dangling = entries.iter().find(|e| e.name == "dangling").unwrap();
            assert_eq!(dangling.kind, EntryKind::Link);
            assert_eq!(dangling.link_target, "nowhere");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_local_stops_at_the_cap() {
        let dir = std::env::temp_dir().join(format!("sofka-read-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..MAX_ENTRIES + 5 {
            std::fs::write(dir.join(format!("f{i:05}")), b"").unwrap();
        }
        let (entries, truncated) = read_local(&dir).expect("readable");
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(truncated, "the local pane must cap like the remote one");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_local_reports_a_directory_it_cannot_open() {
        let missing = std::env::temp_dir().join("sofka-does-not-exist-9e3f");
        assert!(read_local(&missing).is_err());

        // The case that actually happens on a volume: the directory is there,
        // the process just cannot read it. Never an empty listing.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("sofka-noread-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("hidden"), b"x").unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Running as root would read it anyway, and the assertion would be
            // about the test environment rather than the code.
            if std::fs::read_dir(&dir).is_err() {
                let err = read_local(&dir).unwrap_err();
                assert!(err.contains(&dir.display().to_string()), "{err}");
            }
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).ok();
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn keeps_spaces_inside_file_names() {
        let entries = parse_listing("-rw-r--r-- 1 root root 5 Jan  1 00:00 two  words.txt\n");
        assert_eq!(entries[0].name, "two  words.txt");
    }

    #[test]
    fn device_nodes_do_not_shift_the_name() {
        // "1, 3" occupies two fields where a regular file has one.
        let entries = parse_listing("crw-rw-rw- 1 root root 1, 3 Jan  1 00:00 null\n");
        assert_eq!(entries[0].name, "null");
        assert_eq!(entries[0].kind, EntryKind::File);
    }

    /// A pod spec plus the container statuses that say it is actually up —
    /// `find_mount` needs both, since `status.phase` alone says nothing about
    /// whether any individual container can be exec'd into.
    fn running_pod(name: &str, claim: &str, containers: Value, statuses: Value) -> DynamicObject {
        pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": claim}}],
                "containers": containers,
            },
            "status": {"phase": "Running", "containerStatuses": statuses},
        }))
    }

    fn mounted(name: &str, path: &str, read_only: bool) -> Value {
        json!([{ "name": name, "volumeMounts": [
            {"name": "vol", "mountPath": path, "readOnly": read_only}]}])
    }

    fn up(name: &str) -> Value {
        json!([{ "name": name, "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}} }])
    }

    #[test]
    fn find_mount_prefers_a_writable_running_consumer() {
        let ro = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "reader", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "data", "mountPath": "/data", "readOnly": true}]}],
            },
            "status": {"phase": "Running", "containerStatuses": up("app")},
        }));
        let rw = running_pod("writer", "shared", mounted("app", "/srv", false), up("app"));
        let found = find_mount(&[ro, rw], "shared").expect("a consumer");
        assert_eq!(found.pod, "writer");
        assert_eq!(found.path, "/srv");
        assert!(!found.read_only);
    }

    #[test]
    fn find_mount_ignores_pods_that_are_not_running() {
        let pending = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "pending", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "vol", "mountPath": "/srv"}]}],
            },
            "status": {"phase": "Pending"},
        }));
        assert!(find_mount(&[pending], "shared").is_none());
    }

    #[test]
    fn find_mount_ignores_a_claim_the_pod_does_not_use() {
        let other = running_pod(
            "other",
            "elsewhere",
            mounted("app", "/srv", false),
            up("app"),
        );
        assert!(find_mount(&[other], "shared").is_none());
    }

    #[test]
    fn a_crashlooping_pod_is_not_a_way_into_the_volume() {
        // The pod is `Running`; its only container is not. Exec would fail
        // with "container not running", and returning it here would suppress
        // the helper-pod offer and leave the user in a dead end.
        let crashing = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "crasher", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "vol", "mountPath": "/srv"}]}],
            },
            "status": {"phase": "Running", "containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]},
        }));
        assert!(find_mount(&[crashing], "shared").is_none());
    }

    #[test]
    fn a_finished_init_container_is_not_a_way_into_the_volume() {
        // `initContainers` stays in the spec forever. The seed container has
        // exited; only the app container is up, and it mounts nothing.
        let seeded = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "seeded", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "initContainers": [{"name": "seed", "volumeMounts": [
                    {"name": "vol", "mountPath": "/seed"}]}],
                "containers": [{"name": "app"}],
            },
            "status": {"phase": "Running",
                       "containerStatuses": up("app"),
                       "initContainerStatuses": [
                           {"name": "seed", "state": {"terminated": {"exitCode": 0}}}]},
        }));
        assert!(find_mount(&[seeded], "shared").is_none());
    }

    #[test]
    fn a_running_sidecar_is_a_way_into_the_volume() {
        // A native sidecar — an init container with `restartPolicy: Always`,
        // GA since 1.29 — runs for the pod's whole life and mounts the volume.
        let sidecar = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "with-sidecar", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "initContainers": [{"name": "log-shipper", "restartPolicy": "Always",
                                    "volumeMounts": [{"name": "vol", "mountPath": "/logs"}]}],
                "containers": [{"name": "app"}],
            },
            "status": {"phase": "Running",
                       "containerStatuses": up("app"),
                       "initContainerStatuses": up("log-shipper")},
        }));
        let found = find_mount(&[sidecar], "shared").expect("the sidecar mounts it");
        assert_eq!(found.container, "log-shipper");
        assert_eq!(found.path, "/logs");
    }

    #[test]
    fn a_running_ephemeral_container_is_a_way_into_the_volume() {
        let debugged = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "debugged", "namespace": "default"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app"}],
                "ephemeralContainers": [{"name": "debugger", "volumeMounts": [
                    {"name": "vol", "mountPath": "/mnt"}]}],
            },
            "status": {"phase": "Running",
                       "containerStatuses": up("app"),
                       "ephemeralContainerStatuses": up("debugger")},
        }));
        let found = find_mount(&[debugged], "shared").expect("the debugger mounts it");
        assert_eq!(found.container, "debugger");
    }

    #[test]
    fn find_mount_skips_a_pod_on_its_way_out() {
        // Its exec would be torn down with it, and its volume released.
        let dying = pod(json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "dying", "namespace": "default",
                         "deletionTimestamp": "2026-01-01T00:00:00Z"},
            "spec": {
                "volumes": [{"name": "vol", "persistentVolumeClaim": {"claimName": "shared"}}],
                "containers": [{"name": "app", "volumeMounts": [
                    {"name": "vol", "mountPath": "/srv"}]}],
            },
            "status": {"phase": "Running", "containerStatuses": up("app")},
        }));
        assert!(find_mount(&[dying], "shared").is_none());
    }

    const NONCE: &str = "0123456789abcdef0";

    /// Build the stdout a complete run would produce.
    fn output(body: &str, status: i32) -> String {
        format!("{body}sofka-ls-status:{NONCE}:{status}\n")
    }

    fn interpret(
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) -> Result<(Listing, Option<String>), String> {
        interpret_listing(NONCE, exit_code, stdout, stderr)
    }

    /// `ls -l` lines for `n` plain files, as GNU emits them into a pipe.
    fn files(n: usize) -> String {
        (0..n)
            .map(|i| format!("-rw-r--r-- 1 root root 1 Jan  1 00:00 f{i}\n"))
            .collect()
    }

    #[test]
    fn an_unreadable_directory_is_an_error_not_an_empty_one() {
        // Real busybox and GNU behaviour for a directory with mode 0111: `cd`
        // succeeds, `ls` writes only "total 0" and fails. Rendering that as
        // "empty" would tell the user their volume has nothing on it.
        let err = interpret(
            Some(0),
            &output("total 0\n", 1),
            "ls: can't open '.': Permission denied\n",
        )
        .expect_err("an unreadable directory must not read as empty");
        assert!(err.contains("Permission denied"), "{err}");
    }

    #[test]
    fn a_partly_unstattable_directory_lists_with_a_warning() {
        let (listing, warn) = interpret(
            Some(0),
            &output(
                "total 0\n\
                 -????????? ? ? ? ?            ? locked\n\
                 -rw-r--r-- 1 root root 12 Jan  1 00:00 fine.txt\n",
                1,
            ),
            "ls: cannot access 'locked': Permission denied\n",
        )
        .expect("entries came back, so this is a partial success");
        assert_eq!(listing.entries.len(), 2);
        assert!(!listing.truncated);
        assert!(warn.is_some_and(|w| w.contains("Permission denied")));
    }

    #[test]
    fn a_command_that_never_ran_is_an_error_even_with_empty_output() {
        // No marker and no truncation: `sh`/`ls` never reported a status —
        // a missing binary, a pod that isn't running, a denied exec.
        let err = interpret(Some(126), "", "sh: ls: not found\n")
            .expect_err("a failed exec must not read as an empty directory");
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn a_missing_marker_with_a_full_buffer_means_truncated_not_failed() {
        // `head` closed the pipe before `ls` could print the marker.
        let body = files(MAX_ENTRIES + 2);
        let (listing, warn) = interpret(Some(0), &body, "").expect("a truncated listing");
        assert!(listing.truncated);
        assert_eq!(listing.entries.len(), MAX_ENTRIES);
        assert!(warn.is_none());
    }

    #[test]
    fn a_file_name_cannot_forge_the_status_marker() {
        // GNU `ls` prints names raw into a pipe, so a file called
        // "evil\nsofka-ls-status:0" puts a marker-shaped line through — before
        // the real marker, which `head` then cuts. Without the nonce that
        // makes a truncated listing claim to be complete.
        let mut body = String::from("-rw-r--r-- 1 root root 1 Jan  1 00:00 evil\n");
        body.push_str("sofka-ls-status:0\n");
        body.push_str(&files(MAX_ENTRIES + 1));
        let (listing, _) = interpret(Some(0), &body, "").expect("a listing");
        assert!(
            listing.truncated,
            "a forged marker made a truncated listing look complete"
        );
        assert!(listing.status.is_none());
    }

    #[test]
    fn names_containing_newlines_do_not_fake_truncation() {
        // Truncation is measured in lines, not parsed entries: 3000 files
        // whose names contain a newline are 6000 lines and 3000 entries, and
        // `ls` read the directory perfectly.
        let body: String = (0..3_000)
            .map(|i| format!("-rw-r--r-- 1 root root 1 Jan  1 00:00 a{i}\nb{i}\n"))
            .collect();
        let (listing, warn) = interpret(Some(0), &output(&body, 0), "").expect("a clean listing");
        assert!(!listing.truncated, "a complete listing reported truncation");
        assert!(warn.is_none());
    }

    #[test]
    fn an_unreadable_ls_format_is_an_error_not_an_empty_directory() {
        // GNU `ls` honours TIME_STYLE from the container's environment, and
        // `long-iso` prints a two-field date where the parser expects three —
        // shifting the name column so nothing parses. The script unsets it,
        // but any other `ls` (toybox, a future format) does the same, and
        // reporting a full volume as "empty" is the one outcome to avoid.
        let err = interpret(
            Some(0),
            &output(
                "total 8\n\
                 -rw-r--r-- 1 root root 3 2026-09-06 22:24 plain.txt\n\
                 drwxr-xr-x 2 root root 4096 2026-09-06 22:24 sub\n",
                0,
            ),
            "",
        )
        .expect_err("an unparseable listing must not read as empty");
        assert!(err.contains("unexpected `ls` output"), "{err}");
    }

    #[test]
    fn a_truncated_listing_that_parsed_to_nothing_is_still_an_error() {
        // Plenty of output, none of it in a shape we read. "Unreadable" does
        // not become "empty" just because there was a lot of it.
        let body: String = (0..MAX_ENTRIES + 2)
            .map(|i| format!("-rw-r--r-- 1 root root 3 2026-09-06 22:24 f{i}\n"))
            .collect();
        let err = interpret(Some(0), &body, "").unwrap_err();
        assert!(err.contains("unexpected `ls` output"), "{err}");
    }

    #[test]
    fn a_name_beginning_with_a_newline_is_reported_as_such_not_as_garbage() {
        // The head row carries the metadata and no name; the remainder lands
        // on the next line and cannot be parsed. One leftover per unnameable
        // row is the signature of a newline in a name — more than that is a
        // format we do not read, which is a different message.
        let (listing, warn) = interpret(
            Some(0),
            &output("total 0\n-rw-r--r-- 1 root root 0 Jan  1 00:00 \nfoo\n", 0),
            "",
        )
        .expect("readable, just unnameable");
        assert_eq!(listing.unnameable, 1);
        assert_eq!(listing.unparsed, 1, "the name's remainder");
        assert!(warn.is_some_and(|w| w.contains("newline")), "wrong message");
    }

    #[test]
    fn one_empty_name_row_does_not_excuse_a_garbage_listing() {
        // The negative side of the `unparsed <= unnameable` guard: a format we
        // cannot read stays an error even when one row happens to look like a
        // newline-leading name.
        let err = interpret(
            Some(0),
            &output(
                "total 0\n\
                 -rw-r--r-- 1 root root 0 Jan  1 00:00 \n\
                 -rw-r--r-- 1 root root 3 2026-09-06 22:24 a.txt\n\
                 -rw-r--r-- 1 root root 3 2026-09-06 22:24 b.txt\n",
                0,
            ),
            "",
        )
        .unwrap_err();
        assert!(err.contains("unexpected `ls` output"), "{err}");
    }

    #[test]
    fn a_directory_of_only_unnameable_files_is_not_empty() {
        // Its one file is named "\n": `ls` prints a row with no name and then
        // the tail. Nothing parses, but the directory is neither unreadable
        // nor empty — it holds a file sofka has no way to name.
        let (listing, warn) = interpret(
            Some(0),
            &output("total 0\n-rw-r--r-- 1 root root 0 Jan  1 00:00 \n\n", 0),
            "",
        )
        .expect("readable, just unnameable");
        assert!(listing.entries.is_empty());
        assert_eq!(listing.unnameable, 1);
        assert!(
            warn.is_some_and(|w| w.contains("newline")),
            "the pane would have said 'empty'"
        );
    }

    #[test]
    fn one_name_containing_a_newline_does_not_make_a_directory_unreadable() {
        // The tail of such a name is a row with no name field of its own.
        // Counting it as unparseable would let a single adversarial file name
        // hide a directory that `ls` read perfectly.
        let (listing, warn) = interpret(
            Some(0),
            &output(
                "total 0\n\
                 -rw-r--r-- 1 root root 0 Jan  1 00:00 \n\
                 tail-of-the-name\n\
                 -rw-r--r-- 1 root root 3 Jan  1 00:00 real.txt\n",
                0,
            ),
            "",
        )
        .expect("the directory is readable");
        assert!(listing.entries.iter().any(|e| e.name == "real.txt"));
        // The readable entry is listed, and the one that cannot be named is
        // reported rather than silently dropped.
        assert!(warn.is_some_and(|w| w.contains("newline")));
    }

    #[test]
    fn a_genuinely_empty_directory_is_still_empty() {
        let (listing, warn) = interpret(Some(0), &output("total 0\n", 0), "").expect("a listing");
        assert!(listing.entries.is_empty());
        assert_eq!(listing.unparsed, 0);
        assert!(warn.is_none());
    }

    #[test]
    fn the_listing_script_pins_the_output_format_and_needs_both_paths() {
        let script = list_probe("/srv").script;
        // GNU `ls` reshapes its columns from the environment; a container
        // exporting TIME_STYLE would otherwise make every entry unparseable.
        // GNU `ls` takes its date format, its quoting and its size units from
        // the environment; a container that exports any of them would make the
        // listing unparseable, unaddressable, or silently misreport sizes.
        let unset = script
            .lines()
            .find(|l| l.starts_with("unset "))
            .expect("the script unsets nothing");
        for var in ["TIME_STYLE", "QUOTING_STYLE", "BLOCK_SIZE", "LS_BLOCK_SIZE"] {
            assert!(unset.contains(var), "{var} is not unset: {unset}");
        }
        // `cd ""` succeeds in dash and busybox ash, so neither argument may
        // be allowed through empty.
        assert!(script.contains(r#"[ -n "$1" ] && [ -n "$2" ]"#), "{script}");
    }

    #[test]
    fn a_complete_listing_reports_no_truncation() {
        let (listing, warn) = interpret(
            Some(0),
            &output("total 0\n-rw-r--r-- 1 root root 1 Jan  1 00:00 a\n", 0),
            "",
        )
        .expect("a clean listing");
        assert_eq!(listing.entries.len(), 1);
        assert!(!listing.truncated);
        assert!(warn.is_none());
    }

    #[test]
    fn a_path_that_is_not_a_directory_says_so() {
        let err = interpret(Some(EXIT_NOT_A_DIRECTORY), "", "").unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn a_file_name_cannot_inject_a_row_that_escapes_the_mount() {
        // Exactly what GNU `ls` writes into a pipe for a directory holding a
        // file called "x\ndrwxr-xr-x 2 root root 4096 Jan  1 00:00 ..": names
        // go through raw, so the second half arrives as its own row.
        let entries = parse_listing(
            "total 4\n\
             -rw-r--r-- 1 root root    0 Jan  1 00:00 x\n\
             drwxr-xr-x 2 root root 4096 Jan  1 00:00 ..\n",
        );
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["x"],
            "a forged '..' row would walk the browser out of the mount"
        );
    }

    #[test]
    fn a_symlink_target_cannot_inject_an_absolute_path() {
        // A link target is arbitrary bytes, so unlike a file name it can carry
        // "/" — and `Path::join` with an absolute path replaces rather than
        // appends, which on download would write anywhere on the local disk.
        let entries = parse_listing(
            "lrwxrwxrwx 1 root root 51 Jan  1 00:00 evil -> t\n\
             -rw-r--r-- 1 root root  7 Jan  1 00:00 /etc/passwd\n",
        );
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["evil"]
        );
    }

    #[test]
    fn relative_and_separator_bearing_names_never_survive() {
        for forged in [
            "..",
            ".",
            "a/b",
            "/etc/passwd",
            "../../.ssh/authorized_keys",
        ] {
            let line = format!("-rw-r--r-- 1 root root 1 Jan  1 00:00 {forged}\n");
            assert!(
                parse_listing(&line).is_empty(),
                "{forged:?} survived parsing"
            );
        }
        // …while a name that merely contains a dot is ordinary.
        assert_eq!(
            parse_listing("-rw-r--r-- 1 root root 1 Jan  1 00:00 ..hidden\n")[0].name,
            "..hidden"
        );
    }

    #[test]
    fn the_helper_pod_carries_every_piece_of_evidence_the_sweep_requires() {
        let spec = helper_pod("data", "busybox:1.37", 900, json!({}));
        for (k, v) in HELPER_LABELS {
            assert_eq!(spec["metadata"]["labels"][k], v);
        }
        assert_eq!(spec["metadata"]["annotations"][HELPER_ANNOTATION], "data");
        assert_eq!(spec["metadata"]["generateName"], HELPER_PREFIX);
        // The selector `:pvc-clean` lists with must name every label, or the
        // extra evidence buys nothing.
        let selector = helper_selector();
        for (k, v) in HELPER_LABELS {
            assert!(selector.contains(&format!("{k}={v}")), "{selector}");
        }
    }

    #[test]
    fn a_link_that_leaves_the_mount_is_refused_by_its_own_exit_code() {
        let err = interpret(Some(EXIT_OUTSIDE_MOUNT), "", "").unwrap_err();
        assert!(err.contains("outside the volume"), "{err}");
    }

    #[test]
    fn the_listing_script_resolves_both_sides_before_comparing_them() {
        let probe = list_probe("/srv");
        // A mount path can itself sit behind a symlink; comparing the raw
        // strings would refuse the mount's own root.
        assert!(
            probe
                .script
                .contains(r#"root=$(cd -- "$2" 2>/dev/null && pwd -P)"#)
        );
        // Trailing slash on both sides: the boundary is explicit, so `/pvcx`
        // is not inside `/pvc` and a root of `/` needs no special case.
        assert!(
            probe
                .script
                .contains(r#"case "$(pwd -P)/" in "${root%/}/"*)"#),
            "{}",
            probe.script
        );
        assert!(probe.script.contains(&format!("exit {EXIT_OUTSIDE_MOUNT}")));
        assert_eq!(probe.root, "/srv");
    }

    #[test]
    fn parent_path_stops_at_the_mount_root() {
        assert_eq!(parent_path("/pvc/a/b", "/pvc").as_deref(), Some("/pvc/a"));
        assert_eq!(parent_path("/pvc/a", "/pvc").as_deref(), Some("/pvc"));
        assert_eq!(parent_path("/pvc", "/pvc"), None);
        assert_eq!(parent_path("/pvc/", "/pvc"), None);
        // A one-component root: the parent of its child is the root, not "/".
        assert_eq!(parent_path("/data/x", "/data").as_deref(), Some("/data"));
    }

    #[test]
    fn join_path_does_not_double_the_separator() {
        assert_eq!(join_path("/pvc", "a"), "/pvc/a");
        assert_eq!(join_path("/", "a"), "/a");
    }

    #[test]
    fn human_size_keeps_three_significant_figures() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(1024 * 1024 * 20), "20M");
    }

    #[test]
    fn helper_pod_mounts_the_claim_and_expires_twice() {
        let spec = helper_pod("data", "busybox:1.37", 900, json!({}));
        assert_eq!(spec["spec"]["activeDeadlineSeconds"], 900);
        assert_eq!(spec["spec"]["containers"][0]["command"][2], "sleep 900");
        assert_eq!(
            spec["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"],
            "data"
        );
        assert_eq!(
            spec["spec"]["containers"][0]["volumeMounts"][0]["mountPath"],
            HELPER_MOUNT
        );
        assert_eq!(spec["metadata"]["generateName"], HELPER_PREFIX);
    }

    #[test]
    fn helper_pod_limits_match_requests_so_any_limit_range_ratio_admits_it() {
        let cfg = crate::config::PvcExploreConfig::default();
        let spec = helper_pod("data", &cfg.image, 900, cfg.resources());
        let resources = &spec["spec"]["containers"][0]["resources"];
        for key in ["cpu", "memory"] {
            assert!(resources["limits"][key].is_string(), "{resources}");
            assert_eq!(resources["limits"][key], resources["requests"][key]);
        }
    }

    #[test]
    fn helper_pod_takes_configured_resources_and_skips_empty_ones() {
        let cfg = crate::config::PvcExploreConfig {
            cpu_request: "1".into(),
            cpu_limit: "".into(),
            memory_request: "128Mi".into(),
            memory_limit: "nonsense".into(),
            ..Default::default()
        };
        let spec = helper_pod("data", &cfg.image, 900, cfg.resources());
        assert_eq!(
            spec["spec"]["containers"][0]["resources"],
            json!({"requests": {"cpu": "1", "memory": "128Mi"}})
        );
    }

    /// Run a generated script under a real `sh` with `du` shimmed.
    #[cfg(unix)]
    fn probe_with_du(du: &str, path: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let nth = NEXT.fetch_add(1, Ordering::Relaxed);
        let bin = std::env::temp_dir().join(format!("sofka-du-{}-{nth}", std::process::id()));
        std::fs::remove_dir_all(&bin).ok();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("du"), du).unwrap();
        std::fs::set_permissions(bin.join("du"), std::fs::Permissions::from_mode(0o755)).unwrap();
        for tool in ["sh", "printf"] {
            let real = ["/bin", "/usr/bin"]
                .iter()
                .map(|d| std::path::Path::new(d).join(tool))
                .find(|p| p.exists());
            if let Some(real) = real {
                std::os::unix::fs::symlink(real, bin.join(tool)).ok();
            }
        }
        let out = std::process::Command::new("env")
            .arg(format!("PATH={}", bin.display()))
            .args(["sh", "-c", &size_probe(), "sh", path])
            .output()
            .expect("running the probe");
        std::fs::remove_dir_all(&bin).ok();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    #[cfg(unix)]
    fn a_du_without_b_falls_back_to_whole_blocks() {
        // The branch CI never takes: GNU `du -b` always works on Linux, so
        // without a shim the `-k` half of the unit negotiation — and the
        // multiplier that makes it readable — is never run.
        let blocks = "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = -b ] && exit 1; done\nprintf '4\\t%s\\n' \"$3\"\n";
        assert_eq!(probe_with_du(blocks, "/"), "sofka-size:1024:4");
        assert_eq!(parse_size("sofka-size:1024:4"), Some(4096));
    }

    /// Run the watcher under a real `sh` with `du` shimmed, holding its
    /// stdin open so that only its own bounds can end it. Bounded here too:
    /// a bound that stopped working would otherwise hang the suite instead
    /// of failing it.
    #[cfg(unix)]
    fn watch_with_du(du: &str, path: &str) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let nth = NEXT.fetch_add(1, Ordering::Relaxed);
        let bin = std::env::temp_dir().join(format!("sofka-watch-{}-{nth}", std::process::id()));
        std::fs::remove_dir_all(&bin).ok();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("du"), du).unwrap();
        std::fs::set_permissions(bin.join("du"), std::fs::Permissions::from_mode(0o755)).unwrap();
        for tool in ["sh", "printf", "sleep", "cat", "date"] {
            if let Some(real) = ["/bin", "/usr/bin"]
                .iter()
                .map(|dir| std::path::Path::new(dir).join(tool))
                .find(|tool| tool.exists())
            {
                std::os::unix::fs::symlink(real, bin.join(tool)).ok();
            }
        }
        let mut child = std::process::Command::new("env")
            .arg(format!("PATH={}", bin.display()))
            .args(["sh", "-c", &size_watch(), "sh", path])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("running the watcher");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let ended = loop {
            match child.try_wait().expect("waiting on the watcher") {
                Some(status) => break Some(status),
                None if std::time::Instant::now() >= deadline => {
                    child.kill().ok();
                    break None;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        };
        let out = child.wait_with_output().expect("reading the watcher");
        std::fs::remove_dir_all(&bin).ok();
        let status = ended.expect("the watcher never gave up on its own");
        assert!(status.success(), "the watcher exited badly: {status}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn a_watcher_whose_du_cannot_answer_gives_up_rather_than_streaming() {
        // Driven, not grepped: the bound has to be a number of passes the
        // loop really stops after, or a container whose `du` is present but
        // cannot run keeps one going once a second for the whole copy.
        let broken = "#!/bin/sh\nexit 1\n";
        let samples = watch_with_du(broken, "/");
        assert!(
            samples.is_empty(),
            "a `du` that answered nothing was reported as a size: {samples:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_du_that_answers_with_words_is_not_read_as_a_size() {
        // Both guards, driven rather than grepped: a `du` that writes usage
        // to *stdout* would otherwise have its first word parsed, and the
        // fallback would inherit it.
        let usage = "#!/bin/sh\necho 'Usage: du [-abck] [FILE]...'\nexit 1\n";
        assert_eq!(
            probe_with_du(usage, "/"),
            "",
            "usage text was read as a size"
        );

        // A `du` that answers properly is still read.
        let real = "#!/bin/sh\nprintf '4096\\t%s\\n' \"$3\"\n";
        assert_eq!(probe_with_du(real, "/"), "sofka-size:1:4096");
    }

    #[test]
    fn a_size_sample_carries_the_unit_that_measured_it() {
        // The multiplier says which `du` answered.
        assert_eq!(
            parse_size_sample("sofka-size:1:5368709120"),
            Some(5_368_709_120)
        );
        assert_eq!(parse_size_sample("sofka-size:1024:4"), Some(4096));
        // `du` could not read the path yet — the first second of an upload.
        assert_eq!(parse_size_sample("sofka-size:1024:"), None);
        assert_eq!(parse_size_sample("total 4"), None);
        // Zero is a reading, not a non-answer: a destination that is not
        // there yet measures zero, and a copy starting from zero is the
        // whole point. (Zero as a *total* is refused where totals are
        // decided — a bar against it would sit there looking finished.)
        assert_eq!(parse_size_sample("sofka-size:1:0"), Some(0));
        // A line cut off mid-write is not a sample.
        assert_eq!(parse_size_sample("sofka-siz"), None);
        // A count that cannot be multiplied is no sample: wrapping it would
        // turn a huge number into a small and plausible one.
        assert_eq!(
            parse_size_sample("sofka-size:1024:18446744073709551615"),
            None
        );
        // A `du` that answered with usage text rather than a number falls
        // through to the fallback, and then to a zero — never to a sample
        // built out of whatever word came first.
        assert!(size_probe().contains("*[!0-9]*"), "{}", size_probe());
        assert_eq!(parse_size("sofka-size:1:40\nsofka-size:1:"), Some(40));
    }

    #[test]
    fn only_the_number_reaches_stdout_so_a_name_cannot_forge_a_sample() {
        // `set -- $out` takes du's leading field and drops the path, which is
        // the only thing on that line a volume controls.
        assert!(size_probe().contains("set -- $($dl du -s -b"));
        let printed = size_probe()
            .lines()
            .find(|l| l.starts_with("printf"))
            .unwrap_or_default()
            .to_string();
        assert!(
            !printed.contains("$p"),
            "the path must not be echoed: {printed}"
        );
        // A completed run's own last sample is the one that counts.
        let output = "sofka-size:1:120\nsofka-size:1024:999999";
        assert_eq!(parse_size(output), Some(1024 * 999_999));
    }

    #[test]
    fn the_watcher_is_one_exec_that_stops_when_its_reader_goes_away() {
        let script = size_watch();
        // A pass, then at least a second's rest — and more when the `du`
        // itself took longer, so a slow tree is not measured back to back
        // in somebody's production pod.
        assert!(script.contains(r#"sleep "$rest""#), "{script}");
        assert!(
            script.contains(r#"[ "$rest" -gt 1 ] || rest=1"#),
            "{script}"
        );
        // Writes into an abandoned exec stream keep succeeding, so the only
        // signal that reaches the container is stdin closing. The reader
        // waits for it in the background; the loop checks each pass that the
        // reader is still there, and kills it on the way out — so neither
        // half can outlive the other by more than a pass.
        // Explicitly from a dup of stdin: a background command's stdin is
        // /dev/null unless it says otherwise, and that reads EOF at once.
        assert!(script.contains("exec 3<&0"), "{script}");
        assert!(script.contains("cat <&3 > /dev/null &"), "{script}");
        assert!(
            script.contains(r#"kill -0 "$reader" 2>/dev/null || break"#),
            "{script}"
        );
        assert!(script.contains(r#"kill "$reader""#), "{script}");
        assert!(
            script.contains(&format!("lim={SIZE_SAMPLES}")),
            "an unbounded watcher could outlive the copy in somebody's pod"
        );
        // A sample count is not a duration — one `du` over a large tree can
        // take tens of seconds — so the loop watches the clock as well.
        assert!(script.contains("date +%s"), "{script}");
        // And a much shorter one when the container has no clock to bound
        // itself by, since a pass then costs a `du` rather than a second.
        assert!(script.contains(&format!("lim={BLIND_SAMPLES}")), "{script}");
        // And it stops altogether when `du` is there but answers nothing:
        // streaming zeros would pin a bar at zero for the whole copy, while
        // ending the stream tells the sampler to drop the bar.
        assert!(
            script.contains(&format!(r#"[ "$bad" -lt {GIVE_UP} ] || break"#)),
            "{script}"
        );
        // A destination that never appears is the same problem wearing
        // another hat: a parent the container cannot search answers "not
        // there" exactly like a path that is not there.
        assert!(
            script.contains(&format!(
                r#"[ "$gone" -lt {ABSENT_PASSES} ] || bad={GIVE_UP}"#
            )),
            "{script}"
        );
        // A clock that steps forward mid-`du` must not park the loop.
        assert!(
            script.contains(r#"[ "$rest" -lt 30 ] || rest=30"#),
            "{script}"
        );
        // A destination that comes back clears both counters, so a copy
        // that is merely intermittent keeps its bar.
        assert!(script.contains("*) bad=0; gone=0 ;;"), "{script}");
        // And the wall clock ends it even with passes left over.
        assert!(script.contains(r#"-ge "$end""#), "{script}");
        // The word-splitting that reads `du`'s number must not glob a
        // destination path containing a `*` against the working directory.
        assert!(script.contains("set -f"), "{script}");
        // Its ordinary end — the reader gone — is not a failure, and kubectl
        // reports a non-zero exit as an error the copy did not have.
        assert!(script.trim_end().ends_with("exit 0"), "{script}");
        // A container with no `du` cannot be measured at all: no samples,
        // and so no bar, rather than a bar pinned at zero.
        assert!(script.contains("command -v du"), "{script}");
        // Bounded inside the container too, where killing the local
        // `kubectl` cannot reach: a `du` on a wedged mount would otherwise
        // outlive the session that asked for it.
        assert!(
            script.contains(&format!("timeout {DU_TIMEOUT}")),
            "{script}"
        );
    }

    #[test]
    fn a_progress_bar_is_always_its_full_width() {
        use unicode_width::UnicodeWidthStr;
        for done in [0u64, 1, 500, 2_500, 5_000] {
            let (fill, track) = progress_bar(done, 5_000, 8);
            assert_eq!(
                fill.chars().count() + track.chars().count(),
                8,
                "{done}: {fill}|{track}"
            );
            // Columns, not characters: the bar sits in a column the pane
            // measures with `unicode_width`, so a cell that counts as two
            // would push the row through the border.
            assert_eq!(
                fill.width() + track.width(),
                8,
                "{done}: {fill}|{track} is not 8 columns wide"
            );
        }
        assert_eq!(progress_bar(0, 5_000, 8).0, "");
        assert_eq!(progress_bar(5_000, 5_000, 8).1, "");
        // A pane too narrow for a size column is too narrow for a bar.
        assert_eq!(progress_bar(1, 2, 0), (String::new(), String::new()));
    }

    #[test]
    fn a_progress_bar_moves_in_eighths_of_a_cell() {
        // A 5 GB copy across 8 cells: whole cells alone would redraw once
        // every 640 MB.
        let total = 5 * 1024 * 1024 * 1024;
        let (early, _) = progress_bar(total / 64, total, 8);
        assert_eq!(early, "\u{258f}", "the first 80 MB already show");
        let (eighth, _) = progress_bar(total / 8, total, 8);
        assert_eq!(eighth, "\u{2588}", "an eighth of the way is one whole cell");
        let (half, track) = progress_bar(total / 2, total, 8);
        assert_eq!(half, "\u{2588}\u{2588}\u{2588}\u{2588}");
        assert_eq!(track, "\u{2591}\u{2591}\u{2591}\u{2591}");
    }

    #[test]
    fn an_over_estimated_total_still_reports_a_sane_percentage() {
        // Over 100% is the under-estimated total: a subdirectory `du` could
        // not read is missing from it, and the copy moves those bytes anyway.
        // (The whole-block `-k` fallback errs the other way, and only ever
        // leaves the bar short.)
        assert_eq!(progress_pct(120, 100), 100);
        assert_eq!(progress_pct(0, 100), 0);
        assert_eq!(progress_pct(50, 100), 50);
        // Floored: still moving is never 100%.
        assert_eq!(progress_pct(999_999_999, 1_000_000_000), 99);
        // And the bar floors with it: the last cell only becomes a whole
        // block when the copy is actually over, so "solid to the edge" is
        // never something a copy still moving can show.
        let (nearly, _) = progress_bar(999_999_999, 1_000_000_000, 8);
        assert!(
            !nearly.ends_with('\u{2588}'),
            "the bar filled before the copy did: {nearly}"
        );
        let (done, track) = progress_bar(1_000_000_000, 1_000_000_000, 8);
        assert!(
            done.ends_with('\u{2588}') && track.is_empty(),
            "{done}|{track}"
        );
        // Nothing of nothing: a total of zero is refused where totals are
        // decided, and claiming "complete" would be the wrong answer anyway.
        assert_eq!(progress_pct(1, 0), 0);
        assert_eq!(progress_bar(1, 0, 3), (String::new(), "░░░".to_string()));
        assert_eq!(
            progress_bar(120, 100, 4).0,
            "\u{2588}\u{2588}\u{2588}\u{2588}"
        );
    }

    #[test]
    fn sizing_a_local_tree_counts_what_du_counts() {
        let dir = std::env::temp_dir().join(format!("sofka-size-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), b"0123456789").unwrap();
        std::fs::write(dir.join("sub/b"), b"012345").unwrap();
        // Files *and* the directories' own inodes, which is what
        // `du --apparent-size` sums on the volume side. Their size is the
        // filesystem's business, so the expectation asks it rather than
        // hard-coding ext4's 4096 or APFS's couple of hundred bytes.
        let inode = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().len();
        let go = AtomicBool::new(false);
        let sized = |p: &std::path::Path| local_size_capped(p, MAX_SIZE_ENTRIES, &go);
        let dirs = inode(&dir) + inode(&dir.join("sub"));
        assert_eq!(sized(&dir), Measure::Bytes(16 + dirs));
        assert_eq!(sized(&dir.join("a")), Measure::Bytes(10));
        // Not there is its own answer: a copy's destination is like this
        // until `cp` creates it, and it is worth asking about again.
        assert_eq!(sized(&dir.join("nope")), Measure::Absent);
        // A symlink counts as itself and is never followed, which is what
        // `tar` puts on the wire — and the only way to walk a cycle.
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
        let link = inode(&dir.join("loop"));
        assert_eq!(sized(&dir.join("loop")), Measure::Bytes(link));
        // And inside a tree: counted once at its own size, never followed —
        // following it would both double-count and, here, never finish.
        assert_eq!(
            sized(&dir),
            Measure::Bytes(16 + inode(&dir) + inode(&dir.join("sub")) + link),
            "a symlink in the tree was followed"
        );
        std::fs::remove_file(dir.join("loop")).unwrap();
        // And a tree past the cap is a third answer, not a size.
        assert_eq!(local_size_capped(&dir, 2, &go), Measure::TooBig);
        // Asked to stop, it stops — with no reading rather than a partial
        // one, which is the answer that gets asked again.
        let stopped = AtomicBool::new(true);
        assert_eq!(
            local_size_capped(&dir, MAX_SIZE_ENTRIES, &stopped),
            Measure::Absent
        );

        // One subdirectory nobody can enter is the ordinary case on a volume,
        // and it must not cost the whole total — `du` keeps its partial one.
        std::fs::create_dir(dir.join("locked")).unwrap();
        std::fs::write(dir.join("locked/c"), b"01234").unwrap();
        let mut perms = std::fs::metadata(dir.join("locked")).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(dir.join("locked"), perms).unwrap();
        // Re-read: adding an entry grows the parent directory's own inode.
        let reachable = 16 + inode(&dir) + inode(&dir.join("sub")) + inode(&dir.join("locked"));
        assert_eq!(
            sized(&dir),
            Measure::Bytes(reachable),
            "an unreadable subtree ate the total"
        );
        let mut perms = std::fs::metadata(dir.join("locked")).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(dir.join("locked"), perms).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn consumer() -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion":"v1", "kind":"Pod", "metadata":{"name":"app", "namespace":"default"},
            "spec":{"nodeName":"worker-a", "volumes":[{"name":"data", "persistentVolumeClaim":{"claimName":"data"}}],
                "containers":[
                    {"name":"app", "volumeMounts":[{"name":"data", "mountPath":"/data", "subPath":"tenant", "readOnly":true}]},
                    {"name":"tools", "volumeMounts":[{"name":"data", "mountPath":"/tools", "subPath":"tenant"}]},
                    {"name":"broad", "volumeMounts":[{"name":"data", "mountPath":"/all"}]}]},
            "status":{"phase":"Running", "containerStatuses":[
                {"name":"app", "state":{"running":{}}},
                {"name":"tools", "state":{"running":{}}},
                {"name":"broad", "state":{"running":{}}}]}
        })).unwrap()
    }

    fn claim(mode: &str) -> DynamicObject {
        serde_json::from_value(json!({"apiVersion":"v1", "kind":"PersistentVolumeClaim",
            "metadata":{"name":"data"}, "spec":{"accessModes":[mode]}, "status":{"phase":"Bound"}}))
        .unwrap()
    }

    fn original(pod: &DynamicObject) -> Mount {
        find_mounts(std::slice::from_ref(pod), "data")
            .into_iter()
            .find(|m| m.container == "app")
            .unwrap()
    }

    #[test]
    fn recovery_preserves_subpath_and_readonly_and_uses_consumer_node() {
        let pod = consumer();
        let original = original(&pod);
        let plan = recovery_plan(&[pod], &claim("ReadWriteOnce"), &original);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].container, "tools");
        assert!(plan.candidates[0].read_only);
        let options = plan.helper.unwrap();
        assert_eq!(options.node.as_deref(), Some("worker-a"));
        assert_eq!(options.sub_path, "tenant");
        assert!(options.read_only);
        let mut manifest = helper_pod("data", "busybox:1.37", 900, json!({}));
        apply_helper_options(&mut manifest, &options);
        assert_eq!(manifest.pointer("/spec/affinity/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms/0/matchFields/0/values/0"), Some(&json!("worker-a")));
        assert_eq!(
            manifest.pointer("/spec/containers/0/volumeMounts/0/subPath"),
            Some(&json!("tenant"))
        );
        assert_eq!(
            manifest.pointer("/spec/containers/0/volumeMounts/0/readOnly"),
            Some(&json!(true))
        );
        assert_eq!(
            manifest.pointer("/spec/volumes/0/persistentVolumeClaim/readOnly"),
            Some(&json!(true))
        );
    }

    #[test]
    fn recovery_blocks_occupied_rwop_but_allows_existing_container() {
        let pod = consumer();
        let original = original(&pod);
        let plan = recovery_plan(&[pod], &claim("ReadWriteOncePod"), &original);
        assert_eq!(plan.candidates.len(), 1);
        assert!(plan.helper.unwrap_err().contains("ReadWriteOncePod"));
        assert!(helper_options(&[], &claim("ReadWriteOncePod"), &original).is_ok());
    }

    #[test]
    fn recovery_refuses_unknown_subpath_or_ambiguous_rwo_node() {
        let mut pod = consumer();
        let mut original = original(&pod);
        original.sub_path = None;
        let plan = recovery_plan(&[pod.clone()], &claim("ReadWriteMany"), &original);
        assert!(plan.candidates.is_empty());
        assert!(plan.helper.unwrap_err().contains("subPathExpr"));
        original.sub_path = Some("tenant".into());
        pod.data["spec"].as_object_mut().unwrap().remove("nodeName");
        assert!(helper_options(&[pod.clone()], &claim("ReadWriteOnce"), &original).is_err());
        pod.data["spec"]["nodeName"] = json!("worker-b");
        assert!(helper_options(&[pod, consumer()], &claim("ReadWriteOnce"), &original).is_err());
    }

    #[test]
    fn recovery_limits_candidates_and_honors_claim_mount_readonly() {
        let mut pod = consumer();
        pod.data["spec"]["volumes"][0]["persistentVolumeClaim"]["readOnly"] = json!(true);
        assert!(
            find_mounts(&[pod.clone()], "data")
                .iter()
                .all(|m| m.read_only)
        );
        let original = original(&pod);
        let pods: Vec<_> = (0..30)
            .map(|i| {
                let mut p = pod.clone();
                p.metadata.name = Some(format!("pod-{i}"));
                p
            })
            .collect();
        assert_eq!(
            recovery_plan(&pods, &claim("ReadWriteMany"), &original)
                .candidates
                .len(),
            16
        );
    }

    #[test]
    fn missing_tool_checks_are_specific_and_report_before_listing() {
        for error in [
            "exec: \"sh\": executable file not found in $PATH",
            "missing browsing tool: ls",
            "missing browsing tool: head",
        ] {
            assert!(missing_listing_tools(error), "{error}");
        }
        for error in [
            "permission denied",
            "connection refused",
            "listing failed (exit 127)",
            "ls: file not found",
        ] {
            assert!(!missing_listing_tools(error), "{error}");
        }
        let probe = list_probe("/");
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &probe.script, "sh", "/", "/"])
            .env("PATH", "/sofka-test-no-tools")
            .output()
            .unwrap();
        let result = interpret_listing(
            &probe.nonce,
            out.status.code(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(result.unwrap_err(), "missing browsing tool: ls");
    }
}
