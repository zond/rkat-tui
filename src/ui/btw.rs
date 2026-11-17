//! `/btw <question>`: a side question beside the agent's turn. It is not
//! held for the turn's end and never sent to the agent: the runner asks
//! the daemon (`rkat/btw`), and the answer fills the question's
//! [`Block::Btw`]. One at a time; while one is pending the status bar says
//! so, and the first Esc cancels it (and only it: the next Esc is the
//! usual interrupt).

use std::time::Instant;

use super::{App, Block, Effect, NoticeLevel};

impl App {
    /// Whether a `/btw` waits for its answer.
    #[must_use]
    pub fn btw_pending(&self) -> bool {
        self.btw.is_some()
    }

    /// `/btw <question>`: shown as its block, asked at once (also while a
    /// turn runs), with the tool the agent runs now and for how long.
    pub(super) fn ask_btw(&mut self, question: String, now: Instant, fx: &mut Vec<Effect>) {
        if self.btw.is_some() {
            self.transcript.push_notice(
                NoticeLevel::Warn,
                "a /btw question is still waiting for its answer; ask the next one once it is answered (Esc cancels it)",
            );
            self.scroll.bottom();
            return;
        }
        if self.helper_view().is_some() {
            self.transcript.push_notice(
                NoticeLevel::Info,
                "/btw asks about the agent this client works with, not the helper you are viewing",
            );
        }
        let running_tool = self.status.open_tools.first().map(|tool| {
            (
                tool.name.clone(),
                now.saturating_duration_since(tool.since).as_secs(),
            )
        });
        // The running turn as the screen shows it: the session's stored
        // history has a run only once it ended.
        let current_turn = self
            .running
            .then(|| self.transcript.turn_text(self.run_first_block))
            .filter(|text| !text.trim().is_empty());
        let id = self.transcript.push(Block::Btw {
            question: question.clone(),
            answer: None,
        });
        self.btw = Some((id, question.clone()));
        self.status.btw_since = Some(now);
        self.scroll.bottom();
        fx.push(Effect::Btw {
            question,
            running_tool,
            current_turn,
        });
    }

    /// The pending `/btw`'s answer (or why there is none) fills its block;
    /// a block the transcript lost meanwhile (it was shown again) is added
    /// anew.
    pub(super) fn btw_answered(&mut self, answer: Result<String, String>) {
        let Some(pending) = self.btw.take() else {
            return;
        };
        self.status.btw_since = None;
        self.fill_btw(pending, answer);
    }

    /// Esc on a pending `/btw`: it is cancelled, and nothing else is.
    pub(super) fn cancel_btw(&mut self, fx: &mut Vec<Effect>) {
        let Some(pending) = self.btw.take() else {
            return;
        };
        self.status.btw_since = None;
        self.fill_btw(pending, Err("cancelled".to_string()));
        fx.push(Effect::CancelBtw);
    }

    fn fill_btw(
        &mut self,
        (id, question): (super::BlockId, String),
        answer: Result<String, String>,
    ) {
        let ours = matches!(
            self.transcript.block(id),
            Some(Block::Btw { question: shown, .. }) if *shown == question
        );
        let block = Block::Btw {
            question,
            answer: Some(answer),
        };
        if ours {
            self.transcript.replace_block(id, block);
        } else {
            self.transcript.push(block);
        }
        self.scroll.bottom();
    }
}
