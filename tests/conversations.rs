//! Conversations: `/new` seats the daemon's
//! agent on a fresh session, `/resume` lists the workspace's
//! conversations and brings an earlier one back (rendered from its
//! committed messages), another attached client follows the switch, and
//! `/rewind` says why it is not offered.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use common::{ScriptedLlmClient, Step, Workspace, user_texts};
use rig::{Client, Running};
use rkat_tui::ui::{Block, Effect, Msg};
use serde_json::json;

fn reset(msg: &Msg) -> Option<&Vec<Block>> {
    match msg {
        Msg::ResetTranscript(blocks) => Some(blocks),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_starts_fresh_and_resume_brings_the_old_conversation_back() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("one"),
        Step::Text("two"),
        Step::Text("three"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let first = daemon.session.to_string();
    let mut client = Client::attach(&ws.root).await;
    let mut other = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "one");
    let gate_saw = |session: &str| {
        daemon
            .engine()
            .transcript(&meerkat_core::SessionId::parse(session).unwrap())
    };
    assert!(
        gate_saw(&first).is_some_and(|t| t.contains("first")),
        "{:?}",
        gate_saw(&first)
    );

    // /new: an empty transcript, a fresh session for the next prompt; the
    // gate forgot the conversation left.
    client.send(Effect::NewSession);
    let cleared = client.wait_for(|m| reset(m).is_some()).await;
    assert!(reset(&cleared).unwrap().is_empty());
    client
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("a new conversation")))
        .await;
    assert_eq!(gate_saw(&first), None);
    client.send(Effect::StartTurn("second".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "two");
    assert_eq!(user_texts(&llm.recorded()[1].messages), ["second"]);
    // The other client followed the switch.
    other
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("another client switched")))
        .await;

    // /resume: both conversations are listed; the first comes back with
    // its transcript.
    client.send(Effect::RequestResumePicker);
    let picker = client
        .wait_for(|m| matches!(m, Msg::ShowResumePicker(_)))
        .await;
    let Msg::ShowResumePicker(items) = picker else {
        unreachable!()
    };
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(items[0].1.ends_with("(current)"), "{items:?}");
    assert_eq!(items[1].0, first);
    assert!(items[1].1.contains("first"), "{items:?}");
    client.send(Effect::ResumeSession(first.clone()));
    let restored = client
        .wait_for(|m| reset(m).is_some_and(|blocks| !blocks.is_empty()))
        .await;
    let blocks = reset(&restored).unwrap();
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, Block::User { text } if text == "first")),
        "{blocks:?}"
    );
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, Block::Assistant { md } if md == "one")),
        "{blocks:?}"
    );
    // Seeded again from the stored conversation.
    assert!(
        gate_saw(&first).is_some_and(|t| t.contains("first")),
        "{:?}",
        gate_saw(&first)
    );
    client
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("resumed conversation")))
        .await;
    client.send(Effect::StartTurn("third".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "three");
    assert_eq!(user_texts(&llm.recorded()[2].messages), ["first", "third"]);
    // Resuming the current one is a no-op (`last` names the most recent
    // other one).
    client.send(Effect::ResumeSession(first.clone()));
    client
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text == "already in this conversation"),
        )
        .await;
    other.quit().await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// There is no rewind (`/help` says so), and the reason holds: the
/// engine refuses a transcript fork of the member's session (asked past
/// the proxy, which serves no `session/fork_at` to clients at all).
async fn the_engine_refuses_rewinding_a_members_session() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.unwrap();
    let refused = daemon
        .engine_side
        .call(
            "session/fork_at",
            json!({"session_id": daemon.session.to_string(), "message_index": 0}),
        )
        .await
        .unwrap_err();
    assert!(
        refused.contains("mob-owned session transcripts cannot be edited"),
        "{refused}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// The daemon itself refuses a switch while the agent's run is
/// open: the client's guard is in another process (another client's turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_is_refused_while_another_clients_turn_runs() {
    let ws = Workspace::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![Step::Gated(
        std::sync::Arc::clone(&gate),
        Box::new(Step::Text("finished")),
    )]);
    let daemon = Running::start(&ws.root, &llm).await;
    let running = daemon.connect_bound().await;
    running
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "content": "long work"}),
        )
        .await
        .unwrap();
    llm.wait_call(0).await;
    let other = daemon.connect().await;
    let refused = other.call("rkat/new", json!({})).await.unwrap_err();
    assert!(
        matches!(&refused, rkat_tui::client::ClientError::Rpc { code, message, .. }
            if *code == rkat_tui::daemon::protocol::SWITCH_BUSY && message.contains("interrupt")),
        "{refused}"
    );
    // The turn was not cut off: it finishes on the same session.
    gate.notify_one();
    let status = other
        .call(
            "mob/member_status",
            json!({"mob_id": "tui", "agent_identity": daemon.agent}),
        )
        .await
        .unwrap();
    assert_eq!(
        status["current_session_id"],
        json!(daemon.session.to_string())
    );
    // Once the run has ended the switch goes through.
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    loop {
        match other.call("rkat/new", json!({})).await {
            Ok(answer) => {
                assert_ne!(answer["session_id"], json!(daemon.session.to_string()));
                break;
            }
            Err(rkat_tui::client::ClientError::Rpc { code, .. })
                if code == rkat_tui::daemon::protocol::SWITCH_BUSY
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => panic!("{err}"),
        }
    }
    drop((running, other));
    daemon.stop().await;
}

