//! The client's side of `/mcp login`: the loopback redirect the login
//! names, the one-shot callback listener on this machine, and the
//! redirected address the user pastes when the browser is elsewhere.
//!
//! The daemon owns the engine's login (`rkat/mcp_login_start`,
//! `rkat/mcp_login_complete`); the client owns where the browser comes
//! back to. The redirect is `mcp.oauth_redirect_uri` when set (a wrapper
//! may predefine one an authorization server has registered), else
//! `http://127.0.0.1:<a free port>/callback`. No browser and no process is
//! started: the dialog shows the authorize URL, and either the browser's
//! redirect reaches the listener here, or (the browser on another machine,
//! its redirect a connection error) the user pastes the address it shows.
//!
//! The listener binds the loopback interface only (both `127.0.0.1` and
//! `[::1]` for `localhost`, which a browser may resolve either way),
//! answers one GET on the redirect's path whose `state` is the attempt's
//! (any other request is answered and ignored, nothing of it forwarded),
//! and closes then, at the attempt's deadline or on cancel. A pasted
//! address is taken only when its scheme, host, port and path are the
//! redirect's and its `state` the attempt's. The code, the state and the
//! authorize URL are never logged.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The path of the default redirect.
pub const DEFAULT_PATH: &str = "/callback";

/// The setting that predefines the redirect.
pub const SETTING: &str = "mcp.oauth_redirect_uri";

/// The most bytes of a callback request's head read.
const MAX_HEAD: usize = 8 * 1024;

/// How long one connection may take to send its request.
const READ_BOUND: Duration = Duration::from_secs(5);

/// The loopback host a redirect names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopbackHost {
    /// `127.0.0.1`.
    V4,
    /// `[::1]`.
    V6,
    /// `localhost`: both.
    Localhost,
}

impl LoopbackHost {
    fn spelling(self) -> &'static str {
        match self {
            Self::V4 => "127.0.0.1",
            Self::V6 => "[::1]",
            Self::Localhost => "localhost",
        }
    }
}

/// A loopback redirect URI: `http`, a loopback host as the engine accepts
/// it, a port (`0` only for the default: a free one is picked), a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackRedirect {
    pub host: LoopbackHost,
    /// `0`: pick a free port at bind.
    pub port: u16,
    pub path: String,
}

impl LoopbackRedirect {
    /// The default: `http://127.0.0.1:<a free port>/callback`.
    #[must_use]
    pub fn default_random() -> Self {
        Self {
            host: LoopbackHost::V4,
            port: 0,
            path: DEFAULT_PATH.to_string(),
        }
    }

    /// The redirect `mcp.oauth_redirect_uri` holds, else the default.
    ///
    /// # Errors
    ///
    /// Why the setting's value is no loopback redirect, naming it.
    pub fn from_setting(value: Option<&str>) -> Result<Self, String> {
        match value {
            Some(text) => Self::parse(text).map_err(|why| format!("{SETTING}: {why}")),
            None => Ok(Self::default_random()),
        }
    }

