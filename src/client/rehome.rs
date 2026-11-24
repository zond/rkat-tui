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
    Arc::new(|workspace: &Path| crate::daemon::endpoint::candidates(workspace))
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
    /// `/rehome <directory>` (read either way) would move this
    /// workspace's state, which holds no conversation: more likely an
    /// adoption whose reference matched no realm.
    #[error(
        "this workspace's state holds no conversation, so moving it to {} would move nothing (nothing was changed): to adopt another workspace's state here, /rehome from <its id or old path> (/rehome without an argument picks one); to move this empty state anyway, /rehome to {}",
        path.display(),
        path.display()
    )]
    NothingToMove {
        /// The directory named.
        path: PathBuf,
    },
    /// `--remove-state` names a realm whose daemon has clients attached.
    #[error(
        "realm {id} has a daemon (pid {pid}) with {clients} client(s) attached: detach or quit them first (rkat-tui --list shows it), then remove it again; nothing was deleted"
    )]
    Attached {
        /// The realm.
        id: String,
        /// Its daemon's pid.
        pid: u32,
        /// Its clients.
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
    /// This workspace's state moves to another directory (a push), else
    /// another's state is adopted here (a pull).
    pushed: bool,
    /// The user confirmed deleting `replace` whatever it holds.
    discard: bool,
}

impl Plan {
    /// What the re-home is about to do, in words: the direction and both
    /// places ("moving this workspace's state …", "adopting realm …").
    #[must_use]
    pub fn describe(&self) -> String {
        if self.pushed {
            format!(
                "moving this workspace's state (realm {}) from {} to {}",
                self.adopt.id,
                self.adopt.manifest.path.display(),
                self.workspace.display()
            )
        } else {
            format!(
                "adopting realm {} (the state of {}) here in {}",
                self.adopt.id,
                self.adopt.manifest.path.display(),
                self.workspace.display()
            )
        }
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
    /// A push (this workspace's state moved), else a pull (another's
    /// adopted).
    pub pushed: bool,
    /// How many conversations the realm holds (`None`: its log cannot be
    /// read).
    pub conversations: Option<usize>,
}

impl std::fmt::Display for Rehomed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let to = self.home.manifest.path.display();
        let held = match self.conversations {
            Some(n) => crate::text::count(n, "conversation"),
            None => "conversations that cannot be counted".to_string(),
        };
        if self.pushed {
            write!(
                f,
                "moved this workspace's state (realm {}, {held}) from {} to {to}: it now belongs to {to}, and {} has none",
                self.home.id,
                self.from.display(),
                self.from.display()
            )?;
            if let Some(removed) = &self.removed {
                write!(
                    f,
                    "; the state {to} had before, realm {removed}, was removed"
                )?;
            }
        } else {
            write!(
                f,
                "adopted realm {} (was {}) for {to}, with {held}",
                self.home.id,
                self.from.display()
            )?;
            if let Some(removed) = &self.removed {
                write!(
                    f,
                    "; this workspace's own state before, realm {removed}, was removed"
                )?;
            }
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
/// caller, the other daemons by [`carry_out`]. `discard` is the id of the
/// realm the user confirmed deleting: a `replace` of that id is replaced
/// whatever it holds (a turn on its daemon, or another client there,
/// still refuses).
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
    discard: Option<&str>,
) -> Result<Plan, RehomeError> {
    realm_home::check_reach(&adopt, &workspace)?;
    realm_home::check_clear_of_config(realm_home::config_of(root), &workspace)?;
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
        if !held.is_empty() && discard != Some(replace.id.as_str()) {
            return Err(HomeError::Holds {
                id: replace.id.clone(),
                what: held.join("; "),
            }
            .into());
        }
    } else if side == Some(Side::Replace) {
        return Err(HomeError::Changed(format!("{} has no realm", workspace.display())).into());
    }
    let discard = replace
        .as_ref()
        .is_some_and(|replace| discard == Some(replace.id.as_str()));
    Ok(Plan {
        root: root.to_path_buf(),
        adopt,
        adopt_files,
        workspace,
        replace,
        replace_files,
        stop,
        pushed: side == Some(Side::Adopt),
        discard,
    })
}

