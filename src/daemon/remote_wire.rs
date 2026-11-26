//! Wires to agents on other machines over ssh (`/wire ssh://<host>/…`),
//! the way rsync uses ssh: the daemon of the machine where the user typed
//! `/wire` runs `ssh <host> rkat-tui --wire-remote` and speaks a small
//! line protocol with it ([`Request`], [`Reply`]: one JSON line each way,
//! version-checked) to list the remote's agents, fetch the remote agent's
//! card, hand over its own agent's card, and take a wire down again. The
//! remote side ([`crate::client::wire_remote`]) does there only what a
//! local `/wire` does: it calls that user's own daemons through their
//! endpoint files.
//!
//! The messages travel over one long-lived ssh connection per wire
//! ([`SshLinks`]) that carries both directions as Unix-socket forwards: `-L
//! <here>:<the remote agent's socket>` so this agent's sends reach the
//! remote agent, and `-R <there>:<this agent's socket>` so the remote
//! agent's sends reach this one. Each daemon's card for the other names
//! the forwarded socket on its own machine, so each side is an ordinary
//! wire to a local Unix socket (`crate::daemon::wires`). Every envelope is
//! still signed by the sending agent's key and checked against the one key
//! the receiving daemon trusts for that wire (the engine's handler): ssh
//! is the transport, not the trust.
//!
//! This module holds the only `ssh` launch, a direct process launch in
//! library code beside the user's editor, the user's `!` shell and the
//! client's start of the daemon: the argument vector is built here
//! ([`control_argv`], [`link_argv`]) and passed to the program as is, never
//! through a local shell, with the host validated ([`SshHost`]) and `--`
//! before it. Only a client's `rkat/wire_ssh` (the user's `/wire`) and a
//! stored wire made so start one; nothing the agent can call does. ssh
//! runs with `BatchMode=yes`: rkat-tui stores no credentials and never
//! asks for a password; the user's `~/.ssh/config` applies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

use super::protocol::WireCard;

/// The line protocol's version: both ends must speak the same one.
pub const PROTOCOL: u32 = 1;

/// The flag of `rkat-tui` that answers on the remote side.
pub const REMOTE_FLAG: &str = "--wire-remote";

/// The remote program when the user names none: found on the remote's
/// `PATH` for a non-interactive shell.
pub const DEFAULT_PROGRAM: &str = "rkat-tui";

/// How long a control call may take, connection included.
const CALL_BOUND: Duration = Duration::from_secs(30);

/// How long a link may take to be up (connected, both forwards bound).
const READY_BOUND: Duration = Duration::from_secs(30);

/// The first pause before a link is tried again, and the longest.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_mins(1);

/// A link up this long counts as a good one: the next pause starts over.
const STEADY: Duration = Duration::from_mins(1);

/// The ssh options every launch sets: no prompt ever (a host that needs
/// one fails with ssh's own words), and a bounded connect.
const BATCH: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"];

/// The options of a link: it fails when a forward cannot be bound, a dead
/// peer is noticed within about 45 s, and a stale socket file of a dead
/// link here is replaced.
const LINK: [&str; 8] = [
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "StreamLocalBindUnlink=yes",
];

/// Why a remote wire failed, in words the user can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    /// The host is no plain ssh destination.
    #[error(
        "`{0}` is no ssh host: give it as ssh takes it (an alias of your ~/.ssh/config, host or user@host), of letters, digits and `._@:%[]-`, not starting with `-`"
    )]
    BadHost(String),
    /// The remote program's path cannot be passed safely.
    #[error(
        "`{0}` cannot be the remote rkat-tui: give a plain path (letters, digits, `/`, `.`, `_`, `-`, `~`, `+`)"
    )]
    BadProgram(String),
    /// A socket path cannot be forwarded.
    #[error(
        "the socket {path} cannot be forwarded over ssh: {why} (a shorter --config-dir on that machine makes it fit)"
    )]
    BadSocket {
        /// The path.
        path: String,
        why: String,
    },
    /// No `ssh` on this machine.
    #[error("ssh cannot be started on this machine ({0}): install OpenSSH's client")]
    NoSsh(String),
    /// ssh itself failed (connect, authentication, host key).
    #[error(
        "ssh to {host} failed: {said}. rkat-tui runs ssh without prompts (BatchMode): make `ssh {host} true` work first (your key in the agent or ~/.ssh/config, the host in known_hosts)"
    )]
    Ssh {
        host: String,
        /// What ssh said last.
        said: String,
    },
    /// The remote shell found no rkat-tui.
    #[error(
        "no rkat-tui on {host} for a non-interactive shell ({said}): install rkat-tui there (the same version as here, {build}), or name it with `/wire ssh://{host}/… --remote-rkat-tui <path>`"
    )]
    NoRemote {
        host: String,
        /// What the remote said.
        said: String,
        build: String,
    },
    /// The remote rkat-tui does not speak this protocol.
    #[error(
        "the rkat-tui on {host} cannot answer this one ({said}): install the same rkat-tui there as here ({build})"
    )]
    Version {
        host: String,
        /// What was wrong.
        said: String,
        build: String,
    },
    /// The remote side refused (its words).
    #[error("{host}: {said}")]
    Refused {
        host: String,
        /// Its words.
        said: String,
    },
    /// No answer in time.
    #[error("{host} did not answer within {secs} s")]
    Timeout {
        host: String,
        /// The bound.
        secs: u64,
    },
}

