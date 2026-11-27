//! OAuth logins of remote MCP servers (`/mcp`, `/mcp login`, `/mcp
//! logout`), end to end through the daemon and an attach client, against
//! the engine's MCP OAuth fixture (one loopback server: the protected MCP
//! endpoint, its resource and authorization server metadata, dynamic
//! registration, authorize, token, UserInfo). Each test keeps its
//! credentials in a directory of its own (`Setup::credential_root`),
//! never the user's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use meerkat::test_fixtures::mcp_oauth::{
    ECHO_REPLY, ISSUED_SECRET_CANARIES, McpOAuthFixture, follow_authorize_url,
};
use rkat_tui::daemon::protocol::{McpLoginState, McpServerList};
use rkat_tui::ui::modal::{McpLoginShown, Secret};
use rkat_tui::ui::{Effect, Msg, NoticeLevel};

use crate::common::{self, ScriptedLlmClient, Step, Workspace};
use crate::rig::{Attached, Client, Running, Setup, client_config};

/// A `--mcp-server` value for the fixture's protected endpoint, marked for
/// OAuth with `selection`.
fn server_flag(fixture: &McpOAuthFixture, name: &str, selection: &str) -> String {
    format!(
        r#"name="{name}", url="{}", oauth_account_selection="{selection}""#,
        fixture.mcp_url()
    )
}

/// The setup of a daemon with `servers` (`--mcp-server` values) and the
/// credential store `credentials`.
fn setup(servers: &[String], credentials: &Path) -> Setup {
    let mut args = Vec::new();
    for server in servers {
        args.push("--mcp-server".to_string());
        args.push(server.clone());
    }
    Setup {
        args,
        credential_root: Some(credentials.to_path_buf()),
        ..Setup::default()
    }
}

/// A model that calls the MCP tool named in `echo` when the user's words
/// say `CALL-ECHO`, says "echoed" after its result, else "ok".
fn llm(echo: &Arc<parking_lot::Mutex<&'static str>>) -> Arc<ScriptedLlmClient> {
    let echo = Arc::clone(echo);
    let step = Step::Dynamic(Arc::new(move |messages| {
        if common::answers(messages, Some("e1")) {
            Step::Text("echoed")
        } else if common::run_inputs(messages).contains("CALL-ECHO") {
            Step::Tool {
                id: "e1",
                name: *echo.lock(),
                args: serde_json::json!({}),
            }
        } else {
            Step::Text("ok")
        }
    }));
    ScriptedLlmClient::new(vec![step; 60])
}

/// The name of the fixture's `echo` tool in the request `n` and later, if
/// one offers it.
fn echo_tool(llm: &ScriptedLlmClient, from: usize) -> Option<String> {
    llm.recorded()
        .into_iter()
        .skip(from)
        .flat_map(|request| request.tools)
        .find(|name| name.contains("echo"))
}

