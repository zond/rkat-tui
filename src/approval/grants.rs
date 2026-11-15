//! What the user allows while the daemon runs: the calls the gate recently
//! blocked, which `/approve` lists, and the grants an approval makes.
//!
//! A blocked call ([`BlockedCall`]) is kept per session, the newest
//! [`MAX_RECENT_BLOCKS`]: a classifier block, a review that could not be
//! completed, an unattended call that would have asked the user. The user
//! approves one of them:
//!
//! - **once** ([`GrantScope::Once`]): the next identical call of the
//!   session passes, as the escalation dialog's allow once does;
//! - **for this session** ([`GrantScope::Session`]): a grant that lives in
//!   the daemon for the session and its helpers, applied live;
//! - **always** ([`GrantScope::Always`]): the same grant written through
//!   the realm's trusted writer, so it holds for every session and after a
//!   restart;
//! - **always, in every workspace** ([`GrantScope::AlwaysUser`]): the same
//!   written to the user's own `tui.toml` instead.
//!
//! The grant ([`GrantTarget`]) is a directory when a path outside the
//! allowed scope is why the call was blocked (a file tool's own target, or
//! the classifier's `PATH:` line): the path's own directory, or its parent
//! ([`grant_dir_for`]), never wider, never a secret or sealed root, `/`,
//! the home directory itself or a system directory. A directory grant joins
//! the directories the user named for writing (`GrantedDirs::user_write`):
//! a file-tool write there is an in-project edit and the classifier is told
//! the user allows it. Without such a path the grant is a literal allow
//! rule for the call's tool and subject (the dialog's "allow and add a
//! rule"). A directory grant reaches the gate at once; the sandbox of a
//! confined shell is planned at the daemon's start, so the shell itself can
//! write an "always" directory only from the next start on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use meerkat_core::SessionId;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use super::classifier::GrantedDirs;
use super::rules::{Rule, RuleKind, RuleOrigin};
use crate::paths::ProjectRoot;

/// How many blocked calls a session keeps for `/approve`.
pub const MAX_RECENT_BLOCKS: usize = 10;

/// How long an approval holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// The next identical call passes.
    Once,
    /// The grant lives for the session and its helpers.
    Session,
    /// The grant is written to the realm and holds for every session.
    Always,
    /// The grant is written to the user's own `tui.toml` and holds for
    /// every session of every workspace.
    AlwaysUser,
}

impl GrantScope {
    /// How the scope reads in a notice.
    #[must_use]
    pub fn words(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "for this session",
            Self::Always => "always",
            Self::AlwaysUser => "always, in every workspace",
        }
    }

    /// The file an "always" grant is written to: the user's own for
    /// [`Self::AlwaysUser`], else the workspace's.
    #[must_use]
    pub fn level(self) -> crate::config::registry::Level {
        match self {
            Self::AlwaysUser => crate::config::registry::Level::User,
            _ => crate::config::registry::Level::Workspace,
        }
    }
}

/// What a grant allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrantTarget {
    /// Writes (and reads) under this directory.
    Dir {
        /// The canonical directory.
        path: PathBuf,
    },
    /// The literal allow rule of this tool and subject.
    Rule {
        /// The tool.
        tool: String,
        /// The call's subject, matched literally.
        subject: String,
    },
    /// The project's builds and tests after the session changed its
    /// dependencies or build files, with nothing containing the shell
    /// (asked of the user otherwise; for a session only).
    Builds,
}

impl GrantTarget {
    /// What the grant allows, as notices and the agent's note say it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Dir { path } => format!("writes under `{}`", path.display()),
            Self::Rule { tool, subject } => format!("`{tool} {subject}`"),
            Self::Builds => {
                "builds and tests of this project after a dependency change".to_string()
            }
        }
    }
}

/// A call the gate blocked that the user can approve (`/approve`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedCall {
    /// Its number in the daemon (stable while it is listed).
    pub id: u64,
    /// The tool.
    pub tool: String,
    /// The helper that made the call (`None`: the agent itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<String>,
    /// The call's subject in short (a command, a path), at most 200 bytes.
    pub summary: String,
    /// The block rule the classifier named, if it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// The path outside the allowed scope that was why, if one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The kind of effect the classifier objected to, in one general
    /// sentence (its `EFFECT:` line), if it named one: `/approve` offers
    /// it, editable, as a statement ([`super::statements`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    /// Why it was blocked (the denial as the user reads it).
    pub reason: String,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
    /// The grant "for this session" and "always" would make, if one fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantTarget>,
}

