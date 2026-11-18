//! `contacts`: who an agent or a helper can talk to, and what each is.
//!
//! - **Who.** Exactly the peers its own comms runtime can send to (the
//!   engine's peer directory of its session), each with the `peer_id`
//!   `send_message` takes; nothing it cannot reach is listed as reachable.
//! - **What.** Each peer sorted by the daemon's own state: the caller's
//!   parent (a helper's), its own helpers (the seated ones of its tree one
//!   level below it, [`crate::mobs::seated_for`], reachable or not yet),
//!   the helpers its parent connected it with, the workspace's agents (as
//!   the roster note words their states) and the agents of other
//!   workspaces it is wired to (`/wire`). A helper's kind, whether it runs
//!   or since when it is idle, the first line of its task and whom it was
//!   connected to come from the registry.
//! - **Used by** the tool itself, and by the line the agent reads after a
//!   compaction ([`Contacts::held_line`]).

use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use meerkat_core::SessionId;
use meerkat_mob::AgentIdentity;
use meerkat_mob_mcp::MobMcpState;

use crate::mobs::{HelperKey, HelperRegistry, HelperTag};

/// The tool's name.
pub const CONTACTS: &str = "contacts";

/// The tool's description.
pub const DESCRIPTION: &str = "List everyone you can message now and what each is: you, your \
     parent (for a helper), your helpers (kind, running or idle, task, whom they are connected \
     to), helpers you were connected with, this workspace's agents and agents of other \
     workspaces you are wired to; each with the peer_id send_message takes.";

/// What the workspace's agents and the wires say, for [`Contacts`]: the
/// live agents (bound once they exist).
#[async_trait]
pub trait Roster: Send + Sync {
    /// Every live agent of the workspace and its state as the roster note
    /// words it (`working`, `idle, a client attached`, …).
    async fn agent_states(&self) -> Vec<(String, &'static str)>;
    /// The agents of other workspaces `agent` is wired to: label,
    /// workspace and peer id.
    fn wired(&self, agent: &str) -> Vec<(String, std::path::PathBuf, String)>;
}

/// One helper the caller holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// Its name.
    pub name: String,
    /// Its kind's words (`fresh · kept`), when known.
    pub kind: Option<String>,
    /// Whether it runs, and since when (`None`: no run of it seen since
    /// the daemon started).
    pub activity: Option<(bool, Instant)>,
    /// The first line of its task, when kept.
    pub task: Option<String>,
    /// The caller's other helpers it was connected to.
    pub connected: Vec<String>,
    /// Its peer id, when the caller can reach it now.
    pub peer: Option<String>,
}

impl Held {
    /// `running`, `idle for 2 h`, `idle`.
    fn state(&self, now: Instant) -> String {
        match self.activity {
            Some((true, _)) => "running".to_string(),
            Some((false, since)) => format!(
                "idle for {}",
                crate::text::span(now.saturating_duration_since(since).as_secs(), 1)
            ),
            None => "idle".to_string(),
        }
    }

    /// `a (fresh · kept, idle for 2 h)`, as the reminder after a
    /// compaction names it.
    #[cfg(any(test, feature = "test-support"))]
    fn short(&self, now: Instant) -> String {
        match &self.kind {
            Some(kind) => format!("{} ({kind}, {})", self.name, self.state(now)),
            None => format!("{} ({})", self.name, self.state(now)),
        }
    }
}

/// One peer of the caller that is no helper of its own: its name, what it
/// is, and its peer id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The name to call it by.
    pub name: String,
    /// What it is (`whose helper`, an agent's state, a wire's workspace).
    pub what: String,
    /// Its peer id.
    pub peer: String,
}

/// Everything `contacts` answers, by group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// The caller's own name.
    pub you: String,
    /// The caller's own kind, for a helper.
    pub you_kind: Option<String>,
    /// A helper's parent.
    pub parent: Option<Peer>,
    /// The helpers the caller holds.
    pub helpers: Vec<Held>,
    /// How many of its helpers ended since the daemon started.
    pub ended: usize,
    /// Whether it may start helpers at all.
    pub may_hold: bool,
    /// Helpers its parent connected it with.
    pub connected_with: Vec<Peer>,
    /// The workspace's other agents.
    pub agents: Vec<Peer>,
    /// The agents of other workspaces it is wired to.
    pub wired: Vec<Peer>,
    /// Any other peer it can message (a member of a mob it made).
    pub others: Vec<Peer>,
}

