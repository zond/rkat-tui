//! Messages typed while a turn is running: the contract the UI and the
//! runners share, and the UI's queue of them.
//!
//! Such a message is *steered* into the running turn: the runner admits it
//! to the session's runtime as a durable `Steer` input. The runtime writes
//! it into the conversation as an ordinary user message at the running
//! turn's next model boundary, or runs it as one follow-up turn when no
//! boundary opens in time; either way it is written exactly once
//! (`docs/reference/session-contracts.mdx`, "Durable steer"). [`PendingId`]
//! and [`SteerFailure`] are what the runners depend on.
//!
//! On the UI side each such message becomes a [`Pending`] item with a
//! [`Block::Queued`] placeholder in the transcript. The item moves through
//! [`PendingState`] as the runner admits it to the session's runtime
//! ([`super::Msg::SteerAck`]) and the runtime writes it into the
//! conversation ([`super::Msg::SteerDelivered`], where the placeholder
//! becomes a user block in place). [`PendingQueue`]'s transitions keep the
//! item and its placeholder in step; [`super::App`] decides when each
//! happens (it also owns the editor and the effects).
//!
//! Messages the UI still holds ([`PendingState::Deferred`]) are recalled into
//! the editor by the UI alone. Admitted messages come back through the runner
//! ([`super::Effect::RecallQueued`], answered by [`super::Msg::Recalled`]):
//! it abandons the ones the runtime still holds queued, leaving the running
//! turn alone; one that already joined the run stays, and only an interrupt
//! ends it.
//!
//! [`Block::Queued`]: super::transcript::Block::Queued

use std::fmt;

use super::commands;
use super::transcript::{Block, BlockId, QueuedLabel, Transcript};

/// Identifier of one queued message, allocated by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PendingId(pub u64);

impl fmt::Display for PendingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pending#{}", self.0)
    }
}

/// Why a steered message was not admitted to the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerFailure {
    /// No turn is running; the message should be sent as a normal turn.
    NotRunning,
    /// The runtime refused the input; the text never reached the session.
    Rejected(String),
}

impl fmt::Display for SteerFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => f.write_str("no turn is running"),
            Self::Rejected(reason) => write!(f, "the runtime refused it: {reason}"),
        }
    }
}

/// Where a pending message is in its delivery lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// Handed to the runner ([`super::Effect::Steer`]); no ack yet.
    Queued,
    /// Admitted to the runtime as a durable steer: written into the running
    /// turn at its next model boundary, or run as a follow-up turn.
    Admitted,
    /// Held by the UI (compaction is running, or no turn was running to
    /// steer into); sent as a new turn when the current work ends.
    Deferred,
    /// A `!` command (its text is the input as typed), held by the UI
    /// until the turn ends; it runs before the messages held with it.
    Shell,
}

impl PendingState {
    /// The label shown on the transcript placeholder.
    #[must_use]
    pub fn label(self) -> QueuedLabel {
        match self {
            Self::Queued => QueuedLabel::Queued,
            Self::Admitted => QueuedLabel::Delivering,
            Self::Deferred => QueuedLabel::Deferred,
            Self::Shell => QueuedLabel::Shell,
        }
    }

    /// Whether the UI alone can pull the message back into the editor (↑ on an
    /// empty input): only messages the runtime does not hold yet (admitted ones need a
    /// recall through the runner).
    #[must_use]
    pub fn recallable(self) -> bool {
        matches!(self, Self::Deferred | Self::Shell)
    }
}

/// One message typed while a turn was running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Queue id (also the injection id).
    pub id: PendingId,
    /// Raw user text.
    pub text: String,
    /// Lifecycle state.
    pub state: PendingState,
    /// The [`Block::Queued`] placeholder.
    pub block: BlockId,
}

/// Pending messages, oldest first.
#[derive(Debug, Clone, Default)]
pub struct PendingQueue {
    items: Vec<Pending>,
    /// Items already shown as delivered ([`super::Msg::SteerDelivered`])
    /// whose text is kept until the turn ends: a discarded application
    /// ([`super::Msg::SteerRequeued`]) or an undelivered terminal brings
    /// them back.
    settled: Vec<Pending>,
    next_id: u64,
}

