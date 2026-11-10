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
//! explicit ([`rehome`]: `/rehome` in the TUI, `rkat-tui --rehome
//! <id|old path>`): it rewrites the manifest's path, and sets aside the realm
//! the new path already had (its manifest keeps the path as
//! `former_path`; nothing is deleted). It is refused while a daemon may hold
//! either realm ([`prove_idle`]: a live or unreadable lease, or an endpoint
//! file whose address does not refuse a connect). A start in a workspace
//! with no realm while realms whose workspace is gone exist
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
    /// `--rehome`/`/rehome` names no realm.
    #[error(
        "no realm is `{0}`: name one by its id or the workspace path it served (rkat-tui --list shows them)"
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
    /// The realm already serves this workspace.
    #[error("realm {0} already serves this workspace")]
    AlreadyHere(String),
    /// The realm's workspace still exists: re-homing would take its state
    /// from it.
    #[error(
        "realm {id} serves {}, which still exists; re-homing moves the state of a workspace that moved, so move or remove that directory first",
        path.display()
    )]
    StillHomed {
        /// The realm.
        id: String,
        /// Its workspace.
        path: PathBuf,
    },
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

/// A realm directory's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The canonical path of the workspace the realm serves (`None`: set
    /// aside by a re-home).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The path it served before it was re-homed or set aside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub former_path: Option<PathBuf>,
    /// The workspace directory's name, for people.
    #[serde(default)]
    pub name: String,
    /// The workspace's `origin` remote when it had one (credentials
    /// removed), for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    /// When the realm was made (RFC 3339, UTC).
    #[serde(default)]
    pub created: String,
    /// When a start last used it (RFC 3339, UTC).
    #[serde(default)]
    pub last_used: String,
}

impl Manifest {
    /// A new manifest for `workspace` (canonical), made now.
    fn new(workspace: &Path) -> Self {
        let now = now();
        Self {
            path: Some(workspace.to_path_buf()),
            former_path: None,
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
    /// gone). A realm set aside by a re-home is not an orphan; it is
    /// listed, and `--rehome <id>` still adopts it.
    #[must_use]
    pub fn orphaned(&self) -> bool {
        self.manifest
            .path
            .as_deref()
            .is_some_and(|path| matches!(path.try_exists(), Ok(false)))
    }

    /// One line for people: id, the workspace (or what it was), last use.
    #[must_use]
    pub fn describe(&self) -> String {
        let place = match (&self.manifest.path, &self.manifest.former_path) {
            (Some(path), _) if self.orphaned() => format!("{} (gone)", path.display()),
            (Some(path), _) => path.display().to_string(),
            (None, Some(former)) => format!("set aside, was {}", former.display()),
            (None, None) => "set aside".to_string(),
        };
        let mut line = format!("{}\t{place}", self.id);
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
    let mut found = homes
        .iter()
        .filter(|home| home.manifest.path.as_deref() == Some(workspace));
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
        "\nif one of them is this workspace moved, /rehome <id> adopts it here (rkat-tui --rehome <id> from the shell); the new state is set aside, not deleted",
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

/// The realm `reference` names among `homes`: its id, else the workspace
/// path it serves or served (`reference` taken as a path, absolute
/// against the working directory, and canonicalized when it exists).
///
/// # Errors
///
/// [`HomeError::UnknownRealm`], [`HomeError::Ambiguous`].
pub fn resolve_ref<'a>(homes: &'a [Home], reference: &str) -> Result<&'a Home, HomeError> {
    if let Some(home) = homes.iter().find(|home| home.id == reference) {
        return Ok(home);
    }
    let given = Path::new(reference);
    let absolute = if given.is_absolute() {
        given.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(given))
            .unwrap_or_else(|_| given.to_path_buf())
    };
    let wanted: Vec<PathBuf> = [
        Some(normalize(&absolute)),
        std::fs::canonicalize(&absolute).ok(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let named: Vec<&Home> = homes
        .iter()
        .filter(|home| {
            [&home.manifest.path, &home.manifest.former_path]
                .into_iter()
                .flatten()
                .any(|path| wanted.contains(path))
        })
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

/// Prove that no daemon holds `home` (under `root`): no live or unreadable
/// lease in the realm, and every endpoint file of the workspace it serves
/// or served absent or refusing a connect. Anything else is no proof.
///
/// # Errors
///
/// [`HomeError::InUse`] naming what was found.
pub async fn prove_idle(root: &Path, home: &Home) -> Result<(), HomeError> {
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
    let workspaces = [&home.manifest.path, &home.manifest.former_path]
        .into_iter()
        .flatten();
    for workspace in workspaces {
        for file in crate::daemon::endpoint::candidates(
            workspace,
            &crate::daemon::endpoint::RealmKey::Default,
        ) {
            crate::daemon::endpoint::check_stale(&file)
                .await
                .map_err(|err| in_use(err.to_string()))?;
        }
    }
    Ok(())
}

/// What [`rehome`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rehomed {
    /// The realm that serves the workspace now.
    pub home: Home,
    /// The workspace it served before.
    pub from: Option<PathBuf>,
    /// The realm the workspace had, now set aside.
    pub set_aside: Option<Home>,
}

impl std::fmt::Display for Rehomed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "realm {} now serves this workspace", self.home.id)?;
        if let Some(from) = &self.from {
            write!(f, " (it served {})", from.display())?;
        }
        if let Some(aside) = &self.set_aside {
            write!(
                f,
                "; the realm it had, {}, is set aside (rkat-tui --rehome {} brings it back)",
                aside.id, aside.id
            )?;
        }
        Ok(())
    }
}