impl View {
    /// Every peer id the view names.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn peer_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .parent
            .iter()
            .chain(&self.connected_with)
            .chain(&self.agents)
            .chain(&self.wired)
            .chain(&self.others)
            .map(|peer| peer.peer.clone())
            .chain(self.helpers.iter().filter_map(|held| held.peer.clone()))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// The answer, as the model reads it: one compact JSON object, its
    /// groups in a fixed order and left out when empty, every entry with
    /// the `peer_id` `send_message` takes.
    #[must_use]
    pub fn render(&self, now: Instant) -> String {
        let you = match &self.you_kind {
            Some(kind) => format!("{} (a helper, {kind})", self.you),
            None => format!("{} (an agent of this workspace)", self.you),
        };
        let helpers_note = match (self.helpers.is_empty(), self.ended) {
            _ if !self.may_hold && self.helpers.is_empty() && self.ended == 0 => None,
            (true, 0) => Some("you hold no helpers now".to_string()),
            (true, n) => Some(format!(
                "you hold no helpers now; {} ended in this conversation",
                crate::text::count(n, "helper")
            )),
            (false, n) => Some(format!(
                "{n} of your helpers have ended in this conversation"
            )),
        };
        let answer = Answer {
            you,
            parent: self.parent.as_ref().map(Entry::of),
            your_helpers: self
                .helpers
                .iter()
                .map(|held| HeldEntry {
                    name: &held.name,
                    kind: held.kind.as_deref(),
                    state: held.state(now),
                    task: held.task.as_deref(),
                    connected_to: &held.connected,
                    peer_id: held.peer.as_deref(),
                    unreachable: held.peer.is_none().then_some("not reachable yet"),
                })
                .collect(),
            helpers_note,
            connected_with: self.connected_with.iter().map(Entry::of).collect(),
            agents_of_this_workspace: self.agents.iter().map(Entry::of).collect(),
            wired_agents: self.wired.iter().map(Entry::of).collect(),
            others: self.others.iter().map(Entry::of).collect(),
        };
        serde_json::to_string(&answer).unwrap_or_default()
    }
}

/// [`View::render`]'s object.
#[derive(serde::Serialize)]
struct Answer<'a> {
    you: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<Entry<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    your_helpers: Vec<HeldEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    helpers_note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    connected_with: Vec<Entry<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    agents_of_this_workspace: Vec<Entry<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    wired_agents: Vec<Entry<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    others: Vec<Entry<'a>>,
}

/// One peer in [`Answer`].
#[derive(serde::Serialize)]
struct Entry<'a> {
    name: &'a str,
    what: &'a str,
    peer_id: &'a str,
}

impl<'a> Entry<'a> {
    fn of(peer: &'a Peer) -> Self {
        Self {
            name: &peer.name,
            what: &peer.what,
            peer_id: &peer.peer,
        }
    }
}

/// One held helper in [`Answer`].
#[derive(serde::Serialize)]
struct HeldEntry<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    connected_to: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unreachable: Option<&'static str>,
}

/// The daemon's `contacts`.
pub struct Contacts {
    registry: Arc<HelperRegistry>,
    state: OnceLock<Weak<MobMcpState>>,
    links: OnceLock<Weak<super::links::Links>>,
    roster: OnceLock<Weak<dyn Roster>>,
    /// The deepest a helper may be (a helper at it holds none).
    max_depth: u8,
}

impl std::fmt::Debug for Contacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contacts").finish_non_exhaustive()
    }
}

impl Contacts {
    /// The contacts of the members `registry` binds; helpers nest at most
    /// `max_depth` deep.
    #[must_use]
    pub fn new(registry: Arc<HelperRegistry>, max_depth: u8) -> Self {
        Self {
            registry,
            state: OnceLock::new(),
            links: OnceLock::new(),
            roster: OnceLock::new(),
            max_depth,
        }
    }

