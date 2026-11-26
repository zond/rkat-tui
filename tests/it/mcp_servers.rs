//! `--mcp-server`, end to end through the `rkat-tui-mob` daemon: a stdio
//! server given on the command line is started for the agent, and it
//! inherits the daemon's environment (the engine's stdio launch clears
//! nothing; the client's start-on-demand passes its own environment on,
//! `client::launch::spawn_daemon`), so a wrapper hands it a secret in the
//! environment, never on the command line.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{ScriptedLlmClient, Step, Workspace};
use rig::{Client, Running};
use rkat_tui::ui::Effect;

/// Set (in a rerun of the test) in the daemon's environment: the server
/// writes it where the test reads it.
const MCP_ENV_VAR: &str = "RKAT_TUI_TEST_MCP_ENV";

/// The test reruns itself with [`MCP_ENV_VAR`] set: the test process
/// cannot set a variable on itself (`set_var` is unsafe, and the crate
/// denies unsafe code), and the rig's daemon runs in the test's process.
/// The "server" is a shell that writes the variable and exits: its start
/// with the daemon's environment is what is pinned, not the protocol.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_line_stdio_server_starts_with_the_daemon_s_environment() {
    if common::reran_with(
        "mcp_servers::a_command_line_stdio_server_starts_with_the_daemon_s_environment",
        MCP_ENV_VAR,
        "from-the-environment",
    ) {
        return;
    }
    let ws = Workspace::new();
    let marker = ws.root.join("server-saw.txt");
    let server = format!(
        r#"name="search", command="/bin/sh", args=["-c", 'printf %s "${MCP_ENV_VAR}" > {}']"#,
        marker.display()
    );
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let daemon = Running::start_with(
        &common::args_with(&ws.root, &["--mcp-server", &server]),
        Running::options(&ws.root, &llm),
    )
    .await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("hi".into()));
    client.turn_finished().await.unwrap();
    common::until(|| {
        std::fs::read_to_string(&marker).is_ok_and(|seen| seen == "from-the-environment")
    })
    .await;
    client.quit().await;
    daemon.stop().await;
}
