//! Which models helpers run on: the model a spawn names, else
//! `helpers.model`, else the agent's own, never a stored mob profile's;
//! `helpers.allow_models` / `deny_models` judge it. Applied by the spawn
//! customizer to `delegate` helpers and to members seated by a profile's
//! name, on the real daemon with the scripted LLM. The script is keyed on
//! the messages, never on the order of the calls (the agent and its helper
//! both call the model).

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
            .contains(&format!("one that names none runs on `{HAIKU}`")),
        "{}",
        first.system()
    );
    rig.quit().await;
}

/// No setting: the agent's instructions say only where a helper's model
/// comes from, in one sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_settings_the_agent_is_told_one_sentence() {
    let ws = Workspace::new();
    let llm = script(None, None);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    one_turn(&mut rig).await;
    assert_eq!(helper_models(&llm), [MODEL]);
    let system = llm.recorded()[0].system();
    assert!(
        system.contains("Helper models: a helper runs on the model its spawn names (a mob profile's own model is ignored), else on your own.")
            && !system.contains("the user's settings"),
        "{system}"
    );
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

/// The agent makes the tool calls `calls` in turn (one per model call,
/// each after the previous one's result) and ends with "agent done"; a
/// member whose messages carry [`TASK`] answers "helper done", once `gate`
/// (when given) is notified.
fn calls_gated(
    calls: Vec<(&'static str, &'static str, Value)>,
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
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count();
        match calls.get(answered) {
            Some(&(id, name, ref args)) => Step::Tool {
                id,
                name,
                args: args.clone(),
            },
            None => Step::Text("agent done"),
        }
    }));
    ScriptedLlmClient::new(vec![step; 30])
}

/// [`calls_gated`] with no gate.
fn calls(calls: Vec<(&'static str, &'static str, Value)>) -> Arc<ScriptedLlmClient> {
    calls_gated(calls, None)
}

/// The agent's mob tools the deferral keeps out of its requests until it
/// searches for them.
const MOB_TOOLS: [&str; 2] = ["mob_create", "mob_spawn_member"];

/// `mob_create` of the mob `team` whose profile `worker` names `model`.
fn create_team(model: &str) -> (&'static str, &'static str, Value) {
    (
        "m1",
        "mob_create",
        json!({"definition": {"id": "team", "profiles": {"worker": {"model": model, "tools": {"builtins": true, "comms": true}}}}}),
    )
}

/// `mob_spawn_member` of `w1` with the profile `worker` of `team` (with
/// `tooling`, when given), its first turn the helper's task.
fn spawn_worker(tooling: Option<Value>) -> (&'static str, &'static str, Value) {
    let mut args =
        json!({"mob_id": "team", "profile": "worker", "member_id": "w1", "initial_message": TASK});
    if let Some(tooling) = tooling {
        args["tooling"] = tooling;
    }
    ("s1", "mob_spawn_member", args)
}

/// Run one turn of `llm`'s calls on a daemon with `setup`, the mob tools
/// revealed; the models of the helper's calls once it made `helpers` of
/// them, and the text of the spawn's result.
async fn spawn_in_a_team(
    llm: &Arc<ScriptedLlmClient>,
    setup: Setup,
    helpers: usize,
) -> (Vec<String>, String) {
    let ws = Workspace::new();
    let mut rig = Attached::start_with(&ws.root, llm, setup).await;
    rig.daemon.reveal(&rig.daemon.session, &MOB_TOOLS).await;
    one_turn(&mut rig).await;
    let ran = common::poll(async || {
        let ran = helper_models(llm);
        (ran.len() >= helpers).then_some(ran)
    })
    .await;
    let result = delegate_result(llm, "s1");
    rig.quit().await;
    (ran, result)
}

