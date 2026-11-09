//! `!` shell mode: a command the user typed runs under the agent shell's
//! own confinement, and its output joins the session's context.
//!
//! The engine runs the agent's shell only as its `shell` tool
//! (`meerkat_tools::builtin::shell::ShellTool`, behind the dispatcher and
//! the approval gate); its launch path (`custody_spawn`) is private to the
//! crate, so there is no host-callable confined runner to reuse. A user
//! command is therefore launched here through the sandbox crate directly,
//! with exactly what the bootstrap gives the shell tool:
//!
//! - the same requirement ([`crate::bootstrap::ConfinementPlan::shell`]):
//!   [`ShellConfinement::Required`] compiles it with
//!   [`meerkat_sandbox::CompiledConfinement`] and binds one
//!   [`meerkat_sandbox::ProcessLaunchSpec`] per run; under
//!   [`ShellConfinement::TrustedHost`] (`--no-confinement`) the command runs
//!   plain, as the agent's then does. Nothing runs unconfined when the
//!   session is confined: a requirement the backend refuses refuses the run;
//! - the same program (the tool's resolved shell,
//!   [`crate::bootstrap::approval::resolved_shell_program`]) with `-c`;
//! - the same working directory (the canonical workspace) and environment:
//!   `PWD` plus the curated set ([`crate::bootstrap::ConfinementPlan::shell_env`])
//!   when confined, the whole environment of rkat-tui when not;
//! - stdin `/dev/null`.
//!
//! There is no approval gate: the user typed the command. It runs for at
//! most `[shell] user_timeout_secs` (default
//! [`DEFAULT_USER_TIMEOUT_SECS`]); stdout and stderr are captured together
//! in arrival order, up to [`MAX_OUTPUT_BYTES`] with a truncation marker.
//! On a timeout, a cancel (the run's future dropped) or the command's exit,
//! its processes are killed: the process group of a plain run, the PID
//! namespace of a confined one on Linux (the namespace dies with its first
//! process), the direct child on macOS.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use meerkat_tools::builtin::shell::ShellConfinement;
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

/// What runs a user's command: the shell tool's program, workspace,
/// confinement and environment, and the timeout.
#[derive(Clone)]
pub struct UserShell {
    program: Option<PathBuf>,
    workspace: PathBuf,
    launch: Launch,
    timeout: Duration,
}

impl std::fmt::Debug for UserShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserShell")
            .field("program", &self.program)
            .field("workspace", &self.workspace)
            .field("confined", &self.confined())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// How a run is launched.
#[derive(Clone)]
enum Launch {
    /// Plain, inheriting rkat-tui's environment.
    Trusted,
    /// Under the compiled requirement (or the backend's refusal of it),
    /// with exactly `env`.
    Confined {
        compiled:
            Result<Arc<meerkat_sandbox::CompiledConfinement>, meerkat_sandbox::ConfinementRefusal>,
        env: HashMap<String, String>,
    },
}

