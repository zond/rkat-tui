//! `--attach=HOST:PORT` (P3 of the always-daemon plan): a daemon reached
//! through a tunnel. The daemon keeps binding loopback only; the user
//! forwards a local port to it (`ssh -L 127.0.0.1:PORT:127.0.0.1:DAEMON_PORT
//! host`) and copies its token (`--attach-token-file`: the token alone, or
//! the daemon's endpoint file). Before the TUI opens the client attaches as
//! a probe and reads `rkat/status`: the daemon's workspace, pid and host.
//! When that workspace is a directory on this machine (the same host name,
//! the directory exists) the client runs on it as `--attach=DIRECTORY`
//! does; otherwise the features that edit or read local files (`!`, `@`
//! mentions, `/remember`, `/memory`, `/export` to a path) are refused with
//! the reason ([`Reached::remote`]), and everything that goes over the
//! wire (prompts, approvals, modes, rules, `/cost`, `/context`, the
//! pickers) works.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use super::ClientError;
use super::transport::{ATTACH_TIMEOUT, Connection};
use crate::daemon::endpoint::Endpoint;
use crate::daemon::protocol::{self, Status};
use crate::error::TuiError;

/// How long the probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon `--attach=HOST:PORT` reached.
#[derive(Debug, Clone)]
pub struct Reached {
    /// Its address and token, with the pid, workspace and start time its
    /// status reported.
    pub endpoint: Endpoint,
    /// What it reported.
    pub status: Status,
    /// Why its workspace is not local (`None`: it is a directory here).
    pub remote: Option<String>,
}

impl Reached {
    /// Point `config` at this daemon: its address (never the workspace's
    /// endpoint files, unless its workspace is local) and, when its
    /// workspace is not on this machine, why the local features are off.
    pub fn apply(self, config: &mut super::AttachConfig) {
        if self.remote.is_some() {
            config.endpoint_files.clear();
        }
        config.address = Some(self.endpoint);
        config.remote = self.remote;
    }
}

/// The token in `path`: refused when others may read the file; the file
/// holds the token alone, or is a copy of the daemon's endpoint file.
///
/// # Errors
///
/// Why the token cannot be used.
pub fn read_token_file(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|err| format!("{}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode();
        if mode & 0o007 != 0 {
            return Err(format!(
                "{} is accessible to other users (mode {:o}); the token is the whole access control: chmod 600 it",
                path.display(),
                mode & 0o777
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let token = match serde_json::from_str::<Endpoint>(text.trim()) {
        Ok(endpoint) => endpoint.token,
        Err(_) => text.trim().to_string(),
    };
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(format!("{} holds no token", path.display()));
    }
    Ok(token)
}

/// This machine's host name.
#[must_use]
pub fn local_host() -> Option<String> {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .ok()
            .and_then(|name| name.into_string().ok())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Why the daemon of `status` does not serve a local directory, or `None`
/// when it does: the same host (`local_host`) and its workspace exists
/// here. A daemon that does not say its host counts as remote.
#[must_use]
pub fn remote_reason(status: &Status, local_host: Option<&str>) -> Option<String> {
    match (status.host.as_deref(), local_host) {
        (Some(theirs), Some(ours)) if theirs == ours => {
            if status.workspace.is_dir() {
                None
            } else {
                Some(format!(
                    "the daemon's workspace {} is not a directory here",
                    status.workspace.display()
                ))
            }
        }
        (Some(theirs), _) => Some(format!(
            "the daemon runs on {theirs}, its workspace {} is not on this machine",
            status.workspace.display()
        )),
        (None, _) => Some("the daemon does not say which host it runs on".to_string()),
    }
}

/// Resolve `address`, read the token, attach as a probe and read the
/// daemon's status.
///
/// # Errors
///
/// [`TuiError::AttachAddress`] naming what failed.
pub async fn reach(address: &str, token_file: Option<&Path>) -> Result<Reached, TuiError> {
    let fail = |reason: String| TuiError::AttachAddress {
        address: address.to_string(),
        reason,
    };
    let token_file = token_file.ok_or_else(|| {
        fail(
            "needs --attach-token-file (the daemon's token, or a copy of its endpoint file)".into(),
        )
    })?;
    let token = read_token_file(token_file).map_err(fail)?;
    let socket = resolve(address).await.map_err(fail)?;
    let probe = Endpoint {
        address: socket,
        token,
        pid: 0,
        workspace: None,
        started_at: None,
    };
    let status = status_of(&probe)
        .await
        .map_err(|err| fail(format!("no daemon answered with that token: {err}")))?;
    let remote = remote_reason(&status, local_host().as_deref());
    Ok(Reached {
        endpoint: Endpoint {
            pid: status.pid,
            workspace: Some(status.workspace.clone()),
            started_at: Some(status.started_at.clone()),
            ..probe
        },
        status,
        remote,
    })
}

/// The first address `address` (`HOST:PORT`) resolves to.
async fn resolve(address: &str) -> Result<SocketAddr, String> {
    tokio::net::lookup_host(address)
        .await
        .map_err(|err| format!("cannot resolve it: {err}"))?
        .next()
        .ok_or_else(|| "it resolves to no address".to_string())
}

/// Attach to `endpoint` as a probe and read its `rkat/status`.
///
/// # Errors
///
/// The connect, the attach or the call's error.
pub async fn status_of(endpoint: &Endpoint) -> Result<Status, ClientError> {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let (connection, _notes) =
            Connection::attach_as(endpoint, ATTACH_TIMEOUT, None, true).await?;
        super::remote::local::<_, Status>(&connection, protocol::STATUS, &serde_json::json!({}))
            .await
    })
    .await
    .map_err(|_| ClientError::Timeout("reading the daemon's status"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn status(host: Option<&str>, workspace: &Path) -> Status {
        Status {
            workspace: workspace.to_path_buf(),
            host: host.map(str::to_string),
            pid: 1,
            started_at: String::new(),
            clients: 0,
            detached: false,
            owner: false,
            session_id: None,
            running: false,
        }
    }

    #[test]
    fn a_daemon_is_local_only_on_this_host_with_its_workspace_here() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            remote_reason(&status(Some("me"), dir.path()), Some("me")),
            None
        );
        let elsewhere = remote_reason(&status(Some("devbox"), dir.path()), Some("me")).unwrap();
        assert!(elsewhere.contains("runs on devbox"), "{elsewhere}");
        let missing = remote_reason(
            &status(Some("me"), Path::new("/nonexistent/ws")),
            Some("me"),
        )
        .unwrap();
        assert!(missing.contains("not a directory here"), "{missing}");
        assert!(remote_reason(&status(None, dir.path()), Some("me")).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_refused_when_others_can_read_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "abc123\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(&file).unwrap(), "abc123");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refused = read_token_file(&file).unwrap_err();
        assert!(refused.contains("chmod 600"), "{refused}");
        // A copy of the endpoint file works too.
        let endpoint = Endpoint {
            address: "127.0.0.1:1".parse().unwrap(),
            token: "fromfile".into(),
            pid: 1,
            workspace: None,
            started_at: None,
        };
        std::fs::remove_file(&file).unwrap();
        endpoint.write(&file).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(&file).unwrap(), "fromfile");
        std::fs::write(&file, "  \n").unwrap();
        assert!(read_token_file(&file).unwrap_err().contains("no token"));
    }
}
