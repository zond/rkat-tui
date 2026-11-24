//! Where a workspace's realm lives: `<config dir>/workspaces/<id>/`.
//!
//! The realm (the engine's stores, the realm `config.toml` and `tui.toml`,
//! the conversation log, the helper bindings, the cursors, the notes, the
//! audit log) is kept out of the workspace, so that no grant of the agent's
//! confined shell reaches it. `<id>` is random, drawn once when the
//! workspace first starts; nothing about it derives from the workspace's
//! path.
//!
//! Each realm directory holds a manifest, [`MANIFEST_FILE`]
//! (`workspace.toml`): the canonical path of the workspace it serves, its
//! name, its git remote when one is cheap to read, when it was created and
//! last used. The manifests are the only authority for which workspace owns
//! which realm: nothing in the workspace is read for it (the agent's shell
//! can write there and could point itself at another workspace's sessions).
//! A start finds its realm by scanning the manifests for its canonical path
//! ([`find`]); two manifests naming one path refuse the start
//! ([`HomeError::Duplicate`], naming both), an unreadable manifest refuses
//! it too (it may name this workspace: [`HomeError::Unreadable`]).
//!
//! A workspace that moved has no realm under its new path. Re-homing is
//! explicit (`/rehome` in the TUI, `rkat-tui --rehome <id|old path>`, see
//! `crate::client::rehome`): it rewrites a manifest's path ([`commit`]).
//! A workspace that already has a realm takes another one only when its
//! own is provably empty ([`holds`]); that one is removed. A start in a
//! workspace with no realm while realms whose workspace is gone exist
//! ([`Home::orphaned`]) creates a new one and offers those ([`offer`]);
//! nothing is adopted silently.
//!
//! Every scan that may create or rewrite a manifest runs under an exclusive
//! lock on `workspaces/.lock`, so two starts of one new workspace make one
//! realm.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::dirs::GlobalDirs;

/// The directory under the config directory that holds the realms.
pub const WORKSPACES_DIR: &str = "workspaces";

/// The manifest's file name in a realm directory.
pub const MANIFEST_FILE: &str = "workspace.toml";

/// The lock file under [`WORKSPACES_DIR`].
const LOCK_FILE: &str = ".lock";

/// Random bytes of a realm id (hex-encoded: twice as many digits).
const ID_BYTES: usize = 8;

/// Longest git remote kept in a manifest.
const MAX_REMOTE: usize = 200;

/// Why a workspace's realm cannot be found, made or re-homed.
#[derive(Debug, thiserror::Error)]
pub enum HomeError {
    /// There is no config directory to keep realms in.
    #[error(
        "no config directory for this workspace's state (no home directory): pass --config-dir"
    )]
    NoConfigDir,
    /// Two realms name the same workspace.
    #[error(
        "two realms name the workspace {}: {} and {}; remove or re-home one of them (rkat-tui --list shows both)",
        path.display(),
        first.display(),
        second.display()
    )]
    Duplicate {
        /// The workspace.
        path: PathBuf,
        /// The first realm directory.
        first: PathBuf,
        /// The second.
        second: PathBuf,
    },
    /// A manifest cannot be read or parsed: it may name this workspace.
    #[error(
        "cannot read the realm manifest {}: {why}; it may belong to this workspace, so nothing starts until it is repaired or its directory removed",
        file.display()
    )]
    Unreadable {
        /// The manifest.
        file: PathBuf,
        /// Why.
        why: String,
    },
    /// A filesystem operation failed.
    #[error("{context} {}: {source}", path.display())]
    Io {
        /// What was attempted.
        context: &'static str,
        /// On which path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// `--rehome`/`/rehome from` names no realm.
    #[error(
        "`{0}` names no realm: name one by its id, its directory or the workspace path it served (rkat-tui --list shows them; /rehome without an argument picks one)"
    )]
    UnknownRealm(String),
    /// `/rehome` names a directory to move to that does not exist (and,
    /// read either way, no realm either).
    #[error("{}", missing_words(reference, path, *realm_too))]
    NoSuchDirectory {
        /// What was given.
        reference: String,
        /// The absolute path it reads as.
        path: PathBuf,
        /// It was read as a realm too, and names none.
        realm_too: bool,
    },
    /// `/rehome` names a path to move to that is not a directory.
    #[error("{}", not_a_directory_words(reference, path, *realm_too))]
    NotADirectory {
        /// What was given.
        reference: String,
        /// The absolute path it reads as.
        path: PathBuf,
        /// It was read as a realm too, and names none.
        realm_too: bool,
    },
    /// `/rehome` starts with `~` and no home directory is known.
    #[error("`{0}` starts with ~, but no home directory is known to expand it: give the full path")]
    NoHome(String),
    /// A workspace inside the config directory, or one holding it: the
    /// config directory holds every workspace's state.
    #[error("{}", config_words(workspace, config, *inside))]
    InConfig {
        /// The workspace.
        workspace: PathBuf,
        /// The config directory.
        config: PathBuf,
        /// The workspace lies inside it (else it holds it).
        inside: bool,
    },
    /// `--rehome`/`/rehome` names several realms.
    #[error("`{reference}` names several realms ({}); name one by its id", ids.join(", "))]
    Ambiguous {
        /// What was given.
        reference: String,
        /// The realms it names.
        ids: Vec<String>,
    },
    /// `/rehome` reads both ways: a realm to adopt here, and a directory
    /// to move this workspace's state to.
    #[error(
        "`{reference}` reads two ways: adopt realm {id} here (/rehome from {reference}), or move this workspace's state to {} (/rehome to {reference}); say which",
        path.display()
    )]
    TwoReadings {
        /// What was given.
        reference: String,
        /// The realm it names.
        id: String,
        /// The directory it names.
        path: PathBuf,
    },
    /// The realm already serves the workspace.
    #[error("realm {0} already serves this workspace")]
    AlreadyHere(String),
    /// The workspace would reach the realm: its shell's workspace grant
    /// would hold the realm directory, or lie inside it.
    #[error(
        "{} would reach realm {id} ({}): a workspace there could read and write its own state; choose another directory",
        workspace.display(),
        dir.display()
    )]
    Exposed {
        /// The realm.
        id: String,
        /// Its directory.
        dir: PathBuf,
        /// The workspace.
        workspace: PathBuf,
    },
    /// A realm that would be replaced holds something.
    #[error(
        "the destination's realm {id} is not empty: {what}; a re-home replaces only an empty realm unless you confirm deleting it, so nothing was changed (to delete that state from the shell: rkat-tui --remove-state {id} --yes)"
    )]
    Holds {
        /// The realm.
        id: String,
        /// What it holds.
        what: String,
    },
    /// The realms changed while the re-home ran.
    #[error("the realms changed while re-homing ({0}); nothing was changed, try again")]
    Changed(String),
    /// A daemon may hold a realm.
    #[error(
        "realm {id} may be in use: {why}; stop its daemon (rkat-tui --list shows it) and try again"
    )]
    InUse {
        /// The realm.
        id: String,
        /// What the check found.
        why: String,
    },
}

/// What names no realm says first, when a reference was read as one too.
fn no_realm_first(reference: &str, realm_too: bool) -> String {
    if realm_too {
        format!(
            "`{reference}` names no realm (by its id, its directory or the workspace path it served; rkat-tui --list shows them, /rehome without an argument picks one), and "
        )
    } else {
        String::new()
    }
}

/// [`HomeError::NoSuchDirectory`] in words.
fn missing_words(reference: &str, path: &Path, realm_too: bool) -> String {
    format!(
        "{}{} does not exist: check the spelling, or create the directory first (mkdir -p {}) to move this workspace's state there",
        no_realm_first(reference, realm_too),
        path.display(),
        path.display()
    )
}

/// [`HomeError::NotADirectory`] in words.
fn not_a_directory_words(reference: &str, path: &Path, realm_too: bool) -> String {
    format!(
        "{}{} is not a directory: name the directory to move this workspace's state to",
        no_realm_first(reference, realm_too),
        path.display()
    )
}