    /// Read `text` as a predefined redirect.
    ///
    /// # Errors
    ///
    /// What is wrong with it, in words.
    pub fn parse(text: &str) -> Result<Self, String> {
        let url = Url::parse(text.trim()).map_err(|err| format!("`{text}` is no URI: {err}"))?;
        if url.scheme() != "http" {
            return Err(format!(
                "`{text}` must be an http:// URI (a loopback redirect is plain http)"
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!("`{text}` must not name a user"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(format!("`{text}` must have no query and no fragment"));
        }
        let host = match url.host_str().map(str::to_ascii_lowercase).as_deref() {
            Some("127.0.0.1") => LoopbackHost::V4,
            Some("[::1]") => LoopbackHost::V6,
            Some("localhost") => LoopbackHost::Localhost,
            _ => {
                return Err(format!(
                    "`{text}` must name a loopback host: 127.0.0.1, localhost or [::1]"
                ));
            }
        };
        let port = match url.port() {
            Some(port) if port != 0 => port,
            _ => {
                return Err(format!(
                    "`{text}` must name its port (http://localhost:8976/callback)"
                ));
            }
        };
        Ok(Self {
            host,
            port,
            path: url.path().to_string(),
        })
    }

    /// The URI with `port` (the one bound).
    #[must_use]
    pub fn uri(&self, port: u16) -> String {
        format!("http://{}:{port}{}", self.host.spelling(), self.path)
    }

    /// Whether the port was predefined (not picked at bind).
    #[must_use]
    pub fn fixed(&self) -> bool {
        self.port != 0
    }
}

/// Why the listener could not be bound.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// A predefined port is taken.
    #[error(
        "port {port} of {SETTING} is in use on this machine; the browser's redirect cannot reach rkat-tui, so paste the address it shows (or free the port, or set another)"
    )]
    InUse { port: u16 },
    #[error("cannot listen on {address}: {source}; paste the address the browser shows")]
    Io {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// The bound one-shot listener of one login attempt.
#[derive(Debug)]
pub struct CallbackListener {
    listeners: Vec<TcpListener>,
    /// The exact redirect URI (its port the bound one).
    pub redirect_uri: String,
    path: String,
}

impl CallbackListener {
    /// Bind `redirect` on the loopback interface.
    ///
    /// # Errors
    ///
    /// [`BindError::InUse`] for a predefined port that is taken, else
    /// [`BindError::Io`].
    pub async fn bind(redirect: &LoopbackRedirect) -> Result<Self, BindError> {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, redirect.port));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, redirect.port));
        let failed = |address: SocketAddr, source: std::io::Error| {
            if source.kind() == std::io::ErrorKind::AddrInUse && redirect.fixed() {
                BindError::InUse {
                    port: redirect.port,
                }
            } else {
                BindError::Io { address, source }
            }
        };
        let mut listeners = Vec::new();
        let port = match redirect.host {
            LoopbackHost::V4 | LoopbackHost::Localhost => {
                let listener = TcpListener::bind(v4).await.map_err(|err| failed(v4, err))?;
                let port = listener.local_addr().map_err(|err| failed(v4, err))?.port();
                listeners.push(listener);
                if redirect.host == LoopbackHost::Localhost {
                    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
                    match TcpListener::bind(v6).await {
                        Ok(listener) => listeners.push(listener),
                        // Taken on [::1]: a browser resolving localhost
                        // there would reach another program.
                        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                            return Err(failed(v6, err));
                        }
                        // No IPv6 loopback here: no browser reaches it either.
                        Err(err) => {
                            tracing::debug!(error = %err, "no [::1] listener for the MCP login");
                        }
                    }
                }
                port
            }
            LoopbackHost::V6 => {
                let listener = TcpListener::bind(v6).await.map_err(|err| failed(v6, err))?;
                let port = listener.local_addr().map_err(|err| failed(v6, err))?.port();
                listeners.push(listener);
                port
            }
        };
        Ok(Self {
            listeners,
            redirect_uri: redirect.uri(port),
            path: redirect.path.clone(),
        })
    }

    /// The addresses it listens on.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.listeners
            .iter()
            .filter_map(|listener| listener.local_addr().ok())
            .collect()
    }

    /// Wait for the one callback of the attempt whose state is `state`:
    /// its code, or what the authorization server answered instead. Every
    /// other request is answered and ignored. Runs until one comes; the
    /// caller bounds it (the deadline, a cancel) and drops it, which closes
    /// the listener.
    pub async fn wait(self, state: &str) -> Result<String, Redirected> {
        loop {
            let accepted = accept_any(&self.listeners).await;
            let Ok(stream) = accepted else {
                continue;
            };
            if let Ok(Some(outcome)) =
                tokio::time::timeout(READ_BOUND, self.serve(stream, state)).await
            {
                return outcome;
            }
        }
    }

    /// One connection: `Some` when it was the attempt's callback.
    async fn serve(
        &self,
        mut stream: TcpStream,
        state: &str,
    ) -> Option<Result<String, Redirected>> {
        let target = read_target(&mut stream).await?;
        let Ok(url) = Url::parse(&format!("http://loopback{target}")) else {
            let _ = answer(&mut stream, 400, "This is not an rkat-tui login address.").await;
            return None;
        };
        if url.path() != self.path {
            let _ = answer(&mut stream, 404, "This is not an rkat-tui login address.").await;
            return None;
        }
        match redirected(&url, state) {
            Err(Redirected::StateMismatch) => {
                let _ = answer(
                    &mut stream,
                    400,
                    "This address does not belong to the login rkat-tui is waiting for.",
                )
                .await;
                None
            }
            Err(Redirected::NoCode) => {
                let _ = answer(&mut stream, 400, "This address carries no login code.").await;
                None
            }
            Ok(code) => {
                let _ = answer(
                    &mut stream,
                    200,
                    "Login received. You can close this tab and return to rkat-tui.",
                )
                .await;
                Some(Ok(code))
            }
            Err(refused) => {
                let _ = answer(
                    &mut stream,
                    200,
                    "The authorization server refused the login. rkat-tui shows why.",
                )
                .await;
                Some(Err(refused))
            }
        }
    }
}