/// A grant in force, as `/permissions` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRow {
    /// Its number (a session grant's can be removed by it).
    pub id: u64,
    /// What it allows.
    pub target: GrantTarget,
    /// Where it comes from.
    pub origin: GrantOrigin,
    /// Whether `/permissions` can remove it (a session grant).
    pub removable: bool,
}

/// Where a grant in force comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantOrigin {
    /// The sandbox grants it by default.
    Default,
    /// The daemon's command line.
    Flag,
    /// A `tui.toml` file (`[confinement] extra_write`).
    Config,
    /// Granted "always" while this daemon runs (written to a `tui.toml`).
    Always,
    /// Granted for the session.
    Session,
}

impl GrantOrigin {
    /// How `/permissions` names it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Flag => "flag",
            Self::Config => "config",
            Self::Always => "always",
            Self::Session => "session",
        }
    }
}

/// Why no directory grant fits a path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantRefusal {
    /// The path does not resolve.
    #[error("`{0}` does not resolve")]
    Unresolved(String),
    /// Neither the path nor its parent is an existing directory.
    #[error("neither `{0}` nor its parent is an existing directory")]
    NoDirectory(String),
    /// The directory is one no grant may name.
    #[error("`{0}` is never granted ({1})")]
    Refused(String, &'static str),
}

/// System directories no grant names, nor anything below them.
const SYSTEM_DIRS: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/proc", "/sbin", "/sys", "/usr",
    "/System", "/Library",
];

/// The directory a grant for `raw` (a path the call writes or reads)
/// names: the path itself when it is an existing directory, else its
/// parent when that is one. Never wider than the parent, and refused for
/// the root, the home directory itself, a system directory, the project
/// itself and the gate's sealed and secret roots.
///
/// # Errors
///
/// [`GrantRefusal`] when no directory fits.
pub fn grant_dir_for(root: &ProjectRoot, raw: &str) -> Result<PathBuf, GrantRefusal> {
    let resolved = root
        .resolve(raw)
        .map_err(|_| GrantRefusal::Unresolved(raw.to_string()))?;
    let dir = if resolved.abs.is_dir() {
        resolved.abs
    } else {
        resolved
            .abs
            .parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf)
            .ok_or_else(|| GrantRefusal::NoDirectory(raw.to_string()))?
    };
    let shown = dir.display().to_string();
    let refuse = |why| Err(GrantRefusal::Refused(shown.clone(), why));
    let checked = root
        .resolve(&shown)
        .map_err(|_| GrantRefusal::Unresolved(shown.clone()))?;
    if dir == Path::new("/") {
        return refuse("the root directory");
    }
    if dirs::home_dir()
        .and_then(|home| std::fs::canonicalize(home).ok())
        .is_some_and(|home| home == dir)
    {
        return refuse("the home directory itself");
    }
    if SYSTEM_DIRS.iter().any(|system| dir.starts_with(system)) {
        return refuse("a system directory");
    }
    if checked.inside() || root.as_path().starts_with(&dir) {
        return refuse("the project or a directory holding it");
    }
    if checked.sealed || checked.secret {
        return refuse("rkat-tui's own state");
    }
    Ok(dir)
}

/// Where a session's private scratch directory is, if it has one
/// ([`super::scratch`]): the daemon answers from the agent the session
/// belongs to.
pub type ScratchLookup = Arc<dyn Fn(&SessionId) -> Option<PathBuf> + Send + Sync>;

/// The directories and rules the user grants, with the confinement plan's
/// directories they join: the engine's
/// (`crate::approval::ApprovalEngine::grants`). It owns the "always"
/// directories of this run and each session's grants; the files that hold
/// "always" grants for the next start are written by the engine.
///
/// Locks: `always` and `sessions` are never held together, nor across the
/// scratch lookup (the daemon's callback).
pub struct Grants {
    /// The confinement plan's directories (the sandbox's defaults and the
    /// user's named ones).
    plan: GrantedDirs,
    /// Each session's scratch directory ([`Self::set_scratch`]); unset:
    /// none has one.
    scratch: OnceLock<ScratchLookup>,
    /// Directories the user granted "always" while this daemon runs (the
    /// realm's `tui.toml` holds them for the next start), by id.
    always: RwLock<Vec<(u64, PathBuf)>>,
    /// The grants the user made for each session and its helpers, by id.
    sessions: Mutex<HashMap<SessionId, Vec<(u64, GrantTarget)>>>,
}

