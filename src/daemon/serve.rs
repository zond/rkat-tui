//! Serving the engine's method router over loopback TCP.
//!
//! The loop copies the structure of `meerkat_rpc::serve_tcp_with_options_until`
//! (which binds its own listener and so cannot report an ephemeral port, nor
//! run a handshake before the router): one task per accepted connection, a
//! shared runtime, `skip_shutdown_on_eof` so a client leaving never shuts the
//! runtime down. Each connection first passes [`super::handshake`]; only then
//! is the stream handed to `RpcServer`, through [`proxy`]: the daemon's own
//! `rkat/*` methods ([`super::local`]) are answered there, each in a task of
//! its own (a slow `rkat/resume` or `rkat/conversations` must not hold back
//! the client's next lines: its Esc, its `decision/decide`), and every other
//! line goes to the engine (after [`super::local::Local::observe`] read it).
//!
//! The engine's own TCP connection and request admission
//! (`RpcTcpConnectionAdmission`, `RpcRequestAdmission`), its per-connection
//! shutdown signal and its shutdown hold are crate-private, so shutdown here
//! is: stop accepting, give the connections [`DRAIN`] to end on their own,
//! shut the runtime down, then abort what is left.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meerkat_core::ConfigStore;
use meerkat_rpc::server::RpcServer;
use meerkat_rpc::session_runtime::SessionRuntime;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use std::collections::HashSet;

use super::DaemonError;
use super::decisions::ClientGuard;
use super::endpoint::{self, Endpoint};
use super::handshake;
use super::lifecycle::Caller;
use super::local::Local;
use super::protocol::{self, Refusal};
use meerkat_rpc::protocol::RpcId;

/// The in-memory pipe between the proxy and the engine's server.
const PIPE: usize = 1 << 20;

/// How long connections get to end on their own at shutdown.
pub const DRAIN: Duration = Duration::from_secs(2);

/// One line for a client, and who to tell once it is written to the
/// client's socket (a decision notice is recorded as presented only then).
#[derive(Debug)]
pub struct Outgoing {
    /// The line, newline included.
    pub line: String,
    /// Told when the line is written and flushed.
    pub written: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Outgoing {
    pub(crate) fn line(line: String) -> Self {
        Self {
            line,
            written: None,
        }
    }
}

/// The methods the daemon adds to the engine's `initialize`
/// catalog: it pushes `decision/notice` to every attached client and
/// decides `decision/decide` itself (plain TCP serves neither).
pub const DECISION_METHODS: [&str; 2] = [protocol::DECISION_NOTICE, protocol::DECISION_DECIDE];

/// The bound listener and the endpoint file announcing it.
#[derive(Debug)]
pub struct Listener {
    tcp: Option<TcpListener>,
    address: SocketAddr,
    file: PathBuf,
    token: Arc<str>,
}

impl Listener {
    /// Bind `listen` (loopback) and write the endpoint file at `file`,
    /// naming `workspace` (canonical), the start time `started_at` and the
    /// `realm` when it is not the workspace's own.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Io`] when binding or writing fails,
    /// [`DaemonError::Token`] when no token can be drawn.
    pub async fn bind(
        listen: SocketAddr,
        file: &Path,
        workspace: &Path,
        started_at: &str,
        realm: Option<String>,
    ) -> Result<Self, DaemonError> {
        let token: Arc<str> = endpoint::new_token()?.into();
        let tcp = TcpListener::bind(listen)
            .await
            .map_err(|err| DaemonError::io("listen on", Path::new(&listen.to_string()), err))?;
        let address = tcp
            .local_addr()
            .map_err(|err| DaemonError::io("read the bound address of", file, err))?;
        Endpoint {
            address,
            token: token.to_string(),
            pid: std::process::id(),
            workspace: Some(workspace.to_path_buf()),
            started_at: Some(started_at.to_string()),
            realm,
        }
        .write(file)?;
        Ok(Self {
            tcp: Some(tcp),
            address,
            file: file.to_path_buf(),
            token,
        })
    }

    /// The bound address.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The endpoint file.
    #[must_use]
    pub fn endpoint_file(&self) -> &Path {
        &self.file
    }

