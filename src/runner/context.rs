//! Context upkeep: `/remember` and the compaction trigger.
//!
//! `/remember <text>` (or a single-line `#text`) appends `- <text>` to the
//! workspace `AGENTS.md` (created with a header when missing) and pushes
//! the line into the live session as durable system context
//! (`append_system_context`, idempotent per session and line), so the model
//! reads it from its next request; new sessions load it with `AGENTS.md`.
//!
//! Compaction is native (see [`crate::ui::compaction`]): every session build
//! carries the policy's trigger for the model's context window
//! ([`Runner::session_trigger`]). `/compact now` asks the engine to compact
//! at the next compaction check (`MeerkatMachine::request_compaction`), with
//! no rebuild. Upstream reads the trigger when the agent is built and
//! exposes no call to change it on a live session, so a trigger change goes
//! through one workaround, [`Runner::rebuild_session_workaround`]: retire the
//! idle session and resume it with the new trigger. It runs only on what the
//! user does: a model switch or a resume that moves the trigger
//! ([`Runner::ensure_trigger`]), a model switch that needed a new profile,
//! and every model switch (the mob tools capture the model helpers run on).
//! A background list fetch never rebuilds. The upstream ask (a live trigger
//! update) is recorded in `docs/evidence.md`.

use sha2::{Digest, Sha256};

use super::Runner;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use meerkat_core::Message;

use crate::bootstrap::dirs::GlobalDirs;
use crate::bootstrap::instructions::{
    SYSTEM_MD_HEADER, SectionKind, append_remember_line, edited_context, remember_line,
    remember_target, remembered_context,
};
use crate::error::TuiError;
use crate::session::SessionHandle;
use crate::ui::{CompactFailure, CompactionTrigger, MemoryTarget, Msg, NoticeLevel};

/// Why the session is rebuilt (see [`Runner::rebuild_session_workaround`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebuildReason {
    /// The compaction trigger moved (model switch, resume onto another
    /// model). Upstream ask: a live trigger update.
    CompactionTrigger,
    /// The agent's model registry predates a profile the switch needs.
    /// Upstream ask: profiles read at reconfigure time.
    ModelProfile,
    /// A model switch: the agent's mob tools captured the model `delegate`
    /// helpers run on at build. Upstream ask: the mob tools read the
    /// session's current model.
    ModelSwitch,
}

/// How [`Runner::rebuild_session_workaround`] failed.
#[derive(Debug, thiserror::Error)]
pub(super) enum RebuildError {
    /// The session could not be retired; it runs on as it was.
    #[error("{0}")]
    Retire(#[source] TuiError),
    /// The rebuild failed and the session was resumed as it was before.
    #[error("{0} (the session was resumed as it was)")]
    KeptPrevious(#[source] TuiError),
    /// The session was retired and could not be resumed: it is detached
    /// until the user resumes it.
    #[error(
        "the session could not be resumed after a rebuild ({first}; retry: {retry}); \
         it is detached: run /resume to reattach it"
    )]
    Detached {
        /// The failed resume with the new settings.
        first: TuiError,
        /// The failed retry with the previous ones.
        #[source]
        retry: TuiError,
    },
}

impl RebuildError {
    /// Whether the runner holds no live session any more.
    pub(super) fn detached(&self) -> bool {
        matches!(self, Self::Detached { .. })
    }
}

impl Runner {
    /// The native auto-compaction trigger the policy gives the current
    /// model (its profile's window; an estimated window counts as unknown).
    pub(super) fn session_trigger(&self) -> CompactionTrigger {
        let (window, estimated) = self.context_window();
        self.template.policy.trigger((!estimated).then_some(window))
    }

