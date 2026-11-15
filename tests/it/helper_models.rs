//! Which models helpers run on: `helpers.model` (the default when a spawn
//! names none) and `helpers.allow_models` / `deny_models`, applied by the
//! spawn customizer to a `delegate` helper on the real daemon with the
//! scripted LLM. The script is keyed on the messages, never on the order
//! of the calls (the agent and its helper both call the model).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{MODEL, ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Attached, Setup};
use rkat_tui::ui::{Effect, Msg, StatusUpdate};
use serde_json::{Value, json};

const HAIKU: &str = "claude-haiku-4-5";
const OPUS: &str = "claude-opus-4-8";
/// What every helper's task says, so its model calls are told apart.
const TASK: &str = "HELPER-TASK: look around";

/// The agent delegates (with `tooling`, when given) when its last message
/// is the user's, and ends with "agent done" once it has the result; the
/// helper answers "helper done", once `gate` (when given) is notified.
fn script(
    tooling: Option<Value>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::user_texts(messages).concat().contains(TASK) {
            let done = Step::Text("helper done");
            return match &gate {
                Some(gate) => Step::Gated(Arc::clone(gate), Box::new(done)),
                None => done,
            };
        }
        let answered = messages
            .iter()
            .rev()
            .find(|m| !matches!(m, Message::System(_)))
            .is_some_and(|m| matches!(m, Message::ToolResults { .. }));
        if answered {
            return Step::Text("agent done");
        }
        let turn = messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count();
        let mut args = json!({
            "member_id": format!("scout{turn}"),
            "task": TASK,
            "result_label": "result",
            "max_text_bytes": 4000,
        });
        if let Some(tooling) = &tooling {
            args["tooling"] = tooling.clone();
        }
        Step::Tool {
            id: common::leak(format!("d{turn}")),
            name: "delegate",
            args,
        }
    }));
    ScriptedLlmClient::new(vec![step; 30])
}

/// An inline profile naming `model`.
fn inline(model: &str) -> Value {
    json!({"mode": "profile", "source": {"type": "inline", "model": model, "tools": {"builtins": true, "comms": true}}})
}

/// The models the helper's calls ran on.
fn helper_models(llm: &ScriptedLlmClient) -> Vec<String> {
    llm.recorded()
        .into_iter()
        .filter(|request| request.user().contains(TASK))
        .map(|request| request.model)
        .collect()
}

/// The text of the agent's last `delegate` result.
fn delegate_result(llm: &ScriptedLlmClient, id: &str) -> String {
    let last = llm
        .recorded()
        .into_iter()
        .rfind(|request| !request.user().contains(TASK))
        .unwrap();
    common::tool_text(&last.messages, id)
}

async fn one_turn(rig: &mut Attached) {
    rig.send(Effect::StartTurn("USER-ASK delegate it".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "agent done");
}

/// A `delegate` helper that names no model runs on `helpers.model`, and
/// the client's helper view shows it; the agent's instructions name it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_naming_no_model_runs_on_helpers_model() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = script(None, Some(Arc::clone(&gate)));
    let setup = Setup::args(&["--set", &format!("helpers.model={HAIKU}")]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.send(Effect::StartTurn("USER-ASK delegate it".into()));
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::Helpers(h)) if h.len() == 1 && h[0].model == HAIKU))
        .await;
    gate.notify_one();
    assert_eq!(rig.turn_finished().await.unwrap().text, "agent done");
    assert_eq!(helper_models(&llm), [HAIKU]);
    let first = &llm.recorded()[0];
    assert_eq!(first.model, MODEL, "the agent keeps its own");
    assert!(
        first
            .system()
            .contains(&format!("a helper that names no model runs on `{HAIKU}`")),
        "{}",
        first.system()
    );
    rig.quit().await;
}

/// No setting: the agent's instructions say nothing of helper models.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_settings_the_agent_is_told_nothing() {
    let ws = Workspace::new();
    let llm = script(None, None);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    one_turn(&mut rig).await;
    assert_eq!(helper_models(&llm), [MODEL]);
    assert!(!llm.recorded()[0].system().contains("Helper models"));
    rig.quit().await;
}

