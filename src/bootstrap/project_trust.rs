//! One trust decision over the workspace files that make the host run
//! something.
//!
//! Local MCP stdio servers and command hooks run as the trusted host, outside
//! the shell's confinement. Two workspace files can define them:
//!
//! - the project `.rkat/mcp.toml` (MCP servers),
//! - the project `.rkat/config.toml`, whose `[hooks]` the factory discovers
//!   at the conventions root on every build.
//!
//! Either may have come with a cloned repository, so each is trusted only
//! when its whole content is what the host trust record
//! ([`HostTrustRecord`]) holds a digest of: what the user trusted with
//! `/trust`. `--trust-project` trusts both for one run without recording
//! anything. A file that changes after it was trusted is untrusted again,
//! with a start-up notice naming it. The workspace's realm (its
//! `config.toml` included) lives under the config directory, out of the
//! shell's and a clone's reach: it is the user's and is not a project
//! input.
//!
//! An untrusted file's executable entries do not run: [`super::mcp::load`]
//! refuses its MCP servers, and [`ProjectInputs::guard`] (applied by
//! [`super::RealmConfigStore`] to every config the factory builds a session
//! with, helpers' included) shadows each hook of the project
//! `.rkat/config.toml` with a disabled entry of the same id (the factory
//! layers hooks by id, the build's config first, so the project file's
//! entry never runs). User-global files (`~/.rkat/config.toml`,
//! `~/.rkat/mcp.toml`, `~/.rkat/tui`) are the user's own and stay trusted.
//!
//! The guard reads the project `.rkat/config.toml` before the factory reads
//! it again while building, so on its own it cannot cover content written
//! in between. [`ProjectInputs`] is therefore also the factory's
//! [`HookSourceTrust`] (installed with
//! `AgentFactory::with_hook_source_trust`): the factory asks it about the
//! exact bytes it parsed hooks from, and every hook of content whose digest
//! the record does not hold is refused at each invocation
//! (`UntrustedConfigurationSource`), whatever the guard saw.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meerkat::{HookSource, HookSourceDecision, HookSourceDigest, HookSourceTrust};
use meerkat_core::Config;
use meerkat_core::mcp_config::project_mcp_path_in;

use super::scope::RuntimeScope;
use crate::bootstrap::trust::{HostTrustRecord, TrustError, TrustStatus, Untrusted};

/// The host trust record shared by the gate and the project inputs.
pub type SharedTrustRecord = Arc<parking_lot::Mutex<HostTrustRecord>>;

/// The conventions config the factory reads hooks from, under a root.
fn conventions_config(root: &Path) -> PathBuf {
    root.join(".rkat").join("config.toml")
}

/// One project input as `/trust` shows it before recording it
/// ([`ProjectInputs::preview`]); the client sends it back with the yes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TrustPreview {
    pub file: PathBuf,
    /// The SHA-256 (lower-case hex) of the content shown; `None`: absent.
    pub digest: Option<String>,
    /// What of it would run as the host, one line each (`MCP server docs:
    /// npx docs-server --port 3000`, `hook fmt: ./fmt.sh`).
    pub runs: Vec<String>,
    /// Why it cannot be trusted at all (a symlink, not a regular file,
    /// unreadable); it is then not recorded.
    pub refused: Option<String>,
}

