//! The daemon's lifecycle as its clients see it: how many are attached,
//! whether any has detached since the daemon started, which connection
//! started it, and the request to shut down.
//!
//! The rules (`docs/mob-split.md`, "Direction"): `/detach` means "keep
//! going without me" and is recorded ([`Lifecycle::detach`]); `/quit` stops
//! the daemon only when the client that started it quits and nobody has
//! detached since ([`super::protocol::Status`] tells the client); a
//! connection that ends without `rkat/shutdown` (a hangup, a crash, a
//! dropped socket) is a detach when it is the owner's ([`Seat`]'s drop),
//! and never stops the daemon.
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

/// What the connection proved at attach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caller {
    /// It presented the daemon's start token.
    pub owner: bool,
}

/// The daemon's lifecycle state.
#[derive(Debug)]
pub struct Lifecycle {
    workspace: PathBuf,
    started_at: String,
    start_token: Option<String>,
    clients: AtomicUsize,
    detached: AtomicBool,
    /// A shutdown was asked for (or began): connections ending now are no
    /// detach.
    terminating: AtomicBool,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Lifecycle {
    /// The state of a daemon serving `workspace` (canonical), started at
    /// `started_at` (RFC 3339), with the start token its launcher handed
    /// it (an empty one counts as none).
    #[must_use]
    pub fn new(workspace: PathBuf, started_at: String, start_token: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            started_at,
            start_token: start_token.filter(|token| !token.is_empty()),
            clients: AtomicUsize::new(0),
            detached: AtomicBool::new(false),
            terminating: AtomicBool::new(false),
            shutdown: tokio::sync::watch::channel(false).0,
        })
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
            caller: Caller { owner },
            counted,
            withdrawn: false,
        }
    }

    /// The attached clients (probes not counted).
    #[must_use]
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    /// Whether any client has detached since the daemon started.
    #[must_use]
    pub fn detached(&self) -> bool {
        self.detached.load(Ordering::SeqCst)
    }

    /// `rkat/detach`: a client leaves and wants the agent kept running.
    pub fn detach(&self) {
        if !self.detached.swap(true, Ordering::SeqCst) {
            tracing::info!("a client detached: /quit asks before stopping this daemon from now on");
        }
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

    /// Resolves once [`Lifecycle::shutdown`] was called.
    pub async fn shutdown_requested(&self) {
        let mut asked = self.shutdown.subscribe();
        // The sender lives as long as `self`: an error cannot happen.
        let _ = asked.wait_for(|asked| *asked).await;
    }
}

/// One attached connection's place in the [`Lifecycle`]: released when
/// the connection ends. The owner's connection ending without a shutdown
/// is a detach (a hangup, a crash or a dropped socket keeps the agent
/// running, as `/detach` does).
#[derive(Debug)]
pub struct Seat {
    lifecycle: Arc<Lifecycle>,
    caller: Caller,
    counted: bool,
    /// Given up before the acknowledgement reached the client.
    withdrawn: bool,
}

impl Seat {
    /// What the connection proved.
    #[must_use]
    pub fn caller(&self) -> Caller {
        self.caller
    }

    /// Whether the seat counts as an attached client (not a probe).
    #[must_use]
    pub fn counted(&self) -> bool {
        self.counted
    }

    /// Give the seat up before the client learned it attached (its
    /// acknowledgement could not be written): uncounted, and no detach.
    pub fn withdraw(mut self) {
        self.withdrawn = true;
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        if self.counted {
            self.lifecycle.clients.fetch_sub(1, Ordering::SeqCst);
        }
        if self.withdrawn {
            return;
        }
        if self.caller.owner && !self.lifecycle.terminating.load(Ordering::SeqCst) {
            tracing::info!(
                "the client that started this daemon left without stopping it: a detach"
            );
            self.lifecycle.detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attach(start_token: Option<&str>, probe: bool) -> AttachParams {
        AttachParams {
            token: "t".into(),
            start_token: start_token.map(str::to_string),
            probe,
        }
    }

    #[test]
    fn the_owner_presents_the_start_token_and_its_hangup_is_a_detach() {
        let lifecycle = Lifecycle::new("/w".into(), "now".into(), Some("start".into()));
        let other = lifecycle.seat(&attach(Some("wrong"), false));
        let probe = lifecycle.seat(&attach(Some("start"), true));
        assert!(!other.caller().owner && !probe.caller().owner);
        let owner = lifecycle.seat(&attach(Some("start"), false));
        assert!(owner.caller().owner);
        assert_eq!(lifecycle.clients(), 2, "the probe is not counted");
        drop((other, probe));
        assert_eq!(lifecycle.clients(), 1);
        assert!(!lifecycle.detached(), "only the owner's end is a detach");
        drop(owner);
        assert_eq!(lifecycle.clients(), 0);
        assert!(lifecycle.detached());
    }

    #[test]
    fn a_shutdown_is_no_detach_and_without_a_start_token_nobody_owns() {
        let lifecycle = Lifecycle::new("/w".into(), "now".into(), Some("start".into()));
        let owner = lifecycle.seat(&attach(Some("start"), false));
        lifecycle.shutdown();
        drop(owner);
        assert!(!lifecycle.detached());
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
