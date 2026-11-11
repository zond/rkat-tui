//! The endpoint file: where a running daemon listens and the token a
//! client must present ([`super::handshake`]).
//!
//! `<state>/rkat-tui/<key>/mob.endpoint` ([`default_file`]), mode 0600 in
//! a directory of mode 0700, one line of JSON:
//!
//! ```json
//! {"address":"127.0.0.1:43123","token":"<64 lowercase hex digits>","pid":12345,
//!  "workspace":"/home/me/project","started_at":"2026-11-09T12:00:00Z"}
//! ```
//!
//! (on one line). `workspace` is the canonical workspace path the daemon
//! serves and `started_at` when it started (RFC 3339, UTC). Written
//! (atomically, by rename) once the listener is bound, removed on shutdown. A file left by a daemon that died is stale: the next daemon
//! replaces it when nothing accepts at its address.
//!
//! **The token is the whole access control.** Anyone who reads it can
//! attach, answer `decision/decide` and change the permission mode and the
//! rules; nothing on a loopback TCP connection says which program sent it.
//! So the file lives in the user's state directory (`$XDG_STATE_HOME`, else
//! `~/.local/state`), under a key derived from the canonical workspace path
//! ([`workspace_key`]), never inside the workspace:
//! the agent's confined shell is granted the workspace, the temp dir, the
//! Cargo home and the user cache dir, none of which holds it. When the shell's confinement plan would grant that directory after
//! all ([`exposing_grant`]), the daemon falls back to
//! `$XDG_RUNTIME_DIR/rkat-tui/<key>/mob.endpoint` (same modes), and refuses
//! to start when that is granted too, or unset ([`candidates`]; clients
//! look in the same order). The temp dir is granted to the shell and is
//! never used.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::DaemonError;

/// The directory under the user's state directory that holds the
/// per-workspace endpoint directories.
pub const STATE_DIR: &str = "rkat-tui";

/// Hex digits of the workspace key (of the SHA-256 of its canonical path).
pub const KEY_DIGITS: usize = 32;

/// The user's state directory: `$XDG_STATE_HOME` when it is an absolute
/// path, else `~/.local/state`; `None` without a home directory.
#[must_use]
pub fn state_home() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")))
}

/// The user's runtime directory: `$XDG_RUNTIME_DIR` when it is an
/// absolute path (the fallback place of the endpoint file).
#[must_use]
pub fn runtime_home() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The directories holding the per-workspace endpoint directories, in the
/// order the daemon and clients try them: `<state>/rkat-tui`, then
/// `$XDG_RUNTIME_DIR/rkat-tui` (used when the agent's confined shell would
/// be granted the first; never the temp dir, which the shell is granted).
#[must_use]
pub fn search_dirs() -> Vec<PathBuf> {
    [state_home(), runtime_home()]
        .into_iter()
        .flatten()
        .map(|base| base.join(STATE_DIR))
        .collect()
}

/// The places of the endpoint file of `workspace`, in the order the
/// daemon tries them and clients look ([`search_dirs`]).
#[must_use]
pub fn candidates(workspace: &Path) -> Vec<PathBuf> {
    let key = workspace_key(workspace);
    search_dirs()
        .into_iter()
        .map(|dir| dir.join(&key).join(super::ENDPOINT_NAME))
        .collect()
}

/// The key of `workspace`: the first [`KEY_DIGITS`] hex digits of the
/// SHA-256 of the workspace's canonical path (the path as given when it
/// cannot be resolved).
#[must_use]
pub fn workspace_key(workspace: &Path) -> String {
    use sha2::Digest as _;
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut hasher = sha2::Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    crate::hex::encode(&hasher.finalize()[..KEY_DIGITS / 2])
}

/// The endpoint file of `workspace` under the state directory
/// `state_home`.
#[must_use]
pub fn file_in(state_home: &Path, workspace: &Path) -> PathBuf {
    state_home
        .join(STATE_DIR)
        .join(workspace_key(workspace))
        .join(super::ENDPOINT_NAME)
}

