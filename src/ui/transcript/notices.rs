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
        AgentEvent::ToolConfigChanged { payload } => (
            NoticeLevel::Info,
            format!("tools: {:?} {}", payload.operation, payload.target),
        ),
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
