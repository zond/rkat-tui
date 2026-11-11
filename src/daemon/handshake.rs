//! The attach handshake: the one thing on the daemon's wire that is not
//! Meerkat's own JSON-RPC.
//!
//! Loopback TCP is reachable by every local process, and the engine's TCP
//! path has no authentication of its own (`rkat-rpc --allow-remote` is "not
//! an auth mechanism"), so each accepted connection must first prove it
//! holds the daemon's token (from the endpoint file, mode 0600) before the
//! stream is handed to the engine's `RpcServer`.
//!
//! The token itself never travels: each side proves it holds it with an
//! HMAC over both sides' nonces, the daemon first, so a process that took
//! the daemon's port (after a crash, in the shutdown window) learns
//! nothing a client could be talked into sending. The client's first line
//! is
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"rkat/attach","params":{"nonce":"<client nonce>"}}
//! ```
//!
//! (with its start token, probe flag and agent choice beside the nonce),
//! answered with
//! `{"jsonrpc":"2.0","id":1,"result":{"challenge":{"nonce":"<daemon nonce>","proof":"<daemon proof>"}}}`.
//! The client checks the daemon's proof ([`daemon_proof`]) and only then
//! sends its own ([`client_proof`]) in a second attach request,
//! `{"jsonrpc":"2.0","id":2,"method":"rkat/attach","params":{"proof":"…"}}`,
//! answered with `{"jsonrpc":"2.0","id":2,"result":{"attached":true}}`;
//! every later line is the engine's. The two proofs are keyed by the token
//! over distinct labels, so neither can be replayed as the other.
//!
//! An attach without a nonce (a bare token, say) is refused. Any other
//! first line, a wrong proof, a
//! malformed nonce, a line longer than [`MAX_LINE`] or no line within
//! [`TIMEOUT`] is answered with a JSON-RPC error ([`INVALID_REQUEST`] or
//! [`UNAUTHORIZED`]) and the connection is closed.

use std::time::Duration;

use hmac::{Hmac, Mac as _};
use meerkat_rpc::protocol::RpcId;
use serde_json::Value;
use sha2::Sha256;

use super::protocol::{self, AttachChallenge, AttachParams, Attached, Challenged};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};

/// The attach method.
pub const ATTACH_METHOD: &str = protocol::ATTACH;

/// How long a client may take to send its attach line.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The longest attach line accepted, newline included.
pub const MAX_LINE: u64 = 4096;

/// What the daemon's proof is computed over, before the nonces.
const DAEMON_LABEL: &[u8] = b"rkat-tui attach: daemon proof\0";

/// What the client's proof is computed over, before the nonces.
const CLIENT_LABEL: &[u8] = b"rkat-tui attach: client proof\0";

/// Random bytes in a nonce.
const NONCE_BYTES: usize = 32;

/// The attach token could not key a proof. Unreachable with HMAC (it
/// takes a key of any length), and still a failure: a proof that could
/// not be computed never matches anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the attach token cannot key a proof")]
pub struct ProofKeyError;

/// `HMAC-SHA256(token, label ‖ client_nonce ‖ "\0" ‖ daemon_nonce)`, hex.
fn proof(
    token: &str,
    label: &[u8],
    client_nonce: &str,
    daemon_nonce: &str,
) -> Result<String, ProofKeyError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).map_err(|_| ProofKeyError)?;
    mac.update(label);
    mac.update(client_nonce.as_bytes());
    mac.update(b"\0");
    mac.update(daemon_nonce.as_bytes());
    Ok(hex(&mac.finalize().into_bytes()))
}

/// Whether `given` is the `expected` proof: never when the proof could not
/// be computed, never for an empty one.
fn proves(expected: Result<String, ProofKeyError>, given: &str) -> bool {
    expected
        .is_ok_and(|expected| !expected.is_empty() && same(given.as_bytes(), expected.as_bytes()))
}

/// The daemon's proof that it holds `token`, for the two nonces.
///
/// # Errors
///
/// [`ProofKeyError`] when the token cannot key one.
pub fn daemon_proof(
    token: &str,
    client_nonce: &str,
    daemon_nonce: &str,
) -> Result<String, ProofKeyError> {
    proof(token, DAEMON_LABEL, client_nonce, daemon_nonce)
}