/// Re-home the realm `reference` names (its id or the workspace path it
/// served) to `workspace` (canonical), under the lock: refused while a
/// daemon may hold it or the realm `workspace` has now, and when its own
/// workspace still exists. The realm `workspace` had is set aside (its
/// manifest keeps the path as `former_path`) before the adopted one names
/// it, so a failure in between leaves the workspace with no realm, never
/// with two.
///
/// # Errors
///
/// See [`resolve_ref`], [`prove_idle`]; [`HomeError::AlreadyHere`],
/// [`HomeError::StillHomed`], [`HomeError::Duplicate`], and
/// [`HomeError::Io`] when a manifest cannot be written.
pub async fn rehome(root: &Path, reference: &str, workspace: &Path) -> Result<Rehomed, HomeError> {
    let _lock = Lock::take(root)?;
    let target = check_rehome(root, reference, workspace)?;
    let homes = scan(root)?;
    let current = find_in(&homes, workspace)?.cloned();
    prove_idle(root, &target).await?;
    if let Some(current) = &current {
        prove_idle(root, current).await?;
    }
    let set_aside = match current {
        Some(mut current) => {
            current.manifest.former_path = current.manifest.path.take();
            current.write()?;
            Some(current)
        }
        None => None,
    };
    let mut home = target;
    let from = home
        .manifest
        .path
        .take()
        .or_else(|| home.manifest.former_path.clone());
    home.manifest.former_path.clone_from(&from);
    home.manifest.path = Some(workspace.to_path_buf());
    home.manifest.name = name_of(workspace);
    home.manifest.git_remote = git_remote(workspace).or(home.manifest.git_remote);
    home.manifest.last_used = now();
    home.write()?;
    Ok(Rehomed {
        home,
        from,
        set_aside,
    })
}

/// `rkat-tui --rehome <reference>`: [`rehome`] the realm `reference`
/// names to the workspace `args` name (`--workspace`, else the working
/// directory), under the config directory `args` name.
///
/// # Errors
///
/// See [`rehome`] and [`super::scope::lookup`].
pub async fn rehome_workspace(
    args: &crate::cli::Args,
    reference: &str,
) -> Result<Rehomed, crate::TuiError> {
    let (root, workspace, _) = super::scope::lookup(args)?;
    Ok(rehome(&root, reference, &workspace).await?)
}

/// [`rehome_workspace`] once the daemon of this workspace was asked to
/// stop (`/rehome` in the TUI): a realm proven in use is tried again until
/// `wait` has passed (the daemon releases its lease as it ends).
///
/// # Errors
///
/// See [`rehome_workspace`]; the last [`HomeError::InUse`] after `wait`.
pub async fn rehome_after_stop(
    args: &crate::cli::Args,
    reference: &str,
    wait: std::time::Duration,
) -> Result<Rehomed, crate::TuiError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match rehome_workspace(args, reference).await {
            Err(crate::TuiError::RealmHome(HomeError::InUse { .. }))
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            done => return done,
        }
    }
}

/// `/rehome` without an argument: this workspace's realm (`current`) and
/// the others under `root` that a re-home could adopt (those whose
/// workspace is gone or that were set aside), with how.
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
    let others: Vec<&Home> = homes
        .iter()
        .filter(|home| home.orphaned() || home.manifest.path.is_none())
        .collect();
    if others.is_empty() {
        text.push_str("; no other workspace's state is waiting to be adopted");
        return Ok(text);
    }
    text.push_str("; state that /rehome <id> can adopt here:");
    for home in others {
        text.push_str("\n  ");
        text.push_str(&home.describe().replace('\t', "  "));
    }
    text.push_str(
        "\n/rehome <id> stops this workspace's daemon and quits, then adopts it (this workspace's state is set aside, not deleted)",
    );
    Ok(text)
}