impl Grants {
    /// The grants over the confinement plan's directories `plan`.
    pub(super) fn new(plan: GrantedDirs) -> Self {
        Self {
            plan,
            scratch: OnceLock::new(),
            always: RwLock::new(Vec::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Where each session's private scratch directory is: the gate treats
    /// it as the session's own from now on ([`super::scratch`]). Set once;
    /// returns `false` when it was set already.
    pub fn set_scratch(&self, lookup: ScratchLookup) -> bool {
        self.scratch.set(lookup).is_ok()
    }

    /// `session`'s scratch directory, if it has one.
    pub(super) fn scratch(&self, session: &SessionId) -> Option<PathBuf> {
        self.scratch.get().and_then(|lookup| lookup(session))
    }

    /// Whether the confinement plan lets the shell write `path` (a default
    /// or user-named write grant holds it): a scratch directory elsewhere
    /// would be a `TMPDIR` the confined shell cannot write.
    #[must_use]
    pub fn plan_writes(&self, path: &Path) -> bool {
        self.plan
            .baseline_write
            .iter()
            .chain(&self.plan.user_write)
            .any(|dir| path.starts_with(dir))
    }

    /// The directories outside the project `session`'s calls may use: the
    /// confinement plan's, the session's scratch directory, and what the
    /// user granted while the daemon runs ("always" for every session,
    /// "for this session" for this one and its helpers).
    pub(super) fn dirs_for(&self, session: &SessionId) -> GrantedDirs {
        let mut granted = self.plan.clone();
        granted.scratch = self.scratch(session);
        granted
            .user_write
            .extend(self.always.read().iter().map(|(_, dir)| dir.clone()));
        if let Some(grants) = self.sessions.lock().get(session) {
            for (_, grant) in grants {
                if let GrantTarget::Dir { path } = grant {
                    granted.user_write.push(path.clone());
                }
            }
        }
        granted
    }

    /// The allow rules the user granted for `session`.
    pub(super) fn session_rules(&self, session: &SessionId) -> Vec<Rule> {
        let sessions = self.sessions.lock();
        let Some(grants) = sessions.get(session) else {
            return Vec::new();
        };
        grants
            .iter()
            .filter_map(|(_, grant)| match grant {
                GrantTarget::Rule { tool, subject } => {
                    Rule::literal(RuleKind::Allow, tool, subject, RuleOrigin::Session).ok()
                }
                _ => None,
            })
            .collect()
    }

    /// Whether the user allowed builds after a change for `session`.
    pub(super) fn builds_granted(&self, session: &SessionId) -> bool {
        self.sessions.lock().get(session).is_some_and(|grants| {
            grants
                .iter()
                .any(|(_, grant)| *grant == GrantTarget::Builds)
        })
    }

    /// Grant `target` (number `id`) for `session` and its helpers.
    pub(super) fn add_session(&self, session: &SessionId, id: u64, target: GrantTarget) {
        self.sessions
            .lock()
            .entry(session.clone())
            .or_default()
            .push((id, target));
    }

    /// The directory an "always" grant of `path` writes for a project at
    /// `root`, checked against the directories already named.
    ///
    /// # Errors
    ///
    /// Why it cannot be granted always, for the user.
    pub(super) fn always_dir(&self, path: &Path, root: &Path) -> Result<PathBuf, String> {
        let named: Vec<PathBuf> = self
            .plan
            .user_write
            .iter()
            .chain(&self.plan.user_read)
            .cloned()
            .chain(self.always.read().iter().map(|(_, dir)| dir.clone()))
            .collect();
        crate::bootstrap::confinement::grantable_extra_write(path, root, &named).map_err(
            |problem| {
                format!(
                    "`{}` cannot be granted always ({problem}); grant it for this session instead",
                    path.display()
                )
            },
        )
    }

    /// Grant directory `dir` (number `id`) always, for this run.
    pub(super) fn add_always(&self, id: u64, dir: PathBuf) {
        self.always.write().push((id, dir));
    }

    /// The grants in force for `session`, as `/permissions` lists them:
    /// the sandbox's defaults, the user's from the command line and the
    /// `tui.toml` files, the "always" ones of this run, and the session's.
    #[must_use]
    pub fn rows(&self, session: Option<&SessionId>) -> Vec<GrantRow> {
        let dir = |path: &PathBuf| GrantTarget::Dir { path: path.clone() };
        let mut rows: Vec<GrantRow> = self
            .plan
            .baseline_write
            .iter()
            .map(|path| GrantRow {
                id: 0,
                target: dir(path),
                origin: GrantOrigin::Default,
                removable: false,
            })
            .collect();
        for path in &self.plan.user_write {
            rows.push(GrantRow {
                id: 0,
                target: dir(path),
                origin: if self.plan.from_flag.contains(path) {
                    GrantOrigin::Flag
                } else {
                    GrantOrigin::Config
                },
                removable: false,
            });
        }
        for (id, path) in self.always.read().iter() {
            rows.push(GrantRow {
                id: *id,
                target: dir(path),
                origin: GrantOrigin::Always,
                removable: false,
            });
        }
        if let Some(grants) = session.and_then(|s| self.sessions.lock().get(s).cloned()) {
            for (id, target) in grants {
                rows.push(GrantRow {
                    id,
                    target,
                    origin: GrantOrigin::Session,
                    removable: true,
                });
            }
        }
        rows
    }

    /// Remove session grant `id` of `session`: what it granted, if it was
    /// one.
    pub(super) fn remove_session(&self, session: &SessionId, id: u64) -> Option<GrantTarget> {
        let mut sessions = self.sessions.lock();
        let grants = sessions.get_mut(session)?;
        let at = grants.iter().position(|(grant, _)| *grant == id)?;
        Some(grants.remove(at).1)
    }

    /// Forget `session`'s grants.
    pub(super) fn forget(&self, session: &SessionId) {
        self.sessions.lock().remove(session);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_names_the_path_s_directory_or_its_parent_and_never_wider() {
        let base = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        let project = base.join("project");
        let store = base.join("store/v3");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let root = ProjectRoot::new(&project).unwrap();
        let shown = |p: &Path| p.display().to_string();
        // A directory: itself; a file in one: its directory.
        assert_eq!(grant_dir_for(&root, &shown(&store)).unwrap(), store);
        assert_eq!(
            grant_dir_for(&root, &shown(&store.join("files/ab"))).unwrap_err(),
            GrantRefusal::NoDirectory(shown(&store.join("files/ab")))
        );
        assert_eq!(
            grant_dir_for(&root, &shown(&store.join("index.json"))).unwrap(),
            store
        );
        // Never the root, a system directory, the project or its parent.
        for refused in ["/", "/etc/passwd", "/usr/lib"] {
            assert!(
                matches!(
                    grant_dir_for(&root, refused),
                    Err(GrantRefusal::Refused(..))
                ),
                "{refused}"
            );
        }
        assert!(matches!(
            grant_dir_for(&root, &shown(&project.join("x.rs"))),
            Err(GrantRefusal::Refused(..))
        ));
        assert!(matches!(
            grant_dir_for(&root, &shown(&base.join("x.txt"))),
            Err(GrantRefusal::Refused(..))
        ));
        if let Some(home) = dirs::home_dir().and_then(|h| std::fs::canonicalize(h).ok()) {
            assert!(matches!(
                grant_dir_for(&root, &shown(&home.join(".bashrc"))),
                Err(GrantRefusal::Refused(..))
            ));
        }
    }

    #[test]
    fn targets_say_what_they_allow() {
        assert_eq!(
            GrantTarget::Dir {
                path: "/data/x".into()
            }
            .describe(),
            "writes under `/data/x`"
        );
        assert_eq!(
            GrantTarget::Rule {
                tool: "shell".into(),
                subject: "cargo info serde".into()
            }
            .describe(),
            "`shell cargo info serde`"
        );
    }
}
