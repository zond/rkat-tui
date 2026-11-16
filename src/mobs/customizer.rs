//! The spawn customizer every helper passes: it gives the helper the
//! approval gate of the session it works for, that session's model binding,
//! rkat-tui's file tools and the helper instructions, and tells the
//! [`super::follow`] task to observe it.
//!
//! A host that runs its main agent as a mob member (the `rkat-tui-mob`
//! daemon's agent) names it as the customizer's [`RootMember`]: that
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
pub const NESTING_TOOLS: [&str; 4] = [
    crate::tools::ASSIGN,
    "mob_create",
    "mob_spawn_member",
    "council",
];

/// Mob tools denied unless the member's fork is gated
/// ([`HelperCustomizer::with_gated_fork_off`]): a fork child inherits the
/// source's dispatch admission, so the daemon's members keep `fork_off`
/// behind the gate except at the depth limit.
pub const DENIED_SPAWN_TOOLS: [&str; 1] = ["fork_off"];

/// The tool access policy every member's is merged with:
/// [`crate::approval::policy::TUI_OWNED_BUILTINS`], the shell's job controls
/// ([`crate::approval::policy::SHELL_JOB_TOOLS`]), the withdrawn mob tools
/// ([`crate::approval::policy::WITHDRAWN_TOOLS`]: `delegate`) and
/// [`DENIED_SPAWN_TOOLS`] denied.
#[must_use]
pub fn tool_access_policy() -> meerkat_core::ops::ToolAccessPolicy {
    meerkat_core::ops::ToolAccessPolicy::DenyList(
        crate::approval::policy::TUI_OWNED_BUILTINS
            .into_iter()
            .chain(crate::approval::policy::SHELL_JOB_TOOLS)
            .chain(crate::approval::policy::WITHDRAWN_TOOLS)
            .chain(DENIED_SPAWN_TOOLS)
            .collect::<meerkat_core::ToolNameSet>(),
    )
}

/// Builds the external tools of one helper (rkat-tui's file tools).
pub type HelperTools = Arc<dyn Fn() -> Arc<dyn AgentToolDispatcher> + Send + Sync>;

/// A helper about to be spawned, for the follower task.
#[derive(Debug, Clone)]
pub struct Spawning {
    /// Mob id and member identity.
    pub key: HelperKey,
    /// What the helper is.
    pub tag: HelperTag,
    /// A fresh fork child's latch: its tool calls wait (at most
    /// [`crate::approval::admission::WIRE_WAIT`]) until the follower has
    /// wired it to its parent and opened this, or dropped it.
    pub wired: Option<Arc<tokio::sync::watch::Sender<bool>>>,
}

/// What the host supplies for its root members (the daemon's agents) on
/// every spawn and rebuild: the engine persists none of it.
pub trait RootHost: Send + Sync {
    /// The external tools of the member `member` (each its own: the file
    /// tools report their changes under its name).
    fn tools(&self, member: &AgentIdentity) -> Arc<dyn AgentToolDispatcher>;

    /// Its instruction sections on a fresh spawn (the persona first).
    fn instructions(&self, member: &AgentIdentity) -> Vec<String>;

    /// The approval gate of the member `key`'s own calls (`None`: none,
    /// a host without a gate).
    fn admission(&self, key: &HelperKey) -> Option<Arc<dyn ToolDispatchAdmission>> {
        let _ = key;
        None
    }

    /// The `[thinking]` a rebuilt member's identity is configured with
    /// (its model may have been switched since the spawn).
    fn thinking(&self, identity: &meerkat_core::SessionLlmIdentity) -> RootThinking {
        let _ = identity;
        RootThinking::Keep
    }
}

/// What a rebuilt root member's provider params get.
#[derive(Debug, Clone, PartialEq)]
pub enum RootThinking {
    /// The persisted params stay as they are.
    Keep,
    /// The params of this `[thinking]` setting (`None`: no override).
    Set(Option<meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>),
}

/// The host-root members that are owners themselves, not helpers: the mob
/// daemon's agents (its agents), every member of one mob whose
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
    /// That profile as this start defines it, put on every spawn and
    /// rebuild of theirs as the override profile: the mob keeps the
    /// definition of the realm's first start, so without it the MCP
    /// servers, memory tools and compaction threshold of a later start
    /// never reach them (`crate::daemon::mob::main_profile`).
    /// `None`: the mob's stored profile.
    pub current_profile: Option<meerkat_mob::Profile>,
    /// Their tools, instructions, gate and thinking.
    pub host: Arc<dyn RootHost>,
    /// The model it starts on (a rebuilt member keeps its session's).
    pub model: String,
    /// The start-up endpoint's auth binding.
    pub auth_binding: Option<AuthBindingRef>,
}

