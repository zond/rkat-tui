//! Serving the engine's method router over loopback TCP.
//!
//! The loop copies the structure of `meerkat_rpc::serve_tcp_with_options_until`
//! (which binds its own listener and so cannot report an ephemeral port, nor
//! run a handshake before the router): one task per accepted connection, a
//! shared runtime, `skip_shutdown_on_eof` so a client leaving never shuts the
//! runtime down. Each connection first passes [`super::handshake`]; only then
//! is the stream handed to `RpcServer`, through [`proxy`]: the daemon's own
//! `rkat/*` methods ([`super::local`]) are answered there, every other line
//! goes to the engine (after [`super::local::Local::observe`] read it).
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

use super::DaemonError;
use super::endpoint::{self, Endpoint};
use super::handshake;
use super::local::{self, Local};

/// The in-memory pipe between the proxy and the engine's server.
const PIPE: usize = 1 << 20;

/// How long connections get to end on their own at shutdown.
pub const DRAIN: Duration = Duration::from_secs(2);

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

/// Carry one client's lines: from the client, a local method is answered
/// here and anything else goes (observed) to the engine; the engine's lines
/// go back to the client. Ends when the client closes (the engine then sees
/// its end of input) or the engine's side closes.
pub async fn proxy<R, W, E>(client_read: R, mut client_write: W, engine: E, local: Arc<Local>)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    E: tokio::io::AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (engine_read, mut engine_write) = tokio::io::split(engine);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if client_write.write_all(line.as_bytes()).await.is_err()
                || client_write.flush().await.is_err()
            {
                break;
            }
        }
        let _ = client_write.shutdown().await;
    }));
    let mut from_engine = AbortOnDrop(tokio::spawn({
        let out_tx = out_tx.clone();
        async move {
            let mut lines = BufReader::new(engine_read);
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if out_tx.send(std::mem::take(&mut line)).is_err() {
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
        match route(&line, &local).await {
            Routed::Answer(answer) => {
                let _ = out_tx.send(answer);
            }
            Routed::Forward(forward) => {
                if engine_write.write_all(forward.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = engine_write.shutdown().await;
    drop(out_tx);
    let _ = (&mut from_engine.0).await;
    let _ = (&mut writer.0).await;
}

/// Where one client line goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Routed {
    /// The answer to a local method (a line, newline included).
    Answer(String),
    /// The line for the engine (newline included).
    Forward(String),
}

/// Answer `line` when it calls a local method, else hand it on (observed).
pub async fn route(line: &str, local: &Local) -> Routed {
    let Ok(request) = serde_json::from_str::<Value>(line.trim_end()) else {
        // Not JSON: the engine answers it with its own parse error.
        return Routed::Forward(line.to_string());
    };
    let method = request.get("method").and_then(Value::as_str);
    match method {
        Some(method) if method.starts_with(local::PREFIX) => {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            let reply = match local.call(method, params).await {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": code, "message": message},
                }),
            };
            Routed::Answer(format!("{reply}\n"))
        }
        Some(_) => {
            let observed = local.observe(request);
            Routed::Forward(format!("{observed}\n"))
        }
        None => Routed::Forward(line.to_string()),
    }
}
