//! A detached job's outcome as a compact block (display only).
//!
//! A `fork_off` child, a `council` or a background helper reports when it
//! ends through the engine's `BackgroundJob` notice: a header line
//! (`Background fork_off job <id> finished (completed):`) and one JSON
//! object, the job's completion (`meerkat-mob-mcp` `agent_tools.rs`,
//! `ForkOffCompletion`: `agent_identity`, `member_ref`, `status`,
//! `bounded_result {label, status, text}`, `usage`, `turns`, `tool_calls`,
//! `error`, …). Shown verbatim it fills screens with escaped Markdown, so
//! the transcript shows [`Block::Outcome`] instead: a one-line header with
//! the counts, the result's text as Markdown folded to a few lines, and
//! no `member_ref` or job id. A failed or cancelled outcome shows its
//! status and error unfolded. A detail of another shape keeps the raw
//! display. What the model receives is the engine's and unchanged.

use serde_json::Value;

use super::Block;

/// What a job's outcome says, as far as the transcript shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    /// The job's kind as the engine names it (`fork_off`, `council`, …).
    pub kind: String,
    /// The engine's job id (kept to recognise the same outcome twice).
    pub job: Option<String>,
    /// The member that did the work.
    pub agent: Option<String>,
    /// Its terminal status (`completed`, `failed`, `cancelled`, …).
    pub status: String,
    /// Turns and tool calls it took, when reported.
    pub turns: Option<u64>,
    /// Tool calls it made, when reported.
    pub tool_calls: Option<u64>,
    /// Input and output tokens, when reported.
    pub tokens: Option<u64>,
    /// The result's text (Markdown), when there is one.
    pub text: Option<String>,
    /// Why it did not complete, when it says.
    pub error: Option<String>,
}