    /// Remove the endpoint file, when it is still ours.
    pub fn remove_files(&self) {
        let ours = Endpoint::read(&self.file).is_ok_and(|e| *e.token == *self.token);
        if ours && let Err(err) = std::fs::remove_file(&self.file) {
            tracing::warn!(file = %self.file.display(), error = %err, "cannot remove the endpoint file");
        }
    }
}

/// Accept until `shutdown`; then stop accepting, wait up to [`DRAIN`] for
/// the connections to end, and return those still running (the caller
/// aborts them once the runtime has shut down).
///
/// # Errors
///
/// [`DaemonError::Io`] when accepting fails.
pub async fn serve(
    listener: &mut Listener,
    runtime: &Arc<SessionRuntime>,
    config_store: &Arc<dyn ConfigStore>,
    local: &Arc<Local>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<JoinSet<()>, DaemonError> {
    let Some(tcp) = listener.tcp.take() else {
        return Ok(JoinSet::new());
    };
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    let accepted = loop {
        let (stream, peer) = tokio::select! {
            () = &mut shutdown => break Ok(()),
            accepted = tcp.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(err) => break Err(DaemonError::io("accept on", &listener.file, err)),
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
        };
        let _ = stream.set_nodelay(true);
        connections.spawn(connection(
            stream,
            peer,
            Arc::clone(runtime),
            Arc::clone(config_store),
            Arc::clone(local),
            Arc::clone(&listener.token),
        ));
    };
    drop(tcp);
    let _ = tokio::time::timeout(DRAIN, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    accepted.map(|()| connections)
}

/// One client: the handshake, then the engine's server until the client
/// leaves.
async fn connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    runtime: Arc<SessionRuntime>,
    config_store: Arc<dyn ConfigStore>,
    local: Arc<Local>,
    token: Arc<str>,
) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let seat = match admit(
        &mut reader,
        &mut write,
        &token,
        local.lifecycle(),
        Some(local.agents()),
    )
    .await
    {
        Ok(seat) => seat,
        Err(err) => {
            tracing::warn!(%peer, error = %err, "refused a connection");
            return;
        }
    };
    let caller = seat.caller();
    tracing::info!(%peer, owner = caller.owner, counted = seat.counted(), agent = ?caller.agent, "client attached");
    if let Some(agent) = &caller.agent {
        local.bound(agent).await;
    }
    let (engine_side, proxy_side) = tokio::io::duplex(PIPE);
    let (server_read, server_write) = tokio::io::split(engine_side);
    // Aborted with this task (a shutdown aborts the connections), so the
    // client sees its socket close.
    let _proxied = AbortOnDrop(tokio::spawn(proxy(
        reader, write, proxy_side, local, caller,
    )));
    let mut server = match RpcServer::new(
        BufReader::new(server_read),
        server_write,
        runtime,
        config_store,
    ) {
        Ok(server) => server,
        Err(err) => {
            tracing::warn!(%peer, error = %err, "cannot serve the client");
            return;
        }
    };
    // The runtime is shared by every connection: a client leaving must not
    // shut it down.
    server.skip_shutdown_on_eof = true;
    match Box::pin(server.run()).await {
        Ok(()) => tracing::info!(%peer, "client detached"),
        Err(err) => tracing::warn!(%peer, error = %err, "client disconnected"),
    }
    drop(seat);
}

/// The handshake of one connection: read and check the attach line, take
/// the connection's seat in `lifecycle` (counted until the connection
/// ends), bind it to the agent it asks for (with `agents`; a probe is bound
/// to none, a client of before P5 to the most recent agent; a new agent is
/// seated here), and only then acknowledge it, naming the agent, so a
/// client that read `{attached: true}` is already counted for every other
/// connection's `rkat/status`. A seat whose acknowledgement cannot be
/// written is given up without leaving its agent detached: that client
/// never attached. A bind that fails is answered with the refusal.
///
/// # Errors
///
/// [`handshake::HandshakeError`] (a refusal, or the acknowledgement
/// failed).
pub async fn admit<R, W>(
    reader: &mut R,
    writer: &mut W,
    token: &str,
    lifecycle: &Arc<super::lifecycle::Lifecycle>,
    agents: Option<&Arc<super::agents::Agents>>,
) -> Result<super::lifecycle::Seat, handshake::HandshakeError>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let pending = handshake::read_attach(reader, writer, token, handshake::TIMEOUT).await?;
    let mut seat = lifecycle.seat(&pending.params);
    let mut bound = None;
    if let Some(agents) = agents
        && !pending.params.probe
    {
        let choice = pending
            .params
            .agent
            .clone()
            .unwrap_or(protocol::AgentChoice::Recent);
        match agents.bind(&choice).await {
            Ok((binding, agent)) => {
                seat.bind(binding);
                bound = Some(agent);
            }
            Err(err) => {
                let line = protocol::error_line(
                    Some(pending.id.clone()),
                    protocol::NO_AGENT,
                    &err.to_string(),
                );
                let _ = tokio::time::timeout(handshake::TIMEOUT, async {
                    let _ = writer.write_all(line.as_bytes()).await;
                    let _ = writer.flush().await;
                    let _ = writer.shutdown().await;
                })
                .await;
                seat.withdraw();
                return Err(handshake::HandshakeError::NoAgent(err.to_string()));
            }
        }
    }
    match handshake::acknowledge(writer, &pending, bound.as_ref(), handshake::TIMEOUT).await {
        Ok(()) => Ok(seat),
        Err(err) => {
            seat.withdraw();
            Err(err)
        }
    }
}