/// [`HomeError::InConfig`] in words.
fn config_words(workspace: &Path, config: &Path, inside: bool) -> String {
    if inside {
        format!(
            "{} lies inside rkat-tui's config directory {}, which holds every workspace's state: no workspace is served from there; to adopt a realm kept there, /rehome from <its id> (/rehome without an argument picks one); to move this workspace's state, name a directory outside it",
            workspace.display(),
            config.display()
        )
    } else {
        format!(
            "{} holds rkat-tui's config directory {}, which holds every workspace's state: a workspace there could read and rewrite all of it; choose a directory that does not contain it",
            workspace.display(),
            config.display()
        )
    }
}

impl HomeError {
    fn io(context: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            context,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A realm directory's manifest. Read strictly: a manifest of another
/// shape is [`HomeError::Unreadable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The canonical path of the workspace the realm serves.
    pub path: PathBuf,
    /// The workspace directory's name, for people.
    pub name: String,
    /// The workspace's `origin` remote when it had one (credentials
    /// removed), for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    /// When the realm was made (RFC 3339, UTC).
    pub created: String,
    /// When a start last used it (RFC 3339, UTC).
    pub last_used: String,
}

impl Manifest {
    /// A new manifest for `workspace` (canonical), made now.
    fn new(workspace: &Path) -> Self {
        let now = now();
        Self {
            path: workspace.to_path_buf(),
            name: name_of(workspace),
            git_remote: git_remote(workspace),
            created: now.clone(),
            last_used: now,
        }
    }
}

/// One realm directory and its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// The realm id (the directory's name).
    pub id: String,
    /// The realm directory.
    pub dir: PathBuf,
    /// Its manifest.
    pub manifest: Manifest,
}

impl Home {
    /// Whether the workspace it served is gone: its manifest names a path
    /// that does not exist (proven: a path that cannot be looked at is not
    /// gone).
    #[must_use]
    pub fn orphaned(&self) -> bool {
        matches!(self.manifest.path.try_exists(), Ok(false))
    }

    /// One line for people: id, the workspace, last use.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut line = if self.orphaned() {
            format!("{}\t{} (gone)", self.id, self.manifest.path.display())
        } else {
            format!("{}\t{}", self.id, self.manifest.path.display())
        };
        if let Some(remote) = &self.manifest.git_remote {
            line.push_str(&format!("\t{remote}"));
        }
        if !self.manifest.last_used.is_empty() {
            line.push_str(&format!("\tlast used {}", self.manifest.last_used));
        }
        line
    }

    fn write(&self) -> Result<(), HomeError> {
        write_manifest(&self.dir, &self.manifest)
    }
}

/// The directory holding every realm of the config directory `global`.
#[must_use]
pub fn root(global: &GlobalDirs) -> PathBuf {
    global.config.join(WORKSPACES_DIR)
}

/// Now, as a manifest records it.
fn now() -> String {
    chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn name_of(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The `origin` remote of the repository at `workspace`, read from
/// `.git/config` (a worktree's or a submodule's `.git` file is not
/// followed): only for people to recognise the realm, never trusted.
/// Credentials in the URL are removed, control characters dropped.
fn git_remote(workspace: &Path) -> Option<String> {
    let text = std::fs::read_to_string(workspace.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if !in_origin {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "url" {
            return Some(clean_remote(value.trim()));
        }
    }
    None
}

/// `url` without credentials
/// ([`remote_without_credentials`](crate::approval::classifier::remote_without_credentials))
/// or control characters, cut to [`MAX_REMOTE`].
fn clean_remote(url: &str) -> String {
    crate::approval::classifier::remote_without_credentials(url)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REMOTE)
        .collect()
}

/// A fresh realm id: [`ID_BYTES`] from the OS, hex-encoded.
fn new_id() -> Result<String, HomeError> {
    crate::hex::random::<ID_BYTES>()
        .map_err(|err| HomeError::io("draw a realm id from", Path::new("/dev/urandom"), err))
}

/// Create `dir` (and missing parents) with mode 0700; an existing
/// directory is left as it is.
fn create_private_dir(dir: &Path) -> Result<(), HomeError> {
    crate::paths::create_private_dir(dir)
        .map_err(|err| HomeError::io("create the realm directory", dir, err))
}

fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<(), HomeError> {
    let file = dir.join(MANIFEST_FILE);
    let text = toml::to_string_pretty(manifest).map_err(|err| {
        HomeError::io(
            "encode the realm manifest",
            &file,
            std::io::Error::new(std::io::ErrorKind::InvalidData, err),
        )
    })?;
    crate::paths::write_atomic(
        &file,
        text.as_bytes(),
        crate::paths::FileMode::Private,
        Some(dir),
    )
    .map_err(|err| {
        HomeError::io(
            "write the realm manifest",
            &file,
            std::io::Error::other(err),
        )
    })
}

/// The exclusive lock on the realms of one config directory, released
/// when dropped.
pub struct Lock {
    #[cfg(unix)]
    _held: nix::fcntl::Flock<std::fs::File>,
}

impl Lock {
    /// Take the lock under `root` (created 0700 when missing), waiting for
    /// another holder.
    ///
    /// # Errors
    ///
    /// [`HomeError::Io`] when the directory or the lock file cannot be made
    /// or locked.
    pub fn take(root: &Path) -> Result<Self, HomeError> {
        create_private_dir(root)?;
        let path = root.join(LOCK_FILE);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(&path)
            .map_err(|err| HomeError::io("open the realm lock", &path, err))?;
        #[cfg(unix)]
        {
            let held = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).map_err(
                |(_, errno)| {
                    HomeError::io("lock the realms under", root, std::io::Error::from(errno))
                },
            )?;
            Ok(Self { _held: held })
        }
        #[cfg(not(unix))]
        {
            drop(file);
            Ok(Self {})
        }
    }
}

/// Every realm under `root` with a manifest, sorted by id. A directory
/// without a manifest is no realm (one is written before anything else);
/// names starting with `.` are skipped.
///
/// # Errors
///
/// [`HomeError::Unreadable`] for a manifest that cannot be read or parsed
/// (it may name the workspace looked for), [`HomeError::Io`] when `root`
/// cannot be listed.
pub fn scan(root: &Path) -> Result<Vec<Home>, HomeError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(HomeError::io("list the realms in", root, err)),
    };
    let mut homes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| HomeError::io("list the realms in", root, err))?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        let file = dir.join(MANIFEST_FILE);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => continue,
            Err(err) => {
                return Err(HomeError::Unreadable {
                    file,
                    why: err.to_string(),
                });
            }
        };
        if meerkat_store::sanitize_realm_id(&id) != id {
            return Err(HomeError::Unreadable {
                file,
                why: format!("`{id}` is not a realm id"),
            });
        }
        let manifest: Manifest = toml::from_str(&text).map_err(|err| HomeError::Unreadable {
            file: file.clone(),
            why: err.to_string(),
        })?;
        homes.push(Home { id, dir, manifest });
    }
    homes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(homes)
}

/// The realm of `workspace` (canonical) among `homes`.
///
/// # Errors
///
/// [`HomeError::Duplicate`] when two name it.
pub fn find_in<'a>(homes: &'a [Home], workspace: &Path) -> Result<Option<&'a Home>, HomeError> {
    let mut found = homes.iter().filter(|home| home.manifest.path == workspace);
    let first = found.next();
    if let (Some(first), Some(second)) = (first, found.next()) {
        return Err(HomeError::Duplicate {
            path: workspace.to_path_buf(),
            first: first.dir.clone(),
            second: second.dir.clone(),
        });
    }
    Ok(first)
}

