//! Re-homing a workspace's realm: `/rehome` in the TUI (push: this
//! workspace's realm to another directory; pull: a realm to this workspace)
//! and `rkat-tui --rehome <id|old path>` from the shell (pull).
//!
//! One realm and one daemon per workspace stays the model. A re-home
//! rewrites one manifest's path ([`realm_home::commit`]); the workspace
//! root is composed into the daemon at its start (the confinement plan,
//! the file tools, the project layers, the endpoint key), so a daemon on a
//! re-homed realm is stopped and started again, never re-rooted live.
//!
//! [`prepare`] checks everything before anything changes: the new root
//! must not reach the realm ([`realm_home::check_reach`]); no turn runs on
//! a daemon the re-home would stop ([`RehomeError::Running`]); the
//! destination's realm, when it has one, holds nothing (no daemon with
//! another client, no conversation with a user turn, read from its daemon
//! or, with none, from its files; unreadable is not empty). A running
//! daemon on the realm that moves is no reason to refuse: [`carry_out`]
//! stops it (its agents are restored at the next start) and the replaced
//! realm's daemon, then commits once both realms are proven idle.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::launch::{self, Liveness};
use super::transport::Connection;
use crate::bootstrap::realm_home::{self, Home, HomeError, Reading, Way};
use crate::daemon::endpoint::Endpoint;
use crate::daemon::protocol::{self, ShutdownParams, Status};

/// How long a re-home waits for a daemon it asked to stop to release its
/// realm.
pub const STOP_WAIT: Duration = Duration::from_secs(15);

/// How long one question to a daemon may take.
const ASK: Duration = Duration::from_secs(15);

/// Where the daemon of a workspace writes its endpoint file (production:
/// the state directory's places, [`default_endpoint_files`]; tests place
/// theirs beside the workspace).
pub type EndpointFiles = Arc<dyn Fn(&Path) -> Vec<PathBuf> + Send + Sync>;

/// The endpoint files of a workspace's own realm's daemon.
#[must_use]
pub fn default_endpoint_files() -> EndpointFiles {
    Arc::new(|workspace: &Path| {
        crate::daemon::endpoint::candidates(workspace, &crate::daemon::endpoint::RealmKey::Default)
    })
}

/// Why a re-home was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum RehomeError {
    /// The realms refuse it.
    #[error(transparent)]
    Home(#[from] HomeError),
    /// A turn runs on a daemon the re-home would stop.
    #[error(
        "a turn is running on the daemon (pid {pid}) of {}: agent {agent}; interrupt it or wait for it to end, then re-home (nothing was changed)",
        workspace.display()
    )]
    Running {
        /// The daemon's pid.
        pid: u32,
        /// Its workspace.
        workspace: PathBuf,
        /// The agent whose turn runs.
        agent: String,
    },
    /// The destination's realm has a daemon with other clients.
    #[error(
        "the destination's realm {id} has a daemon (pid {pid}) with {clients} other client(s) attached; a re-home replaces only an empty realm, so nothing was changed"
    )]
    Busy {
        /// The realm.
        id: String,
        /// Its daemon's pid.
        pid: u32,
        /// The clients other than the one re-homing.
        clients: usize,
    },
    /// A daemon could not be asked: nothing is proven.
    #[error(
        "cannot ask the daemon of {}: {why}; nothing was changed",
        workspace.display()
    )]
    Unasked {
        /// Its workspace.
        workspace: PathBuf,
        /// Why.
        why: String,
    },
}

/// Which side of the re-home the caller's own daemon serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The realm that moves (a push).
    Adopt,
    /// The destination's realm, which is replaced (a pull).
    Replace,
}

/// A live daemon found for a workspace, asked as a probe.
struct Live {
    endpoint: Endpoint,
    connection: Connection,
    status: Status,
}

/// What a re-home does once checked ([`prepare`], [`carry_out`]).
pub struct Plan {
    root: PathBuf,
    adopt: Home,
    adopt_files: Vec<PathBuf>,
    workspace: PathBuf,
    replace: Option<Home>,
    replace_files: Vec<PathBuf>,
    /// Daemons to stop other than the caller's own.
    stop: Vec<Live>,
}

impl Plan {
    /// The realm that will serve the workspace.
    #[must_use]
    pub fn adopt(&self) -> &Home {
        &self.adopt
    }