/// The bug from real use: the workspace's mob keeps the `main` profile it
/// was created with, on the model of that first start (the engine cannot
/// update a stored definition, lukacf/meerkat#2184). After a restart on
/// another model, a helper the agent seats with `profile: "main"` and no
/// model ran on the first start's model; it runs on the agent's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_of_the_stored_main_profile_runs_on_the_agent_s_model() {
    let ws = Workspace::new();
    // The first start creates the mob, its `main` on MODEL.
    let first = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let mut rig = Attached::start_with(&ws.root, &first, Setup::default()).await;
    rig.send(Effect::StartTurn("first".into()));
    let _ = rig.turn_finished().await;
    rig.quit().await;

    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = calls_gated(
        vec![(
            "s1",
            "spawn_member",
            json!({"profile": "main", "member_id": "scout", "initial_message": TASK}),
        )],
        Some(Arc::clone(&gate)),
    );
    let setup = Setup::args(&["--model", &format!("anthropic:{OPUS}")]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.send(Effect::SetModel(format!("anthropic:{OPUS}")));
    rig.wait_for(|m| {
        matches!(m, Msg::Status(StatusUpdate::Model { model, .. }) if model == OPUS)
            || matches!(m, Msg::Notice { text, .. } if text.starts_with("model not changed"))
    })
    .await;
    rig.send(Effect::StartTurn("USER-ASK seat one".into()));
    // The helper's model call is made (and held): the model it ran on.
    let ran = common::poll(async || {
        let ran = helper_models(&llm);
        (!ran.is_empty()).then_some(ran)
    })
    .await;
    assert_eq!(ran, [OPUS]);
    // The client's helper view names it, and never the start-up model.
    // `spawn_member` does not hold the agent's turn, so its end may show
    // before the view: both are waited for as seen or arriving.
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::Helpers(h)) if h.len() == 1 && h[0].model == OPUS))
        .await;
    let shown: Vec<String> = rig
        .seen
        .iter()
        .filter_map(|m| match m {
            Msg::Status(StatusUpdate::Helpers(h)) => Some(h.iter().map(|h| h.model.clone())),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(shown.iter().all(|model| model == OPUS), "{shown:?}");
    gate.notify_one();
    let finished = rig.saw(|m| matches!(m, Msg::TurnFinished(_))).await;
    assert!(
        matches!(&finished, Msg::TurnFinished(Ok(summary)) if summary.text == "agent done"),
        "{finished:?}"
    );
    rig.quit().await;
}

/// A `fork_off` child continues its parent's conversation on the parent's
/// model as it is now, after a `/model` switch, and `helpers.model` does
/// not move it (the engine copies the agent's spawn-time profile to the
/// child, whose model is the start-up one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_runs_on_its_parent_s_current_model() {
    let ws = Workspace::new();
    let llm = calls(vec![(
        "f1",
        "fork_off",
        json!({"member_id": "kid", "task": TASK}),
    )]);
    let setup = Setup::args(&["--set", &format!("helpers.model={HAIKU}")]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.daemon.reveal(&rig.daemon.session, &["fork_off"]).await;
    rig.send(Effect::SetModel(format!("anthropic:{OPUS}")));
    rig.wait_for(|m| matches!(m, Msg::Status(StatusUpdate::Model { model, .. }) if model == OPUS))
        .await;
    one_turn(&mut rig).await;
    // The call's result names the child, or why there is none.
    let forked = delegate_result(&llm, "f1");
    assert!(!forked.contains("\"error\""), "{forked}");
    let ran = common::poll(async || {
        let ran = helper_models(&llm);
        (!ran.is_empty()).then_some(ran)
    })
    .await;
    assert_eq!(ran, [OPUS], "{forked}");
    // The child's result reaches the agent as a run of its own once the
    // child ends; a daemon stop while the engine delivers it can stall for
    // its bounds (an engine race, `docs/evidence.md`, "A stop while a
    // fork's result is delivered"), which is not what this test is about:
    // the stop waits until that run ended.
    common::poll(async || {
        llm.recorded()
            .iter()
            .any(|request| {
                !request.user().contains(TASK)
                    && format!("{:?}", request.messages.last()).contains("finished (completed)")
            })
            .then_some(())
    })
    .await;
    rig.daemon.idle(&rig.daemon.agent).await;
    rig.quit().await;
}

/// A member of a mob the agent created, seated by its profile's name with
/// no model, runs on the agent's model, not the one the profile names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_of_the_agent_s_mob_runs_on_the_agent_s_model_not_its_profile_s() {
    let llm = calls(vec![create_team(HAIKU), spawn_worker(None)]);
    let (ran, result) = spawn_in_a_team(&llm, Setup::default(), 1).await;
    assert_eq!(ran, [MODEL], "{result}");
}