/// The endpoint file of `workspace` in this user's state directory.
///
/// # Errors
///
/// [`std::io::ErrorKind::NotFound`] when there is no state directory (no
/// `$XDG_STATE_HOME` and no home directory).
pub fn default_file(workspace: &Path) -> Result<PathBuf, std::io::Error> {
    state_home()
        .map(|state| file_in(&state, workspace))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no state directory: set XDG_STATE_HOME or HOME",
            )
        })
}

/// Create `dir` and its missing parents with mode 0700 (an existing
/// directory is left as it is).
///
/// # Errors
///
/// [`DaemonError::Io`] when it cannot be created.
pub fn create_private_dir(dir: &Path) -> Result<(), DaemonError> {
    crate::paths::create_private_dir(dir)
        .map_err(|err| DaemonError::io("create endpoint directory", dir, err))
}

/// The grant of the agent shell's confinement `plan` that would let the
/// shell read `path` (canonical), if any: `Some` when its read access is
/// unrestricted or a granted subtree contains `path` (or a granted file is
/// it). An unconfined shell (`--no-confinement`) is the trusted host and
/// reads what the user reads; it yields `None`, as there is nothing to
/// keep from it.
#[must_use]
pub fn exposing_grant(plan: &crate::bootstrap::ConfinementPlan, path: &Path) -> Option<PathBuf> {
    use meerkat_core::confinement::{FilesystemAccess, PathAccess};
    let meerkat_tools::builtin::shell::ShellConfinement::Required { requirement } = &plan.shell
    else {
        return None;
    };
    let spec = requirement.specification();
    let grants: &[PathAccess] = match &spec.read {
        FilesystemAccess::Unrestricted => return Some(PathBuf::from("/")),
        FilesystemAccess::Paths(paths) => paths,
    };
    grants.iter().find_map(|grant| {
        let hit = match grant {
            PathAccess::Subtree(dir) => path.starts_with(dir),
            PathAccess::Literal(file) => path == file,
        };
        hit.then(|| grant.path().to_path_buf())
    })
}

/// `time` as the endpoint file records it: RFC 3339, UTC, whole seconds.
#[must_use]
pub fn timestamp(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Bytes of randomness in a token (hex-encoded: twice as many digits).
pub const TOKEN_BYTES: usize = 32;

/// The endpoint file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// The bound loopback address.
    pub address: std::net::SocketAddr,
    /// The token the attach handshake requires.
    pub token: String,
    /// The daemon's process id.
    pub pid: u32,
    /// The canonical workspace path the daemon serves (every file names
    /// it; `None`: not known yet, for a daemon reached by address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// When the daemon started, RFC 3339 in UTC (every file names it;
    /// `None` as for `workspace`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

impl Endpoint {
    /// When the daemon started, as a time (`None`: unknown or unreadable).
    #[must_use]
    pub fn started(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let text = self.started_at.as_deref()?;
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|time| time.with_timezone(&chrono::Utc))
    }

    /// Read the endpoint file at `path`.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Io`] when it cannot be read or parsed.
    pub fn read(path: &Path) -> Result<Self, DaemonError> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| DaemonError::io("read endpoint file", path, err))?;
        serde_json::from_str(text.trim()).map_err(|err| {
            DaemonError::io(
                "parse endpoint file",
                path,
                std::io::Error::new(std::io::ErrorKind::InvalidData, err),
            )
        })
    }

    /// Write it to `path` with mode 0600 (via a temporary file and a
    /// rename, so a reader never sees half a file).
    ///
    /// # Errors
    ///
    /// [`DaemonError::Io`] when the directory or the file cannot be written.
    pub fn write(&self, path: &Path) -> Result<(), DaemonError> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        create_private_dir(dir)?;
        let mut line = serde_json::to_string(self).map_err(|err| {
            DaemonError::io("write endpoint file", path, std::io::Error::other(err))
        })?;
        line.push('\n');
        crate::paths::write_atomic(
            path,
            line.as_bytes(),
            crate::paths::FileMode::Private,
            Some(dir),
        )
        .map_err(|err| DaemonError::io("write endpoint file", path, std::io::Error::other(err)))
    }
}