/// Accept on whichever listener has a connection first.
async fn accept_any(listeners: &[TcpListener]) -> std::io::Result<TcpStream> {
    let accepts = listeners
        .iter()
        .map(|listener| Box::pin(async move { listener.accept().await.map(|(s, _)| s) }));
    let (stream, _, _) = futures::future::select_all(accepts).await;
    stream
}

/// The request target of a `GET` (`/callback?code=…`), or `None` for
/// anything else (the connection is then answered and dropped).
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || head.len() + read > MAX_HEAD {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&head);
    let mut line = text.lines().next()?.split(' ');
    let method = line.next()?;
    let target = line.next()?;
    if method != "GET" {
        let _ = answer(stream, 405, "Only the browser's redirect is answered here.").await;
        return None;
    }
    target.starts_with('/').then(|| target.to_string())
}

/// A short plain HTML page with `status`.
async fn answer(stream: &mut TcpStream, status: u16, text: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>rkat-tui</title></head><body><p>{text}</p></body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

/// What a redirect (or a pasted address) did not carry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Redirected {
    /// Its `state` is not the attempt's (another login's, or forged).
    #[error("the address belongs to another login (its state is not this one's)")]
    StateMismatch,
    /// No `code`.
    #[error("the address carries no login code")]
    NoCode,
    /// The authorization server answered an error.
    #[error("the authorization server refused the login: {0}")]
    Refused(String),
    /// A pasted address is not the redirect's.
    #[error(
        "that is not the address the login redirects to ({0}); paste the whole address from the browser's address bar"
    )]
    NotTheRedirect(String),
}

/// The code `url` (on the redirect's path) carries for the attempt whose
/// state is `state`. The state is compared before anything else is read.
fn redirected(url: &Url, state: &str) -> Result<String, Redirected> {
    let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let get = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if get("state") != Some(state) {
        return Err(Redirected::StateMismatch);
    }
    if let Some(error) = get("error") {
        let detail = get("error_description").unwrap_or_default();
        let text = if detail.is_empty() {
            error.to_string()
        } else {
            format!("{error}: {detail}")
        };
        return Err(Redirected::Refused(crate::text::truncate_chars(
            &crate::text::one_line(&text),
            200,
        )));
    }
    match get("code") {
        Some(code) if !code.is_empty() => Ok(code.to_string()),
        _ => Err(Redirected::NoCode),
    }
}