/// An ssh destination as the user gave it: passed to `ssh` as one
/// argument after `--`, so the user's `~/.ssh/config` (aliases, users,
/// ports, jump hosts, keys) applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshHost(String);

impl SshHost {
    /// `host`, when ssh can take it as its destination and no option can
    /// be read into it: not empty, no leading `-`, only letters, digits and
    /// `._@:%[]-`. A shell metacharacter never reaches the user's own
    /// `ProxyCommand ... %h` through a pasted `/wire ssh://…` (the
    /// CVE-2023-51385 class).
    ///
    /// # Errors
    ///
    /// [`RemoteError::BadHost`].
    pub fn parse(host: &str) -> Result<Self, RemoteError> {
        let fine = !host.is_empty()
            && !host.starts_with('-')
            && host.len() <= 255
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._@:%[]-".contains(c));
        if fine {
            Ok(Self(host.to_string()))
        } else {
            Err(RemoteError::BadHost(host.to_string()))
        }
    }

    /// The host as given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SshHost {
    type Error = RemoteError;
    fn try_from(host: String) -> Result<Self, Self::Error> {
        Self::parse(&host)
    }
}

impl From<SshHost> for String {
    fn from(host: SshHost) -> Self {
        host.0
    }
}

impl std::fmt::Display for SshHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The remote rkat-tui's program, as the remote shell runs it.
///
/// # Errors
///
/// [`RemoteError::BadProgram`] for anything but a plain path: ssh hands
/// the remote command to the remote user's shell as one line.
pub fn remote_program(program: Option<&str>) -> Result<String, RemoteError> {
    let program = program.unwrap_or(DEFAULT_PROGRAM);
    let plain = !program.is_empty()
        && !program.starts_with('-')
        && program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~+".contains(c));
    if plain {
        Ok(program.to_string())
    } else {
        Err(RemoteError::BadProgram(program.to_string()))
    }
}

/// One request to the remote side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// [`PROTOCOL`].
    pub rkat_wire: u32,
    /// What to do.
    #[serde(flatten)]
    pub op: Op,
}

impl Request {
    /// `op` at this protocol.
    #[must_use]
    pub fn new(op: Op) -> Self {
        Self {
            rkat_wire: PROTOCOL,
            op,
        }
    }
}

/// What the remote side is asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// The top-level agents of every running daemon there
    /// ([`RemoteAgent`]s).
    List,
    /// The card of the agent `target` names (`<workspace>/<agent>`, the
    /// workspace by name or path, or a workspace with one agent):
    /// [`CardReply`].
    Card {
        /// The agent.
        target: String,
    },
    /// Wire the agent `target` names to `card` (its address the forwarded
    /// socket on that machine): [`crate::daemon::protocol::Wired`].
    Wire {
        /// The agent there.
        target: String,
        /// The agent here, as that machine reaches it.
        card: WireCard,
    },
    /// Remove the wire of the agent `target` names to `peer` of
    /// `workspace`: [`crate::daemon::protocol::Unwired`].
    Unwire {
        /// The agent there.
        target: String,
        /// The other agent's workspace.
        workspace: PathBuf,
        /// The other agent's name.
        peer: String,
    },
    /// Remove a stale forwarded socket left by a dead link (only a socket
    /// named as [`forward_name`] names it, in a realm's wire directory).
    Clear {
        /// The socket.
        path: PathBuf,
    },
    /// The link's own command: answer once `forward` (the `-R` socket)
    /// exists, then hold until the connection ends, and remove it then.
    Hold {
        /// The forwarded socket there.
        forward: PathBuf,
    },
}