impl PendingQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next id.
    pub fn alloc_id(&mut self) -> PendingId {
        self.next_id += 1;
        PendingId(self.next_id)
    }

    /// The most recently allocated id (`PendingId(0)` before the first).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn last_id(&self) -> PendingId {
        PendingId(self.next_id)
    }

    /// Every item the queue holds, queued first then settled.
    #[cfg(any(test, feature = "test-support"))]
    pub fn all(&self) -> impl Iterator<Item = &Pending> {
        self.items.iter().chain(self.settled.iter())
    }

    /// Append an item.
    #[cfg(any(test, feature = "test-support"))]
    pub fn push(&mut self, item: Pending) {
        self.items.push(item);
    }

    /// All items, oldest first.
    #[must_use]
    pub fn items(&self) -> &[Pending] {
        &self.items
    }

    /// Number of items not yet delivered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Item by id.
    #[must_use]
    pub fn get(&self, id: PendingId) -> Option<&Pending> {
        self.items.iter().find(|p| p.id == id)
    }

    /// Mutable item by id.
    pub fn get_mut(&mut self, id: PendingId) -> Option<&mut Pending> {
        self.items.iter_mut().find(|p| p.id == id)
    }

    /// Remove and return an item.
    pub fn remove(&mut self, id: PendingId) -> Option<Pending> {
        let idx = self.items.iter().position(|p| p.id == id)?;
        Some(self.items.remove(idx))
    }

    /// Remove every item matching `pred`, oldest first.
    pub fn take_where(&mut self, mut pred: impl FnMut(&Pending) -> bool) -> Vec<Pending> {
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(self.items.len());
        for item in self.items.drain(..) {
            if pred(&item) {
                taken.push(item);
            } else {
                kept.push(item);
            }
        }
        self.items = kept;
        taken
    }

    /// Number of items in `state`-matching states.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn count(&self, pred: impl Fn(PendingState) -> bool) -> usize {
        self.items.iter().filter(|p| pred(p.state)).count()
    }

    /// The newest item that can still be recalled.
    #[must_use]
    pub fn newest_recallable(&self) -> Option<&Pending> {
        self.items.iter().rev().find(|p| p.state.recallable())
    }

    /// Move an item to the settled list (shown as delivered until the turn
    /// ends). Returns the settled item.
    pub fn settle(&mut self, id: PendingId) -> Option<&Pending> {
        let item = self.remove(id)?;
        self.settled.push(item);
        self.settled.last()
    }

    /// Remove and return a settled item.
    pub fn take_settled(&mut self, id: PendingId) -> Option<Pending> {
        let idx = self.settled.iter().position(|p| p.id == id)?;
        Some(self.settled.remove(idx))
    }

    /// Forget every settled item (the turn ended).
    pub fn clear_settled(&mut self) {
        self.settled.clear();
    }

    /// Fix up block ids after the transcript removed block `removed`.
    pub fn block_removed(&mut self, removed: BlockId) {
        for item in self.items.iter_mut().chain(self.settled.iter_mut()) {
            if item.block.0 > removed.0 {
                item.block = BlockId(item.block.0 - 1);
            }
        }
    }

    /// Hold `text` (a message, or a `!` command as typed) in `state` with
    /// its placeholder appended to the transcript.
    pub fn add(
        &mut self,
        transcript: &mut Transcript,
        text: String,
        state: PendingState,
    ) -> PendingId {
        let id = self.alloc_id();
        let mut item = Pending {
            id,
            text,
            state,
            block: BlockId(0),
        };
        item.block = transcript.push(placeholder(&item));
        self.items.push(item);
        id
    }

    /// Move item `id` to `state`, relabelling its placeholder.
    pub fn set_state(&mut self, transcript: &mut Transcript, id: PendingId, state: PendingState) {
        let Some(item) = self.get_mut(id) else {
            return;
        };
        item.state = state;
        if let Some(Block::Queued { label, .. }) = transcript.block_mut(item.block) {
            *label = state.label();
        }
    }

    /// Show item `id` as delivered: its placeholder becomes a user block in
    /// place. The text is kept until the turn ends, in case the runtime
    /// discards the application again.
    pub fn show_delivered(&mut self, transcript: &mut Transcript, id: PendingId) {
        if let Some(item) = self.settle(id) {
            let (block, text) = (item.block, item.text.clone());
            transcript.replace_block(block, Block::User { text });
        }
    }

    /// The runtime discarded the application of delivered item `id`: show
    /// it as admitted and waiting again.
    pub fn show_waiting_again(&mut self, transcript: &mut Transcript, id: PendingId) {
        if let Some(item) = self.take_settled(id) {
            self.put_back(transcript, item, PendingState::Admitted);
        }
    }

    /// Hold a taken item again (the turn ended without it): deferred, its
    /// placeholder back in place.
    pub fn hold_again(&mut self, transcript: &mut Transcript, item: Pending) {
        self.put_back(transcript, item, PendingState::Deferred);
    }

    /// Send item `id` as a turn of its own now: it leaves the queue and
    /// its placeholder becomes a user block. Returns its text.
    pub fn send_now(&mut self, transcript: &mut Transcript, id: PendingId) -> Option<String> {
        let item = self.remove(id)?;
        transcript.replace_block(
            item.block,
            Block::User {
                text: item.text.clone(),
            },
        );
        Some(item.text)
    }

    /// The transcript was replaced: append a fresh placeholder for every
    /// item still in flight and forget the settled ones.
    pub fn restore_placeholders(&mut self, transcript: &mut Transcript) {
        self.settled.clear();
        for item in &mut self.items {
            item.block = transcript.push(placeholder(item));
        }
    }

    /// After a turn: every item the turn did not deliver, oldest first.
    /// Undelivered steers (queued or already shown as delivered), items the
    /// UI held, and (defensively) admitted items the runtime never reported
    /// on. Items still waiting for their admission ack stay: the ack
    /// decides. The settled items are forgotten.
    pub fn take_unsent(&mut self, undelivered: &[PendingId]) -> Vec<Pending> {
        let mut unsent: Vec<Pending> = undelivered
            .iter()
            .filter_map(|id| self.take_settled(*id))
            .collect();
        unsent.extend(self.take_where(|p| {
            undelivered.contains(&p.id)
                || matches!(p.state, PendingState::Deferred | PendingState::Admitted)
        }));
        self.clear_settled();
        unsent.sort_by_key(|p| p.id);
        unsent
    }

    /// Put `item` back in `state` with its placeholder relabelled.
    fn put_back(&mut self, transcript: &mut Transcript, item: Pending, state: PendingState) {
        let item = Pending { state, ..item };
        transcript.replace_block(item.block, placeholder(&item));
        self.items.push(item);
    }
}