/// A command and its arguments as one line, each word quoted when it
/// holds anything but plain characters.
fn command_line(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|word| {
            if !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
            {
                word.to_string()
            } else {
                crate::error::shell_quote(Path::new(word))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What an `mcp.toml`'s `bytes` would start, one line per server.
fn mcp_runs(bytes: &[u8]) -> Vec<String> {
    use meerkat_core::mcp_config::{McpConfig, McpTransportConfig};
    let parsed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())
        .and_then(|text| toml::from_str::<McpConfig>(text).map_err(|err| err.to_string()));
    match parsed {
        Err(err) => vec![format!(
            "it does not read as an mcp.toml ({}); no server of it would start",
            crate::text::prefix(err.trim(), 200)
        )],
        Ok(config) => config
            .servers
            .iter()
            .map(|server| match &server.transport {
                McpTransportConfig::Stdio(stdio) => {
                    let mut line = format!(
                        "MCP server {}: {}",
                        server.name,
                        command_line(&stdio.command, &stdio.args)
                    );
                    if let Some(cwd) = &stdio.cwd {
                        let _ = write!(line, " (in {cwd})");
                    }
                    if !stdio.env.is_empty() {
                        let mut names: Vec<&str> = stdio.env.keys().map(String::as_str).collect();
                        names.sort_unstable();
                        let _ = write!(line, " (sets {})", names.join(", "));
                    }
                    line
                }
                McpTransportConfig::Http(http) => {
                    format!("MCP server {}: {}", server.name, http.url)
                }
            })
            .collect(),
    }
}

/// What a conventions config's `bytes` would run, one line per hook.
fn hook_runs(bytes: &[u8]) -> Vec<String> {
    use meerkat_core::HookAdapterConfig;
    let mut parsed = Config::default();
    let read = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())
        .and_then(|text| parsed.merge_toml_str(text).map_err(|err| err.to_string()));
    if let Err(err) = read {
        return vec![format!(
            "it does not read as a config.toml ({}); none of its hooks would run",
            crate::text::prefix(err.trim(), 200)
        )];
    }
    parsed
        .hooks
        .entries
        .iter()
        .map(|entry| {
            let runs = match &entry.runtime {
                HookAdapterConfig::Command(command) => {
                    command_line(&command.command, &command.args)
                }
                HookAdapterConfig::Http(http) => format!("{} {}", http.method, http.url),
                HookAdapterConfig::InProcess(_) => "a handler built into the host".to_string(),
            };
            format!("hook {}: {runs}", entry.id.0)
        })
        .collect()
}

/// The project `.rkat/mcp.toml` as [`ProjectInputs::read_mcp`] judged it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMcp {
    /// Trusted (recorded, or `--trust-project`): the bytes judged, `None`
    /// when there is no file.
    Trusted(Option<Vec<u8>>),
    /// Not trusted: none of its servers start.
    Untrusted,
}

/// Something of an untrusted file that was not started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The untrusted file that defines it.
    pub file: PathBuf,
    /// What it is, e.g. `MCP server docs` or `hook fmt`.
    pub what: String,
}

impl Refused {
    /// An MCP server of `file`.
    #[must_use]
    pub fn mcp_server(file: &Path, name: &str) -> Self {
        Self {
            file: file.to_path_buf(),
            what: format!("MCP server {name}"),
        }
    }

    fn hook(file: &Path, id: &str) -> Self {
        Self {
            file: file.to_path_buf(),
            what: format!("hook {id}"),
        }
    }
}

/// The executable project inputs of one workspace, and their trust (see
/// the module docs).
#[derive(Debug, Clone)]
pub struct ProjectInputs {
    record: SharedTrustRecord,
    by_flag: bool,
    mcp: PathBuf,
    hooks: PathBuf,
    /// The user-global conventions config (always trusted).
    user_hooks: Option<PathBuf>,
}

impl ProjectInputs {
    /// The inputs of `scope`, judged by `record`; `by_flag`: `--trust-project`.
    #[must_use]
    pub fn new(scope: &RuntimeScope, record: SharedTrustRecord, by_flag: bool) -> Self {
        let user_hooks = scope.user_config_root.as_deref().map(conventions_config);
        let hooks = conventions_config(&scope.context_root);
        Self {
            record,
            by_flag,
            mcp: project_mcp_path_in(&scope.context_root),
            // A workspace that is the user config root reads the user's own
            // file there.
            hooks,
            user_hooks,
        }
    }

    /// The project `.rkat/mcp.toml`.
    #[must_use]
    pub fn mcp_file(&self) -> &Path {
        &self.mcp
    }

