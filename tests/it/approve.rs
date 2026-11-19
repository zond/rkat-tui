//! An approval from `/permissions`' Blocked section tells whoever made
//! the blocked call at once, and an idle
//! agent goes on by itself (the owner's decision of 2026-11-16): the note
//! is steered into a running turn, or starts a run of its own that the
//! user's act started (attended, shown under "approved: …"), also right
//! after an interrupt; a helper's call goes to that helper while it runs,
//! else to its agent. A call approved once stays on the list, marked,
//! until the agent uses the approval, and can be withdrawn. The
//! approval-events loop and `rkat/set_mode` wait for no turn.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{ScriptedLlmClient, Step, Workspace, leak, within};
use meerkat_core::{AgentEvent, Message};
use rig::{Attached, Client, Setup};
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::grants::GrantScope;
use rkat_tui::ui::{Effect, Msg, PickerKind, Section, TurnFailure};
use serde_json::json;
use tokio::sync::Notify;

/// The label a client shows over an approval's note.
const APPROVED: &str = "approved: the agent is told and goes on";

/// What the agent reads of an approval of its own call.
const TOLD: &str = "The user approved your blocked call";

/// Blocks every call whose arguments name a path with `marker` in it, at
/// every review; allows the rest.
struct BlocksMarkers;

#[async_trait::async_trait]
impl ActionClassifier for BlocksMarkers {
    async fn classify(
        &self,
        input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        if input.args.to_string().contains("marker") {
            Ok(ClassifierVerdict::Block {
                rule: Some("T3".into()),
                reason: "it touches files outside the project".into(),
                annotation: None,
                path: None,
                effect: None,
            })
        } else {
            Ok(ClassifierVerdict::Allow {
                reason: "fine".into(),
            })
        }
    }
}

fn setup() -> Setup {
    Setup::default().classifier(Arc::new(BlocksMarkers) as Arc<dyn ActionClassifier>)
}

/// A file outside the project the agent's shell call touches.
fn marker(ws: &Workspace, name: &str) -> PathBuf {
    ws.sibling(&format!("{name}-marker"))
}

/// The shell call `id` touching `path`.
fn touch(id: &'static str, path: &Path) -> Step {
    Step::Tool {
        id,
        name: "shell",
        args: json!({"command": format!("touch {}", path.display())}),
    }
}

fn results(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResults { .. }))
        .count()
}

/// Whether `messages` hold the result of tool call `id`.
fn has_result(messages: &[Message], id: &str) -> bool {
    messages.iter().any(|m| {
        matches!(m, Message::ToolResults { results, .. } if results.iter().any(|r| r.tool_use_id == id))
    })
}

/// The text of the last user message of `messages`, if the last message
/// is one.
fn last_user(messages: &[Message]) -> Option<String> {
    match messages.last() {
        Some(Message::User(user)) => Some(user.text_content()),
        _ => None,
    }
}

/// The agent's script: a prompt touches `path`; a note telling it the
/// call was approved makes it touch `path` again; a tool result is
/// answered with a text naming how many results it has seen.
fn agent(path: PathBuf) -> Step {
    Step::Dynamic(Arc::new(move |messages: &[Message]| {
        let n = results(messages);
        match last_user(messages) {
            Some(text) if text.contains("The user approved") => {
                touch(leak(format!("t{}", n + 1)), &path)
            }
            Some(_) => touch(leak(format!("t{}", n + 1)), &path),
            None => Step::Text(leak(format!("after {n} results"))),
        }
    }))
}

/// The `/permissions` dialog as the client's `/permissions blocked`
/// opens it: its Blocked section shown, the newest call first.
async fn listed(client: &mut Client) -> rkat_tui::ui::ListPicker {
    client.send(Effect::RequestPermissions(Some(Section::Blocked)));
    let shown = client
        .wait_for(|msg| {
            matches!(msg, Msg::ShowPicker { picker, refresh: false }
                if picker.kind == PickerKind::Permissions && picker.section() == Some(Section::Blocked.index()))
        })
        .await;
    let Msg::ShowPicker { picker, .. } = shown else {
        unreachable!()
    };
    picker
}

/// The run ids the client saw start, in order.
fn runs(client: &Client) -> Vec<meerkat_core::RunId> {
    client
        .events()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::RunStarted { identity, .. } => identity.run_id.clone(),
            _ => None,
        })
        .collect()
}

