//! The compaction policy on Meerkat's native auto-compaction, end to end
//! through the `rkat-tui-mob` daemon with a scripted LLM, driven by an
//! attached client.
//!
//! Six short turns build history; the sixth reports 60k input tokens. With
//! `compact_auto_percent = 5` of the model's window (1M tokens) the threshold is
//! below that:
//! - `auto`: the next turn's first model call compacts first (a summary
//!   call; the model then sees the summary instead of the old turns), and
//!   `CompactionCompleted` (published when the session next runs, once the
//!   runtime finalized the memory projection) reports a smaller history,
//!   the number the gauge shows;
//! - `warn`: the native trigger is off, nothing compacts, and the gauge
//!   warns; `/compact` asks the engine to compact at the next model call
//!   (`request_compaction`, no rebuild), the next turn compacts, and the
//!   one after it does not: the request was consumed and the policy never
//!   changed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::AgentEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use rig::{Attached, Setup};
use rkat_tui::ui::compaction::{CompactionGauge, CompactionPolicy};
use rkat_tui::ui::{CompactMode, Effect, Msg, StatusUpdate};

/// Input tokens the sixth turn reports.
const BIG: u64 = 60_000;

const SHORT_TURNS: usize = 5;

/// Five short turns, then one reporting [`BIG`] input tokens.
fn history() -> Vec<Step> {
    let mut steps = vec![Step::Text("ok"); SHORT_TURNS];
    steps.push(Step::TextUsage("big", BIG));
    steps
}

/// The daemon on `ws` in compaction `mode` (both thresholds at 5%), a
/// client attached, and the policy both run with.
async fn rig(
    llm: &std::sync::Arc<ScriptedLlmClient>,
    ws: &Workspace,
    mode: &str,
) -> (Attached, CompactionPolicy) {
    let setup = Setup::args(&[
        "--set",
        &format!("auto.compact={mode}"),
        "--set",
        "auto.compact_warn_percent=5",
        "--set",
        "auto.compact_auto_percent=5",
    ]);
    let layers = rkat_tui::bootstrap::read_layers(&setup.command_line(&ws.root)).unwrap();
    let policy = layers.layered.settings.compaction_policy(None);
    (Attached::start_with(&ws.root, llm, setup).await, policy)
}

async fn turns(rig: &mut Attached, n: usize, prefix: &str) {
    for i in 0..n {
        rig.send(Effect::StartTurn(format!("{prefix} {i}")));
        rig.turn_finished().await.expect("turn succeeds");
    }
}

/// Wait (bounded by [`within`]) until the session published a completed
/// compaction.
async fn completed(rig: &mut Attached) {
    within(rig.saw(|msg| {
        matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::CompactionCompleted { .. }))
    }))
    .await;
}

/// The compaction's folded line is given its summary, read from the
/// conversation's record, and a restore of the conversation shows the same
/// line with it, and nothing of it as the user's band.
async fn summary_is_folded(rig: &mut Attached) {
    // What the UI asks at the completion (`ui::tests::turns`).
    rig.send(Effect::ReadCompactionSummary);
    let read = within(rig.saw(|msg| matches!(msg, Msg::CompactionSummary(_)))).await;
    assert!(
        matches!(&read, Msg::CompactionSummary(Ok(text)) if text == "SUMMARY of the earlier turns"),
        "{read:?}"
    );
    let restored = rkat_tui::ui::transcript::blocks_from_messages(&rig.messages().await);
    assert!(
        restored.iter().any(|b| matches!(b, rkat_tui::ui::Block::Compaction { summary: Some(Ok(text)), collapsed: true, .. } if text == "SUMMARY of the earlier turns")),
        "{restored:#?}"
    );
    assert!(
        !restored.iter().any(|b| matches!(b, rkat_tui::ui::Block::User { text } | rkat_tui::ui::Block::Assistant { md: text } if text.contains("SUMMARY"))),
        "{restored:#?}"
    );
}

