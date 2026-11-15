//! Smart auto-compaction: before the engine compacts an agent's context on
//! its own, the agent is asked to prepare for it (`[auto]
//! compact_prepare`, "update your plan file") and tells the daemon when it
//! is ready (`compact_ready`), which then requests the compaction.
//!
//! The daemon reads every model call's context of each workspace agent
//! (its event watcher, [`super::approvals::watch`]). When a reading of an
//! agent's own conversation reaches `compact_prepare_at` and the trigger
//! is armed, the agent is asked as a steer into its run (when that model
//! call asked for tools, so the run goes on), or the steer is held for the
//! start of its next run (the call ended the run: an idle agent is never
//! woken for it). The request gives leeway: find a good stopping point
//! soon, finish the step in hand, then the instruction, then
//! `compact_ready`; the agent picks the moment. What bounds it: one
//! reminder when the context passes the midpoint to the engine's own
//! threshold (or a further 10% of the window when there is none) without
//! `compact_ready`, and the engine's own threshold itself. The trigger fires once per crossing
//! and re-arms when a reading is below the threshold again (a compaction
//! brought it down, or another conversation started). The agent's
//! `compact_ready` call requests the compaction (`mob/compact_member`,
//! what `/compact` uses), which the engine applies before the run's next
//! model request. The engine's own threshold stays the backstop: an agent
//! that ignores the instruction is compacted there as before. Helpers and
//! forks have no such tool and get no instruction.
//!
//! Right after each compaction of an agent's conversation the agent gets
//! one system note that what was discarded is searchable
//! ([`MEMORY_NOTE`]), when semantic memory is on.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;
use parking_lot::{Mutex, RwLock};

use super::MOB_ID;
use crate::config::registry::Level;
use crate::config::settings::{
    CompactionPolicy, FALLBACK_PREPARE_WINDOW, PrepareAt, PrepareSetting, Sourced,
};
use crate::text::tokens as fmt_ctx_tokens;

/// The steer's last sentence: the one signal the agent gives.
pub const READY_SUFFIX: &str =
    "Then call `compact_ready`, which compacts the context; you continue after it.";

/// How far past the threshold (share of the window) the reminder comes
/// when the engine never compacts on its own.
pub const REMINDER_PERCENT: u64 = 10;

/// What `compact_ready` answers once the compaction was requested.
pub const REQUESTED: &str = "compaction requested: it happens before your next step; details from before stay searchable with memory_search";

/// The system note an agent gets after each compaction of its
/// conversation (semantic memory on).
pub const MEMORY_NOTE: &str = "Your context was compacted just now. What was discarded is indexed: search it with memory_search when you need an earlier detail.";

/// The share of the window below which `compact_ready` asked without an
/// instruction compacts nothing.
pub const FLOOR_PERCENT: u64 = 20;

/// How long `compact_ready` waits for the engine to take the request.
const REQUEST_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// One agent's trigger.
#[derive(Debug, Default, Clone)]
struct Trigger {
    /// It fired for this crossing (re-armed below the threshold).
    fired: bool,
    /// A steer (the request or the reminder) waits for the agent's next
    /// run: its text, and whether it is the reminder.
    held: Option<(String, bool)>,
    /// The agent was asked and has not called `compact_ready` yet: at
    /// which context.
    asked: Option<u64>,
    /// The one reminder of this crossing went (or waits).
    reminded: bool,
    /// The context of its last reading.
    tokens: Option<u64>,
}

/// What a context reading asks of the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Crossing {
    /// Steer `text` into the agent's run now (`tokens`: the reading).
    Steer {
        /// The steer.
        text: String,
        /// The context that crossed.
        tokens: u64,
        /// It is the crossing's one reminder.
        reminder: bool,
    },
    /// Nothing to send now (held for the next run, or no crossing).
    Nothing,
}

/// What `compact_ready` does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ready {
    /// Request the compaction.
    Request,
    /// Compact nothing, for this reason.
    Refuse(String),
}

/// The instruction and its threshold as they are now.
#[derive(Debug, Clone)]
struct Live {
    /// The instruction (`None`: none, or off where it comes from).
    instruction: Sourced<Option<String>>,
    /// Where the agent is asked.
    at: Sourced<PrepareAt>,
}

