//! Tools an agent's model requests leave out until the agent asks for them.
//!
//! Every tool definition an agent has rides on every model request: a
//! main agent's request carries about 122 KB of them, the eight schedule tools 38.5 KB and the agent mob tools 33 KB
//! (`tests/it/tool_costs.rs`). The engine's own deferred catalog never
//! engages for a mob member (lukacf/meerkat#2180), so rkat-tui defers
//! those two families itself: every tool stays enabled, but a deferrable
//! one is declared to the model only once its conversation revealed it
//! (`docs/evidence.md`, "Search and reveal").
//!
//! This module is what stays when the engine's native deferral ships:
//! which tools are deferrable and which direct ([`Deferrable`], [`DIRECT_MOB_TOOLS`]), the
//! wrappers that compose an agent's schedule and mob tools per session
//! ([`Deferral`], [`Hiding`]), what the gate and `/context` know of the
//! deferrable tools ([`Deferrable`]) and the persona's hint lines
//! ([`PERSONA`]). What goes then is [`reveal`]: the search tool and the
//! per-conversation reveal state with its file.
//!
//! An agent's schedule tools are composed here, not by the engine's
//! schedule category (off for the agents' profile): the runtime's schedule
//! slot is shared by every agent of the daemon and its dispatcher cannot
//! tell which session lists its tools, while the mob tools' factory is
//! called once per session build and names the session. So an agent
//! session's mob tools build returns three dispatchers: the agent mob
//! surface and the schedule tools, each behind [`Hiding`], and the search
//! tool. Helpers and other members pass through unchanged (a helper has
//! no schedule tools and, unless it may nest, no mob tools).

pub mod reveal;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::types::ToolCallView;
use meerkat_core::{
    AgentToolDispatcher, MemberCommsName, SessionId, ToolCatalogCapabilities, ToolCatalogEntry,
    ToolDef, ToolDispatchContext, ToolError, ToolSourceKind,
};
use parking_lot::RwLock;

use super::{AGENT_PROFILE, MOB_ID};
use crate::daemon::context::{FamilySize, family_sizes};

/// The agent mob tools that are never deferred: the ones rkat-tui does not
/// offer ([`crate::approval::policy::NOT_OFFERED`]), which every agent's tool access policy denies, so no search may offer
/// them.
pub const DIRECT_MOB_TOOLS: [&str; 9] = crate::approval::policy::NOT_OFFERED;

/// The engine's mob tool surface of every member's build, by the session
/// it was built for: `assign` with `context: "fork"` forks its caller
/// through the engine's own `fork_off` of the caller's surface
/// (`crate::daemon::assign`), so the fork is the engine's in every respect
/// (the caller's committed transcript, its completion custodian and the
/// `ForkJobTerminal` record a restart delivers again). `fork_off` itself is
/// not offered: the members' tool access policy filters it out of the
/// session's tools, above the surface reached here.
#[derive(Default)]
pub struct ForkRoutes {
    routes: parking_lot::Mutex<std::collections::HashMap<SessionId, std::sync::Weak<Routed>>>,
}

impl std::fmt::Debug for ForkRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkRoutes").finish_non_exhaustive()
    }
}

impl ForkRoutes {
    /// `surface` (the engine's, built for `session`), noted as `session`'s.
    fn wrap(
        self: &Arc<Self>,
        session: SessionId,
        surface: Arc<dyn AgentToolDispatcher>,
    ) -> Arc<dyn AgentToolDispatcher> {
        let routed = Arc::new(Routed {
            inner: surface,
            session,
            routes: Arc::clone(self),
        });
        self.note(&routed);
        routed
    }

    fn note(&self, routed: &Arc<Routed>) {
        let mut routes = self.routes.lock();
        routes.retain(|_, weak| weak.strong_count() > 0);
        routes.insert(routed.session.clone(), Arc::downgrade(routed));
    }

