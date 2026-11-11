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
//! trust record holding the file's digest after `/trust`. Otherwise
//! upstream's trusting loader
//! ([`McpConfig::effective_servers_from_roots_trusting`]) refuses each
//! server of an untrusted `mcp.toml` (unless a trusted source states the
//! same definition), and [`LoadedMcp::refused`] lists them for the
//! start-up notice.

use std::path::Path;

use meerkat_core::mcp_config::{McpConfig, McpScope, McpServerSource};
use meerkat_core::{Config, McpServerConfig};

use super::project_trust::{ProjectInputs, Refused};
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
    let mut refused = Vec::new();
    let mcp_trusted = inputs.mcp_trusted();
    let untrusted = |path: &Path| !mcp_trusted && path == inputs.mcp_file();
    let effective = McpConfig::effective_servers_from_roots_trusting(
        &config.tools.mcp_servers,
        Some(scope.context_root.as_path()),
        scope.user_config_root.as_deref(),
        &untrusted,
    )
    .await?;
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
}