/// The realm of `workspace` (canonical) under `root`, without making one.
///
/// # Errors
///
/// See [`scan`] and [`find_in`].
pub fn find(root: &Path, workspace: &Path) -> Result<Option<Home>, HomeError> {
    let homes = scan(root)?;
    Ok(find_in(&homes, workspace)?.cloned())
}

/// What [`open_or_create`] found.
#[derive(Debug, Clone)]
pub struct Opened {
    /// The workspace's realm.
    pub home: Home,
    /// Whether it was made by this call.
    pub created: bool,
    /// The realms whose workspace is gone (when one was made: the offer).
    pub orphans: Vec<Home>,
}

/// The realm of `workspace` (canonical) under `root`: the one whose
/// manifest names it (its last use recorded now), else a new one, under the
/// lock.
///
/// # Errors
///
/// See [`scan`] and [`find_in`]; [`HomeError::Io`] when the realm or its
/// manifest cannot be written.
pub fn open_or_create(root: &Path, workspace: &Path) -> Result<Opened, HomeError> {
    let _lock = Lock::take(root)?;
    let homes = scan(root)?;
    if let Some(home) = find_in(&homes, workspace)? {
        let mut home = home.clone();
        home.manifest.last_used = now();
        if let Err(err) = home.write() {
            // The last use is for people; a manifest that cannot be
            // rewritten still names the workspace.
            tracing::warn!(error = %err, "the realm manifest's last use was not recorded");
        }
        return Ok(Opened {
            home,
            created: false,
            orphans: Vec::new(),
        });
    }
    let orphans = homes.into_iter().filter(Home::orphaned).collect();
    let home = loop {
        let id = new_id()?;
        let dir = root.join(&id);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => {
                break Home {
                    id,
                    dir,
                    manifest: Manifest::new(workspace),
                };
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(HomeError::io("create the realm directory", &dir, err)),
        }
    };
    home.write()?;
    Ok(Opened {
        home,
        created: true,
        orphans,
    })
}

/// The start-up notice offering `orphans` to a workspace that got a new
/// realm (`None` when there are none).
#[must_use]
pub fn offer(orphans: &[Home]) -> Option<String> {
    if orphans.is_empty() {
        return None;
    }
    let mut text = String::from(
        "this workspace had no saved state, so it starts with a new one; the state of these workspaces whose directory is gone is kept:",
    );
    for home in orphans {
        text.push_str("\n  ");
        text.push_str(&home.describe().replace('\t', "  "));
    }
    text.push_str(
        "\nif one of them is this workspace moved, /rehome picks it to adopt here (or /rehome from <id>; rkat-tui --rehome <id> from the shell); the new state, still empty, is removed",
    );
    Some(text)
}

/// Where a `/rehome` reference is read: the realms' directory, the
/// directory a relative path is taken against, the workspace re-homing
/// (canonical) and the home directory a leading `~` stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// The directory holding the realms ([`root`]).
    pub root: PathBuf,
    /// What a relative path is taken against.
    pub base: PathBuf,
    /// The workspace re-homing (canonical).
    pub workspace: PathBuf,
    /// What `~` expands to (`None`: a reference starting with `~` is
    /// refused, [`HomeError::NoHome`]).
    pub home: Option<PathBuf>,
}

impl Context {
    /// The context of `workspace` under `root`, relative paths against
    /// `base`, `~` the user's home directory.
    #[must_use]
    pub fn new(root: &Path, base: &Path, workspace: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            base: base.to_path_buf(),
            workspace: workspace.to_path_buf(),
            home: dirs::home_dir(),
        }
    }

    /// `reference` as an absolute path: a leading `~` expanded
    /// ([`crate::paths::expand_tilde_in`]), a relative path joined to
    /// [`Self::base`], neither normalized nor resolved.
    fn absolute(&self, reference: &str) -> Result<PathBuf, HomeError> {
        let expanded = crate::paths::expand_tilde_in(reference, self.home.as_deref())
            .map_err(|_| HomeError::NoHome(reference.to_string()))?;
        Ok(self.base.join(expanded))
    }
}

/// The config directory of the realms' directory `root` (it holds every
/// workspace's state, its realms and the user's own `tui.toml`).
#[must_use]
pub fn config_of(root: &Path) -> &Path {
    root.parent().unwrap_or(root)
}

/// Refuse a workspace `workspace` inside the config directory `config`
/// (no workspace is served from there: its shell would reach every
/// realm).
///
/// # Errors
///
/// [`HomeError::InConfig`].
pub fn check_not_in_config(config: &Path, workspace: &Path) -> Result<(), HomeError> {
    let config = super::scope::canonical_prefix(config);
    let workspace = super::scope::canonical_prefix(workspace);
    if workspace.starts_with(&config) {
        return Err(HomeError::InConfig {
            workspace,
            config,
            inside: true,
        });
    }
    Ok(())
}

/// Refuse a re-home's workspace `workspace` that lies inside the config
/// directory `config` or holds it: either way a workspace there reaches
/// every realm, not only the one it is given.
///
/// # Errors
///
/// [`HomeError::InConfig`].
pub fn check_clear_of_config(config: &Path, workspace: &Path) -> Result<(), HomeError> {
    check_not_in_config(config, workspace)?;
    let config = super::scope::canonical_prefix(config);
    let workspace = super::scope::canonical_prefix(workspace);
    if config.starts_with(&workspace) {
        return Err(HomeError::InConfig {
            workspace,
            config,
            inside: false,
        });
    }
    Ok(())
}

/// How a reference named its realm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Named {
    /// By its id.
    Id,
    /// By its directory, or a path inside it.
    Directory,
    /// By the workspace path it serves.
    Workspace,
}

/// The realm `reference` names among `homes` (`target`: the reference as
/// an absolute path, [`Context::absolute`]): its id; else its directory
/// or a path inside it (`<root>/<id>`, its `workspace.toml`); else the
/// workspace path it serves (a gone path compared lexically normalized,
/// an existing one canonical too).
///
/// # Errors
///
/// [`HomeError::UnknownRealm`], [`HomeError::Ambiguous`].
fn resolve_ref<'a>(
    homes: &'a [Home],
    reference: &str,
    target: &Path,
) -> Result<(&'a Home, Named), HomeError> {
    if let Some(home) = homes.iter().find(|home| home.id == reference) {
        return Ok((home, Named::Id));
    }
    let wanted: Vec<PathBuf> = [Some(normalize(target)), std::fs::canonicalize(target).ok()]
        .into_iter()
        .flatten()
        .collect();
    let one = |named: Vec<&'a Home>| match named.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        many => Err(HomeError::Ambiguous {
            reference: reference.to_string(),
            ids: many.iter().map(|home| home.id.clone()).collect(),
        }),
    };
    let inside = homes
        .iter()
        .filter(|home| {
            let dirs = [
                Some(normalize(&home.dir)),
                std::fs::canonicalize(&home.dir).ok(),
            ];
            dirs.iter()
                .flatten()
                .any(|dir| wanted.iter().any(|path| path.starts_with(dir)))
        })
        .collect();
    if let Some(home) = one(inside)? {
        return Ok((home, Named::Directory));
    }
    let served = homes
        .iter()
        .filter(|home| wanted.contains(&home.manifest.path))
        .collect();
    match one(served)? {
        Some(home) => Ok((home, Named::Workspace)),
        None => Err(HomeError::UnknownRealm(reference.to_string())),
    }
}

/// `path` with `.` and `..` resolved lexically (a gone path cannot be
/// canonicalized).
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Which way `/rehome <reference>` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// Pull: adopt this realm for the current workspace (a workspace that
    /// moved here).
    Pull(Home),
    /// Push: move the current workspace's realm to this directory
    /// (canonical).
    Push(PathBuf),
}

/// What `/rehome` was told: a reference read either way, or one the user
/// said the way of (`from <ref>`, `to <path>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    /// Either: a realm's id or a path some manifest names pulls, any other
    /// existing directory pushes, one that is both is refused.
    Either,
    /// `from <ref>`: pull only.
    Pull,
    /// `to <path>`: push only.
    Push,
}

