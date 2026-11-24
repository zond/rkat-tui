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
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use rig::{Client, Restarted, Running};
use rkat_tui::bootstrap::realm_home;
use rkat_tui::daemon::protocol::AgentChoice;
use rkat_tui::ui::{
    Block, Connection as LinkState, Effect, ListPicker, ModalKey, Msg, NoticeLevel, PickAct,
    PickerKind, StatusUpdate,
};
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
    let said = notice(&mut issuer, NoticeLevel::Info, "now belongs to").await;
    assert!(
        said.contains(&realm.id) && said.contains(&new.display().to_string()),
        "{said}"
    );
    assert!(said.contains("moved this workspace's state"), "{said}");
    // The issuer's conversation and the second client's agent's.
    assert!(said.contains("2 conversations"), "{said}");
    assert!(
        said.contains(&format!("this rkat-tui now works in {}", new.display())),
        "{said}"
    );
    // The same conversation, under the same realm, in the new place.
    assert_eq!(shown_session(&issuer), Some(session.to_string()));
    assert_eq!(common::realm_of(&new), realm.dir);
    assert_eq!(realm_home::find(&realms(&ws.root), &ws.root).unwrap(), None);
    issuer.send(Effect::StartTurn("where are you".into()));
    assert_eq!(issuer.turn_finished().await.unwrap().text, "after");
    let requests = llm.requests();
    let pwd = common::tool_text(requests.last().unwrap(), "p1");
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
    assert!(refused.contains("--remove-state"), "{refused}");
    // The dialog asks whether to delete it; nobody answers here (n).
    let asked = issuer
        .seen
        .iter()
        .any(|msg| matches!(msg, Msg::ShowPicker { picker, .. } if picker.kind == PickerKind::Rehome && picker.line.is_some()));
    assert!(asked, "no question about deleting the destination's state");
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
    common::until(|| file(&ws.root).exists()).await;
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

/// The `/rehome` picker the client is shown, seen or arriving.
async fn rehome_picker(client: &mut Client) -> ListPicker {
    let msg = client
        .saw(|msg| matches!(msg, Msg::ShowPicker { picker, refresh: false } if picker.kind == PickerKind::Rehome))
        .await;
    match msg {
        Msg::ShowPicker { picker, .. } => picker,
        _ => unreachable!(),
    }
}

/// What the picker's keys `keys` answer, the last one's act.
fn press(picker: &mut ListPicker, keys: &[KeyCode]) -> Option<PickAct> {
    let mut last = None;
    for code in keys {
        last = match picker.key(&KeyEvent::from(*code)) {
            ModalKey::Choose(act) => Some(act),
            _ => None,
        };
    }
    last
}

/// The realms under the rig's config directory of `root`: (id, path).
fn on_disk(root: &Path) -> Vec<(String, PathBuf)> {
    realm_home::scan(&realms(root))
        .unwrap()
        .into_iter()
        .map(|home| (home.id, home.manifest.path))
        .collect()
}

/// `/rehome` alone opens the picker: the moved workspace's old state is
/// its first row; `n` to the question and Esc change nothing (the same
/// realms on disk, the daemon still serving); Enter then `y` answers
/// `/rehome from <id>`, which adopts it here as the typed form does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_picker_adopts_the_old_state_and_its_no_changes_nothing() {
    let ws = Workspace::new();
    let first = one_conversation(&ws.root, "before the move").await;
    let old = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let moved = ws.sibling("moved");
    std::fs::rename(&ws.root, &moved).unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let running = daemon(&moved, &llm).await;
    let (mut issuer, restarted) = client(&moved, &llm, None).await;
    issuer.send(Effect::Rehome(None));
    let mut picker = rehome_picker(&mut issuer).await;
    assert!(
        picker
            .header
            .as_deref()
            .unwrap()
            .contains(&moved.display().to_string())
    );
    assert_eq!(picker.rows[0].id.as_deref(), Some(old.id.as_str()));
    assert!(picker.rows.last().unwrap().text.starts_with("Move"));
    let before = on_disk(&moved);
    assert_eq!(
        press(&mut picker, &[KeyCode::Enter, KeyCode::Char('n')]),
        None
    );
    assert_eq!(press(&mut picker, &[KeyCode::Esc]), None);
    assert_eq!(on_disk(&moved), before);
    assert!(running.serving());
    assert!(restarted.lock().is_empty());

    let mut picker = rehome_picker(&mut issuer).await;
    let act = press(&mut picker, &[KeyCode::Enter, KeyCode::Char('y')]);
    let Some(PickAct::Rehome(argument)) = act else {
        panic!("{act:?}");
    };
    assert_eq!(argument, format!("from {}", old.id));
    issuer.send(Effect::Rehome(Some(argument)));
    issuer
        .wait_for(|msg| matches!(msg, Msg::Rerooted { realm, .. } if *realm == old.dir))
        .await;
    running.ended().await;
    issuer.attached().await;
    let said = notice(&mut issuer, NoticeLevel::Info, "adopted realm").await;
    assert!(
        said.contains(&format!("(was {})", ws.root.display())) && said.contains("1 conversation"),
        "{said}"
    );
    assert_eq!(shown_session(&issuer), Some(first.to_string()));
    assert_eq!(common::realm_of(&moved), old.dir);
    issuer.quit().await;
    let daemon = restarted.lock().pop().unwrap();
    daemon.stop().await;
}

