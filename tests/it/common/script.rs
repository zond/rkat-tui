//! A scripted model written as rules: each request is answered by the
//! first rule its messages match, in the order the rules were written,
//! else by the script's fallback. It builds the same [`Step::Dynamic`] a
//! hand-written closure would, from the same readings of a request
//! ([`super::after`], [`super::run_inputs`], [`super::is_helper`]).
//!
//! ```ignore
//! // Call `shell`, then say "done" once its result comes back.
//! let llm = Script::new()
//!     .call("s1", "shell", json!({"command": "ls"}))
//!     .text("done")
//!     .client(12);
//! // A helper and its parent in one script.
//! let step = Script::new()
//!     .helper("HELPER-TASK", Step::Text("HELPER-ANSWER"))
//!     .told_finished(Step::Text("NOTED"))
//!     .after("a1", Step::Text("waiting"))
//!     .otherwise(assign("scout", "HELPER-TASK: look"));
//! ```

use std::sync::Arc;

use meerkat_core::Message;
use serde_json::Value;
use tokio::sync::Notify;

use super::{ScriptedLlmClient, Step};

/// Whether a rule applies to a request's messages.
type Test = Arc<dyn Fn(&[Message]) -> bool + Send + Sync>;

/// A step made from a request's messages.
type Make = Arc<dyn Fn(&[Message]) -> Step + Send + Sync>;

/// What a rule answers with.
#[derive(Clone)]
pub enum Reply {
    Step(Step),
    /// What this script answers (a helper's own script, say).
    Script(Arc<Script>),
    /// The step this function makes of the request's messages.
    Make(Make),
}

impl From<Step> for Reply {
    fn from(step: Step) -> Self {
        Self::Step(step)
    }
}

impl From<Script> for Reply {
    fn from(script: Script) -> Self {
        Self::Script(Arc::new(script))
    }
}

impl Reply {
    fn resolve(&self, messages: &[Message]) -> Step {
        match self {
            Self::Step(Step::Dynamic(make)) => make(messages),
            Self::Step(step) => step.clone(),
            Self::Script(script) => script.resolve(messages),
            Self::Make(make) => make(messages),
        }
    }
}

/// A function of a request's messages as a [`Reply`].
pub fn make(f: impl Fn(&[Message]) -> Step + Send + Sync + 'static) -> Reply {
    Reply::Make(Arc::new(f))
}

/// `step`, once `gate` is notified.
pub fn hold(gate: &Arc<Notify>, step: Step) -> Step {
    Step::Gated(Arc::clone(gate), Box::new(step))
}

/// A tool call `id` of `name` with `args`.
pub fn call(id: &'static str, name: &'static str, args: Value) -> Step {
    Step::Tool { id, name, args }
}

/// Rules from a request's messages to a step, first match first, and the
/// fallback when none matches.
#[derive(Clone, Default)]
pub struct Script {
    rules: Vec<(Test, Reply)>,
    otherwise: Option<Reply>,
    /// The ids of the last call [`Self::then`] added: the next step of
    /// the sequence answers its result.
    last_call: Option<Vec<&'static str>>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `reply` to a request `test` holds for.
    pub fn when(
        mut self,
        test: impl Fn(&[Message]) -> bool + Send + Sync + 'static,
        reply: impl Into<Reply>,
    ) -> Self {
        self.rules.push((Arc::new(test), reply.into()));
        self
    }

    /// Answer `reply` to a helper whose task carries `marker`
    /// ([`super::is_helper`]).
    pub fn helper(self, marker: &'static str, reply: impl Into<Reply>) -> Self {
        self.when(move |m| super::is_helper(m, marker), reply)
    }

    /// Answer `reply` to a request any of whose user texts carries `text`
    /// (the whole conversation's, [`super::user_texts`]).
    pub fn mentions(self, text: &'static str, reply: impl Into<Reply>) -> Self {
        self.when(move |m| super::user_texts(m).concat().contains(text), reply)
    }

    /// Answer `reply` to a request whose inputs carry `text`
    /// ([`super::run_inputs`]).
    pub fn input(self, text: &'static str, reply: impl Into<Reply>) -> Self {
        self.when(move |m| super::run_inputs(m).contains(text), reply)
    }

    /// Answer `reply` to a request that answers the call `id`
    /// ([`super::after`]).
    pub fn after(self, id: &'static str, reply: impl Into<Reply>) -> Self {
        self.when(move |m| super::after(m, id), reply)
    }

    /// Answer `reply` to a request whose last message is a tool result
    /// (of any call).
    pub fn after_any(self, reply: impl Into<Reply>) -> Self {
        self.when(
            |m| matches!(m.last(), Some(Message::ToolResults { .. })),
            reply,
        )
    }

    /// Answer `reply` to a request that answers the note that a helper
    /// finished ([`super::told_finished`]).
    pub fn told_finished(self, reply: impl Into<Reply>) -> Self {
        self.when(super::told_finished, reply)
    }

    /// Answer `reply` when no rule matches.
    pub fn otherwise(mut self, reply: impl Into<Reply>) -> Self {
        self.otherwise = Some(reply.into());
        self
    }