/// A spawn by a profile's name right after the mob's create, before the
/// daemon's follower of the mobs' profiles read the new mob (here it never
/// does): the customizer reads the definition itself, and the member is
/// seated on the chosen model, never refused as not known yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_spawned_right_after_its_mob_s_create_is_seated_on_the_chosen_model() {
    let llm = calls(vec![create_team(HAIKU), spawn_worker(None)]);
    let setup = Setup {
        profiles_unfollowed: true,
        ..Setup::args(&["--set", &format!("helpers.model={OPUS}")])
    };
    let (ran, result) = spawn_in_a_team(&llm, setup, 1).await;
    assert_eq!(ran, [OPUS], "{result}");
    assert!(!result.contains("not known"), "{result}");
}

/// The same, with the create and the spawn as parallel calls of one
/// response: the engine dispatches them together (`join_all`), so the
/// spawn can reach the engine before the mob exists (the engine's own
/// answer then); once the mob exists, the member is seated on the chosen
/// model and is never refused as not known.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_spawned_in_the_same_batch_as_its_mob_s_create_is_seated_on_the_chosen_model() {
    let (create, spawn) = (create_team(HAIKU), spawn_worker(None));
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::user_texts(messages).concat().contains(TASK) {
            return Step::Text("helper done");
        }
        if messages
            .iter()
            .any(|m| matches!(m, Message::ToolResults { .. }))
        {
            return Step::Text("agent done");
        }
        Step::Tools(vec![create.clone(), spawn.clone()])
    }));
    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let setup = Setup {
        profiles_unfollowed: true,
        ..Setup::args(&["--set", &format!("helpers.model={OPUS}")])
    };
    let ws = Workspace::new();
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.daemon.reveal(&rig.daemon.session, &MOB_TOOLS).await;
    one_turn(&mut rig).await;
    let result = delegate_result(&llm, "s1");
    assert!(!result.contains("not known"), "{result}");
    if result.contains("mob not found: team") {
        // The engine's answer to a spawn that came first (seen on every
        // run so far): the customizer was never asked.
        assert!(helper_models(&llm).is_empty(), "{result}");
    } else {
        let ran = common::poll(async || {
            let ran = helper_models(&llm);
            (!ran.is_empty()).then_some(ran)
        })
        .await;
        assert_eq!(ran, [OPUS], "{result}");
    }
    rig.quit().await;
}

/// The same member with `helpers.model` set runs on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_of_the_agent_s_mob_runs_on_helpers_model() {
    let llm = calls(vec![create_team(HAIKU), spawn_worker(None)]);
    let setup = Setup::args(&["--set", &format!("helpers.model={OPUS}")]);
    let (ran, result) = spawn_in_a_team(&llm, setup, 1).await;
    assert_eq!(ran, [OPUS], "{result}");
}

/// With an allow list that leaves out the agent's model and no
/// `helpers.model`, the member is refused, though its profile names a
/// permitted model: the profile's model is not the member's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_of_the_agent_s_mob_is_refused_when_its_parent_s_model_is() {
    let llm = calls(vec![create_team(HAIKU), spawn_worker(None)]);
    let setup = Setup::args(&["--set", r#"helpers.allow_models=["claude-haiku-*"]"#]);
    let (ran, result) = spawn_in_a_team(&llm, setup, 0).await;
    assert!(ran.is_empty(), "no helper ran");
    assert!(
        result.contains(&format!(
            "this spawn names no model, so the helper would run on your own model `{MODEL}`"
        )) && result.contains("name one of: claude-haiku-*"),
        "{result}"
    );
}

/// A model the spawn names (here in an inline profile) wins over the
/// mob profile's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_the_spawn_names_wins_over_the_profile_s() {
    let llm = calls(vec![create_team(HAIKU), spawn_worker(Some(inline(OPUS)))]);
    let (ran, result) = spawn_in_a_team(&llm, Setup::default(), 1).await;
    assert_eq!(ran, [OPUS], "{result}");
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
                .contains(&format!("one that names none runs on `{HAIKU}`"))
        });
    assert!(told, "the agent was told");
    rig.quit().await;
}
