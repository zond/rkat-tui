//! When a marked MCP server needs a login, as episodes: an episode opens
//! when the server needs one (its stored login is missing or expired
//! beyond refresh, a session waits for its authorization, or the server
//! refused a call with the stored login) and closes once it is authorized
//! and nothing waits for it or was refused with that login. The clients
//! are told once per episode, so a login that expires or is revoked
//! later, while the daemon runs, is told again.
//!
//! A login is told apart by the stored credential's secret-free facts
//! ([`Stored::Authorized`]'s fingerprint: its expiry and account). A
//! server waiting for authorization while a login is stored is retried
//! only when that login changed since the episode opened (a login in
//! another workspace): one the server refused at its connect stays
//! refused, and is not retried over the network every look.

use std::collections::HashMap;

/// The stored login of a server as the look reads it (no network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// Usable (or refreshable): its fingerprint.
    Authorized(String),
    /// None stored.
    Missing,
    /// Expired beyond refresh, or refused by the token endpoint.
    Expired,
    /// The store did not answer.
    Unknown,
}

/// What one look saw of a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    pub stored: Stored,
    /// A session waits for its authorization.
    pub awaited: bool,
    /// A call was refused for want of authorization since the last look.
    pub refused_at_call: bool,
}

/// What the look does for a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Tell the clients it needs a login (the episode opened).
    Tell,
    /// Retry it in the sessions that wait (a new login is stored).
    Retry,
}

/// An open episode.
#[derive(Debug)]
struct Open {
    /// The login stored when it opened (or was last retried), if any.
    at: Option<String>,
}

#[derive(Debug, Default)]
struct One {
    open: Option<Open>,
    /// The login the server refused a call with.
    refused_with: Option<String>,
    /// A login completed here since the last look: a session still
    /// waiting for it is its retry under way, not a new episode.
    logged_in: bool,
}

/// Every marked server's episode.
#[derive(Debug, Default)]
pub struct Episodes(HashMap<String, One>);

impl Episodes {
    /// One look at `server`: what to do.
    pub fn step(&mut self, server: &str, look: &Look) -> Option<Act> {
        let one = self.0.entry(server.to_string()).or_default();
        let fingerprint = match &look.stored {
            Stored::Authorized(fingerprint) => Some(fingerprint.clone()),
            _ => None,
        };
        match &fingerprint {
            Some(now) if look.refused_at_call => one.refused_with = Some(now.clone()),
            // Another login since the refusal.
            Some(now) if one.refused_with.as_ref().is_some_and(|with| with != now) => {
                one.refused_with = None;
            }
            Some(_) => {}
            None if look.stored != Stored::Unknown => one.refused_with = None,
            None => {}
        }
        let refused = fingerprint.is_some() && one.refused_with.is_some();
        let needs =
            matches!(look.stored, Stored::Missing | Stored::Expired) || look.awaited || refused;
        // Waiting with a usable login is all there is to it.
        let only_waiting = look.awaited && fingerprint.is_some() && !refused;
        if one.logged_in && only_waiting && one.open.is_none() {
            return None;
        }
        one.logged_in = false;
        if !needs {
            one.open = None;
            return None;
        }
        match &one.open {
            None => {
                one.open = Some(Open { at: fingerprint });
                Some(Act::Tell)
            }
            Some(open) if look.awaited && fingerprint.is_some() && open.at != fingerprint => {
                one.open = Some(Open { at: fingerprint });
                Some(Act::Retry)
            }
            Some(_) => None,
        }
    }

    /// Whether `server` refused a call with the login stored now.
    #[must_use]
    pub fn refused(&self, server: &str, stored: &Stored) -> bool {
        let Stored::Authorized(now) = stored else {
            return false;
        };
        self.0
            .get(server)
            .and_then(|one| one.refused_with.as_ref())
            .is_some_and(|with| with == now)
    }