/// Run turns until a request offers the fixture's `echo` tool (the
/// server is connected at a turn's boundary): its name.
async fn until_echo_offered(client: &mut Client, llm: &ScriptedLlmClient) -> String {
    let from = llm.recorded().len();
    // The daemon looks at the sessions' waits every 2 s.
    for _ in 0..40 {
        if let Some(name) = echo_tool(llm, from) {
            return name;
        }
        client.send(Effect::StartTurn("ping".into()));
        client.turn_finished().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    echo_tool(llm, from).unwrap_or_else(|| {
        let log: Vec<String> = common::log_tail(5_000)
            .lines()
            .filter(|line| line.to_ascii_lowercase().contains("mcp"))
            .map(str::to_string)
            .collect();
        panic!(
            "no request offered echo; tools of the last: {:?}; the MCP log lines: {log:#?}",
            llm.recorded().last().map(|r| r.tools.clone())
        )
    })
}

/// `/mcp login <server>` until its dialog opens: what it shows.
async fn login_opened(client: &mut Client, server: &str) -> McpLoginShown {
    client.send(Effect::McpLogin {
        server: server.to_string(),
        confirmed: false,
    });
    match client
        .wait_for(|msg| matches!(msg, Msg::McpLoginOpen(_)))
        .await
    {
        Msg::McpLoginOpen(shown) => shown,
        _ => unreachable!(),
    }
}

/// The login's end: its level and text.
async fn login_closed(client: &mut Client) -> (NoticeLevel, String) {
    match client
        .wait_for(|msg| matches!(msg, Msg::McpLoginClosed { .. }))
        .await
    {
        Msg::McpLoginClosed { level, text } => (level, text),
        _ => unreachable!(),
    }
}

/// The browser on this machine: it follows the authorize URL, and the
/// redirect reaches the client's listener.
async fn browse(shown: &McpLoginShown) {
    follow_authorize_url(&shown.authorize_url.0).await.unwrap();
}

/// The browser elsewhere: the address its redirect lands on (the
/// authorize endpoint's `Location`), as the user would copy it.
async fn redirected_address(shown: &McpLoginShown) -> String {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(&shown.authorize_url.0)
        .send()
        .await
        .unwrap();
    response.headers()["location"].to_str().unwrap().to_string()
}

/// The daemon's `/mcp` rows.
async fn servers(daemon: &Running) -> McpServerList {
    let answer = daemon
        .connect()
        .await
        .call("rkat/mcp_servers", serde_json::json!({}))
        .await
        .unwrap();
    serde_json::from_value(answer).unwrap()
}

fn login_of(list: &McpServerList, name: &str) -> McpLoginState {
    list.servers
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("no {name} in {list:?}"))
        .login
        .clone()
}

/// Every file under `dir` but `except`, each by its path, as text.
fn files_text(dir: &Path, except: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.starts_with(except) {
            continue;
        }
        if path.is_dir() {
            files_text(&path, except, out);
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push((
                path.display().to_string(),
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
    }
}

/// A login through the listener makes the server's tools reach the agent
/// in the running daemon (no restart), the tool answers, and none of the
/// fixture's issued secrets nor the attempt's URL and state appear in the
/// transcript, the realm's files (the audit log, the sessions), the log,
/// or anything the client was told. The daemon said at its start that
/// the server needs a login, from the stored state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_brings_the_server_s_tools_without_a_restart_and_leaks_no_secret() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    let echo = Arc::new(parking_lot::Mutex::new("echo"));
    let llm = llm(&echo);
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        setup(&[server_flag(&fixture, "docs", "discover")], &credentials),
    )
    .await;
    rig.saw(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.contains("MCP server `docs` needs login: /mcp login docs"))
    })
    .await;
    assert_eq!(
        login_of(&servers(&rig.daemon).await, "docs"),
        McpLoginState::NeedsLogin
    );
    rig.send(Effect::StartTurn("hi".into()));
    rig.turn_finished().await.unwrap();
    assert!(echo_tool(&llm, 0).is_none(), "offered before the login");

    let shown = login_opened(&mut rig.client, "docs").await;
    assert!(
        shown.redirect_uri.starts_with("http://127.0.0.1:"),
        "{shown:?}"
    );
    assert!(shown.note.is_none(), "{shown:?}");
    let state = reqwest::Url::parse(&shown.authorize_url.0)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    browse(&shown).await;
    let (level, text) = login_closed(&mut rig.client).await;
    assert_eq!(level, NoticeLevel::Info, "{text}");
    assert!(text.contains("logged in to MCP server `docs`"), "{text}");
    assert!(matches!(
        login_of(&servers(&rig.daemon).await, "docs"),
        McpLoginState::Authorized { .. }
    ));

    let name = until_echo_offered(&mut rig.client, &llm).await;
    *echo.lock() = common::leak(name);
    rig.send(Effect::StartTurn("CALL-ECHO".into()));
    rig.turn_finished().await.unwrap();
    let messages = rig.messages().await;
    assert!(common::tool_text(&messages, "e1").contains(ECHO_REPLY));

    // Nothing of the login anywhere the agent, the log or a file reaches.
    let mut seen = vec![
        (
            "the transcript".to_string(),
            serde_json::to_string(&messages).unwrap(),
        ),
        ("the log".to_string(), common::log_tail(1_000_000)),
        (
            "the client's messages".to_string(),
            format!("{:?}", rig.client.seen),
        ),
    ];
    files_text(&common::config_dir_for(&ws.root), &credentials, &mut seen);
    files_text(&ws.root, &credentials, &mut seen);
    for secret in ISSUED_SECRET_CANARIES {
        for (source, text) in &seen {
            assert!(!text.contains(secret), "{secret} leaked into {source}");
        }
    }
    // The attempt's state is the engine's to keep while it is pending: its
    // OAuth flow owner holds it in the realm's runtime store
    // (`sessions.sqlite3`, `auth_oauth_flow_state`), which neither the
    // agent nor its shell reaches. Nowhere else.
    for (source, text) in &seen {
        let engine_store = source.contains("sessions.sqlite3");
        assert!(
            engine_store || !text.contains(&state),
            "the attempt's state leaked into {source}"
        );
    }
    // The credential is where the store keeps it, owner-only.
    let mut stored = Vec::new();
    files_text(&credentials, Path::new("/nonexistent"), &mut stored);
    assert!(
        stored
            .iter()
            .any(|(_, text)| text.contains(meerkat::test_fixtures::mcp_oauth::ACCESS_CANARY))
    );
    rig.quit().await;
}

