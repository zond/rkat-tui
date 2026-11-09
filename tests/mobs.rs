//! Helpers (mob members) end to end in the daemon, with a scripted LLM: a
//! `delegate` call spawns a helper that runs one turn and returns; the
//! helper's calls pass the orchestrator's approval gate (a read runs, a
//! Tier-3 call is denied with the reason the helper reads, judged against
//! the user's words and counted for the session); its events reach the
//! attached UI nested and named, its usage lands in the cost breakdown and
//! the sidecar; a configured endpoint's session hands its model and binding
//! on; the depth limit holds; a mob nobody owns gets no helper; and a
//! helper's owner survives a daemon restart until it retires.
//! (`tests/attach_helpers.rs` has the follower view, Esc and the model
//! switch.)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_with, within};
use meerkat_core::types::ToolResult;
use meerkat_core::{Message, StreamScopeFrame};
use rig::{Attached, Running, ScriptedClassifier, Setup};
use rkat_tui::approval::classifier::{ActionClassifier, ClassifierVerdict};
use rkat_tui::bootstrap;
use rkat_tui::endpoints::{ENDPOINT_REALM, Endpoint};
use rkat_tui::models::{EndpointListing, ModelEntry, ModelLister, ModelSource};
use rkat_tui::ui::{App, ApprovalNotice, Effect, Msg, StatusInfo, StatusUpdate};
use rkat_tui::workbench::cost_store::load_cost;
use serde_json::{Value, json};

fn delegate(id: &'static str, member: &str, task: &str) -> Step {
    Step::Tool {
        id,
        name: "delegate",
        args: json!({
            "member_id": member,
            "task": task,
            "result_label": "result",
            "max_text_bytes": 4000,
        }),
    }
}