/// What the daemon on `connection` says its realm holds.
pub(crate) async fn held_by(
    connection: &Connection,
    workspace: &Path,
) -> Result<Vec<String>, RehomeError> {
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
        pushed,
        discard,
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
        discard,
        wait,
    )
    .await?;
    let conversations = realm_home::conversations(&home);
    Ok(Rehomed {
        home,
        from,
        removed: replace.map(|home| home.id),
        pushed,
        conversations,
    })
}

/// The `/rehome` picker's question before moving this workspace's state
/// (`current`) to `path`, which has the realm `taken` when it has one.
#[must_use]
pub fn move_question(current: &Home, path: &Path, taken: Option<&Home>) -> String {
    let mut text = format!(
        "Move this workspace's state (realm {}) from {} to {}? The daemon stops, clients attached here are disconnected, and this rkat-tui then works in {}",
        current.id,
        current.manifest.path.display(),
        path.display(),
        path.display()
    );
    if let Some(taken) = taken {
        text.push_str(&format!(
            "; the state {} has now, realm {}, must hold no conversation and is removed",
            path.display(),
            taken.id
        ));
    }
    text.push('.');
    text
}

/// What a row of the `/rehome` picker says of `home` under its line: its
/// last use and remote.
fn row_note(home: &Home, first: Option<&str>) -> String {
    let mut parts: Vec<String> = first.map(ToString::to_string).into_iter().collect();
    if !home.manifest.last_used.is_empty() {
        parts.push(format!("last used {}", home.manifest.last_used));
    }
    if let Some(remote) = &home.manifest.git_remote {
        parts.push(remote.clone());
    }
    parts.join(" · ")
}

/// The `/rehome` picker's `d` on `home`: delete it after a question.
fn delete_key(home: &Home) -> crate::ui::PickKey {
    crate::ui::PickKey::confirmed(
        ratatui::crossterm::event::KeyCode::Char('d'),
        crate::ui::PickAct::RemoveState(home.id.clone()),
        format!(
            "Delete {}? This cannot be undone (refused while a turn runs there or a client is attached).",
            realm_home::deletion_words(home)
        ),
    )
}

