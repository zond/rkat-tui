//! `/wire` and `/unwire`: the client arranges a wire between this client's
//! agent and a top-level agent of another workspace's daemon, both sides.
//!
//! The client holds the user's authority on both daemons: it finds the
//! other daemons as `--list` does (their endpoint files), attaches to one
//! as a probe (not counted as a client, its end no detach), asks each
//! daemon for its agent's card (`rkat/wire_card`) and each to wire its
//! agent to the other's card (`rkat/wire`), the other side first. When
//! this side then fails, the other side is undone (`rkat/unwire`) and the
//! user is told which side failed and why. Running `/wire` is the user's
//! consent to both directions: neither daemon asks again.
//!
//! Neither daemon trusts the other daemon: each trusts one named agent's
//! comms key. A conversation switch changes an agent's key, so after one
//! this client sends the agent's new card to every daemon it is wired to
//! ([`refresh`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::ClientError;
use super::remote;
use super::transport::{ATTACH_TIMEOUT, Connection};
use crate::daemon::endpoint::Endpoint;
use crate::daemon::protocol::{
    METHOD_NOT_FOUND, Method, RunWord, Status, UnwireParams, WireAgentParams, WireCard, WireParams,
    WireRow, method, wire_label, workspace_name,
};

/// How long another daemon may take to answer its status.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one wire call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Another workspace's running daemon.
#[derive(Debug, Clone)]
pub struct Other {
    /// Its endpoint.
    pub endpoint: Endpoint,
    /// Its status (its workspace and agents).
    pub status: Status,
}

/// One top-level agent of another workspace, as the `/wire` picker
/// lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Its workspace.
    pub workspace: PathBuf,
    /// Its name.
    pub agent: String,
    /// Whether it runs.
    pub run: RunWord,
    /// How many clients are attached to it.
    pub clients: usize,
}

impl Candidate {
    /// `<workspace name>/<agent>`.
    #[must_use]
    pub fn label(&self) -> String {
        wire_label(&self.workspace, &self.agent)
    }
}

/// Every other workspace's daemon among `files` that answers (never
/// `own`, the workspace of this client's daemon), asked all at once: a
/// stale endpoint file costs one [`PROBE_TIMEOUT`] in all, not one each.
/// Two files of one workspace: the first one's daemon.
pub async fn others(files: &[PathBuf], own: &Path) -> Vec<Other> {
    let probes = files.iter().filter_map(|file| {
        let endpoint = Endpoint::read(file).ok()?;
        (endpoint.workspace.as_deref() != Some(own)).then_some(async move {
            let status = super::address::status_of(&endpoint, PROBE_TIMEOUT)
                .await
                .ok()?;
            Some(Other { endpoint, status })
        })
    });
    let mut found: Vec<Other> = Vec::new();
    for other in futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
    {
        if other.status.workspace != own
            && !found
                .iter()
                .any(|o| o.status.workspace == other.status.workspace)
        {
            found.push(other);
        }
    }
    found
}

/// The agents of `others`, workspaces and names in order.
#[must_use]
pub fn candidates(others: &[Other]) -> Vec<Candidate> {
    let mut all: Vec<Candidate> = others
        .iter()
        .flat_map(|other| {
            other.status.agents.iter().map(|row| Candidate {
                workspace: other.status.workspace.clone(),
                agent: row.name.clone(),
                run: row.run(),
                clients: row.clients,
            })
        })
        .collect();
    all.sort_by(|a, b| (&a.workspace, &a.agent).cmp(&(&b.workspace, &b.agent)));
    all
}

/// The candidate `target` names among `all`: `<workspace>/<agent>` (the
/// workspace by its name or its path), or a workspace alone when one agent
/// works there.
///
/// # Errors
///
/// What `target` matches instead, in words: nothing (with what there is),
/// or more than one.
pub fn resolve<'a>(target: &str, all: &'a [Candidate]) -> Result<&'a Candidate, String> {
    if all.is_empty() {
        return Err(format!(
            "no other workspace's daemon runs, so nothing is named `{target}`: start rkat-tui in the other workspace first"
        ));
    }
    find_named(target, all, |c| (&c.workspace, &c.agent), Candidate::label)
}

/// `target` read as `<workspace>/<agent>` (the agent a valid name), else
/// as a workspace alone.
#[must_use]
pub fn parse_target(target: &str) -> (&str, Option<&str>) {
    match target.rsplit_once('/') {
        Some((workspace, agent)) if crate::daemon::agents::is_agent(agent) => {
            (workspace, Some(agent))
        }
        _ => (target, None),
    }
}

