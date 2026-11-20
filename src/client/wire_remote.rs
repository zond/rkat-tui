//! `rkat-tui --wire-remote`: the remote side of a wire over ssh
//! (`crate::daemon::remote_wire`). The initiating machine's daemon runs it
//! through `ssh <host>`; it reads one request line on standard input and
//! answers one line on standard output, and does only what `/wire` on this
//! machine lets the same user do: it finds this user's running daemons by
//! their endpoint files (as `--list` does) and calls them as a probe
//! (`rkat/wire_card`, `rkat/wire`, `rkat/unwire`). A link's own command
//! ([`Op::Hold`]) answers once the forwarded socket is there and holds
//! until the connection ends.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

use super::wires::{self as cross, Other};
use crate::daemon::protocol::{UnwireParams, WireAgentParams, WireParams, method};
use crate::daemon::remote_wire::{CardReply, Op, PROTOCOL, RemoteAgent, Reply, Request};

/// How long [`Op::Hold`] waits for the forwarded socket to appear.
const FORWARD_WAIT: Duration = Duration::from_secs(10);

/// One pause of that wait.
const FORWARD_POLL: Duration = Duration::from_millis(100);

/// Answer one request from standard input on standard output.
///
/// # Errors
///
/// Reading standard input or writing standard output failed.
pub async fn serve_stdio() -> Result<(), crate::TuiError> {
    let io = |context: &str| {
        let context = context.to_string();
        move |source| crate::TuiError::Io { context, source }
    };
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .await
        .map_err(io("reading the wire request"))?;
    let reply = match read_request(&line) {
        Err(reply) => reply,
        Ok(Request {
            op: Op::Hold { forward },
            ..
        }) => {
            let (reply, held) = wait_for(&forward).await;
            write(&reply).await.map_err(io("writing the wire answer"))?;
            if let Some(held) = held {
                // The connection's end closes standard input.
                hold_until_closed(&mut stdin).await;
                held.remove();
            }
            return Ok(());
        }
        Ok(request) => answer(request, &super::list::files(None)).await,
    };
    write(&reply).await.map_err(io("writing the wire answer"))
}

/// Read `stdin` until it ends; a "try again" (a non-blocking descriptor)
/// is no end and is waited out.
async fn hold_until_closed(stdin: &mut (impl tokio::io::AsyncRead + Unpin)) {
    let mut buffer = [0u8; 1024];
    loop {
        match stdin.read(&mut buffer).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(FORWARD_POLL).await;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => {
                tracing::debug!(error = %err, "standard input ended");
                return;
            }
        }
    }
}

/// `line` as a request of this protocol, or the refusal to send back.
fn read_request(line: &str) -> Result<Request, Reply> {
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|err| Reply::refused(format!("the request is no JSON line: {err}")))?;
    let version = value.get("rkat_wire").and_then(serde_json::Value::as_u64);
    if version != Some(u64::from(PROTOCOL)) {
        let mut reply = Reply::refused(format!(
            "this rkat-tui ({}) speaks wire protocol {PROTOCOL}, the request {}: install the same rkat-tui on both machines",
            crate::BUILD,
            version.map_or_else(|| "none".to_string(), |v| v.to_string())
        ));
        // The asker reads the version first and says so.
        reply.rkat_wire = PROTOCOL;
        return Err(reply);
    }
    serde_json::from_value(value).map_err(|err| {
        Reply::refused(format!(
            "this rkat-tui ({}) does not know the request: {err}",
            crate::BUILD
        ))
    })
}

async fn write(reply: &Reply) -> std::io::Result<()> {
    let mut line = serde_json::to_string(reply).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut stdout = tokio::io::stdout();
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await
}

/// Ready once `forward` exists, within [`FORWARD_WAIT`]: the reply, and
/// the socket held once ready.
async fn wait_for(forward: &Path) -> (Reply, Option<HeldForward>) {
    if !crate::daemon::remote_wire::is_forward_path(forward) {
        return (
            Reply::refused(format!("{} is no forwarded wire socket", forward.display())),
            None,
        );
    }
    let deadline = tokio::time::Instant::now() + FORWARD_WAIT;
    loop {
        if let Some(held) = HeldForward::seen(forward) {
            return (Reply::ok(serde_json::json!({})), Some(held));
        }
        if tokio::time::Instant::now() >= deadline {
            return (
                Reply::refused(format!(
                    "the forwarded socket {} did not appear: the ssh server may not allow Unix-socket forwarding (AllowStreamLocalForwarding in its sshd_config)",
                    forward.display()
                )),
                None,
            );
        }
        tokio::time::sleep(FORWARD_POLL).await;
    }
}

