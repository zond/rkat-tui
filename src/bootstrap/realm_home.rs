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
    /// `--rehome`/`/rehome` names no realm (and, inside the TUI, no
    /// directory to move to either).
    #[error(
        "`{0}` is no realm and no directory: name a realm by its id or the workspace path it served (rkat-tui --list shows them), or an existing directory to move this workspace's state to"
    )]
    UnknownRealm(String),
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
        "the destination's realm {id} is not empty: {what}; a re-home replaces only an empty realm, so nothing was changed"
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

/// `url` without user information (`https://user:token@host/…` →
/// `https://host/…`) or control characters, cut to [`MAX_REMOTE`].
fn clean_remote(url: &str) -> String {
    let url = match url.split_once("://") {
        Some((scheme, rest)) => {
            let host_end = rest.find('/').unwrap_or(rest.len());
            match rest[..host_end].rfind('@') {
                Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
                None => url.to_string(),
            }
        }
        None => url.to_string(),
    };
    url.chars()
        .filter(|c| !c.is_control())
        .take(MAX_REMOTE)
        .collect()
}

/// A fresh realm id: [`ID_BYTES`] from the OS, hex-encoded.
fn new_id() -> Result<String, HomeError> {
    use std::io::Read as _;
    let mut bytes = [0u8; ID_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|err| HomeError::io("draw a realm id from", Path::new("/dev/urandom"), err))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(ID_BYTES * 2), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

/// Create `dir` (and missing parents) with mode 0700; an existing
/// directory is left as it is.
fn create_private_dir(dir: &Path) -> Result<(), HomeError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
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
        "\nif one of them is this workspace moved, /rehome <id> adopts it here (rkat-tui --rehome <id> from the shell); the new state, still empty, is removed",
    );
    Some(text)
}

/// The notice for a workspace that still holds a realm of an earlier
/// release (`<workspace>/.rkat/tui/`): it is never read or imported.
#[must_use]
pub fn old_realm_notice(workspace: &Path, home: &Home) -> Option<String> {
    let old = workspace.join(".rkat").join("tui");
    std::fs::symlink_metadata(&old).ok()?;
    Some(format!(
        "{} is an earlier release's state directory and is no longer used (this workspace's state is {}); nothing of it was imported, delete it when you no longer need it",
        old.display(),
        home.dir.display()
    ))
}

