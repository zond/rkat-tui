//! The cross-workspace wire methods (`rkat/wire_card`, `rkat/wire`,
//! `rkat/unwire`).

use std::path::Path;

use super::*;
use crate::daemon::audit::Line;
use crate::daemon::protocol::{Unwired, WireCard, Wired};

/// A wire's failure as the method's refusal.
fn wire_refusal(err: crate::daemon::wires::WireError) -> Refusal {
    Refusal::new(protocol::WIRE_REFUSED, err.to_string())
}

impl Local {
    /// The agent a wire method acts on: the caller's own (a bound caller
    /// may name only its own), or the live one a probe names.
    ///
    /// # Errors
    ///
    /// [`protocol::NOT_YOURS`] for a bound caller naming another agent,
    /// [`protocol::NO_AGENT`] for a probe naming none or an agent that is
    /// not live.
    fn wire_agent(&self, caller: &Caller, named: Option<String>) -> Result<String, Refusal> {
        match (caller.agent.as_ref(), named) {
            (Some(mine), Some(named)) if *mine != named => Err(Refusal::new(
                protocol::NOT_YOURS,
                format!("this connection works with {mine}, not {named}"),
            )),
            (Some(mine), _) => Ok(mine.clone()),
            (None, Some(named)) if self.agents.is_live(&named) => Ok(named),
            (None, Some(named)) => Err(Refusal::new(
                protocol::NO_AGENT,
                format!("no agent `{named}` works in this workspace (it may have retired)"),
            )),
            (None, None) => Err(Refusal::new(
                protocol::NO_AGENT,
                "a connection bound to no agent names the agent to wire",
            )),
        }
    }

    /// `rkat/wire_card`: the card of the agent `agent` names
    /// ([`Self::wire_agent`]).
    pub(super) async fn card(
        &self,
        caller: &Caller,
        agent: Option<String>,
    ) -> Result<WireCard, Refusal> {
        let agent = self.wire_agent(caller, agent)?;
        self.agents.wires().card(&agent).await.map_err(wire_refusal)
    }

    /// `rkat/wire`: wire the agent `agent` names to the one `card` names;
    /// a new wire is audited.
    pub(super) async fn wire(
        &self,
        caller: &Caller,
        agent: Option<String>,
        card: WireCard,
    ) -> Result<Wired, Refusal> {
        let agent = self.wire_agent(caller, agent)?;
        let label = card.label();
        let workspace = card.workspace.display().to_string();
        let already = self
            .agents
            .wires()
            .wire(&agent, card)
            .await
            .map_err(wire_refusal)?;
        if !already {
            self.audit(Line::event("wired", format!("{label} ({workspace})")).of_agent(&agent));
        }
        Ok(Wired { label, already })
    }

    /// `rkat/unwire`: remove the wire of the agent `agent` names to `peer`
    /// of `workspace`; a removal is audited.
    pub(super) async fn unwire(
        &self,
        caller: &Caller,
        agent: Option<String>,
        workspace: &Path,
        peer: &str,
    ) -> Result<Unwired, Refusal> {
        let agent = self.wire_agent(caller, agent)?;
        let label = protocol::wire_label(workspace, peer);
        let removed = self
            .agents
            .wires()
            .unwire(&agent, workspace, peer)
            .await
            .map_err(wire_refusal)?;
        if removed {
            self.audit(
                Line::event("unwired", format!("{label} ({})", workspace.display()))
                    .of_agent(&agent),
            );
        }
        Ok(Unwired { label, removed })
    }
}
