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
    rig.quit().await;
}

/// The compaction policy of each start reaches an existing realm's agent.
/// The mob is created once per realm and keeps the definition it was
/// created with, so a policy set later never reached the agent; now the
/// agent is built on the profile of the current start. First start: `warn`
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
