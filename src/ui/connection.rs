//! The daemon connection's state as the UI knows it ([`Connection`]): the
//! status bar's first segment, and what `/quit`, `/detach` and the quit
//! warnings decide on (never a display string).

use std::path::PathBuf;
use std::time::Instant;

/// Where this client stands with the workspace's daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// Looking for the daemon (the first connect, or `/daemon start`).
    Connecting,
    /// A daemon this client started is coming up.
    Starting,
    /// Attached to the daemon at `address`.
    Attached {
        /// The daemon's address.
        address: String,
        /// The daemon's build (`None`: not known); shown
        /// when it is not this client's ([`crate::BUILD`]).
        daemon_build: Option<String>,
    },
    /// The connection dropped and is being re-made.
    Reconnecting,
    /// The connection could not be made.
    Failed {
        /// Why (the daemon's exit status, a refusal, …).
        reason: String,
        /// The daemon's output, when this client started it.
        log: Option<PathBuf>,
        /// When the client tries again by itself (`None`: it does not;
        /// `/daemon start` does).
        retry_at: Option<Instant>,
    },
    /// The daemon shut down after `/daemon stop`.
    Stopped,
}

impl Connection {
    /// Whether this client reached a daemon it is (or was, while it
    /// reconnects) attached to: `/quit` asks the daemon, `/detach` leaves
    /// it.
    #[must_use]
    pub fn reached(&self) -> bool {
        matches!(self, Self::Attached { .. } | Self::Reconnecting)
    }

    /// The status bar's segment at `now`.
    #[must_use]
    pub fn segment(&self, now: Instant) -> String {
        match self {
            Self::Connecting => "connecting".to_string(),
            Self::Starting => "daemon starting".to_string(),
            Self::Attached {
                address,
                daemon_build,
            } => match daemon_build {
                Some(build) if build != crate::BUILD => {
                    format!("attached {address} · daemon build {build}")
                }
                _ => format!("attached {address}"),
            },
            Self::Reconnecting => "reconnecting".to_string(),
            Self::Failed {
                reason,
                log,
                retry_at,
            } => {
                let mut text = reason.clone();
                if let Some(at) = retry_at {
                    let secs = at.saturating_duration_since(now).as_secs_f64().ceil();
                    text.push_str(&format!(" · retry {secs}s"));
                } else {
                    text.push_str(" · /daemon start retries");
                }
                if let Some(log) = log {
                    text.push_str(&format!(" · log {}", log.display()));
                }
                text
            }
            Self::Stopped => "daemon stopped".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_failure_names_its_reason_retry_and_log() {
        let now = Instant::now();
        let failed = Connection::Failed {
            reason: "daemon exited (signal: 7 SIGBUS)".into(),
            log: Some(PathBuf::from("/var/log/rkat-tui-mob.log")),
            retry_at: Some(now + Duration::from_millis(7_200)),
        };
        assert_eq!(
            failed.segment(now),
            "daemon exited (signal: 7 SIGBUS) · retry 8s · log /var/log/rkat-tui-mob.log"
        );
        assert!(!failed.reached());
        let given_up = Connection::Failed {
            reason: "no daemon".into(),
            log: None,
            retry_at: None,
        };
        assert_eq!(given_up.segment(now), "no daemon · /daemon start retries");
    }

    #[test]
    fn another_build_is_shown_and_this_one_is_not() {
        let now = Instant::now();
        let same = Connection::Attached {
            address: "127.0.0.1:1".into(),
            daemon_build: Some(crate::BUILD.into()),
        };
        assert_eq!(same.segment(now), "attached 127.0.0.1:1");
        assert!(same.reached());
        let other = Connection::Attached {
            address: "127.0.0.1:1".into(),
            daemon_build: Some("0.0.1+old".into()),
        };
        assert_eq!(
            other.segment(now),
            "attached 127.0.0.1:1 · daemon build 0.0.1+old"
        );
    }
}
