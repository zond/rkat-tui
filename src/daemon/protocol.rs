//! The daemon's wire, typed once for both ends: the method names (the
//! daemon's own `rkat/*` ones and the engine's the client calls and the
//! daemon observes), the JSON-RPC error codes, and the parameters and
//! results of every `rkat/*` method, of `decision/decide` as a client sends
//! it and of the `rkat/decision_ended` notification.
//!
//! [`super::local`] answers the `rkat/*` methods, [`super::handshake`] the
//! attach line, [`super::decisions`] `decision/decide`; the client
//! (`crate::client::{remote, runner, approvals}`) calls them with these
//! types. `tests/it/daemon.rs` tests the raw wire below them.

use meerkat_contracts::WireDecisionPhase;
use meerkat_core::{Message, SessionId};
use meerkat_rpc::protocol::{RpcId, RpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::conversations::Listed;
use crate::approval::desk::{ModeStatus, RemoteAnswer};

pub use meerkat_rpc::error::{
    INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};

/// Every local method starts with this.
pub const PREFIX: &str = "rkat/";
/// The attach handshake ([`super::handshake`]): [`AttachParams`] →
/// [`Attached`]. Answered before the connection is served, so it is no
/// [`Method`].
pub const ATTACH: &str = "rkat/attach";
/// The notification that a decision a client was shown ended:
/// [`DecisionEnded`].
pub const DECISION_ENDED: &str = "rkat/decision_ended";
/// How many lines `/audit` shows unless it names a number.
pub const AUDIT_LINES: usize = 20;
/// The notification of a change the agent's file tools made:
/// [`FileDiffNote`].
pub const FILE_DIFF: &str = "rkat/file_diff";
/// The notification that a run no user started was fired by a schedule
/// (or a file watch): [`TriggeredNote`].
pub const TRIGGERED: &str = "rkat/triggered";
/// The notification that a conversation's token totals changed (a model
/// call of its agent, a helper or the classifier, a compaction):
/// [`CostNote`].
pub const COST: &str = "rkat/cost";
/// The notification that an agent runs another conversation now (a
/// client's `/new` or `/resume`): [`SwitchedNote`].
pub const SWITCHED: &str = "rkat/switched";
/// The notification that a mob hosts helpers or was destroyed:
/// [`MobNote`].
pub const MOB: &str = "rkat/mob";
/// The notification that an agent is idle ([`IdleNote`],
/// [`super::activity`]): a client's turn ends on it. Pushed when the
/// agent becomes idle, and to a client as it attaches when it is idle
/// then.
pub const IDLE: &str = "rkat/idle";
/// The notification that a run of an agent is open ([`RunningNote`],
/// [`super::activity`]), the counterpart of [`IDLE`] for a client that
/// cannot have seen the run start: pushed to a client as it attaches
/// while a run of its agent is open, and, when the engine ran a run then
/// whose start the daemon had not read yet, at that start. A client joins
/// the run's turn on it and stops the run by its id.
pub const RUNNING: &str = "rkat/running";
/// The notification of something one agent's clients should see in the
/// transcript (the smart compaction's steps): [`AgentNotice`].
pub const AGENT_NOTICE: &str = "rkat/agent_notice";

/// The notification that a helper ended and how its parent was told:
/// [`HelperEnded`].
pub const HELPER_ENDED: &str = "rkat/helper_ended";

/// The `how` of a [`HELPER_ENDED`] that says only that a helper left its
/// mob (it was retired; how it ended, if it was told, came before): its
/// line shows it retired, and nothing else is shown.
pub const HELPER_RETIRED: &str = "retired";

/// The notification of the helpers seated for one agent's conversation
/// now (its whole tree, nested ones too): pushed whenever one is seated or
/// leaves, so a client's live helpers are the daemon's ([`SeatedHelpers`]).
pub const SEATED: &str = "rkat/seated";

/// What [`SEATED`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatedHelpers {
    /// The agent the helpers work for (at the top of their nesting).
    pub agent: String,
    /// The conversation they work for.
    pub session_id: SessionId,
    /// Each seated helper's labels, outermost first (its own last).
    pub helpers: Vec<Vec<String>>,
}

/// [`method::Seated`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatedList {
    /// Each seated helper's labels, outermost first (its own last).
    pub helpers: Vec<Vec<String>>,
}

/// What [`HELPER_ENDED`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperEnded {
    /// The agent the helper worked for (at the top of its nesting).
    pub agent: String,
    /// The conversation it worked for.
    pub session_id: SessionId,
    /// Its labels, outermost first (its own last).
    pub path: Vec<String>,
    /// How it ended, in the words its line shows (`stopped by you`).
    pub how: String,
    /// Who was told and when, in plain words (`main-ab12 is told with your
    /// next message`); `None`: nobody (its parent is gone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub told: Option<String>,
}

/// What [`AGENT_NOTICE`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNotice {
    /// The agent it is about.
    pub agent: String,
    /// A warning (else information).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warn: bool,
    /// What it says.
    pub text: String,
}

/// The notification that a setting in force changed for the clients to
/// follow (a live `[ui]` setting a client applies): [`SettingNote`].
pub const SETTING: &str = "rkat/setting";

/// What [`SETTING`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingNote {
    /// The setting's dotted key.
    pub key: String,
    /// Its value in force as the file holds it (`None`: unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The layer it comes from.
    pub source: crate::config::registry::Level,
    /// What to show the user (`None`: nothing, the client that changed it
    /// has its own confirmation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// [`method::SetSetting`]'s parameters: `/set`, `/unset` and the commands
/// that are shorthands for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSettingParams {
    /// The setting's dotted key.
    pub key: String,
    /// The value as typed (`None`: remove the level's value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The user's own `tui.toml` (else the workspace's).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
}

/// [`method::SetSetting`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingOutcome {
    /// It was written.
    pub ok: bool,
    /// The confirmation, or why nothing was written.
    pub text: String,
    /// What applying it had to warn about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// [`method::Settings`]'s result: every setting as `/settings` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsList {
    /// One row per scalar setting, in the table's order.
    pub rows: Vec<SettingRow>,
    /// The structured sections, with the commands that change them.
    pub sections: Vec<SectionRow>,
    /// The user's own `tui.toml`.
    pub user_file: std::path::PathBuf,
    /// The workspace's realm `tui.toml`.
    pub workspace_file: std::path::PathBuf,
}

/// One setting of [`SettingsList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingRow {
    /// The dotted key.
    pub key: String,
    /// What it does.
    pub about: String,
    /// What it takes.
    pub takes: String,
    /// When a change takes effect.
    pub applies: crate::config::registry::Applies,
    /// The value in force, as shown.
    pub value: String,
    /// The layer it comes from.
    pub source: crate::config::registry::Level,
    /// Each layer that holds a value, with it.
    pub levels: Vec<(crate::config::registry::Level, String)>,
    /// The default, in words.
    pub default: String,
    /// What a project file may do with it.
    pub project: String,
    /// The friendly command, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// One structured section of [`SettingsList`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionRow {
    /// How it is written.
    pub key: String,
    /// What it holds.
    pub about: String,
    /// What a project file may do with it.
    pub project: String,
    /// When a change of the file takes effect.
    pub applies: String,
    /// Its command-line flags.
    pub flags: String,
    /// The commands that change it.
    pub commands: String,
}

/// The notification of the permission mode and the denial counters
/// ([`ModeStatus`]): pushed when a mode switch, a decision, a pause or a
/// resume may have moved them, and to a client as it attaches.
pub const MODE: &str = "rkat/mode";

/// One of the daemon's own `rkat/*` methods: its name, and what it takes
/// and answers. The daemon dispatches on [`Method::NAME`] and answers with
/// the [`Method::Result`] its handler returns (`super::local`), the client
/// calls it through `crate::client::remote::call::<M>`, so a name and its
/// types are paired once, here, and the compiler checks both ends.
pub trait Method {
    /// The method's name on the wire.
    const NAME: &'static str;
    /// What it takes (`{}`: [`Empty`]).
    type Params: Serialize + serde::de::DeserializeOwned + Send + Sync;
    /// What it answers (`{}`: [`Empty`]).
    type Result: Serialize + serde::de::DeserializeOwned + Send;
}

