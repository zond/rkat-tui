//! Leaving the daemon: `/quit` (decided by the daemon), the departure the
//! UI chose, and `/daemon stop`.

use super::*;
use crate::daemon::endpoint::{Reach, reach};

impl Runner {
    /// `/quit`: the daemon decides how this client leaves and does it
    /// (`rkat/quit`); answered with [`Msg::QuitDecided`]. What it did is
    /// said now and again on the terminal; when it asks, the UI's dialog
    /// answers with [`Effect::Leave`].
    pub(super) fn decide_quit(&mut self) {
        // The daemon is gone already (`/daemon stop`): just leave.
        let connection = match (self.conn.stopping(), self.connection()) {
            (true, _) => {
                self.send(Msg::QuitDecided(QuitDecision::Leave));
                return;
            }
            (false, None) => {
                self.send(Msg::QuitDecided(QuitDecision::Ask {
                    reason: "this rkat-tui is not attached to it, so it could not say".to_string(),
                    running: false,
                }));
                return;
            }
            (false, Some(connection)) => connection,
        };
        let endpoint = self.conn.link().map(|link| link.endpoint.clone());
        let pid = endpoint.as_ref().map_or(0, |endpoint| endpoint.pid);
        let agent = connection.agent().to_string();
        let files = self.config.endpoint_files.clone();
        let here = self.config.address.is_none();
        let say = self.say();
        let reports = self.reporter();
        tokio::spawn(async move {
            let mut stopping = false;
            let decided = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Quit>(&connection, &protocol::Empty {}),
            )
            .await;
            let decision = match decided {
                Ok(protocol::QuitOutcome::Left { others }) => {
                    say.farewell(
                    NoticeLevel::Info,
                    format!(
                        "{others} other client{} attached to this agent: left; the agent and the daemon (pid {pid}) keep running for {}",
                        if others == 1 { " is" } else { "s are" },
                        if others == 1 { "it" } else { "them" },
                    ),
                );
                    QuitDecision::Leave
                }
                Ok(protocol::QuitOutcome::Retired { retired }) => {
                    let schedules = if retired.schedules_deleted.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; its schedules were deleted: {}",
                            retired.schedules_deleted.join(", ")
                        )
                    };
                    say.farewell(
                    NoticeLevel::Info,
                    format!(
                        "agent {agent} retired; its conversation stays (rkat-tui --resume lists it){schedules}; the daemon (pid {pid}) keeps running for the other clients"
                    ),
                );
                    QuitDecision::Retire
                }
                Ok(protocol::QuitOutcome::Terminated) => {
                    stopping = true;
                    say.farewell(
                    NoticeLevel::Info,
                    format!(
                        "stopped the daemon (pid {pid}); its agents retired, their conversations stay"
                    ),
                );
                    QuitDecision::Terminate
                }
                Ok(protocol::QuitOutcome::Ask { reason, running }) => {
                    QuitDecision::Ask { reason, running }
                }
                // The connection dropped before the answer: a stop the daemon
                // decided ends it, and only its endpoint file going proves that.
                Err(err) if unanswered(&err) => {
                    if stopped_unconfirmed(&files, endpoint.as_ref(), here).await {
                        stopping = true;
                        say.farewell(NoticeLevel::Info, format!("stopped the daemon (pid {pid})"));
                        QuitDecision::Terminate
                    } else {
                        QuitDecision::Ask {
                            reason: format!("it did not say how to leave ({err})"),
                            running: false,
                        }
                    }
                }
                Err(err) => QuitDecision::Ask {
                    reason: format!("it did not say how to leave ({err})"),
                    running: false,
                },
            };
            reports.send(Report::QuitDecided { decision, stopping });
        });
    }

    /// Leave the daemon as the UI decided: record a detach, or stop it.
    pub(super) async fn depart(&mut self, departure: Departure) {
        let Some(connection) = self.connection() else {
            if departure == Departure::Terminate && !self.conn.stopping() {
                self.farewell(
                    NoticeLevel::Warn,
                    format!(
                        "{}: the daemon was not stopped; {}",
                        self.unattached(),
                        stop_by_hand(None)
                    ),
                );
            }
            return;
        };
        let endpoint = self.conn.link().map(|link| link.endpoint.clone());
        let pid = endpoint.as_ref().map_or(0, |endpoint| endpoint.pid);
        let empty = protocol::Empty {};
        let answered = match departure {
            Departure::Detach => bounded(
                LEAVE_TIMEOUT,
                remote::call::<protocol::method::Detach>(&connection, &empty),
            )
            .await
            .map(drop),
            Departure::Terminate => bounded(
                LEAVE_TIMEOUT,
                remote::call::<protocol::method::Shutdown>(
                    &connection,
                    &protocol::ShutdownParams { retire: true },
                ),
            )
            .await
            .map(drop),
        };
        let agent = connection.agent().to_string();
        match (departure, answered) {
            (Departure::Detach, Ok(())) => self.farewell(
                NoticeLevel::Info,
                format!("detached: agent {agent} keeps running in the daemon (pid {pid}); rkat-tui -c reattaches"),
            ),
            (Departure::Detach, Err(err)) => self.farewell(
                NoticeLevel::Warn,
                format!("detached; the daemon (pid {pid}) was not told ({err}) and keeps running"),
            ),
            (Departure::Terminate, Ok(())) => {
                self.stopped();
                self.farewell(
                    NoticeLevel::Info,
                    format!("stopped the daemon (pid {pid}); its agents retired, their conversations stay"),
                );
            }
            // No answer (the connection dropped, before or after the
            // daemon read the request, or the call timed out): whether it
            // stops is unknown until its endpoint file says so.
            (Departure::Terminate, Err(err)) if unanswered(&err) => {
                tracing::info!(error = %err, "rkat/shutdown brought no answer; checking whether the daemon ended");
                let files = self.config.endpoint_files.clone();
                let here = self.config.address.is_none();
                if stopped_unconfirmed(&files, endpoint.as_ref(), here).await {
                    self.stopped();
                    self.farewell(NoticeLevel::Info, format!("stopped the daemon (pid {pid})"));
                } else {
                    self.farewell(
                        NoticeLevel::Warn,
                        format!(
                            "the daemon (pid {pid}) may still be running: the connection dropped before it confirmed the shutdown; {}",
                            stop_by_hand(Some(pid))
                        ),
                    );
                }
            }
            (Departure::Terminate, Err(err)) => self.farewell(
                NoticeLevel::Error,
                format!(
                    "the daemon (pid {pid}) was not stopped: {err}; {}",
                    stop_by_hand(Some(pid))
                ),
            ),
        }
    }

    /// `/daemon stop`: ask the attached daemon to shut down
    /// (`rkat/shutdown`, its agents kept). Without a connection nothing is
    /// asked: the user is told how to stop it by hand.
    pub(super) fn daemon_stop(&mut self) {
        let Some(connection) = self.connection() else {
            self.notice(
                NoticeLevel::Warn,
                format!(
                    "{}: the daemon cannot be asked to stop; {}",
                    self.unattached(),
                    stop_by_hand(None)
                ),
            );
            return;
        };
        let pid = self.conn.link().map(|link| link.endpoint.pid);
        let reports = self.reporter();
        tokio::spawn(async move {
            let asked = bounded(
                LEAVE_TIMEOUT,
                remote::call::<protocol::method::Shutdown>(
                    &connection,
                    &protocol::ShutdownParams::default(),
                ),
            )
            .await;
            reports.send(Report::StopAsked(match asked {
                // The daemon drops the connection as it stops.
                Ok(_) | Err(CallFailure::Failed(ClientError::Disconnected)) => {
                    Ok("asked the daemon to shut down".to_string())
                }
                Err(err) => Err(format!(
                    "the daemon was not stopped: {err}; {}",
                    stop_by_hand(pid)
                )),
            }));
        });
    }
}

