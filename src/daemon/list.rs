//! `--list` (`rkat-tui --list`, `rkat-tui-mob --list`): every daemon whose
//! endpoint file exists, one line each, then every workspace's realm under
//! the config directory ([`realm_lines`]: id, workspace, last use, daemon).
//!
//! The files are found where the daemon writes them
//! ([`super::endpoint::search_dirs`]: `<state>/rkat-tui/*/mob.endpoint`,
//! then the runtime-dir fallback), or the one `--endpoint-file` names. Each
//! is probed: connect, `rkat/attach` with its token as a probe (not
//! counted as a client), then `rkat/status`. A file whose address refuses
//! the connect is `stale`; it is removed when its pid is gone. One whose
//! connect times out (or fails otherwise) is `unknown (<why>)` and kept:
//! that proves nothing. The token is never printed.
//!
//! A line is tab-separated, in a fixed order: workspace (followed by
//! `(realm <root>)` or `(realm memory)` when the daemon serves another
//! realm than the workspace's own), address, `pid N`,
//! `up <uptime>`, then for a live daemon `<n> client(s)` and `<m>
//! agent(s)`, for a stale one `stale` and whether its file was removed.
//! Under a live daemon's line, one line per agent, indented by a tab: its
//! name, `running` or `idle`, its conversation, `<k> client(s)`, and
//! `detached` when it runs without one. A daemon of before P5 (one agent,
//! no agent list) gets the conversation and `running` or `idle` on its own
//! line instead.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::endpoint::{self, Endpoint};
use super::protocol::{self, Status};
use crate::bootstrap::GlobalDirs;
use crate::bootstrap::realm_home::{self, Home};

/// How long one daemon may take to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// It answered; its status, when it could say.
    Live(Option<Status>),
    /// The connect was refused; whether the file was removed (its pid is
    /// gone).
    Stale {
        /// The file was removed.
        removed: bool,
    },
    /// Whether anything serves there could not be told (the connect timed
    /// out, the network is unreachable): never removed.
    Unknown(String),
}

/// One listed daemon.
#[derive(Debug, Clone)]
pub struct Listed {
    /// What it says.
    pub endpoint: Endpoint,
    /// What the probe found.
    pub found: Found,
}

/// The endpoint files to list: `explicit` alone, or every one under the
/// search directories, sorted.
#[must_use]
pub fn files(explicit: Option<&Path>) -> Vec<PathBuf> {
    if let Some(file) = explicit {
        return vec![file.to_path_buf()];
    }
    files_in(&endpoint::search_dirs())
}

/// The endpoint files one level under each of `dirs`, sorted.
#[must_use]
pub fn files_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path().join(super::ENDPOINT_NAME))
        .filter(|file| file.is_file())
        .collect();
    files.sort();
    files
}

/// Probe the daemon `endpoint` names.
pub async fn probe(endpoint: &Endpoint) -> Found {
    let attached = tokio::time::timeout(
        PROBE_TIMEOUT,
        crate::client::transport::Connection::attach_as(endpoint, PROBE_TIMEOUT, None, true),
    )
    .await;
    let Ok(Ok((connection, _notes))) = attached else {
        // Something may accept but refuse (another program on the port):
        // only a refused connect is stale.
        return match endpoint::reach(endpoint).await {
            endpoint::Reach::Accepts => Found::Live(None),
            endpoint::Reach::Refused => Found::Stale { removed: false },
            endpoint::Reach::Unknown(why) => Found::Unknown(why),
        };
    };
    let status = tokio::time::timeout(
        PROBE_TIMEOUT,
        crate::client::remote::local::<_, Status>(
            &connection,
            protocol::STATUS,
            &serde_json::json!({}),
        ),
    )
    .await;
    Found::Live(status.ok().and_then(Result::ok))
}

/// Whether process `pid` is gone.
pub(crate) fn pid_gone(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        )
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Probe every file of [`files`]; a stale one whose pid is gone is removed.
pub async fn list(explicit: Option<&Path>) -> Vec<Listed> {
    let mut listed = Vec::new();
    for file in files(explicit) {
        let Ok(endpoint) = Endpoint::read(&file) else {
            continue;
        };
        let found = match probe(&endpoint).await {
            Found::Stale { .. } => {
                let removed = pid_gone(endpoint.pid) && std::fs::remove_file(&file).is_ok();
                Found::Stale { removed }
            }
            found => found,
        };
        listed.push(Listed { endpoint, found });
    }
    listed
}

/// `seconds` as `1d2h`, `3h4m`, `5m6s` or `7s`.
#[must_use]
pub fn uptime(seconds: u64) -> String {
    let (d, h, m, s) = (
        seconds / 86_400,
        seconds / 3_600 % 24,
        seconds / 60 % 60,
        seconds % 60,
    );
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s}s"),
        (0, _, _) => format!("{h}h{m}m"),
        _ => format!("{d}d{h}h"),
    }
}