/// The browser elsewhere: the redirect cannot reach the listener (here
/// its predefined port is taken, which the dialog says, naming the port
/// and the setting), and the address the browser shows, pasted, completes
/// the login. An address of another state or another redirect is refused
/// in the dialog and the login stays pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pasted_address_completes_the_login_and_a_wrong_one_is_refused() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let redirect = format!("http://127.0.0.1:{port}/oauth/back");
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut setup = setup(&[server_flag(&fixture, "docs", "unverified")], &credentials);
    setup.args.extend([
        "--set".to_string(),
        format!("mcp.oauth_redirect_uri={redirect}"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    let shown = login_opened(&mut rig.client, "docs").await;
    assert_eq!(shown.redirect_uri, redirect);
    let note = shown.note.clone().unwrap();
    assert!(
        note.contains(&port.to_string()) && note.contains("mcp.oauth_redirect_uri"),
        "{note}"
    );
    let address = redirected_address(&shown).await;
    assert!(address.starts_with(&redirect), "{address}");

    for wrong in [
        address.replace("state=", "state=x"),
        address.replace("/oauth/back", "/elsewhere"),
    ] {
        rig.send(Effect::McpLoginPasted(Secret(wrong)));
        let refused = rig
            .wait_for(|msg| matches!(msg, Msg::McpLoginPasteRefused(_)))
            .await;
        assert!(matches!(refused, Msg::McpLoginPasteRefused(_)));
    }
    rig.send(Effect::McpLoginPasted(Secret(address)));
    let (level, text) = login_closed(&mut rig.client).await;
    assert_eq!(level, NoticeLevel::Info, "{text}");
    assert!(matches!(
        login_of(&servers(&rig.daemon).await, "docs"),
        McpLoginState::Authorized { .. }
    ));
    drop(taken);
    rig.quit().await;
}

/// A server not marked for OAuth has no login (the refusal says how to
/// mark it); one that is not configured, none either; `/mcp` lists the
/// unmarked one as not an OAuth server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unmarked_server_is_refused_with_how_to_mark_it() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let plain = format!(r#"name="plain", url="{}""#, fixture.public_mcp_url());
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&[plain], &credentials)).await;
    assert_eq!(
        login_of(&servers(&rig.daemon).await, "plain"),
        McpLoginState::NotMarked
    );
    for (server, says) in [
        ("plain", "oauth_account_selection = \"unverified\""),
        ("nothing", "no MCP server named `nothing`"),
    ] {
        rig.send(Effect::McpLogin {
            server: server.to_string(),
            confirmed: false,
        });
        rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains(says)))
            .await;
    }
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::McpLoginOpen(_)))
    );
    rig.quit().await;
}

