//! Schedules (P6 item A) end to end through the `rkat-tui-mob` daemon
//! with a scripted LLM: the agent creates a schedule on its own session
//! with the engine's `meerkat_schedule_create`, the daemon's runtime fires
//! it, and the agent's turn runs labelled with the schedule; the gate
//! treats the fired run as one no user started (unattended while nobody
//! is attached to the agent); `/schedules` lists and changes the agent's
//! own; a retiring agent's schedules are deleted and never fire.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Attached, Running};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::daemon::protocol::{AgentChoice, ScheduleOp};
use rkat_tui::ui::{Departure, Effect, Msg};
use serde_json::json;

/// The text of the tool result for call `id` in `messages`.
fn tool_result(messages: &[Message], id: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == id)
                .map(meerkat_core::types::ToolResult::text_content),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// The arguments of a one-shot schedule `name`, due `in_secs` from now, on
/// the agent's own session with `prompt`.
pub fn once(name: &str, in_secs: i64, prompt: &str) -> serde_json::Value {
    let due = chrono::Utc::now() + chrono::Duration::seconds(in_secs);
    json!({
        "name": name,
        "trigger": {"type": "once", "due_at_utc": due.to_rfc3339()},
        "target": {
            "target_kind": "session",
            "type": "current_session",
            "action": {"type": "prompt", "prompt": prompt}
        }
    })
}

/// The agent's call creating the one-shot schedule `name`, due `in_secs`
/// after the call is made (not after the script is written: a loaded
/// machine starts the daemon slowly).
fn at_call(id: &'static str, name: &'static str, in_secs: i64, prompt: &'static str) -> Step {
    Step::Dynamic(Arc::new(move |_: &[meerkat_core::Message]| Step::Tool {
        id,
        name: "meerkat_schedule_create",
        args: once(name, in_secs, prompt),
    }))
}

/// Whether `m` says the run was fired by schedule `name`.
fn fired_by(m: &Msg, name: &str) -> bool {
    matches!(m, Msg::Triggered { label, .. } if label == &format!("scheduled: {name}"))
}

/// The audit log's lines (in the realm).
fn audit_lines(root: &std::path::Path) -> Vec<serde_json::Value> {
    let path = common::realm_of(root).join(rkat_tui::daemon::audit::AUDIT_FILE);
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The agent schedules a one-shot timer on its own session; the daemon
/// fires it with nobody typing: the client is told which schedule fired
/// the run (the transcript labels it), the turn runs, `/schedules` shows
/// it fired once with the run's cost, the audit log names the trigger, and
/// the user's own `/schedules delete` removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_the_agent_creates_fires_its_turn() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        at_call("s1", "check-build", 2, "check the build"),
        Step::Text("scheduled"),
        Step::Text("checked"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("remind me".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.unwrap().text, "scheduled");
    let created = tool_result(&llm.requests()[1], "s1");
    assert!(created.contains("schedule_id"), "{created}");
    // Fired: the prompt, labelled with the schedule, and the turn.
    let fired = rig.wait_for(|m| fired_by(m, "check-build")).await;
    assert!(
        matches!(&fired, Msg::Triggered { prompt, .. } if prompt == "check the build"),
        "{fired:?}"
    );
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.unwrap().text, "checked");
    let last = llm.requests()[2].last().cloned().unwrap();
    assert!(
        matches!(&last, meerkat_core::Message::User(user) if user.text_content() == "check the build"),
        "{last:?}"
    );
    // The audit log names the run's trigger.
    let triggered: Vec<_> = audit_lines(&ws.root)
        .into_iter()
        .filter(|line| line["event"] == "run_triggered")
        .collect();
    assert_eq!(triggered.len(), 1, "{triggered:?}");
    assert!(
        triggered[0]["detail"]
            .as_str()
            .unwrap()
            .starts_with("schedule check-build ("),
        "{triggered:?}"
    );
    assert!(triggered[0]["run"].is_string());
    // Fired once, with what its run cost.
    rig.send(Effect::Schedules(None));
    rig.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. }
            if text.contains("check-build") && text.contains("fired 1x") && text.contains("next none"))
    })
    .await;
    // The user's own delete: no approval.
    rig.send(Effect::Schedules(Some((
        ScheduleOp::Delete,
        "check-build".into(),
    ))));
    rig.wait_for(
        |m| matches!(m, Msg::Notice { text, .. } if text == "deleted schedule check-build"),
    )
    .await;
    rig.send(Effect::Schedules(None));
    rig.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("this agent has no schedules")))
        .await;
    assert_eq!(rig.escalations(), 0);
    rig.quit().await;
}

