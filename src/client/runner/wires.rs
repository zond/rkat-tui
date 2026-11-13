//! `/wire` and `/unwire` in the runner: the pickers, the typed forms, and
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
        if self.wire_refused("nothing was wired") {
            return;
        }
        self.with_scene("nothing was wired", false, |say, connection, scene| async move {
            let others = scene.others().await;
            let all = cross::candidates(&others);
            let Some(target) = target else {
                show(&say, cross::wire_picker(&all, &cross::rows_of(&scene.status)), false);
                return;
            };
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
                    let all = cross::candidates(&others);
                    show(
                        &say,
                        cross::wire_picker(&all, &cross::rows_of(&status)),
                        true,
                    );
                }
            },
        );
    }

    /// `/unwire`: the picker of this agent's wires, or remove the one
    /// `target` names.
    pub(super) fn unwire(&self, target: Option<String>) {
        self.with_scene(
            "nothing was unwired",
            false,
            |say, connection, scene| async move {
                let rows = cross::rows_of(&scene.status);
                let others = scene.others().await;
                let Some(target) = target else {
                    show(&say, cross::unwire_picker(&rows, &others), false);
                    return;
                };
                match cross::resolve_wire(&target, &rows) {
                    Ok(row) => unwire_with(&say, &connection, &scene.status, row, &others).await,
                    Err(why) => {
                        say.notice(NoticeLevel::Warn, format!("nothing was unwired: {why}"));
                    }
                }
            },
        );
    }

    /// The `/unwire` picker's confirmed choice; the picker stays, the wire
    /// then gone from it.
    pub(super) fn unwire_from(&self, workspace: PathBuf, agent: String) {
        self.with_scene(
            "nothing was unwired",
            false,
            |say, connection, scene| async move {
                let others = scene.others().await;
                let row = protocol::WireRow { workspace, agent };
                unwire_with(&say, &connection, &scene.status, &row, &others).await;
                if let Some(status) = daemon_status(&connection).await {
                    show(
                        &say,
                        cross::unwire_picker(&cross::rows_of(&status), &others),
                        true,
                    );
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

async fn unwire_with(
    say: &Say,
    connection: &Connection,
    status: &protocol::Status,
    row: &protocol::WireRow,
    others: &[Other],
) {
    let Some(agent) = status.agent.as_deref() else {
        say.notice(NoticeLevel::Warn, NO_STATUS);
        return;
    };
    match cross::unwire(connection, agent, &status.workspace, row, others).await {
        Ok(text) => say.notice(NoticeLevel::Info, text),
        Err(why) => say.notice(NoticeLevel::Warn, format!("nothing was unwired: {why}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
