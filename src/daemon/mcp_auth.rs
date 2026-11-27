//! OAuth logins of remote MCP servers (`/mcp`, `/mcp login`, `/mcp
//! logout`): the daemon's side.
//!
//! Only a server marked for OAuth takes part: an HTTP entry with
//! `oauth_account = "<subject>"` or `oauth_account_selection = "discover" |
//! "unverified"` (the engine's own `McpHttpConfig` fields). Any other
//! server connects as configured.
//!
//! **One login per user.** The engine keys an MCP credential by its
//! target's binding slug, which hashes the server's name and URL, its
//! account selection and, for a realm-scoped target, the Meerkat realm.
//! The daemon serves its agents a realm-less MCP OAuth authority
//! ([`meerkat::HostAuthService`] without `with_mcp_realm`, its
//! `mcp_oauth_authority()` installed as the factory's `McpAuthResolver`,
//! which wins over the engine's realm-scoped default for every session the
//! runtime builds, mob members included), and logs in through the same
//! service: both address the realm-less slot in the user's credential
//! store, so a login in one workspace serves the same server (name and
//! URL) in every workspace of this user.
//!
//! **No restart.** The factory hands every construction's MCP adapter to
//! [`Adapters`] (`AgentFactory::with_mcp_adapter_custody`). After a login
//! each live adapter retries the server awaiting exactly that target
//! (`McpRouterAdapter::retry_authorized`) and the daemon applies the staged
//! add at once (`apply_staged`: nothing in the engine applies a
//! factory-built router's staged operations at a member's boundary); after
//! a logout each releases it (`McpRouterAdapter::credential_released`,
//! applied the same way). A session records the wait
//! only at a boundary after its connect failed, which may come after the
//! login: the daemon's look at the waits ([`McpAuth::follow`]) retries a
//! waiting server whose login is stored by then (one made here, or in
//! another workspace).
//!
//! **The client owns the browser's way back** (`crate::client::mcp_login`):
//! it binds the loopback redirect and sends it in the start; the daemon
//! never binds a listener and never starts a browser. The authorize URL,
//! the state and the code go only to the calling client and to the engine:
//! never to an agent, a transcript, the audit log or a log line.
//!
//! **Who may log in.** A login starts only from a client's `rkat/*` call
//! (the user's `/mcp login`), never from a file, an agent or a tool. A
//! server of the project's `mcp.toml` (trusted, or it would not be
//! configured) first answers with its URL for the user to confirm; one of
//! an untrusted file is not a server.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use meerkat::{
    HostAuthService, HostMcpAuthPhase, McpOAuthCallback, McpOAuthLoginDisposition,
    McpOAuthLoginStart, McpRouterAdapter, McpServerIdentity,
};
use meerkat_core::McpServerConfig;
use meerkat_core::mcp_config::McpTransportConfig;

use super::decisions::Decisions;
use super::protocol::{
    MCP_AUTH, MCP_REFUSED, McpAuthNote, McpLoginCompleteParams, McpLoginStartParams,
    McpLoginStarted, McpLoginState, McpServerList, McpServerRow, McpSource, Outcome, Refusal,
};

/// How often the agents' adapters are asked which servers wait for a
/// login (the engine pushes no event for it).
const AWAITING_POLL: Duration = Duration::from_secs(2);

/// How to mark a server, said where a login is refused for the lack of it.
pub const MARK_HINT: &str = "mark it for OAuth in its [[servers]] entry: oauth_account_selection = \"unverified\" works with any authorization server; \"discover\" binds the account an authorization server that publishes OpenID Connect UserInfo verifies; oauth_account = \"<subject>\" names the one account to accept";

pub mod episodes;
pub mod guard;

use episodes::{Act, Episodes, Look, Stored};