    /// The next step of a sequence: the first is the fallback (what a
    /// request that answers no call of the sequence gets), each later one
    /// answers the result of the call before it.
    ///
    /// # Panics
    ///
    /// A step after a step that is no tool call (nothing would trigger it).
    pub fn then(mut self, step: Step) -> Self {
        let ids = call_ids(&step);
        match self.last_call.take() {
            None if self.otherwise.is_none() => self.otherwise = Some(step.into()),
            None => panic!("a step of a sequence follows a tool call"),
            Some(previous) => {
                self.rules.push((
                    Arc::new(move |m| previous.iter().any(|id| super::after(m, id))),
                    step.into(),
                ));
            }
        }
        self.last_call = (!ids.is_empty()).then_some(ids);
        self
    }

    /// [`Self::then`] a tool call `id` of `name` with `args`.
    pub fn call(self, id: &'static str, name: &'static str, args: Value) -> Self {
        self.then(call(id, name, args))
    }

    /// [`Self::then`] a reply with `text`.
    pub fn text(self, text: &'static str) -> Self {
        self.then(Step::Text(text))
    }

    /// The step for a request of `messages`.
    pub fn resolve(&self, messages: &[Message]) -> Step {
        self.rules
            .iter()
            .find(|(test, _)| test(messages))
            .map(|(_, reply)| reply)
            .or(self.otherwise.as_ref())
            .map_or(Step::Text("script exhausted"), |reply| {
                reply.resolve(messages)
            })
    }

    /// The script as one step the scripted client resolves per request.
    pub fn step(self) -> Step {
        let script = Arc::new(self);
        Step::Dynamic(Arc::new(move |messages| script.resolve(messages)))
    }

    /// A scripted client answering `requests` requests with this script.
    pub fn client(self, requests: usize) -> Arc<ScriptedLlmClient> {
        ScriptedLlmClient::new(vec![self.step(); requests])
    }
}

/// The ids of the tool calls `step` makes (a held step's inner one's).
fn call_ids(step: &Step) -> Vec<&'static str> {
    match step {
        Step::Tool { id, .. } | Step::ToolUsage { id, .. } => vec![id],
        Step::Tools(calls) => calls.iter().map(|(id, _, _)| *id).collect(),
        Step::Gated(_, inner) | Step::Streaming(_, _, inner) => call_ids(inner),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use meerkat_core::types::{ToolResult, UserMessage};
    use serde_json::json;

    use super::*;

    fn user(text: &str) -> Message {
        Message::User(UserMessage::text(text))
    }

    fn result(id: &str) -> Message {
        Message::tool_results(vec![ToolResult::new(id.into(), "ok".into(), false)])
    }

    fn text(step: Step) -> &'static str {
        match step {
            Step::Text(text) => text,
            Step::Gated(_, inner) => text(*inner),
            _ => panic!("not a text step"),
        }
    }

    fn tool(step: &Step) -> &'static str {
        match step {
            Step::Tool { id, .. } => id,
            _ => panic!("not a tool step"),
        }
    }

    #[test]
    fn a_sequence_answers_each_call_with_the_next_step() {
        let script = Script::new()
            .call("s1", "shell", json!({"command": "ls"}))
            .call("s2", "shell", json!({"command": "pwd"}))
            .text("done");
        assert_eq!(tool(&script.resolve(&[user("go")])), "s1");
        assert_eq!(tool(&script.resolve(&[user("go"), result("s1")])), "s2");
        // (A request answers the results after its last assistant message:
        // here, the one result.)
        assert_eq!(text(script.resolve(&[user("go"), result("s2")])), "done");
        // A later input answers no call: the sequence starts again.
        assert_eq!(tool(&script.resolve(&[user("again")])), "s1");
    }

    #[test]
    fn rules_match_in_the_order_written_before_the_fallback() {
        let script = Script::new()
            .helper("HELPER-TASK", Step::Text("helper"))
            .told_finished(Step::Text("noted"))
            .input("STOP", Step::Text("stopping"))
            .after("a1", Step::Text("waiting"))
            .after_any(Step::Text("some result"))
            .otherwise(Step::Text("start"));
        assert_eq!(text(script.resolve(&[user("HELPER-TASK: x")])), "helper");
        assert_eq!(
            text(script.resolve(&[user("go"), user("scout finished its task")])),
            "noted"
        );
        assert_eq!(text(script.resolve(&[user("STOP")])), "stopping");
        assert_eq!(text(script.resolve(&[user("go"), result("a1")])), "waiting");
        assert_eq!(
            text(script.resolve(&[user("go"), result("zz")])),
            "some result"
        );
        assert_eq!(text(script.resolve(&[user("go")])), "start");
        assert_eq!(
            text(Script::new().resolve(&[user("go")])),
            "script exhausted"
        );
    }

    #[test]
    fn a_nested_script_a_made_step_and_a_held_one() {
        let gate = Arc::new(Notify::new());
        let helper = Script::new()
            .after("h1", Step::Text("helper done"))
            .otherwise(call("h1", "shell", json!({})));
        let script = Script::new()
            .mentions("HELPER", helper)
            .input(
                "count",
                make(|m| Step::Text(super::super::leak(m.len().to_string()))),
            )
            .otherwise(hold(&gate, Step::Text("held")));
        assert_eq!(tool(&script.resolve(&[user("HELPER")])), "h1");
        assert_eq!(
            text(script.resolve(&[user("HELPER"), result("h1")])),
            "helper done"
        );
        assert_eq!(text(script.resolve(&[user("a"), user("count")])), "2");
        assert!(matches!(script.resolve(&[user("go")]), Step::Gated(..)));
    }

    #[test]
    #[should_panic(expected = "a step of a sequence follows a tool call")]
    fn a_step_after_text_is_refused() {
        let _ = Script::new().text("one").text("two");
    }
}
