//! Telling a parent that a helper of its ended ([`crate::mobs::endings`]
//! decides when): the note's words, who receives it, and when.
//!
//! - **Who.** A helper's parent is the session that started it
//!   ([`crate::mobs::HelperRegistry::parent_session`]): an agent's current
//!   conversation, or a helper still running. A parent that is gone (a
//!   helper that ended, a conversation left) hands the note to the agent
//!   the helper worked for, when that agent still runs that conversation.
//! - **When.** At once: into a running parent's run (it reads it at its
//!   next step), or as a message that starts a run for an idle one (a run
//!   no user started: unattended unless a client is with the agent,
//!   [`super::triggers`]). An agent whose last run the user just
//!   interrupted (no run started since) is not woken: the note is held
//!   ([`HeldNotes`], persisted under the realm), shown to its clients at
//!   once, and travels with the user's next message to it, before the
//!   user's words ([`Agents::take_held`]; an append to the agent's system
//!   context is refused by the engine right after an interrupted run,
//!   `docs/evidence.md`), or goes into the next run that starts otherwise.
//! - **Batching.** Endings that come within [`BATCH`] of each other go to
//!   one recipient as one note that lists each.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use meerkat_core::SessionId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::Agents;
use crate::mobs::endings::{Ended, How};
use crate::mobs::{HelperKey, HelperTag};

/// How long the endings that follow the first wait to go with it.
pub const BATCH: Duration = Duration::from_millis(250);

/// The longest answer a note carries; the rest is written to a file.
pub const MAX_ANSWER: usize = 4000;

/// The file the held notes persist in, under the realm.
pub const HELD_FILE: &str = "held-notes.json";

/// One helper's ending as its note words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteItem {
    /// The helper's label.
    pub label: String,
    /// How it ended.
    pub how: How,
    /// Where its whole answer was written when the note cuts it.
    pub rest: Option<PathBuf>,
}

/// `answer` as a note carries it: at most [`MAX_ANSWER`] characters,
/// trimmed, then where the rest is when it was cut (`rest`: the file it was
/// written to).
fn bounded(answer: &str, rest: Option<&Path>) -> String {
    let (shown, cut) = match answer.char_indices().nth(MAX_ANSWER) {
        Some((byte, _)) => (&answer[..byte], true),
        None => (answer, false),
    };
    let rest = match (rest, cut) {
        (_, false) => String::new(),
        (Some(path), true) => format!(
            "\n\n(The answer is cut at {MAX_ANSWER} characters; the whole of it is in {}.)",
            path.display()
        ),
        (None, true) => format!(
            "\n\n(The answer is cut at {MAX_ANSWER} characters; the user can read the whole of it in the helper's view.)"
        ),
    };
    format!("{}{rest}", shown.trim())
}

/// What a parent is told of one ending (no leading list mark).
fn item_text(item: &NoteItem) -> String {
    const NOT_CONTINUED: &str = "Its file changes stay on disk; its conversation is not continued.";
    let label = &item.label;
    match &item.how {
        How::Finished { answer } => format!(
            "Your helper `{label}` finished. The `delegate` call that started it ended before it did (when rkat-tui's daemon stopped), so its answer comes in this note instead:\n\n{}\n\nIt is retired.",
            bounded(answer, item.rest.as_deref())
        ),
        How::Done { answer } => format!(
            "Your helper `{label}` finished its task. Its answer:\n\n{}\n\nIt is retired: it takes no more messages (start another with `assign` for more work).",
            bounded(answer, item.rest.as_deref())
        ),
        How::WentIdle {
            answer,
            failed: None,
        } => format!(
            "Your helper `{label}` went idle. Its last message:\n\n{}\n\nIt is still seated: message it with `send_message` for more work, or retire it with `retire_member` when you are done with it.",
            bounded(answer, item.rest.as_deref())
        ),
        How::WentIdle {
            failed: Some(reason),
            ..
        } => format!(
            "Your helper `{label}` went idle after its run failed: {}. It is still seated: message it with `send_message` to try again, or retire it with `retire_member`.",
            crate::mobs::endings::plain_reason(reason)
        ),
        How::Failed { reason } => format!(
            "Your helper `{label}` failed: {}. {NOT_CONTINUED}",
            crate::mobs::endings::plain_reason(reason)
        ),
        How::StoppedByUser => {
            format!("The user stopped your helper `{label}`. {NOT_CONTINUED}")
        }
        How::DaemonStopped => format!(
            "Your helper `{label}` ended when the rkat-tui daemon stopped, before it finished. {NOT_CONTINUED}"
        ),
        How::CheckpointStopped {
            steps,
            reason,
            status,
            retired: true,
        } => format!(
            "Your helper `{label}` stopped at its checkpoint after {steps} steps and was not continued: {reason}. Its last status, as its answer:\n\n{}\n\nIt is retired: it takes no more messages (start another with `assign` to go on).",
            bounded(status, item.rest.as_deref())
        ),
        How::CheckpointStopped {
            steps,
            reason,
            status,
            retired: false,
        } => format!(
            "Your helper `{label}` stopped at its checkpoint after {steps} steps and was not continued: {reason}. Its last status:\n\n{}\n\nIt is still seated, idle: message it with `send_message` to go on, or retire it with `retire_member`.",
            bounded(status, item.rest.as_deref())
        ),
    }
}