/// `{}`: the parameters of a method that takes none, the result of one
/// that answers nothing. A field is refused, not ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// Declares each method type of [`method`] and [`METHODS`].
macro_rules! methods {
    ($( $(#[$doc:meta])* $ty:ident = $name:literal ($params:ty) -> $result:ty; )*) => {
        /// The daemon's `rkat/*` methods, one type each ([`Method`]).
        pub mod method {
            #[allow(unused_imports)]
            use super::*;
            $(
                $(#[$doc])*
                #[derive(Debug, Clone, Copy)]
                pub struct $ty;

                impl Method for $ty {
                    const NAME: &'static str = $name;
                    type Params = $params;
                    type Result = $result;
                }
            )*
        }

        /// Every `rkat/*` method's name.
        #[cfg(any(test, feature = "test-support"))]
        pub const METHODS: &[&str] = &[$($name),*];
    };
}

methods! {
    /// Switch the permission mode: the daemon's one mode governs every
    /// agent and its helpers, is recorded in the realm rules file, and the
    /// agents are told when plan mode starts or ends.
    SetMode = "rkat/set_mode" (SetModeParams) -> ModeSet;
    /// A `/permissions` command on the daemon's engine, which persists to
    /// the realm rules file.
    Rules = "rkat/rules" (crate::approval::desk::RuleCommand) -> crate::approval::desk::RuleOutcome;
    /// The `/permissions` picker's rows: every rule of the daemon's
    /// engine with its number, its patterns and its layer.
    RuleRows = "rkat/rule_rows" (Empty) -> crate::approval::desk::RuleRows;
    /// The calls the gate recently blocked for this client's agent that the
    /// user can approve (`/permissions`' Blocked section), oldest first.
    Blocked = "rkat/blocked" (Empty) -> BlockedList;
    /// Approve a blocked call once, for this session or always; with an
    /// `effect`, allow that kind of action (a statement the classifier
    /// reads) for this session or always instead.
    Approve = "rkat/approve" (ApproveParams) -> crate::approval::desk::RuleOutcome;
    /// Allow a kind of action the user typed (`/permissions`' `n`), for
    /// this client's agent's session or always: a statement the
    /// classifier reads from the next review on; the agent is told in one
    /// system note.
    AllowStatement = "rkat/allow_statement" (StatementParams) -> crate::approval::desk::RuleOutcome;
    /// Withdraw an approval once that waits for the agent (the Blocked
    /// section's `w`): the call is a blocked call again.
    WithdrawApproval = "rkat/withdraw_approval" (GrantParams) -> crate::approval::desk::RuleOutcome;
    /// Remove a statement of this client's agent's session, or one added
    /// always (`/permissions`).
    RemoveStatement = "rkat/remove_statement" (GrantParams) -> crate::approval::desk::RuleOutcome;
    /// `/set`, `/unset` and their shorthands (`/compact prepare`, `/auto
    /// model`, Ctrl+T): one setting at the user's or the workspace's
    /// level, checked, written to that `tui.toml`, and applied when the
    /// value in force changed (`crate::daemon::settings`).
    SetSetting = "rkat/set_setting" (SetSettingParams) -> SettingOutcome;
    /// `/settings`: every setting, the value in force, its layer and what
    /// each layer holds.
    Settings = "rkat/settings" (Empty) -> SettingsList;
    /// `/compact status`'s daemon side: the prepare instruction, its
    /// threshold and whether it is armed for this client's agent.
    CompactStatus = "rkat/compact_status" (Empty) -> crate::approval::desk::RuleOutcome;
    /// The review numbers (`/auto stats`), since the daemon started and
    /// for this client's agent's conversation.
    AutoStats = "rkat/auto_stats" (Empty) -> crate::approval::desk::RuleOutcome;
    /// Remove a session grant of this client's agent (`/permissions`).
    RemoveGrant = "rkat/remove_grant" (GrantParams) -> crate::approval::desk::RuleOutcome;
    /// The agents' conversations, last used first, the caller's agent's
    /// current one marked ([`crate::daemon::conversations`]).
    Conversations = "rkat/conversations" (Empty) -> ConversationList;
    /// `/new`: the caller's agent on a fresh conversation; refused with
    /// [`SWITCH_BUSY`] while its run is open.
    New = "rkat/new" (Empty) -> Switched;
    /// `/resume <session>`: the caller's agent back on an earlier
    /// conversation of its own (another agent's is refused with where it
    /// is); refused with [`SWITCH_BUSY`] while its run is open.
    Resume = "rkat/resume" (SessionParams) -> Switched;
    /// A conversation's committed messages (a client renders a resumed
    /// conversation from them).
    Transcript = "rkat/transcript" (SessionParams) -> MessageList;
    /// A conversation's last message, its system rows and notices skipped
    /// (none when it has none): what a retried prompt is compared with,
    /// without sending the whole transcript.
    LastMessage = "rkat/last_message" (SessionParams) -> MessageList;
    /// A conversation's total usage as the engine keeps it with the session
    /// (a client seeds its token counter from it on attach and after a
    /// switch; the engine's wire reports usage only per turn), and its
    /// token totals by model as the daemon counted them
    /// ([`crate::daemon::usage`]; [`COST`] pushes later changes) and the
    /// context of its last model call (the gauge's reading).
    Usage = "rkat/usage" (SessionParams) -> UsageTotals;
    /// `/context`: what the caller's agent's current conversation holds,
    /// as the daemon sees it: its messages by kind and the system
    /// prompt's size (read from its history, not sent), and the tool
    /// definitions its model requests carry, by family.
    Context = "rkat/context" (Empty) -> ContextReport;
    /// The configured endpoint a conversation's durable auth binding names
    /// (the status bar shows it).
    Endpoint = "rkat/endpoint" (SessionParams) -> SessionEndpoint;
    /// `/trust`: every present project input recorded in the trust record
    /// the daemon owns, as it is now (read on every session build).
    Trust = "rkat/trust" (Empty) -> crate::approval::desk::RuleOutcome;
    /// A conversation's tasks in the engine's task store (the Ctrl+T
    /// checklist; read on attach, after a switch and after each of the
    /// agent's `task_create` and `task_update` calls).
    Tasks = "rkat/tasks" (SessionParams) -> TaskList;
    /// What `/status` shows of the daemon's side: the realm, the shell's
    /// confinement, the permission mode, the trust of the realm rules and
    /// the project inputs, the helpers' depth limit, the caller's agent's
    /// file watches.
    Report = "rkat/report" (Empty) -> DaemonReport;
    /// `/model`: the caller's agent's current session on another model in
    /// place, with the endpoint's auth binding and the model's `[thinking]`
    /// setting.
    SetModel = "rkat/set_model" (SetModelParams) -> ModelSet;
    /// What the daemon's start-up had to say (the confinement, the rules,
    /// the configuration), which a client shows when it first attaches.
    Notices = "rkat/notices" (Empty) -> NoticeList;
    /// What the caller's agent wanted to do and was denied in turns no user
    /// started while no client was attached
    /// ([`crate::approval::attendance`]); each is answered once, to the
    /// first client that asks (a client asks whenever it attaches).
    Missed = "rkat/missed" (Empty) -> NoticeList;
    /// The last lines of the audit log for the caller's agent (`/audit`,
    /// default [`AUDIT_LINES`]), oldest first.
    Audit = "rkat/audit" (AuditParams) -> AuditTail;
    /// The daemon's lifecycle state as the caller sees it (`--list` and
    /// `/agents` print it).
    Status = "rkat/status" (Empty) -> DaemonStatus;
    /// The caller's agent's schedules (`/schedules`), each with what its
    /// runs cost.
    Schedules = "rkat/schedules" (Empty) -> ScheduleList;
    /// The user's own pause, resume or delete of one of the caller's
    /// agent's schedules (`/schedules pause|resume|delete <name>`).
    Schedule = "rkat/schedule" (ScheduleAction) -> crate::approval::desk::RuleOutcome;
    /// What the realm holds that a re-home replacing it would lose: each
    /// conversation with a user turn (or that cannot be read); `/rehome`
    /// refuses then.
    Holds = "rkat/holds" (Empty) -> HeldList;
    /// `/detach`: the caller leaves and wants its agent kept running; the
    /// daemon records that it has been detached from.
    Detach = "rkat/detach" (Empty) -> Empty;
    /// Stop the daemon: it removes its endpoint file and shuts down as on
    /// SIGTERM; with `retire`, every agent retires first (their
    /// conversations stay).
    Shutdown = "rkat/shutdown" (ShutdownParams) -> Empty;
    /// `/quit`: the daemon decides how the caller leaves and does it, under
    /// the seating lock, so no client binds or quits between the decision
    /// and the act (`super::lifecycle::quit_decision`): it leaves the agent
    /// to its other clients, retires it, stops the daemon, or answers what
    /// to ask the user (then `rkat/detach` or `rkat/shutdown` follows).
    Quit = "rkat/quit" (Empty) -> QuitOutcome;
    /// `/agents`' retire key: another agent, idle and with no client,
    /// leaves the workspace as a `/quit` would retire it (its schedules are
    /// deleted, its conversation stays for `/resume`). Refused with
    /// [`AGENT_IN_USE`] for the caller's own agent (which leaves with
    /// `/quit`), an agent with a client, or one whose turn runs (or whose
    /// state cannot be read); with [`NO_AGENT`] for one that is not live.
    RetireAgent = "rkat/retire_agent" (AgentParams) -> Retired;
    /// Stop one helper of the caller's agent: its run is cancelled and it
    /// is retired; the helpers it started keep running, and its parent is
    /// told. Refused with [`HELPER_REFUSED`] for a helper that is not
    /// running, or that works for another agent.
    StopHelper = "rkat/stop_helper" (StopHelperParams) -> Empty;
    /// The helpers seated now for a conversation (its whole tree): what a
    /// client lists live as it attaches; [`SEATED`] pushes each change.
    Seated = "rkat/seated_helpers" (SessionParams) -> SeatedList;
    /// Take back messages the caller's agent has not read yet: each named
    /// input of its current conversation that the runtime still holds
    /// queued is abandoned, leaving the running turn alone; one that
    /// already joined a run is reported reached and left.
    Recall = "rkat/recall" (RecallParams) -> Recalled;
    /// What became of messages the caller's agent was sent: each named
    /// input of its current conversation as the runtime records it now
    /// (read only; a client asks when the agent is idle and holds nothing
    /// while it still waits for one of its steers).
    Inputs = "rkat/inputs" (RecallParams) -> InputFates;
    /// What the daemon did with the prompt or steer a client sent under
    /// its own key ([`SEND_KEY`]): a client whose send brought no answer
    /// (a timeout, a lost connection) asks before telling the user it was
    /// not sent. Waits a little while the engine has not answered it yet.
    Sent = "rkat/sent" (SentParams) -> SentFate;
    /// Esc: interrupt the caller's agent's run the parameters name (and
    /// only that run: one that has ended is never exchanged for a newer
    /// one), and take back each named message the agent has not read
    /// (`crate::daemon::local`'s `interrupt`). A message the runtime
    /// already gave a follow-up run stops that run too: it belongs to the
    /// interrupted turn.
    Interrupt = "rkat/interrupt" (InterruptParams) -> Interrupted;
    /// `/btw <question>`: a side question about the caller's agent's
    /// session, answered by one model call of its own, on the agent's
    /// model, with no tools (`crate::daemon::btw`). Nothing is written to
    /// the session and the agent's run is untouched. Refused with
    /// [`BTW_REFUSED`] (no conversation, no model client, the provider's
    /// failure, the time bound, a cancel), in plain words.
    Btw = "rkat/btw" (BtwParams) -> BtwAnswer;
    /// Cancel the caller's pending [`method::Btw`] named `id` (Esc); one
    /// that ended is no error.
    BtwCancel = "rkat/btw_cancel" (BtwCancelParams) -> Empty;
    /// A top-level agent's card for a cross-workspace wire: its workspace,
    /// name, comms identity and the address of its Unix socket
    /// ([`crate::daemon::wires`]). A bound caller asks for its own agent;
    /// a probe names one.
    Card = "rkat/wire_card" (WireAgentParams) -> WireCard;
    /// `/wire`: trust the agent another workspace's `card` names and route
    /// messages to it, for the caller's agent (a probe names the agent).
    /// The client asks both daemons: the wire is both directions only when
    /// each side trusts the other.
    Wire = "rkat/wire" (WireParams) -> Wired;
    /// `/unwire`: drop the agent's cross-workspace wire to `peer`.
    Unwire = "rkat/unwire" (UnwireParams) -> Unwired;
    /// `/wire ssh://<host>/…`: this daemon runs ssh to the host, lists its
    /// agents or wires the caller's agent to one of them both ways, and
    /// holds the link ([`crate::daemon::remote_wire`]). A bound caller
    /// only: its own agent.
    WireSsh = "rkat/wire_ssh" (WireSshParams) -> WiredSsh;
    /// `/auto check`: send the corpus cases the selection names through
    /// the daemon's classifier as a review shows them
    /// ([`crate::approval::reviewer::Reviewer::check`]) and answer
    /// what came out. One at a time ([`CHECK_REFUSED`] while one runs, or
    /// without a classifier model); dropped when its client leaves
    /// ([`CANCELLED_ON_LEAVE`]).
    ClassifierCheck = "rkat/classifier_check" (crate::approval::corpus::Selection) -> ClassifierChecked;
}

/// The local methods whose run is dropped when the client that called
/// them leaves (a check sends model calls the user pays for; nobody reads
/// its answer then).
pub const CANCELLED_ON_LEAVE: &[&str] = &[
    <method::ClassifierCheck as Method>::NAME,
    <method::Btw as Method>::NAME,
];

/// [`method::ClassifierCheck`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierChecked {
    /// The classifier's model.
    pub model: String,
    /// What came out.
    pub report: crate::approval::corpus::Report,
}

/// [`method::Status`]'s result.
pub type DaemonStatus = Status;
/// [`method::Report`]'s result.
pub type DaemonReport = Report;
/// [`method::Tasks`]'s result.
pub type TaskList = Tasks;
/// [`method::Notices`]'s and [`method::Missed`]'s result.
pub type NoticeList = Notices;
/// [`method::Holds`]'s result.
pub type HeldList = Holds;
/// [`method::Transcript`]'s result.
pub type MessageList = Transcript;
/// [`method::Usage`]'s result.
pub type UsageTotals = Usage;

/// The engine's decision notice, pushed by the daemon.
pub const DECISION_NOTICE: &str = meerkat_contracts::DECISION_NOTICE_NOTIFICATION;
/// The engine's decide method, decided by the daemon ([`DecideParams`]).
pub const DECISION_DECIDE: &str = meerkat_rpc::router::DECISION_DECIDE_METHOD;

/// The engine's methods the client calls (and the daemon observes on their
/// way to the router).
pub mod engine {
    /// A prompt or a steer for a member.
    pub const MEMBER_SEND: &str = "mob/member_send";
    /// Stop a member's run.
    pub const STOP_MEMBER_RUN: &str = "mob/stop_member_run";
    /// Cancel whatever a member runs.
    pub const FORCE_CANCEL: &str = "mob/force_cancel";
    /// Interrupt a member's current run at once, whichever it is (the
    /// fallback when a stop names a run that is no longer current, or
    /// goes unconfirmed).
    pub const HARD_CANCEL_MEMBER: &str = "mob/hard_cancel_member";
    /// Open a member's or a mob's event stream.
    pub const STREAM_OPEN: &str = "mob/stream_open";
    /// Close a stream.
    pub const STREAM_CLOSE: &str = "mob/stream_close";
    /// A stream's event (a notification).
    pub const STREAM_EVENT: &str = "mob/stream_event";
    /// A stream's end (a notification).
    pub const STREAM_END: &str = "mob/stream_end";
    /// Append to a member's system context.
    pub const APPEND_SYSTEM_CONTEXT: &str = "mob/append_system_context";
    /// The mobs a runtime hosts.
    pub const LIST: &str = "mob/list";
    /// A mob's members.
    pub const MEMBERS: &str = "mob/members";
    /// A member's status (its current session).
    pub const MEMBER_STATUS: &str = "mob/member_status";
    /// Compact a member at its next compaction check.
    pub const COMPACT_MEMBER: &str = "mob/compact_member";
    /// Compact a session.
    pub const SESSION_COMPACT: &str = "session/compact";
    /// A session's durable events after a cursor.
    pub const EVENTS_LIST_SINCE: &str = "events/list_since";
    /// A session's snapshot and its replay cursor.
    pub const EVENTS_SNAPSHOT: &str = "events/snapshot";
    /// The capability handshake.
    pub const INITIALIZE: &str = "initialize";
    /// Read one durable decision (inspection; the decide goes through the
    /// daemon, [`super::DECISION_DECIDE`]).
    pub const DECISION_GET: &str = "decision/get";
    /// List the realm's approval records (inspection).
    pub const APPROVAL_LIST: &str = "approval/list";

    /// Every engine method the daemon forwards to a client's connection:
    /// the ones above, which the client calls, and nothing else. A
    /// session-creating or session-editing method (`session/create`,
    /// `session/fork_at`, `turn/start`, `mob/create`, `mob/spawn`, …)
    /// would build a session outside the daemon's mob, which no
    /// application gate admits, so the proxy refuses it with
    /// [`super::NOT_SERVED`]. So is `mob/turn_start`: the engine's
    /// request-scoped turn carries the runtime's cleanup that destroys
    /// the mobs its session owns when the turn cannot be applied, is
    /// cancelled or its member retired (prompts are [`MEMBER_SEND`],
    /// admitted by the mob with no such cleanup; a model switch is
    /// `rkat/set_model`).
    pub const SERVED: &[&str] = &[
        MEMBER_SEND,
        STOP_MEMBER_RUN,
        FORCE_CANCEL,
        HARD_CANCEL_MEMBER,
        STREAM_OPEN,
        STREAM_CLOSE,
        APPEND_SYSTEM_CONTEXT,
        LIST,
        MEMBERS,
        MEMBER_STATUS,
        COMPACT_MEMBER,
        SESSION_COMPACT,
        EVENTS_LIST_SINCE,
        EVENTS_SNAPSHOT,
        INITIALIZE,
        DECISION_GET,
        APPROVAL_LIST,
    ];

    /// The served methods that act on one member (`mob_id` and
    /// `agent_identity` in their params): the proxy refuses them on a
    /// member that works for another agent than the caller's
    /// ([`super::NOT_YOURS`]). The member reads (`mob/member_status`, a
    /// member's stream) are not among them: every client follows every
    /// mob's helpers.
    pub const MEMBER_ACTIONS: &[&str] = &[
        MEMBER_SEND,
        STOP_MEMBER_RUN,
        FORCE_CANCEL,
        HARD_CANCEL_MEMBER,
        APPEND_SYSTEM_CONTEXT,
        COMPACT_MEMBER,
    ];

    /// Whether the daemon forwards `method` to the engine.
    #[must_use]
    pub fn serves(method: &str) -> bool {
        SERVED.contains(&method)
    }
}

/// The attach token is missing or wrong, or no attach arrived in time.
pub const UNAUTHORIZED: i32 = -32001;
/// The attach named an agent or a conversation the daemon cannot bind the
/// connection to.
pub const NO_AGENT: i32 = -32002;
/// A conversation switch or read the engine refused.
pub const SWITCH_FAILED: i32 = -32010;
/// A conversation switch refused because the agent's turn is
/// running.
pub const SWITCH_BUSY: i32 = -32011;
/// An engine method the daemon does not forward to clients (not in
/// [`engine::SERVED`]): a code of rkat-tui's own, not the engine's
/// `METHOD_NOT_FOUND`, so a client can tell the proxy's refusal from the
/// engine's.
pub const NOT_SERVED: i32 = -32012;
/// An engine request that would act on another agent than the one the
/// connection is bound to (its `agent_identity`, a helper working for
/// another agent, or a session that is not the caller's agent's): the
/// connection drives its own agent only.
pub const NOT_YOURS: i32 = -32013;

/// A retire ([`method::RetireAgent`]) refused because the agent is in
/// use: it is the caller's own, has a client, or its turn runs.
pub const AGENT_IN_USE: i32 = -32014;

/// A cross-workspace wire refused ([`method::Wire`], [`method::Card`],
/// [`method::Unwire`]): the card names this workspace or the agent itself,
/// the agent has no socket, or the engine refused the edge. The message
/// says which and what to do.
pub const WIRE_REFUSED: i32 = -32015;

/// A classifier check ([`method::ClassifierCheck`]) refused: no classifier
/// model is configured, another check runs, or the selection names no
/// case it can send. The message says which and what to do.
pub const CHECK_REFUSED: i32 = -32016;

/// A helper stop ([`method::StopHelper`]) refused: no running helper of
/// the caller's agent has that name. The message says what to do.
pub const HELPER_REFUSED: i32 = -32017;

/// A `/btw` side question ([`method::Btw`]) not answered: no conversation
/// or model client, the provider's failure, the time bound, or a cancel.
/// The message says which, in plain words.
pub const BTW_REFUSED: i32 = -32018;

/// An input refused because a storage failure left its agent needing a
/// reload, which the daemon then did ([`crate::daemon::storage`]): the
/// input was not taken. The agent's event stream ended with its old
/// registration, so the client opens it again ([`RELOADED`]) and then
/// sends the input once more.
pub const AGENT_RELOADED: i32 = -32019;

/// The notification that an agent was reloaded after a storage failure
/// ([`SwitchedNote`]: the agent and its conversation, the same one): its
/// clients open its event stream again.
pub const RELOADED: &str = "rkat/reloaded";

/// A local method's refusal: a JSON-RPC error code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The JSON-RPC error code.
    pub code: i32,
    /// Its message.
    pub message: String,
}

impl Refusal {
    /// A refusal with `code`.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// [`INVALID_PARAMS`] for parameters that do not read as `T`.
    #[must_use]
    pub fn invalid(err: &serde_json::Error) -> Self {
        Self::new(INVALID_PARAMS, err.to_string())
    }

    /// [`INTERNAL_ERROR`] saying `err`.
    #[must_use]
    pub fn internal(err: impl std::fmt::Display) -> Self {
        Self::new(INTERNAL_ERROR, err.to_string())
    }

    /// [`SWITCH_FAILED`] saying `err`.
    #[must_use]
    pub fn switch_failed(err: impl std::fmt::Display) -> Self {
        Self::new(SWITCH_FAILED, err.to_string())
    }
}

/// A switch's error as its refusal: [`SWITCH_BUSY`] while a run is open,
/// else [`SWITCH_FAILED`], saying the error.
impl From<super::conversations::SwitchError> for Refusal {
    fn from(err: super::conversations::SwitchError) -> Self {
        let code = match err {
            super::conversations::SwitchError::Running => SWITCH_BUSY,
            _ => SWITCH_FAILED,
        };
        Self::new(code, err.to_string())
    }
}

/// Read a method's parameters (`null`: none, read as `{}`).
///
/// # Errors
///
/// [`INVALID_PARAMS`] when they do not read as `T`.
pub fn params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Refusal> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(params).map_err(|err| Refusal::invalid(&err))
}

