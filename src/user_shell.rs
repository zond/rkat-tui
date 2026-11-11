//! `!` shell mode: a command the user typed runs as the user, and its
//! output joins the session's context.
//!
//! The command is the user's own, typed at their terminal: it runs like any
//! other terminal program, with no sandbox and no approval gate.
//!
//! - the program is the user's login shell (`$SHELL` when it names an
//!   absolute path, else `/bin/sh`) with `-c`;
//! - the working directory is the canonical workspace;
//! - the environment is rkat-tui's whole environment (with `PWD` set to
//!   the workspace);
//! - stdin is `/dev/null`.
//!
//! It runs for at most `[shell] user_timeout_secs` (default
//! [`DEFAULT_USER_TIMEOUT_SECS`]); stdout and stderr are captured together
//! in arrival order, up to [`MAX_OUTPUT_BYTES`] with a truncation marker.
//! On a timeout, a cancel (the run's future dropped) or the command's exit,
//! its process group is killed.
//!
//! The agent's shell is a different thing: the daemon runs it under the
//! engine's confinement (`--confinement`), with a curated environment.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt};

/// `[shell] user_timeout_secs` when no layer sets it.
pub const DEFAULT_USER_TIMEOUT_SECS: u64 = 120;

/// The most output a run keeps (stdout and stderr together).
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The line that ends output cut at [`MAX_OUTPUT_BYTES`].
pub const TRUNCATION_MARKER: &str = "[output truncated at 64 KiB]";

/// How long the output pipes may stay open after the command's processes
/// were killed (a descendant that escaped the kill keeps them open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellExit {
    /// The shell exited with this code.
    Code(i32),
    /// The shell was killed by this signal.
    Signal(i32),
    /// The run hit the timeout (seconds) and was killed.
    TimedOut(u64),
}

impl std::fmt::Display for ShellExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exit {code}"),
            Self::Signal(signal) => write!(f, "killed by signal {signal}"),
            Self::TimedOut(secs) => write!(f, "timed out after {secs} s"),
        }
    }
}

impl ShellExit {
    /// Whether the command exited with code 0.
    #[must_use]
    pub fn success(self) -> bool {
        self == Self::Code(0)
    }
}

/// One finished run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    /// Stdout and stderr in arrival order (lossy UTF-8), ending with
    /// [`TRUNCATION_MARKER`] when cut.
    pub output: String,
    /// How it ended.
    pub exit: ShellExit,
    /// Wall time.
    pub duration: Duration,
}

/// The session context a run adds: `[shell run by the user] $ <cmd>`, the
/// output, then `(exit <code>)`.
#[must_use]
pub fn context_text(command: &str, run: &ShellRun) -> String {
    let output = run.output.trim_end_matches('\n');
    let mut text = format!("[shell run by the user] $ {command}\n");
    if !output.is_empty() {
        text.push_str(output);
        text.push('\n');
    }
    text.push('(');
    text.push_str(&run.exit.to_string());
    text.push(')');
    text
}

/// What runs a user's command: the user's shell in the workspace, and
/// the timeout.
#[derive(Debug, Clone)]
pub struct UserShell {
    program: PathBuf,
    workspace: PathBuf,
    timeout: Duration,
}

impl UserShell {
    /// The runner of `program` over `workspace` with `timeout`.
    #[must_use]
    pub fn new(program: PathBuf, workspace: &Path, timeout: Duration) -> Self {
        Self {
            program,
            workspace: std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf()),
            timeout,
        }
    }

    /// The user's shell over `workspace` with `timeout`
    /// ([`users_shell`]).
    #[must_use]
    pub fn of_user(workspace: &Path, timeout: Duration) -> Self {
        Self::new(users_shell(std::env::var_os("SHELL")), workspace, timeout)
    }

    /// Run `command` to its end (or the timeout).
    ///
    /// # Errors
    ///
    /// [`UserShellError`] when it cannot start or cannot be waited for.
    pub async fn run(&self, command: &str) -> Result<ShellRun, UserShellError> {
        let started = Instant::now();
        let mut child = self.spawn(command)?;
        let captured = Arc::new(Mutex::new(Captured::default()));
        let mut pumps = tokio::task::JoinSet::new();
        if let Some(out) = child.child.stdout.take() {
            pumps.spawn(pump(out, Arc::clone(&captured)));
        }
        if let Some(err) = child.child.stderr.take() {
            pumps.spawn(pump(err, Arc::clone(&captured)));
        }
        let exit = match tokio::time::timeout(self.timeout, child.child.wait()).await {
            Ok(Ok(status)) => exit_of(status),
            Ok(Err(err)) => {
                child.kill();
                return Err(UserShellError::Wait(err));
            }
            Err(_) => ShellExit::TimedOut(self.timeout.as_secs()),
        };
        // Whatever the command left running dies with it.
        child.kill();
        let _ = tokio::time::timeout(DRAIN_GRACE, async {
            while pumps.join_next().await.is_some() {}
        })
        .await;
        pumps.abort_all();
        let output = captured.lock().text();
        Ok(ShellRun {
            output,
            exit,
            duration: started.elapsed(),
        })
    }

    fn spawn(&self, command: &str) -> Result<Child, UserShellError> {
        let mut process = tokio::process::Command::new(&self.program);
        process
            .args([OsString::from("-c"), OsString::from(command)])
            .current_dir(&self.workspace)
            .envs(BTreeMap::from([(
                OsString::from("PWD"),
                self.workspace.as_os_str().to_owned(),
            )]))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        let child = process.spawn().map_err(|source| UserShellError::Start {
            program: self.program.clone(),
            source,
        })?;
        let group = child.id().map(GroupKill);
        Ok(Child { child, group })
    }
}