impl std::fmt::Debug for RootMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootMember")
            .field("mob", &self.mob)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// rkat-tui's [`SpawnMemberCustomizer`].
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
    /// `helpers.model`, `allow_models` and `deny_models` as in force now
    /// (the daemon's settings hook replaces them).
    models: crate::config::helper_models::SharedHelperModels,
    /// The realm's config document, which holds the self-hosted aliases a
    /// helper's model needs (`None`: not checked).
    realm_config: Option<std::path::PathBuf>,
    /// The mobs' definitions: a member seated by a profile's name gets
    /// that profile inline, with its model, provider and server replaced.
    profiles: Arc<super::profiles::MobProfiles>,
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
            models: crate::config::helper_models::SharedHelperModels::default(),
            realm_config: None,
            profiles: Arc::default(),
        }
    }

    /// Read the profiles members are seated by from `profiles` (the
    /// daemon keeps it current, [`super::profiles::MobProfiles::follow`]).
    #[must_use]
    pub fn with_profiles(mut self, profiles: Arc<super::profiles::MobProfiles>) -> Self {
        self.profiles = profiles;
        self
    }

    /// Refuse a helper on a self-hosted model whose alias the realm's
    /// config document at `path` does not hold, naming the ones it does
    /// ([`unregistered`]).
    #[must_use]
    pub fn with_realm_config(mut self, path: std::path::PathBuf) -> Self {
        self.realm_config = Some(path);
        self
    }

    /// Choose and check helpers' models with `models` (shared: a change
    /// of the settings applies to the helpers spawned from then on).
    #[must_use]
    pub fn with_models(mut self, models: crate::config::helper_models::SharedHelperModels) -> Self {
        self.models = models;
        self
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

    /// The stored profile the fresh spawn `spec` is seated by: none when
    /// it carries its own or names the implicit mob's `delegate` profile;
    /// the workspace mob's `main` as this start defines it (the stored one
    /// is the first start's, lukacf/meerkat#2184); else its mob's.
    fn seat(&self, ctx: &SpawnCustomizationContext, spec: &SpawnMemberSpec) -> Seat {
        let profile = spec.role_name.as_str();
        if spec.override_profile.is_some()
            || (ctx.mob_id.as_str().starts_with("implicit-") && profile == DELEGATE_PROFILE)
        {
            return Seat::Carried;
        }
        if let Some(current) = self
            .root
            .as_ref()
            .filter(|root| root.mob == ctx.mob_id && root.profile == profile)
            .and_then(|root| root.current_profile.clone())
        {
            return Seat::Stored(Box::new(current));
        }
        match self.profiles.lookup(&ctx.mob_id, profile) {
            super::profiles::Stored::Profile(profile) => Seat::Stored(profile),
            super::profiles::Stored::Missing => Seat::Missing,
            super::profiles::Stored::Unknown => Seat::Unknown {
                mob: ctx.mob_id.clone(),
            },
        }
    }
}

/// Shape the root member `key`'s `spec` as an owner session: its tools,
/// the curated shell environment, rkat-tui's tool access policy, its
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
    if let Some(profile) = &root.current_profile {
        spec.override_profile = Some(profile.clone());
    }
    if let Some(admission) = root.host.admission(key) {
        spec.tool_dispatch_admission = Some(admission);
    }
    if spec.external_tools.is_none() {
        spec.external_tools = Some(root.host.tools(&key.member));
    }
    inherit_shell_env(spec, shell_env);
    // The engine's schedule category stays off, also for an agent whose
    // mob was created when its profile named it (the mob keeps its creation
    // definition): an agent's schedule tools come with its mob tools
    // (`crate::daemon::deferral::AgentTools`).
    spec.tool_category_overrides.schedule = meerkat_core::ToolCategoryOverride::Disable;
    if fresh {
        spec.additional_instructions
            .get_or_insert_with(Vec::new)
            .extend(root.host.instructions(&key.member));
    }
    deny_tools(spec, &[], gated_fork_off);
}

/// The model `spec` names (an explicit override or an inline profile's;
/// the daemon has no realm profile store, so a spawn's profile is inline).
/// On a fresh spawn it is read before the stored profile a member is
/// seated by is put on it ([`choose_model`]); on a rebuild it is the model
/// the spawn chose, which the engine gives back.
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
    let own = own_endpoint(owner, endpoints);
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

/// The endpoint `owner` runs through: the configured one of its binding,
/// else (no binding) the built-in one of its model's dialect.
fn own_endpoint<'a>(
    owner: &Owner,
    endpoints: &'a [crate::endpoints::Endpoint],
) -> Option<&'a crate::endpoints::Endpoint> {
    use crate::endpoints::WireDialect;
    endpoints
        .iter()
        .find(|endpoint| endpoint.binding() == owner.auth_binding && !endpoint.is_builtin())
        .or_else(|| {
            owner.auth_binding.is_none().then(|| {
                endpoints.iter().find(|endpoint| {
                    endpoint.is_builtin()
                        && Some(endpoint.dialect) == WireDialect::of_catalog_model(&owner.model)
                })
            })?
        })
}

/// How a fresh helper came to be asked for, which decides where its model
/// comes from when the spawn names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// Any spawn (`delegate`, `spawn_member`, `mob_spawn_member`, a batch):
    /// the model it names, else `helpers.model`, else its owner's.
    Spawn,
    /// A `fork_off` child: it continues its source's conversation, so it
    /// stays on its source's (its owner's) model; `helpers.model` does not
    /// apply, and what the engine copied from the source is not named by
    /// the call.
    Fork,
}

/// The profile the engine seated a `delegate` helper with, in its owner's
/// implicit mob (`implicit-<session id>`); `delegate` is withdrawn
/// ([`crate::approval::policy::WITHDRAWN_TOOLS`]), and such a helper is one
/// a realm of an earlier build holds. The engine keeps that profile on
/// the owner's current model, provider and binding
/// (`MobMcpState::ensure_implicit_mob_for_model`), so it is the one stored
/// profile left as it is.
const DELEGATE_PROFILE: &str = "delegate";

impl Asked {
    /// How the spawn `ctx` describes was asked for (`None`: a rebuild or
    /// a respawn, which keeps the model its spawn chose).
    fn of(ctx: &SpawnCustomizationContext) -> Option<Self> {
        match ctx.spawn_source {
            SpawnSource::Fork => Some(Self::Fork),
            SpawnSource::Resume
            | SpawnSource::PersistedForkResume
            | SpawnSource::Respawn
            | SpawnSource::IdentityReconcile
            | SpawnSource::AttachedForkedParticipant
            | SpawnSource::PolicySpawn => None,
            _ => Some(Self::Spawn),
        }
    }
}

/// The stored profile a fresh spawn is seated by, to be put on it inline
/// ([`choose_model`]).
#[derive(Debug, Clone, PartialEq)]
enum Seat {
    /// The spawn carries its own profile (inline, or a fork's copy of its
    /// source's), or names the implicit mob's `delegate` profile: nothing
    /// to put on it.
    Carried,
    /// This stored profile.
    Stored(Box<meerkat_mob::Profile>),
    /// The mob's definition has no inline profile of the name: the engine
    /// refuses the spawn itself, naming it.
    Missing,
    /// The mob's definition could not be read: the lookup's own read
    /// failed or ran out of its bound ([`super::profiles`]).
    Unknown {
        /// The mob.
        mob: MobId,
    },
}

