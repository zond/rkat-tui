//! What an agent built with the mob tools outlives an abrupt death of its
//! daemon. "Abrupt" here is a SIGKILL of the process the daemon runs in:
//! the test reruns itself as a child process ([`SIDE`]), which starts the
//! rig's daemon (the real one, its model scripted in that process) on the
//! parent's workspace, has the agent build its team, and is killed by the
//! parent once the team stands. Nothing of the daemon's stop runs then
//! (`Daemon::serve_until` after its serve loop: the endpoint file's
//! removal, the conversation log's and the usage ledger's flush, the
//! helper bindings' flush, the mobs' and the runtime's stop, the realm
//! lease's release, the databases' settle), and the OS releases the
//! process's locks as it does for a crash. The endpoint file the killed
//! daemon left is the positive proof that no stop ran. The parent then
//! starts a daemon on the same workspace and asserts on what it restored.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::common::{self, ScriptedLlmClient, Step, Workspace, endpoint_file_for};
use crate::connect::{self, A, MOB_TOOLS, W1, W2, answered, commanded, heard, result_in, tool};
use crate::rig::{Running, ScriptedClassifier, Setup};
use meerkat_core::Message;
use meerkat_mob::{AgentIdentity, MobId};
use rkat_tui::mobs::HelperKey;
use serde_json::json;

/// Set in the rerun: the workspace the killed daemon builds in.
const SIDE: &str = "RKAT_TUI_CRASH_ROOT";
/// The marker file the rerun writes once the team stands (beside the
/// workspace): the agent's name, its session and its `contacts` result,
/// one to a line.
const BUILT: &str = "crash-built";
/// The profile the mob's third member is seated with, on its own model.
const HAIKU: &str = "claude-haiku-4-5";

/// A member `member` of `team` with `profile`, its first turn `task`.
fn spawn(member: &str, profile: &str, task: &str) -> serde_json::Value {
    json!({"mob_id": "team", "profile": profile, "member_id": member, "initial_message": task})
}

/// The script both daemons draw from. The agent, prompted `BUILD`,
/// creates `team` (profiles `worker` and `lead`, each naming its model),
/// keeps the helper `a` in the workspace's mob, seats `w1` and `w2`
/// (workers) and `w3` (the lead) in `team`, and connects `a`, `w1` and
/// `w2` (an edge within `team`, two across mobs). Prompted `AFTER`, it
/// lists its contacts, gives `a` more work by message and seats `w4`.
/// `a`, `w1` and `w2` do what the test tells them
/// ([`connect::commanded`]); `a` answers more work with
/// `A-SECOND-ANSWER`. `w3`'s first run never ends while `killed` (its
/// model call hangs: it is mid-run at the kill); after it, `w3` answers.
fn script(killed: bool) -> Vec<Step> {
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::is_helper(messages, "TASK-OF-A") {
            // The agent's message, among what this request answers.
            let from = messages
                .iter()
                .rposition(|m| matches!(m, Message::BlockAssistant(_)))
                .map_or(0, |at| at + 1);
            if format!("{:?}", &messages[from..]).contains("MORE-WORK") {
                return Step::Text("A-SECOND-ANSWER");
            }
            return commanded(messages, "A", "team/worker/w1");
        }
        if common::is_helper(messages, "TASK-OF-W1") {
            return commanded(messages, "W1", "team/worker/w2");
        }
        if common::is_helper(messages, "TASK-OF-W2") {
            return commanded(messages, "W2", "team/worker/w1");
        }
        if common::is_helper(messages, "TASK-OF-W3") {
            return if killed {
                Step::Hang
            } else {
                Step::Text("W3-AFTER-RESTART")
            };
        }
        if common::is_helper(messages, "TASK-OF-W4") {
            return Step::Text("W4-IDLE");
        }
        // The agent: each step answers the call before it.
        let chain = [
            ("m1", "s1"),
            ("s1", "s2"),
            ("s2", "s3"),
            ("s3", "s4"),
            ("s4", "c1"),
        ];
        if answered(messages, "b1") {
            return Step::Text("built");
        }
        if answered(messages, "c1") {
            return tool("b1", "contacts", json!({}));
        }
        for (done, next) in chain {
            if answered(messages, done) {
                return match next {
                    "s1" => tool(
                        "s1",
                        "assign",
                        json!({"name": "a", "task": "TASK-OF-A", "keep": true}),
                    ),
                    "s2" => tool(
                        "s2",
                        "mob_spawn_member",
                        spawn("w1", "worker", "TASK-OF-W1"),
                    ),
                    "s3" => tool(
                        "s3",
                        "mob_spawn_member",
                        spawn("w2", "worker", "TASK-OF-W2"),
                    ),
                    "s4" => tool("s4", "mob_spawn_member", spawn("w3", "lead", "TASK-OF-W3")),
                    _ => tool(
                        "c1",
                        "connect_helpers",
                        json!({"helpers": ["a", "w1", "w2"]}),
                    ),
                };
            }
        }
        if answered(messages, "k3") {
            return Step::Text("after done");
        }
        if answered(messages, "k2") {
            return tool(
                "k3",
                "mob_spawn_member",
                spawn("w4", "worker", "TASK-OF-W4"),
            );
        }
        if answered(messages, "k1") {
            return connect::send("k2", crate::assign::peer_id(messages, "a"), "MORE-WORK");
        }
        let inputs = common::run_inputs(messages);
        if inputs.contains("BUILD") {
            return tool(
                "m1",
                "mob_create",
                json!({"definition": {"id": "team", "profiles": {
                    "worker": {"model": common::MODEL, "external_addressable": true, "tools": {"comms": true}},
                    "lead": {"model": HAIKU, "external_addressable": true, "tools": {"comms": true}},
                }}}),
            );
        }
        if inputs.contains("AFTER") {
            return tool("k1", "contacts", json!({}));
        }
        Step::Text("ok")
    }));
    vec![step; 160]
}