/// The smart auto-compaction of the workspace's agents.
pub struct Prepare {
    /// The instruction and its threshold in force (the settings path
    /// changes them, [`Self::apply`]).
    setting: RwLock<Live>,
    /// The start-up model's context window.
    window: Option<u64>,
    /// The start-up model (whose window a percentage is taken of).
    model: String,
    /// Where the engine compacts on its own (`None`: never, warn mode).
    backstop: Option<u64>,
    /// Whether semantic memory is on (`memory_search`, compaction
    /// indexing).
    memory: bool,
    /// Each agent's trigger, by name.
    agents: Mutex<HashMap<String, Trigger>>,
    /// The mob host (set once it exists).
    state: OnceLock<Weak<MobMcpState>>,
}

impl std::fmt::Debug for Prepare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepare")
            .field("setting", &*self.setting.read())
            .field("window", &self.window)
            .field("backstop", &self.backstop)
            .finish_non_exhaustive()
    }
}

impl Prepare {
    /// The agents' smart compaction with `setting`, for `model` of
    /// `window` tokens under `policy`.
    #[must_use]
    pub fn new(
        setting: PrepareSetting,
        policy: &CompactionPolicy,
        (window, model): (Option<u64>, &str),
        memory: bool,
    ) -> Self {
        Self {
            setting: RwLock::new(Live {
                instruction: setting.instruction,
                at: setting.at,
            }),
            window,
            model: model.to_string(),
            backstop: policy.backstop_tokens(window),
            memory,
            agents: Mutex::new(HashMap::new()),
            state: OnceLock::new(),
        }
    }