/// The remote side's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    /// [`PROTOCOL`].
    pub rkat_wire: u32,
    /// The remote rkat-tui's build.
    #[serde(default)]
    pub build: String,
    /// What it answers (the op's result).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    /// Why it refused, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    /// The answer `value`.
    #[must_use]
    pub fn ok(value: serde_json::Value) -> Self {
        Self {
            rkat_wire: PROTOCOL,
            build: crate::BUILD.to_string(),
            ok: Some(value),
            error: None,
        }
    }

    /// The refusal `why`.
    #[must_use]
    pub fn refused(why: impl Into<String>) -> Self {
        Self {
            rkat_wire: PROTOCOL,
            build: crate::BUILD.to_string(),
            ok: None,
            error: Some(why.into()),
        }
    }

    /// The answer as `T`, after the version check.
    ///
    /// # Errors
    ///
    /// [`RemoteError::Version`], [`RemoteError::Refused`].
    pub fn into_result<T: serde::de::DeserializeOwned>(
        self,
        host: &SshHost,
    ) -> Result<T, RemoteError> {
        if self.rkat_wire != PROTOCOL {
            return Err(RemoteError::Version {
                host: host.to_string(),
                said: format!(
                    "it speaks wire protocol {} (build {}), this one {PROTOCOL}",
                    self.rkat_wire, self.build
                ),
                build: crate::BUILD.to_string(),
            });
        }
        if let Some(why) = self.error {
            return Err(RemoteError::Refused {
                host: host.to_string(),
                said: why,
            });
        }
        serde_json::from_value(self.ok.unwrap_or_default()).map_err(|err| RemoteError::Version {
            host: host.to_string(),
            said: format!("its answer is unreadable here: {err}"),
            build: crate::BUILD.to_string(),
        })
    }
}

/// One agent the remote side lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAgent {
    /// Its workspace there.
    pub workspace: PathBuf,
    pub agent: String,
    /// `running`, `idle` or `unknown`.
    pub run: String,
    /// How many clients are attached to it there.
    pub clients: usize,
}

/// [`Op::Card`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardReply {
    /// The agent's card (its own socket on that machine).
    pub card: WireCard,
    /// The longest socket path that machine binds.
    pub max_socket_path: usize,
}

/// The name of a forwarded socket in a realm's `wire/` directory: `s-`
/// and 16 hex digits of what it joins.
#[must_use]
pub fn forward_name(key: &str) -> String {
    let digest = crate::hex::sha256(key.as_bytes());
    format!("s-{}.sock", digest.get(..16).unwrap_or(&digest))
}

/// Whether `path` is a forwarded socket's path ([`forward_name`]) in a
/// directory called `wire`.
#[must_use]
pub fn is_forward_path(path: &Path) -> bool {
    let named = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n.len() == 23
            && n.starts_with("s-")
            && n.ends_with(".sock")
            && n[2..18].chars().all(|c| c.is_ascii_hexdigit())
    });
    named
        && path.is_absolute()
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|d| d == super::wires::SOCKET_DIR)
}

/// `path` as one side of a `-L`/`-R` forward: absolute, within `limit`
/// bytes, and with nothing ssh would read as a separator.
///
/// # Errors
///
/// [`RemoteError::BadSocket`].
pub fn forwardable(path: &Path, limit: usize) -> Result<String, RemoteError> {
    let text = path.to_str().ok_or_else(|| RemoteError::BadSocket {
        path: path.display().to_string(),
        why: "it is not UTF-8".to_string(),
    })?;
    let why = if !path.is_absolute() {
        Some("it is not absolute".to_string())
    } else if text.len() > limit {
        Some(format!(
            "it is {} bytes long, more than the {limit} a Unix socket path may have there",
            text.len()
        ))
    } else if text
        .chars()
        .any(|c| c == ':' || c.is_whitespace() || c.is_control())
    {
        Some("it holds a `:` or a space, which ssh's forward syntax cannot carry".to_string())
    } else {
        None
    };
    match why {
        Some(why) => Err(RemoteError::BadSocket {
            path: text.to_string(),
            why,
        }),
        None => Ok(text.to_string()),
    }
}

/// What one link forwards ([`link_argv`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpec {
    pub host: SshHost,
    pub program: String,
    /// The `-L` socket here, which this agent's card for the remote one
    /// names.
    pub local_forward: PathBuf,
    /// The remote agent's own socket there.
    pub remote_target: PathBuf,
    /// The `-R` socket there, which the remote agent's card for this one
    /// names.
    pub remote_forward: PathBuf,
    /// This agent's own socket here.
    pub local_target: PathBuf,
}

/// The argument vector (after the program) of one control call:
/// `-o BatchMode=yes -o ConnectTimeout=15 <extra…> -- <host> <program>
/// --wire-remote`. The request goes on its standard input.
#[must_use]
pub fn control_argv(host: &SshHost, program: &str, extra: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = BATCH.iter().map(|s| (*s).to_string()).collect();
    argv.extend(extra.iter().cloned());
    argv.extend([
        "--".to_string(),
        host.0.clone(),
        program.to_string(),
        REMOTE_FLAG.to_string(),
    ]);
    argv
}