    /// The workspace it will serve.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

/// What a re-home did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rehomed {
    /// The realm that serves the workspace now.
    pub home: Home,
    /// The workspace it served before.
    pub from: PathBuf,
    /// The empty realm the workspace had, removed.
    pub removed: Option<String>,
}

impl std::fmt::Display for Rehomed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "realm {} now serves {} (it served {})",
            self.home.id,
            self.home.manifest.path.display(),
            self.from.display()
        )?;
        if let Some(removed) = &self.removed {
            write!(f, "; the empty realm {removed} it had was removed")?;
        }
        Ok(())
    }
}

/// The method `M` on `connection`, bounded.
async fn ask<M: protocol::Method>(
    connection: &Connection,
    params: &M::Params,
) -> Result<M::Result, String> {
    tokio::time::timeout(ASK, super::remote::call::<M>(connection, params))
        .await
        .map_err(|_| "it did not answer in time".to_string())?
        .map_err(|err| err.to_string())
}

/// The live daemon of `workspace` (its endpoint files `files`), asked for
/// its status; `None` when none answers.
async fn live_daemon(workspace: &Path, files: &[PathBuf]) -> Result<Option<Live>, RehomeError> {
    let unasked = |why: String| RehomeError::Unasked {
        workspace: workspace.to_path_buf(),
        why,
    };
    let Liveness::Live(endpoint) = launch::probe_any(files).await else {
        return Ok(None);
    };
    let (connection, _notes) = Connection::attach_as(&endpoint, ASK, None, true)
        .await
        .map_err(|err| unasked(err.to_string()))?;
    let status = ask::<protocol::method::Status>(&connection, &protocol::Empty {})
        .await
        .map_err(unasked)?;
    Ok(Some(Live {
        endpoint,
        connection,
        status,
    }))
}

/// Refuse when a turn runs on the daemon `status` describes (unknown is
/// not idle).
fn check_idle(status: &Status) -> Result<(), RehomeError> {
    let busy = status
        .agents
        .iter()
        .find(|row| row.running != Some(false))
        .map(|row| row.name.clone())
        .or_else(|| (status.agents.is_empty() && status.running).then(|| "?".to_string()));
    match busy {
        Some(agent) => Err(RehomeError::Running {
            pid: status.pid,
            workspace: status.workspace.clone(),
            agent,
        }),
        None => Ok(()),
    }
}

/// Check that `adopt` (under `root`) may be made the realm of `workspace`
/// (canonical), replacing `replace` (the realm it has, if any): see the
/// module. `mine` is the daemon the caller is attached to and the side it
/// serves (`None` from the shell); it is checked here and stopped by the
/// caller, the other daemons by [`carry_out`].
///
/// # Errors
///
/// [`RehomeError`].
pub async fn prepare(
    root: &Path,
    adopt: Home,
    workspace: PathBuf,
    replace: Option<Home>,
    files: &EndpointFiles,
    mine: Option<(&Connection, Side)>,
) -> Result<Plan, RehomeError> {
    realm_home::check_reach(&adopt, &workspace)?;
    let adopt_files = files(&adopt.manifest.path);
    let replace_files = files(&workspace);
    let mine_status = match mine {
        Some((connection, _)) => Some(
            ask::<protocol::method::Status>(connection, &protocol::Empty {})
                .await
                .map_err(|why| RehomeError::Unasked {
                    workspace: workspace.clone(),
                    why,
                })?,
        ),
        None => None,
    };
    let side = mine.map(|(_, side)| side);
    let mut stop = Vec::new();
    // The realm that moves: a turn on its daemon refuses; its clients
    // are disconnected.
    if side == Some(Side::Adopt) {
        if let Some(status) = &mine_status {
            check_idle(status)?;
        }
    } else if let Some(live) = live_daemon(&adopt.manifest.path, &adopt_files).await? {
        check_idle(&live.status)?;
        stop.push(live);
    }
    // The destination's realm must hold nothing.
    if let Some(replace) = &replace {
        let held = if side == Some(Side::Replace) {
            let (Some((connection, _)), Some(status)) = (mine, &mine_status) else {
                return Err(HomeError::Changed("no daemon to ask".into()).into());
            };
            check_idle(status)?;
            let others = status.clients.saturating_sub(1);
            if others > 0 {
                return Err(RehomeError::Busy {
                    id: replace.id.clone(),
                    pid: status.pid,
                    clients: others,
                });
            }
            held_by(connection, &workspace).await?
        } else if let Some(live) = live_daemon(&workspace, &replace_files).await? {
            check_idle(&live.status)?;
            if live.status.clients > 0 {
                return Err(RehomeError::Busy {
                    id: replace.id.clone(),
                    pid: live.status.pid,
                    clients: live.status.clients,
                });
            }
            let held = held_by(&live.connection, &workspace).await?;
            stop.push(live);
            held
        } else {
            realm_home::holds(root, replace).await
        };
        if !held.is_empty() {
            return Err(HomeError::Holds {
                id: replace.id.clone(),
                what: held.join("; "),
            }
            .into());
        }
    } else if side == Some(Side::Replace) {
        return Err(HomeError::Changed(format!("{} has no realm", workspace.display())).into());
    }
    Ok(Plan {
        root: root.to_path_buf(),
        adopt,
        adopt_files,
        workspace,
        replace,
        replace_files,
        stop,
    })
}