/// Aborts its task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What [`proxy`] answers itself and what it learns from the rest
/// ([`Local`]; tests substitute their own).
#[async_trait::async_trait]
pub trait LocalMethods: Send + Sync {
    /// Answer the local method `method` for `caller`.
    async fn call(&self, caller: Caller, method: &str, params: Value) -> Result<Value, Refusal>;
    /// Learn from an engine request on its way; returns it as forwarded.
    fn observe(&self, request: Value) -> Value;
    /// A client attached: the daemon's consent surface offers it decisions
    /// through `out` until the guard is dropped.
    fn connect(&self, out: mpsc::UnboundedSender<Outgoing>) -> ClientGuard;
    /// The client `client`'s `decision/decide`: its answer line.
    async fn decide(&self, client: u64, id: Option<RpcId>, params: Value) -> String;
}

#[async_trait::async_trait]
impl LocalMethods for Local {
    async fn call(&self, caller: Caller, method: &str, params: Value) -> Result<Value, Refusal> {
        Local::call(self, caller, method, params).await
    }

    fn observe(&self, request: Value) -> Value {
        Local::observe(self, request)
    }

    fn connect(&self, out: mpsc::UnboundedSender<Outgoing>) -> ClientGuard {
        self.decisions().connect(out)
    }

    async fn decide(&self, client: u64, id: Option<RpcId>, params: Value) -> String {
        self.decisions().decide(client, id, params).await
    }
}

