//! Helpers (mob members) end to end in the daemon, with a scripted LLM: an
//! `assign` call seats a helper that runs beside the agent and whose answer
//! reaches it as a note; the
//! helper's calls pass the agent's approval gate (a read runs, a
//! Tier-3 call is denied with the reason the helper reads, judged against
//! the user's words and counted for the session); its events reach the
//! attached UI nested and named, its usage lands in the cost breakdown and
//! the sidecar; a configured endpoint's session hands its model and binding
//! on; the depth limit holds; a mob nobody owns gets no helper; and a
//! helper's owner survives a daemon restart until it retires.
//! (`tests/helpers.rs` has the follower view, Esc and the model
//! switch.)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_with, leak, tool_result, within};
use meerkat_core::{Message, StreamScopeFrame};
use rig::{Attached, Running, ScriptedClassifier, Setup};
use rkat_tui::approval::classifier::{ActionClassifier, ClassifierVerdict};
use rkat_tui::bootstrap;
use rkat_tui::endpoints::{ENDPOINT_REALM, Endpoint};
use rkat_tui::models::{EndpointListing, ModelEntry, ModelLister, ModelSource};
use rkat_tui::ui::{ApprovalNotice, Effect, Msg, StatusUpdate};
use serde_json::json;

/// The agent's side of the helper tests: it assigns `task` to `name` (call
/// `a1`), ends its run with "waiting", and answers the note that the
/// helper finished with "parent done".
fn agent(messages: &[Message], name: &str, task: &str) -> Step {
    if common::told_finished(messages) {
        return Step::Text("parent done");
    }
    if common::after(messages, "a1") {
        return Step::Text("waiting");
    }
    common::assign("a1", name, task)
}

/// The requests of the helper whose task carries `marker`.
fn of_helper(llm: &ScriptedLlmClient, marker: &str) -> Vec<common::Recorded> {
    llm.recorded()
        .into_iter()
        .filter(|r| common::is_helper(&r.messages, marker))
        .collect()
}

