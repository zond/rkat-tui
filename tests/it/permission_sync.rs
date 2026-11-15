//! The permission sections of the user's own `tui.toml` and the
//! workspace's follow a running daemon: a rule, an `[auto] allow`
//! statement or a `[confinement]` directory added or removed by hand (or
//! by another daemon) applies at once, computed as at start, told to
//! every client once with counts and audited; a bad entry is skipped and
//! named while the rest applies; the project's `.rkat/tui.toml` changes
//! nothing live.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{ScriptedLlmClient, Workspace};
use rig::{Attached, Running, Setup};
use rkat_tui::approval::ApprovalEngine;
use rkat_tui::approval::catalog::ToolOrigin;
use rkat_tui::approval::desk::RuleRows;
use rkat_tui::approval::engine::ToolCall;
use rkat_tui::approval::grants::{GrantOrigin, GrantScope, GrantTarget};
use rkat_tui::approval::policy::{Tier, VerdictSummary};
use rkat_tui::approval::statements::StatementOrigin;
use rkat_tui::ui::Msg;
use serde_json::json;

/// The rows `/permissions` shows, through the daemon.
async fn rows(daemon: &Running) -> RuleRows {
    let rows = daemon
        .connect_bound()
        .await
        .call("rkat/rule_rows", json!({}))
        .await
        .unwrap();
    serde_json::from_value(rows).unwrap()
}

/// How the gate decides `name` with `args` for `session` now.
async fn decide(
    engine: &ApprovalEngine,
    session: &meerkat_core::SessionId,
    name: &str,
    args: &serde_json::Value,
) -> VerdictSummary {
    let raw = serde_json::value::to_raw_value(args).unwrap();
    let origin = if name == "shell" {
        ToolOrigin::native(meerkat_core::ToolSourceKind::Builtin)
    } else {
        ToolOrigin::tui(rkat_tui::tools::FILE_TOOLS_SOURCE, name)
    };
    engine
        .decide(
            session,
            ToolCall {
                id: "c1",
                name,
                args: &raw,
                origin: Some(&origin),
            },
        )
        .await
        .summary()
}

/// The notice every client is told when a file's permissions change.
async fn told(rig: &mut Attached, start: &str) -> String {
    let notice = rig
        .saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with(start)))
        .await;
    let Msg::Notice { text, .. } = notice else {
        unreachable!()
    };
    text
}

/// A statement added by hand to your own `tui.toml` is in force without
/// a restart (`/permissions` lists it), told once with its count, and
/// audited. Before, it applied at the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_statement_added_by_hand_to_your_file_applies_without_a_restart() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    let user_file = common::config_dir_for(&ws.root).join("tui.toml");
    std::fs::write(
        &user_file,
        "# mine\n[auto]\nallow = [\"network requests to api.example.com\"]\n",
    )
    .unwrap();
    let text = told(&mut rig, "your tui.toml changed:").await;
    assert_eq!(
        text,
        "your tui.toml changed: 1 statement added; in force now"
    );
    let statements = rows(&rig.daemon).await.statements;
    let row = statements
        .iter()
        .find(|row| row.text == "network requests to api.example.com")
        .unwrap_or_else(|| panic!("{statements:?}"));
    assert_eq!(row.origin, StatementOrigin::Config);
    let audit = std::fs::read_to_string(common::realm_of(&ws.root).join("audit.jsonl")).unwrap();
    assert!(
        audit.contains("statement_added") && audit.contains("your tui.toml changed"),
        "{audit}"
    );
    rig.quit().await;
}

