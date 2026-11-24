//! A checkpoint's steps for an agent and for a helper
//! ([`crate::daemon::checkpoint`] says what a checkpoint is): the status
//! run, the review, and the verdict's line, audit line and continue.

use std::sync::Arc;

use meerkat_core::SessionId;
use meerkat_core::types::HandlingMode;
use meerkat_mob::{AgentIdentity, BoundedResultSpec, BoundedTurnFailure, MemberTurnOptions, MobId};
use tokio::sync::watch;

use super::super::MOB_ID;
use super::Agents;
use super::notes::steer_member;
use crate::approval::checkpoint::CheckpointVerdict;
use crate::config::registry::{CHECKPOINT_CONTINUES, int_in_force};
use crate::daemon::checkpoint::{
    Begun, CONTINUE_INSTRUCTION, CONTINUE_LABEL, Line, MAX_STATUS, Outcome, STATUS_LABEL,
    STATUS_WAIT, Subject, plain, reason_of_error, status_request,
};
use crate::mobs::endings::LimitReached;

/// Whose run a checkpoint reviews, with the session whose transcript it
/// reads.
#[derive(Clone, Copy)]
enum Reviewed<'a> {
    /// The agent's own run, on its session.
    Agent(&'a SessionId),
    /// A helper's run, on the session of the agent it works for (the
    /// user's words read as the agent's).
    Helper(&'a SessionId),
}

/// Why a status could not be read.
enum StatusError {
    /// The user cancelled the checkpoint (its run was interrupted).
    Cancelled,
    /// Anything else, in words.
    Failed(String),
}

/// Resolve when `cancelled` turns `true` (never when its sender is gone).
async fn until_cancelled(cancelled: &mut watch::Receiver<bool>) {
    if cancelled.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}

impl Agents {
    /// `name`'s run in `session` ended at its limit after `steps` model
    /// calls: its checkpoint starts, off the caller's task (one at a time).
    pub(crate) fn agent_at_limit(self: &Arc<Self>, name: &str, session: &SessionId, steps: u64) {
        let subject = Subject::Agent(name.to_string());
        let Some(begun) = self.checkpoints.begin(&subject) else {
            tracing::debug!(agent = %name, "a checkpoint of the agent is under way already");
            return;
        };
        let agents = Arc::clone(self);
        let (name, session) = (name.to_string(), session.clone());
        let id = begun.id;
        let task = tokio::spawn(async move {
            agents.agent_checkpoint(&name, &session, steps, begun).await;
            agents.checkpoints.end(&Subject::Agent(name), id);
        });
        self.checkpoints.running(&subject, id, task.abort_handle());
    }

    /// The user interrupted `name` or sent it a message: a checkpoint of
    /// it under way is cancelled (nothing continues); a message also
    /// starts its streak again.
    pub(crate) fn user_acted(&self, name: &str, message: bool) {
        let subject = Subject::Agent(name.to_string());
        if self.checkpoints.cancel(&subject) {
            tracing::info!(agent = %name, "the user cancelled the agent's checkpoint");
        }
        if message {
            self.checkpoints.reset(&subject);
        }
    }

    /// An agent's checkpoint, start to end.
    async fn agent_checkpoint(&self, name: &str, session: &SessionId, steps: u64, begun: Begun) {
        let Begun { id, mut cancelled } = begun;
        // Busy all along: no idle note goes meanwhile.
        let _busy = self.core.activity.sending(name);
        let subject = Subject::Agent(name.to_string());
        let request = status_request(steps);
        self.audit_run(
            name,
            None,
            "checkpoint",
            format!("after {steps} steps; the agent is asked for its status"),
        );
        self.tell_triggered(name, session, STATUS_LABEL.to_string(), request.clone());
        let (mob, member) = (MobId::from(MOB_ID), AgentIdentity::from(name));
        let status = tokio::select! {
            status = self.ask_status(&mob, &member, request) => status,
            () = until_cancelled(&mut cancelled) => Err(StatusError::Cancelled),
        };
        let mut outcome = match status {
            Err(StatusError::Cancelled) => Outcome::Cancelled,
            Err(StatusError::Failed(why)) => Outcome::Stopped(format!(
                "the agent's status could not be read: {}",
                plain(&why)
            )),
            Ok(status) => {
                let cap = int_in_force(&CHECKPOINT_CONTINUES, &self.core.settings.layers());
                let streak = self.checkpoints.streak(&subject);
                self.review(
                    Reviewed::Agent(session),
                    &status,
                    steps,
                    streak,
                    cap,
                    &mut cancelled,
                )
                .await
            }
        };
        if *cancelled.borrow() {
            outcome = Outcome::Cancelled;
        }
        if self.core.lifecycle.is_terminating() || !self.is_live(name) {
            return;
        }
        let line = Line {
            helper: None,
            steps,
        };
        if let Outcome::Continuing(_) = outcome {
            self.checkpoints.continued(&subject, None);
        }
        self.decided(name, session, &line, &outcome);
        // Over before the continue run starts: that run may reach its own
        // checkpoint at once.
        self.checkpoints.end(&subject, id);
        if let Outcome::Continuing(_) = outcome {
            self.tell_triggered(
                name,
                session,
                CONTINUE_LABEL.to_string(),
                CONTINUE_INSTRUCTION.to_string(),
            );
            let sent =
                super::notes::steer_agent(&self.core, name, CONTINUE_INSTRUCTION.to_string()).await;
            if let Err(err) = sent {
                tracing::warn!(agent = %name, error = %err, "the agent was not told to go on");
                self.notice(
                    name,
                    true,
                    format!("the agent was not told to go on after its checkpoint: {err}; say \"continue\" to go on"),
                );
            }
        }
    }

    /// The review of a status (`streak` automatic continues in a row so
    /// far, `cap` allowed): the cap first, then the classifier's model.
    async fn review(
        &self,
        reviewed: Reviewed<'_>,
        status: &str,
        steps: u64,
        streak: u64,
        cap: u64,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Outcome {
        if streak >= cap {
            return Outcome::Capped(cap);
        }
        let (session, helper) = match reviewed {
            Reviewed::Agent(session) => (session, false),
            Reviewed::Helper(session) => (session, true),
        };
        let verdict = tokio::select! {
            verdict = self.core.engine.review_checkpoint(session, helper, status, steps) => verdict,
            () = until_cancelled(cancelled) => return Outcome::Cancelled,
        };
        match verdict {
            Ok(CheckpointVerdict::Continue { reason }) => Outcome::Continuing(plain(&reason)),
            Ok(CheckpointVerdict::Stop { reason }) => Outcome::Stopped(plain(&reason)),
            Err(err) => Outcome::Stopped(reason_of_error(&err)),
        }
    }

    /// The verdict `outcome` of a checkpoint of `session` (an agent's, or
    /// the one its helper works for): counted, audited and told to the
    /// clients of the agent `name`.
    fn decided(&self, name: &str, session: &SessionId, line: &Line<'_>, outcome: &Outcome) {
        let text = line.text(outcome);
        match outcome {
            Outcome::Continuing(_) => self.core.engine.checkpoint_decided(session, true),
            Outcome::Stopped(_) | Outcome::Capped(_) => {
                self.core.engine.checkpoint_decided(session, false);
            }
            Outcome::Cancelled => {}
        }
        self.audit_run(name, None, "checkpoint_verdict", text.clone());
        self.notice(name, !matches!(outcome, Outcome::Continuing(_)), text);
    }

    /// Tell the clients of `name` `text`.
    fn notice(&self, name: &str, warn: bool, text: String) {
        self.core.decisions.broadcast(
            crate::daemon::protocol::AGENT_NOTICE,
            &crate::daemon::protocol::AgentNotice {
                agent: name.to_string(),
                warn,
                text,
            },
        );
    }

    /// Ask `member` of `mob` for its status in a run of its own with its
    /// tools off, and wait for its answer (at most [`STATUS_WAIT`]).
    async fn ask_status(
        &self,
        mob: &MobId,
        member: &AgentIdentity,
        request: String,
    ) -> Result<String, StatusError> {
        let failed = |err: &dyn std::fmt::Display| StatusError::Failed(err.to_string());
        let spec =
            BoundedResultSpec::new("checkpoint status", MAX_STATUS).map_err(|e| failed(&e))?;
        let handle = self
            .core
            .state
            .handle_for(mob)
            .await
            .map_err(|e| failed(&e))?;
        let member = handle.member(member).await.map_err(|e| failed(&e))?;
        let overlay = meerkat_core::TurnToolOverlay {
            tool_choice_plan: vec![meerkat_core::ToolChoice::None],
            ..meerkat_core::TurnToolOverlay::default()
        };
        let turn = member
            .start_turn_bounded(
                request,
                HandlingMode::Queue,
                MemberTurnOptions::new().with_turn_tool_overlay(overlay),
                None,
                spec.clone(),
            )
            .await
            .map_err(|e| failed(&e))?;
        let Ok(answered) = tokio::time::timeout(STATUS_WAIT, turn.wait_bounded(spec)).await else {
            return Err(StatusError::Failed("it did not answer in time".to_string()));
        };
        match answered {
            Ok(result) => {
                let (_, result) = result.into_parts();
                let text = result.result().text().trim().to_string();
                if text.is_empty() {
                    Err(StatusError::Failed("its answer was empty".to_string()))
                } else {
                    Ok(text)
                }
            }
            Err(err) => match err.failure() {
                BoundedTurnFailure::Cancelled { .. } => Err(StatusError::Cancelled),
                other => Err(failed(other)),
            },
        }
    }

    /// A helper's run reached its limit: its checkpoint starts, off the
    /// caller's task (one at a time per helper).
    pub(crate) fn helper_at_limit(self: &Arc<Self>, reached: LimitReached) {
        let subject = Subject::Helper(reached.key.clone());
        let Some(begun) = self.checkpoints.begin(&subject) else {
            tracing::debug!(helper = %reached.key, "a checkpoint of the helper is under way already");
            return;
        };
        let agents = Arc::clone(self);
        let id = begun.id;
        let task = tokio::spawn(async move {
            let key = reached.key.clone();
            agents.helper_checkpoint(reached, begun).await;
            agents.checkpoints.end(&Subject::Helper(key), id);
        });
        self.checkpoints.running(&subject, id, task.abort_handle());
    }

    /// A helper's checkpoint, start to end: the user's line goes to the
    /// clients of the agent it works for; a stop is told to its parent
    /// ([`crate::mobs::endings::HelperEndings::checkpoint_stopped`]).
    async fn helper_checkpoint(&self, reached: LimitReached, begun: Begun) {
        let Begun { id, mut cancelled } = begun;
        let LimitReached {
            key,
            tag,
            steps,
            run,
        } = reached;
        let subject = Subject::Helper(key.clone());
        let endings = self.core.registry.endings();
        let label = tag.label().to_string();
        let agent = self.agent_running_as(&tag.owner);
        if let Some(agent) = &agent {
            self.audit_run(
                agent,
                None,
                "checkpoint",
                format!("helper {label} after {steps} steps; it is asked for its status"),
            );
        }
        let status = tokio::select! {
            status = self.ask_status(&key.mob, &key.member, status_request(steps)) => status,
            () = until_cancelled(&mut cancelled) => Err(StatusError::Cancelled),
        };
        // Stopped by the user meanwhile: its parent was told so.
        if !endings.is_live(&key) || self.core.lifecycle.is_terminating() {
            return;
        }
        let (outcome, status) = match status {
            Err(StatusError::Cancelled) => (Outcome::Cancelled, String::new()),
            Err(StatusError::Failed(why)) => (
                Outcome::Stopped(format!("its status could not be read: {}", plain(&why))),
                String::new(),
            ),
            Ok(status) => {
                let cap = int_in_force(&CHECKPOINT_CONTINUES, &self.core.settings.layers());
                let streak = self.checkpoints.streak_at(&subject, run);
                let outcome = self
                    .review(
                        Reviewed::Helper(&tag.owner),
                        &status,
                        steps,
                        streak,
                        cap,
                        &mut cancelled,
                    )
                    .await;
                (outcome, status)
            }
        };
        if !endings.is_live(&key) || self.core.lifecycle.is_terminating() {
            return;
        }
        let line = Line {
            helper: Some(&label),
            steps,
        };
        match &outcome {
            Outcome::Continuing(_) => {
                endings.checkpoint_continues(&key);
                self.checkpoints.continued(&subject, Some(run));
            }
            Outcome::Stopped(_) | Outcome::Capped(_) => {
                endings.checkpoint_stopped(&key, steps, outcome.reason(), status);
                self.checkpoints.forget(&subject);
            }
            Outcome::Cancelled => {}
        }
        match &agent {
            Some(agent) => self.decided(agent, &tag.owner, &line, &outcome),
            None => {
                tracing::info!(helper = %key, verdict = %line.text(&outcome), "a helper's checkpoint; its agent is gone");
            }
        }
        self.checkpoints.end(&subject, id);
        if let Outcome::Continuing(_) = outcome
            && let Err(err) = steer_member(
                &self.core,
                &key.mob,
                key.member.clone(),
                CONTINUE_INSTRUCTION.to_string(),
            )
            .await
        {
            tracing::warn!(helper = %key, error = %err, "the helper was not told to go on");
            endings.checkpoint_stopped(
                &key,
                steps,
                format!("it could not be told to go on: {err}"),
                String::new(),
            );
        }
    }
}
