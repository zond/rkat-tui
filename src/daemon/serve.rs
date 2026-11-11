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

use std::collections::{HashMap, HashSet};

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

/// The lines one client's queue holds before the client counts as not
/// reading: it is dropped (a detach, as a hangup is), and it catches up
/// from its cursor when it reconnects. A stopped TUI (Ctrl+Z) or a
/// stalled tunnel no longer makes the daemon buffer a whole event stream.
pub const CLIENT_QUEUE: usize = 4096;
/// How long the engine's next line for a client waits for room in its
/// full queue before the client is dropped (a burst the client drains in
/// time costs nothing).
pub const CLIENT_STALL: Duration = Duration::from_secs(5);

/// A client queue's bounds.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// [`CLIENT_QUEUE`].
    queue: usize,
    /// [`CLIENT_STALL`].
    stall: Duration,
}

impl Limits {
    const DEFAULT: Self = Self {
        queue: CLIENT_QUEUE,
        stall: CLIENT_STALL,
    };
}

/// The sending side of one client's outgoing queue (bounded,
/// [`CLIENT_QUEUE`]): a send that finds it full gives the client up
/// instead of waiting or growing.
#[derive(Debug, Clone)]
pub struct ClientOut {
    tx: mpsc::Sender<Outgoing>,
    overflow: Arc<tokio::sync::Notify>,
}

/// A [`ClientOut`] that does not keep the queue open.
#[derive(Debug, Clone)]
pub struct WeakClientOut {
    tx: mpsc::WeakSender<Outgoing>,
    overflow: Arc<tokio::sync::Notify>,
}

/// The queue was full (the client is being dropped) or closed (it left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotQueued;

impl ClientOut {
    /// A queue of `capacity` lines and its receiving side.
    #[must_use]
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Outgoing>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                overflow: Arc::default(),
            },
            rx,
        )
    }

    /// Queue `outgoing` without waiting (an answer, a decision notice: a
    /// client whose queue is full of the engine's lines is that far
    /// behind, and catches up after its reconnect).
    ///
    /// # Errors
    ///
    /// [`NotQueued`] when the client left, or its queue is full: then the
    /// client is given up ([`Self::overflowed`] resolves).
    pub fn send(&self, outgoing: Outgoing) -> Result<(), NotQueued> {
        match self.tx.try_send(outgoing) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.overflow.notify_one();
                Err(NotQueued)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(NotQueued),
        }
    }

    /// Queue the engine's `outgoing`, waiting up to `patience` for room.
    ///
    /// # Errors
    ///
    /// [`NotQueued`] when the client left, or no room came in time: then
    /// the client is given up ([`Self::overflowed`] resolves).
    pub async fn send_waiting(
        &self,
        outgoing: Outgoing,
        patience: Duration,
    ) -> Result<(), NotQueued> {
        match tokio::time::timeout(patience, self.tx.send(outgoing)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(NotQueued),
            Err(_) => {
                self.overflow.notify_one();
                Err(NotQueued)
            }
        }
    }

    /// Resolves once a send found the queue full.
    pub async fn overflowed(&self) {
        self.overflow.notified().await;
    }

    /// A handle that does not keep the queue open.
    #[must_use]
    pub fn downgrade(&self) -> WeakClientOut {
        WeakClientOut {
            tx: self.tx.downgrade(),
            overflow: Arc::clone(&self.overflow),
        }
    }
}