impl UserShell {
    /// The runner for `shell` (the agent shell's requirement) over
    /// `workspace`, with `shell_env` (the curated environment of a confined
    /// shell) and `timeout`. `program` is the shell to run (`None`: none was
    /// found; every run then fails).
    #[must_use]
    pub fn new(
        program: Option<PathBuf>,
        workspace: &Path,
        shell: &ShellConfinement,
        shell_env: Option<&HashMap<String, String>>,
        timeout: Duration,
    ) -> Self {
        let launch = match shell {
            ShellConfinement::TrustedHost => Launch::Trusted,
            ShellConfinement::Required { requirement } => Launch::Confined {
                compiled: meerkat_sandbox::CompiledConfinement::compile(requirement).map(Arc::new),
                env: shell_env.cloned().unwrap_or_default(),
            },
        };
        Self {
            program,
            workspace: std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf()),
            launch,
            timeout,
        }
    }

    /// Whether runs are confined.
    #[must_use]
    pub fn confined(&self) -> bool {
        matches!(self.launch, Launch::Confined { .. })
    }

    /// Run `command` to its end (or the timeout).
    ///
    /// # Errors
    ///
    /// [`UserShellError`] when it cannot start (no shell, a refused
    /// confinement, a failed spawn) or cannot be waited for.
    pub async fn run(&self, command: &str) -> Result<ShellRun, UserShellError> {
        let program = self.program.as_ref().ok_or(UserShellError::NoShell)?;
        let started = Instant::now();
        let args = [OsString::from("-c"), OsString::from(command)];
        let mut child = match &self.launch {
            Launch::Trusted => self.spawn_trusted(program, &args)?,
            Launch::Confined { compiled, env } => {
                self.spawn_confined(compiled, env, program, &args)?
            }
        };
        let captured = Arc::new(Mutex::new(Captured::default()));
        let mut pumps = tokio::task::JoinSet::new();
        if let Some(out) = child.stdout() {
            pumps.spawn(pump(out, Arc::clone(&captured)));
        }
        if let Some(err) = child.stderr() {
            pumps.spawn(pump(err, Arc::clone(&captured)));
        }
        let exit = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => exit_of(status),
            Ok(Err(err)) => {
                child.kill();
                return Err(UserShellError::Wait(err));
            }
            Err(_) => ShellExit::TimedOut(self.timeout.as_secs()),
        };
        // Whatever the command left running dies with it, as the agent's
        // shell calls do.
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

    fn environment(&self, env: Option<&HashMap<String, String>>) -> BTreeMap<OsString, OsString> {
        let mut out =
            BTreeMap::from([(OsString::from("PWD"), self.workspace.as_os_str().to_owned())]);
        out.extend(
            env.into_iter()
                .flatten()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        out
    }

    fn spawn_trusted(&self, program: &Path, args: &[OsString]) -> Result<Child, UserShellError> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(&self.workspace)
            .envs(self.environment(None))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|source| UserShellError::Start {
            program: program.to_path_buf(),
            source,
        })?;
        Ok(Child::trusted(child))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn spawn_confined(
        &self,
        compiled: &Result<
            Arc<meerkat_sandbox::CompiledConfinement>,
            meerkat_sandbox::ConfinementRefusal,
        >,
        env: &HashMap<String, String>,
        program: &Path,
        args: &[OsString],
    ) -> Result<Child, UserShellError> {
        let refused = UserShellError::Refused;
        let compiled = compiled.as_ref().map_err(|r| refused(*r))?;
        let launch = meerkat_sandbox::ProcessLaunchSpec::new(
            program.to_path_buf(),
            args.to_vec(),
            self.workspace.clone(),
            self.environment(Some(env)),
        )
        .map_err(refused)?;
        let prepared = compiled.bind_launch(launch).map_err(refused)?;
        let child = prepared.spawn().map_err(UserShellError::StartConfined)?;
        Ok(Child::Confined(child))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn spawn_confined(
        &self,
        _compiled: &Result<
            Arc<meerkat_sandbox::CompiledConfinement>,
            meerkat_sandbox::ConfinementRefusal,
        >,
        _env: &HashMap<String, String>,
        _program: &Path,
        _args: &[OsString],
    ) -> Result<Child, UserShellError> {
        Err(UserShellError::NoBackend)
    }
}

/// Why a `!` command could not run.
#[derive(Debug, thiserror::Error)]
pub enum UserShellError {
    /// No shell program was found.
    #[error("no shell found (nu, $SHELL, bash, zsh or sh)")]
    NoShell,
    /// The sandbox refuses the launch.
    #[error("the sandbox refuses the command: {0}")]
    Refused(meerkat_sandbox::ConfinementRefusal),
    /// The shell could not be started.
    #[error("could not start {}: {source}", program.display())]
    Start {
        /// The shell program.
        program: std::path::PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The confined shell could not be started.
    #[error("could not start the confined command: {0}")]
    StartConfined(std::io::Error),
    /// The command's exit could not be read.
    #[error("could not wait for the command: {0}")]
    Wait(std::io::Error),
    /// Confinement is required and this platform has no sandbox backend.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[error("this platform has no sandbox backend")]
    NoBackend,
}

/// A running command.
enum Child {
    Trusted {
        child: tokio::process::Child,
        /// Its process group, killed on drop.
        group: Option<GroupKill>,
    },
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Confined(meerkat_sandbox::NativeChild),
}

impl Child {
    fn trusted(child: tokio::process::Child) -> Self {
        let group = child.id().map(GroupKill);
        Self::Trusted { child, group }
    }

    fn stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        match self {
            Self::Trusted { child, .. } => child.stdout.take(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Confined(child) => child.stdout.take(),
        }
    }

    fn stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        match self {
            Self::Trusted { child, .. } => child.stderr.take(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Confined(child) => child.stderr.take(),
        }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            Self::Trusted { child, .. } => child.wait().await,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Confined(child) => child.wait().await,
        }
    }

    /// Kill what is left: the process group (plain) or the direct child
    /// (confined; on Linux its PID namespace goes with it).
    fn kill(&mut self) {
        match self {
            Self::Trusted { child, group } => {
                if let Some(group) = group {
                    group.kill();
                }
                let _ = child.start_kill();
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Confined(child) => {
                let _ = child.start_kill_without_reap();
            }
        }
    }
}

/// A plain run's process group (its leader's pid), killed when dropped.
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
        UserShell::new(
            Some(PathBuf::from("/bin/sh")),
            &std::env::temp_dir(),
            &ShellConfinement::TrustedHost,
            None,
            timeout,
        )
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