/// The token counter and the context gauge follow the conversation: after
/// `/resume` they show the resumed conversation's own usage (the daemon's
/// `rkat/usage`, the last turn's context), after `/new` nothing, and a TUI
/// that attaches later starts from the session's totals and its last
/// context, not from nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_counter_and_gauge_follow_the_conversation() {
    use rkat_tui::ui::StatusUpdate;
    let tokens = |m: &Msg| match m {
        Msg::Status(StatusUpdate::Tokens { input, output }) => Some((*input, *output)),
        _ => None,
    };
    let gauge = |m: &Msg| match m {
        Msg::Status(StatusUpdate::Context { used, .. }) => Some(*used),
        _ => None,
    };
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::TextUsage("one", 100), Step::TextUsage("two", 7)]);
    let daemon = Running::start(&ws.root, &llm).await;
    let first = daemon.session.to_string();
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.unwrap();

    client.send(Effect::NewSession);
    client.wait_for(|m| reset(m).is_some()).await;
    assert_eq!(
        tokens(&client.wait_for(|m| tokens(m).is_some()).await),
        Some((0, 0))
    );
    assert_eq!(
        gauge(&client.wait_for(|m| gauge(m).is_some()).await),
        Some(None)
    );
    client.send(Effect::StartTurn("second".into()));
    client.turn_finished().await.unwrap();

    client.send(Effect::ResumeSession(first));
    client
        .wait_for(|m| reset(m).is_some_and(|blocks| !blocks.is_empty()))
        .await;
    // Each scripted call reports its input and 5 output tokens.
    assert_eq!(
        tokens(&client.wait_for(|m| tokens(m).is_some()).await),
        Some((100, 5))
    );
    assert_eq!(
        gauge(&client.wait_for(|m| gauge(m).is_some()).await),
        Some(Some(105))
    );

    // A TUI attaching now starts from the resumed conversation's totals.
    let mut late = Client::attach(&ws.root).await;
    late.saw(|m| tokens(m) == Some((100, 5))).await;
    // Its cursor is past the last turn (the replay holds no turn end): the
    // gauge is seeded from the daemon's reading of the last model call, as
    // a switch seeds it.
    late.saw(|m| gauge(m) == Some(Some(105))).await;
    late.quit().await;
    client.quit().await;
    daemon.stop().await;
}

/// `/compact now` in attach mode asks the agent's next compaction
/// check to compact (`mob/compact_member`): the next prompt's first model
/// call is the summary, and the client sees `compaction_completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_now_compacts_at_the_next_turn() {
    let ws = Workspace::new();
    let mut script = vec![Step::Text("ok"); 4];
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::Text("after"));
    let llm = ScriptedLlmClient::new(script);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    for i in 0..4 {
        client.send(Effect::StartTurn(format!("turn {i}")));
        client.turn_finished().await.unwrap();
    }
    client.send(Effect::CompactNow);
    let asked = client
        .wait_for(|m| matches!(m, Msg::CompactRequested(_)))
        .await;
    assert!(matches!(asked, Msg::CompactRequested(Ok(()))), "{asked:?}");
    client.send(Effect::StartTurn("next".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "after");
    client
        .saw(|m| {
            matches!(
                m,
                Msg::Agent(envelope)
                    if matches!(envelope.payload, meerkat_core::AgentEvent::CompactionCompleted { .. })
            )
        })
        .await;
    assert_eq!(llm.recorded().len(), 6, "the summary call came first");
    client.quit().await;
    daemon.stop().await;
}

