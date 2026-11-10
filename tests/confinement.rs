//! The shell under `--confinement`, end to end through the daemon with a
//! scripted LLM: the workspace mode lets a
//! command write the workspace but not outside it and keeps the network;
//! the strict mode denies IP network access. An interrupted shell call runs
//! in durable process custody and leaves no process behind. A `!` command
//! runs in the attached client under the same confinement.
//!
//! The confined cases need the platform backend (Linux: unprivileged user
//! namespaces; macOS: Seatbelt). Each case first probes the backend directly
//! (`meerkat_sandbox` compiles the mode's requirement and launches a
//! confined `/bin/true`); where that fails, as in containers or on Ubuntu
//! without unprivileged user namespaces, the case prints a `SKIPPED:` line
//! with the reason and returns. Apart from the markers the commands echo,
//! nothing is decided from a tool's output text.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::{AgentEvent, Message};
use rig::{Attached, Setup};
use rkat_tui::TuiError;
use rkat_tui::bootstrap::confinement::{HostPaths, lower};
use rkat_tui::cli::ConfinementArg;
use rkat_tui::daemon::DaemonError;
use rkat_tui::ui::Effect;
use serde_json::json;

/// A daemon (its classifier allows every reviewed call: these tests are
/// about the sandbox under the approval gate, not the gate) under `args`,
/// its shell and the client's `!` shell granted nothing outside the
/// workspace.
fn allow_shell(args: &[&str]) -> Setup {
    Setup {
        host_paths: Some(HostPaths::default()),
        ..Setup::args(args)
    }
}

/// [`allow_shell`] with this host's real paths, the endpoint file outside
/// the temp dir (the shell is granted the temp dir, and the daemon refuses
/// an endpoint file its shell could read). Keep the directory alive.
fn allow_shell_detected(args: &[&str]) -> (Setup, tempfile::TempDir) {
    let state = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let setup = Setup {
        host_paths: Some(HostPaths::detect()),
        endpoint_file: Some(state.path().join("mob.endpoint")),
        ..Setup::args(args)
    };
    (setup, state)
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// The text of tool result `id` in `messages`.
fn tool_text(messages: &[Message], id: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == id)
                .map(meerkat_core::types::ToolResult::text_content),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// Why this host cannot run a launch confined by `mode` over `workspace`,
/// probed directly: the requirement is compiled by the sandbox backend and a
/// confined `/bin/true` must exit successfully. `None` when it can.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn sandbox_unavailable(workspace: &Path, mode: ConfinementArg) -> Option<String> {
    let requirement = match lower(mode, workspace, &HostPaths::detect()) {
        Ok(Some(requirement)) => requirement,
        Ok(None) => return None,
        Err(err) => return Some(format!("the requirement is invalid: {err}")),
    };
    let launch = match meerkat_sandbox::ProcessLaunchSpec::new(
        "/bin/true".into(),
        Vec::new(),
        workspace.to_path_buf(),
        BTreeMap::new(),
    ) {
        Ok(launch) => launch,
        Err(refusal) => return Some(format!("the probe launch is invalid: {refusal}")),
    };
    let prepared = match meerkat_sandbox::prepare(&requirement, launch) {
        Ok(prepared) => prepared,
        Err(refusal) => return Some(format!("the backend refuses the requirement: {refusal}")),
    };
    let mut child = match prepared.spawn() {
        Ok(child) => child,
        Err(err) => return Some(format!("a confined launch cannot start: {err}")),
    };
    match child.wait().await {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("a confined /bin/true exited with {status}")),
        Err(err) => Some(format!("a confined launch cannot be awaited: {err}")),
    }
}

/// No sandbox backend on this platform.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn sandbox_unavailable(_workspace: &Path, _mode: ConfinementArg) -> Option<String> {
    Some("this platform has no sandbox backend".to_string())
}

