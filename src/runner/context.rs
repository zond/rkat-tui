//! Context upkeep: the compaction trigger and `/context` (`/remember` and
//! `/memory` are the workbench's, [`crate::workbench::memory`]).
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
//! ([`Runner::ensure_trigger`]) and a model switch that needed a new
//! profile. A plain model switch does not rebuild: the engine's `delegate`
//! reads the session's current model on every call (#2102). A background
//! list fetch never rebuilds. The upstream ask (a live trigger
//! update) is recorded in `docs/evidence.md`.

use super::Runner;
use std::fmt::Write as _;
use std::path::PathBuf;

use meerkat_core::Message;

use crate::bootstrap::instructions::{SectionKind, remember_target};
use crate::error::TuiError;
use crate::session::SessionHandle;
use crate::ui::{CompactFailure, CompactionTrigger, Msg, NoticeLevel};

/// Why the session is rebuilt (see [`Runner::rebuild_session_workaround`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebuildReason {
    /// The compaction trigger moved (model switch, resume onto another
    /// model). Upstream ask: a live trigger update.
    CompactionTrigger,
    /// The agent's model registry predates a profile the switch needs.
    /// Upstream ask: profiles read at reconfigure time.
    ModelProfile,
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
        tracing::debug!(
            ?reason,
            ?trigger,
            "rebuilding the session (upstream workaround)"
        );
        let id = self.session.id().clone();
        let previous = self.session.compaction();
        self.session.retire().await.map_err(RebuildError::Retire)?;
        let result =
            match SessionHandle::resume(&self.runtime, &id, self.template.options(None, trigger))
                .await
            {
                Ok(session) => {
                    self.session = session;
                    Ok(())
                }
                Err(first) => {
                    tracing::warn!(error = %first, "session rebuild failed; resuming as it was");
                    match SessionHandle::resume(
                        &self.runtime,
                        &id,
                        self.template.options(None, previous),
                    )
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
                if file.truncated {
                    ", cut to the cap"
                } else {
                    ""
                }
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

#[cfg(test)]
mod tests {
    use super::{InstructionFile, context_breakdown};
    use meerkat_core::{Message, SystemMessage, UserMessage};
    use std::path::PathBuf;

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
        assert!(
            text.contains("workspace AGENTS.md: /w/AGENTS.md — 800 bytes (~200 tokens)"),
            "{text}"
        );
        assert!(
            text.contains("AGENTS.local.md: /w/AGENTS.local.md — absent"),
            "{text}"
        );
        assert!(text.contains("tools: 9 registered by rkat-tui"), "{text}");
        assert!(text.contains("unknown here"), "{text}");
        let text = context_breakdown("h", Err("gone".into()), &[], 0);
        assert!(
            text.contains("system prompt and messages: unknown (gone)"),
            "{text}"
        );
    }
}
