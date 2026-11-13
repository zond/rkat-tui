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
    METHOD_NOT_FOUND, Status, UnwireParams, WireAgentParams, WireParams, WireRow, method,
    wire_label, workspace_name,
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
    /// `running`, `idle` or `unknown`.
    pub run: &'static str,
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
/// `own`, the workspace of this client's daemon).
pub async fn others(files: &[PathBuf], own: &Path) -> Vec<Other> {
    let mut found: Vec<Other> = Vec::new();
    for file in files {
        let Ok(endpoint) = Endpoint::read(file) else {
            continue;
        };
        if endpoint.workspace.as_deref() == Some(own) {
            continue;
        }
        let Ok(status) = super::address::status_of(&endpoint, PROBE_TIMEOUT).await else {
            continue;
        };
        if status.workspace == own || found.iter().any(|o| o.status.workspace == status.workspace) {
            continue;
        }
        found.push(Other { endpoint, status });
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
                run: row.run_label(),
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
    let names_workspace =
        |c: &Candidate, w: &str| workspace_name(&c.workspace) == w || c.workspace == Path::new(w);
    let matches: Vec<&Candidate> = match target.rsplit_once('/') {
        Some((workspace, agent)) if crate::daemon::agents::is_agent(agent) => all
            .iter()
            .filter(|c| c.agent == agent && names_workspace(c, workspace))
            .collect(),
        _ => all.iter().filter(|c| names_workspace(c, target)).collect(),
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] if all.is_empty() => Err(format!(
            "no other workspace's daemon runs, so nothing is named `{target}`: start rkat-tui in the other workspace first"
        )),
        [] => Err(format!(
            "no agent of another workspace is named `{target}`; there are: {}",
            all.iter()
                .map(Candidate::label)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        several => Err(format!(
            "`{target}` names {} agents: {} (name one as <workspace>/<agent>)",
            several.len(),
            several
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
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

/// A probe connection to `endpoint`.
async fn probe(endpoint: &Endpoint) -> Result<Connection, ClientError> {
    let (connection, _notes) = Connection::attach_as(endpoint, ATTACH_TIMEOUT, None, true).await?;
    Ok(connection)
}

/// Wire this client's agent (`own`, on its daemon's connection) to
/// `peer` of the daemon `other`: the other side first, then this one,
/// the other undone when this one fails. The text says what was done.
///
/// # Errors
///
/// Which side failed and why.
pub async fn wire(own: &Connection, other: &Endpoint, peer: &str) -> Result<String, String> {
    let ours = bounded(remote::call::<method::Card>(
        own,
        &WireAgentParams::default(),
    ))
    .await
    .map_err(|err| words("this workspace's daemon", &err))?;
    let theirs_side = format!("the daemon of {}", other_name(other));
    let remote = probe(other)
        .await
        .map_err(|err| words(&theirs_side, &err))?;
    let theirs = bounded(remote::call::<method::Card>(
        &remote,
        &WireAgentParams {
            agent: Some(peer.to_string()),
        },
    ))
    .await
    .map_err(|err| words(&theirs_side, &err))?;
    let there = bounded(remote::call::<method::Wire>(
        &remote,
        &WireParams {
            agent: Some(peer.to_string()),
            card: ours.clone(),
        },
    ))
    .await
    .map_err(|err| format!("nothing was wired: {}", words(&theirs_side, &err)))?;
    let here = bounded(remote::call::<method::Wire>(
        own,
        &WireParams {
            agent: None,
            card: theirs.clone(),
        },
    ))
    .await;
    let here = match here {
        Ok(here) => here,
        Err(err) => {
            let why = words("this workspace's daemon", &err);
            if there.already {
                return Err(format!(
                    "{why}; the other side was wired already and stays so"
                ));
            }
            let undone = bounded(remote::call::<method::Unwire>(
                &remote,
                &UnwireParams {
                    agent: Some(peer.to_string()),
                    workspace: ours.workspace.clone(),
                    peer: ours.agent.clone(),
                },
            ))
            .await;
            return Err(match undone {
                Ok(_) => format!("{why}; the other side's half was undone, so nothing is wired"),
                Err(err) => format!(
                    "{why}; undoing the other side failed too ({}): /unwire {} removes it",
                    words(&theirs_side, &err),
                    theirs.label()
                ),
            });
        }
    };
    Ok(if here.already && there.already {
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
    let here = bounded(remote::call::<method::Unwire>(
        own,
        &UnwireParams {
            agent: None,
            workspace: row.workspace.clone(),
            peer: row.agent.clone(),
        },
    ))
    .await
    .map_err(|err| words("this workspace's daemon", &err))?;
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
            let there = match probe(&other.endpoint).await {
                Ok(remote) => bounded(remote::call::<method::Unwire>(
                    &remote,
                    &UnwireParams {
                        agent: Some(row.agent.clone()),
                        workspace: own_workspace.to_path_buf(),
                        peer: own_agent.to_string(),
                    },
                ))
                .await
                .map(|_| ()),
                Err(err) => Err(err),
            };
            match there {
                Ok(()) => text.push_str(" on both sides"),
                Err(err) => text.push_str(&format!(
                    "; its side stays: {}",
                    words(&format!("the daemon of {}", row.workspace.display()), &err)
                )),
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
    let ours = match bounded(remote::call::<method::Card>(
        own,
        &WireAgentParams::default(),
    ))
    .await
    {
        Ok(card) => card,
        Err(err) => {
            return vec![format!(
                "the cross-workspace wires may be stale after the switch: {}",
                words("this workspace's daemon", &err)
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
        let sent = match probe(&other.endpoint).await {
            Ok(remote) => bounded(remote::call::<method::Wire>(
                &remote,
                &WireParams {
                    agent: Some(row.agent.clone()),
                    card: ours.clone(),
                },
            ))
            .await
            .map(|_| ()),
            Err(err) => Err(err),
        };
        if let Err(err) = sent {
            stale.push(format!(
                "{label} does not know this agent's new conversation: {}; /wire {label} again",
                words(&format!("the daemon of {}", row.workspace.display()), &err)
            ));
        }
    }
    stale
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
            let state = match c.run {
                "running" => "working",
                other => other,
            };
            let text = format!(
                "{}  {state}  {clients}{}",
                c.label(),
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
                    Some(a) => match a.run_label() {
                        "running" => "working".to_string(),
                        other => other.to_string(),
                    },
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
    let all: Vec<Candidate> = wired
        .iter()
        .map(|w| Candidate {
            workspace: w.workspace.clone(),
            agent: w.agent.clone(),
            run: "unknown",
            clients: 0,
        })
        .collect();
    if all.is_empty() {
        return Err("this agent has no wires to other workspaces".to_string());
    }
    let found = resolve(target, &all)?;
    wired
        .iter()
        .find(|w| w.workspace == found.workspace && w.agent == found.agent)
        .ok_or_else(|| format!("no wire to {target}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn c(workspace: &str, agent: &str) -> Candidate {
        Candidate {
            workspace: PathBuf::from(workspace),
            agent: agent.to_string(),
            run: "idle",
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
        busy.run = "running";
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
