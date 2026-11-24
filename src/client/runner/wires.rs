//! `/wire` in the runner: the picker (this agent's wires, then the agents
//! it can wire to), the typed forms, a wire's removal from the picker, and
//! the refresh of the other sides after a conversation switch
//! ([`crate::client::wires`] does the calls).

use std::path::{Path, PathBuf};

use super::commands::daemon_status;
use super::*;
use crate::client::wires::{self as cross, Other};
use crate::daemon::protocol::workspace_name;

/// What is said when the caller's daemon cannot say what it is.
const NO_STATUS: &str = "the daemon did not say which agent this is";

/// What a wire command works with: this daemon's status, and the other
/// daemons when the command asks for them ([`Scene::others`]).
struct Scene {
    status: protocol::Status,
    files: Vec<PathBuf>,
}

impl Scene {
    /// The other workspaces' daemons that answer now.
    async fn others(&self) -> Vec<Other> {
        cross::others(&self.files, &self.status.workspace).await
    }
}

/// The `/wire` picker as the daemon's `status` and the other daemons
/// `others` have it now; `remote`: this client's daemon is on another
/// machine, so no agent of this machine is offered (its wires still are,
/// to remove).
fn picker_of(status: &protocol::Status, others: &[Other], remote: bool) -> crate::ui::ListPicker {
    let all = if remote {
        Vec::new()
    } else {
        cross::candidates(others)
    };
    cross::wire_picker(&all, &cross::rows_of(status), others)
}

impl Runner {
    /// The endpoint files `/wire` looks through.
    fn wire_files(&self) -> Vec<PathBuf> {
        self.config
            .wire_files
            .clone()
            .unwrap_or_else(|| crate::client::list::files(None))
    }

    /// A client attached to a daemon on another machine: `/wire` wires
    /// daemons of one machine (their sockets are local) and says so.
    fn wire_refused(&self, what: &str) -> bool {
        let Some(why) = &self.config.remote else {
            return false;
        };
        self.notice(
            NoticeLevel::Warn,
            format!(
                "{what}: wires join daemons of one machine through local sockets, and this daemon is not on this one ({why}); run rkat-tui on the daemon's machine to wire its agents"
            ),
        );
        true
    }

