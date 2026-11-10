//! Reattach through the member stream's cursor, and the one rule that keeps
//! an event from showing twice.
//!
//! Every member `mob/stream_event` names the sequence space (`epoch`) of its
//! envelope's `seq`, and `mob/stream_open` takes a cursor `{epoch, seq}`:
//! the stream replays the events the member still retains after it, then is
//! live, with no gap and no duplicate between the two. The client keeps the
//! last position it showed per session ([`Mark`]: session, epoch, sequence)
//! and reattaches with it. An event is shown only when it is past the mark
//! (a later sequence of the same epoch, or any event of another epoch, whose
//! sequences may restart), which then moves to it.
//!
//! A cursor of another epoch (the daemon restarted: the member's events
//! live in a new space, and the old space's tail since the client left is
//! gone) replays from the space's earliest retained event, which is not the
//! start of the conversation. The client therefore does not resume a cursor
//! of a daemon that is not the one it reads from the endpoint file (the
//! cursor records a digest of the daemon's token, [`daemon_id`]), nor one that has no epoch yet, nor one
//! the stream reports truncated: it renders the conversation from its start
//! (its committed transcript) and follows the live stream from there.
//!
//! The mark is saved per workspace in its realm, `<realm>/attach.cursor`
//! ([`CursorFile`]) after every run end and on detach.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The cursor file's name in the workspace's realm.
pub const CURSOR_NAME: &str = "attach.cursor";

/// The last event shown, by session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The orchestrator's session.
    pub session_id: String,
    /// The sequence of the last event shown.
    pub sequence: u64,
    /// The sequence space of `sequence` (the stream's `epoch`); `None`
    /// before the first event of a stream named one (such a cursor does
    /// not resume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// The daemon the events came from ([`daemon_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
}

impl Cursor {
    /// The stream position to resume from, when this cursor can resume the
    /// stream of `session` served by the daemon `daemon` ([`daemon_id`]):
    /// its epoch is known and the daemon is the one that numbered it.
    #[must_use]
    pub fn resumable(&self, session: &str, daemon: &str) -> Option<(String, u64)> {
        (self.session_id == session && self.daemon.as_deref() == Some(daemon))
            .then(|| self.epoch.clone().map(|epoch| (epoch, self.sequence)))
            .flatten()
    }
}

/// The cursor file of a workspace.
#[derive(Debug, Clone)]
pub struct CursorFile {
    root: PathBuf,
    path: PathBuf,
}

impl CursorFile {
    /// The cursor file of the workspace whose realm is `realm_root`.
    #[must_use]
    pub fn of_realm(realm_root: &Path) -> Self {
        Self {
            root: realm_root.to_path_buf(),
            path: realm_root.join(CURSOR_NAME),
        }
    }

    /// The cursor file of `agent` in the same workspace:
    /// `attach-<agent>.cursor` beside this one.
    #[must_use]
    pub fn for_agent(&self, agent: &str) -> Self {
        let name = format!("attach-{agent}.cursor");
        Self {
            root: self.root.clone(),
            path: self.path.with_file_name(name),
        }
    }

    /// Its path.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
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
    epoch: Option<String>,
}

impl Mark {
    /// The mark for `session_id`: the saved cursor's when it is for that
    /// session, else nothing shown yet.
    #[must_use]
    pub fn resume(session_id: &str, saved: Option<&Cursor>) -> Self {
        let saved = saved.filter(|cursor| cursor.session_id == session_id);
        Self {
            session_id: session_id.to_string(),
            sequence: saved.map_or(0, |cursor| cursor.sequence),
            epoch: saved.and_then(|cursor| cursor.epoch.clone()),
        }
    }

