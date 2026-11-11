//! The client end of the daemon's wire: one loopback TCP connection,
//! attached with the endpoint file's token ([`crate::daemon::handshake`]),
//! then newline-delimited engine JSON-RPC multiplexed over it.
//!
//! [`Connection::call`] sends a request under a fresh id and resolves when
//! the line with that id arrives, so any number of calls can be in flight
//! (a `mob/turn_start` that answers only when its turn ends does not hold
//! up the others). Every other line with a `method` is a notification and
//! goes to the receiver [`Connection::attach`] returns; that receiver
//! ending is the one signal that the connection is gone (every call still
//! pending then fails with [`ClientError::Disconnected`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite,
    AsyncWriteExt as _, BufReader,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::ClientError;
use crate::daemon::endpoint::Endpoint;
use crate::daemon::handshake::{self, ATTACH_METHOD};
use crate::daemon::protocol::{AttachParams, Challenged};

/// How long connecting and the attach handshake may take.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// The id of the first attach request (the first line on the wire: the
/// client's nonce).
const ATTACH_ID: u64 = 1;

/// The id of the second attach request (the client's proof).
const PROOF_ID: u64 = 2;

/// A JSON-RPC notification from the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// The method (e.g. `mob/stream_event`).
    pub method: String,
    /// Its params (`Null` when absent).
    pub params: Value,
}

type Reply = Result<Value, ClientError>;

/// Calls waiting for their response; `None` once the connection is gone.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Reply>>>>>;

/// Removes a call's pending entry when the call ends or is dropped (the
/// reader already removed it when the answer came).
struct Forget<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        if let Some(map) = self.pending.lock().as_mut() {
            map.remove(&self.id);
        }
    }
}

/// One attached connection to the daemon.
pub struct Connection {
    address: SocketAddr,
    /// The agent the daemon bound the connection to (`None`: a probe).
    agent: Option<String>,
    /// The daemon seated the agent for this connection.
    seated: bool,
    /// The daemon's build.
    daemon_build: Option<String>,
    outgoing: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    tasks: [JoinHandle<()>; 2],
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("address", &self.address)
            .field("open", &self.is_open())
            .finish_non_exhaustive()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Connection {
    /// Connect to `endpoint`'s address and attach with its token.
    ///
    /// # Errors
    ///
    /// [`ClientError::Connect`] when nothing accepts, [`ClientError::Timeout`]
    /// when connecting or the handshake takes longer than `timeout`,
    /// [`ClientError::Unproven`] when the process there does not prove it
    /// holds the token, [`ClientError::Refused`] when the daemon
    /// refuses the attach, and [`ClientError::Protocol`] for an answer that
    /// is not the handshake's.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn attach(
        endpoint: &Endpoint,
        timeout: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), ClientError> {
        Self::attach_as(endpoint, timeout, None, false).await
    }

    /// [`Connection::attach`], presenting this client's start token (the
    /// daemon counts the client that started it as its owner) or as a
    /// probe that only reads the status (not counted as a client). A
    /// connection that is no probe works with the most recent agent.
    ///
    /// # Errors
    ///
    /// As [`Connection::attach`].
    pub async fn attach_as(
        endpoint: &Endpoint,
        timeout: Duration,
        start_token: Option<&str>,
        probe: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), ClientError> {
        let agent = (!probe).then_some(crate::daemon::protocol::AgentChoice::Recent);
        Self::attach_to(endpoint, timeout, start_token, probe, agent).await
    }