/// A request id as the engine's wire types it (`None`: absent, `null`, or
/// not a number or string).
#[must_use]
pub fn request_id(id: &Value) -> Option<RpcId> {
    serde_json::from_value(id.clone()).ok()
}

/// The answer line (newline included) of the request `id`: its result.
#[must_use]
pub fn result_line(id: Option<RpcId>, result: impl Serialize) -> String {
    response_line(&RpcResponse::success(id, result))
}

/// The notification line (newline included) of `method` with `params`.
#[must_use]
pub fn notification_line(method: &str, params: &impl Serialize) -> String {
    format!(
        "{}\n",
        serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params})
    )
}

/// The answer line (newline included) of the request `id`: a refusal.
#[must_use]
pub fn error_line(id: Option<RpcId>, code: i32, message: &str) -> String {
    response_line(&RpcResponse::error(id, code, message))
}

/// `response` as a line, newline included.
#[must_use]
pub fn response_line(response: &RpcResponse) -> String {
    let mut line = serde_json::to_string(response).unwrap_or_else(|err| {
        format!(
            r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":{INTERNAL_ERROR},"message":"cannot write the answer: {err}"}}}}"#
        )
    });
    line.push('\n');
    line
}

/// [`ATTACH`]'s parameters. The token is never sent: the first attach
/// carries a `nonce`, the daemon answers with an [`AttachChallenge`], and
/// the second attach carries only the client's `proof`
/// ([`super::handshake`]).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachParams {
    /// The client's nonce: asks the daemon to prove it holds the token
    /// before the client proves it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// The client's proof, in the second attach request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
    /// The start token the client that launched the daemon handed it
    /// (`RKAT_TUI_START_TOKEN`): the daemon counts the connection that
    /// presents it as its owner ([`Status::owner`]). A client presents its
    /// own on every attach; a daemon it did not start ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_token: Option<String>,
    /// A connection that only reads the status (`--list`): not counted as
    /// an attached client, and its end is no detach.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probe: bool,
    /// The agent the connection works with; required unless it is a
    /// probe, which is bound to none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentChoice>,
}