/// The user's shell: `shell` (the value of `$SHELL`) when it names an
/// absolute path, else `/bin/sh`.
#[must_use]
pub fn users_shell(shell: Option<OsString>) -> PathBuf {
    shell
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Why a `!` command could not run.
#[derive(Debug, thiserror::Error)]
pub enum UserShellError {
    /// The shell could not be started.
    #[error("could not start {}: {source}", program.display())]
    Start {
        /// The shell program.
        program: std::path::PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The command's exit could not be read.
    #[error("could not wait for the command: {0}")]
    Wait(std::io::Error),
}

/// A running command and its process group.
struct Child {
    child: tokio::process::Child,
    /// Its process group, killed on drop.
    group: Option<GroupKill>,
}

impl Child {
    /// Kill what is left: the process group, then the shell itself.
    fn kill(&mut self) {
        if let Some(group) = &self.group {
            group.kill();
        }
        let _ = self.child.start_kill();
    }
}

/// A run's process group (its leader's pid), killed when dropped.
struct GroupKill(u32);

impl GroupKill {
    fn kill(&self) {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.0) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}

fn exit_of(status: std::process::ExitStatus) -> ShellExit {
    if let Some(code) = status.code() {
        return ShellExit::Code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return ShellExit::Signal(signal);
        }
    }
    ShellExit::Code(-1)
}

/// Output captured so far, capped at [`MAX_OUTPUT_BYTES`].
#[derive(Debug, Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Captured {
    fn push(&mut self, chunk: &[u8]) {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(TRUNCATION_MARKER);
        }
        text
    }
}

/// Copy `reader` into `captured` until EOF (reading on past the cap so the
/// command never blocks on a full pipe).
async fn pump(mut reader: impl AsyncRead + Unpin, captured: Arc<Mutex<Captured>>) {
    let mut chunk = vec![0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => captured.lock().push(&chunk[..n]),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_context_names_the_command_its_output_and_its_exit() {
        let run = ShellRun {
            output: "hi\n".into(),
            exit: ShellExit::Code(0),
            duration: Duration::ZERO,
        };
        assert_eq!(
            context_text("echo hi", &run),
            "[shell run by the user] $ echo hi\nhi\n(exit 0)"
        );
        let silent = ShellRun {
            output: String::new(),
            exit: ShellExit::Code(1),
            duration: Duration::ZERO,
        };
        assert_eq!(
            context_text("false", &silent),
            "[shell run by the user] $ false\n(exit 1)"
        );
        let slow = ShellRun {
            exit: ShellExit::TimedOut(120),
            ..run
        };
        assert!(context_text("sleep 999", &slow).ends_with("hi\n(timed out after 120 s)"));
    }

    #[test]
    fn output_is_capped_with_a_marker() {
        let mut captured = Captured::default();
        captured.push(&vec![b'a'; MAX_OUTPUT_BYTES - 1]);
        captured.push(b"bc");
        let text = captured.text();
        assert_eq!(text.len(), MAX_OUTPUT_BYTES + 1 + TRUNCATION_MARKER.len());
        assert!(text.ends_with(&format!("ab\n{TRUNCATION_MARKER}")));
        let mut small = Captured::default();
        small.push(b"ok\n");
        assert_eq!(small.text(), "ok\n");
    }

    fn plain(timeout: Duration) -> UserShell {
        UserShell::new(PathBuf::from("/bin/sh"), &std::env::temp_dir(), timeout)
    }

    #[test]
    fn the_users_shell_is_shell_when_absolute_else_bin_sh() {
        assert_eq!(
            users_shell(Some("/usr/bin/zsh".into())),
            PathBuf::from("/usr/bin/zsh")
        );
        assert_eq!(users_shell(Some("zsh".into())), PathBuf::from("/bin/sh"));
        assert_eq!(users_shell(None), PathBuf::from("/bin/sh"));
    }

    /// A `!` command inherits rkat-tui's whole environment and runs in the
    /// workspace.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_has_the_users_environment_and_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(dir.path()).unwrap();
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let run = UserShell::new(PathBuf::from("/bin/sh"), &ws, Duration::from_secs(30))
            .run("echo \"HOME=[$HOME]\"; pwd")
            .await
            .unwrap();
        assert!(
            run.output
                .contains(&format!("HOME=[{}]", home.to_string_lossy())),
            "{run:?}"
        );
        assert!(run.output.contains(&ws.display().to_string()), "{run:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_plain_run_merges_both_streams_and_reports_the_exit() {
        let run = plain(Duration::from_secs(30))
            .run("echo out; echo err >&2; exit 3")
            .await
            .unwrap();
        assert!(
            run.output.contains("out\n") && run.output.contains("err\n"),
            "{run:?}"
        );
        assert_eq!(run.exit, ShellExit::Code(3));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_past_the_timeout_is_killed() {
        let started = Instant::now();
        let run = plain(Duration::from_millis(300))
            .run("echo started; sleep 30 & sleep 30")
            .await
            .unwrap();
        assert_eq!(run.exit, ShellExit::TimedOut(0));
        assert!(run.output.contains("started"), "{run:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_output_is_capped() {
        let run = plain(Duration::from_secs(30))
            .run("head -c 200000 /dev/zero | tr '\\0' x")
            .await
            .unwrap();
        assert!(
            run.output.ends_with(TRUNCATION_MARKER),
            "{}",
            run.output.len()
        );
        assert_eq!(run.exit, ShellExit::Code(0));
    }
}
