//! The config the factory reads: the realm's own `config.toml`, through its
//! owner ([`FileConfigStore`]), with what `rkat` applies on top of it at
//! load time (environment overrides) and the TUI's user-global `SYSTEM.md`
//! ([`super::instructions::apply_system_md`]).
//!
//! Every read goes to the document, so what the TUI writes into it (the
//! endpoint connections, `[models.<id>]` profiles; see
//! [`crate::endpoints`]) and what another surface writes is what the next
//! session build sees. Writes go to the document unchanged: the overrides
//! are never persisted.
//!
//! rkat-tui's own mutations go through the realm's [`ConfigRuntime`]
//! ([`RealmConfigStore::runtime`]): the document's writer lock and its
//! generation sidecar (`<realm>/config_state.json`), the contract `rkat
//! config`, REST and RPC write under, so a TUI write and theirs never
//! interleave and a read-modify-write retries on a generation conflict. The
//! [`ConfigStore`] writes of [`RealmConfigStore`] itself are for the owner
//! that already holds that lock: the engine's `ConfigRuntime` over this
//! store (the LLM reconfigure host, same sidecar), never a direct caller.
//! A `ConfigRuntime` writes through the held variants (`set_held`,
//! `patch_held`, `patch_held_with_warnings`) under the guard of the
//! document's admission fence, which both stores name by forwarding
//! [`ConfigStore::admission_fence`] to the file store.
//!
//! When the realm lives inside the workspace its document is a project input
//! ([`super::project_trust`]): every read is guarded
//! ([`ProjectInputs::guard`]: an untrusted document's MCP servers and hooks,
//! and an untrusted project `.rkat/config.toml`'s hooks, do not reach the
//! build), and every write (either path, inside the writer lock) records the
//! new digest only when the document was trusted before it
//! ([`ProjectInputs::wrote_realm_config`]). [`ConfigStore::set`] writes what
//! it is given: a config read from this store carries the guard's disabled
//! hook shadows, so a caller that sets one persists them (rkat-tui never
//! calls it).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::config::{ConfigError, ConfigWarning};
use meerkat_core::{
    AdmissionFence, AdmissionGuard, Config, ConfigDelta, ConfigRuntime, ConfigStore,
    FileConfigStore,
};

use super::dirs::GlobalDirs;
use super::project_trust::ProjectInputs;

/// The realm document as persisted, each write bracketed by the trust
/// record (see the module docs). The store [`RealmConfigStore::runtime`]
/// writes through.
struct RealmDocument {
    file: FileConfigStore,
    inputs: Option<ProjectInputs>,
}

impl RealmDocument {
    async fn write<T>(
        &self,
        write: impl std::future::Future<Output = Result<T, ConfigError>>,
    ) -> Result<T, ConfigError> {
        let before = self
            .inputs
            .as_ref()
            .map(ProjectInputs::before_realm_config_write);
        let written = write.await;
        if let (Some(inputs), Some(before)) = (&self.inputs, before)
            && written.is_ok()
        {
            inputs.wrote_realm_config(before);
        }
        written
    }
}

#[async_trait]
impl ConfigStore for RealmDocument {
    async fn get(&self) -> Result<Config, ConfigError> {
        self.file.get().await
    }

    async fn get_with_warnings(&self) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        self.file.get_with_warnings().await
    }

    async fn set(&self, config: Config) -> Result<(), ConfigError> {
        self.write(self.file.set(config)).await
    }

    async fn patch(&self, delta: ConfigDelta) -> Result<Config, ConfigError> {
        self.write(self.file.patch(delta)).await
    }

    async fn patch_preview(
        &self,
        delta: &ConfigDelta,
    ) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        self.file.patch_preview(delta).await
    }

    async fn patch_with_warnings(
        &self,
        delta: ConfigDelta,
    ) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        self.write(self.file.patch_with_warnings(delta)).await
    }

    fn admission_fence(&self) -> Option<AdmissionFence> {
        self.file.admission_fence()
    }

    async fn set_held(&self, guard: &AdmissionGuard, config: Config) -> Result<(), ConfigError> {
        self.write(self.file.set_held(guard, config)).await
    }

    async fn patch_held(
        &self,
        guard: &AdmissionGuard,
        delta: ConfigDelta,
    ) -> Result<Config, ConfigError> {
        self.write(self.file.patch_held(guard, delta)).await
    }

    async fn patch_held_with_warnings(
        &self,
        guard: &AdmissionGuard,
        delta: ConfigDelta,
    ) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        self.write(self.file.patch_held_with_warnings(guard, delta))
            .await
    }
}

/// The realm document plus the load-time overrides (see the module docs).
pub struct RealmConfigStore {
    document: Arc<RealmDocument>,
    runtime: ConfigRuntime,
    state_path: PathBuf,
    global: Option<GlobalDirs>,
}

impl RealmConfigStore {
    /// The store of the realm document at `path` whose generation sidecar
    /// is `state_path` (`<realm>/config_state.json`, the file every
    /// surface's `ConfigRuntime` of the realm uses); `global` supplies the
    /// user-global `SYSTEM.md`.
    #[must_use]
    pub fn new(path: &Path, state_path: PathBuf, global: Option<GlobalDirs>) -> Self {
        Self::compose(
            RealmDocument {
                file: FileConfigStore::new(path.to_path_buf(), meerkat_models::canonical()),
                inputs: None,
            },
            state_path,
            global,
        )
    }

