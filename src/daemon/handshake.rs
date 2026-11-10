//! The attach handshake: the one thing on the daemon's wire that is not
//! Meerkat's own JSON-RPC.
//!
//! Loopback TCP is reachable by every local process, and the engine's TCP
//! path has no authentication of its own (`rkat-rpc --allow-remote` is "not
//! an auth mechanism"), so each accepted connection must first prove it
//! holds the daemon's token (from the endpoint file, mode 0600) before the
//! stream is handed to the engine's `RpcServer`.
//!
//! The first line the client sends must be the JSON-RPC request
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"rkat/attach","params":{"token":"<token>"}}
//! ```
//!
//! answered with `{"jsonrpc":"2.0","id":1,"result":{"attached":true}}`;
//! every later line is the engine's. Any other first line, a wrong token,
//! a line longer than [`MAX_LINE`] or no line within [`TIMEOUT`] is
//! answered with a JSON-RPC error ([`INVALID_REQUEST`] or
//! [`UNAUTHORIZED`]) and the connection is closed.

use std::time::Duration;

use meerkat_rpc::protocol::RpcId;
use serde_json::Value;

use super::protocol::{self, AttachParams, Attached};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};

/// The attach method.
pub const ATTACH_METHOD: &str = protocol::ATTACH;

/// How long a client may take to send its attach line.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The longest attach line accepted, newline included.
pub const MAX_LINE: u64 = 4096;

pub use super::protocol::{INVALID_REQUEST, UNAUTHORIZED};

/// Why a connection was refused.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// No attach line within [`TIMEOUT`].
    #[error("no attach request within {0:?}")]
    Timeout(Duration),
    /// The client closed before a full line.
    #[error("the client closed before attaching")]
    Closed,
    /// The first line is not an attach request.
    #[error("the first request is not `{ATTACH_METHOD}`: {0}")]
    NotAttach(String),
    /// Missing or wrong token.
    #[error("wrong or missing token")]
    Unauthorized,
    /// The connection could not be bound to the agent it asked for.
    #[error("no agent: {0}")]
    NoAgent(String),
    /// Reading or writing the connection failed.
    #[error("connection error: {0}")]
    Io(#[from] std::io::Error),
}

/// A valid attach line, not acknowledged yet ([`acknowledge`]).
#[derive(Debug)]
pub struct Pending {
    /// The request's id.
    pub id: RpcId,
    /// What the client presented besides the token (its start token,
    /// whether it is a probe).
    pub params: AttachParams,
}

/// [`read_attach`], then [`acknowledge`] at once.
///
/// # Errors
///
/// As [`read_attach`] and [`acknowledge`].
#[cfg(test)]
pub async fn accept<R, W>(
    reader: &mut R,
    writer: &mut W,
    token: &str,
    timeout: Duration,
) -> Result<AttachParams, HandshakeError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let pending = read_attach(reader, writer, token, timeout).await?;
    acknowledge(writer, &pending, None, None, timeout).await?;
    Ok(pending.params)
}

/// Answer `pending` with `{attached: true}` on `writer`, naming the agent
/// the connection was bound to and the daemon's `build`. The daemon registers the connection first
/// ([`super::serve`]): a client that read this answer is counted for every
/// other connection.
///
/// # Errors
///
/// [`HandshakeError::Io`] or [`HandshakeError::Timeout`] when the answer
/// cannot be written.
pub async fn acknowledge<W: AsyncWrite + Unpin>(
    writer: &mut W,
    pending: &Pending,
    agent: Option<&super::agents::Bound>,
    build: Option<&str>,
    timeout: Duration,
) -> Result<(), HandshakeError> {
    let reply = protocol::result_line(
        Some(pending.id.clone()),
        Attached {
            attached: true,
            agent: agent.map(|bound| bound.name.clone()),
            seated: agent.is_some_and(|bound| bound.seated),
            build: build.map(str::to_string),
        },
    );
    tokio::time::timeout(timeout, write_line(writer, &reply))
        .await
        .map_err(|_| HandshakeError::Timeout(timeout))??;
    Ok(())
}

/// Read and check the attach line from `reader` against `token`; a
/// refusal is answered on `writer`, a valid line is not answered yet
/// ([`acknowledge`]). On success the caller hands `reader` (which may
/// already buffer the client's next requests) and `writer` to the engine.
///
/// # Errors
///
/// [`HandshakeError`] after the refusal was written (best effort).
pub async fn read_attach<R, W>(
    reader: &mut R,
    writer: &mut W,
    token: &str,
    timeout: Duration,
) -> Result<Pending, HandshakeError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = String::new();
    let read =
        tokio::time::timeout(timeout, (&mut *reader).take(MAX_LINE).read_line(&mut line)).await;
    let checked = match read {
        Err(_) => Err((None, HandshakeError::Timeout(timeout))),
        Ok(Err(err)) => Err((None, HandshakeError::Io(err))),
        Ok(Ok(0)) => Err((None, HandshakeError::Closed)),
        Ok(Ok(_)) if !line.ends_with('\n') => Err((
            None,
            HandshakeError::NotAttach(format!("no complete line within {MAX_LINE} bytes")),
        )),
        Ok(Ok(_)) => check(&line, token),
    };
    match checked {
        Ok((id, params)) => Ok(Pending { id, params }),
        Err((id, err)) => {
            if !matches!(err, HandshakeError::Closed | HandshakeError::Io(_)) {
                let code = match err {
                    HandshakeError::NotAttach(_) => INVALID_REQUEST,
                    _ => UNAUTHORIZED,
                };
                let reply = protocol::error_line(id, code, &err.to_string());
                // Bounded: a client that never reads must not hold the
                // connection task.
                let _ = tokio::time::timeout(timeout, async {
                    let _ = write_line(writer, &reply).await;
                    let _ = writer.shutdown().await;
                })
                .await;
            }
            Err(err)
        }
    }
}

