//! The config the factory reads: the realm's own `config.toml`, through its
//! owner ([`FileConfigStore`]), with the load-time environment overrides
//! and the user-global `SYSTEM.md`
//! ([`super::instructions::apply_system_md`]) applied on top.
//!
//! Every read goes to the document, so what rkat-tui writes into it (the
//! endpoint connections, `[models.<id>]` profiles; see
//! [`crate::endpoints`]) is what the next session build sees. Writes go to
//! the document unchanged: the overrides are never persisted.
//!
//! rkat-tui's own mutations go through the realm's [`ConfigRuntime`]
//! ([`RealmConfigStore::runtime`]): the document's writer lock and its
//! generation sidecar (`<realm>/config_state.json`), so two writes never
//! interleave and a read-modify-write retries on a generation conflict. The
//! [`ConfigStore`] writes of [`RealmConfigStore`] itself are for the owner
//! that already holds that lock: the engine's `ConfigRuntime` over this
//! store (the LLM reconfigure host, same sidecar), never a direct caller.
//! A `ConfigRuntime` writes through the held variants (`set_held`,
//! `patch_held`, `patch_held_with_warnings`) under the guard of the
//! document's admission fence, which both stores name by forwarding
//! [`ConfigStore::admission_fence`] to the file store.
//!
//! Every read is guarded by the project inputs ([`ProjectInputs::guard`]:
//! an untrusted project `.rkat/config.toml`'s hooks do not reach the
//! build). [`ConfigStore::set`] writes what it is given: a config read
//! from this store carries the guard's disabled hook shadows, so a caller
//! that sets one persists them (rkat-tui never calls it).

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

/// The realm document plus the load-time overrides (see the module docs).
pub struct RealmConfigStore {
    document: Arc<FileConfigStore>,
    runtime: ConfigRuntime,
    state_path: PathBuf,
    global: Option<GlobalDirs>,
    inputs: Option<ProjectInputs>,
}

impl RealmConfigStore {
    /// The store of the realm document at `path` whose generation sidecar
    /// is `state_path` (`<realm>/config_state.json`, the file every
    /// `ConfigRuntime` of the realm uses); `global` supplies the
    /// user-global `SYSTEM.md`.
    #[must_use]
    pub fn new(path: &Path, state_path: PathBuf, global: Option<GlobalDirs>) -> Self {
        let document = Arc::new(FileConfigStore::new(
            path.to_path_buf(),
            meerkat_models::canonical(),
        ));
        let runtime = ConfigRuntime::new(
            Arc::clone(&document) as Arc<dyn ConfigStore>,
            state_path.clone(),
        );
        Self {
            document,
            runtime,
            state_path,
            global,
            inputs: None,
        }
    }

    /// Guard reads with the project inputs' trust (see the module docs).
    #[must_use]
    pub fn with_inputs(mut self, inputs: ProjectInputs) -> Self {
        self.inputs = Some(inputs);
        self
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
        self.document.path()
    }

    /// The document as persisted with the load-time overrides applied.
    async fn effective(&self, mut config: Config) -> Result<Config, ConfigError> {
        if let Some(inputs) = &self.inputs {
            let refused = inputs.guard(&mut config).await;
            if !refused.is_empty() {
                tracing::debug!(
                    ?refused,
                    "untrusted project inputs left out of the build config"
                );
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
        let (config, warnings) = self.document.patch_held_with_warnings(guard, delta).await?;
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
        // Another writer changes the document: the next read sees it.
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
            Err(meerkat_core::ConfigRuntimeError::GenerationConflict {
                expected: 0,
                current: 1
            })
        ));
    }
}
