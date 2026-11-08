//! Serving the engine's method router over loopback TCP.
//!
//! The loop copies the structure of `meerkat_rpc::serve_tcp_with_options_until`
//! (which binds its own listener and so cannot report an ephemeral port, nor
//! run a handshake before the router): one task per accepted connection, a
//! shared runtime, `skip_shutdown_on_eof` so a client leaving never shuts the
//! runtime down. Each connection first passes [`super::handshake`]; only then
//! is the stream handed to `RpcServer`.
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
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use super::DaemonError;
use super::endpoint::{self, Endpoint};
use super::handshake;

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
    token: Arc<str>,
) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    if let Err(err) = handshake::accept(&mut reader, &mut write, &token, handshake::TIMEOUT).await {
        tracing::warn!(%peer, error = %err, "refused a connection");
        return;
    }
    tracing::info!(%peer, "client attached");
    let mut server = match RpcServer::new(reader, write, runtime, config_store) {
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
