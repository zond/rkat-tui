//! The wrapper the daemon composes around every construction's MCP
//! adapter (`AgentFactory::with_mcp_adapter_composer`): everything is the
//! adapter's, but a tool call the engine refused for want of
//! authorization (no usable stored login, a refused refresh, or a server
//! that answered `401` with the stored one) is noted for the episodes
//! ([`super::episodes`]) and its error tells the agent what to do: the
//! engine's own words (`MCP server '<name>' is awaiting human
//! authorization`, meerkat-mcp `router.rs`, the `McpError` to `ToolError`
//! mapping) name no remedy.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::types::ToolCallView;
use meerkat_core::{
    AgentToolDispatcher, ToolCatalogCapabilities, ToolCatalogEntry, ToolDef, ToolDispatchContext,
    ToolError,
};

use crate::daemon::deferral::forward_dispatcher;

/// The servers whose calls were refused for want of authorization since
/// the last look.
#[derive(Debug, Default)]
pub struct Refusals(parking_lot::Mutex<HashSet<String>>);

impl Refusals {
    fn note(&self, server: &str) {
        self.0.lock().insert(server.to_string());
    }

    /// Take whether `server` had a call refused.
    pub fn take(&self, server: &str) -> bool {
        self.0.lock().remove(server)
    }
}

/// The server a refusal names, when `message` is the engine's refusal for
/// want of authorization.
#[must_use]
pub fn refused_server(message: &str) -> Option<&str> {
    message
        .strip_prefix("MCP server '")?
        .strip_suffix("' is awaiting human authorization")
}

/// What the agent reads instead.
#[must_use]
pub fn needs_login_error(server: &str) -> String {
    format!(
        "MCP server `{server}` needs a new login: its stored login is missing, expired or was refused by the server. The user must run `/mcp login {server}` in rkat-tui; its tools work again right after, with no restart. Tell the user, and do not call this server's tools again until they say it is done."
    )
}

/// One construction's MCP adapter, its refusals noted and explained.
pub struct Guarded {
    inner: Arc<dyn AgentToolDispatcher>,
    refusals: Arc<Refusals>,
}

impl Guarded {
    #[must_use]
    pub fn new(inner: Arc<dyn AgentToolDispatcher>, refusals: Arc<Refusals>) -> Self {
        Self { inner, refusals }
    }

    fn explained<T>(&self, result: Result<T, ToolError>) -> Result<T, ToolError> {
        match result {
            Err(ToolError::ExecutionFailed { message }) => match refused_server(&message) {
                Some(server) => {
                    self.refusals.note(server);
                    Err(ToolError::execution_failed(needs_login_error(server)))
                }
                None => Err(ToolError::ExecutionFailed { message }),
            },
            other => other,
        }
    }
}

forward_dispatcher! {
    impl AgentToolDispatcher for Guarded {
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
            poll_external_updates,
            external_tool_surface_snapshot,
            capabilities,
            completion_enrichment,
            bind_mcp_server_lifecycle_handle,
            bind_external_tool_surface_handle,
            bind_session_mob_tool_authority,
            observes_bindings_only,
        ];

        async fn dispatch(
            &self,
            call: ToolCallView<'_>,
        ) -> Result<meerkat_core::ToolDispatchOutcome, ToolError> {
            self.explained(self.inner.dispatch(call).await)
        }

        async fn dispatch_with_context(
            &self,
            call: ToolCallView<'_>,
            context: &ToolDispatchContext,
        ) -> Result<meerkat_core::ToolDispatchOutcome, ToolError> {
            self.explained(self.inner.dispatch_with_context(call, context).await)
        }

        async fn dispatch_resolved_with_context(
            &self,
            call: ToolCallView<'_>,
            context: &ToolDispatchContext,
            plan: &meerkat_core::ResolvedToolExecutionPlan,
        ) -> Result<meerkat_core::ToolDispatchOutcome, ToolError> {
            self.explained(
                self.inner
                    .dispatch_resolved_with_context(call, context, plan)
                    .await,
            )
        }

        fn bind_ops_lifecycle(
            self: Arc<Self>,
            registry: Arc<dyn meerkat_core::ops_lifecycle::OpsLifecycleRegistry>,
            owner_bridge_session_id: meerkat_core::SessionId,
        ) -> Result<meerkat_core::agent::BindOutcome, meerkat_core::agent::OpsLifecycleBindError>
        {
            use meerkat_core::agent::{BindOutcome, OpsLifecycleBindError};
            let this =
                Arc::try_unwrap(self).map_err(|_| OpsLifecycleBindError::SharedOwnership)?;
            let Self { inner, refusals } = this;
            let wrap = |inner| -> Arc<dyn AgentToolDispatcher> {
                Arc::new(Self { inner, refusals })
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The engine's refusal names its server; the agent's text names the
    /// command; any other failure is left alone.
    #[test]
    fn the_engine_s_refusal_becomes_the_command_to_run() {
        assert_eq!(
            refused_server("MCP server 'docs' is awaiting human authorization"),
            Some("docs")
        );
        assert_eq!(refused_server("Connection failed: refused"), None);
        assert!(needs_login_error("docs").contains("/mcp login docs"));
    }
}
