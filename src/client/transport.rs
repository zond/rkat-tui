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
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::ClientError;
use crate::daemon::endpoint::Endpoint;
use crate::daemon::handshake::ATTACH_METHOD;

/// How long connecting and the attach handshake may take.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// The id of the attach request (the first line on the wire).
const ATTACH_ID: u64 = 1;

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
    /// [`ClientError::Refused`] when the daemon refuses the token, and
    /// [`ClientError::Protocol`] for an answer that is not the handshake's.
    pub async fn attach(
        endpoint: &Endpoint,
        timeout: Duration,
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
        Self::attach_io(read, write, &endpoint.token, endpoint.address, timeout).await
    }

    /// Attach over an already open byte stream (`address` is what
    /// [`Connection::address`] reports).
    ///
    /// # Errors
    ///
    /// As [`Connection::attach`], without the connect step.
    pub async fn attach_io<R, W>(
        read: R,
        mut write: W,
        token: &str,
        address: SocketAddr,
        timeout: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Notification>), ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = BufReader::new(read);
        let request = json!({
            "jsonrpc": "2.0",
            "id": ATTACH_ID,
            "method": ATTACH_METHOD,
            "params": {"token": token},
        });
        let handshake = async {
            write_line(&mut write, &request).await?;
            let mut line = String::new();
            let read = reader.read_line(&mut line).await.map_err(ClientError::Io)?;
            if read == 0 {
                return Err(ClientError::Disconnected);
            }
            Ok(line)
        };
        let line = tokio::time::timeout(timeout, handshake)
            .await
            .map_err(|_| ClientError::Timeout("the attach handshake"))??;
        check_attached(&line)?;

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
                outgoing,
                pending,
                next: AtomicU64::new(ATTACH_ID + 1),
                tasks: [reader_task, writer_task],
            },
            notify_rx,
        ))
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

/// Check the handshake's answer.
fn check_attached(line: &str) -> Result<(), ClientError> {
    let reply: Value = serde_json::from_str(line.trim())
        .map_err(|err| ClientError::Protocol(format!("not JSON ({err}): {}", line.trim())))?;
    if reply.get("id") != Some(&json!(ATTACH_ID)) {
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
        Ok(())
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

        /// Answer the attach line like the daemon does.
        async fn accept(&mut self, token: &str) {
            let attach = self.line().await.unwrap();
            assert_eq!(attach["method"], ATTACH_METHOD);
            if attach["params"]["token"] == token {
                self.send(
                    json!({"jsonrpc": "2.0", "id": attach["id"], "result": {"attached": true}}),
                )
                .await;
            } else {
                self.send(json!({"jsonrpc": "2.0", "id": attach["id"], "error": {"code": UNAUTHORIZED, "message": "wrong or missing token"}}))
                    .await;
            }
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
        let attach = Connection::attach_io(cr, cw, "secret", ADDR.parse().unwrap(), ATTACH_TIMEOUT);
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
        let (client, server) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(server);
        let mut server = Server {
            reader: BufReader::new(sr),
            writer: sw,
        };
        let (cr, cw) = tokio::io::split(client);
        let attach = Connection::attach_io(cr, cw, "wrong", ADDR.parse().unwrap(), ATTACH_TIMEOUT);
        let (attached, ()) = tokio::join!(attach, server.accept("secret"));
        assert!(
            matches!(
                attached,
                Err(ClientError::Refused {
                    code: UNAUTHORIZED,
                    ..
                })
            ),
            "{attached:?}"
        );
        assert!(matches!(
            check_attached(&json!({"jsonrpc": "2.0", "id": 1, "error": {"code": INVALID_REQUEST, "message": "x"}}).to_string()),
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
        let attached =
            Connection::attach_io(cr, cw, "t", ADDR.parse().unwrap(), Duration::from_secs(1)).await;
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