/// The realm `reference` names among `homes` (as `/rehome from` reads
/// it, [`Context`]; this workspace's own realm too): what
/// `--remove-state` deletes.
///
/// # Errors
///
/// [`HomeError::UnknownRealm`], [`HomeError::Ambiguous`],
/// [`HomeError::NoHome`].
pub fn named(homes: &[Home], reference: &str, at: &Context) -> Result<Home, HomeError> {
    let target = at.absolute(reference)?;
    resolve_ref(homes, reference, &target).map(|(home, _)| home.clone())
}

/// `/rehome`'s argument: `from <ref>`, `to <path>`, or a reference read
/// either way.
#[must_use]
pub fn parse_way(argument: &str) -> (Way, &str) {
    let argument = argument.trim();
    if let Some(rest) = argument.strip_prefix("from ") {
        return (Way::Pull, rest.trim());
    }
    if let Some(rest) = argument.strip_prefix("to ") {
        return (Way::Push, rest.trim());
    }
    (Way::Either, argument)
}

/// Read `reference` among `homes` for the workspace of `at`: a realm's
/// id, its directory (or a path inside it), or a path some manifest names
/// is a pull; any other existing directory (a leading `~` expanded, a
/// relative path against [`Context::base`]) a push. One that reads both
/// ways is refused naming both, unless `way` says which; a realm's
/// directory reads as that realm only (no workspace is served from the
/// config directory). A push into the config directory, or to a directory
/// holding it, is refused, and so is a reference that is neither: each
/// refusal says what the path is (missing, not a directory) and what to do.
///
/// # Errors
///
/// [`HomeError::TwoReadings`], [`HomeError::UnknownRealm`],
/// [`HomeError::NoSuchDirectory`], [`HomeError::NotADirectory`],
/// [`HomeError::NoHome`], [`HomeError::InConfig`],
/// [`HomeError::Ambiguous`], [`HomeError::AlreadyHere`],
/// [`HomeError::Io`] when the path cannot be looked at.
pub fn read_reference(
    homes: &[Home],
    reference: &str,
    way: Way,
    at: &Context,
) -> Result<Reading, HomeError> {
    let workspace = at.workspace.as_path();
    let target = at.absolute(reference)?;
    let pull = match way {
        Way::Push => None,
        Way::Pull | Way::Either => match resolve_ref(homes, reference, &target) {
            Ok((home, named)) => Some((home.clone(), named)),
            Err(HomeError::UnknownRealm(_)) if way == Way::Either => None,
            Err(err) => return Err(err),
        },
    };
    if let Some((home, _)) = &pull
        && home.manifest.path == workspace
    {
        return Err(HomeError::AlreadyHere(home.id.clone()));
    }
    // A realm's own directory is that realm, never a place to move to.
    if let Some((home, Named::Directory)) = &pull
        && way == Way::Either
    {
        return Ok(Reading::Pull(home.clone()));
    }
    let push = match way {
        Way::Pull => None,
        Way::Push | Way::Either => Some(push_target(reference, &target, way == Way::Either)),
    };
    let push = match push {
        Some(Ok(path)) => match check_clear_of_config(config_of(&at.root), &path) {
            Ok(()) => Some(Ok(path)),
            // Read either way, a realm it names wins over the refusal.
            Err(_) if pull.is_some() => None,
            Err(err) => return Err(err),
        },
        other => other,
    };
    let pull = pull.map(|(home, _)| home);
    if matches!(&push, Some(Ok(path)) if path == workspace) && pull.is_none() {
        return match find_in(homes, workspace)? {
            Some(home) => Err(HomeError::AlreadyHere(home.id.clone())),
            None => Err(HomeError::UnknownRealm(reference.to_string())),
        };
    }
    let push = push.filter(|push| !matches!(push, Ok(path) if path == workspace));
    match (pull, push) {
        (Some(home), Some(Ok(path))) => Err(HomeError::TwoReadings {
            reference: reference.to_string(),
            id: home.id,
            path,
        }),
        (Some(home), _) => Ok(Reading::Pull(home)),
        (None, Some(Ok(path))) => Ok(Reading::Push(path)),
        (None, Some(Err(err))) => Err(err),
        (None, None) => Err(HomeError::UnknownRealm(reference.to_string())),
    }
}

