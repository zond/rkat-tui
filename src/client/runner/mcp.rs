//! `/mcp`, `/mcp login` and `/mcp logout` ([`crate::daemon::mcp_auth`] is
//! the daemon's side, [`crate::client::mcp_login`] the listener and the
//! pasted address's check).
//!
//! A login: the runner binds the loopback listener of the redirect
//! (`mcp.oauth_redirect_uri`, else a free port on `127.0.0.1`), asks the
//! daemon to start the attempt for it, shows the dialog, and completes the
//! attempt with whichever comes first: the browser's redirect at the
//! listener, or the address the user pastes. The attempt's deadline, Esc,
//! an agent switch, a re-home and leaving retire it and close the listener.
//! Over an unencrypted attach (`--attach-insecure` to a network address)
//! no login starts: its URL and code would cross the network in the clear.

use super::*;
use crate::client::mcp_login::{BindError, CallbackListener, LoopbackRedirect, Redirected};
use crate::ui::modal::{McpLoginShown, Secret};

/// Bound on the completion (the daemon exchanges the code with the
/// authorization server).
const COMPLETE_TIMEOUT: Duration = Duration::from_mins(1);

/// The login attempt this client waits for.
pub(super) struct PendingLogin {
    server: String,
    redirect_uri: String,
    state: String,
    connection: Arc<Connection>,
    /// Stops the attempt's task (listener and deadline).
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

/// The attempt pending, shared with its task.
pub(super) type McpLogins = Arc<parking_lot::Mutex<Option<PendingLogin>>>;

/// The plain words of a failed call: a refusal's message alone.
fn failure_text(err: &CallFailure) -> String {
    match err {
        CallFailure::Failed(ClientError::Rpc { message, .. }) => message.clone(),
        other => other.to_string(),
    }
}

/// Take the attempt pending if it is the one with `state`.
fn take_if(slot: &McpLogins, state: &str) -> Option<PendingLogin> {
    let mut slot = slot.lock();
    if slot.as_ref().is_some_and(|pending| pending.state == state) {
        slot.take()
    } else {
        None
    }
}

/// Complete `pending` with `code`, and tell the UI how it ended.
async fn complete(pending: PendingLogin, code: String, ui_tx: mpsc::UnboundedSender<Msg>) {
    let PendingLogin {
        server,
        redirect_uri,
        state,
        connection,
        mut stop,
    } = pending;
    if let Some(stop) = stop.take() {
        let _ = stop.send(());
    }
    let params = protocol::McpLoginCompleteParams {
        server: server.clone(),
        redirect_uri,
        state,
        code,
    };
    let done = bounded(
        COMPLETE_TIMEOUT,
        remote::call::<protocol::method::McpLoginComplete>(&connection, &params),
    )
    .await;
    let (level, text) = match done {
        Ok(outcome) if outcome.ok => (NoticeLevel::Info, outcome.text),
        Ok(outcome) => (NoticeLevel::Error, outcome.text),
        Err(err) => (NoticeLevel::Error, failure_text(&err)),
    };
    let _ = ui_tx.send(Msg::McpLoginClosed { level, text });
}

/// Retire `pending` on the daemon (best effort).
async fn retire(pending: PendingLogin) {
    let PendingLogin {
        server,
        connection,
        mut stop,
        ..
    } = pending;
    if let Some(stop) = stop.take() {
        let _ = stop.send(());
    }
    let _ = bounded(
        CALL_TIMEOUT,
        remote::call::<protocol::method::McpLoginCancel>(
            &connection,
            &protocol::McpServerParams { server },
        ),
    )
    .await;
}

impl Runner {
    /// `/mcp`: the servers, one line each, as a notice.
    pub(super) fn mcp_servers(&self) {
        self.ask::<protocol::method::McpServers>(protocol::Empty {}, "/mcp needs it", |answer| {
            match answer {
                Ok(list) if list.servers.is_empty() => vec![(
                    NoticeLevel::Info,
                    "no MCP servers are configured (~/.rkat/mcp.toml, the project's .rkat/mcp.toml, the realm config, --mcp-server)".to_string(),
                )],
                Ok(list) => {
                    let mut lines = vec!["MCP servers:".to_string()];
                    for row in &list.servers {
                        let mut line = format!(
                            "  {} ({}): {} — {}",
                            row.name,
                            row.source.words(),
                            row.address,
                            row.login.words()
                        );
                        if row.login.wants_login() {
                            line.push_str(&format!(": /mcp login {}", row.name));
                        }
                        lines.push(line);
                    }
                    vec![(NoticeLevel::Info, lines.join("\n"))]
                }
                Err(err) => vec![(NoticeLevel::Error, format!("/mcp failed: {}", failure_text(&err)))],
            }
        });
    }

    /// `/mcp logout <server>`.
    pub(super) fn mcp_logout(&mut self, server: String) {
        self.cancel_mcp_login(None);
        self.ask::<protocol::method::McpLogout>(
            protocol::McpServerParams { server },
            "/mcp logout needs it",
            |answer| match answer {
                Ok(outcome) => vec![(
                    if outcome.ok {
                        NoticeLevel::Info
                    } else {
                        NoticeLevel::Error
                    },
                    outcome.text,
                )],
                Err(err) => vec![(NoticeLevel::Error, failure_text(&err))],
            },
        );
    }