/// The argument vector (after the program) of one link: the control
/// options, the link's ([`LINK`]), `-L <local forward>:<remote target>
/// -R <remote forward>:<local target>`, `<extra…> -- <host> <program>
/// --wire-remote`, with [`Op::Hold`] on its standard input.
///
/// # Errors
///
/// [`RemoteError::BadSocket`] for a path a forward cannot carry.
pub fn link_argv(
    spec: &LinkSpec,
    extra: &[String],
    remote_limit: usize,
) -> Result<Vec<String>, RemoteError> {
    let limit = super::wires::MAX_SOCKET_PATH;
    let local_forward = forwardable(&spec.local_forward, limit)?;
    let local_target = forwardable(&spec.local_target, limit)?;
    let remote_forward = forwardable(&spec.remote_forward, remote_limit)?;
    let remote_target = forwardable(&spec.remote_target, remote_limit)?;
    let mut argv: Vec<String> = BATCH.iter().map(|s| (*s).to_string()).collect();
    argv.extend(LINK.iter().map(|s| (*s).to_string()));
    argv.extend([
        "-L".to_string(),
        format!("{local_forward}:{remote_target}"),
        "-R".to_string(),
        format!("{remote_forward}:{local_target}"),
    ]);
    argv.extend(extra.iter().cloned());
    argv.extend([
        "--".to_string(),
        spec.host.0.clone(),
        spec.program.clone(),
        REMOTE_FLAG.to_string(),
    ]);
    Ok(argv)
}

/// A link that is up: it ends when its connection does.
pub trait LinkRun: Send {
    /// Resolves when the link ended, with why in words. Dropping the
    /// link takes it down.
    fn ended(self: Box<Self>) -> BoxFuture<'static, String>;
}

/// How the daemon reaches another machine: [`Ssh`], or a stand-in under
/// test.
pub trait Transport: Send + Sync {
    /// One control call: `request` to the remote side, its reply.
    fn call(
        &self,
        host: &SshHost,
        program: &str,
        request: Request,
    ) -> BoxFuture<'_, Result<Reply, RemoteError>>;
    /// One link, once up (both forwards bound, the remote side holding).
    fn link(
        &self,
        spec: LinkSpec,
        remote_limit: usize,
    ) -> BoxFuture<'_, Result<Box<dyn LinkRun>, RemoteError>>;
}

/// The `ssh` program, with options of the user's own (`extra`, before
/// the host).
#[derive(Debug, Clone)]
pub struct Ssh {
    program: PathBuf,
    extra: Vec<String>,
}

impl Default for Ssh {
    fn default() -> Self {
        Self {
            program: PathBuf::from("ssh"),
            extra: Vec::new(),
        }
    }
}

impl Ssh {
    /// `program` (in place of `ssh`) with `extra` options before the host
    /// (a test's private ssh configuration).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with(program: PathBuf, extra: Vec<String>) -> Self {
        Self { program, extra }
    }

    /// The one launch: `argv` passed to the program as is (no shell), its
    /// three streams piped, killed when dropped.
    fn spawn(&self, argv: &[String]) -> Result<tokio::process::Child, RemoteError> {
        tokio::process::Command::new(&self.program)
            .args(argv)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| RemoteError::NoSsh(format!("{}: {err}", self.program.display())))
    }
}

/// What a failed ssh launch comes to, from its exit code and what it
/// wrote to standard error.
#[must_use]
pub fn launch_failure(host: &SshHost, code: Option<i32>, stderr: &str) -> RemoteError {
    let said = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("it said nothing")
        .to_string();
    let lower = stderr.to_lowercase();
    let not_found = lower.contains("command not found")
        || (lower.contains("no such file or directory") && !lower.contains("ssh:"));
    if code == Some(127) || not_found {
        return RemoteError::NoRemote {
            host: host.to_string(),
            said,
            build: crate::BUILD.to_string(),
        };
    }
    if lower.contains(&REMOTE_FLAG.to_lowercase())
        && (lower.contains("unexpected argument")
            || lower.contains("unrecognized")
            || lower.contains("unknown"))
    {
        return RemoteError::Version {
            host: host.to_string(),
            said: format!("it has no {REMOTE_FLAG}"),
            build: crate::BUILD.to_string(),
        };
    }
    RemoteError::Ssh {
        host: host.to_string(),
        said,
    }
}

