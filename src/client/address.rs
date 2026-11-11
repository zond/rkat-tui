//! `--attach=HOST:PORT` (P3 of the always-daemon plan): a daemon reached
//! through a tunnel. The daemon keeps binding loopback only; the user
//! forwards a local port to it (`ssh -L 127.0.0.1:PORT:127.0.0.1:DAEMON_PORT
//! host`) and copies its token (`--attach-token-file`: the token alone, or
//! the daemon's endpoint file). Before the TUI opens the client attaches as
//! a probe and reads `rkat/status`: the daemon's workspace, pid and host.
//! When that workspace is provably on this machine (an endpoint file here
//! names the daemon by the token presented, for that workspace, and the
//! directory exists; a matching host name and path are not enough, two
//! machines can share both) the client runs on it as `--attach=DIRECTORY`
//! does; otherwise the features that edit or read local files (`!`, `@`
//! mentions, `/remember`, `/memory`, `/export` to a path) are refused with
//! the reason ([`Reached::remote`]), and everything that goes over the
//! wire (prompts, approvals, modes, rules, `/cost`, `/context`, the
//! pickers) works.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    pub fn apply(self, config: &mut super::ClientConfig) {
        if self.remote.is_some() {
            config.endpoint_files.clear();
        }
        config.address = Some(self.endpoint);
        config.remote = self.remote;
    }
}

/// The token in `path`: refused when its group or others may read or
/// write the file (any of mode `0o077`); the file
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
        // Group bits count too: a group member who can read the token
        // can attach, and one who can write the file can replace it.
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} is accessible to other users or its group (mode {:o}); the token is the whole access control: chmod 600 it",
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

/// Why the daemon of `status`, reached with `token`, does not serve a
/// local directory, or `None` when it does. Local is established only by
/// this machine's own record: one of `local_files` (the endpoint files
/// here, [`crate::client::list::files`]) names the daemon by `token` for
/// the workspace `status` reports, and that workspace is a directory. A
/// matching host name and an existing path are not evidence: two machines
/// can share both, and treating the other's workspace as this one's would
/// run `!` and write files in the wrong tree. `local_host` only words the
/// reason.
#[must_use]
pub fn remote_reason(
    status: &Status,
    local_host: Option<&str>,
    token: &str,
    local_files: &[PathBuf],
) -> Option<String> {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let workspace = canonical(&status.workspace);
    let recorded = local_files.iter().any(|file| {
        Endpoint::read(file).is_ok_and(|endpoint| {
            endpoint.token == token
                && endpoint
                    .workspace
                    .as_deref()
                    .is_some_and(|theirs| canonical(theirs) == workspace)
        })
    });
    if recorded && status.workspace.is_dir() {
        return None;
    }
    Some(match (status.host.as_deref(), local_host) {
        (Some(theirs), Some(ours)) if theirs != ours => format!(
            "the daemon runs on {theirs}, its workspace {} is not on this machine",
            status.workspace.display()
        ),
        (None, _) => "the daemon does not say which host it runs on".to_string(),
        _ if !status.workspace.is_dir() => format!(
            "the daemon's workspace {} is not a directory here",
            status.workspace.display()
        ),
        _ => format!(
            "no endpoint file on this machine names this daemon, so its workspace {} may be another machine's (the same host name and path prove nothing)",
            status.workspace.display()
        ),
    })
}

