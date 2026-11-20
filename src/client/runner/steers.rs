//! This client's steers on their way to the agent, matched to the
//! runtime's inputs by id. They are followed across turns: a steer the
//! runtime still holds when a turn ends (it started no run for it in
//! time) stays with the agent until its own boundary event, or the run
//! that carries it, arrives.
//!
//! `mob/member_send` answers the input the member's runtime admitted for a
//! steer (`admission.input_id`); the agent's
//! `BoundaryAppendApplied` names the input it wrote into the running turn.
//! A steer is delivered when the event names its input, so with two
//! clients steering one turn each marks only its own. The event can come
//! before the send's answer (the stream is not ordered with the call): an
//! applied input no steer is known for yet is kept, and the answer that
//! names it delivers at once. Another client's input is kept the same way
//! and never matches; it is kept only while a send is unanswered.

use std::collections::HashSet;

use meerkat_core::lifecycle::InputId;

use crate::ui::delivery::PendingId;

/// The runner's side of its steers: the steers followed, the ones the
/// daemon admitted, and whether what became of them is being asked.
#[derive(Debug, Default)]
pub(super) struct Steering {
    /// This client's steers on their way to the agent, followed across
    /// turns by the inputs the runtime admitted them as.
    pub(super) steers: Steers,
    /// The steers the daemon admitted: the agent has them, or gets them
    /// after this client leaves, so quitting saves none of them.
    pub(super) admitted: Vec<PendingId>,
    /// The question of what became of the followed steers
    /// (`rkat/inputs`).
    pub(super) fates: FatesAsk,
}

/// Where the question of the followed steers' fates stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum FatesAsk {
    /// Nothing is being asked.
    #[default]
    NotAsked,
    /// The question is on its way.
    Asking,
    /// An idle notice came while it was: ask again on the answer.
    AskAgain,
}

/// This client's steers on their way to the agent.
#[derive(Debug, Default)]
pub(super) struct Steers {
    /// Sent, the daemon's answer not in yet (oldest first).
    sending: Vec<PendingId>,
    /// Admitted under the input the runtime named, not applied yet
    /// (oldest first).
    admitted: Vec<(InputId, PendingId)>,
    /// Inputs applied while a send was unanswered that no admitted steer
    /// was known for when the event came: this client's, answered later,
    /// or another client's (forgotten once no send is unanswered).
    applied: HashSet<InputId>,
    /// Each followed steer's text as sent: a run's start names no inputs
    /// (`RunStarted` carries the run's content only), so a run is taken to
    /// carry a steer only when its content holds that text
    /// ([`Self::carried`]).
    texts: Vec<(PendingId, String)>,
}

impl Steers {
    /// `id` was handed to the sender.
    pub(super) fn sending(&mut self, id: PendingId) {
        self.sending.push(id);
    }

    /// The send of `id` was answered with the input the runtime admitted:
    /// `true` when that input was already applied (deliver it now). A
    /// steer a run took meanwhile ([`Self::carried`]) is not filed
    /// again.
    pub(super) fn admitted(&mut self, id: PendingId, input: InputId) -> bool {
        let Some(at) = self.sending.iter().position(|sent| *sent == id) else {
            return false;
        };
        self.sending.remove(at);
        let applied = self.applied.remove(&input);
        if !applied {
            self.admitted.push((input, id));
        }
        self.forget_applied();
        applied
    }

    /// The send of `id` failed, or was answered without an input: it is
    /// not followed any more.
    pub(super) fn forget(&mut self, id: PendingId) {
        self.sending.retain(|sent| *sent != id);
        self.admitted.retain(|(_, steer)| *steer != id);
        self.forget_applied();
    }

    /// No send is unanswered: no answer can name an applied input any
    /// more.
    fn forget_applied(&mut self) {
        if self.sending.is_empty() {
            self.applied.clear();
        }
    }