    /// The one place the TUI retires and resumes its own idle session (see
    /// the module docs): the agent is rebuilt from the current config with
    /// `trigger`, the event stream followed again, and the identity re-read.
    ///
    /// The runner never keeps a retired session behind a resume that
    /// failed: the resume is retried once with the session's previous
    /// trigger ([`RebuildError::KeptPrevious`]); only when that fails too is
    /// the session left detached ([`RebuildError::Detached`]).
    pub(super) async fn rebuild_session_workaround(
        &mut self,
        reason: RebuildReason,
        trigger: CompactionTrigger,
    ) -> Result<(), RebuildError> {
        tracing::debug!(?reason, ?trigger, "rebuilding the session (upstream workaround)");
        let id = self.session.id().clone();
        let previous = self.session.compaction();
        self.session.retire().await.map_err(RebuildError::Retire)?;
        let result = match SessionHandle::resume(
            &self.runtime,
            &id,
            self.template.options(None, trigger),
        )
        .await
        {
            Ok(session) => {
                self.session = session;
                Ok(())
            }
            Err(first) => {
                tracing::warn!(error = %first, "session rebuild failed; resuming as it was");
                match SessionHandle::resume(&self.runtime, &id, self.template.options(None, previous))
                    .await
                {
                    Ok(session) => {
                        self.session = session;
                        Err(RebuildError::KeptPrevious(first))
                    }
                    Err(retry) => return Err(RebuildError::Detached { first, retry }),
                }
            }
        };
        self.follow_events().await;
        self.refresh_identity().await;
        result
    }

    /// Rebuild the idle session when its agent runs on another trigger than
    /// [`Runner::session_trigger`] (after a model switch or a resume onto
    /// another model). A running turn keeps the session as it is.
    pub(super) async fn ensure_trigger(&mut self) {
        let wanted = self.session_trigger();
        if self.turn.is_active() || self.session.compaction() == wanted {
            return;
        }
        if let Err(err) = self
            .rebuild_session_workaround(RebuildReason::CompactionTrigger, wanted)
            .await
        {
            let level = if err.detached() {
                NoticeLevel::Error
            } else {
                NoticeLevel::Warn
            };
            self.notice(
                level,
                format!("could not apply the compaction trigger to this session: {err}"),
            );
        }
    }

    /// [`crate::ui::Effect::CompactNow`]: ask the engine to compact at the
    /// session's next compaction check (`MeerkatMachine::request_compaction`;
    /// no rebuild); answered with [`Msg::CompactRequested`].
    pub(super) async fn compact_now(&mut self) {
        let result = match self.session.request_compaction().await {
            Ok(receipt) => {
                tracing::debug!(?receipt, "compaction requested");
                Ok(())
            }
            Err(err) => Err(CompactFailure::Request(err.to_string())),
        };
        self.send(Msg::CompactRequested(result));
    }