    /// Every input file, present or not, in a fixed order.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.mcp.as_path())
            .chain((!self.hooks_are_the_users()).then_some(self.hooks.as_path()))
    }

    fn hooks_are_the_users(&self) -> bool {
        self.user_hooks.as_deref() == Some(self.hooks.as_path())
    }

    /// Whether `--trust-project` trusts every input for this run.
    #[must_use]
    pub fn by_flag(&self) -> bool {
        self.by_flag
    }

    /// The recorded trust state of `file` (the flag aside).
    #[must_use]
    pub fn status(&self, file: &Path) -> TrustStatus {
        self.record.lock().check(file)
    }

    /// Whether `file`'s entries may run: the flag is set, the file is
    /// absent, or its digest is recorded. A file that is not a project
    /// input is the user's and trusted.
    #[must_use]
    pub fn trusted(&self, file: &Path) -> bool {
        if self.by_flag || !self.files().any(|f| f == file) {
            return true;
        }
        matches!(
            self.status(file),
            TrustStatus::Missing | TrustStatus::Trusted
        )
    }

    /// Whether the project `.rkat/mcp.toml` may start servers (the load
    /// itself judges the bytes it reads, [`Self::read_mcp`]).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn mcp_trusted(&self) -> bool {
        self.trusted(&self.mcp)
    }

    /// The project `.rkat/mcp.toml` read once, judged on the bytes read
    /// ([`HostTrustRecord::read_checked`]): what [`super::mcp::load`]
    /// starts the project's servers from.
    #[must_use]
    pub fn read_mcp(&self) -> ProjectMcp {
        let (status, bytes) = self.record.lock().read_checked(&self.mcp);
        match (status, bytes) {
            (TrustStatus::Missing, _) => ProjectMcp::Trusted(None),
            (TrustStatus::Trusted, bytes) => ProjectMcp::Trusted(bytes),
            // `--trust-project` trusts the content, never a symlinked or
            // unreadable file (no bytes were read).
            (TrustStatus::Untrusted(_), Some(bytes)) if self.by_flag => {
                ProjectMcp::Trusted(Some(bytes))
            }
            (TrustStatus::Untrusted(_), _) => ProjectMcp::Untrusted,
        }
    }

    /// What `/trust` shows before it records anything: each input read once,
    /// its digest, and what of it would run as the host (each MCP server's
    /// command line, each hook's command), from exactly the bytes read.
    #[must_use]
    pub fn preview(&self) -> Vec<TrustPreview> {
        let record = self.record.lock();
        self.files()
            .map(|file| {
                let (status, bytes) = record.read_checked(file);
                let refused = match &status {
                    TrustStatus::Untrusted(
                        why @ (Untrusted::Symlinked(_)
                        | Untrusted::NotRegular
                        | Untrusted::Unreadable(_)),
                    ) => Some(why.to_string()),
                    _ => None,
                };
                let runs = match &bytes {
                    Some(bytes) if file == self.mcp => mcp_runs(bytes),
                    Some(bytes) => hook_runs(bytes),
                    None => Vec::new(),
                };
                TrustPreview {
                    file: file.to_path_buf(),
                    digest: bytes.as_deref().map(crate::hex::sha256),
                    runs,
                    refused,
                }
            })
            .collect()
    }

    /// `/trust`, answered yes: record the digests the user was shown
    /// (`shown`, from [`Self::preview`]); a file shown absent forgets its
    /// digest, and one the preview could not read (it said why) is left
    /// as it is. Only the project inputs are recorded, each as shown,
    /// never as read now: content that changed since stays untrusted (the
    /// answer's [`Self::changed_since`] says so). Returns the files
    /// recorded.
    ///
    /// # Errors
    ///
    /// [`TrustError`] when the record cannot be saved.
    pub fn trust_shown(&self, shown: &[TrustPreview]) -> Result<Vec<PathBuf>, TrustError> {
        let mut record = self.record.lock();
        let mut recorded = Vec::new();
        for file in self.files() {
            let Some(seen) = shown.iter().find(|seen| seen.file == file) else {
                continue;
            };
            if seen.refused.is_some() {
                continue;
            }
            record.record_digest(file, seen.digest.as_deref())?;
            if seen.digest.is_some() {
                recorded.push(file.to_path_buf());
            }
        }
        Ok(recorded)
    }

    /// Of `files`, the ones whose content is no longer the recorded one
    /// (changed after the user was shown it).
    #[must_use]
    pub fn changed_since(&self, files: &[PathBuf]) -> Vec<PathBuf> {
        let record = self.record.lock();
        files
            .iter()
            .filter(|file| record.check(file) != TrustStatus::Trusted)
            .cloned()
            .collect()
    }

    /// `/trust` without the question: record every present input as it is
    /// now (an absent one forgets its digest). Returns the files recorded.
    ///
    /// # Errors
    ///
    /// [`TrustError`] when a file cannot be trusted (symlinked, not a
    /// regular file) or the record cannot be saved; the files before it
    /// stay recorded.
    #[cfg(any(test, feature = "test-support"))]
    pub fn trust_all(&self) -> Result<Vec<PathBuf>, TrustError> {
        let mut record = self.record.lock();
        let mut recorded = Vec::new();
        for file in self.files() {
            let present = record.check(file) != TrustStatus::Missing;
            record.record_file(file)?;
            if present {
                recorded.push(file.to_path_buf());
            }
        }
        Ok(recorded)
    }

    /// Make `config` (the realm document) safe to build a session with:
    /// every hook of an untrusted project `.rkat/config.toml` shadowed by a
    /// disabled entry of its id. Returns what was refused.
    ///
    /// The shadowing is kept although the factory's [`HookSourceTrust`]
    /// question refuses an untrusted project file's hooks on its own: a
    /// refused hook keeps its slot and withholds what it gates (a refused
    /// `pre_tool_execution` guardrail blocks the tool), while a shadowed one
    /// is disabled and gates nothing. A file known to be untrusted before
    /// the build therefore stays inert, as the start-up notice says, and
    /// the refusal covers only content that changed after this read, which
    /// fails closed.
    pub async fn guard(&self, config: &mut Config) -> Vec<Refused> {
        let mut refused = Vec::new();
        if self.hooks_are_the_users() || self.trusted(&self.hooks) {
            return refused;
        }
        let project = read_hooks(&self.hooks).await;
        if project.is_empty() {
            return refused;
        }
        let user = match &self.user_hooks {
            Some(file) => read_hooks(file).await,
            None => Vec::new(),
        };
        for entry in project {
            // The build's own entry of that id wins anyway, and a definition
            // the user's file states too is the user's.
            if config.hooks.entries.iter().any(|e| e.id == entry.id) || user.contains(&entry) {
                continue;
            }
            refused.push(Refused::hook(&self.hooks, &entry.id.0));
            config.hooks.entries.push(meerkat_core::HookEntryConfig {
                enabled: false,
                ..entry
            });
        }
        refused
    }

    /// The start-up notice for `refused`: each untrusted file, why, and what
    /// of it did not start. `None` when nothing was refused.
    #[must_use]
    pub fn notice(&self, refused: &[Refused]) -> Option<String> {
        let mut by_file: BTreeMap<&Path, Vec<&str>> = BTreeMap::new();
        for item in refused {
            by_file.entry(&item.file).or_default().push(&item.what);
        }
        if by_file.is_empty() {
            return None;
        }
        let mut text = String::from(
            "project files not trusted, so these are left out (they would run with your privileges):",
        );
        for (file, items) in by_file {
            let why = match self.status(file) {
                TrustStatus::Untrusted(why) => why,
                TrustStatus::Missing | TrustStatus::Trusted => Untrusted::NotRecorded,
            };
            let _ = write!(text, "\n  {} ({why}): {}", file.display(), items.join(", "));
        }
        text.push_str(
            "\n/trust trusts every project input as it is now (from the next session); --trust-project trusts them for one run",
        );
        Some(text)
    }
}

