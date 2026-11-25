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
//!
//! `--mcp-server` adds servers of the command line's
//! ([`command_line_servers`], joined by [`LoadedMcp::with_command_line`]):
//! the user's own like the user's `mcp.toml`, so no trust question, and
//! each wins over a server of the same name from the files or the realm
//! config, with a start-up notice. They are taken as written: no `${VAR}`
//! is expanded (a stdio server inherits the daemon's environment).

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

impl LoadedMcp {
    /// This set with the command line's `servers` joined: each replaces a
    /// server of its name (said in a notice), the others are added.
    #[must_use]
    pub fn with_command_line(mut self, servers: Vec<McpServerConfig>) -> Self {
        for server in servers {
            if let Some(index) = self.servers.iter().position(|s| s.name == server.name) {
                self.notices.push(format!(
                    "--mcp-server `{}` replaces the MCP server of that name from mcp.toml or the realm config",
                    server.name
                ));
                self.servers[index] = server;
            } else {
                self.servers.push(server);
            }
        }
        self
    }
}

/// The servers of the `--mcp-server` values `values`: each the body of one
/// TOML inline table with the fields of one `[[servers]]` entry of
/// `mcp.toml` (the engine's [`McpServerConfig`], stdio or HTTP), checked
/// as the engine checks a file's (its access policy, its working
/// directory) and taken as written: a value holding `${` is refused,
/// since nothing expands it.
///
/// # Errors
///
/// [`TuiError::McpServerFlag`] naming which value and why, never a value
/// of it (an `env` or header value may be a secret): it does not read as
/// one server, a check refuses it, or two values name one server.
pub fn command_line_servers(values: &[String]) -> Result<Vec<McpServerConfig>, TuiError> {
    /// The document a value is read as: `server = { <value> }`.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct One {
        server: McpServerConfig,
    }
    const OPEN: &str = "server = { ";
    let mut servers: Vec<McpServerConfig> = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let refuse = |reason: String| TuiError::McpServerFlag {
            position: index + 1,
            reason,
        };
        // One line: a newline in the value would end the inline table.
        if value.contains(['\n', '\r']) {
            return Err(refuse(
                "it holds a line break; give the table's fields on one line".to_string(),
            ));
        }
        let One { server } = toml::from_str(&format!("{OPEN}{value} }}"))
            .map_err(|err| refuse(toml_reason(&err, OPEN.len())))?;
        let named = |reason: &dyn std::fmt::Display| refuse(format!("{reason}"));
        server.validate_access().map_err(|err| named(&err))?;
        server.validate_cwd().map_err(|err| named(&err))?;
        if let Err(reference) = meerkat_core::mcp_config::reject_realm_server_env_references(
            std::slice::from_ref(&server),
        ) {
            return Err(refuse(format!(
                "server `{}` has `${{` in its {}; a command-line server is taken as written and never expanded (a stdio server inherits the daemon's environment: let it read the variable itself)",
                server.name, reference.field
            )));
        }
        if servers.iter().any(|s| s.name == server.name) {
            return Err(refuse(format!(
                "a server named `{}` is given by an earlier --mcp-server too; give each server once",
                server.name
            )));
        }
        servers.push(server);
    }
    Ok(servers)
}