    /// [`crate::ui::Effect::ContextReport`]: the UI's `header` plus what the
    /// context holds, as one notice.
    pub(super) async fn context_report(&mut self, header: &str) {
        let session = self.session.read().await.map_err(|err| err.to_string());
        let workspace = &self.runtime.scope.context_root;
        let size = |path: &std::path::Path| {
            std::fs::metadata(path)
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|m| usize::try_from(m.len()).unwrap_or(usize::MAX))
        };
        let mut files = Vec::new();
        if let Some(global) = &self.runtime.scope.global {
            let path = global.system_md();
            files.push(InstructionFile {
                label: "SYSTEM.md (replaces the base prompt)".into(),
                bytes: size(&path),
                path,
                truncated: false,
            });
        }
        let agents = remember_target(workspace);
        files.push(InstructionFile {
            label: "workspace AGENTS.md".into(),
            bytes: size(&agents),
            path: agents,
            truncated: false,
        });
        for section in self.template.instructions.discover() {
            let label = match section.kind {
                SectionKind::UserGlobal => "user AGENTS.md",
                SectionKind::Ancestor => "parent AGENTS.md",
                SectionKind::Local => "AGENTS.local.md",
            };
            files.push(InstructionFile {
                label: label.into(),
                bytes: Some(section.body.len()),
                path: section.path,
                truncated: section.truncated,
            });
        }
        let tools = self.template.external_tools.tools().len();
        let text = context_breakdown(
            header,
            session.as_ref().map(|s| s.messages()).map_err(Clone::clone),
            &files,
            tools,
        );
        self.notice(NoticeLevel::Info, text);
    }

    /// [`crate::ui::Effect::OpenMemory`]: resolve the file (a missing
    /// `SYSTEM.md` is created with [`SYSTEM_MD_HEADER`]), remember its
    /// bytes, and ask the UI to open it ([`Msg::EditFile`]).
    pub(super) fn open_memory(&mut self, target: MemoryTarget) {
        let scope = &self.runtime.scope;
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
    /// the live session the way `/remember` does (its new content as
    /// durable system context, idempotent per path and content); `SYSTEM.md`,
    /// an emptied file and anything the session cannot take apply from the
    /// next session.
    pub(super) async fn memory_edited(&mut self, target: MemoryTarget, path: &Path) {
        let before = match self.memory_before.take() {
            Some((p, before)) if p == path => before,
            _ => None,
        };
        let now = std::fs::read(path).ok();
        if now == before {
            self.notice(NoticeLevel::Info, format!("{} is unchanged", path.display()));
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
        match self.session.append_system_context(context, key).await {
            Ok(_) => self.notice(
                NoticeLevel::Info,
                format!(
                    "saved {}: this session reads the new content from its next model call; new sessions load it as usual",
                    path.display()
                ),
            ),
            Err(err) => self.notice(
                NoticeLevel::Warn,
                format!("{next} (this session could not take it now: {err})"),
            ),
        }
    }

    /// [`crate::ui::Effect::Remember`].
    pub(super) async fn remember(&mut self, text: &str) {
        let Some(line) = remember_line(text) else {
            self.notice(NoticeLevel::Warn, "nothing to remember");
            return;
        };
        let path = remember_target(&self.runtime.scope.context_root);
        if let Err(err) = append_remember_line(&path, &line) {
            self.notice(
                NoticeLevel::Error,
                format!("could not write {}: {err}", path.display()),
            );
            return;
        }
        let key = remember_key(&line);
        match self
            .session
            .append_system_context(remembered_context(&line), key)
            .await
        {
            Ok(_) => self.notice(
                NoticeLevel::Info,
                format!(
                    "remembered in {}: this session reads it from its next model call; it is fully applied (as part of AGENTS.md) from the next session",
                    path.display()
                ),
            ),
            Err(err) => self.notice(
                NoticeLevel::Warn,
                format!(
                    "remembered in {}, but this session could not take it now ({err}); it applies from the next session",
                    path.display()
                ),
            ),
        }
    }
}

/// An instruction file `/context` names: a label, its path and its size on
/// disk (`None`: absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InstructionFile {
    pub(super) label: String,
    pub(super) path: PathBuf,
    pub(super) bytes: Option<usize>,
    pub(super) truncated: bool,
}

/// `~N tokens` for `bytes` of text (four bytes a token).
fn approx_tokens(bytes: usize) -> String {
    let tokens = u64::try_from(bytes / 4).unwrap_or(u64::MAX);
    format!("~{} tokens", crate::ui::render::fmt_ctx_tokens(tokens))
}

