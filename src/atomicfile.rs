//! Crash-safe replacement of the small state files sofka rewrites.
//!
//! Sort, namespace, and fleet choices are rewritten whole every time one
//! changes. Writing them in place is not safe: `File::create` truncates the
//! target before the new bytes land, so a crash, a power loss, or a second
//! sofka writing the same file can leave a half-written mix that `load()`
//! then silently discards as unparsable — the user's remembered choices gone
//! because the process died at the wrong microsecond. Renaming a finished
//! temp file over the target has no such window.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Separates concurrent writes from this process; see [`temp_path`].
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` as one indivisible replacement, creating the
/// parent directory if it is missing.
///
/// The bytes go to a sibling temp file that is flushed and then renamed over
/// the target. `rename` replaces atomically, so every reader — another sofka,
/// a `cat`, this process on its next launch — sees either the whole previous
/// file or the whole new one, never a tear.
pub fn write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let temp = temp_path(path);
    write_then_rename(&temp, path, contents).map_err(|e| {
        // A failed attempt must not litter the state directory with a
        // `sort.toml.tmp…` nobody will ever clean up.
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })
}

fn write_then_rename(temp: &Path, path: &Path, contents: &str) -> std::io::Result<()> {
    let mut file = std::fs::File::create(temp)?;
    file.write_all(contents.as_bytes())?;
    // Without this the rename can reach disk before the bytes it publishes,
    // so a power loss leaves the target pointing at a zero-filled file —
    // exactly the tear the rename is here to prevent. The directory entry is
    // deliberately left unsynced: if the rename itself is lost, the previous
    // complete file survives, which is a fine outcome for remembered UI state
    // and saves a second flush on every keystroke that changes a sort.
    file.sync_all()?;
    drop(file);
    std::fs::rename(temp, path)
}

/// A sibling of `path` — `rename` is only atomic within one filesystem, so the
/// temp file cannot live in `/tmp` — that no concurrent writer can collide
/// with: another sofka has another pid, and the counter separates writes
/// racing inside this process.
fn temp_path(path: &Path) -> PathBuf {
    let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(path.as_os_str());
    name.push(format!(".tmp{}.{n}", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sofka-atomicfile-{}-{tag}-{}",
            std::process::id(),
            NEXT_TEMP.load(Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn creates_missing_parents_and_replaces_in_place() {
        let dir = scratch("replace");
        let path = dir.join("nested").join("sort.toml");
        write(&path, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        write(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = scratch("cleanup");
        let path = dir.join("sort.toml");
        write(&path, "a").unwrap();
        write(&path, "bb").unwrap();
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![OsString::from("sort.toml")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_write_keeps_the_old_file_and_cleans_up() {
        let dir = scratch("failure");
        // Renaming a file over a directory fails, which stands in for any
        // mid-write failure: the point is that the target is untouched.
        let good = dir.join("sort.toml");
        write(&good, "kept").unwrap();
        let blocked = dir.join("blocked.toml");
        std::fs::create_dir(&blocked).unwrap();

        assert!(write(&blocked, "never lands").is_err());
        assert_eq!(std::fs::read_to_string(&good).unwrap(), "kept");
        assert!(blocked.is_dir(), "target survives the failed write");
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![OsString::from("blocked.toml"), OsString::from("sort.toml")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_paths_are_siblings_and_unique() {
        let path = Path::new("/var/state/sofka/sort.toml");
        let a = temp_path(path);
        let b = temp_path(path);
        assert_ne!(a, b);
        assert_eq!(a.parent(), path.parent());
        assert_eq!(b.parent(), path.parent());
    }
}