/// Resolve `address` (refused unless it is loopback or `insecure`, see
/// [`check_loopback`]), read the token, attach as a probe and read the
/// daemon's status; its workspace is local only when one of
/// `local_files` names it ([`remote_reason`]).
///
/// # Errors
///
/// [`TuiError::AttachAddress`] naming what failed.
pub async fn reach(
    address: &str,
    token_file: Option<&Path>,
    insecure: bool,
    local_files: &[PathBuf],
) -> Result<Reached, TuiError> {
    let fail = |reason: String| TuiError::AttachAddress {
        address: address.to_string(),
        reason,
    };
    let token_file = token_file.ok_or_else(|| {
        fail(
            "needs --attach-token-file (the daemon's token, or a copy of its endpoint file)".into(),
        )
    })?;
    let socket = resolve(address).await.map_err(fail)?;
    check_loopback(socket, insecure).map_err(fail)?;
    let token = read_token_file(token_file).map_err(fail)?;
    let probe = Endpoint {
        address: socket,
        token,
        pid: 0,
        workspace: None,
        started_at: None,
    };
    let status = status_of(&probe, PROBE_TIMEOUT)
        .await
        .map_err(|err| fail(format!("no daemon answered with that token: {err}")))?;
    let remote = remote_reason(
        &status,
        crate::paths::local_host().as_deref(),
        &probe.token,
        local_files,
    );
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

/// Refuse `socket` unless it is a loopback address or `insecure` allows
/// another: the token never crosses the wire (the mutual HMAC handshake,
/// [`crate::daemon::handshake`]), but the session after it does,
/// unencrypted, and the documented way to reach a daemon elsewhere
/// (`ssh -L`) always gives a loopback address and an encrypted tunnel, so
/// anything else is a typo or a link that should not carry the session.
///
/// # Errors
///
/// Why, with the tunnel recipe.
pub fn check_loopback(socket: SocketAddr, insecure: bool) -> Result<(), String> {
    if insecure || socket.ip().is_loopback() {
        return Ok(());
    }
    Err(format!(
        "{} is not a loopback address: the session (your prompts, the agent's output, the approvals) would cross that network unencrypted (the token itself never does); forward the daemon's port instead (ssh -N -L 127.0.0.1:4100:127.0.0.1:<daemon port> <host>, then --attach=127.0.0.1:4100), or pass --attach-insecure to attach anyway",
        socket.ip()
    ))
}

/// The first address `address` (`HOST:PORT`) resolves to.
async fn resolve(address: &str) -> Result<SocketAddr, String> {
    tokio::net::lookup_host(address)
        .await
        .map_err(|err| format!("cannot resolve it: {err}"))?
        .next()
        .ok_or_else(|| "it resolves to no address".to_string())
}

/// Attach to `endpoint` as a probe and read its `rkat/status`, all within
/// `wait`.
///
/// # Errors
///
/// The connect, the attach or the call's error.
pub async fn status_of(endpoint: &Endpoint, wait: Duration) -> Result<Status, ClientError> {
    tokio::time::timeout(wait, async {
        let (connection, _notes) =
            Connection::attach_as(endpoint, ATTACH_TIMEOUT.min(wait), None, true).await?;
        super::remote::call::<protocol::method::Status>(&connection, &protocol::Empty {}).await
    })
    .await
    .map_err(|_| ClientError::Timeout("reading the daemon's status"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Any host named in `--attach=HOST:PORT` would carry the session
    /// unencrypted: only a loopback address (an `ssh -L` tunnel) is
    /// attached to unless `--attach-insecure`.
    #[test]
    fn only_a_loopback_address_is_attached_to_unless_insecure() {
        let remote: SocketAddr = "192.0.2.7:4100".parse().unwrap();
        let refused = check_loopback(remote, false).unwrap_err();
        assert!(refused.contains("unencrypted"), "{refused}");
        assert!(refused.contains("the token itself never does"), "{refused}");
        assert!(refused.contains("ssh -N -L"), "{refused}");
        assert!(refused.contains("--attach-insecure"), "{refused}");
        assert_eq!(check_loopback(remote, true), Ok(()));
        for local in ["127.0.0.1:4100", "[::1]:4100", "127.3.2.1:9"] {
            assert_eq!(
                check_loopback(local.parse().unwrap(), false),
                Ok(()),
                "{local}"
            );
        }
    }

    /// A non-loopback address is refused before the token is read or
    /// anything is connected to.
    #[tokio::test]
    async fn reach_refuses_a_non_loopback_address_before_the_token() {
        let refused = reach(
            "192.0.2.7:4100",
            Some(Path::new("/nonexistent/token")),
            false,
            &[],
        )
        .await;
        assert!(
            matches!(&refused, Err(TuiError::AttachAddress { reason, .. })
                if reason.contains("not a loopback address")),
            "{refused:?}"
        );
    }

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
            agent: None,
            agents: Vec::new(),
        }
    }

    /// The finding this guards: the same host name and an existing
    /// directory counted as local, but two machines can share both; only
    /// an endpoint file here naming the daemon's token for that workspace
    /// does.
    #[test]
    fn a_daemon_is_local_only_when_an_endpoint_file_here_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let file = state.path().join("mob.endpoint");
        let mut recorded = Endpoint {
            address: "127.0.0.1:1".parse().unwrap(),
            token: "tok".into(),
            pid: 1,
            workspace: Some(dir.path().to_path_buf()),
            started_at: None,
        };
        let files = vec![file.clone()];
        let here = status(Some("me"), dir.path());
        // Identical host name, identical existing path, no record: remote.
        let twin = remote_reason(&here, Some("me"), "tok", &[]).unwrap();
        assert!(twin.contains("no endpoint file on this machine"), "{twin}");
        assert!(remote_reason(&here, Some("me"), "tok", &files).is_some());
        // Recorded with that token and workspace: local.
        recorded.write(&file).unwrap();
        assert_eq!(remote_reason(&here, Some("me"), "tok", &files), None);
        // Another token (another daemon of this path here): remote.
        assert!(remote_reason(&here, Some("me"), "other", &files).is_some());
        // Recorded for another workspace: remote.
        recorded.workspace = Some(PathBuf::from("/elsewhere"));
        recorded.write(&file).unwrap();
        assert!(remote_reason(&here, Some("me"), "tok", &files).is_some());
        // The reasons name what is known.
        let elsewhere =
            remote_reason(&status(Some("devbox"), dir.path()), Some("me"), "tok", &[]).unwrap();
        assert!(elsewhere.contains("runs on devbox"), "{elsewhere}");
        let missing = remote_reason(
            &status(Some("me"), Path::new("/nonexistent/ws")),
            Some("me"),
            "tok",
            &[],
        )
        .unwrap();
        assert!(missing.contains("not a directory here"), "{missing}");
        let silent = remote_reason(&status(None, dir.path()), Some("me"), "tok", &[]).unwrap();
        assert!(silent.contains("does not say"), "{silent}");
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
        // The finding this guards: only the other bits were checked, so a
        // group-readable (0640) or group-writable (0620) token passed.
        for mode in [0o644, 0o640, 0o620, 0o604, 0o602] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            let refused = read_token_file(&file).unwrap_err();
            assert!(refused.contains("chmod 600"), "{mode:o}: {refused}");
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(read_token_file(&file).unwrap(), "abc123");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
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