/// Start a daemon whose shell is allowed without review, under `mode`,
/// and attach; `None` (after printing a `SKIPPED:` line) when this host
/// cannot run the mode.
async fn confined_rig(
    ws: &Workspace,
    llm: &Arc<ScriptedLlmClient>,
    mode: &str,
) -> Option<Attached> {
    let arg = match mode {
        "strict" => ConfinementArg::Strict,
        _ => ConfinementArg::Workspace,
    };
    if let Some(reason) = sandbox_unavailable(&ws.root, arg).await {
        println!("SKIPPED: --confinement={mode} is unavailable on this host: {reason}");
        return None;
    }
    let flag = format!("--confinement={mode}");
    match Attached::try_start_with(&ws.root, llm, allow_shell(&[&flag])).await {
        Ok(rig) => Some(rig),
        Err(DaemonError::Bootstrap(err)) if matches!(*err, TuiError::Confinement(_)) => {
            panic!("the probe launched confined, but the daemon refused the mode: {err}")
        }
        Err(other) => panic!("daemon start: {other}"),
    }
}

/// Run one shell call `command` in a fresh confined session and return its
/// output; `None` when the mode is unavailable here.
async fn run_confined(ws: &Workspace, mode: &str, command: &'static str) -> Option<String> {
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "sh",
            name: "shell",
            args: json!({ "command": command }),
        },
        Step::Text("done"),
    ]);
    let mut rig = confined_rig(ws, &llm, mode).await?;
    rig.send(Effect::StartTurn("run it".into()));
    rig.turn_finished().await;
    let text = tool_text(&llm.requests()[1], "sh");
    rig.quit().await;
    Some(text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_mode_confines_writes_to_the_workspace() {
    let ws = Workspace::new();
    let outside = ws.sibling("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let command = leak(format!(
        "echo in > inside.txt; echo out > {}/outside.txt; echo END",
        outside.display()
    ));
    let Some(output) = run_confined(&ws, "workspace", command).await else {
        return;
    };
    assert!(output.contains("END"), "{output}");
    assert!(ws.root.join("inside.txt").exists(), "{output}");
    assert!(
        !outside.join("outside.txt").exists(),
        "a confined command wrote outside the workspace: {output}"
    );
}

/// A confined command gets rkat-tui's curated environment: `HOME` and
/// `PATH` as rkat-tui has them (so `~/.cargo/bin` tools resolve), and none
/// of the rest (a sentinel `RKAT_TUI_TEST_SECRET` stands in for a provider
/// API key: set in rkat-tui's environment, not on the allowlist).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::disallowed_methods)] // test-only process launch: this test rerun
async fn workspace_mode_passes_the_curated_environment() {
    // A sentinel variable the curated environment must drop. The test
    // process cannot set one on itself (`set_var` is unsafe, and the crate
    // denies unsafe code), so the test reruns itself with it set.
    if std::env::var_os(SENTINEL_VAR).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_mode_passes_the_curated_environment",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SENTINEL_VAR, "sentinel-secret")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
        return;
    }
    let ws = Workspace::new();
    let command = "echo \"HOME=[$HOME]\"; echo \"PATH=[$PATH]\"; echo \"OUTSIDE=[${RKAT_TUI_TEST_SECRET:-unset}]\"; echo END";
    let Some(output) = run_confined(&ws, "workspace", command).await else {
        return;
    };
    assert!(output.contains("END"), "{output}");
    if let Ok(home) = std::env::var("HOME") {
        assert!(output.contains(&format!("HOME=[{home}]")), "{output}");
    }
    if let Ok(path) = std::env::var("PATH") {
        assert!(output.contains(&format!("PATH=[{path}]")), "{output}");
    }
    assert!(output.contains("OUTSIDE=[unset]"), "{output}");
    assert!(!output.contains("sentinel-secret"), "{output}");
}

/// Set (in a rerun of the test) to a value the confined shell must not see.
const SENTINEL_VAR: &str = "RKAT_TUI_TEST_SECRET";

