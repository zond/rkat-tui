//! One trust decision over the workspace files that make the host run
//! something.
//!
//! Local MCP stdio servers and command hooks run as the trusted host, outside
//! the shell's confinement. Three workspace files can define them:
//!
//! - the project `.rkat/mcp.toml` (MCP servers),
//! - the project `.rkat/config.toml`, whose `[hooks]` the factory discovers
//!   at the conventions root on every build,
//! - the realm `config.toml` when the realm lives inside the workspace (the
//!   default `<workspace>/.rkat/tui/config.toml`): its `[[tools.mcp_servers]]`
//!   and `[hooks]`.
//!
//! Any of them may have come with a cloned repository, and on Linux a
//! confined shell can write `.rkat/` (the backend has no deny lists), so each
//! is trusted only when its whole content is what the host trust record
//! ([`HostTrustRecord`]) holds a digest of: what the user trusted with
//! `/trust` (alias `/mcp trust`), or, for the realm config, what rkat-tui
//! itself last wrote into a file that was trusted before the write.
//! `--trust-project` (alias `--allow-project-mcp`) trusts all of them for one
//! run without recording anything. A file that changes after it was trusted
//! is untrusted again, with a start-up notice naming it.
//!
//! An untrusted file's executable entries do not run: [`super::mcp::load`]
//! refuses its MCP servers, and [`ProjectInputs::guard`] (applied by
//! [`super::RealmConfigStore`] to every config the factory builds a session
//! with, helpers' included) removes the realm config's MCP servers and hooks
//! and shadows each hook of the project `.rkat/config.toml` with a disabled
//! entry of the same id (the factory layers hooks by id, the build's config
//! first, so the project file's entry never runs). User-global files
//! (`~/.rkat/config.toml`, `~/.rkat/mcp.toml`, `~/.rkat/tui`) are the
//! user's own and stay trusted.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meerkat_core::Config;
use meerkat_core::mcp_config::project_mcp_path_in;

use super::scope::RuntimeScope;
use crate::approval::trust::{HostTrustRecord, TrustError, TrustStatus, Untrusted};

/// The host trust record shared by the gate and the project inputs.
pub type SharedTrustRecord = Arc<parking_lot::Mutex<HostTrustRecord>>;