/// The note the agent got when its helper `name` finished.
fn note_of(llm: &ScriptedLlmClient, name: &str) -> String {
    let note = format!("Your helper `{name}` finished its task");
    llm.recorded()
        .iter()
        .flat_map(|r| common::user_texts(&r.messages))
        .find(|text| text.contains(&note))
        .unwrap_or_default()
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
async fn an_assigned_helper_runs_behind_the_sessions_gate() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "HELPER-READ-CONTENT\n").unwrap();
    let marker = ws.sibling("pwned");
    let command = leak(format!("touch {}", marker.display()));
    let classifier = ScriptedClassifier::new(vec![Ok(ClassifierVerdict::Block {
        rule: Some("D1".into()),
        reason: "touches files outside the project".into(),
        annotation: None,
        path: None,
        effect: None,
    })]);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::is_helper(messages, "HELPER-TASK") {
            return if common::after(messages, "h1") {
                Step::Text("HELPER-DONE: read it, the touch was blocked")
            } else {
                Step::Tools(vec![
                    ("h1", "read_file", json!({"path": "notes.txt"})),
                    ("h2", "shell", json!({ "command": command })),
                ])
            };
        }
        agent(
            messages,
            "scout",
            "HELPER-TASK: read notes.txt and touch the marker",
        )
    }));
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let realm = bootstrap::resolve_scope(&args_with(&ws.root, &[]))
        .unwrap()
        .realm_root();
    let setup = Setup::default().classifier(Arc::clone(&classifier) as Arc<dyn ActionClassifier>);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    let id = rig.current_session().await;
    rig.send(Effect::StartTurn("USER-ASK look into the notes".into()));
    rig.turn_finished().await.expect("turn succeeds");
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;

    let helper = of_helper(&llm, "HELPER-TASK");
    assert_eq!(helper.len(), 2, "two calls of the helper");
    // The helper ran on the session's model, with the helper instructions
    // and the TUI's file tools, and its task as its prompt.
    assert_eq!(helper[0].model, common::MODEL);
    assert!(helper[0].system().contains("Working as a helper"));
    assert!(helper[0].tools.iter().any(|t| t == "read_file"));
    // The read passed the gate; the Tier-3 shell call was denied, and the
    // helper read why.
    let read = tool_result(&helper[1].messages, "h1");
    assert!(!read.is_error, "{}", read.text_content());
    assert!(read.text_content().contains("HELPER-READ-CONTENT"));
    let denied = tool_result(&helper[1].messages, "h2");
    assert!(denied.is_error);
    assert!(
        denied
            .text_content()
            .contains("Blocked by auto-mode safety policy: the safety classifier blocked it: touches files outside the project"),
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
    // The parent got the helper's answer, in the note that it finished.
    assert!(note_of(&llm, "scout").contains("HELPER-DONE"));

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

    // The daemon counted its usage to the conversation, under its label,
    // and pushed the totals; the breakdown names it.
    let scout = |cost: &rkat_tui::cost::SessionCost| {
        cost.helpers
            .get("scout")
            .and_then(|models| models.get(common::MODEL))
            .map(|totals| (totals.input, totals.output))
    };
    let pushed = |m: &Msg| match m {
        Msg::Status(StatusUpdate::CostTotals(cost)) if scout(cost) == Some((20, 10)) => {
            Some(cost.clone())
        }
        _ => None,
    };
    rig.saw(|m| pushed(m).is_some()).await;
    let cost = rig.seen.iter().find_map(pushed).expect("pushed");
    let breakdown = cost.breakdown(&rkat_tui::pricing::PriceTable::default());
    assert!(
        breakdown.contains("scout (claude-sonnet-4-6): in 20"),
        "{breakdown}"
    );
    // The daemon wrote it down by the time it stopped.
    rig.quit().await;
    let stored = rkat_tui::daemon::usage::read_record(
        &rkat_tui::daemon::usage::Account::Session(id.clone()).path(&realm),
    );
    assert_eq!(scout(&stored.cost), Some((20, 10)));
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
                            capabilities: Default::default(),
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

/// Start a helper once on `proxy:<model>`; its stored metadata.
async fn assign_on_the_proxy(model: &'static str) -> meerkat_core::SessionMetadata {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if common::is_helper(messages, "say hi") {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("hi")));
            }
            agent(messages, "scout", "say hi")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let mut rig = Attached::start_with(&ws.root, &llm, proxy_setup(model)).await;
    rig.send(Effect::StartTurn("start one".into()));
    let _ = rig.turn_finished().await;
    // The helper's session, while it runs (it is retired when done).
    let registry = rig.daemon.registry();
    let key = rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("tui"),
        meerkat_mob::AgentIdentity::from("scout"),
    );
    let helper = common::poll(async || registry.member_session(&key)).await;
    gate.notify_one();
    common::until(|| common::parent_told(&llm, "say hi", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    let recorded = llm.recorded();
    assert_eq!(recorded[0].model, model);
    let ran = of_helper(&llm, "say hi");
    assert_eq!(
        ran.len(),
        1,
        "{}",
        tool_result(&recorded[1].messages, "a1").text_content()
    );
    assert_eq!(
        ran[0].model, model,
        "the helper runs on the session's model"
    );
    drop(registry);
    rig.quit().await;
    rig::stored_session(&ws.root, &helper)
        .await
        .session_metadata()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configured_endpoints_helper_uses_the_same_binding() {
    let meta = assign_on_the_proxy(common::MODEL).await;
    assert_eq!(meta.model, common::MODEL);
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
    // Named as the agent's implicit mob, but created by the host:
    // its helpers work for nobody, and never for the agent.
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

/// The daemon follows a seated helper until the mob's own `MemberRetired`
/// says it left: while it idles it stays live (nothing else ends the
/// follow), and its retirement drops it and its binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followed_helper_stays_live_until_its_member_retired_arrives() {
    use rkat_tui::mobs::HelperKey;

    let ws = Workspace::new();
    let mob = meerkat_mob::MobId::from("team");
    let key = HelperKey::new(mob.clone(), meerkat_mob::AgentIdentity::from("worker"));
    let llm = ScriptedLlmClient::new(vec![Step::Text("idle")]);
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
    let mut spec = meerkat_mob::SpawnMemberSpec::new("delegate", "worker");
    spec.initial_message = Some("start".into());
    within(handle.spawn_spec(spec)).await.unwrap();
    llm.wait_call(0).await;
    let registry = daemon.registry();
    // Idle after its turn: still followed, still live. The sleep gives a
    // wrong retire the time to happen (an absence has no event to wait
    // on); the assertion is the condition.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(registry.live_for(&owner), vec![key.clone()]);
    within(handle.retire(key.member.clone())).await.unwrap();
    common::until(|| {
        registry.live_for(&owner).is_empty() && registry.owner_for_resume(&key).is_err()
    })
    .await;
    drop(handle);
    drop(registry);
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
    assert!(!daemon.created, "the agent is restored");
    let registry = daemon.registry();
    assert_eq!(registry.owner_for_resume(&key).unwrap().owner, owner);
    let handle = within(daemon.state().handle_for(&mob)).await.unwrap();
    // The restored member is dormant until work wakes it, which rebuilds it
    // on its durable session through `customize_resume`.
    let member = within(handle.member(&key.member)).await.unwrap();
    within(member.internal_turn("wake up")).await.unwrap();
    common::until(|| !registry.live_for(&owner).is_empty()).await;
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
    common::until(|| registry.owner_for_resume(&key).is_err()).await;
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
                "assign"
                    | "delegate"
                    | "council"
                    | "fork_off"
                    | "spawn_member"
                    | "spawn_many_members"
            ) || tool.starts_with("mob_")
        })
        .collect()
}

