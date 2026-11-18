//! This build's own instructions on a conversation an earlier build began.
//!
//! An agent's instructions are written into its conversation's system
//! prompt once, when it is first seated; a helper's likewise. The engine
//! keeps that prompt row as it is across every later seat (a resume writes
//! no prompt), so after an upgrade an agent restored on an old
//! conversation keeps the old build's sentences about its tools and
//! helpers, and acts on them.
//!
//! On a seat on a stored conversation (a restore at start, `/resume`, an
//! attach to a conversation whose agent quit) and on a restored helper, the
//! daemon reads the conversation's prompt row: when it holds this build's
//! own sections word for word ([`agent_sections`], [`helper_sections`]),
//! nothing is done. Else the row is replaced through the engine's keyed
//! system-prompt update ([`meerkat_core::SystemPromptUpdateRequest`]): the
//! first one adopts the unkeyed row the first seat wrote, a later one
//! supersedes the latest version of [`PROMPT_KEY`]. The model and a
//! compaction read only the latest version of a key, so the old text does
//! not stay beside the new; the rest of the row (the base prompt, the
//! instruction files, the skills, the tools' instructions) is kept as it
//! was. The prompt row itself is the record of the revision: no other
//! state is stored, and a row with none of this build's text is older.
//!
//! The update needs the session idle; one refused as busy is made again
//! when its run ends ([`Instructions::run_ended`]).

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use meerkat_core::{Message, SessionId};
use meerkat_rpc::session_runtime::SessionRuntime;

use super::audit::{AuditLog, Line};

/// The key of the replaceable prompt slot this module writes.
pub const PROMPT_KEY: &str = "rkat-tui-instructions";

/// What the agent's clients are told once its instructions were replaced.
pub const UPGRADED_NOTICE: &str = "the agent's instructions were brought up to this build's";

/// The audit event of a replacement.
const AUDIT_EVENT: &str = "instructions_updated";

/// The start of the scratch-directory line
/// ([`crate::mobs::customizer::scratch_instruction`]): a section of its own.
const SCRATCH_LINE: &str = "Your scratch directory is ";

/// The start of the helper-models line
/// ([`crate::config::helper_models::HelperModels::instruction`]).
const MODELS_LINE: &str = "Helper models:";

/// The sections of an agent's instructions that are this build's own text
/// (the instruction files, the notes index and the helper models are the
/// workspace's and the settings', and stay as the conversation began);
/// `memory`: the realm's semantic memory is on.
#[must_use]
pub fn agent_sections(name: &str, memory: bool) -> Vec<String> {
    vec![
        crate::bootstrap::persona::CODING_PERSONA.to_string(),
        crate::bootstrap::persona::agents_persona(name),
        super::deferral::PERSONA.to_string(),
        crate::bootstrap::persona::compaction_persona(memory, false),
    ]
}

/// The sections of a helper's instructions that are this build's own text;
/// `memory`: the realm's semantic memory is on.
#[must_use]
pub fn helper_sections(memory: bool) -> Vec<String> {
    vec![
        crate::bootstrap::persona::HELPER_PERSONA.to_string(),
        crate::bootstrap::persona::compaction_persona(memory, true),
    ]
}

/// Tests: `section` as an earlier build might have written it (its
/// heading, then a sentence this build does not have).
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn stale(section: &str) -> String {
    match section.split_once('\n') {
        Some((heading, rest)) => {
            format!("{heading}\n\nOLD-BUILD-LINE: what an earlier build said here.\n{rest}")
        }
        None => format!("{section}\nOLD-BUILD-LINE\n"),
    }
}

/// Whether `line` begins a section that is not one of this build's own:
/// where an earlier build's section ends in a prompt.
fn begins_other(line: &str) -> bool {
    line.starts_with("# ")
        || line.starts_with('<')
        || line.starts_with(SCRATCH_LINE)
        || line.starts_with(MODELS_LINE)
}

/// The byte range in `prompt` of the section headed by `heading` (a line
/// of its own): from the heading to the next line that begins another
/// section, else the end.
fn span_of(prompt: &str, heading: &str) -> Option<std::ops::Range<usize>> {
    let mut offset = 0;
    let mut start = None;
    for line in prompt.split_inclusive('\n') {
        let text = line.trim_end_matches('\n');
        match start {
            None if text == heading => start = Some(offset),
            Some(start) if begins_other(text) => return Some(start..offset),
            _ => {}
        }
        offset += line.len();
    }
    start.map(|start| start..prompt.len())
}

