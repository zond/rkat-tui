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
//!   restart.
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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

impl GrantScope {
    /// How the scope reads in a notice.
    #[must_use]
    pub fn words(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "for this session",
            Self::Always => "always",
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
}

impl GrantTarget {
    /// What the grant allows, as notices and the agent's note say it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Dir { path } => format!("writes under `{}`", path.display()),
            Self::Rule { tool, subject } => format!("`{tool} {subject}`"),
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
    /// Where it comes from: `default`, `flag`, `config`, `session` or
    /// `always`.
    pub origin: String,
    /// Whether `/permissions` can remove it (a session grant).
    pub removable: bool,
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