/// The request id and parameters of a valid attach `line` for `token`, or
/// the id to answer with and the refusal.
fn check(
    line: &str,
    token: &str,
) -> Result<(RpcId, AttachParams), (Option<RpcId>, HandshakeError)> {
    let request: Value = serde_json::from_str(line.trim_end())
        .map_err(|err| (None, HandshakeError::NotAttach(err.to_string())))?;
    let id = protocol::request_id(request.get("id").unwrap_or(&Value::Null));
    let method = request.get("method").and_then(Value::as_str);
    let attach = request.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && method == Some(ATTACH_METHOD);
    let Some(id) = id.clone().filter(|_| attach) else {
        let what = method.map_or_else(|| "no method".to_string(), |m| format!("`{m}`"));
        return Err((id, HandshakeError::NotAttach(what)));
    };
    let given = request
        .get("params")
        .cloned()
        .and_then(|params| serde_json::from_value::<AttachParams>(params).ok());
    match given {
        Some(given) if same(given.token.as_bytes(), token.as_bytes()) => Ok((id, given)),
        _ => Err((Some(id), HandshakeError::Unauthorized)),
    }
}

/// Compare without stopping at the first differing byte.
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    async fn run(
        input: &[u8],
        timeout: Duration,
    ) -> (Result<AttachParams, HandshakeError>, Value, String) {
        let mut reader = BufReader::new(input);
        let mut out = Vec::new();
        let result = accept(&mut reader, &mut out, "secret", timeout).await;
        let mut rest = String::new();
        reader.read_to_string(&mut rest).await.unwrap();
        let out = String::from_utf8(out).unwrap();
        let reply = out
            .lines()
            .next()
            .map_or(Value::Null, |l| serde_json::from_str(l).unwrap());
        (result, reply, rest)
    }

    #[tokio::test]
    async fn the_right_token_attaches_and_keeps_the_next_requests() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"rkat/attach\",\"params\":{\"token\":\"secret\"}}\n{\"next\":1}\n";
        let (result, reply, rest) = run(input, TIMEOUT).await;
        assert_eq!(
            result.unwrap(),
            AttachParams {
                token: "secret".into(),
                ..AttachParams::default()
            }
        );
        assert_eq!(
            reply,
            json!({"jsonrpc":"2.0","id":7,"result":{"attached":true}})
        );
        assert_eq!(rest, "{\"next\":1}\n", "pipelined requests stay buffered");
    }

    #[tokio::test]
    async fn the_attach_carries_the_start_token_and_the_probe_flag() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"rkat/attach\",\"params\":{\"token\":\"secret\",\"start_token\":\"s\",\"probe\":true}}\n";
        let (result, _, _) = run(input, TIMEOUT).await;
        let params = result.unwrap();
        assert_eq!(params.start_token.as_deref(), Some("s"));
        assert!(params.probe);
        // Debug never shows a token.
        let shown = format!("{params:?}");
        assert!(
            !shown.contains("secret") && !shown.contains("\"s\""),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_wrong_or_missing_token_is_unauthorized() {
        for params in ["{\"token\":\"nope\"}", "{}", "{\"token\":\"secre\"}"] {
            let input = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"rkat/attach\",\"params\":{params}}}\n"
            );
            let (result, reply, _) = run(input.as_bytes(), TIMEOUT).await;
            assert!(
                matches!(result, Err(HandshakeError::Unauthorized)),
                "{params}"
            );
            assert_eq!(reply["error"]["code"], UNAUTHORIZED);
            assert_eq!(reply["id"], 1);
        }
    }

    #[tokio::test]
    async fn any_other_first_request_is_refused() {
        for line in [
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"rkat/attach\",\"params\":{\"token\":\"secret\"}}\n",
            "not json\n",
        ] {
            let (result, reply, _) = run(line.as_bytes(), TIMEOUT).await;
            assert!(
                matches!(result, Err(HandshakeError::NotAttach(_))),
                "{line}"
            );
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{line}");
        }
        let long = vec![b'x'; MAX_LINE as usize + 10];
        let (result, reply, _) = run(&long, TIMEOUT).await;
        assert!(matches!(result, Err(HandshakeError::NotAttach(_))));
        assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_times_out() {
        let (client, server) = tokio::io::duplex(4096);
        let (read, mut write) = tokio::io::split(server);
        let mut reader = BufReader::new(read);
        let result = accept(&mut reader, &mut write, "secret", TIMEOUT).await;
        assert!(matches!(result, Err(HandshakeError::Timeout(_))));
        drop(client);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_to_a_client_that_never_reads_does_not_hang() {
        // A one-byte pipe the client never drains.
        let (client, server) = tokio::io::duplex(1);
        let (read, mut write) = tokio::io::split(server);
        let (_client_read, mut client_write) = tokio::io::split(client);
        tokio::spawn(async move {
            let _ = client_write.write_all(b"nope\n").await;
            std::future::pending::<()>().await;
        });
        let mut reader = BufReader::new(read);
        let result = accept(&mut reader, &mut write, "secret", TIMEOUT).await;
        assert!(matches!(result, Err(HandshakeError::NotAttach(_))));
    }

    #[tokio::test]
    async fn eof_is_closed() {
        let (result, reply, _) = run(b"", TIMEOUT).await;
        assert!(matches!(result, Err(HandshakeError::Closed)));
        assert_eq!(reply, Value::Null);
    }
}