/// Which agent a connection works with ([`AttachParams::agent`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub enum AgentChoice {
    /// A new agent (the one the daemon seated at start, while no client
    /// took it).
    New,
    /// The most recently active agent; with none but the start's, the
    /// conversation used last, seated again under the agent that ran it.
    Recent,
    /// This agent (`/agents <name>`, a reconnect).
    Agent {
        /// Its name.
        name: String,
    },
    /// The agent that owns this conversation (`--resume <ref>`; a session
    /// id or a unique prefix), seated again on it when that agent quit.
    Conversation {
        /// The reference.
        reference: String,
    },
}

/// One live agent ([`Status::agents`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    /// Its name (`main-<id>`).
    pub name: String,
    /// The conversation it runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Whether its turn is running (`None`: its state could not be read,
    /// which is not idle).
    #[serde(default)]
    pub running: Option<bool>,
    /// The clients attached to it.
    pub clients: usize,
    /// It was left running without a client (`/detach`, a hangup).
    pub detached: bool,
    /// How many live schedules target it.
    #[serde(default)]
    pub schedules: usize,
    /// Its wires to agents of other workspaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wires: Vec<WireRow>,
}

/// Whether an agent runs, as a client words it ([`AgentRow::run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunWord {
    /// A run of it is open.
    Running,
    /// No run of it is open.
    Idle,
    /// Its state could not be read.
    Unknown,
}