/// A fresh helper's model, chosen and checked, and put on `spec`: the model
/// its spawn names (an override, an inline profile's), else `helpers.model`,
/// else its owner's; a `fork_off` child always its owner's. A stored
/// profile's model never decides: the profile the member is seated by
/// (`seat`) is put on the spawn inline, and the chosen model replaces its
/// model, as the engine's `model_override` replaces only the model of a
/// profile it resolves itself and keeps the provider and server it
/// declares. The model, as its plain name (an `endpoint:` prefix of the
/// owner's endpoint removed), must pass the user's `helpers.allow_models` /
/// `deny_models`. It runs on the owner's endpoint: on a self-hosted one as
/// the alias `<endpoint>:<model>`, and every profile on the spawn names
/// that endpoint's provider and server. The engine id it runs on.
///
/// # Errors
///
/// The refusal, in words for the agent.
fn choose_model(
    owner: &Owner,
    spec: &mut SpawnMemberSpec,
    endpoints: &[crate::endpoints::Endpoint],
    models: &crate::config::helper_models::HelperModels,
    asked: Asked,
    seat: Seat,
) -> Result<String, String> {
    use crate::config::helper_models::ModelSource;
    let own = own_endpoint(owner, endpoints);
    let named = match asked {
        Asked::Spawn => requested_model(spec),
        Asked::Fork => None,
    };
    let (raw, source) = match (named, &models.default, asked) {
        (Some(model), _, _) => (model, ModelSource::Named),
        (None, Some(default), Asked::Spawn) => (default.clone(), ModelSource::Default),
        _ => (owner.model.clone(), ModelSource::Owner),
    };
    let plain = plain_model(&raw, own, endpoints, source)?;
    if let Some(refused) = models.refusal(&plain, source) {
        return Err(refused);
    }
    let engine = match (source, own.filter(|endpoint| endpoint.is_self_hosted())) {
        // The owner's own model is its engine id already.
        (ModelSource::Owner, _) => owner.model.clone(),
        (_, Some(endpoint)) => endpoint.engine_model(&plain),
        (_, None) => plain,
    };
    match seat {
        Seat::Carried | Seat::Missing => {}
        Seat::Stored(mut profile) => {
            // A fallback chain the profile names is a stored profile's
            // models too.
            profile.model_fallback = None;
            spec.override_profile = Some(*profile);
        }
        Seat::Unknown { mob } => {
            return Err(format!(
                "rkat-tui could not read the profiles of the mob `{mob}` in time, so `{}` cannot be given its model; call the spawn again, or give the profile inline in `tooling`",
                spec.identity
            ));
        }
    }
    spec.model_override = Some(engine.clone());
    if let Some(profile) = spec.override_profile.as_mut() {
        profile.model.clone_from(&engine);
        if let Some(endpoint) = own {
            profile.provider = Some(endpoint.engine_provider());
            profile.self_hosted_server_id = endpoint.self_hosted_server();
        }
    }
    Ok(engine)
}