fn started(rig: &Attached) -> usize {
    compactions(rig)
        .iter()
        .filter(|e| matches!(e, AgentEvent::CompactionStarted { .. }))
        .count()
}

fn compactions(rig: &Attached) -> Vec<AgentEvent> {
    rig.events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::CompactionStarted { .. }
                    | AgentEvent::CompactionCompleted { .. }
                    | AgentEvent::CompactionFailed { .. }
            )
        })
        .cloned()
        .collect()
}

/// The context window the client reported for the session's model.
fn window(rig: &Attached) -> u64 {
    rig.seen
        .iter()
        .rev()
        .find_map(|msg| match msg {
            Msg::Status(StatusUpdate::ContextWindow(Some(w))) => Some(*w),
            _ => None,
        })
        .expect("a context window")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_mode_compacts_natively_at_the_threshold() {
    let ws = Workspace::new();
    let mut script = history();
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::Text("after"));
    script.push(Step::Text("later"));
    let llm = ScriptedLlmClient::new(script);
    let (mut rig, policy) = rig(&llm, &ws, "auto").await;
    assert_eq!(policy.mode, CompactMode::Auto);
    turns(&mut rig, SHORT_TURNS + 1, "turn").await;
    assert_eq!(started(&rig), 0, "nothing crossed yet");
    let window = window(&rig);
    assert!(policy.auto_tokens(Some(window)) < BIG, "window {window}");

    rig.send(Effect::StartTurn("next".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "after");
    assert_eq!(started(&rig), 1);
    let requests = llm.recorded();
    assert_eq!(
        requests.len(),
        SHORT_TURNS + 3,
        "one extra call: the summary"
    );
    let after = requests.last().unwrap();
    assert!(after.messages.len() < requests[SHORT_TURNS].messages.len());
    assert!(
        after.user().contains("SUMMARY of the earlier turns"),
        "{}",
        after.user()
    );
    assert!(!after.user().contains("turn 0"), "{}", after.user());

    // The completion is published when the session runs again.
    rig.send(Effect::StartTurn("again".into()));
    rig.turn_finished().await.expect("turn succeeds");
    completed(&mut rig).await;
    let left = compactions(&rig)
        .iter()
        .find_map(|e| match e {
            AgentEvent::CompactionCompleted {
                estimated_history_tokens_after,
                messages_before,
                messages_after,
                ..
            } => {
                assert!(messages_after < messages_before);
                *estimated_history_tokens_after
            }
            _ => None,
        })
        .expect("a measured history");
    assert!(left < BIG, "{left}");
    assert_eq!(started(&rig), 1, "no second compaction");
    summary_is_folded(&mut rig).await;
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warn_mode_warns_and_compacts_only_on_compact() {
    let ws = Workspace::new();
    let mut script = history();
    // A turn past the threshold that does not compact.
    script.push(Step::Text("still whole"));
    // `/compact now` (a compaction request): the next turn's first call is
    // the summary.
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::TextUsage("compacted", BIG));
    // Back on the policy: no compaction despite the size.
    script.push(Step::Text("whole again"));
    let llm = ScriptedLlmClient::new(script);
    let (mut rig, policy) = rig(&llm, &ws, "warn").await;
    assert_eq!(policy.mode, CompactMode::Warn);
    turns(&mut rig, SHORT_TURNS + 2, "turn").await;
    assert_eq!(started(&rig), 0, "warn mode never compacts by size");
    assert_eq!(llm.recorded().len(), SHORT_TURNS + 2);
    // The gauge the UI runs with this policy warns at this usage.
    let mut gauge = CompactionGauge::new(policy);
    let warning = gauge
        .after_turn(BIG + 5, Some(window(&rig)))
        .expect("a warning");
    assert!(warning.contains("auto-compaction is off"), "{warning}");

    // A bare `/compact` typed into the UI compacts (as `/compact now` did).
    let mut app = rkat_tui::ui::App::new(rkat_tui::ui::StatusInfo::default());
    app.status.apply(StatusUpdate::Context {
        used: Some(BIG),
        approx: false,
    });
    for c in "/compact".chars() {
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
    }
    let effects = app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)));
    assert_eq!(effects, vec![Effect::CompactNow]);
    for effect in effects {
        rig.send(effect);
    }
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::CompactRequested(_)))
        .await;
    assert!(matches!(msg, Msg::CompactRequested(Ok(()))), "{msg:?}");
    // The agent was idle: the client starts the run whose check compacts,
    // with no message of the user's. The UI hears that it runs and ends,
    // and of it only the compaction.
    assert!(
        rig.seen
            .iter()
            .any(|m| matches!(m, Msg::CompactionRun(true)))
    );
    let seen_before = rig.seen.len();
    rig.wait_for(|msg| matches!(msg, Msg::CompactionRun(false)))
        .await;
    assert_eq!(started(&rig), 1);
    let quiet = &rig.seen[seen_before..];
    assert!(
        !quiet
            .iter()
            .any(|m| matches!(m, Msg::TurnFinished(_) | Msg::RemoteTurn { .. })),
        "{quiet:#?}"
    );
    assert!(
        !quiet.iter().any(|m| matches!(m, Msg::Agent(env) if matches!(env.payload, AgentEvent::TextDelta { .. } | AgentEvent::RunStarted { .. }))),
        "{quiet:#?}"
    );
    let recorded = llm.recorded();
    assert_eq!(
        recorded.len(),
        SHORT_TURNS + 4,
        "the summary and one short call"
    );
    let last = recorded.last().unwrap().clone();
    assert!(
        last.user().contains("SUMMARY of the earlier turns"),
        "{}",
        last.user()
    );
    assert!(
        last.user().contains(rkat_tui::ui::COMPACT_PROMPT),
        "{}",
        last.user()
    );
    // A restored transcript leaves the compaction's prompt and reply out.
    let restored = rkat_tui::ui::transcript::blocks_from_messages(&rig.messages().await);
    assert!(
        !restored
            .iter()
            .any(|b| matches!(b, rkat_tui::ui::Block::User { text } if text.contains("/compact"))),
        "{restored:#?}"
    );
    assert!(
        !restored
            .iter()
            .any(|b| matches!(b, rkat_tui::ui::Block::Assistant { md } if md == "compacted")),
        "{restored:#?}"
    );

    rig.send(Effect::StartTurn("one more".into()));
    assert_eq!(
        rig.turn_finished().await.expect("turn succeeds").text,
        "whole again"
    );
    assert_eq!(started(&rig), 1, "the request was consumed");
    assert_eq!(llm.recorded().len(), SHORT_TURNS + 5);
    // The summary stays in the history.
    let last = llm.recorded().last().unwrap().clone();
    assert!(
        last.user().contains("SUMMARY of the earlier turns"),
        "{}",
        last.user()
    );
    assert!(!last.user().contains("turn 0"), "{}", last.user());
    completed(&mut rig).await;
    summary_is_folded(&mut rig).await;
    rig.quit().await;
}