    /// Run `act` on a task of its own with this client's connection and
    /// its daemon's status; without a connection, or a status, it says so
    /// (`what` was not done) unless `quiet`.
    fn with_scene<F, Fut>(&self, what: &str, quiet: bool, act: F)
    where
        F: FnOnce(Say, Arc<Connection>, Scene) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let connection = if quiet {
            self.connection()
        } else {
            self.link_or_say(what)
        };
        let Some(connection) = connection else {
            return;
        };
        let files = self.wire_files();
        let say = self.say();
        tokio::spawn(async move {
            let Some(status) = daemon_status(&connection).await else {
                if !quiet {
                    say.notice(NoticeLevel::Warn, NO_STATUS);
                }
                return;
            };
            act(say, connection, Scene { status, files }).await;
        });
    }

    /// `/wire`: the picker of other workspaces' agents, or wire to the one
    /// `target` names.
    pub(super) fn wire(&self, target: Option<String>) {
        if let Some(text) = target.as_deref().filter(|t| t.starts_with(SSH_SCHEME)) {
            let parsed = parse_ssh(text);
            self.with_scene(
                "nothing was wired",
                false,
                |say, connection, _scene| async move {
                    match parsed {
                        Ok(params) => wire_over_ssh(&say, &connection, params).await,
                        Err(why) => {
                            say.notice(NoticeLevel::Warn, format!("nothing was wired: {why}"));
                        }
                    }
                },
            );
            return;
        }
        let remote = self.config.remote.is_some();
        // Attached from another machine the picker still lists this
        // agent's wires, to remove; nothing can be wired from here.
        if target.is_some() && self.wire_refused("nothing was wired") {
            return;
        }
        self.with_scene("nothing was wired", false, move |say, connection, scene| async move {
            let others = scene.others().await;
            let Some(target) = target else {
                show(&say, picker_of(&scene.status, &others, remote), false);
                return;
            };
            let all = cross::candidates(&others);
            if names_own(&target, &scene.status.workspace) {
                say.notice(
                    NoticeLevel::Warn,
                    format!(
                        "`{target}` is this workspace's: its agents are wired to each other already (/agents lists them)"
                    ),
                );
                return;
            }
            match cross::resolve(&target, &all) {
                Ok(found) => {
                    wire_with(&say, &connection, &others, &found.workspace, &found.agent).await;
                }
                Err(why) => say.notice(NoticeLevel::Warn, format!("nothing was wired: {why}")),
            }
        });
    }

    /// The `/wire` picker's choice; the picker stays, its row then marked
    /// wired.
    pub(super) fn wire_to(&self, workspace: PathBuf, agent: String) {
        if self.wire_refused("nothing was wired") {
            return;
        }
        self.with_scene(
            "nothing was wired",
            false,
            |say, connection, scene| async move {
                let others = scene.others().await;
                wire_with(&say, &connection, &others, &workspace, &agent).await;
                if let Some(status) = daemon_status(&connection).await {
                    show(&say, picker_of(&status, &others, false), true);
                }
            },
        );
    }

    /// The `/wire` picker's confirmed `d` on a wired row: remove the wire
    /// (both sides when the other daemon runs); the picker stays, the row
    /// then no longer wired.
    pub(super) fn remove_wire(&self, workspace: PathBuf, agent: String) {
        let remote = self.config.remote.is_some();
        self.with_scene(
            "the wire was not removed",
            false,
            move |say, connection, scene| async move {
                let others = scene.others().await;
                let row = cross::rows_of(&scene.status)
                    .into_iter()
                    .find(|row| row.workspace == workspace && row.agent == agent)
                    .unwrap_or(protocol::WireRow {
                        workspace,
                        agent,
                        host: None,
                        from: None,
                        link: None,
                    });
                let done = remove_with(&say, &connection, &scene.status, &row, &others).await;
                if let Some(status) = daemon_status(&connection).await {
                    show(&say, picker_of(&status, &others, remote), true);
                }
                match done {
                    Ok(text) => say.picker_done(crate::ui::PickerKind::Wire, Some(text)),
                    Err(why) => {
                        let _ = say.ui_tx.send(Msg::PickerRefused {
                            kind: crate::ui::PickerKind::Wire,
                            why,
                        });
                    }
                }
            },
        );
    }

    /// This agent switched conversation, which gave it a new comms key:
    /// the other side of each of its wires gets its new card; a wire that
    /// could not be told is a warning.
    pub(super) fn refresh_wires(&self) {
        if self.config.remote.is_some() {
            return;
        }
        self.with_scene("", true, |say, connection, scene| async move {
            let rows = cross::rows_of(&scene.status);
            if rows.is_empty() {
                return;
            }
            let others = scene.others().await;
            for line in cross::refresh(&connection, &rows, &others).await {
                say.notice(NoticeLevel::Warn, line);
            }
        });
    }
}

/// How `/wire` names an agent on another machine.
const SSH_SCHEME: &str = "ssh://";

/// The flag of `/wire ssh://…` that names the remote rkat-tui.
const REMOTE_PROGRAM_FLAG: &str = "--remote-rkat-tui";

/// How long the daemon may take to make a wire over ssh (a few ssh
/// calls and the link).
const SSH_WIRE_BOUND: std::time::Duration = std::time::Duration::from_secs(150);

/// `ssh://<host>[/<workspace>/<agent>] [--remote-rkat-tui <path>]` as the
/// daemon's parameters (no target: list the agents there).
fn parse_ssh(text: &str) -> Result<protocol::WireSshParams, String> {
    let mut words = text.split_whitespace();
    let url = words.next().unwrap_or_default();
    let program = match (words.next(), words.next(), words.next()) {
        (None, ..) => None,
        (Some(REMOTE_PROGRAM_FLAG), Some(path), None) => Some(path.to_string()),
        _ => {
            return Err(format!(
                "write it as /wire ssh://<host>/<workspace>/<agent> [{REMOTE_PROGRAM_FLAG} <path>]"
            ));
        }
    };
    let rest = url.strip_prefix(SSH_SCHEME).unwrap_or(url);
    let (host, target) = match rest.split_once('/') {
        Some((host, "")) => (host, None),
        Some((host, target)) => (host, Some(target.to_string())),
        None => (rest, None),
    };
    if host.is_empty() {
        return Err(format!("{text} names no host"));
    }
    Ok(protocol::WireSshParams {
        host: host.to_string(),
        target,
        program,
    })
}