/// The client's proof that it holds `token`, for the two nonces.
///
/// # Errors
///
/// [`ProofKeyError`] when the token cannot key one.
pub fn client_proof(
    token: &str,
    client_nonce: &str,
    daemon_nonce: &str,
) -> Result<String, ProofKeyError> {
    proof(token, CLIENT_LABEL, client_nonce, daemon_nonce)
}

/// Whether `given` is the daemon's proof for `token` and the two nonces.
#[must_use]
pub fn is_daemon_proof(token: &str, client_nonce: &str, daemon_nonce: &str, given: &str) -> bool {
    proves(daemon_proof(token, client_nonce, daemon_nonce), given)
}

/// Whether `given` is the client's proof for `token` and the two nonces.
#[must_use]
pub fn is_client_proof(token: &str, client_nonce: &str, daemon_nonce: &str, given: &str) -> bool {
    proves(client_proof(token, client_nonce, daemon_nonce), given)
}

/// A fresh nonce: [`NONCE_BYTES`] from the OS, hex-encoded.
///
/// # Errors
///
/// When the OS randomness cannot be read.
pub fn new_nonce() -> std::io::Result<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; NONCE_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

/// Whether `nonce` is one a client may send: 32 to 128 hex digits.
fn valid_nonce(nonce: &str) -> bool {
    (32..=128).contains(&nonce.len()) && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

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
    /// Missing or wrong token, or a wrong proof.
    #[error("wrong or missing token")]
    Unauthorized,
    /// The connection could not be bound to the agent it asked for.
    #[error("no agent: {0}")]
    NoAgent(String),
    /// The daemon could not compute its own proof: nobody is let in.
    #[error("{0}")]
    Proof(ProofKeyError),
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

/// Read and check the attach request(s) from `reader` against `token`:
/// answer the challenge to the client's nonce and read the client's
/// proof; an attach without a nonce is refused. A refusal is answered on `writer`, a valid attach is not answered yet
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
    let checked = match read_request(reader, timeout).await {
        Ok(line) => match check(&line) {
            Ok((id, params)) => match params.nonce.clone() {
                Some(client_nonce) => {
                    challenge(reader, writer, token, timeout, id, params, &client_nonce).await
                }
                None => Err((Some(id), HandshakeError::Unauthorized)),
            },
            Err(refused) => Err(refused),
        },
        Err(err) => Err((None, err)),
    };
    match checked {
        Ok((id, params)) => Ok(Pending { id, params }),
        Err((id, err)) => {
            refuse(writer, id, &err, timeout).await;
            Err(err)
        }
    }
}

/// The mutual part: answer `id` with the daemon's nonce and proof, then
/// read the client's proof (a second attach request) and check it.
async fn challenge<R, W>(
    reader: &mut R,
    writer: &mut W,
    token: &str,
    timeout: Duration,
    id: RpcId,
    mut params: AttachParams,
    client_nonce: &str,
) -> Result<(RpcId, AttachParams), (Option<RpcId>, HandshakeError)>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !valid_nonce(client_nonce) {
        return Err((
            Some(id),
            HandshakeError::NotAttach("a nonce of 32 to 128 hex digits".into()),
        ));
    }
    let daemon_nonce = new_nonce().map_err(|err| (Some(id.clone()), HandshakeError::Io(err)))?;
    let proof = daemon_proof(token, client_nonce, &daemon_nonce)
        .map_err(|err| (Some(id.clone()), HandshakeError::Proof(err)))?;
    let reply = protocol::result_line(
        Some(id),
        Challenged {
            challenge: AttachChallenge {
                proof,
                nonce: daemon_nonce.clone(),
            },
        },
    );
    tokio::time::timeout(timeout, write_line(writer, &reply))
        .await
        .map_err(|_| (None, HandshakeError::Timeout(timeout)))?
        .map_err(|err| (None, HandshakeError::Io(err)))?;
    let line = read_request(reader, timeout)
        .await
        .map_err(|err| (None, err))?;
    let (id, answer) = check(&line)?;
    match answer.proof {
        Some(given) if is_client_proof(token, client_nonce, &daemon_nonce, &given) => {
            params.nonce = None;
            Ok((id, params))
        }
        _ => Err((Some(id), HandshakeError::Unauthorized)),
    }
}