/// Approving once while the agent is idle starts a run of the note: the
/// client shows it headed by why, the agent repeats the call and it runs
/// (the file exists), the run is the user's (attended), and the list is
/// empty afterwards. Before the change the note was appended and nothing
/// ran until the user's next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_agent_runs_its_call_again_once_approved() {
    let ws = Workspace::new();
    let path = marker(&ws, "idle");
    let llm = ScriptedLlmClient::new(vec![agent(path.clone()); 10]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    rig.send(Effect::StartTurn("touch the marker".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "after 1 results");
    assert!(!path.exists());
    let picker = listed(&mut rig).await;
    assert_eq!(picker.rows.len(), 1, "{:?}", picker.rows);
    let id = rig.daemon.engine().blocks().list(&rig.daemon.session)[0].id;
    rig.send(Effect::PickedApproval(id, GrantScope::Once, None));
    rig.saw(|m| matches!(m, Msg::Triggered { label, prompt, .. } if label == APPROVED && prompt.starts_with(TOLD)))
        .await;
    assert_eq!(rig.turn_finished().await.unwrap().text, "after 2 results");
    assert!(path.exists(), "the repeated call ran");
    let told = llm.recorded()[2].user();
    assert!(
        told.contains("once. Run it again exactly as before"),
        "{told}"
    );
    // The run is the user's: its calls are attended.
    let run = runs(&rig).last().cloned().unwrap();
    assert!(rig.daemon.engine().human_run(&rig.daemon.session, &run));
    assert!(
        rig.daemon
            .engine()
            .blocks()
            .list(&rig.daemon.session)
            .is_empty()
    );
    rig.quit().await;
}

/// The reported case: the user approves once while the agent's turn is
/// still running (its model call after the block is held). The note is
/// steered into that turn, which goes on and repeats the call: one turn
/// ends, with the file there. Before the change the note was appended,
/// read only by a later turn, and the turn ended asking for approval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_during_the_turn_is_steered_into_it() {
    let ws = Workspace::new();
    let path = marker(&ws, "running");
    let gate = Arc::new(Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        let path = path.clone();
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let told = common::user_texts(messages)
                .concat()
                .contains("The user approved");
            match (results(messages), told) {
                (0, _) => touch("t1", &path),
                // The model call after the block: held, then it ends the
                // turn asking for approval.
                (1, false) => {
                    Step::Gated(Arc::clone(&gate), Box::new(Step::Text("please approve it")))
                }
                (1, true) => touch("t2", &path),
                _ => Step::Text("done"),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    rig.send(Effect::StartTurn("touch the marker".into()));
    common::until(|| llm.recorded().len() == 2).await;
    let id = rig.daemon.engine().blocks().list(&rig.daemon.session)[0].id;
    rig.send(Effect::PickedApproval(id, GrantScope::Once, None));
    // Told: the steer reached the engine.
    rig.saw(|m| matches!(m, Msg::Triggered { label, .. } if label == APPROVED))
        .await;
    gate.notify_one();
    let first = rig.turn_finished().await.unwrap();
    assert_eq!(first.text, "done", "the same turn went on");
    assert!(path.exists(), "the repeated call ran");
    assert!(
        rig.daemon
            .engine()
            .blocks()
            .list(&rig.daemon.session)
            .is_empty()
    );
    rig.quit().await;
}

/// For this session and always likewise wake the idle agent, once each
/// (the next model request after the approval's run is the user's next
/// prompt); removing the session grant wakes nobody (the next request
/// after it is the user's prompt again).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grants_from_approve_wake_the_agent_once_and_removing_one_does_not() {
    let ws = Workspace::new();
    let first = marker(&ws, "session");
    let second = marker(&ws, "always");
    let step = {
        let (first, second) = (first.clone(), second.clone());
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let n = results(messages);
            let path = if common::user_texts(messages).concat().contains("SECOND") {
                &second
            } else {
                &first
            };
            match last_user(messages) {
                Some(text) if text.contains("NOTHING") => Step::Text("nothing to do"),
                Some(_) => touch(leak(format!("t{}", n + 1)), path),
                None => Step::Text(leak(format!("after {n} results"))),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    let session = rig.daemon.session.clone();
    for (prompt, path, scope) in [
        ("touch the first", &first, GrantScope::Session),
        ("SECOND: touch the second", &second, GrantScope::Always),
    ] {
        rig.send(Effect::StartTurn(prompt.into()));
        rig.turn_finished().await.unwrap();
        let id = rig.daemon.engine().blocks().list(&session)[0].id;
        rig.send(Effect::PickedApproval(id, scope, None));
        rig.turn_finished().await.unwrap();
        assert!(path.exists(), "{scope:?}: the repeated call ran");
        let calls = llm.recorded().len();
        assert!(
            llm.recorded()[calls - 2].user().contains("by granting"),
            "{scope:?}"
        );
        // Once: the next request is the user's next prompt.
        rig.send(Effect::StartTurn("NOTHING here".into()));
        assert_eq!(rig.turn_finished().await.unwrap().text, "nothing to do");
        assert_eq!(llm.recorded().len(), calls + 1, "{scope:?}: woken once");
    }
    // Removing the session grant starts no run.
    let grant = rig
        .daemon
        .engine()
        .grants()
        .rows(Some(&session))
        .into_iter()
        .find(|row| row.removable)
        .unwrap();
    let calls = llm.recorded().len();
    rig.send(Effect::PickedGrantRemoval(grant.id));
    rig.wait_for(
        |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("removed the session grant")),
    )
    .await;
    rig.send(Effect::StartTurn("NOTHING again".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "nothing to do");
    assert_eq!(llm.recorded().len(), calls + 1, "the removal woke nobody");
    assert!(llm.recorded()[calls].user().ends_with("NOTHING again"));
    rig.quit().await;
}

/// A call approved once waits on the list, marked, while the agent has not
/// run it again (its model call is held); the Blocked section with only
/// that entry shows it. Withdrawn, it is blocked again: the agent's repeat is
/// blocked, and listed once. Approved again, the repeat runs and the list
/// is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_once_waits_on_the_list_until_used_and_can_be_withdrawn() {
    let ws = Workspace::new();
    let path = marker(&ws, "waiting");
    let gate = Arc::new(Notify::new());
    let held = Arc::new(AtomicUsize::new(0));
    let step = {
        let (gate, held, path) = (Arc::clone(&gate), Arc::clone(&held), path.clone());
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let n = results(messages);
            match last_user(messages) {
                // The first note's run: held before it repeats the call.
                Some(text) if text.contains("The user approved") => {
                    let call = touch(leak(format!("t{}", n + 1)), &path);
                    if held.fetch_add(1, Ordering::SeqCst) == 0 {
                        Step::Gated(Arc::clone(&gate), Box::new(call))
                    } else {
                        call
                    }
                }
                Some(_) => touch(leak(format!("t{}", n + 1)), &path),
                None => Step::Text(leak(format!("after {n} results"))),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    let session = rig.daemon.session.clone();
    rig.send(Effect::StartTurn("touch the marker".into()));
    rig.turn_finished().await.unwrap();
    let id = rig.daemon.engine().blocks().list(&session)[0].id;
    rig.send(Effect::PickedApproval(id, GrantScope::Once, None));
    // The approval's run asks the model, which is held.
    common::until(|| held.load(Ordering::SeqCst) == 1).await;
    let picker = listed(&mut rig).await;
    assert_eq!(picker.rows.len(), 1);
    assert!(
        picker.rows[0]
            .text
            .ends_with("approved once · waiting for the agent to run it again"),
        "{}",
        picker.rows[0].text
    );
    rig.send(Effect::PickedWithdrawal(id));
    rig.wait_for(
        |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("withdrew the approval")),
    )
    .await;
    let refreshed = rig
        .wait_for(|m| matches!(m, Msg::ShowPicker { refresh: true, .. }))
        .await;
    assert!(
        matches!(&refreshed, Msg::ShowPicker { picker, .. }
            if picker.rows.len() == 1 && !picker.rows[0].text.contains("approved once")),
        "{refreshed:?}"
    );
    // The repeat now meets the block again.
    gate.notify_one();
    assert_eq!(rig.turn_finished().await.unwrap().text, "after 2 results");
    assert!(!path.exists(), "the withdrawn approval let nothing run");
    let listed_now = rig.daemon.engine().blocks().list(&session);
    assert_eq!(listed_now.len(), 1, "listed once: {listed_now:?}");
    // Approved again: the repeat runs and the entry goes.
    rig.send(Effect::PickedApproval(
        listed_now[0].id,
        GrantScope::Once,
        None,
    ));
    assert_eq!(rig.turn_finished().await.unwrap().text, "after 3 results");
    assert!(path.exists());
    assert!(rig.daemon.engine().blocks().list(&session).is_empty());
    rig.quit().await;
}

/// An approval right after the user interrupted the agent starts its run
/// all the same: the user is acting on the agent now (a helper's ending
/// is held in that state; an approval is not).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_right_after_an_interrupt_starts_the_run() {
    let ws = Workspace::new();
    let path = marker(&ws, "interrupted");
    let step = {
        let path = path.clone();
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let n = results(messages);
            match last_user(messages) {
                Some(text) if text.contains("The user approved") => {
                    touch(leak(format!("t{}", n + 1)), &path)
                }
                Some(_) => touch("t1", &path),
                // The model call after the block never answers: the user
                // interrupts it.
                None if n == 1 => Step::Hang,
                None => Step::Text("done"),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    rig.send(Effect::StartTurn("touch the marker".into()));
    common::until(|| llm.recorded().len() == 2).await;
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    let id = rig.daemon.engine().blocks().list(&rig.daemon.session)[0].id;
    rig.send(Effect::PickedApproval(id, GrantScope::Once, None));
    rig.saw(|m| matches!(m, Msg::Triggered { label, .. } if label == APPROVED))
        .await;
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    assert!(path.exists(), "the approval's run repeated the call");
    rig.quit().await;
}

/// The agent's side of the helper tests: a prompt assigns `scout` its
/// task; told that its helper's call was approved, it makes the call
/// itself; after the `assign` it ends its run ("waiting"), or, with `hold`,
/// is held in one more call (a `glob`) until `hold` is notified; the
/// note that its helper finished, or the end of that call, ends its turn.
fn agent_assigning(messages: &[Message], path: &Path, hold: Option<&Arc<Notify>>) -> Step {
    let told = common::user_texts(messages)
        .concat()
        .contains("that your helper `scout` made");
    if told {
        return if has_result(messages, "a2") {
            Step::Text("agent done")
        } else {
            touch("a2", path)
        };
    }
    if common::after(messages, "a1") {
        return match hold {
            Some(gate) => Step::Gated(
                Arc::clone(gate),
                Box::new(Step::Tool {
                    id: "g1",
                    name: "glob",
                    args: json!({"pattern": "*.nothing"}),
                }),
            ),
            None => Step::Text("waiting"),
        };
    }
    if results(messages) > 0 {
        return Step::Text("agent done");
    }
    common::assign("a1", "scout", "HELPER-TASK: touch the marker")
}

/// The rule for a helper's call: the agent is told (the daemon does not
/// message a helper about its own call). With the helper still running
/// (its model call after the block held) and the agent's run held in a
/// call, the note goes into the agent's running turn, named as the
/// helper's call; the agent makes the call itself, which the approval lets
/// run. The helper never reads the note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_approved_call_is_told_to_its_running_agent() {
    let ws = Workspace::new();
    let path = marker(&ws, "helper");
    let gate = Arc::new(Notify::new());
    let hold = Arc::new(Notify::new());
    let step = {
        let (gate, hold, path) = (Arc::clone(&gate), Arc::clone(&hold), path.clone());
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if common::is_helper(messages, "HELPER-TASK") {
                return match results(messages) {
                    0 => touch("h1", &path),
                    _ => Step::Gated(Arc::clone(&gate), Box::new(Step::Text("helper could not"))),
                };
            }
            agent_assigning(messages, &path, Some(&hold))
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    let session = rig.daemon.session.clone();
    rig.send(Effect::StartTurn("start a helper".into()));
    common::until(|| !rig.daemon.engine().blocks().list(&session).is_empty()).await;
    // The agent's run is held in its call after the `assign`.
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| common::after(&r.messages, "a1"))
    })
    .await;
    let call = rig.daemon.engine().blocks().list(&session)[0].clone();
    assert_eq!(call.helper.as_deref(), Some("scout"));
    rig.send(Effect::PickedApproval(call.id, GrantScope::Once, None));
    rig.saw(|m| matches!(m, Msg::Triggered { label, prompt, .. } if label == APPROVED && prompt.contains("that your helper `scout` made")))
        .await;
    hold.notify_one();
    gate.notify_one();
    // The agent made the call itself, and read its result.
    common::until(|| {
        llm.requests()
            .iter()
            .any(|messages| has_result(messages, "a2"))
    })
    .await;
    assert!(path.exists());
    let helper_told = llm
        .recorded()
        .iter()
        .filter(|r| common::is_helper(&r.messages, "HELPER-TASK"))
        .any(|r| r.user().contains("The user approved"));
    assert!(!helper_told, "the helper was not told");
    assert!(rig.daemon.engine().blocks().list(&session).is_empty());
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    rig.quit().await;
}

/// A helper's blocked call approved after the helper ended goes to its
/// agent the same way: the idle agent gets a run of the note, makes the
/// call itself, and it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_helper_s_approved_call_goes_to_its_agent() {
    let ws = Workspace::new();
    let path = marker(&ws, "ended");
    let step = {
        let path = path.clone();
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if common::is_helper(messages, "HELPER-TASK") {
                return match results(messages) {
                    0 => touch("h1", &path),
                    _ => Step::Text("helper could not"),
                };
            }
            agent_assigning(messages, &path, None)
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    let session = rig.daemon.session.clone();
    rig.send(Effect::StartTurn("start a helper".into()));
    // The helper ended, its agent was told, and the agent is idle.
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    common::until(|| rig.daemon.registry().live_for(&session).is_empty()).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    let call = rig.daemon.engine().blocks().list(&session)[0].clone();
    assert_eq!(call.helper.as_deref(), Some("scout"));
    rig.send(Effect::PickedApproval(call.id, GrantScope::Once, None));
    rig.saw(|m| matches!(m, Msg::Triggered { label, .. } if label == APPROVED))
        .await;
    common::until(|| {
        llm.requests()
            .iter()
            .any(|messages| has_result(messages, "a2"))
    })
    .await;
    assert!(path.exists(), "the agent made the call itself");
    let told = llm
        .recorded()
        .iter()
        .map(common::Recorded::user)
        .find(|user| user.contains("that your helper `scout` made"))
        .unwrap();
    assert!(
        told.contains("The approval holds for you and your helpers: if the call is still needed, make it again exactly as before (any other call is reviewed as usual), yourself or through a helper, and go on."),
        "{told}"
    );
    rig.quit().await;
}

/// The approval-events loop waits for no turn: with the agent's turn
/// running, an approval from `/permissions` and then another block of the gate; that
/// block's push reaches the client while the turn still runs. Before the
/// fix the loop awaited the approval's note, which the engine applies only
/// when the turn ends, and the push came after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_decision_after_an_approval_reaches_the_client_during_the_turn() {
    let ws = Workspace::new();
    let first = marker(&ws, "first");
    let second = marker(&ws, "second");
    let (gate, end) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let step = {
        let (gate, end, calls) = (Arc::clone(&gate), Arc::clone(&end), Arc::clone(&calls));
        let (first, second) = (first.clone(), second.clone());
        Step::Dynamic(Arc::new(move |_: &[Message]| {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => touch("t1", &first),
                1 => Step::Gated(Arc::clone(&gate), Box::new(touch("t2", &second))),
                2 => Step::Gated(Arc::clone(&end), Box::new(Step::Text("end"))),
                _ => Step::Text("end"),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    rig.send(Effect::StartTurn("touch both".into()));
    // The first call's block is pushed; the model call after it is held.
    rig.saw(|m| matches!(m, Msg::Approval(notice) if !notice.allowed))
        .await;
    common::until(|| llm.recorded().len() == 2).await;
    let id = rig.daemon.engine().blocks().list(&rig.daemon.session)[0].id;
    rig.send(Effect::PickedApproval(id, GrantScope::Once, None));
    rig.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("approved once")))
        .await;
    gate.notify_one();
    // The second call's block is pushed while the turn is held open.
    rig.wait_for(|m| matches!(m, Msg::Approval(notice) if !notice.allowed))
        .await;
    assert!(second.parent().is_some() && !second.exists());
    assert!(
        !rig.seen.iter().any(|m| matches!(m, Msg::TurnFinished(_))),
        "the turn still runs"
    );
    end.notify_one();
    rig.turn_finished().await.unwrap();
    rig.quit().await;
}

/// `rkat/set_mode` into plan mode while the agent's turn runs answers
/// before the turn ends (its model call is held), and the note reaches
/// that run: its next request carries it. Before the fix the answer waited
/// for the turn's end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mode_change_during_a_turn_answers_before_the_turn_ends() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if results(messages) == 0 {
                Step::Tool {
                    id: "r1",
                    name: "read_file",
                    args: json!({"path": "notes.txt"}),
                }
            } else if common::user_texts(messages)
                .concat()
                .contains("# Plan mode is on")
            {
                Step::Text("planning")
            } else {
                Step::Gated(Arc::clone(&gate), Box::new(Step::Text("not told")))
            }
        }))
    };
    std::fs::write(ws.root.join("notes.txt"), "notes\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup()).await;
    rig.send(Effect::StartTurn("read the notes".into()));
    common::until(|| llm.recorded().len() == 2).await;
    let connection = rig.daemon.connect_bound().await;
    let answer = within(connection.call("rkat/set_mode", json!({"mode": "plan"})))
        .await
        .unwrap();
    assert_ne!(answer["ok"], false, "{answer}");
    assert!(
        !rig.seen.iter().any(|m| matches!(m, Msg::TurnFinished(_))),
        "answered while the turn runs"
    );
    gate.notify_one();
    assert_eq!(rig.turn_finished().await.unwrap().text, "planning");
    rig.quit().await;
}
