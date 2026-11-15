//! Smart auto-compaction end to end through the daemon with a scripted
//! LLM (`src/daemon/prepare.rs`): a top-level agent whose context crosses
//! `[auto] compact_prepare_at` is steered the instruction; its
//! `compact_ready` makes the engine compact before the run's next model
//! request; an ignored request gets one reminder and still ends in the
//! normal automatic compaction; an idle agent gets it at its next turn, not by being woken;
//! it fires once per crossing; a helper's context never fires it. After a
//! compaction the agent is told what was discarded is searchable.
//!
//! The scripted model's window is 1M tokens; the thresholds are tokens.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{Recorded, ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::AgentEvent;
use rig::{Attached, Setup};
use rkat_tui::daemon::prepare::{CONTINUE_NOTE, MEMORY_NOTE, READY_SUFFIX, REQUESTED};
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;

const INSTRUCTION: &str = "update PLAN.md with where you are";

/// Five short turns: enough history for a compaction to fold.
fn history() -> Vec<Step> {
    vec![Step::Text("ok"); 5]
}

/// A daemon whose instruction runs at `at` tokens, in compaction `mode`
/// (the engine's own threshold at 5% of the window in auto mode).
async fn rig(llm: &Arc<ScriptedLlmClient>, ws: &Workspace, mode: &str, at: u64) -> Attached {
    let setup = Setup::args(&[
        "--set",
        &format!("auto.compact={mode}"),
        "--set",
        "auto.compact_auto_percent=5",
        "--set",
        "auto.compact_warn_percent=5",
        "--set",
        &format!("auto.compact_prepare={INSTRUCTION}"),
        "--set",
        &format!("auto.compact_prepare_at={at}"),
    ]);
    Attached::start_with(&ws.root, llm, setup).await
}

async fn turns(rig: &mut Attached, n: usize) {
    for i in 0..n {
        rig.send(Effect::StartTurn(format!("turn {i}")));
        rig.turn_finished().await.expect("turn succeeds");
    }
}

fn started(rig: &Attached) -> usize {
    rig.events()
        .iter()
        .filter(|e| matches!(e, AgentEvent::CompactionStarted { .. }))
        .count()
}

/// How many "asked the agent" notices the client showed.
fn asked(rig: &Attached) -> usize {
    rig.notices()
        .lines()
        .filter(|line| line.contains("asked the agent to prepare for compaction"))
        .count()
}

fn steered(request: &Recorded) -> bool {
    let text = request.user();
    text.contains(INSTRUCTION) && text.contains(READY_SUFFIX)
}

fn glob(id: &'static str, input: u64) -> Step {
    Step::ToolUsage {
        id,
        name: "glob",
        args: json!({"pattern": "*"}),
        input,
    }
}

/// `step`, streamed once `gate` is notified.
fn gated(gate: &Arc<Notify>, step: Step) -> Step {
    Step::Gated(Arc::clone(gate), Box::new(step))
}

/// Wait for the client's notice that the agent was asked, then let the
/// gated model call answer: the steer is admitted by then, so it is in the
/// run at the boundary after that call at the latest.
async fn once_asked(rig: &mut Attached, gate: &Arc<Notify>) {
    rig.saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("asked the agent to prepare")))
        .await;
    gate.notify_one();
}

/// The daemon's `/compact status` line for the rig's agent.
async fn status(rig: &Attached) -> String {
    let answer = rig
        .daemon
        .connect_bound()
        .await
        .call("rkat/compact_status", json!({}))
        .await
        .unwrap();
    answer["text"].as_str().unwrap().to_string()
}

