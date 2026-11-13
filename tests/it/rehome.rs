//! Re-homing a workspace's realm from inside the TUI (`/rehome`: push to
//! another directory, pull a realm that moved here) and from the shell
//! (`rkat-tui --rehome`), on the rig: real daemons with a scripted LLM,
//! started again by the client's re-root (`rig::reroot`), each workspace's
//! endpoint file beside it ([`file`]).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_for, args_with, no_lists, user_texts};
use meerkat_core::Message;
use rig::{Client, Restarted, Running};
use rkat_tui::bootstrap::realm_home;
use rkat_tui::daemon::protocol::AgentChoice;
use rkat_tui::ui::{Block, Connection as LinkState, Effect, Msg, NoticeLevel, StatusUpdate};
use serde_json::json;
use tokio::sync::Notify;

/// The endpoint file of the rig's daemon on `workspace`: one per
/// workspace, beside it.
fn file(workspace: &Path) -> PathBuf {
    workspace.parent().unwrap().join("state").join(format!(
        "{}.endpoint",
        workspace.file_name().unwrap().to_string_lossy()
    ))
}

/// The realms of the rig's config directory beside `root`.
fn realms(root: &Path) -> PathBuf {
    common::config_dir_for(root).join(realm_home::WORKSPACES_DIR)
}

/// A daemon on `root` writing [`file`].
async fn daemon(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Running {
    daemon_with(root, llm, &[]).await
}

/// [`daemon`] with extra arguments.
async fn daemon_with(root: &Path, llm: &Arc<ScriptedLlmClient>, extra: &[&str]) -> Running {
    Running::start_with(
        &args_with(root, extra),
        Running::options(root, llm).with_endpoint_file(file(root)),
    )
    .await
}

/// A client on `root`'s daemon asking for `agent` (`None`: the most
/// recent one), able to re-home (its re-roots start daemons with `llm`).
async fn client(
    root: &Path,
    llm: &Arc<ScriptedLlmClient>,
    agent: Option<AgentChoice>,
) -> (Client, Restarted) {
    client_as(root, root, llm, agent).await
}

/// [`client`], its re-roots starting daemons with `user_root` as their
/// user config root (where the comms identity the realm's sessions are
/// bound to lives; the rig gives each workspace its own).
async fn client_as(
    root: &Path,
    user_root: &Path,
    llm: &Arc<ScriptedLlmClient>,
    agent: Option<AgentChoice>,
) -> (Client, Restarted) {
    let mut args = args_for(root);
    args.user_config_root = Some(user_root.to_path_buf());
    let (reroot, restarted) = rig::reroot(args, llm, file);
    let mut config =
        rig::client_config(&args_for(root), None, no_lists(), &file(root), agent).await;
    config.reroot = Some(reroot);
    (Client::attach_with_config(config).await, restarted)
}

/// One conversation in `root` with the prompt `prompt`, then the daemon
/// stopped: its session.
async fn one_conversation(root: &Path, prompt: &str) -> meerkat_core::SessionId {
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let running = daemon(root, &llm).await;
    let (mut client, _) = client(root, &llm, None).await;
    client.send(Effect::StartTurn(prompt.into()));
    let _ = client.turn_finished().await;
    let session = running.current_session().await;
    client.quit().await;
    running.stop().await;
    session
}

/// The session the status bar shows last.
fn shown_session(client: &Client) -> Option<String> {
    client.seen.iter().rev().find_map(|msg| match msg {
        Msg::Status(StatusUpdate::Session(session)) => Some(session.clone()),
        _ => None,
    })
}

/// A notice of `level` containing `text`, seen or arriving.
async fn notice(client: &mut Client, level: NoticeLevel, text: &str) -> String {
    let wanted = |msg: &Msg| matches!(msg, Msg::Notice { level: l, text: t } if *l == level && t.contains(text));
    let msg = match client.seen.iter().find(|msg| wanted(msg)) {
        Some(msg) => msg.clone(),
        None => client.wait_for(wanted).await,
    };
    match msg {
        Msg::Notice { text, .. } => text,
        _ => unreachable!(),
    }
}

/// The text of tool result `id` in `messages`.
fn tool_result(messages: &[Message], id: &str) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|result| result.tool_use_id == id)
                .map(meerkat_core::ToolResult::text_content),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result {id}"))
}