/// The factory's question about a convention hook file it read: the
/// project `.rkat/config.toml` is trusted when the record holds the digest
/// of exactly the bytes the factory parsed, or under `--trust-project`.
/// Any other source (the user's `~/.rkat/config.toml`, or a workspace that
/// is the user config root) is the user's own and trusted.
///
/// The engine runs `decide` on the build's own task and asks that it not
/// block or do I/O: this is a lookup in the in-memory trust record (one
/// short mutex hold, no file read).
impl HookSourceTrust for ProjectInputs {
    fn decide(&self, source: HookSource<'_>) -> HookSourceDecision {
        self.decide_source(source.path, source.sha256)
    }
}

impl ProjectInputs {
    /// [`HookSourceTrust::decide`] over its two fields (the engine's
    /// `HookSource` is `#[non_exhaustive]`, so only the factory builds one).
    fn decide_source(&self, path: &Path, sha256: HookSourceDigest) -> HookSourceDecision {
        if self.by_flag || self.hooks_are_the_users() || path != self.hooks {
            return HookSourceDecision::Trusted;
        }
        if self
            .record
            .lock()
            .holds_digest(&self.hooks, &sha256.to_string())
        {
            HookSourceDecision::Trusted
        } else {
            HookSourceDecision::Untrusted
        }
    }
}