    /// Read the members through `state` and the connections across mobs
    /// from `links` (once; they are made after the tools).
    pub fn bind(&self, state: &Arc<MobMcpState>, links: &Arc<super::links::Links>) {
        let _ = self.state.set(Arc::downgrade(state));
        let _ = self.links.set(Arc::downgrade(links));
    }

    /// Read the agents and the wires from `roster` (once; the agents are
    /// made after the mob).
    pub fn bind_roster(&self, roster: Weak<dyn Roster>) {
        let _ = self.roster.set(roster);
    }

    fn state(&self) -> Option<Arc<MobMcpState>> {
        self.state.get().and_then(Weak::upgrade)
    }

    /// The caller running as `session`: the agent's session its tree
    /// works for, its own path (empty for an agent), its name and its tag
    /// (a helper's).
    fn caller(
        &self,
        session: &SessionId,
    ) -> Option<(SessionId, Vec<String>, String, Option<HelperTag>)> {
        if let Some(root) = self.registry.root_of_session(session) {
            return Some((session.clone(), Vec::new(), root.member.to_string(), None));
        }
        let key = self.registry.member_key_of_session(session)?;
        let tag = self
            .registry
            .bound_members()
            .into_iter()
            .find_map(|(bound, tag)| (bound == key).then_some(tag))?;
        Some((
            tag.owner.clone(),
            tag.path.clone(),
            tag.label().to_string(),
            Some(tag),
        ))
    }

    /// The helpers held by the member at `path` of `owner`'s tree (empty:
    /// the agent itself): the seated ones one level below it, by mob and
    /// name.
    pub async fn held(&self, owner: &SessionId, path: &[String]) -> Vec<(HelperKey, Held)> {
        let Some(state) = self.state() else {
            return Vec::new();
        };
        let seated = crate::mobs::seated_for(&state, &self.registry, owner).await;
        let own: Vec<(HelperKey, HelperTag)> = seated
            .into_iter()
            .filter(|(_, tag)| tag.path.len() == path.len() + 1 && tag.path.starts_with(path))
            .collect();
        // Whom each is wired to in its mob (the parent's other helpers
        // among them were connected to it).
        let mut wired: std::collections::HashMap<HelperKey, Vec<AgentIdentity>> =
            std::collections::HashMap::new();
        let mut mobs: Vec<meerkat_mob::MobId> =
            own.iter().map(|(key, _)| key.mob.clone()).collect();
        mobs.sort();
        mobs.dedup();
        for mob in mobs {
            if let Ok(handle) = state.handle_for(&mob).await {
                for member in handle.list_members().await {
                    wired.insert(
                        HelperKey::new(mob.clone(), member.agent_identity),
                        member.wired_to.into_iter().collect(),
                    );
                }
            }
        }
        let links = self
            .links
            .get()
            .and_then(Weak::upgrade)
            .map(|links| links.links())
            .unwrap_or_default();
        own.iter()
            .map(|(key, tag)| {
                let mut connected: Vec<String> = own
                    .iter()
                    .filter(|(other, _)| other != key)
                    .filter(|(other, _)| {
                        let in_mob = other.mob == key.mob
                            && wired.get(key).is_some_and(|to| to.contains(&other.member));
                        let across = links.iter().any(|link| {
                            let ends = [&link.ends[0].key, &link.ends[1].key];
                            ends.contains(&key) && ends.contains(&other)
                        });
                        in_mob || across
                    })
                    .map(|(_, other)| other.label().to_string())
                    .collect();
                connected.sort();
                let held = Held {
                    name: tag.label().to_string(),
                    kind: tag.kind.text(),
                    activity: self.registry.activity(key),
                    task: self.registry.task_of(key),
                    connected,
                    peer: None,
                };
                (key.clone(), held)
            })
            .collect()
    }

