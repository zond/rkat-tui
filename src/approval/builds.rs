//! Build trust: whether a session's builds run as Tier 1, and what a
//! change to the project's build-defining files does to that.
//!
//! A build is trusted as the user set `[auto] trust_project_builds`, else
//! while something contains the shell, and in either case only while no
//! build-defining file ([`is_build_defining`]) changed in the session by
//! any route the gate sees: its file tools ([`Builds::written`]), or the
//! digest of the root's build files against the session's baseline
//! ([`Builds::snapshot`], taken when the session is first seen, and moved
//! by [`Builds::rebase`] when a reviewed build ran). Contained, a build
//! after such a change stays trusted and the first one is noticed
//! ([`Builds::notice`]); uncontained it is put to the user (the engine's
//! `build_asks`).
//!
//! Locks: the per-session map's, alone, never across the file reads.

use std::collections::HashMap;

use meerkat_core::SessionId;
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};

use super::engine::{hash_len, hash_str};
use super::policy::{Action, ActionKind};
use crate::paths::ProjectRoot;

/// Commands whose execution is defined by project files.
const BUILD_RUNNERS: &[&str] = &["cargo", "make", "npm", "pnpm", "yarn", "npx"];

/// Project files that decide what a Rust build executes, besides the
/// manifests ([`super::classifier::MANIFEST_FILES`]). With the manifests
/// they are the build-defining files ([`is_build_defining`]): a change to
/// one makes the next build reviewed even where builds are trusted.
pub(super) const CARGO_BUILD_FILES: &[&str] = &[
    "build.rs",
    ".cargo/config.toml",
    ".cargo/config",
    "rust-toolchain.toml",
    "rust-toolchain",
];

/// Why a build after a dependency change is put to the user when nothing
/// contains the shell.
pub const BUILD_AFTER_CHANGE_ASK: &str = "a build after the agent changed the project's dependencies or build files in this session, and nothing contains the shell (--no-confinement): it runs code nobody reviewed, so you decide";

/// The notice of the first build after such a change when something
/// contains the shell.
pub const BUILD_AFTER_CHANGE_NOTICE: &str =
    "building with dependencies changed in this session; the sandbox contains it";

/// What the session grant of a build after a dependency change covers,
/// as the classifier reads it among the user's statements.
pub const BUILDS_STATEMENT: &str = "building and testing the project after the agent changed its dependencies or build files (the user allowed it for this session)";

/// Whether `subject` (a display path) is a build-defining file anywhere in
/// the project: a dependency manifest or lockfile
/// ([`super::classifier::is_manifest`]) or one of [`CARGO_BUILD_FILES`].
pub(super) fn is_build_defining(subject: &str) -> bool {
    super::classifier::is_manifest(subject)
        || CARGO_BUILD_FILES
            .iter()
            .any(|file| subject == *file || subject.ends_with(&format!("/{file}")))
}

/// Build runners ([`BUILD_RUNNERS`]) invoked anywhere in `command`. Best effort:
/// splits on anything that separates commands and skips `VAR=value` prefixes.
pub(super) fn build_runners(command: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    for piece in command.split([';', '&', '|', '\n', '(', ')', '`', '{', '}']) {
        let word = piece
            .split_whitespace()
            .find(|w| !w.contains('='))
            .map(|w| w.trim_start_matches('\\'));
        let Some(word) = word else { continue };
        let base = word.rsplit('/').next().unwrap_or(word);
        if let Some(runner) = BUILD_RUNNERS.iter().find(|r| **r == base)
            && !found.contains(runner)
        {
            found.push(*runner);
        }
    }
    found
}

/// Whether `action` runs a build (a build runner anywhere in its command).
pub(super) fn is_build(action: &Action<'_>) -> bool {
    matches!(&action.kind, ActionKind::Shell { command, .. } if !build_runners(command).is_empty())
}

/// Whether `action` runs a Cargo build.
pub(super) fn is_cargo_build(action: &Action<'_>) -> bool {
    matches!(&action.kind, ActionKind::Shell { command, .. } if build_runners(command).contains(&"cargo"))
}

/// A session's build trust.
#[derive(Debug, Default)]
struct SessionBuilds {
    /// The digest of the project's build-defining files when the session
    /// was first seen, or when a reviewed build last ran.
    digest: Option<[u8; 32]>,
    /// The session's file tools wrote a build-defining file or a
    /// dependency manifest since then.
    written: bool,
    /// The first build after such a change with the shell contained was
    /// noticed ([`Builds::notice`]).
    noticed: bool,
}

/// Build trust (see the [module docs](self)).
#[derive(Debug)]
pub(super) struct Builds {
    root: ProjectRoot,
    /// Something contains the shell (rkat-tui's sandbox, or one the user
    /// declares).
    contained: bool,
    /// `[auto] trust_project_builds` (`None`: trusted while contained).
    trust: RwLock<Option<bool>>,
    sessions: Mutex<HashMap<SessionId, SessionBuilds>>,
}