/// The transcript placeholder of a held item: the message, or `$ <cmd>`
/// for a `!` command.
#[must_use]
pub fn placeholder(item: &Pending) -> Block {
    let text = match item.state {
        PendingState::Shell => format!(
            "$ {}",
            commands::parse_shell(&item.text).unwrap_or(&item.text)
        ),
        _ => item.text.clone(),
    };
    Block::Queued {
        id: item.id,
        text,
        label: item.state.label(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A message's life on the queue keeps its placeholder in step: queued,
    /// admitted, shown as delivered (a user block), discarded back to
    /// waiting, then handed back as unsent when the turn ends.
    #[test]
    fn transitions_keep_the_placeholder_in_step() {
        let mut transcript = Transcript::new();
        let mut q = PendingQueue::new();
        let id = q.add(&mut transcript, "hi".into(), PendingState::Queued);
        let block = q.get(id).unwrap().block;
        let label = |t: &Transcript| match t.block(block) {
            Some(Block::Queued { label, .. }) => Some(*label),
            _ => None,
        };
        assert_eq!(label(&transcript), Some(QueuedLabel::Queued));
        q.set_state(&mut transcript, id, PendingState::Admitted);
        assert_eq!(label(&transcript), Some(QueuedLabel::Delivering));
        q.show_delivered(&mut transcript, id);
        assert!(q.is_empty());
        assert_eq!(
            transcript.block(block),
            Some(&Block::User { text: "hi".into() })
        );
        q.show_waiting_again(&mut transcript, id);
        assert_eq!(label(&transcript), Some(QueuedLabel::Delivering));
        let unsent = q.take_unsent(&[]);
        assert_eq!(unsent.iter().map(|p| p.id).collect::<Vec<_>>(), [id]);
        q.hold_again(&mut transcript, unsent.into_iter().next().unwrap());
        assert_eq!(label(&transcript), Some(QueuedLabel::Deferred));
        assert_eq!(q.send_now(&mut transcript, id).as_deref(), Some("hi"));
        assert_eq!(
            transcript.block(block),
            Some(&Block::User { text: "hi".into() })
        );
        let shell = q.add(&mut transcript, "!ls -a".into(), PendingState::Shell);
        assert!(matches!(
            transcript.block(q.get(shell).unwrap().block),
            Some(Block::Queued { text, .. }) if text == "$ ls -a"
        ));
    }

    #[test]
    fn failures_display() {
        assert_eq!(SteerFailure::NotRunning.to_string(), "no turn is running");
        assert_eq!(
            SteerFailure::Rejected("closed".into()).to_string(),
            "the runtime refused it: closed"
        );
        assert_eq!(PendingId(3).to_string(), "pending#3");
    }

    fn item(q: &mut PendingQueue, text: &str, block: usize) -> PendingId {
        let id = q.alloc_id();
        q.push(Pending {
            id,
            text: text.into(),
            state: PendingState::Queued,
            block: BlockId(block),
        });
        id
    }

    #[test]
    fn ids_are_unique_and_lookup_works() {
        let mut q = PendingQueue::new();
        let a = item(&mut q, "a", 1);
        let b = item(&mut q, "b", 3);
        assert_ne!(a, b);
        assert_eq!(q.len(), 2);
        assert_eq!(q.get(b).map(|p| p.text.as_str()), Some("b"));
        q.get_mut(a).unwrap().state = PendingState::Deferred;
        assert_eq!(q.count(|s| s == PendingState::Deferred), 1);
        assert_eq!(q.remove(a).map(|p| p.text), Some("a".into()));
        assert!(q.get(a).is_none());
        // Ids keep increasing after removal.
        assert!(item(&mut q, "c", 4) > b);
    }

    #[test]
    fn only_deferred_items_are_recallable() {
        let mut q = PendingQueue::new();
        let a = item(&mut q, "a", 0);
        let b = item(&mut q, "b", 1);
        assert!(q.newest_recallable().is_none());
        q.get_mut(a).unwrap().state = PendingState::Deferred;
        assert_eq!(q.newest_recallable().map(|p| p.id), Some(a));
        q.get_mut(b).unwrap().state = PendingState::Deferred;
        assert_eq!(q.newest_recallable().map(|p| p.id), Some(b));
        q.get_mut(b).unwrap().state = PendingState::Admitted;
        assert_eq!(q.newest_recallable().map(|p| p.id), Some(a));
    }

    #[test]
    fn take_where_keeps_order_and_block_fixup_shifts() {
        let mut q = PendingQueue::new();
        let a = item(&mut q, "a", 2);
        let b = item(&mut q, "b", 5);
        let c = item(&mut q, "c", 7);
        let taken = q.take_where(|p| p.id != b);
        assert_eq!(taken.iter().map(|p| p.id).collect::<Vec<_>>(), vec![a, c]);
        assert_eq!(q.len(), 1);
        q.block_removed(BlockId(3));
        assert_eq!(q.get(b).unwrap().block, BlockId(4));
        q.block_removed(BlockId(9));
        assert_eq!(q.get(b).unwrap().block, BlockId(4));
        // Settled items leave the queue but keep their text and block fix-ups.
        assert_eq!(q.settle(b).map(|p| p.text.as_str()), Some("b"));
        assert!(q.is_empty());
        q.block_removed(BlockId(0));
        let settled = q.take_settled(b).unwrap();
        assert_eq!(settled.block, BlockId(3));
        assert!(q.take_settled(b).is_none());
    }

    #[test]
    fn labels_follow_states() {
        assert_eq!(PendingState::Queued.label(), QueuedLabel::Queued);
        assert_eq!(PendingState::Admitted.label(), QueuedLabel::Delivering);
        assert_eq!(PendingState::Deferred.label(), QueuedLabel::Deferred);
        assert!(PendingState::Deferred.recallable());
        assert!(!PendingState::Admitted.recallable());
        assert!(!PendingState::Queued.recallable());
    }
}
