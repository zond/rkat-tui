//! The MCP servers of the TUI's sessions.
//!
//! The effective set is upstream's one owner of it,
//! [`McpConfig::effective_servers_from_roots`]: the realm config's
//! `[[tools.mcp_servers]]` plus the project `.rkat/mcp.toml` (at the
//! workspace) and the user `~/.rkat/mcp.toml` (at the user-config root),
//! project winning over user by name. It is loaded once at start-up and
//! handed to every session build as its declarative servers
//! (`SessionBuildOptions::mcp_servers`): the factory then builds the
//! session-owned router, connects the servers in the background (the model
//! sees the native `[MCP_PENDING]` notice while they connect) and, with
//! `--wait-for-mcp` (`FactoryAgentBuilder::wait_for_mcp`), waits for them
//! before the first turn.
//!
//! Every MCP tool call passes the approval gate like any other tool: an MCP
//! tool is not a builtin the policy knows, so it reaches the classifier
//! unless a user rule allows or denies it by name.
//!
//! Local stdio servers run as the trusted host (see
//! [`super::confinement`]). The project `mcp.toml` is a project input
//! ([`super::project_trust`]): it may have come with the repository, and a
//! confined shell can write it on Linux. Its servers therefore start only
//! when the file is trusted: `--trust-project` for this run, or the host
//! trust record holding the file's digest after `/trust`. The file is read
//! once ([`ProjectInputs::read_mcp`]: no symlinks, a regular file) and
//! judged on those bytes; trusted, the engine loads a private copy of
//! exactly them from a directory under the realm, never the workspace's
//! path again, so a writer that swaps the file after the check starts
//! nothing (hooks have the same closure through `HookSourceTrust`).
//! Otherwise upstream's trusting loader
//! ([`McpConfig::effective_servers_from_roots_trusting`]) refuses each
//! server of an untrusted `mcp.toml` (unless a trusted source states the
//! same definition), and [`LoadedMcp::refused`] lists them for the
//! start-up notice.

use std::path::Path;

use meerkat_core::mcp_config::{McpConfig, McpScope, McpServerSource, project_mcp_path_in};
use meerkat_core::{Config, McpServerConfig};

use super::project_trust::{ProjectInputs, ProjectMcp, Refused};
use super::scope::RuntimeScope;
use crate::error::TuiError;

/// The loaded MCP set and what to tell the user about it.
#[derive(Debug, Default)]
pub struct LoadedMcp {
    /// Servers every session build carries.
    pub servers: Vec<McpServerConfig>,
    /// Servers of untrusted project inputs that do not start.
    pub refused: Vec<Refused>,
    /// Start-up notices (project-scoped servers, the build's MCP support).
    pub notices: Vec<String>,
}

/// Load the effective MCP set for `scope` (see the module docs): the
/// project's servers only when `inputs` trusts its file.
///
/// # Errors
///
/// [`TuiError::McpConfig`] when an `mcp.toml` cannot be read or parsed, or the
/// realm config and a file define one server name differently.
pub async fn load(
    scope: &RuntimeScope,
    config: &Config,
    inputs: &ProjectInputs,
) -> Result<LoadedMcp, TuiError> {
    load_judged(scope, config, inputs, inputs.read_mcp()).await
}

/// [`load`] with the project file as it was read and judged (`judged`).
async fn load_judged(
    scope: &RuntimeScope,
    config: &Config,
    inputs: &ProjectInputs,
    judged: ProjectMcp,
) -> Result<LoadedMcp, TuiError> {
    let mut refused = Vec::new();
    // The project's file is read once and judged on those bytes; trusted,
    // the engine loads a private copy of exactly them (in the realm, out
    // of the shell's reach), never the workspace's path again.
    let (snapshot, trusted) = match judged {
        ProjectMcp::Trusted(Some(bytes)) => {
            (Some(Snapshot::write(&scope.realm_root(), &bytes)?), true)
        }
        ProjectMcp::Trusted(None) => (None, true),
        ProjectMcp::Untrusted => (None, false),
    };
    let context_root = match &snapshot {
        Some(snapshot) => Some(snapshot.root()),
        // Untrusted: read as written and never expanded, each server
        // refused (the engine's trusting loader).
        None if !trusted => Some(scope.context_root.as_path()),
        None => None,
    };
    let project_file = context_root.map(project_mcp_path_in);
    let untrusted = |path: &Path| !trusted && Some(path) == project_file.as_deref();
    let effective = McpConfig::effective_servers_from_roots_trusting(
        &config.tools.mcp_servers,
        context_root,
        scope.user_config_root.as_deref(),
        &untrusted,
    )
    .await;
    drop(snapshot);
    let effective = effective?;
    let servers = effective.admitted;
    refused.extend(
        effective
            .refused
            .iter()
            .map(|r| Refused::mcp_server(inputs.mcp_file(), &r.server.name)),
    );
    let project: Vec<&str> = servers
        .iter()
        .filter(|s| s.source == McpServerSource::File(McpScope::Project))
        .map(|s| s.server.name.as_str())
        .collect();
    let mut notices = Vec::new();
    if !project.is_empty() {
        notices.push(format!(
            "MCP servers from the project's .rkat/mcp.toml start with your privileges: {}",
            project.join(", ")
        ));
    }
    Ok(LoadedMcp {
        servers: servers.into_iter().map(|s| s.server).collect(),
        refused,
        notices,
    })
}