/// How the user stops the daemon by hand (`pid` when known).
fn stop_by_hand(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("rkat-tui --list shows it, `kill {pid}` stops it"),
        None => "rkat-tui --list shows it and its pid, `kill <pid>` stops it".to_string(),
    }
}

/// Whether a stop request brought no answer at all (the connection
/// dropped, before or after the daemon read it, or the call timed out):
/// then only [`stopped_unconfirmed`] can tell whether it stopped.
fn unanswered(err: &CallFailure) -> bool {
    matches!(
        err,
        CallFailure::TimedOut | CallFailure::Failed(ClientError::Disconnected)
    )
}

/// After a stop request that brought no answer: whether the daemon
/// `endpoint` names ended within [`STOP_VERIFY`] ([`daemon_ended`];
/// `direct`: it was reached at its own endpoint). No endpoint is no
/// evidence.
async fn stopped_unconfirmed(files: &[PathBuf], endpoint: Option<&Endpoint>, direct: bool) -> bool {
    match endpoint {
        Some(endpoint) => daemon_ended(files, endpoint, direct, STOP_VERIFY).await,
        None => false,
    }
}

/// Whether the daemon `endpoint` names ended within `wait`. Evidence that
/// it did: no endpoint file at `files` names it any more (each is gone,
/// or names another daemon), its process is gone, or a connect to its
/// address is refused outright ([`Reach::Refused`]). A file that cannot
/// be read or parsed is no evidence either way, and neither is a connect
/// that timed out or failed otherwise ([`Reach::Unknown`]: a loaded host
/// or a local socket error says nothing about the daemon). `direct`: the
/// daemon was reached at its own endpoint, not by address
/// (`--attach=HOST:PORT`). Through a tunnel neither its pid (perhaps
/// another machine's) nor the local port (the forward may have exited
/// while the daemon runs on) says anything about the daemon, so only the
/// endpoint files count there. No files is no evidence.
/// How long [`daemon_ended`] waits for one connect to its address.
const PROBE_WAIT: Duration = Duration::from_secs(1);

pub(crate) async fn daemon_ended(
    files: &[PathBuf],
    endpoint: &Endpoint,
    direct: bool,
    wait: Duration,
) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if no_file_names(files, &endpoint.token)
            || (direct
                && (crate::client::list::pid_gone(endpoint.pid)
                    || reach(endpoint.address, PROBE_WAIT).await == Reach::Refused))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether every file of `files` is gone or names a daemon with another
/// token (an unreadable or malformed file counts as possibly naming it).
pub(super) fn no_file_names(files: &[PathBuf], token: &str) -> bool {
    !files.is_empty()
        && files.iter().all(|file| match std::fs::metadata(file) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
            Ok(_) => Endpoint::read(file).is_ok_and(|now| now.token != token),
        })
}