/// The models the self-hosted `endpoint` has an alias for in the realm
/// config document at `config` (`None`: it does not read, and the engine
/// is left to judge). A self-hosted model runs only with its alias
/// registered, which the start-up does for every model the endpoint lists
/// and `/model` for the one it switches to; the build of a helper on any
/// other is refused by the engine as `self-hosted model … is not
/// registered in config`.
fn registered_models(
    config: &std::path::Path,
    endpoint: &crate::endpoints::Endpoint,
) -> Option<Vec<String>> {
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    let table: toml::Table = text.parse().ok()?;
    let prefix = format!("{}:", endpoint.name);
    let mut models: Vec<String> = table
        .get("self_hosted")
        .and_then(|value| value.get("models"))
        .and_then(toml::Value::as_table)
        .map(|models| {
            models
                .keys()
                .filter_map(|alias| alias.strip_prefix(&prefix).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    Some(models)
}

/// A helper on `engine` (a self-hosted alias) of the owner's endpoint
/// `endpoint`, refused when the alias is not registered, naming the
/// models that are and what the user can do.
fn unregistered(
    config: &std::path::Path,
    endpoint: &crate::endpoints::Endpoint,
    engine: &str,
) -> Option<String> {
    let models = registered_models(config, endpoint)?;
    let model = engine.strip_prefix(&format!("{}:", endpoint.name))?;
    if models.iter().any(|known| known == model) {
        return None;
    }
    Some(format!(
        "the endpoint `{}` has no model `{model}` registered: it serves helpers only the models it listed when the daemon started or that /model switched to ({}); name one of those, or ask the user to switch to `{model}` once with /model (which registers it) or to restart the daemon once the endpoint lists it",
        endpoint.name,
        if models.is_empty() {
            "none".to_string()
        } else {
            models.join(", ")
        }
    ))
}

/// `model` as its plain name: an `endpoint:` prefix naming the owner's
/// endpoint `own` removed. A prefix naming another configured endpoint is
/// refused (a helper runs through its owner's endpoint); a `:` after
/// anything else is part of the model's name.
///
/// # Errors
///
/// The refusal, in words for the agent.
fn plain_model(
    model: &str,
    own: Option<&crate::endpoints::Endpoint>,
    endpoints: &[crate::endpoints::Endpoint],
    source: crate::config::helper_models::ModelSource,
) -> Result<String, String> {
    use crate::config::helper_models::{MODEL_KEY, ModelSource};
    let Some((prefix, rest)) = model.split_once(':') else {
        return Ok(model.to_string());
    };
    if own.is_some_and(|endpoint| endpoint.name == prefix) {
        return Ok(rest.to_string());
    }
    if !endpoints.iter().any(|endpoint| endpoint.name == prefix) {
        return Ok(model.to_string());
    }
    let own = own.map_or_else(|| "unknown".to_string(), |e| format!("`{}`", e.name));
    Err(match source {
        ModelSource::Default => format!(
            "{MODEL_KEY} = {model} names the endpoint `{prefix}`, but a helper runs through its owner's endpoint ({own}); the call must name a model that endpoint serves (and the user's {MODEL_KEY} should name one)"
        ),
        ModelSource::Named | ModelSource::Owner => format!(
            "`{model}` names the endpoint `{prefix}`, but a helper runs through its owner's endpoint ({own}); name a model that endpoint serves, without an endpoint, or switch your own model (/model) first"
        ),
    })
}

/// What [`shape`] puts on a helper's spec, besides its owner and key.
struct Shaping<'a> {
    registry: &'a Arc<HelperRegistry>,
    engine: &'a Arc<ApprovalEngine>,
    tools: &'a HelperTools,
    shell_env: Option<&'a HashMap<String, String>>,
    /// A fresh fork's latch ([`ApprovalGate::waiting_for`]).
    wired: Option<tokio::sync::watch::Receiver<bool>>,
}

/// Apply the owner's gate, tools, instructions and the shell's environment
/// to `spec` (its binding is [`helper_binding`]'s).
fn shape(spec: &mut SpawnMemberSpec, owner: &Owner, key: &HelperKey, shaping: Shaping<'_>) {
    let Shaping {
        registry,
        engine,
        tools,
        shell_env,
        wired,
    } = shaping;
    // A Stop answer to one of the helper's dialogs stops this helper only.
    let on_stop: crate::approval::admission::OnStop = {
        let registry = Arc::downgrade(registry);
        let key = key.clone();
        Arc::new(move || {
            if let Some(registry) = registry.upgrade() {
                let tag = registry.live_tag(&key);
                registry.endings().stop(&key, tag);
            }
        })
    };
    let gate = ApprovalGate::for_helper(
        Arc::clone(engine),
        owner.session.clone(),
        key.member.as_str(),
    )
    .after(spec.tool_dispatch_admission.take())
    .stopping_with(on_stop)
    .waiting_for(wired);
    spec.tool_dispatch_admission = Some(Arc::new(gate));
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
/// Every helper's tool access policy denies rkat-tui's denied tools
/// ([`tool_access_policy`]), merged into whatever policy its
/// spec names: none or `Inherit` becomes rkat-tui's deny list, a deny list
/// gains the names, an allow list loses them, constraints gain a deny
/// constraint, and `ReadOnly` (which already admits no mutating tool) stays.
/// `also` adds names to deny (a helper at the depth limit gets
/// [`NESTING_TOOLS`]); with any, `ReadOnly` becomes a read-only constraint
/// beside the deny constraint, so the names are denied whatever read-only
/// admits. `gated_fork_off` keeps `fork_off` when nothing else is denied
/// (`also` is empty: a member at the depth limit may not fork either, as a
/// fork child is a helper one level deeper).
fn deny_tools(spec: &mut SpawnMemberSpec, also: &[&str], gated_fork_off: bool) {
    use meerkat_core::ops::{ToolAccessConstraint, ToolAccessPolicy};
    let Some(denied) = denied_names(also, gated_fork_off) else {
        // The TUI policy is a deny list; anything else is a programming
        // error that must not widen a helper's tools.
        spec.tool_access_policy = Some(tool_access_policy());
        return;
    };
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

/// The names [`deny_tools`] denies (`None`: [`tool_access_policy`] is not a
/// deny list).
fn denied_names(also: &[&str], gated_fork_off: bool) -> Option<meerkat_core::ToolNameSet> {
    let meerkat_core::ops::ToolAccessPolicy::DenyList(mut denied) = tool_access_policy() else {
        return None;
    };
    for name in also {
        denied.insert((*name).to_string());
    }
    if gated_fork_off && also.is_empty() {
        for name in DENIED_SPAWN_TOOLS {
            denied.remove(name);
        }
    }
    Some(denied)
}

/// The tool access policy the daemon's agents are built with
/// ([`shape_root`] with `fork_off` gated): what a scheduled helper target
/// one of them creates inherits.
#[must_use]
pub fn agent_tool_access_policy() -> meerkat_core::ops::ToolAccessPolicy {
    denied_names(&[], true).map_or_else(
        tool_access_policy,
        meerkat_core::ops::ToolAccessPolicy::DenyList,
    )
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

/// The line an agent's (or helper's) instructions get naming its scratch
/// directory.
#[must_use]
pub fn scratch_instruction(dir: &std::path::Path) -> String {
    format!(
        "Your scratch directory is `{}` (it is also TMPDIR, TMP and TEMP in your shell): use it for temporary files, logs and experiments instead of /tmp or the project. It is private to you and your helpers, writing there needs no review, and it is removed when you are retired or the daemon stops. `blob_save_file` and `view_image` accept only paths inside the project, not this directory: have `blob_save_file` save inside the project and move the file from there with the shell, and copy an image into the project before `view_image` reads it.",
        dir.display()
    )
}

/// Point `spec`'s shell at the scratch directory `dir`: its temp variables
/// name it, on top of the environment the shell already gets (the
/// confined shell's curated set; an unconfined shell inherits the daemon's
/// and only these are set), and, when `instruct`, its instructions say so.
fn give_scratch(spec: &mut SpawnMemberSpec, dir: &std::path::Path, instruct: bool) {
    spec.shell_env = Some(crate::approval::scratch::ScratchDirs::shell_env(
        spec.shell_env.as_ref(),
        dir,
    ));
    if instruct {
        spec.additional_instructions
            .get_or_insert_with(Vec::new)
            .push(scratch_instruction(dir));
    }
}

impl HelperCustomizer {
    /// Shape `spec` for `owner`, bind it as `key` and have it followed. A
    /// fresh spawn names its `kind` (and, when it was `asked` for, its
    /// model is chosen); a rebuild keeps the kind it was bound with.
    fn admit(
        &self,
        key: HelperKey,
        owner: &Owner,
        spec: &mut SpawnMemberSpec,
        session: Option<&SessionId>,
        kind: Option<super::HelperKind>,
        asked: Option<(Asked, Seat)>,
    ) -> Result<(), MobError> {
        let label = spec.identity.to_string();
        let fresh = kind.is_some();
        let kind = kind.unwrap_or_else(|| {
            self.registry
                .owner_for_resume(&key)
                .map_or(super::HelperKind::Other, |tag| tag.kind)
        });
        // A rebuilt or respawned member keeps the model its spawn chose
        // (the engine gives its spec the persisted override or profile
        // back).
        let chosen = match asked.filter(|_| fresh) {
            Some((asked, seat)) => {
                let models = self.models.read().clone();
                let chosen = choose_model(owner, spec, &self.endpoints, &models, asked, seat)
                    .map_err(refuse)?;
                if let (Some(config), Some(endpoint)) = (
                    &self.realm_config,
                    own_endpoint(owner, &self.endpoints).filter(|e| e.is_self_hosted()),
                ) && chosen != owner.model
                    && let Some(refused) = unregistered(config, endpoint, &chosen)
                {
                    return Err(refuse(refused));
                }
                Some(chosen)
            }
            None => None,
        };
        let model = chosen
            .or_else(|| requested_model(spec))
            .unwrap_or_else(|| owner.model.clone());
        let mut path = owner.path.clone();
        path.push(label.clone());
        let tag = HelperTag {
            owner: owner.session.clone(),
            path,
            model: model.clone(),
            kind,
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
        if fresh && kind == super::HelperKind::Fork {
            self.registry.endings().first_run_told_by_engine(&key);
        }
        if let Some(session) = session {
            self.registry.bind_session(&key, &tag, session);
        }
        // A fresh fork's first steps wait for its edge to its parent: its
        // tools then find the parent in `peers` from its first call.
        let wired = (fresh && kind == super::HelperKind::Fork)
            .then(|| Arc::new(tokio::sync::watch::channel(false).0));
        shape(
            spec,
            owner,
            &key,
            Shaping {
                registry: &self.registry,
                engine: &self.engine,
                tools: &self.tools,
                shell_env: self.shell_env.as_ref(),
                wired: wired.as_ref().map(|wired| wired.subscribe()),
            },
        );
        // The helper shares its agent's scratch directory; its persona is
        // given again on every build, and so is this line.
        if let Some(dir) = self.registry.scratch_of_session(&owner.session) {
            give_scratch(spec, &dir, true);
        }
        deny_tools(spec, depth.nesting_denied(), self.gated_fork_off);
        if depth == Depth::MayNest {
            // A helper that may start helpers is told their models (its
            // instructions are given again on every build).
            let line = self.models.read().instruction();
            spec.additional_instructions
                .get_or_insert_with(Vec::new)
                .push(line);
        } else {
            drop_mob_tools(spec);
        }
        self.bind_policy(spec);
        let _ = self.spawning.send(Spawning { key, tag, wired });
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
            // Its instructions are given once, at its first seat; the
            // directory's name stays the same for it.
            if let Some(dir) = self
                .registry
                .scratch_for_agent(key.member.as_str(), |path| {
                    self.engine.grants().plan_writes(path)
                })
            {
                give_scratch(spec, &dir, fresh);
            }
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
        // Every child is wired to the member that seats it, asked or not:
        // the engine wires a spawn in its parent's own mob only when this
        // is set (a `delegate` helper is wired across mobs by the engine
        // itself; a `fork_off` child's spawn ignores it, and the follower
        // wires the child once it is seated, `super::follow`).
        if fresh {
            spec.auto_wire_parent = true;
        }
        let asked = Asked::of(ctx)
            .filter(|_| fresh)
            .map(|asked| (asked, self.seat(ctx, spec)));
        let kind = fresh.then(|| kind_of(ctx, spec));
        self.admit(key, &owner, spec, None, kind, asked)
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
            if let Some(dir) = self
                .registry
                .scratch_for_agent(key.member.as_str(), |path| {
                    self.engine.grants().plan_writes(path)
                })
            {
                give_scratch(spec, &dir, false);
            }
            if let Some(identity) = &durable.llm_identity
                && let RootThinking::Set(thinking) = root.host.thinking(identity)
                && let crate::thinking::Reapplied::Replace(params) =
                    crate::thinking::reapplied(identity.provider_params.as_ref(), thinking.as_ref())
            {
                spec.resume_provider_params = Some(params.unwrap_or_default());
            }
            self.bind_policy(spec);
            return Ok(());
        }
        let owner = self.persisted_owner(&key)?;
        self.admit(key, &owner, spec, Some(&durable.session_id), None, None)
    }
}

/// What a fresh spawn starts, as far as its parent's notes go: an `assign`
/// helper carries [`super::ASSIGN_LABEL`]; a member an agent or helper
/// seats in a mob of its own (`spawn_member`, `mob_spawn_member`, a batch)
/// is kept until retired; a `fork_off` child is a fork (wired to its parent,
/// its later runs told); a `delegate` helper and a council's participant are
/// told of by their own tools.
fn kind_of(ctx: &SpawnCustomizationContext, spec: &SpawnMemberSpec) -> super::HelperKind {
    use super::HelperKind;
    // A fork first: a fork of an `assign` helper carries its source's
    // labels, the assign label among them. A `fork_off` child's seat asks
    // with `PersistedForkResume` (its fork is committed first), a
    // prompt-context fork's with `Fork` (both probed 2026-11-16).
    if matches!(
        ctx.spawn_source,
        SpawnSource::Fork | SpawnSource::PersistedForkResume
    ) {
        return HelperKind::Fork;
    }
    if spec
        .labels
        .as_ref()
        .is_some_and(|labels| labels.contains_key(super::ASSIGN_LABEL))
    {
        return HelperKind::Assigned;
    }
    let mob = ctx.mob_id.as_str();
    match ctx.spawn_source {
        SpawnSource::HelperSpawn | SpawnSource::AttachedForkedParticipant => HelperKind::Other,
        _ if mob.starts_with("implicit-") || mob.starts_with("council") => HelperKind::Other,
        _ => HelperKind::Member,
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::helper_models::{HelperModels, ModelPattern, ModelRule};

    /// The scratch line names the directory and says that the engine's
    /// `blob_save_file` and `view_image` refuse it (their own check keeps
    /// them inside the project root), with the way around.
    #[test]
    fn the_scratch_line_says_the_blob_tools_stay_in_the_project() {
        let line = scratch_instruction(std::path::Path::new("/s/main-ab12"));
        assert!(line.contains("`/s/main-ab12`"), "{line}");
        for part in [
            "`blob_save_file` and `view_image` accept only paths inside the project",
            "move the file from there with the shell",
            "copy an image into the project",
        ] {
            assert!(line.contains(part), "{part}: {line}");
        }
    }

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
        // `delegate` is withdrawn from every member, agents included: the
        // agents' policy keeps `fork_off` (gated), not `delegate`.
        assert!(names.0.iter().any(|n| n.as_str() == "delegate"));
        let ToolAccessPolicy::DenyList(agents) = agent_tool_access_policy() else {
            unreachable!("a deny list");
        };
        assert!(agents.contains("delegate") && !agents.contains("fork_off"));
    }

    /// E2 of the security review: a helper on another model than its
    /// owner's runs through the owner's endpoint or is refused; it never
    /// falls back to the model's provider default (its public URL and an
    /// environment key).
    #[test]
    fn a_helper_runs_through_its_owners_endpoint_or_is_refused() {
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let ep = |name: &str, dialect, builtin: bool| {
            let mut endpoint = if builtin {
                Endpoint::builtin(dialect, None, SecretString::new("k"))
            } else {
                Endpoint::configured(
                    name,
                    dialect,
                    Some("http://localhost:4000".to_string()),
                    SecretString::new("k"),
                    "KEY",
                    0,
                    crate::endpoints::ApiStyle::default(),
                )
            };
            endpoint.name = name.to_string();
            endpoint
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
        let proxy = Endpoint::configured(
            "litellm",
            WireDialect::OpenAi,
            Some("http://localhost:4000".to_string()),
            SecretString::new("k"),
            "KEY",
            0,
            crate::endpoints::ApiStyle::default(),
        );
        let endpoints = vec![proxy.clone()];
        let owner = Owner {
            session: SessionId::new(),
            path: Vec::new(),
            model: "litellm:gpt-5.5".into(),
            auth_binding: proxy.binding(),
        };
        let none = HelperModels::default();
        let choose = |owner: &Owner, spec: &mut SpawnMemberSpec| {
            choose_model(owner, spec, &endpoints, &none, Asked::Spawn, Seat::Carried)
        };
        for (asked, runs) in [
            ("claude-opus-4-6", "litellm:claude-opus-4-6"),
            ("litellm:house", "litellm:house"),
        ] {
            let mut spec = SpawnMemberSpec::new("delegate", "worker");
            spec.model_override = Some(asked.to_string());
            assert_eq!(choose(&owner, &mut spec).unwrap(), runs);
            assert_eq!(spec.model_override.as_deref(), Some(runs));
            assert_eq!(
                helper_binding(&owner, runs, &endpoints),
                Ok(proxy.binding()),
                "{runs}"
            );
        }
        let mut inherited = SpawnMemberSpec::new("delegate", "worker");
        assert_eq!(choose(&owner, &mut inherited).unwrap(), "litellm:gpt-5.5");
        assert_eq!(
            inherited.model_override.as_deref(),
            Some("litellm:gpt-5.5"),
            "the owner's model, named on the spawn"
        );
        let elsewhere = Owner {
            auth_binding: None,
            model: "gpt-5.4".into(),
            ..owner
        };
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.model_override = Some("gpt-5.5".to_string());
        choose(&elsewhere, &mut spec).unwrap();
        assert_eq!(spec.model_override.as_deref(), Some("gpt-5.5"));
    }

    /// The bug from real use: a helper of an owner on a self-hosted
    /// endpoint that names its model through an inline profile (not an
    /// override) runs on the endpoint's alias with the self-hosted
    /// provider and server; before, the profile kept the plain catalog
    /// name, the engine resolved it to the model's public provider, and
    /// the build was refused against the self-hosted binding.
    #[test]
    fn an_inline_profile_s_model_is_qualified_on_a_self_hosted_endpoint() {
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let proxy = Endpoint::configured(
            "litellm",
            WireDialect::OpenAi,
            Some("http://localhost:4000".to_string()),
            SecretString::new("k"),
            "KEY",
            0,
            crate::endpoints::ApiStyle::default(),
        );
        let endpoints = vec![proxy.clone()];
        let owner = Owner {
            session: SessionId::new(),
            path: Vec::new(),
            model: "litellm:local-coder".into(),
            auth_binding: proxy.binding(),
        };
        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        let profile: meerkat_mob::Profile =
            serde_json::from_value(serde_json::json!({"model": "gpt-5.5", "provider": "openai"}))
                .unwrap();
        assert_eq!(profile.provider, Some(meerkat_core::Provider::OpenAI));
        spec.override_profile = Some(profile);
        let chosen = choose_model(
            &owner,
            &mut spec,
            &endpoints,
            &HelperModels::default(),
            Asked::Spawn,
            Seat::Carried,
        )
        .unwrap();
        assert_eq!(chosen, "litellm:gpt-5.5");
        let profile = spec.override_profile.as_ref().unwrap();
        assert_eq!(profile.model, "litellm:gpt-5.5");
        assert_eq!(profile.provider, Some(meerkat_core::Provider::SelfHosted));
        assert_eq!(profile.self_hosted_server_id.as_deref(), Some("litellm"));
        assert_eq!(spec.model_override.as_deref(), Some("litellm:gpt-5.5"));
        // The rule reads the plain name.
        let models = HelperModels {
            default: None,
            rule: ModelRule::Deny(vec![ModelPattern::parse("gpt-*").unwrap()]),
        };
        let mut again = SpawnMemberSpec::new("delegate", "worker");
        again.model_override = Some("litellm:gpt-5.5".into());
        let refused = choose_model(
            &owner,
            &mut again,
            &endpoints,
            &models,
            Asked::Spawn,
            Seat::Carried,
        )
        .unwrap_err();
        assert!(refused.contains("`gpt-5.5` is not permitted"), "{refused}");
    }

    /// The reported bug and its rule: a member seated by a stored profile's
    /// name (the workspace mob's `main` of the first start, or a profile of
    /// a mob the agent created) never runs on that profile's model. The
    /// profile is put on the spawn inline with the chosen model, the
    /// owner's endpoint's provider and server, and no fallback chain; the
    /// model is the spawn's, else `helpers.model`, else the owner's, and
    /// the rule judges it (there is no refusal of an unchecked profile
    /// model any more). A mob whose definition could not be read is
    /// refused with what to do.
    #[test]
    fn a_stored_profile_s_model_never_decides() {
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let proxy = Endpoint::configured(
            "litellm",
            WireDialect::OpenAi,
            Some("http://localhost:4000".to_string()),
            SecretString::new("k"),
            "KEY",
            0,
            crate::endpoints::ApiStyle::default(),
        );
        let endpoints = vec![proxy.clone()];
        let owner = Owner {
            session: SessionId::new(),
            path: Vec::new(),
            model: "litellm:local-coder".into(),
            auth_binding: proxy.binding(),
        };
        let mut stored: meerkat_mob::Profile = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "provider": "anthropic",
            "self_hosted_server_id": "elsewhere",
        }))
        .unwrap();
        stored.model_fallback = Some(meerkat_core::config::ModelFallbackConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let run = |models: &HelperModels, named: Option<&str>, asked: Asked, seat: Seat| {
            let mut spec = SpawnMemberSpec::new("worker", "w1");
            spec.model_override = named.map(str::to_string);
            choose_model(&owner, &mut spec, &endpoints, models, asked, seat).map(|m| (m, spec))
        };
        let seat = || Seat::Stored(Box::new(stored.clone()));
        let (model, spec) = run(&HelperModels::default(), None, Asked::Spawn, seat()).unwrap();
        assert_eq!(model, "litellm:local-coder");
        assert_eq!(spec.model_override.as_deref(), Some("litellm:local-coder"));
        let profile = spec.override_profile.unwrap();
        assert_eq!(profile.model, "litellm:local-coder");
        assert_eq!(profile.provider, Some(meerkat_core::Provider::SelfHosted));
        assert_eq!(profile.self_hosted_server_id.as_deref(), Some("litellm"));
        assert_eq!(profile.model_fallback, None);
        // `helpers.model`, then a model the spawn names.
        let default = HelperModels {
            default: Some("claude-opus-4-8".into()),
            rule: ModelRule::Any,
        };
        let (model, _) = run(&default, None, Asked::Spawn, seat()).unwrap();
        assert_eq!(model, "litellm:claude-opus-4-8");
        let (model, spec) = run(&default, Some("house"), Asked::Spawn, seat()).unwrap();
        assert_eq!(model, "litellm:house");
        assert_eq!(spec.override_profile.unwrap().model, "litellm:house");
        // A fork stays on its owner's model, `helpers.model` or not, and
        // whatever the engine copied from its source.
        let (model, _) = run(&default, Some("copied"), Asked::Fork, seat()).unwrap();
        assert_eq!(model, "litellm:local-coder");
        // An allow list: judged on the model chosen, not the profile's.
        let haiku_only = HelperModels {
            default: None,
            rule: ModelRule::Allow(vec![ModelPattern::parse("claude-sonnet-*").unwrap()]),
        };
        let refused = run(&haiku_only, None, Asked::Spawn, seat()).unwrap_err();
        assert!(
            refused.contains("your own model `local-coder`")
                && !refused.contains("cannot be checked"),
            "{refused}"
        );
        let local = HelperModels {
            default: None,
            rule: ModelRule::Allow(vec![ModelPattern::parse("local-*").unwrap()]),
        };
        assert!(run(&local, None, Asked::Spawn, seat()).is_ok());
        // A mob whose definition could not be read.
        let unknown = run(
            &HelperModels::default(),
            None,
            Asked::Spawn,
            Seat::Unknown {
                mob: MobId::from("team"),
            },
        )
        .unwrap_err();
        assert!(
            unknown.contains("mob `team`") && unknown.contains("call the spawn again"),
            "{unknown}"
        );
    }

    /// The model a fresh helper gets: the one its spawn names, else
    /// `helpers.model` (not for a fork), else its owner's; and
    /// the model that results must pass the user's rule, with a refusal
    /// for each case that says what to do.
    #[test]
    fn a_helper_s_model_is_named_then_the_default_then_the_owner_s() {
        let owner = Owner {
            session: SessionId::new(),
            path: Vec::new(),
            model: "claude-opus-4-8".into(),
            auth_binding: None,
        };
        let haiku = |models: &HelperModels, named: Option<&str>, asked: Asked| {
            let mut spec = SpawnMemberSpec::new("worker", "w1");
            spec.model_override = named.map(str::to_string);
            choose_model(&owner, &mut spec, &[], models, asked, Seat::Carried)
                .map(|model| (model, spec.model_override))
        };
        let allow = |list: &[&str]| {
            ModelRule::Allow(
                list.iter()
                    .map(|p| ModelPattern::parse(p).unwrap())
                    .collect(),
            )
        };
        let default = HelperModels {
            default: Some("claude-haiku-4-5".into()),
            rule: ModelRule::Any,
        };
        let both = |model: &str| (model.to_string(), Some(model.to_string()));
        // Named wins over the default.
        assert_eq!(
            haiku(&default, Some("claude-sonnet-4-6"), Asked::Spawn).unwrap(),
            both("claude-sonnet-4-6")
        );
        // The default, for every spawn that names none.
        assert_eq!(
            haiku(&default, None, Asked::Spawn).unwrap(),
            both("claude-haiku-4-5")
        );
        // A fork keeps its source's model.
        assert_eq!(
            haiku(&default, None, Asked::Fork).unwrap(),
            both("claude-opus-4-8")
        );
        // Nothing set: the owner's, named on the spawn.
        assert_eq!(
            haiku(&HelperModels::default(), None, Asked::Spawn).unwrap(),
            both("claude-opus-4-8")
        );
        let haiku_only = HelperModels {
            default: None,
            rule: allow(&["claude-haiku-*"]),
        };
        // Named outside an allow list.
        let named = haiku(&haiku_only, Some("claude-sonnet-4-6"), Asked::Spawn).unwrap_err();
        assert!(
            named.contains("`claude-sonnet-4-6` is not permitted")
                && named.ends_with("name one of: claude-haiku-*"),
            "{named}"
        );
        // The owner's model outside it, nothing named.
        let own = haiku(&haiku_only, None, Asked::Spawn).unwrap_err();
        assert!(
            own.contains("your own model `claude-opus-4-8`")
                && own.contains("the call must name a permitted model"),
            "{own}"
        );
        // A default the rule refuses.
        let bad_default = HelperModels {
            default: Some("claude-sonnet-4-6".into()),
            rule: allow(&["claude-haiku-*"]),
        };
        let refused = haiku(&bad_default, None, Asked::Spawn).unwrap_err();
        assert!(
            refused.contains("the default helper model `claude-sonnet-4-6` (helpers.model)"),
            "{refused}"
        );
        // A named model inside the list passes, the default does not matter.
        assert!(haiku(&bad_default, Some("claude-haiku-4-5"), Asked::Spawn).is_ok());
    }

    /// A self-hosted helper model needs its alias in the realm document;
    /// one without is refused naming the endpoint's registered models.
    #[test]
    fn an_unregistered_self_hosted_model_is_refused_with_the_registered_ones() {
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let proxy = Endpoint::configured(
            "litellm",
            WireDialect::OpenAi,
            Some("http://localhost:4000".to_string()),
            SecretString::new("k"),
            "KEY",
            0,
            crate::endpoints::ApiStyle::default(),
        );
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        std::fs::write(
            &config,
            "[self_hosted.models.\"litellm:local-coder\"]\nremote_model = \"local-coder\"\n[self_hosted.models.\"other:x\"]\nremote_model = \"x\"\n",
        )
        .unwrap();
        assert_eq!(unregistered(&config, &proxy, "litellm:local-coder"), None);
        let refused = unregistered(&config, &proxy, "litellm:unlisted").unwrap();
        assert!(
            refused.contains("has no model `unlisted` registered")
                && refused.contains("(local-coder)")
                && refused.contains("/model"),
            "{refused}"
        );
        // A document that does not read leaves it to the engine.
        std::fs::write(&config, "not toml [").unwrap();
        assert_eq!(unregistered(&config, &proxy, "litellm:unlisted"), None);
    }

    /// An `endpoint:` prefix of the owner's endpoint is dropped (the
    /// rule and the engine read the plain name); one naming another
    /// configured endpoint is refused, since a helper runs through its
    /// owner's; any other `:` is part of the name.
    #[test]
    fn an_endpoint_prefix_is_the_owner_s_or_refused() {
        use crate::config::helper_models::ModelSource;
        use crate::endpoints::{Endpoint, SecretString, WireDialect};
        let anthropic = Endpoint::builtin(WireDialect::Anthropic, None, SecretString::new("k"));
        let openai = Endpoint::builtin(WireDialect::OpenAi, None, SecretString::new("k"));
        let endpoints = vec![anthropic, openai];
        let own = Some(&endpoints[0]);
        let name = &endpoints[0].name;
        assert_eq!(
            plain_model(
                &format!("{name}:claude-haiku-4-5"),
                own,
                &endpoints,
                ModelSource::Named
            ),
            Ok("claude-haiku-4-5".to_string())
        );
        assert_eq!(
            plain_model("llama3:8b", own, &endpoints, ModelSource::Named),
            Ok("llama3:8b".to_string())
        );
        let refused =
            plain_model("openai:gpt-5.5", own, &endpoints, ModelSource::Default).unwrap_err();
        assert!(
            refused.contains("helpers.model = openai:gpt-5.5 names the endpoint `openai`"),
            "{refused}"
        );
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
            deny_tools(&mut spec, &[], false);
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
        deny_tools(&mut spec, &[], false);
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
        deny_tools(&mut spec, &[], false);
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
        deny_tools(&mut spec, &[], false);
        assert_eq!(spec.tool_access_policy, Some(ToolAccessPolicy::ReadOnly));

        let mut spec = SpawnMemberSpec::new("delegate", "worker");
        spec.tool_access_policy = Some(ToolAccessPolicy::Constraints(vec![
            ToolAccessConstraint::ReadOnly,
        ]));
        deny_tools(&mut spec, &[], false);
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
        deny_tools(&mut spec, &NESTING_TOOLS, false);
        let names = denied_names(&spec);
        for name in ["delegate", "mob_create", "mob_spawn_member", "council"] {
            assert!(names.contains(name), "{name}: {names:?}");
        }
        assert!(denies_fork_off(&names), "{names:?}");
        // A read-only helper keeps read-only and loses them too.
        let mut spec = SpawnMemberSpec::new("delegate", "scout");
        spec.tool_access_policy = Some(meerkat_core::ops::ToolAccessPolicy::ReadOnly);
        deny_tools(&mut spec, &NESTING_TOOLS, false);
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
        deny_tools(&mut spec, &[], false);
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
            current_profile: None,
            host: Arc::new(TestHost),
            model: "m".to_string(),
            auth_binding: None,
        }
    }

    /// A host without a gate or a `[thinking]`.
    struct TestHost;

    impl RootHost for TestHost {
        fn tools(&self, _: &AgentIdentity) -> Arc<dyn AgentToolDispatcher> {
            Arc::new(meerkat_tools::EmptyToolDispatcher)
        }

        fn instructions(&self, _: &AgentIdentity) -> Vec<String> {
            vec!["persona".to_string(), "agents".to_string()]
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
                realm: meerkat_core::RealmId::parse(crate::endpoints::ENDPOINT_REALM).unwrap(),
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
