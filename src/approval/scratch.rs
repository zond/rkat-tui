//! Each agent's private scratch directory.
//!
//! Every agent of the daemon gets one directory of its own under the
//! system temp directory, `<temp>/rkat-tui-<12 hex digits>/`, created
//! `0700`, shared by its conversations, its helpers and their forks. Its
//! shell (confined or not) gets it as `TMPDIR`, `TMP` and `TEMP`, so test
//! suites, compilers and the agent's own redirects write their temp files
//! there; the agent is told its path in its instructions. The gate treats
//! it as the agent's own: a file tool's write there is an in-project edit
//! (tier 2), a read tier 1, a shell redirect into it read-only shell, and
//! the classifier is told the directory is the session's
//! ([`super::prompts::SCRATCH_NOTE`]).
//!
//! **Why under the temp directory and not the realm.** The confined shell
//! must be able to write it, and the sandbox already grants the temp
//! directory; the realm is sealed from the agent and the confinement plan
//! refuses any grant that reaches it (the endpoint and the attach token
//! live there). The temp directory is shared with other local users, so
//! the name is fixed per agent (a resumed agent's instructions stay true)
//! and the directory is trusted only when it is a real directory owned by
//! this user with no access for anyone else: one somebody else made first,
//! or a link, is never used (the agent then has no scratch directory, and
//! the daemon logs why).
//!
//! The directory goes when its agent is retired and when the daemon
//! stops (best effort: the removal never follows a link out of it); a
//! resumed agent gets it back, empty, under the same name.

use std::path::{Path, PathBuf};

/// The name every scratch directory starts with.
pub const SCRATCH_PREFIX: &str = "rkat-tui-";

/// The variables of the agent's shell that name its scratch directory.
pub const SCRATCH_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];

/// Why an agent has no scratch directory.
#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    /// There is no usable temp directory to hold it.
    #[error("no temp directory to hold it")]
    NoBase,
    /// The directory could not be created.
    #[error("creating {}: {source}", .path.display())]
    Create {
        /// The directory.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// Something is in its place that this user did not make private.
    #[error(
        "{} exists but is not a private directory of this user (a link, another user's, or open to others); remove it to get a scratch directory",
        .0.display()
    )]
    NotPrivate(PathBuf),
}

/// Where the daemon's agents' scratch directories live, and how each is
/// named. Dropped (the daemon stops), it removes the directories it made
/// or adopted.
#[derive(Debug)]
pub struct ScratchDirs {
    /// The canonical temp directory (`None`: scratch directories are off).
    base: Option<PathBuf>,
    /// What makes the names of this daemon's agents its own (the realm).
    salt: String,
    /// The agents whose directory [`Self::ensure`] made or adopted.
    made: parking_lot::Mutex<std::collections::HashSet<String>>,
}

impl Drop for ScratchDirs {
    fn drop(&mut self) {
        let made = std::mem::take(&mut *self.made.lock());
        for agent in made {
            self.remove(&agent);
        }
    }
}

