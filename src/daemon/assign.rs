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
//!   this call asked for ([`HelperRegistry::reserve`]).
//! - **How many.** Each agent and each helper holds at most
//!   `helpers.max_live` helpers of its own from `assign` at a time
//!   ([`crate::mobs::held_by`]: running, kept and idle, or being seated); a
//!   call's [`crate::mobs::registry::Reservation`] holds its name and a slot from
//!   before the spawn, so two calls of one step cannot both take the last
//!   one or one name, and a failed or cancelled call lets both go.
//! - **Fresh.** Through the mob host's spawn, wired to its spawner both
//!   ways (`auto_wire_parent`), exactly as the engine seats any member.
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

use crate::mobs::{HelperContext, HelperKey, HelperKind, HelperLife, HelperRegistry, Refused};
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

/// The engine's words for a spawn of a member id its mob holds
/// (`MobError::MemberAlreadyExists`), as a fork's refusal carries them.
const MEMBER_EXISTS: &str = "mob member already exists";

/// Why `name` cannot be given: a member is seated under it, or another
/// `assign` call is seating one.
#[must_use]
pub fn name_in_use(name: &str) -> String {
    format!(
        "the name `{name}` is in use by a member seated now (`contacts` lists your helpers); call `assign` again with another name"
    )
}

/// One helper a caller holds, as the limit's refusal names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    /// Its name.
    pub name: String,
    /// Whether it is kept and idle now (the ones to retire).
    pub kept_idle: bool,
}

/// How many names the limit's refusal lists before it says how many more.
const NAMES_LISTED: usize = 12;

/// Why a caller holding `held` gets no other helper under `limit`
/// (`helpers.max_live`): what it holds, the kept idle ones first (they are
/// the ones to retire), and what to do.
#[must_use]
pub fn limit_refusal(held: &[Holding], limit: usize) -> String {
    let idle = held.iter().filter(|holding| holding.kept_idle).count();
    let group = |idle: bool| {
        let mut names: Vec<&str> = held
            .iter()
            .filter(|holding| holding.kept_idle == idle)
            .map(|holding| holding.name.as_str())
            .collect();
        names.sort_unstable();
        names
    };
    let mut names = group(true);
    names.extend(group(false));
    let more = names.len().saturating_sub(NAMES_LISTED);
    names.truncate(NAMES_LISTED);
    let listed = match more {
        0 => names.join(", "),
        more => format!(
            "{} and {more} more (`contacts` lists them all)",
            names.join(", ")
        ),
    };
    let count = crate::text::count(held.len(), "helper");
    let idle = match idle {
        0 => String::new(),
        n => format!(" ({n} of them kept and idle)"),
    };
    let against = if held.len() > limit {
        format!("more than this workspace's limit of {limit}")
    } else {
        "this workspace's limit".to_string()
    };
    format!(
        "you hold {count}{idle}, which is {against} (helpers.max_live): {listed}. Retire with retire_helper the ones you no longer need, or wait for a single-use one to finish, then call assign again."
    )
}

/// The answer to an `assign` that seated `name` as `kind`.
#[must_use]
pub fn started_text(name: &str, kind: HelperKind) -> String {
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
            "It is kept: after each run it stays seated and idle, and you are told in a note with its answer; do not wait or poll for it. A message to it starts another run, so send one only with more work. It stays until you retire it with `retire_helper`: do so as soon as its work is done."
        }
        (HelperContext::Fork, HelperLife::Kept) => {
            "It is kept: it then stays seated and idle, and after each later run you are told in a note with its answer; do not wait or poll for it. A message to it starts another run, so send one only with more work. It stays until you retire it with `retire_helper`: do so as soon as its work is done."
        }
    };
    format!(
        "{start} {end} Meanwhile you can message it with `send_message` (`contacts` gives its peer_id), and it can message you."
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
    /// `contacts` ([`super::contacts`]).
    contacts: Arc<super::contacts::Contacts>,
}

impl std::fmt::Debug for Assigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assigner").finish_non_exhaustive()
    }
}

impl Assigner {
    /// Seat helpers for the sessions `registry` knows; `contacts` answers
    /// who each can message.
    #[must_use]
    pub fn new(registry: Arc<HelperRegistry>, contacts: Arc<super::contacts::Contacts>) -> Self {
        Self {
            connector: super::connect::Connector::new(Arc::clone(&registry)),
            registry,
            state: OnceLock::new(),
            routes: OnceLock::new(),
            contacts,
        }
    }