/// A directory granted "always" and then removed by hand from the
/// workspace's file stops applying at once: a file-tool write there is an
/// in-project edit before and goes to the classifier after. The grant
/// notice's limit (the confined shell from the next start) is the one a
/// newly granted directory's notice repeats.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_grant_removed_by_hand_stops_applying() {
    let ws = Workspace::new();
    let granted = ws.sibling("granted");
    std::fs::create_dir_all(&granted).unwrap();
    let granted = std::fs::canonicalize(&granted).unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    let session = rig.daemon.session.clone();
    let approved = rig
        .daemon
        .engine()
        .grant(
            &session,
            GrantTarget::Dir {
                path: granted.clone(),
            },
            GrantScope::Always,
        )
        .unwrap();
    assert!(
        approved
            .notice
            .ends_with(&format!("({})", rkat_tui::approval::ALWAYS_DIR_LIMIT)),
        "{}",
        approved.notice
    );
    let write = json!({"path": granted.join("out.txt").display().to_string(), "content": "x"});
    assert!(matches!(
        decide(&rig.daemon.engine(), &session, "write_file", &write).await,
        VerdictSummary::Allowed {
            tier: Tier::InProjectEdit,
            ..
        }
    ));
    let realm_file = common::realm_of(&ws.root).join("tui.toml");
    let text = std::fs::read_to_string(&realm_file).unwrap();
    assert!(text.contains(&granted.display().to_string()), "{text}");
    std::fs::write(&realm_file, "# granted by hand, now gone\n").unwrap();
    let text = told(&mut rig, "this workspace's tui.toml changed:").await;
    assert_eq!(
        text,
        "this workspace's tui.toml changed: 1 granted directory removed; in force now"
    );
    assert!(matches!(
        decide(&rig.daemon.engine(), &session, "write_file", &write).await,
        VerdictSummary::Allowed {
            tier: Tier::Classifier,
            ..
        }
    ));
    let grants = rows(&rig.daemon).await.grants;
    assert!(
        !grants.iter().any(|row| row.target
            == GrantTarget::Dir {
                path: granted.clone()
            }),
        "{grants:?}"
    );
    rig.quit().await;
}

/// A deny rule added by hand to the workspace's file applies at once:
/// `/permissions` lists it as the realm's and the gate denies by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rule_added_by_hand_to_the_workspace_file_applies() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    let session = rig.daemon.session.clone();
    let push = json!({"command": "git push origin main"});
    assert!(matches!(
        decide(&rig.daemon.engine(), &session, "shell", &push).await,
        VerdictSummary::Allowed { .. }
    ));
    let realm_file = common::realm_of(&ws.root).join("tui.toml");
    let before = std::fs::read_to_string(&realm_file).unwrap_or_default();
    std::fs::write(
        &realm_file,
        format!("{before}\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n"),
    )
    .unwrap();
    let text = told(&mut rig, "this workspace's tui.toml changed:").await;
    assert_eq!(
        text,
        "this workspace's tui.toml changed: 1 rule added; in force now"
    );
    let rules = rows(&rig.daemon).await.rules;
    assert!(
        rules.iter().any(|rule| rule.deny
            && rule.arg.as_deref() == Some("git push*")
            && rule.origin == rkat_tui::approval::rules::RuleOrigin::Realm),
        "{rules:?}"
    );
    assert!(matches!(
        decide(&rig.daemon.engine(), &session, "shell", &push).await,
        VerdictSummary::Denied {
            tier: Tier::UserDeny(_),
            ..
        }
    ));
    rig.quit().await;
}

/// The project's `.rkat/tui.toml` is writable by the agent's own shell: a
/// change of its sections changes nothing live (its deny rule is not in
/// force, no statement of it ever is); it is named as applying at the
/// next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_section_change_in_the_project_file_changes_nothing_live() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    let session = rig.daemon.session.clone();
    let project_file = ws.root.join(".rkat").join("tui.toml");
    std::fs::create_dir_all(project_file.parent().unwrap()).unwrap();
    std::fs::write(
        &project_file,
        "[auto]\nallow = [\"network requests to api.example.com\"]\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    )
    .unwrap();
    told(
        &mut rig,
        "the rules, statements, grants, endpoints, prices or thinking of the project's .rkat/tui.toml changed: they apply at the next start",
    )
    .await;
    let rows = rows(&rig.daemon).await;
    assert!(rows.rules.is_empty(), "{:?}", rows.rules);
    assert!(rows.statements.is_empty(), "{:?}", rows.statements);
    assert!(matches!(
        decide(
            &rig.daemon.engine(),
            &session,
            "shell",
            &json!({"command": "git push origin main"})
        )
        .await,
        VerdictSummary::Allowed { .. }
    ));
    assert!(!rig.notices().contains("in force now"), "{}", rig.notices());
    rig.quit().await;
}