/// Whether `request` is the engine's compaction summary (its prompt is the
/// last user text: `compact_prepare.rs` tells it the same way).
fn is_summary(request: &common::Recorded) -> bool {
    common::user_texts(&request.messages)
        .last()
        .is_some_and(|text| text.contains("CONTEXT COMPACTION"))
}

/// The auto-compaction of [`auto_mode_compacts_natively_at_the_threshold`]
/// with the summary step `summary`: the requests of the run, the
/// compaction completed (its completion is published at the next run).
async fn compacted(summary: Step) -> (Attached, std::sync::Arc<ScriptedLlmClient>, Workspace) {
    let ws = Workspace::new();
    let mut script = history();
    script.push(summary);
    script.push(Step::Text("after"));
    script.push(Step::Text("later"));
    let llm = ScriptedLlmClient::new(script);
    let (mut rig, _) = rig(&llm, &ws, "auto").await;
    turns(&mut rig, SHORT_TURNS + 1, "turn").await;
    rig.send(Effect::StartTurn("next".into()));
    assert_eq!(
        rig.turn_finished().await.expect("turn succeeds").text,
        "after"
    );
    rig.send(Effect::StartTurn("again".into()));
    rig.turn_finished().await.expect("turn succeeds");
    completed(&mut rig).await;
    (rig, llm, ws)
}