    /// Seat and connect them through `state` (once; it is made after the
    /// tools).
    pub fn bind(&self, state: &Arc<MobMcpState>, links: &Arc<super::links::Links>) {
        let _ = self.state.set(Arc::downgrade(state));
        self.connector.bind(state, links);
        self.contacts.bind(state, links);
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
        let (owner, path) = self.place(caller, &parent).ok_or(not_here)?;
        let handle = state.handle_for(&parent.mob).await.map_err(|err| {
            format!("your mob cannot be reached, so no helper was started: {err}")
        })?;
        let members = handle.list_members().await;
        if members
            .iter()
            .any(|member| member.agent_identity.as_str() == name)
        {
            return Err(name_in_use(&name));
        }
        let role = members
            .iter()
            .find(|member| member.agent_identity == parent.member)
            .map(|member| member.role.clone())
            .ok_or(not_here)?;
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
        // Its binding keeps the task's first line (`contacts` shows it).
        let given = task.clone();
        // The name and a slot under the caller's limit, held until the
        // spawn is done (or dropped with a call cancelled meanwhile).
        let held = crate::mobs::held_by(
            crate::mobs::seated_for(&state, &self.registry, &owner).await,
            &parent.mob,
            &path,
        );
        let held_keys: Vec<HelperKey> = held.iter().map(|(key, _)| key.clone()).collect();
        let reservation = self
            .registry
            .reserve(key.clone(), kind, &owner, &path, &held_keys)
            .map_err(|refused| match refused {
                Refused::NameInUse => name_in_use(&name),
                Refused::Full { starting, limit } => {
                    let held: Vec<Holding> = held
                        .iter()
                        .map(|(key, tag)| Holding {
                            name: tag.label().to_string(),
                            kept_idle: tag.kind.life() == Some(HelperLife::Kept)
                                && !self
                                    .registry
                                    .activity(key)
                                    .is_some_and(|(running, _)| running),
                        })
                        .chain(starting.iter().map(|key| Holding {
                            name: key.member.to_string(),
                            kept_idle: false,
                        }))
                        .collect();
                    limit_refusal(&held, limit)
                }
            })?;
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
                    .map_err(|err| match err {
                        meerkat_mob::MobError::MemberAlreadyExists(_) => name_in_use(&name),
                        err => format!("the helper `{name}` was not started: {err}"),
                    })
            }
            HelperStart::Fork => self.fork(caller, id, context, &name, task).await,
        };
        if seated.is_ok() {
            reservation.launched();
            self.registry.note_task(&key, &given);
        }
        seated.map(|()| started_text(&name, kind))
    }

    /// Where the caller of `key`, running as `caller`, sits: the owner
    /// session of its tree and its own path (empty for an agent).
    fn place(&self, caller: &SessionId, key: &HelperKey) -> Option<(SessionId, Vec<String>)> {
        if self.registry.root_session(key).as_ref() == Some(caller) {
            return Some((caller.clone(), Vec::new()));
        }
        self.registry
            .bound_members()
            .into_iter()
            .find_map(|(bound, tag)| (&bound == key).then_some((tag.owner, tag.path)))
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
            .map_err(|err| {
                let err = err.to_string();
                if err.contains(MEMBER_EXISTS) {
                    name_in_use(name)
                } else {
                    format!("the fork `{name}` was not started: {err}")
                }
            })
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

    async fn contacts(&self, caller: Option<&SessionId>) -> (String, bool) {
        self.contacts.answer(caller).await
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

    /// What counts against a caller's limit: its own helpers one level
    /// below it in its own mob; not its helpers' helpers, not another
    /// agent's, not a member of a mob it made with `mob_create`.
    #[test]
    fn the_limit_counts_the_caller_s_own_helpers_in_its_mob() {
        use meerkat_mob::MobId;
        let agent = SessionId::new();
        let other = SessionId::new();
        let tag = |owner: &SessionId, path: &[&str]| crate::mobs::HelperTag {
            owner: owner.clone(),
            path: path.iter().map(ToString::to_string).collect(),
            model: "m".into(),
            kind: HelperKind::Started(HelperContext::Fresh, HelperLife::Kept),
        };
        let key = |mob: &str, member: &str| HelperKey::new(MobId::from(mob), member.into());
        let seated = [
            (key("tui", "a"), tag(&agent, &["a"])),
            (key("tui", "b"), tag(&agent, &["b"])),
            (key("tui", "a1"), tag(&agent, &["a", "a1"])),
            (key("team", "t"), tag(&agent, &["t"])),
            (key("tui", "x"), tag(&other, &["x"])),
        ];
        let names = |held: Vec<(HelperKey, crate::mobs::HelperTag)>| {
            held.into_iter()
                .map(|(key, _)| key.member.to_string())
                .collect::<Vec<_>>()
        };
        let mine: Vec<_> = seated
            .iter()
            .filter(|(_, tag)| tag.owner == agent)
            .cloned()
            .collect();
        assert_eq!(
            names(crate::mobs::held_by(mine.clone(), &MobId::from("tui"), &[])),
            ["a", "b"]
        );
        assert_eq!(
            names(crate::mobs::held_by(
                mine,
                &MobId::from("tui"),
                &["a".to_string()]
            )),
            ["a1"]
        );
    }

    /// The limit's refusal: how many, which, the kept idle ones first and
    /// counted, what to do; a long list is cut; over a lowered limit it
    /// says so.
    #[test]
    fn the_limit_s_refusal_names_the_helpers_and_what_to_do() {
        let held = |names: &[&str], idle: &[&str]| -> Vec<Holding> {
            names
                .iter()
                .map(|name| Holding {
                    name: (*name).to_string(),
                    kept_idle: idle.contains(name),
                })
                .collect()
        };
        let eight = held(&["a", "b", "c", "d", "e", "f", "g", "h"], &["f", "h"]);
        assert_eq!(
            limit_refusal(&eight, 8),
            "you hold 8 helpers (2 of them kept and idle), which is this workspace's limit (helpers.max_live): f, h, a, b, c, d, e, g. Retire with retire_helper the ones you no longer need, or wait for a single-use one to finish, then call assign again."
        );
        let many: Vec<String> = (0..20).map(|n| format!("w{n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let text = limit_refusal(&held(&many, &[]), 4);
        assert!(
            text.starts_with("you hold 20 helpers, which is more than this workspace's limit of 4 (helpers.max_live): w0, w1,")
                && text.contains("w19 and 8 more (`contacts` lists them all). Retire with retire_helper"),
            "{text}"
        );
    }

    /// Each of the four kinds is told how it ends and how to reach it; a
    /// kept one how to retire it.
    #[test]
    fn the_answer_says_how_each_kind_ends() {
        use HelperContext::{Fork, Fresh};
        use HelperLife::{Kept, Single};
        for (context, life) in [(Fresh, Single), (Fresh, Kept), (Fork, Single), (Fork, Kept)] {
            let text = started_text("w", HelperKind::Started(context, life));
            assert!(text.contains("`contacts` gives its peer_id"), "{text}");
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