impl RunWord {
    /// `running`, `idle` or `unknown`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    /// As a picker row says it: `working`, `idle` or `unknown`.
    #[must_use]
    pub fn picker_word(self) -> &'static str {
        match self {
            Self::Running => "working",
            other => other.as_str(),
        }
    }
}

impl AgentRow {
    /// Whether it runs.
    #[must_use]
    pub fn run(&self) -> RunWord {
        match self.running {
            Some(true) => RunWord::Running,
            Some(false) => RunWord::Idle,
            None => RunWord::Unknown,
        }
    }

    /// Its run state in words: `running`, `idle` or `unknown`.
    #[must_use]
    pub fn run_label(&self) -> &'static str {
        self.run().as_str()
    }

    /// Why this agent may not be retired from `/agents` by a client
    /// working with `mine` (`None`: it may): it is that client's own (which
    /// leaves with `/quit`), it has a client, or its turn runs or its state
    /// cannot be read (unknown is not idle).
    #[must_use]
    pub fn retire_refusal(&self, mine: Option<&str>) -> Option<String> {
        let name = &self.name;
        if mine == Some(name.as_str()) {
            return Some(format!(
                "{name} is this rkat-tui's own agent: /quit retires it"
            ));
        }
        if self.clients > 0 {
            return Some(format!(
                "{name} has {}: only an agent nobody works with is retired",
                crate::text::count(self.clients, "client")
            ));
        }
        match self.running {
            Some(false) => None,
            Some(true) => Some(format!(
                "{name} is running a turn: retire it once it is idle"
            )),
            None => Some(format!(
                "{name}'s state cannot be read: it is not retired while it may be running"
            )),
        }
    }
}

impl std::fmt::Debug for AttachParams {
    /// Never the tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachParams")
            .field("start_token", &self.start_token.as_ref().map(|_| "…"))
            .field("probe", &self.probe)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

/// The daemon's answer to an attach that carries a nonce: its own nonce
/// and its proof that it holds the token ([`super::handshake`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachChallenge {
    /// The daemon's nonce.
    pub nonce: String,
    /// `HMAC-SHA256(token, daemon label ‖ client nonce ‖ daemon nonce)`, hex.
    pub proof: String,
}

/// [`ATTACH`]'s first result in the mutual handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenged {
    /// The challenge.
    pub challenge: AttachChallenge,
}

/// [`ATTACH`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    /// Always `true`.
    pub attached: bool,
    /// The agent the connection is bound to (`None`: a probe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The conversation that agent runs as now (later moves are pushed:
    /// [`SWITCHED`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// The agent was seated for this connection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seated: bool,
    /// The daemon's build ([`crate::BUILD`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

/// [`method::Status`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The canonical workspace path the daemon serves.
    pub workspace: std::path::PathBuf,
    /// The host name of the machine the daemon runs on (`None`: it could
    /// not read it). A client reached through a
    /// tunnel compares it with its own to tell whether the workspace is
    /// on its machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The daemon's process id.
    pub pid: u32,
    /// When it started, RFC 3339 in UTC.
    pub started_at: String,
    /// The attached clients, the caller included (probes are not counted).
    pub clients: usize,
    /// Whether a live agent was left running without a client
    /// (`/detach`, a client's hangup, or the daemon restored it).
    pub detached: bool,
    /// Whether the caller is the client that started the daemon.
    pub owner: bool,
    /// The caller's agent's current conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Whether the caller's agent's turn is running, or its state cannot
    /// be read (unknown is not idle: `/quit` warns as for a running turn).
    pub running: bool,
    /// The agent the caller is bound to (`None`: a probe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Every live agent, names in order.
    pub agents: Vec<AgentRow>,
}

/// [`method::Holds`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holds {
    /// Each conversation with a user turn, or that cannot be read, in
    /// words (empty: the realm is provably empty).
    pub held: Vec<String>,
}

/// [`method::Shutdown`]'s parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownParams {
    /// Retire every agent before stopping (else they are restored at the
    /// next start, as after SIGTERM).
    #[serde(default)]
    pub retire: bool,
}

/// [`method::Blocked`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedList {
    /// The blocked calls, oldest first.
    pub calls: Vec<crate::approval::grants::BlockedCall>,
}

/// [`method::Approve`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveParams {
    /// The blocked call's number.
    pub id: u64,
    /// How long the approval holds.
    pub scope: crate::approval::grants::GrantScope,
    /// The kind of action to allow, in the user's words (the call's
    /// `EFFECT:` sentence as the user edited it): a statement instead of
    /// the call's grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
}

/// [`method::AllowStatement`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementParams {
    /// The kind of action to allow, in the user's words.
    pub text: String,
    /// How long it holds: this session, always for this workspace or in
    /// every workspace.
    pub scope: crate::approval::grants::GrantScope,
}

/// [`method::RemoveGrant`]'s parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantParams {
    /// The grant's number, as `/permissions` lists it.
    pub id: u64,
}

/// [`method::Recall`]'s and [`method::Inputs`]'s parameters: the inputs
/// `mob/member_send` admitted (`admission.input_id`), oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallParams {
    /// The inputs to take back, or to read.
    pub input_ids: Vec<meerkat_core::lifecycle::InputId>,
    /// A recall of messages the daemon still holds only
    /// (`super::activity::steers`): one sent meanwhile is left with the
    /// agent (reached), never recalled from the engine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_only: bool,
}

/// [`method::Recall`]'s result, each list in the order asked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recalled {
    /// Abandoned while queued: the agent never reads them.
    pub recalled: Vec<meerkat_core::lifecycle::InputId>,
    /// Already joined a run (or ended): left as they are.
    pub reached: Vec<meerkat_core::lifecycle::InputId>,
    /// Not inputs of the caller's agent's current conversation (another
    /// agent's, another conversation's, or none at all): left alone.
    pub foreign: Vec<meerkat_core::lifecycle::InputId>,
    /// The cancels the runtime refused, in words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// [`method::Btw`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtwParams {
    /// The client's number for it ([`method::BtwCancel`] names it).
    pub id: u64,
    /// The user's question.
    pub question: String,
    /// The tool call the agent is running as the client saw it: its name
    /// and how long it has run, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_tool: Option<(String, u64)>,
    /// The running turn as the client shows it: the session's stored
    /// history has a run only once it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn: Option<String>,
}

/// [`method::Btw`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtwAnswer {
    /// The answer's text.
    pub answer: String,
}

/// [`method::BtwCancel`]'s parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtwCancelParams {
    /// The [`BtwParams::id`] to cancel.
    pub id: u64,
}

/// Which run an interrupt ([`method::Interrupt`]) is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum InterruptTarget {
    /// This run, and no other: once it has ended the interrupt stops
    /// nothing.
    Run {
        /// The run.
        run_id: meerkat_core::lifecycle::RunId,
    },
    /// The run the agent has as the daemon reads it (a client that knows no
    /// run's id); the answer names it.
    Current,
    /// No run: only the named messages are taken back (one admitted after
    /// the interrupt's first press), with the follow-up run that took one.
    Inputs,
}

/// [`method::Interrupt`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptParams {
    /// The run to interrupt.
    #[serde(flatten)]
    pub target: InterruptTarget,
    /// The caller's messages the agent has not read yet (the inputs
    /// `mob/member_send` admitted), oldest first: each is taken back
    /// unless the agent has read it.
    #[serde(default)]
    pub input_ids: Vec<meerkat_core::lifecycle::InputId>,
    /// Cancel the run at once (a second Esc, or no end within the
    /// client's deadline), not at the stop's own pace.
    #[serde(default)]
    pub hard: bool,
}