/// `/wire ssh://…` through this client's daemon, which runs ssh.
async fn wire_over_ssh(say: &Say, connection: &Connection, params: protocol::WireSshParams) {
    let host = params.host.clone();
    let listing = params.target.is_none();
    let call = crate::client::remote::call::<protocol::method::WireSsh>(connection, &params);
    let answer = match tokio::time::timeout(SSH_WIRE_BOUND, call).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(crate::client::ClientError::Rpc { message, .. })) => {
            say.notice(NoticeLevel::Warn, format!("nothing was wired: {message}"));
            return;
        }
        Ok(Err(err)) => {
            say.notice(
                NoticeLevel::Warn,
                format!("nothing was wired: this workspace's daemon did not answer: {err}"),
            );
            return;
        }
        Err(_) => {
            say.notice(
                NoticeLevel::Warn,
                format!("this workspace's daemon took longer than {} s with the wire to {host}; /agents shows whether it was made", SSH_WIRE_BOUND.as_secs()),
            );
            return;
        }
    };
    if listing {
        let text = if answer.agents.is_empty() {
            format!(
                "no rkat-tui daemon runs on {host}: start rkat-tui in the workspace there, then /wire ssh://{host}/<workspace>/<agent>"
            )
        } else {
            let rows: Vec<String> = answer
                .agents
                .iter()
                .map(|a| {
                    format!(
                        "{} ({}, {}, {})",
                        protocol::wire_label(&a.workspace, &a.agent),
                        a.run,
                        crate::text::count(a.clients, "client"),
                        a.workspace.display()
                    )
                })
                .collect();
            format!(
                "agents on {host}: {}; /wire ssh://{host}/<workspace>/<agent> wires one",
                rows.join("; ")
            )
        };
        say.notice(NoticeLevel::Info, text);
        return;
    }
    say.notice(NoticeLevel::Info, answer.text.unwrap_or_default());
}

/// Show `picker` (`refresh`: only in place of an open one of its kind).
fn show(say: &Say, picker: crate::ui::ListPicker, refresh: bool) {
    let _ = say.ui_tx.send(Msg::ShowPicker { picker, refresh });
}

/// Whether `target` names the workspace `own` (by its name or its path,
/// alone or before `/<agent>`).
fn names_own(target: &str, own: &Path) -> bool {
    let (workspace, _) = cross::parse_target(target);
    workspace == workspace_name(own) || Path::new(workspace) == own
}

async fn wire_with(
    say: &Say,
    connection: &Connection,
    others: &[Other],
    workspace: &Path,
    agent: &str,
) {
    let Some(other) = others.iter().find(|o| o.status.workspace == workspace) else {
        say.notice(
            NoticeLevel::Warn,
            format!(
                "nothing was wired: the daemon of {} is not running; start rkat-tui there, then /wire again",
                workspace.display()
            ),
        );
        return;
    };
    match cross::wire(connection, &other.endpoint, agent).await {
        Ok(text) => say.notice(NoticeLevel::Info, text),
        Err(why) => say.notice(NoticeLevel::Warn, why),
    }
}

async fn remove_with(
    say: &Say,
    connection: &Connection,
    status: &protocol::Status,
    row: &protocol::WireRow,
    others: &[Other],
) -> Result<String, String> {
    let Some(agent) = status.agent.as_deref() else {
        say.notice(NoticeLevel::Warn, NO_STATUS);
        return Err(NO_STATUS.to_string());
    };
    match cross::remove_wire(connection, agent, &status.workspace, row, others).await {
        Ok(text) => {
            say.notice(NoticeLevel::Info, text.clone());
            Ok(text)
        }
        Err(why) => {
            let why = format!("the wire was not removed: {why}");
            say.notice(NoticeLevel::Warn, why.clone());
            Err(why)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_ssh_target_is_a_host_an_agent_there_and_the_remote_program() {
        let p = parse_ssh("ssh://me@box/api/main-ab12").unwrap();
        assert_eq!(
            (p.host.as_str(), p.target.as_deref(), p.program),
            ("me@box", Some("api/main-ab12"), None)
        );
        let p =
            parse_ssh("ssh://box//home/u/api/main-ab12 --remote-rkat-tui /opt/rkat-tui").unwrap();
        assert_eq!(p.target.as_deref(), Some("/home/u/api/main-ab12"));
        assert_eq!(p.program.as_deref(), Some("/opt/rkat-tui"));
        for listing in ["ssh://box", "ssh://box/"] {
            let p = parse_ssh(listing).unwrap();
            assert_eq!((p.host.as_str(), p.target), ("box", None));
        }
        assert!(
            parse_ssh("ssh:///api")
                .unwrap_err()
                .contains("names no host")
        );
        assert!(
            parse_ssh("ssh://box/api --other x")
                .unwrap_err()
                .contains("--remote-rkat-tui <path>")
        );
    }

    #[test]
    fn a_target_in_the_own_workspace_is_told_apart() {
        let own = Path::new("/w/api");
        assert!(names_own("api", own));
        assert!(names_own("api/main-ab12", own));
        assert!(names_own("/w/api/main-ab12", own));
        assert!(!names_own("web/main-ab12", own));
        assert!(!names_own("web", own));
    }
}