/// A TOML reader's error on a `--mcp-server` value read after `offset`
/// bytes of its document: the column in the value and what is wrong,
/// without the value the reader quotes (as the engine words a file's,
/// `meerkat_core::redact`, whose helper is the crate's own).
fn toml_reason(err: &toml::de::Error, offset: usize) -> String {
    let message = err.message();
    let mut what = message.to_string();
    for prefix in ["invalid type: ", "invalid value: "] {
        if let Some(rest) = message.strip_prefix(prefix) {
            // `<kind> <value>, expected <what>`: the kind is the words
            // before the value's opening quote.
            let (unexpected, expected) = match rest.rsplit_once(", expected ") {
                Some((unexpected, expected)) => (unexpected, Some(expected)),
                None => (rest, None),
            };
            let kind = unexpected
                .split(['`', '"'])
                .next()
                .unwrap_or_default()
                .trim_end();
            what = match expected {
                Some(expected) => format!("{prefix}{kind}, expected {expected}"),
                None => format!("{prefix}{kind}"),
            };
        }
    }
    if let Some(rest) = message.strip_prefix("unknown variant `") {
        what = match rest.rsplit_once("`, expected ") {
            Some((_, expected)) => format!("unknown variant, expected {expected}"),
            None => "unknown variant".to_string(),
        };
    }
    match err.span() {
        Some(span) if span.start >= offset => {
            format!("at column {}: {what}", span.start - offset + 1)
        }
        _ => what,
    }
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

    fn values(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    /// A `--mcp-server` value is one `[[servers]]` entry as an inline
    /// table: stdio with its args and env, HTTP with its headers.
    #[test]
    fn a_command_line_server_reads_as_one_mcp_toml_entry() {
        use meerkat_core::mcp_config::McpTransportConfig;
        let servers = command_line_servers(&values(&[
            r#"name="search", command="/opt/wrapper/tool", args=["mcp","search"], env={ MODE="ci" }"#,
            r#"name = "docs", url = "https://docs.example/mcp", headers = { X-Team = "t" }"#,
        ]))
        .unwrap();
        assert_eq!(servers.len(), 2);
        let McpTransportConfig::Stdio(stdio) = &servers[0].transport else {
            panic!("{:?}", servers[0]);
        };
        assert_eq!(servers[0].name, "search");
        assert_eq!(stdio.command, "/opt/wrapper/tool");
        assert_eq!(stdio.args, ["mcp", "search"]);
        assert_eq!(stdio.env.get("MODE").map(String::as_str), Some("ci"));
        let McpTransportConfig::Http(http) = &servers[1].transport else {
            panic!("{:?}", servers[1]);
        };
        assert_eq!(http.url, "https://docs.example/mcp");
        assert!(command_line_servers(&[]).unwrap().is_empty());
    }

    /// A value that is not one server is refused naming which flag and
    /// why, and never quoting what it holds; so are `${`, a line break,
    /// a key beside the table and a name given twice.
    #[test]
    fn a_bad_command_line_server_is_refused_without_its_values() {
        let reason = |given: &[&str]| match command_line_servers(&values(given)) {
            Err(err @ TuiError::McpServerFlag { .. }) => err.to_string(),
            other => panic!("{given:?}: {other:?}"),
        };
        let secret = "s3cr3t-value";
        for given in [
            format!(r#"name="search", command="/opt/wrapper/tool", env="{secret}""#),
            format!(r#"name="search", command="/opt/wrapper/tool", args="{secret}""#),
            format!(r#"name="search", command=["{secret}"]"#),
            format!(r#"name="search", url="https://h/mcp", headers={{ A="{secret}" }}, nope=1"#),
            format!(r#"name="search", command="/opt/wrapper/tool", env={{ T="{secret}" "#),
        ] {
            let text = reason(&[&given]);
            assert!(text.starts_with("--mcp-server (number 1"), "{text}");
            assert!(!text.contains(secret), "{text}");
        }
        assert!(reason(&["name=\"search\"\ncommand=\"/x\""]).contains("line break"));
        let text = reason(&[r#"name="search", command="/opt/wrapper/tool", env={ T="${TOKEN}" }"#]);
        assert!(text.contains("`${` in its env"), "{text}");
        let text = reason(&[
            r#"name="search", command="/a""#,
            r#"name="search", command="/b""#,
        ]);
        assert!(text.starts_with("--mcp-server (number 2"), "{text}");
        assert!(text.contains("given by an earlier --mcp-server"), "{text}");
        // A value that closes the table to add keys beside it does not
        // read: the document is one key and its table.
        let text = reason(&[r#"name="search", command="/a" }, other = { x = 1"#]);
        assert!(
            text.contains("at column 30: unexpected key or value"),
            "{text}"
        );
        let text = reason(&[r#"name="search", command="/a" }
other = { x = 1"#]);
        assert!(text.contains("line break"), "{text}");
    }

    /// The command line's servers join the files': one of a name already
    /// there replaces it, with a notice; the others are added.
    #[tokio::test]
    async fn the_command_line_wins_over_a_file_server_of_its_name() {
        let (_dir, _ws, scope) = fixture();
        let flagged = inputs(
            &scope,
            HostTrustRecord::in_memory(&scope.context_root),
            true,
        );
        let loaded = load(&scope, &Config::default(), &flagged)
            .await
            .unwrap()
            .with_command_line(
                command_line_servers(&values(&[
                    r#"name="usr", command="/opt/wrapper/usr""#,
                    r#"name="search", command="/opt/wrapper/tool""#,
                ]))
                .unwrap(),
            );
        assert_eq!(names(&loaded), ["search", "usr"]);
        let usr = loaded.servers.iter().find(|s| s.name == "usr").unwrap();
        assert!(
            matches!(&usr.transport, meerkat_core::mcp_config::McpTransportConfig::Stdio(s) if s.command == "/opt/wrapper/usr"),
            "{usr:?}"
        );
        assert!(
            loaded
                .notices
                .iter()
                .any(|n| n.starts_with("--mcp-server `usr` replaces")),
            "{:?}",
            loaded.notices
        );
        assert!(!loaded.notices.iter().any(|n| n.contains("`search`")));
    }
}