    fn compose(document: RealmDocument, state_path: PathBuf, global: Option<GlobalDirs>) -> Self {
        let document = Arc::new(document);
        let runtime = ConfigRuntime::new(
            Arc::clone(&document) as Arc<dyn ConfigStore>,
            state_path.clone(),
        );
        Self {
            document,
            runtime,
            state_path,
            global,
        }
    }

    /// Guard reads and record writes with the project inputs' trust (see
    /// the module docs).
    #[must_use]
    pub fn with_inputs(self, inputs: ProjectInputs) -> Self {
        let document = RealmDocument {
            file: FileConfigStore::new(
                self.document.file.path().to_path_buf(),
                meerkat_models::canonical(),
            ),
            inputs: Some(inputs),
        };
        Self::compose(document, self.state_path, self.global)
    }

    /// The realm document's config runtime: rkat-tui's writes of individual
    /// entries go through it, under the writer lock and the generation
    /// check; its reads are the document as persisted (no overrides, no
    /// guard).
    #[must_use]
    pub fn runtime(&self) -> &ConfigRuntime {
        &self.runtime
    }

    /// The generation sidecar the realm's config runtimes share.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// The document's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.document.file.path()
    }

    /// The document as persisted with the load-time overrides applied.
    async fn effective(&self, mut config: Config) -> Result<Config, ConfigError> {
        if let Some(inputs) = &self.document.inputs {
            let refused = inputs.guard(&mut config).await;
            if !refused.is_empty() {
                tracing::debug!(?refused, "untrusted project inputs left out of the build config");
            }
        }
        config.apply_env_overrides()?;
        super::instructions::apply_system_md(&mut config, self.global.as_ref());
        Ok(config)
    }
}

impl std::fmt::Debug for RealmConfigStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealmConfigStore")
            .field("path", &self.path())
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

/// For the holder of the realm's writer lock (see the module docs).
#[async_trait]
impl ConfigStore for RealmConfigStore {
    async fn get(&self) -> Result<Config, ConfigError> {
        self.effective(self.document.get().await?).await
    }

    async fn get_with_warnings(&self) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        let (config, warnings) = self.document.get_with_warnings().await?;
        Ok((self.effective(config).await?, warnings))
    }

    async fn set(&self, config: Config) -> Result<(), ConfigError> {
        self.document.set(config).await
    }

    async fn patch(&self, delta: ConfigDelta) -> Result<Config, ConfigError> {
        let patched = self.document.patch(delta).await?;
        self.effective(patched).await
    }

    async fn patch_preview(
        &self,
        delta: &ConfigDelta,
    ) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        let (config, warnings) = self.document.patch_preview(delta).await?;
        Ok((self.effective(config).await?, warnings))
    }

    async fn patch_with_warnings(
        &self,
        delta: ConfigDelta,
    ) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        let (config, warnings) = self.document.patch_with_warnings(delta).await?;
        Ok((self.effective(config).await?, warnings))
    }

    fn admission_fence(&self) -> Option<AdmissionFence> {
        self.document.admission_fence()
    }

    async fn set_held(&self, guard: &AdmissionGuard, config: Config) -> Result<(), ConfigError> {
        self.document.set_held(guard, config).await
    }

    async fn patch_held(
        &self,
        guard: &AdmissionGuard,
        delta: ConfigDelta,
    ) -> Result<Config, ConfigError> {
        let patched = self.document.patch_held(guard, delta).await?;
        self.effective(patched).await
    }

    async fn patch_held_with_warnings(
        &self,
        guard: &AdmissionGuard,
        delta: ConfigDelta,
    ) -> Result<(Config, Vec<ConfigWarning>), ConfigError> {
        let (config, warnings) = self
            .document
            .patch_held_with_warnings(guard, delta)
            .await?;
        Ok((self.effective(config).await?, warnings))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_follow_the_document_and_writes_reach_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let store = RealmConfigStore::new(&path, tmp.path().join("config_state.json"), None);
        assert!(store.get().await.unwrap().models.custom.is_empty());
        // Another writer (rkat) changes the document: the next read sees it.
        std::fs::write(
            &path,
            "[models.\"m-1\"]\nprovider = \"openai\"\ncontext_window = 1000\n",
        )
        .unwrap();
        let config = store.get().await.unwrap();
        assert_eq!(config.models.custom["m-1"].context_window, Some(1000));
        // A write through the store's config runtime lands in the document
        // and publishes a generation in the realm's sidecar.
        let written = store
            .runtime()
            .patch(
                ConfigDelta(serde_json::json!({
                    "models": { "m-2": { "provider": "anthropic" } }
                })),
                Some(0),
            )
            .await
            .unwrap();
        assert_eq!(written.generation, 1);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("m-2"), "{text}");
        assert!(store.get().await.unwrap().models.custom.contains_key("m-2"));
        // A writer that read an older generation is refused.
        assert!(matches!(
            store
                .runtime()
                .patch(ConfigDelta(serde_json::json!({ "models": {} })), Some(0))
                .await,
            Err(meerkat_core::ConfigRuntimeError::GenerationConflict { expected: 0, current: 1 })
        ));
    }
}