/// Name resolution inside workspace mode, with this host's real system
/// paths (`HostPaths::detect`): `localhost` resolves, so the plan grants
/// `/etc/hosts` and `/etc/nsswitch.conf` to the confined shell. Resolving a
/// DNS name through a granted (or symbolically linked) `/etc/resolv.conf`
/// is the sandbox's own property, tested upstream
/// (`meerkat-sandbox` `process_confinement.rs`,
/// `linux_resolves_a_host_name_through_a_symlinked_resolv_conf`).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_mode_resolves_localhost() {
    let ws = Workspace::new();
    if let Some(reason) = sandbox_unavailable(&ws.root, ConfinementArg::Workspace).await {
        println!("SKIPPED: --confinement=workspace is unavailable on this host: {reason}");
        return;
    }
    let host = HostPaths::detect();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "sh",
            name: "shell",
            args: json!({
                "command": "getent hosts localhost && echo LOCAL-OK; echo END"
            }),
        },
        Step::Text("done"),
    ]);
    let (setup, _state) = allow_shell_detected(&["--confinement=workspace"]);
    assert_eq!(setup.host_paths.as_ref(), Some(&host));
    let mut rig = Attached::try_start_with(&ws.root, &llm, setup)
        .await
        .unwrap_or_else(|err| panic!("daemon start: {err}"));
    rig.send(Effect::StartTurn("resolve".into()));
    rig.turn_finished().await;
    let output = tool_text(&llm.requests()[1], "sh");
    rig.quit().await;
    assert!(output.contains("END"), "{output}");
    assert!(
        output.contains("LOCAL-OK"),
        "localhost did not resolve: {output}"
    );
}

/// Accept connections on a loopback port until the test ends.
async fn listener() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    port
}

fn connect_command(port: u16) -> &'static str {
    leak(format!(
        "if bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}' 2>/dev/null; then echo NET-OPEN; else echo NET-DENIED; fi"
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_mode_denies_the_network_and_workspace_mode_allows_it() {
    let port = listener().await;
    let ws = Workspace::new();
    let Some(strict) = run_confined(&ws, "strict", connect_command(port)).await else {
        return;
    };
    assert!(strict.contains("NET-DENIED"), "{strict}");
    let ws = Workspace::new();
    let Some(open) = run_confined(&ws, "workspace", connect_command(port)).await else {
        return;
    };
    assert!(open.contains("NET-OPEN"), "{open}");
}

/// A Rust toolchain runs confined: the sandbox gives the launch its own pid
/// namespace with a fresh `/proc` (the rustup proxies, rustc's library
/// lookup and the linker wrapper all read `/proc/self/exe`) and the
/// `/etc/alternatives` grant lets Debian's `cc` link resolve. Prints
/// `SKIPPED:` when the host has no `rustc` on its `PATH`.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::disallowed_methods)] // test-only: is a toolchain installed?
async fn workspace_mode_runs_the_rust_toolchain() {
    if std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIPPED: no rustc on PATH");
        return;
    }
    let ws = Workspace::new();
    if let Some(reason) = sandbox_unavailable(&ws.root, ConfinementArg::Workspace).await {
        println!("SKIPPED: --confinement=workspace is unavailable on this host: {reason}");
        return;
    }
    let command = "echo pid=$$; printf 'fn main() { println!(\"linked-ok\"); }\n' > m.rs && rustc m.rs -o m && ./m; echo END";
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "sh",
            name: "shell",
            args: json!({ "command": command }),
        },
        Step::Text("done"),
    ]);
    // The host's real paths: the toolchain lives in the rustup and cargo
    // homes, which the rig's hermetic default leaves out.
    let (setup, _state) = allow_shell_detected(&["--confinement=workspace"]);
    let mut rig = Attached::try_start_with(&ws.root, &llm, setup)
        .await
        .unwrap_or_else(|err| panic!("daemon start: {err}"));
    rig.send(Effect::StartTurn("build it".into()));
    rig.turn_finished().await;
    let output = tool_text(&llm.requests()[1], "sh");
    rig.quit().await;
    assert!(output.contains("END"), "{output}");
    assert!(output.contains("pid=2\n"), "{output}");
    assert!(output.contains("linked-ok"), "{output}");
}