/// `/context`: the UI's `header`, then the session's system prompt and
/// message counts (`messages`, or why they could not be read), the
/// instruction files on disk, and the tools rkat-tui registers itself.
pub(super) fn context_breakdown(
    header: &str,
    messages: Result<&[Message], String>,
    files: &[InstructionFile],
    tui_tools: usize,
) -> String {
    let mut out = header.to_string();
    match messages {
        Ok(messages) => {
            let (mut system, mut system_bytes) = (0usize, 0usize);
            let (mut user, mut assistant, mut tools, mut notices) = (0usize, 0, 0, 0);
            for message in messages {
                match message {
                    Message::System(s) => {
                        system += 1;
                        system_bytes += s.content.len();
                    }
                    Message::SystemNotice(_) => notices += 1,
                    Message::User(_) => user += 1,
                    Message::BlockAssistant(_) => assistant += 1,
                    Message::ToolResults { .. } => tools += 1,
                }
            }
            let _ = write!(
                out,
                "\nsystem prompt: {system_bytes} bytes ({}) in {system} system message{}",
                approx_tokens(system_bytes),
                if system == 1 { "" } else { "s" }
            );
            let _ = write!(
                out,
                "\nmessages: {} (user {user}, assistant {assistant}, tool results {tools}, notices {notices})",
                user + assistant + tools + notices
            );
        }
        Err(err) => {
            let _ = write!(out, "\nsystem prompt and messages: unknown ({err})");
        }
    }
    out.push_str(
        "\ninstruction files on disk (new sessions load them; a resumed session keeps the copy it was built with):",
    );
    for file in files {
        let size = match file.bytes {
            Some(bytes) => format!(
                "{bytes} bytes ({}){}",
                approx_tokens(bytes),
                if file.truncated { ", cut to the cap" } else { "" }
            ),
            None => "absent".to_string(),
        };
        let _ = write!(out, "\n  {}: {} — {size}", file.label, file.path.display());
    }
    let _ = write!(
        out,
        "\ntools: {tui_tools} registered by rkat-tui (file, notes, fork); the engine's built-in, MCP and helper tools: unknown here"
    );
    out
}

/// The file `/memory` opens: the workspace `AGENTS.md` (the one
/// `/remember` writes), or the user-global `AGENTS.md` or `SYSTEM.md`.
///
/// # Errors
///
/// A notice when there is no user config directory for the global files.
pub(crate) fn memory_path(
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
pub(crate) fn prepare_memory_file(target: MemoryTarget, path: &Path) -> std::io::Result<()> {
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
pub(crate) fn memory_key(context: &str) -> String {
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
pub(crate) fn remember_key(line: &str) -> String {
    let digest = Sha256::digest(line.as_bytes());
    let mut key = String::with_capacity(18 + 64);
    key.push_str("rkat-tui:remember:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

#[cfg(test)]
mod tests {
    use super::{
        InstructionFile, MemoryTarget, context_breakdown, memory_key, memory_path,
        prepare_memory_file, remember_key,
    };
    use crate::bootstrap::dirs::GlobalDirs;
    use crate::bootstrap::instructions::{SYSTEM_MD_HEADER, system_md_is_empty};
    use meerkat_core::{Message, SystemMessage, UserMessage};
    use std::path::PathBuf;

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
    fn the_context_breakdown_counts_the_prompt_messages_files_and_tools() {
        let messages = vec![
            Message::System(SystemMessage::new("x".repeat(4000))),
            Message::User(UserMessage::text("hi")),
        ];
        let files = [
            InstructionFile {
                label: "workspace AGENTS.md".into(),
                path: PathBuf::from("/w/AGENTS.md"),
                bytes: Some(800),
                truncated: false,
            },
            InstructionFile {
                label: "AGENTS.local.md".into(),
                path: PathBuf::from("/w/AGENTS.local.md"),
                bytes: None,
                truncated: false,
            },
        ];
        let text = context_breakdown("context: head", Ok(&messages), &files, 9);
        assert!(text.starts_with("context: head\n"), "{text}");
        assert!(
            text.contains("system prompt: 4000 bytes (~1k tokens) in 1 system message"),
            "{text}"
        );
        assert!(
            text.contains("messages: 1 (user 1, assistant 0, tool results 0, notices 0)"),
            "{text}"
        );
        assert!(text.contains("workspace AGENTS.md: /w/AGENTS.md — 800 bytes (~200 tokens)"), "{text}");
        assert!(text.contains("AGENTS.local.md: /w/AGENTS.local.md — absent"), "{text}");
        assert!(text.contains("tools: 9 registered by rkat-tui"), "{text}");
        assert!(text.contains("unknown here"), "{text}");
        let text = context_breakdown("h", Err("gone".into()), &[], 0);
        assert!(text.contains("system prompt and messages: unknown (gone)"), "{text}");
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