    /// The runtime wrote `input` into the running turn: the steer it
    /// belongs to, if it is this client's and known yet.
    pub(super) fn applied(&mut self, input: &InputId) -> Option<PendingId> {
        match self
            .admitted
            .iter()
            .position(|(admitted, _)| admitted == input)
        {
            Some(at) => Some(self.admitted.remove(at).1),
            None => {
                if !self.sending.is_empty() {
                    self.applied.insert(input.clone());
                }
                None
            }
        }
    }

    /// The daemon says `input` reached a terminal state (written into a
    /// run, or ended without one): its steer, no longer followed, if it is
    /// still followed.
    pub(super) fn settled(&mut self, input: &InputId) -> Option<PendingId> {
        let at = self
            .admitted
            .iter()
            .position(|(admitted, _)| admitted == input)?;
        Some(self.admitted.remove(at).1)
    }

    /// The admitted steers not applied yet, oldest first (what a recall,
    /// or a question of their fates, names to the daemon).
    pub(super) fn outstanding(&self) -> Vec<(PendingId, InputId)> {
        self.admitted
            .iter()
            .map(|(input, id)| (*id, input.clone()))
            .collect()
    }

    /// `inputs` were taken back before they reached the agent: their
    /// steers, no longer followed, in the order they were admitted.
    pub(super) fn recalled(&mut self, inputs: &[InputId]) -> Vec<PendingId> {
        let mut taken = Vec::new();
        self.admitted.retain(|(input, id)| {
            let recalled = inputs.contains(input);
            if recalled {
                taken.push(*id);
            }
            !recalled
        });
        taken
    }

    /// The text `id` was sent with (a steer of this client's whose run is
    /// told by its content, [`Self::carried`]).
    pub(super) fn sent_text(&mut self, id: PendingId, text: &str) {
        self.texts.retain(|(steer, _)| *steer != id);
        self.texts.push((id, text.trim().to_string()));
    }

    /// The steers still followed that a run whose content is `content`
    /// carries, oldest first, taken: each whose text the content holds
    /// (a run carries the inputs that missed the turn's boundary, merged).
    /// A run of anything else (a daemon's note, a peer's message) carries
    /// none of them: they stay followed until their own boundary event,
    /// run, or the daemon's word on them (`rkat/inputs`).
    pub(super) fn carried(&mut self, content: &str) -> Vec<PendingId> {
        let followed = |id: &PendingId, steers: &Self| {
            steers.sending.contains(id) || steers.admitted.iter().any(|(_, steer)| steer == id)
        };
        let texts = std::mem::take(&mut self.texts);
        let mut taken = Vec::new();
        for (id, text) in texts {
            if !followed(&id, self) {
                continue;
            }
            if !text.is_empty() && content.contains(&text) {
                taken.push(id);
            } else {
                self.texts.push((id, text));
            }
        }
        self.sending.retain(|id| !taken.contains(id));
        self.admitted.retain(|(_, id)| !taken.contains(id));
        self.forget_applied();
        taken.sort();
        taken
    }

    /// Whether a steer's send is still unanswered (it may start a run of
    /// its own: the turn waits for it).
    pub(super) fn sending_any(&self) -> bool {
        !self.sending.is_empty()
    }

