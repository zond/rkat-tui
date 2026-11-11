//! The agents' schedules (P6): the engine's schedule service, as the
//! daemon's runtime composes it, seen per agent.
//!
//! An agent creates a schedule with the engine's `meerkat_schedule_create`
//! on its own session (`"type": "current_session"`): the engine records it
//! as an identity target naming the agent's member in the daemon's mob
//! (`mob_member:{"mob_id": "tui", "member": "main-…"}`), so it follows the
//! agent across conversations and restarts, and the runtime's schedule
//! host fires it as a `member_send` of the action's prompt. This module
//!
//! - says which schedules are an agent's ([`agent_of`]) and lists them for
//!   `/schedules` ([`Schedules::rows`]), with the user's own pause, resume
//!   and delete ([`Schedules::act`]), no gate in between;
//! - deletes an agent's schedules when it retires ([`Schedules::delete_all`],
//!   the decision recorded in `docs/evidence.md`: nothing wakes a retired
//!   agent, so nothing is left to fire for it);
//! - attributes a run no user started to the schedule that fired it, on
//!   positive evidence only ([`Schedules::attribute`]): an occurrence of one
//!   of the agent's schedules, dispatched since the daemon started, not
//!   attributed before, whose prompt is the run's input;
//! - keeps what each schedule's runs cost in the realm's cost records,
//!   under the key `schedule-<id>` ([`Schedules::count_cost`], written behind by [`Schedules::write_cost`]).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use meerkat::{
    Occurrence, OccurrenceId, OccurrencePhase, Schedule, ScheduleDomainError, ScheduleId,
    SchedulePhase, ScheduleService, TargetBinding, TriggerSpec,
};
use meerkat_core::{RunId, Usage};

use super::MOB_ID;
use super::protocol::{ScheduleOp, ScheduleRow};

/// How often, and how long apart, a run start looks for the occurrence
/// that fired it: the store records the dispatch before the delivery
/// reaches the member, but the watcher may read before that write lands.
const ATTRIBUTE_TRIES: usize = 10;
const ATTRIBUTE_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// The agent a schedule targets: its identity target names a member of the
/// daemon's mob.
#[must_use]
pub fn agent_of(schedule: &Schedule) -> Option<String> {
    target_agent(&schedule.target)
}

fn target_agent(target: &TargetBinding) -> Option<String> {
    let TargetBinding::Identity(binding) = target else {
        return None;
    };
    member_of(binding.identity())
}

/// The member a `mob_member:` identity names in the daemon's mob.
fn member_of(identity: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Key {
        mob_id: String,
        member: String,
    }
    let json = identity.strip_prefix(meerkat::MOB_MEMBER_IDENTITY_TARGET_PREFIX)?;
    let key: Key = serde_json::from_str(json).ok()?;
    (key.mob_id == MOB_ID).then_some(key.member)
}

/// The prompt an occurrence delivers, as text.
fn prompt_of(target: &TargetBinding) -> Option<String> {
    let TargetBinding::Identity(binding) = target else {
        return None;
    };
    match binding.action() {
        meerkat::ScheduledSessionAction::Prompt { prompt, .. } => Some(prompt.text_content()),
        meerkat::ScheduledSessionAction::Event { .. } => None,
    }
}

/// A schedule's name, else its id.
#[must_use]
pub fn name_of(schedule: &Schedule) -> String {
    schedule
        .config
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| schedule.schedule_id.to_string())
}

/// When `trigger` fires, in a line.
#[must_use]
pub fn expression(trigger: &TriggerSpec) -> String {
    match trigger {
        TriggerSpec::Once { due_at_utc } => format!("once at {}", stamp(*due_at_utc)),
        TriggerSpec::Interval(interval) => {
            let mut text = format!(
                "every {}s from {}",
                interval.every_seconds,
                stamp(interval.start_at_utc)
            );
            if let Some(end) = interval.end_at_utc {
                text.push_str(&format!(" until {}", stamp(end)));
            }
            text
        }
        TriggerSpec::Calendar(calendar) => {
            let fields = serde_json::to_value(calendar).unwrap_or_default();
            format!("calendar {fields}")
        }
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%SZ").to_string()
}

fn phase_word(phase: SchedulePhase) -> &'static str {
    match phase {
        SchedulePhase::Active => "active",
        SchedulePhase::Paused => "paused",
        SchedulePhase::Deleted => "deleted",
    }
}