/// What the daemon on `connection` says its realm holds.
async fn held_by(connection: &Connection, workspace: &Path) -> Result<Vec<String>, RehomeError> {
    ask::<protocol::method::Holds>(connection, &protocol::Empty {})
        .await
        .map(|holds| holds.held)
        .map_err(|why| RehomeError::Unasked {
            workspace: workspace.to_path_buf(),
            why,
        })
}

/// Ask the daemon on `connection` to stop, keeping its agents (they are
/// restored at its next start). A connection that drops is an answer too.
///
/// # Errors
///
/// Why it was not asked.
pub async fn stop(connection: &Connection) -> Result<(), String> {
    let params = ShutdownParams { retire: false };
    let asked = tokio::time::timeout(
        ASK,
        super::remote::call::<protocol::method::Shutdown>(connection, &params),
    )
    .await;
    match asked {
        Ok(Ok(_) | Err(super::ClientError::Disconnected)) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("it did not answer in time".to_string()),
    }
}

/// Carry out `plan`: stop the daemons it found (the caller stopped its own
/// already), then commit once both realms are proven idle (waiting up to
/// `wait`).
///
/// # Errors
///
/// [`RehomeError`]; a daemon that cannot be asked to stop is
/// [`RehomeError::Unasked`].
pub async fn carry_out(plan: Plan, wait: Duration) -> Result<Rehomed, RehomeError> {
    let Plan {
        root,
        adopt,
        adopt_files,
        workspace,
        replace,
        replace_files,
        stop: daemons,
    } = plan;
    for live in &daemons {
        stop(&live.connection)
            .await
            .map_err(|why| RehomeError::Unasked {
                workspace: live.status.workspace.clone(),
                why,
            })?;
        tracing::info!(pid = live.endpoint.pid, "re-home: asked a daemon to stop");
    }
    drop(daemons);
    let from = adopt.manifest.path.clone();
    let home = realm_home::commit(
        &root,
        &adopt,
        &adopt_files,
        &workspace,
        replace
            .as_ref()
            .map(|home| (home, replace_files.as_slice())),
        wait,
    )
    .await?;
    Ok(Rehomed {
        home,
        from,
        removed: replace.map(|home| home.id),
    })
}

/// `rkat-tui --rehome <reference>`: pull the realm `reference` names (its
/// id, or the workspace path it served, against the working directory) to
/// the workspace `args` name, stopping the daemons of both realms when
/// they are idle. No daemon is started.
///
/// # Errors
///
/// [`crate::TuiError`] when the workspace or the realms cannot be read,
/// [`RehomeError`] when it is refused.
pub async fn pull_here(
    args: &crate::cli::Args,
    reference: &str,
    files: &EndpointFiles,
) -> Result<Rehomed, crate::TuiError> {
    let (root, workspace, current) = crate::bootstrap::scope::lookup(args)?;
    let homes = realm_home::scan(&root)?;
    let base = std::env::current_dir().map_err(|source| crate::TuiError::ProjectRoot {
        path: PathBuf::from("."),
        source,
    })?;
    let adopt = match realm_home::read_reference(&homes, reference, Way::Pull, &base, &workspace)? {
        Reading::Pull(home) => home,
        Reading::Push(_) => return Err(HomeError::UnknownRealm(reference.to_string()).into()),
    };
    let plan = prepare(&root, adopt, workspace, current, files, None).await?;
    Ok(carry_out(plan, STOP_WAIT).await?)
}
