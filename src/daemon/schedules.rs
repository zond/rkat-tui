//! The agents' schedules: the engine's schedule service, as the
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
//! - shows what each schedule's runs used: the event watcher counts a
//!   scheduled run's model calls to its schedule as well as to its
//!   conversation ([`super::usage::Account::Schedule`]), and drops the
//!   record of a schedule that is gone ([`Schedules::reconcile`]: at the
//!   daemon's start, after the agent's own `meerkat_schedule_delete`, and
//!   when `/schedules` lists them).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use meerkat::{
    Occurrence, OccurrenceId, OccurrencePhase, Schedule, ScheduleDomainError, ScheduleId,
    SchedulePhase, ScheduleService, TargetBinding, TriggerSpec,
};
use meerkat_core::RunId;

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
    /// Where each schedule's runs are counted ([`super::usage::Account::Schedule`]).
    ledger: Arc<super::usage::Ledger>,
    /// When the daemon started: an occurrence dispatched before was not
    /// this process's to attribute.
    started: DateTime<Utc>,
    /// The occurrences already attributed to a run, oldest first (at
    /// most [`MAX_ATTRIBUTED`]).
    attributed: parking_lot::Mutex<Attributed>,
    /// The runs attributed to a schedule, until each ends
    /// ([`Self::run_ended`]).
    runs: parking_lot::Mutex<HashMap<RunId, ScheduleId>>,
}

impl std::fmt::Debug for Schedules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schedules").finish_non_exhaustive()
    }
}

impl Schedules {
    /// The schedules of `service`, whose runs are counted in `ledger`.
    #[must_use]
    pub fn new(service: ScheduleService, ledger: Arc<super::usage::Ledger>) -> Self {
        Self {
            service,
            ledger,
            started: Utc::now(),
            attributed: parking_lot::Mutex::new(Attributed::default()),
            runs: parking_lot::Mutex::new(HashMap::new()),
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
                cost: Some(
                    self.ledger
                        .get(&super::usage::Account::Schedule(
                            schedule.schedule_id.clone(),
                        ))
                        .await
                        .cost,
                )
                .filter(|cost| !cost.is_empty()),
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
                self.ledger
                    .forget(&super::usage::Account::Schedule(id.clone()))
                    .await;
                Ok(format!("deleted schedule {label}"))
            }
        }
    }

    /// Drop the usage record of every schedule that no longer lives: one
    /// the agent deleted with its own tools (the daemon is not told of
    /// it otherwise), or one deleted while the daemon was down. Decided
    /// on the service's list of live schedules; when that cannot be read
    /// nothing is dropped.
    pub async fn reconcile(&self) {
        let live: HashSet<ScheduleId> = match self.service.list().await {
            Ok(schedules) => schedules
                .into_iter()
                .filter(|s| s.phase != SchedulePhase::Deleted)
                .map(|s| s.schedule_id)
                .collect(),
            Err(err) => {
                tracing::warn!(error = %err, "cannot list the schedules: their usage records stay");
                return;
            }
        };
        for id in self.ledger.schedule_accounts().await {
            if !live.contains(&id) {
                tracing::info!(schedule = %id, "a deleted schedule's usage record is dropped");
                self.ledger
                    .forget(&super::usage::Account::Schedule(id))
                    .await;
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
                Ok(_) => {
                    self.ledger
                        .forget(&super::usage::Account::Schedule(
                            schedule.schedule_id.clone(),
                        ))
                        .await;
                    deleted.push(name_of(&schedule));
                }
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
            let mut attributed = self.attributed.lock();
            let mut candidates: Vec<&Occurrence> = occurrences
                .iter()
                .filter(|o| {
                    o.dispatched_at_utc
                        .is_some_and(|at| at >= self.started && attributed.after_forgotten(at))
                })
                .filter(|o| prompt_of(&o.target_snapshot).as_deref() == Some(prompt))
                .collect();
            candidates.sort_by_key(|o| o.dispatched_at_utc);
            if let Some(occurrence) = candidates
                .into_iter()
                .find(|o| !attributed.contains(&o.occurrence_id))
            {
                attributed.insert(
                    occurrence.occurrence_id.clone(),
                    occurrence.dispatched_at_utc,
                );
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

    /// `run` ended: its attribution is not needed any more.
    pub fn run_ended(&self, run: &RunId) {
        self.runs.lock().remove(run);
    }

    /// The schedule that fired `run`, if one did.
    #[must_use]
    pub fn schedule_of(&self, run: &RunId) -> Option<ScheduleId> {
        self.runs.lock().get(run).cloned()
    }
}

/// How many attributed occurrences are remembered.
pub const MAX_ATTRIBUTED: usize = 1024;

/// The occurrences attributed to a run, oldest first. Past
/// [`MAX_ATTRIBUTED`] the oldest are forgotten, and with them every
/// occurrence dispatched at or before the newest forgotten one: such an
/// occurrence is never attributed again, so a forgotten one cannot be
/// taken for a new run's.
#[derive(Debug, Default)]
struct Attributed {
    ids: HashSet<OccurrenceId>,
    order: std::collections::VecDeque<(OccurrenceId, Option<chrono::DateTime<chrono::Utc>>)>,
    forgotten_until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Attributed {
    fn contains(&self, id: &OccurrenceId) -> bool {
        self.ids.contains(id)
    }

    /// Whether an occurrence dispatched `at` is newer than every forgotten
    /// one.
    fn after_forgotten(&self, at: chrono::DateTime<chrono::Utc>) -> bool {
        self.forgotten_until.is_none_or(|until| at > until)
    }

    fn insert(&mut self, id: OccurrenceId, at: Option<chrono::DateTime<chrono::Utc>>) {
        if !self.ids.insert(id.clone()) {
            return;
        }
        self.order.push_back((id, at));
        while self.order.len() > MAX_ATTRIBUTED {
            if let Some((id, at)) = self.order.pop_front() {
                self.ids.remove(&id);
                self.forgotten_until = self.forgotten_until.max(at);
            }
        }
    }
}

/// Whether an occurrence was delivered to its target.
fn delivered(occurrence: &Occurrence) -> bool {
    matches!(
        occurrence.phase,
        OccurrencePhase::Completed | OccurrencePhase::AwaitingCompletion
    ) && occurrence.dispatched_at_utc.is_some()
}

#[cfg(test)]
mod attributed_tests {
    use super::*;

    /// Past the bound the oldest attributed occurrences are forgotten, and
    /// so is every occurrence dispatched no later than they were: none of
    /// them can be attributed to a new run again.
    #[test]
    fn forgotten_occurrences_are_never_attributed_again() {
        let mut attributed = Attributed::default();
        let base = Utc::now();
        let ids: Vec<OccurrenceId> = (0..MAX_ATTRIBUTED + 2)
            .map(|_| OccurrenceId::new())
            .collect();
        for (n, id) in ids.iter().enumerate() {
            let at = base + chrono::Duration::seconds(i64::try_from(n).unwrap_or(i64::MAX));
            attributed.insert(id.clone(), Some(at));
        }
        assert_eq!(attributed.ids.len(), MAX_ATTRIBUTED);
        assert!(!attributed.contains(&ids[0]));
        assert!(!attributed.after_forgotten(base));
        assert!(!attributed.after_forgotten(base + chrono::Duration::seconds(1)));
        assert!(attributed.after_forgotten(base + chrono::Duration::seconds(2)));
        assert!(attributed.contains(&ids[MAX_ATTRIBUTED + 1]));
    }
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
