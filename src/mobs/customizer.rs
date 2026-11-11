//! The spawn customizer every helper passes: it gives the helper the
//! approval gate of the session it works for, that session's model binding,
//! the TUI's file tools and the helper instructions, and tells the
//! [`super::follow`] task to observe it.
//!
//! A host that runs its main agent as a mob member (the `rkat-tui-mob`
//! daemon's orchestrator) names it as the customizer's [`RootMember`]: that
//! member is shaped as the owner itself (the session's tools, persona,
//! instructions, curated shell environment and model binding), and the
//! helpers it spawns work for its session.

use std::collections::HashMap;
use std::sync::Arc;

use meerkat_core::{AgentToolDispatcher, AuthBindingRef, SessionId, ToolDispatchAdmission};
use meerkat_mob::{
    AgentIdentity, MobError, MobId, ResumedMemberView, SpawnCustomizationContext,
    SpawnMemberCustomizer, SpawnMemberSpec, SpawnSource,
};
use tokio::sync::mpsc;

use super::registry::{HelperKey, HelperRegistry, HelperTag, Owner};
use crate::approval::{ApprovalEngine, ApprovalGate};
use crate::bootstrap::HELPER_PERSONA;

/// The mob tools that start a member or a council: denied to a helper at
/// the configured depth limit (`[helpers] max_depth`), so helpers nest no
/// deeper. `fork_off` ([`DENIED_SPAWN_TOOLS`]) is denied unless the fork is gated;
/// the daemon's members keep it
/// ([`HelperCustomizer::with_gated_fork_off`]) except at the depth limit.
pub const NESTING_TOOLS: [&str; 4] = ["delegate", "mob_create", "mob_spawn_member", "council"];

/// Mob tools denied unless the member's fork is gated
/// ([`HelperCustomizer::with_gated_fork_off`]): a fork child inherits the
/// source's dispatch admission, so the daemon's members keep `fork_off`
/// behind the gate except at the depth limit.
pub const DENIED_SPAWN_TOOLS: [&str; 1] = ["fork_off"];

/// The tool access policy every member's is merged with:
/// [`crate::approval::policy::TUI_OWNED_BUILTINS`], the shell's job controls
/// ([`crate::approval::policy::SHELL_JOB_TOOLS`]) and [`DENIED_SPAWN_TOOLS`]
/// denied.
#[must_use]
pub fn tool_access_policy() -> meerkat_core::ops::ToolAccessPolicy {
    meerkat_core::ops::ToolAccessPolicy::DenyList(
        crate::approval::policy::TUI_OWNED_BUILTINS
            .into_iter()
            .chain(crate::approval::policy::SHELL_JOB_TOOLS)
            .chain(DENIED_SPAWN_TOOLS)
            .collect::<meerkat_core::ToolNameSet>(),
    )
}

/// Builds the external tools of one helper (the TUI's file tools).
pub type HelperTools = Arc<dyn Fn() -> Arc<dyn AgentToolDispatcher> + Send + Sync>;

/// A helper about to be spawned, for the follower task.
#[derive(Debug, Clone)]
pub struct Spawning {
    /// Mob id and member identity.
    pub key: HelperKey,
    /// What the helper is.
    pub tag: HelperTag,
}

/// Builds the instruction sections of a fresh root member (the persona
/// first) for its identity, read again on every fresh spawn.
pub type RootInstructions = Arc<dyn Fn(&AgentIdentity) -> Vec<String> + Send + Sync>;

/// Builds the external tools of one root member (each its own: the file
/// tools report their changes under the member's name).
pub type RootTools = Arc<dyn Fn(&AgentIdentity) -> Arc<dyn AgentToolDispatcher> + Send + Sync>;

/// Builds the approval gate of one root member (its key names it).
pub type RootAdmission = Arc<dyn Fn(&HelperKey) -> Arc<dyn ToolDispatchAdmission> + Send + Sync>;

/// The `[thinking]` a rebuilt root member's identity is configured with
/// (its model may have been switched since the spawn).
pub type RootThinking = Arc<
    dyn Fn(
            &meerkat_core::SessionLlmIdentity,
        ) -> Option<meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>
        + Send
        + Sync,
>;

/// The host-root members that are owners themselves, not helpers: the mob
/// daemon's agents (its orchestrators), every member of one mob whose
/// identity [`RootMember::is_root`] accepts. A member an agent seats
/// itself (`spawn_member`) under such a name is refused: the names are
/// the daemon's.
pub struct RootMember {
    /// Their mob.
    pub mob: MobId,
    /// Whether an identity of that mob names one of them.
    pub is_root: fn(&str) -> bool,
    /// The profile the daemon seats them with.
    pub profile: String,
    /// Each one's external tools (the TUI session's file and note tools);
    /// not persisted by the engine, so supplied again on every rebuild.
    pub tools: RootTools,
    /// Its instruction sections on a fresh spawn.
    pub instructions: RootInstructions,
    /// The model it starts on (a rebuilt member keeps its session's).
    pub model: String,
    /// The start-up endpoint's auth binding.
    pub auth_binding: Option<AuthBindingRef>,
    /// The approval gate for a root member's own calls (the daemon's
    /// `RootGate`: the TUI's tiers, rules, classifier and modes, consent
    /// through the approval store). Re-installed on every rebuild: the
    /// engine does not persist an admission.
    pub admission: Option<RootAdmission>,
    /// The `[thinking]` of a rebuilt member's model; `None`: the persisted
    /// provider params stay as they are.
    pub thinking: Option<RootThinking>,
}

