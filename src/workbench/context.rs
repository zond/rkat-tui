//! `/context`'s breakdown: the UI's header, the session's system prompt
//! and message counts and its tool definitions by family, as the daemon
//! reports them (`rkat/context`), and the instruction files on disk,
//! found the same way the session builds find them
//! ([`InstructionSources::of`]).

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::daemon::protocol::ContextReport;

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
/// message counts and its tool definitions by family (`report`, or why
/// the daemon could not say), and the instruction files on disk.
pub fn context_breakdown(
    header: &str,
    report: Result<&ContextReport, String>,
    files: &[InstructionFile],
) -> String {
    let mut out = header.to_string();
    match &report {
        Ok(report) => {
            let m = &report.messages;
            let _ = write!(
                out,
                "\nsystem prompt: {} bytes ({}) in {} system message{}",
                m.system_bytes,
                approx_tokens(m.system_bytes),
                m.system,
                if m.system == 1 { "" } else { "s" }
            );
            let _ = write!(
                out,
                "\nmessages: {} (user {}, assistant {}, tool results {}, notices {})",
                m.total(),
                m.user,
                m.assistant,
                m.tool_results,
                m.notices
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
    match report.map(|report| report.tools.as_deref()) {
        Ok(Some(families)) => {
            let bytes: usize = families.iter().map(|family| family.bytes).sum();
            let tools: usize = families.iter().map(|family| family.tools).sum();
            let _ = write!(
                out,
                "\ntool definitions (every model request carries them): {bytes} bytes ({}) in {tools} tools",
                approx_tokens(bytes)
            );
            for family in families {
                let _ = write!(
                    out,
                    "\n  {}: {} tool{}, {} bytes ({})",
                    family.family.name(),
                    family.tools,
                    if family.tools == 1 { "" } else { "s" },
                    family.bytes,
                    approx_tokens(family.bytes)
                );
            }
        }
        Ok(None) => out.push_str(
            "\ntool definitions: not read yet (the daemon reads them while the agent is idle)",
        ),
        Err(_) => out.push_str("\ntool definitions: unknown"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{InstructionFile, context_breakdown};
    use crate::daemon::context::{FamilySize, MessageCounts, ToolFamily};
    use crate::daemon::protocol::ContextReport;
    use meerkat_core::{Message, SessionId, SystemMessage, UserMessage};
    use std::path::PathBuf;

    #[test]
    fn the_context_breakdown_counts_the_prompt_messages_files_and_tools() {
        let messages = vec![
            Message::System(SystemMessage::new("x".repeat(4000))),
            Message::User(UserMessage::text("hi")),
        ];
        let report = ContextReport {
            session_id: SessionId::new(),
            messages: MessageCounts::of(&messages),
            tools: Some(vec![
                FamilySize {
                    family: ToolFamily::Schedule,
                    tools: 8,
                    bytes: 36_000,
                },
                FamilySize {
                    family: ToolFamily::Files,
                    tools: 1,
                    bytes: 400,
                },
            ]),
        };
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
        let text = context_breakdown("context: head", Ok(&report), &files);
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
        assert!(
            text.contains("tool definitions (every model request carries them): 36400 bytes (~9.1k tokens) in 9 tools"),
            "{text}"
        );
        assert!(
            text.contains("schedule: 8 tools, 36000 bytes (~9k tokens)"),
            "{text}"
        );
        assert!(
            text.contains("files: 1 tool, 400 bytes (~100 tokens)"),
            "{text}"
        );
        let unread = ContextReport {
            tools: None,
            ..report
        };
        let text = context_breakdown("h", Ok(&unread), &[]);
        assert!(text.contains("tool definitions: not read yet"), "{text}");
        let text = context_breakdown("h", Err("gone".into()), &[]);
        assert!(
            text.contains("system prompt and messages: unknown (gone)"),
            "{text}"
        );
        assert!(text.contains("tool definitions: unknown"), "{text}");
    }
}
