//! Leaving the daemon: `/quit` (decided by the daemon), the departure the
//! UI chose, and `/daemon stop`.

use super::link::{Probe, probe_address};
use super::*;

impl Runner {
    /// `/quit`: the daemon decides how this client leaves and does it
    /// (`rkat/quit`); answered with [`Msg::QuitDecided`]. What it did is
    /// said now and again on the terminal; when it asks, the UI's dialog
    /// answers with [`Effect::Leave`].
    pub(super) fn decide_quit(&mut self) {
        // The daemon is gone already (`/daemon stop`): just leave.
        let connection = match (self.stopping, self.connection()) {
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
        let endpoint = self.link.as_ref().map(|link| link.endpoint.clone());
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
                Err(
                    err @ (CallFailure::TimedOut | CallFailure::Failed(ClientError::Disconnected)),
                ) => {
                    let ended = match &endpoint {
                        Some(endpoint) => daemon_ended(&files, endpoint, here, STOP_VERIFY).await,
                        None => false,
                    };
                    if ended {
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
            if departure == Departure::Terminate && !self.stopping {
                self.farewell(
                    NoticeLevel::Warn,
                    format!(
                        "{}: the daemon was not stopped (/daemon stop from an attached rkat-tui, or SIGTERM)",
                        self.unattached()
                    ),
                );
            }
            return;
        };
        let endpoint = self.link.as_ref().map(|link| link.endpoint.clone());
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
                self.stopping = true;
                self.farewell(
                    NoticeLevel::Info,
                    format!("stopped the daemon (pid {pid}); its agents retired, their conversations stay"),
                );
            }
            // No answer (the connection dropped, before or after the
            // daemon read the request, or the call timed out): whether it
            // stops is unknown until its endpoint file says so.
            (
                Departure::Terminate,
                Err(
                    err @ (CallFailure::TimedOut
                    | CallFailure::Failed(ClientError::Disconnected)),
                ),
            ) => {
                tracing::info!(error = %err, "rkat/shutdown brought no answer; checking whether the daemon ended");
                let ended = match &endpoint {
                    Some(endpoint) => {
                        daemon_ended(
                            &self.config.endpoint_files,
                            endpoint,
                            self.config.address.is_none(),
                            STOP_VERIFY,
                        )
                        .await
                    }
                    None => false,
                };
                if ended {
                    self.stopping = true;
                    self.farewell(NoticeLevel::Info, format!("stopped the daemon (pid {pid})"));
                } else {
                    self.farewell(
                        NoticeLevel::Warn,
                        format!(
                            "the daemon (pid {pid}) may still be running: the connection dropped before it confirmed the shutdown; rkat-tui --list shows it, /daemon stop from an attached rkat-tui or `kill {pid}` stops it"
                        ),
                    );
                }
            }
            // A daemon that refused `rkat/shutdown` (one without it):
            // SIGTERM, as `/daemon stop`.
            (Departure::Terminate, Err(err)) => {
                tracing::info!(error = %err, "rkat/shutdown failed; sending SIGTERM");
                match self.sigterm_attached().await {
                    Ok(pid) => self.farewell(
                        NoticeLevel::Info,
                        format!("asked the daemon (pid {pid}) to shut down"),
                    ),
                    Err(why) => self.farewell(
                        NoticeLevel::Error,
                        format!("the daemon was not stopped: {why}"),
                    ),
                }
            }
        }
    }

    /// `/daemon stop`: SIGTERM to the pid the endpoint file names, once the
    /// file still names the daemon this client is attached to; a daemon
    /// reached by address (its pid may be another machine's) is asked.
    pub(super) fn daemon_stop(&mut self) {
        let reports = self.reporter();
        if self.config.address.is_some() {
            let Some(connection) = self.link_or_say("the daemon was not asked to stop") else {
                return;
            };
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
                    Ok(_) | Err(CallFailure::Failed(ClientError::Disconnected)) => {
                        Ok("asked the daemon to shut down".to_string())
                    }
                    Err(err) => Err(format!("the daemon was not stopped: {err}")),
                }));
            });
            return;
        }
        let files = self.config.endpoint_files.clone();
        let connected = self.link.as_ref().map(|link| link.endpoint.address);
        tokio::spawn(async move {
            let signalled = sigterm(&files, connected)
                .await
                .map(|pid| format!("asked the daemon (pid {pid}) to shut down"));
            reports.send(Report::StopAsked(signalled));
        });
    }

    /// SIGTERM to the daemon this client is attached to (the endpoint
    /// file's pid, when its address is the attached one).
    pub(super) async fn sigterm_attached(&mut self) -> Result<u32, String> {
        if self.config.address.is_some() {
            return Err(
                "the daemon was reached by address: its pid may be another machine's, so it is not signalled; /daemon stop asks it to shut down".to_string(),
            );
        }
        let connected = self.link.as_ref().map(|link| link.endpoint.address);
        let pid = sigterm(&self.config.endpoint_files, connected).await?;
        self.stopping = true;
        Ok(pid)
    }
}

/// SIGTERM to the daemon the endpoint `files` name, when its address is
/// `connected` (the one this client is attached to).
pub(super) async fn sigterm(
    files: &[PathBuf],
    connected: Option<std::net::SocketAddr>,
) -> Result<u32, String> {
    let liveness = launch::probe_any(files).await;
    let pid = stop_target(&liveness, connected)?;
    terminate(pid).map_err(|err| format!("cannot stop the daemon: {err}"))?;
    Ok(pid)
}

/// Whether the daemon `endpoint` names ended within `wait`. Evidence that
/// it did: no endpoint file at `files` names it any more (each is gone,
/// or names another daemon), its process is gone, or a connect to its
/// address is refused outright ([`Probe::Refused`]). A file that cannot
/// be read or parsed is no evidence either way, and neither is a connect
/// that timed out or failed otherwise ([`Probe::Unknown`]: a loaded host
/// or a local socket error says nothing about the daemon). `direct`: the
/// daemon was reached at its own endpoint, not by address
/// (`--attach=HOST:PORT`). Through a tunnel neither its pid (perhaps
/// another machine's) nor the local port (the forward may have exited
/// while the daemon runs on) says anything about the daemon, so only the
/// endpoint files count there. No files is no evidence.
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
                    || probe_address(endpoint.address) == Probe::Refused))
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

/// The pid `/daemon stop` signals: the endpoint file's, when its address
/// still answers and is the one this client is attached to.
///
/// # Errors
///
/// Why nothing is signalled.
pub fn stop_target(
    liveness: &launch::Liveness,
    connected: Option<std::net::SocketAddr>,
) -> Result<u32, String> {
    match (liveness, connected) {
        (launch::Liveness::Live(endpoint), Some(address)) if endpoint.address == address => {
            Ok(endpoint.pid)
        }
        (launch::Liveness::Live(endpoint), _) => Err(format!(
            "the endpoint file names a daemon at {} this client is not attached to; not stopping it",
            endpoint.address
        )),
        (launch::Liveness::Stale(_) | launch::Liveness::Absent, _) => {
            Err("no daemon answers for this workspace".to_string())
        }
    }
}

#[cfg(unix)]
pub(super) fn terminate(pid: u32) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| format!("pid {pid} is out of range"))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|err| err.to_string())
}

#[cfg(not(unix))]
pub(super) fn terminate(_pid: u32) -> Result<(), String> {
    Err("stopping the daemon is supported on Unix only".to_string())
}