/// `/mcp logout` removes the credential: `/mcp` says it needs a login
/// again, and once a turn's boundary releases the server, the agent's
/// session awaits a login and the clients are told so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_removes_the_login_and_the_session_asks_for_one_again() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    let echo = Arc::new(parking_lot::Mutex::new("echo"));
    let llm = llm(&echo);
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        setup(&[server_flag(&fixture, "docs", "unverified")], &credentials),
    )
    .await;
    let shown = login_opened(&mut rig.client, "docs").await;
    browse(&shown).await;
    login_closed(&mut rig.client).await;
    until_echo_offered(&mut rig.client, &llm).await;
    rig.drain();
    let before = rig.seen.len();
    rig.send(Effect::McpLogout("docs".into()));
    rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("logged out of MCP server `docs`")))
        .await;
    assert_eq!(
        login_of(&servers(&rig.daemon).await, "docs"),
        McpLoginState::NeedsLogin
    );
    rig.send(Effect::StartTurn("after".into()));
    rig.turn_finished().await.unwrap();
    rig.saw(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.contains("MCP server `docs` needs login"))
    })
    .await;
    assert!(rig.seen.len() > before);
    rig.quit().await;
}

/// Over an unencrypted attach (`--attach-insecure` to a network address)
/// no login starts, and the refusal says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_login_starts_over_an_insecure_attach() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let setup = setup(&[server_flag(&fixture, "docs", "unverified")], &credentials);
    let daemon =
        Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await;
    let mut config = client_config(
        &setup.command_line(&ws.root),
        None,
        common::no_lists(),
        &setup.endpoint_file(&ws.root),
        None,
    )
    .await;
    config.insecure = true;
    let mut client = Client::attach_with_config(config).await;
    client.send(Effect::McpLogin {
        server: "docs".into(),
        confirmed: false,
    });
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { level: NoticeLevel::Error, text } if text.contains("--attach-insecure")))
        .await;
    assert!(
        !client
            .seen
            .iter()
            .any(|msg| matches!(msg, Msg::McpLoginOpen(_)))
    );
    assert_eq!(
        login_of(&servers(&daemon).await, "docs"),
        McpLoginState::NeedsLogin
    );
    client.quit().await;
    daemon.stop().await;
}

