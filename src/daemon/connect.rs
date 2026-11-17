//! `connect_helpers` in the daemon: wire two or more of the caller's own
//! helpers to each other, both ways.
//!
//! - **Whose.** Only the caller's own helpers: the members its own spawns
//!   seated (`assign`, `fork_off`, `spawn_member` in the workspace's mob,
//!   `mob_spawn_member` in a mob it created), by the names those spawns
//!   gave them, as the helper bindings hold them ([`own_helpers`]): another
//!   agent's helper, a helper's helper, the workspace's agents and the
//!   caller itself are refused, each saying what to do.
//! - **How.** The engine refuses an agent's own `wire_members` in the
//!   workspace's mob (lukacf/meerkat#2233), so the host wires them, as it
//!   wires a fork to its parent (`mob_wire_members_batch`; an edge that
//!   exists stays). An edge joins two members of one mob; two helpers in
//!   different mobs are refused (`docs/evidence.md`, "Helpers connected by
//!   their parent").
//! - **Told.** Each helper with a new edge is told once who it can reach
//!   now and how, in a system note it reads at its next step (the note
//!   wakes no one).
//! - **Life.** The engine keeps a mob's edges across a restart and drops a
//!   member's edges when it retires (an `assign` helper retires when it
//!   finishes).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, MobError, MobId};
use meerkat_mob_mcp::MobMcpState;

use crate::mobs::{HelperKey, HelperRegistry, HelperTag};

/// The source of the notes a connection sends.
const NOTE_SOURCE: &str = "rkat-tui-connect";

/// How long a helper not yet seated in its mob (a fork whose spawn is
/// still building) is waited for.
const SEAT_WAIT: Duration = Duration::from_secs(12);
/// The first pause before trying again; each next doubles.
const SEAT_RECHECK_FIRST: Duration = Duration::from_millis(25);
/// The longest pause between two tries.
const SEAT_RECHECK_MAX: Duration = Duration::from_millis(500);

/// Who calls: the root of its tree and where in it it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The agent's session its tree works for.
    pub owner: SessionId,
    /// Its own place: empty for an agent, else its helper path.
    pub path: Vec<String>,
}

/// One of the caller's helpers, chosen by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    /// Its mob and member id.
    pub key: HelperKey,
    /// The name the caller knows it by.
    pub name: String,
}

/// The caller's own helpers among `bound`: one level below it in its
/// tree, by name.
#[must_use]
pub fn own_helpers(caller: &Caller, bound: &[(HelperKey, HelperTag)]) -> Vec<Chosen> {
    let mut own: Vec<Chosen> = bound
        .iter()
        .filter(|(_, tag)| {
            tag.owner == caller.owner
                && tag.path.len() == caller.path.len() + 1
                && tag.path.starts_with(&caller.path)
        })
        .map(|(key, tag)| Chosen {
            key: key.clone(),
            name: tag.label().to_string(),
        })
        .collect();
    own.sort_by(|a, b| a.name.cmp(&b.name));
    own
}

/// The names `own` holds, for a refusal.
fn listed(own: &[Chosen]) -> String {
    if own.is_empty() {
        return "you have no running helper now (start them with `assign`, `fork_off` or `spawn_member`)".to_string();
    }
    let names: Vec<String> = own.iter().map(|c| format!("`{}`", c.name)).collect();
    format!("your running helpers are {}", names.join(", "))
}

/// The helpers `names` choose among the caller's `own`, or why not, in
/// words a model can act on. `agents` are the workspace's agents, `self_name`
/// the caller's own name, and `elsewhere` every other bound helper's name
/// (someone else's).
///
/// # Errors
///
/// Fewer than two distinct names, or a name that is not one of `own`.
pub fn choose(
    names: &[String],
    own: &[Chosen],
    self_name: &str,
    agents: &[String],
    elsewhere: &[String],
) -> Result<Vec<Chosen>, String> {
    let mut distinct: Vec<&str> = Vec::new();
    for name in names {
        let name = name.trim();
        if !name.is_empty() && !distinct.contains(&name) {
            distinct.push(name);
        }
    }
    if distinct.len() < 2 {
        return Err(format!(
            "name two or more of your helpers to connect; {}",
            listed(own)
        ));
    }
    let mut chosen = Vec::new();
    let mut refused = Vec::new();
    for name in distinct {
        let matches: Vec<&Chosen> = own
            .iter()
            .filter(|c| c.name == name || format!("{}/{}", c.key.mob, c.name) == name)
            .collect();
        match matches.as_slice() {
            [one] => chosen.push((*one).clone()),
            [] if name == self_name => refused.push(format!(
                "`{name}` is you: name only helpers you started"
            )),
            [] if agents.iter().any(|agent| agent == name) || super::agents::is_agent(name) => refused.push(format!(
                "`{name}` is an agent of this workspace, not a helper of yours (the agents are wired to each other already)"
            )),
            [] if elsewhere.iter().any(|other| other == name) => refused.push(format!(
                "`{name}` is not a helper you started (it is another agent's or another helper's): you can connect only your own"
            )),
            [] => refused.push(format!(
                "`{name}` is no running helper of yours (a helper that finished or was retired takes no messages: start another)"
            )),
            _ => refused.push(format!(
                "`{name}` names helpers in more than one mob: name it as `<mob>/{name}`"
            )),
        }
    }
    if !refused.is_empty() {
        return Err(format!(
            "nothing was connected: {}; {}",
            refused.join("; "),
            listed(own)
        ));
    }
    Ok(chosen)
}