/// Read one reply line from `stdout`, or the launch's failure.
async fn read_reply(
    host: &SshHost,
    child: &mut tokio::process::Child,
) -> Result<Reply, RemoteError> {
    let stdout = child.stdout.take();
    let mut line = String::new();
    if let Some(stdout) = stdout {
        let mut reader = tokio::io::BufReader::new(stdout);
        let _ = reader.read_line(&mut line).await;
        child.stdout = Some(reader.into_inner());
    }
    if let Ok(reply) = serde_json::from_str::<Reply>(line.trim()) {
        return Ok(reply);
    }
    // No reply: what ssh, or the remote shell, said instead.
    let status = child.wait().await.ok();
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut err, &mut stderr).await;
    }
    if !line.trim().is_empty() && stderr.trim().is_empty() {
        return Err(RemoteError::Version {
            host: host.to_string(),
            said: format!("it answered `{}`", line.trim()),
            build: crate::BUILD.to_string(),
        });
    }
    Err(launch_failure(host, status.and_then(|s| s.code()), &stderr))
}

/// Write `request` as one line to the child's standard input.
async fn write_request(child: &mut tokio::process::Child, request: &Request) -> Result<(), String> {
    let mut line = serde_json::to_string(request).map_err(|err| err.to_string())?;
    line.push('\n');
    let Some(stdin) = child.stdin.as_mut() else {
        return Err("no standard input".to_string());
    };
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    stdin.flush().await.map_err(|err| err.to_string())
}

/// A link that runs as an ssh process.
struct SshLink {
    child: tokio::process::Child,
}

impl LinkRun for SshLink {
    fn ended(self: Box<Self>) -> BoxFuture<'static, String> {
        let mut child = self.child;
        Box::pin(async move {
            // `wait` closes the child's standard input first, and its end
            // is the end of the remote side's hold: kept open until then.
            let stdin = child.stdin.take();
            let status = child.wait().await;
            drop(stdin);
            let mut stderr = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut err, &mut stderr).await;
            }
            let said = stderr
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .map(|line| format!(": {line}"))
                .unwrap_or_default();
            match status {
                Ok(status) => format!("ssh ended ({status}){said}"),
                Err(err) => format!("ssh could not be waited for: {err}"),
            }
        })
    }
}

impl Transport for Ssh {
    fn call(
        &self,
        host: &SshHost,
        program: &str,
        request: Request,
    ) -> BoxFuture<'_, Result<Reply, RemoteError>> {
        let argv = control_argv(host, program, &self.extra);
        let host = host.clone();
        Box::pin(async move {
            let mut child = self.spawn(&argv)?;
            let talk = async {
                if let Err(why) = write_request(&mut child, &request).await {
                    tracing::debug!(%host, %why, "the request did not go to ssh");
                }
                // The remote side answers one line and ends.
                drop(child.stdin.take());
                read_reply(&host, &mut child).await
            };
            tokio::time::timeout(CALL_BOUND, talk)
                .await
                .map_err(|_| RemoteError::Timeout {
                    host: host.to_string(),
                    secs: CALL_BOUND.as_secs(),
                })?
        })
    }

    fn link(
        &self,
        spec: LinkSpec,
        remote_limit: usize,
    ) -> BoxFuture<'_, Result<Box<dyn LinkRun>, RemoteError>> {
        Box::pin(async move {
            let argv = link_argv(&spec, &self.extra, remote_limit)?;
            let mut child = self.spawn(&argv)?;
            let request = Request::new(Op::Hold {
                forward: spec.remote_forward.clone(),
            });
            let host = spec.host.clone();
            let ready = async {
                if let Err(why) = write_request(&mut child, &request).await {
                    tracing::debug!(%host, %why, "the hold request did not go to ssh");
                }
                // Standard input stays open: its end is the link's end there.
                read_reply(&host, &mut child).await
            };
            let reply = tokio::time::timeout(READY_BOUND, ready)
                .await
                .map_err(|_| RemoteError::Timeout {
                    host: host.to_string(),
                    secs: READY_BOUND.as_secs(),
                })??;
            reply.into_result::<serde_json::Value>(&host)?;
            Ok(Box::new(SshLink { child }) as Box<dyn LinkRun>)
        })
    }
}

/// Whether a link is up, as `/agents`, the `/wire` picker and the gate see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// Being made (the first time since the daemon started).
    Connecting,
    Up,
    /// Down; the daemon tries again.
    Down,
}

impl LinkState {
    /// `connecting`, `up` or `down`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

impl std::fmt::Display for LinkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Tells the user about one agent's link (agent, warn, text).
pub type Tell = Arc<dyn Fn(&str, bool, String) + Send + Sync>;

/// The pause after the `n`th failure in a row (from 0): doubling from
/// [`BACKOFF_FIRST`], at most [`BACKOFF_MAX`].
#[must_use]
pub fn backoff(failures: u32) -> Duration {
    BACKOFF_FIRST
        .saturating_mul(2u32.saturating_pow(failures.min(16)))
        .min(BACKOFF_MAX)
}

/// One running link's task and its state.
struct Held {
    state: Arc<parking_lot::Mutex<LinkState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The links of the daemon's ssh wires, one per (agent, remote label),
/// each made again after it drops, with a pause that doubles up to a
/// minute.
pub struct SshLinks {
    transport: Arc<dyn Transport>,
    tell: parking_lot::Mutex<Option<Tell>>,
    links: parking_lot::Mutex<HashMap<(String, String), Held>>,
}

impl std::fmt::Debug for SshLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshLinks")
            .field("links", &self.links.lock().len())
            .finish_non_exhaustive()
    }
}

