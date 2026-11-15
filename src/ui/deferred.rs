//! Commands typed while a turn runs that need the agent idle (`/model`,
//! `/new`, `/resume`, `/init`, `/agents <name>` and the agent picker's
//! switches, `/rehome <ref>`): instead of being refused they are held in
//! the pending queue ([`PendingState::Command`]), shown with "when the
//! turn ends", in one order with the messages typed after them, and run
//! once the agent is idle (the turn's end, which the daemon's idle note
//! decides). ↑ on an empty input takes one back like a held message; one
//! still held when the client quits or detaches is dropped with a notice
//! naming it. A picker command (`/model`, `/resume`, `/agents` with no
//! argument) opens its picker at once; the choice is what waits.

use std::time::Instant;

use super::{App, Command, Effect, NoticeLevel, PendingId, PendingState};

/// What a held command does once the agent is idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Deferred {
    /// A typed command, run as typed.
    Typed(Command),
    /// A picker's choice, as its effect.
    Choice(Effect),
}

impl App {
    /// Whether the agent's side is busy, which a held command waits out:
    /// a turn runs, a compaction is asked for (`/compact now`, unanswered)
    /// or its run goes ([`Msg::CompactionRun`](super::Msg::CompactionRun)),
    /// or a `!` command runs. A prompt asks less: it starts a turn while a
    /// compaction is only asked for (the runner admits it after the request;
    /// effects are served in order), and a `!` command does not wait for a
    /// compaction ([`Self::turn_or_shell`]).
    pub(super) fn agent_busy(&self) -> bool {
        self.running || self.compacting || self.compaction_run || self.shell_busy()
    }

    /// Whether a turn or a `!` command runs: what a `!` command, and a
    /// message that cannot be steered into the turn, waits for.
    pub(super) fn turn_or_shell(&self) -> bool {
        self.running || self.shell_busy()
    }

    /// Whether a command that needs the agent idle waits now instead of
    /// running: the agent is busy ([`Self::agent_busy`]), or another
    /// command waits already (they keep their order). Never while a held
    /// command itself runs.
    pub(super) fn defers(&self) -> bool {
        !self.draining && (self.agent_busy() || self.has_held_command())
    }

    /// Whether a command waits for the turn's end.
    pub(super) fn has_held_command(&self) -> bool {
        self.pending
            .items()
            .iter()
            .any(|p| p.state == PendingState::Command)
    }

    /// Hold `work`, shown as `text`, until the agent is idle when
    /// [`Self::defers`]; `false` (nothing held) otherwise.
    pub(super) fn defer(&mut self, text: impl Into<String>, work: Deferred) -> bool {
        if !self.defers() {
            return false;
        }
        let id = self
            .pending
            .add(&mut self.transcript, text.into(), PendingState::Command);
        self.deferred.insert(id, work);
        self.scroll.bottom();
        true
    }

    /// The agent is idle and nothing else runs: what was held goes, in
    /// the order it was typed. Each command runs (its result or failure
    /// shows as when typed); messages up to the next command go as one
    /// turn, and what follows them waits for that turn's end, as does
    /// everything after a command that starts a turn (`/init`).
    ///
    /// It waits out what [`Self::defers`] held them for
    /// ([`Self::agent_busy`]); each path that ends one of those calls it
    /// again (a turn's end, a `!` command's, a compaction's answer or run).
    pub(super) fn run_held(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        loop {
            if self.agent_busy() {
                return;
            }
            let mut held: Vec<(PendingId, PendingState)> = self
                .pending
                .items()
                .iter()
                .filter(|p| matches!(p.state, PendingState::Deferred | PendingState::Command))
                .map(|p| (p.id, p.state))
                .collect();
            held.sort_by_key(|(id, _)| *id);
            let Some(&(first, state)) = held.first() else {
                return;
            };
            if state == PendingState::Command {
                let Some(item) = self.pending.remove(first) else {
                    return;
                };
                self.drop_blocks([item.block]);
                let Some(work) = self.deferred.remove(&first) else {
                    continue;
                };
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    format!("{} (held until the turn ended)", item.text),
                );
                self.draining = true;
                match work {
                    Deferred::Typed(command) => self.run_command(command, now, fx),
                    Deferred::Choice(effect) => fx.push(effect),
                }
                self.draining = false;
                self.scroll.bottom();
                continue;
            }
            let upto = held
                .iter()
                .position(|(_, state)| *state == PendingState::Command)
                .unwrap_or(held.len());
            let ids: Vec<PendingId> = held[..upto].iter().map(|(id, _)| *id).collect();
            let mut messages = self.pending.take_where(|p| ids.contains(&p.id));
            messages.sort_by_key(|p| p.id);
            self.send_unsent(messages, now, fx);
            return;
        }
    }

    /// Leaving: the held commands are dropped (they would have run on
    /// this client); returns their texts, oldest first, and forgets them.
    pub(super) fn drop_held_commands(&mut self) -> Vec<String> {
        let mut dropped = self
            .pending
            .take_where(|p| p.state == PendingState::Command);
        dropped.sort_by_key(|p| p.id);
        self.deferred.clear();
        dropped.into_iter().map(|p| p.text).collect()
    }
}