/// A fresh random token: [`TOKEN_BYTES`] from the OS, hex-encoded.
///
/// # Errors
///
/// [`DaemonError::Token`] when the OS randomness cannot be read.
pub fn new_token() -> Result<String, DaemonError> {
    crate::hex::random::<TOKEN_BYTES>().map_err(DaemonError::Token)
}

/// Refuse unless the endpoint file at `path` is absent or proven stale:
/// only a refused connect proves that no daemon serves at its address
/// anymore (then [`Endpoint::write`] replaces it). A connect that times
/// out or fails otherwise, and a file that cannot be read, prove nothing,
/// and the start is refused rather than risking a second daemon beside a
/// live one.
///
/// # Errors
///
/// [`DaemonError::AlreadyRunning`] when something accepts at the
/// address, [`DaemonError::EndpointUndecided`] when it cannot be told.
pub async fn check_stale(path: &Path) -> Result<(), DaemonError> {
    let undecided = |why: String| DaemonError::EndpointUndecided {
        file: path.to_path_buf(),
        why,
    };
    match path.try_exists() {
        Ok(false) => return Ok(()),
        Ok(true) => {}
        Err(err) => return Err(undecided(format!("cannot look at it: {err}"))),
    }
    let endpoint = Endpoint::read(path).map_err(|err| undecided(err.to_string()))?;
    match reach(endpoint.address, REACH_TIMEOUT).await {
        Reach::Accepts => Err(DaemonError::AlreadyRunning {
            address: endpoint.address,
            pid: endpoint.pid,
            file: path.to_path_buf(),
        }),
        Reach::Refused => {
            tracing::info!(file = %path.display(), address = %endpoint.address, "replacing a stale endpoint file");
            Ok(())
        }
        Reach::Unknown(why) => Err(undecided(format!(
            "its address {}: {why}",
            endpoint.address
        ))),
    }
}

/// How long [`reach`] waits for a connect.
pub const REACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What a connect to an endpoint's address found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Something accepts there.
    Accepts,
    /// The connect was refused: nothing listens (the daemon is gone).
    Refused,
    /// Neither could be told (a timeout, an unreachable network, …): why.
    Unknown(String),
}

/// Connect to a daemon's `address` within `wait`: the one liveness probe
/// of the daemon and its clients (a start, `--list`, a reconnect, a
/// leave). Only a refused (or reset) connect counts as [`Reach::Refused`],
/// the evidence that nothing listens; a timeout or another failure proves
/// nothing ([`Reach::Unknown`]).
pub async fn reach(address: std::net::SocketAddr, wait: std::time::Duration) -> Reach {
    match tokio::time::timeout(wait, tokio::net::TcpStream::connect(address)).await {
        Ok(Ok(_)) => Reach::Accepts,
        Ok(Err(err)) => classify(&err),
        Err(_) => Reach::Unknown(format!("no answer within {}s", wait.as_secs())),
    }
}

