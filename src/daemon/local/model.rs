//! The model and the permission mode: `rkat/set_model` and a model switch's `[thinking]`, `rkat/set_mode` and the plan-mode notes.

use super::*;

impl Local {
    /// `rkat/set_model`: switch the agent's session to another model
    /// in place: the engine's LLM reconfigure on the live session,
    /// with the endpoint's auth binding and the `[thinking]` setting the
    /// new model is configured with. Refused while a turn runs;
    /// the session keeps the model, and the next prompt is an
    /// ordinary `mob/member_send`, tied to no connection.
    pub(super) async fn set_model(
        &self,
        agent: &str,
        params: SetModelParams,
    ) -> Result<ModelSet, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;

        let SetModelParams {
            model,
            provider,
            auth_binding,
            listed,
        } = params;
        // The configured endpoint the switch's binding names (`None`: a
        // built-in endpoint).
        let endpoint_name = auth_binding
            .as_ref()
            .and_then(|binding| bound_endpoint(binding.realm.as_str(), binding.binding.as_str()));
        let endpoint = endpoint_name
            .and_then(|name| self.endpoints.get(name))
            .cloned();
        let reseated = self
            .ensure_alias(
                agent,
                &model,
                provider.as_deref(),
                endpoint.as_ref(),
                listed,
            )
            .await?;
        let provider_params = provider.as_deref().and_then(|provider| {
            let dialect = WireDialect::parse(provider)?;
            let configured = self.configured_thinking(endpoint_name, provider, dialect, &model);
            Some(match thinking::params_for(configured.as_ref()) {
                Some(params) => TurnMetadataOverride::Set(params),
                None => TurnMetadataOverride::Clear,
            })
        });
        // On a self-hosted endpoint the engine runs the model's alias.
        let (engine_model, engine_provider, self_hosted_server_id) = match &endpoint {
            Some(endpoint) if endpoint.is_self_hosted() => (
                endpoint.engine_model(&model),
                Some(endpoint.engine_provider().as_str().to_string()),
                endpoint.self_hosted_server(),
            ),
            _ => (model, provider, None),
        };
        let request = meerkat_runtime::SessionLlmReconfigureRequest {
            model: Some(engine_model),
            provider: engine_provider,
            self_hosted_server_id,
            provider_params,
            auth_binding: Some(match auth_binding {
                Some(binding) => TurnMetadataOverride::Set(binding),
                None => TurnMetadataOverride::Clear,
            }),
        };
        let machine = Arc::clone(&self.machine);
        let (session, report) = self
            .conversations
            .while_idle(agent, |session| async move {
                let report = machine
                    .reconfigure_session_llm_identity(&session, request)
                    .await?;
                Ok::<_, meerkat_runtime::RuntimeDriverError>((session, report))
            })
            .await
            .map_err(|err| match err {
                crate::daemon::conversations::SwitchError::Running => Refusal::new(
                    SWITCH_BUSY,
                    "a turn is running; switch models once it ends (or interrupt it)",
                ),
                other => other.into(),
            })?
            .map_err(Refusal::switch_failed)?;
        tracing::debug!(?report, "switched the agent's model");
        let (model, provider) = crate::endpoints::from_engine(
            &report.new_identity.model,
            report.new_identity.provider.as_str(),
        );
        Ok(ModelSet {
            session_id: session,
            model,
            provider,
            reseated,
        })
    }

    /// A model on a self-hosted endpoint needs its alias in the realm
    /// config before the runtime accepts it; anywhere else only a
    /// catalogued model runs (refused at the reconfigure, naming the way
    /// out). When the alias is new, the live agent's model registry
    /// predates it: the agent is seated again on its session, which
    /// rebuilds it. Whether it was.
    pub(super) async fn ensure_alias(
        &self,
        agent: &str,
        model: &str,
        provider: Option<&str>,
        endpoint: Option<&crate::endpoints::Endpoint>,
        listed: Option<protocol::ListedModel>,
    ) -> Result<bool, Refusal> {
        let Some(dialect) = provider.and_then(WireDialect::parse) else {
            return Ok(false);
        };
        let listed = listed.map(|listed| crate::models::ModelEntry {
            id: model.to_string(),
            display_name: None,
            source: crate::models::ModelSource::Provider,
            context_window: listed.context_window,
            max_output_tokens: listed.max_output_tokens,
            capabilities: crate::models::ModelCapabilities {
                reasoning: listed.reasoning,
                vision: listed.vision,
                function_calling: None,
            },
        });
        // A built-in endpoint is named by the dialect.
        let Some(target) = endpoint.or_else(|| self.endpoints.get(dialect.as_str())) else {
            return Ok(false);
        };
        let written = crate::endpoints::ensure_endpoint_model(
            self.realm_config.runtime(),
            target,
            model,
            listed.as_ref(),
        )
        .await
        .map_err(Refusal::switch_failed)?;
        if written {
            self.agents.reseat(agent).await.map_err(Refusal::from)?;
        }
        Ok(written)
    }

    /// The `[thinking]` setting `model` of `provider` (its `dialect`) is
    /// configured with, a configured endpoint named by `endpoint` (its auth
    /// binding), a built-in one by the provider.
    pub(super) fn configured_thinking(
        &self,
        endpoint: Option<&str>,
        provider: &str,
        dialect: WireDialect,
        model: &str,
    ) -> Option<meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig> {
        self.thinking
            .resolve(Some(endpoint.unwrap_or(provider)), dialect.into(), model)
    }

    /// The permission mode and the denial counters now.
    #[must_use]
    pub fn mode(&self) -> ModeStatus {
        ModeStatus::of(&self.engine)
    }

    pub(super) async fn set_mode(&self, mode: PermissionMode) -> ModeSet {
        self.engine.set_mode(mode);
        // Every client shows it.
        self.decisions
            .broadcast(protocol::MODE, &ModeStatus::of(&self.engine));
        let mut warnings = Vec::new();
        let engine = Arc::clone(&self.engine);
        let persisted = tokio::task::spawn_blocking(move || engine.persist_mode())
            .await
            .map_err(|err| err.to_string())
            .and_then(|written| written.map_err(|err| err.to_string()));
        if let Err(err) = persisted {
            warnings.push(format!(
                "the mode applies to this run but was not saved: {err}"
            ));
        }
        if let Some(warning) = self.note_plan_mode().await {
            warnings.push(warning);
        }
        ModeSet {
            status: ModeStatus::of(&self.engine),
            notice: mode_notice(mode).to_string(),
            warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
        }
    }

    /// At start: when the persisted mode is plan, tell every agent (a
    /// fresh one never heard it; a restored one may have been switched to
    /// plan mode by another process). Returns a warning when one could not
    /// be told.
    pub async fn announce_plan_mode(&self) -> Option<String> {
        if self.engine.mode() != PermissionMode::Plan {
            return None;
        }
        self.note_plan(true).await
    }

    /// Tell every live agent plan mode started or ended, once per change
    /// (an agent never told is told only when plan mode is on). The mode is
    /// read under the lock, not taken from the caller: of two overlapping
    /// changes, the later note always matches the mode that holds, and the
    /// notes alternate.
    pub(super) async fn note_plan_mode(&self) -> Option<String> {
        self.note_plan(false).await
    }

    /// Tell each live agent the plan mode that holds when its last note
    /// said otherwise (`all`: every live agent, whatever it was told).
    pub(super) async fn note_plan(&self, all: bool) -> Option<String> {
        let mut noted = self.plan_noted.lock().await;
        let plan = self.engine.mode() == PermissionMode::Plan;
        let live = self.agents.names();
        noted.retain(|agent, _| live.contains(agent));
        let mut warnings = Vec::new();
        for agent in live {
            if !all && noted.get(&agent).copied().unwrap_or(false) == plan {
                continue;
            }
            match self.append_plan_note(&agent, plan).await {
                None => {
                    noted.insert(agent, plan);
                }
                Some(warning) => warnings.push(warning),
            }
        }
        (!warnings.is_empty()).then(|| warnings.join("\n"))
    }

    pub(super) async fn append_plan_note(&self, agent: &str, plan: bool) -> Option<String> {
        let note = if plan {
            crate::approval::mode::PLAN_ON_NOTE
        } else {
            crate::approval::mode::PLAN_OFF_NOTE
        };
        match crate::daemon::agents::append_note(
            &self.state,
            agent,
            SYSTEM_CONTEXT_SOURCE,
            note.to_string(),
        )
        .await
        {
            Ok(()) => None,
            Err(err) => Some(format!(
                "agent {agent} could not be told about plan mode now (the tool errors say it): {err}"
            )),
        }
    }

    /// A connection was just bound to `agent`: when plan mode is on and the
    /// agent was never told (one seated for it), tell it.
    pub async fn bound(&self, agent: &str) {
        if self.engine.mode() == PermissionMode::Plan
            && let Some(warning) = self.note_plan_mode().await
        {
            tracing::warn!(%agent, %warning, "plan mode");
        }
    }

    /// A model switch carries the `[thinking]` the new model is configured
    /// with (or clears the old model's), unless the caller names params.
    pub(super) fn with_thinking(&self, params: &mut Value) {
        if params.get("provider_params").is_some() {
            return;
        }
        let (Some(model), Some(provider)) = (
            params.get("model").and_then(Value::as_str),
            params.get("provider").and_then(Value::as_str),
        ) else {
            return;
        };
        let binding = params.pointer("/auth_binding/value");
        let endpoint = binding.and_then(|binding| {
            bound_endpoint(
                binding.get("realm")?.as_str()?,
                binding.get("binding")?.as_str()?,
            )
        });
        let Some(dialect) = WireDialect::parse(provider) else {
            return;
        };
        let configured = self.configured_thinking(endpoint, provider, dialect, model);
        params["provider_params"] = thinking_override(configured.as_ref());
    }
}

/// The configured endpoint an auth binding of `realm` names (`binding`);
/// `None` for a built-in endpoint's binding.
pub(super) fn bound_endpoint<'a>(realm: &str, binding: &'a str) -> Option<&'a str> {
    is_endpoint_realm(realm).then_some(binding)
}

/// The `provider_params` override of a model switch: set the thinking the
/// new model is configured with, or clear the old model's.
#[must_use]
pub fn thinking_override(
    thinking: Option<&meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>,
) -> Value {
    match thinking::params_for(thinking) {
        Some(params) => json!({
            "action": "set",
            "value": meerkat_contracts::wire::runtime::WireProviderParamsOverride::from(params),
        }),
        None => json!({"action": "clear"}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_model_switch_sets_or_clears_thinking() {
        let clear = thinking_override(None);
        assert_eq!(clear, json!({"action": "clear"}));
        let set = thinking_override(Some(
            &meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig::Adaptive,
        ));
        assert_eq!(set["action"], "set");
        assert!(set["value"]["provider_tag"].is_object(), "{set}");
    }
}
