//! The daemon's lifecycle as its clients see it: how many are attached,
//! which connection started it, and the request to shut down. Which agent
//! each connection works with, and whether an agent was left running
//! without a client, is [`super::agents`]'.
//!
//! The rules (`docs/mob-split.md`, "Direction"): `/detach` means "keep my
//! agent going without me" and is recorded with the agent
//! ([`super::agents::Agents::detach`]); `/quit` stops the daemon only when
//! the client that started it is the last one and no agent was left
//! detached ([`super::protocol::Status`] tells the client); a connection
//! that ends without retiring its agent or a shutdown (a hangup, a crash, a
//! dropped socket) leaves the agent detached when it was its last client
//! ([`super::agents::Binding`]'s drop), and never stops the daemon.
//!
//! The owner is the connection that presents the start token: the client
//! that launches the daemon draws one, hands it to the daemon in
//! [`START_TOKEN_ENV`] and presents it in its `rkat/attach`. A daemon
//! started by hand, or restarted by a client after it died, has none and
//! so no owner: every `/quit` then asks.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::protocol::AttachParams;

/// The environment variable a launching client hands the daemon its start
/// token in (the agent's confined shell gets a curated environment
/// without it).
pub const START_TOKEN_ENV: &str = "RKAT_TUI_START_TOKEN";

/// What the connection proved at attach, and the agent it is bound to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// It presented the daemon's start token.
    pub owner: bool,
    /// The agent it works with (`None`: a probe, or a connection that
    /// asked for none).
    pub agent: Option<String>,
}

/// The daemon's lifecycle state.
#[derive(Debug)]
pub struct Lifecycle {
    build: String,
    workspace: PathBuf,
    started_at: String,
    start_token: Option<String>,
    clients: AtomicUsize,
    /// A shutdown was asked for (or began): connections ending now are no
    /// detach.
    terminating: AtomicBool,
    /// The shutdown retires every agent first (`rkat/shutdown {retire}`).
    retire_all: AtomicBool,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Lifecycle {
    /// The state of a daemon serving `workspace` (canonical), started at
    /// `started_at` (RFC 3339), with the start token its launcher handed
    /// it (an empty one counts as none).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(workspace: PathBuf, started_at: String, start_token: Option<String>) -> Arc<Self> {
        Self::built(workspace, started_at, start_token, crate::BUILD.to_string())
    }

    /// [`Self::new`], naming `build` as the daemon's build (tests name
    /// another than [`crate::BUILD`]).
    #[must_use]
    pub fn built(
        workspace: PathBuf,
        started_at: String,
        start_token: Option<String>,
        build: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            build,
            workspace,
            started_at,
            start_token: start_token.filter(|token| !token.is_empty()),
            clients: AtomicUsize::new(0),
            terminating: AtomicBool::new(false),
            retire_all: AtomicBool::new(false),
            shutdown: tokio::sync::watch::channel(false).0,
        })
    }

    /// The daemon's build, as its attach answer names it.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }

    /// The workspace the daemon serves.
    #[must_use]
    pub fn workspace(&self) -> &std::path::Path {
        &self.workspace
    }

    /// When the daemon started (RFC 3339).
    #[must_use]
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// A connection attached with `params`: counted until the seat drops
    /// (a probe is not), the owner when it presented the start token.
    #[must_use]
    pub fn seat(self: &Arc<Self>, params: &AttachParams) -> Seat {
        let owner = !params.probe
            && match (&self.start_token, &params.start_token) {
                (Some(ours), Some(given)) => {
                    super::handshake::same(ours.as_bytes(), given.as_bytes())
                }
                _ => false,
            };
        let counted = !params.probe;
        if counted {
            self.clients.fetch_add(1, Ordering::SeqCst);
        }
        Seat {
            lifecycle: Arc::clone(self),
            caller: Caller { owner, agent: None },
            counted,
            binding: None,
        }
    }

    /// The attached clients (probes not counted).
    #[must_use]
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    /// `rkat/shutdown {"retire": true}`: stop the daemon, every agent
    /// retired first.
    pub fn shutdown_retiring(&self) {
        self.retire_all.store(true, Ordering::SeqCst);
        self.shutdown();
    }

    /// Whether the shutdown retires every agent.
    #[must_use]
    pub fn retires_all(&self) -> bool {
        self.retire_all.load(Ordering::SeqCst)
    }

    /// `rkat/shutdown` (or a signal): stop the daemon.
    pub fn shutdown(&self) {
        self.terminating.store(true, Ordering::SeqCst);
        self.shutdown.send_replace(true);
    }

    /// The daemon is shutting down (whatever asked): connections ending
    /// from now on are no detach.
    pub fn terminating(&self) {
        self.terminating.store(true, Ordering::SeqCst);
    }

    /// Whether the daemon is shutting down.
    #[must_use]
    pub fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::SeqCst)
    }

    /// Resolves once [`Lifecycle::shutdown`] was called.
    pub async fn shutdown_requested(&self) {
        let mut asked = self.shutdown.subscribe();
        // The sender lives as long as `self`: an error cannot happen.
        let _ = asked.wait_for(|asked| *asked).await;
    }
}