/// The one item of `all` that `target` names ([`parse_target`]; the
/// workspace by its name or its path), `key` giving each item's workspace
/// and agent and `label` its name in words.
///
/// # Errors
///
/// What `target` matched instead, in words: none (with what there is), or
/// more than one.
fn find_named<'a, T>(
    target: &str,
    all: &'a [T],
    key: impl Fn(&T) -> (&Path, &str),
    label: impl Fn(&T) -> String,
) -> Result<&'a T, String> {
    let (workspace, agent) = parse_target(target);
    let matched: Vec<&T> = all
        .iter()
        .filter(|item| {
            let (w, a) = key(item);
            (workspace_name(w) == workspace || w == Path::new(workspace))
                && agent.is_none_or(|agent| a == agent)
        })
        .collect();
    let labels =
        |items: &mut dyn Iterator<Item = &T>| items.map(&label).collect::<Vec<_>>().join(", ");
    match matched.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no agent of another workspace is named `{target}`; there are: {}",
            labels(&mut all.iter())
        )),
        several => Err(format!(
            "`{target}` names {} agents: {} (name one as <workspace>/<agent>)",
            several.len(),
            labels(&mut several.iter().copied())
        )),
    }
}

/// Why a wire failed, in the user's words.
fn words(side: &str, err: &ClientError) -> String {
    match err {
        ClientError::Rpc { code, .. } if *code == METHOD_NOT_FOUND => format!(
            "{side} is an older rkat-tui-mob without cross-workspace wires: stop it (/quit there) and start rkat-tui there again with this version"
        ),
        ClientError::Rpc { message, .. } => format!("{side} refused: {message}"),
        other => format!("{side} did not answer: {other}"),
    }
}

async fn bounded<T>(
    call: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| ClientError::Timeout("waiting for a daemon's wire answer"))?
}

/// One daemon of a wire and how its failures are named (`this
/// workspace's daemon`, `the daemon of <workspace>`).
struct Side<'c> {
    name: String,
    connection: &'c Connection,
}

impl<'c> Side<'c> {
    /// This client's own daemon.
    fn own(connection: &'c Connection) -> Self {
        Self {
            name: "this workspace's daemon".to_string(),
            connection,
        }
    }

    /// One bounded call of `M`, its failure in words ([`words`]).
    async fn call<M: Method>(&self, params: &M::Params) -> Result<M::Result, String> {
        bounded(remote::call::<M>(self.connection, params))
            .await
            .map_err(|err| words(&self.name, &err))
    }
}

/// The name of the daemon of `workspace`, as failures say it.
fn daemon_of(workspace: &Path) -> String {
    format!("the daemon of {}", workspace.display())
}

/// A probe connection to `endpoint` (not counted as a client), its
/// failure in words as `name`'s.
async fn probe(endpoint: &Endpoint, name: &str) -> Result<Connection, String> {
    Connection::attach_as(endpoint, ATTACH_TIMEOUT, None, true)
        .await
        .map(|(connection, _notes)| connection)
        .map_err(|err| words(name, &err))
}

/// Wire this client's agent (`own`, on its daemon's connection) to
/// `peer` of the daemon `other`: the other side first, then this one,
/// the other undone when this one fails. The text says what was done.
///
/// # Errors
///
/// Which side failed and why.
pub async fn wire(own: &Connection, other: &Endpoint, peer: &str) -> Result<String, String> {
    let here = Side::own(own);
    let ours = here
        .call::<method::Card>(&WireAgentParams::default())
        .await?;
    let name = format!("the daemon of {}", other_name(other));
    let connection = probe(other, &name).await?;
    let there = Side {
        name,
        connection: &connection,
    };
    let peer_params = |card: WireCard| WireParams {
        agent: Some(peer.to_string()),
        card,
    };
    let theirs = there
        .call::<method::Card>(&WireAgentParams {
            agent: Some(peer.to_string()),
        })
        .await?;
    let wired_there = there
        .call::<method::Wire>(&peer_params(ours.clone()))
        .await
        .map_err(|why| format!("nothing was wired: {why}"))?;
    let wired_here = match here
        .call::<method::Wire>(&WireParams {
            agent: None,
            card: theirs.clone(),
        })
        .await
    {
        Ok(wired) => wired,
        Err(why) if wired_there.already => {
            return Err(format!(
                "{why}; the other side was wired already and stays so"
            ));
        }
        Err(why) => {
            let undone = there
                .call::<method::Unwire>(&UnwireParams {
                    agent: Some(peer.to_string()),
                    workspace: ours.workspace.clone(),
                    peer: ours.agent.clone(),
                })
                .await;
            return Err(match undone {
                Ok(_) => format!("{why}; the other side's half was undone, so nothing is wired"),
                Err(undo) => format!(
                    "{why}; undoing the other side failed too ({undo}): /unwire {} removes it",
                    theirs.label()
                ),
            });
        }
    };
    Ok(if wired_here.already && wired_there.already {
        format!(
            "{} and {} are wired already: /unwire {} removes it",
            ours.label(),
            theirs.label(),
            theirs.label()
        )
    } else {
        format!(
            "wired {} and {}: each can message the other (send_message), and a message to an idle one starts a model turn there; /unwire {} removes it",
            ours.label(),
            theirs.label(),
            theirs.label()
        )
    })
}

