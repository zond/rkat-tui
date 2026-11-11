//! The messages the agent never got: saved on quit to
//! `<realm>/tui_unsent.txt`, announced at the next start.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ui::delivery::PendingId;

/// File in the realm directory that collects unsent messages.
pub const UNSENT_FILE: &str = "tui_unsent.txt";

/// Line written in front of every saved message.
const UNSENT_HEADER: &str = "--- unsent message ---";

/// Where unsent messages were saved on quit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedUnsent {
    /// Messages written by this shutdown.
    pub count: usize,
    /// The file they were appended to.
    pub path: PathBuf,
}

/// Append the `held` messages the agent did not get (`delivered` names
/// those it did) to the realm's [`UNSENT_FILE`]; where, when there were any.
pub(super) fn save_unsent(
    realm_root: &Path,
    held: Vec<(Option<PendingId>, String)>,
    delivered: &[PendingId],
) -> Option<SavedUnsent> {
    let unsent = unsent_texts(held, delivered);
    if unsent.is_empty() {
        return None;
    }
    let path = realm_root.join(UNSENT_FILE);
    match append_unsent(&path, &unsent) {
        Ok(()) => Some(SavedUnsent {
            count: unsent.len(),
            path,
        }),
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "could not save unsent messages");
            None
        }
    }
}

/// Texts of the held messages the runtime did not deliver, oldest first
/// (the Ctrl-X Ctrl-S prompt, which has no id, last).
fn unsent_texts(
    mut held: Vec<(Option<PendingId>, String)>,
    delivered: &[PendingId],
) -> Vec<String> {
    held.retain(|(id, _)| id.is_none_or(|id| !delivered.contains(&id)));
    held.sort_by_key(|(id, _)| id.map_or(u64::MAX, |id| id.0));
    held.into_iter().map(|(_, text)| text).collect()
}

/// Append `texts` to the unsent-messages file, each after a header line.
fn append_unsent(path: &Path, texts: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for text in texts {
        writeln!(file, "{UNSENT_HEADER}")?;
        writeln!(file, "{}", text.trim_end())?;
    }
    file.flush()
}

/// Number of messages in an unsent-messages file.
#[must_use]
pub(crate) fn count_unsent(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|content| content.lines().filter(|l| *l == UNSENT_HEADER).count())
        .unwrap_or(0)
}

/// Start-up notice when earlier unsent messages are waiting in the realm.
#[must_use]
pub(super) fn unsent_notice(realm_root: &Path) -> Option<String> {
    let path = realm_root.join(UNSENT_FILE);
    match count_unsent(&path) {
        0 => None,
        n => Some(format!(
            "{n} unsent message{} from an earlier run saved in {} (delete the file once handled)",
            if n == 1 { "" } else { "s" },
            path.display()
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn unsent_file_appends_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        assert!(unsent_notice(dir.path()).is_none());
        let path = dir.path().join(UNSENT_FILE);
        append_unsent(&path, &["one".into(), "two\nlines\n".into()]).unwrap();
        append_unsent(&path, &["three".into()]).unwrap();
        assert_eq!(count_unsent(&path), 3);
        let notice = unsent_notice(dir.path()).unwrap();
        assert!(notice.starts_with("3 unsent messages"), "{notice}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("two\nlines\n---"), "{content}");
    }

    #[test]
    fn delivered_messages_are_not_saved() {
        let held = vec![
            (None, "next".to_string()),
            (Some(PendingId(2)), "two".to_string()),
            (Some(PendingId(1)), "one".to_string()),
        ];
        assert_eq!(
            unsent_texts(held, &[PendingId(2)]),
            vec!["one".to_string(), "next".to_string()]
        );
    }
}