    /// Whether `id` is still followed.
    #[cfg(test)]
    pub(super) fn holds(&self, id: PendingId) -> bool {
        self.sending.contains(&id) || self.admitted.iter().any(|(_, steer)| *steer == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> InputId {
        InputId::new()
    }

    #[test]
    fn a_steer_is_delivered_by_its_own_input_only() {
        let mut steers = Steers::default();
        let (mine, other) = (input(), input());
        steers.sending(PendingId(1));
        assert!(!steers.admitted(PendingId(1), mine.clone()));
        // Another client's input is applied first: it marks nothing.
        assert_eq!(steers.applied(&other), None);
        assert!(steers.holds(PendingId(1)));
        assert_eq!(steers.applied(&mine), Some(PendingId(1)));
        assert!(!steers.holds(PendingId(1)));
        // The same input again (a redelivery) marks nothing twice.
        assert_eq!(steers.applied(&mine), None);
    }

    #[test]
    fn an_input_applied_before_its_answer_delivers_on_the_answer() {
        let mut steers = Steers::default();
        let early = input();
        steers.sending(PendingId(1));
        assert_eq!(steers.applied(&early), None);
        assert!(steers.admitted(PendingId(1), early));
        assert!(!steers.holds(PendingId(1)));
        assert!(steers.outstanding().is_empty());
    }

    #[test]
    fn steers_are_matched_out_of_order() {
        let mut steers = Steers::default();
        let (first, second) = (input(), input());
        steers.sending(PendingId(1));
        steers.sending(PendingId(2));
        steers.admitted(PendingId(1), first.clone());
        steers.admitted(PendingId(2), second.clone());
        assert_eq!(steers.applied(&second), Some(PendingId(2)));
        assert!(steers.holds(PendingId(1)));
        assert_eq!(steers.applied(&first), Some(PendingId(1)));
    }

    /// A run takes only the steers its content holds; a run
    /// of something else (a daemon's note) takes none, and they stay
    /// followed. A failed send is forgotten.
    #[test]
    fn a_run_takes_only_the_steers_it_carries() {
        let mut steers = Steers::default();
        for (n, text) in [(1, "use nextest"), (2, "never mind"), (3, "and the docs")] {
            steers.sending(PendingId(n));
            steers.sent_text(PendingId(n), text);
        }
        steers.admitted(PendingId(1), input());
        steers.forget(PendingId(2));
        // A run of a helper's note carries none of them.
        assert!(
            steers
                .carried("rkat-tui note: helper `scout` finished")
                .is_empty()
        );
        assert!(steers.holds(PendingId(1)) && steers.holds(PendingId(3)));
        // A follow-up run with both, merged (and a note before them).
        assert_eq!(
            steers.carried("a note\n\nuse nextest\n\nand the docs"),
            vec![PendingId(1), PendingId(3)]
        );
        assert!(!steers.holds(PendingId(1)) && !steers.holds(PendingId(3)));
        // 3's answer after the run took it files nothing.
        assert!(!steers.admitted(PendingId(3), input()));
        assert!(steers.carried("use nextest").is_empty());
        // The forgotten one is never taken.
        assert!(steers.carried("never mind").is_empty());
    }

    /// Other clients' inputs are kept only while a send is unanswered.
    #[test]
    fn applied_inputs_are_kept_only_while_a_send_is_unanswered() {
        let mut steers = Steers::default();
        assert_eq!(steers.applied(&input()), None);
        assert!(steers.applied.is_empty());
        steers.sending(PendingId(1));
        assert!(steers.sending_any());
        assert_eq!(steers.applied(&input()), None);
        assert_eq!(steers.applied.len(), 1);
        steers.admitted(PendingId(1), input());
        assert!(steers.applied.is_empty());
        assert!(!steers.sending_any());
        assert!(steers.holds(PendingId(1)));
    }

    #[test]
    fn a_settled_input_ends_only_its_own_steer_once() {
        let mut steers = Steers::default();
        let (first, second) = (input(), input());
        steers.sending(PendingId(1));
        steers.sending(PendingId(2));
        steers.admitted(PendingId(1), first.clone());
        steers.admitted(PendingId(2), second);
        assert_eq!(steers.settled(&input()), None);
        assert_eq!(steers.settled(&first), Some(PendingId(1)));
        assert_eq!(steers.settled(&first), None);
        assert!(steers.holds(PendingId(2)));
        // Its boundary event after the answer marks nothing twice.
        assert_eq!(steers.applied(&first), None);
    }

    #[test]
    fn a_recall_takes_only_the_named_inputs() {
        let mut steers = Steers::default();
        let (first, second) = (input(), input());
        steers.sending(PendingId(1));
        steers.sending(PendingId(2));
        steers.admitted(PendingId(1), first.clone());
        steers.admitted(PendingId(2), second);
        assert_eq!(steers.recalled(&[first, input()]), vec![PendingId(1)]);
        assert!(steers.holds(PendingId(2)));
        assert!(!steers.holds(PendingId(1)));
    }
}