/// Check that `reference` names a realm under `root` that may be
/// re-homed to `workspace` (canonical) as far as can be told before the
/// daemon stops: it exists, is not this workspace's, and its own
/// workspace is gone (or it was set aside).
///
/// # Errors
///
/// See [`resolve_ref`]; [`HomeError::AlreadyHere`], [`HomeError::StillHomed`].
pub fn check_rehome(root: &Path, reference: &str, workspace: &Path) -> Result<Home, HomeError> {
    let homes = scan(root)?;
    let target = resolve_ref(&homes, reference)?.clone();
    if target.manifest.path.as_deref() == Some(workspace) {
        return Err(HomeError::AlreadyHere(target.id));
    }
    if let Some(path) = &target.manifest.path
        && !target.orphaned()
    {
        return Err(HomeError::StillHomed {
            id: target.id,
            path: path.clone(),
        });
    }
    Ok(target)
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
        assert_eq!(opened.home.manifest.path.as_deref(), Some(ws.as_path()));
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
    fn orphans_are_offered_and_set_aside_realms_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let gone = tmp.path().join("gone");
        plant(&root, "dddd", &manifest(&gone));
        let mut aside = manifest(&gone);
        aside.former_path = aside.path.take();
        plant(&root, "eeee", &aside);
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

    #[tokio::test]
    async fn a_rehome_moves_the_path_and_sets_the_new_realm_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let old = tmp.path().join("old");
        plant(&root, "ffff", &manifest(&old));
        let ws = tmp.path().join("new");
        std::fs::create_dir_all(&ws).unwrap();
        let fresh = open_or_create(&root, &ws).unwrap().home;
        // By the old path.
        let done = rehome(&root, old.to_str().unwrap(), &ws).await.unwrap();
        assert_eq!(done.home.id, "ffff");
        assert_eq!(done.from.as_deref(), Some(old.as_path()));
        assert_eq!(
            done.set_aside.as_ref().map(|h| h.id.clone()),
            Some(fresh.id.clone())
        );
        let found = find(&root, &ws).unwrap().unwrap();
        assert_eq!(found.id, "ffff");
        assert_eq!(found.manifest.former_path.as_deref(), Some(old.as_path()));
        // The set-aside realm is still there and comes back by its id.
        let back = rehome(&root, &fresh.id, &ws).await.unwrap();
        assert_eq!(back.home.id, fresh.id);
        assert_eq!(find(&root, &ws).unwrap().unwrap().id, fresh.id);
        // Refusals: the realm here already, an unknown one, one whose
        // workspace still exists.
        assert!(matches!(
            rehome(&root, &fresh.id, &ws).await,
            Err(HomeError::AlreadyHere(_))
        ));
        assert!(matches!(
            rehome(&root, "nope", &ws).await,
            Err(HomeError::UnknownRealm(_))
        ));
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let other = open_or_create(&root, &elsewhere).unwrap().home;
        assert!(matches!(
            rehome(&root, &other.id, &ws).await,
            Err(HomeError::StillHomed { .. })
        ));
    }

    #[tokio::test]
    async fn a_rehome_is_refused_while_a_lease_is_live_or_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let old = tmp.path().join("old");
        let dir = plant(&root, "abcd", &manifest(&old));
        let ws = tmp.path().join("new");
        std::fs::create_dir_all(&ws).unwrap();
        let lease = meerkat_store::start_realm_lease_in(&root, "abcd", None, "test")
            .await
            .unwrap();
        let err = rehome(&root, "abcd", &ws).await.unwrap_err();
        assert!(matches!(err, HomeError::InUse { .. }), "{err}");
        assert!(err.to_string().contains("live lease"), "{err}");
        lease.shutdown().await;
        // A lease that cannot be read proves nothing either.
        std::fs::create_dir_all(dir.join("leases")).unwrap();
        std::fs::write(dir.join("leases").join("x.json"), "{").unwrap();
        let err = rehome(&root, "abcd", &ws).await.unwrap_err();
        assert!(err.to_string().contains("cannot be read"), "{err}");
        std::fs::remove_file(dir.join("leases").join("x.json")).unwrap();
        rehome(&root, "abcd", &ws).await.unwrap();
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