/// The context crosses during a run that goes on: the instruction is
/// steered into it with the fixed suffix and the client shows the notice;
/// `compact_ready` requests the compaction and the engine compacts before
/// the next model request; afterwards the agent is told that what was
/// discarded is searchable. `compact_ready` is a direct tool of the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossing_steers_the_instruction_and_compact_ready_compacts() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let mut script = history();
    script.push(glob("g1", 60_000));
    script.push(gated(&gate, glob("g2", 61_000)));
    script.push(Step::ToolUsage {
        id: "c1",
        name: "compact_ready",
        args: json!({}),
        input: 61_000,
    });
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::Text("continued"));
    script.push(Step::Text("later"));
    let llm = ScriptedLlmClient::new(script);
    let mut rig = rig(&llm, &ws, "warn", 50_000).await;
    turns(&mut rig, 5).await;
    let first = &llm.recorded()[0];
    assert!(
        first.tools.iter().any(|tool| tool == "compact_ready"),
        "a direct tool: {:?}",
        first.tools
    );
    assert!(status(&rig).await.contains("armed"));

    rig.send(Effect::StartTurn("work".into()));
    once_asked(&mut rig, &gate).await;
    assert_eq!(rig.turn_finished().await.unwrap().text, "continued");
    let recorded = llm.recorded();
    // A call after the crossing, before compact_ready's, carries the steer.
    assert!(
        recorded[6..=7].iter().any(steered),
        "{}",
        recorded[7].user()
    );
    assert_eq!(asked(&rig), 1, "{}", rig.notices());
    assert!(
        rig.notices().contains("context at 60k"),
        "{}",
        rig.notices()
    );
    // compact_ready's answer, then the summary call, then the run goes on
    // over the summary.
    assert_eq!(started(&rig), 1);
    let summary_call = recorded
        .iter()
        .position(|r| format!("{:?}", r.messages).contains(REQUESTED))
        .expect("compact_ready's answer reached the model");
    let after = recorded.last().unwrap();
    assert!(
        after.user().contains("SUMMARY of the earlier turns"),
        "{summary_call}: {}",
        after.user()
    );
    assert!(!after.user().contains("turn 0"), "{}", after.user());
    // Armed again below the threshold; after the compaction completed (the
    // session ran again) the agent is told about memory_search.
    rig.send(Effect::StartTurn("again".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "later");
    within(rig.saw(|msg| {
        matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::CompactionCompleted { .. }))
    }))
    .await;
    common::until_async(async || status(&rig).await.contains(": armed")).await;
    rig.quit().await;
    // The note is in the conversation the next run reads.
    let llm = ScriptedLlmClient::new(vec![Step::Text("noted")]);
    let mut setup = Setup::args(&[]);
    setup.client_args = vec!["-c".to_string()];
    let mut again = Attached::start_with(&ws.root, &llm, setup).await;
    again.send(Effect::StartTurn("next".into()));
    again.turn_finished().await.unwrap();
    assert!(
        format!("{:?}", llm.recorded()[0].messages).contains(MEMORY_NOTE),
        "{:?}",
        llm.recorded()[0].messages
    );
    // It asked for that compaction itself: the note says to carry on.
    assert!(
        format!("{:?}", llm.recorded()[0].messages).contains(CONTINUE_NOTE),
        "{:?}",
        llm.recorded()[0].messages
    );
    again.quit().await;
}

/// An agent that ignores the request is reminded once, past the midpoint
/// to the engine's own threshold (30k and 50k: 40k), with the automatic
/// compaction named; until then `/compact status` shows the request
/// pending. It is then compacted at the engine's own threshold as before,
/// which clears the pending state; nothing came twice for the crossing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ignored_request_is_reminded_once_then_compacted_automatically() {
    let ws = Workspace::new();
    let asked_gate = Arc::new(Notify::new());
    let reminded_gate = Arc::new(Notify::new());
    let mut script = history();
    script.push(glob("g1", 32_000));
    script.push(gated(&asked_gate, glob("g2", 41_000)));
    script.push(gated(&reminded_gate, glob("g3", 42_000)));
    script.push(Step::TextUsage("ignored it", 60_000));
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::Text("after"));
    let llm = ScriptedLlmClient::new(script);
    // The engine's threshold: 5% of 1M = 50k; the instruction's 30k.
    let mut rig = rig(&llm, &ws, "auto", 30_000).await;
    turns(&mut rig, 5).await;
    rig.send(Effect::StartTurn("work".into()));
    once_asked(&mut rig, &asked_gate).await;
    rig.saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("reminded the agent to prepare")))
        .await;
    reminded_gate.notify_one();
    assert_eq!(rig.turn_finished().await.unwrap().text, "ignored it");
    let recorded = llm.recorded();
    assert!(recorded[6..=7].iter().any(steered));
    let reminder = "Compaction happens automatically at 50k tokens whether or not you have prepared: do it now: update PLAN.md with where you are, then `compact_ready`.";
    assert!(
        recorded[8].user().contains(reminder),
        "{}",
        recorded[8].user()
    );
    assert_eq!(asked(&rig), 1);
    assert_eq!(started(&rig), 0);
    let pending = status(&rig).await;
    assert!(
        pending.contains(
            "compaction requested of the agent at 32k tokens; it prepares when it reaches a stopping point"
        ),
        "{pending}"
    );
    rig.send(Effect::StartTurn("more".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "after");
    assert_eq!(started(&rig), 1, "the engine's own compaction");
    assert_eq!(asked(&rig), 1, "once per crossing: {}", rig.notices());
    assert_eq!(
        rig.notices().matches("reminded the agent").count(),
        1,
        "one reminder: {}",
        rig.notices()
    );
    assert!(
        !status(&rig)
            .await
            .contains("compaction requested of the agent")
    );
    rig.quit().await;
}