/// The script of the depth tests: the agent assigns `HELPER-TASK` to
/// `scout`; the helper assigns `NESTED-TASK` to `inner` (call `a2`), then
/// answers "HELPER-DONE" once the nested helper's note came (or at once
/// when its call failed); the nested helper answers "NESTED-DONE".
fn nesting() -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::is_helper(messages, "NESTED-TASK") {
            return Step::Text("NESTED-DONE");
        }
        if common::is_helper(messages, "HELPER-TASK") {
            if common::told_finished(messages) {
                return Step::Text("HELPER-DONE: with its answer");
            }
            return match messages.iter().rev().find_map(|m| match m {
                Message::ToolResults { results, .. } => Some(results.iter().any(|r| r.is_error)),
                _ => None,
            }) {
                None => common::assign("a2", "inner", "NESTED-TASK"),
                Some(true) => Step::Text("HELPER-DONE: did it myself"),
                Some(false) => Step::Text("waiting for inner"),
            };
        }
        agent(messages, "scout", "HELPER-TASK: start one of your own")
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_at_the_default_depth_cannot_start_helpers() {
    let ws = Workspace::new();
    let llm = nesting();
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("USER-ASK start a helper".into()));
    rig.turn_finished().await.expect("turn succeeds");
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    let recorded = llm.recorded();
    let helper = of_helper(&llm, "HELPER-TASK");
    assert_eq!(helper.len(), 2, "two calls of the helper; no nested helper");
    // The agent may start helpers; its helper, at depth 1 of 1, is
    // offered no tool that starts helpers, and its call is refused.
    assert!(recorded[0].tools.iter().any(|t| t == "assign"));
    assert!(
        nesting_tools(&helper[0]).is_empty(),
        "{:?}",
        helper[0].tools
    );
    assert!(helper[0].system().contains("cannot delegate further"));
    // Its other tools stay: only the mob category is off.
    for tool in ["shell", "read_file"] {
        assert!(helper[0].tools.iter().any(|t| t == tool), "{tool}");
    }
    let refused = tool_result(&helper[1].messages, "a2");
    assert!(refused.is_error, "{}", refused.text_content());
    assert!(
        of_helper(&llm, "NESTED-TASK").is_empty(),
        "no nested helper ran"
    );
    assert!(note_of(&llm, "scout").contains("HELPER-DONE: did it myself"));
    rig.quit().await;
}