/// `--resume <ref>` attaches and switches the daemon to that conversation
/// (a prompt on the command line goes to it, not to the one it left);
/// `-c` attaches to the current one without a word; `--resume <ref>` of
/// the current one says it is current already.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_and_continue_choose_the_conversation_at_attach() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("one"),
        Step::Text("two"),
        Step::Text("three"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let first = daemon.session.to_string();
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.unwrap();
    client.send(Effect::NewSession);
    client.wait_for(|m| reset(m).is_some()).await;
    client.send(Effect::StartTurn("second".into()));
    client.turn_finished().await.unwrap();
    client.quit().await;

    // -c: the current (second) conversation, no switch.
    let continued =
        Client::attach_args(&ws.root, &common::args_with(&ws.root, &["-c"]), None).await;
    let shown: Vec<&String> = continued
        .seen
        .iter()
        .filter_map(|m| match m {
            Msg::Status(rkat_tui::ui::StatusUpdate::Session(id)) => Some(id),
            _ => None,
        })
        .collect();
    assert!(shown.len() == 1 && *shown[0] != first, "{shown:?}");
    assert!(
        !continued.notices().contains("conversation"),
        "{}",
        continued.notices()
    );
    continued.quit().await;

    // --resume <prefix of the first>: switched at attach, with its
    // transcript; the held prompt lands in it.
    let prefix = &first[..first.len() - 4];
    let args = common::args_with(&ws.root, &["--resume", prefix]);
    let mut config = rkat_tui::client::configure(&args, rkat_tui::cli::AttachTarget::Existing)
        .await
        .unwrap();
    config.lister = common::no_lists();
    config.endpoint_files = vec![common::endpoint_file_for(&ws.root)];
    let mut resumed = Client {
        harness: rkat_tui::client::testing::spawn_attach_runner(config),
        seen: Vec::new(),
    };
    resumed.send(Effect::StartTurn("third".into()));
    let restored = resumed
        .wait_for(|m| reset(m).is_some_and(|blocks| !blocks.is_empty()))
        .await;
    assert!(
        reset(&restored)
            .unwrap()
            .iter()
            .any(|b| matches!(b, Block::User { text } if text == "first")),
        "{restored:?}"
    );
    resumed
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("resumed conversation")))
        .await;
    assert_eq!(resumed.turn_finished().await.unwrap().text, "three");
    assert_eq!(user_texts(&llm.recorded()[2].messages), ["first", "third"]);
    resumed.quit().await;

    // --resume <the current one>: attached to the agent running it, with
    // no switch.
    let args = common::args_with(&ws.root, &["--resume", &first]);
    let mut again = Client::attach_args(&ws.root, &args, None).await;
    again
        .saw(|m| matches!(m, Msg::Status(rkat_tui::ui::StatusUpdate::Session(id)) if *id == first))
        .await;
    assert_eq!(again.agent().await, daemon.agent);
    assert!(
        !again.notices().contains("resumed conversation"),
        "{}",
        again.notices()
    );
    again.quit().await;
    daemon.stop().await;
}

/// The client a command line with neither `-c` nor `--resume` makes.
async fn attach_as_typed(root: &std::path::Path, extra: &[&str]) -> Client {
    let args = common::args_with(root, extra);
    let choice = rkat_tui::client::agent_choice(&args);
    Client::attach_config(
        &args,
        None,
        common::no_lists(),
        &common::endpoint_file_for(root),
        Some(choice),
    )
    .await
}

/// The start semantics: the first `rkat-tui` takes the daemon's
/// first agent; a second one in the same workspace starts a new agent and
/// names the others working there; `-c` joins the most recently active
/// one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_rkat_tui_starts_a_new_agent_and_c_joins_the_most_recent() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("from b"), Step::Text("from a")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = attach_as_typed(&ws.root, &[]).await;
    let a_name = a.agent().await;
    assert_eq!(
        a_name, daemon.agent,
        "the first one takes the start's agent"
    );
    assert!(
        !a.notices().contains("other agents work"),
        "{}",
        a.notices()
    );
    let mut b = attach_as_typed(&ws.root, &[]).await;
    let b_name = b.agent().await;
    assert_ne!(b_name, a_name, "a second rkat-tui is a new agent");
    b.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. }
            if text.contains(&format!("other agents work in this workspace: {a_name} is idle")))
    })
    .await;
    b.send(Effect::StartTurn("b's work".into()));
    assert_eq!(b.turn_finished().await.unwrap().text, "from b");
    a.send(Effect::StartTurn("a's work".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "from a");
    // A ran last: `-c` joins it.
    let mut c = attach_as_typed(&ws.root, &["-c"]).await;
    assert_eq!(c.agent().await, a_name);
    for client in [a, b, c] {
        client.quit().await;
    }
    daemon.stop().await;
}

/// `--resume <ref>` attaches to the agent owning the conversation; when
/// that agent quit, it is seated again on the conversation (under its
/// name) and the conversation goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_of_a_quit_agents_conversation_seats_it_again() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let keeper = daemon.connect().await;
    let mut quitter = attach_as_typed(&ws.root, &[]).await;
    let mut other = attach_as_typed(&ws.root, &[]).await;
    let name = quitter.agent().await;
    assert_ne!(other.agent().await, name);
    quitter.send(Effect::StartTurn("remember the first".into()));
    assert_eq!(quitter.turn_finished().await.unwrap().text, "one");
    let session = daemon.session_of(&name).await;
    quitter.send(Effect::DecideQuit);
    quitter
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("retired")))
        .await;
    quitter.quit().await;
    let status = daemon.status().await;
    assert!(
        status.agents.iter().all(|row| row.name != name),
        "{status:?}"
    );
    let reference = session.to_string();
    let mut back = attach_as_typed(&ws.root, &["--resume", &reference]).await;
    assert_eq!(back.agent().await, name, "seated again under its name");
    back.saw(|m| {
        reset(m).is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| matches!(b, Block::User { text } if text == "remember the first"))
        })
    })
    .await;
    back.send(Effect::StartTurn("and go on".into()));
    assert_eq!(back.turn_finished().await.unwrap().text, "two");
    assert_eq!(daemon.session_of(&name).await, session);
    assert!(
        user_texts(&llm.requests()[1])
            .iter()
            .any(|text| text.contains("remember the first")),
        "the conversation went on"
    );
    drop(keeper);
    back.quit().await;
    other.quit().await;
    daemon.stop().await;
}