/// One attached connection's place in the [`Lifecycle`], and its binding
/// to an agent: released when the connection ends (the binding's end
/// leaves an agent without a client detached, see
/// [`super::agents::Binding`]).
#[derive(Debug)]
pub struct Seat {
    lifecycle: Arc<Lifecycle>,
    caller: Caller,
    counted: bool,
    binding: Option<super::agents::Binding>,
}

impl Seat {
    /// What the connection proved, and its agent.
    #[must_use]
    pub fn caller(&self) -> Caller {
        self.caller.clone()
    }

    /// The connection works with the agent `binding` holds.
    pub fn bind(&mut self, binding: super::agents::Binding) {
        self.caller.agent = Some(binding.name().to_string());
        self.binding = Some(binding);
    }

    /// Whether the seat counts as an attached client (not a probe).
    #[must_use]
    pub fn counted(&self) -> bool {
        self.counted
    }

    /// Give the seat up before the client learned it attached (its
    /// acknowledgement could not be written): uncounted, and its agent is
    /// not detached.
    pub fn withdraw(mut self) {
        if let Some(binding) = self.binding.take() {
            binding.withdraw();
        }
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        if self.counted {
            self.lifecycle.clients.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attach(start_token: Option<&str>, probe: bool) -> AttachParams {
        AttachParams {
            start_token: start_token.map(str::to_string),
            probe,
            agent: None,
            ..AttachParams::default()
        }
    }

    #[test]
    fn the_owner_presents_the_start_token_and_a_probe_is_not_counted() {
        let lifecycle = Lifecycle::new("/w".into(), "now".into(), Some("start".into()));
        let other = lifecycle.seat(&attach(Some("wrong"), false));
        let probe = lifecycle.seat(&attach(Some("start"), true));
        assert!(!other.caller().owner && !probe.caller().owner);
        let owner = lifecycle.seat(&attach(Some("start"), false));
        assert!(owner.caller().owner);
        assert_eq!(owner.caller().agent, None, "bound to no agent yet");
        assert_eq!(lifecycle.clients(), 2, "the probe is not counted");
        drop((other, probe));
        assert_eq!(lifecycle.clients(), 1);
        drop(owner);
        assert_eq!(lifecycle.clients(), 0);
    }

    #[test]
    fn a_shutdown_is_terminating_and_without_a_start_token_nobody_owns() {
        let lifecycle = Lifecycle::new("/w".into(), "now".into(), Some("start".into()));
        let owner = lifecycle.seat(&attach(Some("start"), false));
        assert!(!lifecycle.is_terminating());
        lifecycle.shutdown();
        drop(owner);
        assert!(lifecycle.is_terminating());
        let unowned = Lifecycle::new("/w".into(), "now".into(), Some(String::new()));
        assert!(!unowned.seat(&attach(Some(""), false)).caller().owner);
        assert!(!unowned.seat(&attach(None, false)).caller().owner);
    }

    #[tokio::test]
    async fn a_shutdown_request_resolves_waiters_before_and_after() {
        let lifecycle = Lifecycle::new("/w".into(), "now".into(), None);
        let waiting = tokio::spawn({
            let lifecycle = Arc::clone(&lifecycle);
            async move { lifecycle.shutdown_requested().await }
        });
        tokio::task::yield_now().await;
        lifecycle.shutdown();
        assert!(waiting.await.is_ok());
        lifecycle.shutdown_requested().await;
    }
}