/// The crossing on the call that ended the run waits: the idle agent is
/// not woken (no model call, the trigger says it waits for the next
/// turn); its next turn gets the instruction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_agent_gets_the_instruction_at_its_next_turn() {
    let ws = Workspace::new();
    let mut script = history();
    script.push(Step::TextUsage("big", 60_000));
    script.push(Step::TextUsage("next", 60_000));
    script.push(Step::TextUsage("prepared", 60_000));
    script.push(Step::TextUsage("spare", 60_000));
    let llm = ScriptedLlmClient::new(script);
    let mut rig = rig(&llm, &ws, "warn", 50_000).await;
    turns(&mut rig, 6).await;
    let agent = rig.agent().await;
    rig.daemon.idle(&agent).await;
    assert!(
        status(&rig).await.contains("next turn"),
        "{}",
        status(&rig).await
    );
    assert_eq!(llm.recorded().len(), 6, "the idle agent was not woken");
    assert_eq!(asked(&rig), 0);
    rig.send(Effect::StartTurn("go on".into()));
    rig.turn_finished().await.unwrap();
    rig.saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("asked the agent to prepare")))
        .await;
    common::until(|| llm.recorded().iter().skip(6).any(steered)).await;
    rig.quit().await;
}

/// A helper's context never fires it, and a helper has no `compact_ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helpers_context_does_not_fire_it() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "d1",
            name: "delegate",
            args: json!({
                "member_id": "h1",
                "task": "read a lot",
                "result_label": "result",
                "max_text_bytes": 4000,
            }),
        },
        Step::TextUsage("helper done", 90_000),
        Step::Text("ok"),
    ]);
    let mut rig = rig(&llm, &ws, "warn", 50_000).await;
    rig.send(Effect::StartTurn("delegate it".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "ok");
    let recorded = llm.recorded();
    assert!(recorded[0].tools.iter().any(|t| t == "compact_ready"));
    assert!(
        !recorded[1].tools.iter().any(|t| t == "compact_ready"),
        "{:?}",
        recorded[1].tools
    );
    assert_eq!(asked(&rig), 0, "{}", rig.notices());
    assert!(
        status(&rig).await.contains(": armed"),
        "{}",
        status(&rig).await
    );
    rig.quit().await;
}