    /// `/mcp login <server>` (`confirmed`: the user said yes to a project
    /// server's URL).
    pub(super) fn mcp_login(&mut self, server: String, confirmed: bool) {
        if self.config.insecure {
            self.notice(
                NoticeLevel::Error,
                format!(
                    "/mcp login {server} is refused over this connection: it was attached with --attach-insecure to a network address, so the login's URL and code would cross that network unencrypted; attach through an ssh tunnel (ssh -L) instead"
                ),
            );
            return;
        }
        let Some(connection) = self.link_or_say("/mcp login needs it") else {
            return;
        };
        let redirect = match LoopbackRedirect::from_setting(self.config.mcp_redirect.as_deref()) {
            Ok(redirect) => redirect,
            Err(why) => {
                self.notice(NoticeLevel::Error, format!("/mcp login {server}: {why}"));
                return;
            }
        };
        self.cancel_mcp_login(None);
        let slot = Arc::clone(&self.mcp);
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let say = |level, text: String| {
                let _ = ui_tx.send(Msg::Notice { level, text });
            };
            let (listener, note) = match CallbackListener::bind(&redirect).await {
                Ok(listener) => (Some(listener), None),
                // A predefined redirect: the paste still works.
                Err(err @ BindError::InUse { .. }) => (None, Some(err.to_string())),
                Err(err) if redirect.fixed() => (None, Some(err.to_string())),
                Err(err) => {
                    say(NoticeLevel::Error, format!("/mcp login {server}: {err}"));
                    return;
                }
            };
            let redirect_uri = listener
                .as_ref()
                .map_or_else(|| redirect.uri(redirect.port), |l| l.redirect_uri.clone());
            let params = protocol::McpLoginStartParams {
                server: server.clone(),
                redirect_uri,
                confirmed,
            };
            let started = bounded(
                COMPLETE_TIMEOUT,
                remote::call::<protocol::method::McpLoginStart>(&connection, &params),
            )
            .await;
            let (authorize_url, state, redirect_uri, expires_in) = match started {
                Err(err) => {
                    say(NoticeLevel::Error, failure_text(&err));
                    return;
                }
                Ok(protocol::McpLoginStarted::Confirm { url }) => {
                    let _ = ui_tx.send(Msg::McpLoginConfirm { server, url });
                    return;
                }
                Ok(protocol::McpLoginStarted::Started {
                    authorize_url,
                    state,
                    redirect_uri,
                    expires_in,
                }) => (authorize_url, state, redirect_uri, expires_in),
            };
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            *slot.lock() = Some(PendingLogin {
                server: server.clone(),
                redirect_uri: redirect_uri.clone(),
                state: state.clone(),
                connection,
                stop: Some(stop_tx),
            });
            let _ = ui_tx.send(Msg::McpLoginOpen(McpLoginShown {
                server: server.clone(),
                authorize_url: Secret(authorize_url),
                redirect_uri,
                note,
            }));
            let callback = async {
                match listener {
                    Some(listener) => listener.wait(&state).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                // Cancelled, or a pasted address took the attempt.
                _ = stop_rx => {}
                () = tokio::time::sleep(Duration::from_secs(expires_in.max(1))) => {
                    if let Some(pending) = take_if(&slot, &state) {
                        retire(pending).await;
                        let _ = ui_tx.send(Msg::McpLoginClosed {
                            level: NoticeLevel::Warn,
                            text: format!("the login of MCP server `{server}` expired: /mcp login {server} to try again"),
                        });
                    }
                }
                got = callback => {
                    let Some(pending) = take_if(&slot, &state) else {
                        return;
                    };
                    match got {
                        Ok(code) => complete(pending, code, ui_tx.clone()).await,
                        Err(refused) => {
                            retire(pending).await;
                            let _ = ui_tx.send(Msg::McpLoginClosed {
                                level: NoticeLevel::Error,
                                text: format!("the login of MCP server `{server}` failed: {refused}"),
                            });
                        }
                    }
                }
            }
        });
    }

    /// The address the user pasted into the login dialog.
    pub(super) fn mcp_login_pasted(&mut self, text: &Secret) {
        let checked = {
            let slot = self.mcp.lock();
            let Some(pending) = slot.as_ref() else {
                self.send(Msg::McpLoginClosed {
                    level: NoticeLevel::Warn,
                    text: "no MCP login is pending".to_string(),
                });
                return;
            };
            crate::client::mcp_login::pasted(&text.0, &pending.redirect_uri, &pending.state)
                .map(|code| (code, pending.state.clone()))
        };
        match checked {
            Err(Redirected::Refused(why)) => {
                if let Some(pending) = self.mcp.lock().take() {
                    let server = pending.server.clone();
                    tokio::spawn(retire(pending));
                    self.send(Msg::McpLoginClosed {
                        level: NoticeLevel::Error,
                        text: format!(
                            "the login of MCP server `{server}` failed: the authorization server refused it: {why}"
                        ),
                    });
                }
            }
            Err(why) => self.send(Msg::McpLoginPasteRefused(why.to_string())),
            Ok((code, state)) => {
                if let Some(pending) = take_if(&self.mcp, &state) {
                    tokio::spawn(complete(pending, code, self.ui_tx.clone()));
                }
            }
        }
    }

    /// Retire the attempt pending (Esc; `why`: an agent switch, a
    /// re-home, leaving) and say so.
    pub(super) fn cancel_mcp_login(&mut self, why: Option<&str>) {
        let Some(pending) = self.mcp.lock().take() else {
            return;
        };
        let server = pending.server.clone();
        tokio::spawn(retire(pending));
        let text = match why {
            Some(why) => format!("the login of MCP server `{server}` was cancelled: {why}"),
            None => format!("the login of MCP server `{server}` was cancelled"),
        };
        self.send(Msg::McpLoginClosed {
            level: NoticeLevel::Info,
            text,
        });
    }
}