fn tool_result(messages: &[Message], id: &str) -> ToolResult {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => {
                results.iter().find(|r| r.tool_use_id == id).cloned()
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn no_helpers(msg: &Msg) -> bool {
    matches!(msg, Msg::Status(StatusUpdate::Helpers(h)) if h.is_empty())
}

/// The helper label of a scoped message.
fn helper_of(msg: &Msg) -> Option<(String, usize)> {
    let Msg::Scoped(ev) = msg else { return None };
    let frames: Vec<&str> = ev
        .scope_path
        .iter()
        .filter_map(|f| match f {
            StreamScopeFrame::MobMember { agent_identity, .. } => Some(agent_identity.as_str()),
            _ => None,
        })
        .collect();
    frames
        .last()
        .map(|label| ((*label).to_string(), frames.len()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegated_helper_runs_behind_the_sessions_gate() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "HELPER-READ-CONTENT\n").unwrap();
    let marker = ws.sibling("pwned");
    let command = leak(format!("touch {}", marker.display()));
    let classifier = ScriptedClassifier::new(vec![Ok(ClassifierVerdict::Block {
        rule: Some("D1".into()),
        reason: "touches files outside the project".into(),
    })]);
    let llm = ScriptedLlmClient::new(vec![
        delegate(
            "d1",
            "scout",
            "HELPER-TASK: read notes.txt and touch the marker",
        ),
        Step::Tools(vec![
            ("h1", "read_file", json!({"path": "notes.txt"})),
            ("h2", "shell", json!({ "command": command })),
        ]),
        Step::Text("HELPER-DONE: read it, the touch was blocked"),
        Step::Text("parent done"),
    ]);
    let realm = bootstrap::resolve_scope(&args_with(&ws.root, &[]))
        .unwrap()
        .realm_root();
    let setup = Setup::default().classifier(Arc::clone(&classifier) as Arc<dyn ActionClassifier>);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    let id = rig.current_session().await;
    rig.send(Effect::StartTurn("USER-ASK look into the notes".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "parent done");

    let recorded = llm.recorded();
    assert_eq!(recorded.len(), 4, "parent, helper ×2, parent");
    // The helper ran on the session's model, with the helper instructions
    // and the TUI's file tools, and its task as its prompt.
    assert_eq!(recorded[1].model, common::MODEL);
    assert!(recorded[1].system().contains("Working as a helper"));
    assert!(recorded[1].tools.iter().any(|t| t == "read_file"));
    assert!(recorded[1].user().contains("HELPER-TASK"));
    // The read passed the gate; the Tier-3 shell call was denied, and the
    // helper read why.
    let read = tool_result(&recorded[2].messages, "h1");
    assert!(!read.is_error, "{}", read.text_content());
    assert!(read.text_content().contains("HELPER-READ-CONTENT"));
    let denied = tool_result(&recorded[2].messages, "h2");
    assert!(denied.is_error);
    assert!(
        denied
            .text_content()
            .contains("Blocked by auto-mode safety policy: touches files outside the project"),
        "{}",
        denied.text_content()
    );
    assert!(!marker.exists(), "the denied command never ran");
    // Judged against the user's words, never the helper's task.
    let seen = classifier.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "shell");
    assert!(seen[0].2.contains("USER-ASK"), "{}", seen[0].2);
    // The helper's task is the parent's action, never the user's words.
    assert!(
        seen[0]
            .2
            .lines()
            .filter(|line| line.starts_with("[user]"))
            .all(|line| !line.contains("HELPER-TASK")),
        "{}",
        seen[0].2
    );
    // Counted for the session, and named as the helper's.
    let (counters, _) = rig.daemon.engine().auto_state();
    assert_eq!(counters.total, 1);
    rig.saw(|m| {
        matches!(m, Msg::Approval(ApprovalNotice { allowed: false, tool, .. }) if tool == "[scout] shell")
    })
    .await;
    // The parent got the helper's result.
    let result = tool_result(&recorded[3].messages, "d1");
    assert!(!result.is_error, "{}", result.text_content());
    assert!(result.text_content().contains("HELPER-DONE"));

    // The UI heard the helper: running (then not), its events nested one
    // level deep and named.
    rig.saw(no_helpers).await;
    let seen_msgs = rig.seen.clone();
    assert!(seen_msgs.iter().any(|m| matches!(
        m,
        Msg::Status(StatusUpdate::Helpers(h))
            if h.len() == 1 && h[0].label == "scout" && h[0].model == common::MODEL
    )));
    assert!(
        seen_msgs
            .iter()
            .filter_map(helper_of)
            .any(|(label, depth)| label == "scout" && depth == 1),
        "no scoped helper events"
    );

    // Its usage reaches the cost breakdown and the sidecar.
    let mut app = App::new(StatusInfo::default());
    let mut snapshots = Vec::new();
    for msg in seen_msgs {
        if matches!(msg, Msg::Scoped(_) | Msg::Status(StatusUpdate::Helpers(_))) {
            snapshots.extend(
                app.update(msg)
                    .into_iter()
                    .filter(|e| matches!(e, Effect::CostSnapshot(_))),
            );
        }
    }
    let helper = &app.status.cost().helpers["scout"][common::MODEL];
    assert_eq!((helper.input, helper.output), (20, 10), "two helper calls");
    let breakdown = app.status.cost().breakdown(app.status.prices());
    assert!(
        breakdown.contains("scout (claude-sonnet-4-6): in 20"),
        "{breakdown}"
    );
    let last = snapshots.pop().expect("a cost snapshot");
    rig.send(last);
    rig.quit().await;
    let stored = load_cost(&realm, &id.to_string()).expect("sidecar");
    assert_eq!(stored.helpers["scout"][common::MODEL].input, 20);
}

/// The lister of an Anthropic-dialect gateway `proxy` listing `model`.
fn proxy_lister(model: &'static str) -> ModelLister {
    Arc::new(move |endpoints: Vec<Endpoint>| {
        Box::pin(async move {
            endpoints
                .into_iter()
                .map(|e| {
                    let models = if e.name == "proxy" {
                        vec![ModelEntry {
                            id: model.into(),
                            display_name: None,
                            source: ModelSource::Provider,
                            context_window: Some(50_000),
                            max_output_tokens: Some(4_096),
                        }]
                    } else {
                        Vec::new()
                    };
                    (
                        e.name,
                        Ok(EndpointListing {
                            models,
                            prices: Vec::new(),
                        }),
                    )
                })
                .collect()
        })
    })
}

/// A daemon on the gateway `proxy`, started on `proxy:<model>`.
fn proxy_setup(model: &'static str) -> Setup {
    let mut setup = Setup::args(&[
        "--set",
        "endpoints.proxy.dialect=anthropic",
        "--set",
        "endpoints.proxy.base_url=http://127.0.0.1:9",
        "--set",
        // Keys come from the environment only; `PATH` is set wherever
        // tests run, and the scripted client never sends it.
        "endpoints.proxy.api_key_env=PATH",
        "--model",
    ]);
    setup.args.push(format!("proxy:{model}"));
    setup.lister = Some(proxy_lister(model));
    setup
}

/// The helper session a `delegate` call `id` reports in `messages`.
fn helper_session(messages: &[Message], id: &str) -> meerkat_core::SessionId {
    let result = tool_result(messages, id);
    let value: Value = serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| panic!("delegate failed: {}", result.text_content()));
    meerkat_core::SessionId::parse(value["session_id"].as_str().unwrap()).unwrap()
}

/// Delegate once on `proxy:<model>`; the helper's stored metadata.
async fn delegate_on_the_proxy(model: &'static str) -> meerkat_core::SessionMetadata {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "scout", "say hi"),
        Step::Text("hi"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, proxy_setup(model)).await;
    rig.send(Effect::StartTurn("delegate".into()));
    rig.turn_finished().await;
    let recorded = llm.recorded();
    assert_eq!(recorded[0].model, model);
    assert_eq!(
        recorded.len(),
        3,
        "{}",
        tool_result(&recorded[1].messages, "d1").text_content()
    );
    assert_eq!(
        recorded[1].model, model,
        "the helper runs on the session's model"
    );
    let helper = helper_session(&recorded[2].messages, "d1");
    rig.quit().await;
    rig::stored_session(&ws.root, &helper)
        .await
        .session_metadata()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configured_endpoints_helper_uses_the_same_binding() {
    let meta = delegate_on_the_proxy(common::MODEL).await;
    assert_eq!(meta.model, common::MODEL);
    let binding = meta.auth_binding.expect("the proxy's binding");
    assert_eq!(binding.realm.as_str(), ENDPOINT_REALM);
    assert_eq!(binding.binding.as_str(), "proxy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_on_an_endpoint_only_model_delegates_on_it() {
    // A model id only the gateway lists.
    const ENDPOINT_MODEL: &str = "my-alias";
    let meta = delegate_on_the_proxy(ENDPOINT_MODEL).await;
    assert_eq!(meta.model, ENDPOINT_MODEL);
    let binding = meta.auth_binding.expect("the proxy's binding");
    assert_eq!(binding.realm.as_str(), ENDPOINT_REALM);
    assert_eq!(binding.binding.as_str(), "proxy");
}

/// A definition with one durable inline `delegate` profile, under `id`.
fn team(id: &str) -> meerkat_mob::MobDefinition {
    let mut definition = meerkat_mob::MobDefinition::implicit("unused", common::MODEL);
    definition.id = meerkat_mob::MobId::from(id);
    // Durable members are rebuilt on their sessions when the mob is restored.
    for profile in definition.profiles.values_mut() {
        if let meerkat_mob::ProfileBinding::Inline(profile) = profile {
            profile.durable = true;
        }
    }
    definition
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_no_open_session_owns_is_refused() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("never")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let id = daemon.session.clone();
    // Named as the orchestrator's implicit mob, but created by the host:
    // its helpers work for nobody, and never for the orchestrator.
    let definition = meerkat_mob::MobDefinition::implicit(&id.to_string(), common::MODEL);
    let mob = within(daemon.state().mob_create_definition(definition))
        .await
        .unwrap();
    let handle = within(daemon.state().handle_for(&mob)).await.unwrap();
    let refused =
        within(handle.spawn_spec(meerkat_mob::SpawnMemberSpec::new("delegate", "stray"))).await;
    let Err(err) = refused else {
        panic!("the spawn is refused");
    };
    assert!(err.to_string().contains("helper refused"), "{err}");
    assert!(daemon.registry().live().is_empty());
    assert!(daemon.registry().live_for(&id).is_empty());
    let members = within(handle.list_members()).await;
    assert!(members.is_empty(), "members: {members:?}");
    assert!(llm.recorded().is_empty(), "no member ran");
    drop(handle);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helpers_owner_survives_a_restart_until_it_retires() {
    use rkat_tui::mobs::HelperKey;

    let ws = Workspace::new();
    let mob = meerkat_mob::MobId::from("team");
    let key = HelperKey::new(mob.clone(), meerkat_mob::AgentIdentity::from("worker"));
    let llm = ScriptedLlmClient::new(vec![Step::Text("idle"), Step::Text("idle")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let owner = daemon.session.clone();
    within(
        daemon
            .state()
            .mob_create_definition_with_owner_bridge_session(
                team("team"),
                owner.clone(),
                false,
                false,
            ),
    )
    .await
    .unwrap();
    let handle = within(daemon.state().handle_for(&mob)).await.unwrap();
    // It runs a turn, so its durable session exists.
    let mut spec = meerkat_mob::SpawnMemberSpec::new("delegate", "worker");
    spec.initial_message = Some("start".into());
    within(handle.spawn_spec(spec)).await.unwrap();
    assert_eq!(daemon.registry().live_for(&owner), vec![key.clone()]);
    llm.wait_call(0).await;
    // The daemon stops: its mob shuts down (not retiring anyone).
    drop(handle);
    daemon.stop().await;

    // A new daemon over the same realm: the restored member is gated for
    // the session that spawned it.
    let llm = ScriptedLlmClient::new(vec![Step::Text("idle"), Step::Text("idle")]);
    let daemon = Running::start(&ws.root, &llm).await;
    assert!(!daemon.created, "the orchestrator is restored");
    let registry = daemon.registry();
    assert_eq!(registry.owner_for_resume(&key).unwrap().owner, owner);
    let handle = within(daemon.state().handle_for(&mob)).await.unwrap();
    // The restored member is dormant until work wakes it, which rebuilds it
    // on its durable session through `customize_resume`.
    let member = within(handle.member(&key.member)).await.unwrap();
    within(member.internal_turn("wake up")).await.unwrap();
    within(async {
        while registry.live_for(&owner).is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(registry.live_for(&owner), vec![key.clone()]);
    assert_eq!(registry.owner_for_resume(&key).unwrap().owner, owner);

    // Forgetting the owner prunes its helpers here (twice is a no-op); the
    // binding stays on disk for a later resume.
    daemon.engine().forget_session(&owner);
    daemon.engine().forget_session(&owner);
    assert!(registry.live_for(&owner).is_empty());
    assert!(registry.live().is_empty());
    assert_eq!(registry.owner_for_resume(&key).unwrap().owner, owner);

    // Retiring drops the binding for good.
    within(handle.retire(key.member.clone())).await.unwrap();
    within(async {
        while registry.owner_for_resume(&key).is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    let file = std::fs::read_to_string(
        bootstrap::resolve_scope(&args_with(&ws.root, &[]))
            .unwrap()
            .realm_root()
            .join(rkat_tui::mobs::bindings::BINDINGS_FILE),
    )
    .unwrap();
    assert!(!file.contains("worker"), "{file}");
    drop(handle);
    drop(registry);
    daemon.stop().await;
}

/// The tool definitions of `recorded` that start helpers.
fn nesting_tools(recorded: &common::Recorded) -> Vec<&str> {
    recorded
        .tools
        .iter()
        .map(String::as_str)
        .filter(|tool| {
            matches!(
                *tool,
                "delegate" | "council" | "fork_off" | "spawn_member" | "spawn_many_members"
            ) || tool.starts_with("mob_")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_at_the_default_depth_cannot_delegate() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "scout", "HELPER-TASK: delegate onwards"),
        delegate("d2", "inner", "NESTED-TASK"),
        Step::Text("HELPER-DONE: did it myself"),
        Step::Text("parent done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("USER-ASK delegate it".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "parent done");
    let recorded = llm.recorded();
    assert_eq!(
        recorded.len(),
        4,
        "parent, helper ×2, parent; no nested helper"
    );
    // The orchestrator may delegate; its helper, at depth 1 of 1, is
    // offered no tool that starts helpers, and its call is refused.
    assert!(recorded[0].tools.iter().any(|t| t == "delegate"));
    assert!(recorded[1].user().contains("HELPER-TASK"));
    assert!(
        nesting_tools(&recorded[1]).is_empty(),
        "{:?}",
        recorded[1].tools
    );
    assert!(recorded[1].system().contains("cannot delegate further"));
    // Its other tools stay: only the mob category is off.
    for tool in ["shell", "read_file"] {
        assert!(recorded[1].tools.iter().any(|t| t == tool), "{tool}");
    }
    let refused = tool_result(&recorded[2].messages, "d2");
    assert!(refused.is_error, "{}", refused.text_content());
    assert!(
        recorded.iter().all(|r| !r.user().contains("NESTED-TASK")),
        "no nested helper ran"
    );
    let result = tool_result(&recorded[3].messages, "d1");
    assert!(result.text_content().contains("HELPER-DONE"));
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_below_a_raised_depth_limit_delegates_once_more() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "scout", "HELPER-TASK: delegate onwards"),
        delegate("d2", "inner", "NESTED-TASK"),
        Step::Text("NESTED-DONE"),
        Step::Text("HELPER-DONE"),
        Step::Text("parent done"),
    ]);
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.send(Effect::StartTurn("USER-ASK delegate it twice".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "parent done");
    let recorded = llm.recorded();
    assert_eq!(recorded.len(), 5, "parent, helper, nested, helper, parent");
    assert!(recorded[1].tools.iter().any(|t| t == "delegate"));
    // The nested helper is at the limit: no tool that starts helpers.
    assert!(recorded[2].user().contains("NESTED-TASK"));
    assert!(
        nesting_tools(&recorded[2]).is_empty(),
        "{:?}",
        recorded[2].tools
    );
    let nested = tool_result(&recorded[3].messages, "d2");
    assert!(!nested.is_error, "{}", nested.text_content());
    assert!(nested.text_content().contains("NESTED-DONE"));
    let result = tool_result(&recorded[4].messages, "d1");
    assert!(result.text_content().contains("HELPER-DONE"));
    rig.quit().await;
}
