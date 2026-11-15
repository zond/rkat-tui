//! The two files rkat-tui writes on the user's command: the user's own
//! `tui.toml` and the workspace's realm `tui.toml`, each through the one
//! writer ([`super::write::SettingsFile`]).

use std::path::PathBuf;

use super::registry::Level;
use super::write::SettingsFile;

/// The user's `~/.rkat/tui/tui.toml` and the workspace's realm
/// `tui.toml`: what the daemon writes for a `/set`, a mode change, a rule,
/// an "always" grant or statement. Clones share each file's lock.
#[derive(Debug, Clone)]
pub struct SettingsFiles {
    /// The user's own file (every workspace).
    pub user: SettingsFile,
    /// The workspace's realm file.
    pub workspace: SettingsFile,
}

impl SettingsFiles {
    /// The user's file at `user` and the realm's at `workspace`.
    #[must_use]
    pub fn new(user: PathBuf, workspace: PathBuf) -> Self {
        Self {
            user: SettingsFile::new(Level::User, user),
            workspace: SettingsFile::new(Level::Workspace, workspace),
        }
    }

    /// The file of `level`: the user's for [`Level::User`], the
    /// workspace's for every other (a project's file is never written).
    #[must_use]
    pub fn of(&self, level: Level) -> &SettingsFile {
        match level {
            Level::User => &self.user,
            _ => &self.workspace,
        }
    }
}