/// Whether process `pid` is alive (not reaped and not a zombie).
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(Path::new("/proc").join(pid).join("stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_string))
        })
        .is_some_and(|state| state != "Z")
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_shell_call_leaves_custody_records_and_no_process() {
    let ws = Workspace::new();
    let pid_file = ws.root.join("pid");
    let command = leak(format!("echo $$ > {}; exec sleep 60", pid_file.display()));
    let llm = ScriptedLlmClient::new(vec![Step::Tool {
        id: "sleep",
        name: "shell",
        args: json!({ "command": command, "timeout_secs": 120 }),
    }]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell(&[])).await;
    let session = rig.current_session().await;
    rig.send(Effect::StartTurn("sleep".into()));
    let pid = within(async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file)
                && !text.trim().is_empty()
            {
                return text.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(alive(&pid), "the shell process {pid} should be running");
    rig.send(Effect::Interrupt);
    let outcome = rig.turn_finished().await;
    assert!(outcome.result.is_err(), "{outcome:?}");
    within(async {
        while alive(&pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // The call ran in the session's durable process custody.
    let custody = rkat_tui::bootstrap::resolve_scope(&common::args_for(&ws.root))
        .unwrap()
        .realm_root()
        .join(meerkat_tools::builtin::shell::PROCESS_CUSTODY_DIR);
    assert!(custody.is_dir(), "{}", custody.display());
    let has_session_scope = walk(&custody)
        .iter()
        .any(|path| path.to_string_lossy().contains(&session.to_string()));
    assert!(has_session_scope, "no custody records for {session}");
    assert!(rig.events().iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionStarted { id, .. } if id == "sleep"
    )));
    rig.quit().await;
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}

/// A realm of the user's own (`--state-root`, outside the workspace) whose
/// `[confinement]` requires the shell confined is the floor: `off` is a
/// typed start-up error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_confinement_below_a_realm_policy_is_a_typed_startup_error() {
    let ws = Workspace::new();
    let state = ws.sibling("realms");
    let state_arg = leak(format!("--state-root={}", state.display()));
    let realm = rkat_tui::bootstrap::resolve_scope(&common::args_with(&ws.root, &[state_arg]))
        .unwrap()
        .realm_root();
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::write(
        realm.join("config.toml"),
        "[confinement.shell]\nmode = \"workspace\"\nnetwork = \"unrestricted\"\n",
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(Vec::new());
    // `--confinement=off`, the same as `--no-confinement`.
    let Err(err) = Attached::try_start_with(
        &ws.root,
        &llm,
        allow_shell(&["--confinement=off", state_arg]),
    )
    .await
    else {
        panic!("--no-confinement started below the realm's policy");
    };
    let DaemonError::Bootstrap(err) = err else {
        panic!("not a bootstrap error: {err}");
    };
    assert!(
        matches!(
            &*err,
            TuiError::Confinement(rkat_tui::ConfinementError::BelowRealmFloor { policy, .. })
                if policy.contains("workspace preset")
        ),
        "{err}"
    );
}

/// The workspace realm's `config.toml` sets no confinement (S1 of the
/// security review: the agent's shell can write it on Linux, and a clone
/// can carry it): its `[confinement]` is ignored with a notice, here one
/// that would have refused `off`, and the command line decides.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_workspace_realms_confinement_table_is_ignored_with_a_notice() {
    let ws = Workspace::new();
    let realm = rkat_tui::bootstrap::resolve_scope(&common::args_for(&ws.root))
        .unwrap()
        .realm_root();
    std::fs::write(
        realm.join("config.toml"),
        "[confinement.shell]\nmode = \"workspace\"\nnetwork = \"unrestricted\"\n",
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(Vec::new());
    let mut attached =
        Attached::try_start_with(&ws.root, &llm, allow_shell(&["--confinement=off"]))
            .await
            .unwrap_or_else(|err| panic!("the workspace realm's table still applies: {err}"));
    attached
        .client
        .wait_for(|msg| {
            matches!(msg, rkat_tui::ui::Msg::Notice { text, .. } if text.contains("[confinement] table is ignored"))
        })
        .await;
    attached.client.quit().await;
    attached.daemon.stop().await;
}

/// A `!` command runs under the shell's own confinement: it writes the
/// workspace, not outside it, and gets the curated environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bang_command_runs_under_the_shells_confinement() {
    use rkat_tui::ui::{Msg, ShellOutcome, ShellRunId};

    let ws = Workspace::new();
    let outside = ws.sibling("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let Some(mut rig) = confined_rig(&ws, &llm, "workspace").await else {
        return;
    };
    rig.send(Effect::RunShell {
        id: ShellRunId(1),
        command: format!(
            "echo in > inside.txt; echo out > {}/outside.txt; echo \"KEY=[${{ANTHROPIC_API_KEY:-unset}}]\"; echo END",
            outside.display()
        ),
    });
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::ShellFinished { .. }))
        .await;
    let Msg::ShellFinished {
        outcome: ShellOutcome::Ran { run, context },
        ..
    } = msg
    else {
        panic!("{msg:?}");
    };
    assert!(run.output.contains("END"), "{run:?}");
    assert!(run.output.contains("KEY=[unset]"), "{run:?}");
    assert!(ws.root.join("inside.txt").exists(), "{run:?}");
    assert!(
        !outside.join("outside.txt").exists(),
        "a confined ! command wrote outside the workspace: {run:?}"
    );
    assert_eq!(context, Ok(()));
    rig.quit().await;
}