/// The note telling a parent of `items` (one or more endings).
#[must_use]
pub fn note_text(items: &[NoteItem]) -> String {
    match items {
        [one] => item_text(one),
        many => {
            let list = many
                .iter()
                .map(|item| format!("- {}", item_text(item).replace('\n', "\n  ")))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{} notes about your helpers:\n{list}", many.len())
        }
    }
}

/// Who a note goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recipient {
    /// A live agent, running `session`.
    Agent { name: String, session: SessionId },
    /// A running helper.
    Helper { key: HelperKey, tag: HelperTag },
}

/// A note held for an agent its user just interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Held {
    agent: String,
    session: SessionId,
    text: String,
}

/// The file's contents.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HeldFile {
    version: u32,
    #[serde(default)]
    notes: Vec<Held>,
}

/// The held notes' format version.
const HELD_VERSION: u32 = 1;

/// The notes held for agents whose user just interrupted them, persisted
/// under the realm ([`HELD_FILE`]) so a daemon restart does not lose them.
#[derive(Debug, Default)]
pub struct HeldNotes {
    path: Option<PathBuf>,
    notes: parking_lot::Mutex<Vec<Held>>,
}

impl HeldNotes {
    /// The notes held under `realm` (none when there is no file; an
    /// unreadable one is logged and starts empty).
    #[must_use]
    pub fn open(realm: &Path) -> Self {
        let path = realm.join(HELD_FILE);
        let notes = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<HeldFile>(&bytes) {
                Ok(file) if file.version == HELD_VERSION => file.notes,
                Ok(file) => {
                    tracing::warn!(path = %path.display(), version = file.version, "held helper notes of an unknown version; starting empty");
                    Vec::new()
                }
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "held helper notes unreadable; starting empty");
                    Vec::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "held helper notes unreadable; starting empty");
                Vec::new()
            }
        };
        Self {
            path: Some(path),
            notes: parking_lot::Mutex::new(notes),
        }
    }

    fn persist(&self, notes: &[Held]) {
        let Some(path) = &self.path else {
            return;
        };
        let file = HeldFile {
            version: HELD_VERSION,
            notes: notes.to_vec(),
        };
        let written = serde_json::to_vec_pretty(&file)
            .map_err(|err| err.to_string())
            .and_then(|bytes| {
                crate::paths::write_atomic(
                    path,
                    &bytes,
                    crate::paths::FileMode::Private,
                    path.parent(),
                )
                .map_err(|err| err.to_string())
            });
        if let Err(err) = written {
            tracing::warn!(path = %path.display(), error = %err, "held helper notes not saved; a restart loses them");
        }
    }

    /// Hold `text` for `agent`, which runs `session`.
    fn hold(&self, agent: &str, session: &SessionId, text: String) {
        let mut notes = self.notes.lock();
        notes.push(Held {
            agent: agent.to_string(),
            session: session.clone(),
            text,
        });
        self.persist(&notes);
    }

    /// Take every note held for `agent`: the ones for `session` joined in
    /// the order they were held (`None`: none); the ones for another
    /// conversation of it go (it was left).
    fn take(&self, agent: &str, session: &SessionId) -> Option<String> {
        let mut notes = self.notes.lock();
        if !notes.iter().any(|held| held.agent == agent) {
            return None;
        }
        let (mine, rest): (Vec<Held>, Vec<Held>) =
            notes.drain(..).partition(|held| held.agent == agent);
        *notes = rest;
        self.persist(&notes);
        let texts: Vec<String> = mine
            .into_iter()
            .filter(|held| &held.session == session)
            .map(|held| held.text)
            .collect();
        (!texts.is_empty()).then(|| texts.join("\n\n"))
    }

    /// Whether a note is held for `agent`.
    fn holds(&self, agent: &str) -> bool {
        self.notes.lock().iter().any(|held| held.agent == agent)
    }
}