impl ScratchDirs {
    /// Scratch directories under the canonical `base`, named for agents of
    /// `salt` (the realm's root).
    #[must_use]
    pub fn new(base: Option<PathBuf>, salt: impl Into<String>) -> Self {
        Self {
            base: base.and_then(|base| std::fs::canonicalize(base).ok()),
            salt: salt.into(),
            made: parking_lot::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Scratch directories under this process's temp directory.
    #[must_use]
    pub fn in_temp(salt: impl Into<String>) -> Self {
        Self::new(Some(std::env::temp_dir()), salt)
    }

    /// No scratch directories.
    #[must_use]
    pub fn off() -> Self {
        Self::new(None, String::new())
    }

    /// The scratch directory `agent` has or would have.
    #[must_use]
    pub fn path_for(&self, agent: &str) -> Option<PathBuf> {
        let base = self.base.as_ref()?;
        let digest = crate::hex::sha256(format!("{}\0{agent}", self.salt).as_bytes());
        Some(base.join(format!("{SCRATCH_PREFIX}{}", &digest[..12])))
    }

    /// `agent`'s scratch directory, created private (`0700`) when it is
    /// not there yet.
    ///
    /// # Errors
    ///
    /// [`ScratchError`] when there is no temp directory, the directory
    /// cannot be created, or something not private to this user is in its
    /// place.
    pub fn ensure(&self, agent: &str) -> Result<PathBuf, ScratchError> {
        let path = self.path_for(agent).ok_or(ScratchError::NoBase)?;
        match create_private(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if !is_private_dir(&path) {
                    return Err(ScratchError::NotPrivate(path));
                }
            }
            Err(source) => return Err(ScratchError::Create { path, source }),
        }
        self.made.lock().insert(agent.to_string());
        Ok(path)
    }

    /// `agent`'s scratch directory when it exists and is private to this
    /// user: what the gate trusts.
    #[must_use]
    pub fn existing(&self, agent: &str) -> Option<PathBuf> {
        self.path_for(agent).filter(|path| is_private_dir(path))
    }

    /// Remove `agent`'s scratch directory and everything in it (best
    /// effort; a link in it is removed, never followed). Nothing happens
    /// when the directory is not a private one of this user.
    pub fn remove(&self, agent: &str) {
        self.made.lock().remove(agent);
        let Some(path) = self.existing(agent) else {
            return;
        };
        // `remove_dir_all` does not follow symbolic links, also not one
        // swapped in while it runs.
        if let Err(err) = std::fs::remove_dir_all(&path) {
            tracing::warn!(path = %path.display(), error = %err, "the agent's scratch directory could not be removed");
        }
    }

    /// `env` (the shell's environment, or none for an unconfined shell,
    /// which inherits the daemon's) with the scratch directory `dir` as
    /// [`SCRATCH_VARS`].
    #[must_use]
    pub fn shell_env(
        env: Option<&std::collections::HashMap<String, String>>,
        dir: &Path,
    ) -> std::collections::HashMap<String, String> {
        let mut env = env.cloned().unwrap_or_default();
        for var in SCRATCH_VARS {
            env.insert(var.to_string(), dir.display().to_string());
        }
        env
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "scratch directories need a Unix host",
    ))
}

/// Whether `path` itself (not a link's target) is a directory owned by this
/// user with mode `0700`: only its owner can enter it.
#[cfg(unix)]
fn is_private_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_dir()
            && meta.uid() == nix::unistd::geteuid().as_raw()
            && meta.mode() & 0o777 == 0o700
    })
}

#[cfg(not(unix))]
fn is_private_dir(_path: &Path) -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn each_agent_gets_its_own_private_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ScratchDirs::new(Some(temp.path().to_path_buf()), "/realm/a");
        let main = dirs.ensure("main").unwrap();
        let other = dirs.ensure("main-bbbb").unwrap();
        assert_ne!(main, other);
        assert!(
            main.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(SCRATCH_PREFIX)
        );
        assert_eq!(dirs.ensure("main").unwrap(), main, "stable per agent");
        assert_eq!(dirs.existing("main"), Some(main.clone()));
        // Another realm's agent of the same name has another directory.
        let elsewhere = ScratchDirs::new(Some(temp.path().to_path_buf()), "/realm/b");
        assert_ne!(elsewhere.path_for("main").unwrap(), main);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&main).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::write(main.join("x"), "x").unwrap();
        dirs.remove("main");
        assert!(!main.exists());
        assert_eq!(dirs.existing("main"), None);
        assert!(ScratchDirs::off().ensure("main").is_err());
        // The daemon stops: what it made goes.
        let again = dirs.ensure("main").unwrap();
        drop(dirs);
        assert!(!again.exists() && !other.exists());
    }

    /// A directory somebody else put in the place, open to others, or a
    /// link, is never used and never removed through.
    #[cfg(unix)]
    #[test]
    fn a_planted_directory_or_link_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let dirs = ScratchDirs::new(Some(temp.path().to_path_buf()), "/realm");
        let path = dirs.path_for("main").unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            dirs.ensure("main"),
            Err(ScratchError::NotPrivate(_))
        ));
        assert_eq!(dirs.existing("main"), None);
        std::fs::remove_dir(&path).unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), "keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(
            dirs.ensure("main"),
            Err(ScratchError::NotPrivate(_))
        ));
        dirs.remove("main");
        assert!(target.join("keep").exists(), "never removed through a link");
    }

    #[test]
    fn the_shell_gets_the_directory_as_its_temp_variables() {
        let base: std::collections::HashMap<String, String> =
            [("PATH".to_string(), "/bin".to_string())].into();
        let env = ScratchDirs::shell_env(Some(&base), Path::new("/tmp/rkat-tui-x"));
        for var in SCRATCH_VARS {
            assert_eq!(env.get(var).map(String::as_str), Some("/tmp/rkat-tui-x"));
        }
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(ScratchDirs::shell_env(None, Path::new("/s")).len(), 3);
    }
}
