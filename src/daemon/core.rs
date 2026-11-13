//! The daemon's shared handles, built once at its start and held as one
//! `Arc` by the live agents ([`super::agents::Agents`]) and the local
//! methods ([`super::local::Local`]), instead of each copying them field
//! by field.

use std::sync::Arc;

use meerkat_mob_mcp::MobMcpState;

use super::activity::Activity;
use super::approvals::DaemonToolOrigins;
use super::audit::AuditLog;
use super::conversations::Conversations;
use super::decisions::Decisions;
use super::deferral::Deferral;
use super::lifecycle::Lifecycle;
use super::schedules::Schedules;
use super::usage::Ledger;
use super::watches::Watches;
use super::wires::Wires;
use crate::approval::ApprovalEngine;
use crate::mobs::HelperRegistry;

/// What the agents and the local methods share.
pub struct Core {
    /// The mob host every member lives in.
    pub state: Arc<MobMcpState>,
    /// The approval engine every member's gate shares.
    pub engine: Arc<ApprovalEngine>,
    /// The members' tool catalogs the gate reads.
    pub origins: Arc<DaemonToolOrigins>,
    /// The helpers and whom they work for.
    pub registry: Arc<HelperRegistry>,
    /// The consent surface; its broadcast reaches every client.
    pub decisions: Arc<Decisions>,
    /// What every agent's and helper's model calls used.
    pub ledger: Arc<Ledger>,
    /// The agents' hidden tools and their reveals.
    pub deferral: Arc<Deferral>,
    /// The agents' file watches.
    pub watches: Arc<Watches>,
    /// The agents' conversations.
    pub conversations: Arc<Conversations>,
    /// The clients, the detach record and the shutdown request.
    pub lifecycle: Arc<Lifecycle>,
    /// The agents' sockets and wires to other workspaces.
    pub wires: Arc<Wires>,
    /// The agents' schedules.
    pub schedules: Arc<Schedules>,
    /// When the agents are idle.
    pub activity: Arc<Activity>,
    /// The audit log, when it could be opened.
    pub audit: Option<Arc<AuditLog>>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core").finish_non_exhaustive()
    }
}
