//! The daemon's shared handles, built once at its start and held as one
//! `Arc` by the live agents ([`super::agents::Agents`]) and the local
//! methods ([`super::local::Local`]).

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
    /// The connections of helpers in different mobs (`connect_helpers`).
    pub links: Arc<super::links::Links>,
    /// The agents' schedules.
    pub schedules: Arc<Schedules>,
    /// When the agents are idle.
    pub activity: Arc<Activity>,
    /// The audit log, when it could be opened.
    pub audit: Option<Arc<AuditLog>>,
    /// The agents' smart compaction (`[auto] compact_prepare`).
    pub prepare: Arc<super::prepare::Prepare>,
    /// `helpers.model`, `allow_models` and `deny_models` in force: the
    /// spawn customizer reads them, the settings hook replaces them.
    pub helper_models: crate::config::helper_models::SharedHelperModels,
    /// Every `tui.toml` layer's values now and the one path a change of a
    /// setting takes.
    pub settings: Arc<super::settings::Settings>,
    /// The realm's directory (what a storage failure names,
    /// [`super::storage`]).
    pub realm: std::path::PathBuf,
    /// This build's instructions on conversations earlier builds began.
    pub revisions: Arc<super::instructions::Instructions>,
    /// Tests: how many of the next member reloads fail
    /// ([`super::storage::reload`]).
    #[cfg(any(test, feature = "test-support"))]
    pub reload_faults: std::sync::atomic::AtomicU32,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core").finish_non_exhaustive()
    }
}