/// The push, the primary form: with an idle daemon on the realm, `/rehome
/// <directory>` stops it, moves the realm, starts the daemon in the new
/// directory and the same client reattaches to the same conversation,
/// without quitting. The agent's shell and file tools work there
/// afterwards (a `pwd` and a relative write prove it). A second client is
/// disconnected and does not reach the moved realm from the old directory;
/// a fresh `-c` in the new directory continues the conversation; a start
/// in the old directory gets a fresh realm without the old sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_moves_the_session_to_the_new_workspace_without_a_restart() {
    let ws = Workspace::new();
    let new = ws.sibling("pushed");
    std::fs::create_dir_all(&new).unwrap();
    let new = std::fs::canonicalize(&new).unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("before"),
        Step::Tools(vec![
            ("p1", "shell", json!({"command": "pwd"})),
            (
                "w1",
                "write_file",
                json!({"path": "marker.txt", "content": "moved\n"}),
            ),
        ]),
        Step::Text("after"),
    ]);
    let old_daemon = daemon(&ws.root, &llm).await;
    let realm = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let (mut issuer, restarted) = client(&ws.root, &llm, None).await;
    let (mut other, _) = client(&ws.root, &llm, Some(AgentChoice::New)).await;
    issuer.send(Effect::StartTurn("before the move".into()));
    let _ = issuer.turn_finished().await;
    let session = old_daemon.current_session().await;
    assert_eq!(shown_session(&issuer), Some(session.to_string()));

    issuer.send(Effect::Rehome(Some(new.display().to_string())));
    issuer
        .wait_for(
            |msg| matches!(msg, Msg::Rerooted { workspace, .. } if workspace.as_path() == new),
        )
        .await;
    old_daemon.ended().await;
    issuer.attached().await;
    let said = notice(&mut issuer, NoticeLevel::Info, "now serves").await;
    assert!(
        said.contains(&realm.id) && said.contains(&new.display().to_string()),
        "{said}"
    );
    // The same conversation, under the same realm, in the new place.
    assert_eq!(shown_session(&issuer), Some(session.to_string()));
    assert_eq!(common::realm_of(&new), realm.dir);
    assert_eq!(realm_home::find(&realms(&ws.root), &ws.root).unwrap(), None);
    issuer.send(Effect::StartTurn("where are you".into()));
    assert_eq!(issuer.turn_finished().await.unwrap().text, "after");
    let requests = llm.requests();
    let pwd = tool_result(requests.last().unwrap(), "p1");
    assert!(pwd.contains(&new.display().to_string()), "{pwd}");
    assert_eq!(
        std::fs::read_to_string(new.join("marker.txt")).unwrap(),
        "moved\n"
    );
    assert!(!ws.root.join("marker.txt").exists());
    assert_eq!(
        user_texts(requests.last().unwrap()),
        vec!["before the move".to_string(), "where are you".to_string()]
    );

    // The second client was disconnected, and its reconnect, which looks
    // for the old directory's daemon, reached nothing: the new daemon
    // counts the issuer only, and no daemon serves the old directory.
    other
        .wait_for(|msg| {
            matches!(
                msg,
                Msg::Status(StatusUpdate::Connection(LinkState::Reconnecting))
            )
        })
        .await;
    let new_daemon = restarted.lock().pop().unwrap();
    assert_eq!(new_daemon.status().await.clients, 1);
    assert!(!file(&ws.root).exists());
    let attaches = other
        .seen
        .iter()
        .filter(|msg| {
            matches!(
                msg,
                Msg::Status(StatusUpdate::Connection(LinkState::Attached { .. }))
            )
        })
        .count();
    assert_eq!(attaches, 1, "attached once, before the move");
    other.quit().await;
    issuer.quit().await;
    new_daemon.stop().await;

    // A fresh `-c` in the new directory continues the conversation.
    let llm = ScriptedLlmClient::new(vec![]);
    let fresh = daemon(&new, &llm).await;
    let mut config = rig::client_config(
        &args_with(&new, &["-c"]),
        None,
        no_lists(),
        &file(&new),
        None,
    )
    .await;
    config.reroot = None;
    let again = Client::attach_with_config(config).await;
    assert_eq!(shown_session(&again), Some(session.to_string()));
    again.quit().await;
    fresh.stop().await;

    // A start in the old directory gets a fresh realm and none of the
    // old sessions.
    let llm = ScriptedLlmClient::new(vec![]);
    let restart = daemon(&ws.root, &llm).await;
    assert_ne!(common::realm_of(&ws.root), realm.dir);
    assert_ne!(restart.current_session().await, session);
    let listed = restart
        .connect()
        .await
        .call("rkat/conversations", json!({}))
        .await
        .unwrap();
    assert!(
        !listed.to_string().contains(&session.to_string()),
        "{listed}"
    );
    restart.stop().await;
}