/// The `/rehome` picker for the workspace `workspace` (canonical), whose
/// state is `current`, over the realms `homes`: a line on this
/// workspace's state; a row per realm whose workspace is gone, then one
/// per realm whose workspace still exists (dim: adopting it takes its
/// state from there, and that directory starts anew), Enter on either
/// asking before it runs `/rehome from <id>`; then the row that opens a
/// directory to move this workspace's state to (`/rehome to <dir>`, after
/// the runner's check and a question).
#[must_use]
pub fn picker(homes: &[Home], current: &Home, workspace: &Path) -> crate::ui::ListPicker {
    use crate::ui::{Field, FieldFor, ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use ratatui::crossterm::event::KeyCode;
    let (gone, kept) = realm_home::adoptable(homes, workspace);
    let mut rows = Vec::new();
    for home in &gone {
        let question = format!(
            "Adopt realm {} (the state of {}, which is gone) here in {}? This workspace's daemon restarts on it, and this workspace's own state, realm {}, must hold no conversation and is removed.",
            home.id,
            home.manifest.path.display(),
            workspace.display(),
            current.id
        );
        rows.push(
            PickRow::new(
                format!("{}  {} (gone)", home.id, home.manifest.path.display()),
                vec![
                    PickKey::confirmed(
                        KeyCode::Enter,
                        PickAct::Rehome(format!("from {}", home.id)),
                        question,
                    ),
                    delete_key(home),
                ],
            )
            .noted(row_note(home, None))
            .keyed(home.id.clone()),
        );
    }
    for home in &kept {
        let question = format!(
            "Adopt realm {} from {}, which still exists, here in {}? Its daemon is stopped (refused while a turn runs there), {} starts with fresh state the next time rkat-tui runs there, this workspace's daemon restarts on the adopted state, and this workspace's own state, realm {}, must hold no conversation and is removed.",
            home.id,
            home.manifest.path.display(),
            workspace.display(),
            home.manifest.path.display(),
            current.id
        );
        rows.push(
            PickRow::new(
                format!(
                    "{}  {} (still there)",
                    home.id,
                    home.manifest.path.display()
                ),
                vec![
                    PickKey::confirmed(
                        KeyCode::Enter,
                        PickAct::Rehome(format!("from {}", home.id)),
                        question,
                    ),
                    delete_key(home),
                ],
            )
            .noted(row_note(
                home,
                Some("its directory still exists: adopting takes its state from there"),
            ))
            .keyed(home.id.clone())
            .dimmed(),
        );
    }
    rows.push(
        PickRow::new(
            "Move this workspace's state to another directory…",
            vec![PickKey::field(
                KeyCode::Enter,
                Field {
                    what: FieldFor::Directory { refused: None },
                    text: String::new(),
                },
            )],
        )
        .noted("type the directory; the daemon restarts there and this rkat-tui follows"),
    );
    let mut header = format!(
        "{} keeps its state in realm {} ({})",
        workspace.display(),
        current.id,
        current.dir.display()
    );
    header.push_str(match (gone.is_empty(), kept.is_empty()) {
        (true, true) => "; no other workspace's state is here to adopt",
        (false, _) => "; adopt the state of a workspace that moved here, or move this one's:",
        (true, false) => {
            "; no state whose workspace is gone; other workspaces' state (dim) can be adopted too:"
        }
    });
    let mut picker = ListPicker::new(
        PickerKind::Rehome,
        rows,
        "",
        "↑/↓ move · Enter adopt that state here, or type a directory to move to · d delete that state (each asks first) · Esc close",
    )
    .with_header(header);
    picker.actions = 1;
    picker
}

/// The dialog a re-home refused for state it would replace (`taken`,
/// which holds something) opens: one row on it, the question already
/// asked; `y` runs `/rehome <argument>` again with `taken` deleted, `n`
/// or Esc keeps it (the notice beside it names the shell command).
#[must_use]
pub fn discard_picker(taken: &Home, argument: &str) -> crate::ui::ListPicker {
    use crate::ui::{ListPicker, PickAct, PickKey, PickRow, PickerKind, PickerLine};
    use ratatui::crossterm::event::KeyCode;
    let act = PickAct::RehomeDiscarding {
        argument: argument.to_string(),
        discard: taken.id.clone(),
    };
    let question = format!(
        "Delete {} and re-home? This cannot be undone.",
        realm_home::deletion_words(taken)
    );
    let row = PickRow::new(
        format!("{}  {}", taken.id, taken.manifest.path.display()),
        vec![PickKey::confirmed(
            KeyCode::Enter,
            act.clone(),
            question.clone(),
        )],
    )
    .noted(format!(
        "to delete it from the shell instead: rkat-tui --remove-state {} --yes",
        taken.id
    ))
    .keyed(taken.id.clone());
    let mut picker = ListPicker::new(
        PickerKind::Rehome,
        vec![row],
        "",
        "y delete it and re-home · n or Esc keep it (nothing changes) · Esc again close",
    )
    .with_header(format!(
        "/rehome {argument} would replace state that holds conversations:"
    ));
    picker.line = Some(PickerLine::Confirm {
        question,
        act,
        nudged: false,
    });
    picker
}

/// What `--remove-state` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removed {
    /// It would delete this (no `--yes`): the words and the command.
    Would(String),
    /// It deleted this.
    Deleted(String),
}

