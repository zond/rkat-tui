//! `/remember` and `/memory`.
//!
//! `/remember <text>` (or a single-line `#text`) appends `- <text>` to the
//! workspace `AGENTS.md` (created with a header when missing) and hands the
//! line to the agent as durable system context ([`AgentContext`],
//! idempotent per session and line), so the model reads it from its next
//! request; new sessions load it with `AGENTS.md`.
//!
//! `/memory` opens the workspace or user-global `AGENTS.md`, or `SYSTEM.md`,
//! in the editor; an edited `AGENTS.md` reaches the agent the way
//! `/remember` does, `SYSTEM.md` applies from the next session.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{AgentContext, Workbench};
use crate::bootstrap::dirs::GlobalDirs;
use crate::bootstrap::instructions::{
    SYSTEM_MD_HEADER, append_remember_line, edited_context, remember_line, remember_target,
    remembered_context,
};
use crate::ui::{MemoryTarget, Msg, NoticeLevel};

impl Workbench {
    /// [`crate::ui::Effect::Remember`]: write the line to `AGENTS.md` and
    /// hand it to `agent` (by a task of its own: the daemon's answer is
    /// waited for there, and the notice says how it went).
    pub fn remember(&self, text: &str, agent: &AgentContext) {
        let Some(line) = remember_line(text) else {
            self.notice(NoticeLevel::Warn, "nothing to remember");
            return;
        };
        let path = remember_target(&self.scope.context_root);
        if let Err(err) = append_remember_line(&path, &line) {
            self.notice(
                NoticeLevel::Error,
                format!("could not write {}: {err}", path.display()),
            );
            return;
        }
        let key = remember_key(&line);
        let agent = agent.clone();
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (level, text) = match agent
                .append_system_context(remembered_context(&line), key)
                .await
            {
                Ok(()) => (
                    NoticeLevel::Info,
                    format!(
                        "remembered in {}: the agent reads it from its next model call; it is fully applied (as part of AGENTS.md) from the next session",
                        path.display()
                    ),
                ),
                Err(err) => (
                    NoticeLevel::Warn,
                    format!(
                        "remembered in {}, but the agent could not take it now ({err}); it applies from the next session",
                        path.display()
                    ),
                ),
            };
            let _ = ui_tx.send(Msg::Notice { level, text });
        });
    }

    /// [`crate::ui::Effect::OpenMemory`]: resolve the file (a missing
    /// `SYSTEM.md` is created with [`SYSTEM_MD_HEADER`]), remember its
    /// bytes, and ask the UI to open it ([`Msg::EditFile`]).
    pub fn open_memory(&mut self, target: MemoryTarget) {
        let scope = &self.scope;
        let path = match memory_path(target, &scope.context_root, scope.global.as_ref()) {
            Ok(path) => path,
            Err(why) => {
                self.notice(NoticeLevel::Warn, why);
                return;
            }
        };
        if let Err(err) = prepare_memory_file(target, &path) {
            self.notice(
                NoticeLevel::Error,
                format!("could not prepare {}: {err}", path.display()),
            );
            return;
        }
        self.memory_before = Some((path.clone(), std::fs::read(&path).ok()));
        self.send(Msg::EditFile { target, path });
    }

    /// [`crate::ui::Effect::MemoryEdited`]: an edited `AGENTS.md` reaches
    /// `agent` the way `/remember` does (its new content as durable system
    /// context, idempotent per path and content); `SYSTEM.md`, an emptied
    /// file and anything the agent cannot take apply from the next session.
    pub fn memory_edited(&mut self, target: MemoryTarget, path: &Path, agent: &AgentContext) {
        let before = match self.memory_before.take() {
            Some((p, before)) if p == path => before,
            _ => None,
        };
        let now = std::fs::read(path).ok();
        if now == before {
            self.notice(
                NoticeLevel::Info,
                format!("{} is unchanged", path.display()),
            );
            return;
        }
        let next = format!(
            "saved {}; it applies from the next session (/new)",
            path.display()
        );
        let content = now
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let context = match target {
            MemoryTarget::System => None,
            MemoryTarget::Workspace | MemoryTarget::User => edited_context(path, &content),
        };
        let Some(context) = context else {
            self.notice(NoticeLevel::Info, next);
            return;
        };
        let key = memory_key(&context);
        let agent = agent.clone();
        let ui_tx = self.ui_tx.clone();
        let path = path.to_path_buf();
        tokio::spawn(async move {
            let (level, text) = match agent.append_system_context(context, key).await {
                Ok(()) => (
                    NoticeLevel::Info,
                    format!(
                        "saved {}: the agent reads the new content from its next model call; new sessions load it as usual",
                        path.display()
                    ),
                ),
                Err(err) => (
                    NoticeLevel::Warn,
                    format!("{next} (the agent could not take it now: {err})"),
                ),
            };
            let _ = ui_tx.send(Msg::Notice { level, text });
        });
    }
}