/// The command line of a workspace whose project `.rkat/mcp.toml` defines
/// `docs`, with the user's own config elsewhere (`trusted`: with
/// `--trust-project`).
fn project_args(ws: &Workspace, trusted: bool) -> rkat_tui::Args {
    let home = ws.sibling("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = ws.root.to_str().unwrap().to_string();
    let cfg = common::config_dir_for(&ws.root);
    let mut argv = vec![
        "rkat-tui".to_string(),
        "-w".into(),
        root,
        "--user-config-root".into(),
        home.to_str().unwrap().into(),
        "--config-dir".into(),
        cfg.to_str().unwrap().into(),
        "--no-pricing-fetch".into(),
        "--model".into(),
        common::MODEL.into(),
        "--no-confinement".into(),
    ];
    if trusted {
        argv.push("--trust-project".into());
    }
    <rkat_tui::Args as clap::Parser>::try_parse_from(argv).unwrap()
}

/// A server of a trusted project `mcp.toml` is logged into only after the
/// dialog showed its URL and the user said yes; one of an untrusted file
/// does not exist for login (and `/mcp` says why it did not start).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_project_server_asks_first_and_an_untrusted_one_cannot_log_in() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(
        ws.root.join(".rkat/mcp.toml"),
        format!(
            "[[servers]]\nname = \"docs\"\nurl = \"{}\"\noauth_account_selection = \"unverified\"\n",
            fixture.mcp_url()
        ),
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let setup = Setup {
        credential_root: Some(credentials.clone()),
        ..Setup::default()
    };
    for trusted in [true, false] {
        let args = project_args(&ws, trusted);
        let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
        let mut client = Client::attach_setup(
            &args,
            None,
            common::no_lists(),
            &setup.endpoint_file(&ws.root),
        )
        .await;
        client.send(Effect::McpLogin {
            server: "docs".into(),
            confirmed: false,
        });
        if trusted {
            let asked = client
                .wait_for(|msg| matches!(msg, Msg::McpLoginConfirm { .. }))
                .await;
            let Msg::McpLoginConfirm { server, url } = asked else {
                unreachable!()
            };
            assert_eq!((server.as_str(), url), ("docs", fixture.mcp_url()));
            assert!(
                !client
                    .seen
                    .iter()
                    .any(|msg| matches!(msg, Msg::McpLoginOpen(_)))
            );
            let shown = login_opened_confirmed(&mut client).await;
            browse(&shown).await;
            let (level, text) = login_closed(&mut client).await;
            assert_eq!(level, NoticeLevel::Info, "{text}");
        } else {
            client
                .wait_for(
                    |msg| matches!(msg, Msg::Notice { text, .. } if text.contains("not trusted")),
                )
                .await;
            assert!(
                !client
                    .seen
                    .iter()
                    .any(|msg| matches!(msg, Msg::McpLoginConfirm { .. } | Msg::McpLoginOpen(_)))
            );
            assert_eq!(
                login_of(&servers(&daemon).await, "docs"),
                McpLoginState::Untrusted
            );
        }
        client.quit().await;
        daemon.stop().await;
    }
}

/// The user's yes to a project server's URL.
async fn login_opened_confirmed(client: &mut Client) -> McpLoginShown {
    client.send(Effect::McpLogin {
        server: "docs".into(),
        confirmed: true,
    });
    match client
        .wait_for(|msg| matches!(msg, Msg::McpLoginOpen(_)))
        .await
    {
        Msg::McpLoginOpen(shown) => shown,
        _ => unreachable!(),
    }
}

/// One login per user: logged into in workspace A, the same server (name
/// and URL) is authorized in workspace B, whose agent gets its tools with
/// no login of its own, and B said at its start that nothing needs one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_login_serves_every_workspace_of_the_user() {
    let fixture = McpOAuthFixture::spawn().await.unwrap();
    let a = Workspace::new();
    let b = Workspace::new();
    let credentials = a.sibling("credentials");
    let flag = server_flag(&fixture, "docs", "unverified");
    let echo = Arc::new(parking_lot::Mutex::new("echo"));
    let llm_a = llm(&echo);
    let mut in_a = Attached::start_with(
        &a.root,
        &llm_a,
        setup(std::slice::from_ref(&flag), &credentials),
    )
    .await;
    let shown = login_opened(&mut in_a.client, "docs").await;
    browse(&shown).await;
    let (level, text) = login_closed(&mut in_a.client).await;
    assert_eq!(level, NoticeLevel::Info, "{text}");
    in_a.quit().await;

    let llm_b = llm(&echo);
    let mut in_b = Attached::start_with(&b.root, &llm_b, setup(&[flag], &credentials)).await;
    assert!(matches!(
        login_of(&servers(&in_b.daemon).await, "docs"),
        McpLoginState::Authorized { .. }
    ));
    let name = until_echo_offered(&mut in_b.client, &llm_b).await;
    *echo.lock() = common::leak(name);
    in_b.send(Effect::StartTurn("CALL-ECHO".into()));
    in_b.turn_finished().await.unwrap();
    assert!(common::tool_text(&in_b.messages().await, "e1").contains(ECHO_REPLY));
    assert!(
        !in_b.notices().contains("needs login"),
        "{}",
        in_b.notices()
    );
    in_b.quit().await;
}

