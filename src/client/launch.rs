//! Start-on-demand: find a daemon that answers for the workspace, or start
//! `rkat-tui-mob` in the background and wait for its endpoint file.
//!
//! This is the one place the TUI launches a process besides the user's
//! editor and the user's `!` shell. The daemon is detached from the TUI's
//! terminal: through `setsid(1)` when it is on `PATH` (a new session with
//! no controlling terminal, so closing the terminal sends it no SIGHUP),
//! else in its own process group (`process_group(0)`: the terminal's
//! Ctrl+C and job control never reach it; `setsid(2)` itself would need an
//! `unsafe` `pre_exec`). It has stdin closed and stdout and stderr
//! appended to `<realm>/logs/rkat-tui-mob.stderr.log`, and keeps running
//! when the TUI exits. Without `setsid(1)` it stays in the terminal's
//! session: a shell that hangs up every process group of the session
//! when the terminal closes would stop it too (bash and zsh hang up only
//! their own jobs).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::ClientError;
use crate::cli::{Args, AttachMode};
use crate::daemon::endpoint::Endpoint;
use crate::daemon::lifecycle::START_TOKEN_ENV;

/// The daemon's binary name.
pub const DAEMON_BINARY: &str = "rkat-tui-mob";

/// How long a started daemon may take to write its endpoint file.
pub const START_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the liveness probe waits for the address to accept.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How often the endpoint file is polled while the daemon starts.
pub const POLL: Duration = Duration::from_millis(100);

/// What the endpoint file says about a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// No endpoint file (or one that cannot be read or parsed).
    Absent,
    /// A file whose address does not accept: the daemon died.
    Stale(Endpoint),
    /// A file whose address accepts.
    Live(Endpoint),
}

/// Read the endpoint file at `file` and probe its address.
pub async fn probe(file: &Path) -> Liveness {
    let Ok(endpoint) = Endpoint::read(file) else {
        return Liveness::Absent;
    };
    // Anything but an accepting address counts as stale here: a start
    // over a file whose daemon may still live is refused by that start's
    // own check (`endpoint::check_stale`).
    if crate::daemon::endpoint::reach(endpoint.address, PROBE_TIMEOUT).await
        == crate::daemon::endpoint::Reach::Accepts
    {
        Liveness::Live(endpoint)
    } else {
        Liveness::Stale(endpoint)
    }
}

/// Probe the places of `files` in order: the first live endpoint, else the
/// first stale one, else absent.
pub async fn probe_any(files: &[PathBuf]) -> Liveness {
    let mut found = Liveness::Absent;
    for file in files {
        match probe(file).await {
            Liveness::Live(endpoint) => return Liveness::Live(endpoint),
            Liveness::Stale(endpoint) if found == Liveness::Absent => {
                found = Liveness::Stale(endpoint);
            }
            Liveness::Stale(_) | Liveness::Absent => {}
        }
    }
    found
}

/// The daemon's arguments for waiting at `files`: with one place, the
/// daemon is told to write there (`--endpoint-file`); with the default
/// places it picks one itself, in the order the client probes them.
#[must_use]
pub fn argv_for(argv: &[OsString], files: &[PathBuf]) -> Vec<OsString> {
    match files {
        [file] => with_endpoint_file(argv, file),
        _ => argv.to_vec(),
    }
}

/// What to do about the endpoint file's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Attach to this endpoint.
    Attach(Endpoint),
    /// Start the daemon and wait for its endpoint.
    Start,
    /// No daemon answers and none may be started.
    Refuse,
}

/// The start-on-demand decision.
#[must_use]
pub fn decide(liveness: Liveness, mode: AttachMode) -> Decision {
    match (liveness, mode) {
        (Liveness::Live(endpoint), _) => Decision::Attach(endpoint),
        (Liveness::Absent | Liveness::Stale(_), AttachMode::Auto) => Decision::Start,
        (Liveness::Absent | Liveness::Stale(_), AttachMode::Existing) => Decision::Refuse,
    }
}

