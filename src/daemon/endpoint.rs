//! The endpoint file: where a running daemon listens and the token a
//! client must present ([`super::handshake`]).
//!
//! `<state>/rkat-tui/<key>/mob.endpoint` ([`default_file`]), mode 0600 in
//! a directory of mode 0700, one line of JSON:
//!
//! ```json
//! {"address":"127.0.0.1:43123","token":"<64 lowercase hex digits>","pid":12345}
//! ```
//!
//! Written (atomically, by rename) once the listener is bound, removed on
//! shutdown. A file left by a daemon that died is stale: the next daemon
//! replaces it when nothing accepts at its address.
//!
//! **The token is the whole access control.** Anyone who reads it can
//! attach, answer `decision/decide` and change the permission mode and the
//! rules; nothing on a loopback TCP connection says which program sent it.
//! So the file lives in the user's state directory (`$XDG_STATE_HOME`, else
//! `~/.local/state`), under a key derived from the canonical workspace path
//! (the first 32 hex digits of its SHA-256), never inside the workspace:
//! the agent's confined shell is granted the workspace (`.rkat` included),
//! the temp dir, the Cargo home and the user cache dir, none of which holds
//! it. The daemon refuses to start when the shell's confinement plan would
//! grant the file's directory after all ([`exposing_grant`]).

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

/// The key of `workspace`: the first [`KEY_DIGITS`] hex digits of the
/// SHA-256 of its canonical path (the path as given when it cannot be
/// resolved).
#[must_use]
pub fn workspace_key(workspace: &Path) -> String {
    use sha2::Digest as _;
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let digest = sha2::Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let mut hex = String::with_capacity(KEY_DIGITS);
    for byte in digest.iter().take(KEY_DIGITS / 2) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The endpoint file of `workspace` under the state directory `state_home`.
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
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
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
}

impl Endpoint {
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
        use std::io::Write as _;
        #[cfg(unix)]
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        create_private_dir(dir)?;
        let tmp = temp_path(path);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let written = options.open(&tmp).and_then(|mut file| {
            #[cfg(unix)]
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            let mut line = serde_json::to_string(self).map_err(std::io::Error::other)?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
            file.sync_all()
        });
        written
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|err| {
                let _ = std::fs::remove_file(&tmp);
                DaemonError::io("write endpoint file", path, err)
            })
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// A fresh random token: [`TOKEN_BYTES`] from the OS, hex-encoded.
///
/// # Errors
///
/// [`DaemonError::Token`] when the OS randomness cannot be read.
pub fn new_token() -> Result<String, DaemonError> {
    use std::io::Read as _;
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(DaemonError::Token)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(TOKEN_BYTES * 2), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

/// Refuse when the endpoint file at `path` names a daemon that still
/// accepts; a stale or unreadable file is left for [`Endpoint::write`] to
/// replace.
///
/// # Errors
///
/// [`DaemonError::AlreadyRunning`].
pub async fn check_stale(path: &Path) -> Result<(), DaemonError> {
    let Ok(endpoint) = Endpoint::read(path) else {
        return Ok(());
    };
    if answers(&endpoint).await {
        return Err(DaemonError::AlreadyRunning {
            address: endpoint.address,
            pid: endpoint.pid,
            file: path.to_path_buf(),
        });
    }
    tracing::info!(file = %path.display(), address = %endpoint.address, "replacing a stale endpoint file");
    Ok(())
}

/// Whether something accepts at `endpoint`'s address (within 2 s).
pub async fn answers(endpoint: &Endpoint) -> bool {
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect(endpoint.address),
    )
    .await;
    matches!(probe, Ok(Ok(_)))
}

/// Where a daemon of an earlier version wrote its endpoint file: inside
/// the workspace, `<workspace>/.rkat/tui/mob.endpoint`, which the agent's
/// confined shell can read.
#[must_use]
pub fn legacy_file(workspace: &Path) -> PathBuf {
    workspace
        .join(".rkat")
        .join("tui")
        .join(super::ENDPOINT_NAME)
}