/// The name of `endpoint`'s workspace (its address when it names none).
fn other_name(endpoint: &Endpoint) -> String {
    endpoint
        .workspace
        .as_ref()
        .map_or_else(|| endpoint.address.to_string(), |w| w.display().to_string())
}

/// Remove this client's agent's wire `row`: this side always, the other
/// side when its daemon runs (found among `others`). The text says what
/// was done.
///
/// # Errors
///
/// This side's failure.
pub async fn unwire(
    own: &Connection,
    own_agent: &str,
    own_workspace: &Path,
    row: &WireRow,
    others: &[Other],
) -> Result<String, String> {
    let label = row.label();
    let here = Side::own(own)
        .call::<method::Unwire>(&UnwireParams {
            agent: None,
            workspace: row.workspace.clone(),
            peer: row.agent.clone(),
        })
        .await?;
    let mut text = if here.removed {
        format!("unwired {label}")
    } else {
        format!("this agent had no wire to {label}")
    };
    match others.iter().find(|o| o.status.workspace == row.workspace) {
        None => text.push_str(&format!(
            "; the daemon of {} is not running, so its side is removed when you run /unwire there or that agent retires (it can no longer reach this one either way)",
            row.workspace.display()
        )),
        Some(other) => {
            let params = UnwireParams {
                agent: Some(row.agent.clone()),
                workspace: own_workspace.to_path_buf(),
                peer: own_agent.to_string(),
            };
            match call_other::<method::Unwire>(&other.endpoint, &row.workspace, &params).await {
                Ok(_) => text.push_str(" on both sides"),
                Err(why) => text.push_str(&format!("; its side stays: {why}")),
            }
        }
    }
    Ok(text)
}

/// After a conversation switch (which gives the agent a new comms key):
/// send this agent's new card to the other side of each of its wires
/// `rows` whose daemon runs among `others`. One line per wire that could
/// not be refreshed.
pub async fn refresh(own: &Connection, rows: &[WireRow], others: &[Other]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ours = match Side::own(own)
        .call::<method::Card>(&WireAgentParams::default())
        .await
    {
        Ok(card) => card,
        Err(why) => {
            return vec![format!(
                "the cross-workspace wires may be stale after the switch: {why}"
            )];
        }
    };
    let mut stale = Vec::new();
    for row in rows {
        let label = row.label();
        let Some(other) = others.iter().find(|o| o.status.workspace == row.workspace) else {
            stale.push(format!(
                "{label} does not know this agent's new conversation (its daemon is not running): /wire {label} again once rkat-tui runs there"
            ));
            continue;
        };
        let params = WireParams {
            agent: Some(row.agent.clone()),
            card: ours.clone(),
        };
        if let Err(why) = call_other::<method::Wire>(&other.endpoint, &row.workspace, &params).await
        {
            stale.push(format!(
                "{label} does not know this agent's new conversation: {why}; /wire {label} again"
            ));
        }
    }
    stale
}

/// One bounded call of `M` on the daemon of `workspace` at `endpoint`,
/// through a probe connection of its own; the failure in words.
async fn call_other<M: Method>(
    endpoint: &Endpoint,
    workspace: &Path,
    params: &M::Params,
) -> Result<M::Result, String> {
    let name = daemon_of(workspace);
    let connection = probe(endpoint, &name).await?;
    Side {
        name,
        connection: &connection,
    }
    .call::<M>(params)
    .await
}

/// This client's agent's wires, from `status`.
#[must_use]
pub fn rows_of(status: &Status) -> Vec<WireRow> {
    status
        .agent
        .as_ref()
        .and_then(|agent| status.agents.iter().find(|row| &row.name == agent))
        .map(|row| row.wires.clone())
        .unwrap_or_default()
}