/// A bad entry beside a good one: the good rule and statement apply, the
/// rule that does not parse and the statement that allows anything are
/// skipped and named in the notice (a warning).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_entry_is_skipped_and_named_while_the_good_one_applies() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    let user_file = common::config_dir_for(&ws.root).join("tui.toml");
    std::fs::write(
        &user_file,
        "[auto]\nallow = [\"anything\", \"network requests to api.example.com\"]\n[[deny]]\ntool = \"[\"\n[[deny]]\ntool = \"shell\"\narg = \"rm -rf *\"\n",
    )
    .unwrap();
    let text = told(&mut rig, "your tui.toml changed:").await;
    assert!(
        text.starts_with(
            "your tui.toml changed: 1 rule added, 1 statement added; in force now; skipped: "
        ),
        "{text}"
    );
    assert!(text.contains("invalid pattern `[`"), "{text}");
    assert!(
        text.contains("the next start refuses the file until it is fixed"),
        "{text}"
    );
    assert!(
        text.contains("the statement \"anything\" of your tui.toml (the statement allows anything"),
        "{text}"
    );
    let rows = rows(&rig.daemon).await;
    assert_eq!(rows.rules.len(), 1, "{:?}", rows.rules);
    assert_eq!(rows.rules[0].arg.as_deref(), Some("rm -rf *"));
    assert_eq!(rows.statements.len(), 1, "{:?}", rows.statements);
    rig.quit().await;
}

/// Two daemons on one user's `tui.toml` (the first its own process, the
/// real `rkat-tui-mob`): a directory and a statement granted "always, in
/// every workspace" in the second (what `/approve`'s `A` does) reach the
/// first while it runs, in force there as your file's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_in_every_workspace_in_one_daemon_reaches_the_other() {
    let first = Workspace::new();
    let second = Workspace::new();
    let cfg = first.sibling("shared-cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    // Not beside the first's endpoint file: its directory is sealed there
    // (that daemon would skip the grant and say so).
    let granted = second.sibling("granted");
    std::fs::create_dir_all(&granted).unwrap();
    let granted = std::fs::canonicalize(&granted).unwrap();
    // The process daemon reaches no model: an endpoint it never calls.
    std::fs::write(
        cfg.join("tui.toml"),
        "[endpoints.fake]\ndialect = \"anthropic\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"HOME\"\n",
    )
    .unwrap();
    let cfg_arg = cfg.to_str().unwrap().to_string();
    let args_a = common::args_with(&first.root, &["--config-dir", &cfg_arg]);
    let file_a = first.sibling("first.endpoint");
    let argv = rkat_tui::client::launch::with_endpoint_file(
        &rkat_tui::client::launch::daemon_argv(&args_a, &first.root),
        &file_a,
    );
    let _first_daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_rkat-tui-mob"))
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    common::until(|| file_a.exists()).await;
    let endpoint_a = rkat_tui::daemon::endpoint::Endpoint::read(&file_a).unwrap();
    let rows_a = async || -> RuleRows {
        let (connection, _notes) = rkat_tui::client::transport::Connection::attach_as(
            &endpoint_a,
            std::time::Duration::from_secs(5),
            None,
            true,
        )
        .await
        .unwrap();
        connection.call("initialize", json!({})).await.unwrap();
        serde_json::from_value(connection.call("rkat/rule_rows", json!({})).await.unwrap()).unwrap()
    };
    let (watching, mut notes) = rkat_tui::client::transport::Connection::attach_as(
        &endpoint_a,
        std::time::Duration::from_secs(5),
        None,
        true,
    )
    .await
    .unwrap();
    watching.call("initialize", json!({})).await.unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let b = Running::start_with(
        &common::args_with(&second.root, &["--config-dir", &cfg_arg]),
        Setup::args(&["--config-dir", &cfg_arg]).options(&second.root, &llm),
    )
    .await;
    let engine = b.engine();
    engine
        .grant(
            &b.session,
            GrantTarget::Dir {
                path: granted.clone(),
            },
            GrantScope::AlwaysUser,
        )
        .unwrap();
    engine
        .allow_statement(
            &b.session,
            "network requests to api.example.com",
            GrantScope::AlwaysUser,
        )
        .unwrap();
    drop(engine);
    let reached = |rows: &RuleRows| {
        rows.statements.iter().any(|row| {
            row.text == "network requests to api.example.com"
                && row.origin == StatementOrigin::Config
        }) && rows.grants.iter().any(|row| {
            row.target
                == GrantTarget::Dir {
                    path: granted.clone(),
                }
                && row.origin == GrantOrigin::Config
        })
    };
    let mut last = rows_a().await;
    for _ in 0..80 {
        if reached(&last) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        last = rows_a().await;
    }
    let mut told = Vec::new();
    while let Ok(note) = notes.try_recv() {
        if note.method == rkat_tui::daemon::protocol::AGENT_NOTICE {
            told.push(note.params);
        }
    }
    assert!(
        reached(&last),
        "the first daemon holds {:?} and {:?}; the file: {}; it told: {told:?}",
        last.statements,
        last.grants,
        std::fs::read_to_string(cfg.join("tui.toml")).unwrap(),
    );
    drop(watching);
    b.stop().await;
}
