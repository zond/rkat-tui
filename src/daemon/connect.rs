//! `connect_helpers` in the daemon: wire two or more of the caller's own
//! helpers to each other, both ways.
//!
//! - **Whose.** Only the caller's own helpers: the members its own spawns
//!   seated (`assign` in the workspace's mob,
//!   `mob_spawn_member` in a mob it created), by the names those spawns
//!   gave them, as the helper bindings hold them ([`own_helpers`]): another
//!   agent's helper, a helper's helper, the workspace's agents and the
//!   caller itself are refused, each saying what to do.
//! - **How.** The engine refuses an agent's own `wire_members` in the
//!   workspace's mob (lukacf/meerkat#2233), so the host wires them, as it
//!   wires a fork to its parent (`mob_wire_members_batch`; an edge that
//!   exists stays). An edge joins two members of one mob; two helpers in
//!   different mobs are connected through the daemon's sockets
//!   ([`super::links`]: the engine's in-process transport does not reach
//!   across mobs).
//! - **Told.** Each helper with a new edge is told once who it can reach
//!   now and how, in a system note it reads at its next step (the note
//!   wakes no one).
//! - **Life.** The engine keeps a mob's edges across a restart and drops a
//!   member's edges when it retires (an `assign` helper retires when it
//!   finishes); a pair across mobs ends the same way, by the daemon
//!   ([`super::links`]).

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
        return "you have no running helper now (start them with `assign`; `contacts` lists whom you can message)".to_string();
    }
    let names: Vec<String> = own.iter().map(|c| format!("`{}`", c.name)).collect();
    format!(
        "your running helpers are {} (`contacts` says what each is)",
        names.join(", ")
    )
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
    let names: Vec<String> = peers.iter().map(|(name, _)| format!("`{name}`")).collect();
    format!(
        "`{parent}`, who started you, connected you with its other helper{s} {}: you can message {them} with `send_message` (`contacts` gives the peer_id), and {they} can message you. Do so when your task needs what {they} know or do; each message wakes its receiver, so keep it to what is needed.",
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
    links: OnceLock<Arc<super::links::Links>>,
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
            links: OnceLock::new(),
        }
    }

    /// Wire them through `state`, and helpers in different mobs through
    /// `links` (once; they are made after the tools).
    pub fn bind(&self, state: &Arc<MobMcpState>, links: &Arc<super::links::Links>) {
        let _ = self.state.set(Arc::downgrade(state));
        let _ = self.links.set(Arc::clone(links));
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

    /// Retire the caller's own helper `name` (any kind; its run is
    /// cancelled, and the helpers it started, theirs too, are stopped and
    /// retired before it, which the engine's retire of a member does not
    /// do): the answer, naming those, or why not. Nobody is told: the caller
    /// asked. The engine's `retire_member` cannot do this for a fresh
    /// helper (it lets a member retire only the members whose spawn it
    /// made inside its own turn, which only `fork_off` records).
    ///
    /// # Errors
    ///
    /// Every refusal, in words a model can act on.
    pub async fn retire(&self, caller: Option<&SessionId>, name: &str) -> Result<String, String> {
        let not_here = "`retire_helper` is not available to this caller (it is no agent or helper of this workspace's daemon)";
        let (caller, self_name) = caller.and_then(|s| self.caller(s)).ok_or(not_here)?;
        let state = self
            .state
            .get()
            .and_then(Weak::upgrade)
            .ok_or("the daemon is stopping: nothing was retired")?;
        let bound = self.registry.bound_members();
        let own = own_helpers(&caller, &bound);
        let name = name.trim();
        let matches: Vec<&Chosen> = own
            .iter()
            .filter(|c| c.name == name || format!("{}/{}", c.key.mob, c.name) == name)
            .collect();
        let helper = match matches.as_slice() {
            [one] => (*one).clone(),
            [] if name == self_name => {
                return Err(format!(
                    "`{name}` is you: name a helper you started; {}",
                    listed(&own)
                ));
            }
            [] => {
                return Err(format!(
                    "`{name}` is no running helper of yours (one that finished or was retired is gone already); {}",
                    listed(&own)
                ));
            }
            _ => {
                return Err(format!(
                    "`{name}` names helpers in more than one mob: name it as `<mob>/{name}`"
                ));
            }
        };
        state.handle_for(&helper.key.mob).await.map_err(|err| {
            format!("the mob of `{name}` cannot be reached, so it was not retired: {err}")
        })?;
        let Some(retiring) = self.registry.endings().retirements().begin() else {
            return Err("the daemon is stopping: nothing was retired".to_string());
        };
        // Ended with its parent's word: no note of it. Its own helpers go
        // first, deepest first: none stays seated without its owner. The
        // work runs on a task of its own, so a call cancelled meanwhile (an
        // interrupt) still finishes it; owed until the engine retired it,
        // so a stop before that leaves it to the next start.
        let registry = Arc::clone(&self.registry);
        let key = helper.key.clone();
        let work = tokio::spawn(async move {
            let _retiring = retiring;
            let endings = registry.endings();
            let below = endings.take_below(&key);
            endings.left(&key);
            registry.owe_retire(&key);
            let outcome = endings.retire_now(&key, &below).await;
            let labels: Vec<String> = below
                .iter()
                .map(|(_, tag)| tag.label().to_string())
                .collect();
            (outcome, labels)
        });
        let (outcome, labels) = work
            .await
            .map_err(|err| format!("the retire of `{name}` did not finish: {err}"))?;
        let with = crate::mobs::endings::retired_with(&labels);
        match outcome {
            Ok(()) => Ok(match with {
                Some(with) => format!("Helper `{}` was retired. {with}", helper.name),
                None => format!("Helper `{}` was retired.", helper.name),
            }),
            Err(why) => Err(format!(
                "Helper `{}` could not be retired ({why}): it is still seated and `contacts` lists it. Call `retire_helper` for it again; if it still fails, it is retired when the daemon next starts.{}",
                helper.name,
                with.map_or_else(String::new, |with| format!(" {with}"))
            )),
        }
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
        let helpers = choose(names, &own, &self_name, &agents, &elsewhere)?;
        // The comms names the helpers see each other under, by mob.
        let mut by_mob: BTreeMap<MobId, Vec<Chosen>> = BTreeMap::new();
        for helper in &helpers {
            by_mob
                .entry(helper.key.mob.clone())
                .or_default()
                .push(helper.clone());
        }
        let mut roles: BTreeMap<HelperKey, String> = BTreeMap::new();
        for mob in by_mob.keys() {
            let handle = state.handle_for(mob).await.map_err(|err| {
                format!(
                    "the mob `{mob}` of the helpers cannot be reached, so nothing was connected: {err}"
                )
            })?;
            for member in handle.list_members().await {
                roles.insert(
                    HelperKey::new(mob.clone(), member.agent_identity),
                    member.role.to_string(),
                );
            }
        }
        let peer_name = |key: &HelperKey| {
            roles
                .get(key)
                .and_then(|role| {
                    meerkat_core::MemberCommsName::new(key.mob.as_str(), role, key.member.as_str())
                        .ok()
                })
                .map_or_else(|| key.to_string(), |name| name.to_string())
        };
        // Within one mob, edges of that mob; across mobs, the daemon's
        // sockets (`super::links`). The pairs new now:
        let mut new: Vec<(HelperKey, HelperKey)> = Vec::new();
        for (mob, members) in &by_mob {
            let mut edges = Vec::new();
            for (i, a) in members.iter().enumerate() {
                for b in &members[i + 1..] {
                    edges.push((a.key.member.clone(), b.key.member.clone()));
                }
            }
            if edges.is_empty() {
                continue;
            }
            let report = wire_when_seated(&state, mob, edges).await?;
            new.extend(report.wired.into_iter().map(|edge| {
                (
                    HelperKey::new(mob.clone(), edge.a),
                    HelperKey::new(mob.clone(), edge.b),
                )
            }));
        }
        let crossing: Vec<(&Chosen, &Chosen)> = helpers
            .iter()
            .enumerate()
            .flat_map(|(i, a)| helpers[i + 1..].iter().map(move |b| (a, b)))
            .filter(|(a, b)| a.key.mob != b.key.mob)
            .collect();
        if !crossing.is_empty() {
            let links = self
                .links
                .get()
                .ok_or("the daemon is stopping: nothing more was connected")?;
            let end = |helper: &Chosen| super::links::End {
                key: helper.key.clone(),
                peer: peer_name(&helper.key),
                name: helper.name.clone(),
            };
            for (a, b) in crossing {
                let already = links.link(end(a), end(b)).await.map_err(|why| {
                    let made: Vec<String> = new
                        .iter()
                        .map(|(x, y)| format!("`{x}` and `{y}`"))
                        .collect();
                    format!(
                        "`{}` and `{}` (in different mobs) were not connected: {why}{}",
                        a.name,
                        b.name,
                        if made.is_empty() {
                            String::new()
                        } else {
                            format!("; connected before that: {}", made.join(", "))
                        }
                    )
                })?;
                if !already {
                    new.push((a.key.clone(), b.key.clone()));
                }
            }
        }
        let name_of = |key: &HelperKey| {
            helpers
                .iter()
                .find(|h| &h.key == key)
                .map_or_else(|| key.to_string(), |h| h.name.clone())
        };
        let mut lines = Vec::new();
        for (i, a) in helpers.iter().enumerate() {
            for b in &helpers[i + 1..] {
                let fresh = new
                    .iter()
                    .any(|(x, y)| (x == &a.key && y == &b.key) || (x == &b.key && y == &a.key));
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
                    if x == &helper.key {
                        Some(y)
                    } else if y == &helper.key {
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
        Ok(format!(
            "Connected, each pair both ways: {}. Each can now message the others with `send_message`; each newly connected helper was told whom it can reach. A pair stays connected until one of them leaves: it is retired or stopped, an `assign` helper finishes, or its mob is destroyed (`contacts` shows who is connected to whom).",
            lines.join(", "),
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
            kind: crate::mobs::HelperKind::Started(
                crate::mobs::HelperContext::Fresh,
                crate::mobs::HelperLife::Single,
            ),
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
        assert!(note.contains("helper `b`: you can message it"), "{note}");
        assert!(note.contains("`contacts` gives the peer_id"), "{note}");
        assert!(
            note.contains("send_message") && note.contains("wakes"),
            "{note}"
        );
    }
}