/// The daemon binary: next to the running `rkat-tui` when it is there,
/// else `rkat-tui-mob` from `PATH`.
#[must_use]
pub fn daemon_program() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(DAEMON_BINARY)))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(DAEMON_BINARY))
}

/// The daemon's arguments for the TUI's `args`: every engine flag the two
/// binaries share (model, endpoints, confinement, trust, `--set`),
/// with the workspace made explicit; the UI-only ones (prompt, resume,
/// mouse, attach) are left out.
#[must_use]
pub fn daemon_argv(args: &Args, workspace: &Path) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec!["-w".into(), workspace.into()];
    let mut value = |flag: &str, v: Option<&OsString>| {
        if let Some(v) = v {
            argv.push(flag.into());
            argv.push(v.clone());
        }
    };
    let os = |s: &String| OsString::from(s);
    value("--model", args.model.as_ref().map(os).as_ref());
    value("--endpoint", args.endpoint.as_ref().map(os).as_ref());
    value(
        "--user-config-root",
        args.user_config_root.as_ref().map(OsString::from).as_ref(),
    );
    value(
        "--config-dir",
        args.config_dir.as_ref().map(OsString::from).as_ref(),
    );
    value(
        "--classifier-model",
        args.classifier_model.as_ref().map(os).as_ref(),
    );
    value("--confinement", value_name(args.confinement).as_ref());
    let repeated: [(&str, Vec<OsString>); 6] = [
        ("--set", args.set.iter().map(OsString::from).collect()),
        ("--allow", args.allow.iter().map(OsString::from).collect()),
        ("--deny", args.deny.iter().map(OsString::from).collect()),
        (
            "--endpoint-def",
            args.endpoint_def.iter().map(OsString::from).collect(),
        ),
        (
            "--confinement-read",
            args.confinement_read.iter().map(OsString::from).collect(),
        ),
        (
            "--confinement-write",
            args.confinement_write.iter().map(OsString::from).collect(),
        ),
    ];
    for (flag, values) in repeated {
        for v in values {
            argv.push(flag.into());
            argv.push(v);
        }
    }
    let switches = [
        (args.trust_project_builds, "--trust-project-builds"),
        (args.no_trust_project_builds, "--no-trust-project-builds"),
        (args.no_confinement, "--no-confinement"),
        (args.external_no_network, "--external-no-network"),
        (args.no_auto, "--no-auto"),
        (args.trust_project, "--trust-project"),
        (args.wait_for_mcp, "--wait-for-mcp"),
        (args.auto_compact, "--auto-compact"),
        (args.no_auto_compact, "--no-auto-compact"),
        (args.no_pricing_fetch, "--no-pricing-fetch"),
    ];
    argv.extend(
        switches
            .into_iter()
            .filter(|(given, _)| *given)
            .map(|(_, flag)| OsString::from(flag)),
    );
    argv
}

/// The command-line name clap gives `value` (a `ValueEnum`), so the
/// forwarded spelling is the one the daemon's parser reads.
fn value_name<T: clap::ValueEnum>(value: T) -> Option<OsString> {
    value
        .to_possible_value()
        .map(|possible| OsString::from(possible.get_name()))
}

/// The ids of the `rkat-tui` arguments [`daemon_argv`] does not forward:
/// the UI's own (the prompt, resuming, the mouse, the daemon) and the
/// workspace, which it makes explicit. Every other argument is forwarded;
/// `tests::every_engine_flag_reaches_the_daemon` fails for one that is not.
#[cfg(any(test, feature = "test-support"))]
pub const NOT_FORWARDED: &[&str] = &[
    "prompt",
    "resume",
    "continue_last",
    "no_mouse",
    "attach",
    "attach_token_file",
    "attach_insecure",
    "context_root",
    "list",
    "rehome",
    "audit",
    "classifier_check",
];

/// `argv` with `--endpoint-file <file>`: the daemon writes its endpoint
/// where this client waits for it.
#[must_use]
pub fn with_endpoint_file(argv: &[OsString], file: &Path) -> Vec<OsString> {
    let mut argv = argv.to_vec();
    argv.push("--endpoint-file".into());
    argv.push(file.into());
    argv
}