impl WeakClientOut {
    /// The queue, while it is open.
    #[must_use]
    pub fn upgrade(&self) -> Option<ClientOut> {
        Some(ClientOut {
            tx: self.tx.upgrade()?,
            overflow: Arc::clone(&self.overflow),
        })
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
    /// naming `workspace` (canonical) and the start time `started_at`.
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

    /// Remove the endpoint file, when it is still ours, and its
    /// per-workspace directory once that is empty (a later daemon creates
    /// it again).
    pub fn remove_files(&self) {
        let ours = Endpoint::read(&self.file).is_ok_and(|e| *e.token == *self.token);
        if !ours {
            return;
        }
        if let Err(err) = std::fs::remove_file(&self.file) {
            tracing::warn!(file = %self.file.display(), error = %err, "cannot remove the endpoint file");
            return;
        }
        // Fails, and is left, when anything else is in it.
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Accept until `shutdown`; then stop accepting, wait up to [`DRAIN`] for
/// the connections to end, and return those still running (the caller
/// aborts them once the runtime has shut down). A failed accept (out of
/// file descriptors, a connection aborted before it was accepted) ends
/// nothing: it is logged and accepting goes on after [`ACCEPT_BACKOFF`].
pub async fn serve(
    listener: &mut Listener,
    runtime: &Arc<SessionRuntime>,
    config_store: &Arc<dyn ConfigStore>,
    local: &Arc<Local>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> JoinSet<()> {
    let mut connections = JoinSet::new();
    let Some(tcp) = listener.tcp.take() else {
        return connections;
    };
    let token = Arc::clone(&listener.token);
    accept_until(
        || tcp.accept(),
        shutdown,
        &mut connections,
        |stream: tokio::net::TcpStream, peer| {
            let _ = stream.set_nodelay(true);
            connection(
                stream,
                peer,
                Arc::clone(runtime),
                Arc::clone(config_store),
                Arc::clone(local),
                Arc::clone(&token),
            )
        },
    )
    .await;
    drop(tcp);
    let _ = tokio::time::timeout(DRAIN, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections
}

/// The wait after a failed accept before accepting again.
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Accept with `accept` until `shutdown`, serving each connection with
/// `serve_one` in `connections` (reaped as they end); a failed accept is
/// logged and accepting goes on after [`ACCEPT_BACKOFF`].
async fn accept_until<S, A, AF, F, FF>(
    mut accept: A,
    shutdown: impl std::future::Future<Output = ()>,
    connections: &mut JoinSet<()>,
    mut serve_one: F,
) where
    A: FnMut() -> AF,
    AF: std::future::Future<Output = std::io::Result<(S, SocketAddr)>>,
    F: FnMut(S, SocketAddr) -> FF,
    FF: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            accepted = accept() => match accepted {
                Ok((stream, peer)) => {
                    connections.spawn(serve_one(stream, peer));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "accept failed; accepting again shortly");
                    tokio::select! {
                        () = &mut shutdown => return,
                        () = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                    }
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
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
    let deferral = Arc::clone(local.deferral());
    let _proxied = AbortOnDrop(tokio::spawn(proxy(
        reader, write, proxy_side, local, caller,
    )));
    // Its router installs the engine's default mob tools factory; the
    // agents' is installed again before any agent is built.
    let made = deferral
        .with_router(&Arc::clone(&runtime), move || {
            RpcServer::new(
                BufReader::new(server_read),
                server_write,
                runtime,
                config_store,
            )
        })
        .await;
    let mut server = match made {
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
/// to none, any other connection must name its choice; a new agent is
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
        let bind = match pending.params.agent.clone() {
            Some(choice) => agents.bind(&choice).await,
            None => Err(super::agents::BindError::Unnamed),
        };
        match bind {
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
    let session = bound
        .as_ref()
        .zip(agents)
        .and_then(|(bound, agents)| agents.session_of(&bound.name));
    match handshake::acknowledge(
        writer,
        &pending,
        bound.as_ref(),
        session.as_ref(),
        Some(lifecycle.build()),
        handshake::TIMEOUT,
    )
    .await
    {
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
    /// Learn from `caller`'s engine request on its way; returns it as
    /// forwarded, or the refusal to answer instead (a request acting on
    /// another agent than the caller's).
    ///
    /// # Errors
    ///
    /// The refusal, answered to the client; the request is not forwarded.
    fn observe(&self, caller: &Caller, request: Value) -> Result<Value, Refusal>;
    /// A client attached: the daemon's consent surface offers it decisions
    /// through `out` until the guard is dropped.
    fn connect(&self, out: ClientOut) -> ClientGuard;
    /// The client `client`'s `decision/decide`: its answer line.
    async fn decide(&self, client: u64, id: Option<RpcId>, params: Value) -> String;
    /// The mobs destroyed from now on: the proxy closes the client's
    /// mob-wide streams on each (`None`: none are reported).
    fn destroyed_mobs(&self) -> Option<tokio::sync::broadcast::Receiver<meerkat_mob::MobId>> {
        None
    }
}

#[async_trait::async_trait]
impl LocalMethods for Local {
    async fn call(&self, caller: Caller, method: &str, params: Value) -> Result<Value, Refusal> {
        Local::call(self, caller, method, params).await
    }

    fn observe(&self, caller: &Caller, request: Value) -> Result<Value, Refusal> {
        Local::observe(self, caller, request)
    }

    fn connect(&self, out: ClientOut) -> ClientGuard {
        self.decisions().connect(out)
    }

    async fn decide(&self, client: u64, id: Option<RpcId>, params: Value) -> String {
        self.decisions().decide(client, id, params).await
    }

    fn destroyed_mobs(&self) -> Option<tokio::sync::broadcast::Receiver<meerkat_mob::MobId>> {
        Some(self.registry().destroyed_mobs())
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
/// half), and its answer is dropped. Ends too when the client's queue
/// fills ([`CLIENT_QUEUE`]): a client that does not read is dropped.
/// `caller` is what the client proved at attach (the lifecycle methods
/// answer for it).
pub async fn proxy<R, W, E, L>(
    client_read: R,
    client_write: W,
    engine: E,
    local: Arc<L>,
    caller: Caller,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    E: tokio::io::AsyncRead + AsyncWrite + Unpin + Send + 'static,
    L: LocalMethods + ?Sized + 'static,
{
    proxy_with(
        client_read,
        client_write,
        engine,
        local,
        caller,
        Limits::DEFAULT,
    )
    .await;
}

/// [`proxy`] with the client queue's `limits`.
async fn proxy_with<R, W, E, L>(
    client_read: R,
    mut client_write: W,
    engine: E,
    local: Arc<L>,
    caller: Caller,
    limits: Limits,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    E: tokio::io::AsyncRead + AsyncWrite + Unpin + Send + 'static,
    L: LocalMethods + ?Sized + 'static,
{
    let (engine_read, mut engine_write) = tokio::io::split(engine);
    let (out_tx, mut out_rx) = ClientOut::channel(limits.queue);
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
    // The client's mob-wide streams, closed when their mob is destroyed.
    let streams: Arc<parking_lot::Mutex<MobStreams>> = Arc::default();
    let mut destroyed = local.destroyed_mobs();
    let mut from_engine = AbortOnDrop(tokio::spawn({
        let out_tx = out_tx.clone();
        let initializing = Arc::clone(&initializing);
        let streams = Arc::clone(&streams);
        async move {
            let mut lines = BufReader::new(engine_read);
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let mut forward = std::mem::take(&mut line);
                        if !streams.lock().answered(&forward) {
                            // The answer to a close of the daemon's own.
                            continue;
                        }
                        if !initializing.lock().is_empty() {
                            forward = with_decision_methods(forward, &initializing);
                        }
                        if out_tx
                            .send_waiting(Outgoing::line(forward), limits.stall)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    }));
    // `next_line` may lose nothing when another branch wins (a destroyed
    // mob's streams are closed between two client lines).
    let mut lines = client_read.lines();
    let mut overflowed = false;
    loop {
        let line = tokio::select! {
            read = lines.next_line() => match read {
                Ok(Some(line)) => line,
                Ok(None) | Err(_) => break,
            },
            mob = next_destroyed(&mut destroyed) => {
                let closes = streams.lock().close(mob.as_str());
                for close in closes {
                    if engine_write.write_all(close.as_bytes()).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            () = out_tx.overflowed() => {
                tracing::warn!(
                    agent = ?caller.agent,
                    queued = limits.queue,
                    "dropped a client that does not read what the daemon sends (it reconnects and catches up)"
                );
                overflowed = true;
                break;
            }
        };
        match route(&line, &*local, &caller) {
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
            Routed::Denied { id, refusal } => {
                tracing::warn!(message = %refusal.message, "refused an engine request for another agent");
                if id.is_some() {
                    let _ = out_tx.send(Outgoing::line(protocol::error_line(
                        id,
                        refusal.code,
                        &refusal.message,
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
                streams.lock().opening(&forward);
                if engine_write.write_all(forward.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
    drop(decisions);
    let _ = engine_write.shutdown().await;
    drop(out_tx);
    if overflowed {
        // The writer is stuck on a client that does not read: abort it
        // (dropped here) rather than wait for what it still holds.
        return;
    }
    let _ = (&mut from_engine.0).await;
    let _ = (&mut writer.0).await;
}

/// The next mob `destroyed` reports; never, without a report.
async fn next_destroyed(
    destroyed: &mut Option<tokio::sync::broadcast::Receiver<meerkat_mob::MobId>>,
) -> meerkat_mob::MobId {
    loop {
        let Some(receiver) = destroyed.as_mut() else {
            return std::future::pending().await;
        };
        match receiver.recv().await {
            Ok(mob) => return mob,
            // A report missed: the next one still closes what is open.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => *destroyed = None,
        }
    }
}

/// One client's mob-wide streams (`mob/stream_open` with a `mob_id` and no
/// `agent_identity`), by mob: the engine's subscription behind an open one
/// is an owner of the mob's database, so a destroy's storage cleanup is
/// refused while the client keeps it open (it closes a stream only when
/// the stream ends, which the engine waits with until the cleanup is
/// done). When the mob is destroyed the proxy closes them on the client's
/// behalf; the engine then ends each with `mob/stream_end`, as for a close
/// of the client's own.
#[derive(Debug, Default)]
struct MobStreams {
    /// Open requests not answered yet: request id (as JSON text) → mob.
    pending: HashMap<String, String>,
    /// Open streams: mob → stream ids.
    open: HashMap<String, Vec<String>>,
    /// The ids of the daemon's own close requests, whose answers are not
    /// the client's.
    closing: HashSet<String>,
    /// Numbers the daemon's own requests.
    next: u64,
}

impl MobStreams {
    /// The client sends `line`: note a mob-wide stream open.
    fn opening(&mut self, line: &str) {
        if !line.contains(protocol::engine::STREAM_OPEN) {
            return;
        }
        let Ok(request) = serde_json::from_str::<Value>(line.trim_end()) else {
            return;
        };
        let params = request.get("params");
        if request.get("method").and_then(Value::as_str) != Some(protocol::engine::STREAM_OPEN)
            || params.and_then(|p| p.get("agent_identity")).is_some()
        {
            return;
        }
        if let (Some(id), Some(mob)) = (
            request.get("id").map(Value::to_string),
            params.and_then(|p| p.get("mob_id")).and_then(Value::as_str),
        ) {
            self.pending.insert(id, mob.to_string());
        }
    }

    /// The engine sends `line`: note the stream an open answered with.
    /// `false`: the line answers a close of the daemon's own and does not
    /// go to the client.
    fn answered(&mut self, line: &str) -> bool {
        if self.pending.is_empty() && self.closing.is_empty() {
            return true;
        }
        let Ok(answer) = serde_json::from_str::<Value>(line.trim_end()) else {
            return true;
        };
        if answer.get("method").is_some() {
            return true;
        }
        let Some(id) = answer.get("id").map(Value::to_string) else {
            return true;
        };
        if self.closing.remove(&id) {
            return false;
        }
        if let Some(mob) = self.pending.remove(&id)
            && let Some(stream) = answer.pointer("/result/stream_id").and_then(Value::as_str)
        {
            self.open.entry(mob).or_default().push(stream.to_string());
        }
        true
    }

    /// `mob` was destroyed: the close requests for its open streams.
    fn close(&mut self, mob: &str) -> Vec<String> {
        let streams = self.open.remove(mob).unwrap_or_default();
        streams
            .into_iter()
            .map(|stream| {
                self.next += 1;
                let id = format!("rkat-close-{}", self.next);
                self.closing.insert(Value::String(id.clone()).to_string());
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": protocol::engine::STREAM_CLOSE,
                        "params": {"stream_id": stream},
                    })
                )
            })
            .collect()
    }
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
    /// A served engine request the caller may not send (see
    /// [`LocalMethods::observe`]): answered with the refusal (a request)
    /// or dropped (a notification), never forwarded.
    Denied {
        /// The request id (`None`: a notification).
        id: Option<RpcId>,
        /// Why.
        refusal: Refusal,
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
pub fn route<L: LocalMethods + ?Sized>(line: &str, local: &L, caller: &Caller) -> Routed {
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
            let id = id();
            match local.observe(caller, request) {
                Ok(observed) => Routed::Forward(format!("{observed}\n")),
                Err(refusal) => Routed::Denied { id, refusal },
            }
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

    /// A destroyed mob's open mob-wide streams are closed, once, and the
    /// answers to those closes are the daemon's; a member stream and
    /// another mob's are left alone.
    #[test]
    fn a_destroyed_mobs_streams_are_closed_for_the_client() {
        let mut streams = MobStreams::default();
        streams.opening(r#"{"jsonrpc":"2.0","id":1,"method":"mob/stream_open","params":{"mob_id":"council--x"}}"#);
        streams.opening(r#"{"jsonrpc":"2.0","id":2,"method":"mob/stream_open","params":{"mob_id":"council--x","agent_identity":"p0"}}"#);
        streams.opening(
            r#"{"jsonrpc":"2.0","id":3,"method":"mob/stream_open","params":{"mob_id":"tui"}}"#,
        );
        for answer in [
            r#"{"jsonrpc":"2.0","id":1,"result":{"stream_id":"s1"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"stream_id":"s2"}}"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"stream_id":"s3"}}"#,
        ] {
            assert!(streams.answered(answer));
        }
        let closes = streams.close("council--x");
        assert_eq!(closes.len(), 1, "{closes:?}");
        let close: Value = serde_json::from_str(closes[0].trim_end()).unwrap();
        assert_eq!(close["method"], protocol::engine::STREAM_CLOSE);
        assert_eq!(close["params"]["stream_id"], "s1");
        // Its answer stays with the daemon; the engine's stream end goes on.
        let answer = json!({"jsonrpc": "2.0", "id": close["id"], "result": {"closed": true}});
        assert!(!streams.answered(&answer.to_string()));
        assert!(streams.answered(
            r#"{"jsonrpc":"2.0","method":"mob/stream_end","params":{"stream_id":"s1"}}"#
        ));
        // Once: the hook reports the destroy again before each retry.
        assert!(streams.close("council--x").is_empty());
        assert_eq!(streams.close("tui").len(), 1);
    }

    /// The daemon's side of a connection: writes go to the client, and the
    /// acknowledgement's write records the client count the lifecycle had
    /// then (and fails, with `fail`).
    struct Recording {
        inner: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        lifecycle: Arc<super::super::lifecycle::Lifecycle>,
        seen: Option<usize>,
        fail: bool,
    }

    impl tokio::io::AsyncWrite for Recording {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if String::from_utf8_lossy(buf).contains("\"attached\"") {
                self.seen = Some(self.lifecycle.clients());
                if self.fail {
                    return std::task::Poll::Ready(Err(std::io::Error::from(
                        std::io::ErrorKind::BrokenPipe,
                    )));
                }
            }
            std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Admit one client holding the token `t` and presenting the start
    /// token `start` through the mutual handshake, the daemon writing
    /// through a [`Recording`] (`fail`: its acknowledgement fails): the
    /// admission and the client count the acknowledgement saw.
    async fn admit_one(
        lifecycle: &Arc<super::super::lifecycle::Lifecycle>,
        fail: bool,
    ) -> (
        Result<super::super::lifecycle::Seat, handshake::HandshakeError>,
        Option<usize>,
    ) {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let client_side = tokio::spawn(async move {
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = tokio::io::BufReader::new(cr);
            let nonce = "0123456789abcdef0123456789abcdef";
            let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "rkat/attach",
                "params": {"nonce": nonce, "start_token": "start", "agent": {"choice": "recent"}}});
            cw.write_all(format!("{hello}\n").as_bytes()).await.unwrap();
            let mut line = String::new();
            cr.read_line(&mut line).await.unwrap();
            let challenge: Value = serde_json::from_str(&line).unwrap();
            let daemon_nonce = challenge["result"]["challenge"]["nonce"]
                .as_str()
                .unwrap()
                .to_string();
            let proof = json!({"jsonrpc": "2.0", "id": 2, "method": "rkat/attach",
                "params": {"proof": handshake::client_proof("t", nonce, &daemon_nonce).unwrap()}});
            cw.write_all(format!("{proof}\n").as_bytes()).await.unwrap();
            let mut ack = String::new();
            let _ = cr.read_line(&mut ack).await;
        });
        let mut reader = tokio::io::BufReader::new(sr);
        let mut writer = Recording {
            inner: sw,
            lifecycle: Arc::clone(lifecycle),
            seen: None,
            fail,
        };
        let admitted = admit(&mut reader, &mut writer, "t", lifecycle, None).await;
        let seen = writer.seen;
        drop(writer);
        drop(reader);
        let _ = client_side.await;
        (admitted, seen)
    }

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
        let (admitted, seen) = admit_one(&lifecycle, false).await;
        let seat = admitted.unwrap();
        assert_eq!(seen, Some(1), "counted when the answer was written");
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
        let (refused, seen) = admit_one(&lifecycle, true).await;
        assert!(refused.is_err());
        assert_eq!(seen, Some(1), "the seat was taken before the write");
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

        fn observe(&self, _caller: &Caller, request: Value) -> Result<Value, Refusal> {
            Ok(request)
        }

        fn connect(&self, out: ClientOut) -> ClientGuard {
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

    fn slow_local() -> Arc<Slow> {
        let service = meerkat_runtime::decision::DurableDecisionService::new(Arc::new(
            meerkat_core::approval::decision::InMemoryDecisionStore::new(),
        ));
        let principal = super::super::decisions::Principal::new("alice").unwrap();
        Arc::new(Slow {
            gate: tokio::sync::Notify::new(),
            decisions: super::super::decisions::Decisions::new(service, principal, None).unwrap(),
        })
    }

    /// Write `n` engine lines on `engine` (errors ignored: the proxy may
    /// be gone).
    fn engine_lines(engine: tokio::io::DuplexStream, n: usize) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (_read, mut write) = tokio::io::split(engine);
            for i in 0..n {
                let line = format!(
                    "{}\n",
                    json!({"jsonrpc": "2.0", "method": "mob/stream_event", "params": {"n": i}})
                );
                if write.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
            // Keep the engine's side open: the proxy ends by itself.
            std::future::pending::<()>().await;
        })
    }

    /// The finding this guards: the engine's
    /// lines for a client went into an unbounded queue, so a client that
    /// stopped reading (a suspended TUI, a stalled tunnel) made the daemon
    /// buffer its whole event stream.
    #[tokio::test]
    async fn a_client_that_does_not_read_is_dropped_and_another_keeps_streaming() {
        let limits = Limits {
            queue: 8,
            stall: Duration::from_millis(200),
        };
        const LINES: usize = 500;
        // A client that never reads, behind a tiny pipe.
        let (stalled_client, stalled_side) = tokio::io::duplex(64);
        let (stalled_engine, stalled_engine_side) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(stalled_side);
        let stalled = tokio::spawn(proxy_with(
            BufReader::new(read),
            write,
            stalled_engine_side,
            slow_local(),
            Caller::default(),
            limits,
        ));
        // A client that reads.
        let (healthy_client, healthy_side) = tokio::io::duplex(1 << 16);
        let (healthy_engine, healthy_engine_side) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(healthy_side);
        let _healthy = AbortOnDrop(tokio::spawn(proxy_with(
            BufReader::new(read),
            write,
            healthy_engine_side,
            slow_local(),
            Caller::default(),
            limits,
        )));
        let _stalled_writes = AbortOnDrop(engine_lines(stalled_engine, LINES));
        let _healthy_writes = AbortOnDrop(engine_lines(healthy_engine, LINES));
        tokio::time::timeout(Duration::from_secs(5), stalled)
            .await
            .expect("the client that does not read is dropped")
            .unwrap();
        let (client_in, _client_out) = tokio::io::split(healthy_client);
        let mut client_in = BufReader::new(client_in);
        for i in 0..LINES {
            let line = next_line(&mut client_in).await;
            assert!(line.contains(&format!("\"n\":{i}}}")), "{i}: {line}");
        }
        drop(stalled_client);
    }

    /// The finding this guards: one failed
    /// `accept` (out of file descriptors, a connection aborted before it
    /// was accepted) ended the serve loop and with it the daemon.
    #[tokio::test]
    async fn a_failed_accept_does_not_end_accepting() {
        let peer = SocketAddr::from(([127, 0, 0, 1], 1));
        // Taken when an accept is polled, as a listener's accept is
        // cancel-safe: one dropped unpolled takes nothing.
        let script: Arc<parking_lot::Mutex<std::collections::VecDeque<std::io::Result<u32>>>> =
            Arc::new(parking_lot::Mutex::new(
                [
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted)),
                    Ok(1),
                    Err(std::io::Error::other("too many open files")),
                    Ok(2),
                ]
                .into(),
            ));
        let served = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let accepting = tokio::spawn({
            let served = Arc::clone(&served);
            async move {
                let mut connections = JoinSet::new();
                accept_until(
                    move || {
                        let script = Arc::clone(&script);
                        async move {
                            let next = script.lock().pop_front();
                            match next {
                                Some(result) => result.map(|n| (n, peer)),
                                None => std::future::pending().await,
                            }
                        }
                    },
                    async {
                        let _ = stopped.await;
                    },
                    &mut connections,
                    |n, _| {
                        let served = Arc::clone(&served);
                        async move { served.lock().push(n) }
                    },
                )
                .await;
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while served.lock().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both connections after the failures are served");
        assert_eq!(*served.lock(), vec![1, 2]);
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), accepting)
            .await
            .expect("the shutdown ends accepting")
            .unwrap();
    }
}