/// What an interrupt did to its run ([`Interrupted`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunInterrupt {
    /// The run was stopped (or is being stopped by the runtime already).
    Stopped,
    /// The run had already ended: nothing was stopped.
    Ended,
    /// The agent runs nothing ([`InterruptTarget::Current`]), or no run
    /// was asked for ([`InterruptTarget::Inputs`]).
    Idle,
    /// The runtime did not take the interrupt; `why` says, in the user's
    /// words, what that means and what to do (the engine's own words are
    /// in the daemon's log and the audit log).
    Failed {
        /// What happened and what to do.
        why: String,
    },
}

/// [`method::Interrupt`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interrupted {
    /// The run the interrupt was for (the one it found, for
    /// [`InterruptTarget::Current`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<meerkat_core::lifecycle::RunId>,
    /// What became of that run.
    #[serde(flatten)]
    pub run: RunInterrupt,
    /// Follow-up runs stopped because they had taken one of the messages.
    #[serde(default)]
    pub follow_ups: usize,
    /// Messages taken back: the agent never reads them.
    #[serde(default)]
    pub returned: Vec<meerkat_core::lifecycle::InputId>,
    /// Messages the agent has read: they stay.
    #[serde(default)]
    pub read: Vec<meerkat_core::lifecycle::InputId>,
}

/// What became of one input ([`method::Inputs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fate", rename_all = "snake_case")]
pub enum InputFate {
    /// The runtime still holds it: not written into a run yet.
    Pending,
    /// Written into a run (applied or consumed, alone or merged into
    /// another input).
    Taken,
    /// Ended without being written into a run (abandoned, superseded):
    /// the agent never reads it.
    Dropped {
        /// Why, in words, when the runtime records why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// Not an input of the caller's agent's current conversation.
    Unknown,
}

/// One input and its fate ([`InputFates`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRow {
    /// The input.
    pub input_id: meerkat_core::lifecycle::InputId,
    /// What became of it.
    #[serde(flatten)]
    pub fate: InputFate,
}

/// The field of a client's `mob/member_send` params that names the send
/// with a key of the client's own ([`method::Sent`]). The daemon takes it
/// out before the request goes to the engine (whose params refuse unknown
/// fields).
pub const SEND_KEY: &str = "rkat_send_key";

/// [`method::Sent`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentParams {
    /// The client's key of the send ([`SEND_KEY`]).
    pub key: String,
}

/// What the daemon did with a send ([`method::Sent`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fate", rename_all = "snake_case")]
pub enum SentFate {
    /// No send under the key reached the daemon (or one so long ago it is
    /// forgotten): the agent does not have it.
    NotReceived,
    /// Forwarded to the engine, which has not answered it yet.
    OnItsWay,
    /// The engine admitted it, as the input it names (when it named one).
    Admitted {
        /// The input it became.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<meerkat_core::lifecycle::InputId>,
    },
    /// The engine refused it, in its words: the agent does not have it.
    Refused {
        /// Why.
        why: String,
    },
    /// The daemon holds it behind the user's message in flight
    /// ([`HELD_ANSWER`]), under the id it names.
    Held {
        /// The id the client follows it by.
        input_id: meerkat_core::lifecycle::InputId,
    },
}

/// The field of the daemon's own answer to a `mob/member_send` it held
/// (`super::activity::steers`): the id it is followed by from now on (it is
/// no engine input). The engine's answer is never so: a client reads this
/// field before it reads the engine's.
pub const HELD_ANSWER: &str = "rkat_held";

/// The notification that held messages moved on ([`HeldChange`],
/// `super::activity::steers`): sent to the engine, written into a request,
/// or dropped. Pushed to every client; one knows the ids of its own.
pub const HELD: &str = "rkat/held";

/// [`HELD`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldChange {
    /// The agent.
    pub agent: String,
    /// The held messages, by the ids their clients know them by.
    pub ids: Vec<meerkat_core::lifecycle::InputId>,
    /// What became of them.
    pub now: HeldNow,
}

/// What became of held messages ([`HeldChange`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeldNow {
    /// Sent to the engine as one steer, the input it names: it goes in at
    /// the agent's next step, and its client follows it by that input
    /// from now on (it outlives the daemon, the held id does not).
    Released {
        /// The input they went as.
        input_id: meerkat_core::lifecycle::InputId,
    },
    /// Written into the request that started now.
    Applied,
    /// Never sent (why, in words): its client takes it back.
    Dropped {
        /// Why.
        why: String,
    },
    /// Written into a request whose answer the daemon's stop cut off, so
    /// the engine does not keep it (why, in words): the client that sent
    /// it, which saw it delivered, takes this text back.
    Unkept {
        /// Why.
        why: String,
        /// What the message said (several sent as one: all of them).
        text: String,
    },
}

/// [`method::Inputs`]'s result, in the order asked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFates {
    /// Each input asked about (one that could not be read is left out).
    pub inputs: Vec<InputRow>,
}

/// What retired ([`QuitOutcome::Retired`], [`method::RetireAgent`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    /// The agent that retired.
    pub agent: String,
    /// The conversation it ran as (it stays in the conversation list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// The schedules deleted with it (by name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules_deleted: Vec<String>,
}

/// What [`method::Quit`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum QuitOutcome {
    /// Other clients work with the caller's agent: the caller left, the
    /// agent stays for them.
    Left {
        /// How many other clients the agent has.
        others: usize,
    },
    /// Other clients work with other agents: the caller's agent retired,
    /// the daemon stays for them.
    Retired {
        /// What retired.
        retired: Retired,
    },
    /// The caller started the daemon, was its last client and no agent was
    /// left running without one: the daemon stops, every agent retired.
    Terminated,
    /// Nothing was done: ask the user whether to detach or terminate.
    Ask {
        /// Why the daemon is not just stopped.
        reason: String,
        /// The agent's turn runs (terminating interrupts it).
        running: bool,
    },
}

/// A user's action on one schedule (`/schedules pause|resume|delete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleOp {
    /// Fire nothing until resumed.
    Pause,
    /// Fire again.
    Resume,
    /// Fire never again.
    Delete,
}

/// [`method::Schedule`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleAction {
    /// What to do.
    pub op: ScheduleOp,
    /// The schedule's name or id.
    pub schedule: String,
}

/// One schedule as [`method::Schedules`] lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRow {
    /// The engine's id.
    pub id: String,
    /// Its name (else the id).
    pub name: String,
    /// When it fires, in a line.
    pub expression: String,
    /// The next occurrence due, when one is planned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// `active` or `paused`.
    pub state: String,
    /// How many times it fired (occurrences delivered to the agent).
    pub fired: usize,
    /// The tokens its runs used, by model, as the daemon counted them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<crate::cost::SessionCost>,
}

/// [`method::Schedules`]'s list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleList {
    /// The caller's agent's live schedules.
    pub schedules: Vec<ScheduleRow>,
}

/// [`TRIGGERED`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggeredNote {
    /// The agent whose run it is.
    pub agent: String,
    /// The run's session.
    pub session_id: SessionId,
    /// What fired it: `scheduled: <name>` or `files changed: <paths>`.
    pub label: String,
    /// The run's input.
    pub prompt: String,
    /// A note of helpers' endings: its folded line, made from the endings
    /// themselves (absent for any other run, and from an earlier daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::daemon::agents::endings::NoteSummary>,
}

/// [`SWITCHED`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchedNote {
    /// The agent.
    pub agent: String,
    /// The conversation it runs as now.
    pub session_id: SessionId,
}

/// [`IDLE`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdleNote {
    /// The agent.
    pub agent: String,
    /// The conversation it runs as.
    pub session_id: SessionId,
    /// The run of it that ended last (`None`: none since the daemon
    /// started following it): a client ends a turn in which it saw that
    /// run end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<meerkat_core::RunId>,
    /// The inputs it still holds: not zero only when the runtime started
    /// no run for them within [`super::activity::SETTLE_WAIT`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub held: usize,
}

/// [`RUNNING`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningNote {
    /// The agent.
    pub agent: String,
    /// The conversation the run is of.
    pub session_id: SessionId,
    /// The open run.
    pub run_id: meerkat_core::RunId,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// What [`MOB`] says of a mob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobChange {
    /// It hosts a helper the daemon follows: a client opens its stream.
    Hosts,
    /// It was destroyed.
    Destroyed,
}

/// [`MOB`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobNote {
    /// The mob.
    pub mob_id: meerkat_mob::MobId,
    /// What happened.
    pub change: MobChange,
}

/// [`COST`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostNote {
    /// The conversation.
    pub session_id: SessionId,
    /// Its totals now.
    pub cost: crate::cost::SessionCost,
    /// The context of its last model call (the gauge), as the daemon's
    /// ledger keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<super::usage::ContextReading>,
}

