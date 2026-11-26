//! Telling a parent that a helper of its ended ([`crate::mobs::endings`]
//! decides when): the note's words, who receives it, and when.
//!
//! - **Who.** A helper's parent is the session that started it
//!   ([`crate::mobs::HelperRegistry::parent_session`]): an agent's current
//!   conversation, or a helper still running. A parent that is gone (a
//!   helper that ended, a conversation left) hands the note to the agent
//!   the helper worked for, when that agent still runs that conversation.
//!   A parent helper that ends while the note is on its way (it finished,
//!   [`crate::mobs::endings::HelperEndings::while_unsettled`]) or refuses
//!   it passes it on as its own ending would go: to the next one up its
//!   path, and finally to the agent, once.
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
//! - **Never dropped.** A steer into an agent that fails (the engine's
//!   refusal, after the one reload a storage failure needs) holds the note
//!   as an interrupt does, and its clients read why ("is told with its
//!   next run: …"). The steer carries no idempotency key and the daemon
//!   sets it no deadline, so an error is the engine's answer; should the
//!   engine have taken it all the same, the run it starts with those words
//!   drops the held copy ([`HeldNotes::delivered`]). A note every helper on
//!   its way refused, whose agent runs another conversation now, is written
//!   to the agent's scratch directory and the agent's clients are told
//!   where.

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

/// How a [`crate::daemon::protocol::HelperEnded::told`] ends when the note
/// reached its recipient now: a client then pairs its notice with the
/// folded note of the run it started
/// (`crate::ui::transcript::Transcript::ending_shown`).
pub const TOLD: &str = " was told";

/// One helper's ending as its note words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteItem {
    pub label: String,
    pub how: How,
    /// Where its whole answer was written when the note cuts it.
    pub rest: Option<PathBuf>,
    /// Its own helpers retired with it.
    pub with: Vec<String>,
}

/// `answer` as a note carries it: at most [`MAX_ANSWER`] characters,
/// trimmed, then where the rest is when it was cut (`rest`: the file it was
/// written to).
fn bounded(answer: &str, rest: Option<&Path>) -> String {
    let (shown, cut) = crate::text::head_chars(answer, MAX_ANSWER);
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

/// What a parent is told of one ending (no leading list mark), naming
/// the helpers of its own that were retired with it.
fn item_text(item: &NoteItem) -> String {
    let text = ending_text(item);
    match crate::mobs::endings::retired_with(&item.with) {
        Some(with) => format!("{text} {with}"),
        None => text,
    }
}

/// [`item_text`] without the helpers retired with it.
fn ending_text(item: &NoteItem) -> String {
    const NOT_CONTINUED: &str = "Its file changes stay on disk; its conversation is not continued.";
    let label = &item.label;
    match &item.how {
        How::Done { answer } => format!(
            "Your helper `{label}` finished its task. Its answer:\n\n{}\n\nIt is retired: it takes no more messages (start another helper for more work).",
            bounded(answer, item.rest.as_deref())
        ),
        How::WentIdle {
            answer,
            failed: None,
        } => format!(
            "Your helper `{label}` went idle. Its last message:\n\n{}\n\nIt is still seated, idle: a message to it starts another run of it, so message it with `send_message` only to give it more work, and retire it with `retire_helper` when you are done with it.",
            bounded(answer, item.rest.as_deref())
        ),
        How::WentIdle {
            failed: Some(reason),
            ..
        } => format!(
            "Your helper `{label}` went idle after its run failed: {}. It is still seated: message it with `send_message` to try again, or retire it with `retire_helper`.",
            crate::mobs::endings::plain_reason(reason)
        ),
        How::Failed { reason } => format!(
            "Your helper `{label}` failed: {}. {NOT_CONTINUED}",
            crate::mobs::endings::plain_reason(reason)
        ),
        How::StoppedByUser => {
            format!("The user stopped your helper `{label}`. {NOT_CONTINUED}")
        }
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
            "Your helper `{label}` stopped at its checkpoint after {steps} steps and was not continued: {reason}. Its last status:\n\n{}\n\nIt is still seated, idle: message it with `send_message` to go on, or retire it with `retire_helper`.",
            bounded(status, item.rest.as_deref())
        ),
    }
}

/// What the first line of every note [`note_text`] writes starts with: a
/// header the code writes from [`NoteKind`] and reads back
/// ([`note_summary`]), so the fold of a stored note does not depend on
/// the wording of the sentences below it.
pub const NOTE_HEADER: &str = "rkat-tui note: ";

/// What a note says of a helper, as its header and its folded line name
/// it ([`NoteSummary::kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// It finished (its answer follows).
    Finished,
    /// It went idle (its last message follows).
    Idle,
    /// It went idle after its run failed.
    IdleFailed,
    /// Its run failed.
    Failed,
    /// The user stopped it.
    Stopped,
    /// It ended when the daemon stopped.
    DaemonStopped,
    /// It stopped at its checkpoint (its status follows).
    Checkpoint,
    /// A note of several endings.
    Several,
}

impl NoteKind {
    /// The kinds of one ending, a longer header word before one it starts
    /// with (`went idle after a failed run` before `went idle`).
    const ONE: [Self; 7] = [
        Self::Finished,
        Self::IdleFailed,
        Self::Idle,
        Self::Failed,
        Self::Stopped,
        Self::DaemonStopped,
        Self::Checkpoint,
    ];

    /// The kind of `how`.
    #[must_use]
    pub fn of(how: &How) -> Self {
        match how {
            How::Done { .. } => Self::Finished,
            How::WentIdle { failed: None, .. } => Self::Idle,
            How::WentIdle {
                failed: Some(_), ..
            } => Self::IdleFailed,
            How::Failed { .. } => Self::Failed,
            How::StoppedByUser => Self::Stopped,
            How::CheckpointStopped { .. } => Self::Checkpoint,
        }
    }