/// One line for `listed`, its uptime counted to `now`.
#[must_use]
pub fn line(listed: &Listed, now: chrono::DateTime<chrono::Utc>) -> String {
    let endpoint = &listed.endpoint;
    let status = match &listed.found {
        Found::Live(status) => status.as_ref(),
        Found::Stale { .. } | Found::Unknown(_) => None,
    };
    let mut workspace = status
        .map(|status| status.workspace.clone())
        .or_else(|| endpoint.workspace.clone())
        .map_or_else(|| "?".to_string(), |w| w.display().to_string());
    if let Some(realm) = &endpoint.realm {
        workspace.push_str(&format!(" (realm {realm})"));
    }
    let up = endpoint
        .started()
        .and_then(|started| u64::try_from((now - started).num_seconds()).ok())
        .map_or_else(|| "?".to_string(), uptime);
    let mut fields = vec![
        workspace,
        endpoint.address.to_string(),
        format!("pid {}", endpoint.pid),
        format!("up {up}"),
    ];
    match (&listed.found, status) {
        (Found::Live(_), Some(status)) if !status.agents.is_empty() => {
            fields.push(count(status.clients, "client"));
            fields.push(count(status.agents.len(), "agent"));
        }
        (Found::Live(_), Some(status)) => {
            fields.push(format!(
                "{} client{}",
                status.clients,
                if status.clients == 1 { "" } else { "s" }
            ));
            fields.push(
                status
                    .session_id
                    .as_ref()
                    .map_or_else(|| "no conversation".to_string(), ToString::to_string),
            );
            fields.push(if status.running { "running" } else { "idle" }.to_string());
        }
        (Found::Live(_), None) => fields.push("live (no status: an older daemon)".to_string()),
        (Found::Unknown(why), _) => fields.push(format!("unknown ({why})")),
        (Found::Stale { removed }, _) => fields.push(
            if *removed {
                "stale (file removed)"
            } else {
                "stale"
            }
            .to_string(),
        ),
    }
    fields.join("\t")
}

/// `n` things, singular or plural.
fn count(n: usize, thing: &str) -> String {
    format!("{n} {thing}{}", if n == 1 { "" } else { "s" })
}

/// The lines for `listed`: the daemon's ([`line()`]), then one per agent of
/// a live one, indented by a tab.
#[must_use]
pub fn lines(listed: &Listed, now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let mut lines = vec![line(listed, now)];
    if let Found::Live(Some(status)) = &listed.found {
        for agent in &status.agents {
            let mut fields = vec![
                String::new(),
                agent.name.clone(),
                agent.run_label().to_string(),
                agent
                    .session_id
                    .as_ref()
                    .map_or_else(|| "no conversation".to_string(), ToString::to_string),
                count(agent.clients, "client"),
            ];
            if agent.detached && agent.clients == 0 {
                fields.push("detached".to_string());
            }
            lines.push(fields.join("\t"));
        }
    }
    lines
}

/// What `--list` says of the daemon of `home` among `listed`: the daemon
/// of its workspace's own realm, by the workspace path its endpoint file
/// names.
#[must_use]
pub fn daemon_of(home: &Home, listed: &[Listed]) -> String {
    let daemon = home.manifest.path.as_ref().and_then(|path| {
        listed.iter().find(|listed| {
            listed.endpoint.realm.is_none() && listed.endpoint.workspace.as_ref() == Some(path)
        })
    });
    match daemon {
        None => "no daemon".to_string(),
        Some(listed) => match &listed.found {
            Found::Live(_) => format!("daemon pid {}", listed.endpoint.pid),
            Found::Stale { .. } => "no daemon (a stale endpoint file)".to_string(),
            Found::Unknown(why) => format!("daemon unknown ({why})"),
        },
    }
}

/// One line per realm of `homes`: its id, its workspace (`(gone)` when
/// that no longer exists, or `set aside, was …`), the git remote, the last
/// use, and its daemon among `listed`, tab-separated.
#[must_use]
pub fn realm_lines(homes: &[Home], listed: &[Listed]) -> Vec<String> {
    homes
        .iter()
        .map(|home| format!("{}\t{}", home.describe(), daemon_of(home, listed)))
        .collect()
}

