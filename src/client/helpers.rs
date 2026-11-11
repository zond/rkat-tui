//! The agent's helpers in the client: the follower view
//! (`[helper · model]` headers, their progress and result, the running
//! count, per-helper cost) from the mobs' own event streams.
//!
//! The daemon hosts every helper the agent starts: `delegate`
//! helpers in the agent session's implicit mob
//! (`implicit-<session>`), `spawn_member` members in the agent's own
//! mob, and the helpers of helpers in theirs. A mob-wide `mob/stream_open
//! {mob_id}` (no `agent_identity`) carries every member's events, each
//! attributed to its member ([`AttributedEvent`]); [`Followers`] turns them
//! into the [`Msg::Scoped`] events and [`StatusUpdate::Helpers`] updates
//! the UI renders.
//!
//! What the streams do not give, and how it is filled in:
//!
//! - A mob-wide stream subscribes a member that joins later from the
//!   member's live events on, so a helper's first events (its run start)
//!   can come before the stream has it; and an implicit mob exists only
//!   once the first `delegate` creates it. The runner lists the mobs once
//!   per attach, the daemon pushes each mob that hosts a helper it follows
//!   (`rkat/mob`), the runner opens a stream on each of this agent's, and
//!   the first time a helper is seen its session's durable
//!   events are read (`events/list_since`, [`Action::Backfill`]); its live
//!   events are held until that read returns, then shown in order, each
//!   once (a high-water mark per helper session, as for the agent).
//! - The model a helper runs on is read from its session
//!   (`events/snapshot`, [`Action::Snapshot`]); until then the header
//!   names the agent's model, which a `delegate` helper inherits.
//! - The engine attributes a helper of a helper to its own mob
//!   (`implicit-<parent session>`): the path of the helper whose session
//!   that is nests it.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use meerkat_core::{AgentEvent, EventEnvelope, EventSourceIdentity, SessionId};
use meerkat_mob::AttributedEvent;

use crate::daemon::MOB_ID;
use crate::daemon::agents::is_agent;
use crate::mobs::{HelperStatus, HelperTag};
use crate::ui::{Msg, StatusUpdate};

/// The prefix of a session's implicit (`delegate`) mob.
pub const IMPLICIT_PREFIX: &str = "implicit-";

/// A helper: its mob and member identity.
pub type HelperId = (String, String);

/// What the runner does for [`Followers`].
#[derive(Debug)]
pub enum Action {
    /// Send this to the UI.
    Ui(Msg),
    /// Read `session`'s durable events after `after` and hand them to
    /// [`Followers::backfilled`].
    Backfill {
        /// The helper.
        helper: HelperId,
        /// Its session.
        session: SessionId,
        /// The sequence to read after.
        after: u64,
    },
    /// Read `session`'s model and hand it to [`Followers::model`].
    Snapshot {
        /// The helper.
        helper: HelperId,
        /// Its session.
        session: SessionId,
    },
}

#[derive(Debug, Default)]
struct Helper {
    /// Labels from the outermost helper to this one.
    path: Vec<String>,
    session: Option<SessionId>,
    /// The highest sequence shown.
    shown: u64,
    /// Live events held while the backfill runs.
    held: Option<Vec<EventEnvelope<AgentEvent>>>,
    live: bool,
    model: Option<String>,
}

/// The helpers followed on one connection, for one agent session.
#[derive(Debug, Default)]
pub struct Followers {
    owner: Option<SessionId>,
    /// Stream id → mob.
    streams: HashMap<String, String>,
    /// Mobs with a stream open or opening.
    open: HashSet<String>,
    helpers: IndexMap<HelperId, Helper>,
    /// The councils the agent or one of its helpers convened, by their
    /// temporary mob, with the labels above their participants
    /// ([`Self::convening`]).
    convened: HashMap<String, Vec<String>>,
    /// The agent's model (a helper's until its own is read).
    default_model: String,
}

impl Followers {
    /// Follow the helpers of the agent running as `owner`; what was
    /// followed before is forgotten (a new conversation).
    pub fn reset(&mut self, owner: SessionId) {
        *self = Self {
            owner: Some(owner),
            default_model: std::mem::take(&mut self.default_model),
            ..Self::default()
        };
    }