/// Which agents' last run the user interrupted, with no run started since.
#[derive(Debug, Default)]
pub struct Interrupted(parking_lot::Mutex<HashSet<String>>);

/// Hand every ending `endings` delivers to `agents`, those that come within
/// [`BATCH`] of each other together, until the channel closes.
pub(crate) async fn deliver(agents: Weak<Agents>, mut endings: mpsc::UnboundedReceiver<Ended>) {
    while let Some(first) = endings.recv().await {
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + BATCH;
        while let Ok(Some(next)) = tokio::time::timeout_at(deadline, endings.recv()).await {
            batch.push(next);
        }
        let Some(agents) = agents.upgrade() else {
            return;
        };
        agents.helpers_ended(batch).await;
    }
}

impl Agents {
    /// The user interrupted `name`'s run: the endings of its helpers are
    /// held until its next run (or the user's next message).
    pub fn interrupted(&self, name: &str) {
        self.interrupted.0.lock().insert(name.to_string());
    }

    /// A run of `name` started: it is no longer just interrupted, and the
    /// notes held for it go into this run.
    pub(crate) fn helpers_run_started(self: &Arc<Self>, name: &str) {
        self.interrupted.0.lock().remove(name);
        if !self.held.holds(name) {
            return;
        }
        let Some(session) = self.core.conversations.current(name) else {
            return;
        };
        if let Some(text) = self.held.take(name, &session) {
            let agents = Arc::clone(self);
            let name = name.to_string();
            tokio::spawn(async move { agents.steer_note(&name, text).await });
        }
    }

    /// The notes held for `name` (running `session`), as one, taken: they
    /// travel with the user's message that is on its way, before its
    /// words.
    #[must_use]
    pub fn take_held(&self, name: &str, session: &SessionId) -> Option<String> {
        self.held.take(name, session)
    }

    /// Tell each parent of `batch` its helpers' endings (one note per
    /// parent), and the clients how each helper ended.
    async fn helpers_ended(self: &Arc<Self>, batch: Vec<Ended>) {
        let mut groups: Vec<(Option<Recipient>, Vec<Ended>)> = Vec::new();
        for ended in batch {
            let recipient = self.recipient(&ended.key, &ended.tag);
            match groups.iter_mut().find(|(r, _)| *r == recipient) {
                Some((_, endings)) => endings.push(ended),
                None => groups.push((recipient, vec![ended])),
            }
        }
        for (recipient, endings) in groups {
            let told = match &recipient {
                Some(recipient) => Some(self.tell(recipient, &endings).await),
                None => {
                    tracing::info!(
                        helpers = endings.len(),
                        "a helper's parent is gone: nobody is told of its ending"
                    );
                    None
                }
            };
            for ended in &endings {
                self.announce_ended(ended, told.clone());
            }
        }
    }

    /// Who is told of the helper `key`'s (`tag`) ending: its parent, else
    /// the agent it worked for (`None`: neither runs).
    fn recipient(&self, key: &HelperKey, tag: &HelperTag) -> Option<Recipient> {
        let registry = &self.core.registry;
        let agent_of = |session: &SessionId| {
            self.live()
                .into_iter()
                .find(|(_, running)| running == session)
                .map(|(name, session)| Recipient::Agent { name, session })
        };
        if let Some(parent) = registry.parent_session(key) {
            if let Some(agent) = agent_of(&parent) {
                return Some(agent);
            }
            if let Some((key, tag)) = registry.live_by_session(&parent) {
                return Some(Recipient::Helper { key, tag });
            }
        }
        // A member seated in a mob no session created (the workspace's
        // own, where `assign` and `spawn_member` seat helpers): the helper
        // above it on its path, which seated it, while it runs.
        if !tag.parent_path().is_empty()
            && let Some((key, tag)) = registry.live_by_path(&tag.owner, tag.parent_path())
        {
            return Some(Recipient::Helper { key, tag });
        }
        agent_of(&tag.owner)
    }