/// `prompt` with this build's `sections` in it, or `None` when it holds
/// each of them word for word already.
///
/// A section is known by its heading (its first line): an earlier build's
/// section under the same heading is replaced, the first one found by all
/// of `sections` in their order and the others removed. A prompt holding
/// none of the headings gets the sections at its end.
#[must_use]
pub fn brought_up(prompt: &str, sections: &[String]) -> Option<String> {
    if sections
        .iter()
        .all(|section| prompt.contains(section.trim_end()))
    {
        return None;
    }
    let joined = sections
        .iter()
        .map(|section| section.trim_end())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut spans: Vec<std::ops::Range<usize>> = sections
        .iter()
        .filter_map(|section| section.lines().next())
        .filter_map(|heading| span_of(prompt, heading))
        .collect();
    spans.sort_by_key(|span| span.start);
    spans.dedup();
    let Some(first) = spans.first().cloned() else {
        return Some(format!("{}\n\n{joined}\n", prompt.trim_end()));
    };
    let mut out = String::with_capacity(prompt.len() + joined.len());
    out.push_str(&prompt[..first.start]);
    out.push_str(&joined);
    out.push_str(if first.end < prompt.len() {
        "\n\n"
    } else {
        "\n"
    });
    let mut at = first.end;
    for span in &spans[1..] {
        if span.start >= at {
            out.push_str(&prompt[at..span.start]);
            at = span.end;
        }
    }
    out.push_str(&prompt[at..]);
    Some(out)
}

/// The prompt row of `messages` that holds the conversation's
/// instructions: the latest version of [`PROMPT_KEY`], else the first
/// plain System row naming one of `sections`' headings, else the first
/// plain System row. With its index and keyed version.
fn target<'a>(
    messages: &'a [Message],
    sections: &[String],
) -> Option<(usize, &'a str, Option<meerkat_core::SystemPromptVersion>)> {
    let keyed = messages.iter().enumerate().rev().find_map(|(index, m)| {
        let Message::System(system) = m else {
            return None;
        };
        let version = system.prompt_version.as_ref()?;
        (version.key.as_str() == PROMPT_KEY).then_some((
            index,
            system.content.as_str(),
            Some(version.version),
        ))
    });
    if keyed.is_some() {
        return keyed;
    }
    let plain: Vec<(usize, &str)> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, m)| match m {
            Message::System(system)
                if system.identity.is_none()
                    && system.prompt_version.is_none()
                    && system.instruction_activation.is_none()
                    && system.instruction_entry.is_none() =>
            {
                Some((index, system.content.as_str()))
            }
            _ => None,
        })
        .collect();
    let headings: Vec<&str> = sections.iter().filter_map(|s| s.lines().next()).collect();
    plain
        .iter()
        .find(|(_, content)| {
            headings
                .iter()
                .any(|heading| span_of(content, heading).is_some())
        })
        .or_else(|| plain.first())
        .map(|(index, content)| (*index, *content, None))
}

/// What a bring-up did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The conversation's instructions are this build's: nothing was sent.
    Current,
    /// They were replaced now.
    Updated,
    /// The session was busy: they are replaced when its run ends.
    Waiting,
    /// The conversation has no prompt row to bring up, or it could not be
    /// read or written (logged).
    Failed(String),
}

/// Who a conversation's instructions are for (the audit line names it).
#[derive(Debug, Clone)]
pub struct Whose {
    /// The agent, or the helper's label.
    pub name: String,
    /// The agent the audit line is of (`None`: a helper's, a line of the
    /// workspace's).
    pub agent: Option<String>,
    /// The sections this build gives it.
    pub sections: Vec<String>,
}

/// The daemon's bring-ups.
pub struct Instructions {
    runtime: Weak<SessionRuntime>,
    audit: parking_lot::Mutex<Option<Arc<AuditLog>>>,
    /// The conversations whose bring-up waits for their run's end.
    waiting: parking_lot::Mutex<HashMap<SessionId, Whose>>,
    /// Tells the agents' side of a bring-up made at a run's end.
    on_updated: parking_lot::Mutex<Option<OnUpdated>>,
    /// Tests: the agents and helpers are seated with [`stale`] sections.
    #[cfg(any(test, feature = "test-support"))]
    stale: bool,
}

/// Called with the conversation and whose it is when a waiting bring-up
/// was made.
pub type OnUpdated = Arc<dyn Fn(&SessionId, &Whose) + Send + Sync>;

impl std::fmt::Debug for Instructions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instructions").finish_non_exhaustive()
    }
}

impl Instructions {
    /// Over `runtime`'s sessions.
    #[must_use]
    pub fn new(runtime: &Arc<SessionRuntime>) -> Arc<Self> {
        Arc::new(Self {
            runtime: Arc::downgrade(runtime),
            audit: parking_lot::Mutex::new(None),
            waiting: parking_lot::Mutex::new(HashMap::new()),
            on_updated: parking_lot::Mutex::new(None),
            #[cfg(any(test, feature = "test-support"))]
            stale: false,
        })
    }