impl std::fmt::Debug for RootMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootMember")
            .field("mob", &self.mob)
            .field("model", &self.model)
            .field("gated", &self.admission.is_some())
            .field("thinking", &self.thinking.is_some())
            .finish_non_exhaustive()
    }
}

/// The TUI's [`SpawnMemberCustomizer`].
pub struct HelperCustomizer {
    registry: Arc<HelperRegistry>,
    engine: Arc<ApprovalEngine>,
    tools: HelperTools,
    /// The environment of the helper's shell commands (the confined
    /// shell's curated set), `None` when the shell runs unconfined.
    shell_env: Option<HashMap<String, String>>,
    spawning: mpsc::UnboundedSender<Spawning>,
    /// How deep helpers may nest (`[helpers] max_depth`).
    max_depth: u8,
    /// The host-root member treated as an owner, if any.
    root: Option<RootMember>,
    /// The application tool policy every member this customizer shapes is
    /// built with (`None`: the spawn's own).
    application_tool_policy: Option<meerkat_core::ApplicationToolPolicyBinding>,
    /// Members keep `fork_off`: the host's engine seats a fork child under
    /// its source's dispatch admission (it asks this customizer for the
    /// child), so the child runs behind the gate.
    gated_fork_off: bool,
    /// The configured endpoints (a helper on another model than its
    /// owner's is routed through the owner's, see [`helper_binding`]).
    endpoints: Vec<crate::endpoints::Endpoint>,
}

impl std::fmt::Debug for HelperCustomizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperCustomizer").finish_non_exhaustive()
    }
}

impl HelperCustomizer {
    /// A customizer gating helpers with `engine`; a helper's shell
    /// commands get `shell_env` (see [`inherit_shell_env`]); helpers nest
    /// at most `max_depth` levels deep (see [`Depth`]).
    #[must_use]
    pub fn new(
        registry: Arc<HelperRegistry>,
        engine: Arc<ApprovalEngine>,
        tools: HelperTools,
        shell_env: Option<HashMap<String, String>>,
        spawning: mpsc::UnboundedSender<Spawning>,
        max_depth: u8,
    ) -> Self {
        Self {
            registry,
            engine,
            tools,
            shell_env,
            spawning,
            max_depth,
            root: None,
            application_tool_policy: None,
            gated_fork_off: false,
            endpoints: Vec::new(),
        }
    }

    /// The configured endpoints, which name the owner's endpoint and the
    /// ones a refused helper model could run on ([`helper_binding`]).
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: Vec<crate::endpoints::Endpoint>) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Let the members keep `fork_off` (see the field). Only for a host
    /// whose fork children reach this customizer: the mob daemon's, where
    /// `a_fork_off_child_runs_behind_the_gate` pins it.
    #[must_use]
    pub fn with_gated_fork_off(mut self) -> Self {
        self.gated_fork_off = true;
        self
    }

    /// Build every member it shapes (the root and its helpers) with the
    /// application tool policy `binding`: the engine records it with the
    /// member and refuses to rebuild the member without its registry.
    #[must_use]
    pub fn with_application_tool_policy(
        mut self,
        binding: meerkat_core::ApplicationToolPolicyBinding,
    ) -> Self {
        self.application_tool_policy = Some(binding);
        self
    }

    fn bind_policy(&self, spec: &mut SpawnMemberSpec) {
        if let Some(binding) = &self.application_tool_policy {
            spec.application_tool_policy = Some(binding.clone());
        }
    }

    /// Treat `root` as an owner itself rather than as a helper.
    #[must_use]
    pub fn with_root(mut self, root: RootMember) -> Self {
        self.root = Some(root);
        self
    }

    /// The root members, when `key` names one of them.
    fn root_for(&self, key: &HelperKey) -> Option<&RootMember> {
        self.root
            .as_ref()
            .filter(|root| root.mob == key.mob && (root.is_root)(key.member.as_str()))
    }
}

/// Shape the root member `key`'s `spec` as an owner session: its tools,
/// the curated shell environment, the TUI's tool access policy, its
/// binding and (fresh only) its instructions. The model is the mob
/// profile's.
fn shape_root(
    spec: &mut SpawnMemberSpec,
    key: &HelperKey,
    root: &RootMember,
    shell_env: Option<&HashMap<String, String>>,
    fresh: bool,
    gated_fork_off: bool,
) {
    // A fresh agent runs on the start-up endpoint; a rebuilt one keeps its
    // session's own binding (its customizer sets it), which may name
    // another endpoint than the start-up one, or none.
    if fresh && spec.auth_binding.is_none() {
        spec.auth_binding.clone_from(&root.auth_binding);
    }
    if let Some(admission) = &root.admission {
        spec.tool_dispatch_admission = Some(admission(key));
    }
    if spec.external_tools.is_none() {
        spec.external_tools = Some((root.tools)(&key.member));
    }
    inherit_shell_env(spec, shell_env);
    // The schedule tools: also for an agent whose mob was created before
    // its profile named them (the mob keeps its creation definition).
    spec.tool_category_overrides.schedule = meerkat_core::ToolCategoryOverride::Enable;
    if fresh {
        spec.additional_instructions
            .get_or_insert_with(Vec::new)
            .extend((root.instructions)(&key.member));
    }
    deny_tools(spec, &[], gated_fork_off);
}