    /// [`Connection::attach_as`], asking to work with the agent `agent`
    /// names ([`Connection::agent`] says which one the daemon bound).
    ///
    /// # Errors
    ///
    /// As [`Connection::attach`]; [`ClientError::Refused`] when the daemon
    /// cannot bind the connection to that agent.
    pub async fn attach_to(
        endpoint: &Endpoint,
        timeout: Duration,
        start_token: Option<&str>,
        probe: bool,
        agent: Option<crate::daemon::protocol::AgentChoice>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), ClientError> {
        let stream =
            tokio::time::timeout(timeout, tokio::net::TcpStream::connect(endpoint.address))
                .await
                .map_err(|_| ClientError::Timeout("connecting to the daemon"))?
                .map_err(|source| ClientError::Connect {
                    address: endpoint.address,
                    source,
                })?;
        let _ = stream.set_nodelay(true);
        let (read, write) = stream.into_split();
        let params = AttachParams {
            start_token: start_token.map(str::to_string),
            probe,
            agent,
            ..AttachParams::default()
        };
        Self::attach_io(
            read,
            write,
            &endpoint.token,
            params,
            endpoint.address,
            timeout,
        )
        .await
    }

    /// Attach over an already open byte stream (`address` is what
    /// [`Connection::address`] reports). `token` is the secret the
    /// proofs are keyed with and is never written to the stream
    /// ([`crate::daemon::handshake`]): the daemon proves it holds the token
    /// first, and only then does this client prove it does.
    ///
    /// # Errors
    ///
    /// As [`Connection::attach`], without the connect step.
    pub async fn attach_io<R, W>(
        read: R,
        mut write: W,
        token: &str,
        params: AttachParams,
        address: SocketAddr,
        timeout: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = BufReader::new(read);
        let mut params = params;
        let client_nonce = handshake::new_nonce().map_err(ClientError::Io)?;
        params.nonce = Some(client_nonce.clone());
        let handshake = async {
            let hello = json!({
                "jsonrpc": "2.0",
                "id": ATTACH_ID,
                "method": ATTACH_METHOD,
                "params": params,
            });
            write_line(&mut write, &hello).await?;
            let line = read_reply(&mut reader).await?;
            let daemon_nonce = check_challenge(&line, token, &client_nonce, address)?;
            let own_proof = handshake::client_proof(token, &client_nonce, &daemon_nonce)
                .map_err(|err| ClientError::Protocol(err.to_string()))?;
            let proof = json!({
                "jsonrpc": "2.0",
                "id": PROOF_ID,
                "method": ATTACH_METHOD,
                "params": AttachParams {
                    proof: Some(own_proof),
                    ..AttachParams::default()
                },
            });
            write_line(&mut write, &proof).await?;
            read_reply(&mut reader).await
        };
        let line = tokio::time::timeout(timeout, handshake)
            .await
            .map_err(|_| ClientError::Timeout("the attach handshake"))??;
        let attached = check_attached(&line)?;

        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
        let reader_task = tokio::spawn(read_loop(reader, Arc::clone(&pending), notify_tx));
        let writer_pending = Arc::clone(&pending);
        let writer_task = tokio::spawn(async move {
            while let Some(line) = outgoing_rx.recv().await {
                if write.write_all(line.as_bytes()).await.is_err() || write.flush().await.is_err() {
                    break;
                }
            }
            let _ = write.shutdown().await;
            fail_pending(&writer_pending);
        });
        Ok((
            Self {
                address,
                agent: attached.agent,
                seated: attached.seated,
                daemon_build: attached.build,
                outgoing,
                pending,
                next: AtomicU64::new(PROOF_ID + 1),
                tasks: [reader_task, writer_task],
            },
            notify_rx,
        ))
    }

    /// The agent the daemon bound this connection to (empty for a probe,
    /// which is bound to none).
    #[must_use]
    pub fn agent(&self) -> &str {
        self.agent.as_deref().unwrap_or_default()
    }

    /// Whether the daemon seated the agent for this connection.
    #[must_use]
    pub fn seated(&self) -> bool {
        self.seated
    }

    /// The daemon's build ([`crate::BUILD`] of its binary).
    #[must_use]
    pub fn daemon_build(&self) -> Option<&str> {
        self.daemon_build.as_deref()
    }