/// One request line within `timeout` and [`MAX_LINE`].
async fn read_request<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
) -> Result<String, HandshakeError> {
    let mut line = String::new();
    let read =
        tokio::time::timeout(timeout, (&mut *reader).take(MAX_LINE).read_line(&mut line)).await;
    match read {
        Err(_) => Err(HandshakeError::Timeout(timeout)),
        Ok(Err(err)) => Err(HandshakeError::Io(err)),
        Ok(Ok(0)) => Err(HandshakeError::Closed),
        Ok(Ok(_)) if !line.ends_with('\n') => Err(HandshakeError::NotAttach(format!(
            "no complete line within {MAX_LINE} bytes"
        ))),
        Ok(Ok(_)) => Ok(line),
    }
}

/// Answer a refusal (best effort, bounded) and close the writer; nothing
/// for a connection that is closed or broken.
async fn refuse<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Option<RpcId>,
    err: &HandshakeError,
    timeout: Duration,
) {
    if matches!(err, HandshakeError::Closed | HandshakeError::Io(_)) {
        return;
    }
    let code = match err {
        HandshakeError::NotAttach(_) => INVALID_REQUEST,
        _ => UNAUTHORIZED,
    };
    let reply = protocol::error_line(id, code, &err.to_string());
    // Bounded: a client that never reads must not hold the connection
    // task.
    let _ = tokio::time::timeout(timeout, async {
        let _ = write_line(writer, &reply).await;
        let _ = writer.shutdown().await;
    })
    .await;
}