    /// The agent session whose helpers these are.
    #[must_use]
    pub fn owner(&self) -> Option<&SessionId> {
        self.owner.as_ref()
    }

    /// The connection is gone: its streams with it. What was shown stays,
    /// so a reattach shows nothing twice.
    pub fn streams_lost(&mut self) {
        self.streams.clear();
        self.open.clear();
    }

    /// The agent runs on `model`.
    pub fn set_default_model(&mut self, model: &str) {
        model.clone_into(&mut self.default_model);
    }

    /// Whether `mob` should be opened: one of this agent's (the daemon's
    /// mob, the implicit mob of the agent's session or of one of its
    /// helpers' sessions; another agent's helpers are its own clients'),
    /// and not yet open or opening. Marks it opening.
    pub fn should_open(&mut self, mob: &str) -> bool {
        if !self.is_ours(mob) {
            return false;
        }
        self.open.insert(mob.to_string())
    }

    /// `event` of the agent (`by` `None`) or of its helper `by`: a
    /// `council` call convenes a council, whose temporary mob (the one the
    /// engine creates for the call, `crate::mobs::council::council_mob`)
    /// holds this agent's helpers from now on.
    pub fn convening(&mut self, event: &AgentEvent, by: Option<&HelperId>) {
        let AgentEvent::ToolCallRequested { id, name, args } = event else {
            return;
        };
        if name != crate::mobs::council::COUNCIL {
            return;
        }
        let (session, path) = match by {
            None => (self.owner.clone(), Vec::new()),
            Some(helper) => match self.helpers.get(helper) {
                Some(helper) => (helper.session.clone(), helper.path.clone()),
                None => return,
            },
        };
        if let Some(mob) = session
            .and_then(|session| crate::mobs::council::council_mob(&session, id, args.as_value()))
        {
            self.convened.insert(mob.to_string(), path);
        }
    }

