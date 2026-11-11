//! The connection's lifecycle as one state ([`Conn`]): which of connecting,
//! attached, waiting to retry, failed, stopped or re-homing the runner is
//! in, with each state's own data. A combination the flags it replaces
//! could hold (a link and a failed first connect, a stop and a retry) is
//! not representable.

use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{ClientError, Established, Link, Notification};

/// The task making a connect.
pub(super) type ConnectTask = JoinHandle<Result<Established, ClientError>>;

/// Where the connection is.
pub(super) enum Conn {
    /// Nothing runs: before the first connect, after a link was dropped
    /// for an agent switch, or after a re-home that did not start.
    Idle,
    /// A connect runs; `tries` connects failed before it in a row.
    Connecting {
        /// The connect.
        task: ConnectTask,
        /// The failed connects before it ([`super::START_RETRIES`]).
        tries: u32,
    },
    /// Attached.
    Up {
        /// The connection and the agent's stream.
        link: Link,
        /// Why the daemon is about to end the link, when it is.
        leaving: Option<Leaving>,
    },
    /// A connect failed; the next is made by itself at `at`.
    Retrying {
        /// When.
        at: Instant,
        /// The failed connects so far.
        tries: u32,
    },
    /// The connect failed for good: `/daemon start` tries again.
    Failed,
    /// The daemon was stopped (`/daemon stop`, a terminating quit, a
    /// re-home that stranded this client): a lost connection is not made
    /// again.
    Stopped,
    /// A re-home stopped the daemon; the runner attaches again when the
    /// re-home ends.
    Rehoming,
}

/// Why an attached daemon is about to end the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leaving {
    /// It was asked to stop.
    Stop,
    /// A re-home stops it.
    Rehome,
}

/// What happened to the connection ([`Conn::next`]).
pub(super) enum ConnEvent {
    /// The link carried a notification.
    Note(Notification),
    /// The link is gone.
    Lost,
    /// The connect ended.
    Connected(Result<Result<Established, ClientError>, tokio::task::JoinError>),
    /// The retry is due.
    RetryDue,
}

impl Conn {
    /// The link, when attached.
    pub(super) fn link(&self) -> Option<&Link> {
        match self {
            Self::Up { link, .. } => Some(link),
            _ => None,
        }
    }

    /// The link, when attached.
    pub(super) fn link_mut(&mut self) -> Option<&mut Link> {
        match self {
            Self::Up { link, .. } => Some(link),
            _ => None,
        }
    }

    /// Whether the daemon was stopped, or asked to stop while attached.
    pub(super) fn stopping(&self) -> bool {
        matches!(
            self,
            Self::Stopped
                | Self::Up {
                    leaving: Some(Leaving::Stop),
                    ..
                }
        )
    }

    /// The next thing that happens to the connection; pending in a state
    /// where nothing can (failed, stopped, idle, re-homing). Cancel-safe.
    pub(super) async fn next(&mut self) -> ConnEvent {
        match self {
            Self::Up { link, .. } => match link.notifications.recv().await {
                Some(note) => ConnEvent::Note(note),
                None => ConnEvent::Lost,
            },
            Self::Connecting { task, .. } => ConnEvent::Connected(task.await),
            Self::Retrying { at, .. } => {
                tokio::time::sleep_until(*at).await;
                ConnEvent::RetryDue
            }
            Self::Idle | Self::Failed | Self::Stopped | Self::Rehoming => {
                std::future::pending().await
            }
        }
    }

    /// Why there is no connection now, as a notice says it.
    pub(super) fn unattached(&self) -> &'static str {
        match self {
            _ if self.stopping() => "the daemon was stopped; /daemon start starts it again",
            Self::Connecting { .. } => "connecting to it",
            Self::Failed => "the connect failed; /daemon start tries again",
            Self::Retrying { .. } => "the connect is tried again shortly",
            Self::Rehoming => "re-homing; it attaches again when that ends",
            Self::Idle | Self::Up { .. } | Self::Stopped => "reconnecting",
        }
    }
}