    /// The daemon's address.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Whether the connection still serves calls.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.pending.lock().is_some()
    }

    /// Call `method` with `params` and wait for its result (no timeout:
    /// callers bound the calls that should be quick).
    ///
    /// # Errors
    ///
    /// [`ClientError::Rpc`] for an error response,
    /// [`ClientError::Disconnected`] when the connection is (or goes) away
    /// before the response.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock();
            let Some(map) = pending.as_mut() else {
                return Err(ClientError::Disconnected);
            };
            map.insert(id, tx);
        }
        // A caller that gives up (a timeout drops this future) takes its
        // entry with it: an answer that never comes leaks nothing.
        let _forget = Forget {
            pending: &self.pending,
            id,
        };
        let mut line =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        if self.outgoing.send(line).is_err() {
            return Err(ClientError::Disconnected);
        }
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(ClientError::Rpc { code, message, .. })) => Err(ClientError::Rpc {
                method: method.to_string(),
                code,
                message,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ClientError::Disconnected),
        }
    }
}

/// One line of the daemon's, during the handshake (at most
/// [`handshake::MAX_LINE`] bytes: whoever answers is not trusted yet).
async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<String, ClientError> {
    let mut line = String::new();
    let read = (&mut *reader)
        .take(handshake::MAX_LINE)
        .read_line(&mut line)
        .await
        .map_err(ClientError::Io)?;
    if read == 0 {
        return Err(ClientError::Disconnected);
    }
    Ok(line)
}

/// The daemon's nonce from its answer to the first attach request, once
/// its proof checks out for `token`. Nothing else is trusted: an answer
/// without a valid proof is [`ClientError::Unproven`] (the token is not
/// sent), and a refusal is [`ClientError::Refused`].
fn check_challenge(
    line: &str,
    token: &str,
    client_nonce: &str,
    address: SocketAddr,
) -> Result<String, ClientError> {
    let unproven = |reason: String| ClientError::Unproven { address, reason };
    let reply: Value =
        serde_json::from_str(line.trim()).map_err(|_| unproven("its answer is not JSON".into()))?;
    if reply.get("id") != Some(&json!(ATTACH_ID)) {
        return Err(unproven("it answered another request".into()));
    }
    if let Some(error) = reply.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        let message = error.get("message").and_then(Value::as_str);
        return Err(ClientError::Refused {
            code: code.and_then(|code| i32::try_from(code).ok()).unwrap_or(0),
            message: message.unwrap_or_default().to_string(),
        });
    }
    let Some(Challenged { challenge }) = reply
        .get("result")
        .cloned()
        .and_then(|result| serde_json::from_value::<Challenged>(result).ok())
    else {
        return Err(unproven("it answered without a proof".into()));
    };
    if challenge.nonce.is_empty()
        || challenge.nonce == client_nonce
        || !handshake::is_daemon_proof(token, client_nonce, &challenge.nonce, &challenge.proof)
    {
        return Err(unproven("its proof does not match the token".into()));
    }
    Ok(challenge.nonce)
}

/// Check the handshake's answer.
fn check_attached(line: &str) -> Result<crate::daemon::protocol::Attached, ClientError> {
    let reply: Value = serde_json::from_str(line.trim())
        .map_err(|err| ClientError::Protocol(format!("not JSON ({err}): {}", line.trim())))?;
    if reply.get("id") != Some(&json!(PROOF_ID)) {
        return Err(ClientError::Protocol(format!(
            "an answer to another request: {reply}"
        )));
    }
    if let Some(error) = reply.get("error") {
        return Err(ClientError::Refused {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    if reply.pointer("/result/attached") == Some(&Value::Bool(true)) {
        reply
            .get("result")
            .cloned()
            .and_then(|result| serde_json::from_value(result).ok())
            .ok_or_else(|| ClientError::Protocol(format!("an unreadable attach answer: {reply}")))
    } else {
        Err(ClientError::Protocol(format!(
            "not an attach result: {reply}"
        )))
    }
}

async fn write_line<W: AsyncWrite + Unpin>(
    write: &mut W,
    value: &Value,
) -> Result<(), ClientError> {
    let mut line = value.to_string();
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .map_err(ClientError::Io)?;
    write.flush().await.map_err(ClientError::Io)
}

/// Route every line: responses to their callers, notifications to `notify`.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: BufReader<R>,
    pending: Pending,
    notify: mpsc::UnboundedSender<Notification>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            tracing::warn!(line = %line.trim(), "the daemon sent a line that is not JSON");
            continue;
        };
        route(message, &pending, &notify);
    }
    fail_pending(&pending);
}