    /// Tell `recipient` of `endings`; what the clients are told of it.
    async fn tell(self: &Arc<Self>, recipient: &Recipient, endings: &[Ended]) -> String {
        let items: Vec<NoteItem> = endings
            .iter()
            .map(|ended| NoteItem {
                label: ended.tag.label().to_string(),
                rest: self.write_rest(ended),
                how: ended.how.clone(),
            })
            .collect();
        let text = note_text(&items);
        match recipient {
            Recipient::Agent { name, session } => {
                if self.interrupted.0.lock().contains(name) {
                    self.held.hold(name, session, text);
                    format!("{name} is told with your next message to it")
                } else {
                    self.steer_note(name, text).await;
                    format!("{name} was told")
                }
            }
            Recipient::Helper { key, tag } => {
                let sent = super::notes::steer_member(
                    &self.core.state,
                    &key.mob,
                    key.member.clone(),
                    text,
                )
                .await;
                match sent {
                    Ok(()) => format!("its parent helper {} was told", tag.label()),
                    Err(err) => {
                        tracing::warn!(helper = %key, error = %err, "a helper was not told of its own helper's ending");
                        format!("its parent helper {} could not be told: {err}", tag.label())
                    }
                }
            }
        }
    }

    /// Steer `text` into `name`'s run, or start a run with it when it is
    /// idle (a run no user started, attributed as a helper's ending).
    async fn steer_note(&self, name: &str, text: String) {
        if let Err(err) = self
            .steer(name, text, super::notes::Wake::HelperEnded)
            .await
        {
            tracing::warn!(agent = %name, error = %err, "a helper's ending did not reach the agent");
        }
    }

