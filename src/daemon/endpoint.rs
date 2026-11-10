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
//! serves and `started_at` when it started (RFC 3339, UTC); a file written
//! by an older daemon has neither and still reads. A daemon of another
//! realm than the workspace's own adds `"realm"` (its root, or `memory`). Written (atomically, by
//! rename) once the listener is bound, removed on shutdown. A file left by a daemon that died is stale: the next daemon
//! replaces it when nothing accepts at its address.
//!
//! **The token is the whole access control.** Anyone who reads it can
//! attach, answer `decision/decide` and change the permission mode and the
//! rules; nothing on a loopback TCP connection says which program sent it.
//! So the file lives in the user's state directory (`$XDG_STATE_HOME`, else
//! `~/.local/state`), under a key derived from the canonical workspace path
//! and, for any realm but the workspace's own, the realm's identity
//! ([`workspace_key`], [`RealmKey`]), never inside the workspace:
//! the agent's confined shell is granted the workspace (`.rkat` included),
//! the temp dir, the Cargo home and the user cache dir, none of which holds
//! it. When the shell's confinement plan would grant that directory after
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

/// Which realm of a workspace a daemon serves, as far as the endpoint key
/// and `--list` care: the workspace's own realm (`<workspace>/.rkat/tui`),
/// another realm by its root (`--realm`, `--state-root`, `--isolated`), or
/// an in-memory one (`--realm-backend memory`), which holds nothing a
/// later start could reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealmKey {
    /// The workspace's own realm: the key is the workspace's alone.
    Default,
    /// Another realm, by its root directory.
    Root(PathBuf),
    /// An in-memory realm (its nominal root, for the display).
    Memory(PathBuf),
}

impl RealmKey {
    /// The realm `scope` resolved to.
    #[must_use]
    pub fn of(scope: &crate::bootstrap::RuntimeScope) -> Self {
        let root = scope.realm_root();
        if scope.backend_hint == Some(meerkat_store::RealmBackend::Memory) {
            return Self::Memory(root);
        }
        let default = meerkat_store::realm_paths_in(
            &scope
                .context_root
                .join(crate::bootstrap::scope::WORKSPACE_STATE_DIR),
            crate::bootstrap::scope::WORKSPACE_REALM_ID,
        )
        .root;
        if root == default {
            Self::Default
        } else {
            Self::Root(root)
        }
    }

    /// What `--list` shows for it: `None` for the workspace's own realm.
    #[must_use]
    pub fn describe(&self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::Root(root) => Some(root.display().to_string()),
            Self::Memory(_) => Some("memory".to_string()),
        }
    }

    /// The bytes it adds to the key (`None`: the workspace's alone).
    fn salt(&self) -> Option<Vec<u8>> {
        match self {
            Self::Default => None,
            Self::Root(root) => {
                let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                let mut salt = b"realm:".to_vec();
                salt.extend_from_slice(canonical.as_os_str().as_encoded_bytes());
                Some(salt)
            }
            Self::Memory(root) => {
                let mut salt = b"memory:".to_vec();
                salt.extend_from_slice(root.as_os_str().as_encoded_bytes());
                Some(salt)
            }
        }
    }
}

/// The places of the endpoint file of `workspace`'s `realm`, in the order
/// the daemon tries them and clients look ([`search_dirs`]).
#[must_use]
pub fn candidates(workspace: &Path, realm: &RealmKey) -> Vec<PathBuf> {
    let key = workspace_key(workspace, realm);
    search_dirs()
        .into_iter()
        .map(|dir| dir.join(&key).join(super::ENDPOINT_NAME))
        .collect()
}

