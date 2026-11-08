//! The endpoint file: where a running daemon listens and the token a
//! client must present ([`super::handshake`]).
//!
//! `<workspace>/.rkat/tui/mob.endpoint`, mode 0600, one line of JSON:
//!
//! ```json
//! {"address":"127.0.0.1:43123","token":"<64 lowercase hex digits>","pid":12345}
//! ```
//!
//! Written (atomically, by rename) once the listener is bound, removed on
//! shutdown. A file left by a daemon that died is stale: the next daemon
//! replaces it when nothing accepts at its address.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::DaemonError;

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
        std::fs::create_dir_all(dir)
            .map_err(|err| DaemonError::io("create endpoint directory", dir, err))?;
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
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect(endpoint.address),
    )
    .await;
    if matches!(probe, Ok(Ok(_))) {
        return Err(DaemonError::AlreadyRunning {
            address: endpoint.address,
            file: path.to_path_buf(),
        });
    }
    tracing::info!(file = %path.display(), address = %endpoint.address, "replacing a stale endpoint file");
    Ok(())
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
}
