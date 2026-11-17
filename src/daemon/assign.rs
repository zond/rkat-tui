//! `assign` in the daemon: seat a helper beside its caller and answer at
//! once. Two choices, independent: what it starts from (`context`: the
//! task alone, or a fork of the caller's conversation) and how long it
//! stays (`keep`: single-use, or kept until its parent retires it).
//!
//! - **Where.** In the caller's own mob (an agent's is the workspace's
//!   mob `tui`; a helper's is the one it is seated in), under the caller's
//!   own profile, with the caller's session as the owner context: the
//!   engine finds the caller as the spawner (`spawner_for_bridge_session`)
//!   and the customizer shapes the helper as every helper (gate, tools,
//!   persona, scratch directory, model rule, bookkeeping), with the kind
//!   this call asked for ([`HelperRegistry::intend`]).
//! - **Fresh.** Through the mob host's spawn, wired to its spawner both
//!   ways (`auto_wire_parent`), exactly as the engine's `spawn_member`
//!   seats a member.
//! - **Fork.** Through the engine's own `fork_off` of the caller's mob tool
//!   surface ([`super::deferral::ForkRoutes`]), with this call's dispatch
//!   context: the engine forks the caller's committed transcript, runs the
//!   child under its completion custodian and records its outcome
//!   (`ForkJobTerminal`), from which a restart delivers it again; the
//!   follower wires the child to its parent once it is seated
//!   ([`crate::mobs`]). A fork runs on its caller's model.
//! - **When it ends.** A single-use helper's parent is told when it
//!   finishes (its run ended and the runtime holds no unfinished input of
//!   it), with its answer, and it is retired then; a kept one's parent is
//!   told each time it goes idle ([`crate::mobs::endings`]). A fork's first
//!   run is told by the engine's own outcome.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use meerkat_core::{SessionId, ToolCallView, ToolDispatchContext};
use meerkat_mob::{AgentIdentity, SpawnMemberSpec};
use meerkat_mob_mcp::MobMcpState;

use crate::mobs::{HelperContext, HelperKey, HelperKind, HelperLife, HelperRegistry};
use crate::tools::{AssignArgs, AssignHost, HelperStart};

/// The longest name a helper gets.
pub const MAX_NAME: usize = 32;

/// What a well-formed name is, for the refusal of one that is not.
const NAME_RULE: &str =
    "a name is 1 to 32 lowercase letters, digits and '-', starting with a letter or digit";

/// The engine's tool a fork is started with (not offered to any member:
/// only `assign` reaches it, [`super::deferral::ForkRoutes`]).
const FORK_OFF: &str = "fork_off";

/// Why `name` is no helper's name (`None`: it is one).
#[must_use]
pub fn name_refusal(name: &str) -> Option<String> {
    let well_formed = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if !well_formed {
        return Some(format!(
            "`{name}` is not a helper name: {NAME_RULE}; call `assign` again with one"
        ));
    }
    if super::agents::is_agent(name) {
        return Some(format!(
            "`{name}` is reserved for the workspace's agents (`main-` and four characters); call `assign` again with another name"
        ));
    }
    None
}

/// Why a fork with a `model` is refused.
pub const FORK_MODEL_REFUSAL: &str = "a fork runs on your own model (it goes on from your conversation), so `model` cannot be given with `context: \"fork\"`; call `assign` again without `model`, or with `context: \"fresh\"` and a task that carries what the helper needs";

/// The answer to an `assign` that seated `name` (`peer` in `peers`) as
/// `kind`.
#[must_use]
pub fn started_text(name: &str, peer: &str, kind: HelperKind) -> String {
    let (context, life) = match kind {
        HelperKind::Started(context, life) => (context, life),
        HelperKind::Other => (HelperContext::Fresh, HelperLife::Single),
    };
    let start = match context {
        HelperContext::Fresh => format!("Helper `{name}` is running."),
        HelperContext::Fork => format!(
            "Helper `{name}` is running on a fork of this conversation. Its first answer is written into your conversation as \"Background fork_off job … finished\"."
        ),
    };
    let end = match (context, life) {
        (HelperContext::Fresh, HelperLife::Single) => {
            "You will be told in a note when it finishes, with its answer, and it is then retired; do not wait or poll for it."
        }
        (HelperContext::Fork, HelperLife::Single) => {
            "It is then retired; do not wait or poll for it."
        }
        (HelperContext::Fresh, HelperLife::Kept) => {
            "It is kept: after each run it stays seated and idle, and you are told in a note with its answer; do not wait or poll for it. A message to it starts another run, so send one only with more work, and retire it with `retire_helper` when you need it no more."
        }
        (HelperContext::Fork, HelperLife::Kept) => {
            "It is kept: it then stays seated and idle, and after each later run you are told in a note with its answer; do not wait or poll for it. A message to it starts another run, so send one only with more work, and retire it with `retire_helper` when you need it no more."
        }
    };
    format!(
        "{start} {end} Meanwhile you can message it with `send_message` (it is `{peer}` in `peers`), and it can message you."
    )
}