/// `rkat/compact_prepare` through the daemon: the instruction comes from
/// the user's own `tui.toml`; a new threshold applies at once (the next
/// crossing of it asks the agent) and is written to the realm's
/// `tui.toml`; `off` is none in this workspace although the user's file
/// has one, says so, and holds across a daemon restart; `default` goes
/// back, live, to the user's instruction and threshold, each named by
/// where it comes from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_workspace_sets_off_and_defaults_over_the_users_file_live() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("tui.toml"),
        format!("[auto]\ncompact_prepare = \"{INSTRUCTION}\"\n"),
    )
    .unwrap();
    let realm_toml = common::realm_of(&ws.root).join("tui.toml");
    let stored = || -> toml::Table {
        toml::from_str(&std::fs::read_to_string(&realm_toml).unwrap_or_default()).unwrap()
    };
    let mut script = history();
    script.push(Step::TextUsage("big", 60_000));
    let llm = ScriptedLlmClient::new(script);
    let setup = Setup::args(&["--set", "auto.compact=warn"]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    let start = status(&rig).await;
    assert!(
        start.starts_with(&format!(
            "compact prepare: \"{INSTRUCTION}\", from your tui.toml; asked at 70% of the window (about 700k tokens for "
        )),
        "{start}"
    );
    assert!(start.contains("), the default: armed"), "{start}");
    // The threshold, live: the next reading over it asks the agent.
    let answer = set(&rig, "auto.compact_prepare_at", Some("50000")).await;
    let realm_toml_shown = realm_toml.display().to_string();
    assert_eq!(
        answer["text"],
        format!(
            "auto.compact_prepare_at = 50000: written to this workspace's tui.toml ({realm_toml_shown}); in force: 50000 (from this workspace); applied at once; the agents are asked at 50k tokens to: \"{INSTRUCTION}\""
        )
    );
    assert!(answer.get("warning").is_none(), "{answer}");
    assert_eq!(
        stored()["auto"]["compact_prepare_at"].as_integer(),
        Some(50_000)
    );
    turns(&mut rig, 6).await;
    let agent = rig.agent().await;
    rig.daemon.idle(&agent).await;
    assert!(
        status(&rig).await.contains("next turn"),
        "{}",
        status(&rig).await
    );
    // Off for the workspace, over the user's file.
    let answer = set(&rig, "auto.compact_prepare", Some("")).await;
    assert!(
        answer["text"].as_str().unwrap().contains(
            "in force: \"\" (off) (from this workspace); applied at once; no instruction is in force; the threshold is 50k tokens"
        ),
        "{answer}"
    );
    assert_eq!(stored()["auto"]["compact_prepare"].as_str(), Some(""));
    rig.quit().await;
    // A restart keeps it off.
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut setup = Setup::args(&["--set", "auto.compact=warn"]);
    setup.client_args = vec!["-c".to_string()];
    let rig = Attached::start_with(&ws.root, &llm, setup).await;
    let off = status(&rig).await;
    assert!(
        off.starts_with(
            "compact prepare: off for this workspace (your tui.toml has one: /compact prepare default uses it); its threshold is 50k tokens, from this workspace"
        ),
        "{off}"
    );
    // Both back to the user's file, live, and gone from the realm's.
    set(&rig, "auto.compact_prepare_at", None).await;
    let answer = set(&rig, "auto.compact_prepare", None).await;
    let text = answer["text"].as_str().unwrap();
    assert!(
        text.starts_with("auto.compact_prepare: removed from this workspace's tui.toml")
            && text.contains(&format!(
                "in force: {INSTRUCTION} (from your tui.toml); applied at once; the agents are asked at 70% of the window"
            )),
        "{text}"
    );
    let auto = stored()
        .get("auto")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    assert!(auto.get("compact_prepare").is_none(), "{auto}");
    assert!(auto.get("compact_prepare_at").is_none(), "{auto}");
    assert!(status(&rig).await.contains("from your tui.toml"));
    rig.quit().await;
}

/// `rkat/set_setting` of `key` for the workspace, as `/compact prepare`
/// sends it.
async fn set(rig: &Attached, key: &str, value: Option<&str>) -> serde_json::Value {
    rig.daemon
        .connect_bound()
        .await
        .call("rkat/set_setting", json!({"key": key, "value": value}))
        .await
        .unwrap()
}

/// A threshold at or above the automatic compaction applies and warns
/// with the start-up notice's text; one the command line overrides is
/// written, not applied, and the answer names the command line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_threshold_is_set_with_the_start_up_warning() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let rig = rig(&llm, &ws, "auto", 30_000).await;
    let over = set(&rig, "auto.compact_prepare_at", Some("90%")).await;
    let text = over["text"].as_str().unwrap();
    assert!(
        text.contains("in force: 30000 (from the command line); the value in force did not change; the daemon's command line sets it (30000), which wins until the daemon restarts"),
        "{text}"
    );
    assert!(over.get("warning").is_none(), "{over}");
    rig.quit().await;
    // Without the command line's threshold: applied, with the warning.
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let setup = Setup::args(&[
        "--set",
        "auto.compact=auto",
        "--set",
        "auto.compact_auto_percent=5",
        "--set",
        "auto.compact_warn_percent=5",
        "--set",
        &format!("auto.compact_prepare={INSTRUCTION}"),
    ]);
    let rig = Attached::start_with(&ws.root, &llm, setup).await;
    let answer = set(&rig, "auto.compact_prepare_at", Some("90%")).await;
    let warning = answer["warning"].as_str().unwrap();
    assert!(
        warning.starts_with("[auto] compact_prepare_at = 90% (900k tokens) is at or above the automatic compaction at 50k tokens, so the prepare instruction would never run first"),
        "{warning}"
    );
    assert!(
        answer["text"]
            .as_str()
            .unwrap()
            .contains("the agents are asked at 90% of the window (about 900k tokens for "),
        "{answer}"
    );
    rig.quit().await;
}