/// The `[hooks]` entries of a conventions config, as the factory reads them
/// (an absent or unparsable file has none).
async fn read_hooks(file: &Path) -> Vec<meerkat_core::HookEntryConfig> {
    let Ok(text) = tokio::fs::read_to_string(file).await else {
        return Vec::new();
    };
    let mut parsed = Config::default();
    match parsed.merge_toml_str(&text) {
        Ok(()) => parsed.hooks.entries,
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    struct Fixture {
        _dir: tempfile::TempDir,
        ws: PathBuf,
        home: PathBuf,
        scope: RuntimeScope,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let ws = base.join("ws");
        let home = base.join("home");
        std::fs::create_dir_all(ws.join(".rkat")).unwrap();
        std::fs::create_dir_all(home.join(".rkat")).unwrap();
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
        Fixture {
            _dir: dir,
            ws,
            home,
            scope,
        }
    }

    fn record(scope: &RuntimeScope) -> SharedTrustRecord {
        Arc::new(parking_lot::Mutex::new(
            super::super::approval::trust_record(scope),
        ))
    }

    const HOOK: &str = "[[hooks.entries]]\nid = \"fmt\"\npoint = \"turn_boundary\"\n\n[hooks.entries.runtime]\ntype = \"command\"\ncommand = \"touch\"\nargs = [\"/tmp/pwned\"]\n";

    #[test]
    fn the_inputs_are_the_project_files() {
        let fx = fixture();
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        let files: Vec<&Path> = inputs.files().collect();
        assert_eq!(
            files,
            [
                fx.ws.join(".rkat/mcp.toml").as_path(),
                fx.ws.join(".rkat/config.toml").as_path(),
            ]
        );
        // The user's files are not inputs: trusted; nor is the realm's
        // config, which is the user's too.
        assert!(inputs.trusted(&fx.home.join(".rkat/config.toml")));
        assert!(inputs.trusted(&fx.scope.config_path()));
        // Absent inputs are trusted (nothing to run).
        assert!(inputs.mcp_trusted());
    }

    #[tokio::test]
    async fn project_hooks_are_shadowed_until_trusted() {
        let fx = fixture();
        let project = fx.ws.join(".rkat/config.toml");
        std::fs::write(&project, HOOK).unwrap();
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        let mut config = Config::default();
        let refused = inputs.guard(&mut config).await;
        assert_eq!(refused, [Refused::hook(&project, "fmt")]);
        // The factory's own layering (build config > project > user) now
        // resolves the id to the disabled shadow.
        let layered =
            meerkat::resolve_layered_hooks_config(Some(&fx.ws), Some(&fx.home), &config).await;
        assert_eq!(layered.entries.len(), 1);
        assert!(!layered.entries[0].enabled);

        // Trusted: nothing is shadowed and the project's hook runs.
        inputs.trust_all().unwrap();
        let mut config = Config::default();
        assert!(inputs.guard(&mut config).await.is_empty());
        let layered =
            meerkat::resolve_layered_hooks_config(Some(&fx.ws), Some(&fx.home), &config).await;
        assert!(layered.entries[0].enabled);

        // Changed after trust: shadowed again.
        std::fs::write(&project, HOOK.replace("fmt", "fmt2")).unwrap();
        let mut config = Config::default();
        assert_eq!(
            inputs.guard(&mut config).await,
            [Refused::hook(&project, "fmt2")]
        );
    }

    /// `/trust` shows each input's MCP servers' command lines and hooks'
    /// commands, read once, and records nothing; the yes records the
    /// digests shown, and content changed in between stays untrusted.
    #[test]
    fn trust_shows_what_runs_then_records_what_was_shown() {
        let fx = fixture();
        let mcp = fx.ws.join(".rkat/mcp.toml");
        let hooks = fx.ws.join(".rkat/config.toml");
        std::fs::write(
            &mcp,
            "[[servers]]\nname = \"docs\"\ncommand = \"npx\"\nargs = [\"docs-server\", \"--port\", \"3 000\"]\nenv = { TOKEN = \"x\" }\n\n[[servers]]\nname = \"web\"\nurl = \"https://mcp.example.com/x\"\n",
        )
        .unwrap();
        std::fs::write(&hooks, HOOK).unwrap();
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        let shown = inputs.preview();
        assert_eq!(shown.len(), 2);
        assert_eq!(
            shown[0].runs,
            [
                "MCP server docs: npx docs-server --port '3 000' (sets TOKEN)",
                "MCP server web: https://mcp.example.com/x",
            ]
        );
        assert_eq!(shown[1].runs, ["hook fmt: touch /tmp/pwned"]);
        assert!(!inputs.mcp_trusted(), "a preview records nothing");

        // Yes, but the hook file changed after it was shown.
        std::fs::write(&hooks, HOOK.replace("pwned", "other")).unwrap();
        let recorded = inputs.trust_shown(&shown).unwrap();
        assert_eq!(recorded, [mcp.clone(), hooks.clone()]);
        assert!(inputs.mcp_trusted());
        assert!(!inputs.trusted(&hooks));
        assert_eq!(inputs.changed_since(&recorded), [hooks]);

        // A file shown absent forgets its digest.
        std::fs::remove_file(&mcp).unwrap();
        let gone = inputs.preview();
        assert_eq!(gone[0].digest, None);
        inputs.trust_shown(&gone).unwrap();
        std::fs::write(&mcp, "[[servers]]\nname = \"docs\"\ncommand = \"npx\"\n").unwrap();
        assert!(!inputs.mcp_trusted());
    }

    #[tokio::test]
    async fn a_hook_the_user_file_states_too_is_the_users() {
        let fx = fixture();
        std::fs::write(fx.ws.join(".rkat/config.toml"), HOOK).unwrap();
        std::fs::write(fx.home.join(".rkat/config.toml"), HOOK).unwrap();
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        let mut config = Config::default();
        assert!(inputs.guard(&mut config).await.is_empty());
        assert!(config.hooks.entries.is_empty());
    }

    fn decide(inputs: &ProjectInputs, path: &Path, bytes: &[u8]) -> HookSourceDecision {
        inputs.decide_source(path, HookSourceDigest::of(bytes))
    }

    /// The factory's question is answered over the bytes it read, not the
    /// file as it is when asked.
    #[test]
    fn the_hook_source_trust_judges_the_bytes_the_factory_read() {
        let fx = fixture();
        let project = fx.ws.join(".rkat/config.toml");
        std::fs::write(&project, HOOK).unwrap();
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        // Not recorded: untrusted.
        assert_eq!(
            decide(&inputs, &project, HOOK.as_bytes()),
            HookSourceDecision::Untrusted
        );
        inputs.trust_all().unwrap();
        // The recorded digest: trusted, even after the file changed on disk.
        std::fs::write(&project, HOOK.replace("fmt", "fmt2")).unwrap();
        assert_eq!(
            decide(&inputs, &project, HOOK.as_bytes()),
            HookSourceDecision::Trusted
        );
        // Other bytes than the recorded ones: untrusted.
        let planted = HOOK.replace("fmt", "planted");
        assert_eq!(
            decide(&inputs, &project, planted.as_bytes()),
            HookSourceDecision::Untrusted
        );
        // `--trust-project` trusts any content for the run.
        let flagged = ProjectInputs::new(&fx.scope, record(&fx.scope), true);
        assert_eq!(
            decide(&flagged, &project, planted.as_bytes()),
            HookSourceDecision::Trusted
        );
        // The user's file is the user's own, whatever it holds.
        assert_eq!(
            decide(
                &inputs,
                &fx.home.join(".rkat/config.toml"),
                planted.as_bytes()
            ),
            HookSourceDecision::Trusted
        );
    }

    /// A workspace that is the user config root reads the user's own file.
    #[test]
    fn the_hook_source_trust_keeps_a_workspace_that_is_the_user_root() {
        let dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(dir.path()).unwrap();
        let args = crate::cli::Args::try_parse_from([
            "rkat-tui",
            "-w",
            ws.to_str().unwrap(),
            "--user-config-root",
            ws.to_str().unwrap(),
            "--config-dir",
            ws.join("cfg").to_str().unwrap(),
        ])
        .unwrap();
        let scope = super::super::resolve_scope(&args).unwrap();
        let inputs = ProjectInputs::new(&scope, record(&scope), false);
        assert_eq!(
            decide(&inputs, &ws.join(".rkat/config.toml"), HOOK.as_bytes()),
            HookSourceDecision::Trusted
        );
    }
}