/// The `/wire` picker: a row per candidate; Enter wires to it (an agent
/// wired already says so). `wired`: this agent's wires.
#[must_use]
pub fn wire_picker(all: &[Candidate], wired: &[WireRow]) -> crate::ui::ListPicker {
    use crate::ui::{ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use ratatui::crossterm::event::KeyCode;
    let rows = all
        .iter()
        .map(|c| {
            let is_wired = wired
                .iter()
                .any(|w| w.workspace == c.workspace && w.agent == c.agent);
            let clients = if c.clients == 0 {
                "no client".to_string()
            } else {
                crate::text::count(c.clients, "client")
            };
            let text = format!(
                "{}  {}  {clients}{}",
                c.label(),
                c.run.picker_word(),
                if is_wired { "  · wired" } else { "" }
            );
            let enter = if is_wired {
                PickKey::refused(
                    KeyCode::Enter,
                    format!(
                        "this agent is wired to {} already: /unwire removes it",
                        c.label()
                    ),
                )
            } else {
                PickKey::now(
                    KeyCode::Enter,
                    PickAct::Wire {
                        workspace: c.workspace.clone(),
                        agent: c.agent.clone(),
                    },
                )
            };
            PickRow::new(text, vec![enter]).noted(c.workspace.display().to_string())
        })
        .collect();
    ListPicker::new(
        PickerKind::Wire,
        rows,
        "no other workspace's daemon runs: start rkat-tui in the other workspace, then /wire again",
        "↑/↓ move · Enter wire this agent to it (both ways) · Esc close",
    )
}

/// The `/unwire` picker: a row per wire of this agent (`running`: whether
/// the other workspace's daemon runs); Enter removes it after a
/// confirmation.
#[must_use]
pub fn unwire_picker(wired: &[WireRow], others: &[Other]) -> crate::ui::ListPicker {
    use crate::ui::{ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use ratatui::crossterm::event::KeyCode;
    let rows = wired
        .iter()
        .map(|w| {
            let other = others.iter().find(|o| o.status.workspace == w.workspace);
            let state = match other {
                None => "its daemon is not running".to_string(),
                Some(o) => match o.status.agents.iter().find(|a| a.name == w.agent) {
                    None => "retired there (the wire is stale)".to_string(),
                    Some(a) => a.run().picker_word().to_string(),
                },
            };
            let act = PickAct::Unwire {
                workspace: w.workspace.clone(),
                agent: w.agent.clone(),
            };
            PickRow::new(
                format!("{}  {state}", w.label()),
                vec![PickKey::confirmed(
                    KeyCode::Enter,
                    act,
                    format!("unwire {}? messages between the two stop", w.label()),
                )],
            )
            .noted(w.workspace.display().to_string())
        })
        .collect();
    ListPicker::new(
        PickerKind::Unwire,
        rows,
        "this agent has no wires to other workspaces (/wire makes one)",
        "↑/↓ move · Enter unwire · Esc close",
    )
}

/// The wire of `wired` that `target` names (`<workspace>/<agent>`, the
/// workspace by name or path, or a workspace alone with one wire there).
///
/// # Errors
///
/// What `target` matches instead.
pub fn resolve_wire<'a>(target: &str, wired: &'a [WireRow]) -> Result<&'a WireRow, String> {
    if wired.is_empty() {
        return Err("this agent has no wires to other workspaces".to_string());
    }
    find_named(target, wired, |w| (&w.workspace, &w.agent), WireRow::label)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn c(workspace: &str, agent: &str) -> Candidate {
        Candidate {
            workspace: PathBuf::from(workspace),
            agent: agent.to_string(),
            run: RunWord::Idle,
            clients: 0,
        }
    }

    #[test]
    fn a_target_is_a_workspace_and_agent_or_a_workspace_with_one_agent() {
        let all = vec![
            c("/w/api", "main-aaaa"),
            c("/w/api", "main-bbbb"),
            c("/w/web", "main-cccc"),
        ];
        assert_eq!(resolve("api/main-bbbb", &all).unwrap().agent, "main-bbbb");
        assert_eq!(
            resolve("/w/api/main-aaaa", &all).unwrap().agent,
            "main-aaaa"
        );
        assert_eq!(resolve("web", &all).unwrap().agent, "main-cccc");
        assert_eq!(resolve("/w/web", &all).unwrap().agent, "main-cccc");
        let several = resolve("api", &all).unwrap_err();
        assert!(
            several.contains("names 2 agents") && several.contains("api/main-aaaa, api/main-bbbb"),
            "{several}"
        );
        let none = resolve("db/main-aaaa", &all).unwrap_err();
        assert!(none.contains("there are: api/main-aaaa"), "{none}");
        let nothing = resolve("api", &[]).unwrap_err();
        assert!(
            nothing.contains("no other workspace's daemon runs"),
            "{nothing}"
        );
    }

    #[test]
    fn the_wire_picker_rows_name_the_workspace_agent_state_and_clients() {
        let mut busy = c("/w/api", "main-aaaa");
        busy.run = RunWord::Running;
        busy.clients = 1;
        let all = vec![busy, c("/w/web", "main-cccc")];
        let wired = vec![WireRow {
            workspace: "/w/web".into(),
            agent: "main-cccc".into(),
        }];
        let picker = wire_picker(&all, &wired);
        let texts: Vec<&str> = picker.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "api/main-aaaa  working  1 client",
                "web/main-cccc  idle  no client  · wired"
            ]
        );
        assert_eq!(picker.rows[0].note.as_deref(), Some("/w/api"));
        assert!(matches!(
            &picker.rows[0].keys[0].act,
            Ok(crate::ui::PickAct::Wire { agent, .. }) if agent == "main-aaaa"
        ));
        assert!(matches!(&picker.rows[1].keys[0].act, Err(why) if why.contains("already")));
        assert!(
            wire_picker(&[], &[])
                .empty
                .contains("no other workspace's daemon runs")
        );
    }

    #[test]
    fn the_unwire_picker_says_whether_the_other_side_runs() {
        let wired = vec![WireRow {
            workspace: "/w/web".into(),
            agent: "main-cccc".into(),
        }];
        let picker = unwire_picker(&wired, &[]);
        assert_eq!(
            picker.rows[0].text,
            "web/main-cccc  its daemon is not running"
        );
        assert!(picker.rows[0].keys[0].confirm.is_some());
        assert_eq!(resolve_wire("web", &wired).unwrap().agent, "main-cccc");
        assert!(resolve_wire("web", &[]).is_err());
        // The other daemon runs, but the agent retired there: stale.
        let status: Status = serde_json::from_value(serde_json::json!({
            "workspace": "/w/web", "pid": 1, "started_at": "2026-11-13T00:00:00Z",
            "clients": 0, "detached": false, "owner": false, "running": false,
            "agents": [{"name": "main-dddd", "clients": 0, "detached": false}],
        }))
        .unwrap();
        let other = Other {
            endpoint: Endpoint {
                address: "127.0.0.1:9".parse().unwrap(),
                token: String::new(),
                pid: 1,
                workspace: Some("/w/web".into()),
                started_at: None,
            },
            status,
        };
        let picker = unwire_picker(&wired, &[other]);
        assert_eq!(
            picker.rows[0].text,
            "web/main-cccc  retired there (the wire is stale)"
        );
    }

    /// Endpoint files whose daemons never answer cost one probe bound in
    /// all, not one each (review 5, CW2): each listener takes the
    /// connection and says nothing.
    #[tokio::test]
    async fn the_other_daemons_are_probed_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        let mut listeners = Vec::new();
        for i in 0..3 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let file = dir.path().join(format!("{i}.endpoint"));
            Endpoint {
                address: listener.local_addr().unwrap(),
                token: "t".into(),
                pid: 1,
                workspace: Some(format!("/w/silent-{i}").into()),
                started_at: None,
            }
            .write(&file)
            .unwrap();
            files.push(file);
            listeners.push(listener);
        }
        let started = std::time::Instant::now();
        assert!(others(&files, Path::new("/w/own")).await.is_empty());
        let took = started.elapsed();
        assert!(took < PROBE_TIMEOUT * 2, "{took:?}");
    }

    /// A daemon without the methods is told apart, and says what to do.
    #[test]
    fn an_older_daemon_is_named_with_what_to_do() {
        let old = ClientError::Rpc {
            method: "rkat/wire_card".into(),
            code: METHOD_NOT_FOUND,
            message: "unknown method".into(),
        };
        let said = words("the daemon of /w/api", &old);
        assert!(
            said.contains("older rkat-tui-mob without cross-workspace wires")
                && said.contains("start rkat-tui there again with this version"),
            "{said}"
        );
        let refused = ClientError::Rpc {
            method: "rkat/wire".into(),
            code: crate::daemon::protocol::WIRE_REFUSED,
            message: "nope".into(),
        };
        assert_eq!(words("x", &refused), "x refused: nope");
    }
}