/// The engine's summary request as Meerkat 0.9.19 sends it: the agent's
/// request before it has the agent's tools, the summary request none, with
/// the agent's provider tag (under the gate's `DisableAll` native-tool
/// policy the engine sends the session's tag) and caching disabled by
/// rkat-tui's wrapper (`rkat_tui::daemon::compaction_call::without_cache_write`):
/// with no tools it cannot read the agent's prompt cache, so a cache write
/// would be billed for nothing. Its rows are the history's (the
/// compactor's source) and the compaction prompt last.
///
/// It fails on purpose the day the engine sends the summary with the
/// agent's tools; then the summary could read the agent's cache, the
/// workaround stops applying by itself (it applies only to a request with
/// no tools), and it goes with this assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_summary_request_carries_no_tools_and_its_own_params() {
    let (rig, llm, _ws) = compacted(Step::Text("SUMMARY of the earlier turns")).await;
    let recorded = llm.recorded();
    let at = recorded
        .iter()
        .position(is_summary)
        .expect("a summary request");
    assert_eq!(at, SHORT_TURNS + 1, "the next turn's first call");
    let (before, summary) = (&recorded[at - 1], &recorded[at]);
    assert!(
        !before.tools.is_empty(),
        "the agent's request has its tools"
    );
    assert!(
        summary.tools.is_empty(),
        "the summary request carries no tools: {:?}",
        summary.tools
    );
    // The agent's own provider tag, with only the cache policy changed.
    let expected = rkat_tui::daemon::compaction_call::without_cache_write(
        meerkat_core::Provider::Anthropic,
        &[],
        Some(&meerkat_core::ProviderParamsOverride {
            provider_tag: before.provider_params.clone(),
            ..meerkat_core::ProviderParamsOverride::default()
        }),
        None,
    )
    .and_then(|params| params.provider_tag);
    assert!(before.provider_params.is_some(), "the agent's tag");
    assert_eq!(summary.provider_params, expected);
    // Its own output budget (the compactor's), the same sampling.
    assert!(
        summary.max_tokens < before.max_tokens,
        "{} {}",
        summary.max_tokens,
        before.max_tokens
    );
    assert_eq!(summary.temperature, before.temperature);
    // The rows: the agent's system prompt as it was, the agent's history
    // row for row (same roles, same text; the assistant rows rewritten
    // without their run identity), the rest of the history since, and the
    // compaction prompt last.
    let rows = |messages: &[meerkat_core::Message]| -> Vec<(&'static str, String)> {
        messages.iter().map(row).collect()
    };
    let (was, sent) = (rows(&before.messages), rows(&summary.messages));
    assert_eq!(
        sent[..was.len()],
        was[..],
        "the history as the agent had it"
    );
    assert_eq!(
        sent[was.len()..]
            .iter()
            .map(|(role, text)| (*role, text.chars().take(24).collect::<String>()))
            .collect::<Vec<_>>(),
        [
            ("assistant", "big".to_string()),
            ("user", "next".to_string()),
            ("user", "You are performing a CON".to_string()),
        ]
    );
    for (n, (was, sent)) in before.messages.iter().zip(&summary.messages).enumerate() {
        if let (
            meerkat_core::Message::BlockAssistant(was),
            meerkat_core::Message::BlockAssistant(sent),
        ) = (was, sent)
        {
            assert!(was.identity.run_id.is_some(), "row {n}");
            assert!(sent.identity.run_id.is_none(), "row {n}");
        }
    }
    rig.quit().await;
}