    /// The mark after a render from the conversation's start whose last
    /// event is `sequence` (in a space not named yet).
    #[must_use]
    pub fn rendered(session_id: &str, sequence: u64) -> Self {
        Self {
            session_id: session_id.to_string(),
            sequence,
            epoch: None,
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

    /// The sequence space of the mark, once an event named one.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn epoch(&self) -> Option<&str> {
        self.epoch.as_deref()
    }

    /// Whether the event with `sequence` of space `epoch` is new; if so the
    /// mark moves to it. An event of another space than the mark's is new
    /// (its sequences may restart); an event that names no space is judged
    /// by its sequence.
    pub fn admit(&mut self, epoch: Option<&str>, sequence: u64) -> bool {
        let other_space =
            matches!((self.epoch.as_deref(), epoch), (Some(mine), Some(its)) if mine != its);
        if other_space || sequence > self.sequence {
            self.sequence = sequence;
            if let Some(epoch) = epoch {
                self.epoch = Some(epoch.to_string());
            }
            true
        } else {
            if self.epoch.is_none()
                && let Some(epoch) = epoch
            {
                self.epoch = Some(epoch.to_string());
            }
            false
        }
    }

    /// The cursor to save, for events served by the daemon `daemon`.
    #[must_use]
    pub fn cursor(&self, daemon: Option<&str>) -> Cursor {
        Cursor {
            session_id: self.session_id.clone(),
            sequence: self.sequence,
            epoch: self.epoch.clone(),
            daemon: daemon.map(str::to_string),
        }
    }
}

/// Which daemon a cursor's events came from: the first 16 hex digits of
/// the SHA-256 of its attach token (each daemon draws its own; the token
/// itself never goes into the workspace, where the agent's shell can read
/// it).
#[must_use]
pub fn daemon_id(token: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(token.as_bytes()).iter().take(8).fold(
        String::with_capacity(16),
        |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    )
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
            if mark.admit(None, *seq) {
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
            .filter(|seq| mark.admit(None, *seq))
            .collect();
        assert_eq!(shown, vec![1, 2, 3, 4]);
    }

    #[test]
    fn the_cursor_survives_per_workspace_and_only_for_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let file = CursorFile::of_realm(dir.path());
        assert_eq!(file.load(), None);
        let mut mark = Mark::resume("s1", None);
        assert!(mark.admit(Some("e1"), 7));
        file.save(&mark.cursor(Some("d1")));
        assert_eq!(file.path(), dir.path().join(CURSOR_NAME));
        let saved = file.load().unwrap();
        assert_eq!(Mark::resume("s1", Some(&saved)).sequence(), 7);
        // Another session (the daemon's orchestrator was replaced) starts
        // from the beginning.
        assert_eq!(Mark::resume("s2", Some(&saved)).sequence(), 0);
        // The cursor resumes the same daemon's stream of that session only.
        assert_eq!(saved.resumable("s1", "d1"), Some(("e1".to_string(), 7)));
        assert_eq!(saved.resumable("s1", "d2"), None, "a restarted daemon");
        assert_eq!(saved.resumable("s2", "d1"), None);
        // A cursor saved before its stream named an epoch does not resume.
        let unnamed = Cursor {
            epoch: None,
            ..saved
        };
        assert_eq!(unnamed.resumable("s1", "d1"), None);
        assert_ne!(daemon_id("token-a"), daemon_id("token-b"));
        assert_eq!(daemon_id("token-a").len(), 16);
        // Each agent has its own file.
        let agent = file.for_agent("main-ab12");
        assert_eq!(agent.path(), dir.path().join("attach-main-ab12.cursor"));
        assert_eq!(agent.load(), None, "another agent's position is its own");
    }

    #[test]
    fn another_epoch_is_a_new_space_and_the_mark_follows_it() {
        let mut mark = Mark::rendered("s", 10);
        // The render's last event; then the live stream names its space.
        assert!(!mark.admit(Some("e1"), 10));
        assert_eq!(mark.epoch(), Some("e1"));
        assert!(mark.admit(Some("e1"), 11));
        assert!(!mark.admit(Some("e1"), 11), "once");
        // A new space restarts its sequences: still new.
        assert!(mark.admit(Some("e2"), 1));
        assert_eq!((mark.epoch(), mark.sequence()), (Some("e2"), 1));
        assert!(!mark.admit(Some("e2"), 1));
    }
}