impl std::fmt::Display for Removed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Would(text) | Self::Deleted(text) => f.write_str(text),
        }
    }
}

/// Delete the realm `home` under `root`: refused while its workspace's
/// daemon runs a turn or has a client attached; an idle one is stopped
/// first. Without `yes` nothing is deleted: the answer says what would
/// be and the command line that deletes it (`config_dir`: the
/// `--config-dir` the caller was given, repeated in it).
///
/// # Errors
///
/// [`RehomeError`].
pub async fn remove_home(
    root: &Path,
    home: &Home,
    files: &EndpointFiles,
    yes: bool,
    config_dir: Option<&Path>,
) -> Result<Removed, RehomeError> {
    let what = realm_home::deletion_words(home);
    let endpoint_files = files(&home.manifest.path);
    let live = live_daemon(&home.manifest.path, &endpoint_files).await?;
    if let Some(live) = &live {
        check_idle(&live.status)?;
        if live.status.clients > 0 {
            return Err(RehomeError::Attached {
                id: home.id.clone(),
                pid: live.status.pid,
                clients: live.status.clients,
            });
        }
    }
    if !yes {
        let config = config_dir
            .map(|dir| format!(" --config-dir {}", dir.display()))
            .unwrap_or_default();
        let daemon = if live.is_some() {
            "; its idle daemon would be stopped first"
        } else {
            ""
        };
        return Ok(Removed::Would(format!(
            "would delete {what}{daemon}; this cannot be undone. To delete it: rkat-tui{config} --remove-state {} --yes",
            home.id
        )));
    }
    if let Some(live) = live {
        stop(&live.connection)
            .await
            .map_err(|why| RehomeError::Unasked {
                workspace: home.manifest.path.clone(),
                why,
            })?;
    }
    realm_home::remove(root, home, &endpoint_files, STOP_WAIT).await?;
    // A daemon that stopped removed its endpoint file; one left over is
    // stale (no daemon answered on it).
    for file in &endpoint_files {
        if file.exists()
            && let Err(err) = std::fs::remove_file(file)
        {
            tracing::warn!(file = %file.display(), error = %err, "a stale endpoint file was not removed");
        }
    }
    Ok(Removed::Deleted(format!("deleted {what}")))
}

/// `rkat-tui --remove-state <reference> [--yes]`: the realm `reference`
/// names (its id, its directory, or the workspace path it served, against
/// the working directory), deleted with `yes` ([`remove_home`]).
///
/// # Errors
///
/// [`crate::TuiError`] when the realms cannot be read, [`RehomeError`]
/// when it is refused.
pub async fn remove_state(
    args: &crate::cli::Args,
    reference: &str,
    files: &EndpointFiles,
) -> Result<Removed, crate::TuiError> {
    let global = crate::bootstrap::GlobalDirs::resolve(
        args.config_dir.as_deref(),
        args.user_config_root.as_deref(),
    )
    .ok_or(HomeError::NoConfigDir)?;
    let root = realm_home::root(&global);
    let homes = realm_home::scan(&root)?;
    let base = std::env::current_dir().map_err(|source| crate::TuiError::ProjectRoot {
        path: PathBuf::from("."),
        source,
    })?;
    let at = realm_home::Context::new(&root, &base, &base);
    let home = realm_home::named(&homes, reference, &at)?;
    Ok(remove_home(&root, &home, files, args.yes, args.config_dir.as_deref()).await?)
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
    let at = realm_home::Context::new(&root, &base, &workspace);
    let adopt = match realm_home::read_reference(&homes, reference, Way::Pull, &at)? {
        Reading::Pull(home) => home,
        Reading::Push(_) => return Err(HomeError::UnknownRealm(reference.to_string()).into()),
    };
    let plan = prepare(&root, adopt, workspace, current, files, None, None).await?;
    Ok(carry_out(plan, STOP_WAIT).await?)
}