/// A file and its trust in words (`absent`, `trusted`, `not trusted: …`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedFile {
    /// The file.
    pub path: std::path::PathBuf,
    /// Its trust in words.
    pub trust: String,
}

/// [`method::Report`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The realm directory.
    pub realm: std::path::PathBuf,
    /// The realm id.
    pub realm_id: String,
    /// The shell's confinement mode (`workspace`, `strict`, `off`).
    pub confinement: String,
    /// The permission mode and the denial counters.
    pub mode: crate::approval::desk::ModeStatus,
    /// The realm rules file (`<realm>/tui.toml`).
    pub realm_rules: std::path::PathBuf,
    /// Each project input (`/trust`) and its trust.
    pub project_inputs: Vec<TrustedFile>,
    /// `[helpers] max_depth`.
    pub helper_depth: u8,
    /// The caller's agent's file watches (`watch_paths`), described.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watches: Vec<String>,
    /// The approval classifier's model (`None`: none is configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_model: Option<String>,
}

/// [`method::SetMode`]'s parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetModeParams {
    /// The mode.
    pub mode: crate::approval::mode::PermissionMode,
    /// Recorded in the user's own `tui.toml` (`/mode <mode> --user`)
    /// instead of the workspace's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user: bool,
}

/// [`method::SetMode`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSet {
    /// The mode and the counters now.
    pub status: ModeStatus,
    /// What the mode does.
    pub notice: String,
    /// What did not work (not saved, the agent not told).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// [`method::Conversations`]'s result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationList {
    /// Last used first.
    pub conversations: Vec<Listed>,
}

/// The name a workspace is shown by in a cross-workspace label: its
/// directory's last component (the whole path when it has none).
#[must_use]
pub fn workspace_name(workspace: &std::path::Path) -> String {
    workspace.file_name().map_or_else(
        || workspace.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// What another workspace's agent is called across a wire:
/// `<workspace name>/<agent>` (both workspaces name their agents
/// `main-<id>`, so the agent's name alone could be a local one).
#[must_use]
pub fn wire_label(workspace: &std::path::Path, agent: &str) -> String {
    format!("{}/{agent}", workspace_name(workspace))
}

/// A top-level agent's card ([`method::Card`]): what another
/// workspace's daemon needs to trust it and reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCard {
    /// The canonical workspace the agent works in.
    pub workspace: std::path::PathBuf,
    /// The agent's name (`main-<id>`).
    pub agent: String,
    /// Its comms peer id (derived from the key).
    pub peer_id: String,
    /// Its comms public key, `ed25519:<base64>`.
    pub pubkey: String,
    /// Its socket, `uds://<path>`: for an agent on another machine, the
    /// forwarded socket on this one.
    pub address: String,
    /// How this daemon reaches it when it is on another machine (`/wire
    /// ssh://…`); `None`: on this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// On the remote half of a wire made over ssh from another machine:
    /// that machine's host name as it calls itself (empty: unknown). Its
    /// daemon holds the link, `address` is the link's forwarded socket
    /// here. `None`: a wire made on this machine (and in files of builds
    /// before 0.0.19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_from: Option<String>,
}

/// The ssh link of a wire to an agent on another machine
/// (`crate::daemon::remote_wire`), as the initiating daemon keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Via {
    /// The ssh host, as the user gave it.
    pub host: String,
    /// The remote rkat-tui's program (`None`: `rkat-tui` on its `PATH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// The remote agent's own socket there (the `-L` target).
    pub remote_target: std::path::PathBuf,
    /// The forwarded socket there that reaches this daemon's agent (the
    /// `-R` socket), which the remote's card for this agent names.
    pub remote_forward: std::path::PathBuf,
    /// The longest socket path that machine binds.
    pub remote_limit: usize,
    /// This agent's key as last handed to the remote daemon.
    #[serde(default)]
    pub sent_key: String,
}

impl WireCard {
    /// `<workspace name>/<agent>` ([`wire_label`]).
    #[must_use]
    pub fn label(&self) -> String {
        wire_label(&self.workspace, &self.agent)
    }
    /// Whether it is the card of `agent` in `workspace`.
    #[must_use]
    pub fn names(&self, workspace: &std::path::Path, agent: &str) -> bool {
        self.workspace == workspace && self.agent == agent
    }
}

/// [`method::Card`]'s parameters: the agent (a probe names it; a
/// bound caller names none, or its own).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireAgentParams {
    /// The agent; `None`: the caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// [`method::Wire`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireParams {
    /// The agent of this daemon; `None`: the caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The other workspace's agent.
    pub card: WireCard,
}

/// [`method::Wire`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wired {
    /// The other agent's label.
    pub label: String,
    /// The wire was there already, to the same key: nothing changed.
    pub already: bool,
}

/// [`method::Unwire`]'s parameters: the other agent by its workspace and
/// name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnwireParams {
    /// The agent of this daemon; `None`: the caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The other agent's workspace.
    pub workspace: std::path::PathBuf,
    /// The other agent's name.
    pub peer: String,
}

/// [`method::Unwire`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unwired {
    /// The other agent's label.
    pub label: String,
    /// A wire was removed (`false`: there was none).
    pub removed: bool,
    /// For a wire over ssh: what became of the remote half, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// [`method::WireSsh`]'s parameters: the host, the agent there
/// (`<workspace>/<agent>`, the workspace by name or path, or a workspace
/// with one agent; `None`: list the agents there), and the remote
/// rkat-tui's program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireSshParams {
    /// The ssh host.
    pub host: String,
    /// The agent there; `None`: only list them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The remote rkat-tui (`None`: `rkat-tui` on its `PATH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
}

/// [`method::WireSsh`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WiredSsh {
    /// The agents there (a listing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<crate::daemon::remote_wire::RemoteAgent>,
    /// What was done, in words (a wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One cross-workspace wire of an agent ([`AgentRow::wires`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRow {
    /// The other agent's workspace.
    pub workspace: std::path::PathBuf,
    /// The other agent's name.
    pub agent: String,
    /// The ssh host it is reached through (`None`: on this machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The machine the wire was made from over ssh, whose daemon holds
    /// the link (the remote half, [`WireCard::ssh_from`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Its link's state, for one over ssh: `connecting`, `up` or `down`
    /// (on the remote half: `up` while the link's socket is here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl WireRow {
    /// `<workspace name>/<agent>`.
    #[must_use]
    pub fn label(&self) -> String {
        wire_label(&self.workspace, &self.agent)
    }

    /// The label as `/agents` shows it: one over ssh with its host and
    /// its link's state (`api/main-ab12 (ssh box, link up)`).
    #[must_use]
    pub fn shown(&self) -> String {
        match (&self.host, &self.from) {
            (Some(host), _) => format!(
                "{} (ssh {host}, link {})",
                self.label(),
                self.link.as_deref().unwrap_or("down")
            ),
            (None, Some(from)) => format!(
                "{} (ssh from {}, link {})",
                self.label(),
                machine(from),
                self.link.as_deref().unwrap_or("down")
            ),
            (None, None) => self.label(),
        }
    }
}

/// A machine by its host name as a sentence names it (`another machine`
/// when it is not known).
#[must_use]
pub fn machine(host: &str) -> &str {
    if host.is_empty() {
        "another machine"
    } else {
        host
    }
}

/// [`method::RetireAgent`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentParams {
    /// The agent's name (`main-<id>`).
    pub name: String,
}

/// [`method::StopHelper`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopHelperParams {
    /// The helper's labels, outermost first (its own last), as its line
    /// shows them.
    pub path: Vec<String>,
}

/// The parameters of the methods on one conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParams {
    /// The conversation's session.
    pub session_id: SessionId,
}

/// [`method::New`]'s and [`method::Resume`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switched {
    /// The session the agent runs as now.
    pub session_id: SessionId,
    /// What did not work (the new conversation not told about plan mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// [`method::Transcript`]'s result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    /// The committed messages.
    pub messages: Vec<Message>,
}

/// A notification the daemon pushes to every attached client with what the
/// approval engine reports ([`ApprovalNote`]): the classifier's verdicts,
/// auto mode pausing and resuming, the classifier's work (its token usage
/// is counted in the daemon, [`COST`]).
pub const APPROVAL_NOTE: &str = "rkat/approval_note";