/// A private copy of the trusted project `mcp.toml`'s bytes, laid out as a
/// conventions root (`<dir>/.rkat/mcp.toml`) in a fresh directory under the
/// realm, which neither the agent's shell nor its file tools reach. Removed
/// when dropped (it is read once, at start-up).
struct Snapshot {
    dir: tempfile::TempDir,
}

impl Snapshot {
    fn write(realm: &Path, bytes: &[u8]) -> Result<Self, TuiError> {
        let failed = |err| TuiError::io("copy the trusted project mcp.toml into the realm", err);
        std::fs::create_dir_all(realm).map_err(failed)?;
        let dir = tempfile::Builder::new()
            .prefix("trusted-mcp-")
            .tempdir_in(realm)
            .map_err(failed)?;
        let file = project_mcp_path_in(dir.path());
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(failed)?;
        }
        std::fs::write(&file, bytes).map_err(failed)?;
        Ok(Self { dir })
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bootstrap::trust::HostTrustRecord;
    use clap::Parser;
    use std::sync::Arc;

    fn inputs(scope: &RuntimeScope, record: HostTrustRecord, flag: bool) -> ProjectInputs {
        ProjectInputs::new(scope, Arc::new(parking_lot::Mutex::new(record)), flag)
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, RuntimeScope) {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let ws = base.join("ws");
        let home = base.join("home");
        std::fs::create_dir_all(ws.join(".rkat")).unwrap();
        std::fs::create_dir_all(home.join(".rkat")).unwrap();
        std::fs::write(
            home.join(".rkat/mcp.toml"),
            "[[servers]]\nname = \"usr\"\ncommand = \"user-server\"\n",
        )
        .unwrap();
        let args = crate::cli::Args::try_parse_from([
            "rkat-tui",
            "-w",
            ws.to_str().unwrap(),
            "--user-config-root",
            home.to_str().unwrap(),
            "--config-dir",
            base.join("cfg").to_str().unwrap(),
        ])
        .unwrap();
        let scope = super::super::resolve_scope(&args).unwrap();
        (dir, ws, scope)
    }

    fn names(loaded: &LoadedMcp) -> Vec<String> {
        let mut names: Vec<_> = loaded.servers.iter().map(|s| s.name.clone()).collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn project_and_user_files_compose() {
        let (_dir, ws, scope) = fixture();
        let project = ws.join(".rkat/mcp.toml");
        std::fs::write(
            &project,
            "[[servers]]\nname = \"proj\"\ncommand = \"proj-server\"\n",
        )
        .unwrap();
        let flagged = inputs(
            &scope,
            HostTrustRecord::in_memory(&scope.context_root),
            true,
        );
        let loaded = load(&scope, &Config::default(), &flagged).await.unwrap();
        assert!(loaded.refused.is_empty());
        assert_eq!(names(&loaded), ["proj", "usr"]);
        assert_eq!(loaded.notices.len(), 1);
        assert!(
            loaded.notices[0].ends_with(": proj"),
            "{:?}",
            loaded.notices
        );

        // Untrusted: the project's servers are refused and listed; the
        // user's still start.
        let untrusted = inputs(
            &scope,
            HostTrustRecord::in_memory(&scope.context_root),
            false,
        );
        assert!(!untrusted.mcp_trusted());
        let loaded = load(&scope, &Config::default(), &untrusted).await.unwrap();
        assert_eq!(loaded.refused, [Refused::mcp_server(&project, "proj")]);
        assert_eq!(names(&loaded), ["usr"]);
        // `/trust` records the digest: trusted until the file changes.
        untrusted.trust_all().unwrap();
        assert!(untrusted.mcp_trusted());
        let loaded = load(&scope, &Config::default(), &untrusted).await.unwrap();
        assert!(loaded.refused.is_empty());
        std::fs::write(&project, "not toml [").unwrap();
        assert!(!untrusted.mcp_trusted());
        assert!(matches!(
            load(&scope, &Config::default(), &flagged).await,
            Err(TuiError::McpConfig(_))
        ));
    }

    /// The project's servers come from the bytes that were judged trusted,
    /// never from a second read of the workspace's file: a writer that
    /// swaps the file after the check (the confined shell may write
    /// `.rkat/`) starts nothing of its own, and the private copy is gone
    /// after the load.
    #[tokio::test]
    async fn the_servers_come_from_the_bytes_judged_not_a_second_read() {
        let (_dir, ws, scope) = fixture();
        let project = ws.join(".rkat/mcp.toml");
        std::fs::write(
            &project,
            "[[servers]]\nname = \"proj\"\ncommand = \"proj-server\"\n",
        )
        .unwrap();
        let inputs = inputs(
            &scope,
            HostTrustRecord::in_memory(&scope.context_root),
            false,
        );
        inputs.trust_all().unwrap();
        let judged = inputs.read_mcp();
        assert!(matches!(judged, ProjectMcp::Trusted(Some(_))));
        // Swapped between the check and the load.
        std::fs::write(
            &project,
            "[[servers]]\nname = \"evil\"\ncommand = \"evil-server\"\n",
        )
        .unwrap();
        let loaded = load_judged(&scope, &Config::default(), &inputs, judged)
            .await
            .unwrap();
        assert_eq!(names(&loaded), ["proj", "usr"]);
        let left: Vec<_> = std::fs::read_dir(scope.realm_root())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("trusted-mcp-"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        // Judged now, the swapped file is untrusted: nothing of it starts.
        let loaded = load(&scope, &Config::default(), &inputs).await.unwrap();
        assert_eq!(names(&loaded), ["usr"]);
        assert_eq!(loaded.refused, [Refused::mcp_server(&project, "evil")]);
    }
}