impl Builds {
    pub(super) fn new(root: ProjectRoot, contained: bool, trust: Option<bool>) -> Self {
        Self {
            root,
            contained,
            trust: RwLock::new(trust),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Whether something contains the shell.
    pub(super) fn contained(&self) -> bool {
        self.contained
    }

    /// `[auto] trust_project_builds` changed (`None`: unset).
    pub(super) fn set_trust(&self, trust: Option<bool>) {
        *self.trust.write() = trust;
    }

    /// The setting, as it is now.
    pub(super) fn trust(&self) -> Option<bool> {
        *self.trust.read()
    }

    /// SHA-256 over the project root's build-defining files
    /// ([`CARGO_BUILD_FILES`] and the manifests) as they are now (a
    /// missing file counts as missing).
    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        let manifests = super::classifier::MANIFEST_FILES.iter();
        for file in CARGO_BUILD_FILES.iter().chain(manifests) {
            hash_str(file, &mut hasher);
            match std::fs::read(self.root.as_path().join(file)) {
                Ok(bytes) => {
                    hasher.update([1]);
                    hash_len(bytes.len(), &mut hasher);
                    hasher.update(&bytes);
                }
                Err(_) => hasher.update([0]),
            }
        }
        hasher.finalize().into()
    }

    /// Take the snapshot of the build-defining files a session's trusted
    /// builds are checked against, when it has none yet.
    pub(super) fn snapshot(&self, session: &SessionId) {
        if self
            .sessions
            .lock()
            .get(session)
            .is_some_and(|state| state.digest.is_some())
        {
            return;
        }
        let digest = self.digest();
        let mut sessions = self.sessions.lock();
        let state = sessions.entry(session.clone()).or_default();
        if state.digest.is_none() {
            state.digest = Some(digest);
        }
    }

    /// A reviewed build of `session` ran: what it was shown is the new
    /// baseline of its trusted builds.
    pub(super) fn rebase(&self, session: &SessionId) {
        let digest = self.digest();
        let mut sessions = self.sessions.lock();
        let state = sessions.entry(session.clone()).or_default();
        state.digest = Some(digest);
        state.written = false;
    }

    /// The session's file tools were allowed to write a build-defining
    /// file.
    pub(super) fn written(&self, session: &SessionId) {
        self.sessions
            .lock()
            .entry(session.clone())
            .or_default()
            .written = true;
    }

    /// Whether the project's builds are trusted for `session`'s `action`:
    /// as the user set `trust_project_builds`, else when something
    /// contains the shell; and in either case only while no
    /// build-defining file changed in the session by any route the gate
    /// sees (its file tools, or the files' digest).
    ///
    /// When something contains the shell a build stays trusted after such
    /// a change too (the sandbox is the boundary; the owner's decision):
    /// the first one shows a notice ([`Self::notice`]).
    /// Uncontained, a build after a change is put to the user.
    pub(super) fn trusted(&self, session: &SessionId, action: &Action<'_>) -> bool {
        let trusted = self.trust().unwrap_or(self.contained);
        let ActionKind::Shell { command, .. } = &action.kind else {
            return trusted;
        };
        if !trusted || self.contained || !build_runners(command).contains(&"cargo") {
            return trusted;
        }
        !self.changed(session)
    }

    /// Whether a build-defining file or dependency manifest changed in the
    /// session since its baseline, by any route the gate sees (its file
    /// tools, or the files' digest).
    pub(super) fn changed(&self, session: &SessionId) -> bool {
        let (snapshot, written) = {
            let sessions = self.sessions.lock();
            sessions
                .get(session)
                .map_or((None, false), |state| (state.digest, state.written))
        };
        written || snapshot.is_some_and(|digest| digest != self.digest())
    }

    /// Whether `action` is the session's first build after a change with
    /// something containing the shell, to be noticed: if so it is now
    /// noticed, and the caller tells the user.
    pub(super) fn notice(&self, session: &SessionId, action: &Action<'_>) -> bool {
        if !self.contained
            || !is_build(action)
            || self
                .sessions
                .lock()
                .get(session)
                .is_some_and(|state| state.noticed)
            || !self.changed(session)
        {
            return false;
        }
        self.sessions
            .lock()
            .entry(session.clone())
            .or_default()
            .noticed = true;
        true
    }

    /// Forget `session`'s build trust.
    pub(super) fn forget(&self, session: &SessionId) {
        self.sessions.lock().remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build-defining file is one of the Cargo build files or a manifest
    /// of any ecosystem, at any depth; a neighbour of one is not.
    #[test]
    fn build_defining_files_are_the_cargo_files_and_every_manifest() {
        for yes in [
            "build.rs",
            "crates/x/build.rs",
            ".cargo/config.toml",
            "sub/.cargo/config",
            "rust-toolchain.toml",
            "Cargo.lock",
            "web/package.json",
            "go.mod",
            "requirements-dev.txt",
        ] {
            assert!(is_build_defining(yes), "{yes}");
        }
        for no in [
            "src/build.rs.orig",
            "notbuild.rs",
            "src/main.rs",
            "README.md",
        ] {
            assert!(!is_build_defining(no), "{no}");
        }
    }
}