    /// Tests: [`Self::new`], seating with [`stale`] sections.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn stale(runtime: &Arc<SessionRuntime>) -> Arc<Self> {
        Arc::new(Self {
            runtime: Arc::downgrade(runtime),
            audit: parking_lot::Mutex::new(None),
            waiting: parking_lot::Mutex::new(HashMap::new()),
            on_updated: parking_lot::Mutex::new(None),
            stale: true,
        })
    }

    /// `section` as this daemon seats it (tests: [`stale`]).
    #[must_use]
    pub fn seated(&self, section: String) -> String {
        #[cfg(any(test, feature = "test-support"))]
        if self.stale {
            return stale(&section);
        }
        section
    }

    /// Write the audit lines to `audit`.
    pub fn set_audit(&self, audit: Option<Arc<AuditLog>>) {
        *self.audit.lock() = audit;
    }

    /// Call `on_updated` when a waiting bring-up was made.
    pub fn on_updated(&self, on_updated: OnUpdated) {
        *self.on_updated.lock() = Some(on_updated);
    }

    /// Whether `messages` need a bring-up for `sections`.
    #[must_use]
    pub fn needed(messages: &[Message], sections: &[String]) -> bool {
        target(messages, sections)
            .is_some_and(|(_, prompt, _)| brought_up(prompt, sections).is_some())
    }

    /// Whether `session`'s stored conversation needs a bring-up for
    /// `whose` (unreadable: no).
    pub async fn needs(&self, session: &SessionId, whose: &Whose) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if self.stale {
            return false;
        }
        match self.messages(session).await {
            Ok(messages) => Self::needed(&messages, &whose.sections),
            Err(err) => {
                tracing::warn!(%session, error = %err, "cannot read a conversation's instructions");
                false
            }
        }
    }

    async fn messages(&self, session: &SessionId) -> Result<Vec<Message>, String> {
        let runtime = self.runtime.upgrade().ok_or("the daemon stops")?;
        runtime
            .session_service()
            .read_history(
                session,
                meerkat_core::SessionHistoryQuery {
                    offset: 0,
                    limit: None,
                },
            )
            .await
            .map(|page| page.messages)
            .map_err(|err| err.to_string())
    }

    /// Bring `session`'s instructions up to `whose`'s sections: the session
    /// is live (woken) and idle, or the update waits for its run's end.
    pub async fn bring_up(&self, session: &SessionId, whose: &Whose) -> Outcome {
        #[cfg(any(test, feature = "test-support"))]
        if self.stale {
            return Outcome::Current;
        }
        let messages = match self.messages(session).await {
            Ok(messages) => messages,
            Err(err) => return self.failed(session, whose, err),
        };
        let Some((index, prompt, version)) = target(&messages, &whose.sections) else {
            return self.failed(session, whose, "it has no system prompt".to_string());
        };
        let Some(content) = brought_up(prompt, &whose.sections) else {
            self.waiting.lock().remove(session);
            return Outcome::Current;
        };
        let Some(runtime) = self.runtime.upgrade() else {
            return Outcome::Failed("the daemon stops".to_string());
        };
        let Ok(key) = meerkat_core::SystemPromptKey::new(PROMPT_KEY) else {
            return Outcome::Failed("the prompt key is empty".to_string());
        };
        let request = meerkat_core::SystemPromptUpdateRequest {
            key,
            expected_version: version,
            target_message_index: version.is_none().then_some(index),
            content,
            actor: Some("rkat-tui".to_string()),
            expected_parent_revision: None,
        };
        match runtime.update_system_prompt(session, request).await {
            Ok(result) => {
                self.waiting.lock().remove(session);
                tracing::info!(%session, name = %whose.name, version = result.version.get(), "brought a conversation's instructions up to this build's");
                if let Some(audit) = self.audit.lock().as_ref() {
                    let mut line = Line::event(
                        AUDIT_EVENT,
                        format!(
                            "the instructions of `{}` were brought up to this build's (prompt version {})",
                            whose.name,
                            result.version.get()
                        ),
                    );
                    line.agent.clone_from(&whose.agent);
                    line.session = Some(session.to_string());
                    audit.write(&line);
                }
                Outcome::Updated
            }
            Err(err) if err.code == meerkat_rpc::error::SESSION_BUSY => {
                tracing::info!(%session, name = %whose.name, "a conversation's instructions are brought up when its run ends");
                self.waiting.lock().insert(session.clone(), whose.clone());
                Outcome::Waiting
            }
            Err(err) => self.failed(session, whose, err.message),
        }
    }

    fn failed(&self, session: &SessionId, whose: &Whose, why: String) -> Outcome {
        self.waiting.lock().remove(session);
        tracing::warn!(%session, name = %whose.name, %why, "cannot bring a conversation's instructions up to this build's");
        Outcome::Failed(why)
    }

    /// Whether a bring-up of `session` waits for its run's end.
    #[must_use]
    pub fn is_waiting(&self, session: &SessionId) -> bool {
        self.waiting.lock().contains_key(session)
    }

    /// Make `session`'s bring-up for `whose` when its run ends (it runs
    /// now).
    pub fn wait(&self, session: &SessionId, whose: Whose) {
        self.waiting.lock().insert(session.clone(), whose);
    }

    /// `session`'s run ended: a bring-up waiting for it is made now.
    pub async fn run_ended(&self, session: &SessionId) {
        let Some(whose) = self.waiting.lock().get(session).cloned() else {
            return;
        };
        if self.bring_up(session, &whose).await == Outcome::Updated {
            let on_updated = self.on_updated.lock().clone();
            if let Some(on_updated) = on_updated {
                on_updated(session, &whose);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn prompt(sections: &[String]) -> String {
        format!(
            "You are a base prompt.\n\n# Instructions (from AGENTS.md)\n\nrules\n\n{}\n\nHelper models: on your own.\n\n# Inter-agent Communication\n\ntools\n",
            sections.join("\n\n")
        )
    }

    /// No section of this build holds a line that would end it early.
    #[test]
    fn a_section_of_this_build_is_one_span() {
        let mut all = agent_sections("main-ab12", true);
        all.extend(helper_sections(true));
        for section in &all {
            let heading = section.lines().next().unwrap();
            assert!(heading.starts_with("# "), "{heading}");
            let span = span_of(section, heading).unwrap();
            assert_eq!(span, 0..section.len(), "{section}");
        }
        // The lines that end one are what the other sections begin with.
        let scratch = crate::mobs::customizer::scratch_instruction(std::path::Path::new("/s"));
        assert!(scratch.starts_with(SCRATCH_LINE), "{scratch}");
        let models = crate::config::helper_models::HelperModels::default().instruction();
        assert!(models.starts_with(MODELS_LINE), "{models}");
    }

    #[test]
    fn a_prompt_with_this_build_s_sections_is_left_alone() {
        let sections = agent_sections("main-ab12", true);
        assert_eq!(brought_up(&prompt(&sections), &sections), None);
    }

    /// An earlier build's sections are replaced in place: the rest of the
    /// prompt stays, the old sentences go, the new ones are there once.
    #[test]
    fn an_older_build_s_sections_are_replaced_in_place() {
        let sections = agent_sections("main-ab12", true);
        let old: Vec<String> = sections.iter().map(|s| stale(s)).collect();
        let new = brought_up(&prompt(&old), &sections).unwrap();
        assert_eq!(new, prompt(&sections).replace("\n\n\n", "\n\n"));
        assert!(!new.contains("OLD-BUILD-LINE"), "{new}");
        assert_eq!(brought_up(&new, &sections), None);
    }

    /// A section an earlier build did not have joins the ones it had; one
    /// with no heading of this build's gets them at its end.
    #[test]
    fn missing_sections_are_added() {
        let sections = agent_sections("main-ab12", true);
        let old = vec![stale(&sections[0]), stale(&sections[1])];
        let new = brought_up(&prompt(&old), &sections).unwrap();
        for section in &sections {
            assert_eq!(new.matches(section.trim_end()).count(), 1, "{new}");
        }
        assert!(new.contains("Helper models: on your own."), "{new}");
        let bare = "You are a base prompt.\n";
        let new = brought_up(bare, &helper_sections(true)).unwrap();
        assert!(
            new.starts_with("You are a base prompt.\n\n# Working as a helper"),
            "{new}"
        );
    }

    fn system(content: &str) -> Message {
        Message::System(meerkat_core::types::SystemMessage::new(content))
    }

    /// The row brought up is the one holding the instructions, not a later
    /// `/remember` row.
    #[test]
    fn the_target_is_the_row_with_the_instructions() {
        let sections = helper_sections(true);
        let messages = vec![
            system("base\n\n# Working as a helper\n\nold\n"),
            system("# Remembered instruction\n\nx"),
        ];
        let (index, _, version) = target(&messages, &sections).unwrap();
        assert_eq!((index, version), (0, None));
        assert!(Instructions::needed(&messages, &sections));
    }
}