/// Carry one client's lines: from the client, a local method is answered
/// here (in a task of its own, the answer going out when it is ready), a
/// `decision/decide` is decided by the daemon's consent surface (likewise),
/// and anything else goes (observed) to the engine; the engine's lines go
/// back to the client, with the decision methods added to an `initialize`
/// answer, and so do the decision notices the
/// consent surface offers this client. Ends when the client closes (the
/// engine then sees its end of input) or the engine's side closes; a local
/// call still running then finishes (a conversation switch is not cut in
/// half), and its answer is dropped. `caller` is what the client proved
/// at attach (the lifecycle methods answer for it).
pub async fn proxy<R, W, E, L>(
    client_read: R,
    mut client_write: W,
    engine: E,
    local: Arc<L>,
    caller: Caller,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    E: tokio::io::AsyncRead + AsyncWrite + Unpin + Send + 'static,
    L: LocalMethods + ?Sized + 'static,
{
    let (engine_read, mut engine_write) = tokio::io::split(engine);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outgoing>();
    let mut writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(outgoing) = out_rx.recv().await {
            if client_write
                .write_all(outgoing.line.as_bytes())
                .await
                .is_err()
                || client_write.flush().await.is_err()
            {
                break;
            }
            if let Some(written) = outgoing.written {
                let _ = written.send(());
            }
        }
        let _ = client_write.shutdown().await;
    }));
    // The client's registration with the consent surface: dropped (and the
    // client's decide eligibility with it) when the client leaves.
    let decisions = local.connect(out_tx.clone());
    let client_id = decisions.id();
    // `initialize` requests whose answers get the decision methods.
    let initializing: Arc<parking_lot::Mutex<HashSet<String>>> = Arc::default();
    let mut from_engine = AbortOnDrop(tokio::spawn({
        let out_tx = out_tx.clone();
        let initializing = Arc::clone(&initializing);
        async move {
            let mut lines = BufReader::new(engine_read);
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let mut forward = std::mem::take(&mut line);
                        if !initializing.lock().is_empty() {
                            forward = with_decision_methods(forward, &initializing);
                        }
                        if out_tx.send(Outgoing::line(forward)).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }));
    let mut lines = client_read;
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        match route(&line, &*local) {
            Routed::Local { id, method, params } => {
                let local = Arc::clone(&local);
                let caller = caller.clone();
                // Weak: a call still running when the client leaves does
                // not keep the writer waiting for its answer.
                let out_tx = out_tx.downgrade();
                tokio::spawn(async move {
                    let line = answer(&*local, caller, id, &method, params).await;
                    if let Some(out_tx) = out_tx.upgrade() {
                        let _ = out_tx.send(Outgoing::line(line));
                    }
                });
            }
            Routed::Decide { id, params } => {
                let local = Arc::clone(&local);
                let out_tx = out_tx.downgrade();
                tokio::spawn(async move {
                    let line = local.decide(client_id, id, params).await;
                    if let Some(out_tx) = out_tx.upgrade() {
                        let _ = out_tx.send(Outgoing::line(line));
                    }
                });
            }
            Routed::Refused { id, method } => {
                if id.is_some() {
                    tracing::warn!(%method, "refused an engine method the daemon does not serve");
                    let _ = out_tx.send(Outgoing::line(protocol::error_line(
                        id,
                        protocol::NOT_SERVED,
                        &format!(
                            "rkat-tui-mob does not serve {method}: clients reach the agent through the daemon's mob only, so every session runs behind the approval gate"
                        ),
                    )));
                }
            }
            Routed::Malformed { answer } => {
                if let Some(answer) = answer {
                    tracing::warn!("answered a malformed client line without forwarding it");
                    let _ = out_tx.send(Outgoing::line(answer));
                }
            }
            Routed::Forward(forward) => {
                if let Some(id) = initialize_id(&forward) {
                    initializing.lock().insert(id);
                }
                if engine_write.write_all(forward.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
    drop(decisions);
    let _ = engine_write.shutdown().await;
    drop(out_tx);
    let _ = (&mut from_engine.0).await;
    let _ = (&mut writer.0).await;
}

/// The id (as JSON text) of an `initialize` request `line`.
fn initialize_id(line: &str) -> Option<String> {
    if !line.contains(protocol::engine::INITIALIZE) {
        return None;
    }
    let request: Value = serde_json::from_str(line.trim_end()).ok()?;
    (request.get("method").and_then(Value::as_str) == Some(protocol::engine::INITIALIZE))
        .then(|| request.get("id").map(Value::to_string))
        .flatten()
}

/// `line` from the engine, with [`DECISION_METHODS`] added to its method
/// catalog when it answers one of the `initialize` requests in `waiting`
/// (which then forgets it).
fn with_decision_methods(line: String, waiting: &parking_lot::Mutex<HashSet<String>>) -> String {
    let Ok(mut answer) = serde_json::from_str::<Value>(line.trim_end()) else {
        return line;
    };
    let Some(id) = answer.get("id").map(Value::to_string) else {
        return line;
    };
    if !waiting.lock().remove(&id) {
        return line;
    }
    let Some(methods) = answer
        .pointer_mut("/result/methods")
        .and_then(Value::as_array_mut)
    else {
        return line;
    };
    for method in DECISION_METHODS {
        if !methods.iter().any(|known| known.as_str() == Some(method)) {
            methods.push(json!(method));
        }
    }
    format!("{answer}\n")
}

/// Where one client line goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Routed {
    /// A `decision/decide`, for the daemon's consent surface.
    Decide {
        /// The request id.
        id: Option<RpcId>,
        /// Its params (`Null` when absent).
        params: Value,
    },
    /// A local method call, to answer with [`answer`].
    Local {
        /// The request id.
        id: Option<RpcId>,
        /// The method.
        method: String,
        /// Its params (`Null` when absent).
        params: Value,
    },
    /// The line for the engine (newline included): the proxy's own
    /// serialization of the request it parsed, never the client's bytes.
    Forward(String),
    /// A line that is not a JSON-RPC request the proxy accepts (unparsable,
    /// not an object, no `method`, a bad `jsonrpc` or `id`): never
    /// forwarded; `answer` is the error line for the client, `None` for a
    /// blank line or one without an id.
    Malformed {
        /// The error line (newline included) to answer with.
        answer: Option<String>,
    },
    /// An engine method the daemon does not serve
    /// ([`protocol::engine::SERVED`]): answered with
    /// [`protocol::NOT_SERVED`] (a request) or dropped (a notification).
    Refused {
        /// The request id (`None`: a notification, which gets no answer).
        id: Option<RpcId>,
        /// The method.
        method: String,
    },
}

/// Pick out a local method call, else hand a served engine request on
/// (observed), as the proxy's own serialization of what it parsed.
///
/// The proxy forwards only what it parsed. A line it cannot read as a
/// JSON-RPC request object (`jsonrpc` "2.0", `method` a string, `id` absent,
/// a string or an integer) is answered here ([`Routed::Malformed`]) and
/// never reaches the engine: the engine's frame parser trims Unicode
/// whitespace that `serde_json` does not, so a raw line this parse refused
/// could still be a request the engine runs without having met
/// [`protocol::engine::SERVED`] (a leading form feed did exactly that).
pub fn route<L: LocalMethods + ?Sized>(line: &str, local: &L) -> Routed {
    if line.trim().is_empty() {
        return Routed::Malformed { answer: None };
    }
    let Ok(request) = serde_json::from_str::<Value>(line.trim_end_matches(['\r', '\n'])) else {
        return Routed::Malformed {
            answer: Some(protocol::error_line(
                None,
                protocol::PARSE_ERROR,
                "not a JSON-RPC message: the daemon reads one JSON object per line",
            )),
        };
    };
    let method = match envelope(&request) {
        Ok(method) => method,
        Err(problem) => {
            // A request (it has an id) gets an answer, under its id when
            // that reads as one; anything else is dropped.
            let answer = request.get("id").map(|id| {
                protocol::error_line(protocol::request_id(id), protocol::INVALID_REQUEST, problem)
            });
            return Routed::Malformed { answer };
        }
    };
    let id = || protocol::request_id(request.get("id").unwrap_or(&Value::Null));
    match method {
        protocol::DECISION_DECIDE => Routed::Decide {
            id: id(),
            params: request.get("params").cloned().unwrap_or(Value::Null),
        },
        method if method.starts_with(protocol::PREFIX) => Routed::Local {
            id: id(),
            method: method.to_string(),
            params: request.get("params").cloned().unwrap_or(Value::Null),
        },
        method if !protocol::engine::serves(method) => Routed::Refused {
            id: id(),
            method: method.to_string(),
        },
        _ => {
            let observed = local.observe(request);
            Routed::Forward(format!("{observed}\n"))
        }
    }
}

/// The method of `request` when it is a JSON-RPC request object the proxy
/// accepts, else what is wrong with it.
fn envelope(request: &Value) -> Result<&str, &'static str> {
    let Some(object) = request.as_object() else {
        return Err("not a JSON-RPC request: one object per line (no batches)");
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("not a JSON-RPC 2.0 request: \"jsonrpc\" must be \"2.0\"");
    }
    match object.get("id") {
        None | Some(Value::String(_)) => {}
        Some(Value::Number(n)) if n.is_i64() => {}
        Some(_) => return Err("the request id must be a string or an integer"),
    }
    object
        .get("method")
        .and_then(Value::as_str)
        .ok_or("not a request: the daemon takes no responses, and a request names its method")
}

/// The answer line (newline included) of `caller`'s local call `method`.
pub async fn answer<L: LocalMethods + ?Sized>(
    local: &L,
    caller: Caller,
    id: Option<RpcId>,
    method: &str,
    params: Value,
) -> String {
    match local.call(caller, method, params).await {
        Ok(result) => protocol::result_line(id, result),
        Err(refusal) => protocol::error_line(id, refusal.code, &refusal.message),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A writer that fails every write after recording the client count
    /// the lifecycle had when the acknowledgement was written.
    struct Recording {
        lifecycle: Arc<super::super::lifecycle::Lifecycle>,
        seen: Option<usize>,
        fail: bool,
    }

    impl tokio::io::AsyncWrite for Recording {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.seen = Some(self.lifecycle.clients());
            if self.fail {
                std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
            } else {
                std::task::Poll::Ready(Ok(buf.len()))
            }
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    const ATTACH_LINE: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"rkat/attach\",\"params\":{\"token\":\"t\",\"start_token\":\"start\"}}\n";

    /// The CI failure this guards: the acknowledgement was written before
    /// the connection's seat was taken, so a client that had attached could
    /// be missing from another connection's `rkat/status`. The seat is
    /// taken first: at the moment the acknowledgement is written the
    /// client is counted.
    #[tokio::test]
    async fn the_client_is_counted_before_its_attach_is_acknowledged() {
        let lifecycle = super::super::lifecycle::Lifecycle::new(
            "/w".into(),
            "now".into(),
            Some("start".into()),
        );
        let mut reader = tokio::io::BufReader::new(ATTACH_LINE);
        let mut writer = Recording {
            lifecycle: Arc::clone(&lifecycle),
            seen: None,
            fail: false,
        };
        let seat = admit(&mut reader, &mut writer, "t", &lifecycle, None)
            .await
            .unwrap();
        assert_eq!(writer.seen, Some(1), "counted when the answer was written");
        assert!(seat.caller().owner);
        drop(seat);
        assert_eq!(lifecycle.clients(), 0);
    }

    /// An acknowledgement that cannot be written gives the seat up: the
    /// count drops back, and the owner that never learned it attached did
    /// not detach.
    #[tokio::test]
    async fn a_failed_acknowledgement_gives_the_seat_up_without_a_detach() {
        let lifecycle = super::super::lifecycle::Lifecycle::new(
            "/w".into(),
            "now".into(),
            Some("start".into()),
        );
        let mut reader = tokio::io::BufReader::new(ATTACH_LINE);
        let mut writer = Recording {
            lifecycle: Arc::clone(&lifecycle),
            seen: None,
            fail: true,
        };
        let refused = admit(&mut reader, &mut writer, "t", &lifecycle, None).await;
        assert!(refused.is_err());
        assert_eq!(writer.seen, Some(1), "the seat was taken before the write");
        assert_eq!(lifecycle.clients(), 0, "and given up after it failed");
    }

    /// `rkat/slow` waits for its gate; everything else passes unchanged.
    struct Slow {
        gate: tokio::sync::Notify,
        decisions: Arc<super::super::decisions::Decisions>,
    }

    #[async_trait::async_trait]
    impl LocalMethods for Slow {
        async fn call(
            &self,
            _caller: Caller,
            method: &str,
            _params: Value,
        ) -> Result<Value, Refusal> {
            if method == "rkat/slow" {
                self.gate.notified().await;
            }
            Ok(json!({"method": method}))
        }

        fn observe(&self, request: Value) -> Value {
            request
        }

        fn connect(&self, out: mpsc::UnboundedSender<Outgoing>) -> ClientGuard {
            self.decisions.connect(out)
        }

        async fn decide(&self, client: u64, id: Option<RpcId>, params: Value) -> String {
            self.decisions.decide(client, id, params).await
        }
    }

    async fn next_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> String {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("not held back")
            .unwrap();
        line
    }

    /// The finding this guards: the proxy awaited each local call before
    /// reading the client's next line, so a slow `rkat/resume` held back
    /// the client's Esc (`mob/stop_member_run`) and its other calls.
    #[tokio::test]
    async fn a_slow_local_call_holds_back_no_other_line() {
        let (client, client_side) = tokio::io::duplex(1 << 16);
        let (engine, engine_side) = tokio::io::duplex(1 << 16);
        let (client_read, client_write) = tokio::io::split(client_side);
        let service = meerkat_runtime::decision::DurableDecisionService::new(Arc::new(
            meerkat_core::approval::decision::InMemoryDecisionStore::new(),
        ));
        let principal = super::super::decisions::Principal::new("alice").unwrap();
        let local = Arc::new(Slow {
            gate: tokio::sync::Notify::new(),
            decisions: super::super::decisions::Decisions::new(service, principal, None).unwrap(),
        });
        let proxied = tokio::spawn(proxy(
            BufReader::new(client_read),
            client_write,
            engine_side,
            Arc::clone(&local),
            Caller::default(),
        ));
        let (client_in, mut client_out) = tokio::io::split(client);
        let mut client_in = BufReader::new(client_in);
        let (engine_in, engine_out) = tokio::io::split(engine);
        let mut engine_in = BufReader::new(engine_in);
        for line in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "rkat/slow"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "mob/stop_member_run", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "rkat/quick"}),
        ] {
            client_out
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }
        // The stop reaches the engine while `rkat/slow` still runs.
        let line = next_line(&mut engine_in).await;
        assert!(line.contains("mob/stop_member_run"), "{line}");
        // The quick local call is answered first.
        let line = next_line(&mut client_in).await;
        assert!(line.contains("\"id\":3"), "{line}");
        local.gate.notify_one();
        let line = next_line(&mut client_in).await;
        assert!(line.contains("\"id\":1"), "{line}");
        drop((client_out, client_in, engine_out, engine_in));
        tokio::time::timeout(Duration::from_secs(5), proxied)
            .await
            .expect("the proxy ends with the client")
            .unwrap();
    }
}
