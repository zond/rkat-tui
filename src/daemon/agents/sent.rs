//! What the daemon last sent each agent, so the run a note starts is
//! attributed to it ([`super::triggers`], [`super::notes`]).
//!
//! One [`Sent`] record per agent holds every kind of note:
//! - the last watch event ([`Kind::Watch`]): a run it starts is the
//!   watch's;
//! - the last note of a helper's ending ([`Kind::HelperEnding`]): a run it
//!   starts is that ending's, announced with the note's folded line when
//!   the daemon made one ([`Sent::helper_summary`]);
//! - the last note of an approval from `/permissions` ([`Kind::Approval`]):
//!   a run it starts is the user's.
//!
//! [`SentNotes`] owns the records: an agent's departure drops its record in
//! one take, and a record with nothing left in it goes, so the map holds
//! only agents with a note in flight. Each method takes the lock and lets
//! it go before it returns; no caller holds it across an await.

use std::collections::HashMap;

use super::endings::NoteSummary;

/// A kind of note the daemon sends an agent and attributes the run of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A watch's batch of file events.
    Watch,
    /// A note of helpers' endings.
    HelperEnding,
    /// A note of the user's approval of a blocked call.
    Approval,
}

/// What the daemon last sent one agent, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Sent {
    watch: Option<String>,
    helper: Option<String>,
    /// The last helper-ending note steered in, with its folded line.
    helper_summary: Option<(String, NoteSummary)>,
    approval: Option<String>,
}

impl Sent {
    /// The text of `kind`'s last note.
    fn slot(&mut self, kind: Kind) -> &mut Option<String> {
        match kind {
            Kind::Watch => &mut self.watch,
            Kind::HelperEnding => &mut self.helper,
            Kind::Approval => &mut self.approval,
        }
    }

    /// Whether nothing of the agent is held.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Every agent's [`Sent`] record (see the module doc).
#[derive(Debug, Default)]
pub(super) struct SentNotes(parking_lot::Mutex<HashMap<String, Sent>>);

impl SentNotes {
    /// Change `name`'s record with `f`; a record left empty goes.
    fn with<T>(&self, name: &str, f: impl FnOnce(&mut Sent) -> T) -> T {
        let mut records = self.0.lock();
        let record = records.entry(name.to_string()).or_default();
        let out = f(record);
        if record.is_empty() {
            records.remove(name);
        }
        out
    }

    /// `text` is the last note of `kind` sent to `name`.
    pub(super) fn record(&self, name: &str, kind: Kind, text: String) {
        self.with(name, |sent| *sent.slot(kind) = Some(text));
    }

    /// `name`'s last note of `kind` is no longer in flight.
    pub(super) fn forget(&self, name: &str, kind: Kind) {
        self.with(name, |sent| *sent.slot(kind) = None);
    }

    /// Whether `name`'s last note of `kind` is one `started` says started
    /// a run; it is taken then (one run per note).
    pub(super) fn take_if(
        &self,
        name: &str,
        kind: Kind,
        started: impl FnOnce(&str) -> bool,
    ) -> bool {
        self.with(name, |sent| {
            let slot = sent.slot(kind);
            let matched = slot.as_deref().is_some_and(started);
            if matched {
                *slot = None;
            }
            matched
        })
    }

    /// The helper-ending note `text` steered into `name` folds to
    /// `summary`.
    pub(super) fn record_summary(&self, name: &str, text: String, summary: NoteSummary) {
        self.with(name, |sent| sent.helper_summary = Some((text, summary)));
    }

    /// Take `name`'s last helper-ending note's folded line, with the note.
    pub(super) fn take_summary(&self, name: &str) -> Option<(String, NoteSummary)> {
        self.with(name, |sent| sent.helper_summary.take())
    }

    /// `name` left: nothing of it stays.
    pub(super) fn drop_agent(&self, name: &str) {
        self.0.lock().remove(name);
    }

    /// Whether anything of `name` is held (tests).
    #[cfg(test)]
    fn holds(&self, name: &str) -> bool {
        self.0.lock().contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::daemon::agents::endings::NoteKind;

    fn summary(title: &str) -> NoteSummary {
        NoteSummary {
            title: title.to_string(),
            first: String::new(),
            helpers: vec!["scout".to_string()],
            failed: false,
            kind: NoteKind::Finished,
        }
    }

    #[test]
    fn an_agents_departure_leaves_nothing_of_it_behind() {
        let notes = SentNotes::default();
        notes.record("a", Kind::Watch, "files".into());
        notes.record("a", Kind::HelperEnding, "ended".into());
        notes.record("a", Kind::Approval, "approved".into());
        notes.record_summary("a", "ended".into(), summary("helper scout finished"));
        notes.record("b", Kind::Watch, "files".into());
        notes.drop_agent("a");
        assert!(!notes.holds("a"));
        assert!(!notes.take_if("a", Kind::Watch, |_| true));
        assert!(!notes.take_if("a", Kind::HelperEnding, |_| true));
        assert!(!notes.take_if("a", Kind::Approval, |_| true));
        assert_eq!(notes.take_summary("a"), None);
        assert!(notes.holds("b"), "another agent's notes stay");
    }

    #[test]
    fn each_note_stays_attributed_to_its_agent_its_kind_and_its_text() {
        let notes = SentNotes::default();
        notes.record("a", Kind::HelperEnding, "a's ending".into());
        notes.record("a", Kind::Approval, "a's approval".into());
        notes.record("b", Kind::HelperEnding, "b's ending".into());
        notes.record_summary("a", "a's ending".into(), summary("helper scout finished"));
        // Another text, kind or agent does not take it.
        assert!(!notes.take_if("a", Kind::HelperEnding, |text| text == "b's ending"));
        assert!(!notes.take_if("a", Kind::Watch, |_| true));
        assert!(!notes.take_if("c", Kind::HelperEnding, |_| true));
        // Its own does, once, and leaves the other kinds.
        assert!(notes.take_if("a", Kind::HelperEnding, |text| text == "a's ending"));
        assert!(!notes.take_if("a", Kind::HelperEnding, |_| true));
        assert!(notes.take_if("a", Kind::Approval, |text| text.contains("approval")));
        assert_eq!(
            notes.take_summary("a"),
            Some(("a's ending".to_string(), summary("helper scout finished")))
        );
        assert!(!notes.holds("a"), "an emptied record goes");
        assert!(notes.take_if("b", Kind::HelperEnding, |text| text == "b's ending"));
    }

    #[test]
    fn a_later_note_of_a_kind_replaces_the_earlier_and_forget_drops_it() {
        let notes = SentNotes::default();
        notes.record("a", Kind::Watch, "first".into());
        notes.record("a", Kind::Watch, "second".into());
        assert!(!notes.take_if("a", Kind::Watch, |text| text == "first"));
        notes.forget("a", Kind::Watch);
        assert!(!notes.holds("a"));
    }
}
