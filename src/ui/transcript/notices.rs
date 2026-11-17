//! The transcript's notices for engine events that have no block of their
//! own: retries, compaction skips and the rest of the upstream events.

use meerkat_core::event::BackgroundJobTerminalStatus;
use meerkat_core::{AgentEvent, LlmRetrySchedule};

use super::NoticeLevel;

/// The retry notice: attempt, diagnostic and back-off delay.
pub(super) fn retry_notice(retry: &LlmRetrySchedule) -> String {
    format!(
        "⟳ retry {}/{}: {} ({}ms)",
        retry.plan.attempt,
        retry.plan.max_retries,
        retry.failure.message,
        retry.plan.selected_delay_ms
    )
}

/// A change of the agent's tools in plain words, or `None` when it adds
/// nothing to what the transcript shows: the tool list taking in a
/// `tool_search`'s reveal (its call's line says what was revealed), and the
/// engine's bookkeeping of hidden tools, draw nothing; a server's tools
/// coming, going or failing, and tools closed by a failure, are said.
fn tool_change_notice(
    payload: &meerkat_core::event::ToolConfigChangedPayload,
) -> Option<(NoticeLevel, String)> {
    use meerkat_core::event::{
        ExternalToolDeltaPhase, ToolConfigChangeOperation, ToolConfigChangeStatus,
    };
    let target = &payload.target;
    match payload.status_info() {
        ToolConfigChangeStatus::BoundaryApplied { .. }
        | ToolConfigChangeStatus::DeferredCatalogDelta { .. } => None,
        ToolConfigChangeStatus::WarningFailedClosed { error } => Some((
            NoticeLevel::Warn,
            format!("some tools are unavailable: {error}"),
        )),
        ToolConfigChangeStatus::ExternalToolDelta { phase, detail, .. } => match phase {
            ExternalToolDeltaPhase::Pending | ExternalToolDeltaPhase::Draining => None,
            ExternalToolDeltaPhase::Failed => Some((
                NoticeLevel::Warn,
                match detail {
                    Some(detail) => format!("the tools of `{target}` failed: {detail}"),
                    None => format!("the tools of `{target}` failed"),
                },
            )),
            ExternalToolDeltaPhase::Applied | ExternalToolDeltaPhase::Forced => {
                let what = match payload.operation {
                    ToolConfigChangeOperation::Add => "are available",
                    ToolConfigChangeOperation::Remove => "were removed",
                    ToolConfigChangeOperation::Reload => "were reloaded",
                };
                Some((NoticeLevel::Info, format!("the tools of `{target}` {what}")))
            }
        },
    }
}