/// `reference` as a path: absolute against `base`, lexically normalized,
/// and canonical too when it exists.
fn as_paths(reference: &str, base: &Path) -> Vec<PathBuf> {
    let absolute = base.join(reference);
    [
        Some(normalize(&absolute)),
        std::fs::canonicalize(&absolute).ok(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The realm `reference` names among `homes`: its id, else the workspace
/// path it serves (`reference` taken as a path against `base`; a gone path
/// is compared lexically normalized, an existing one canonical too).
///
/// # Errors
///
/// [`HomeError::UnknownRealm`], [`HomeError::Ambiguous`].
pub fn resolve_ref<'a>(
    homes: &'a [Home],
    reference: &str,
    base: &Path,
) -> Result<&'a Home, HomeError> {
    if let Some(home) = homes.iter().find(|home| home.id == reference) {
        return Ok(home);
    }
    let wanted = as_paths(reference, base);
    let named: Vec<&Home> = homes
        .iter()
        .filter(|home| wanted.contains(&home.manifest.path))
        .collect();
    match named.as_slice() {
        [] => Err(HomeError::UnknownRealm(reference.to_string())),
        [one] => Ok(one),
        many => Err(HomeError::Ambiguous {
            reference: reference.to_string(),
            ids: many.iter().map(|home| home.id.clone()).collect(),
        }),
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

/// Read `reference` among `homes` for the workspace `workspace`
/// (canonical): a realm's id, or a path some manifest names, is a pull; any
/// other existing directory (resolved against `base`) a push. One that
/// reads both ways is refused naming both, unless `way` says which.
///
/// # Errors
///
/// [`HomeError::TwoReadings`], [`HomeError::UnknownRealm`],
/// [`HomeError::Ambiguous`], [`HomeError::AlreadyHere`].
pub fn read_reference(
    homes: &[Home],
    reference: &str,
    way: Way,
    base: &Path,
    workspace: &Path,
) -> Result<Reading, HomeError> {
    let pull = match way {
        Way::Push => None,
        Way::Pull | Way::Either => match resolve_ref(homes, reference, base) {
            Ok(home) => Some(home.clone()),
            Err(HomeError::UnknownRealm(_)) if way == Way::Either => None,
            Err(err) => return Err(err),
        },
    };
    let push = match way {
        Way::Pull => None,
        Way::Push | Way::Either => std::fs::canonicalize(base.join(reference))
            .ok()
            .filter(|path| path.is_dir()),
    };
    if let Some(home) = &pull
        && home.manifest.path == workspace
    {
        return Err(HomeError::AlreadyHere(home.id.clone()));
    }
    if push.as_deref() == Some(workspace) && pull.is_none() {
        return match find_in(homes, workspace)? {
            Some(home) => Err(HomeError::AlreadyHere(home.id.clone())),
            None => Err(HomeError::UnknownRealm(reference.to_string())),
        };
    }
    match (pull, push.filter(|path| path != workspace)) {
        (Some(home), Some(path)) => Err(HomeError::TwoReadings {
            reference: reference.to_string(),
            id: home.id,
            path,
        }),
        (Some(home), None) => Ok(Reading::Pull(home)),
        (None, Some(path)) => Ok(Reading::Push(path)),
        (None, None) => Err(HomeError::UnknownRealm(reference.to_string())),
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
/// with `adopt`, never with two.
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
        let held = holds(root, replace).await;
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

/// `/rehome` without an argument: this workspace's realm (`current`) and
/// the others under `root` whose workspace is gone, which a re-home could
/// adopt.
///
/// # Errors
///
/// See [`scan`].
pub fn adoptable(root: &Path, current: Option<&Home>) -> Result<String, HomeError> {
    let homes = scan(root)?;
    let mut text = match current {
        Some(home) => format!(
            "this workspace's state is realm {} ({})",
            home.id,
            home.dir.display()
        ),
        None => "this workspace has no state of its own".to_string(),
    };
    let others: Vec<&Home> = homes.iter().filter(|home| home.orphaned()).collect();
    if others.is_empty() {
        text.push_str("; no other workspace's state is waiting to be adopted");
    } else {
        text.push_str("; state whose workspace is gone, which /rehome <id> adopts here:");
        for home in others {
            text.push_str("\n  ");
            text.push_str(&home.describe().replace('\t', "  "));
        }
    }
    text.push_str(
        "\n/rehome <directory> moves this workspace's state there; either way the daemon restarts in the new place and this rkat-tui reattaches",
    );
    Ok(text)
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
        let root = base.join("workspaces");
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
        let read = |reference: &str| {
            let (way, reference) = parse_way(reference);
            read_reference(&homes, reference, way, &here, &here)
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
        assert!(matches!(read("nope"), Err(HomeError::UnknownRealm(_))));
        assert!(matches!(
            read("from ../free"),
            Err(HomeError::UnknownRealm(_))
        ));
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
        let refused = commit(&root, &adopt, &[], &ws, Some((&fresh, &[])), wait)
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
        let home = commit(&root, &adopt, &[], &ws, Some((&fresh, &[])), wait)
            .await
            .unwrap();
        assert_eq!(home.id, "ffff");
        assert_eq!(find(&root, &ws).unwrap().unwrap().id, "ffff");
        assert!(!fresh.dir.exists());
        assert_eq!(find(&root, &old).unwrap(), None);
        // The realms changed since the check: refused.
        let changed = commit(&root, &adopt, &[], &old, None, wait).await;
        assert!(matches!(changed, Err(HomeError::Changed(_))), "{changed:?}");
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
        let err = commit(&root, &adopt, &[], &ws, None, wait)
            .await
            .unwrap_err();
        assert!(matches!(err, HomeError::InUse { .. }), "{err}");
        assert!(err.to_string().contains("live lease"), "{err}");
        lease.shutdown().await;
        // A lease that cannot be read proves nothing either.
        std::fs::create_dir_all(dir.join("leases")).unwrap();
        std::fs::write(dir.join("leases").join("x.json"), "{").unwrap();
        let err = commit(&root, &adopt, &[], &ws, None, wait)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");
        assert_eq!(find(&root, &old).unwrap().unwrap().id, "abcd");
        std::fs::remove_file(dir.join("leases").join("x.json")).unwrap();
        commit(&root, &adopt, &[], &ws, None, wait).await.unwrap();
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
        assert_eq!(
            clean_remote("git@github.com:a/b.git"),
            "git@github.com:a/b.git"
        );
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
        };
        assert_eq!(exposing_grant(&off, &inside), None);
    }
}
