//! Runs no client started: a schedule's or a watch's (audited and told to
//! the clients), and the profile hint for a read-only mob with comms.

use std::sync::Arc;

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobId};

use super::super::MOB_ID;
use super::Agents;
use super::notes::note_agent;

/// What an agent is told when it makes a profile read-only with comms.
pub const READ_ONLY_COMMS_NOTE: &str = "A mob profile you just wrote has `read_only: true` and `comms: true`: its members can read messages, but they cannot send any (`read_only` blocks every mutating tool, `send_message` and the other comms sends included). Drop `read_only` (and narrow the tools another way) if they must answer.";

/// Whether `args` hold a profile's tool settings with both `read_only`
/// and `comms` true (anywhere: a definition's profiles, or one profile).
#[must_use]
pub fn read_only_with_comms(args: &serde_json::Value) -> bool {
    match args {
        serde_json::Value::Object(map) => {
            let set = |key: &str| map.get(key).and_then(serde_json::Value::as_bool) == Some(true);
            (set("read_only") && set("comms")) || map.values().any(read_only_with_comms)
        }
        serde_json::Value::Array(items) => items.iter().any(read_only_with_comms),
        _ => false,
    }
}

impl Agents {
    /// A run of `name` that no client's input started began with input
    /// `prompt`: when a schedule of the agent fired it (on the evidence
    /// [`crate::daemon::schedules::Schedules::attribute`] takes), the audit log
    /// records the trigger and the agent's clients are told
    /// ([`crate::daemon::protocol::TRIGGERED`]).
    pub async fn unprompted_run(
        &self,
        name: &str,
        session: &SessionId,
        run: &meerkat_core::RunId,
        prompt: &str,
    ) {
        // The watch event the daemon sent it: positive by construction.
        let from_watch = {
            let mut sent = self.watch_sent.lock();
            if sent.get(name).is_some_and(|text| text == prompt) {
                sent.remove(name);
                true
            } else {
                false
            }
        };
        if from_watch {
            self.audit_run(
                name,
                Some((session, run)),
                "run_triggered",
                format!("watch ({prompt})"),
            );
            return;
        }
        let Some(fired) = self.schedules.attribute(name, run, prompt).await else {
            return;
        };
        let label = format!("scheduled: {}", fired.name);
        tracing::info!(agent = %name, %run, schedule = %fired.schedule_id, "a schedule fired the agent's run");
        self.announce(
            name,
            session,
            run,
            label,
            prompt,
            &format!("schedule {} ({})", fired.name, fired.schedule_id),
        );
    }

    /// Record and announce a run `trigger` started (`label` as the clients
    /// show it, `detail` as the audit log names it).
    pub fn announce(
        &self,
        name: &str,
        session: &SessionId,
        run: &meerkat_core::RunId,
        label: String,
        prompt: &str,
        detail: &str,
    ) {
        self.audit_run(
            name,
            Some((session, run)),
            "run_triggered",
            detail.to_string(),
        );
        self.tell_triggered(name, session, label, prompt.to_string());
    }

    /// The audit log's `event` line for `name` (of its `session`'s `run`,
    /// when there is one).
    fn audit_run(
        &self,
        name: &str,
        run: Option<(&SessionId, &meerkat_core::RunId)>,
        event: &str,
        detail: String,
    ) {
        if let Some(audit) = &self.audit {
            let mut line = crate::daemon::audit::Line::event(event, detail).of_agent(name);
            if let Some((session, run)) = run {
                line.session = Some(session.to_string());
                line.run = Some(run.to_string());
            }
            audit.write(&line);
        }
    }

    /// Tell the clients a run of `name` that no client started began
    /// ([`crate::daemon::protocol::TRIGGERED`]).
    fn tell_triggered(&self, name: &str, session: &SessionId, label: String, prompt: String) {
        self.decisions.broadcast(
            crate::daemon::protocol::TRIGGERED,
            &crate::daemon::protocol::TriggeredNote {
                agent: name.to_string(),
                session_id: session.clone(),
                label,
                prompt,
            },
        );
    }

    /// `name` called `tool` with `args`: a profile made read-only with
    /// comms on (`mob_create`, `mob_profile_create`, `mob_profile_update`)
    /// gets the agent a note that its members cannot send messages (the
    /// engine's `read_only` blocks every mutating tool, `send_message`
    /// included). The note reaches it at its next turn boundary.
    pub fn tool_requested(&self, name: &str, tool: &str, args: &serde_json::Value) {
        if !matches!(
            tool,
            "mob_create" | "mob_profile_create" | "mob_profile_update"
        ) || !read_only_with_comms(args)
        {
            return;
        }
        tokio::spawn(note_agent(
            Arc::clone(&self.state),
            name.to_string(),
            READ_ONLY_COMMS_NOTE.to_string(),
        ));
    }

    /// Deliver a watch's `batch` to its agent: a message into its
    /// session (into a running turn at its next step, else it starts one,
    /// which is then a turn no user started), announced to its clients
    /// and written to the audit log.
    pub async fn watch_event(&self, batch: crate::daemon::watches::Batch) {
        let name = batch.agent.clone();
        let Some(session) = self.conversations.current(&name) else {
            return;
        };
        if !self.table.lock().contains_key(&name) {
            return;
        }
        let text = batch.text();
        self.watch_sent.lock().insert(name.clone(), text.clone());
        let _sending = self.activity.sending(&name);
        let sent = self
            .state
            .mob_member_send(
                &MobId::from(MOB_ID),
                AgentIdentity::from(name.as_str()),
                meerkat_core::types::ContentInput::from(text.clone()),
                meerkat_core::types::HandlingMode::Steer,
                None,
            )
            .await;
        if let Err(err) = sent {
            tracing::warn!(agent = %name, error = %err, "a watch event did not reach the agent");
            self.watch_sent.lock().remove(&name);
            return;
        }
        self.audit_run(&name, None, "watch_event", text.clone());
        self.tell_triggered(&name, &session, "files changed".to_string(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_profile_with_comms_is_found_wherever_it_is() {
        let definition = serde_json::json!({"definition": {"profiles": {
            "worker": {"tools": {"read_only": true, "comms": true, "shell": false}},
            "lead": {"tools": {"comms": true}}
        }}});
        assert!(read_only_with_comms(&definition));
        let profile = serde_json::json!({"name": "w", "profile": {"tools": {"read_only": true, "comms": false}}});
        assert!(!read_only_with_comms(&profile));
        assert!(!read_only_with_comms(
            &serde_json::json!({"tools": {"comms": true}})
        ));
    }
}