/// Why a schedule action failed.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    /// The engine's schedule service refused.
    #[error("{0}")]
    Service(#[from] ScheduleDomainError),
    /// No live schedule of the agent has that name or id.
    #[error("this agent has no schedule `{0}` (/schedules lists them)")]
    Unknown(String),
    /// Several of the agent's schedules have that name.
    #[error("several schedules are called `{0}`; name one by its id (/schedules shows them)")]
    Ambiguous(String),
}

/// A run attributed to the schedule that fired it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    /// The schedule.
    pub schedule_id: ScheduleId,
    /// Its name.
    pub name: String,
}

/// The agents' schedules.
pub struct Schedules {
    service: ScheduleService,
    /// Where the cost records live (the realm).
    realm: PathBuf,
    /// When the daemon started: an occurrence dispatched before was not
    /// this process's to attribute.
    started: DateTime<Utc>,
    /// The occurrences already attributed to a run.
    attributed: parking_lot::Mutex<HashSet<OccurrenceId>>,
    /// The runs attributed to a schedule.
    runs: parking_lot::Mutex<HashMap<RunId, ScheduleId>>,
    /// What each schedule's runs cost as counted (its record read on
    /// first use); the record is written behind ([`Self::write_cost`]).
    costs: parking_lot::Mutex<HashMap<ScheduleId, crate::ui::SessionCost>>,
    /// One record write at a time.
    writing: parking_lot::Mutex<()>,
}

impl std::fmt::Debug for Schedules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schedules")
            .field("realm", &self.realm)
            .finish_non_exhaustive()
    }
}

impl Schedules {
    /// The schedules of `service`, whose runs' costs are recorded in the
    /// realm at `realm`.
    #[must_use]
    pub fn new(service: ScheduleService, realm: PathBuf) -> Self {
        Self {
            service,
            realm,
            started: Utc::now(),
            attributed: parking_lot::Mutex::new(HashSet::new()),
            runs: parking_lot::Mutex::new(HashMap::new()),
            costs: parking_lot::Mutex::new(HashMap::new()),
            writing: parking_lot::Mutex::new(()),
        }
    }

    /// `agent`'s live schedules (active and paused), in creation order.
    ///
    /// # Errors
    ///
    /// The service's refusal.
    pub async fn of_agent(&self, agent: &str) -> Result<Vec<Schedule>, ScheduleError> {
        Ok(self
            .service
            .list()
            .await?
            .into_iter()
            .filter(|s| s.phase != SchedulePhase::Deleted)
            .filter(|s| agent_of(s).as_deref() == Some(agent))
            .collect())
    }