/// A daemon on `root` whose classifier allows every call it reviews.
async fn daemon(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Running {
    let classifier = ScriptedClassifier::allowing(Vec::new());
    let setup = Setup::default().classifier(classifier as _);
    Running::start_with(&setup.command_line(root), setup.options(root, llm)).await
}

/// The helpers a `contacts` result lists, each without its `state` (how
/// long it has been idle, or that it runs).
fn helpers_listed(contacts: &str) -> Vec<serde_json::Value> {
    let contacts: serde_json::Value = serde_json::from_str(contacts).unwrap();
    let mut helpers: Vec<serde_json::Value> = contacts["your_helpers"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(|mut helper| {
            helper.as_object_mut().unwrap().remove("state");
            helper
        })
        .collect();
    helpers.sort_by_key(|helper| helper["name"].to_string());
    assert_eq!(helpers.len(), 4, "{contacts}");
    helpers
}

fn key(mob: &str, member: &str) -> HelperKey {
    HelperKey::new(MobId::from(mob), AgentIdentity::from(member))
}

/// Wait until each of `members` has ended a run and is idle.
async fn idle(daemon: &Running, members: &[(&str, &str)]) {
    let registry = daemon.registry();
    common::until(|| {
        members.iter().all(|(mob, member)| {
            registry
                .activity(&key(mob, member))
                .is_some_and(|(running, _)| !running)
        })
    })
    .await;
}

/// The killed side: build the team on the workspace `root`, say so, and
/// wait to be killed (bounded: a parent that is gone leaves no process).
async fn build(root: PathBuf) {
    common::capture_log();
    let llm = ScriptedLlmClient::new(script(true));
    let daemon = daemon(&root, &llm).await;
    daemon.reveal(&daemon.session, &MOB_TOOLS).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "BUILD").await;
    let (answer, error) = result_in(&llm, "BUILD", "c1").await;
    assert!(!error, "{answer}");
    assert!(
        answer.contains("`a` and `w1` (new)") && answer.contains("`w1` and `w2` (new)"),
        "{answer}"
    );
    // `w3` is in its first model call, which never returns.
    common::until(|| {
        llm.requests()
            .iter()
            .any(|m| common::is_helper(m, "TASK-OF-W3"))
    })
    .await;
    // The others' first runs ended, and the agent read `a`'s idle note.
    idle(&daemon, &[("tui", "a"), ("team", "w1"), ("team", "w2")]).await;
    common::until(|| heard(&llm, "BUILD", "Your helper `a` went idle")).await;
    daemon.idle(&daemon.agent).await;
    let (contacts, error) = result_in(&llm, "BUILD", "b1").await;
    assert!(!error, "{contacts}");
    let marker = root.parent().unwrap().join(BUILT);
    let tmp = marker.with_extension("tmp");
    std::fs::write(
        &tmp,
        format!("{}\n{}\n{contacts}", daemon.agent, daemon.session),
    )
    .unwrap();
    std::fs::rename(&tmp, &marker).unwrap();
    tokio::time::sleep(Duration::from_mins(2)).await;
    panic!("never killed");
}

/// A team an agent made with `mob_create`, `mob_spawn_member`, `assign`
/// (kept) and `connect_helpers` outlives a SIGKILL of its daemon (the
/// module's doc says what that skips). After the kill the endpoint file is
/// still there (no stop ran) and the owner record and the record of the
/// connections across mobs name the team. The daemon started again on the
/// workspace restores the agent's conversation (the same agent and session,
/// its transcript holding the build); lists `team` with its four members
/// on their profiles, the definition naming each profile's model, and
/// every helper bound to the agent's session; reports the edge within
/// `team`; the members talk within the mob and across mobs; the agent's
/// `contacts` lists its helpers as it did before the kill (their kinds,
/// connections and reachability); its message gives the kept helper `a`
/// more work, whose answer reaches it; and it seats one more member in
/// `team`. The member that was mid-run at the kill (`w3`, its model call
/// hanging) is restored seated, and its interrupted turn runs again: its
/// model is asked once more, its task given once, and its parent is told
/// its answer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::disallowed_methods)] // test-only process launch: the killed side
async fn a_team_the_agent_built_outlives_a_sigkill_of_its_daemon() {
    use std::os::unix::process::ExitStatusExt;
    if let Ok(root) = std::env::var(SIDE) {
        build(PathBuf::from(root)).await;
        return;
    }
    let ws = Workspace::new();
    let log = ws.sibling("killed.log");
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash::a_team_the_agent_built_outlives_a_sigkill_of_its_daemon",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SIDE, &ws.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&log).unwrap())
        .stderr(std::fs::File::create(ws.sibling("killed.err")).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let marker = ws.sibling(BUILT);
    let deadline = Instant::now() + Duration::from_mins(2);
    let built = loop {
        if let Ok(body) = std::fs::read_to_string(&marker) {
            break body;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "the killed side ended by itself ({status}):\n{}\n{}",
                std::fs::read_to_string(&log).unwrap_or_default(),
                std::fs::read_to_string(ws.sibling("killed.err")).unwrap_or_default()
            );
        }
        assert!(Instant::now() < deadline, "the team was never built");
        tokio::time::sleep(common::POLL).await;
    };
    let mut built = built.splitn(3, '\n');
    let (agent, session, contacts_before) = (
        built.next().unwrap(),
        built.next().unwrap(),
        built.next().unwrap(),
    );
    child.start_kill().unwrap();
    let status = common::within(child.wait()).await.unwrap();
    assert_eq!(status.signal(), Some(9), "{status}");

    // No stop ran: its first step removes the endpoint file.
    assert!(endpoint_file_for(&ws.root).exists());
    let owners: serde_json::Value = serde_json::from_slice(
        &std::fs::read(common::realm_of(&ws.root).join("tui_helpers.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(owners["mobs"]["team"], session, "{owners}");
    for (mob, member) in [("tui", "a"), ("team", "w1"), ("team", "w2"), ("team", "w3")] {
        assert!(
            owners["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| { m["mob"] == mob && m["member"] == member && m["owner"] == session }),
            "{mob}/{member}: {owners}"
        );
    }
    let links = connect::record(&ws);
    assert!(
        links.contains("\"w1\"") && links.contains("tui/main/a"),
        "{links}"
    );

    // The same workspace, a new daemon.
    let llm = ScriptedLlmClient::new(script(false));
    let daemon = daemon(&ws.root, &llm).await;
    assert!(!daemon.created, "the agent is restored");
    assert_eq!(daemon.agent, agent);
    assert_eq!(daemon.session.to_string(), session);
    let transcript = format!("{:?}", daemon.transcript(&daemon.session).await);
    assert!(
        transcript.contains("connect_helpers") && transcript.contains("built"),
        "{transcript}"
    );

    // The team: its members on their profiles, the profiles' models.
    let state = daemon.state();
    let team = MobId::from("team");
    let members = state.mob_list_members(&team).await.unwrap();
    let roles: Vec<(String, String)> = members
        .iter()
        .map(|m| (m.agent_identity.to_string(), m.role.to_string()))
        .collect();
    for (member, role) in [("w1", "worker"), ("w2", "worker"), ("w3", "lead")] {
        assert!(
            roles.contains(&(member.to_string(), role.to_string())),
            "{roles:?}"
        );
    }
    let handle = state.handle_for(&team).await.unwrap();
    let definition = serde_json::to_value(handle.definition()).unwrap();
    assert_eq!(
        definition["profiles"]["worker"]["model"],
        common::MODEL,
        "{definition}"
    );
    assert_eq!(
        definition["profiles"]["lead"]["model"], HAIKU,
        "{definition}"
    );
    let w1 = members
        .iter()
        .find(|m| m.agent_identity.as_str() == "w1")
        .unwrap();
    assert!(
        w1.wired_to.contains(&AgentIdentity::from("w2")),
        "{:?}",
        w1.wired_to
    );
    assert!(
        common::members(&daemon.connect().await, "tui")
            .await
            .contains(&"a".to_string())
    );
    let bound = daemon.registry().bound_members();
    for (mob, member) in [("tui", "a"), ("team", "w1"), ("team", "w2"), ("team", "w3")] {
        assert!(
            bound
                .iter()
                .any(|(k, tag)| *k == key(mob, member) && tag.owner.to_string() == session),
            "{mob}/{member}: {bound:?}"
        );
    }

    // The wiring talks: within `team`, and across mobs.
    let connection = daemon.connect_bound().await;
    connect::relay(&connection, &llm, W1, 1, "TASK-OF-W2").await;
    connect::relay(&connection, &llm, W2, 2, "TASK-OF-W1").await;
    connect::relay(&connection, &llm, A, 3, "TASK-OF-W1").await;

    // The agent uses its team.
    daemon.prompt(&connection, "AFTER").await;
    let (contacts, error) = result_in(&llm, "BUILD", "k1").await;
    assert!(!error, "{contacts}");
    // What the agent's `contacts` says of its helpers is what it said
    // before the kill, but for how long each has been idle.
    assert_eq!(
        helpers_listed(&contacts),
        helpers_listed(contacts_before),
        "before: {contacts_before}\nafter: {contacts}"
    );
    let (sent, error) = result_in(&llm, "BUILD", "k2").await;
    assert!(!error && sent.contains("\"sent\""), "{sent}");
    let (seated, error) = result_in(&llm, "BUILD", "k3").await;
    assert!(!error, "{seated}");
    common::until(|| heard(&llm, "BUILD", "A-SECOND-ANSWER")).await;
    let roles = state.mob_list_members(&team).await.unwrap();
    assert!(
        roles
            .iter()
            .any(|m| m.agent_identity.as_str() == "w4" && m.role.as_str() == "worker"),
        "{roles:?}"
    );

    // The member mid-run at the kill: its interrupted turn ran again (its
    // model asked once, from its task), and its parent was told its answer.
    idle(&daemon, &[("team", "w3")]).await;
    common::until(|| heard(&llm, "BUILD", "W3-AFTER-RESTART")).await;
    let reruns: Vec<Vec<String>> = llm
        .requests()
        .iter()
        .filter(|m| common::is_helper(m, "TASK-OF-W3"))
        .map(Vec::as_slice)
        .map(common::user_texts)
        .collect();
    assert_eq!(reruns.len(), 1, "{reruns:?}");
    assert_eq!(
        reruns[0]
            .iter()
            .filter(|t| t.contains("TASK-OF-W3"))
            .count(),
        1,
        "{reruns:?}"
    );

    idle(
        &daemon,
        &[("tui", "a"), ("team", "w1"), ("team", "w2"), ("team", "w4")],
    )
    .await;
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}