/// One row as its role and text.
fn row(message: &meerkat_core::Message) -> (&'static str, String) {
    use meerkat_core::{AssistantBlock, Message};
    match message {
        Message::System(s) => ("system", s.content.clone()),
        Message::User(u) => ("user", u.text_content()),
        Message::BlockAssistant(a) => (
            "assistant",
            a.blocks
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        ),
        other => ("other", format!("{other:?}")),
    }
}

/// A summary call whose provider reports its full usage is counted as
/// reported, cache counts and all, not as the engine's history estimate:
/// the record holds the measured figures under the summaries' share, and
/// `/cost` shows them on a line of their own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_summary_call_s_reported_usage_is_its_cost() {
    let usage = meerkat_core::Usage {
        input_tokens: 34,
        output_tokens: 37,
        cache_read_tokens: Some(200),
        cache_creation_tokens: Some(5_000),
        ..meerkat_core::Usage::default()
    };
    let (mut rig, _llm, _ws) =
        compacted(Step::TextWith("SUMMARY of the earlier turns", usage)).await;
    // The daemon pushes the totals after it counted the completion.
    let pushed = within(rig.saw(|m| {
        matches!(m, Msg::Status(StatusUpdate::CostTotals(cost)) if !cost.compactions.is_empty())
    }))
    .await;
    let Msg::Status(StatusUpdate::CostTotals(cost)) = pushed else {
        panic!("{pushed:?}");
    };
    let measured = rkat_tui::cost::UsageTotals {
        input: 34,
        output: 37,
        cache_read: 200,
        cache_write: 5_000,
    };
    assert_eq!(
        cost.compactions
            .measured
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![measured],
        "{cost:?}"
    );
    assert!(cost.compactions.estimated.is_empty(), "{cost:?}");
    let breakdown = cost.breakdown(&rkat_tui::pricing::PriceTable::default());
    assert!(
        breakdown.contains("of which compaction summaries:")
            && breakdown.contains(": in 34 · out 37 · cache read 200 · cache write 5000"),
        "{breakdown}"
    );
    rig.quit().await;
}

/// The compaction policy of each start reaches an existing realm's agent.
/// The mob is created once per realm and keeps the definition it was
/// created with; the agent is built on the profile of the current start,
/// so a policy set later reaches it. First start: `warn`
/// (no native compaction), the history grows past the threshold and
/// nothing compacts. Second start of the same realm and conversation:
/// `auto` at 5%, and the next turn compacts first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_compaction_policy_reaches_an_existing_realms_agent() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(history());
    let (mut first, policy) = rig(&llm, &ws, "warn").await;
    assert_eq!(policy.mode, CompactMode::Warn);
    turns(&mut first, SHORT_TURNS + 1, "turn").await;
    assert_eq!(started(&first), 0);
    let session = first.current_session().await;
    first.quit().await;

    let llm = ScriptedLlmClient::new(vec![
        Step::Text("SUMMARY of the earlier turns"),
        Step::Text("after"),
    ]);
    let mut setup = Setup::args(&[
        "--set",
        "auto.compact=auto",
        "--set",
        "auto.compact_warn_percent=5",
        "--set",
        "auto.compact_auto_percent=5",
    ]);
    setup.client_args = vec!["-c".to_string()];
    let mut second = Attached::start_with(&ws.root, &llm, setup).await;
    assert_eq!(second.current_session().await, session);
    second.send(Effect::StartTurn("next".into()));
    let outcome = second.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "after");
    assert_eq!(started(&second), 1, "the new policy compacted");
    let requests = llm.recorded();
    assert_eq!(requests.len(), 2, "one extra call: the summary");
    assert!(
        requests[1].user().contains("SUMMARY of the earlier turns"),
        "{}",
        requests[1].user()
    );
    second.quit().await;
}