    /// The engine's mob tool surface of `session`'s build in use (`None`:
    /// the session has none, or it is gone).
    #[must_use]
    pub fn surface(&self, session: &SessionId) -> Option<Arc<dyn AgentToolDispatcher>> {
        let routed = self.routes.lock().get(session)?.upgrade()?;
        Some(routed as Arc<dyn AgentToolDispatcher>)
    }
}

/// `impl AgentToolDispatcher for <wrapper> { forward [..]; <methods> }`:
/// the wrapper's own `<methods>` as written and, for each method named in
/// the `forward` list, one that passes the call to `self.inner` unchanged.
/// A name it has no arm for does not compile. Every method of the trait
/// is in the list or written by hand: `tests/it/layout.rs` reads both and
/// fails when the engine's trait declares a method that neither names (a
/// method the engine adds with a default would otherwise be answered by
/// that default instead of by `inner`).
///
/// The methods are gathered first and the impl written last, so that
/// `async_trait` sees every method (it does not expand a macro inside the
/// impl it rewrites).
macro_rules! forward_dispatcher {
    (impl AgentToolDispatcher for $ty:ident { forward [$($name:ident),* $(,)?]; $($own:tt)* }) => {
        forward_dispatcher!(@each $ty [$($own)*] [$($name)*]);
    };
    (@each $ty:ident [$($done:tt)*] []) => {
        #[async_trait]
        impl AgentToolDispatcher for $ty {
            $($done)*
        }
    };
    (@each $ty:ident [$($done:tt)*] [tools $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn tools(&self) -> Arc<[Arc<ToolDef>]> {
                self.inner.tools()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [resolve_tool_application $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            async fn resolve_tool_application(
                &self,
                source_tool: &str,
                request: &meerkat_core::ToolApplicationRequest,
                invocation: &serde_json::Value,
                context: &ToolDispatchContext,
            ) -> Result<meerkat_core::tool_application::ToolApplicationResolution, ToolError> {
                self.inner
                    .resolve_tool_application(source_tool, request, invocation, context)
                    .await
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [tool_catalog_capabilities $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn tool_catalog_capabilities(&self) -> ToolCatalogCapabilities {
                self.inner.tool_catalog_capabilities()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [tool_catalog $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn tool_catalog(&self) -> Arc<[ToolCatalogEntry]> {
                self.inner.tool_catalog()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [tool_mutation_class $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn tool_mutation_class(&self, tool_name: &str) -> meerkat_core::ToolMutationClass {
                self.inner.tool_mutation_class(tool_name)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [live_bridge_effect_kind $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn live_bridge_effect_kind(
                &self,
                tool_name: &str,
            ) -> meerkat_core::LiveBridgeEffectKind {
                self.inner.live_bridge_effect_kind(tool_name)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [review_entry_support $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn review_entry_support(
                &self,
                tool_name: &str,
            ) -> meerkat_core::approval::review::ReviewEntrySupport {
                self.inner.review_entry_support(tool_name)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [execution_binding_epoch $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn execution_binding_epoch(&self, tool_name: &str) -> u64 {
                self.inner.execution_binding_epoch(tool_name)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [execution_binding_fingerprint $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn execution_binding_fingerprint(
                &self,
                tool_name: &str,
            ) -> Result<
                meerkat_core::EphemeralToolBindingFingerprint,
                meerkat_core::ToolExecutionResolutionError,
            > {
                self.inner.execution_binding_fingerprint(tool_name)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [resolve_execution_plan $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn resolve_execution_plan(
                &self,
                call: ToolCallView<'_>,
                dispatch_context: &ToolDispatchContext,
                resolution_context: &meerkat_core::ToolExecutionResolutionContext,
            ) -> Result<
                meerkat_core::ResolvedToolExecutionPlan,
                meerkat_core::ToolExecutionResolutionError,
            > {
                self.inner
                    .resolve_execution_plan(call, dispatch_context, resolution_context)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [validate_resolved_execution_plan $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn validate_resolved_execution_plan(
                &self,
                call: ToolCallView<'_>,
                resolution_context: &meerkat_core::ToolExecutionResolutionContext,
                plan: &meerkat_core::ResolvedToolExecutionPlan,
            ) -> Result<(), meerkat_core::ToolExecutionResolutionError> {
                self.inner
                    .validate_resolved_execution_plan(call, resolution_context, plan)
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [pending_catalog_sources $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn pending_catalog_sources(&self) -> Arc<[String]> {
                self.inner.pending_catalog_sources()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [adjudicates_tool_grants $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn adjudicates_tool_grants(&self) -> bool {
                self.inner.adjudicates_tool_grants()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [dispatch $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            async fn dispatch(
                &self,
                call: ToolCallView<'_>,
            ) -> Result<meerkat_core::ToolDispatchOutcome, ToolError> {
                self.inner.dispatch(call).await
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [dispatch_with_context $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            async fn dispatch_with_context(
                &self,
                call: ToolCallView<'_>,
                context: &ToolDispatchContext,
            ) -> Result<meerkat_core::ToolDispatchOutcome, ToolError> {
                self.inner.dispatch_with_context(call, context).await
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [dispatch_resolved_with_context $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            async fn dispatch_resolved_with_context(
                &self,
                call: ToolCallView<'_>,
                context: &ToolDispatchContext,
                plan: &meerkat_core::ResolvedToolExecutionPlan,
            ) -> Result<meerkat_core::ToolDispatchOutcome, ToolError> {
                self.inner
                    .dispatch_resolved_with_context(call, context, plan)
                    .await
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [poll_external_updates $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            async fn poll_external_updates(&self) -> meerkat_core::ExternalToolUpdate {
                self.inner.poll_external_updates().await
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [external_tool_surface_snapshot $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn external_tool_surface_snapshot(
                &self,
            ) -> Option<meerkat_core::ExternalToolSurfaceSnapshot> {
                self.inner.external_tool_surface_snapshot()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [capabilities $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn capabilities(&self) -> meerkat_core::agent::DispatcherCapabilities {
                self.inner.capabilities()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [completion_enrichment $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn completion_enrichment(
                &self,
            ) -> Option<Arc<dyn meerkat_core::completion_feed::CompletionEnrichmentProvider>> {
                self.inner.completion_enrichment()
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [bind_mcp_server_lifecycle_handle $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn bind_mcp_server_lifecycle_handle(
                &self,
                handle: Arc<dyn meerkat_core::handles::McpServerLifecycleHandle>,
            ) {
                self.inner.bind_mcp_server_lifecycle_handle(handle);
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [bind_external_tool_surface_handle $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn bind_external_tool_surface_handle(
                &self,
                handle: Arc<dyn meerkat_core::handles::ExternalToolSurfaceHandle>,
            ) {
                self.inner.bind_external_tool_surface_handle(handle);
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [bind_session_mob_tool_authority $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn bind_session_mob_tool_authority(
                &self,
                authority: Option<
                    Arc<std::sync::RwLock<meerkat_core::service::MobToolAuthorityContext>>,
                >,
            ) {
                self.inner.bind_session_mob_tool_authority(authority);
            }
        ] [$($rest)*]);
    };
    (@each $ty:ident [$($done:tt)*] [observes_bindings_only $($rest:ident)*]) => {
        forward_dispatcher!(@each $ty [$($done)*
            fn observes_bindings_only(&self) -> bool {
                self.inner.observes_bindings_only()
            }
        ] [$($rest)*]);
    };
}

/// A member's engine mob tool surface, reachable from [`ForkRoutes`] (it
/// holds only a weak reference, so binding the surface to its lifecycle,
/// which needs sole ownership, still can). Everything is `inner`'s: every
/// method of the trait but the binding, which wraps the bound surface
/// again, is forwarded unchanged (`forward_dispatcher!`).
pub struct Routed {
    inner: Arc<dyn AgentToolDispatcher>,
    session: SessionId,
    routes: Arc<ForkRoutes>,
}

forward_dispatcher! {
    impl AgentToolDispatcher for Routed {
        forward [
            tools,
            resolve_tool_application,
            tool_catalog_capabilities,
            tool_catalog,
            tool_mutation_class,
            live_bridge_effect_kind,
            review_entry_support,
            execution_binding_epoch,
            execution_binding_fingerprint,
            resolve_execution_plan,
            validate_resolved_execution_plan,
            pending_catalog_sources,
            adjudicates_tool_grants,
            dispatch,
            dispatch_with_context,
            dispatch_resolved_with_context,
            poll_external_updates,
            external_tool_surface_snapshot,
            capabilities,
            completion_enrichment,
            bind_mcp_server_lifecycle_handle,
            bind_external_tool_surface_handle,
            bind_session_mob_tool_authority,
            observes_bindings_only,
        ];

        fn bind_ops_lifecycle(
            self: Arc<Self>,
            registry: Arc<dyn meerkat_core::ops_lifecycle::OpsLifecycleRegistry>,
            owner_bridge_session_id: SessionId,
        ) -> Result<meerkat_core::agent::BindOutcome, meerkat_core::agent::OpsLifecycleBindError>
        {
            use meerkat_core::agent::{BindOutcome, OpsLifecycleBindError};
            let this =
                Arc::try_unwrap(self).map_err(|_| OpsLifecycleBindError::SharedOwnership)?;
            let Self {
                inner,
                session,
                routes,
            } = this;
            let wrap = |inner| routes.wrap(session, inner);
            Ok(
                match inner.bind_ops_lifecycle(registry, owner_bridge_session_id)? {
                    BindOutcome::Bound(inner) => BindOutcome::Bound(wrap(inner)),
                    BindOutcome::Skipped(inner) => BindOutcome::Skipped(wrap(inner)),
                },
            )
        }
    }
}

/// The persona's lines naming each deferred capability, so a model that
/// was never shown a schedule or council tool knows to search for one.
pub const PERSONA: &str = "\
# Tools you can reveal

Some tools are left out of your tool list until you search for them with `tool_search`:
- You can schedule yourself (reminders, recurring checks, follow-ups): call `tool_search` with family `schedule`.
- Teams, councils and mob profiles: call `tool_search` with family `mob`. Among them: `mob_create` and `mob_spawn_member` start a team in a mob of its own that stays after its task, which you check on with `mob_check_member` and retire with `mob_retire_member`; `council` convenes existing members for a few rounds, and its summary arrives later in your transcript.
- A helper's status (`member_status`, `mob_check_member`) shows no live progress: `output_preview` and `tokens_used` are from its last completed turn, so while a helper runs its first turn they are empty or zero, and a fork's preview is the answer it inherited from your conversation, not its own work. To know how a running helper is doing, ask it with `send_message` (every helper you start is wired to you) or wait for its note.
";

/// The families whose tools are deferrable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HiddenFamily {
    /// The engine's schedule tools (`meerkat_schedule_*`).
    Schedule,
    /// The agent mob tools other than [`DIRECT_MOB_TOOLS`].
    Mob,
}

impl HiddenFamily {
    /// Every family.
    pub const ALL: [Self; 2] = [Self::Schedule, Self::Mob];

    /// Its name, as a search and `/context` say it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Mob => "mob",
        }
    }

    /// The family `name` names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|family| family.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The family of a deferrable tool `def` (its provenance: the agent
    /// mob surface's and the mob operator tools' are both `mob`, so which
    /// tools are deferrable is [`Deferrable`]'s to say).
    #[must_use]
    pub fn of(def: &ToolDef) -> Option<Self> {
        match def.provenance.as_ref()?.kind {
            ToolSourceKind::Schedule => Some(Self::Schedule),
            ToolSourceKind::Mob => Some(Self::Mob),
            _ => None,
        }
    }
}

/// Which deferrable tools a session declares to its model.
pub trait Shown: Send + Sync {
    /// Whether `session` declares the deferrable tool `name`.
    fn shown(&self, session: &SessionId, name: &str) -> bool;
}

/// Every deferrable tool an agent's build was composed with, by name: the
/// same set for every agent of the daemon. The gate judges a revealed
/// tool by it in the run that reveals it (before the catalog read at the
/// run's end names it), and `/context` reports what is hidden from it.
#[derive(Debug, Default)]
pub struct Deferrable {
    defs: RwLock<BTreeMap<String, Arc<ToolDef>>>,
    /// The same tools as the gate knows them, built when one is noted.
    catalog: RwLock<crate::approval::catalog::ToolCatalog>,
}

impl Deferrable {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note `defs` (a hidden family's dispatcher's tools) but its
    /// `direct` ones as deferrable.
    pub fn note(&self, defs: &[Arc<ToolDef>], direct: &[&str]) {
        let mut known = self.defs.write();
        let mut added = false;
        for def in defs
            .iter()
            .filter(|def| !direct.contains(&def.name.as_str()) && HiddenFamily::of(def).is_some())
        {
            known.entry(def.name.to_string()).or_insert_with(|| {
                added = true;
                Arc::clone(def)
            });
        }
        if added {
            self.catalog
                .write()
                .add_missing(known.values().map(AsRef::as_ref));
        }
    }

    /// Add the deferrable tools `catalog` does not name yet (a tool
    /// revealed in a run is in no catalog read before it).
    pub fn add_to(&self, catalog: &mut crate::approval::catalog::ToolCatalog) {
        catalog.add_missing_from(&self.catalog.read());
    }

    /// Where the deferrable tool `name` comes from, when it is one.
    #[must_use]
    pub fn origin(&self, name: &str) -> Option<crate::approval::catalog::ToolOrigin> {
        self.catalog.read().get(name).cloned()
    }

    /// Every deferrable tool noted, by name.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<ToolDef>> {
        self.defs.read().values().cloned().collect()
    }

    /// Whether `name` is a deferrable tool.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn contains(&self, name: &str) -> bool {
        self.defs.read().contains_key(name)
    }

    /// Every deferrable tool `session` does not declare, by family, the
    /// largest first.
    #[must_use]
    pub fn hidden_sizes(&self, session: &SessionId, shown: &dyn Shown) -> Vec<FamilySize> {
        let defs = self.defs.read();
        family_sizes(
            defs.values()
                .filter(|def| !shown.shown(session, def.name.as_str()))
                .map(AsRef::as_ref),
        )
    }
}

/// `inner` (a hidden family's dispatcher) with its tools but the `direct`
/// ones left out of the list unless `session` shows them: a hidden tool is
/// neither declared nor callable (the engine refuses a call to an
/// undeclared tool as not found before it reaches a dispatcher).
/// Everything else is `inner`'s, forwarded unchanged
/// (`forward_dispatcher!`).
pub struct Hiding {
    inner: Arc<dyn AgentToolDispatcher>,
    direct: &'static [&'static str],
    session: SessionId,
    shown: Arc<dyn Shown>,
}

impl Hiding {
    /// `inner` as `session` declares it.
    #[must_use]
    pub fn new(
        inner: Arc<dyn AgentToolDispatcher>,
        direct: &'static [&'static str],
        session: SessionId,
        shown: Arc<dyn Shown>,
    ) -> Self {
        Self {
            inner,
            direct,
            session,
            shown,
        }
    }

    fn declared(&self, def: &ToolDef) -> bool {
        self.direct.contains(&def.name.as_str())
            || self.shown.shown(&self.session, def.name.as_str())
    }
}

forward_dispatcher! {
    impl AgentToolDispatcher for Hiding {
        forward [
            resolve_tool_application,
            tool_catalog_capabilities,
            tool_mutation_class,
            live_bridge_effect_kind,
            review_entry_support,
            execution_binding_epoch,
            execution_binding_fingerprint,
            resolve_execution_plan,
            validate_resolved_execution_plan,
            pending_catalog_sources,
            adjudicates_tool_grants,
            dispatch,
            dispatch_with_context,
            dispatch_resolved_with_context,
            poll_external_updates,
            external_tool_surface_snapshot,
            capabilities,
            completion_enrichment,
            bind_mcp_server_lifecycle_handle,
            bind_external_tool_surface_handle,
            bind_session_mob_tool_authority,
            observes_bindings_only,
        ];

        fn tools(&self) -> Arc<[Arc<ToolDef>]> {
            self.inner
                .tools()
                .iter()
                .filter(|def| self.declared(def))
                .cloned()
                .collect::<Vec<_>>()
                .into()
        }

        fn tool_catalog(&self) -> Arc<[ToolCatalogEntry]> {
            self.inner
                .tool_catalog()
                .iter()
                .filter(|entry| self.declared(&entry.tool))
                .cloned()
                .collect::<Vec<_>>()
                .into()
        }

        fn bind_ops_lifecycle(
            self: Arc<Self>,
            registry: Arc<dyn meerkat_core::ops_lifecycle::OpsLifecycleRegistry>,
            owner_bridge_session_id: SessionId,
        ) -> Result<meerkat_core::agent::BindOutcome, meerkat_core::agent::OpsLifecycleBindError>
        {
            use meerkat_core::agent::{BindOutcome, OpsLifecycleBindError};
            let this =
                Arc::try_unwrap(self).map_err(|_| OpsLifecycleBindError::SharedOwnership)?;
            let Self {
                inner,
                direct,
                session,
                shown,
            } = this;
            let wrap = |inner| {
                Arc::new(Self::new(inner, direct, session, shown)) as Arc<dyn AgentToolDispatcher>
            };
            Ok(
                match inner.bind_ops_lifecycle(registry, owner_bridge_session_id)? {
                    BindOutcome::Bound(inner) => BindOutcome::Bound(wrap(inner)),
                    BindOutcome::Skipped(inner) => BindOutcome::Skipped(wrap(inner)),
                },
            )
        }
    }
}

/// The deferral's parts the daemon shares: the deferrable tools, the
/// reveals, and the agents' mob tools factory with the gate that keeps it
/// installed.
///
/// Every `MethodRouter` the engine builds (one per client connection)
/// installs the engine's default mob tools factory into the runtime's
/// shared slot, which would leave an agent built after it with every mob
/// tool declared, no schedule tools and no search. So the daemon builds
/// each router under [`Self::with_router`], which installs this factory
/// again before anything else may build, and seats agents under
/// [`Self::building`], so no agent is built while a router is made.
pub struct Deferral {
    /// The deferrable tools.
    pub deferrable: Arc<Deferrable>,
    /// Each conversation's reveals.
    pub reveals: Arc<reveal::Reveals>,
    /// Every member build's engine mob tool surface (`assign`'s forks).
    pub routes: Arc<ForkRoutes>,
    factory: Arc<dyn meerkat_core::service::MobToolsFactory>,
    gate: tokio::sync::RwLock<()>,
}

impl std::fmt::Debug for Deferral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deferral").finish_non_exhaustive()
    }
}

impl Deferral {
    /// The agents' factory over `inner` (the engine's
    /// `AgentMobToolSurfaceFactory`) and the runtime's `schedules`.
    #[must_use]
    pub fn new(
        inner: Arc<dyn meerkat_core::service::MobToolsFactory>,
        schedules: meerkat::ScheduleService,
        deferrable: Arc<Deferrable>,
        reveals: Arc<reveal::Reveals>,
    ) -> Self {
        let routes = Arc::new(ForkRoutes::default());
        let factory = Arc::new(AgentTools {
            inner,
            schedules,
            policy: crate::mobs::customizer::tool_access_policy(),
            reveals: Arc::clone(&reveals),
            deferrable: Arc::clone(&deferrable),
            routes: Arc::clone(&routes),
        });
        Self {
            deferrable,
            reveals,
            routes,
            factory,
            gate: tokio::sync::RwLock::new(()),
        }
    }

    /// Put the agents' factory into `runtime`'s mob tools slot.
    pub fn install(&self, runtime: &meerkat_rpc::session_runtime::SessionRuntime) {
        runtime.set_mob_tools(Arc::clone(&self.factory));
    }

    /// Hold while an agent is built: no router is made meanwhile.
    pub async fn building(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.gate.read().await
    }

    /// `make` a router of `runtime` (which installs the engine's default
    /// factory), then install the agents' factory again, while no agent
    /// is built.
    pub async fn with_router<T>(
        &self,
        runtime: &meerkat_rpc::session_runtime::SessionRuntime,
        make: impl FnOnce() -> T,
    ) -> T {
        let _making = self.gate.write().await;
        let made = make();
        self.install(runtime);
        made
    }

    /// The deferrable tools `session` does not declare, by family.
    #[must_use]
    pub fn hidden(&self, session: &SessionId) -> Vec<FamilySize> {
        self.deferrable.hidden_sizes(session, self.reveals.as_ref())
    }
}

/// The member a mob tools build is for, when it is one of the daemon's
/// agents (its comms name `tui/main/main-<id>`).
fn agent_member(comms_name: Option<&str>) -> Option<meerkat_core::MobMemberBinding> {
    let name: MemberCommsName = comms_name?.parse().ok()?;
    (name.mob_id() == MOB_ID
        && name.role() == AGENT_PROFILE
        && super::agents::is_agent(name.member()))
    .then(|| name.to_member_binding())
}

/// The daemon's mob tools factory: an agent session's build gets the agent
/// mob surface and its own schedule tools, each behind [`Hiding`], and the
/// search tool; every other member's build is `inner`'s.
struct AgentTools {
    inner: Arc<dyn meerkat_core::service::MobToolsFactory>,
    schedules: meerkat::ScheduleService,
    /// The agents' tool access policy, which a scheduled helper target an
    /// agent creates inherits (the engine applies the creating session's at
    /// creation, `CurrentSessionScheduleToolDispatcher`).
    policy: meerkat_core::ops::ToolAccessPolicy,
    reveals: Arc<reveal::Reveals>,
    deferrable: Arc<Deferrable>,
    routes: Arc<ForkRoutes>,
}

#[async_trait]
impl meerkat_core::service::MobToolsFactory for AgentTools {
    async fn build_mob_tools(
        &self,
        args: meerkat_core::service::MobToolsBuildArgs,
    ) -> Result<Arc<dyn AgentToolDispatcher>, Box<dyn std::error::Error + Send + Sync>> {
        let session = args.session_id.clone();
        let member = agent_member(args.comms_name.as_deref());
        let surface = self
            .routes
            .wrap(session.clone(), self.inner.build_mob_tools(args).await?);
        let Some(member) = member else {
            return Ok(surface);
        };
        // The conversation's reveals, read before its first list.
        self.reveals.load().await;
        let schedule: Arc<dyn AgentToolDispatcher> = Arc::new(
            meerkat_schedule::CurrentSessionScheduleToolDispatcher::new_with_resolver(
                Arc::new(meerkat::ScheduleToolDispatcher::new(self.schedules.clone())),
                session.clone(),
                Arc::new(meerkat::surface::MobMemberCurrentSessionScheduleResolver::new(member)),
            )
            .with_creator_tool_access_policy(Some(self.policy.clone())),
        );
        self.deferrable.note(&surface.tools(), &DIRECT_MOB_TOOLS);
        self.deferrable.note(&schedule.tools(), &[]);
        let shown = Arc::clone(&self.reveals) as Arc<dyn Shown>;
        let search = reveal::SearchTool::new(
            session.clone(),
            Arc::clone(&self.reveals),
            Arc::clone(&self.deferrable),
        );
        Ok(Arc::new(meerkat_core::DynamicToolComposite::new(vec![
            Arc::new(Hiding::new(
                surface,
                &DIRECT_MOB_TOOLS,
                session.clone(),
                Arc::clone(&shown),
            )),
            Arc::new(Hiding::new(schedule, &[], session, shown)),
            Arc::new(search),
        ])))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use meerkat_core::ToolProvenance;
    use serde_json::json;

    fn def(name: &str, kind: ToolSourceKind) -> ToolDef {
        ToolDef::new(name, "", json!({})).with_provenance(ToolProvenance::new(kind, "x"))
    }

    /// Each hint line names the call exactly: the search tool and one of
    /// its families, so a model does not look for a tool named after the
    /// catalog.
    #[test]
    fn the_hints_name_the_search_call_for_every_family() {
        for family in HiddenFamily::ALL {
            let call = format!(
                "call `{}` with family `{}`",
                reveal::TOOL_SEARCH,
                family.name()
            );
            assert!(PERSONA.contains(&call), "{call}");
        }
        assert!(!PERSONA.contains("tool catalog"));
        // The withdrawn `delegate` is never suggested.
        assert!(!PERSONA.contains("delegate"), "{PERSONA}");
        // What a helper's status does not show while it runs (the engine's
        // status is its last completed turn's; a fork's is inherited).
        assert!(PERSONA.contains("shows no live progress"));
        assert!(PERSONA.contains("the answer it inherited from your conversation"));
    }

    /// The `mob` line names only tools that are in the hidden family (the
    /// engine's agent mob tools but the withdrawn ones), and the direct operator
    /// tools only on a line of their own that says they need no search.
    #[test]
    fn the_mob_hint_names_only_hidden_tools() {
        let line = |start: &str| {
            PERSONA
                .lines()
                .find(|line| line.starts_with(start))
                .expect("the hint has this line")
        };
        let named = |line: &str| -> Vec<String> {
            line.split('`')
                .skip(1)
                .step_by(2)
                .filter(|word| word.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
                .map(str::to_owned)
                .collect()
        };
        let mob = line("- Teams, councils");
        let tools: Vec<String> = named(mob)
            .into_iter()
            .filter(|word| !["tool_search", "mob"].contains(&word.as_str()))
            .collect();
        for tool in ["mob_create", "mob_spawn_member", "council"] {
            assert!(tools.iter().any(|t| t == tool), "{tool}: {mob}");
        }
        for tool in &tools {
            assert!(
                meerkat_mob::AGENT_MOB_TOOL_NAMES.contains(&tool.as_str())
                    && !DIRECT_MOB_TOOLS.contains(&tool.as_str()),
                "{tool} is not a hidden mob tool"
            );
        }
        // No line names a direct tool: helpers are started with `assign`,
        // whose own description says the rest.
        assert!(
            !PERSONA.lines().any(|l| l.starts_with("- Without a search")),
            "{PERSONA}"
        );
    }

    /// The deferrable tools are what a hidden family's dispatcher offers
    /// but its direct ones; the family is the provenance's.
    #[test]
    fn deferrable_tools_are_the_families_but_their_direct_ones() {
        let registry = Deferrable::new();
        let mob = |name| Arc::new(def(name, ToolSourceKind::Mob));
        registry.note(&[mob("delegate"), mob("council")], &DIRECT_MOB_TOOLS);
        registry.note(
            &[Arc::new(def(
                "meerkat_schedule_create",
                ToolSourceKind::Schedule,
            ))],
            &[],
        );
        assert!(registry.contains("council"));
        assert!(registry.contains("meerkat_schedule_create"));
        assert!(!registry.contains("delegate"));
        assert_eq!(
            HiddenFamily::of(&def("meerkat_schedule_create", ToolSourceKind::Schedule)),
            Some(HiddenFamily::Schedule)
        );
        assert_eq!(
            HiddenFamily::of(&def("council", ToolSourceKind::Mob)),
            Some(HiddenFamily::Mob)
        );
        assert_eq!(HiddenFamily::of(&def("x", ToolSourceKind::Mcp)), None);
        assert_eq!(
            HiddenFamily::parse(" Schedule"),
            Some(HiddenFamily::Schedule)
        );
        assert_eq!(HiddenFamily::parse("mobs"), None);
    }

    /// Only the daemon's agents get the hidden families and the search
    /// tool; a helper or a member under another role does not.
    #[test]
    fn only_the_agents_builds_are_composed() {
        assert!(agent_member(Some("tui/main/main-ab12")).is_some());
        assert!(agent_member(Some("tui/helper/main-ab12")).is_none());
        assert!(agent_member(Some("other/main/main-ab12")).is_none());
        assert!(agent_member(Some("tui/main/h1")).is_none());
        assert!(agent_member(None).is_none());
    }
}