/// `target` (what `reference` reads as) as a directory to move to:
/// canonical when it is one, else why not (`realm_too`: the reference was
/// read as a realm too and named none).
fn push_target(reference: &str, target: &Path, realm_too: bool) -> Result<PathBuf, HomeError> {
    let path = normalize(target);
    match std::fs::metadata(target) {
        Ok(meta) if meta.is_dir() => std::fs::canonicalize(target)
            .map_err(|err| HomeError::io("resolve the directory", &path, err)),
        Ok(_) => Err(HomeError::NotADirectory {
            reference: reference.to_string(),
            path,
            realm_too,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(HomeError::NoSuchDirectory {
            reference: reference.to_string(),
            path,
            realm_too,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => {
            Err(HomeError::NoSuchDirectory {
                reference: reference.to_string(),
                path,
                realm_too,
            })
        }
        Err(err) => Err(HomeError::io("look at", &path, err)),
    }
}

/// How many conversations `home`'s log lists (`None`: it cannot be read).
#[must_use]
pub fn conversations(home: &Home) -> Option<usize> {
    use crate::daemon::conversations::Log;
    match Log::read(&Log::path(&home.dir)) {
        Ok(Some(log)) => Some(log.sessions.len()),
        Ok(None) => Some(0),
        Err(_) => None,
    }
}

/// Refuse a workspace `workspace` whose shell's workspace grant would
/// reach `home`'s directory (holding it, or inside it). The daemon's start
/// checks the whole confinement plan again ([`exposing_grant`]).
///
/// # Errors
///
/// [`HomeError::Exposed`].
pub fn check_reach(home: &Home, workspace: &Path) -> Result<(), HomeError> {
    let realm = super::scope::canonical_prefix(&home.dir);
    let workspace = super::scope::canonical_prefix(workspace);
    if realm.starts_with(&workspace) || workspace.starts_with(&realm) {
        return Err(HomeError::Exposed {
            id: home.id.clone(),
            dir: home.dir.clone(),
            workspace,
        });
    }
    Ok(())
}

/// Prove that no daemon holds `home` (under `root`): no live or unreadable
/// lease in the realm, and every endpoint file of `files` (its workspace's)
/// absent or refusing a connect. Anything else is no proof.
///
/// # Errors
///
/// [`HomeError::InUse`] naming what was found.
pub async fn prove_idle(root: &Path, home: &Home, files: &[PathBuf]) -> Result<(), HomeError> {
    let in_use = |why: String| HomeError::InUse {
        id: home.id.clone(),
        why,
    };
    let leases = meerkat_store::inspect_realm_leases_in(root, &home.id, false)
        .await
        .map_err(|err| in_use(format!("its leases cannot be read ({err})")))?;
    if let Some(lease) = leases.active.first() {
        return Err(in_use(format!(
            "a live lease of pid {} ({})",
            lease.pid, lease.surface
        )));
    }
    if let Some(file) = leases.unparseable.first() {
        return Err(in_use(format!(
            "the lease {} cannot be read",
            file.display()
        )));
    }
    for file in files {
        crate::daemon::endpoint::check_stale(file)
            .await
            .map_err(|err| in_use(err.to_string()))?;
    }
    Ok(())
}

/// [`prove_idle`], tried again until `wait` has passed while the realm is
/// proven in use (a daemon that was asked to stop releases its lease as it
/// ends).
///
/// # Errors
///
/// The last [`HomeError::InUse`] after `wait`.
pub async fn wait_idle(
    root: &Path,
    home: &Home,
    files: &[PathBuf],
    wait: std::time::Duration,
) -> Result<(), HomeError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match prove_idle(root, home, files).await {
            Err(HomeError::InUse { .. }) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            done => return done,
        }
    }
}

/// What `home` (under `root`) holds, read from its files with no daemon
/// running on it: each conversation of its log with a user turn, in words.
/// Empty: provably empty (no log, or no conversation in it with a user
/// message). What cannot be read is not empty.
pub async fn holds(root: &Path, home: &Home) -> Vec<String> {
    let log = crate::daemon::conversations::Log::path(&home.dir);
    let log = match crate::daemon::conversations::Log::read(&log) {
        Ok(Some(log)) => log,
        Ok(None) => return Vec::new(),
        Err(err) => return vec![err.to_string()],
    };
    if log.sessions.is_empty() {
        return Vec::new();
    }
    let persistence = match meerkat::open_realm_persistence_in(root, &home.id, None, None).await {
        Ok((_, persistence)) => persistence,
        Err(err) => return vec![format!("its sessions cannot be read ({err})")],
    };
    let store = persistence.session_store();
    let mut held = Vec::new();
    for entry in &log.sessions {
        match store.load(&entry.session_id).await {
            Ok(Some(session)) => {
                if crate::daemon::conversations::has_user_turn(session.messages()) {
                    held.push(format!("conversation {} has user turns", entry.session_id));
                }
            }
            Ok(None) => {}
            Err(err) => held.push(format!(
                "conversation {} cannot be read ({err})",
                entry.session_id
            )),
        }
    }
    held
}

/// Make `adopt` (under `root`) the realm of `workspace` (canonical), under
/// the lock, once no daemon holds it or `replace` (each proven with its
/// workspace's endpoint `files`, waiting up to `wait` for daemons that
/// were asked to stop). `replace` is the realm `workspace` has: it must
/// still be (none when it had none), and still be provably empty
/// ([`holds`]); its manifest goes first (a directory without one is no
/// realm), then `adopt`'s names `workspace`, then the rest of `replace`'s
/// directory. A failure in between leaves the workspace with no realm or
/// with `adopt`, never with two. With `discard`, the user confirmed
/// deleting `replace` whatever it holds: it need not be empty.
///
/// # Errors
///
/// [`HomeError::Changed`] when the realms are no longer as checked,
/// [`HomeError::InUse`], [`HomeError::Holds`], [`HomeError::Io`].
pub async fn commit(
    root: &Path,
    adopt: &Home,
    adopt_files: &[PathBuf],
    workspace: &Path,
    replace: Option<(&Home, &[PathBuf])>,
    discard: bool,
    wait: std::time::Duration,
) -> Result<Home, HomeError> {
    let _lock = Lock::take(root)?;
    let homes = scan(root)?;
    let now_adopt = homes
        .iter()
        .find(|home| home.id == adopt.id)
        .ok_or_else(|| HomeError::Changed(format!("realm {} is gone", adopt.id)))?;
    if now_adopt.manifest.path != adopt.manifest.path {
        return Err(HomeError::Changed(format!(
            "realm {} serves {} now",
            adopt.id,
            now_adopt.manifest.path.display()
        )));
    }
    let now_replace = find_in(&homes, workspace)?.map(|home| home.id.clone());
    if now_replace != replace.map(|(home, _)| home.id.clone()) {
        return Err(HomeError::Changed(format!(
            "{} has realm {} now",
            workspace.display(),
            now_replace.as_deref().unwrap_or("none")
        )));
    }
    wait_idle(root, adopt, adopt_files, wait).await?;
    if let Some((replace, files)) = replace {
        wait_idle(root, replace, files, wait).await?;
        let held = if discard {
            Vec::new()
        } else {
            holds(root, replace).await
        };
        if !held.is_empty() {
            return Err(HomeError::Holds {
                id: replace.id.clone(),
                what: held.join("; "),
            });
        }
        let manifest = replace.dir.join(MANIFEST_FILE);
        std::fs::remove_file(&manifest)
            .map_err(|err| HomeError::io("remove the realm manifest", &manifest, err))?;
    }
    let mut home = now_adopt.clone();
    home.manifest.path = workspace.to_path_buf();
    home.manifest.name = name_of(workspace);
    home.manifest.git_remote = git_remote(workspace).or(home.manifest.git_remote);
    home.manifest.last_used = now();
    home.write()?;
    if let Some((replace, _)) = replace
        && let Err(err) = std::fs::remove_dir_all(&replace.dir)
    {
        // No manifest names anything there any more: it is no realm.
        tracing::warn!(dir = %replace.dir.display(), error = %err, "the replaced realm's directory was not removed");
    }
    Ok(home)
}

/// Delete the realm `home` under `root`, under the lock, once no daemon
/// holds it (its workspace's endpoint `files`, waiting up to `wait` for
/// one asked to stop): its manifest first (a directory without one is no
/// realm), then its directory. Only a directory directly under `root`
/// whose manifest is still the one checked is deleted.
///
/// # Errors
///
/// [`HomeError::Changed`] when the realm is no longer as checked or does
/// not lie under `root`, [`HomeError::InUse`], [`HomeError::Io`].
pub async fn remove(
    root: &Path,
    home: &Home,
    files: &[PathBuf],
    wait: std::time::Duration,
) -> Result<(), HomeError> {
    let _lock = Lock::take(root)?;
    if home.dir.parent() != Some(root) || home.id.is_empty() || home.id.starts_with('.') {
        return Err(HomeError::Changed(format!(
            "{} is not a realm directory under {}",
            home.dir.display(),
            root.display()
        )));
    }
    let homes = scan(root)?;
    match homes.iter().find(|now| now.id == home.id) {
        Some(now) if now.manifest.path == home.manifest.path => {}
        Some(now) => {
            return Err(HomeError::Changed(format!(
                "realm {} serves {} now",
                home.id,
                now.manifest.path.display()
            )));
        }
        None => return Err(HomeError::Changed(format!("realm {} is gone", home.id))),
    }
    wait_idle(root, home, files, wait).await?;
    let manifest = home.dir.join(MANIFEST_FILE);
    std::fs::remove_file(&manifest)
        .map_err(|err| HomeError::io("remove the realm manifest", &manifest, err))?;
    std::fs::remove_dir_all(&home.dir)
        .map_err(|err| HomeError::io("remove the realm directory", &home.dir, err))
}

/// One line on `home` for a question before deleting it: its id, the
/// workspace it serves, how many conversations and when last used.
#[must_use]
pub fn deletion_words(home: &Home) -> String {
    let held = match conversations(home) {
        Some(n) => crate::text::count(n, "conversation"),
        None => "a conversation log that cannot be read".to_string(),
    };
    let gone = if home.orphaned() {
        ", which is gone"
    } else {
        ""
    };
    let mut text = format!(
        "realm {} (the state of {}{gone}: {held}",
        home.id,
        home.manifest.path.display()
    );
    if !home.manifest.last_used.is_empty() {
        text.push_str(&format!(", last used {}", home.manifest.last_used));
    }
    text.push_str(&format!(", in {})", home.dir.display()));
    text
}

/// The realms of `homes` a pull into `workspace` (canonical) could adopt:
/// those whose workspace is gone, then those whose workspace still exists
/// (adopting one takes its state from there; that directory starts anew),
/// each by id. The workspace's own realm is neither.
#[must_use]
pub fn adoptable<'a>(homes: &'a [Home], workspace: &Path) -> (Vec<&'a Home>, Vec<&'a Home>) {
    homes
        .iter()
        .filter(|home| home.manifest.path != workspace)
        .partition(|home| home.orphaned())
}

/// The grant of the shell's confinement `plan` that reaches the realm
/// directory `realm`: one that holds it (a writable or readable ancestor,
/// or the directory itself), one inside it, or unrestricted access.
/// `None` for an unconfined shell (the trusted host) or when no grant
/// reaches it. Both are compared canonical as far as they exist.
#[must_use]
pub fn exposing_grant(plan: &super::ConfinementPlan, realm: &Path) -> Option<PathBuf> {
    use meerkat_core::confinement::{FilesystemAccess, PathAccess};
    let meerkat_tools::builtin::shell::ShellConfinement::Required { requirement } = &plan.shell
    else {
        return None;
    };
    let realm = super::scope::canonical_prefix(realm);
    let spec = requirement.specification();
    for access in [&spec.read, &spec.write] {
        let grants: &[PathAccess] = match access {
            FilesystemAccess::Unrestricted => return Some(PathBuf::from("/")),
            FilesystemAccess::Paths(paths) => paths,
        };
        for grant in grants {
            let granted = super::scope::canonical_prefix(grant.path());
            if realm.starts_with(&granted) || granted.starts_with(&realm) {
                return Some(grant.path().to_path_buf());
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn manifest(path: &Path) -> Manifest {
        Manifest::new(path)
    }

    fn plant(root: &Path, id: &str, manifest: &Manifest) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        write_manifest(&dir, manifest).unwrap();
        dir
    }

    #[test]
    fn a_new_workspace_gets_a_random_private_realm_with_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let ws = tmp.path().join("project");
        std::fs::create_dir_all(&ws).unwrap();
        let opened = open_or_create(&root, &ws).unwrap();
        assert!(opened.created);
        assert_eq!(opened.home.id.len(), ID_BYTES * 2);
        assert!(opened.home.id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(opened.home.dir, root.join(&opened.home.id));
        assert_eq!(opened.home.manifest.path, ws);
        assert_eq!(opened.home.manifest.name, "project");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&opened.home.dir)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        // The next start finds it; another workspace gets another id.
        let again = open_or_create(&root, &ws).unwrap();
        assert!(!again.created);
        assert_eq!(again.home.id, opened.home.id);
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert_ne!(
            open_or_create(&root, &other).unwrap().home.id,
            opened.home.id
        );
    }

    #[test]
    fn two_manifests_naming_one_workspace_refuse_and_name_both() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let ws = tmp.path().join("project");
        std::fs::create_dir_all(&ws).unwrap();
        let a = plant(&root, "aaaa", &manifest(&ws));
        let b = plant(&root, "bbbb", &manifest(&ws));
        let err = open_or_create(&root, &ws).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, HomeError::Duplicate { .. }), "{text}");
        assert!(text.contains(a.to_str().unwrap()), "{text}");
        assert!(text.contains(b.to_str().unwrap()), "{text}");
        // Nothing was made beside them.
        assert_eq!(scan(&root).unwrap().len(), 2);
    }

    #[test]
    fn an_unreadable_manifest_refuses_a_start() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        std::fs::create_dir_all(root.join("cccc")).unwrap();
        std::fs::write(root.join("cccc").join(MANIFEST_FILE), "path = [").unwrap();
        let err = open_or_create(&root, tmp.path()).unwrap_err();
        assert!(matches!(err, HomeError::Unreadable { .. }), "{err}");
        // A directory without a manifest is no realm.
        std::fs::remove_file(root.join("cccc").join(MANIFEST_FILE)).unwrap();
        assert!(open_or_create(&root, tmp.path()).unwrap().created);
    }

    #[test]
    fn orphans_are_offered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let gone = tmp.path().join("gone");
        plant(&root, "dddd", &manifest(&gone));
        let here = tmp.path().join("here");
        std::fs::create_dir_all(&here).unwrap();
        plant(&root, "eeee", &manifest(&here));
        let ws = tmp.path().join("new");
        std::fs::create_dir_all(&ws).unwrap();
        let opened = open_or_create(&root, &ws).unwrap();
        let ids: Vec<&str> = opened.orphans.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["dddd"]);
        // The picker's adoptable realms: the gone one, then the one whose
        // workspace exists; never this workspace's own.
        let homes = scan(&root).unwrap();
        let mine = find_in(&homes, &ws).unwrap().unwrap().id.clone();
        let (gone_ids, kept_ids) = adoptable(&homes, &ws);
        let ids = |homes: Vec<&Home>| homes.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(gone_ids), ["dddd"]);
        assert_eq!(ids(kept_ids), ["eeee"]);
        assert_ne!(mine, "dddd");
        let offer = offer(&opened.orphans).unwrap();
        assert!(
            offer.contains("dddd") && offer.contains("/rehome"),
            "{offer}"
        );
        assert!(!offer.contains("eeee"), "{offer}");
        assert!(super::offer(&[]).is_none());
    }

    /// A manifest of another shape (a field it does not know, the path
    /// missing) is unreadable, not read with defaults.
    #[test]
    fn a_manifest_is_read_strictly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let dir = root.join("abcd");
        std::fs::create_dir_all(&dir).unwrap();
        for text in [
            "name = \"x\"\ncreated = \"\"\nlast_used = \"\"\n",
            "path = \"/x\"\nformer_path = \"/y\"\nname = \"x\"\ncreated = \"\"\nlast_used = \"\"\n",
        ] {
            std::fs::write(dir.join(MANIFEST_FILE), text).unwrap();
            let err = scan(&root).unwrap_err();
            assert!(matches!(err, HomeError::Unreadable { .. }), "{err}");
        }
    }

    /// An id or a path some manifest names pulls; any other existing
    /// directory pushes; one that is both is refused naming both, unless
    /// `from`/`to` says which.
    #[test]
    fn a_reference_reads_as_a_pull_or_a_push_and_both_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("cfg").join(WORKSPACES_DIR);
        let here = base.join("here");
        let other = base.join("other");
        let free = base.join("free");
        for dir in [&here, &other, &free] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let gone = base.join("gone");
        plant(&root, "aaaa", &manifest(&here));
        plant(&root, "bbbb", &manifest(&gone));
        plant(&root, "cccc", &manifest(&other));
        let homes = scan(&root).unwrap();
        let at = context(&root, &here, None);
        let read = |reference: &str| {
            let (way, reference) = parse_way(reference);
            read_reference(&homes, reference, way, &at)
        };
        let pulled = |reading: Result<Reading, HomeError>| match reading {
            Ok(Reading::Pull(home)) => home.id,
            other => panic!("{other:?}"),
        };
        assert_eq!(pulled(read("bbbb")), "bbbb");
        assert_eq!(pulled(read(gone.to_str().unwrap())), "bbbb");
        assert_eq!(pulled(read("../gone")), "bbbb");
        assert_eq!(read("../free").unwrap(), Reading::Push(free.clone()));
        assert_eq!(
            read(&format!("to {}", free.display())).unwrap(),
            Reading::Push(free)
        );
        let both = read(other.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(&both, HomeError::TwoReadings { id, path, .. } if id == "cccc" && *path == other),
            "{both}"
        );
        assert!(both.to_string().contains("/rehome from"), "{both}");
        assert_eq!(pulled(read(&format!("from {}", other.display()))), "cccc");
        assert_eq!(
            read(&format!("to {}", other.display())).unwrap(),
            Reading::Push(other)
        );
        assert!(matches!(read("aaaa"), Err(HomeError::AlreadyHere(_))));
        assert!(matches!(read("."), Err(HomeError::AlreadyHere(_))));
        assert!(matches!(
            read("nope"),
            Err(HomeError::NoSuchDirectory {
                realm_too: true,
                ..
            })
        ));
        assert!(matches!(
            read("from ../free"),
            Err(HomeError::UnknownRealm(_))
        ));
        // Two realms naming one gone path: by id only.
        plant(&root, "dddd", &manifest(&gone));
        let homes = scan(&root).unwrap();
        let ambiguous = read_reference(&homes, "../gone", Way::Pull, &at).unwrap_err();
        assert!(
            matches!(&ambiguous, HomeError::Ambiguous { ids, .. } if ids.len() == 2),
            "{ambiguous}"
        );
    }

    /// The reading context of `workspace` under `root`, `~` standing for
    /// `home` (never the user's own).
    fn context(root: &Path, workspace: &Path, home: Option<&Path>) -> Context {
        Context {
            root: root.to_path_buf(),
            base: workspace.to_path_buf(),
            workspace: workspace.to_path_buf(),
            home: home.map(Path::to_path_buf),
        }
    }

    /// A leading `~` stands for the home directory, in a pull by the path
    /// a workspace had and in a push; `~user` is not expanded, and with no
    /// home a `~` path is refused saying to give the full path.
    #[test]
    fn a_tilde_is_the_home_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("cfg/workspaces");
        let home = base.join("home");
        let here = base.join("here");
        let free = home.join("free");
        for dir in [&here, &free] {
            std::fs::create_dir_all(dir).unwrap();
        }
        plant(&root, "aaaa", &manifest(&here));
        plant(&root, "bbbb", &manifest(&home.join("old/project")));
        let homes = scan(&root).unwrap();
        let at = context(&root, &here, Some(&home));
        let read = |reference: &str, way| read_reference(&homes, reference, way, &at);
        assert_eq!(
            read("~/free", Way::Either).unwrap(),
            Reading::Push(free.clone())
        );
        assert_eq!(read("~/free", Way::Push).unwrap(), Reading::Push(free));
        assert!(matches!(
            read("~/old/project", Way::Either).unwrap(),
            Reading::Pull(found) if found.id == "bbbb"
        ));
        assert!(matches!(
            read("~/old/project/", Way::Pull).unwrap(),
            Reading::Pull(found) if found.id == "bbbb"
        ));
        let literal = read("~nobody/free", Way::Push).unwrap_err();
        assert!(
            matches!(&literal, HomeError::NoSuchDirectory { path, .. } if *path == here.join("~nobody/free")),
            "{literal}"
        );
        let homeless = context(&root, &here, None);
        let refused = read_reference(&homes, "~/free", Way::Push, &homeless).unwrap_err();
        assert!(matches!(refused, HomeError::NoHome(_)), "{refused}");
        assert!(
            refused.to_string().contains("give the full path"),
            "{refused}"
        );
    }

    /// A directory that does not exist, a file, and a name that is no
    /// realm are each refused saying which, with the absolute path and
    /// what to do.
    #[test]
    fn each_refusal_says_what_the_path_is_and_what_to_do() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("cfg/workspaces");
        let here = base.join("here");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::write(base.join("a-file"), "x").unwrap();
        plant(&root, "aaaa", &manifest(&here));
        let homes = scan(&root).unwrap();
        let at = context(&root, &here, None);
        let read = |reference: &str, way| read_reference(&homes, reference, way, &at);
        let missing = base.join("typo");
        let push = read("../typo", Way::Push).unwrap_err();
        assert!(
            matches!(&push, HomeError::NoSuchDirectory { path, realm_too: false, .. } if *path == missing),
            "{push}"
        );
        let words = push.to_string();
        assert!(
            words.starts_with(&format!("{} does not exist", missing.display())),
            "{words}"
        );
        assert!(words.contains("mkdir -p"), "{words}");
        let either = read("../typo", Way::Either).unwrap_err().to_string();
        assert!(either.contains("names no realm"), "{either}");
        assert!(either.contains("rkat-tui --list"), "{either}");
        assert!(either.contains(&missing.display().to_string()), "{either}");
        let file = read("../a-file", Way::Either).unwrap_err();
        assert!(
            matches!(&file, HomeError::NotADirectory { path, .. } if *path == base.join("a-file")),
            "{file}"
        );
        assert!(file.to_string().contains("is not a directory"), "{file}");
        // Below a file is no directory either.
        assert!(matches!(
            read("../a-file/x", Way::Push),
            Err(HomeError::NoSuchDirectory { .. })
        ));
        let pull = read("nope", Way::Pull).unwrap_err();
        assert!(matches!(pull, HomeError::UnknownRealm(_)), "{pull}");
        assert!(pull.to_string().contains("rkat-tui --list"), "{pull}");
    }

    /// A realm's own directory, or a path inside it, reads as that realm
    /// (with `~`, with a trailing slash, its manifest), never as a place
    /// to move to; `to` there, or into the config directory or a directory
    /// holding it, is refused saying what to do instead.
    #[test]
    fn a_realm_s_directory_is_that_realm_and_the_config_directory_no_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let home = base.join("home");
        let config = home.join(".rkat/tui");
        let root = config.join(WORKSPACES_DIR);
        let here = base.join("here");
        let old = base.join("old");
        for dir in [&here, &old] {
            std::fs::create_dir_all(dir).unwrap();
        }
        plant(&root, "aaaa", &manifest(&here));
        let old_dir = plant(&root, "bbbb", &manifest(&old));
        let homes = scan(&root).unwrap();
        let at = context(&root, &here, Some(&home));
        let read = |reference: &str| {
            let (way, reference) = parse_way(reference);
            read_reference(&homes, reference, way, &at)
        };
        let pulled = |reading: Result<Reading, HomeError>| match reading {
            Ok(Reading::Pull(home)) => home.id,
            other => panic!("{other:?}"),
        };
        let dir = old_dir.display().to_string();
        assert_eq!(pulled(read(&dir)), "bbbb");
        assert_eq!(pulled(read(&format!("{dir}/"))), "bbbb");
        assert_eq!(pulled(read("~/.rkat/tui/workspaces/bbbb")), "bbbb");
        assert_eq!(pulled(read(&format!("{dir}/{MANIFEST_FILE}"))), "bbbb");
        assert_eq!(pulled(read(&format!("from {dir}"))), "bbbb");
        // This workspace's own realm directory: it is here already.
        assert!(matches!(
            read(&root.join("aaaa").display().to_string()),
            Err(HomeError::AlreadyHere(_))
        ));
        for refused in [
            format!("to {dir}"),
            format!("to {}", config.display()),
            format!("to {}", root.display()),
            format!("{}", root.display()),
            "to ~".to_string(),
        ] {
            let err = read(&refused).unwrap_err();
            assert!(
                matches!(err, HomeError::InConfig { .. }),
                "{refused}: {err}"
            );
        }
        let inside = read(&format!("to {dir}")).unwrap_err().to_string();
        assert!(inside.contains("/rehome from"), "{inside}");
        let holds = read("to ~").unwrap_err().to_string();
        assert!(holds.contains("does not contain it"), "{holds}");
        // The daemon's start refuses a workspace inside it too.
        assert!(check_not_in_config(&config, &old_dir).is_err());
        assert!(check_not_in_config(&config, &here).is_ok());
        assert!(check_clear_of_config(&config, &home).is_err());
    }

    /// A commit names the new workspace in the adopted realm's manifest
    /// and removes the empty realm the workspace had; a realm that holds
    /// a conversation log it cannot read is not empty, and nothing
    /// changes.
    #[tokio::test]
    async fn a_commit_moves_the_path_and_removes_only_an_empty_realm() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let old = tmp.path().join("old");
        plant(&root, "ffff", &manifest(&old));
        let ws = tmp.path().join("new");
        std::fs::create_dir_all(&ws).unwrap();
        let fresh = open_or_create(&root, &ws).unwrap().home;
        let adopt = find(&root, &old).unwrap().unwrap();
        // Not empty: its conversation log cannot be read.
        let log = crate::daemon::conversations::Log::path(&fresh.dir);
        std::fs::write(&log, "{").unwrap();
        let wait = std::time::Duration::ZERO;
        let refused = commit(&root, &adopt, &[], &ws, Some((&fresh, &[])), false, wait)
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, HomeError::Holds { id, what } if *id == fresh.id && what.contains("conversation log")),
            "{refused}"
        );
        assert_eq!(find(&root, &ws).unwrap().unwrap().id, fresh.id);
        assert_eq!(find(&root, &old).unwrap().unwrap().id, "ffff");
        // Empty (no log at all): removed, and the adopted realm serves.
        std::fs::remove_file(&log).unwrap();
        let home = commit(&root, &adopt, &[], &ws, Some((&fresh, &[])), false, wait)
            .await
            .unwrap();
        assert_eq!(home.id, "ffff");
        assert_eq!(find(&root, &ws).unwrap().unwrap().id, "ffff");
        assert!(!fresh.dir.exists());
        assert_eq!(find(&root, &old).unwrap(), None);
        // The realms changed since the check: refused.
        let changed = commit(&root, &adopt, &[], &old, None, false, wait).await;
        assert!(matches!(changed, Err(HomeError::Changed(_))), "{changed:?}");
    }

    /// A realm that holds something is replaced once the user confirmed
    /// deleting it (`discard`); its refusal without says how to delete it
    /// from the shell.
    #[tokio::test]
    async fn a_confirmed_discard_replaces_a_realm_that_holds_something() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let old = tmp.path().join("old");
        plant(&root, "ffff", &manifest(&old));
        let ws = tmp.path().join("new");
        std::fs::create_dir_all(&ws).unwrap();
        let fresh = open_or_create(&root, &ws).unwrap().home;
        let adopt = find(&root, &old).unwrap().unwrap();
        std::fs::write(crate::daemon::conversations::Log::path(&fresh.dir), "{").unwrap();
        let wait = std::time::Duration::ZERO;
        let refused = commit(&root, &adopt, &[], &ws, Some((&fresh, &[])), false, wait)
            .await
            .unwrap_err();
        let words = refused.to_string();
        assert!(
            words.contains(&format!("rkat-tui --remove-state {} --yes", fresh.id)),
            "{words}"
        );
        assert!(fresh.dir.exists());
        commit(&root, &adopt, &[], &ws, Some((&fresh, &[])), true, wait)
            .await
            .unwrap();
        assert!(!fresh.dir.exists());
        assert_eq!(find(&root, &ws).unwrap().unwrap().id, "ffff");
    }

    /// `remove` deletes exactly the realm named, under the root only, and
    /// refuses one that changed or a live lease; `named` reads a
    /// reference as `/rehome from` does.
    #[tokio::test]
    async fn remove_deletes_exactly_that_realm() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("cfg").join(WORKSPACES_DIR);
        let one = plant(&root, "aaaa", &manifest(&base.join("one")));
        let two = plant(&root, "bbbb", &manifest(&base.join("two")));
        std::fs::write(two.join("keep.txt"), "x").unwrap();
        let homes = scan(&root).unwrap();
        let at = context(&root, &base, None);
        assert!(matches!(
            named(&homes, "nope", &at),
            Err(HomeError::UnknownRealm(_))
        ));
        let target = named(&homes, "one", &at).unwrap();
        assert_eq!(target.id, "aaaa");
        assert_eq!(
            named(&homes, &one.display().to_string(), &at).unwrap().id,
            "aaaa"
        );
        let wait = std::time::Duration::ZERO;
        // A directory not directly under the root is refused.
        let mut elsewhere = target.clone();
        elsewhere.dir = base.join("one");
        assert!(matches!(
            remove(&root, &elsewhere, &[], wait).await,
            Err(HomeError::Changed(_))
        ));
        let lease = meerkat_store::start_realm_lease_in(&root, "aaaa", None, "test")
            .await
            .unwrap();
        assert!(matches!(
            remove(&root, &target, &[], wait).await,
            Err(HomeError::InUse { .. })
        ));
        lease.shutdown().await;
        assert!(one.exists());
        remove(&root, &target, &[], wait).await.unwrap();
        assert!(!one.exists());
        assert!(two.join("keep.txt").exists());
        assert_eq!(scan(&root).unwrap().len(), 1);
        assert!(matches!(
            remove(&root, &target, &[], wait).await,
            Err(HomeError::Changed(_))
        ));
    }

    #[tokio::test]
    async fn a_commit_is_refused_while_a_lease_is_live_or_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let old = tmp.path().join("old");
        let dir = plant(&root, "abcd", &manifest(&old));
        let adopt = find(&root, &old).unwrap().unwrap();
        let ws = tmp.path().join("new");
        std::fs::create_dir_all(&ws).unwrap();
        let wait = std::time::Duration::ZERO;
        let lease = meerkat_store::start_realm_lease_in(&root, "abcd", None, "test")
            .await
            .unwrap();
        let err = commit(&root, &adopt, &[], &ws, None, false, wait)
            .await
            .unwrap_err();
        assert!(matches!(err, HomeError::InUse { .. }), "{err}");
        assert!(err.to_string().contains("live lease"), "{err}");
        lease.shutdown().await;
        // A lease that cannot be read proves nothing either.
        std::fs::create_dir_all(dir.join("leases")).unwrap();
        std::fs::write(dir.join("leases").join("x.json"), "{").unwrap();
        let err = commit(&root, &adopt, &[], &ws, None, false, wait)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");
        assert_eq!(find(&root, &old).unwrap().unwrap().id, "abcd");
        std::fs::remove_file(dir.join("leases").join("x.json")).unwrap();
        commit(&root, &adopt, &[], &ws, None, false, wait)
            .await
            .unwrap();
        assert_eq!(find(&root, &ws).unwrap().unwrap().id, "abcd");
    }

    /// A workspace that holds the realm, or lies inside it, is refused.
    #[test]
    fn a_workspace_that_reaches_the_realm_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cfg/workspaces");
        let dir = plant(&root, "abcd", &manifest(&tmp.path().join("old")));
        let home = find(&root, &tmp.path().join("old")).unwrap().unwrap();
        let elsewhere = tmp.path().join("ws");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(check_reach(&home, &elsewhere).is_ok());
        for reaching in [tmp.path().join("cfg"), dir.join("sub")] {
            assert!(
                matches!(
                    check_reach(&home, &reaching),
                    Err(HomeError::Exposed { .. })
                ),
                "{}",
                reaching.display()
            );
        }
    }

    #[test]
    fn remotes_lose_their_credentials() {
        assert_eq!(
            clean_remote("https://me:tok@github.com/a/b.git"),
            "https://github.com/a/b.git"
        );
        assert_eq!(clean_remote("git@github.com:a/b.git"), "github.com:a/b.git");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(".git/config"),
            "[core]\n\turl = no\n[remote \"origin\"]\n\turl = https://x@h/r\n",
        )
        .unwrap();
        assert_eq!(git_remote(tmp.path()).as_deref(), Some("https://h/r"));
    }

    #[test]
    fn a_grant_holding_or_inside_the_realm_exposes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let realm = base.join("cfg/workspaces/abcd");
        std::fs::create_dir_all(&realm).unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let plan = |extra: Vec<PathBuf>| {
            let requirement = super::super::confinement::lower_with(
                crate::cli::ConfinementArg::Workspace,
                &ws,
                &super::super::HostPaths::default(),
                &super::super::confinement::ExtraGrants {
                    write: extra,
                    ..super::super::confinement::ExtraGrants::default()
                },
            )
            .unwrap()
            .unwrap();
            super::super::ConfinementPlan {
                shell: meerkat_tools::builtin::shell::ShellConfinement::Required { requirement },
                notices: Vec::new(),
                shell_env: None,
                granted: Default::default(),
            }
        };
        assert_eq!(exposing_grant(&plan(Vec::new()), &realm), None);
        let cfg = base.join("cfg");
        assert_eq!(exposing_grant(&plan(vec![cfg.clone()]), &realm), Some(cfg));
        // A realm inside the workspace is reached by the workspace grant.
        let inside = ws.join(".cfg/workspaces/abcd");
        assert!(exposing_grant(&plan(Vec::new()), &inside).is_some());
        let off = super::super::ConfinementPlan {
            shell: meerkat_tools::builtin::shell::ShellConfinement::TrustedHost,
            notices: Vec::new(),
            shell_env: None,
            granted: Default::default(),
        };
        assert_eq!(exposing_grant(&off, &inside), None);
    }
}