/// The daemon's `assign` ([`AssignHost`]).
pub struct Assigner {
    registry: Arc<HelperRegistry>,
    state: OnceLock<Weak<MobMcpState>>,
    /// The members' engine mob tool surfaces, which forks go through.
    routes: OnceLock<Arc<super::deferral::ForkRoutes>>,
    /// `connect_helpers` ([`super::connect`]).
    connector: super::connect::Connector,
}

impl std::fmt::Debug for Assigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assigner").finish_non_exhaustive()
    }
}

impl Assigner {
    /// Seat helpers for the sessions `registry` knows.
    #[must_use]
    pub fn new(registry: Arc<HelperRegistry>) -> Self {
        Self {
            connector: super::connect::Connector::new(Arc::clone(&registry)),
            registry,
            state: OnceLock::new(),
            routes: OnceLock::new(),
        }
    }

    /// Seat and connect them through `state` (once; it is made after the
    /// tools).
    pub fn bind(&self, state: &Arc<MobMcpState>, links: &Arc<super::links::Links>) {
        let _ = self.state.set(Arc::downgrade(state));
        self.connector.bind(state, links);
    }

    /// Start forks through `routes` (once; made with the mob tools
    /// factory, after the tools).
    pub fn fork_through(&self, routes: Arc<super::deferral::ForkRoutes>) {
        let _ = self.routes.set(routes);
    }

    /// The member running as `caller`: an agent, or a helper.
    fn caller_key(&self, caller: &SessionId) -> Option<HelperKey> {
        self.registry
            .root_of_session(caller)
            .or_else(|| self.registry.member_key_of_session(caller))
    }

    async fn seat(
        &self,
        caller: Option<&SessionId>,
        id: &str,
        context: &ToolDispatchContext,
        args: AssignArgs,
    ) -> Result<String, String> {
        let AssignArgs {
            name,
            task,
            context: start,
            keep,
            model,
        } = args;
        if let Some(refused) = name_refusal(&name) {
            return Err(refused);
        }
        if task.trim().is_empty() {
            return Err(
                "`task` is empty: give the helper its whole task and the context it needs"
                    .to_string(),
            );
        }
        if start == HelperStart::Fork && model.is_some() {
            return Err(FORK_MODEL_REFUSAL.to_string());
        }
        let not_here = "`assign` is not available to this caller (it is no agent or helper of this workspace's daemon)";
        let caller = caller.ok_or(not_here)?;
        let parent = self.caller_key(caller).ok_or(not_here)?;
        let state = self
            .state
            .get()
            .and_then(Weak::upgrade)
            .ok_or("the daemon is stopping: no helper can be started")?;
        let handle = state.handle_for(&parent.mob).await.map_err(|err| {
            format!("your mob cannot be reached, so no helper was started: {err}")
        })?;
        let members = handle.list_members().await;
        if members
            .iter()
            .any(|member| member.agent_identity.as_str() == name)
        {
            return Err(format!(
                "the name `{name}` is taken by another member (`list_members` shows them); call `assign` again with another name"
            ));
        }
        let role = members
            .iter()
            .find(|member| member.agent_identity == parent.member)
            .map(|member| member.role.clone())
            .ok_or(not_here)?;
        let peer = meerkat_core::MemberCommsName::new(parent.mob.as_str(), role.as_str(), &name)
            .map_or_else(|_| name.clone(), |peer| peer.to_string());
        let life = if keep {
            HelperLife::Kept
        } else {
            HelperLife::Single
        };
        let kind = match start {
            HelperStart::Fresh => HelperKind::Started(HelperContext::Fresh, life),
            HelperStart::Fork => HelperKind::Started(HelperContext::Fork, life),
        };
        let key = HelperKey::new(parent.mob.clone(), AgentIdentity::from(name.as_str()));
        self.registry.intend(key.clone(), kind);
        let seated = match start {
            HelperStart::Fresh => {
                let mut spec = SpawnMemberSpec::new(role, AgentIdentity::from(name.as_str()))
                    .with_initial_message(task)
                    .with_auto_wire_parent(true);
                spec.model_override = model;
                let witness = meerkat_mob::CreationSourceCapture::classify(
                    handle.capture_member_creation_source(caller).await,
                )
                .into_admitted_witness(crate::tools::ASSIGN);
                spec = spec.with_creation_source(witness);
                handle
                    .spawn_spec_with_generated_owner_context(spec, caller.clone())
                    .await
                    .map(|_| ())
                    .map_err(|err| format!("the helper `{name}` was not started: {err}"))
            }
            HelperStart::Fork => self.fork(caller, id, context, &name, task).await,
        };
        self.registry.forget_intent(&key);
        seated.map(|()| started_text(&name, &peer, kind))
    }