/// The picker's move row: a directory that does not exist is refused by
/// the check, naming it, with nothing changed; once it exists the check
/// asks, and `/rehome to <dir>` moves the state there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_picker_checks_the_directory_and_moves_the_state_there() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let running = daemon(&ws.root, &llm).await;
    let realm = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let (mut issuer, restarted) = client(&ws.root, &llm, None).await;
    let new = ws.sibling("elsewhere");
    let typed = "../elsewhere".to_string();
    issuer.send(Effect::RehomeCheck(typed.clone()));
    let answer = issuer
        .wait_for(|msg| matches!(msg, Msg::RehomeChecked { .. }))
        .await;
    let Msg::RehomeChecked {
        answer: Err(why), ..
    } = answer
    else {
        panic!("{answer:?}");
    };
    assert!(
        why.contains(&format!("{} does not exist", new.display())),
        "{why}"
    );
    assert!(running.serving());
    std::fs::create_dir_all(&new).unwrap();
    let new = std::fs::canonicalize(&new).unwrap();
    issuer.send(Effect::RehomeCheck(typed));
    let answer = issuer
        .wait_for(|msg| matches!(msg, Msg::RehomeChecked { .. }))
        .await;
    let Msg::RehomeChecked {
        answer: Ok((question, path)),
        ..
    } = answer
    else {
        panic!("{answer:?}");
    };
    assert_eq!(path, new);
    assert!(question.contains(&realm.id), "{question}");
    // Nothing moved by the check itself.
    assert_eq!(common::realm_of(&ws.root), realm.dir);
    issuer.send(Effect::Rehome(Some(format!("to {}", path.display()))));
    issuer
        .wait_for(
            |msg| matches!(msg, Msg::Rerooted { workspace, .. } if workspace.as_path() == new),
        )
        .await;
    running.ended().await;
    issuer.attached().await;
    notice(&mut issuer, NoticeLevel::Info, "now belongs to").await;
    assert_eq!(common::realm_of(&new), realm.dir);
    issuer.quit().await;
    let daemon = restarted.lock().pop().unwrap();
    daemon.stop().await;
}

/// No push into the config directory: `to` a realm's directory (the
/// owner's mistake: the old state's own directory, typed to adopt it) is
/// refused naming `/rehome from`, with nothing changed on disk and the
/// daemon still serving the workspace; read either way, that directory
/// is the realm and is adopted. A directory read either way, to which a
/// state with no conversation would move, is refused as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_into_the_config_directory_or_of_nothing_is_refused() {
    let ws = Workspace::new();
    let other = ws.sibling("other");
    std::fs::create_dir_all(&other).unwrap();
    one_conversation(&other, "the other's").await;
    let other_realm = realm_home::find(&realms(&ws.root), &std::fs::canonicalize(&other).unwrap())
        .unwrap()
        .unwrap();
    let free = ws.sibling("free");
    std::fs::create_dir_all(&free).unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let running = daemon(&ws.root, &llm).await;
    let (mut issuer, restarted) = client(&ws.root, &llm, None).await;
    let before = on_disk(&ws.root);
    issuer.send(Effect::Rehome(Some(format!(
        "to {}",
        other_realm.dir.display()
    ))));
    let refused = notice(&mut issuer, NoticeLevel::Warn, "config directory").await;
    assert!(refused.contains("/rehome from"), "{refused}");
    issuer.send(Effect::Rehome(Some(free.display().to_string())));
    let empty = notice(&mut issuer, NoticeLevel::Warn, "holds no conversation").await;
    assert!(empty.contains("/rehome to"), "{empty}");
    assert_eq!(on_disk(&ws.root), before);
    assert!(running.serving());
    assert!(restarted.lock().is_empty());
    // Read either way, the realm's directory is that realm: a pull.
    issuer.send(Effect::Rehome(Some(other_realm.dir.display().to_string())));
    issuer
        .wait_for(|msg| matches!(msg, Msg::Rerooted { realm, .. } if *realm == other_realm.dir))
        .await;
    running.ended().await;
    issuer.attached().await;
    issuer.quit().await;
    let daemon = restarted.lock().pop().unwrap();
    daemon.stop().await;
}