/// An allow list refuses a model the spawn names outside it; the agent's
/// tool result says which models it may name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allow_list_refuses_a_named_model_outside_it() {
    let ws = Workspace::new();
    let llm = script(Some(inline(OPUS)), None);
    let setup = Setup::args(&[
        "--set",
        &format!(r#"helpers.allow_models=["claude-haiku-*", "{MODEL}"]"#),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    one_turn(&mut rig).await;
    assert!(helper_models(&llm).is_empty(), "no helper ran");
    let result = delegate_result(&llm, "d1");
    assert!(
        result.contains(&format!(
            "the model `{OPUS}` is not permitted for helpers by the user's helpers.allow_models = [claude-haiku-*, {MODEL}]; name one of: claude-haiku-*, {MODEL}"
        )),
        "{result}"
    );
    rig.quit().await;
}

/// A deny list refuses a listed model; a model it does not list runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deny_list_refuses_a_listed_model() {
    let ws = Workspace::new();
    let llm = script(Some(inline(OPUS)), None);
    let setup = Setup::args(&["--set", r#"helpers.deny_models=["claude-opus-*"]"#]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    one_turn(&mut rig).await;
    assert!(helper_models(&llm).is_empty(), "no helper ran");
    let result = delegate_result(&llm, "d1");
    assert!(
        result.contains(&format!("the model `{OPUS}` is not permitted"))
            && result.contains("name a model other than claude-opus-*"),
        "{result}"
    );
    rig.quit().await;
}

/// The same deny list lets an inline profile's model it does not list
/// run (the profile's model is the helper's).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deny_list_lets_another_named_model_run() {
    let ws = Workspace::new();
    let llm = script(Some(inline(HAIKU)), None);
    let setup = Setup::args(&["--set", r#"helpers.deny_models=["claude-opus-*"]"#]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    one_turn(&mut rig).await;
    assert_eq!(helper_models(&llm), [HAIKU]);
    rig.quit().await;
}

/// An allow list that leaves out the owner's model, with no
/// `helpers.model`: a spawn naming no model is refused, telling the agent
/// to name a permitted one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unnamed_spawn_on_a_refused_owner_model_must_name_one() {
    let ws = Workspace::new();
    let llm = script(None, None);
    let setup = Setup::args(&["--set", r#"helpers.allow_models=["claude-haiku-*"]"#]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    one_turn(&mut rig).await;
    assert!(helper_models(&llm).is_empty(), "no helper ran");
    let result = delegate_result(&llm, "d1");
    assert!(
        result.contains(&format!(
            "this spawn names no model, so the helper would run on your own model `{MODEL}`"
        )) && result.contains("the call must name a permitted model: name one of: claude-haiku-*"),
        "{result}"
    );
    rig.quit().await;
}

/// A live `/set helpers.model` takes effect for the next helper, and the
/// agent is told.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_set_of_helpers_model_reaches_the_next_helper() {
    let ws = Workspace::new();
    let llm = script(None, None);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    one_turn(&mut rig).await;
    assert_eq!(helper_models(&llm), [MODEL]);
    let set = rig
        .daemon
        .connect_bound()
        .await
        .call(
            "rkat/set_setting",
            json!({"key": "helpers.model", "value": HAIKU, "user": false}),
        )
        .await
        .unwrap();
    assert_eq!(set["ok"], true, "{set}");
    assert!(
        set["text"]
            .as_str()
            .unwrap()
            .contains("the helpers spawned from now on take it"),
        "{set}"
    );
    one_turn(&mut rig).await;
    assert_eq!(helper_models(&llm), [MODEL, HAIKU]);
    // The agent's next request carries the note.
    let told = llm
        .recorded()
        .into_iter()
        .filter(|request| !request.user().contains(TASK))
        .any(|request| {
            format!("{:?}", request.messages)
                .contains(&format!("a helper that names no model runs on `{HAIKU}`"))
        });
    assert!(told, "the agent was told");
    rig.quit().await;
}
