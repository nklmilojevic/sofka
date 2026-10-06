//! `e` in the decoded Secret view: edit the values as plain text.
//!
//! The text keys of `data` go to `$EDITOR` as a `stringData` document in a
//! private temp file. When the editor closes, the result is compared with what
//! was written, and only the keys that changed are patched back, base64
//! encoded. The patch carries the `resourceVersion` that was read, so a Secret
//! that changed in the meantime is refused instead of overwritten.
//!
//! The file holds plaintext values, so it is deleted as soon as the editor
//! closes, and [`sweep_abandoned`] removes what a crashed sofka left behind.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fs2::FileExt as _;

use super::*;

/// Every edit directory starts with this.
const DIR_PREFIX: &str = "sofka-secret-";

/// The file in an edit directory that its owner holds an exclusive lock on
/// for as long as the edit is open. The lock goes away with the process, so
/// a lock anyone can take means the edit is abandoned.
const LOCK_FILE: &str = "owner.lock";

/// An edit directory with no lock file yet is still being created, unless it
/// is older than this.
const UNLOCKED_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

const HEADER: &str = "\
# Values are plain text and are base64-encoded on save. Only changed keys
# are patched. Delete a key to remove it. Save the file unchanged to cancel.
";

/// A decoded Secret open in the editor.
pub(super) struct SecretEdit {
    dir: PathBuf,
    lock: Option<std::fs::File>,
    path: PathBuf,
    kind: Kind,
    object: DynamicObject,
    original: BTreeMap<String, String>,
    binary: BTreeSet<String>,
    written: String,
}