    /// The mob host the requests and steers go through.
    pub fn bind(&self, state: &Arc<MobMcpState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    fn state(&self) -> Option<Arc<MobMcpState>> {
        self.state.get().and_then(Weak::upgrade)
    }

    /// Whether the agents get [`MEMORY_NOTE`] after a compaction.
    #[must_use]
    pub fn memory(&self) -> bool {
        self.memory
    }

    /// The threshold in tokens, when an instruction is set.
    #[must_use]
    pub fn threshold(&self) -> Option<u64> {
        let live = self.setting.read();
        live.instruction
            .value
            .as_ref()
            .map(|_| live.at.value.tokens(self.window))
    }

    /// The start-up notice when the threshold is at or above the
    /// engine's own (the instruction would never run first).
    #[must_use]
    pub fn startup_notice(&self) -> Option<String> {
        let live = self.setting.read().clone();
        live.instruction.value.as_ref()?;
        self.late_notice(live.at.value)
    }

    /// The warning when `at` is at or above the engine's own threshold
    /// (the start-up notice's, and `/compact prepare at`'s).
    fn late_notice(&self, at: PrepareAt) -> Option<String> {
        let tokens = at.tokens(self.window);
        let backstop = self.backstop?;
        (tokens >= backstop).then(|| {
            format!(
                "[auto] compact_prepare_at = {at} ({} tokens) is at or above the automatic compaction at {} tokens, so the prepare instruction would never run first: lower compact_prepare_at (e.g. \"70%\") or raise compact_auto_percent with compact = \"auto\"",
                fmt_ctx_tokens(tokens),
                fmt_ctx_tokens(backstop)
            )
        })
    }

    /// Where the crossing's one reminder comes: midway between the
    /// threshold `at` and the engine's own, or a further
    /// [`REMINDER_PERCENT`] of the window when the engine never compacts
    /// on its own (or its threshold is not above `at`).
    #[must_use]
    pub fn reminder_at(&self, at: u64) -> u64 {
        match self.backstop {
            Some(backstop) if backstop > at => at + (backstop - at) / 2,
            _ => at.saturating_add(self.window_or_fallback() * REMINDER_PERCENT / 100),
        }
    }

    fn window_or_fallback(&self) -> u64 {
        self.window
            .filter(|w| *w > 0)
            .unwrap_or(crate::config::settings::FALLBACK_PREPARE_WINDOW)
    }

    /// A context reading of `agent`'s own conversation: `tokens` after a
    /// model call that `continues` the run (it asked for tools) or ended
    /// it. A crossing with the trigger armed fires it: the request (the
    /// agent picks its moment) goes now when the run continues, else it
    /// waits for the next run. Past [`Self::reminder_at`] with no
    /// `compact_ready` yet, one reminder goes the same way. A reading below
    /// the threshold re-arms it.
    pub fn reading(&self, agent: &str, tokens: u64, continues: bool) -> Crossing {
        let current = {
            let live = self.setting.read();
            live.instruction
                .value
                .clone()
                .map(|instruction| (instruction, live.at.value.tokens(self.window)))
        };
        let Some((instruction, at)) = current else {
            self.agents
                .lock()
                .entry(agent.to_string())
                .or_default()
                .tokens = Some(tokens);
            return Crossing::Nothing;
        };
        let remind_at = self.reminder_at(at);
        let mut agents = self.agents.lock();
        let trigger = agents.entry(agent.to_string()).or_default();
        trigger.tokens = Some(tokens);
        if tokens < at {
            *trigger = Trigger {
                tokens: Some(tokens),
                ..Trigger::default()
            };
            return Crossing::Nothing;
        }
        let (text, reminder) = if !trigger.fired {
            trigger.fired = true;
            (self.request_text(&instruction, tokens), false)
        } else if trigger.asked.is_some() && !trigger.reminded && tokens >= remind_at {
            trigger.reminded = true;
            (self.reminder_text(&instruction, tokens), true)
        } else {
            return Crossing::Nothing;
        };
        if trigger.asked.is_none() {
            trigger.asked = Some(tokens);
        }
        if continues {
            Crossing::Steer {
                text,
                tokens,
                reminder,
            }
        } else {
            trigger.held = Some((text, reminder));
            Crossing::Nothing
        }
    }

    /// A compaction of `agent`'s conversation started: an instruction not
    /// answered yet is moot (a later `compact_ready` is judged afresh).
    pub fn compaction_started(&self, agent: &str) {
        if let Some(trigger) = self.agents.lock().get_mut(agent) {
            trigger.asked = None;
            trigger.held = None;
        }
    }

    /// A compaction estimate of `agent`'s conversation (`tokens` left):
    /// below the threshold the trigger is armed again.
    pub fn compacted(&self, agent: &str, tokens: u64) {
        let threshold = self.threshold();
        let mut agents = self.agents.lock();
        let trigger = agents.entry(agent.to_string()).or_default();
        trigger.asked = None;
        trigger.held = None;
        trigger.tokens = Some(tokens);
        if threshold.is_none_or(|at| tokens < at) {
            trigger.fired = false;
            trigger.reminded = false;
        }
    }

    /// `agent`'s run started: a held steer goes now, when there is one
    /// (`(text, tokens, reminder)`).
    pub fn run_started(&self, agent: &str) -> Option<(String, u64, bool)> {
        let mut agents = self.agents.lock();
        let trigger = agents.get_mut(agent)?;
        let (text, reminder) = trigger.held.take()?;
        Some((text, trigger.tokens.unwrap_or(0), reminder))
    }

    /// The request: the context, leeway to finish the step in hand, the
    /// instruction and the one signal.
    fn request_text(&self, instruction: &str, tokens: u64) -> String {
        format!(
            "Your context is at {} of {} tokens. Find a good stopping point soon: finish the step you are in, do not start a large new piece of work. Then: {instruction}. {READY_SUFFIX}",
            fmt_ctx_tokens(tokens),
            fmt_ctx_tokens(self.window_or_fallback())
        )
    }

    /// The one reminder: the automatic compaction comes regardless.
    fn reminder_text(&self, instruction: &str, tokens: u64) -> String {
        match self.backstop {
            Some(backstop) => format!(
                "Compaction happens automatically at {} tokens whether or not you have prepared: do it now: {instruction}, then `compact_ready`.",
                fmt_ctx_tokens(backstop)
            ),
            None => format!(
                "Your context is at {} tokens and still growing: do it now: {instruction}, then `compact_ready`.",
                fmt_ctx_tokens(tokens)
            ),
        }
    }

    /// The notice an agent's clients get when it is reminded.
    #[must_use]
    pub fn reminded_notice(tokens: u64) -> String {
        format!(
            "context at {}: reminded the agent to prepare for compaction now",
            fmt_ctx_tokens(tokens)
        )
    }

    /// The notice an agent's clients get when it is asked.
    #[must_use]
    pub fn asked_notice(tokens: u64) -> String {
        format!(
            "context at {}: asked the agent to prepare for compaction",
            fmt_ctx_tokens(tokens)
        )
    }

    /// What `agent`'s `compact_ready` does: after the instruction, or with
    /// enough context, request the compaction; else compact nothing.
    fn ready(&self, agent: &str) -> Ready {
        let mut agents = self.agents.lock();
        let trigger = agents.entry(agent.to_string()).or_default();
        if trigger.asked.take().is_some() {
            trigger.held = None;
            return Ready::Request;
        }
        let window = self.window_or_fallback();
        let floor = window.saturating_mul(FLOOR_PERCENT) / 100;
        match trigger.tokens {
            Some(tokens) if tokens < floor => Ready::Refuse(format!(
                "nothing was compacted: the context is at {} tokens, below {FLOOR_PERCENT}% of the window ({}); there is little to gain yet",
                fmt_ctx_tokens(tokens),
                fmt_ctx_tokens(floor)
            )),
            _ => Ready::Request,
        }
    }

    /// `agent` called `compact_ready`: the answer for the model, and
    /// whether it is an error.
    pub async fn compact_ready(&self, agent: &str) -> (String, bool) {
        if !super::agents::is_agent(agent) {
            return (
                "compact_ready is for the workspace's agents; nothing was compacted".to_string(),
                true,
            );
        }
        if let Ready::Refuse(why) = self.ready(agent) {
            return (why, false);
        }
        let Some(state) = self.state() else {
            return (
                "the compaction could not be requested: the daemon is stopping".to_string(),
                true,
            );
        };
        let asked = tokio::time::timeout(
            REQUEST_WAIT,
            state.mob_compact_member(&MobId::from(MOB_ID), AgentIdentity::from(agent)),
        )
        .await;
        match asked {
            Ok(Ok(_)) => (REQUESTED.to_string(), false),
            Ok(Err(err)) => (
                format!("the compaction could not be requested: {err}"),
                true,
            ),
            Err(_) => (
                "the compaction could not be requested: the engine did not answer in time"
                    .to_string(),
                true,
            ),
        }
    }

    /// The settings path's hook for `[auto] compact_prepare` and
    /// `compact_prepare_at` (`/set`, `/compact prepare`, a changed file):
    /// the instruction and the threshold in force from now on, and every
    /// agent's trigger armed against them. The start-up notice's warning
    /// when an instruction is set (or the threshold moved) and the
    /// threshold comes too late to run first.
    pub fn apply(
        &self,
        instruction: Sourced<Option<String>>,
        at: Sourced<PrepareAt>,
    ) -> Option<String> {
        let moved = {
            let mut live = self.setting.write();
            let moved = live.at.value != at.value;
            *live = Live { instruction, at };
            moved
        };
        // What is now set is armed for every agent.
        for trigger in self.agents.lock().values_mut() {
            *trigger = Trigger {
                tokens: trigger.tokens,
                ..Trigger::default()
            };
        }
        let live = self.setting.read().clone();
        (live.instruction.value.is_some() || moved)
            .then(|| self.late_notice(live.at.value))
            .flatten()
    }

    /// The instruction in force in words, with where it comes from:
    /// `"update PLAN.md", from your tui.toml`, `none`, or `off for this
    /// workspace` with what removing the workspace's would bring back
    /// (`beneath`).
    fn instruction_words(
        instruction: &Sourced<Option<String>>,
        beneath: &Sourced<Option<String>>,
    ) -> String {
        match (&instruction.value, instruction.source) {
            (Some(text), source) => format!("\"{text}\", {}", source.source_words()),
            (None, Level::Default) => "none".to_string(),
            (None, Level::Workspace) => match &beneath.value {
                Some(_) => format!(
                    "off for this workspace ({} has one: /compact prepare default uses it)",
                    beneath.source.name()
                ),
                None => "off for this workspace".to_string(),
            },
            (None, source) => format!("off, {}", source.source_words()),
        }
    }

    /// The threshold in force in words, with where it comes from.
    fn threshold_words(&self, at: &Sourced<PrepareAt>) -> String {
        format!("{}, {}", self.describe(at.value), at.source.source_words())
    }

    /// `at` in words: `65% of the window (about 650k tokens for <model>)`
    /// or `300k tokens`.
    pub fn describe(&self, at: PrepareAt) -> String {
        match at {
            PrepareAt::Tokens(tokens) => format!("{} tokens", fmt_ctx_tokens(tokens)),
            PrepareAt::Percent(percent) => {
                let tokens = fmt_ctx_tokens(at.tokens(self.window));
                match self.window.filter(|w| *w > 0) {
                    Some(_) => format!(
                        "{percent}% of the window (about {tokens} tokens for {})",
                        self.model
                    ),
                    None => format!(
                        "{percent}% of the window (about {tokens} tokens: the window of {} is unknown, taken as {})",
                        self.model,
                        fmt_ctx_tokens(FALLBACK_PREPARE_WINDOW)
                    ),
                }
            }
        }
    }

    /// `/compact status`'s daemon side for `agent`; `beneath` is the
    /// instruction the layers under the workspace's give.
    #[must_use]
    pub fn status(&self, agent: Option<&str>, beneath: &Sourced<Option<String>>) -> String {
        let backstop = match self.backstop {
            Some(tokens) => format!(
                "the engine compacts on its own at {} tokens",
                fmt_ctx_tokens(tokens)
            ),
            None => "the engine does not compact on its own (compact = \"warn\")".to_string(),
        };
        let live = self.setting.read().clone();
        let asked = self.threshold_words(&live.at);
        let Some(instruction) = live.instruction.value.clone() else {
            let what = match live.instruction.source {
                Level::Default => "none ([auto] compact_prepare, or /compact prepare <text>, sets an instruction the agent gets first)".to_string(),
                _ => Self::instruction_words(&live.instruction, beneath),
            };
            return format!("compact prepare: {what}; its threshold is {asked}; {backstop}");
        };
        let source = live.instruction.source.source_words();
        let trigger = agent
            .and_then(|agent| self.agents.lock().get(agent).cloned())
            .unwrap_or_default();
        let state = match (&trigger.held, trigger.asked) {
            (Some(_), _) => "fired: the agent is asked at the start of its next turn".to_string(),
            (None, Some(at)) => pending_text(at),
            (None, None) if trigger.fired => {
                "fired for this crossing: armed again once the context is below it".to_string()
            }
            (None, None) => "armed".to_string(),
        };
        let now = trigger.tokens.map_or_else(String::new, |tokens| {
            format!(", the context is at {}", fmt_ctx_tokens(tokens))
        });
        format!(
            "compact prepare: \"{instruction}\", {source}; asked at {asked}: {state}{now}; {backstop}"
        )
    }
}

#[async_trait::async_trait]
impl crate::tools::CompactHost for Prepare {
    async fn compact_ready(&self, agent: &str) -> (String, bool) {
        Prepare::compact_ready(self, agent).await
    }
}

/// The pending state, from the request until `compact_ready` or the
/// automatic compaction.
#[must_use]
pub fn pending_text(at: u64) -> String {
    format!(
        "compaction requested of the agent at {} tokens; it prepares when it reaches a stopping point",
        fmt_ctx_tokens(at)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::settings::CompactMode;

    fn prepare(at: PrepareAt, mode: CompactMode) -> Prepare {
        Prepare::new(
            PrepareSetting {
                instruction: Sourced::new(Some("update PLAN.md".into()), Level::User),
                at: Sourced::new(at, Level::User),
            },
            &CompactionPolicy {
                mode,
                ..CompactionPolicy::default()
            },
            (Some(1_000_000), "big-model"),
            true,
        )
    }

    /// A crossing fires once: in a run that goes on the request goes now,
    /// worded with leeway (finish the step in hand), carrying the
    /// instruction and the one signal; further readings above it do
    /// nothing until the reminder's point; one below re-arms it.
    #[test]
    fn a_crossing_fires_once_and_re_arms_below() {
        let p = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        assert_eq!(p.reading("main-a", 100_000, true), Crossing::Nothing);
        let Crossing::Steer {
            text,
            tokens,
            reminder: false,
        } = p.reading("main-a", 310_000, true)
        else {
            panic!("a steer")
        };
        assert_eq!(tokens, 310_000);
        assert_eq!(
            text,
            "Your context is at 310k of 1M tokens. Find a good stopping point soon: finish the step you are in, do not start a large new piece of work. Then: update PLAN.md. Then call `compact_ready`, which compacts the context; you continue after it."
        );
        assert!(text.ends_with(READY_SUFFIX), "{text}");
        assert_eq!(p.reading("main-a", 320_000, true), Crossing::Nothing);
        // Another agent has its own trigger.
        assert!(matches!(
            p.reading("main-b", 400_000, true),
            Crossing::Steer { .. }
        ));
        p.compacted("main-a", 20_000);
        assert!(matches!(
            p.reading("main-a", 330_000, true),
            Crossing::Steer { .. }
        ));
        assert!(
            p.status(Some("main-a"), &Sourced::new(None, Level::Default))
                .contains("compaction requested of the agent at 330k tokens; it prepares when it reaches a stopping point"),
            "{}",
            p.status(Some("main-a"), &Sourced::new(None, Level::Default))
        );
    }

    /// One reminder per crossing: past the midpoint between the threshold
    /// and the engine's own (300k and 800k: 550k) with no `compact_ready`,
    /// and never again; none after `compact_ready`; with no automatic
    /// compaction (warn mode) a further 10% of the window.
    #[test]
    fn one_reminder_at_the_midpoint_and_none_after() {
        let p = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        assert_eq!(p.reminder_at(300_000), 550_000);
        p.reading("main-a", 310_000, true);
        assert_eq!(p.reading("main-a", 540_000, true), Crossing::Nothing);
        let Crossing::Steer {
            text,
            reminder: true,
            ..
        } = p.reading("main-a", 560_000, true)
        else {
            panic!("the reminder")
        };
        assert_eq!(
            text,
            "Compaction happens automatically at 800k tokens whether or not you have prepared: do it now: update PLAN.md, then `compact_ready`."
        );
        assert_eq!(p.reading("main-a", 700_000, true), Crossing::Nothing);
        assert_eq!(p.reading("main-a", 790_000, false), Crossing::Nothing);
        assert_eq!(p.run_started("main-a"), None, "nothing held either");
        // After compact_ready: no reminder, and the pending state is gone.
        let q = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        q.reading("main-a", 310_000, true);
        assert!(
            q.status(Some("main-a"), &Sourced::new(None, Level::Default))
                .contains("compaction requested of the agent")
        );
        assert_eq!(q.ready("main-a"), Ready::Request);
        assert!(
            !q.status(Some("main-a"), &Sourced::new(None, Level::Default))
                .contains("compaction requested of the agent")
        );
        assert_eq!(q.reading("main-a", 600_000, true), Crossing::Nothing);
        // The automatic compaction clears it too.
        let r = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        r.reading("main-a", 310_000, true);
        r.compaction_started("main-a");
        assert!(
            !r.status(Some("main-a"), &Sourced::new(None, Level::Default))
                .contains("compaction requested of the agent")
        );
        // Warn mode: at the threshold plus 10% of the window; a held
        // reminder goes at the next run.
        let w = prepare(PrepareAt::Tokens(300_000), CompactMode::Warn);
        assert_eq!(w.reminder_at(300_000), 400_000);
        w.reading("main-a", 310_000, true);
        assert_eq!(w.reading("main-a", 410_000, false), Crossing::Nothing);
        let (text, tokens, reminder) = w.run_started("main-a").unwrap();
        assert!(reminder && tokens == 410_000, "{text}");
        assert!(
            text.contains("still growing: do it now: update PLAN.md"),
            "{text}"
        );
    }

    /// A crossing on the call that ended the run waits for the next run.
    #[test]
    fn an_idle_agent_gets_it_at_its_next_run() {
        let p = prepare(PrepareAt::Percent(30), CompactMode::Native);
        assert_eq!(p.reading("main-a", 310_000, false), Crossing::Nothing);
        assert!(
            p.status(Some("main-a"), &Sourced::new(None, Level::Default))
                .contains("next turn")
        );
        let (text, tokens, reminder) = p.run_started("main-a").unwrap();
        assert_eq!(tokens, 310_000);
        assert!(!reminder);
        assert!(text.contains("update PLAN.md"));
        assert!(
            p.status(Some("main-a"), &Sourced::new(None, Level::Default))
                .contains("compaction requested of the agent at 310k")
        );
        assert_eq!(p.run_started("main-a"), None);
    }

    /// `compact_ready` after the instruction requests; asked unprompted it
    /// requests too unless the context is below the floor.
    #[test]
    fn ready_requests_unless_below_the_floor() {
        let p = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        p.reading("main-a", 310_000, true);
        assert_eq!(p.ready("main-a"), Ready::Request);
        p.compacted("main-a", 50_000);
        assert!(matches!(p.ready("main-a"), Ready::Refuse(why) if why.contains("below 20%")),);
        p.reading("main-a", 250_000, false);
        assert_eq!(p.ready("main-a"), Ready::Request);
        // No instruction at all: the agent may still ask.
        let none = Prepare::new(
            PrepareSetting::of(&crate::config::registry::Layers::default()),
            &CompactionPolicy::default(),
            (Some(1_000_000), "big-model"),
            true,
        );
        none.reading("main-a", 500_000, true);
        assert_eq!(none.ready("main-a"), Ready::Request);
        assert!(
            none.status(None, &Sourced::new(None, Level::Default))
                .starts_with("compact prepare: none")
        );
    }

    /// The start-up notice names a threshold the engine reaches first.
    #[test]
    fn a_threshold_past_the_engines_is_named_at_start() {
        let late = prepare(PrepareAt::Percent(85), CompactMode::Native);
        let notice = late.startup_notice().unwrap();
        assert!(notice.contains("would never run first"), "{notice}");
        assert!(notice.contains("compact_prepare_at"), "{notice}");
        assert_eq!(
            prepare(PrepareAt::Percent(70), CompactMode::Native).startup_notice(),
            None
        );
        // Warn mode never compacts on its own: no notice.
        assert_eq!(
            prepare(PrepareAt::Percent(95), CompactMode::Warn).startup_notice(),
            None
        );
    }

    /// A new threshold applies live and arms the trigger against it; a
    /// late one warns with the start-up notice's text.
    #[test]
    fn a_new_threshold_applies_live_re_arms_and_warns_when_late() {
        let p = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        assert!(matches!(
            p.reading("main-a", 310_000, true),
            Crossing::Steer { .. }
        ));
        let user = Sourced::new(Some("update PLAN.md".to_string()), Level::User);
        let warning = p.apply(
            user.clone(),
            Sourced::new(PrepareAt::Percent(65), Level::Workspace),
        );
        assert_eq!(warning, None);
        assert_eq!(p.threshold(), Some(650_000));
        // Armed again, against 650k: below it nothing, above it the request.
        assert_eq!(p.reading("main-a", 640_000, true), Crossing::Nothing);
        assert!(matches!(
            p.reading("main-a", 660_000, true),
            Crossing::Steer { .. }
        ));
        let none = Sourced::new(None, Level::Default);
        let status = p.status(Some("main-a"), &none);
        assert!(
            status.starts_with("compact prepare: \"update PLAN.md\", from your tui.toml; asked at 65% of the window (about 650k tokens for big-model), from this workspace: compaction requested of the agent at 660k tokens"),
            "{status}"
        );
        let warning = p.apply(user, Sourced::new(PrepareAt::Percent(85), Level::Workspace));
        let late = prepare(PrepareAt::Percent(85), CompactMode::Native);
        assert_eq!(warning, late.startup_notice());
        assert!(warning.unwrap().contains("would never run first"));
    }

    /// Off for the workspace says what removing it would bring back; with
    /// nothing beneath, off is just off.
    #[test]
    fn off_for_the_workspace_names_what_is_beneath() {
        let p = prepare(PrepareAt::Tokens(300_000), CompactMode::Native);
        p.apply(
            Sourced::new(None, Level::Workspace),
            Sourced::new(PrepareAt::Tokens(300_000), Level::Workspace),
        );
        assert_eq!(p.threshold(), None);
        assert_eq!(p.reading("main-a", 900_000, true), Crossing::Nothing);
        let user = Sourced::new(Some("update PLAN.md".to_string()), Level::User);
        assert!(
            p.status(None, &user).starts_with(
                "compact prepare: off for this workspace (your tui.toml has one: /compact prepare default uses it); its threshold is 300k tokens"
            ),
            "{}",
            p.status(None, &user)
        );
        let none = Sourced::new(None, Level::Default);
        assert!(
            p.status(None, &none)
                .starts_with("compact prepare: off for this workspace; its threshold"),
            "{}",
            p.status(None, &none)
        );
    }
}