    /// Its words in a note's header.
    fn word(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Idle => "went idle",
            Self::IdleFailed => "went idle after a failed run",
            Self::Failed => "failed",
            Self::Stopped => "was stopped by the user",
            Self::DaemonStopped => "ended when the daemon stopped",
            Self::Checkpoint => "stopped at its checkpoint",
            Self::Several => "ended",
        }
    }

    /// What the folded line says of it (`reason`: why it failed).
    fn what(self, reason: Option<&str>) -> String {
        let reason = reason.unwrap_or_default();
        match self {
            Self::IdleFailed => format!("went idle: {reason}"),
            Self::Failed => format!("failed: {reason}"),
            Self::Stopped => "stopped by you".to_string(),
            other => other.word().to_string(),
        }
    }

    /// Whether it names why it failed.
    fn has_reason(self) -> bool {
        matches!(self, Self::IdleFailed | Self::Failed)
    }

    /// Whether the note carries the helper's answer (or status).
    fn has_answer(self) -> bool {
        matches!(self, Self::Finished | Self::Idle | Self::Checkpoint)
    }
}

/// One ending as a note's header names it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    label: String,
    kind: NoteKind,
    reason: Option<String>,
}

impl Entry {
    fn of(item: &NoteItem) -> Self {
        let reason = match &item.how {
            How::Failed { reason }
            | How::WentIdle {
                failed: Some(reason),
                ..
            } => Some(one_line(&crate::mobs::endings::plain_reason(reason))),
            _ => None,
        };
        Self {
            label: item.label.clone(),
            kind: NoteKind::of(&item.how),
            reason,
        }
    }

    /// Its words in the header: `` `review-x` failed: <reason> ``.
    fn header(&self) -> String {
        match &self.reason {
            Some(reason) => format!("`{}` {}: {reason}", self.label, self.kind.word()),
            None => format!("`{}` {}", self.label, self.kind.word()),
        }
    }

    /// The entry [`Self::header`] wrote.
    fn parse(text: &str) -> Option<Self> {
        let (label, rest) = text.strip_prefix('`')?.split_once("` ")?;
        NoteKind::ONE.iter().find_map(|&kind| {
            let after = rest.strip_prefix(kind.word())?;
            let reason = match (kind.has_reason(), after) {
                (false, "") => None,
                (true, after) => Some(after.strip_prefix(": ")?.to_string()),
                (false, _) => return None,
            };
            Some(Self {
                label: label.to_string(),
                kind,
                reason,
            })
        })
    }

    /// The folded line's words of it: `review-x failed: <reason>`.
    fn what(&self) -> String {
        format!("{} {}", self.label, self.kind.what(self.reason.as_deref()))
    }
}

/// `text` on one line, with no `;` (the header separates entries by it).
fn one_line(text: &str) -> String {
    crate::text::one_line(text).replace(';', ",")
}