fn route(message: Value, pending: &Pending, notify: &mpsc::UnboundedSender<Notification>) {
    let id = message.get("id").and_then(Value::as_u64);
    let is_response = message.get("result").is_some() || message.get("error").is_some();
    match (id, is_response) {
        (Some(id), true) => {
            let waiter = pending.lock().as_mut().and_then(|map| map.remove(&id));
            let Some(waiter) = waiter else {
                tracing::debug!(id, "a response nobody waits for");
                return;
            };
            let reply = match message.get("error") {
                Some(error) => Err(ClientError::Rpc {
                    method: String::new(),
                    code: error
                        .get("code")
                        .and_then(Value::as_i64)
                        .and_then(|code| i32::try_from(code).ok())
                        .unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(reply);
        }
        _ => match message.get("method").and_then(Value::as_str) {
            Some(method) => {
                let _ = notify.send(Notification {
                    method: method.to_string(),
                    params: message.get("params").cloned().unwrap_or(Value::Null),
                });
            }
            None => tracing::debug!(%message, "an unexpected line from the daemon"),
        },
    }
}

/// The connection is gone: fail every waiting call and refuse new ones.
fn fail_pending(pending: &Pending) {
    if let Some(map) = pending.lock().take() {
        for (_, waiter) in map {
            let _ = waiter.send(Err(ClientError::Disconnected));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::daemon::handshake::{INVALID_REQUEST, UNAUTHORIZED};
    use tokio::io::DuplexStream;

    const ADDR: &str = "127.0.0.1:9";

    /// The server end of a duplex: reads lines, writes lines.
    struct Server {
        reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
        writer: tokio::io::WriteHalf<DuplexStream>,
    }

    impl Server {
        async fn line(&mut self) -> Option<Value> {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).await.unwrap();
            (read > 0).then(|| serde_json::from_str(&line).unwrap())
        }

        async fn send(&mut self, value: Value) {
            write_line(&mut self.writer, &value).await.unwrap();
        }

        /// Answer the attach with the daemon's own handshake.
        async fn accept(&mut self, token: &str) {
            if let Ok(pending) = handshake::read_attach(
                &mut self.reader,
                &mut self.writer,
                token,
                handshake::TIMEOUT,
            )
            .await
            {
                handshake::acknowledge(&mut self.writer, &pending, None, None, handshake::TIMEOUT)
                    .await
                    .unwrap();
            }
        }

        /// Everything the client wrote until it closed.
        async fn rest(&mut self) -> String {
            let mut rest = String::new();
            let _ = self.reader.read_to_string(&mut rest).await;
            rest
        }
    }

    /// A client and a server over a duplex, the client's attach not
    /// started yet.
    fn raw(
        token: &'static str,
    ) -> (
        Server,
        impl std::future::Future<
            Output = Result<(Connection, mpsc::UnboundedReceiver<Notification>), ClientError>,
        >,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let (cr, cw) = tokio::io::split(client);
        (
            Server {
                reader: BufReader::new(sr),
                writer: sw,
            },
            Connection::attach_io(
                cr,
                cw,
                token,
                AttachParams::default(),
                ADDR.parse().unwrap(),
                ATTACH_TIMEOUT,
            ),
        )
    }

    /// D1 of the security review: a process on the daemon's port that
    /// does not know the token never receives it, whatever it answers.
    #[tokio::test]
    async fn a_squatter_that_does_not_know_the_token_never_receives_it() {
        let answers = [
            json!({"jsonrpc": "2.0", "id": 1, "result": {"attached": true}}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"challenge": {"nonce": "ab".repeat(32), "proof": "00".repeat(32)}}}),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"challenge": {"nonce": "ab".repeat(32), "proof": handshake::daemon_proof("guess", "x", &"ab".repeat(32)).unwrap()}}}),
            json!({"jsonrpc": "2.0", "id": 9, "result": {}}),
        ];
        for answer in answers {
            let (mut server, attach) = raw("secret");
            let squatter = async {
                let hello = server.line().await.unwrap();
                assert_eq!(hello["method"], ATTACH_METHOD);
                assert!(hello["params"].get("token").is_none(), "{hello}");
                server.send(answer.clone()).await;
            };
            let (attached, ()) = tokio::join!(attach, squatter);
            assert!(
                matches!(attached, Err(ClientError::Unproven { .. })),
                "{answer}: {attached:?}"
            );
            let rest = server.rest().await;
            assert!(!rest.contains("secret"), "{answer}: {rest}");
        }
    }

    /// The daemon's side: a client proof that does not match (one keyed by
    /// another token, or the daemon's own proof played back) is refused.
    #[tokio::test]
    async fn a_client_without_the_token_cannot_prove_it() {
        let nonce = "cd".repeat(32);
        for forge in [
            |_: &str, dn: &str| handshake::client_proof("guess", &"cd".repeat(32), dn).unwrap(),
            |_: &str, dn: &str| handshake::daemon_proof("secret", &"cd".repeat(32), dn).unwrap(),
            |proof: &str, _: &str| proof.to_string(),
        ] {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (sr, mut sw) = tokio::io::split(server);
            let (cr, mut cw) = tokio::io::split(client);
            let mut sr = BufReader::new(sr);
            let mut cr = BufReader::new(cr);
            let daemon = handshake::read_attach(&mut sr, &mut sw, "secret", handshake::TIMEOUT);
            let attacker = async {
                write_line(&mut cw, &json!({"jsonrpc": "2.0", "id": 1, "method": ATTACH_METHOD, "params": {"nonce": nonce}})).await.unwrap();
                let challenge: Value =
                    serde_json::from_str(&read_reply(&mut cr).await.unwrap()).unwrap();
                let dn = challenge["result"]["challenge"]["nonce"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let daemon_proof = challenge["result"]["challenge"]["proof"]
                    .as_str()
                    .unwrap()
                    .to_string();
                write_line(&mut cw, &json!({"jsonrpc": "2.0", "id": 2, "method": ATTACH_METHOD, "params": {"proof": forge(&daemon_proof, &dn)}})).await.unwrap();
                let refusal: Value =
                    serde_json::from_str(&read_reply(&mut cr).await.unwrap()).unwrap();
                assert_eq!(refusal["error"]["code"], UNAUTHORIZED);
            };
            let (admitted, ()) = tokio::join!(daemon, attacker);
            assert!(
                matches!(admitted, Err(handshake::HandshakeError::Unauthorized)),
                "{admitted:?}"
            );
        }
    }

    fn pair() -> (
        Server,
        impl std::future::Future<
            Output = Result<(Connection, mpsc::UnboundedReceiver<Notification>), ClientError>,
        >,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let (cr, cw) = tokio::io::split(client);
        let attach = Connection::attach_io(
            cr,
            cw,
            "secret",
            AttachParams::default(),
            ADDR.parse().unwrap(),
            ATTACH_TIMEOUT,
        );
        (
            Server {
                reader: BufReader::new(sr),
                writer: sw,
            },
            attach,
        )
    }

    /// A call whose caller gave up (a timeout dropped it) leaves no
    /// pending entry behind, though its answer never comes.
    #[tokio::test]
    async fn a_dropped_call_forgets_its_pending_entry() {
        let (mut server, attach) = pair();
        let server_task = tokio::spawn(async move {
            server.accept("secret").await;
            let unanswered = server.line().await.unwrap();
            assert_eq!(unanswered["method"], "never");
            server
        });
        let (connection, _notifications) = attach.await.unwrap();
        let gave_up = tokio::time::timeout(
            Duration::from_millis(50),
            connection.call("never", json!({})),
        )
        .await;
        assert!(gave_up.is_err());
        let left = connection.pending.lock().as_ref().map(HashMap::len);
        assert_eq!(left, Some(0));
        drop(server_task.await.unwrap());
    }

    #[tokio::test]
    async fn the_attach_line_comes_first_and_calls_are_pipelined() {
        let (mut server, attach) = pair();
        let server_task = tokio::spawn(async move {
            server.accept("secret").await;
            // Two requests in flight; answer them out of order, with a
            // notification in between.
            let first = server.line().await.unwrap();
            let second = server.line().await.unwrap();
            assert_eq!(first["method"], "slow");
            assert_eq!(second["method"], "quick");
            assert_ne!(first["id"], second["id"]);
            server
                .send(json!({"jsonrpc": "2.0", "id": second["id"], "result": {"n": 2}}))
                .await;
            server
                .send(json!({"jsonrpc": "2.0", "method": "mob/stream_event", "params": {"sequence": 1}}))
                .await;
            server
                .send(json!({"jsonrpc": "2.0", "id": first["id"], "error": {"code": -32602, "message": "bad"}}))
                .await;
            server
        });
        let (connection, mut notifications) = attach.await.unwrap();
        let connection = Arc::new(connection);
        let slow = {
            let connection = Arc::clone(&connection);
            tokio::spawn(async move { connection.call("slow", json!({})).await })
        };
        // Let `slow` go out first.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let quick = connection.call("quick", json!({})).await.unwrap();
        assert_eq!(quick, json!({"n": 2}));
        let slow = slow.await.unwrap();
        assert!(
            matches!(&slow, Err(ClientError::Rpc { method, code: -32602, message }) if method == "slow" && message == "bad"),
            "{slow:?}"
        );
        let note = notifications.recv().await.unwrap();
        assert_eq!(note.method, "mob/stream_event");
        assert_eq!(note.params["sequence"], 1);
        // The server going away fails the next call and ends the
        // notifications.
        drop(server_task.await.unwrap());
        assert!(notifications.recv().await.is_none());
        assert!(matches!(
            connection.call("after", json!({})).await,
            Err(ClientError::Disconnected)
        ));
        assert!(!connection.is_open());
    }

    #[tokio::test]
    async fn a_pending_call_fails_when_the_connection_drops() {
        let (mut server, attach) = pair();
        let server_task = tokio::spawn(async move {
            server.accept("secret").await;
            let _ = server.line().await;
            server
        });
        let (connection, _notifications) = attach.await.unwrap();
        let call = connection.call("never", json!({}));
        let dropper = async {
            drop(server_task.await.unwrap());
        };
        let (result, ()) = tokio::join!(call, dropper);
        assert!(
            matches!(result, Err(ClientError::Disconnected)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_wrong_token_or_another_answer_is_refused() {
        // A client with another token cannot check the daemon's proof:
        // to it, that daemon is unproven.
        let (mut server, attach) = raw("wrong");
        let (attached, ()) = tokio::join!(attach, server.accept("secret"));
        assert!(
            matches!(attached, Err(ClientError::Unproven { .. })),
            "{attached:?}"
        );
        assert!(!server.rest().await.contains("wrong"));
        assert!(matches!(
            check_attached(&json!({"jsonrpc": "2.0", "id": 2, "error": {"code": INVALID_REQUEST, "message": "x"}}).to_string()),
            Err(ClientError::Refused { code: INVALID_REQUEST, .. })
        ));
        assert!(matches!(
            check_attached(&json!({"jsonrpc": "2.0", "id": 7, "result": {}}).to_string()),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            check_attached("hello"),
            Err(ClientError::Protocol(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_server_times_the_handshake_out() {
        let (client, _server) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client);
        let attached = Connection::attach_io(
            cr,
            cw,
            "t",
            AttachParams::default(),
            ADDR.parse().unwrap(),
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(attached, Err(ClientError::Timeout(_))),
            "{attached:?}"
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_a_connect_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = Endpoint {
            address,
            token: "t".into(),
            pid: 1,
            workspace: None,
            started_at: None,
        };
        let attached = Connection::attach(&endpoint, ATTACH_TIMEOUT).await;
        assert!(
            matches!(attached, Err(ClientError::Connect { .. })),
            "{attached:?}"
        );
    }
}