/// What the legacy endpoint file ([`legacy_file`]) says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Legacy {
    /// No legacy file (or it was stale and is now removed).
    Clear,
    /// A daemon of an earlier version still answers at its address.
    Live(Endpoint),
}

/// Look at the legacy endpoint file of `workspace`: a daemon of an earlier
/// version that still answers there is [`Legacy::Live`]; a file nothing
/// answers for (or one that cannot be parsed) is removed, as it holds a
/// token the agent's shell could read, and that is logged.
pub async fn check_legacy(workspace: &Path) -> Legacy {
    let file = legacy_file(workspace);
    if !file.exists() {
        return Legacy::Clear;
    }
    if let Ok(endpoint) = Endpoint::read(&file)
        && answers(&endpoint).await
    {
        return Legacy::Live(endpoint);
    }
    match std::fs::remove_file(&file) {
        Ok(()) => tracing::info!(
            file = %file.display(),
            "removed the stale endpoint file an earlier rkat-tui-mob left in the workspace"
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(
            file = %file.display(),
            error = %err,
            "cannot remove the stale endpoint file an earlier rkat-tui-mob left in the workspace"
        ),
    }
    Legacy::Clear
}

/// Refuse to start while a daemon of an earlier version serves `workspace`
/// from its legacy endpoint file ([`check_legacy`]; a stale legacy file is
/// removed).
///
/// # Errors
///
/// [`DaemonError::AlreadyRunning`] naming the legacy file and its pid.
pub async fn refuse_legacy(workspace: &Path) -> Result<(), DaemonError> {
    match check_legacy(workspace).await {
        Legacy::Clear => Ok(()),
        Legacy::Live(endpoint) => Err(DaemonError::AlreadyRunning {
            address: endpoint.address,
            pid: endpoint.pid,
            file: legacy_file(workspace),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_one_private_json_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("mob.endpoint");
        let endpoint = Endpoint {
            address: "127.0.0.1:4242".parse().unwrap(),
            token: new_token().unwrap(),
            pid: 7,
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
        };
        live.write(&path).unwrap();
        assert!(matches!(
            check_stale(&path).await,
            Err(DaemonError::AlreadyRunning { .. })
        ));
        drop(listener);
        check_stale(&path).await.unwrap();
    }

    /// The upgrade path: a daemon of the previous version wrote its
    /// endpoint (and token) into the workspace.
    #[tokio::test]
    async fn a_stale_legacy_file_is_removed_and_a_live_legacy_daemon_refuses() {
        let workspace = tempfile::tempdir().unwrap();
        let legacy = legacy_file(workspace.path());
        assert!(legacy.ends_with(".rkat/tui/mob.endpoint"));
        assert_eq!(check_legacy(workspace.path()).await, Legacy::Clear);
        refuse_legacy(workspace.path()).await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = Endpoint {
            address: listener.local_addr().unwrap(),
            token: "t".into(),
            pid: 4242,
        };
        live.write(&legacy).unwrap();
        assert_eq!(
            check_legacy(workspace.path()).await,
            Legacy::Live(live.clone())
        );
        let refused = refuse_legacy(workspace.path()).await;
        assert!(
            matches!(&refused, Err(DaemonError::AlreadyRunning { pid: 4242, file, .. }) if *file == legacy),
            "{refused:?}"
        );
        let text = refused.unwrap_err().to_string();
        assert!(
            text.contains("pid 4242") && text.contains("/daemon stop"),
            "{text}"
        );
        assert!(legacy.exists(), "a live daemon keeps its file");

        drop(listener);
        refuse_legacy(workspace.path()).await.unwrap();
        assert!(
            !legacy.exists(),
            "the stale legacy file (a token) is removed"
        );
        // An unparsable one holds nothing to keep either.
        std::fs::write(&legacy, "garbage").unwrap();
        assert_eq!(check_legacy(workspace.path()).await, Legacy::Clear);
        assert!(!legacy.exists());
    }
}