/// A push onto a directory whose state holds a conversation, once the
/// user confirmed deleting it (the dialog's `y`): that state is deleted
/// and this workspace's moves there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_confirmed_discard_deletes_the_destination_s_state_and_moves_there() {
    let ws = Workspace::new();
    let taken = ws.sibling("taken");
    std::fs::create_dir_all(&taken).unwrap();
    one_conversation(&taken, "mine").await;
    let taken = std::fs::canonicalize(&taken).unwrap();
    let theirs = realm_home::find(&realms(&ws.root), &taken)
        .unwrap()
        .unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let running = daemon(&ws.root, &llm).await;
    let realm = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let (mut issuer, restarted) = client(&ws.root, &llm, None).await;
    issuer.send(Effect::StartTurn("here".into()));
    let _ = issuer.turn_finished().await;
    issuer.send(Effect::Rehome(Some(format!("to {}", taken.display()))));
    notice(&mut issuer, NoticeLevel::Warn, "is not empty").await;
    let mut asked = rehome_picker(&mut issuer).await;
    let act = press(&mut asked, &[KeyCode::Char('y')]);
    let Some(PickAct::RehomeDiscarding { argument, discard }) = act else {
        panic!("{act:?}");
    };
    assert_eq!(discard, theirs.id);
    assert!(theirs.dir.exists());
    issuer.send(Effect::RehomeDiscarding { argument, discard });
    issuer
        .wait_for(
            |msg| matches!(msg, Msg::Rerooted { workspace, .. } if workspace.as_path() == taken),
        )
        .await;
    running.ended().await;
    issuer.attached().await;
    assert!(!theirs.dir.exists(), "the confirmed state is deleted");
    assert_eq!(common::realm_of(&taken), realm.dir);
    issuer.quit().await;
    let daemon = restarted.lock().pop().unwrap();
    daemon.stop().await;
}

/// `rkat-tui --remove-state`: an unknown reference is refused; with a
/// client attached it refuses; without `--yes` it says what it would
/// delete and deletes nothing; with `--yes` it stops the idle daemon and
/// deletes exactly that realm, the others untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_state_deletes_exactly_the_named_state() {
    use rkat_tui::client::rehome::{self, Removed};
    let ws = Workspace::new();
    let keep = ws.sibling("keep");
    std::fs::create_dir_all(&keep).unwrap();
    one_conversation(&keep, "kept").await;
    let llm = ScriptedLlmClient::new(vec![]);
    let running = daemon(&ws.root, &llm).await;
    let realm = realm_home::find(&realms(&ws.root), &ws.root)
        .unwrap()
        .unwrap();
    let files: rehome::EndpointFiles = Arc::new(|workspace: &Path| vec![file(workspace)]);
    let mut args = args_for(&ws.root);
    let unknown = rehome::remove_state(&args, "nope", &files)
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("names no realm"), "{unknown}");
    let (attached, _) = client(&ws.root, &llm, None).await;
    args.yes = true;
    let busy = rehome::remove_state(&args, &realm.id, &files)
        .await
        .unwrap_err();
    assert!(busy.to_string().contains("client(s) attached"), "{busy}");
    attached.quit().await;
    // The daemon counts the client gone once its connection closed.
    let mut clients = running.status().await.clients;
    for _ in 0..200 {
        if clients == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        clients = running.status().await.clients;
    }
    assert_eq!(clients, 0, "the client's leave was not counted");
    args.yes = false;
    let before = on_disk(&ws.root);
    let would = rehome::remove_state(&args, &realm.id, &files)
        .await
        .unwrap();
    let Removed::Would(text) = &would else {
        panic!("{would:?}");
    };
    assert!(
        text.contains(&format!("--remove-state {} --yes", realm.id)),
        "{text}"
    );
    assert_eq!(on_disk(&ws.root), before);
    assert!(running.serving());
    args.yes = true;
    let done = rehome::remove_state(&args, &realm.id, &files)
        .await
        .unwrap();
    assert!(matches!(done, Removed::Deleted(_)), "{done:?}");
    running.ended().await;
    assert!(!realm.dir.exists());
    let left = on_disk(&ws.root);
    assert_eq!(left.len(), before.len() - 1);
    assert!(left.iter().all(|(id, _)| *id != realm.id));
    assert!(realms(&ws.root).exists());
}