    /// How many live schedules each agent has.
    pub async fn counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        match self.service.list().await {
            Ok(schedules) => {
                for schedule in schedules {
                    if schedule.phase != SchedulePhase::Deleted
                        && let Some(agent) = agent_of(&schedule)
                    {
                        *counts.entry(agent).or_insert(0) += 1;
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "cannot list the schedules"),
        }
        counts
    }

    /// `/schedules`: a row per live schedule of `agent`.
    ///
    /// # Errors
    ///
    /// The service's refusal.
    pub async fn rows(&self, agent: &str) -> Result<Vec<ScheduleRow>, ScheduleError> {
        let mut rows = Vec::new();
        for schedule in self.of_agent(agent).await? {
            let occurrences = self.service.list_occurrences(&schedule.schedule_id).await?;
            let next = occurrences
                .iter()
                .filter(|o| o.phase == OccurrencePhase::Pending)
                .map(|o| o.due_at_utc)
                .min();
            let fired = occurrences.iter().filter(|o| delivered(o)).count();
            rows.push(ScheduleRow {
                id: schedule.schedule_id.to_string(),
                name: name_of(&schedule),
                expression: expression(&schedule.trigger),
                next: next.map(stamp),
                state: phase_word(schedule.phase).to_string(),
                fired,
                cost: self.cost_of(&schedule.schedule_id),
            });
        }
        Ok(rows)
    }

    /// The user's own `/schedules pause|resume|delete <name>` on one of
    /// `agent`'s schedules. Returns what was done.
    ///
    /// # Errors
    ///
    /// [`ScheduleError`].
    pub async fn act(
        &self,
        agent: &str,
        op: ScheduleOp,
        name: &str,
    ) -> Result<String, ScheduleError> {
        let schedule = self.find(agent, name).await?;
        let id = &schedule.schedule_id;
        let label = name_of(&schedule);
        match op {
            ScheduleOp::Pause => {
                self.service.pause(id).await?;
                Ok(format!("paused schedule {label}"))
            }
            ScheduleOp::Resume => {
                self.service.resume(id).await?;
                Ok(format!("resumed schedule {label}"))
            }
            ScheduleOp::Delete => {
                self.service.delete(id).await?;
                Ok(format!("deleted schedule {label}"))
            }
        }
    }

    async fn find(&self, agent: &str, name: &str) -> Result<Schedule, ScheduleError> {
        let mut matching: Vec<Schedule> = self
            .of_agent(agent)
            .await?
            .into_iter()
            .filter(|s| s.schedule_id.to_string() == name || name_of(s) == name)
            .collect();
        match matching.len() {
            0 => Err(ScheduleError::Unknown(name.to_string())),
            1 => Ok(matching.remove(0)),
            _ => Err(ScheduleError::Ambiguous(name.to_string())),
        }
    }

    /// Delete every live schedule of `agent` (it retires). Returns the
    /// names of those deleted; one the service refuses to delete is logged
    /// and left out.
    pub async fn delete_all(&self, agent: &str) -> Vec<String> {
        let schedules = match self.of_agent(agent).await {
            Ok(schedules) => schedules,
            Err(err) => {
                tracing::warn!(%agent, error = %err, "cannot list the retiring agent's schedules");
                return Vec::new();
            }
        };
        let mut deleted = Vec::new();
        for schedule in schedules {
            match self.service.delete(&schedule.schedule_id).await {
                Ok(_) => deleted.push(name_of(&schedule)),
                Err(err) => {
                    tracing::warn!(%agent, schedule = %schedule.schedule_id, error = %err, "a retiring agent's schedule was not deleted");
                }
            }
        }
        deleted
    }

    /// The schedule that fired `agent`'s run `run` with input `prompt`, on
    /// positive evidence (see the module docs); `None` when no occurrence
    /// proves it. Tried for about a second while nothing matches.
    pub async fn attribute(&self, agent: &str, run: &RunId, prompt: &str) -> Option<Fired> {
        // An agent with no schedule (most runs nobody typed are a peer's)
        // costs one read, not a second of the event watcher's time.
        let any = self.service.list().await.ok()?;
        if !any.iter().any(|s| agent_of(s).as_deref() == Some(agent)) {
            return None;
        }
        for attempt in 0..ATTRIBUTE_TRIES {
            if attempt > 0 {
                tokio::time::sleep(ATTRIBUTE_PAUSE).await;
            }
            if let Some(fired) = self.attribute_once(agent, run, prompt).await {
                return Some(fired);
            }
        }
        None
    }

    async fn attribute_once(&self, agent: &str, run: &RunId, prompt: &str) -> Option<Fired> {
        let schedules = self.service.list().await.ok()?;
        for schedule in schedules {
            if agent_of(&schedule).as_deref() != Some(agent) {
                continue;
            }
            let Ok(occurrences) = self.service.list_occurrences(&schedule.schedule_id).await else {
                continue;
            };
            let mut candidates: Vec<&Occurrence> = occurrences
                .iter()
                .filter(|o| o.dispatched_at_utc.is_some_and(|at| at >= self.started))
                .filter(|o| prompt_of(&o.target_snapshot).as_deref() == Some(prompt))
                .collect();
            candidates.sort_by_key(|o| o.dispatched_at_utc);
            let mut attributed = self.attributed.lock();
            if let Some(occurrence) = candidates
                .into_iter()
                .find(|o| !attributed.contains(&o.occurrence_id))
            {
                attributed.insert(occurrence.occurrence_id.clone());
                self.runs
                    .lock()
                    .insert(run.clone(), schedule.schedule_id.clone());
                return Some(Fired {
                    schedule_id: schedule.schedule_id.clone(),
                    name: name_of(&schedule),
                });
            }
        }
        None
    }

    /// The schedule that fired `run`, if one did.
    #[must_use]
    pub fn schedule_of(&self, run: &RunId) -> Option<ScheduleId> {
        self.runs.lock().get(run).cloned()
    }

    /// Count `usage` on `model` to `schedule`, in memory (its record is
    /// read once, on first use); [`Self::write_cost`] writes it.
    pub fn count_cost(&self, schedule: &ScheduleId, model: &str, usage: &Usage) {
        let mut costs = self.costs.lock();
        let cost = costs.entry(schedule.clone()).or_insert_with(|| {
            crate::workbench::cost_store::load_cost(&self.realm, &cost_key(schedule))
                .unwrap_or_default()
        });
        cost.record(model, usage);
    }

    /// Write `schedule`'s cost record as counted now (blocking file I/O;
    /// one write at a time, each of the latest count).
    pub fn write_cost(&self, schedule: &ScheduleId) {
        let _one = self.writing.lock();
        let Some(cost) = self.costs.lock().get(schedule).cloned() else {
            return;
        };
        if let Err(err) =
            crate::workbench::cost_store::save_cost(&self.realm, &cost_key(schedule), &cost)
        {
            tracing::warn!(%schedule, error = %err, "a scheduled run's cost was not recorded");
        }
    }

    /// `schedule`'s cost as counted, else as recorded.
    fn cost_of(&self, schedule: &ScheduleId) -> Option<crate::ui::SessionCost> {
        if let Some(cost) = self.costs.lock().get(schedule) {
            return Some(cost.clone());
        }
        crate::workbench::cost_store::load_cost(&self.realm, &cost_key(schedule))
    }
}

/// Whether an occurrence was delivered to its target.
fn delivered(occurrence: &Occurrence) -> bool {
    matches!(
        occurrence.phase,
        OccurrencePhase::Completed | OccurrencePhase::AwaitingCompletion
    ) && occurrence.dispatched_at_utc.is_some()
}

/// The cost record a schedule's runs are kept under.
fn cost_key(schedule: &ScheduleId) -> String {
    format!("schedule-{schedule}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_member_identity_of_the_daemon_s_mob_names_its_agent() {
        let identity = |mob: &str| {
            format!(
                "{}{{\"schema\":\"meerkat.schedule.mob_member_identity.v2\",\"mob_id\":\"{mob}\",\"member\":\"main-ab12\"}}",
                meerkat::MOB_MEMBER_IDENTITY_TARGET_PREFIX
            )
        };
        assert_eq!(member_of(&identity(MOB_ID)).as_deref(), Some("main-ab12"));
        assert_eq!(member_of(&identity("other")), None);
        assert_eq!(member_of("session:xyz"), None);
    }

    #[test]
    fn triggers_read_as_a_line() {
        let at = DateTime::parse_from_rfc3339("2026-11-10T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            expression(&TriggerSpec::Once { due_at_utc: at }),
            "once at 2026-11-10 09:00:00Z"
        );
        assert_eq!(
            expression(&TriggerSpec::Interval(meerkat::IntervalTriggerSpec {
                start_at_utc: at,
                every_seconds: 600,
                end_at_utc: None,
            })),
            "every 600s from 2026-11-10 09:00:00Z"
        );
    }
}