impl Drop for SecretEdit {
    fn drop(&mut self) {
        // Windows cannot remove a directory holding an open file.
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What an edit changes, by key name. Never holds values.
#[derive(Debug, Default, PartialEq)]
pub(super) struct SecretChanges {
    pub changed: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl SecretChanges {
    fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }

    /// Every key by name: the confirmation is where the operator checks
    /// what the patch touches, so nothing is abbreviated.
    fn summary(&self) -> String {
        [
            ("change", &self.changed),
            ("add", &self.added),
            ("remove", &self.removed),
        ]
        .into_iter()
        .filter(|(_, keys)| !keys.is_empty())
        .map(|(verb, keys)| format!("{verb} {}", keys.join(", ")))
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

/// Split a Secret's `data` into values that decode to text, which can be
/// edited, and the names of those that do not, which are left alone.
pub(super) fn split_secret_data(
    obj: &DynamicObject,
) -> (BTreeMap<String, String>, BTreeSet<String>) {
    let mut text = BTreeMap::new();
    let mut binary = BTreeSet::new();
    let data = obj.data.get("data").and_then(Value::as_object);
    for (key, value) in data.into_iter().flatten() {
        let decoded = value
            .as_str()
            .and_then(|b64| BASE64.decode(b64).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok());
        match decoded {
            Some(value) => {
                text.insert(key.clone(), value);
            }
            None => {
                binary.insert(key.clone());
            }
        }
    }
    (text, binary)
}

/// The document written to the editor.
pub(super) fn secret_edit_document(
    title: &str,
    original: &BTreeMap<String, String>,
    binary: &BTreeSet<String>,
) -> Result<String, String> {
    let mut doc = format!("# Secret {title}\n{HEADER}");
    if !binary.is_empty() {
        let keys = binary.iter().cloned().collect::<Vec<_>>().join(", ");
        doc.push_str(&format!("# Not text, not shown, left unchanged: {keys}\n"));
    }
    doc.push_str(
        &serde_yaml::to_string(&BTreeMap::from([("stringData", original)]))
            .map_err(|e| e.to_string())?,
    );
    Ok(doc)
}

/// Whether `key` is a valid Secret data key.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 253
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Read the edited document back. Every value has to be a YAML string, so a
/// bare `8080` or `true` is refused rather than silently stringified.
pub(super) fn parse_secret_edit(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    let root = match doc {
        serde_yaml::Value::Mapping(root) => root,
        serde_yaml::Value::Null => return Err("the document is empty".into()),
        _ => return Err("expected a stringData mapping".into()),
    };
    let mut values = BTreeMap::new();
    let mut seen = false;
    for (field, entries) in root {
        if field.as_str() != Some("stringData") {
            return Err(format!(
                "unexpected field {}: only stringData can be edited here",
                serde_yaml::to_string(&field).unwrap_or_default().trim_end()
            ));
        }
        seen = true;
        let entries = match entries {
            serde_yaml::Value::Mapping(entries) => entries,
            // An empty `stringData:` reads as null. Taking that as "remove
            // every key" would make a cleared block delete the Secret's data;
            // removing everything needs an explicit `{}`.
            _ => {
                return Err(
                    "stringData must be a mapping of keys to values; use {} to remove every key"
                        .into(),
                );
            }
        };
        for (key, value) in entries {
            let Some(key) = key.as_str() else {
                return Err("every key must be a string".into());
            };
            if !valid_key(key) {
                return Err(format!(
                    "invalid key '{key}': use letters, digits, '-', '_' or '.'"
                ));
            }
            let Some(value) = value.as_str() else {
                return Err(format!("the value of '{key}' must be a string; quote it"));
            };
            values.insert(key.to_string(), value.to_string());
        }
    }
    if !seen {
        return Err("expected a stringData mapping".into());
    }
    Ok(values)
}

/// Keys of `edited` that name a value left out of the document because it
/// is not text. Writing one would replace those bytes with text.
fn binary_overwrites(binary: &BTreeSet<String>, edited: &BTreeMap<String, String>) -> Vec<String> {
    edited
        .keys()
        .filter(|key| binary.contains(*key))
        .cloned()
        .collect()
}

pub(super) fn secret_changes(
    original: &BTreeMap<String, String>,
    edited: &BTreeMap<String, String>,
) -> SecretChanges {
    let mut changes = SecretChanges::default();
    for (key, value) in edited {
        match original.get(key) {
            Some(old) if old == value => {}
            Some(_) => changes.changed.push(key.clone()),
            None => changes.added.push(key.clone()),
        }
    }
    changes.removed = original
        .keys()
        .filter(|key| !edited.contains_key(*key))
        .cloned()
        .collect();
    changes
}

/// A merge patch for exactly the changed keys, pinned to `resource_version`
/// when there is one.
pub(super) fn secret_patch(
    changes: &SecretChanges,
    edited: &BTreeMap<String, String>,
    resource_version: &str,
) -> Value {
    let mut data = serde_json::Map::new();
    for key in changes.changed.iter().chain(&changes.added) {
        data.insert(key.clone(), json!(BASE64.encode(&edited[key])));
    }
    for key in &changes.removed {
        data.insert(key.clone(), Value::Null);
    }
    let mut patch = json!({"data": data});
    if !resource_version.is_empty() {
        patch["metadata"] = json!({"resourceVersion": resource_version});
    }
    patch
}

/// The command that opens `path` in the user's editor: `KUBE_EDITOR`, then
/// `EDITOR`, like `kubectl edit`.
pub(super) fn editor_argv(editor: Option<String>, path: &Path) -> Vec<String> {
    let path = path.to_string_lossy().into_owned();
    #[cfg(unix)]
    {
        // Through the shell, so `code --wait` and quoted paths work.
        let editor = editor.unwrap_or_else(|| "vi".into());
        vec![
            "sh".into(),
            "-c".into(),
            format!("{editor} \"$1\""),
            "sofka-editor".into(),
            path,
        ]
    }
    #[cfg(not(unix))]
    {
        let editor = editor.unwrap_or_else(|| "notepad".into());
        let mut argv: Vec<String> = editor.split_whitespace().map(String::from).collect();
        argv.push(path);
        argv
    }
}

fn configured_editor() -> Option<String> {
    ["KUBE_EDITOR", "EDITOR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
}

/// Create `path` readable only by this user and write `contents` to it. It
/// must not exist yet.
fn create_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

/// Create a directory only this user can read, holding one file with
/// `contents`. The file name is fixed: a Secret name can be longer than a
/// file name may be, or reserved on Windows (`con`).
fn private_file(contents: &str) -> std::io::Result<(PathBuf, std::fs::File, PathBuf)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "{DIR_PREFIX}{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir)?;
    let created = (|| {
        // Lock under another name and only then rename it into place: a
        // sweep must never find `owner.lock` before it is held, or it could
        // take the lock first and delete this edit.
        let staging = dir.join(format!("{LOCK_FILE}.new"));
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        lock.try_lock_exclusive()?;
        std::fs::rename(&staging, dir.join(LOCK_FILE))?;
        let path = dir.join("secret.yaml");
        create_private(&path, contents)?;
        Ok((lock, path))
    })();
    match created {
        Ok((lock, path)) => Ok((dir, lock, path)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// Whether nobody holds the edit directory `dir` open. Taking its lock
/// proves the owner is gone; the lock is dropped again before returning.
fn abandoned(dir: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(LOCK_FILE))
    {
        Ok(lock) => lock.try_lock_exclusive().is_ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > UNLOCKED_GRACE),
        Err(_) => false,
    }
}

/// Remove edit directories in `root` left by a sofka that crashed or was
/// killed with the editor open. An edit whose owner still runs keeps its
/// lock and is left alone however old it is. Another user's directories
/// cannot be opened and are skipped.
pub(super) fn sweep_abandoned_in(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let is_edit = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(DIR_PREFIX));
        if !is_edit || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        if abandoned(&dir) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// [`sweep_abandoned_in`] over the system temp directory.
pub fn sweep_abandoned() {
    sweep_abandoned_in(&std::env::temp_dir());
}

impl App {
    /// Open the fresh copy of the displayed Secret in the editor.
    pub(super) fn edit_decoded_secret(&mut self, fresh: DynamicObject) {
        let Some(kind) = self.document_source.as_ref().map(|s| s.kind.clone()) else {
            return;
        };
        if fresh.data.get("immutable").and_then(Value::as_bool) == Some(true) {
            self.flash_warn("secret is immutable — it cannot be edited");
            return;
        }
        let name = fresh.metadata.name.clone().unwrap_or_default();
        let ns = fresh.metadata.namespace.clone().unwrap_or_default();
        if self
            .guard(
                "secret-edit",
                "secrets",
                &[(name.clone(), ns.clone())],
                ConfirmLevel::Plain,
            )
            .is_none()
        {
            return;
        }
        let (original, binary) = split_secret_data(&fresh);
        let title = if ns.is_empty() {
            name.clone()
        } else {
            format!("{name} in {ns}")
        };
        let written = match secret_edit_document(&title, &original, &binary) {
            Ok(doc) => doc,
            Err(e) => {
                self.flash_warn(&format!("cannot edit: {e}"));
                return;
            }
        };
        let (dir, lock, path) = match private_file(&written) {
            Ok(paths) => paths,
            Err(e) => {
                self.flash_warn(&format!("cannot edit: temp file: {e}"));
                return;
            }
        };
        self.pending = Some(Suspend::Shell(editor_argv(configured_editor(), &path)));
        self.secret_edit = Some(SecretEdit {
            dir,
            lock: Some(lock),
            path,
            kind,
            object: fresh,
            original,
            binary,
            written,
        });
    }

    /// The editor closed: read the result, delete the file, and confirm the
    /// patch. A document that does not parse goes back to the editor with
    /// the error on top, like `kubectl edit`.
    pub(super) fn finish_secret_edit(&mut self) {
        let Some(edit) = self.secret_edit.take() else {
            return;
        };
        if self.command_failure.is_some() {
            self.flash_warn("editor failed — secret not changed");
            return;
        }
        let bytes = std::fs::read(&edit.path);
        let _ = std::fs::remove_file(&edit.path);
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) => {
                self.flash_warn(&format!("secret not changed: {e}"));
                return;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                let text = String::from_utf8_lossy(e.as_bytes()).into_owned();
                self.reopen_secret_edit(edit, &text, "the file is not valid UTF-8");
                return;
            }
        };
        if text == edit.written {
            self.set_flash("secret not changed");
            return;
        }
        let edited = match parse_secret_edit(&text) {
            Ok(edited) => edited,
            Err(error) => {
                self.reopen_secret_edit(edit, &text, &error);
                return;
            }
        };
        let overwrites = binary_overwrites(&edit.binary, &edited);
        if !overwrites.is_empty() {
            let error = format!(
                "{} not text and cannot be edited here; remove it from stringData",
                overwrites.join(", ")
            );
            self.reopen_secret_edit(edit, &text, &error);
            return;
        }
        let changes = secret_changes(&edit.original, &edited);
        if changes.is_empty() {
            self.set_flash("secret not changed");
            return;
        }
        let name = edit.object.metadata.name.clone().unwrap_or_default();
        let ns = edit.object.metadata.namespace.clone().unwrap_or_default();
        let rv = edit
            .object
            .metadata
            .resource_version
            .clone()
            .unwrap_or_default();
        let patch = secret_patch(&changes, &edited, &rv);
        let summary = changes.summary();
        let Some(level) = self.guard(
            "secret-edit",
            "secrets",
            &[(name.clone(), ns.clone())],
            ConfirmLevel::Plain,
        ) else {
            return;
        };
        let where_ns = if ns.is_empty() {
            String::new()
        } else {
            format!(" in {ns}")
        };
        let managed = managed_by(&edit.object)
            .map(|owner| format!("⚠ Managed by {owner} — it may overwrite this change."));
        let kind = edit.kind.clone();
        let update = format!("Update secret {name}{where_ns}: {summary}?");
        // A long key list scrolls, so the warning goes where the dialog
        // opens: first in the y/n confirmation, which opens at the top.
        let label = match &managed {
            Some(warning) => format!("{warning}\n\n{update}"),
            None => update.clone(),
        };
        self.begin_guarded(
            ConfirmAction::SecretEdit {
                kind,
                name: name.clone(),
                ns,
                patch,
            },
            label,
            level,
            name,
        );
        if matches!(self.mode, Mode::Confirm | Mode::Prompt) {
            self.confirm_return = Mode::Detail;
        }
        // A typed guardrail prompt only says what to type. Every key goes
        // above that, and the warning right above what to type. The prompt
        // opens scrolled to the bottom, as it does while typing, so the
        // warning, what to type, and the input are in view however many keys
        // there are.
        if self.mode == Mode::Prompt {
            let warning = managed.map(|w| format!("{w}\n\n")).unwrap_or_default();
            self.prompt_label = format!("{update}\n\n{warning}{}", self.prompt_label);
            self.popup_scroll = usize::MAX;
        }
    }

    fn reopen_secret_edit(&mut self, mut edit: SecretEdit, text: &str, error: &str) {
        let kept: String = text
            .lines()
            .skip_while(|line| line.starts_with("# error:"))
            .map(|line| format!("{line}\n"))
            .collect();
        let error = error.replace('\n', " ");
        let written = format!("# error: {error}\n{kept}");
        if let Err(e) = create_private(&edit.path, &written) {
            self.flash_warn(&format!("secret not changed: {e}"));
            return;
        }
        edit.written = written;
        self.flash_warn(&format!("secret not changed: {error}"));
        self.pending = Some(Suspend::Shell(editor_argv(configured_editor(), &edit.path)));
        self.secret_edit = Some(edit);
    }

    pub(super) fn apply_secret_edit(&mut self, kind: Kind, name: String, ns: String, patch: Value) {
        if self.deny_readonly() {
            return;
        }
        let label = if ns.is_empty() {
            name.clone()
        } else {
            format!("{name} in {ns}")
        };
        self.note_action("secret-edit", label);
        let claim = self.claim_status(format!("updating secret {name}…"));
        let client = self.cluster.client.clone();
        let tx = self.tx.clone();
        let generation = self.generation;
        tokio::spawn(async move {
            let api: Api<DynamicObject> = if kind.namespaced && !ns.is_empty() {
                Api::namespaced_with(client, &ns, &kind.ar)
            } else {
                Api::all_with(client, &kind.ar)
            };
            let result = api
                .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .map(|_| format!("secret {name} updated"))
                .map_err(|e| match e {
                    kube::Error::Api(status) if status.code == 409 => format!(
                        "secret {name} changed since it was read — not updated; press e to edit it again"
                    ),
                    e => format!("secret update failed: {e}"),
                });
            let _ = tx
                .send(Msg::SecretEditApplied {
                    generation,
                    claim,
                    result,
                })
                .await;
        });
    }

    pub(super) fn secret_edit_applied(
        &mut self,
        claim: StatusClaim,
        result: Result<String, String>,
    ) {
        match result {
            Ok(message) => {
                self.set_claimed_status(claim, message, false);
                let decoded = self
                    .document_source
                    .as_ref()
                    .is_some_and(|s| matches!(s.view, refresh::RefreshView::DecodedSecret));
                if decoded {
                    let flash = (self.flash.clone(), self.flash_err);
                    self.reload_document();
                    (self.flash, self.flash_err) = flash;
                }
            }
            Err(message) => self.set_claimed_status(claim, message, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(data: Value) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {"name": "creds", "namespace": "default"},
            "data": data,
        }))
        .unwrap()
    }

    fn b64(s: &[u8]) -> String {
        BASE64.encode(s)
    }

    #[test]
    fn document_round_trips_every_text_value() {
        let obj = secret(json!({
            "plain": b64(b"hunter2"),
            "number": b64(b"8080"),
            "flag": b64(b"true"),
            "empty": b64(b""),
            "multi": b64(b"line one\nline two\n"),
            "spaced": b64(b"  padded  "),
            "binary": b64(&[0xff, 0xfe, 0x00]),
        }));
        let (original, binary) = split_secret_data(&obj);
        assert_eq!(binary, BTreeSet::from(["binary".to_string()]));
        let doc = secret_edit_document("creds in default", &original, &binary).unwrap();
        assert!(doc.starts_with("# Secret creds in default\n"), "{doc}");
        assert!(doc.contains("left unchanged: binary"), "{doc}");
        assert_eq!(parse_secret_edit(&doc).unwrap(), original);
        assert!(secret_changes(&original, &original).is_empty());
    }

    #[test]
    fn changes_name_only_the_keys_that_moved() {
        let original = BTreeMap::from([
            ("keep".to_string(), "a".to_string()),
            ("change".to_string(), "b".to_string()),
            ("drop".to_string(), "c".to_string()),
        ]);
        let edited = BTreeMap::from([
            ("keep".to_string(), "a".to_string()),
            ("change".to_string(), "B".to_string()),
            ("new".to_string(), "d".to_string()),
            ("other".to_string(), "e".to_string()),
        ]);
        let changes = secret_changes(&original, &edited);
        assert_eq!(
            changes,
            SecretChanges {
                changed: vec!["change".into()],
                added: vec!["new".into(), "other".into()],
                removed: vec!["drop".into()],
            }
        );
        assert_eq!(
            changes.summary(),
            "change change · add new, other · remove drop"
        );
        let binary = BTreeSet::from(["cert".to_string(), "other".to_string()]);
        assert_eq!(binary_overwrites(&binary, &edited), ["other"]);
        let patch = secret_patch(&changes, &edited, "42");
        assert_eq!(
            patch,
            json!({
                "metadata": {"resourceVersion": "42"},
                "data": {
                    "change": b64(b"B"),
                    "new": b64(b"d"),
                    "other": b64(b"e"),
                    "drop": null,
                },
            })
        );
    }

    #[test]
    fn parse_refuses_values_that_are_not_strings() {
        let err = parse_secret_edit("stringData:\n  port: 8080\n").unwrap_err();
        assert!(err.contains("'port' must be a string"), "{err}");
        let err = parse_secret_edit("stringData:\n").unwrap_err();
        assert!(err.contains("use {} to remove every key"), "{err}");
        assert!(parse_secret_edit("stringData: null\n").is_err());
        assert!(parse_secret_edit("# nothing\n").is_err());
        assert!(parse_secret_edit("{}\n").is_err());
        let err = parse_secret_edit("stringData:\n  empty:\n").unwrap_err();
        assert!(err.contains("'empty' must be a string"), "{err}");
        let err = parse_secret_edit("data:\n  a: b\n").unwrap_err();
        assert!(err.contains("only stringData"), "{err}");
        let err = parse_secret_edit("stringData:\n  'bad key': x\n").unwrap_err();
        assert!(err.contains("invalid key"), "{err}");
        assert!(parse_secret_edit("stringData: [\n").is_err());
        assert_eq!(
            parse_secret_edit("# all gone\nstringData: {}\n").unwrap(),
            BTreeMap::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn editor_runs_through_the_shell_with_the_path_as_an_argument() {
        let argv = editor_argv(Some("code --wait".into()), Path::new("/tmp/a b/creds.yaml"));
        assert_eq!(
            argv,
            [
                "sh",
                "-c",
                "code --wait \"$1\"",
                "sofka-editor",
                "/tmp/a b/creds.yaml"
            ]
        );
        assert_eq!(editor_argv(None, Path::new("/x"))[2], "vi \"$1\"");
    }

    #[cfg(unix)]
    #[test]
    fn temp_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _lock, path) = private_file("stringData: {}\n").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(path.file_name().unwrap(), "secret.yaml");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sweep_removes_only_edits_nobody_holds() {
        let root = std::env::temp_dir().join(format!(
            "sofka-sweep-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let edit = |name: &str| {
            let dir = root.join(format!("{DIR_PREFIX}{name}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join(LOCK_FILE), "").unwrap();
            std::fs::write(dir.join("secret.yaml"), "stringData: {}\n").unwrap();
            dir
        };
        let gone = edit("gone");
        let live = edit("live");
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(live.join(LOCK_FILE))
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        // Being created right now: no lock file yet.
        let starting = root.join(format!("{DIR_PREFIX}starting"));
        std::fs::create_dir(&starting).unwrap();
        let other = root.join("unrelated");
        std::fs::create_dir(&other).unwrap();

        sweep_abandoned_in(&root);
        assert!(!gone.exists());
        assert!(
            live.join("secret.yaml").exists(),
            "a held edit is never swept"
        );
        assert!(starting.exists());
        assert!(other.exists());

        drop(lock);
        sweep_abandoned_in(&root);
        assert!(!live.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_summaries_name_every_key() {
        let changes = SecretChanges {
            changed: (0..20).map(|i| format!("key-number-{i}")).collect(),
            added: vec!["new".into()],
            removed: vec![],
        };
        let summary = changes.summary();
        assert!(summary.contains("key-number-0, "), "{summary}");
        assert!(summary.contains("key-number-19 · add new"), "{summary}");
    }

    #[test]
    fn a_new_edit_is_locked_before_its_lock_file_appears() {
        let (dir, lock, _path) = private_file("stringData: {}\n").unwrap();
        assert!(!dir.join(format!("{LOCK_FILE}.new")).exists());
        assert!(!abandoned(&dir), "the owner holds the lock");
        drop(lock);
        assert!(abandoned(&dir));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