/// A note's header line for `entries`.
fn header(entries: &[Entry]) -> String {
    match entries {
        [one] => format!("{NOTE_HEADER}helper {}", one.header()),
        many => format!(
            "{NOTE_HEADER}{} helpers: {}",
            many.len(),
            many.iter()
                .map(Entry::header)
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}

/// The entries of a header line [`header`] wrote (without [`NOTE_HEADER`]).
fn parse_header(line: &str) -> Option<Vec<Entry>> {
    if let Some(one) = line.strip_prefix("helper ") {
        return Some(vec![Entry::parse(one)?]);
    }
    let (count, list) = line.split_once(" helpers: ")?;
    let count: usize = count.parse().ok()?;
    let entries: Vec<Entry> = list.split("; ").map(Entry::parse).collect::<Option<_>>()?;
    (entries.len() == count && count > 1).then_some(entries)
}

/// The summary of `entries`, the one answer's first line `first`.
fn summary_from(entries: &[Entry], first: String) -> NoteSummary {
    match entries {
        [one] => NoteSummary {
            title: format!("helper {}", one.what()),
            first,
            helpers: vec![one.label.clone()],
            failed: one.kind.has_reason(),
            kind: one.kind,
        },
        many => NoteSummary {
            title: format!("{} helpers ended", many.len()),
            first: many.iter().map(Entry::what).collect::<Vec<_>>().join("; "),
            helpers: many.iter().map(|entry| entry.label.clone()).collect(),
            failed: many.iter().any(|entry| entry.kind.has_reason()),
            kind: NoteKind::Several,
        },
    }
}

/// The first line of `items`' one answer, as the note carries it (none for
/// several, or for an ending without one).
fn answer_first(items: &[NoteItem]) -> String {
    let [item] = items else {
        return String::new();
    };
    let answer = match &item.how {
        How::Done { answer }
        | How::WentIdle {
            answer,
            failed: None,
        }
        | How::CheckpointStopped { status: answer, .. } => answer,
        _ => return String::new(),
    };
    bounded(answer, None)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The note telling a parent of `items` (one or more endings): a header
/// line ([`NOTE_HEADER`]), then the endings in words.
#[must_use]
pub fn note_text(items: &[NoteItem]) -> String {
    let entries: Vec<Entry> = items.iter().map(Entry::of).collect();
    let words = match items {
        [one] => item_text(one),
        many => {
            let list = many
                .iter()
                .map(|item| format!("- {}", item_text(item).replace('\n', "\n  ")))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{} notes about your helpers:\n{list}", many.len())
        }
    };
    format!("{}\n\n{words}", header(&entries))
}

/// The folded line of the note [`note_text`] writes of `items`, made from
/// the items themselves: what the daemon sends its clients with a run the
/// note started ([`crate::daemon::protocol::TriggeredNote::summary`]).
#[must_use]
pub fn summary_of(items: &[NoteItem]) -> NoteSummary {
    let entries: Vec<Entry> = items.iter().map(Entry::of).collect();
    summary_from(&entries, answer_first(items))
}

/// A helper's ending note as a client folds it to one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteSummary {
    /// What happened, in a few words: `helper review-x finished`,
    /// `helper review-x failed: <reason>`, `2 helpers ended`.
    pub title: String,
    /// The first line of the answer (or of the endings, for several).
    pub first: String,
    /// The helpers the note is about.
    pub helpers: Vec<String>,
    /// Whether one of them failed.
    pub failed: bool,
    /// What it says of them.
    pub kind: NoteKind,
}

/// The [`NoteSummary`] of `text` when it is a whole note [`note_text`]
/// wrote (a held note that travels with the user's words is not one):
/// read from its header line, or, for a note an earlier build wrote (no
/// header), from its sentences.
#[must_use]
pub fn note_summary(text: &str) -> Option<NoteSummary> {
    if text.contains(crate::daemon::local::HELD_NOTE_SEPARATOR) {
        return None;
    }
    if let Some(rest) = text.strip_prefix(NOTE_HEADER) {
        let (line, words) = rest.split_once("\n\n")?;
        let entries = parse_header(line)?;
        let first = match entries.as_slice() {
            // The answer is the paragraph after the sentence that leads it
            // (trimmed, so an empty one leaves a blank line).
            [one] if one.kind.has_answer() => words
                .split_once("\n\n")
                .and_then(|(_, answer)| (!answer.starts_with('\n')).then_some(answer))
                .and_then(|answer| answer.lines().next())
                .unwrap_or_default()
                .trim()
                .to_string(),
            _ => String::new(),
        };
        return Some(summary_from(&entries, first));
    }
    legacy_summary(text)
}

/// [`note_summary`] of a note an earlier build wrote: its sentences read
/// back (kept for stored conversations; this build's notes carry the
/// header).
fn legacy_summary(text: &str) -> Option<NoteSummary> {
    let Some((count, list)) = text.split_once(" notes about your helpers:\n") else {
        return item_summary(text);
    };
    let count: usize = count.parse().ok()?;
    let items: Vec<NoteSummary> = list
        .split("\n- ")
        .map(|item| {
            item.strip_prefix("- ")
                .unwrap_or(item)
                .replace("\n  ", "\n")
        })
        .map(|item| item_summary(&item))
        .collect::<Option<_>>()?;
    Some(NoteSummary {
        title: format!("{count} helpers ended"),
        first: items
            .iter()
            .map(|item| item.title.strip_prefix("helper ").unwrap_or(&item.title))
            .collect::<Vec<_>>()
            .join("; "),
        helpers: items.iter().flat_map(|item| item.helpers.clone()).collect(),
        failed: items.iter().any(|item| item.failed),
        kind: NoteKind::Several,
    })
}

/// [`legacy_summary`] of one ending's words (an earlier build's
/// [`item_text`]).
fn item_summary(text: &str) -> Option<NoteSummary> {
    let first_of = |body: &str| {
        body.split_once("\n\n")
            .and_then(|(_, answer)| answer.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let summary = |label: &str, kind: NoteKind, reason: Option<String>, first: String| {
        summary_from(
            &[Entry {
                label: label.to_string(),
                kind,
                reason,
            }],
            first,
        )
    };
    if let Some(rest) = text.strip_prefix("The user stopped your helper `") {
        let (label, _) = rest.split_once('`')?;
        return Some(summary(label, NoteKind::Stopped, None, String::new()));
    }
    let (label, rest) = text.strip_prefix("Your helper `")?.split_once("` ")?;
    let reason = |after: &str, until: &str| {
        rest.strip_prefix(after)
            .and_then(|r| r.split_once(until))
            .map(|(reason, _)| reason.to_string())
    };
    if rest.starts_with("finished") {
        Some(summary(label, NoteKind::Finished, None, first_of(rest)))
    } else if let Some(why) = reason("went idle after its run failed: ", ". It is still seated") {
        Some(summary(
            label,
            NoteKind::IdleFailed,
            Some(why),
            String::new(),
        ))
    } else if rest.starts_with("went idle") {
        Some(summary(label, NoteKind::Idle, None, first_of(rest)))
    } else if let Some(why) = reason("failed: ", ". Its file changes") {
        Some(summary(label, NoteKind::Failed, Some(why), String::new()))
    } else if rest.starts_with("ended when the rkat-tui daemon stopped") {
        Some(summary(label, NoteKind::DaemonStopped, None, String::new()))
    } else if rest.starts_with("stopped at its checkpoint") {
        Some(summary(label, NoteKind::Checkpoint, None, first_of(rest)))
    } else {
        None
    }
}

/// What the clients are told of a note's delivery, `told`, naming the
/// helpers it passed over because they ended first.
fn with_passed(told: String, passed: &[String]) -> String {
    if passed.is_empty() {
        told
    } else {
        format!("{told} ({} ended first)", passed.join(", "))
    }
}

/// Why a note's steer into its agent failed, as its clients are told
/// (`err`: the engine's refusal, after the reload a storage failure
/// needs, [`super::notes::steer_member`]).
fn steer_failure(err: &str) -> String {
    if crate::daemon::storage::is_storage_failure(err) {
        "the disk that holds rkat-tui's state is full or failing".to_string()
    } else {
        format!("the agent did not take it now ({})", err.trim())
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

/// A note held for an agent its user just interrupted, or one its steer
/// failed for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Held {
    agent: String,
    session: SessionId,
    text: String,
    /// Held after its steer failed: the engine may have taken it all the
    /// same, and a run that starts with it shows it did
    /// ([`HeldNotes::delivered`]). Absent in the files of earlier builds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unsure: bool,
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
    held: parking_lot::Mutex<Holding>,
}

/// What [`HeldNotes`] holds, under its one lock.
#[derive(Debug, Default)]
struct Holding {
    notes: Vec<Held>,
    /// The newest (agent, words) a run of a helper's note started with,
    /// [`SEEN`] at most: a failed steer's note is not held when a run took
    /// it already.
    seen: std::collections::VecDeque<(String, String)>,
}

/// How many runs started by a helper's note [`HeldNotes`] remembers.
const SEEN: usize = 16;

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
            held: parking_lot::Mutex::new(Holding {
                notes,
                seen: std::collections::VecDeque::new(),
            }),
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

    /// Hold `text` for `agent`, which runs `session` (`unsure`: its steer
    /// failed, [`Held::unsure`]).
    fn hold(&self, agent: &str, session: &SessionId, text: String, unsure: bool) {
        let mut held = self.held.lock();
        if unsure
            && let Some(at) = held
                .seen
                .iter()
                .position(|(seen_agent, words)| seen_agent == agent && *words == text)
        {
            held.seen.remove(at);
            return;
        }
        held.notes.push(Held {
            agent: agent.to_string(),
            session: session.clone(),
            text,
            unsure,
        });
        self.persist(&held.notes);
    }

    /// A run of `agent` started with `prompt`, a helper's note: one held
    /// after a failed steer with these very words reached it after all,
    /// and goes; none held yet, the words are remembered, so a failed
    /// steer's answer that comes after the run's start holds nothing.
    /// Whether one went.
    pub(super) fn delivered(&self, agent: &str, prompt: &str) -> bool {
        let mut held = self.held.lock();
        let before = held.notes.len();
        held.notes
            .retain(|note| !(note.unsure && note.agent == agent && note.text == prompt));
        let gone = held.notes.len() != before;
        if gone {
            self.persist(&held.notes);
        } else {
            if held.seen.len() == SEEN {
                held.seen.pop_front();
            }
            held.seen.push_back((agent.to_string(), prompt.to_string()));
        }
        gone
    }

    /// Take every note held for `agent`: the ones for `session` joined in
    /// the order they were held (`None`: none); the ones for another
    /// conversation of it go (it was left), and so does one held after a
    /// failed steer that `started_with` (the input of the run starting
    /// now) shows reached it after all.
    fn take(&self, agent: &str, session: &SessionId, started_with: Option<&str>) -> Option<String> {
        let mut held = self.held.lock();
        if !held.notes.iter().any(|note| note.agent == agent) {
            return None;
        }
        let (mine, rest): (Vec<Held>, Vec<Held>) =
            held.notes.drain(..).partition(|note| note.agent == agent);
        held.notes = rest;
        self.persist(&held.notes);
        let texts: Vec<String> = mine
            .into_iter()
            .filter(|held| &held.session == session)
            .filter(|held| !(held.unsure && Some(held.text.as_str()) == started_with))
            .map(|held| held.text)
            .collect();
        (!texts.is_empty()).then(|| texts.join("\n\n"))
    }

    /// Whether a note is held for `agent`.
    fn holds(&self, agent: &str) -> bool {
        self.held
            .lock()
            .notes
            .iter()
            .any(|held| held.agent == agent)
    }
}

/// Which agents' last run the user interrupted, with no run started since.
#[derive(Debug, Default)]
pub struct Interrupted(parking_lot::Mutex<HashSet<String>>);

/// Hand every ending `endings` delivers to `agents`, those that come within
/// [`BATCH`] of each other together, until the channel closes or the
/// daemon stops (`stopping` turns true). At the stop the batch waiting for
/// its window and whatever else is queued are not lost: they are held for
/// their agents ([`Agents::hold_at_stop`]), durably, and travel with the
/// next message to the agent after the next start (every restored agent
/// starts as just interrupted), as the endings the stop itself causes do.
/// A batch being told when the stop comes is told to its end first, so
/// each ending is told once.
pub(crate) async fn deliver(
    agents: Weak<Agents>,
    mut endings: mpsc::UnboundedReceiver<Ended>,
    mut stopping: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let first = tokio::select! {
            next = endings.recv() => match next {
                Some(first) => first,
                None => return,
            },
            _ = stopping.wait_for(|stopping| *stopping) => {
                if let Some(agents) = agents.upgrade() {
                    agents.hold_at_stop(Vec::new(), &mut endings);
                }
                return;
            }
        };
        let mut batch = vec![first];
        let Some(window) = agents.upgrade().map(|agents| agents.batch_window()) else {
            return;
        };
        let deadline = tokio::time::Instant::now() + window;
        let stopped = loop {
            tokio::select! {
                next = tokio::time::timeout_at(deadline, endings.recv()) => match next {
                    Ok(Some(next)) => batch.push(next),
                    _ => break false,
                },
                _ = stopping.wait_for(|stopping| *stopping) => break true,
            }
        };
        let Some(agents) = agents.upgrade() else {
            return;
        };
        if stopped {
            agents.hold_at_stop(batch, &mut endings);
            return;
        }
        agents.helpers_ended(batch).await;
    }
}

impl Agents {
    /// How long the endings after a batch's first wait to go with it
    /// ([`BATCH`]; tests can hold a batch open until the daemon stops).
    #[cfg_attr(not(any(test, feature = "test-support")), allow(clippy::unused_self))]
    fn batch_window(&self) -> Duration {
        #[cfg(any(test, feature = "test-support"))]
        if self.core.registry.endings().batches_held() {
            return Duration::from_hours(1);
        }
        BATCH
    }

    /// The daemon stops while `batch` (and what else `endings` holds)
    /// waits to be told: each ending is held for the agent its helper
    /// worked for, one note per agent, persisted ([`HeldNotes`]); it
    /// travels with the next message to that agent after the next start.
    /// An ending whose agent runs another conversation now goes to nobody,
    /// as at any time.
    fn hold_at_stop(&self, mut batch: Vec<Ended>, endings: &mut mpsc::UnboundedReceiver<Ended>) {
        while let Ok(next) = endings.try_recv() {
            batch.push(next);
        }
        let mut groups: Vec<((String, SessionId), Vec<NoteItem>)> = Vec::new();
        for ended in batch {
            let Some(agent) = self.agent_running_as(&ended.tag.owner) else {
                tracing::info!(helper = %ended.key, "a helper's ending at the daemon's stop: its agent runs another conversation; nobody is told");
                continue;
            };
            let item = self.note_item(&ended);
            let recipient = (agent, ended.tag.owner.clone());
            match groups.iter_mut().find(|(r, _)| *r == recipient) {
                Some((_, items)) => items.push(item),
                None => groups.push((recipient, vec![item])),
            }
        }
        for ((agent, session), items) in groups {
            tracing::info!(%agent, endings = items.len(), "helpers' endings held at the daemon's stop for the agent's next message");
            self.held.hold(&agent, &session, note_text(&items), false);
        }
    }

    /// The user interrupted `name`'s run: the endings of its helpers are
    /// held until its next run (or the user's next message).
    pub fn interrupted(&self, name: &str) {
        self.interrupted.0.lock().insert(name.to_string());
    }

    /// A run of `name` started (with input `prompt`, when known): it is no
    /// longer just interrupted, and the notes held for it go into this run
    /// (held again when that steer fails too). A note held after a failed
    /// steer that this run started with reached it after all, and goes.
    pub(crate) fn helpers_run_started(self: &Arc<Self>, name: &str, prompt: Option<&str>) {
        self.interrupted.0.lock().remove(name);
        if !self.held.holds(name) {
            return;
        }
        let Some(session) = self.core.conversations.current(name) else {
            return;
        };
        if let Some(text) = self.held.take(name, &session, prompt) {
            let agents = Arc::clone(self);
            let name = name.to_string();
            tokio::spawn(async move {
                if let Err(reason) = agents.steer_or_hold(&name, &session, text).await {
                    tracing::warn!(agent = %name, %reason, "held notes did not reach the agent's new run; they are held again");
                }
            });
        }
    }

    /// The notes held for `name` (running `session`), as one, taken: they
    /// travel with the user's message that is on its way, before its
    /// words.
    #[must_use]
    pub fn take_held(&self, name: &str, session: &SessionId) -> Option<String> {
        self.held.take(name, session, None)
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
        // own, where `assign` seats helpers): the helper
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
        let items: Vec<NoteItem> = endings.iter().map(|ended| self.note_item(ended)).collect();
        let text = note_text(&items);
        let mut recipient = recipient.clone();
        // Helpers passed over: they ended, or refused the note.
        let mut passed: Vec<String> = Vec::new();
        loop {
            let (key, tag) = match recipient {
                Recipient::Agent { name, session } => {
                    let told = if self.interrupted.0.lock().contains(&name) {
                        self.held.hold(&name, &session, text, false);
                        format!("{name} is told with your next message to it")
                    } else {
                        // The run it may start is announced with its
                        // folded line, made from the items.
                        self.sent
                            .record_summary(&name, text.clone(), summary_of(&items));
                        match self.steer_or_hold(&name, &session, text).await {
                            Ok(()) => format!("{name}{TOLD}"),
                            Err(reason) => {
                                format!("{name} is told with its next run: {reason}")
                            }
                        }
                    };
                    return with_passed(told, &passed);
                }
                Recipient::Helper { key, tag } => (key, tag),
            };
            let endings = self.core.registry.endings();
            let sent = endings
                .while_unsettled(
                    &key,
                    super::notes::steer_member(
                        &self.core,
                        &key.mob,
                        key.member.clone(),
                        text.clone(),
                    ),
                )
                .await;
            match sent {
                Some(Ok(())) => {
                    return with_passed(
                        format!("its parent helper {}{TOLD}", tag.label()),
                        &passed,
                    );
                }
                Some(Err(err)) => {
                    tracing::warn!(helper = %key, error = %err, "a helper refused its own helper's ending; it goes further up");
                }
                None => {
                    tracing::info!(helper = %key, "a helper ended before its own helper's ending reached it; it goes further up");
                }
            }
            passed.push(tag.label().to_string());
            // The one the passed-over helper's own ending goes to.
            match self.recipient(&key, &tag) {
                Some(next) => recipient = next,
                None => {
                    let passed = passed.join(", ");
                    return match self.keep_untold(&tag.owner, &text) {
                        Some(path) => format!(
                            "nobody could be told: {passed} ended first; the note is in {}",
                            path.display()
                        ),
                        None => format!("nobody could be told: {passed} ended first"),
                    };
                }
            }
        }
    }

    /// Steer `text` into `name`'s run (it runs `session`), or start a run
    /// with it when it is idle (a run no user started, attributed as a
    /// helper's ending). A steer that fails holds the note ([`HeldNotes`],
    /// durably): it travels with the next message to the agent or goes
    /// into its next run, once. Why it failed, in plain words.
    async fn steer_or_hold(
        self: &Arc<Self>,
        name: &str,
        session: &SessionId,
        text: String,
    ) -> Result<(), String> {
        let Err(err) = self
            .steer(name, text.clone(), super::notes::Wake::HelperEnded)
            .await
        else {
            return Ok(());
        };
        tracing::warn!(agent = %name, error = %err, "a helper's note did not reach the agent; it is held for its next run");
        // Not held when a run started with it meanwhile (the engine took
        // it before it answered the error).
        self.held.hold(name, session, text, true);
        Err(steer_failure(&err))
    }

    /// A note nobody can take (every helper on its way ended or refused
    /// it, and its agent runs another conversation now): held for that
    /// agent's conversation if it runs it again meanwhile, else written to
    /// the agent's scratch directory, and its clients are told where.
    /// Where it was written (`None`: held, or no directory to write to).
    fn keep_untold(&self, owner: &SessionId, text: &str) -> Option<PathBuf> {
        if let Some(agent) = self.agent_running_as(owner) {
            self.held.hold(&agent, owner, text.to_string(), false);
            return None;
        }
        let registry = &self.core.registry;
        let agent = registry
            .root_of_session(owner)
            .map(|key| key.member.to_string());
        let written = registry.scratch_of_session(owner).and_then(|dir| {
            let id = crate::hex::random::<4>().unwrap_or_else(|_| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_nanos());
                format!("{nanos:x}")
            });
            let path = dir.join(format!("helper-note-{id}.md"));
            match crate::paths::write_atomic(
                &path,
                text.as_bytes(),
                crate::paths::FileMode::Private,
                Some(&dir),
            ) {
                Ok(()) => Some(path),
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "a note nobody could take was not written");
                    None
                }
            }
        });
        match (&written, agent) {
            (Some(path), Some(agent)) => self.core.decisions.agent_notice(
                agent,
                true,
                format!(
                    "a note of your helpers' endings reached nobody (the helpers it went to ended, and that conversation is not running); it is in {}",
                    path.display()
                ),
            ),
            (Some(path), None) => {
                tracing::warn!(path = %path.display(), "a note nobody could take was written; its agent is gone");
            }
            (None, _) => tracing::warn!(
                "a helper's parents are gone and its note could not be kept: nobody is told of its ending"
            ),
        }
        written
    }

    /// `ended` as its note words it (a long answer written in full,
    /// [`Self::write_rest`]).
    fn note_item(&self, ended: &Ended) -> NoteItem {
        NoteItem {
            label: ended.tag.label().to_string(),
            rest: self.write_rest(ended),
            how: ended.how.clone(),
            with: ended.with.clone(),
        }
    }

    /// A finished helper's answer longer than a note carries, written in
    /// full to its agent's scratch directory (`None`: not cut, or there is
    /// no directory, or it could not be written).
    fn write_rest(&self, ended: &Ended) -> Option<PathBuf> {
        let (How::Done { answer }
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
                ending: ended.how.ending(),
                how: ended.how.text(),
                told,
            },
        );
    }

    /// A helper of any kind whose retire was decided before the last stop
    /// (it was done) and did not reach the engine is retired now, when its
    /// mob still seats it; nobody is told (its parent was, then). A
    /// `delegate` helper's row an earlier build stored (its mob
    /// `implicit-<session>`) is read and left alone.
    pub(crate) async fn settle_left_helpers(&self) {
        let registry = &self.core.registry;
        for (key, _) in registry.bound_members() {
            if registry.retire_owed(&key) {
                match self.core.state.handle_for(&key.mob).await {
                    Ok(handle)
                        if handle
                            .list_members()
                            .await
                            .iter()
                            .any(|member| member.agent_identity == key.member) =>
                    {
                        registry.endings().retire_owed(&key);
                    }
                    // Its mob seats it no more: nothing is owed.
                    Ok(_) => registry.retired(&key),
                    Err(err) => {
                        tracing::warn!(helper = %key, error = %err, "cannot reach the mob of a helper whose retire is owed");
                    }
                }
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
            with: Vec::new(),
        }
    }

    /// The note's words below its header line.
    fn words(items: &[NoteItem]) -> String {
        let text = note_text(items);
        let (header, words) = text.split_once("\n\n").unwrap();
        assert!(header.starts_with(NOTE_HEADER), "{header}");
        words.to_string()
    }

    /// Every kind of ending, and the endings with something odd in them:
    /// an empty answer, a reason over lines with a `;`, a long answer.
    fn every_kind(answer: &str) -> Vec<NoteItem> {
        let mut items = vec![
            item(
                "b",
                How::Done {
                    answer: answer.into(),
                },
            ),
            item(
                "c",
                How::WentIdle {
                    answer: answer.into(),
                    failed: None,
                },
            ),
            item(
                "d",
                How::WentIdle {
                    answer: answer.into(),
                    failed: Some("a 400; then\nnothing".into()),
                },
            ),
            item(
                "e",
                How::Failed {
                    reason: "InterruptedByRecovery".into(),
                },
            ),
            item(
                "f",
                How::Failed {
                    reason: "it timed out; twice".into(),
                },
            ),
            item("g", How::StoppedByUser),
        ];
        for retired in [true, false] {
            items.push(item(
                if retired { "i" } else { "j" },
                How::CheckpointStopped {
                    steps: 40,
                    reason: "no progress".into(),
                    status: answer.into(),
                    retired,
                },
            ));
        }
        items[0].with = vec!["k".into()];
        items
    }

    /// The header both ways: what [`note_text`] writes of every kind of
    /// ending, alone and in a batch, [`note_summary`] reads back as the
    /// summary [`summary_of`] makes from the items themselves. A wording
    /// change of the sentences cannot break the fold; a change of the
    /// header that breaks it fails here.
    #[test]
    fn every_note_s_header_reads_back_as_its_items_summary() {
        for answer in [
            "  The parser drops the last token.\nSecond line.",
            "",
            "one line",
            &"x".repeat(MAX_ANSWER + 10),
        ] {
            let items = every_kind(answer);
            for one in &items {
                let one = std::slice::from_ref(one);
                assert_eq!(
                    note_summary(&note_text(one)),
                    Some(summary_of(one)),
                    "{}",
                    note_text(one)
                );
            }
            assert_eq!(
                note_summary(&note_text(&items)),
                Some(summary_of(&items)),
                "{}",
                note_text(&items)
            );
            assert_eq!(
                note_summary(&note_text(&items[..2])),
                Some(summary_of(&items[..2]))
            );
        }
        // The header's words, pinned.
        let text = note_text(&[item("review-x", How::StoppedByUser)]);
        assert!(
            text.starts_with("rkat-tui note: helper `review-x` was stopped by the user\n\n"),
            "{text}"
        );
        let text = note_text(&[
            item(
                "review-x",
                How::Done {
                    answer: "ok".into(),
                },
            ),
            item(
                "lint-y",
                How::Failed {
                    reason: "it timed out".into(),
                },
            ),
        ]);
        assert!(
            text.starts_with(
                "rkat-tui note: 2 helpers: `review-x` finished; `lint-y` failed: it timed out\n\n"
            ),
            "{text}"
        );
        let kinds: Vec<NoteKind> = every_kind("a")
            .iter()
            .map(|item| summary_of(std::slice::from_ref(item)).kind)
            .collect();
        assert_eq!(
            kinds,
            [
                NoteKind::Finished,
                NoteKind::Idle,
                NoteKind::IdleFailed,
                NoteKind::Failed,
                NoteKind::Failed,
                NoteKind::Stopped,
                NoteKind::Checkpoint,
                NoteKind::Checkpoint,
            ]
        );
        // A stored note of a helper that ended with the daemon's stop
        // (earlier builds wrote them) still reads back.
        assert_eq!(
            note_summary("rkat-tui note: helper `x` ended when the daemon stopped\n\nwords")
                .map(|summary| summary.kind),
            Some(NoteKind::DaemonStopped)
        );
        // A header this build cannot read is no note of its own.
        assert_eq!(
            note_summary("rkat-tui note: helper `x` danced\n\nwords"),
            None
        );
    }

    /// Notes earlier builds wrote (no header) still fold: their literal
    /// texts, as stored conversations hold them.
    #[test]
    fn notes_of_earlier_builds_still_fold() {
        let line = |text: &str| {
            let summary = note_summary(text).unwrap();
            (summary.title, summary.first, summary.failed, summary.kind)
        };
        assert_eq!(
            line(
                "Your helper `scout` finished its task. Its answer:\n\nthe bug is in parse()\n\nIt is retired: it takes no more messages (start another helper for more work)."
            ),
            (
                "helper scout finished".into(),
                "the bug is in parse()".into(),
                false,
                NoteKind::Finished
            )
        );
        assert_eq!(
            line(
                "Your helper `w1` went idle. Its last message:\n\nwaiting for more\n\nIt is still seated, idle: a message to it starts another run of it, so message it with `send_message` only to give it more work, and retire it with `retire_helper` when you are done with it."
            ),
            (
                "helper w1 went idle".into(),
                "waiting for more".into(),
                false,
                NoteKind::Idle
            )
        );
        assert_eq!(
            line(
                "Your helper `w1` went idle after its run failed: a 400. It is still seated: message it with `send_message` to try again, or retire it with `retire_helper`."
            ),
            (
                "helper w1 went idle: a 400".into(),
                String::new(),
                true,
                NoteKind::IdleFailed
            )
        );
        assert_eq!(
            line(
                "Your helper `scout` failed: it timed out. Its file changes stay on disk; its conversation is not continued."
            ),
            (
                "helper scout failed: it timed out".into(),
                String::new(),
                true,
                NoteKind::Failed
            )
        );
        assert_eq!(
            line(
                "The user stopped your helper `scout`. Its file changes stay on disk; its conversation is not continued."
            ),
            (
                "helper scout stopped by you".into(),
                String::new(),
                false,
                NoteKind::Stopped
            )
        );
        assert_eq!(
            line(
                "Your helper `scout` ended when the rkat-tui daemon stopped, before it finished. Its file changes stay on disk; its conversation is not continued."
            ),
            (
                "helper scout ended when the daemon stopped".into(),
                String::new(),
                false,
                NoteKind::DaemonStopped
            )
        );
        assert_eq!(
            line(
                "Your helper `scout` stopped at its checkpoint after 40 steps and was not continued: no progress. Its last status:\n\nhalf done\n\nIt is still seated, idle: message it with `send_message` to go on, or retire it with `retire_helper`."
            ),
            (
                "helper scout stopped at its checkpoint".into(),
                "half done".into(),
                false,
                NoteKind::Checkpoint
            )
        );
        let two = "2 notes about your helpers:\n- The user stopped your helper `a`. Its file changes stay on disk; its conversation is not continued.\n- Your helper `b` failed: it timed out. Its file changes stay on disk; its conversation is not continued.";
        assert_eq!(
            line(two),
            (
                "2 helpers ended".into(),
                "a stopped by you; b failed: it timed out".into(),
                true,
                NoteKind::Several
            )
        );
        // A held note with the user's words after it is no whole note.
        assert_eq!(
            note_summary(&format!(
                "{two}\n\n{}\n\nnow fix it",
                crate::daemon::local::HELD_NOTE_SEPARATOR
            )),
            None
        );
    }

    /// A note names the helpers of its own a helper was retired with, and
    /// still reads back as its one line.
    #[test]
    fn a_note_names_the_helpers_retired_with_it() {
        let mut done = item(
            "p1",
            How::Done {
                answer: "all done".into(),
            },
        );
        done.with = vec!["c1".into()];
        let text = note_text(&[done.clone()]);
        assert!(
            text.ends_with("Its own helper `c1` was retired with it."),
            "{text}"
        );
        assert_eq!(
            note_summary(&text).map(|s| s.title),
            Some("helper p1 finished".to_string())
        );
        done.with = vec!["c2".into(), "c1".into()];
        let mut stopped = item("p2", How::StoppedByUser);
        stopped.with = vec!["c3".into()];
        let text = note_text(&[done, stopped]);
        assert!(
            text.contains("Its own helpers `c2`, `c1` were retired with it."),
            "{text}"
        );
        assert!(
            text.contains("The user stopped your helper `p2`. Its file changes stay on disk; its conversation is not continued. Its own helper `c3` was retired with it."),
            "{text}"
        );
    }

    /// Every note [`note_text`] writes reads back as its one line: what
    /// happened, the answer's first line, the helpers, whether one failed;
    /// a held note with the user's words after it, and other text, do not.
    #[test]
    fn every_note_reads_back_as_one_line() {
        let answer = "\n  The parser drops the last token.\nSecond line.".to_string();
        let one = |how: How| note_summary(&note_text(&[item("review-x", how)])).unwrap();
        let line = |s: &NoteSummary| (s.title.clone(), s.first.clone(), s.failed);
        let first = "The parser drops the last token.".to_string();
        assert_eq!(
            line(&one(How::Done {
                answer: answer.clone()
            })),
            ("helper review-x finished".into(), first.clone(), false)
        );
        assert_eq!(
            line(&one(How::WentIdle {
                answer: answer.clone(),
                failed: None
            })),
            ("helper review-x went idle".into(), first.clone(), false)
        );
        assert_eq!(
            line(&one(How::WentIdle {
                answer: String::new(),
                failed: Some("the model refused".into())
            })),
            (
                "helper review-x went idle: the model refused".into(),
                String::new(),
                true
            )
        );
        assert_eq!(
            line(&one(How::Failed {
                reason: "the model refused".into()
            })),
            (
                "helper review-x failed: the model refused".into(),
                String::new(),
                true
            )
        );
        assert_eq!(
            one(How::StoppedByUser).title,
            "helper review-x stopped by you"
        );
        for retired in [true, false] {
            assert_eq!(
                line(&one(How::CheckpointStopped {
                    steps: 40,
                    reason: "no progress".into(),
                    status: answer.clone(),
                    retired,
                })),
                (
                    "helper review-x stopped at its checkpoint".into(),
                    first.clone(),
                    false
                )
            );
        }
        let two = note_text(&[
            item(
                "review-x",
                How::Done {
                    answer: "line one\n- a list item\nmore".into(),
                },
            ),
            item(
                "lint-y",
                How::Failed {
                    reason: "it timed out".into(),
                },
            ),
        ]);
        let both = note_summary(&two).unwrap();
        assert_eq!(both.title, "2 helpers ended");
        assert_eq!(both.first, "review-x finished; lint-y failed: it timed out");
        assert_eq!(both.helpers, ["review-x", "lint-y"]);
        assert!(both.failed);
        let held = format!(
            "{two}\n\n{}\n\nnow fix it",
            crate::daemon::local::HELD_NOTE_SEPARATOR
        );
        assert_eq!(note_summary(&held), None);
        assert_eq!(note_summary("Your helper is great"), None);
        assert_eq!(note_summary("please review the parser"), None);
    }

    #[test]
    fn each_ending_is_told_in_plain_words() {
        let done = words(&[item(
            "scout",
            How::Done {
                answer: "  the bug is in parse()\n".into(),
            },
        )]);
        assert_eq!(
            done,
            "Your helper `scout` finished its task. Its answer:\n\nthe bug is in parse()\n\nIt is retired: it takes no more messages (start another helper for more work)."
        );
        let idle = words(&[item(
            "w1",
            How::WentIdle {
                answer: "waiting for more".into(),
                failed: None,
            },
        )]);
        assert_eq!(
            idle,
            "Your helper `w1` went idle. Its last message:\n\nwaiting for more\n\nIt is still seated, idle: a message to it starts another run of it, so message it with `send_message` only to give it more work, and retire it with `retire_helper` when you are done with it."
        );
        let idle_failed = words(&[item(
            "w1",
            How::WentIdle {
                answer: String::new(),
                failed: Some("a 400".into()),
            },
        )]);
        assert!(
            idle_failed.starts_with("Your helper `w1` went idle after its run failed: a 400.")
                && idle_failed.contains("`retire_helper`"),
            "{idle_failed}"
        );
        let stopped = words(&[item("scout", How::StoppedByUser)]);
        assert_eq!(
            stopped,
            "The user stopped your helper `scout`. Its file changes stay on disk; its conversation is not continued."
        );
        let failed = words(&[item(
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
        let cut = words(&[item(
            "scout",
            How::Done {
                answer: answer.clone(),
            },
        )]);
        assert!(!cut.contains(&answer));
        assert!(cut.contains("the user can read the whole of it in the helper's view"));
        let mut written = item("scout", How::Done { answer });
        written.rest = Some(PathBuf::from("/tmp/scratch/helper-scout-answer.md"));
        let cut = note_text(&[written]);
        assert!(
            cut.contains("the whole of it is in /tmp/scratch/helper-scout-answer.md"),
            "{cut}"
        );
    }

    #[test]
    fn several_endings_go_as_one_note_listing_each() {
        let text = words(&[
            item("a", How::StoppedByUser),
            item(
                "b",
                How::Done {
                    answer: "line one\nline two".into(),
                },
            ),
        ]);
        assert!(
            text.starts_with("2 notes about your helpers:\n- The user stopped your helper `a`.")
        );
        assert!(
            text.contains("\n- Your helper `b` finished its task."),
            "{text}"
        );
        assert!(text.contains("\n  line one\n  line two"), "{text}");
    }

    #[test]
    fn held_notes_survive_a_reopen_and_go_once_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let held = HeldNotes::open(dir.path());
        held.hold("main-a", &session, "first".into(), false);
        held.hold("main-a", &session, "second".into(), true);
        held.hold("main-b", &session, "other".into(), false);
        assert!(held.holds("main-a"));
        let reopened = HeldNotes::open(dir.path());
        assert_eq!(
            reopened.take("main-a", &session, None).as_deref(),
            Some("first\n\nsecond")
        );
        assert_eq!(reopened.take("main-a", &session, None), None);
        // A note for a conversation the agent left goes, untold.
        reopened.hold("main-b", &SessionId::new(), "stale".into(), false);
        assert_eq!(
            reopened.take("main-b", &session, None).as_deref(),
            Some("other")
        );
        assert!(!reopened.holds("main-b"));
        // No file, or an unreadable one: nothing held.
        let empty = tempfile::tempdir().unwrap();
        assert!(!HeldNotes::open(empty.path()).holds("main-a"));
        std::fs::write(empty.path().join(HELD_FILE), b"not json").unwrap();
        assert!(!HeldNotes::open(empty.path()).holds("main-a"));
    }

    /// A note held after a failed steer goes once: with the next take, or
    /// at the run that started with its very words (the engine took it
    /// after all), never both; a note held at an interrupt is not taken
    /// for delivered by a run's words. The file of an earlier build (no
    /// `unsure`) still opens.
    #[test]
    fn a_note_held_after_a_failed_steer_goes_once() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let held = HeldNotes::open(dir.path());
        held.hold("main-a", &session, "NOTE".into(), true);
        assert!(held.delivered("main-a", "NOTE"));
        assert!(!held.holds("main-a"));
        held.hold("main-a", &session, "NOTE".into(), true);
        held.hold("main-a", &session, "OTHER".into(), true);
        assert_eq!(
            held.take("main-a", &session, Some("NOTE")).as_deref(),
            Some("OTHER")
        );
        // The run started before the failed steer's answer came.
        assert!(!held.delivered("main-a", "EARLY"));
        held.hold("main-a", &session, "EARLY".into(), true);
        assert!(!held.holds("main-a"));
        held.hold("main-a", &session, "KEPT".into(), false);
        assert!(!held.delivered("main-a", "KEPT"));
        assert_eq!(
            HeldNotes::open(dir.path())
                .take("main-a", &session, Some("KEPT"))
                .as_deref(),
            Some("KEPT")
        );
        let old = tempfile::tempdir().unwrap();
        std::fs::write(
            old.path().join(HELD_FILE),
            format!(
                r#"{{"version":1,"notes":[{{"agent":"main-a","session":"{session}","text":"OLD"}}]}}"#
            ),
        )
        .unwrap();
        assert_eq!(
            HeldNotes::open(old.path())
                .take("main-a", &session, None)
                .as_deref(),
            Some("OLD")
        );
        assert_eq!(
            steer_failure("SQLite failed: database or disk is full"),
            "the disk that holds rkat-tui's state is full or failing"
        );
        assert_eq!(
            steer_failure("member gone "),
            "the agent did not take it now (member gone)"
        );
    }
}