/// A push while a turn runs on the daemon (another client's) is refused,
/// naming the agent, and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_while_a_turn_runs_is_refused() {
    let ws = Workspace::new();
    let new = ws.sibling("pushed");
    std::fs::create_dir_all(&new).unwrap();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![Step::Gated(
        Arc::clone(&gate),
        Box::new(Step::Text("done")),
    )]);
    let running = daemon(&ws.root, &llm).await;
    let realm = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let (mut issuer, restarted) = client(&ws.root, &llm, None).await;
    let (mut other, _) = client(&ws.root, &llm, Some(AgentChoice::New)).await;
    let busy = other.agent().await;
    other.send(Effect::StartTurn("a long one".into()));
    llm.wait_call(0).await;
    issuer.send(Effect::Rehome(Some(new.display().to_string())));
    let refused = notice(&mut issuer, NoticeLevel::Warn, "a turn is running").await;
    assert!(refused.contains(&busy), "{refused}");
    let still = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    assert_eq!(
        (still.id, still.manifest.path),
        (realm.id, realm.manifest.path)
    );
    assert!(restarted.lock().is_empty());
    assert!(running.serving());
    gate.notify_one();
    let _ = other.turn_finished().await;
    other.quit().await;
    issuer.quit().await;
    running.stop().await;
}

/// A push into a directory whose realm holds a conversation with a user
/// turn is refused and changes nothing; so is a pull into a workspace
/// whose own realm holds one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rehome_into_a_realm_that_holds_a_conversation_is_refused() {
    let ws = Workspace::new();
    let taken = ws.sibling("taken");
    std::fs::create_dir_all(&taken).unwrap();
    one_conversation(&taken, "mine").await;
    let gone = ws.sibling("gone");
    std::fs::create_dir_all(&gone).unwrap();
    one_conversation(&gone, "moved away").await;
    let orphan = realm_home::find(&realms(&ws.root), &std::fs::canonicalize(&gone).unwrap())
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let before = realm_home::scan(&realms(&ws.root)).unwrap();

    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let running = daemon(&ws.root, &llm).await;
    let (mut issuer, restarted) = client(&ws.root, &llm, None).await;
    issuer.send(Effect::StartTurn("here too".into()));
    let _ = issuer.turn_finished().await;
    // Push: the destination's realm holds "mine" (its path reads both
    // ways, so `to` says which).
    issuer.send(Effect::Rehome(Some(format!("to {}", taken.display()))));
    let refused = notice(&mut issuer, NoticeLevel::Warn, "is not empty").await;
    assert!(refused.contains("user turns"), "{refused}");
    // Pull: this workspace's realm holds "here too".
    issuer.send(Effect::Rehome(Some(orphan.id.clone())));
    notice(
        &mut issuer,
        NoticeLevel::Warn,
        "a re-home replaces only an empty realm",
    )
    .await;
    let mut after = realm_home::scan(&realms(&ws.root)).unwrap();
    // The issuer's own realm was there before too (made by its daemon).
    after.retain(|home| home.manifest.path != ws.root);
    let mut expected = before;
    expected.retain(|home| home.manifest.path != ws.root);
    assert_eq!(
        after
            .iter()
            .map(|h| (&h.id, &h.manifest.path))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|h| (&h.id, &h.manifest.path))
            .collect::<Vec<_>>()
    );
    assert!(restarted.lock().is_empty());
    assert!(running.serving());
    issuer.quit().await;
    running.stop().await;
}

/// The ordinary flow after a move: `rkat-tui` started in the moved
/// directory makes it an empty realm and offers the old one; `/rehome
/// <id>` removes the empty realm, restarts the daemon on the adopted one
/// and the same client continues its latest conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_from_a_fresh_start_in_the_moved_directory_adopts_the_old_realm() {
    let ws = Workspace::new();
    let first = one_conversation(&ws.root, "before the move").await;
    let old = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let moved = ws.sibling("moved");
    std::fs::rename(&ws.root, &moved).unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("still here")]);
    let running = daemon(&moved, &llm).await;
    let fresh = realm_home::find(&realms(&moved), &moved).unwrap().unwrap();
    assert_ne!(fresh.id, old.id);
    let (mut issuer, restarted) = client(&moved, &llm, None).await;
    issuer.send(Effect::Rehome(Some(old.id.clone())));
    issuer
        .wait_for(|msg| matches!(msg, Msg::Rerooted { realm, .. } if *realm == old.dir))
        .await;
    running.ended().await;
    issuer.attached().await;
    let blocks = match issuer
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(blocks) if !blocks.is_empty()))
        .await
    {
        Msg::ResetTranscript(blocks) => blocks,
        _ => unreachable!(),
    };
    assert!(blocks.contains(&Block::User {
        text: "before the move".into()
    }));
    assert_eq!(shown_session(&issuer), Some(first.to_string()));
    assert!(!fresh.dir.exists(), "the empty realm is removed");
    assert_eq!(common::realm_of(&moved), old.dir);
    issuer.send(Effect::StartTurn("and now".into()));
    assert_eq!(issuer.turn_finished().await.unwrap().text, "still here");
    issuer.quit().await;
    let daemon = restarted.lock().pop().unwrap();
    daemon.stop().await;
}

