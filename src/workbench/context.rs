//! `/context`'s breakdown: the UI's header, the session's system prompt
//! and message counts, the instruction files on disk and what is known of
//! the tools. The client reads the messages from the daemon
//! (`rkat/transcript`); the instruction files are found the same way the
//! session builds find them ([`InstructionSources::of`]).

use std::fmt::Write as _;
use std::path::PathBuf;

use meerkat_core::Message;

use crate::bootstrap::RuntimeScope;
use crate::bootstrap::instructions::{InstructionSources, SectionKind, remember_target};

/// The instruction files `/context` names for `scope`: the user's
/// `SYSTEM.md`, the workspace `AGENTS.md` and every discovered section.
#[must_use]
pub fn instruction_files(
    scope: &RuntimeScope,
    sources: &InstructionSources,
) -> Vec<InstructionFile> {
    let size = |path: &std::path::Path| {
        std::fs::metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|m| usize::try_from(m.len()).unwrap_or(usize::MAX))
    };
    let mut files = Vec::new();
    let path = scope.global.system_md();
    files.push(InstructionFile {
        label: "SYSTEM.md (replaces the base prompt)".into(),
        bytes: size(&path),
        path,
        truncated: false,
    });
    let agents = remember_target(&scope.context_root);
    files.push(InstructionFile {
        label: "workspace AGENTS.md".into(),
        bytes: size(&agents),
        path: agents,
        truncated: false,
    });
    for section in sources.discover() {
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
    files
}

/// An instruction file `/context` names: a label, its path and its size on
/// disk (`None`: absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub label: String,
    pub path: PathBuf,
    pub bytes: Option<usize>,
    pub truncated: bool,
}

/// `~N tokens` for `bytes` of text (four bytes a token).
fn approx_tokens(bytes: usize) -> String {
    let tokens = u64::try_from(bytes / 4).unwrap_or(u64::MAX);
    format!("~{} tokens", crate::ui::render::fmt_ctx_tokens(tokens))
}

/// `/context`: the UI's `header`, then the session's system prompt and
/// message counts (`messages`, or why they could not be read), the
/// instruction files on disk, and `tools`: what is known of the tools
/// (rkat-tui's own; the engine's are not counted here).
pub fn context_breakdown(
    header: &str,
    messages: Result<&[Message], String>,
    files: &[InstructionFile],
    tools: &str,
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
        "\ntools: {tools}; the engine's built-in, MCP and helper tools: unknown here"
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
        let text = context_breakdown(
            "context: head",
            Ok(&messages),
            &files,
            "9 registered by rkat-tui (file, notes, fork)",
        );
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
        let text = context_breakdown("h", Err("gone".into()), &[], "none");
        assert!(
            text.contains("system prompt and messages: unknown (gone)"),
            "{text}"
        );
    }
}