/// The model `spec` asks for, when it names one (an explicit override or an
/// inline profile); otherwise the member runs on its mob's profile, which
/// for a `delegate` helper is its delegator's model.
fn requested_model(spec: &SpawnMemberSpec) -> Option<String> {
    spec.model_override.clone().or_else(|| {
        spec.override_profile
            .as_ref()
            .map(|profile| profile.model.clone())
            .filter(|model| !model.is_empty())
    })
}

/// Give `spec`'s shell commands `shell_env` (the confined shell's curated
/// environment) unless the spawn already names one. A helper's shell runs
/// under the same confinement as its owner's, so it needs the same
/// environment.
fn inherit_shell_env(spec: &mut SpawnMemberSpec, shell_env: Option<&HashMap<String, String>>) {
    if spec.shell_env.is_none() {
        spec.shell_env = shell_env.cloned();
    }
}

/// Where a helper sits against the depth limit. A helper's depth is the
/// length of its path: the session's own helpers are at depth 1, theirs at
/// 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// It may start helpers of its own.
    MayNest,
    /// It may not: its [`NESTING_TOOLS`] are denied.
    Last,
    /// It should not exist: its spawner should not have had the tools.
    TooDeep,
}

impl Depth {
    /// The [`NESTING_TOOLS`] a helper at this depth is denied.
    fn nesting_denied(self) -> &'static [&'static str] {
        match self {
            Self::MayNest => &[],
            Self::Last | Self::TooDeep => &NESTING_TOOLS,
        }
    }

    fn of(depth: usize, max_depth: u8) -> Self {
        match depth.cmp(&usize::from(max_depth)) {
            std::cmp::Ordering::Less => Self::MayNest,
            std::cmp::Ordering::Equal => Self::Last,
            std::cmp::Ordering::Greater => Self::TooDeep,
        }
    }

    /// Where the helper `label` at `depth` sits, refusing a `fresh` spawn
    /// beyond the limit: a second line of defence, as its spawner was at
    /// the limit and should not have had the tools. A member restored from
    /// before the limit was lowered keeps running, with the tools denied.
    fn admit(label: &str, depth: usize, max_depth: u8, fresh: bool) -> Result<Self, MobError> {
        match Self::of(depth, max_depth) {
            Self::TooDeep if fresh => Err(refuse(format_args!(
                "`{label}` would be a helper {depth} levels deep, but helpers nest at most {max_depth} (tui.toml [helpers] max_depth, or --set helpers.max_depth=N); do the work without delegating"
            ))),
            Self::TooDeep => Ok(Self::Last),
            depth => Ok(depth),
        }
    }
}

/// The auth binding a helper of `owner` on `model` runs with: always its
/// owner's endpoint, never the model's provider default (its public URL
/// with an environment key). On another model than its owner's, that
/// endpoint must speak the model's dialect: a model the catalog does not
/// know is the endpoint's to serve or refuse; one the catalog places with
/// another provider is refused, naming the configured endpoints that speak
/// its dialect.
///
/// # Errors
///
/// The refusal, in words for the agent.
fn helper_binding(
    owner: &Owner,
    model: &str,
    endpoints: &[crate::endpoints::Endpoint],
) -> Result<Option<AuthBindingRef>, String> {
    use crate::endpoints::WireDialect;
    if model == owner.model {
        return Ok(owner.auth_binding.clone());
    }
    let Some(wanted) = WireDialect::of_catalog_model(model) else {
        return Ok(owner.auth_binding.clone());
    };
    let own = endpoints
        .iter()
        .find(|endpoint| endpoint.binding() == owner.auth_binding && !endpoint.builtin)
        .or_else(|| {
            // A built-in endpoint (no binding): the one of the owner's
            // model's dialect.
            owner.auth_binding.is_none().then(|| {
                endpoints.iter().find(|endpoint| {
                    endpoint.builtin
                        && Some(endpoint.dialect) == WireDialect::of_catalog_model(&owner.model)
                })
            })?
        });
    let own_dialect = own
        .map(|endpoint| endpoint.dialect)
        .or_else(|| WireDialect::of_catalog_model(&owner.model));
    if own_dialect == Some(wanted) {
        return Ok(owner.auth_binding.clone());
    }
    let serving: Vec<&str> = endpoints
        .iter()
        .filter(|endpoint| endpoint.dialect == wanted)
        .map(|endpoint| endpoint.name.as_str())
        .collect();
    Err(format!(
        "a helper runs through its owner's endpoint ({}), which does not serve `{model}` ({wanted} dialect){}; use a model that endpoint serves, or switch your own model (/model) first",
        own.map_or_else(
            || "unknown".to_string(),
            |endpoint| format!("`{}`", endpoint.name)
        ),
        if serving.is_empty() {
            String::new()
        } else {
            format!(
                "; configured endpoints of that dialect: {}",
                serving.join(", ")
            )
        }
    ))
}

/// A helper of an owner on a self-hosted endpoint runs on that endpoint: a
/// model it asks for by its plain name becomes the endpoint's alias of it
/// (`<endpoint>:<model>`), the engine id the endpoint's facts are
/// registered under.
fn qualify_model(
    owner: &Owner,
    spec: &mut SpawnMemberSpec,
    endpoints: &[crate::endpoints::Endpoint],
) {
    let Some(endpoint) = endpoints
        .iter()
        .find(|e| e.is_self_hosted() && e.binding() == owner.auth_binding)
    else {
        return;
    };
    if let Some(model) = &spec.model_override
        && !model.starts_with(&format!("{}:", endpoint.name))
    {
        spec.model_override = Some(endpoint.engine_model(model));
    }
}