/// What the approval engine reported, as [`APPROVAL_NOTE`] carries it (the
/// engine's `ApprovalEvent` without the dialogs, which the consent surface
/// pushes as decisions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalNote {
    /// A tool call was decided.
    Decision {
        /// The session whose gate decided (the agent's, for its helpers
        /// too).
        session_id: SessionId,
        /// The helper that made the call (`None`: the agent itself).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// Its tool-use id.
        tool_use_id: String,
        /// The tool.
        tool: String,
        /// The tier and who decided.
        verdict: crate::approval::policy::VerdictSummary,
        /// Why it was denied, or the classifier's reason for an allow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Auto mode paused.
    Paused {
        /// Why.
        cause: crate::approval::counters::PauseCause,
    },
    /// Auto mode resumed.
    Resumed,
    /// Broad allow rules were dropped on entering auto mode (described).
    RulesDropped {
        /// The rules, as `/permissions` lists them.
        rules: Vec<String>,
    },
    /// The classifier started or finished a review.
    Classifying {
        /// Whether one runs now.
        running: bool,
    },
    /// Something the human should know about the approval machinery.
    Notice {
        /// What.
        text: String,
    },
    /// A classifier check (`/auto check`) judged `done` of its `total`
    /// cases (every client shows it: the daemon runs one at a time).
    CheckProgress {
        /// Cases judged.
        done: usize,
        /// Cases it sends.
        total: usize,
        /// Whether it still runs.
        running: bool,
    },
}

impl ApprovalNote {
    /// The note for `event` (`None`: a withdrawn escalation, which the
    /// decision push already closes, and a classifier call's usage, which
    /// the daemon counts).
    #[must_use]
    pub fn of(event: crate::approval::ApprovalEvent) -> Option<Self> {
        use crate::approval::ApprovalEvent;
        Some(match event {
            ApprovalEvent::Decision {
                session_id,
                agent,
                tool_use_id,
                tool,
                verdict,
                reason,
                ..
            } => Self::Decision {
                session_id,
                agent,
                tool_use_id,
                tool,
                verdict,
                reason,
            },
            ApprovalEvent::Paused(cause) => Self::Paused { cause },
            ApprovalEvent::Resumed => Self::Resumed,
            ApprovalEvent::RulesDropped(rules) => Self::RulesDropped {
                rules: rules
                    .iter()
                    .map(crate::approval::desk::describe_rule)
                    .collect(),
            },
            ApprovalEvent::Classifying(running) => Self::Classifying { running },
            ApprovalEvent::Notice(text) => Self::Notice { text },
            ApprovalEvent::CheckProgress {
                done,
                total,
                running,
            } => Self::CheckProgress {
                done,
                total,
                running,
            },
            ApprovalEvent::EscalationWithdrawn(_)
            | ApprovalEvent::ClassifierUsage { .. }
            | ApprovalEvent::Granted { .. } => {
                return None;
            }
        })
    }
}

/// [`method::Endpoint`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEndpoint {
    /// The configured endpoint (`None`: a built-in one, named by the
    /// provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// [`method::Context`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReport {
    /// The conversation.
    pub session_id: SessionId,
    /// Its messages by kind.
    pub messages: super::context::MessageCounts,
    /// The tool definitions its model requests carry, by family, the
    /// largest first (`None`: not read yet; the daemon reads them while
    /// the agent is idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<super::context::FamilySize>>,
    /// The tool definitions its requests leave out until a search reveals
    /// them, by family, the largest first (`super::deferral`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<super::context::FamilySize>,
    /// The skills directories a session build lists skills from.
    pub skills: crate::bootstrap::skills::SkillsReport,
}

/// [`method::Usage`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// The token totals by model.
    #[serde(default)]
    pub cost: crate::cost::SessionCost,
    /// The context of the conversation's last model call, as the daemon
    /// saw it (the client's context gauge until its next turn ends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<super::usage::ContextReading>,
}

/// [`method::Tasks`]'s result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tasks {
    /// The conversation's tasks as the store lists them.
    pub tasks: Vec<meerkat_tools::builtin::types::Task>,
}

/// [`method::SetModel`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetModelParams {
    /// The model id.
    pub model: String,
    /// Its provider (`anthropic`, `openai`, `gemini`); `None` keeps the
    /// session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The configured endpoint's binding; `None` (a built-in endpoint)
    /// clears the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_binding: Option<meerkat_core::AuthBindingRef>,
    /// What the endpoint's model list says of the model (a model only an
    /// endpoint knows gets a profile in the realm config from it); `None`:
    /// not listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<ListedModel>,
}

/// A model as the client's endpoint lists report it ([`SetModelParams`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedModel {
    /// Its context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Its output limit in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Whether it reasons (LiteLLM's `supports_reasoning`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Whether it takes images (LiteLLM's `supports_vision`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
}

/// [`method::SetModel`]'s result: the session's identity after the switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSet {
    /// The session the switch applied to.
    pub session_id: SessionId,
    /// Its model now.
    pub model: String,
    /// Its provider now.
    pub provider: String,
    /// The agent was seated again on the session first (its agent
    /// rebuilt for a model it had no profile for): the conversation's
    /// stream is a new one, which the client opens again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reseated: bool,
}

/// `decision/decide` as a client of the daemon sends it: the engine's
/// closed request, and the dialog's answer (stripped before dispatch).
/// Closed: a field that names who decided (`actor`, `approver`, …) is
/// refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideParams {
    /// The decision.
    pub decision_id: String,
    /// Approved (else rejected).
    pub approve: bool,
    /// The digest of the rendering the client showed.
    pub presented_digest: String,
    /// The decider's reason (the dialog's comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The dialog's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<RemoteAnswer>,
}

/// One start-up notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupNotice {
    /// A warning (else information).
    pub warn: bool,
    /// What it says.
    pub text: String,
}

impl StartupNotice {
    /// Information.
    #[must_use]
    pub fn info(text: String) -> Self {
        Self { warn: false, text }
    }

    /// A warning.
    #[must_use]
    pub fn warn(text: String) -> Self {
        Self { warn: true, text }
    }
}

/// [`method::Audit`]'s parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditParams {
    /// How many lines (default [`AUDIT_LINES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
}

/// [`method::Audit`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditTail {
    /// The audit log's file.
    pub path: std::path::PathBuf,
    /// Its last lines for the agent, oldest first, as written (JSON).
    pub lines: Vec<String>,
}

/// [`method::Notices`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notices {
    /// In the order the daemon's start-up produced them, which a client
    /// keeps.
    pub notices: Vec<StartupNotice>,
}

/// [`FILE_DIFF`]'s parameters: one file change, as the agent's
/// file tools report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffNote {
    /// The agent whose file tools made it.
    pub agent: String,
    /// The tool call that made the change.
    pub tool_use_id: String,
    /// The file (relative to the workspace when inside it).
    pub path: String,
    /// The unified diff.
    pub unified: String,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
}

/// [`DECISION_ENDED`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionEnded {
    /// The decision.
    pub decision_id: String,
    /// The phase it ended in, when known.
    pub phase: Option<WireDecisionPhase>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every method has its own `rkat/*` name (the dispatcher takes the
    /// first match).
    #[test]
    fn the_methods_have_distinct_local_names() {
        let mut seen = std::collections::HashSet::new();
        for name in METHODS {
            assert!(name.starts_with(PREFIX), "{name}");
            assert!(seen.insert(*name), "{name} twice");
        }
        assert!(!METHODS.contains(&ATTACH));
    }

    #[test]
    fn answer_lines_are_json_rpc_responses() {
        let ok = result_line(
            Some(RpcId::Num(3)),
            Usage {
                input_tokens: 1,
                output_tokens: 2,
                cost: crate::cost::SessionCost::default(),
                context: None,
            },
        );
        let ok: Value = serde_json::from_str(ok.trim_end()).unwrap();
        assert_eq!(
            ok,
            json!({"jsonrpc": "2.0", "id": 3, "result": {"input_tokens": 1, "output_tokens": 2, "cost": {}}})
        );
        let err = error_line(request_id(&json!("a")), SWITCH_BUSY, "busy");
        let err: Value = serde_json::from_str(err.trim_end()).unwrap();
        assert_eq!(
            err,
            json!({"jsonrpc": "2.0", "id": "a", "error": {"code": SWITCH_BUSY, "message": "busy"}})
        );
        assert_eq!(request_id(&Value::Null), None);
    }

    #[test]
    fn a_decide_naming_who_decided_is_refused_and_no_params_read_as_empty() {
        assert!(params::<DecideParams>(json!({"decision_id": "d", "approve": true, "presented_digest": "x", "actor": "mallory"})).is_err());
        assert!(params::<SetModeParams>(Value::Null).is_err());
        let refused = params::<DecideParams>(json!({})).unwrap_err();
        assert_eq!(refused.code, INVALID_PARAMS);
    }
}
