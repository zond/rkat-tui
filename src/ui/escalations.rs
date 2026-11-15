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
    pub(super) fn offer_plan(&mut self, answered: bool) {
        if answered
            && !self.running
            && self.status.mode == Some(PermissionMode::Plan)
            && matches!(self.modal, None | Some(Modal::Keys))
        {
            self.modal = Some(Modal::PlanReady(PlanDialog::default()));
        }
    }

    pub(super) fn escalation(&mut self, prompt: RecordedPrompt, id: EscalationId) {
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
            }
        }
    }

    pub(super) fn advance_escalation(&mut self) {
        self.modal = match self.pending_escalations.pop_front() {
            Some((id, prompt)) => Some(Modal::Escalation {
                id,
                prompt,
                queued_behind: self.pending_escalations.len(),
                dialog: EscalationDialog::default(),
            }),
            None => self.parked.take(),
        };
    }

    pub(super) fn escalation_resolved(&mut self, resolved: EscalationId) {
        match &mut self.modal {
            Some(Modal::Escalation { id, .. }) if *id == resolved => self.advance_escalation(),
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
    pub(super) fn stop_escalations_of_shown(&mut self, fx: &mut Vec<Effect>) {
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
        self.advance_escalation();
    }
}