impl SshLinks {
    /// SshLinks over `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Arc<Self> {
        Arc::new(Self {
            transport,
            tell: parking_lot::Mutex::new(None),
            links: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// The transport (control calls go through it too).
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    /// How the user is told of a link going down and coming back.
    pub fn set_tell(&self, tell: Tell) {
        *self.tell.lock() = Some(tell);
    }

    /// The state of `agent`'s link to `label` (`None`: none is held).
    #[must_use]
    pub fn state(&self, agent: &str, label: &str) -> Option<LinkState> {
        self.links
            .lock()
            .get(&(agent.to_string(), label.to_string()))
            .map(|held| *held.state.lock())
    }

    /// Hold `agent`'s link to `label` as `spec` from now on (a link held
    /// already is replaced). `up`: the caller made it up just now (`link`).
    pub fn hold(
        self: &Arc<Self>,
        agent: &str,
        label: &str,
        spec: LinkSpec,
        remote_limit: usize,
        up: Option<Box<dyn LinkRun>>,
    ) {
        let state = Arc::new(parking_lot::Mutex::new(if up.is_some() {
            LinkState::Up
        } else {
            LinkState::Connecting
        }));
        let task = tokio::spawn(Self::run(
            Arc::downgrade(self),
            agent.to_string(),
            label.to_string(),
            spec,
            remote_limit,
            up,
            Arc::clone(&state),
        ));
        self.links
            .lock()
            .insert((agent.to_string(), label.to_string()), Held { state, task });
    }

    /// Take `agent`'s link to `label` down for good.
    pub fn drop_link(&self, agent: &str, label: &str) {
        self.links
            .lock()
            .remove(&(agent.to_string(), label.to_string()));
    }

    /// Take every link of `agent` down (it retired).
    pub fn drop_agent(&self, agent: &str) {
        self.links.lock().retain(|(a, _), _| a != agent);
    }

    /// Take every link down (the daemon stops).
    pub fn close(&self) {
        self.links.lock().clear();
    }

    /// Warn `agent`'s clients of `text`.
    pub fn tell(&self, agent: &str, text: String) {
        let tell = self.tell.lock().clone();
        match tell {
            Some(tell) => tell(agent, true, text),
            None => tracing::warn!(%agent, %text, "nobody to tell"),
        }
    }

    fn say(this: &std::sync::Weak<Self>, agent: &str, warn: bool, text: String) {
        let tell = this.upgrade().and_then(|links| links.tell.lock().clone());
        match tell {
            Some(tell) => tell(agent, warn, text),
            None => tracing::info!(%agent, %text, "a link changed with nobody to tell"),
        }
    }

    /// One link's life: up, then made again whenever it drops.
    async fn run(
        this: std::sync::Weak<Self>,
        agent: String,
        label: String,
        spec: LinkSpec,
        remote_limit: usize,
        mut up: Option<Box<dyn LinkRun>>,
        state: Arc<parking_lot::Mutex<LinkState>>,
    ) {
        let host = spec.host.clone();
        let mut failures: u32 = 0;
        let mut told_down = false;
        loop {
            let link = match up.take() {
                Some(link) => Ok(link),
                None => {
                    let Some(links) = this.upgrade() else { return };
                    let transport = Arc::clone(&links.transport);
                    drop(links);
                    let made = transport.link(spec.clone(), remote_limit).await;
                    if made.is_err() {
                        // A stale `-R` socket there refuses the forward:
                        // removed for the next try (best effort).
                        let clear = Request::new(Op::Clear {
                            path: spec.remote_forward.clone(),
                        });
                        if let Ok(reply) = transport.call(&host, &spec.program, clear).await
                            && let Err(err) = reply.into_result::<serde_json::Value>(&host)
                        {
                            tracing::info!(%agent, peer = %label, error = %err, "the stale forwarded socket there stays");
                        }
                    }
                    made
                }
            };
            match link {
                Ok(link) => {
                    let was = std::mem::replace(&mut *state.lock(), LinkState::Up);
                    if told_down || was == LinkState::Down {
                        Self::say(
                            &this,
                            &agent,
                            false,
                            format!(
                                "the link to {host} is back: messages between this agent and {label} go through again"
                            ),
                        );
                    }
                    tracing::info!(%agent, peer = %label, %host, "the ssh link is up");
                    let since = tokio::time::Instant::now();
                    let why = link.ended().await;
                    *state.lock() = LinkState::Down;
                    tracing::info!(%agent, peer = %label, %host, %why, "the ssh link dropped");
                    Self::say(
                        &this,
                        &agent,
                        true,
                        format!(
                            "the link to {host} is down ({why}): messages between this agent and {label} fail and are not kept until it is back; this machine makes it again"
                        ),
                    );
                    told_down = true;
                    failures = if since.elapsed() >= STEADY {
                        0
                    } else {
                        failures.saturating_add(1)
                    };
                }
                Err(err) => {
                    let was = std::mem::replace(&mut *state.lock(), LinkState::Down);
                    if !told_down {
                        Self::say(
                            &this,
                            &agent,
                            true,
                            format!(
                                "the link to {host} for {label} is down: {err}; this machine tries again"
                            ),
                        );
                        told_down = true;
                    }
                    if was != LinkState::Down {
                        tracing::warn!(%agent, peer = %label, error = %err, "the ssh link could not be made");
                    }
                    failures = failures.saturating_add(1);
                }
            }
            tokio::time::sleep(backoff(failures.saturating_sub(1))).await;
        }
    }
}

/// What a message to `label` over the link to `host` meets while that
/// link is down.
#[must_use]
pub fn down_words(label: &str, host: &str) -> String {
    format!(
        "`{label}` cannot get messages now: the link to {host} is down. The message was not sent and is not kept; the user's machine makes the link again by itself, so try again later, or tell the user."
    )
}

/// What a message to `label` meets on the remote half of a wire made over
/// ssh from the machine `from` while the link is down (its forwarded
/// socket here is gone): that machine holds the link.
#[must_use]
pub fn down_from_words(label: &str, from: &str) -> String {
    format!(
        "`{label}` cannot get messages now: it works on {}, and the ssh link from that machine to this one is down. The message was not sent and is not kept; that machine makes the link again by itself, so try again later, or tell the user.",
        super::protocol::machine(from)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn host(h: &str) -> SshHost {
        SshHost::parse(h).unwrap()
    }

    #[test]
    fn a_host_is_one_plain_argument() {
        for fine in [
            "box",
            "me@box",
            "box.example.org",
            "box:2222",
            "[::1]",
            "alias_1",
        ] {
            assert!(SshHost::parse(fine).is_ok(), "{fine}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "a b",
            "a\tb",
            "a\nb",
            "box/ws",
            "\u{7f}x",
            // Shell metacharacters a `ProxyCommand ... %h` would run.
            "box;id",
            "$(id)",
            "`id`",
            "a|b",
            "a&b",
            "a'b",
            "a\"b",
            "box>x",
        ] {
            let err = SshHost::parse(bad).unwrap_err();
            assert!(err.to_string().contains("is no ssh host"), "{bad}: {err}");
        }
        let json: SshHost = serde_json::from_str("\"me@box\"").unwrap();
        assert_eq!(json.as_str(), "me@box");
        assert!(serde_json::from_str::<SshHost>("\"-x\"").is_err());
    }

    #[test]
    fn the_remote_program_is_a_plain_path() {
        assert_eq!(remote_program(None).unwrap(), "rkat-tui");
        assert_eq!(
            remote_program(Some("~/.cargo/bin/rkat-tui")).unwrap(),
            "~/.cargo/bin/rkat-tui"
        );
        for bad in ["rkat-tui; rm -rf ~", "$(x)", "a b", "-x", "`x`", "a'b", ""] {
            assert!(remote_program(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_control_argv_is_exact() {
        assert_eq!(
            control_argv(&host("me@box"), "rkat-tui", &[]),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "--",
                "me@box",
                "rkat-tui",
                "--wire-remote"
            ]
        );
        assert_eq!(
            control_argv(&host("box"), "/opt/rkat-tui", &["-F".into(), "/t/c".into()]),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-F",
                "/t/c",
                "--",
                "box",
                "/opt/rkat-tui",
                "--wire-remote"
            ]
        );
    }

    fn spec() -> LinkSpec {
        LinkSpec {
            host: host("box"),
            program: "rkat-tui".into(),
            local_forward: "/h/r/wire/s-0123456789abcdef.sock".into(),
            remote_target: "/b/r/wire/main-cd34.sock".into(),
            remote_forward: "/b/r/wire/s-fedcba9876543210.sock".into(),
            local_target: "/h/r/wire/main-ab12.sock".into(),
        }
    }

    #[test]
    fn the_link_argv_is_exact_and_refuses_what_a_forward_cannot_carry() {
        assert_eq!(
            link_argv(&spec(), &[], 103).unwrap(),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-L",
                "/h/r/wire/s-0123456789abcdef.sock:/b/r/wire/main-cd34.sock",
                "-R",
                "/b/r/wire/s-fedcba9876543210.sock:/h/r/wire/main-ab12.sock",
                "--",
                "box",
                "rkat-tui",
                "--wire-remote"
            ]
        );
        let mut colon = spec();
        colon.remote_target = "/b/r:x/wire/main-cd34.sock".into();
        let err = link_argv(&colon, &[], 103).unwrap_err().to_string();
        assert!(err.contains("`:`"), "{err}");
        let mut long = spec();
        long.remote_forward =
            PathBuf::from(format!("/{}/wire/s-0123456789abcdef.sock", "d".repeat(90)));
        let err = link_argv(&long, &[], 103).unwrap_err().to_string();
        assert!(err.contains("more than the 103"), "{err}");
        let mut relative = spec();
        relative.local_target = "r/wire/main-ab12.sock".into();
        assert!(link_argv(&relative, &[], 103).is_err());
    }

    #[test]
    fn a_forward_path_is_known_by_its_name_and_directory() {
        let name = forward_name("box/ws/main-cd34");
        assert!(
            name.starts_with("s-") && name.ends_with(".sock") && name.len() == 23,
            "{name}"
        );
        assert_eq!(name, forward_name("box/ws/main-cd34"));
        assert_ne!(name, forward_name("box/ws/main-cd35"));
        assert!(is_forward_path(&Path::new("/r/wire").join(&name)));
        assert!(!is_forward_path(&Path::new("/r/other").join(&name)));
        assert!(!is_forward_path(Path::new("/r/wire/main-cd34.sock")));
        assert!(!is_forward_path(&Path::new("wire").join(&name)));
    }

    #[test]
    fn the_protocol_round_trips_and_checks_its_version() {
        let request = Request::new(Op::Card {
            target: "api/main-cd34".into(),
        });
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(
            line,
            r#"{"rkat_wire":1,"op":"card","target":"api/main-cd34"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        let hold: Request =
            serde_json::from_str(r#"{"rkat_wire":1,"op":"hold","forward":"/r/wire/x"}"#).unwrap();
        assert!(matches!(hold.op, Op::Hold { .. }));
        let box_host = host("box");
        let ok = Reply::ok(serde_json::json!({"label": "a/main-1", "already": false}));
        let wired: crate::daemon::protocol::Wired = ok.into_result(&box_host).unwrap();
        assert_eq!(wired.label, "a/main-1");
        let refused =
            Reply::refused("no daemon runs there").into_result::<serde_json::Value>(&box_host);
        assert_eq!(
            refused.unwrap_err().to_string(),
            "box: no daemon runs there"
        );
        let mut other = Reply::ok(serde_json::json!({}));
        other.rkat_wire = 2;
        let err = other
            .into_result::<serde_json::Value>(&box_host)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("speaks wire protocol 2") && err.contains("install the same rkat-tui"),
            "{err}"
        );
    }

    #[test]
    fn a_failed_launch_says_what_to_do() {
        let h = host("box");
        let auth =
            launch_failure(&h, Some(255), "me@box: Permission denied (publickey).\n").to_string();
        assert!(
            auth.contains("make `ssh box true` work first") && auth.contains("Permission denied"),
            "{auth}"
        );
        let missing = launch_failure(&h, Some(127), "bash: line 1: rkat-tui: command not found\n")
            .to_string();
        assert!(
            missing.contains("no rkat-tui on box") && missing.contains("--remote-rkat-tui <path>"),
            "{missing}"
        );
        let old = launch_failure(
            &h,
            Some(2),
            "error: unexpected argument '--wire-remote' found\n",
        )
        .to_string();
        assert!(
            old.contains("has no --wire-remote") && old.contains("install the same rkat-tui"),
            "{old}"
        );
    }

    #[test]
    fn the_backoff_doubles_to_a_minute() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(5), Duration::from_secs(32));
        assert_eq!(backoff(6), Duration::from_mins(1));
        assert_eq!(backoff(u32::MAX), Duration::from_mins(1));
    }

    #[test]
    fn the_down_words_say_what_the_agent_can_do() {
        let words = down_words("ws/main-cd34", "box");
        assert!(
            words.contains("the link to box is down")
                && words.contains("try again later, or tell the user"),
            "{words}"
        );
        let there = down_from_words("ws/main-ab12", "laptop");
        assert!(
            there.contains("it works on laptop")
                && there.contains("the ssh link from that machine to this one is down")
                && there.contains("not sent and is not kept")
                && there.contains("try again later, or tell the user")
                && !there.contains("not running"),
            "{there}"
        );
        assert!(
            down_from_words("ws/main-ab12", "").contains("it works on another machine"),
            "unknown host"
        );
    }
}