/// A pull whose adopted realm has a live, idle daemon (in the directory
/// the workspace was copied from) stops it and goes through; the old
/// directory's path reads both ways (it exists, and a manifest names it),
/// so it is refused until `from` says which.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_stops_the_idle_daemon_of_the_adopted_realm() {
    let ws = Workspace::new();
    let first = one_conversation(&ws.root, "in the original").await;
    let old = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    // The original's daemon runs as its own process (two daemons cannot
    // share one process): the real binary, on an endpoint it never calls.
    let user = common::config_dir_for(&ws.root);
    std::fs::write(
        user.join("tui.toml"),
        "[endpoints.fake]\ndialect = \"anthropic\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"HOME\"\n",
    )
    .unwrap();
    let argv = rkat_tui::client::launch::with_endpoint_file(
        &rkat_tui::client::launch::daemon_argv(&args_for(&ws.root), &ws.root),
        &file(&ws.root),
    );
    let mut original = tokio::process::Command::new(env!("CARGO_BIN_EXE_rkat-tui-mob"))
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    common::within(async {
        while !file(&ws.root).exists() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    let llm = ScriptedLlmClient::new(vec![]);
    let copy = ws.sibling("copy");
    common::copy_tree(&ws.root, &copy);
    let running = daemon(&copy, &llm).await;
    let (mut issuer, restarted) = client(&copy, &llm, None).await;
    issuer.send(Effect::Rehome(Some(ws.root.display().to_string())));
    notice(&mut issuer, NoticeLevel::Warn, "reads two ways").await;
    assert!(original.try_wait().unwrap().is_none(), "still serving");
    issuer.send(Effect::Rehome(Some(format!("from {}", ws.root.display()))));
    issuer
        .wait_for(|msg| matches!(msg, Msg::Rerooted { realm, .. } if *realm == old.dir))
        .await;
    let status = common::within(original.wait()).await.unwrap();
    assert!(status.success(), "{status}");
    running.ended().await;
    issuer.attached().await;
    assert_eq!(shown_session(&issuer), Some(first.to_string()));
    assert_eq!(common::realm_of(&copy), old.dir);
    assert_eq!(realm_home::find(&realms(&ws.root), &ws.root).unwrap(), None);
    issuer.quit().await;
    let daemon = restarted.lock().pop().unwrap();
    daemon.stop().await;
}

/// `rkat-tui --rehome <old path>` in the moved workspace adopts its state,
/// removing the empty one a start there made: `-c` continues the latest
/// conversation there, under the realm id it always had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_shell_rehome_continue_resumes_the_latest_conversation_in_the_new_place() {
    let ws = Workspace::new();
    let first = one_conversation(&ws.root, "before the move").await;
    let old = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let moved = ws.sibling("renamed-project");
    std::fs::rename(&ws.root, &moved).unwrap();
    let fresh = common::realm_of(&moved);
    let files: rkat_tui::client::rehome::EndpointFiles =
        Arc::new(|workspace: &Path| vec![file(workspace)]);
    let done =
        rkat_tui::client::rehome::pull_here(&args_for(&moved), ws.root.to_str().unwrap(), &files)
            .await
            .unwrap();
    assert_eq!(done.home.id, old.id);
    assert_eq!(done.from, ws.root);
    assert!(done.removed.is_some());
    assert!(!fresh.exists());
    let llm = ScriptedLlmClient::new(vec![]);
    let running = daemon(&moved, &llm).await;
    let mut config = rig::client_config(
        &args_with(&moved, &["-c"]),
        None,
        no_lists(),
        &file(&moved),
        None,
    )
    .await;
    config.reroot = None;
    let again = Client::attach_with_config(config).await;
    assert_eq!(shown_session(&again), Some(first.to_string()));
    assert_eq!(
        user_texts(&running.transcript(&first).await),
        vec!["before the move".to_string()]
    );
    assert_eq!(common::realm_of(&moved), old.dir);
    again.quit().await;
    running.stop().await;
}
