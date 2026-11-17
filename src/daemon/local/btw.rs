//! `rkat/btw` and `rkat/btw_cancel`: the user's side question
//! ([`crate::daemon::btw`]), answered beside the agent's run.

use super::*;

use crate::daemon::btw;
use protocol::{BTW_REFUSED, BtwAnswer, BtwParams};

impl Local {
    /// `rkat/btw` for `caller`: one model call on the agent's current
    /// model with no tools, over the session's stored messages; nothing is
    /// written to the session. Cancelled by [`Self::btw_cancel`] or when
    /// the client leaves.
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] without an agent's conversation;
    /// [`BTW_REFUSED`] for everything else, in plain words.
    pub(super) async fn btw(
        &self,
        caller: &Caller,
        params: BtwParams,
    ) -> Result<BtwAnswer, Refusal> {
        let refuse = |text: String| Refusal::new(BTW_REFUSED, text);
        let session = self.caller_session(caller, "/btw asks about")?;
        let identity = self
            .core
            .conversations
            .llm_identity(&session)
            .await
            .ok_or_else(|| {
                refuse("the agent's model could not be read; ask again in a moment".to_string())
            })?;
        let client = (self.parts.side_clients)(identity.clone())
            .await
            .map_err(|why| {
                refuse(format!(
                    "no model client for the agent's model {}: {why}",
                    identity.model
                ))
            })?;
        let messages = self
            .core
            .conversations
            .messages(&session)
            .await
            .map_err(|err| refuse(format!("the conversation could not be read: {err}")))?;
        let record = btw::record(&messages, btw::BUDGET);
        let running = params
            .running_tool
            .map(|(name, secs)| btw::Running { name, secs });
        let prompt = btw::prompt(
            &record,
            params.current_turn.as_deref(),
            running.as_ref(),
            &params.question,
        );
        let model = identity.model.clone();
        let task = tokio::spawn(async move {
            tokio::time::timeout(btw::TIMEOUT, btw::ask(&*client, &model, prompt)).await
        });
        self.btws.lock().insert(params.id, task.abort_handle());
        let joined = task.await;
        self.btws.lock().remove(&params.id);
        let (answer, usage) = match joined {
            Err(_) => return Err(refuse("cancelled".to_string())),
            Ok(Err(_)) => {
                return Err(refuse(format!(
                    "no answer within {}s; ask again, or wait for the agent",
                    btw::TIMEOUT.as_secs()
                )));
            }
            Ok(Ok(Err(why))) => return Err(refuse(why)),
            Ok(Ok(Ok(answered))) => answered,
        };
        if let Some(usage) = &usage {
            self.core
                .ledger
                .side_question(&session, &identity.model, usage)
                .await;
        }
        if answer.trim().is_empty() {
            return Err(refuse("the model gave no answer".to_string()));
        }
        Ok(BtwAnswer { answer })
    }

    /// `rkat/btw_cancel`: the side question `id` stops (its model call is
    /// dropped); one that ended already is no error.
    pub(super) fn btw_cancel(&self, id: u64) -> Empty {
        if let Some(task) = self.btws.lock().remove(&id) {
            task.abort();
        }
        Empty {}
    }
}