/// The forwarded socket one link's hold answered for, known by its file
/// (device, inode, change time), not only its path: a link's connection can end on
/// this machine long after the other machine made the next link, whose ssh
/// server bound a new socket at the same path (a dropped network is
/// noticed only at a keepalive's timeout). The hold's end removes the
/// socket only while it is still the one it held ([`file_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldForward {
    path: PathBuf,
    file: FileId,
}

impl HeldForward {
    /// The socket at `path` now, when there is one and it is a forwarded
    /// socket's path ([`crate::daemon::remote_wire::is_forward_path`]).
    #[must_use]
    pub fn seen(path: &Path) -> Option<Self> {
        if !crate::daemon::remote_wire::is_forward_path(path) {
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
            file: file_of(path)?,
        })
    }

    /// Remove the socket, when the file at its path is still the one held
    /// (a newer link's socket stays).
    pub fn remove(&self) {
        if file_of(&self.path) == Some(self.file) {
            let _ = std::fs::remove_file(&self.path);
        } else {
            tracing::debug!(path = %self.path.display(), "a newer link's socket is there: it stays");
        }
    }
}

/// The file at `path` (not followed) as its device, inode and change time:
/// an inode number freed by the old socket's unlink can be the new
/// socket's, its change time (to the nanosecond) is not.
fn file_of(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

/// [`file_of`]'s identity of a file.
type FileId = (u64, u64, i64, i64);

/// The answer to `request` (anything but [`Op::Hold`]) on the machine
/// whose daemons' endpoint files are `files`.
pub async fn answer(request: Request, files: &[PathBuf]) -> Reply {
    match answered(request.op, files).await {
        Ok(value) => Reply::ok(value),
        Err(why) => Reply::refused(why),
    }
}

async fn answered(op: Op, files: &[PathBuf]) -> Result<serde_json::Value, String> {
    let json =
        |value: Result<serde_json::Value, serde_json::Error>| value.map_err(|err| err.to_string());
    match op {
        Op::List => {
            let others = cross::others(files, Path::new("")).await;
            let agents: Vec<RemoteAgent> = cross::candidates(&others)
                .into_iter()
                .map(|c| RemoteAgent {
                    workspace: c.workspace,
                    agent: c.agent,
                    run: c.run.as_str().to_string(),
                    clients: c.clients,
                })
                .collect();
            json(serde_json::to_value(agents))
        }
        Op::Card { target } => {
            let (other, agent) = find(&target, files).await?;
            let card = cross::call_other::<method::Card>(
                &other.endpoint,
                &other.status.workspace,
                &WireAgentParams { agent: Some(agent) },
            )
            .await?;
            json(serde_json::to_value(CardReply {
                card,
                max_socket_path: crate::daemon::wires::MAX_SOCKET_PATH,
            }))
        }
        Op::Wire { target, card } => {
            let (other, agent) = find(&target, files).await?;
            let wired = cross::call_other::<method::Wire>(
                &other.endpoint,
                &other.status.workspace,
                &WireParams {
                    agent: Some(agent),
                    card,
                },
            )
            .await?;
            json(serde_json::to_value(wired))
        }
        Op::Unwire {
            target,
            workspace,
            peer,
        } => {
            let (other, agent) = find(&target, files).await?;
            let unwired = cross::call_other::<method::Unwire>(
                &other.endpoint,
                &other.status.workspace,
                &UnwireParams {
                    agent: Some(agent),
                    workspace,
                    peer,
                },
            )
            .await?;
            json(serde_json::to_value(unwired))
        }
        Op::Clear { path } => clear(&path).map(|()| serde_json::json!({})),
        Op::Hold { .. } => Err("a hold is a link's own command".to_string()),
    }
}

/// The running daemon and agent `target` names on this machine.
async fn find(target: &str, files: &[PathBuf]) -> Result<(Other, String), String> {
    let others = cross::others(files, Path::new("")).await;
    let all = cross::candidates(&others);
    let found = cross::resolve(target, &all).map_err(|why| {
        if all.is_empty() {
            format!("no rkat-tui daemon runs on this machine for `{target}`: start rkat-tui in that workspace there first")
        } else {
            why
        }
    })?;
    let other = others
        .into_iter()
        .find(|o| o.status.workspace == found.workspace)
        .ok_or_else(|| {
            format!(
                "the daemon of {} stopped meanwhile",
                found.workspace.display()
            )
        })?;
    Ok((other, found.agent.clone()))
}

/// Remove the stale forwarded socket `path` (only a forward's, and only a
/// socket).
fn clear(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt as _;
    if !crate::daemon::remote_wire::is_forward_path(path) {
        return Err(format!("{} is no forwarded wire socket", path.display()));
    }
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("cannot look at {}: {err}", path.display())),
        Ok(meta) if !meta.file_type().is_socket() => {
            Err(format!("{} is no socket; it stays", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).map_err(|err| {
            format!(
                "cannot remove the stale socket {} ({err}): remove it there by hand, or let that ssh server replace it (StreamLocalBindUnlink yes in its sshd_config)",
                path.display()
            )
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_request_of_another_version_is_refused_with_what_to_do() {
        let ok = read_request(r#"{"rkat_wire":1,"op":"list"}"#).unwrap();
        assert_eq!(ok.op, Op::List);
        let other = read_request(r#"{"rkat_wire":7,"op":"list"}"#).unwrap_err();
        let why = other.error.unwrap();
        assert!(
            why.contains("the request 7") && why.contains("install the same rkat-tui"),
            "{why}"
        );
        let none = read_request(r#"{"op":"list"}"#).unwrap_err();
        assert!(none.error.unwrap().contains("the request none"));
        let unknown = read_request(r#"{"rkat_wire":1,"op":"fly"}"#).unwrap_err();
        assert!(unknown.error.unwrap().contains("does not know the request"));
        assert!(read_request("not json").is_err());
    }

    #[tokio::test]
    async fn no_daemon_there_says_to_start_rkat_tui() {
        let reply = answer(
            Request::new(Op::Card {
                target: "api/main-ab12".into(),
            }),
            &[],
        )
        .await;
        let why = reply.error.unwrap();
        assert!(
            why.contains("start rkat-tui in that workspace there first"),
            "{why}"
        );
        let list = answer(Request::new(Op::List), &[]).await;
        assert_eq!(list.ok, Some(serde_json::json!([])));
    }

    /// A hold that ends after the next link bound its socket at the same
    /// path leaves that socket; its own it removes.
    #[test]
    fn a_hold_removes_only_the_socket_it_held() {
        let dir = tempfile::tempdir().unwrap();
        let wire = dir.path().join("wire");
        std::fs::create_dir(&wire).unwrap();
        let socket = wire.join(crate::daemon::remote_wire::forward_name("k"));
        let old = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let held_old = HeldForward::seen(&socket).unwrap();
        // The next link's ssh server unlinks the old file and binds anew.
        std::fs::remove_file(&socket).unwrap();
        let new = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(old);
        held_old.remove();
        assert!(socket.exists(), "the newer link's socket was removed");
        let held_new = HeldForward::seen(&socket).unwrap();
        held_new.remove();
        assert!(!socket.exists());
        drop(new);
        assert_eq!(HeldForward::seen(&wire.join("main-ab12.sock")), None);
    }

    #[test]
    fn only_a_stale_forward_socket_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let wire = dir.path().join("wire");
        std::fs::create_dir(&wire).unwrap();
        let name = crate::daemon::remote_wire::forward_name("k");
        let socket = wire.join(&name);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(listener);
        assert!(socket.exists());
        clear(&socket).unwrap();
        assert!(!socket.exists());
        clear(&socket).unwrap();
        std::fs::write(&socket, b"x").unwrap();
        assert!(clear(&socket).unwrap_err().contains("is no socket"));
        assert!(
            clear(&wire.join("main-ab12.sock"))
                .unwrap_err()
                .contains("no forwarded wire socket")
        );
    }
}