/// In manual mode the agent's `meerkat_schedule_create` asks the user,
/// showing the trigger and the target; a denied one creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creating_a_schedule_asks_in_manual_mode() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        at_call("s1", "nightly", 3600, "run the nightly checks"),
        Step::Text("not scheduled"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.set_mode(PermissionMode::Manual).await;
    rig.send(Effect::StartTurn("schedule the nightly checks".into()));
    let (prompt, id) = rig.escalation().await;
    assert_eq!(prompt.tool, "meerkat_schedule_create");
    let shown = format!(
        "{} {}",
        prompt.args_preview,
        prompt.args_full.clone().unwrap_or_default()
    );
    assert!(shown.contains("once at "), "{shown}");
    assert!(shown.contains("current_session"), "{shown}");
    assert!(shown.contains("run the nightly checks"), "{shown}");
    rig.send(Effect::RespondEscalation(
        id,
        rkat_tui::ui::EscalationAnswer::Deny { comment: None },
    ));
    rig.turn_finished().await.result.unwrap();
    rig.send(Effect::Schedules(None));
    rig.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("this agent has no schedules")))
        .await;
    rig.quit().await;
}

/// A fired schedule is a turn no user started: with nobody attached to
/// its agent, the reviewed call it makes is denied without asking the
/// classifier, and the notice is shown when a client attaches; with a
/// client attached, the same call is reviewed as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fired_schedule_is_unattended_while_nobody_is_attached() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        // fetch-a fires while nobody is attached.
        at_call("s1", "fetch-a", 2, "fetch a"),
        Step::Text("scheduled"),
        // fetch-a, nobody attached.
        Step::Tool {
            id: "a1",
            name: "shell",
            args: json!({"command": "curl -s http://127.0.0.1:9/a"}),
        },
        Step::Text("fetched a"),
        // fetch-b, created by the attached user's turn, due when made.
        at_call("s2", "fetch-b", 2, "fetch b"),
        Step::Text("scheduled b"),
        Step::Tool {
            id: "b1",
            name: "shell",
            args: json!({"command": "curl -s http://127.0.0.1:9/b"}),
        },
        Step::Text("fetched b"),
    ]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let agent = a.agent().await;
    a.send(Effect::StartTurn("schedule a fetch".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "scheduled");
    a.send(Effect::Leave(Departure::Detach));
    a.quit().await;
    // fetch-a's run: its curl is denied unattended, no classifier asked.
    // Its run's second call carries the denied curl's result.
    llm.wait_call(3).await;
    let curls = || {
        classifier
            .seen()
            .iter()
            .filter(|(tool, args, _)| tool == "shell" && args.to_string().contains("curl"))
            .count()
    };
    assert_eq!(curls(), 0, "the unattended curl was not reviewed");
    let result = tool_result(&llm.requests()[3], "a1");
    assert!(result.contains("needs a user attached"), "{result}");
    // A client attaches before fetch-b: it is shown what was denied, and
    // fetch-b's curl is reviewed.
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::Agent { name: agent }).await;
    b.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. } if text.contains("wanted to run `curl -s http://127.0.0.1:9/a`"))
    })
    .await;
    b.send(Effect::StartTurn("schedule another".into()));
    assert_eq!(b.turn_finished().await.result.unwrap().text, "scheduled b");
    b.wait_for(|m| fired_by(m, "fetch-b")).await;
    assert_eq!(b.turn_finished().await.result.unwrap().text, "fetched b");
    assert_eq!(curls(), 1, "attached, the fired run's curl was reviewed");
    let result = tool_result(&llm.requests()[7], "b1");
    assert!(!result.contains("needs a user attached"), "{result}");
    b.quit().await;
    daemon.stop().await;
}