/// Apply the owner's gate, tools, instructions and the shell's environment
/// to `spec` (its binding is [`helper_binding`]'s).
fn shape(
    spec: &mut SpawnMemberSpec,
    owner: &Owner,
    label: &str,
    registry: &Arc<HelperRegistry>,
    engine: &Arc<ApprovalEngine>,
    tools: &HelperTools,
    shell_env: Option<&HashMap<String, String>>,
) {
    let gate = ApprovalGate::for_helper(Arc::clone(engine), owner.session.clone(), label)
        .after(spec.tool_dispatch_admission.take());
    // A council it convenes works for its owner, through it.
    spec.tool_dispatch_admission = Some(Arc::new(super::council::Convening::new(
        Arc::new(gate),
        registry,
        None,
    )));
    if spec.external_tools.is_none() {
        spec.external_tools = Some(tools());
    }
    inherit_shell_env(spec, shell_env);
    spec.additional_instructions
        .get_or_insert_with(Vec::new)
        .push(HELPER_PERSONA.to_string());
}

/// A helper is a seated member, so the engine would let it `fork_off`; its
/// fork child would seat without this customizer and so without the gate.
/// Every helper's tool access policy denies the TUI's denied tools
/// ([`tool_access_policy`]), merged into whatever policy its
/// spec names: none or `Inherit` becomes the TUI's deny list, a deny list
/// gains the names, an allow list loses them, constraints gain a deny
/// constraint, and `ReadOnly` (which already admits no mutating tool) stays.
/// `also` adds names to deny (a helper at the depth limit gets
/// [`NESTING_TOOLS`]); with any, `ReadOnly` becomes a read-only constraint
/// beside the deny constraint, so the names are denied whatever read-only
/// admits.
#[cfg(test)]
fn deny_ungated_tools(spec: &mut SpawnMemberSpec, also: &[&str]) {
    deny_tools(spec, also, false);
}

/// `deny_ungated_tools` (a test helper), keeping `fork_off` when `gated_fork_off` and
/// nothing else is denied (`also` is empty: a member at the depth limit
/// may not fork either, as a fork child is a helper one level deeper).
fn deny_tools(spec: &mut SpawnMemberSpec, also: &[&str], gated_fork_off: bool) {
    use meerkat_core::ops::{ToolAccessConstraint, ToolAccessPolicy};
    let ToolAccessPolicy::DenyList(mut denied) = tool_access_policy() else {
        // The TUI policy is a deny list; anything else is a programming
        // error that must not widen a helper's tools.
        spec.tool_access_policy = Some(tool_access_policy());
        return;
    };
    for name in also {
        denied.insert((*name).to_string());
    }
    if gated_fork_off && also.is_empty() {
        for name in DENIED_SPAWN_TOOLS {
            denied.remove(name);
        }
    }
    spec.tool_access_policy = Some(match spec.tool_access_policy.take() {
        None | Some(ToolAccessPolicy::Inherit) => ToolAccessPolicy::DenyList(denied),
        Some(ToolAccessPolicy::DenyList(mut names)) => {
            for name in denied.iter() {
                names.insert(name.clone());
            }
            ToolAccessPolicy::DenyList(names)
        }
        Some(ToolAccessPolicy::AllowList(mut names)) => {
            names.retain(|name| !denied.contains(name.as_str()));
            ToolAccessPolicy::AllowList(names)
        }
        Some(ToolAccessPolicy::ReadOnly) if also.is_empty() => ToolAccessPolicy::ReadOnly,
        Some(ToolAccessPolicy::ReadOnly) => ToolAccessPolicy::Constraints(vec![
            ToolAccessConstraint::ReadOnly,
            ToolAccessConstraint::DenyNames(denied),
        ]),
        Some(ToolAccessPolicy::Constraints(mut constraints)) => {
            constraints.push(ToolAccessConstraint::DenyNames(denied));
            ToolAccessPolicy::Constraints(constraints)
        }
    });
}

/// Keep the mob tools out of a helper's tool list: the deny list refuses
/// their calls, but every schema left in the list costs tokens in every
/// call (the mob and council tools are about 25k).
///
/// The mob category goes off through the spec's category overrides. A
/// member that inherits its spawner's tool filter (a `delegate` helper)
/// without an override profile otherwise gets every category opened for
/// that filter to narrow, which the engine does only while the spawn
/// overrides nothing; so such a spawn names every other category open
/// itself, exactly as the engine would have.
fn drop_mob_tools(spec: &mut SpawnMemberSpec) {
    use meerkat_core::ToolCategoryOverride;
    let overrides = &mut spec.tool_category_overrides;
    if spec.inherited_tool_filter.is_some() && spec.override_profile.is_none() {
        for category in [
            &mut overrides.builtins,
            &mut overrides.shell,
            &mut overrides.comms,
            &mut overrides.memory,
            &mut overrides.workgraph,
            &mut overrides.schedule,
            &mut overrides.image_generation,
        ] {
            if *category == ToolCategoryOverride::Inherit {
                *category = ToolCategoryOverride::Enable;
            }
        }
    }
    overrides.mob = ToolCategoryOverride::Disable;
    // A helper schedules nothing: a schedule targets an agent's own
    // session and fires later on its own, which is the agent's to decide.
    overrides.schedule = ToolCategoryOverride::Disable;
}