/// The key of `workspace`'s `realm`: the first [`KEY_DIGITS`] hex digits
/// of the SHA-256 of the workspace's canonical path (the path as given
/// when it cannot be resolved), followed for any realm but the
/// workspace's own by a NUL and the realm's identity, so an `--isolated`
/// or memory-realm daemon never shares the ordinary daemon's file.
#[must_use]
pub fn workspace_key(workspace: &Path, realm: &RealmKey) -> String {
    use sha2::Digest as _;
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut hasher = sha2::Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    if let Some(salt) = realm.salt() {
        hasher.update([0u8]);
        hasher.update(salt);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(KEY_DIGITS);
    for byte in digest.iter().take(KEY_DIGITS / 2) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The endpoint file of `workspace`'s `realm` under the state directory
/// `state_home`.
#[must_use]
pub fn file_in(state_home: &Path, workspace: &Path, realm: &RealmKey) -> PathBuf {
    state_home
        .join(STATE_DIR)
        .join(workspace_key(workspace, realm))
        .join(super::ENDPOINT_NAME)
}

/// The endpoint file of `workspace`'s `realm` in this user's state
/// directory.
///
/// # Errors
///
/// [`std::io::ErrorKind::NotFound`] when there is no state directory (no
/// `$XDG_STATE_HOME` and no home directory).
pub fn default_file(workspace: &Path, realm: &RealmKey) -> Result<PathBuf, std::io::Error> {
    state_home()
        .map(|state| file_in(&state, workspace, realm))
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
    /// The canonical workspace path the daemon serves (`None`: a file an
    /// older daemon wrote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// When the daemon started, RFC 3339 in UTC (`None`: an older file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// The realm it serves when that is not the workspace's own
    /// ([`RealmKey::describe`]: its root, or `memory`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
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
    match reach(&endpoint).await {
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

/// Connect to `endpoint`'s address (within [`REACH_TIMEOUT`]); only a
/// refused (or reset) connect counts as [`Reach::Refused`].
pub async fn reach(endpoint: &Endpoint) -> Reach {
    match tokio::time::timeout(
        REACH_TIMEOUT,
        tokio::net::TcpStream::connect(endpoint.address),
    )
    .await
    {
        Ok(Ok(_)) => Reach::Accepts,
        Ok(Err(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Reach::Refused
        }
        Ok(Err(err)) => Reach::Unknown(err.to_string()),
        Err(_) => Reach::Unknown(format!("no answer within {}s", REACH_TIMEOUT.as_secs())),
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
            workspace: Some(PathBuf::from("/w")),
            started_at: Some(timestamp(std::time::UNIX_EPOCH)),
            realm: None,
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

    fn scope(extra: &[&str]) -> crate::bootstrap::RuntimeScope {
        use clap::Parser as _;
        let mut argv = vec!["rkat-tui"];
        argv.extend_from_slice(extra);
        crate::bootstrap::resolve_scope(&crate::cli::Args::try_parse_from(argv).unwrap()).unwrap()
    }

    /// The finding this guards: the key was the workspace's alone, so an
    /// `--isolated` or memory-realm daemon took the workspace's endpoint
    /// file and a plain start attached to the throwaway realm.
    #[test]
    fn the_key_tells_the_workspace_realm_from_isolated_and_memory_ones() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let ws = dir.path().to_str().unwrap();
        let own = scope(&["-w", ws]);
        assert_eq!(RealmKey::of(&own), RealmKey::Default);
        assert_eq!(RealmKey::Default.describe(), None);
        let workspace = &own.context_root;
        let plain = workspace_key(workspace, &RealmKey::Default);
        let isolated = scope(&[
            "-w",
            ws,
            "--isolated",
            "--state-root",
            state.path().to_str().unwrap(),
        ]);
        let isolated_key = RealmKey::of(&isolated);
        assert_eq!(isolated_key, RealmKey::Root(isolated.realm_root()));
        assert_eq!(
            isolated_key.describe(),
            Some(isolated.realm_root().display().to_string())
        );
        let memory = scope(&["-w", ws, "--realm-backend", "memory"]);
        let memory_key = RealmKey::of(&memory);
        assert!(matches!(memory_key, RealmKey::Memory(_)), "{memory_key:?}");
        assert_eq!(memory_key.describe().as_deref(), Some("memory"));
        let keys = [
            plain.clone(),
            workspace_key(workspace, &isolated_key),
            workspace_key(workspace, &memory_key),
        ];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[0], keys[2]);
        assert_ne!(keys[1], keys[2]);
        for key in &keys {
            assert_eq!(key.len(), KEY_DIGITS);
        }
        // A second isolated start mints another realm, hence another key.
        let again = scope(&[
            "-w",
            ws,
            "--isolated",
            "--state-root",
            state.path().to_str().unwrap(),
        ]);
        assert_ne!(workspace_key(workspace, &RealmKey::of(&again)), keys[1]);
        // The workspace's own realm keeps the key it always had.
        assert_eq!(plain, workspace_key(workspace, &RealmKey::Default));
    }

    /// A client started with `--isolated` hands its daemon the realm it
    /// minted, not `--isolated` again (which would mint another and leave
    /// the client waiting at the wrong endpoint file).
    #[test]
    fn an_isolated_client_pins_its_realm_for_the_daemon() {
        use clap::Parser as _;
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let argv = [
            "rkat-tui",
            "-w",
            dir.path().to_str().unwrap(),
            "--isolated",
            "--state-root",
            state.path().to_str().unwrap(),
        ];
        let args = crate::cli::Args::try_parse_from(argv).unwrap();
        let minted = crate::bootstrap::resolve_scope(&args).unwrap();
        let pinned = crate::client::launch::pin_realm(&args, &minted);
        assert!(!pinned.isolated);
        let daemon = crate::bootstrap::resolve_scope(&pinned).unwrap();
        assert_eq!(daemon.realm_root(), minted.realm_root());
        assert_eq!(RealmKey::of(&daemon), RealmKey::of(&minted));
        // Anything else goes as it is.
        let plain = crate::cli::Args::try_parse_from(&argv[..3]).unwrap();
        let pinned = crate::client::launch::pin_realm(&plain, &minted);
        assert_eq!(format!("{pinned:?}"), format!("{plain:?}"));
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
            realm: None,
        };
        live.write(&path).unwrap();
        assert!(matches!(
            check_stale(&path).await,
            Err(DaemonError::AlreadyRunning { .. })
        ));
        drop(listener);
        check_stale(&path).await.unwrap();
    }

    /// The finding this guards (second review, item 9): a probe that timed
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
            realm: None,
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