/// `/quit` that retires the agent deletes its schedules (the farewell
/// names them), and the deleted one never fires: past its due time the
/// other agent's sentinel turn is the only model call, and the stored
/// schedule is deleted with no occurrence dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retiring_agent_s_schedules_are_deleted_and_never_fire() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        at_call("s1", "wake-b", 8, "wake b"),
        Step::Text("scheduled"),
        Step::Text("sentinel"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    b.send(Effect::StartTurn("wake me later".into()));
    assert_eq!(b.turn_finished().await.result.unwrap().text, "scheduled");
    // Created during that turn: due no earlier than 8 s from its start.
    let due = chrono::Utc::now() + chrono::Duration::seconds(8);
    let status = daemon.status().await;
    let b_name = b.agent().await;
    assert_eq!(
        status
            .agents
            .iter()
            .find(|row| row.name == b_name)
            .unwrap()
            .schedules,
        1
    );
    // Listed with its next occurrence while it waits.
    b.send(Effect::Schedules(None));
    b.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. }
            if text.contains("wake-b  once at ") && text.contains("next 20") && text.contains("active  fired 0x"))
    })
    .await;
    b.send(Effect::Leave(Departure::Retire));
    b.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("its schedules were deleted: wake-b")))
        .await;
    b.quit().await;
    // Past the due time (a deadline, not a margin), the other agent's
    // turn is the next model call.
    common::within(async {
        while chrono::Utc::now() < due + chrono::Duration::seconds(2) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    a.send(Effect::StartTurn("sentinel".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "sentinel");
    let prompts: Vec<String> = llm
        .requests()
        .iter()
        .filter_map(|messages| messages.last().cloned())
        .filter_map(|m| match m {
            meerkat_core::Message::User(user) => Some(user.text_content()),
            _ => None,
        })
        .collect();
    assert!(!prompts.iter().any(|p| p == "wake b"), "{prompts:?}");
    assert_eq!(llm.requests().len(), 3);
    a.quit().await;
    daemon.stop().await;
    // The stored schedule: deleted, nothing dispatched.
    let schedules = stored_schedules(&ws).await;
    assert_eq!(schedules.len(), 1);
    let (schedule, occurrences) = &schedules[0];
    assert_eq!(schedule.phase, meerkat::SchedulePhase::Deleted);
    assert!(
        occurrences.iter().all(|o| o.dispatched_at_utc.is_none()),
        "{occurrences:?}"
    );
}

/// The realm's schedules and their occurrences, read from the store after
/// the daemon stopped.
async fn stored_schedules(ws: &Workspace) -> Vec<(meerkat::Schedule, Vec<meerkat::Occurrence>)> {
    let realm = rkat_tui::bootstrap::resolve_scope(&common::args_with(&ws.root, &[]))
        .unwrap()
        .realm_root();
    // The engine keeps the schedule domain in the realm's sessions
    // database.
    let path = realm.join("sessions.sqlite3");
    let store = meerkat_store::SqliteScheduleStore::open(&path).unwrap();
    use meerkat::ScheduleStore as _;
    let schedules = store
        .list_schedules(meerkat::ScheduleFilter {
            include_deleted: true,
            ..meerkat::ScheduleFilter::default()
        })
        .await
        .unwrap();
    let mut out = Vec::new();
    for schedule in schedules {
        let occurrences = store
            .list_occurrences(meerkat::OccurrenceFilter {
                schedule_id: Some(schedule.schedule_id.clone()),
                include_terminal: true,
                ..meerkat::OccurrenceFilter::default()
            })
            .await
            .unwrap();
        out.push((schedule, occurrences));
    }
    out
}