/// Print every daemon, its agents under it (to stderr when there is
/// none), then every workspace's realm under `global`'s config directory.
pub async fn print(explicit: Option<&Path>, global: Option<&GlobalDirs>) {
    let listed = list(explicit).await;
    if listed.is_empty() {
        eprintln!("no rkat-tui-mob daemon has an endpoint file");
    }
    let now = chrono::Utc::now();
    for listed in &listed {
        for line in lines(listed, now) {
            println!("{line}");
        }
    }
    let Some(global) = global else {
        return;
    };
    let root = realm_home::root(global);
    match realm_home::scan(&root) {
        Ok(homes) if homes.is_empty() => {
            eprintln!("no workspace has state in {}", root.display());
        }
        Ok(homes) => {
            println!();
            println!("workspace state in {}:", root.display());
            for line in realm_lines(&homes, &listed) {
                println!("{line}");
            }
        }
        Err(err) => eprintln!("rkat-tui: {err}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_files_are_one_level_under_the_search_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("rkat-tui");
        for key in ["b", "a"] {
            std::fs::create_dir_all(base.join(key)).unwrap();
            std::fs::write(base.join(key).join(crate::daemon::ENDPOINT_NAME), "{}").unwrap();
        }
        std::fs::create_dir_all(base.join("empty")).unwrap();
        assert_eq!(
            files_in(&[base.clone(), dir.path().join("missing")]),
            vec![
                base.join("a").join(crate::daemon::ENDPOINT_NAME),
                base.join("b").join(crate::daemon::ENDPOINT_NAME)
            ]
        );
    }

    #[test]
    fn uptimes_are_short() {
        assert_eq!(uptime(7), "7s");
        assert_eq!(uptime(306), "5m6s");
        assert_eq!(uptime(3 * 3600 + 4 * 60 + 5), "3h4m");
        assert_eq!(uptime(86_400 + 2 * 3600), "1d2h");
    }

    #[test]
    fn a_line_never_holds_the_token_and_keeps_its_column_order() {
        let endpoint = Endpoint {
            address: "127.0.0.1:4000".parse().unwrap(),
            token: "secret-token".into(),
            pid: 9,
            workspace: Some("/w".into()),
            started_at: Some("2026-11-09T10:00:00Z".into()),
            realm: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-09T11:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stale = Listed {
            endpoint: endpoint.clone(),
            found: Found::Stale { removed: true },
        };
        assert_eq!(
            line(&stale, now),
            "/w\t127.0.0.1:4000\tpid 9\tup 1h30m\tstale (file removed)"
        );
        let live = Listed {
            endpoint: endpoint.clone(),
            found: Found::Live(Some(Status {
                host: None,
                workspace: "/w".into(),
                pid: 9,
                started_at: String::new(),
                clients: 2,
                detached: false,
                owner: false,
                session_id: None,
                running: true,
                agent: None,
                agents: Vec::new(),
                shell: None,
            })),
        };
        let shown = line(&live, now);
        assert_eq!(
            shown,
            "/w\t127.0.0.1:4000\tpid 9\tup 1h30m\t2 clients\tno conversation\trunning"
        );
        assert!(!shown.contains("secret"));
        // A daemon with agents: one line each under its own.
        let session = meerkat_core::SessionId::new();
        let agents = Listed {
            endpoint: endpoint.clone(),
            found: Found::Live(Some(Status {
                host: None,
                workspace: "/w".into(),
                pid: 9,
                started_at: String::new(),
                clients: 1,
                detached: true,
                owner: false,
                session_id: None,
                running: false,
                agent: None,
                agents: vec![
                    crate::daemon::protocol::AgentRow {
                        name: "main-ab12".into(),
                        session_id: Some(session.clone()),
                        running: Some(true),
                        clients: 1,
                        detached: false,
                    },
                    crate::daemon::protocol::AgentRow {
                        name: "main-cd34".into(),
                        session_id: None,
                        running: None,
                        clients: 0,
                        detached: true,
                    },
                ],
                shell: None,
            })),
        };
        assert_eq!(
            lines(&agents, now),
            vec![
                "/w\t127.0.0.1:4000\tpid 9\tup 1h30m\t1 client\t2 agents".to_string(),
                format!("\tmain-ab12\trunning\t{session}\t1 client"),
                // Unknown is not idle.
                "\tmain-cd34\tunknown\tno conversation\t0 clients\tdetached".to_string(),
            ]
        );
        // A connect that timed out proves nothing: unknown, not stale.
        let silent = Listed {
            endpoint: endpoint.clone(),
            found: Found::Unknown("no answer within 2s".into()),
        };
        assert_eq!(
            line(&silent, now),
            "/w\t127.0.0.1:4000\tpid 9\tup 1h30m\tunknown (no answer within 2s)"
        );
        // Another realm than the workspace's own is named after it.
        let isolated = Listed {
            endpoint: Endpoint {
                realm: Some("/realms/x1".into()),
                ..endpoint
            },
            found: Found::Stale { removed: false },
        };
        assert_eq!(
            line(&isolated, now),
            "/w (realm /realms/x1)\t127.0.0.1:4000\tpid 9\tup 1h30m\tstale"
        );
    }
}