/// What a failed connect says: a refusal (or a reset, which a loopback
/// connect to a port nobody listens on can report) is evidence of a stop;
/// anything else (a timeout, a local socket error) is not.
#[must_use]
pub fn classify(err: &std::io::Error) -> Reach {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
            Reach::Refused
        }
        _ => Reach::Unknown(err.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The probe is async: a listener accepts, a closed port refuses.
    #[tokio::test]
    async fn the_probe_tells_a_listener_from_a_closed_port() {
        let wait = std::time::Duration::from_secs(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        assert_eq!(reach(open, wait).await, Reach::Accepts);
        drop(listener);
        assert_eq!(reach(open, wait).await, Reach::Refused);
    }

    /// Only a refusal proves a stop: a connect that timed out or failed
    /// otherwise leaves the question open.
    #[test]
    fn only_a_refused_connect_counts_as_a_stopped_daemon() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify(&Error::from(ErrorKind::ConnectionRefused)),
            Reach::Refused
        );
        assert_eq!(
            classify(&Error::from(ErrorKind::ConnectionReset)),
            Reach::Refused
        );
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::WouldBlock,
            ErrorKind::PermissionDenied,
            ErrorKind::AddrNotAvailable,
            ErrorKind::Other,
        ] {
            assert!(
                matches!(classify(&Error::from(kind)), Reach::Unknown(_)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_file_is_one_private_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("mob.endpoint");
        let endpoint = Endpoint {
            address: "127.0.0.1:4242".parse().unwrap(),
            token: new_token().unwrap(),
            pid: 7,
            workspace: Some(PathBuf::from("/w")),
            started_at: Some(timestamp(std::time::UNIX_EPOCH)),
        };
        assert_eq!(endpoint.token.len(), TOKEN_BYTES * 2);
        assert!(endpoint.token.chars().all(|c| c.is_ascii_hexdigit()));
        endpoint.write(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(
            text.starts_with("{\"address\":\"127.0.0.1:4242\",\"token\":\""),
            "{text}"
        );
        assert_eq!(Endpoint::read(&path).unwrap(), endpoint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_ne!(new_token().unwrap(), endpoint.token);
        assert!(
            text.contains("\"workspace\":\"/w\",\"started_at\":\"1970-01-01T00:00:00Z\""),
            "{text}"
        );
        assert_eq!(
            endpoint.started(),
            Some(chrono::DateTime::<chrono::Utc>::from(std::time::UNIX_EPOCH))
        );
    }

    #[test]
    fn a_file_an_older_daemon_wrote_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mob.endpoint");
        std::fs::write(
            &path,
            "{\"address\":\"127.0.0.1:1\",\"token\":\"t\",\"pid\":3}\n",
        )
        .unwrap();
        let old = Endpoint::read(&path).unwrap();
        assert_eq!((old.pid, old.started()), (3, None));
        assert_eq!(old.workspace, None);
    }

    /// The key is the workspace's alone: the same path gives the same key,
    /// another path another one, each [`KEY_DIGITS`] long.
    #[test]
    fn the_key_is_the_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let key = workspace_key(dir.path());
        assert_eq!(key.len(), KEY_DIGITS);
        assert_eq!(key, workspace_key(dir.path()));
        assert_ne!(key, workspace_key(other.path()));
    }

    #[tokio::test]
    async fn a_stale_file_is_replaced_and_a_live_one_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mob.endpoint");
        check_stale(&path).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = Endpoint {
            address: listener.local_addr().unwrap(),
            token: "t".into(),
            pid: 1,
            workspace: None,
            started_at: None,
        };
        live.write(&path).unwrap();
        assert!(matches!(
            check_stale(&path).await,
            Err(DaemonError::AlreadyRunning { .. })
        ));
        drop(listener);
        check_stale(&path).await.unwrap();
    }

    /// The finding this guards: a probe that timed
    /// out, or a file that could not be read, was taken as stale, so a
    /// daemon could start beside a live one that was slow to accept.
    #[tokio::test]
    async fn only_a_refused_connect_proves_a_file_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mob.endpoint");
        // An address nothing answers at in time (or that is unreachable):
        // not proven stale.
        let silent = Endpoint {
            address: "10.255.255.1:9".parse().unwrap(),
            token: "t".into(),
            pid: 1,
            workspace: None,
            started_at: None,
        };
        silent.write(&path).unwrap();
        let refused = check_stale(&path).await;
        assert!(
            matches!(&refused, Err(DaemonError::EndpointUndecided { file, .. }) if file == &path),
            "{refused:?}"
        );
        // A file that cannot be read is no proof either.
        std::fs::write(&path, "not an endpoint").unwrap();
        assert!(matches!(
            check_stale(&path).await,
            Err(DaemonError::EndpointUndecided { .. })
        ));
    }
}
