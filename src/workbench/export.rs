//! `/export [file]`: the transcript as plain text, written by the TUI
//! itself (not a tool write: no approval and no sealed-path check, like the
//! history and cost files).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::Workbench;
use crate::paths::{FileMode, write_atomic};
use crate::ui::NoticeLevel;

/// Where `/export` writes: `typed` (relative to `workspace`), or
/// `<realm>/export-<session>-<unix seconds>.txt` in the workspace's realm.
fn export_path(
    workspace: &Path,
    realm: &Path,
    typed: Option<&str>,
    session: &str,
    now: SystemTime,
) -> PathBuf {
    match typed.map(str::trim).filter(|t| !t.is_empty()) {
        Some(typed) => {
            let typed = match typed.strip_prefix("~/") {
                Some(rest) => {
                    dirs::home_dir().map_or_else(|| PathBuf::from(typed), |h| h.join(rest))
                }
                None => PathBuf::from(typed),
            };
            if typed.is_absolute() {
                typed
            } else {
                workspace.join(typed)
            }
        }
        None => {
            let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            realm.join(format!("export-{session}-{secs}.txt"))
        }
    }
}

impl Workbench {
    /// [`crate::ui::Effect::Export`]: write `text` and say where (the
    /// default file name carries `session`).
    pub fn export(&self, typed: Option<&str>, text: &str, session: &str) {
        let realm = self.scope.realm_root();
        let path = export_path(
            &self.scope.context_root,
            &realm,
            typed,
            session,
            SystemTime::now(),
        );
        // The default lies in the realm the TUI already trusts; a typed
        // path is the user's own choice, so only its file is checked.
        let trusted = if typed.is_some() {
            path.parent().map(Path::to_path_buf)
        } else {
            Some(realm)
        };
        match write_atomic(
            &path,
            text.as_bytes(),
            FileMode::Private,
            trusted.as_deref(),
        ) {
            Ok(()) => self.notice(
                NoticeLevel::Info,
                format!("exported the transcript to {}", path.display()),
            ),
            Err(err) => self.notice(NoticeLevel::Error, format!("export failed: {err}")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_default_export_path_is_in_the_workspace_realm() {
        let ws = Path::new("/w");
        let realm = Path::new("/c/workspaces/ab");
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            export_path(ws, realm, None, "abc", now),
            PathBuf::from("/c/workspaces/ab/export-abc-1700000000.txt")
        );
        assert_eq!(
            export_path(ws, realm, Some("out/chat.txt"), "abc", now),
            PathBuf::from("/w/out/chat.txt")
        );
        assert_eq!(
            export_path(ws, realm, Some("/tmp/chat.txt"), "abc", now),
            PathBuf::from("/tmp/chat.txt")
        );
        assert_eq!(
            export_path(ws, realm, Some("  "), "abc", now),
            PathBuf::from("/c/workspaces/ab/export-abc-1700000000.txt")
        );
    }

    #[test]
    fn an_export_round_trips_through_the_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = export_path(dir.path(), dir.path(), None, "s1", SystemTime::now());
        write_atomic(&path, b"## user\nhi\n", FileMode::Private, Some(dir.path())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "## user\nhi\n");
    }
}