/// A refused helper, as the mob reports it.
fn refuse(err: impl std::fmt::Display) -> MobError {
    MobError::Internal(format!("rkat-tui: helper refused: {err}"))
}

impl HelperCustomizer {
    /// Shape `spec` for `owner`, bind it as `key` and have it followed.
    fn admit(
        &self,
        key: HelperKey,
        owner: &Owner,
        spec: &mut SpawnMemberSpec,
        session: Option<&SessionId>,
        fresh: bool,
    ) -> Result<(), MobError> {
        let label = spec.identity.to_string();
        qualify_model(owner, spec, &self.endpoints);
        let model = requested_model(spec).unwrap_or_else(|| owner.model.clone());
        let mut path = owner.path.clone();
        path.push(label.clone());
        let tag = HelperTag {
            owner: owner.session.clone(),
            path,
            model: model.clone(),
        };
        let depth = Depth::admit(&label, tag.path.len(), self.max_depth, fresh)?;
        if spec.auth_binding.is_none() {
            spec.auth_binding = helper_binding(owner, &model, &self.endpoints).map_err(refuse)?;
        }
        // Bound durably before the member exists: a member whose owner a
        // restart would lose never runs.
        self.registry
            .spawning(key.clone(), tag.clone())
            .map_err(refuse)?;
        if let Some(session) = session {
            self.registry.bind_session(&key, &tag, session);
        }
        shape(
            spec,
            owner,
            &label,
            &self.registry,
            &self.engine,
            &self.tools,
            self.shell_env.as_ref(),
        );
        deny_tools(spec, depth.nesting_denied(), self.gated_fork_off);
        if depth != Depth::MayNest {
            drop_mob_tools(spec);
        }
        self.bind_policy(spec);
        let _ = self.spawning.send(Spawning { key, tag });
        Ok(())
    }

    /// The owner a rebuilt member `key` was bound to.
    fn persisted_owner(&self, key: &HelperKey) -> Result<Owner, MobError> {
        let tag = self.registry.owner_for_resume(key).map_err(refuse)?;
        Ok(Owner {
            auth_binding: self.registry.auth_binding_of(&tag.owner),
            path: tag.parent_path().to_vec(),
            session: tag.owner,
            model: tag.model,
        })
    }
}

impl SpawnMemberCustomizer for HelperCustomizer {
    fn customize_spawn(
        &self,
        ctx: &SpawnCustomizationContext,
        spec: &mut SpawnMemberSpec,
    ) -> Result<(), MobError> {
        let key = HelperKey::new(ctx.mob_id.clone(), spec.identity.clone());
        if let Some(root) = self.root_for(&key) {
            let fresh = !matches!(ctx.spawn_source, SpawnSource::Resume);
            // Only the host seats an agent: a member another member seats
            // under an agent's name would be shaped as an agent without
            // being one of the daemon's.
            if fresh
                && (ctx.spawner_identity.is_some()
                    || ctx.requested_profile.as_str() != root.profile)
            {
                return Err(refuse(format_args!(
                    "the member name `{}` is reserved for the workspace's agents; choose another",
                    key.member
                )));
            }
            shape_root(
                spec,
                &key,
                root,
                self.shell_env.as_ref(),
                fresh,
                self.gated_fork_off,
            );
            self.bind_policy(spec);
            return Ok(());
        }
        // A fork-derived member's rebuild asks with `Resume`: it keeps the
        // owner it was bound to, when it was.
        let persisted = if matches!(ctx.spawn_source, SpawnSource::Resume) {
            self.persisted_owner(&key).ok()
        } else {
            None
        };
        // Fail closed: a helper whose owner does not resolve to a session
        // open here is refused, never attributed to another session.
        let owner = match persisted {
            Some(owner) => owner,
            None => self
                .registry
                .owner_for_spawn(&ctx.mob_id, ctx.spawner_identity.as_ref())
                .map_err(refuse)?,
        };
        let fresh = !matches!(ctx.spawn_source, SpawnSource::Resume);
        self.admit(key, &owner, spec, None, fresh)
    }