/// Where a started daemon's stdout and stderr go.
#[must_use]
pub fn daemon_log(realm_root: &Path) -> PathBuf {
    realm_root.join("logs").join("rkat-tui-mob.stderr.log")
}

/// Start `program` with `argv` in the background (its own process group,
/// stdin closed, stdout and stderr appended to `log`) and reap it on a
/// thread when it exits. `start_token` goes to the daemon in
/// [`START_TOKEN_ENV`] (the client presenting it at attach is the owner;
/// `None`: the daemon has no owner). Returns its pid and a receiver that
/// yields how it exited if it exits ([`DaemonExit`]).
///
/// # Errors
///
/// [`ClientError::Spawn`] when the log cannot be opened or the program
/// cannot be started.
pub fn spawn_daemon(
    program: &Path,
    argv: &[OsString],
    log: &Path,
    start_token: Option<&str>,
) -> Result<(u32, tokio::sync::oneshot::Receiver<DaemonExit>), ClientError> {
    let spawn_error = |source| ClientError::Spawn {
        program: program.to_path_buf(),
        source,
    };
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(spawn_error)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(spawn_error)?;
    let err = out.try_clone().map_err(spawn_error)?;
    // What this start writes to the log begins here.
    let start = out.metadata().map_or(0, |meta| meta.len());
    // Through `setsid` a missing program would only show as `setsid`
    // exiting; refuse it here, as a direct spawn would.
    if !program_exists(program) {
        return Err(spawn_error(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such program",
        )));
    }
    let mut command = match setsid_program() {
        // `setsid` runs the daemon in place (same pid) when it is not a
        // process group leader, which a child of the TUI is not.
        Some(setsid) => {
            let mut command = std::process::Command::new(setsid);
            command.arg(program);
            command
        }
        None => {
            let mut command = std::process::Command::new(program);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt as _;
                command.process_group(0);
            }
            command
        }
    };
    match start_token {
        Some(token) => command.env(START_TOKEN_ENV, token),
        None => command.env_remove(START_TOKEN_ENV),
    };
    command
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    let mut child = command.spawn().map_err(spawn_error)?;
    let pid = child.id();
    let (exited_tx, exited_rx) = tokio::sync::oneshot::channel();
    let log = log.to_path_buf();
    std::thread::spawn(move || {
        let status = child
            .wait()
            .map_or_else(|err| err.to_string(), |status| status.to_string());
        let said = last_words(&log, start);
        let _ = exited_tx.send(DaemonExit { status, said });
    });
    Ok((pid, exited_rx))
}

/// How a started daemon exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonExit {
    /// Its exit status.
    pub status: String,
    /// The error it wrote last ([`last_words`]), if any.
    pub said: Option<String>,
}

/// Most lines of a daemon's last words.
const LAST_WORDS_LINES: usize = 3;
/// Most characters of one line of them.
const LAST_WORDS_LINE_CHARS: usize = 300;
/// How much of the log's end is read for them.
const LAST_WORDS_BYTES: u64 = 16 * 1024;

/// What the daemon wrote to `log` from byte `start` on (this start's
/// output) that says why it stopped: its own error lines
/// (`rkat-tui-mob: …`), else its last non-empty lines; at most
/// [`LAST_WORDS_LINES`] lines of at most [`LAST_WORDS_LINE_CHARS`]
/// characters, from the last [`LAST_WORDS_BYTES`] of it. `None` when it
/// wrote nothing.
#[must_use]
pub fn last_words(log: &Path, start: u64) -> Option<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    let from = start.max(len.saturating_sub(LAST_WORDS_BYTES));
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.take(LAST_WORDS_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let prefix = format!("{DAEMON_BINARY}: ");
    let own: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.starts_with(&prefix))
        .collect();
    let chosen = if own.is_empty() { lines } else { own };
    let chosen = &chosen[chosen.len().saturating_sub(LAST_WORDS_LINES)..];
    if chosen.is_empty() {
        return None;
    }
    let cut: Vec<String> = chosen
        .iter()
        .map(
            |line| match line.char_indices().nth(LAST_WORDS_LINE_CHARS) {
                Some((at, _)) => format!("{}…", &line[..at]),
                None => (*line).to_string(),
            },
        )
        .collect();
    Some(cut.join("\n"))
}