    /// The line an agent reads after a compaction when it holds helpers:
    /// who they are, and how to let them go (`None`: it holds none).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn held_line(&self, session: &SessionId) -> Option<String> {
        let (owner, path, _, _) = self.caller(session)?;
        let held = self.held(&owner, &path).await;
        if held.is_empty() {
            return None;
        }
        let now = Instant::now();
        let names: Vec<String> = held.iter().map(|(_, held)| held.short(now)).collect();
        Some(format!(
            "You still hold {}: {}. Retire with `retire_helper` those you no longer need; `{CONTACTS}` shows them.",
            crate::text::count(held.len(), "helper"),
            names.join(", ")
        ))
    }

    /// What `contacts` answers the member running as `session`.
    ///
    /// # Errors
    ///
    /// The member is no agent or helper here, or the daemon is stopping.
    pub async fn view(&self, session: &SessionId) -> Result<View, String> {
        let not_here = "`contacts` is not available to this caller (it is no agent or helper of this workspace's daemon)";
        let (owner, path, you, tag) = self.caller(session).ok_or(not_here)?;
        let state = self
            .state()
            .ok_or("the daemon is stopping: try again after its next start")?;
        let directory = match state.session_service().comms_runtime(session).await {
            Some(comms) => comms.peers().await,
            None => Vec::new(),
        };
        let mut helpers = self.held(&owner, &path).await;
        let agent = self
            .registry
            .root_of_session(&owner)
            .map(|root| root.member.to_string());
        let parent = path.len().checked_sub(1).map(|up| {
            path.get(..up)
                .and_then(<[String]>::last)
                .cloned()
                .or_else(|| agent.clone())
                .unwrap_or_default()
        });
        let roster = self.roster.get().and_then(Weak::upgrade);
        let states = match &roster {
            Some(roster) => roster.agent_states().await,
            None => Vec::new(),
        };
        let wires = match (&roster, &agent) {
            (Some(roster), Some(agent)) if path.is_empty() => roster.wired(agent),
            _ => Vec::new(),
        };
        // The caller's siblings: its parent's other helpers.
        let siblings: Vec<(HelperKey, HelperTag)> = match path.split_last() {
            Some((_, up)) => crate::mobs::seated_for(&state, &self.registry, &owner)
                .await
                .into_iter()
                .filter(|(_, other)| {
                    other.path.len() == path.len()
                        && other.path.starts_with(up)
                        && other.path != path
                })
                .collect(),
            None => Vec::new(),
        };
        let mut view = View {
            you,
            you_kind: tag
                .as_ref()
                .map(|tag| tag.kind.text().unwrap_or_else(|| "helper".to_string())),
            ended: self.registry.ended_count(&owner, &path),
            may_hold: path.len() < usize::from(self.max_depth),
            ..View::default()
        };
        for entry in directory {
            let id = entry.peer_id.to_string();
            let name = entry.name.as_str().to_string();
            if let Some((label, workspace, _)) = wires.iter().find(|(_, _, peer)| *peer == id) {
                view.wired.push(Peer {
                    name: label.clone(),
                    what: format!("workspace {}", workspace.display()),
                    peer: id,
                });
                continue;
            }
            let member = name
                .parse::<meerkat_core::MemberCommsName>()
                .map(|comms| comms.member().to_string())
                .unwrap_or_else(|_| name.clone());
            if let Some((_, held)) = helpers.iter_mut().find(|(_, held)| held.name == member) {
                held.peer = Some(id);
                continue;
            }
            if parent.as_deref() == Some(member.as_str()) {
                view.parent = Some(Peer {
                    name: member,
                    what: if path.len() == 1 {
                        "the agent that started you".to_string()
                    } else {
                        "the helper that started you".to_string()
                    },
                    peer: id,
                });
                continue;
            }
            if let Some((_, sibling)) = siblings.iter().find(|(_, other)| other.label() == member) {
                let whose = sibling
                    .parent_path()
                    .last()
                    .cloned()
                    .or_else(|| agent.clone())
                    .unwrap_or_default();
                view.connected_with.push(Peer {
                    name: member,
                    what: format!("{whose}'s helper"),
                    peer: id,
                });
                continue;
            }
            if super::agents::is_agent(&member) {
                let state = states
                    .iter()
                    .find(|(name, _)| *name == member)
                    .map_or("an agent of this workspace", |(_, state)| *state);
                view.agents.push(Peer {
                    name: member,
                    what: state.to_string(),
                    peer: id,
                });
                continue;
            }
            view.others.push(Peer {
                what: name.clone(),
                name: member,
                peer: id,
            });
        }
        view.helpers = helpers.into_iter().map(|(_, held)| held).collect();
        Ok(view)
    }

    /// `contacts` for the member running as `caller`: its answer, and
    /// whether that is an error.
    pub async fn answer(&self, caller: Option<&SessionId>) -> (String, bool) {
        let Some(caller) = caller else {
            return (
                "`contacts` is not available to this caller (it is no agent or helper of this workspace's daemon)".to_string(),
                true,
            );
        };
        match self.view(caller).await {
            Ok(view) => (view.render(Instant::now()), false),
            Err(why) => (why, true),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn peer(name: &str, what: &str, id: &str) -> Peer {
        Peer {
            name: name.to_string(),
            what: what.to_string(),
            peer: id.to_string(),
        }
    }

    /// The answer names each group once, leaves out the empty ones, and
    /// gives every peer its `peer_id`.
    #[test]
    fn the_answer_reads_by_group_with_each_peer_id() {
        let now = Instant::now();
        let view = View {
            you: "main-ab12".into(),
            helpers: vec![
                Held {
                    name: "scout".into(),
                    kind: Some("fresh · kept".into()),
                    activity: Some((
                        false,
                        now.checked_sub(std::time::Duration::from_hours(2)).unwrap(),
                    )),
                    task: Some("Find where the config file is parsed".into()),
                    connected: vec!["tester".into()],
                    peer: Some("P1".into()),
                },
                Held {
                    name: "tester".into(),
                    kind: Some("fork · single-use".into()),
                    activity: Some((true, now)),
                    task: None,
                    connected: vec!["scout".into()],
                    peer: Some("P2".into()),
                },
            ],
            ended: 3,
            may_hold: true,
            agents: vec![peer("main-cd34", "working", "P3")],
            wired: vec![peer("other/main-ef56", "workspace /src/other", "P4")],
            ..View::default()
        };
        assert_eq!(
            view.render(now),
            concat!(
                r#"{"you":"main-ab12 (an agent of this workspace)","#,
                r#""your_helpers":[{"name":"scout","kind":"fresh · kept","state":"idle for 2h","task":"Find where the config file is parsed","connected_to":["tester"],"peer_id":"P1"},"#,
                r#"{"name":"tester","kind":"fork · single-use","state":"running","connected_to":["scout"],"peer_id":"P2"}],"#,
                r#""helpers_note":"3 of your helpers have ended in this conversation","#,
                r#""agents_of_this_workspace":[{"name":"main-cd34","what":"working","peer_id":"P3"}],"#,
                r#""wired_agents":[{"name":"other/main-ef56","what":"workspace /src/other","peer_id":"P4"}]}"#,
            )
        );
        assert_eq!(view.peer_ids(), ["P1", "P2", "P3", "P4"]);
        let helper = View {
            you: "scout".into(),
            you_kind: Some("fresh · kept".into()),
            parent: Some(peer("main-ab12", "the agent that started you", "P0")),
            connected_with: vec![peer("tester", "main-ab12's helper", "P2")],
            ..View::default()
        };
        assert_eq!(
            helper.render(now),
            concat!(
                r#"{"you":"scout (a helper, fresh · kept)","#,
                r#""parent":{"name":"main-ab12","what":"the agent that started you","peer_id":"P0"},"#,
                r#""connected_with":[{"name":"tester","what":"main-ab12's helper","peer_id":"P2"}]}"#,
            )
        );
        // One that may start helpers and holds none says so.
        let alone = View {
            you: "main-ab12".into(),
            may_hold: true,
            ..View::default()
        };
        assert_eq!(
            alone.render(now),
            r#"{"you":"main-ab12 (an agent of this workspace)","helpers_note":"you hold no helpers now"}"#
        );
    }
}