/// Below a raised limit a helper starts one more with `assign`, which is
/// at the limit itself and runs; its note is told (to the helper, or to
/// the agent when the helper finished first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_below_a_raised_depth_limit_starts_one_more() {
    let ws = Workspace::new();
    let llm = nesting();
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.send(Effect::StartTurn(
        "USER-ASK start a helper that starts one".into(),
    ));
    rig.turn_finished().await.expect("turn succeeds");
    common::until(|| !note_of(&llm, "inner").is_empty()).await;
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    let helper = of_helper(&llm, "HELPER-TASK");
    assert!(helper[0].tools.iter().any(|t| t == "assign"));
    // The nested helper is at the limit: no tool that starts helpers.
    let nested = of_helper(&llm, "NESTED-TASK");
    assert_eq!(nested.len(), 1);
    assert!(
        nesting_tools(&nested[0]).is_empty(),
        "{:?}",
        nested[0].tools
    );
    let started = tool_result(&helper[1].messages, "a2");
    assert!(!started.is_error, "{}", started.text_content());
    assert!(note_of(&llm, "inner").contains("NESTED-DONE"));
    rig.quit().await;
}

/// Holds each named helper's going idle at its decision
/// (`HelperEndings::before_deciding`) until the test opens it: `reached`
/// says it got there, `open` lets it (and every later decision of it) go.
struct DecisionGates {
    gates: Vec<(
        &'static str,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Sender<bool>,
    )>,
}

impl DecisionGates {
    fn install(rig: &Attached, names: &[&'static str]) -> Arc<Self> {
        let gates = Arc::new(Self {
            gates: names
                .iter()
                .map(|name| {
                    (
                        *name,
                        tokio::sync::watch::channel(false).0,
                        tokio::sync::watch::channel(false).0,
                    )
                })
                .collect(),
        });
        let registry = rig.daemon.registry();
        let endings = registry.endings();
        endings.before_deciding(Self::hook(&gates, ""));
        endings.before_retiring(Self::hook(&gates, "retire "));
        gates
    }

    /// The hook that holds the helper at the gate named `prefix` and its
    /// name.
    fn hook(gates: &Arc<Self>, prefix: &'static str) -> rkat_tui::mobs::endings::DecidingHookFn {
        let gates = Arc::clone(gates);
        Arc::new(move |key: rkat_tui::mobs::HelperKey| {
            let gates = Arc::clone(&gates);
            Box::pin(async move {
                let label = format!("{prefix}{}", key.member.as_str());
                let Some((_, reached, open)) =
                    gates.gates.iter().find(|(name, _, _)| *name == label)
                else {
                    return;
                };
                reached.send_replace(true);
                let mut open = open.subscribe();
                let _ = open.wait_for(|open| *open).await;
            })
        })
    }

    fn gate(
        &self,
        name: &str,
    ) -> &(
        &'static str,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Sender<bool>,
    ) {
        self.gates.iter().find(|(n, _, _)| *n == name).unwrap()
    }

    async fn reached(&self, name: &str) {
        let mut reached = self.gate(name).1.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(30), reached.wait_for(|r| *r))
            .await
            .unwrap_or_else(|_| panic!("{name} never reached its decision"))
            .unwrap();
    }

    fn open(&self, name: &str) {
        self.gate(name).2.send_replace(true);
    }
}

/// How many user messages of the last request of `requests` tell that
/// the helper `name` finished.
fn notes_in_last(requests: &[common::Recorded], name: &str) -> usize {
    let note = format!("Your helper `{name}` finished its task");
    requests.last().map_or(0, |r| {
        common::user_texts(&r.messages)
            .iter()
            .filter(|text| text.contains(&note))
            .count()
    })
}

/// The agent's own requests (no helper's).
fn of_agent(llm: &ScriptedLlmClient) -> Vec<common::Recorded> {
    llm.recorded()
        .into_iter()
        .filter(|r| !common::is_helper(&r.messages, "TASK"))
        .collect()
}

