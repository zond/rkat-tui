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
            .filter(|binding| is_endpoint_realm(binding.realm.as_str()))
            .map(|binding| binding.binding.as_str());
        let endpoint = endpoint_name
            .and_then(|name| self.parts.endpoints.get(name))
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
            // rkat-tui sets no provider-neutral level (its `[thinking]` is
            // the provider's own parameters above), so there is none to
            // keep or clear.
            reasoning_effort: None,
            auth_binding: Some(match auth_binding {
                Some(binding) => TurnMetadataOverride::Set(binding),
                None => TurnMetadataOverride::Clear,
            }),
        };
        let machine = Arc::clone(&self.parts.machine);
        let (session, report) = self
            .core
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
        // Its helpers from now on run on the new model, through its
        // endpoint (a spawn names its owner's model, `crate::mobs::customizer`).
        self.core.registry.register_session(
            &session,
            &report.new_identity.model,
            report.new_identity.auth_binding.as_ref(),
        );
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
    async fn ensure_alias(
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
        let Some(target) = endpoint.or_else(|| self.parts.endpoints.get(dialect.as_str())) else {
            return Ok(false);
        };
        let written = crate::endpoints::ensure_endpoint_model(
            self.parts.realm_config.runtime(),
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
    fn configured_thinking(
        &self,
        endpoint: Option<&str>,
        provider: &str,
        dialect: WireDialect,
        model: &str,
    ) -> Option<meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig> {
        self.parts
            .thinking
            .resolve(Some(endpoint.unwrap_or(provider)), dialect.into(), model)
    }

    /// `[auto] classifier_model`'s hook (`/auto model`, `/set`): the
    /// classifier reviews with `model` of `endpoint` from now on (a review
    /// that started finishes on the old one), audited, and the start-up
    /// check runs against it in the background, its result a notice. An
    /// endpoint this daemon does not have is refused with what to do.
    pub(super) async fn switch_classifier_model(&self, endpoint: &str, model: &str) -> RuleOutcome {
        let refused = |text: String| RuleOutcome { ok: false, text };
        let model = model.trim();
        if model.is_empty() {
            return refused("/auto model needs a model: /auto model [endpoint:]<id>, or /auto model to pick one".into());
        }
        let Some(target) = self.parts.endpoints.get(endpoint).cloned() else {
            return refused(format!(
                "the classifier model was not changed: this daemon has no endpoint `{endpoint}` (it has {}); define it under [endpoints.{endpoint}] in your own tui.toml and restart the daemon, or pick a model of one it has",
                self.parts.endpoints.names().join(", ")
            ));
        };
        if target.is_self_hosted()
            && let Err(err) = crate::endpoints::ensure_endpoint_model(
                self.parts.realm_config.runtime(),
                &target,
                model,
                None,
            )
            .await
        {
            return refused(format!(
                "the classifier model was not changed: {model} could not be registered for {endpoint}: {err}"
            ));
        }
        let label = format!("{endpoint}:{model}");
        match self
            .core
            .engine
            .reviewer()
            .switch(target, model.to_string(), label)
            .await
        {
            Ok(text) => {
                let engine = Arc::clone(&self.core.engine);
                tokio::spawn(async move {
                    if tokio::time::timeout(
                        std::time::Duration::from_mins(2),
                        engine.reviewer().probe_switched(),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!("the new classifier model's check did not finish in 120 s");
                    }
                });
                RuleOutcome { ok: true, text }
            }
            Err(text) => refused(format!("the classifier model was not changed: {text}")),
        }
    }

    /// The permission mode and the denial counters now.
    #[must_use]
    pub fn mode(&self) -> ModeStatus {
        ModeStatus::of(&self.core.engine)
    }

    /// `rkat/set_mode` (Shift+Tab, `/mode`): `[auto] mode` written to the
    /// workspace's `tui.toml`, or the user's own (`user`), and the mode
    /// switched live ([`Self::switch_mode`], the one switch `/set
    /// auto.mode` takes too) whatever the write did: a failed write, or a
    /// higher layer that wins at the next start, is the warning.
    pub(super) async fn set_mode(&self, mode: PermissionMode, user: bool) -> ModeSet {
        use crate::config::registry::{Level, MODE};
        let level = if user { Level::User } else { Level::Workspace };
        let value =
            toml::Value::try_from(mode).unwrap_or_else(|_| toml::Value::String(mode.to_string()));
        let settings = Arc::clone(&self.core.settings);
        let written =
            tokio::task::spawn_blocking(move || settings.write_value(&MODE, Some(value), level))
                .await
                .map_err(|err| err.to_string())
                .and_then(|written| written);
        let mut warnings = Vec::new();
        match written {
            Ok(written) => {
                self.audit_setting(&written);
                if written.after.source > written.level {
                    warnings.push(written.confirmation());
                }
            }
            Err(why) => warnings.push(format!("it applies to this run but was not saved: {why}")),
        }
        warnings.extend(self.switch_mode(mode).await);
        ModeSet {
            status: ModeStatus::of(&self.core.engine),
            notice: mode_notice(mode).to_string(),
            warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
        }
    }

    /// The mode switched live: the engine's, every client's push, and the
    /// agents' plan-mode notes. The warnings.
    pub(super) async fn switch_mode(&self, mode: PermissionMode) -> Vec<String> {
        self.core.engine.set_mode(mode);
        // Every client shows it.
        self.core
            .decisions
            .broadcast(protocol::MODE, &ModeStatus::of(&self.core.engine));
        #[cfg(any(test, feature = "test-support"))]
        let _held = self.mode_hold.read().await;
        self.note_plan_mode().await.into_iter().collect()
    }

    /// At start: when the persisted mode is plan, tell every agent (a
    /// fresh one never heard it; a restored one may have been switched to
    /// plan mode by another process). Returns a warning when one could not
    /// be told.
    pub async fn announce_plan_mode(&self) -> Option<String> {
        if self.core.engine.mode() != PermissionMode::Plan {
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
    async fn note_plan(&self, all: bool) -> Option<String> {
        let mut noted = self.plan_noted.lock().await;
        let plan = self.core.engine.mode() == PermissionMode::Plan;
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

    /// Tell `agent` plan mode is on (`plan`) or off. An agent with a run
    /// open gets the note steered into that run (it reads it at its next
    /// step): an append to its system context would land only when the
    /// turn ends, and the mode change (`rkat/set_mode`) would answer only
    /// then. An idle agent gets it appended, which the engine applies at
    /// once and which starts no run: a mode change wakes nobody. (An agent
    /// whose run ends between the look and the steer gets a run of the
    /// note: the engine has no "steer only into an open run".) A prompt to
    /// an idle agent sent after the change waits for this note
    /// ([`Local::routing_local`]); one to a running agent is a steer and
    /// follows the note's steer into the same run.
    async fn append_plan_note(&self, agent: &str, plan: bool) -> Option<String> {
        let note = if plan {
            crate::approval::mode::PLAN_ON_NOTE
        } else {
            crate::approval::mode::PLAN_OFF_NOTE
        };
        let told = if self.core.activity.running(agent) {
            let _sending = self.core.activity.sending(agent);
            crate::daemon::agents::steer_member(
                &self.core,
                &meerkat_mob::MobId::from(crate::daemon::MOB_ID),
                meerkat_mob::AgentIdentity::from(agent),
                note.to_string(),
            )
            .await
            .map_err(|err| err.to_string())
        } else {
            crate::daemon::agents::append_note(
                &self.core.state,
                agent,
                SYSTEM_CONTEXT_SOURCE,
                note.to_string(),
            )
            .await
            .map_err(|err| err.to_string())
        };
        told.err().map(|err| {
            format!(
                "agent {agent} could not be told about plan mode now (the tool errors say it): {err}"
            )
        })
    }

    /// A connection was just bound to `agent`: when plan mode is on and the
    /// agent was never told (one seated for it), tell it.
    pub async fn bound(&self, agent: &str) {
        if self.core.engine.mode() == PermissionMode::Plan
            && let Some(warning) = self.note_plan_mode().await
        {
            tracing::warn!(%agent, %warning, "plan mode");
        }
    }
}