/// S3 of the security review: the Cargo home is granted entry by entry.
/// Under workspace confinement with this host's real paths, `cargo build`
/// of a crate with a registry dependency (offline, from the local cache)
/// still works, while `bin/` and `config.toml` of the Cargo home cannot
/// be written (a confined command must not replace a tool or set
/// `build.rustc-wrapper`).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cargo_builds_under_the_narrowed_cargo_home_grant() {
    let ws = Workspace::new();
    if let Some(reason) = sandbox_unavailable(&ws.root, ConfinementArg::Workspace).await {
        println!("SKIPPED: --confinement=workspace is unavailable on this host: {reason}");
        return;
    }
    let Some(home) = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))
        .filter(|home| home.join("registry").is_dir())
    else {
        println!("SKIPPED: no Cargo home with a registry on this host");
        return;
    };
    let krate = ws.root.join("probe");
    std::fs::create_dir_all(krate.join("src")).unwrap();
    std::fs::write(
        krate.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncfg-if = \"1\"\n",
    )
    .unwrap();
    std::fs::write(
        krate.join("src/main.rs"),
        "cfg_if::cfg_if! { if #[cfg(unix)] { fn os() -> &'static str { \"unix\" } } else { fn os() -> &'static str { \"other\" } } }\nfn main() { println!(\"built-ok {}\", os()); }\n",
    )
    .unwrap();
    let command = leak(format!(
        "cd probe && cargo build --offline -q 2>&1; echo BUILD=$?; ./target/debug/probe; \
         touch '{bin}/rkat-probe' 2>/dev/null && echo BIN-WRITABLE; \
         touch '{config}' 2>/dev/null && echo CONFIG-WRITABLE; echo END",
        bin = home.join("bin").display(),
        config = home.join("config.toml").display(),
    ));
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "sh",
            name: "shell",
            args: json!({ "command": command, "timeout_secs": 300 }),
        },
        Step::Text("done"),
    ]);
    let (setup, _state) = allow_shell_detected(&["--confinement=workspace"]);
    let mut rig = Attached::try_start_with(&ws.root, &llm, setup)
        .await
        .unwrap_or_else(|err| panic!("daemon start: {err}"));
    rig.send(Effect::StartTurn("build it".into()));
    rig.turn_finished().await;
    let output = tool_text(&llm.requests()[1], "sh");
    rig.quit().await;
    assert!(output.contains("END"), "{output}");
    assert!(
        output.contains("BUILD=0") && output.contains("built-ok unix"),
        "{output}"
    );
    assert!(!output.contains("BIN-WRITABLE"), "{output}");
    assert!(!output.contains("CONFIG-WRITABLE"), "{output}");
    assert!(!home.join("bin/rkat-probe").exists());
}