impl JobOutcome {
    /// The outcome of job `job` of `kind` whose completion `detail` is the
    /// JSON object above; `None` for any other shape.
    #[must_use]
    pub fn from_detail(kind: &str, job: Option<&str>, detail: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(detail.trim()).ok()?;
        let obj = value.as_object()?;
        let status = obj.get("status")?.as_str()?.to_string();
        let bounded = obj.get("bounded_result").and_then(Value::as_object);
        let agent = obj
            .get("agent_identity")
            .and_then(Value::as_str)
            .map(str::to_string);
        if agent.is_none() && bounded.is_none() {
            return None;
        }
        let count = |key: &str| obj.get(key).and_then(Value::as_u64);
        let tokens = obj
            .get("usage")
            .and_then(Value::as_object)
            .and_then(|usage| {
                let input = usage.get("input_tokens").and_then(Value::as_u64)?;
                let output = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Some(input + output)
            });
        Some(Self {
            kind: kind.to_string(),
            job: job.map(str::to_string),
            agent,
            status,
            turns: count("turns"),
            tool_calls: count("tool_calls"),
            tokens,
            text: bounded
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|text| !text.trim().is_empty()),
            error: obj.get("error").and_then(Value::as_str).map(str::to_string),
        })
    }

    /// The outcome in the notice's text form (`Background <kind> job <id>
    /// finished (<status>):` and the detail on the lines after it).
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        let rest = text.trim_start().strip_prefix("Background ")?;
        let (kind, rest) = rest.split_once(" job ")?;
        let (job, rest) = rest.split_once(" finished (")?;
        let (_, detail) = rest.split_once("):")?;
        Self::from_detail(kind, Some(job), detail)
    }

    /// Whether it ended without its result.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.status != "completed"
    }

    /// `fork kid finished (completed) · 11 turns · 39 tool calls · 12.3k tokens`.
    #[must_use]
    pub fn header(&self) -> String {
        let kind = match self.kind.as_str() {
            "fork_off" => "fork",
            other => other,
        };
        let mut header = match &self.agent {
            Some(agent) => format!("{kind} {agent} finished ({})", self.status),
            None => format!("{kind} finished ({})", self.status),
        };
        if let Some(turns) = self.turns {
            header.push_str(&format!(" · {}", crate::text::count(turns, "turn")));
        }
        if let Some(calls) = self.tool_calls {
            header.push_str(&format!(" · {}", crate::text::count(calls, "tool call")));
        }
        if let Some(tokens) = self.tokens {
            header.push_str(&format!(
                " · {} tokens",
                crate::ui::render::fmt_tokens(tokens)
            ));
        }
        header
    }

    /// The transcript block: folded when it completed, unfolded (with its
    /// error) when it did not.
    #[must_use]
    pub fn block(&self) -> Block {
        let mut md = self.text.clone().unwrap_or_default();
        if let Some(error) = &self.error {
            if !md.is_empty() {
                md.push_str("\n\n");
            }
            md.push_str(&format!("**error:** {error}"));
        }
        Block::Outcome {
            header: self.header(),
            md,
            failed: self.failed(),
            collapsed: !self.failed(),
            job: self.job.clone(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn completed() -> String {
        serde_json::json!({
            "agent_identity": "kid",
            "bounded_result": {"label": "fork_off_result", "status": "completed", "text": "# Report\n\n- one\n- two"},
            "member_ref": "dGVzdA==",
            "status": "completed",
            "tool_calls": 39,
            "turns": 11,
            "usage": {"cache_creation_tokens": 5, "cache_read_tokens": 7, "input_tokens": 12000, "output_tokens": 300, "reasoning_tokens": 0}
        })
        .to_string()
    }

    #[test]
    fn a_completed_fork_is_one_header_and_its_report() {
        let text = format!(
            "Background fork_off job 0b1c finished (completed):\n{}",
            completed()
        );
        let outcome = JobOutcome::from_text(&text).expect("the shape");
        assert_eq!(
            outcome.header(),
            "fork kid finished (completed) · 11 turns · 39 tool calls · 12.3k tokens"
        );
        let Block::Outcome {
            header,
            md,
            failed,
            collapsed,
            job,
        } = outcome.block()
        else {
            panic!("not an outcome block");
        };
        assert!(!header.contains("0b1c") && !md.contains("dGVzdA=="));
        assert_eq!(md, "# Report\n\n- one\n- two");
        assert!(!failed && collapsed);
        assert_eq!(job.as_deref(), Some("0b1c"));
    }

    #[test]
    fn a_failed_fork_shows_its_error_unfolded() {
        let detail = serde_json::json!({
            "agent_identity": "kid",
            "member_ref": "x",
            "status": "failed",
            "error": "the child's run failed: boom"
        })
        .to_string();
        let outcome = JobOutcome::from_detail("fork_off", Some("j"), &detail).unwrap();
        let Block::Outcome {
            md,
            failed,
            collapsed,
            header,
            ..
        } = outcome.block()
        else {
            panic!("not an outcome block");
        };
        assert_eq!(header, "fork kid finished (failed)");
        assert!(failed && !collapsed);
        assert!(md.contains("boom"), "{md}");
    }

    /// The job's event becomes one compact block, shown once although the
    /// run it starts carries it again; it renders folded to a few lines
    /// with the way to expand, and a click unfolds it; another shape keeps
    /// the raw notice.
    #[test]
    fn the_event_and_the_run_show_one_folded_block() {
        use crate::ui::render::{OUTCOME_PREVIEW_LINES, block_lines};
        let report = (1..=10)
            .map(|n| format!("- item {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = serde_json::json!({
            "agent_identity": "kid",
            "bounded_result": {"label": "fork_off_result", "status": "completed", "text": report},
            "member_ref": "SECRET-REF",
            "status": "completed",
            "turns": 2,
            "tool_calls": 3
        })
        .to_string();
        let mut t = super::super::Transcript::new();
        t.apply_agent_event(&meerkat_core::AgentEvent::BackgroundJobCompleted {
            job_id: "job-1".into(),
            display_name: "fork_off".into(),
            terminal_status: meerkat_core::event::BackgroundJobTerminalStatus::Completed,
            detail: detail.clone(),
        });
        let text = format!("Background fork_off job job-1 finished (completed):\n{detail}");
        t.push_outcome(&JobOutcome::from_text(&text).unwrap());
        let blocks = t.blocks();
        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        let rows: Vec<String> = block_lines(&blocks[0], 60)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            rows[0],
            "⚙ fork kid finished (completed) · 2 turns · 3 tool calls"
        );
        assert_eq!(rows.len(), 1 + OUTCOME_PREVIEW_LINES + 2, "{rows:#?}");
        assert!(rows[OUTCOME_PREVIEW_LINES + 1].contains("more lines (click or Ctrl-O to expand)"));
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("SECRET-REF") || r.contains("job-1"))
        );
        let id = t.last_tool_with_output().expect("toggleable");
        assert_eq!(t.toggle_tool(id), Some(false));
        let open: Vec<String> = block_lines(&t.blocks()[0], 60)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(open.iter().any(|r| r.contains("item 10")), "{open:#?}");
        // Another shape: the notice as before.
        let mut t = super::super::Transcript::new();
        t.apply_agent_event(&meerkat_core::AgentEvent::BackgroundJobCompleted {
            job_id: "job-2".into(),
            display_name: "fork_off".into(),
            terminal_status: meerkat_core::event::BackgroundJobTerminalStatus::Completed,
            detail: "plain words".into(),
        });
        assert!(
            matches!(&t.blocks()[0], Block::Notice { text, .. } if text.contains("plain words"))
        );
    }

    #[test]
    fn another_shape_is_not_an_outcome() {
        for detail in [
            "not json",
            "[1, 2]",
            r#"{"status": "completed"}"#,
            r#"{"agent_identity": "kid"}"#,
        ] {
            assert!(
                JobOutcome::from_detail("fork_off", None, detail).is_none(),
                "{detail}"
            );
        }
        assert!(JobOutcome::from_text("Background job without the rest").is_none());
    }
}