/// The loopback redirect in every spelling the engine accepts, each with
/// a predefined port and path (`mcp.oauth_redirect_uri`, the client's
/// value: the daemon takes the redirect the client's start names): the
/// listener is bound there (for `localhost` on both loopback addresses)
/// and the browser's redirect completes the login.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_predefined_redirect_in_each_loopback_spelling_completes_the_login() {
    let ws = Workspace::new();
    let credentials = ws.sibling("credentials");
    let mut fixtures = Vec::new();
    let mut flags = Vec::new();
    for name in ["v4", "lh", "v6"] {
        let fixture = McpOAuthFixture::spawn().await.unwrap();
        flags.push(server_flag(&fixture, name, "unverified"));
        fixtures.push(fixture);
    }
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let setup = setup(&flags, &credentials);
    let daemon =
        Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await;
    let free = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let v6 = std::net::TcpListener::bind("[::1]:0").is_ok();
    for (name, host) in [("v4", "127.0.0.1"), ("lh", "localhost"), ("v6", "[::1]")] {
        if host == "[::1]" && !v6 {
            println!("SKIPPED: [::1]: this host has no IPv6 loopback");
            continue;
        }
        let redirect = format!("http://{host}:{}/cb/{name}", free());
        let mut config = client_config(
            &setup.command_line(&ws.root),
            None,
            common::no_lists(),
            &setup.endpoint_file(&ws.root),
            None,
        )
        .await;
        config.mcp_redirect = Some(redirect.clone());
        let mut client = Client::attach_with_config(config).await;
        let shown = login_opened(&mut client, name).await;
        assert_eq!(shown.redirect_uri, redirect);
        assert!(shown.note.is_none(), "{shown:?}");
        browse(&shown).await;
        let (level, text) = login_closed(&mut client).await;
        assert_eq!(level, NoticeLevel::Info, "{host}: {text}");
        client.quit().await;
    }
    let list = servers(&daemon).await;
    for name in ["v4", "lh"] {
        assert!(
            matches!(login_of(&list, name), McpLoginState::Authorized { .. }),
            "{name}"
        );
    }
    drop(fixtures);
    daemon.stop().await;
}

/// A predefined redirect the client binds: `--set` takes only a loopback
/// `http` URI with port and path, and a project file's value is ignored.
#[test]
fn the_redirect_setting_takes_only_a_loopback_uri_and_never_a_project_s() {
    let ws = Workspace::new();
    let set = |value: &str| {
        rkat_tui::bootstrap::read_layers(&common::args_with(
            &ws.root,
            &["--set", &format!("mcp.oauth_redirect_uri={value}")],
        ))
    };
    assert!(set("http://localhost:8976/callback").is_ok());
    for bad in [
        "https://localhost:1/cb",
        "http://example.com:1/cb",
        "http://127.0.0.1/cb",
    ] {
        let err = set(bad).unwrap_err().to_string();
        assert!(err.contains("mcp.oauth_redirect_uri"), "{bad}: {err}");
    }
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(
        ws.root.join(".rkat/tui.toml"),
        "[mcp]\noauth_redirect_uri = \"http://127.0.0.1:9/evil\"\n",
    )
    .unwrap();
    let layers = rkat_tui::bootstrap::read_layers(&project_args(&ws, false)).unwrap();
    assert_eq!(
        rkat_tui::config::registry::mcp_redirect_in_force(&layers.layered.layers),
        None
    );
    assert!(
        layers.notices.iter().any(|n| n.contains("ignored [mcp]")),
        "{:?}",
        layers.notices
    );
}