/// The code of the address the user pasted, checked against the pending
/// `redirect_uri` (scheme, host, port and path) and `state`.
///
/// # Errors
///
/// [`Redirected`] saying why it is not the attempt's.
pub fn pasted(text: &str, redirect_uri: &str, state: &str) -> Result<String, Redirected> {
    let not_it = || Redirected::NotTheRedirect(redirect_uri.to_string());
    let expected = Url::parse(redirect_uri).map_err(|_| not_it())?;
    let url = Url::parse(text.trim()).map_err(|_| not_it())?;
    let host_of = |url: &Url| url.host_str().map(str::to_ascii_lowercase);
    if url.scheme() != expected.scheme()
        || host_of(&url) != host_of(&expected)
        || url.port_or_known_default() != expected.port_or_known_default()
        || url.path() != expected.path()
    {
        return Err(not_it());
    }
    redirected(&url, state)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every spelling the engine accepts reads, with its port and path;
    /// anything else is refused saying why.
    #[test]
    fn a_predefined_redirect_is_an_http_loopback_uri_with_port_and_path() {
        for (text, host, port, path) in [
            (
                "http://127.0.0.1:8976/callback",
                LoopbackHost::V4,
                8976,
                "/callback",
            ),
            (
                "http://localhost:8976/oauth/cb",
                LoopbackHost::Localhost,
                8976,
                "/oauth/cb",
            ),
            ("http://LOCALHOST:8080/", LoopbackHost::Localhost, 8080, "/"),
            ("http://[::1]:9000/done", LoopbackHost::V6, 9000, "/done"),
        ] {
            let redirect = LoopbackRedirect::parse(text).unwrap();
            assert_eq!(
                (redirect.host, redirect.port, redirect.path.as_str()),
                (host, port, path)
            );
            assert!(redirect.fixed());
        }
        assert_eq!(
            LoopbackRedirect::parse("http://localhost:8976/callback")
                .unwrap()
                .uri(8976),
            "http://localhost:8976/callback"
        );
        for (text, why) in [
            ("https://127.0.0.1:1/cb", "http://"),
            ("http://example.com:1/cb", "loopback host"),
            ("http://127.0.0.2:1/cb", "loopback host"),
            ("http://127.0.0.1/cb", "port"),
            ("http://127.0.0.1:0/cb", "port"),
            ("http://127.0.0.1:1/cb?x=1", "query"),
            ("http://u@127.0.0.1:1/cb", "user"),
            ("not a uri", "no URI"),
        ] {
            let refused = LoopbackRedirect::parse(text).unwrap_err();
            assert!(refused.contains(why), "{text}: {refused}");
        }
        let default = LoopbackRedirect::from_setting(None).unwrap();
        assert_eq!(default, LoopbackRedirect::default_random());
        assert!(!default.fixed());
        assert!(
            LoopbackRedirect::from_setting(Some("http://x:1/"))
                .unwrap_err()
                .starts_with(SETTING)
        );
    }

    /// A pasted address is the attempt's only with the redirect's
    /// scheme, host, port and path and the attempt's state.
    #[test]
    fn a_pasted_address_is_checked_against_the_redirect_and_the_state() {
        let redirect = "http://127.0.0.1:8976/callback";
        let ok = "http://127.0.0.1:8976/callback?code=c-1&state=s-1";
        assert_eq!(pasted(ok, redirect, "s-1"), Ok("c-1".to_string()));
        assert_eq!(
            pasted(&format!("  {ok}\n"), redirect, "s-1"),
            Ok("c-1".to_string())
        );
        assert_eq!(pasted(ok, redirect, "s-2"), Err(Redirected::StateMismatch));
        for other in [
            "https://127.0.0.1:8976/callback?code=c-1&state=s-1",
            "http://localhost:8976/callback?code=c-1&state=s-1",
            "http://127.0.0.1:43123/callback?code=c-1&state=s-1",
            "http://127.0.0.1:8976/other?code=c-1&state=s-1",
            "code=c-1&state=s-1",
        ] {
            assert!(
                matches!(
                    pasted(other, redirect, "s-1"),
                    Err(Redirected::NotTheRedirect(_))
                ),
                "{other}"
            );
        }
        assert_eq!(
            pasted("http://127.0.0.1:8976/callback?state=s-1", redirect, "s-1"),
            Err(Redirected::NoCode)
        );
        assert_eq!(
            pasted(
                "http://127.0.0.1:8976/callback?state=s-1&error=access_denied",
                redirect,
                "s-1"
            ),
            Err(Redirected::Refused("access_denied".to_string()))
        );
    }

    async fn get(address: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    /// The listener answers a wrong path and a wrong state and goes on;
    /// the attempt's own callback ends it with its code. With `localhost`
    /// it listens on both loopback addresses, on one port.
    #[tokio::test]
    async fn the_listener_takes_only_the_attempt_s_callback() {
        let redirect = LoopbackRedirect {
            host: LoopbackHost::Localhost,
            port: 0,
            path: "/cb".to_string(),
        };
        let listener = CallbackListener::bind(&redirect).await.unwrap();
        let addresses = listener.addresses();
        let port = addresses[0].port();
        assert!(
            addresses
                .iter()
                .all(|a| a.port() == port && a.ip().is_loopback())
        );
        assert_eq!(listener.redirect_uri, format!("http://localhost:{port}/cb"));
        let at = addresses[addresses.len() - 1];
        let waiting = tokio::spawn(listener.wait("s-1"));
        assert!(
            get(at, "/other?code=c&state=s-1")
                .await
                .starts_with("HTTP/1.1 404")
        );
        assert!(
            get(at, "/cb?code=c&state=s-2")
                .await
                .starts_with("HTTP/1.1 400")
        );
        assert!(!waiting.is_finished());
        assert!(
            get(at, "/cb?code=c-9&state=s-1")
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert_eq!(waiting.await.unwrap(), Ok("c-9".to_string()));
    }

    /// A predefined port in use is a clear error naming the port and the
    /// setting.
    #[tokio::test]
    async fn a_predefined_port_in_use_names_the_port_and_the_setting() {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = taken.local_addr().unwrap().port();
        let redirect =
            LoopbackRedirect::parse(&format!("http://127.0.0.1:{port}/callback")).unwrap();
        let err = CallbackListener::bind(&redirect).await.unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, BindError::InUse { port: p } if p == port),
            "{text}"
        );
        assert!(
            text.contains(&port.to_string()) && text.contains(SETTING),
            "{text}"
        );
        assert!(text.contains("paste"), "{text}");
    }
}