/// Every MCP adapter the factory's constructions made (held weakly: a
/// session's end drops its adapter), and the calls their wrappers saw
/// refused for want of authorization ([`guard`]).
#[derive(Default)]
pub struct Adapters {
    held: parking_lot::Mutex<Vec<Weak<McpRouterAdapter>>>,
    refusals: Arc<guard::Refusals>,
}

impl meerkat::McpAdapterComposer for Adapters {
    fn compose(
        &self,
        adapter: &Arc<McpRouterAdapter>,
        _session_id: &meerkat_core::SessionId,
        _member: Option<&meerkat_core::MobMemberBinding>,
    ) -> Result<Arc<dyn meerkat_core::AgentToolDispatcher>, meerkat::McpAdapterComposeRefusal> {
        Ok(Arc::new(guard::Guarded::new(
            Arc::clone(adapter) as Arc<dyn meerkat_core::AgentToolDispatcher>,
            Arc::clone(&self.refusals),
        )))
    }
}

impl std::fmt::Debug for Adapters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adapters").finish_non_exhaustive()
    }
}

impl meerkat::McpAdapterCustody for Adapters {
    fn adopt(
        &self,
        adapter: &Arc<McpRouterAdapter>,
    ) -> Result<(), meerkat::McpAdapterCustodyClosed> {
        let mut held = self.held.lock();
        held.retain(|weak| weak.strong_count() > 0);
        held.push(Arc::downgrade(adapter));
        Ok(())
    }
}

impl Adapters {
    /// The adapters still live.
    fn live(&self) -> Vec<Arc<McpRouterAdapter>> {
        self.held.lock().iter().filter_map(Weak::upgrade).collect()
    }
}

/// One configured server.
#[derive(Debug, Clone)]
struct Server {
    config: McpServerConfig,
    source: McpSource,
}

/// The MCP logins of the daemon.
pub struct McpAuth {
    /// `None`: no credential store (none could be opened).
    service: Option<HostAuthService>,
    servers: Vec<Server>,
    /// The servers of an untrusted project `mcp.toml`.
    untrusted: Vec<McpServerConfig>,
    adapters: Arc<Adapters>,
    decisions: Arc<Decisions>,
    /// The attempt pending per server.
    pending: parking_lot::Mutex<HashMap<String, McpOAuthLoginStart>>,
    /// When each marked server needs a login ([`episodes`]).
    episodes: parking_lot::Mutex<Episodes>,
}

impl std::fmt::Debug for McpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAuth")
            .field("servers", &self.servers.len())
            .finish_non_exhaustive()
    }
}

/// A refusal in plain words.
fn refused(text: impl Into<String>) -> Refusal {
    Refusal::new(MCP_REFUSED, text)
}

