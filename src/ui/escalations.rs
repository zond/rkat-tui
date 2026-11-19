//! Approval prompts (escalations) and plan mode's "Plan ready" dialog:
//! queueing, answering and stopping them.

use std::time::Instant;

use super::{
    App, Effect, EscalationAnswer, EscalationDialog, EscalationId, Modal, PROCEED_PROMPT,
    PlanChoice, PlanDialog,
};
use crate::approval::escalation::RecordedPrompt;
use crate::approval::mode::PermissionMode;

impl App {
    /// Escalations waiting behind the open modal.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_escalations(&self) -> usize {
        self.pending_escalations.len()
    }

    /// Tests: the user has been quiet for [`super::ARM_AFTER`] since the
    /// dialog the application opened by itself appeared: it takes keys.
    #[cfg(any(test, feature = "test-support"))]
    pub fn quiet_past_arming(&mut self) {
        self.arming = None;
    }

    /// The answer to the "Plan ready" dialog: proceed in accept-edits or
    /// manual mode with a fixed prompt, or keep planning.
    pub(super) fn plan_chosen(&mut self, choice: PlanChoice, now: Instant, fx: &mut Vec<Effect>) {
        let mode = match choice {
            PlanChoice::KeepPlanning => return,
            PlanChoice::Proceed => PermissionMode::AcceptEdits,
            PlanChoice::ProceedManual => PermissionMode::Manual,
        };
        fx.push(Effect::SetMode(mode));
        let prompt = PROCEED_PROMPT.to_string();
        if self.running {
            self.enqueue(prompt, fx);
        } else if self.shell_busy() {
            self.hold(prompt);
        } else {
            self.start_turn(prompt, now, fx);
        }
    }

    /// After a turn: in plan mode an answer opens the "Plan ready" dialog
    /// (unless another dialog is open or the next turn already started).
    /// It opens by itself, at `now`: armed only once the user has been
    /// quiet ([`super::Arming`]).
    pub(super) fn offer_plan(&mut self, answered: bool, now: Instant) {
        if answered
            && !self.running
            && self.status.mode == Some(PermissionMode::Plan)
            && matches!(self.modal, None | Some(Modal::Keys))
        {
            self.modal = Some(Modal::PlanReady(PlanDialog::default()));
            self.arming = Some(super::Arming::opened(now));
        }
    }

    /// An approval asked at `now`: shown at once (armed only once the
    /// user has been quiet, [`super::Arming`]), or queued behind the one
    /// shown.
    pub(super) fn escalation(&mut self, prompt: RecordedPrompt, id: EscalationId, now: Instant) {
        match &mut self.modal {
            Some(Modal::Escalation { queued_behind, .. }) => {
                self.pending_escalations.push_back((id, prompt));
                *queued_behind = self.pending_escalations.len();
            }
            _ => {
                // Escalations are time-limited and shown at once; an open
                // picker waits until they are answered (the shortcut panel
                // just closes).
                if let Some(picker) = self.modal.take()
                    && picker != Modal::Keys
                {
                    self.parked = Some(picker);
                }
                self.modal = Some(Modal::Escalation {
                    id,
                    prompt,
                    queued_behind: self.pending_escalations.len(),
                    dialog: EscalationDialog::default(),
                });
                self.arming = Some(super::Arming::opened(now));
            }
        }
    }

    /// The shown approval is answered or gone (at `now`): the next queued
    /// one, else the picker it displaced, comes up by itself, each armed
    /// on its own (a double tap must not answer the next).
    pub(super) fn advance_escalation(&mut self, now: Instant) {
        self.modal = match self.pending_escalations.pop_front() {
            Some((id, prompt)) => Some(Modal::Escalation {
                id,
                prompt,
                queued_behind: self.pending_escalations.len(),
                dialog: EscalationDialog::default(),
            }),
            None => self.parked.take(),
        };
        self.arming = self.modal.as_ref().map(|_| super::Arming::opened(now));
    }

    pub(super) fn escalation_resolved(&mut self, resolved: EscalationId, now: Instant) {
        match &mut self.modal {
            Some(Modal::Escalation { id, .. }) if *id == resolved => self.advance_escalation(now),
            Some(Modal::Escalation { queued_behind, .. }) => {
                self.pending_escalations.retain(|(id, _)| *id != resolved);
                *queued_behind = self.pending_escalations.len();
            }
            _ => self.pending_escalations.retain(|(id, _)| *id != resolved),
        }
    }

    /// Ctrl-C on the open dialog: Stop it and every waiting dialog of the same
    /// caller. The agent's own call stops its run (its dialogs go with it);
    /// a helper's stops that helper only (its other dialogs go with it), and
    /// the other callers' dialogs stay, the next one shown.
    pub(super) fn stop_escalations_of_shown(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        let Some(Modal::Escalation { id, prompt, .. }) = self.modal.take() else {
            return;
        };
        fx.push(Effect::RespondEscalation(id, EscalationAnswer::Stop));
        let caller = prompt.agent;
        self.pending_escalations.retain(|(id, prompt)| {
            if prompt.agent == caller {
                fx.push(Effect::RespondEscalation(*id, EscalationAnswer::Stop));
                false
            } else {
                true
            }
        });
        self.advance_escalation(now);
    }
}