    /// Fork `caller` as `name` on `task` through the engine's `fork_off` of
    /// its own mob tool surface, in the call `id` dispatched with
    /// `context`.
    async fn fork(
        &self,
        caller: &SessionId,
        id: &str,
        context: &ToolDispatchContext,
        name: &str,
        task: String,
    ) -> Result<(), String> {
        let surface = self
            .routes
            .get()
            .and_then(|routes| routes.surface(caller))
            .ok_or_else(|| {
                format!("the fork `{name}` was not started: this conversation has no mob tools to fork it with; start a helper with `context: \"fresh\"` and a task that carries what it needs")
            })?;
        let args = serde_json::json!({"member_id": name, "task": task});
        let args = serde_json::value::RawValue::from_string(args.to_string())
            .map_err(|err| format!("the fork `{name}` was not started: {err}"))?;
        let call = ToolCallView {
            id,
            name: FORK_OFF,
            args: &args,
        };
        surface
            .dispatch_with_context(call, context)
            .await
            .map(|_| ())
            .map_err(|err| format!("the fork `{name}` was not started: {err}"))
    }
}

#[async_trait]
impl AssignHost for Assigner {
    async fn assign(
        &self,
        caller: Option<&SessionId>,
        id: &str,
        context: &ToolDispatchContext,
        args: AssignArgs,
    ) -> (String, bool) {
        match self.seat(caller, id, context, args).await {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    async fn connect(
        &self,
        caller: Option<&SessionId>,
        args: crate::tools::ConnectArgs,
    ) -> (String, bool) {
        match self.connector.connect(caller, &args.helpers).await {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    async fn retire(
        &self,
        caller: Option<&SessionId>,
        args: crate::tools::RetireArgs,
    ) -> (String, bool) {
        match self.connector.retire(caller, &args.name).await {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_short_lowercase_and_no_agent_s() {
        for good in ["scout", "a", "test-runner-2", "9lives"] {
            assert_eq!(name_refusal(good), None, "{good}");
        }
        for bad in ["", "Scout", "has space", "-lead", "x_y", &"a".repeat(33)] {
            let refused = name_refusal(bad).unwrap_or_default();
            assert!(
                refused.contains("1 to 32 lowercase letters")
                    && refused.contains("call `assign` again"),
                "{bad}: {refused}"
            );
        }
        assert!(
            name_refusal("main-ab12")
                .is_some_and(|refused| refused.contains("reserved for the workspace's agents"))
        );
    }

    /// Each of the four kinds is told how it ends and how to reach it; a
    /// kept one how to retire it.
    #[test]
    fn the_answer_says_how_each_kind_ends() {
        use HelperContext::{Fork, Fresh};
        use HelperLife::{Kept, Single};
        for (context, life) in [(Fresh, Single), (Fresh, Kept), (Fork, Single), (Fork, Kept)] {
            let text = started_text("w", "tui/main/w", HelperKind::Started(context, life));
            assert!(text.contains("`tui/main/w` in `peers`"), "{text}");
            assert_eq!(text.contains("retire_helper"), life == Kept, "{text}");
            assert_eq!(text.contains("then retired"), life == Single, "{text}");
            assert_eq!(
                text.contains("Background fork_off job"),
                context == Fork,
                "{text}"
            );
        }
    }
}