/// The race behind a hosted CI failure: a nested `assign` helper's ending
/// reached while its parent helper's own ending is being decided. Here the
/// nested helper `inner` ends just before `scout` (its parent, itself an
/// `assign` helper whose run already ended) is decided: the note lands in
/// `scout` first, which goes on and reads it; the agent is not told of
/// `inner`, only of `scout`. Told once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nested_helper_s_note_that_lands_before_its_parent_is_decided_goes_to_the_parent() {
    let ws = Workspace::new();
    let llm = nesting();
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    let gates = DecisionGates::install(&rig, &["scout", "inner"]);
    rig.send(Effect::StartTurn(
        "USER-ASK start a helper that starts one".into(),
    ));
    rig.turn_finished().await.expect("turn succeeds");
    gates.reached("scout").await;
    gates.reached("inner").await;
    gates.open("inner");
    // (No gate holds the retires.)
    common::until(|| notes_in_last(&of_helper(&llm, "HELPER-TASK"), "inner") == 1).await;
    gates.open("scout");
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    assert_eq!(notes_in_last(&of_helper(&llm, "HELPER-TASK"), "inner"), 1);
    assert_eq!(notes_in_last(&of_agent(&llm), "inner"), 0);
    assert_eq!(notes_in_last(&of_agent(&llm), "scout"), 1);
    rig.quit().await;
}

/// The other order of the same race: `scout`'s run end is decided first,
/// while `inner`'s run has ended and its own decision is still held.
/// Before 2026-11-18 `scout` was done then and `inner`'s note went up to
/// the agent (before the fix of 2026-11-17 it went to the helper being
/// retired and was lost, or refused: "mob member not found: scout"). A
/// helper whose own helper is still at work is not done now (else it
/// would be retired with it, losing its answer): `scout` stays, is not
/// retired, `inner`'s note reaches it once, and the agent is told once,
/// of `scout`, never of `inner`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nested_helper_s_note_after_its_parent_was_decided_reaches_the_parent() {
    let ws = Workspace::new();
    let llm = nesting();
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    let gates = DecisionGates::install(&rig, &["scout", "inner", "retire scout"]);
    rig.send(Effect::StartTurn(
        "USER-ASK start a helper that starts one".into(),
    ));
    rig.turn_finished().await.expect("turn succeeds");
    gates.reached("scout").await;
    gates.reached("inner").await;
    gates.open("scout");
    // Decided, not done: nothing retires it while inner is at work.
    let registry = rig.daemon.registry();
    let scout = rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("tui"),
        meerkat_mob::AgentIdentity::from("scout"),
    );
    common::until(|| registry.endings().idle_decided() >= 1).await;
    assert!(registry.endings().is_tracked(&scout), "scout is not done");
    gates.open("inner");
    common::until(|| notes_in_last(&of_helper(&llm, "HELPER-TASK"), "inner") == 1).await;
    gates.reached("retire scout").await;
    gates.open("retire scout");
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    rig.daemon.idle(&rig.daemon.agent).await;
    assert_eq!(notes_in_last(&of_agent(&llm), "scout"), 1);
    assert_eq!(notes_in_last(&of_agent(&llm), "inner"), 0);
    assert_eq!(notes_in_last(&of_helper(&llm, "HELPER-TASK"), "inner"), 1);
    rig.quit().await;
}

/// Item 20: an agent that writes a mob profile read-only with comms on is
/// told, by its next turn, that the profile's members cannot send
/// messages (whatever the engine made of the call).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_only_profile_with_comms_gets_the_agent_a_note() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "m1",
            name: "mob_profile_create",
            args: json!({"mob_id": "team", "name": "reviewer",
                "profile": {"model": "claude-sonnet-4-6", "tools": {"read_only": true, "comms": true}}}),
        },
        Step::Text("made it"),
        Step::Text("next"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    // Revealed as a `tool_search` would (`tests/deferral.rs`).
    let session = rig.current_session().await;
    rig.daemon.reveal(&session, &["mob_profile_create"]).await;
    rig.send(Effect::StartTurn("make a reviewer profile".into()));
    let _ = rig.turn_finished().await;
    rig.send(Effect::StartTurn("go on".into()));
    let _ = rig.turn_finished().await;
    let seen = format!("{:?}", llm.requests()[2]);
    assert!(
        seen.contains("cannot send any (`read_only` blocks every mutating tool"),
        "{seen}"
    );
    rig.quit().await;
}
