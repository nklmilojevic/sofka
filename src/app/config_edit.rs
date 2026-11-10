//! `:config edit [base|cluster|context]`: open one level's config file in
//! the editor and reload the config when the editor closes.
//!
//! sofka never writes settings itself. The file stays the user's, so a file
//! sofka cannot write in place (a symlink, or one without write access, as
//! Home Manager leaves it) is refused instead of edited somewhere it would be
//! replaced.

use std::path::{Path, PathBuf};

use super::secret_edit::{configured_editor, create_private, editor_argv};
use super::*;

/// The palette words after `:config`, in completion order.
pub(super) const SUBCOMMANDS: &[&str] = &["edit"];
pub(super) const LEVELS: &[&str] = &["base", "cluster", "context"];

const USAGE: &str = "usage: :config [edit [base|cluster|context]]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Base,
    Cluster,
    Context,
}

impl Level {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "base" => Some(Self::Base),
            "cluster" => Some(Self::Cluster),
            "context" => Some(Self::Context),
            _ => None,
        }
    }
}

/// The file to open for the level in `dir`. A missing file is created empty,
/// with its directories, so the editor has somewhere to save. Only its owner
/// can read it: config can hold credentials, such as provider headers.
fn editable_file(dir: &Path) -> Result<PathBuf, String> {
    let path = crate::config::level_file(dir).map_err(|e| format!("{e} (:config for details)"))?;
    let managed = || {
        format!(
            "{} is managed (a symlink or read-only); edit it at its source",
            path.display()
        )
    };
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => Err(managed()),
        Ok(_) => match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(_) => Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(managed()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)
                .and_then(|()| create_private(&path, ""))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(path)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Which word of a `:config …` palette line is being typed: 1 for the
/// subcommand, 2 for the level.
pub(super) fn config_word(command: &str) -> usize {
    let words = command.split_whitespace().count();
    if command.ends_with(char::is_whitespace) {
        words
    } else {
        words.saturating_sub(1)
    }
}

impl App {
    /// `:config <args>`: `edit [level]` opens that level's file for the
    /// current context in the editor.
    pub(super) fn run_config_command(&mut self, args: &str) {
        let mut words = args.split_whitespace();
        if words.next() != Some("edit") {
            self.flash_warn(USAGE);
            return;
        }
        let level = match words.next() {
            None => Level::Base,
            Some(word) => match Level::parse(word) {
                Some(level) => level,
                None => {
                    self.flash_warn(USAGE);
                    return;
                }
            },
        };
        if words.next().is_some() {
            self.flash_warn(USAGE);
            return;
        }
        let context = self.cluster.context.clone();
        let cluster = self.cluster.cluster_name.clone();
        let dir = match level {
            Level::Base => self
                .config
                .dir()
                .map(Path::to_path_buf)
                .ok_or("no config directory: set XDG_CONFIG_HOME or HOME"),
            Level::Cluster => self
                .config
                .cluster_dir(&cluster)
                .ok_or("cluster config needs a config directory and a kubeconfig cluster"),
            Level::Context => self
                .config
                .context_dir(&context, &cluster)
                .ok_or("context config needs a config directory and a kubeconfig context"),
        };
        let path = match dir
            .map_err(String::from)
            .and_then(|dir| editable_file(&dir))
        {
            Ok(path) => path,
            Err(e) => {
                self.flash_warn(&format!("cannot edit config: {e}"));
                return;
            }
        };
        self.pending = Some(Suspend::Shell(editor_argv(configured_editor(), &path)));
        self.config_edit = true;
    }

    /// The editor closed: apply the file like `:reload`. An editor that
    /// failed (or was quit with an error, like vim's `:cq`) leaves the
    /// config as it was.
    pub(super) fn finish_config_edit(&mut self) {
        if !std::mem::take(&mut self.config_edit) {
            return;
        }
        if self.command_failure.is_some() {
            self.flash_warn("editor failed — config not reloaded (:reload applies saved changes)");
            return;
        }
        self.reload_config();
    }

    /// Palette completions for `:config <subcommand> <level>`.
    pub(super) fn suggest_config_args(&mut self) {
        let slot = config_word(&self.command);
        let words: Vec<&str> = self.command.split_whitespace().collect();
        let candidates: &[&str] = match slot {
            1 => SUBCOMMANDS,
            2 if words.get(1) == Some(&"edit") => LEVELS,
            _ => &[],
        };
        let arg = words.get(slot).copied().unwrap_or("");
        let mut scored: Vec<(i64, &str)> = candidates
            .iter()
            .filter_map(|&name| {
                let score = if arg.is_empty() {
                    0
                } else if name == arg {
                    i64::MAX
                } else {
                    self.matcher.score(name, arg)?
                };
                Some((score, name))
            })
            .collect();
        // Levels keep their documented order when nothing is typed.
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        self.cmd_suggestions = scored
            .into_iter()
            .map(|(_, label)| Suggestion {
                label: label.to_string(),
                kind: SuggestKind::ConfigArg,
            })
            .collect();
        self.cmd_sel = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_word_counts_the_word_being_typed() {
        assert_eq!(config_word("config "), 1);
        assert_eq!(config_word("config ed"), 1);
        assert_eq!(config_word("config edit "), 2);
        assert_eq!(config_word("config edit clu"), 2);
    }

    #[test]
    fn levels_parse_only_their_names() {
        assert_eq!(Level::parse("base"), Some(Level::Base));
        assert_eq!(Level::parse("cluster"), Some(Level::Cluster));
        assert_eq!(Level::parse("context"), Some(Level::Context));
        assert_eq!(Level::parse("ctx"), None);
    }
}