    /// Whether `mob` holds this agent's helpers.
    fn is_ours(&self, mob: &str) -> bool {
        if self.convened.contains_key(mob) {
            return true;
        }
        let Some(session) = mob.strip_prefix(IMPLICIT_PREFIX) else {
            return mob == MOB_ID;
        };
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.to_string() == session)
            || self
                .helpers
                .values()
                .filter_map(|helper| helper.session.as_ref())
                .any(|helper| helper.to_string() == session)
    }

    /// Opening `mob`'s stream failed: it may be tried again.
    pub fn open_failed(&mut self, mob: &str) {
        self.open.remove(mob);
    }

    /// `stream` carries `mob`'s events now.
    pub fn opened(&mut self, stream: String, mob: &str) {
        self.open.insert(mob.to_string());
        self.streams.insert(stream, mob.to_string());
    }

    /// Whether `stream` is one of the followers'.
    #[must_use]
    pub fn owns(&self, stream: &str) -> bool {
        self.streams.contains_key(stream)
    }

    /// `stream` ended (its mob was destroyed, or the daemon dropped it).
    pub fn ended(&mut self, stream: &str) {
        if let Some(mob) = self.streams.remove(stream) {
            self.open.remove(&mob);
        }
    }

    /// The helpers running now.
    #[must_use]
    pub fn live(&self) -> Vec<HelperStatus> {
        self.helpers
            .values()
            .filter(|helper| helper.live)
            .map(|helper| HelperStatus {
                label: helper.path.last().cloned().unwrap_or_default(),
                model: helper
                    .model
                    .clone()
                    .unwrap_or_else(|| self.default_model.clone()),
                depth: helper.path.len(),
            })
            .collect()
    }

    fn live_update(&self) -> Action {
        Action::Ui(Msg::Status(StatusUpdate::Helpers(self.live())))
    }

    /// The labels above a helper of `mob`: none for the agent's own
    /// mobs, else the path of the helper whose implicit mob it is.
    fn parent_path(&self, mob: &str) -> Vec<String> {
        if let Some(path) = self.convened.get(mob) {
            return path.clone();
        }
        let Some(session) = mob
            .strip_prefix(IMPLICIT_PREFIX)
            .and_then(|id| SessionId::parse(id).ok())
        else {
            return Vec::new();
        };
        if self.owner.as_ref() == Some(&session) {
            return Vec::new();
        }
        self.helpers
            .values()
            .find(|helper| helper.session.as_ref() == Some(&session))
            .map(|helper| helper.path.clone())
            .unwrap_or_default()
    }

    /// A helper not seen before: start following it (its backfill and
    /// model read when its session is known).
    fn first_sight(&mut self, id: &HelperId, session: Option<SessionId>) -> Vec<Action> {
        let mut path = self.parent_path(&id.0);
        path.push(id.1.clone());
        let held = session.as_ref().map(|_| Vec::new());
        self.helpers.insert(
            id.clone(),
            Helper {
                path,
                session: session.clone(),
                held,
                ..Helper::default()
            },
        );
        match session {
            Some(session) => vec![
                Action::Backfill {
                    helper: id.clone(),
                    session: session.clone(),
                    after: 0,
                },
                Action::Snapshot {
                    helper: id.clone(),
                    session,
                },
            ],
            None => Vec::new(),
        }
    }

    /// A member of `mob` found by listing it after its stream opened.
    pub fn member(&mut self, mob: &str, label: &str, session: Option<SessionId>) -> Vec<Action> {
        if mob == MOB_ID && is_agent(label) {
            return Vec::new();
        }
        let id = (mob.to_string(), label.to_string());
        if self.helpers.contains_key(&id) {
            return Vec::new();
        }
        self.first_sight(&id, session)
    }

    /// The helper `attributed` of `stream` is from, when the stream is a
    /// followed mob's.
    #[must_use]
    pub fn helper_of(&self, stream: &str, attributed: &AttributedEvent) -> Option<HelperId> {
        let mob = self.streams.get(stream)?;
        Some((mob.clone(), attributed.source.identity.to_string()))
    }

    /// One event of a followed stream.
    pub fn event(&mut self, stream: &str, attributed: AttributedEvent) -> Vec<Action> {
        let Some(mob) = self.streams.get(stream).cloned() else {
            return Vec::new();
        };
        let label = attributed.source.identity.to_string();
        if mob == MOB_ID && is_agent(&label) {
            // The agent's own stream shows its events; another agent's
            // are its clients'.
            return Vec::new();
        }
        let envelope = attributed.envelope;
        let session = match &envelope.source {
            EventSourceIdentity::Session { session_id } => Some(session_id.clone()),
            _ => None,
        };
        let id = (mob, label);
        let mut actions = Vec::new();
        if !self.helpers.contains_key(&id) {
            actions.extend(self.first_sight(&id, session.clone()));
        }
        let Some(helper) = self.helpers.get_mut(&id) else {
            return actions;
        };
        if helper.session.is_none()
            && let Some(session) = session
        {
            helper.session = Some(session);
        }
        if let Some(held) = &mut helper.held {
            held.push(envelope);
            return actions;
        }
        actions.extend(self.show(&id, vec![envelope]));
        actions
    }

    /// The durable events read for `helper`: shown in order with the live
    /// ones held meanwhile, each once.
    pub fn backfilled(
        &mut self,
        helper: &HelperId,
        mut events: Vec<EventEnvelope<AgentEvent>>,
    ) -> Vec<Action> {
        let Some(entry) = self.helpers.get_mut(helper) else {
            return Vec::new();
        };
        events.extend(entry.held.take().unwrap_or_default());
        events.sort_by_key(|envelope| envelope.seq);
        self.show(helper, events)
    }

    /// `helper` runs on `model`.
    pub fn model(&mut self, helper: &HelperId, model: String) -> Vec<Action> {
        let Some(entry) = self.helpers.get_mut(helper) else {
            return Vec::new();
        };
        if entry.model.as_ref() == Some(&model) {
            return Vec::new();
        }
        entry.model = Some(model);
        if entry.live {
            vec![self.live_update()]
        } else {
            Vec::new()
        }
    }

    /// Show `events` of `helper` above its high-water mark.
    fn show(&mut self, id: &HelperId, events: Vec<EventEnvelope<AgentEvent>>) -> Vec<Action> {
        let Some(owner) = self.owner.clone() else {
            return Vec::new();
        };
        let default_model = self.default_model.clone();
        let Some(helper) = self.helpers.get_mut(id) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        let mut live_changed = false;
        for envelope in events {
            if envelope.seq != 0 && envelope.seq <= helper.shown {
                continue;
            }
            helper.shown = helper.shown.max(envelope.seq);
            let running = !matches!(
                envelope.payload,
                AgentEvent::RunCompleted { .. } | AgentEvent::RunFailed { .. }
            );
            if helper.live != running {
                helper.live = running;
                live_changed = true;
            }
            let tag = HelperTag {
                owner: owner.clone(),
                path: helper.path.clone(),
                model: helper
                    .model
                    .clone()
                    .unwrap_or_else(|| default_model.clone()),
            };
            actions.push(Action::Ui(Msg::Scoped(tag.scope(envelope.payload))));
        }
        if live_changed {
            actions.push(self.live_update());
        }
        actions
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::StreamScopeFrame;
    use meerkat_mob::{AgentIdentity, AgentRuntimeId, FenceToken, ProfileName};

    fn attributed(
        label: &str,
        session: &SessionId,
        seq: u64,
        event: AgentEvent,
    ) -> AttributedEvent {
        AttributedEvent {
            source: AgentRuntimeId::initial(AgentIdentity::from(label)),
            source_fence_token: FenceToken::new(1),
            role: ProfileName::from("delegate"),
            envelope: EventEnvelope::new_session(session.clone(), seq, None, event),
        }
    }

    fn text(t: &str) -> AgentEvent {
        AgentEvent::TextComplete {
            content: t.to_string(),
            assistant_message_id: None,
        }
    }

    fn run_completed(session: &SessionId) -> AgentEvent {
        let usage = serde_json::to_value(meerkat_core::CumulativeUsage::default()).unwrap();
        serde_json::from_value(serde_json::json!({
            "type": "run_completed",
            "session_id": session,
            "result": "done",
            "usage": usage,
        }))
        .unwrap()
    }

    fn scoped(actions: &[Action]) -> Vec<(Vec<String>, String)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Ui(Msg::Scoped(ev)) => Some((
                    ev.scope_path
                        .iter()
                        .filter_map(|f| match f {
                            StreamScopeFrame::MobMember { agent_identity, .. } => {
                                Some(agent_identity.clone())
                            }
                            _ => None,
                        })
                        .collect(),
                    match &ev.event {
                        AgentEvent::TextComplete { content, .. } => content.clone(),
                        other => format!("{other:?}").chars().take(12).collect(),
                    },
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_helper_is_backfilled_before_its_live_events_and_shown_once() {
        let owner = SessionId::new();
        let helper_session = SessionId::new();
        let mut followers = Followers::default();
        followers.reset(owner.clone());
        followers.set_default_model("m");
        let mob = format!("{IMPLICIT_PREFIX}{owner}");
        followers.opened("s1".into(), &mob);
        // The first live event: held, backfill and model asked for.
        let actions = followers.event("s1", attributed("scout", &helper_session, 3, text("three")));
        assert!(scoped(&actions).is_empty());
        assert!(
            matches!(&actions[0], Action::Backfill { after: 0, session, .. } if session == &helper_session)
        );
        assert!(matches!(&actions[1], Action::Snapshot { .. }));
        assert!(
            followers
                .event("s1", attributed("scout", &helper_session, 4, text("four")))
                .is_empty()
        );
        let id = (mob.clone(), "scout".to_string());
        let replayed = vec![
            EventEnvelope::new_session(helper_session.clone(), 1, None, text("one")),
            EventEnvelope::new_session(helper_session.clone(), 3, None, text("three")),
        ];
        let actions = followers.backfilled(&id, replayed);
        let shown: Vec<String> = scoped(&actions).into_iter().map(|(_, t)| t).collect();
        assert_eq!(shown, ["one", "three", "four"]);
        assert_eq!(followers.live().len(), 1);
        assert_eq!(followers.live()[0].model, "m");
        // Its own model, once read.
        let actions = followers.model(&id, "helper-model".into());
        assert!(
            matches!(&actions[..], [Action::Ui(Msg::Status(StatusUpdate::Helpers(h)))] if h[0].model == "helper-model")
        );
        // A repeat is dropped; the run's end takes it off the running set.
        assert!(
            scoped(&followers.event("s1", attributed("scout", &helper_session, 4, text("four"))))
                .is_empty()
        );
        let actions = followers.event(
            "s1",
            attributed("scout", &helper_session, 5, run_completed(&helper_session)),
        );
        assert!(actions.iter().any(
            |a| matches!(a, Action::Ui(Msg::Status(StatusUpdate::Helpers(h))) if h.is_empty())
        ));
    }

    #[test]
    fn a_council_the_agent_convenes_is_followed() {
        let owner = SessionId::new();
        let mut followers = Followers::default();
        followers.reset(owner.clone());
        let args = meerkat_core::ToolCallArguments::from_value(serde_json::json!({})).unwrap();
        let mob = crate::mobs::council::council_mob(&owner, "c1", args.as_value()).unwrap();
        assert!(!followers.should_open(mob.as_str()));
        followers.convening(
            &AgentEvent::ToolCallRequested {
                id: "c1".into(),
                name: "council".into(),
                args,
            },
            None,
        );
        assert!(followers.should_open(mob.as_str()));
        // Another agent's council is not this one's.
        assert!(!followers.should_open("council--agent-other"));
    }

    #[test]
    fn the_agent_is_not_a_helper_and_a_helpers_helper_nests() {
        let owner = SessionId::new();
        let mut followers = Followers::default();
        followers.reset(owner.clone());
        followers.opened("tui".into(), MOB_ID);
        assert!(
            followers
                .event("tui", attributed("main-ab12", &owner, 1, text("x")))
                .is_empty()
        );
        assert!(
            followers
                .member(MOB_ID, "main-ab12", Some(owner.clone()))
                .is_empty()
        );
        // A helper with a known session, then its own implicit mob.
        let scout = SessionId::new();
        let mob = format!("{IMPLICIT_PREFIX}{owner}");
        followers.opened("a".into(), &mob);
        let actions = followers.member(&mob, "scout", Some(scout.clone()));
        assert_eq!(actions.len(), 2);
        followers.backfilled(&(mob.clone(), "scout".into()), Vec::new());
        let nested = format!("{IMPLICIT_PREFIX}{scout}");
        followers.opened("b".into(), &nested);
        let inner = SessionId::new();
        followers.event("b", attributed("inner", &inner, 1, text("x")));
        let actions = followers.backfilled(&(nested.clone(), "inner".into()), Vec::new());
        assert_eq!(
            scoped(&actions),
            [(
                vec!["scout".to_string(), "inner".to_string()],
                "x".to_string()
            )]
        );
        // A stream that ends may be opened again.
        assert!(!followers.should_open(&nested));
        followers.ended("b");
        assert!(followers.should_open(&nested));
        assert!(!followers.owns("b"));
    }

    /// Another agent's helpers are its own clients': its implicit mob is
    /// not opened, and an agent in the daemon's mob is no helper.
    #[test]
    fn another_agents_helpers_are_not_followed() {
        let owner = SessionId::new();
        let other = SessionId::new();
        let mut followers = Followers::default();
        followers.reset(owner.clone());
        assert!(followers.should_open(MOB_ID));
        assert!(!followers.should_open(&format!("{IMPLICIT_PREFIX}{other}")));
        assert!(followers.should_open(&format!("{IMPLICIT_PREFIX}{owner}")));
        followers.opened("tui".into(), MOB_ID);
        assert!(
            followers
                .event("tui", attributed("main-ab12", &other, 1, text("x")))
                .is_empty()
        );
        assert!(
            followers
                .member(MOB_ID, "main-ab12", Some(other))
                .is_empty()
        );
    }
}