/// The file `/memory` opens: the workspace `AGENTS.md` (the one
/// `/remember` writes), or the user-global `AGENTS.md` or `SYSTEM.md`.
///
/// # Errors
///
/// A notice when there is no user config directory for the global files.
fn memory_path(
    target: MemoryTarget,
    workspace: &Path,
    global: Option<&GlobalDirs>,
) -> Result<PathBuf, &'static str> {
    match (target, global) {
        (MemoryTarget::Workspace, _) => Ok(remember_target(workspace)),
        (MemoryTarget::User, Some(global)) => Ok(global.agents_md()),
        (MemoryTarget::System, Some(global)) => Ok(global.system_md()),
        (MemoryTarget::User | MemoryTarget::System, None) => Err(
            "there is no user config directory (--config-dir) for /memory user or /memory system",
        ),
    }
}

/// Create `path`'s directory, and a missing `SYSTEM.md` with
/// [`SYSTEM_MD_HEADER`] (an `AGENTS.md` the editor creates itself).
fn prepare_memory_file(target: MemoryTarget, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if target == MemoryTarget::System && !path.exists() {
        std::fs::write(path, format!("{SYSTEM_MD_HEADER}\n"))?;
    }
    Ok(())
}

/// The idempotency key of an edited instruction file's system context (a
/// SHA-256 digest of it, which names the path).
fn memory_key(context: &str) -> String {
    let digest = Sha256::digest(context.as_bytes());
    let mut key = String::with_capacity(16 + 64);
    key.push_str("rkat-tui:memory:");
    for byte in digest {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// The idempotency key of a remembered line (per session: the same line
/// twice is one system message). A SHA-256 digest of the line, so the key
/// is the same across processes, builds and toolchains.
fn remember_key(line: &str) -> String {
    let digest = Sha256::digest(line.as_bytes());
    let mut key = String::with_capacity(18 + 64);
    key.push_str("rkat-tui:remember:");
    for byte in digest {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

#[cfg(test)]
mod tests {
    use super::{MemoryTarget, memory_key, memory_path, prepare_memory_file, remember_key};
    use crate::bootstrap::dirs::GlobalDirs;
    use crate::bootstrap::instructions::{SYSTEM_MD_HEADER, system_md_is_empty};

    #[test]
    #[allow(clippy::unwrap_used)]
    fn memory_opens_the_workspace_user_or_system_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let global = GlobalDirs {
            config: tmp.path().join("config"),
            cache: tmp.path().join("cache"),
        };
        // The workspace file is the one /remember writes: AGENTS.md, else
        // an existing .rkat/AGENTS.md.
        assert_eq!(
            memory_path(MemoryTarget::Workspace, &ws, None),
            Ok(ws.join("AGENTS.md"))
        );
        std::fs::create_dir_all(ws.join(".rkat")).unwrap();
        std::fs::write(ws.join(".rkat/AGENTS.md"), "x").unwrap();
        assert_eq!(
            memory_path(MemoryTarget::Workspace, &ws, Some(&global)),
            Ok(ws.join(".rkat/AGENTS.md"))
        );
        assert_eq!(
            memory_path(MemoryTarget::User, &ws, Some(&global)),
            Ok(global.config.join("AGENTS.md"))
        );
        assert_eq!(
            memory_path(MemoryTarget::System, &ws, Some(&global)),
            Ok(global.config.join("SYSTEM.md"))
        );
        assert!(memory_path(MemoryTarget::System, &ws, None).is_err());
        assert!(memory_path(MemoryTarget::User, &ws, None).is_err());

        // A missing SYSTEM.md starts with its header, which counts as
        // empty; an existing one and an AGENTS.md are left alone.
        let system = global.system_md();
        prepare_memory_file(MemoryTarget::System, &system).unwrap();
        let created = std::fs::read_to_string(&system).unwrap();
        assert_eq!(created, format!("{SYSTEM_MD_HEADER}\n"));
        assert!(system_md_is_empty(&created));
        std::fs::write(&system, "You are X.").unwrap();
        prepare_memory_file(MemoryTarget::System, &system).unwrap();
        assert_eq!(std::fs::read_to_string(&system).unwrap(), "You are X.");
        let user = global.agents_md();
        std::fs::remove_dir_all(&global.config).unwrap();
        prepare_memory_file(MemoryTarget::User, &user).unwrap();
        assert!(global.config.is_dir() && !user.exists());
    }

    #[test]
    fn memory_keys_follow_the_context() {
        assert_eq!(memory_key("a"), memory_key("a"));
        assert_ne!(memory_key("a"), memory_key("b"));
        assert!(memory_key("a").starts_with("rkat-tui:memory:"));
    }

    #[test]
    fn remember_keys_are_stable_digests_per_line() {
        assert_eq!(remember_key("a"), remember_key("a"));
        assert_ne!(remember_key("a"), remember_key("b"));
        // Pinned: SHA-256("a"), the same in every process and build.
        assert_eq!(
            remember_key("a"),
            "rkat-tui:remember:ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
        );
    }
}