/// Whether `program` is a file, or a bare name found on `PATH`.
fn program_exists(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// `setsid(1)`, when it is on `PATH` (Unix only).
#[must_use]
pub fn setsid_program() -> Option<PathBuf> {
    if !cfg!(unix) {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("setsid"))
        .find(|candidate| candidate.is_file())
}

/// Wait (up to `timeout`) for a live endpoint at one of `files`; fail early
/// when the started daemon exits (`exited`).
///
/// # Errors
///
/// [`ClientError::DaemonExited`] or [`ClientError::NeverCameUp`].
pub async fn wait_for_endpoint(
    files: &[PathBuf],
    timeout: Duration,
    exited: &mut tokio::sync::oneshot::Receiver<DaemonExit>,
    log: &Path,
) -> Result<Endpoint, ClientError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Liveness::Live(endpoint) = probe_any(files).await {
            return Ok(endpoint);
        }
        if let Ok(exit) = exited.try_recv() {
            return Err(ClientError::DaemonExited {
                status: exit.status,
                said: exit.said,
                log: log.to_path_buf(),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ClientError::NeverCameUp {
                waited: timeout,
                log: log.to_path_buf(),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn endpoint(address: std::net::SocketAddr) -> Endpoint {
        Endpoint {
            address,
            token: "t".into(),
            pid: 1,
            workspace: None,
            started_at: None,
        }
    }

    #[tokio::test]
    async fn the_decision_follows_the_endpoint_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mob.endpoint");
        // Absent.
        assert_eq!(probe(&file).await, Liveness::Absent);
        assert_eq!(decide(Liveness::Absent, AttachMode::Auto), Decision::Start);
        assert_eq!(
            decide(Liveness::Absent, AttachMode::Existing),
            Decision::Refuse
        );
        // Unparseable: as absent (the daemon replaces it).
        std::fs::write(&file, "garbage").unwrap();
        assert_eq!(probe(&file).await, Liveness::Absent);
        // Live.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = endpoint(listener.local_addr().unwrap());
        live.write(&file).unwrap();
        assert_eq!(probe(&file).await, Liveness::Live(live.clone()));
        assert_eq!(
            decide(Liveness::Live(live.clone()), AttachMode::Existing),
            Decision::Attach(live.clone())
        );
        // Stale: nothing accepts there any more.
        drop(listener);
        assert_eq!(probe(&file).await, Liveness::Stale(live.clone()));
        assert_eq!(
            decide(Liveness::Stale(live.clone()), AttachMode::Auto),
            Decision::Start
        );
        assert_eq!(
            decide(Liveness::Stale(live.clone()), AttachMode::Existing),
            Decision::Refuse
        );
        // The places in order: a live one anywhere wins, else the first
        // stale one.
        let other = dir.path().join("fallback.endpoint");
        let both = [file.clone(), other.clone()];
        assert_eq!(probe_any(&both).await, Liveness::Stale(live.clone()));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(probe_any(&both).await, Liveness::Absent);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fallback_live = endpoint(listener.local_addr().unwrap());
        fallback_live.write(&other).unwrap();
        live.write(&file).unwrap();
        assert_eq!(probe_any(&both).await, Liveness::Live(fallback_live));
        // One place: the daemon is told; the default places: it picks.
        assert!(
            argv_for(&[], std::slice::from_ref(&file)).contains(&OsString::from("--endpoint-file"))
        );
        assert!(argv_for(&[], &both).is_empty());
    }

    #[test]
    fn the_daemon_gets_every_shared_engine_flag_and_no_ui_flag() {
        let tui = Args::try_parse_from([
            "rkat-tui",
            "--model",
            "anthropic:claude-sonnet-4-6",
            "--set",
            "auto.compact=auto",
            "--set",
            "a=1",
            "--allow",
            "shell=ls*",
            "--deny",
            "write_file",
            "--endpoint-def",
            "x=openai=http://h",
            "--confinement",
            "strict",
            "--confinement-read",
            "/r",
            "--config-dir",
            "/cfg",
            "--user-config-root",
            "/u",
            "--trust-project",
            "--no-pricing-fetch",
            "--no-auto",
            "--classifier-model",
            "claude-haiku-4-5",
            "--no-mouse",
            "-c",
            "a prompt",
        ])
        .unwrap();
        let mut argv = vec![OsString::from("rkat-tui-mob")];
        argv.extend(with_endpoint_file(
            &daemon_argv(&tui, Path::new("/ws")),
            Path::new("/state/mob.endpoint"),
        ));
        let daemon = crate::daemon::DaemonCli::try_parse_from(argv).unwrap();
        daemon.check().unwrap();
        // The daemon writes its endpoint where the client waits for it.
        assert_eq!(
            daemon.endpoint_file.as_deref(),
            Some(Path::new("/state/mob.endpoint"))
        );
        let shared = daemon.tui;
        assert_eq!(shared.context_root.as_deref(), Some(Path::new("/ws")));
        // Every engine flag survives the round trip; the UI ones do not.
        let mut expected = tui;
        expected.context_root = Some(PathBuf::from("/ws"));
        expected.prompt = None;
        expected.continue_last = false;
        expected.resume = None;
        expected.no_mouse = false;
        expected.attach = None;
        assert_eq!(format!("{shared:?}"), format!("{expected:?}"));
    }

    /// The finding this guards: every flag was hand-written into the
    /// daemon's argv a third time, so a new one could silently not be
    /// forwarded. Each argument clap knows is given alone and must come out
    /// of the daemon's parser as it went in, unless it is listed in
    /// [`NOT_FORWARDED`].
    #[test]
    fn every_engine_flag_reaches_the_daemon() {
        use clap::CommandFactory as _;
        let command = Args::command();
        let mut checked = 0;
        for arg in command.get_arguments() {
            let id = arg.get_id().as_str();
            if NOT_FORWARDED.contains(&id) || matches!(id, "help" | "version") {
                continue;
            }
            let flag = arg
                .get_long()
                .map(|long| format!("--{long}"))
                .expect("every forwarded argument has a long name");
            let mut argv = vec!["rkat-tui".to_string(), flag.clone()];
            if arg.get_action().takes_values() {
                // A value other than the default where clap lists them.
                let possible = arg.get_possible_values();
                let defaults: Vec<_> = arg.get_default_values().to_vec();
                let value = possible
                    .iter()
                    .map(|p| p.get_name().to_string())
                    .find(|name| !defaults.iter().any(|d| d == name.as_str()))
                    .unwrap_or_else(|| "/rkat-sample".to_string());
                argv.push(value);
            }
            let tui = Args::try_parse_from(&argv).unwrap_or_else(|err| panic!("{argv:?}: {err}"));
            let mut daemon = vec![OsString::from("rkat-tui-mob")];
            daemon.extend(daemon_argv(&tui, Path::new("/ws")));
            let shared = crate::daemon::DaemonCli::try_parse_from(&daemon)
                .unwrap_or_else(|err| panic!("{daemon:?}: {err}"))
                .tui;
            let mut expected = tui;
            expected.context_root = Some(PathBuf::from("/ws"));
            assert_eq!(
                format!("{shared:?}"),
                format!("{expected:?}"),
                "{flag} is not forwarded (or add it to NOT_FORWARDED)"
            );
            checked += 1;
        }
        assert!(checked > 20, "{checked}");
    }

    #[test]
    fn the_daemon_binary_is_found_next_to_the_tui_or_on_the_path() {
        let program = daemon_program();
        assert!(program.ends_with(DAEMON_BINARY), "{}", program.display());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_started_daemon_leaves_the_terminals_session_when_setsid_exists() {
        let dir = tempfile::tempdir().unwrap();
        let log = daemon_log(dir.path());
        let (pid, mut exited) =
            spawn_daemon(Path::new("/bin/sleep"), &["5".into()], &log, None).unwrap();
        // Once `setsid` has run (it execs the program in place).
        for _ in 0..200 {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() == "sleep" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // /proc/<pid>/stat: pid (comm) state ppid pgrp session …
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let (pgrp, session) = (fields[2], fields[3]);
        let ours = std::fs::read_to_string("/proc/self/stat").unwrap();
        let ours: Vec<&str> = ours
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        assert_ne!(pgrp, ours[2], "the daemon is in its own process group");
        if setsid_program().is_some() {
            assert_eq!(
                session,
                pid.to_string(),
                "setsid made it a session leader in place"
            );
            assert_ne!(session, ours[3]);
        } else {
            eprintln!("SKIPPED: no setsid on PATH; the daemon only has its own process group");
        }
        assert!(exited.try_recv().is_err(), "still running");
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
            nix::sys::signal::Signal::SIGTERM,
        );
    }

    /// The start token reaches the daemon in its environment, and a
    /// restart (no token) does not inherit one from this process.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_start_token_is_handed_over_in_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let log = daemon_log(dir.path());
        let script = format!("echo \"token=${{{START_TOKEN_ENV}:-none}}\"");
        for (token, expected) in [(Some("abc"), "token=abc"), (None, "token=none")] {
            let _ = std::fs::remove_file(&log);
            let (_, exited) = spawn_daemon(
                Path::new("/bin/sh"),
                &["-c".into(), script.clone().into()],
                &log,
                token,
            )
            .unwrap();
            exited.await.unwrap();
            let out = std::fs::read_to_string(&log).unwrap();
            assert_eq!(out.trim(), expected);
        }
    }

    #[tokio::test]
    async fn a_daemon_that_exits_is_reported_with_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = daemon_log(dir.path());
        let (_, mut exited) = spawn_daemon(Path::new("/bin/false"), &[], &log, None).unwrap();
        let waited = wait_for_endpoint(
            &[dir.path().join("mob.endpoint")],
            Duration::from_secs(10),
            &mut exited,
            &log,
        )
        .await;
        assert!(
            matches!(&waited, Err(ClientError::DaemonExited { log: at, .. }) if at == &log),
            "{waited:?}"
        );
        assert!(log.exists());
        assert!(matches!(
            spawn_daemon(Path::new("/nonexistent/rkat-tui-mob"), &[], &log, None),
            Err(ClientError::Spawn { .. })
        ));
    }

    /// A daemon that exits before serving is reported with what it said
    /// last, bounded, from this start's output only: an older run's lines
    /// in the same log are not shown.
    #[tokio::test]
    async fn a_daemon_that_exits_is_reported_with_its_last_words() {
        let dir = tempfile::tempdir().unwrap();
        let log = daemon_log(dir.path());
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "rkat-tui-mob: an older run's error\n").unwrap();
        let script = format!(
            "echo 'INFO starting'; echo 'rkat-tui-mob: create endpoint directory /x: Read-only file system' >&2; echo '{}' >&2; exit 3",
            "y".repeat(1000)
        );
        let (_, mut exited) = spawn_daemon(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            &log,
            None,
        )
        .unwrap();
        let waited = wait_for_endpoint(
            &[dir.path().join("mob.endpoint")],
            Duration::from_secs(10),
            &mut exited,
            &log,
        )
        .await;
        let Err(err @ ClientError::DaemonExited { .. }) = waited else {
            panic!("{waited:?}");
        };
        let text = err.to_string();
        assert!(
            text.contains("rkat-tui-mob: create endpoint directory /x: Read-only file system"),
            "{text}"
        );
        assert!(
            !text.contains("older run") && !text.contains("INFO starting"),
            "{text}"
        );
        // Nothing of its own: the last lines it wrote, each cut.
        let quiet = dir.path().join("quiet.log");
        std::fs::write(&quiet, format!("a\n\nb\nc\nd {}\n", "z".repeat(1000))).unwrap();
        let said = last_words(&quiet, 2).unwrap();
        assert_eq!(said.lines().count(), 3, "{said}");
        assert!(
            said.starts_with("b\nc\nd ") && said.ends_with('…'),
            "{said}"
        );
        assert_eq!(last_words(&quiet, 10_000), None);
    }
}
