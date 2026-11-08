//! Catch-up after an attach or a reconnect, and the one rule that keeps the
//! replay and the live stream from showing an event twice.
//!
//! Every event of the orchestrator's session carries the session's durable
//! sequence: the live stream's envelope `seq` (not the notification's
//! per-stream `sequence`) and the replay's `cursor.sequence` are the same
//! number for the same event, and it keeps counting across daemon restarts
//! (probed, see `docs/evidence.md`). So the client keeps one high-water mark
//! per session ([`Mark`]): an event is shown only when its sequence is above
//! the mark, which then moves to it. The live stream is opened *before* the
//! replay is read, so nothing falls in the gap between them; whatever the
//! two deliver twice is dropped by the mark, whichever arrives first.
//!
//! The mark is saved per workspace in `<workspace>/.rkat/tui/attach.cursor`
//! ([`CursorFile`]) after every event shown, so a later `rkat-tui --attach`
//! replays only what this workspace's client has not shown yet.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The cursor file's name under `<workspace>/.rkat/tui/`.
pub const CURSOR_NAME: &str = "attach.cursor";

/// The last event shown, by session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The orchestrator's session.
    pub session_id: String,
    /// The durable sequence of the last event shown.
    pub sequence: u64,
}

/// The cursor file of a workspace.
#[derive(Debug, Clone)]
pub struct CursorFile {
    root: PathBuf,
    path: PathBuf,
}

impl CursorFile {
    /// The cursor file of the workspace at `context_root`.
    #[must_use]
    pub fn of_workspace(context_root: &Path) -> Self {
        Self {
            root: context_root.to_path_buf(),
            path: context_root.join(".rkat").join("tui").join(CURSOR_NAME),
        }
    }

    /// Its path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved cursor (`None`: none, or unreadable).
    #[must_use]
    pub fn load(&self) -> Option<Cursor> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(text.trim()).ok()
    }

    /// Save `cursor` (best effort: a failure is logged, the session goes
    /// on and the next attach replays more).
    pub fn save(&self, cursor: &Cursor) {
        let Ok(mut line) = serde_json::to_string(cursor) else {
            return;
        };
        line.push('\n');
        if let Err(err) = crate::paths::write_atomic(
            &self.path,
            line.as_bytes(),
            crate::paths::FileMode::Private,
            Some(&self.root),
        ) {
            tracing::warn!(path = %self.path.display(), error = %err, "cannot save the attach cursor");
        }
    }
}

/// The high-water mark of one session (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    session_id: String,
    sequence: u64,
}

impl Mark {
    /// The mark for `session_id`: the saved cursor's when it is for that
    /// session, else nothing shown yet.
    #[must_use]
    pub fn resume(session_id: &str, saved: Option<&Cursor>) -> Self {
        let sequence = saved
            .filter(|cursor| cursor.session_id == session_id)
            .map_or(0, |cursor| cursor.sequence);
        Self {
            session_id: session_id.to_string(),
            sequence,
        }
    }

    /// The session.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The sequence of the last event shown (0: none).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether the event with `sequence` is new; if so the mark moves to it.
    pub fn admit(&mut self, sequence: u64) -> bool {
        if sequence > self.sequence {
            self.sequence = sequence;
            true
        } else {
            false
        }
    }

    /// The cursor to save.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor {
            session_id: self.session_id.clone(),
            sequence: self.sequence,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_replay_and_the_live_stream_show_each_event_once() {
        let mut mark = Mark::resume("s", None);
        // The live stream (opened first) delivers 5 and 6 while the replay
        // still reads 3..=6: each event is shown once, in either order.
        let live = [5, 6];
        let replay = [3, 4, 5, 6];
        let mut shown = Vec::new();
        for seq in live
            .iter()
            .take(1)
            .chain(&replay)
            .chain(live.iter().skip(1))
        {
            if mark.admit(*seq) {
                shown.push(*seq);
            }
        }
        // 5 came live before 3 and 4 were replayed: those are older than
        // what was shown, so the live event wins and the older ones are
        // dropped.
        assert_eq!(shown, vec![5, 6]);
        assert_eq!(mark.sequence(), 6);
        // The usual order: replay first, then the live tail.
        let mut mark = Mark::resume("s", None);
        let shown: Vec<u64> = [1, 2, 3, 2, 3, 4]
            .into_iter()
            .filter(|seq| mark.admit(*seq))
            .collect();
        assert_eq!(shown, vec![1, 2, 3, 4]);
    }

    #[test]
    fn the_cursor_survives_per_workspace_and_only_for_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let file = CursorFile::of_workspace(dir.path());
        assert_eq!(file.load(), None);
        let mut mark = Mark::resume("s1", None);
        assert!(mark.admit(7));
        file.save(&mark.cursor());
        assert_eq!(
            file.path(),
            dir.path().join(".rkat").join("tui").join(CURSOR_NAME)
        );
        let saved = file.load().unwrap();
        assert_eq!(Mark::resume("s1", Some(&saved)).sequence(), 7);
        // Another session (the daemon's orchestrator was replaced) starts
        // from the beginning.
        assert_eq!(Mark::resume("s2", Some(&saved)).sequence(), 0);
    }
}