/// Notices for the upstream events the transcript has no dedicated block for.
///
/// Model fallback, background jobs, tool-config changes, peer content, run
/// resumption and request-bounded tool results become notices. A steered
/// message's `boundary_append_applied` yields `None`: the runner maps it to
/// [`crate::ui::Msg::SteerDelivered`] and its placeholder becomes the user block.
/// Events with no user-facing meaning (run/hook/interaction lifecycle,
/// accounting disputes, transcript rewrites, cache breakpoints, reasoning
/// lowering) yield `None`.
pub(super) fn upstream_notice(event: &AgentEvent) -> Option<(NoticeLevel, String)> {
    let notice = match event {
        AgentEvent::BoundaryAppendsDiscarded(discarded) => (
            NoticeLevel::Info,
            format!(
                "↺ {} message(s) taken back out of the stopped step; the runtime delivers them again",
                discarded.input_ids.len()
            ),
        ),
        AgentEvent::ModelFallbackCommitted {
            previous, target, ..
        } => (
            NoticeLevel::Warn,
            format!("⇄ model fallback: {} → {}", previous.model, target.model),
        ),
        AgentEvent::ModelFallbackTargetFailed { target, error, .. } => (
            NoticeLevel::Warn,
            format!(
                "✗ fallback model {} failed: {}",
                target.model, error.message
            ),
        ),
        AgentEvent::BackgroundJobCompleted {
            display_name,
            terminal_status,
            detail,
            ..
        } => {
            let level = if *terminal_status == BackgroundJobTerminalStatus::Completed {
                NoticeLevel::Info
            } else {
                NoticeLevel::Warn
            };
            let mut text = format!("⚙ {display_name}: {}", terminal_status.as_str());
            if !detail.is_empty() {
                text.push_str(" — ");
                text.push_str(detail);
            }
            (level, text)
        }
        AgentEvent::ToolConfigChanged { payload } => return tool_change_notice(payload),
        AgentEvent::PeerContentIngested { kind, .. } => (
            NoticeLevel::Info,
            format!("✉ peer content received ({kind:?})"),
        ),
        AgentEvent::RunResumed { round, .. } => {
            (NoticeLevel::Info, format!("⟳ run resumed (round {round})"))
        }
        AgentEvent::ToolResultsBoundedForRequest { results, .. } => {
            let names = results
                .iter()
                .map(|r| format!("{} {}/{} bytes", r.tool_name, r.kept_bytes, r.total_bytes))
                .collect::<Vec<_>>()
                .join(", ");
            (
                NoticeLevel::Info,
                format!("tool results trimmed for the request: {names}"),
            )
        }
        AgentEvent::TurnUsageAccountingUnmeasured { .. } => (
            NoticeLevel::Info,
            "the provider reported no usage for a turn; the cost readout misses it".to_string(),
        ),
        AgentEvent::ServerToolContent { kind, .. } => {
            (NoticeLevel::Info, format!("◆ server tool: {kind}"))
        }
        AgentEvent::AssistantImageAppended { .. } => (
            NoticeLevel::Info,
            "◆ image added to the transcript".to_string(),
        ),
        _ => return None,
    };
    Some(notice)
}

/// Why a requested compaction (`/compact now`) compacted nothing.
pub(super) fn compaction_skipped(reason: meerkat_core::RequestedCompactionSkipReason) -> String {
    use meerkat_core::RequestedCompactionSkipReason as Why;
    let why = match reason {
        Why::FirstModelRequest => "the conversation has no model call to compact yet",
        Why::NothingToFold => "nothing older than the turn in flight to fold",
        _ => "the engine found nothing to fold",
    };
    format!("/compact: nothing was compacted ({why})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use meerkat_core::event::{
        ExternalToolDeltaPhase, ToolConfigChangeOperation as Op, ToolConfigChangeStatus as Status,
        ToolConfigChangedPayload as Payload,
    };

    fn notice(payload: Payload) -> Option<(NoticeLevel, String)> {
        upstream_notice(&AgentEvent::ToolConfigChanged { payload })
    }

    /// The tool list taking in a search's reveal draws nothing (the
    /// search's own line says what it revealed), and no notice speaks the
    /// engine's words (`Reload tool_scope`).
    #[test]
    fn a_tool_change_is_said_plainly_or_not_at_all() {
        assert_eq!(
            notice(Payload::new(
                Op::Reload,
                "tool_scope",
                Status::boundary_applied(true, true, 3)
            )),
            None
        );
        assert_eq!(
            notice(Payload::new(
                Op::Reload,
                "tool_scope",
                Status::deferred_catalog_delta(1, 0, 0)
            )),
            None
        );
        assert_eq!(
            notice(Payload::new(
                Op::Add,
                "docs",
                Status::external_tool_delta(ExternalToolDeltaPhase::Pending, None)
            )),
            None
        );
        assert_eq!(
            notice(Payload::new(
                Op::Add,
                "docs",
                Status::external_tool_delta(ExternalToolDeltaPhase::Applied, None)
            )),
            Some((
                NoticeLevel::Info,
                "the tools of `docs` are available".to_string()
            ))
        );
        assert_eq!(
            notice(Payload::new(
                Op::Add,
                "docs",
                Status::external_tool_delta(
                    ExternalToolDeltaPhase::Failed,
                    Some("refused".to_string())
                )
            )),
            Some((
                NoticeLevel::Warn,
                "the tools of `docs` failed: refused".to_string()
            ))
        );
        assert_eq!(
            notice(Payload::new(
                Op::Reload,
                "tool_scope",
                Status::warning_failed_closed("no grants")
            )),
            Some((
                NoticeLevel::Warn,
                "some tools are unavailable: no grants".to_string()
            ))
        );
    }
}