    /// A finished helper's answer longer than a note carries, written in
    /// full to its agent's scratch directory (`None`: not cut, or there is
    /// no directory, or it could not be written).
    fn write_rest(&self, ended: &Ended) -> Option<PathBuf> {
        let (How::Finished { answer }
        | How::Done { answer }
        | How::WentIdle { answer, .. }
        | How::CheckpointStopped { status: answer, .. }) = &ended.how
        else {
            return None;
        };
        answer.char_indices().nth(MAX_ANSWER)?;
        let dir = self.core.registry.scratch_of_session(&ended.tag.owner)?;
        let name: String = ended
            .tag
            .label()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("helper-{name}-answer.md"));
        match crate::paths::write_atomic(
            &path,
            answer.as_bytes(),
            crate::paths::FileMode::Private,
            Some(&dir),
        ) {
            Ok(()) => Some(path),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "a helper's whole answer was not written");
                None
            }
        }
    }

    /// Tell the clients that `ended`'s helper ended and how its parent
    /// heard of it (`told`).
    fn announce_ended(&self, ended: &Ended, told: Option<String>) {
        // An idle member did not end: its view shows its run's end itself.
        if matches!(
            ended.how,
            How::WentIdle { .. } | How::CheckpointStopped { retired: false, .. }
        ) {
            return;
        }
        let Some(agent) = self.agent_running_as(&ended.tag.owner) else {
            return;
        };
        self.core.decisions.broadcast(
            crate::daemon::protocol::HELPER_ENDED,
            &crate::daemon::protocol::HelperEnded {
                agent,
                session_id: ended.tag.owner.clone(),
                path: ended.tag.path.clone(),
                how: ended.how.text(),
                told,
            },
        );
    }

    /// The `delegate` helpers the last daemon left (`delegate` is withdrawn
    /// since: they are a realm of an earlier build's, `crate::mobs::delegates`):
    /// their calls ended with it. One its mob still seats was restored by
    /// the engine, which resumes its run, and its answer comes to its
    /// parent when it finishes. One its mob no longer seats is gone: its
    /// parent is told so ([`crate::mobs::endings::HelperEndings::lost`]).
    pub(crate) async fn settle_left_helpers(&self) {
        let registry = &self.core.registry;
        for (key, tag) in registry.bound_members() {
            if !key.mob.as_str().starts_with("implicit-") {
                continue;
            }
            let seated = match self.core.state.handle_for(&key.mob).await {
                Ok(handle) => handle
                    .list_members()
                    .await
                    .iter()
                    .any(|member| member.agent_identity == key.member),
                Err(_) => false,
            };
            if seated {
                tracing::info!(helper = %key, "a delegate helper the last daemon left runs on; its answer goes to its parent");
                registry.endings().spawned(&key, &tag);
                registry.endings().left_over(&key);
            } else {
                registry.endings().lost(&key, tag);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn item(label: &str, how: How) -> NoteItem {
        NoteItem {
            label: label.into(),
            how,
            rest: None,
        }
    }

    #[test]
    fn each_ending_is_told_in_plain_words() {
        let finished = note_text(&[item(
            "scout",
            How::Finished {
                answer: "the bug is in parse()".into(),
            },
        )]);
        assert!(
            finished.starts_with("Your helper `scout` finished.")
                && finished.contains("ended before it did")
                && finished.contains("\n\nthe bug is in parse()\n\n"),
            "{finished}"
        );
        let done = note_text(&[item(
            "scout",
            How::Done {
                answer: "  the bug is in parse()\n".into(),
            },
        )]);
        assert_eq!(
            done,
            "Your helper `scout` finished its task. Its answer:\n\nthe bug is in parse()\n\nIt is retired: it takes no more messages (start another with `assign` for more work)."
        );
        let idle = note_text(&[item(
            "w1",
            How::WentIdle {
                answer: "waiting for more".into(),
                failed: None,
            },
        )]);
        assert_eq!(
            idle,
            "Your helper `w1` went idle. Its last message:\n\nwaiting for more\n\nIt is still seated: message it with `send_message` for more work, or retire it with `retire_member` when you are done with it."
        );
        let idle_failed = note_text(&[item(
            "w1",
            How::WentIdle {
                answer: String::new(),
                failed: Some("a 400".into()),
            },
        )]);
        assert!(
            idle_failed.starts_with("Your helper `w1` went idle after its run failed: a 400.")
                && idle_failed.contains("`retire_member`"),
            "{idle_failed}"
        );
        let stopped = note_text(&[item("scout", How::StoppedByUser)]);
        assert_eq!(
            stopped,
            "The user stopped your helper `scout`. Its file changes stay on disk; its conversation is not continued."
        );
        let lost = note_text(&[item("scout", How::DaemonStopped)]);
        assert!(
            lost.contains("ended when the rkat-tui daemon stopped"),
            "{lost}"
        );
        let failed = note_text(&[item(
            "scout",
            How::Failed {
                reason: "InterruptedByRecovery".into(),
            },
        )]);
        assert!(
            failed.contains("failed: its run was cut off when the daemon stopped")
                && !failed.contains("Recovery"),
            "{failed}"
        );
    }

    #[test]
    fn a_long_answer_is_cut_and_says_where_the_rest_is() {
        let answer = "x".repeat(MAX_ANSWER + 10);
        let cut = note_text(&[item(
            "scout",
            How::Finished {
                answer: answer.clone(),
            },
        )]);
        assert!(!cut.contains(&answer));
        assert!(cut.contains("the user can read the whole of it in the helper's view"));
        let mut written = item("scout", How::Finished { answer });
        written.rest = Some(PathBuf::from("/tmp/scratch/helper-scout-answer.md"));
        let cut = note_text(&[written]);
        assert!(
            cut.contains("the whole of it is in /tmp/scratch/helper-scout-answer.md"),
            "{cut}"
        );
    }

    #[test]
    fn several_endings_go_as_one_note_listing_each() {
        let text = note_text(&[
            item("a", How::StoppedByUser),
            item(
                "b",
                How::Finished {
                    answer: "line one\nline two".into(),
                },
            ),
        ]);
        assert!(
            text.starts_with("2 notes about your helpers:\n- The user stopped your helper `a`.")
        );
        assert!(text.contains("\n- Your helper `b` finished."), "{text}");
        assert!(text.contains("\n  line one\n  line two"), "{text}");
    }

    #[test]
    fn held_notes_survive_a_reopen_and_go_once_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let held = HeldNotes::open(dir.path());
        held.hold("main-a", &session, "first".into());
        held.hold("main-a", &session, "second".into());
        held.hold("main-b", &session, "other".into());
        assert!(held.holds("main-a"));
        let reopened = HeldNotes::open(dir.path());
        assert_eq!(
            reopened.take("main-a", &session).as_deref(),
            Some("first\n\nsecond")
        );
        assert_eq!(reopened.take("main-a", &session), None);
        // A note for a conversation the agent left goes, untold.
        reopened.hold("main-b", &SessionId::new(), "stale".into());
        assert_eq!(reopened.take("main-b", &session).as_deref(), Some("other"));
        assert!(!reopened.holds("main-b"));
        // No file, or an unreadable one: nothing held.
        let empty = tempfile::tempdir().unwrap();
        assert!(!HeldNotes::open(empty.path()).holds("main-a"));
        std::fs::write(empty.path().join(HELD_FILE), b"not json").unwrap();
        assert!(!HeldNotes::open(empty.path()).holds("main-a"));
    }
}
