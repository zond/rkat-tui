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
use super::local::{self, Local};

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
    fn line(line: String) -> Self {
        Self {
            line,
            written: None,
        }
    }
}

/// The methods a daemon with decisions adds to the engine's `initialize`
/// catalog: it pushes `decision/notice` to every attached client and
/// decides `decision/decide` itself (plain TCP serves neither).
pub const DECISION_METHODS: [&str; 2] = [super::decisions::NOTICE, super::decisions::DECIDE];

/// The bound listener and the endpoint file announcing it.
#[derive(Debug)]
pub struct Listener {
    tcp: Option<TcpListener>,
    address: SocketAddr,
    file: PathBuf,
    token: Arc<str>,
}

impl Listener {
    /// Bind `listen` (loopback) and write the endpoint file at `file`.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Io`] when binding or writing fails,
    /// [`DaemonError::Token`] when no token can be drawn.
    pub async fn bind(listen: SocketAddr, file: &Path) -> Result<Self, DaemonError> {
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
    if let Err(err) = handshake::accept(&mut reader, &mut write, &token, handshake::TIMEOUT).await {
        tracing::warn!(%peer, error = %err, "refused a connection");
        return;
    }
    tracing::info!(%peer, "client attached");
    let (engine_side, proxy_side) = tokio::io::duplex(PIPE);
    let (server_read, server_write) = tokio::io::split(engine_side);
    // Aborted with this task (a shutdown aborts the connections), so the
    // client sees its socket close.
    let _proxied = AbortOnDrop(tokio::spawn(proxy(reader, write, proxy_side, local)));
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
    /// Answer the local method `method`.
    async fn call(&self, method: &str, params: Value) -> Result<Value, local::Refusal>;
    /// Learn from an engine request on its way; returns it as forwarded.
    fn observe(&self, request: Value) -> Value;
    /// A client attached: the daemon's consent surface offers it decisions
    /// through `out` until the guard is dropped. `None`: no decisions.
    fn connect(&self, _out: mpsc::UnboundedSender<Outgoing>) -> Option<ClientGuard> {
        None
    }
    /// The client `client`'s `decision/decide`: its answer line.
    async fn decide(&self, _client: u64, id: Value, _params: Value) -> String {
        format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": local::METHOD_NOT_FOUND, "message": "this daemon decides no decisions"}})
        )
    }
}

#[async_trait::async_trait]
impl LocalMethods for Local {
    async fn call(&self, method: &str, params: Value) -> Result<Value, local::Refusal> {
        Local::call(self, method, params).await
    }

    fn observe(&self, request: Value) -> Value {
        Local::observe(self, request)
    }

    fn connect(&self, out: mpsc::UnboundedSender<Outgoing>) -> Option<ClientGuard> {
        self.decisions().map(|decisions| decisions.connect(out))
    }

    async fn decide(&self, client: u64, id: Value, params: Value) -> String {
        match self.decisions() {
            Some(decisions) => decisions.decide(client, id, params).await,
            None => format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": local::METHOD_NOT_FOUND, "message": "this daemon decides no decisions"}})
            ),
        }
    }
}

/// Carry one client's lines: from the client, a local method is answered
/// here (in a task of its own, the answer going out when it is ready), a
/// `decision/decide` is decided by the daemon's consent surface (likewise),
/// and anything else goes (observed) to the engine; the engine's lines go
/// back to the client, with the decision methods added to an `initialize`
/// answer when the daemon has decisions, and so do the decision notices the
/// consent surface offers this client. Ends when the client closes (the
/// engine then sees its end of input) or the engine's side closes; a local
/// call still running then finishes (a conversation switch is not cut in
/// half), and its answer is dropped.
pub async fn proxy<R, W, E, L>(client_read: R, mut client_write: W, engine: E, local: Arc<L>)
where
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
    let client_id = decisions.as_ref().map(ClientGuard::id);
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
                // Weak: a call still running when the client leaves does
                // not keep the writer waiting for its answer.
                let out_tx = out_tx.downgrade();
                tokio::spawn(async move {
                    let line = answer(&*local, id, &method, params).await;
                    if let Some(out_tx) = out_tx.upgrade() {
                        let _ = out_tx.send(Outgoing::line(line));
                    }
                });
            }
            Routed::Decide { id, params } => {
                let local = Arc::clone(&local);
                let out_tx = out_tx.downgrade();
                let client = client_id.unwrap_or(0);
                tokio::spawn(async move {
                    let line = local.decide(client, id, params).await;
                    if let Some(out_tx) = out_tx.upgrade() {
                        let _ = out_tx.send(Outgoing::line(line));
                    }
                });
            }
            Routed::Forward(forward) => {
                if decisions.is_some()
                    && let Some(id) = initialize_id(&forward)
                {
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
    if !line.contains("\"initialize\"") {
        return None;
    }
    let request: Value = serde_json::from_str(line.trim_end()).ok()?;
    (request.get("method").and_then(Value::as_str) == Some("initialize"))
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
        id: Value,
        /// Its params (`Null` when absent).
        params: Value,
    },
    /// A local method call, to answer with [`answer`].
    Local {
        /// The request id.
        id: Value,
        /// The method.
        method: String,
        /// Its params (`Null` when absent).
        params: Value,
    },
    /// The line for the engine (newline included).
    Forward(String),
}

/// Pick out a local method call, else hand `line` on (observed).
pub fn route<L: LocalMethods + ?Sized>(line: &str, local: &L) -> Routed {
    let Ok(request) = serde_json::from_str::<Value>(line.trim_end()) else {
        // Not JSON: the engine answers it with its own parse error.
        return Routed::Forward(line.to_string());
    };
    let method = request.get("method").and_then(Value::as_str);
    match method {
        Some(super::decisions::DECIDE) => Routed::Decide {
            id: request.get("id").cloned().unwrap_or(Value::Null),
            params: request.get("params").cloned().unwrap_or(Value::Null),
        },
        Some(method) if method.starts_with(local::PREFIX) => Routed::Local {
            id: request.get("id").cloned().unwrap_or(Value::Null),
            method: method.to_string(),
            params: request.get("params").cloned().unwrap_or(Value::Null),
        },
        Some(_) => {
            let observed = local.observe(request);
            Routed::Forward(format!("{observed}\n"))
        }
        None => Routed::Forward(line.to_string()),
    }
}

/// The answer line (newline included) of the local call `method`.
pub async fn answer<L: LocalMethods + ?Sized>(
    local: &L,
    id: Value,
    method: &str,
    params: Value,
) -> String {
    let reply = match local.call(method, params).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    };
    format!("{reply}\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `rkat/slow` waits for its gate; everything else passes unchanged.
    struct Slow {
        gate: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl LocalMethods for Slow {
        async fn call(&self, method: &str, _params: Value) -> Result<Value, local::Refusal> {
            if method == "rkat/slow" {
                self.gate.notified().await;
            }
            Ok(json!({"method": method}))
        }

        fn observe(&self, request: Value) -> Value {
            request
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
        let local = Arc::new(Slow {
            gate: tokio::sync::Notify::new(),
        });
        let proxied = tokio::spawn(proxy(
            BufReader::new(client_read),
            client_write,
            engine_side,
            Arc::clone(&local),
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