/// The address `/mcp` shows of `config`.
fn address_of(config: &McpServerConfig) -> String {
    match &config.transport {
        McpTransportConfig::Http(http) => http.url.clone(),
        McpTransportConfig::Stdio(stdio) => std::iter::once(stdio.command.as_str())
            .chain(stdio.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Stage the server awaiting `target` in `adapter` again and apply it at
/// once: whether it was staged. A factory-built router applies its staged
/// operations only when its host does (the engine's own boundary apply is
/// for the RPC runtime's `mcp/add` sessions), so the daemon applies them,
/// as a live attachment does; the connection then runs in the background
/// and its tools reach the agent at its next turn.
async fn retry_on(adapter: &Arc<McpRouterAdapter>, target: &McpServerIdentity) -> bool {
    let server = target.server_name();
    match adapter.retry_authorized(target).await {
        Ok(meerkat::AuthorizedRetry::Staged) => {
            apply_now(adapter, server).await;
            true
        }
        // Staged by an earlier retry and not applied yet.
        Ok(meerkat::AuthorizedRetry::AlreadyPending) => {
            apply_now(adapter, server).await;
            false
        }
        Ok(retry) => {
            tracing::debug!(server, ?retry, "no MCP server to retry after the login");
            false
        }
        Err(err) => {
            tracing::warn!(server, error = %err, "an agent's session could not stage the MCP server after its login");
            false
        }
    }
}

/// Apply `adapter`'s staged operations now (see [`retry_on`]).
async fn apply_now(adapter: &Arc<McpRouterAdapter>, server: &str) {
    if let Err(err) = adapter.apply_staged().await {
        tracing::warn!(server, error = %err, "an agent's session could not apply the MCP server's change");
    }
}

/// Whether `config` is marked `oauth_account_selection = "unverified"`.
fn unverified(config: &McpServerConfig) -> bool {
    matches!(
        &config.transport,
        McpTransportConfig::Http(http) if http.oauth_account_selection
            == Some(meerkat_core::mcp_config::McpOAuthAccountSelection::Unverified)
    )
}

/// The OAuth target of a marked HTTP server: `Ok(None)` for one that is
/// not marked (or not HTTP), `Err` for a mark the engine refuses.
fn target_of(config: &McpServerConfig) -> Result<Option<McpServerIdentity>, String> {
    if !matches!(config.transport, McpTransportConfig::Http(_)) {
        return Ok(None);
    }
    let target = McpServerIdentity::from_config(config).map_err(|err| err.to_string())?;
    Ok(target.is_selected().then_some(target))
}

impl McpAuth {
    /// The logins over `service` (the realm-less one the agents' resolver
    /// is), for `servers` by their `sources` and the `untrusted` ones.
    #[must_use]
    pub fn new(
        service: Option<HostAuthService>,
        servers: &[McpServerConfig],
        sources: &[(String, McpSource)],
        untrusted: Vec<McpServerConfig>,
        adapters: Arc<Adapters>,
        decisions: Arc<Decisions>,
    ) -> Self {
        let servers = servers
            .iter()
            .map(|config| Server {
                source: sources
                    .iter()
                    .find(|(name, _)| *name == config.name)
                    .map_or(McpSource::Realm, |(_, source)| *source),
                config: config.clone(),
            })
            .collect();
        Self {
            service,
            servers,
            untrusted,
            adapters,
            decisions,
            pending: parking_lot::Mutex::default(),
            episodes: parking_lot::Mutex::default(),
        }
    }

    /// The stored login of `target`, as the episodes read it (no network).
    async fn stored(&self, target: &McpServerIdentity) -> Stored {
        let Some(service) = &self.service else {
            return Stored::Unknown;
        };
        match service.mcp_status(target).await {
            Ok(status) => match status.phase {
                // The secret-free facts that tell one login from the next.
                HostMcpAuthPhase::Authorized => {
                    Stored::Authorized(format!("{:?}/{:?}", status.expires_at, status.account_id))
                }
                HostMcpAuthPhase::ReauthRequired => Stored::Expired,
                HostMcpAuthPhase::AuthorizationRequired => Stored::Missing,
            },
            Err(_) => Stored::Unknown,
        }
    }

    /// The login state of `config` from the credential store (no network).
    async fn state_of(&self, config: &McpServerConfig) -> McpLoginState {
        if !matches!(config.transport, McpTransportConfig::Http(_)) {
            return McpLoginState::Local;
        }
        let target = match target_of(config) {
            Ok(Some(target)) => target,
            Ok(None) => return McpLoginState::NotMarked,
            Err(why) => return McpLoginState::Unknown { why },
        };
        let Some(service) = &self.service else {
            return McpLoginState::Unknown {
                why: "this daemon has no credential store".to_string(),
            };
        };
        let stored = self.stored(&target).await;
        if self.episodes.lock().refused(&config.name, &stored) {
            return McpLoginState::Refused;
        }
        match service.mcp_status(&target).await {
            Ok(status) => match status.phase {
                HostMcpAuthPhase::Authorized => McpLoginState::Authorized {
                    expires_at: status.expires_at.map(|at| at.to_rfc3339()),
                },
                HostMcpAuthPhase::ReauthRequired => McpLoginState::NeedsRelogin,
                HostMcpAuthPhase::AuthorizationRequired => McpLoginState::NeedsLogin,
            },
            Err(err) => McpLoginState::Unknown {
                why: err.to_string(),
            },
        }
    }

    /// `/mcp`: every server, the untrusted ones last.
    pub async fn list(&self) -> McpServerList {
        let mut servers = Vec::new();
        for server in &self.servers {
            servers.push(McpServerRow {
                name: server.config.name.clone(),
                source: server.source,
                address: address_of(&server.config),
                login: self.state_of(&server.config).await,
            });
        }
        for config in &self.untrusted {
            servers.push(McpServerRow {
                name: config.name.clone(),
                source: McpSource::Project,
                address: address_of(config),
                login: McpLoginState::Untrusted,
            });
        }
        McpServerList { servers }
    }

    /// The text of a server that needs a login.
    fn needs_login_text(name: &str) -> String {
        format!("MCP server `{name}` needs login: /mcp login {name}")
    }

    /// The marked servers and their targets.
    fn marked_targets(&self) -> impl Iterator<Item = (&str, McpServerIdentity)> {
        self.servers
            .iter()
            .filter_map(|server| match target_of(&server.config) {
                Ok(Some(target)) => Some((server.config.name.as_str(), target)),
                _ => None,
            })
    }

    /// The start-up notices: each marked server that needs a login, from
    /// the stored state (no network); each opens its episode.
    pub async fn startup_notices(&self) -> Vec<String> {
        let mut notices = Vec::new();
        for (name, target) in self.marked_targets() {
            let look = Look {
                stored: self.stored(&target).await,
                awaited: false,
                refused_at_call: false,
            };
            if self.episodes.lock().step(name, &look) == Some(Act::Tell) {
                notices.push(Self::needs_login_text(name));
            }
        }
        notices
    }

    /// The marked server `name`, its target and source.
    fn marked(&self, name: &str) -> Result<(McpServerIdentity, &Server), Refusal> {
        let Some(server) = self.servers.iter().find(|s| s.config.name == name) else {
            if self.untrusted.iter().any(|c| c.name == name) {
                return Err(refused(format!(
                    "MCP server `{name}` comes from the project's .rkat/mcp.toml, which is not trusted: it does not start and cannot be logged into (/trust, then restart the daemon)"
                )));
            }
            let known: Vec<&str> = self
                .servers
                .iter()
                .map(|s| s.config.name.as_str())
                .collect();
            return Err(refused(if known.is_empty() {
                format!("no MCP server named `{name}`: none is configured")
            } else {
                format!(
                    "no MCP server named `{name}` (configured: {})",
                    known.join(", ")
                )
            }));
        };
        if !matches!(server.config.transport, McpTransportConfig::Http(_)) {
            return Err(refused(format!(
                "MCP server `{name}` is a local (stdio) server: it has no login"
            )));
        }
        match target_of(&server.config) {
            Ok(Some(target)) => Ok((target, server)),
            Ok(None) => Err(refused(format!(
                "MCP server `{name}` is not marked for OAuth login; {MARK_HINT}"
            ))),
            Err(why) => Err(refused(format!(
                "MCP server `{name}` cannot log in as configured: {why}"
            ))),
        }
    }

    fn service(&self) -> Result<&HostAuthService, Refusal> {
        self.service.as_ref().ok_or_else(|| {
            refused(
                "this daemon has no credential store, so nothing can be logged into (see its log)",
            )
        })
    }

    /// `rkat/mcp_login_start`.
    ///
    /// # Errors
    ///
    /// [`MCP_REFUSED`] saying why.
    pub async fn login_start(
        &self,
        params: McpLoginStartParams,
    ) -> Result<McpLoginStarted, Refusal> {
        let McpLoginStartParams {
            server,
            redirect_uri,
            confirmed,
        } = params;
        let (target, entry) = self.marked(&server)?;
        if entry.source == McpSource::Project && !confirmed {
            return Ok(McpLoginStarted::Confirm {
                url: target.server_url().to_string(),
            });
        }
        let service = self.service()?;
        let failed = |err: meerkat::HostAuthError| {
            refused(format!("the login of `{server}` did not start: {err}"))
        };
        let mut start = match service.mcp_login_start(&target, &redirect_uri, None).await {
            // The engine never reconnects over a stored unverified login
            // (meerkat-auth-core `attempt_selection`): the user's new login
            // replaces it, so the old one goes first (it binds no account).
            Err(meerkat::HostAuthError::McpOAuth(meerkat::McpOAuthError::DisconnectRequired {
                ..
            })) if unverified(&entry.config) => {
                service.mcp_logout(&target).await.map_err(failed)?;
                service
                    .mcp_login_start(&target, &redirect_uri, None)
                    .await
                    .map_err(failed)?
            }
            other => other.map_err(failed)?,
        };
        // An attempt pending for another redirect (an earlier client's):
        // retired, and this one admitted.
        if start.disposition == McpOAuthLoginDisposition::Joined
            && start.redirect_uri != redirect_uri
        {
            if let Err(err) = service.mcp_login_cancel(&target, &start) {
                tracing::warn!(server = %server, error = %err, "could not retire an earlier MCP login attempt");
            }
            start = service
                .mcp_login_start(&target, &redirect_uri, None)
                .await
                .map_err(failed)?;
        }
        let started = McpLoginStarted::Started {
            authorize_url: start.authorize_url.clone(),
            state: start.state.clone(),
            redirect_uri: start.redirect_uri.clone(),
            expires_in: start.remaining().as_secs(),
        };
        self.pending.lock().insert(server, start);
        Ok(started)
    }

    /// `rkat/mcp_login_complete`: the engine exchanges the code; every
    /// live session then connects the server at its next turn.
    ///
    /// # Errors
    ///
    /// [`MCP_REFUSED`] for no pending attempt, another redirect or state,
    /// or the engine's refusal.
    pub async fn login_complete(&self, params: McpLoginCompleteParams) -> Result<Outcome, Refusal> {
        let McpLoginCompleteParams {
            server,
            redirect_uri,
            state,
            code,
        } = params;
        let (target, _) = self.marked(&server)?;
        let service = self.service()?;
        let start = {
            let mut pending = self.pending.lock();
            match pending.get(&server) {
                None => {
                    return Err(refused(format!(
                        "no login of `{server}` is pending (it expired or was cancelled): /mcp login {server}"
                    )));
                }
                Some(start) if start.redirect_uri != redirect_uri || start.state != state => {
                    return Err(refused(format!(
                        "that redirect does not belong to the pending login of `{server}`"
                    )));
                }
                Some(_) => pending.remove(&server),
            }
        };
        drop(start);
        let done = service
            .mcp_login_complete(
                &target,
                McpOAuthCallback {
                    redirect_uri,
                    state,
                    code,
                },
            )
            .await
            .map_err(|err| refused(format!("the login of `{server}` failed: {err}")))?;
        // No refusal stands for this login; the episode closes at the next
        // look that finds it authorized and nothing waiting.
        let _ = self.adapters.refusals.take(&server);
        self.episodes.lock().logged_in(&server);
        let mut retried = 0;
        for adapter in self.adapters.live() {
            if retry_on(&adapter, &target).await {
                retried += 1;
            }
        }
        tracing::info!(server = %server, sessions = retried, "MCP server logged in");
        let account = done
            .account_id
            .map(|account| format!(" as {account}"))
            .unwrap_or_default();
        let text = format!(
            "logged in to MCP server `{server}`{account}: its tools reach the agents at their next turn"
        );
        Ok(Outcome::ok(text))
    }

    /// `rkat/mcp_login_cancel`.
    pub fn login_cancel(&self, server: &str) {
        let Some(start) = self.pending.lock().remove(server) else {
            return;
        };
        if let (Some(service), Ok((target, _))) = (&self.service, self.marked(server))
            && let Err(err) = service.mcp_login_cancel(&target, &start)
        {
            tracing::debug!(server = %server, error = %err, "the MCP login attempt was gone already");
        }
    }

    /// `rkat/mcp_logout`.
    ///
    /// # Errors
    ///
    /// [`MCP_REFUSED`] saying why.
    pub async fn logout(&self, server: &str) -> Result<Outcome, Refusal> {
        let (target, _) = self.marked(server)?;
        self.login_cancel(server);
        let service = self.service()?;
        service
            .mcp_logout(&target)
            .await
            .map_err(|err| refused(format!("the logout of `{server}` failed: {err}")))?;
        for adapter in self.adapters.live() {
            match adapter.credential_released(&target).await {
                Ok(meerkat::ReleasedCredential::Unpublished) => apply_now(&adapter, server).await,
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(server = %server, error = %err, "an agent's session could not release the MCP server");
                }
            }
        }
        let text = format!(
            "logged out of MCP server `{server}` (in every workspace): its tools leave the agents at their next turn; nothing is revoked at the provider"
        );
        // The next look finds no login stored: a new episode, told.
        Ok(Outcome::ok(text))
    }

    fn tell(&self, server: &str, login: McpLoginState, text: &str) {
        self.decisions.broadcast(
            MCP_AUTH,
            &McpAuthNote {
                server: server.to_string(),
                login,
                text: text.to_string(),
            },
        );
    }

    /// One look at every marked server ([`episodes`]): its stored login
    /// (no network), whether a session waits for its authorization, and
    /// whether a call was refused for want of it. The clients are told
    /// once per episode; a waiting server is retried when a new login is
    /// stored (one of this daemon before the session recorded its wait,
    /// or one in another workspace): the session connects it at once.
    async fn look(&self) {
        let adapters = self.adapters.live();
        let mut waits: Vec<(Arc<McpRouterAdapter>, McpServerIdentity)> = Vec::new();
        for adapter in &adapters {
            for target in adapter.servers_awaiting_authorization().await {
                waits.push((Arc::clone(adapter), target));
            }
        }
        for (name, target) in self.marked_targets() {
            let awaited: Vec<&Arc<McpRouterAdapter>> = waits
                .iter()
                .filter(|(_, waiting)| *waiting == target)
                .map(|(adapter, _)| adapter)
                .collect();
            let look = Look {
                stored: self.stored(&target).await,
                awaited: !awaited.is_empty(),
                refused_at_call: self.adapters.refusals.take(name),
            };
            let act = self.episodes.lock().step(name, &look);
            match act {
                Some(Act::Tell) => {
                    let login = if matches!(look.stored, Stored::Authorized(_)) {
                        McpLoginState::Refused
                    } else {
                        McpLoginState::NeedsLogin
                    };
                    self.tell(name, login, &Self::needs_login_text(name));
                }
                Some(Act::Retry) => {
                    for adapter in awaited {
                        retry_on(adapter, &target).await;
                    }
                }
                None => {}
            }
        }
    }

    /// Follow the agents' sessions for servers that need a login
    /// ([`Self::look`]) while the daemon runs; nothing when no
    /// server is marked.
    #[must_use]
    pub fn follow(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let marked = self
            .servers
            .iter()
            .any(|server| matches!(target_of(&server.config), Ok(Some(_))));
        if !marked {
            return None;
        }
        let this = Arc::downgrade(self);
        Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(AWAITING_POLL);
            loop {
                tick.tick().await;
                let Some(auth) = this.upgrade() else {
                    return;
                };
                auth.look().await;
            }
        }))
    }
}