/// The conventions config the factory reads hooks from, under a root.
fn conventions_config(root: &Path) -> PathBuf {
    root.join(".rkat").join("config.toml")
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

/// The executable project inputs of one workspace and realm, and their
/// trust (see the module docs).
#[derive(Debug, Clone)]
pub struct ProjectInputs {
    record: SharedTrustRecord,
    by_flag: bool,
    mcp: PathBuf,
    hooks: PathBuf,
    /// `None`: the realm lives outside the workspace (the user's own).
    realm_config: Option<PathBuf>,
    /// The user-global conventions config (always trusted).
    user_hooks: Option<PathBuf>,
}

impl ProjectInputs {
    /// The inputs of `scope`, judged by `record`; `by_flag`: `--trust-project`.
    #[must_use]
    pub fn new(scope: &RuntimeScope, record: SharedTrustRecord, by_flag: bool) -> Self {
        let realm_config = scope.config_path();
        let user_hooks = scope.user_config_root.as_deref().map(conventions_config);
        let hooks = conventions_config(&scope.context_root);
        Self {
            record,
            by_flag,
            mcp: project_mcp_path_in(&scope.context_root),
            // A workspace that is the user config root reads the user's own
            // file there.
            hooks,
            realm_config: realm_config
                .starts_with(&scope.context_root)
                .then_some(realm_config),
            user_hooks,
        }
    }

    /// The project `.rkat/mcp.toml`.
    #[must_use]
    pub fn mcp_file(&self) -> &Path {
        &self.mcp
    }

    /// The realm `config.toml`, when it is a project input.
    #[must_use]
    pub fn realm_config_file(&self) -> Option<&Path> {
        self.realm_config.as_deref()
    }

    /// Every input file, present or not, in a fixed order.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.mcp.as_path())
            .chain(self.realm_config.as_deref())
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
        matches!(self.status(file), TrustStatus::Missing | TrustStatus::Trusted)
    }

    /// Whether the project `.rkat/mcp.toml` may start servers.
    #[must_use]
    pub fn mcp_trusted(&self) -> bool {
        self.trusted(&self.mcp)
    }

    /// Whether the realm config's MCP servers and hooks may run.
    #[must_use]
    pub fn realm_config_trusted(&self) -> bool {
        self.realm_config.as_deref().is_none_or(|f| self.trusted(f))
    }

    /// `/trust`: record every present input as it is now (an absent one
    /// forgets its digest). Returns the files recorded.
    ///
    /// # Errors
    ///
    /// [`TrustError`] when a file cannot be trusted (symlinked, not a
    /// regular file) or the record cannot be saved; the files before it
    /// stay recorded.
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

    /// Before rkat-tui writes the realm config: whether the file is trusted
    /// by its record now (absent or recorded; the flag does not count), so
    /// [`Self::wrote_realm_config`] may record what it writes. Writing into
    /// a file that was not trusted never makes it trusted.
    pub fn before_realm_config_write(&self) -> RealmConfigWrite {
        RealmConfigWrite(self.realm_config.as_deref().is_some_and(|file| {
            matches!(self.status(file), TrustStatus::Missing | TrustStatus::Trusted)
        }))
    }

    /// After rkat-tui wrote the realm config: record its digest when it was
    /// trusted before the write.
    pub fn wrote_realm_config(&self, before: RealmConfigWrite) {
        let (RealmConfigWrite(true), Some(file)) = (before, &self.realm_config) else {
            return;
        };
        if let Err(err) = self.record.lock().record_file(file) {
            tracing::warn!(error = %err, file = %file.display(), "could not record the realm config's digest");
        }
    }

    /// Make `config` (the realm document) safe to build a session with:
    /// without the realm config's MCP servers and hooks when it is not
    /// trusted, and with every hook of an untrusted project
    /// `.rkat/config.toml` shadowed by a disabled entry of its id. Returns
    /// what was refused.
    pub async fn guard(&self, config: &mut Config) -> Vec<Refused> {
        let mut refused = Vec::new();
        if let Some(file) = self.realm_config.as_deref()
            && !self.trusted(file)
        {
            refused.extend(
                config
                    .tools
                    .mcp_servers
                    .drain(..)
                    .map(|server| Refused::mcp_server(file, &server.name)),
            );
            refused.extend(
                config
                    .hooks
                    .entries
                    .drain(..)
                    .map(|entry| Refused::hook(file, &entry.id.0)),
            );
        }
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
            "project files not trusted, so these run nothing (they would run with your privileges):",
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

/// See [`ProjectInputs::before_realm_config_write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct RealmConfigWrite(bool);

impl RealmConfigWrite {
    /// A write with no project inputs to record.
    pub const UNGUARDED: Self = Self(false);
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
        Arc::new(parking_lot::Mutex::new(super::super::approval::trust_record(
            scope,
        )))
    }

    const HOOK: &str = "[[hooks.entries]]\nid = \"fmt\"\npoint = \"turn_boundary\"\n\n[hooks.entries.runtime]\ntype = \"command\"\ncommand = \"touch\"\nargs = [\"/tmp/pwned\"]\n";

    #[test]
    fn the_inputs_are_the_project_files_and_the_workspace_realm_config() {
        let fx = fixture();
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        let files: Vec<&Path> = inputs.files().collect();
        assert_eq!(
            files,
            [
                fx.ws.join(".rkat/mcp.toml").as_path(),
                fx.ws.join(".rkat/tui/config.toml").as_path(),
                fx.ws.join(".rkat/config.toml").as_path(),
            ]
        );
        // The user's files are not inputs: trusted.
        assert!(inputs.trusted(&fx.home.join(".rkat/config.toml")));
        // Absent inputs are trusted (nothing to run).
        assert!(inputs.mcp_trusted() && inputs.realm_config_trusted());
    }

    #[tokio::test]
    async fn a_realm_config_from_the_checkout_runs_nothing_until_trusted() {
        let fx = fixture();
        let realm_config = fx.ws.join(".rkat/tui/config.toml");
        std::fs::write(
            &realm_config,
            format!("[[tools.mcp_servers]]\nname = \"planted\"\ncommand = \"evil\"\n\n{HOOK}"),
        )
        .unwrap();
        let shared = record(&fx.scope);
        let inputs = ProjectInputs::new(&fx.scope, Arc::clone(&shared), false);
        let document = Config::default();
        let mut parsed = document.clone();
        parsed
            .merge_toml_str(&std::fs::read_to_string(&realm_config).unwrap())
            .unwrap();
        assert_eq!(parsed.tools.mcp_servers.len(), 1);

        // Initial checkout: refused, and the notice names the file.
        assert!(!inputs.realm_config_trusted());
        let mut guarded = parsed.clone();
        let refused = inputs.guard(&mut guarded).await;
        assert!(guarded.tools.mcp_servers.is_empty());
        assert!(guarded.hooks.entries.is_empty());
        assert_eq!(
            refused,
            [
                Refused::mcp_server(&realm_config, "planted"),
                Refused::hook(&realm_config, "fmt")
            ]
        );
        let notice = inputs.notice(&refused).unwrap();
        assert!(notice.contains(&realm_config.display().to_string()), "{notice}");
        assert!(notice.contains("may have come with the repository"), "{notice}");
        assert!(notice.contains("/trust") && notice.contains("--trust-project"));

        // `/trust`: admitted, for later processes too.
        let recorded = inputs.trust_all().unwrap();
        assert_eq!(recorded, std::slice::from_ref(&realm_config));
        let reopened = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        let mut guarded = parsed.clone();
        assert!(reopened.guard(&mut guarded).await.is_empty());
        assert_eq!(guarded.tools.mcp_servers.len(), 1);
        assert_eq!(guarded.hooks.entries.len(), 1);

        // Modified after trust: refused again, and the notice says so.
        std::fs::write(
            &realm_config,
            "[[tools.mcp_servers]]\nname = \"planted\"\ncommand = \"worse\"\n",
        )
        .unwrap();
        let mut guarded = parsed.clone();
        let refused = reopened.guard(&mut guarded).await;
        assert!(guarded.tools.mcp_servers.is_empty());
        assert!(
            reopened
                .notice(&refused)
                .unwrap()
                .contains("it changed since")
        );

        // `--trust-project` admits for this run without recording.
        let flagged = ProjectInputs::new(&fx.scope, record(&fx.scope), true);
        let mut guarded = parsed.clone();
        assert!(flagged.guard(&mut guarded).await.is_empty());
        assert!(!reopened.realm_config_trusted());
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

    #[test]
    fn rkat_tui_writes_keep_trust_but_never_launder_a_planted_file() {
        let fx = fixture();
        let realm_config = fx.ws.join(".rkat/tui/config.toml");
        let inputs = ProjectInputs::new(&fx.scope, record(&fx.scope), false);
        // Absent: rkat-tui's first write is recorded.
        let before = inputs.before_realm_config_write();
        std::fs::write(&realm_config, "[agent]\nmodel = \"m\"\n").unwrap();
        inputs.wrote_realm_config(before);
        assert_eq!(inputs.status(&realm_config), TrustStatus::Trusted);
        // Trusted: the next write is recorded too.
        let before = inputs.before_realm_config_write();
        std::fs::write(&realm_config, "[agent]\nmodel = \"n\"\n").unwrap();
        inputs.wrote_realm_config(before);
        assert_eq!(inputs.status(&realm_config), TrustStatus::Trusted);
        // Planted behind rkat-tui's back: a later rkat-tui write keeps it
        // untrusted.
        std::fs::write(&realm_config, "[[tools.mcp_servers]]\nname = \"x\"\ncommand = \"y\"\n")
            .unwrap();
        let before = inputs.before_realm_config_write();
        std::fs::write(
            &realm_config,
            "[[tools.mcp_servers]]\nname = \"x\"\ncommand = \"y\"\n[agent]\nmodel = \"n\"\n",
        )
        .unwrap();
        inputs.wrote_realm_config(before);
        assert!(matches!(
            inputs.status(&realm_config),
            TrustStatus::Untrusted(Untrusted::Changed)
        ));
    }
}