/// The note a newly connected helper reads: who it can reach now, why,
/// and how.
#[must_use]
pub fn connected_note(parent: &str, peers: &[(String, String)]) -> String {
    let names: Vec<String> = peers
        .iter()
        .map(|(name, peer)| format!("`{name}` (`{peer}` in `peers`)"))
        .collect();
    format!(
        "`{parent}`, who started you, connected you with its other helper{s} {}: you can message {them} with `send_message`, and {they} can message you. Do so when your task needs what {they} know or do; each message wakes its receiver, so keep it to what is needed.",
        names.join(", "),
        s = if peers.len() == 1 { "" } else { "s" },
        them = if peers.len() == 1 { "it" } else { "them" },
        they = if peers.len() == 1 { "it" } else { "they" },
    )
}

/// The daemon's `connect_helpers`.
pub struct Connector {
    registry: Arc<HelperRegistry>,
    state: OnceLock<Weak<MobMcpState>>,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connector").finish_non_exhaustive()
    }
}

impl Connector {
    /// Connect the helpers `registry` binds.
    #[must_use]
    pub fn new(registry: Arc<HelperRegistry>) -> Self {
        Self {
            registry,
            state: OnceLock::new(),
        }
    }

    /// Wire them through `state` (once; it is made after the tools).
    pub fn bind(&self, state: &Arc<MobMcpState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    /// The caller running as `session`, with its own name.
    fn caller(&self, session: &SessionId) -> Option<(Caller, String)> {
        if let Some(root) = self.registry.root_of_session(session) {
            return Some((
                Caller {
                    owner: session.clone(),
                    path: Vec::new(),
                },
                root.member.to_string(),
            ));
        }
        let key = self.registry.member_key_of_session(session)?;
        let tag = self
            .registry
            .bound_members()
            .into_iter()
            .find_map(|(bound, tag)| (bound == key).then_some(tag))?;
        let name = tag.label().to_string();
        Some((
            Caller {
                owner: tag.owner,
                path: tag.path,
            },
            name,
        ))
    }

    /// Connect `names` for the session `caller`: the answer, or why not.
    ///
    /// # Errors
    ///
    /// Every refusal, in words a model can act on.
    pub async fn connect(
        &self,
        caller: Option<&SessionId>,
        names: &[String],
    ) -> Result<String, String> {
        let not_here = "`connect_helpers` is not available to this caller (it is no agent or helper of this workspace's daemon)";
        let (caller, self_name) = caller.and_then(|s| self.caller(s)).ok_or(not_here)?;
        let state = self
            .state
            .get()
            .and_then(Weak::upgrade)
            .ok_or("the daemon is stopping: nothing was connected")?;
        let bound = self.registry.bound_members();
        let own = own_helpers(&caller, &bound);
        let elsewhere: Vec<String> = bound
            .iter()
            .map(|(_, tag)| tag.label().to_string())
            .collect();
        let agents: Vec<String> = bound
            .iter()
            .filter_map(|(_, tag)| self.registry.root_of_session(&tag.owner))
            .map(|root| root.member.to_string())
            .chain(std::iter::once(self_name.clone()).filter(|_| caller.path.is_empty()))
            .collect();
        let chosen = choose(names, &own, &self_name, &agents, &elsewhere)?;
        // By mob: an edge joins two members of one mob.
        let mut by_mob: BTreeMap<MobId, Vec<Chosen>> = BTreeMap::new();
        for helper in &chosen {
            by_mob
                .entry(helper.key.mob.clone())
                .or_default()
                .push(helper.clone());
        }
        if by_mob.len() > 1 {
            let groups: Vec<String> = by_mob
                .iter()
                .map(|(mob, helpers)| {
                    let names: Vec<String> =
                        helpers.iter().map(|h| format!("`{}`", h.name)).collect();
                    format!("{} in the mob `{mob}`", names.join(", "))
                })
                .collect();
            return Err(format!(
                "nothing was connected: these helpers are in different mobs ({}), and rkat-tui connects only helpers of one mob (an edge joins members of one mob). Start the helpers that must talk to each other in the same mob: all with `assign`, `fork_off` or `spawn_member` (your own mob), or all with `mob_spawn_member` in one mob you created",
                groups.join("; ")
            ));
        }
        let Some((mob, helpers)) = by_mob.into_iter().next() else {
            return Err(not_here.to_string());
        };
        let handle = state.handle_for(&mob).await.map_err(|err| {
            format!("the helpers' mob cannot be reached, so nothing was connected: {err}")
        })?;
        let mut edges = Vec::new();
        for (i, a) in helpers.iter().enumerate() {
            for b in &helpers[i + 1..] {
                edges.push((a.key.member.clone(), b.key.member.clone()));
            }
        }
        let report = wire_when_seated(&state, &mob, edges).await?;
        let new: Vec<(AgentIdentity, AgentIdentity)> = report
            .wired
            .into_iter()
            .map(|edge| (edge.a, edge.b))
            .collect();
        // The comms names the helpers see each other under.
        let roles: BTreeMap<AgentIdentity, String> = handle
            .list_members()
            .await
            .into_iter()
            .map(|member| (member.agent_identity, member.role.to_string()))
            .collect();
        let peer_name = |member: &AgentIdentity| {
            roles
                .get(member)
                .and_then(|role| {
                    meerkat_core::MemberCommsName::new(mob.as_str(), role, member.as_str()).ok()
                })
                .map_or_else(|| member.to_string(), |name| name.to_string())
        };
        let name_of = |member: &AgentIdentity| {
            helpers
                .iter()
                .find(|h| &h.key.member == member)
                .map_or_else(|| member.to_string(), |h| h.name.clone())
        };
        let mut lines = Vec::new();
        for (i, a) in helpers.iter().enumerate() {
            for b in &helpers[i + 1..] {
                let fresh = new.iter().any(|(x, y)| {
                    (x == &a.key.member && y == &b.key.member)
                        || (x == &b.key.member && y == &a.key.member)
                });
                lines.push(format!(
                    "`{}` and `{}`{}",
                    a.name,
                    b.name,
                    if fresh {
                        " (new)"
                    } else {
                        " (connected already)"
                    }
                ));
            }
        }
        // Each helper with a new edge is told once, of its new peers.
        for helper in &helpers {
            let peers: Vec<(String, String)> = new
                .iter()
                .filter_map(|(x, y)| {
                    if x == &helper.key.member {
                        Some(y)
                    } else if y == &helper.key.member {
                        Some(x)
                    } else {
                        None
                    }
                })
                .map(|peer| (name_of(peer), peer_name(peer)))
                .collect();
            if peers.is_empty() {
                continue;
            }
            let note = connected_note(&self_name, &peers);
            // Off the call: a note to a helper in the middle of a step
            // waits for that step's end, and the caller need not.
            let state = Arc::downgrade(&state);
            let key = helper.key.clone();
            tokio::spawn(async move {
                let Some(state) = state.upgrade() else {
                    return;
                };
                if let Err(err) =
                    super::agents::append_note_to(&state, &key.mob, &key.member, NOTE_SOURCE, note)
                        .await
                {
                    tracing::warn!(helper = %key, error = %err, "a connected helper was not told of its new peers");
                }
            });
        }
        let peers: Vec<String> = helpers
            .iter()
            .map(|h| format!("`{}` is `{}`", h.name, peer_name(&h.key.member)))
            .collect();
        Ok(format!(
            "Connected, each pair both ways: {}. Each can now message the others with `send_message` ({} in `peers`); each newly connected helper was told whom it can reach. They stay connected until one of them is retired (an `assign` helper is retired when it finishes).",
            lines.join(", "),
            peers.join(", ")
        ))
    }
}

/// Wire `edges` of `mob` once their members are seated: a fork whose
/// spawn is still building is listed in its mob only later, and the
/// engine then answers "member not found"; tried again within
/// [`SEAT_WAIT`].
async fn wire_when_seated(
    state: &MobMcpState,
    mob: &MobId,
    edges: Vec<(AgentIdentity, AgentIdentity)>,
) -> Result<meerkat_mob::MobWireMembersBatchReport, String> {
    let deadline = tokio::time::Instant::now() + SEAT_WAIT;
    let mut pause = SEAT_RECHECK_FIRST;
    loop {
        match state.mob_wire_members_batch(mob, edges.clone()).await {
            Ok(report) => return Ok(report),
            Err(MobError::MemberNotFound(member)) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(format!(
                        "nothing was connected: the helper `{member}` is not seated in its mob yet (a fork's start can take a few seconds); call `connect_helpers` again in a moment"
                    ));
                }
                tokio::time::sleep(pause.min(deadline - now)).await;
                pause = (pause * 2).min(SEAT_RECHECK_MAX);
            }
            Err(err) => return Err(format!("nothing was connected: {err}")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tag(owner: &SessionId, path: &[&str]) -> HelperTag {
        HelperTag {
            owner: owner.clone(),
            path: path.iter().map(ToString::to_string).collect(),
            model: "m".into(),
            kind: crate::mobs::HelperKind::Assigned,
        }
    }

    fn key(mob: &str, member: &str) -> HelperKey {
        HelperKey::new(MobId::from(mob), AgentIdentity::from(member))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    /// An agent's own helpers are the ones one level below it in its tree:
    /// not another agent's, not its helpers' helpers.
    #[test]
    fn own_helpers_are_one_level_below_the_caller() {
        let mine = SessionId::new();
        let theirs = SessionId::new();
        let bound = vec![
            (key("tui", "a"), tag(&mine, &["a"])),
            (key("tui", "b"), tag(&mine, &["b"])),
            (key("tui", "a-kid"), tag(&mine, &["a", "a-kid"])),
            (key("team", "c"), tag(&mine, &["c"])),
            (key("tui", "x"), tag(&theirs, &["x"])),
        ];
        let agent = Caller {
            owner: mine.clone(),
            path: Vec::new(),
        };
        let own: Vec<String> = own_helpers(&agent, &bound)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(own, names(&["a", "b", "c"]));
        // A helper's own are its children only.
        let helper = Caller {
            owner: mine,
            path: names(&["a"]),
        };
        let own: Vec<String> = own_helpers(&helper, &bound)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(own, names(&["a-kid"]));
    }

    /// Every refusal says why and lists the caller's helpers.
    #[test]
    fn a_name_that_is_not_the_caller_s_own_helper_is_refused_saying_why() {
        let mine = SessionId::new();
        let bound = vec![
            (key("tui", "a"), tag(&mine, &["a"])),
            (key("tui", "b"), tag(&mine, &["b"])),
        ];
        let own = own_helpers(
            &Caller {
                owner: mine,
                path: Vec::new(),
            },
            &bound,
        );
        let agents = names(&["main-ab12", "main-cd34"]);
        let elsewhere = names(&["a", "b", "x"]);
        let refuse = |list: &[&str]| {
            choose(&names(list), &own, "main-ab12", &agents, &elsewhere).unwrap_err()
        };
        let one = refuse(&["a", "a"]);
        assert!(
            one.contains("two or more") && one.contains("`a`, `b`"),
            "{one}"
        );
        let me = refuse(&["a", "main-ab12"]);
        assert!(me.contains("`main-ab12` is you"), "{me}");
        let agent = refuse(&["a", "main-cd34"]);
        assert!(agent.contains("an agent of this workspace"), "{agent}");
        let other = refuse(&["a", "x"]);
        assert!(other.contains("not a helper you started"), "{other}");
        let gone = refuse(&["a", "gone"]);
        assert!(gone.contains("finished or was retired"), "{gone}");
        assert!(gone.contains("your running helpers are `a`, `b`"), "{gone}");
        let chosen = choose(&names(&["b", "a"]), &own, "main-ab12", &agents, &elsewhere).unwrap();
        assert_eq!(chosen.len(), 2);
    }

    #[test]
    fn the_note_names_the_peers_the_parent_and_how() {
        let note = connected_note("main-ab12", &[("b".into(), "tui/main/b".into())]);
        assert!(note.contains("`main-ab12`, who started you"), "{note}");
        assert!(note.contains("`b` (`tui/main/b` in `peers`)"), "{note}");
        assert!(
            note.contains("send_message") && note.contains("wakes"),
            "{note}"
        );
    }
}