    /// A login of `server` completed here: no refusal stands and its
    /// episode is over; the next one is told again.
    pub fn logged_in(&mut self, server: &str) {
        let one = self.0.entry(server.to_string()).or_default();
        one.refused_with = None;
        one.open = None;
        one.logged_in = true;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn look(stored: Stored, awaited: bool, refused_at_call: bool) -> Look {
        Look {
            stored,
            awaited,
            refused_at_call,
        }
    }

    fn ok(at: &str) -> Stored {
        Stored::Authorized(at.to_string())
    }

    /// Told once per episode: an expiry after a login is told again, and
    /// nothing is told while the server stays authorized.
    #[test]
    fn a_login_that_expires_later_is_told_again() {
        let mut episodes = Episodes::default();
        assert_eq!(
            episodes.step("docs", &look(Stored::Missing, false, false)),
            Some(Act::Tell)
        );
        assert_eq!(
            episodes.step("docs", &look(Stored::Missing, true, false)),
            None
        );
        // Logged in: the wait ends, the episode closes.
        assert_eq!(episodes.step("docs", &look(ok("a"), false, false)), None);
        assert_eq!(episodes.step("docs", &look(ok("a"), false, false)), None);
        // It expires while the daemon runs.
        assert_eq!(
            episodes.step("docs", &look(Stored::Expired, false, false)),
            Some(Act::Tell)
        );
        assert_eq!(
            episodes.step("docs", &look(Stored::Expired, false, false)),
            None
        );
        assert_eq!(episodes.step("docs", &look(ok("b"), false, false)), None);
        // An unreadable store decides nothing.
        assert_eq!(
            episodes.step("docs", &look(Stored::Unknown, false, false)),
            None
        );
    }

    /// A call refused with a stored login opens an episode, `/mcp` says
    /// so, and it stays refused until another login is stored.
    #[test]
    fn a_refused_call_stands_until_another_login() {
        let mut episodes = Episodes::default();
        assert_eq!(episodes.step("docs", &look(ok("a"), false, false)), None);
        assert_eq!(
            episodes.step("docs", &look(ok("a"), false, true)),
            Some(Act::Tell)
        );
        assert!(episodes.refused("docs", &ok("a")));
        assert_eq!(episodes.step("docs", &look(ok("a"), false, false)), None);
        assert!(episodes.refused("docs", &ok("a")));
        assert_eq!(episodes.step("docs", &look(ok("b"), false, false)), None);
        assert!(!episodes.refused("docs", &ok("b")));
        // A login here clears it even when the facts did not change.
        assert_eq!(
            episodes.step("docs", &look(ok("b"), false, true)),
            Some(Act::Tell)
        );
        episodes.logged_in("docs");
        assert!(!episodes.refused("docs", &ok("b")));
        assert_eq!(episodes.step("docs", &look(ok("b"), false, false)), None);
    }

    /// A login here ends the episode: the sessions' waits while their
    /// retry is under way tell nothing, a logout right after it is told.
    #[test]
    fn a_login_here_ends_the_episode_and_the_next_is_told() {
        let mut episodes = Episodes::default();
        assert_eq!(
            episodes.step("docs", &look(Stored::Missing, true, false)),
            Some(Act::Tell)
        );
        episodes.logged_in("docs");
        assert_eq!(episodes.step("docs", &look(ok("a"), true, false)), None);
        assert_eq!(episodes.step("docs", &look(ok("a"), true, false)), None);
        assert_eq!(
            episodes.step("docs", &look(Stored::Missing, false, false)),
            Some(Act::Tell)
        );
        episodes.logged_in("docs");
        assert_eq!(episodes.step("docs", &look(ok("b"), false, false)), None);
        assert_eq!(
            episodes.step("docs", &look(Stored::Missing, false, false)),
            Some(Act::Tell)
        );
    }

    /// A session waiting while a login is stored is retried only when the
    /// login changed since the episode opened: one the server refused at
    /// its connect is not retried at every look.
    #[test]
    fn a_waiting_server_is_retried_only_for_a_new_login() {
        let mut episodes = Episodes::default();
        assert_eq!(
            episodes.step("docs", &look(ok("a"), true, false)),
            Some(Act::Tell)
        );
        assert_eq!(episodes.step("docs", &look(ok("a"), true, false)), None);
        assert_eq!(
            episodes.step("docs", &look(ok("b"), true, false)),
            Some(Act::Retry)
        );
        assert_eq!(episodes.step("docs", &look(ok("b"), true, false)), None);
        assert_eq!(episodes.step("docs", &look(ok("b"), false, false)), None);
        // Missing at the start, logged in elsewhere while it waits.
        assert_eq!(
            episodes.step("kb", &look(Stored::Missing, true, false)),
            Some(Act::Tell)
        );
        assert_eq!(
            episodes.step("kb", &look(ok("c"), true, false)),
            Some(Act::Retry)
        );
    }
}
