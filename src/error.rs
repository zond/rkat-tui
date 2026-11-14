//! Error type for the `rkat-tui` surface.

use std::path::PathBuf;

/// Top-level error for the TUI surface.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// The working directory or an explicit root could not be resolved.
    #[error("failed to resolve project root from {path}: {source}")]
    ProjectRoot {
        /// Path that was being resolved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A generic I/O failure with context.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The realm config could not be loaded or env overrides were invalid.
    #[error("{context}: {source}")]
    Config {
        /// What was being attempted.
        context: &'static str,
        /// Underlying config error.
        #[source]
        source: meerkat_core::ConfigError,
    },
    /// The realm config could not be read through its config runtime.
    #[error("{context}: {source}")]
    ConfigRuntime {
        /// What was being attempted.
        context: &'static str,
        /// Underlying config runtime error.
        #[source]
        source: meerkat_core::ConfigRuntimeError,
    },
    /// The workspace's state directory could not be opened at start-up:
    /// the message names the directory and, when another `rkat-tui` version
    /// may have written it (a schema, ledger or manifest it does not
    /// understand; see [`suggests_move_aside`]), how to start fresh. Other
    /// failures (a realm another process holds, contention, permissions) get
    /// the plain error.
    #[error("{}", persistence_open_message(realm_root, source))]
    PersistenceOpen {
        /// The realm's directory.
        realm_root: PathBuf,
        /// Underlying persistence error (boxed: the variant must not widen the
        /// error type).
        #[source]
        source: Box<meerkat::PersistenceError>,
    },
    /// The realm lease (the heartbeat record `rkat realm prune` and realm
    /// inspection read to tell a live realm from an idle one) could not be
    /// written.
    #[error("cannot take the lease of realm {realm_id} in {lease_dir}: {source}")]
    RealmLease {
        /// The realm id.
        realm_id: String,
        /// The realm's lease directory.
        lease_dir: PathBuf,
        /// Underlying store error (boxed: the variant must not widen the
        /// error type).
        #[source]
        source: Box<meerkat_store::StoreError>,
    },
    /// The effective MCP set (realm servers, project and user `mcp.toml`)
    /// could not be composed: a file cannot be read or parsed, or a realm
    /// server and a file server share a name with different definitions.
    #[error("MCP config error: {0}")]
    McpConfig(#[from] meerkat_core::mcp_config::EffectiveMcpServersError),
    /// No endpoint/model could be selected, an endpoint is unusable, or the
    /// endpoint projection into the realm config failed.
    #[error(transparent)]
    Endpoint(#[from] crate::endpoints::EndpointError),
    /// The session's external tools (file and note tools) could not be
    /// composed into one gateway.
    #[error("cannot compose the file and note tools: {0}")]
    ToolGateway(#[source] meerkat_core::error::ToolError),
    /// stdin or stdout is not a terminal; the TUI needs one.
    #[error("stdin and stdout must be a terminal; rkat-tui is interactive")]
    NotATerminal,
    /// `--classifier-check` could not run, or stopped before its last case.
    #[error("the classifier check did not complete: {0}")]
    ClassifierCheck(String),
    /// `--classifier-check` ran and the model is outside the targets.
    #[error("the classifier model is outside the targets (the report above says how)")]
    ClassifierOutside,
    /// The terminal UI failed.
    #[error(transparent)]
    Ui(#[from] crate::ui::UiError),
    /// A command-line rule or setting flag is invalid.
    #[error(transparent)]
    Overlay(#[from] crate::overlay::OverlayError),
    /// The approval rules could not be loaded.
    #[error("approval setup failed: {0}")]
    Approval(#[from] crate::approval::ApprovalError),
    /// The selected process confinement cannot be installed on this host,
    /// or the command line asks for less than the realm's `[confinement]`.
    #[error("process confinement: {0}")]
    Confinement(#[from] crate::bootstrap::confinement::ConfinementError),
    /// The workspace daemon's side failed (its endpoint file cannot be
    /// placed, or it cannot start).
    #[error(transparent)]
    Daemon(Box<crate::daemon::DaemonError>),
    /// The workspace's realm cannot be found, made or re-homed.
    #[error(transparent)]
    RealmHome(#[from] crate::bootstrap::realm_home::HomeError),
    /// A re-home was refused or failed.
    #[error(transparent)]
    Rehome(#[from] crate::client::rehome::RehomeError),
    /// The shell's confinement plan reaches the realm directory: the
    /// agent's (or the `!`) shell could read or rewrite the sessions, the
    /// approvals and the audit log.
    #[error(
        "refusing to start: the confined shell is granted {}, which reaches this workspace's state {} (sessions, approvals, rules, the audit log); drop that grant, or keep the state elsewhere with --config-dir",
        grant.display(),
        realm.display()
    )]
    RealmExposed {
        /// The realm directory.
        realm: PathBuf,
        /// The shell's grant that reaches it.
        grant: PathBuf,
    },
    /// `--attach=DIRECTORY`: no daemon has an endpoint file for that
    /// workspace.
    #[error(
        "no rkat-tui-mob runs for {} (it has no endpoint file); start one there (rkat-tui in that directory)",
        workspace.display()
    )]
    NoDaemonAt {
        /// The workspace the directory resolved to.
        workspace: std::path::PathBuf,
    },
    /// `--attach=HOST:PORT` cannot be used as given.
    #[error("--attach={address}: {reason}")]
    AttachAddress {
        /// The address given.
        address: String,
        /// Why not.
        reason: String,
    },
}

impl TuiError {
    /// Wrap an I/O error with a short description of what was being attempted.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Process exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::ProjectRoot { .. }
            | Self::Config { .. }
            | Self::ConfigRuntime { .. }
            | Self::Endpoint(_)
            | Self::McpConfig(_)
            | Self::Overlay(_)
            | Self::Approval(_)
            | Self::Confinement(_)
            | Self::RealmHome(_)
            | Self::Rehome(_)
            | Self::RealmExposed { .. }
            | Self::NoDaemonAt { .. }
            | Self::AttachAddress { .. }
            | Self::NotATerminal => 2,
            Self::Io { .. }
            | Self::PersistenceOpen { .. }
            | Self::RealmLease { .. }
            | Self::ToolGateway(_)
            | Self::Daemon(_)
            | Self::ClassifierCheck(_)
            | Self::Ui(_) => 1,
            Self::ClassifierOutside => 3,
        }
    }
}

/// `path` as one POSIX shell word: single-quoted, each `'` written `'\''`.
#[must_use]
pub fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Whether `err` says the state directory was written by another version
/// (a schema, ledger or manifest from the future, unledgered or malformed
/// schema objects): the one case where moving it aside helps. Every error
/// in the source chain is inspected.
#[must_use]
pub fn suggests_move_aside(err: &meerkat::PersistenceError) -> bool {
    use meerkat_store::StoreError;
    fn incompatible(store: &StoreError) -> bool {
        matches!(
            store,
            StoreError::SchemaFromTheFuture { .. }
                | StoreError::UnledgeredDomainObjects { .. }
                | StoreError::ManifestFromTheFuture { .. }
                | StoreError::CorruptLedger { .. }
        )
    }
    // A transparent wrapper's `source()` skips the wrapped error itself, so
    // each level is matched as a store error or a persistence error.
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(error) = current {
        let store = error.downcast_ref::<StoreError>().or_else(|| match error
            .downcast_ref::<meerkat::PersistenceError>(
        ) {
            Some(meerkat::PersistenceError::Store(store)) => Some(store),
            _ => None,
        });
        if store.is_some_and(incompatible) {
            return true;
        }
        current = error.source();
    }
    false
}

fn persistence_open_message(
    realm_root: &std::path::Path,
    source: &meerkat::PersistenceError,
) -> String {
    let mut out = format!(
        "failed to open the state directory {}: {source}",
        realm_root.display()
    );
    if suggests_move_aside(source) {
        let old = realm_root.with_file_name(format!(
            "{}.old",
            realm_root
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        ));
        out.push_str(&format!(
            "\n\nIf another rkat-tui version wrote it and you do not need its sessions, \
             move it aside and start fresh:\n  mv {} {}",
            shell_quote(realm_root),
            shell_quote(&old),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::TuiError;
    use std::path::PathBuf;

    fn open_error(realm_root: &str, source: meerkat::PersistenceError) -> TuiError {
        TuiError::PersistenceOpen {
            realm_root: PathBuf::from(realm_root),
            source: Box::new(source),
        }
    }

    fn from_another_version() -> meerkat::PersistenceError {
        meerkat::PersistenceError::Store(meerkat_store::StoreError::SchemaFromTheFuture {
            domain: "session-store".to_string(),
            found: 9,
            supported: 3,
        })
    }

    #[test]
    fn a_state_dir_from_another_version_names_the_way_out() {
        let err = open_error("/work/.rkat/tui", from_another_version());
        let text = err.to_string();
        assert!(
            text.starts_with("failed to open the state directory /work/.rkat/tui: "),
            "{text}"
        );
        assert!(
            text.contains("mv '/work/.rkat/tui' '/work/.rkat/tui.old'"),
            "{text}"
        );
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn other_open_failures_get_no_move_aside_advice() {
        for source in [
            meerkat::PersistenceError::RealmGoverned {
                realm_id: "tui".to_string(),
            },
            meerkat::PersistenceError::Store(meerkat_store::StoreError::RealmManifestLockTimeout {
                realm_id: "tui".to_string(),
            }),
            meerkat::PersistenceError::Store(meerkat_store::StoreError::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
        ] {
            let text = open_error("/work/.rkat/tui", source).to_string();
            assert!(
                text.starts_with("failed to open the state directory /work/.rkat/tui: "),
                "{text}"
            );
            assert!(!text.contains("mv "), "{text}");
        }
    }

    #[test]
    fn hint_paths_with_quotes_stay_one_shell_word() {
        assert_eq!(
            super::shell_quote(std::path::Path::new("/it's here")),
            "'/it'\\''s here'"
        );
        let err = open_error("/w/it's/.rkat/tui", from_another_version());
        let text = err.to_string();
        assert!(
            text.contains("mv '/w/it'\\''s/.rkat/tui' '/w/it'\\''s/.rkat/tui.old'"),
            "{text}"
        );
    }
}