    fn customize_resume(
        &self,
        ctx: &SpawnCustomizationContext,
        durable: &ResumedMemberView,
        spec: &mut SpawnMemberSpec,
    ) -> Result<(), MobError> {
        // A restored member is gated for the owner it was bound to, whether
        // or not that session is open; a member nobody bound is refused.
        let key = HelperKey::new(ctx.mob_id.clone(), spec.identity.clone());
        if let Some(root) = self.root_for(&key) {
            // Rebuilt on its durable session: it keeps that session's model
            // and binding, and its helpers work for that session.
            let (model, binding) = durable.llm_identity.as_ref().map_or_else(
                || (root.model.clone(), root.auth_binding.clone()),
                |identity| (identity.model.clone(), identity.auth_binding.clone()),
            );
            self.registry
                .register_root(key.clone(), &durable.session_id, &model, binding.as_ref());
            if spec.auth_binding.is_none() {
                spec.auth_binding = binding;
            }
            shape_root(
                spec,
                &key,
                root,
                self.shell_env.as_ref(),
                false,
                self.gated_fork_off,
            );
            if let (Some(thinking), Some(identity)) = (&root.thinking, &durable.llm_identity)
                && let crate::thinking::Reapplied::Replace(params) = crate::thinking::reapplied(
                    identity.provider_params.as_ref(),
                    thinking(identity).as_ref(),
                )
            {
                spec.resume_provider_params = Some(params.unwrap_or_default());
            }
            self.bind_policy(spec);
            return Ok(());
        }
        let owner = self.persisted_owner(&key)?;
        self.admit(key, &owner, spec, Some(&durable.session_id), false)
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_tui_owned_builtins_are_denied() {
        use crate::approval::policy::TUI_OWNED_BUILTINS;
        use meerkat_core::ops::ToolAccessPolicy;
        let ToolAccessPolicy::DenyList(names) = tool_access_policy() else {
            unreachable!("a deny list");
        };
        for name in TUI_OWNED_BUILTINS
            .into_iter()
            .chain(crate::approval::policy::SHELL_JOB_TOOLS)
            .chain(DENIED_SPAWN_TOOLS)
        {
            assert!(names.0.iter().any(|n| n.as_str() == name), "{name}");
        }
        assert!(names.0.iter().any(|n| n.as_str() == "fork_off"));
    }

    /// E2 of the security review: a helper on another model than its
    /// owner's runs through the owner's endpoint or is refused; it never
    /// falls back to the model's provider default (its public URL and an
    /// environment key).
    #[test]
    fn a_helper_runs_through_its_owners_endpoint_or_is_refused() {
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let ep = |name: &str, dialect, builtin: bool| Endpoint {
            name: name.to_string(),
            dialect,
            base_url: (!builtin).then(|| "http://localhost:4000".to_string()),
            api_key: SecretString::new("k"),
            key_env: None,
            builtin,
            priority: 0,
            api_style: crate::endpoints::ApiStyle::default(),
        };
        let gateway = ep("gateway", WireDialect::Anthropic, false);
        let other = ep("oai-proxy", WireDialect::OpenAi, false);
        let endpoints = vec![
            ep("anthropic", WireDialect::Anthropic, true),
            ep("openai", WireDialect::OpenAi, true),
            gateway.clone(),
            other,
        ];
        let owner = Owner {
            session: SessionId::new(),
            path: Vec::new(),
            model: "claude-sonnet-4-6".into(),
            auth_binding: gateway.binding(),
        };
        // The same model, another model of the endpoint's dialect, and one
        // the catalog does not know: the owner's endpoint.
        for model in ["claude-sonnet-4-6", "claude-opus-4-6", "house-model"] {
            assert_eq!(
                helper_binding(&owner, model, &endpoints),
                Ok(gateway.binding()),
                "{model}"
            );
        }
        // Another provider's model: refused, naming the endpoints that
        // speak its dialect; never `None` (the provider default).
        let refused = helper_binding(&owner, "gpt-5.5", &endpoints).unwrap_err();
        assert!(
            refused.contains("`gateway`") && refused.contains("openai, oai-proxy"),
            "{refused}"
        );
        // An owner on a built-in endpoint: same rule, its dialect.
        let builtin = Owner {
            auth_binding: None,
            ..owner
        };
        assert_eq!(
            helper_binding(&builtin, "claude-opus-4-6", &endpoints),
            Ok(None)
        );
        assert!(helper_binding(&builtin, "gpt-5.5", &endpoints).is_err());
    }

    /// A helper of an owner on a self-hosted endpoint (an OpenAI-dialect
    /// proxy) that asks for a model by its plain name runs on the
    /// endpoint's alias of it, through the owner's binding, a catalogued
    /// Claude id included; an alias is kept, and an owner on another
    /// endpoint keeps the plain name.
    #[test]
    fn a_helper_of_a_self_hosted_owner_asks_for_the_endpoint_s_alias() {
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let proxy = Endpoint {
            name: "litellm".to_string(),
            dialect: WireDialect::OpenAi,
            base_url: Some("http://localhost:4000".to_string()),
            api_key: SecretString::new("k"),
            key_env: Some("KEY".to_string()),
            builtin: false,
            priority: 0,
            api_style: crate::endpoints::ApiStyle::default(),
        };
        let endpoints = vec![proxy.clone()];
        let owner = Owner {
            session: SessionId::new(),
            path: Vec::new(),
            model: "litellm:gpt-5.5".into(),
            auth_binding: proxy.binding(),
        };
        for (asked, runs) in [
            ("claude-opus-4-6", "litellm:claude-opus-4-6"),
            ("litellm:house", "litellm:house"),
        ] {
            let mut spec = SpawnMemberSpec::new("delegate", "worker");
            spec.model_override = Some(asked.to_string());
            qualify_model(&owner, &mut spec, &endpoints);
            assert_eq!(spec.model_override.as_deref(), Some(runs));
            assert_eq!(
                helper_binding(&owner, runs, &endpoints),
                Ok(proxy.binding()),
                "{runs}"
            );
        }
        let mut inherited = SpawnMemberSpec::new("delegate", "worker");
        qualify_model(&owner, &mut inherited, &endpoints);
        assert_eq!(inherited.model_override, None);
        let elsewhere = Owner {
            auth_binding: None,
            ..owner
        };
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.model_override = Some("gpt-5.5".to_string());
        qualify_model(&elsewhere, &mut spec, &endpoints);
        assert_eq!(spec.model_override.as_deref(), Some("gpt-5.5"));
    }

    fn denies_fork_off(names: &meerkat_core::ToolNameSet) -> bool {
        names.contains("fork_off")
    }

    #[test]
    fn a_helper_without_a_policy_or_inheriting_gets_the_tui_deny_list() {
        use meerkat_core::ops::ToolAccessPolicy;
        for policy in [None, Some(ToolAccessPolicy::Inherit)] {
            let mut spec = SpawnMemberSpec::new("delegate", "worker");
            spec.tool_access_policy = policy;
            deny_ungated_tools(&mut spec, &[]);
            assert_eq!(spec.tool_access_policy, Some(tool_access_policy()));
        }
    }

    #[test]
    fn a_helpers_deny_list_gains_the_tui_names() {
        use meerkat_core::ops::ToolAccessPolicy;
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.tool_access_policy = Some(ToolAccessPolicy::DenyList(
            ["web_fetch"].into_iter().collect(),
        ));
        deny_ungated_tools(&mut spec, &[]);
        let Some(ToolAccessPolicy::DenyList(names)) = &spec.tool_access_policy else {
            panic!("{:?}", spec.tool_access_policy);
        };
        assert!(
            names.contains("web_fetch") && denies_fork_off(names),
            "{names:?}"
        );
    }

    #[test]
    fn a_helpers_allow_list_loses_the_tui_names() {
        use meerkat_core::ops::ToolAccessPolicy;
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.tool_access_policy = Some(ToolAccessPolicy::AllowList(
            ["read_file", "fork_off"].into_iter().collect(),
        ));
        deny_ungated_tools(&mut spec, &[]);
        let Some(ToolAccessPolicy::AllowList(names)) = &spec.tool_access_policy else {
            panic!("{:?}", spec.tool_access_policy);
        };
        assert!(
            names.contains("read_file") && !names.contains("fork_off"),
            "{names:?}"
        );
    }

    #[test]
    fn a_read_only_helper_stays_read_only_and_constraints_gain_a_deny() {
        use meerkat_core::ops::{ToolAccessConstraint, ToolAccessPolicy};
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.tool_access_policy = Some(ToolAccessPolicy::ReadOnly);
        deny_ungated_tools(&mut spec, &[]);
        assert_eq!(spec.tool_access_policy, Some(ToolAccessPolicy::ReadOnly));

        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.tool_access_policy = Some(ToolAccessPolicy::Constraints(vec![
            ToolAccessConstraint::ReadOnly,
        ]));
        deny_ungated_tools(&mut spec, &[]);
        let Some(ToolAccessPolicy::Constraints(constraints)) = &spec.tool_access_policy else {
            panic!("{:?}", spec.tool_access_policy);
        };
        assert_eq!(constraints.len(), 2);
        assert!(matches!(
            &constraints[1],
            ToolAccessConstraint::DenyNames(names) if denies_fork_off(names)
        ));
    }

    fn denied_names(spec: &SpawnMemberSpec) -> meerkat_core::ToolNameSet {
        use meerkat_core::ops::ToolAccessPolicy;
        let Some(ToolAccessPolicy::DenyList(names)) = &spec.tool_access_policy else {
            panic!("{:?}", spec.tool_access_policy);
        };
        names.clone()
    }

    #[test]
    fn a_helper_at_the_depth_limit_is_denied_the_nesting_tools() {
        // The session's own helpers are at depth 1; with the default limit
        // they may not start helpers.
        let depth = Depth::admit("scout", 1, 1, true).unwrap();
        assert_eq!(depth, Depth::Last);
        let mut spec = SpawnMemberSpec::new("delegate", "scout");
        deny_ungated_tools(&mut spec, &NESTING_TOOLS);
        let names = denied_names(&spec);
        for name in ["delegate", "mob_create", "mob_spawn_member", "council"] {
            assert!(names.contains(name), "{name}: {names:?}");
        }
        assert!(denies_fork_off(&names), "{names:?}");
        // A read-only helper keeps read-only and loses them too.
        let mut spec = SpawnMemberSpec::new("delegate", "scout");
        spec.tool_access_policy = Some(meerkat_core::ops::ToolAccessPolicy::ReadOnly);
        deny_ungated_tools(&mut spec, &NESTING_TOOLS);
        let Some(meerkat_core::ops::ToolAccessPolicy::Constraints(constraints)) =
            &spec.tool_access_policy
        else {
            panic!("{:?}", spec.tool_access_policy);
        };
        assert!(matches!(
            constraints.as_slice(),
            [
                meerkat_core::ops::ToolAccessConstraint::ReadOnly,
                meerkat_core::ops::ToolAccessConstraint::DenyNames(names),
            ] if names.contains("delegate") && names.contains("council")
        ));
    }

    #[test]
    fn a_helper_at_the_depth_limit_has_the_mob_category_off() {
        use meerkat_core::ToolCategoryOverride::{Disable, Inherit};
        // Without an inherited filter only the mob category changes.
        let mut spec = SpawnMemberSpec::new("delegate", "scout");
        drop_mob_tools(&mut spec);
        let overrides = &spec.tool_category_overrides;
        assert_eq!(overrides.mob, Disable);
        assert_eq!((overrides.shell, overrides.builtins), (Inherit, Inherit));
    }

    #[test]
    fn a_helper_below_the_depth_limit_keeps_the_nesting_tools() {
        let depth = Depth::admit("scout", 1, 2, true).unwrap();
        assert_eq!(depth, Depth::MayNest);
        let mut spec = SpawnMemberSpec::new("delegate", "scout");
        deny_ungated_tools(&mut spec, &[]);
        let names = denied_names(&spec);
        for name in NESTING_TOOLS {
            assert!(!names.contains(name), "{name}: {names:?}");
        }
        assert!(denies_fork_off(&names));
        assert_eq!(Depth::admit("inner", 2, 2, true).unwrap(), Depth::Last);
    }

    #[test]
    fn a_spawn_beyond_the_depth_limit_is_refused() {
        let err = Depth::admit("inner", 2, 1, true).unwrap_err().to_string();
        assert!(err.contains("helper refused"), "{err}");
        assert!(
            err.contains("`inner`") && err.contains("helpers.max_depth"),
            "{err}"
        );
        // At 0 even the session's own helpers are refused.
        assert!(Depth::admit("scout", 1, 0, true).is_err());
        // A restored member from before the limit was lowered keeps running
        // with the tools denied.
        assert_eq!(Depth::admit("inner", 2, 1, false).unwrap(), Depth::Last);
    }

    fn key() -> HelperKey {
        HelperKey::new(
            meerkat_mob::MobId::from("tui"),
            meerkat_mob::AgentIdentity::from("main"),
        )
    }

    fn root() -> RootMember {
        RootMember {
            mob: meerkat_mob::MobId::from("tui"),
            is_root: |name| name == "main" || name.starts_with("main-"),
            profile: "main".to_string(),
            tools: Arc::new(|_| {
                Arc::new(meerkat_tools::EmptyToolDispatcher) as Arc<dyn AgentToolDispatcher>
            }),
            instructions: Arc::new(|_| vec!["persona".to_string(), "agents".to_string()]),
            model: "m".to_string(),
            auth_binding: None,
            admission: None,
            thinking: None,
        }
    }

    #[test]
    fn the_root_member_is_shaped_as_an_owner() {
        let curated = HashMap::from([("PATH".to_string(), "/bin".to_string())]);
        let root = root();
        let mut spec = SpawnMemberSpec::host_root("main", "main");
        shape_root(&mut spec, &key(), &root, Some(&curated), true, false);
        assert!(spec.external_tools.is_some());
        assert!(spec.tool_dispatch_admission.is_none(), "no gate was given");
        assert_eq!(spec.shell_env.as_ref(), Some(&curated));
        assert_eq!(
            spec.additional_instructions,
            Some(vec!["persona".to_string(), "agents".to_string()])
        );
        assert_eq!(spec.tool_access_policy, Some(tool_access_policy()));
        // A rebuild gets its tools back, not a second set of instructions,
        // and never the start-up endpoint's binding (it keeps its own).
        let mut resumed = SpawnMemberSpec::host_root("main", "main");
        let on_proxy = RootMember {
            auth_binding: Some(meerkat_core::AuthBindingRef {
                realm: meerkat_core::RealmId::parse(crate::endpoints::SELF_HOSTED_REALM).unwrap(),
                binding: meerkat_core::BindingId::parse("litellm").unwrap(),
                profile: None,
                origin: meerkat_core::BindingOrigin::Configured,
            }),
            ..self::root()
        };
        shape_root(
            &mut resumed,
            &key(),
            &on_proxy,
            Some(&curated),
            false,
            false,
        );
        assert!(resumed.external_tools.is_some());
        assert_eq!(resumed.additional_instructions, None);
        assert_eq!(resumed.auth_binding, None);
        let mut fresh = SpawnMemberSpec::host_root("main", "main");
        shape_root(&mut fresh, &key(), &on_proxy, Some(&curated), true, false);
        assert_eq!(fresh.auth_binding, on_proxy.auth_binding);
        // The daemon's root keeps `fork_off` (its children are gated).
        let mut gated = SpawnMemberSpec::host_root("main", "main");
        shape_root(&mut gated, &key(), &root, Some(&curated), true, true);
        let Some(meerkat_core::ops::ToolAccessPolicy::DenyList(names)) = &gated.tool_access_policy
        else {
            panic!("{:?}", gated.tool_access_policy);
        };
        assert!(
            !names.contains("fork_off") && names.contains("apply_patch"),
            "{names:?}"
        );
        // At the depth limit a member forks no further.
        let mut last = SpawnMemberSpec::new("delegate", "worker");
        deny_tools(&mut last, &NESTING_TOOLS, true);
        let Some(meerkat_core::ops::ToolAccessPolicy::DenyList(names)) = &last.tool_access_policy
        else {
            panic!("{:?}", last.tool_access_policy);
        };
        assert!(denies_fork_off(names), "{names:?}");
    }

    #[test]
    fn a_helper_gets_the_confined_shell_environment_unless_it_names_one() {
        let curated = HashMap::from([("PATH".to_string(), "/home/u/.cargo/bin".to_string())]);
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        inherit_shell_env(&mut spec, Some(&curated));
        assert_eq!(spec.shell_env.as_ref(), Some(&curated));

        let own = HashMap::from([("PATH".to_string(), "/opt/bin".to_string())]);
        let mut named = SpawnMemberSpec::new("delegate", "worker");
        named.shell_env = Some(own.clone());
        inherit_shell_env(&mut named, Some(&curated));
        assert_eq!(named.shell_env, Some(own));

        let mut unconfined = SpawnMemberSpec::new("delegate", "worker");
        inherit_shell_env(&mut unconfined, None);
        assert_eq!(unconfined.shell_env, None);
    }
}
