//! `--no-attach` while a daemon serves the workspace (P3 of the
//! always-daemon plan): before the first prompt the user learns which
//! daemon it is and what running alongside it means (no memory search, the
//! daemon holds the index; two agents in one tree), and in the interactive
//! TUI chooses: attach, run alongside, or stop the daemon first
//! ([`crate::ui::ask_alongside`]). A start with a prompt on the command
//! line gets the same as a one-line notice and runs in-process.
//!
//! `--no-attach -c` (or `--resume`) that lands on the daemon's current
//! conversation would fail with the engine's "served by another runtime
//! owner"; [`explain`] turns that into a plain message.

use std::path::PathBuf;
use std::time::Duration;

use meerkat_core::SessionId;

use super::launch::{self, Liveness};
use crate::daemon::endpoint::Endpoint;
use crate::daemon::protocol::{self, Status};
use crate::error::TuiError;

/// How long a stopped daemon may take to go away.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// A daemon that answers for the workspace.
#[derive(Debug, Clone)]
pub struct LiveDaemon {
    /// Its endpoint file's content.
    pub endpoint: Endpoint,
    /// Its `rkat/status` (`None`: it could not say).
    pub status: Option<Status>,
}

/// The daemon live at one of `files` (the workspace's endpoint file
/// places), with its status.
pub async fn find(files: &[PathBuf]) -> Option<LiveDaemon> {
    let Liveness::Live(endpoint) = launch::probe_any(files).await else {
        return None;
    };
    let status = crate::daemon::list::probe(&endpoint).await?;
    Some(LiveDaemon { endpoint, status })
}

impl LiveDaemon {
    /// "the workspace's daemon serves it (/w, pid 12, up 3m, idle)".
    #[must_use]
    pub fn describe(&self, now: chrono::DateTime<chrono::Utc>) -> String {
        let workspace = self
            .status
            .as_ref()
            .map(|status| status.workspace.clone())
            .or_else(|| self.endpoint.workspace.clone())
            .map_or_else(|| "?".to_string(), |w| w.display().to_string());
        let up = self
            .endpoint
            .started()
            .and_then(|started| u64::try_from((now - started).num_seconds()).ok())
            .map_or_else(|| "?".to_string(), crate::daemon::list::uptime);
        let state = match &self.status {
            Some(status) if status.running => ", running a turn",
            Some(_) => ", idle",
            None => "",
        };
        format!(
            "the rkat-tui-mob daemon serves this workspace ({workspace}, pid {}, up {up}{state})",
            self.endpoint.pid
        )
    }

    /// The one-line notice of a start that does not ask (a prompt on the
    /// command line, or no terminal to ask on).
    #[must_use]
    pub fn notice(&self, now: chrono::DateTime<chrono::Utc>) -> String {
        format!(
            "--no-attach, but {}: running alongside it, so no memory search (the daemon holds the index) and two agents work in one tree; rkat-tui without --no-attach attaches to it",
            self.describe(now)
        )
    }

    /// The daemon's current conversation.
    #[must_use]
    pub fn session(&self) -> Option<&SessionId> {
        self.status.as_ref().and_then(|s| s.session_id.as_ref())
    }

    /// Stop the daemon (`rkat/shutdown`) and wait until it ended.
    ///
    /// # Errors
    ///
    /// Why it was not stopped.
    pub async fn stop(&self, files: &[PathBuf]) -> Result<(), String> {
        let (connection, _notes) = super::transport::Connection::attach_as(
            &self.endpoint,
            super::transport::ATTACH_TIMEOUT,
            None,
            false,
        )
        .await
        .map_err(|err| format!("cannot reach the daemon: {err}"))?;
        let answered = tokio::time::timeout(
            STOP_WAIT,
            super::remote::local::<_, serde_json::Value>(
                &connection,
                protocol::SHUTDOWN,
                &serde_json::json!({}),
            ),
        )
        .await;
        if let Ok(Err(err)) = &answered
            && !matches!(err, super::ClientError::Disconnected)
        {
            return Err(format!("the daemon refused to stop: {err}"));
        }
        drop(connection);
        if super::runner::daemon_ended(files, &self.endpoint, STOP_WAIT).await {
            Ok(())
        } else {
            Err(format!(
                "the daemon (pid {}) did not stop within {}s",
                self.endpoint.pid,
                STOP_WAIT.as_secs()
            ))
        }
    }
}

/// The plain error for an in-process start the engine refused because
/// `daemon` runs the session: `None` when `err` is something else.
#[must_use]
pub fn explain(err: &TuiError, daemon: &LiveDaemon) -> Option<TuiError> {
    let refused = served_elsewhere(err)?;
    (daemon.session() == Some(&refused)).then(|| TuiError::RunningInDaemon {
        session: refused.to_string(),
    })
}

/// The session the engine said another runtime owner serves, when `err`
/// is that refusal (opening it, or materializing it on the runtime).
#[must_use]
pub fn served_elsewhere(err: &TuiError) -> Option<SessionId> {
    use meerkat::surface::SurfaceRuntimeMaterializeError as Materialize;
    use meerkat_core::service::SessionError;
    use meerkat_runtime::RuntimeBindingsError;
    let refusal = match err {
        TuiError::Session(refusal)
        | TuiError::Materialize(
            Materialize::Session(refusal)
            | Materialize::RuntimeBindings(RuntimeBindingsError::Hosting(_, refusal)),
        ) => refusal,
        _ => return None,
    };
    match refusal {
        SessionError::ServedElsewhere { id } => Some(id.clone()),
        _ => None,
    }
}