/// The request id and parameters of an attach `line`, or the id to answer
/// with and the refusal.
fn check(line: &str) -> Result<(RpcId, AttachParams), (Option<RpcId>, HandshakeError)> {
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
    request
        .get("params")
        .cloned()
        .and_then(|params| serde_json::from_value::<AttachParams>(params).ok())
        .map(|params| (id.clone(), params))
        .ok_or((Some(id), HandshakeError::Unauthorized))
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

    /// A client nonce (32 hex digits).
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// The mutual handshake as a client holding `token` plays it, with
    /// `extra` beside the nonce in the first request and `then` sent after
    /// the proof: the daemon's result, its last answer and what it left
    /// unread.
    async fn mutual(
        token: &str,
        extra: Value,
        then: &str,
    ) -> (Result<AttachParams, HandshakeError>, Value, String) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, mut sw) = tokio::io::split(server);
        let daemon = tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let result = accept(&mut reader, &mut sw, "secret", TIMEOUT).await;
            let mut rest = String::new();
            let _ = reader.read_to_string(&mut rest).await;
            (result, rest)
        });
        let (cr, mut cw) = tokio::io::split(client);
        let mut cr = BufReader::new(cr);
        let mut params = extra;
        params["nonce"] = json!(NONCE);
        let hello = json!({"jsonrpc": "2.0", "id": 7, "method": "rkat/attach", "params": params});
        write_line(&mut cw, &format!("{hello}\n")).await.unwrap();
        let mut line = String::new();
        cr.read_line(&mut line).await.unwrap();
        let challenge: Value = serde_json::from_str(&line).unwrap();
        let daemon_nonce = challenge["result"]["challenge"]["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            challenge["result"]["challenge"]["proof"],
            daemon_proof("secret", NONCE, &daemon_nonce).unwrap()
        );
        let proof = json!({"jsonrpc": "2.0", "id": 8, "method": "rkat/attach", "params": {"proof": client_proof(token, NONCE, &daemon_nonce).unwrap()}});
        write_line(&mut cw, &format!("{proof}\n{then}"))
            .await
            .unwrap();
        cw.shutdown().await.unwrap();
        let mut last = String::new();
        cr.read_line(&mut last).await.unwrap();
        let (result, rest) = daemon.await.unwrap();
        let last = serde_json::from_str(&last).unwrap_or(Value::Null);
        (result, last, rest)
    }

    #[tokio::test]
    async fn the_right_proof_attaches_and_keeps_the_next_requests() {
        let (result, reply, rest) = mutual("secret", json!({}), "{\"next\":1}\n").await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            reply,
            json!({"jsonrpc":"2.0","id":8,"result":{"attached":true}})
        );
        assert_eq!(rest, "{\"next\":1}\n", "pipelined requests stay buffered");
    }

    #[tokio::test]
    async fn the_attach_carries_the_start_token_and_the_probe_flag() {
        let (result, _, _) = mutual("secret", json!({"start_token": "s", "probe": true}), "").await;
        let params = result.unwrap();
        assert_eq!(params.start_token.as_deref(), Some("s"));
        assert!(params.probe);
        // Debug never shows a token.
        let shown = format!("{params:?}");
        assert!(!shown.contains("\"s\""), "{shown}");
    }

    /// A proof keyed by another token is refused; so is an attach that
    /// sends the token itself instead of a nonce, or nothing at all.
    #[tokio::test]
    async fn a_wrong_proof_or_a_bare_token_is_unauthorized() {
        let (result, reply, _) = mutual("nope", json!({}), "").await;
        assert!(
            matches!(result, Err(HandshakeError::Unauthorized)),
            "{result:?}"
        );
        assert_eq!(reply["error"]["code"], UNAUTHORIZED);
        for params in ["{\"token\":\"secret\"}", "{}"] {
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
            "{\"jsonrpc\":\"2.0\",\"method\":\"rkat/attach\",\"params\":{\"nonce\":\"0123456789abcdef0123456789abcdef\"}}\n",
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

    /// A nonce that is not 32 to 128 hex digits gets no challenge.
    #[tokio::test]
    async fn a_malformed_nonce_gets_no_challenge() {
        for nonce in ["", "abc", "zz", &"a".repeat(130)] {
            let input = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"rkat/attach\",\"params\":{{\"nonce\":\"{nonce}\"}}}}\n"
            );
            let (result, reply, _) = run(input.as_bytes(), TIMEOUT).await;
            assert!(
                matches!(result, Err(HandshakeError::NotAttach(_))),
                "{nonce}"
            );
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{nonce}");
        }
    }

    /// The two proofs are keyed by the token and labelled apart.
    #[test]
    fn the_proofs_depend_on_the_token_both_nonces_and_the_side() {
        let p = daemon_proof("t", "a", "b").unwrap();
        assert_eq!(p.len(), 64);
        assert_ne!(p, client_proof("t", "a", "b").unwrap());
        assert_ne!(p, daemon_proof("u", "a", "b").unwrap());
        assert_ne!(p, daemon_proof("t", "b", "a").unwrap());
        assert_ne!(
            daemon_proof("t", "ab", "c").unwrap(),
            daemon_proof("t", "a", "bc").unwrap()
        );
        assert!(is_daemon_proof("t", "a", "b", &p));
        assert!(!is_client_proof("t", "a", "b", &p));
    }

    /// Fail closed: a proof that could not be computed matches nothing,
    /// not even an empty proof (two empty strings compare equal), and an
    /// empty proof never proves anything.
    #[test]
    fn a_proof_that_could_not_be_computed_proves_nothing() {
        assert!(!proves(Err(ProofKeyError), ""));
        assert!(!proves(Err(ProofKeyError), "00"));
        assert!(!proves(Ok(String::new()), ""));
        assert!(!is_client_proof("t", "a", "b", ""));
        assert!(!is_daemon_proof("t", "a", "b", ""));
        let p = client_proof("t", "a", "b").unwrap();
        assert!(proves(Ok(p.clone()), &p));
    }

    #[tokio::test]
    async fn eof_is_closed() {
        let (result, reply, _) = run(b"", TIMEOUT).await;
        assert!(matches!(result, Err(HandshakeError::Closed)));
        assert_eq!(reply, Value::Null);
    }
}
