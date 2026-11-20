//! The user's messages into one agent's open run: at most one of them in
//! flight in the engine, the rest held here and sent as one.
//!
//! The engine stages one steer per model request, oldest first
//! (`docs/evidence.md`, "When a typed message goes in"): three messages
//! sent while one model call runs would ride the next three requests. So
//! the first goes to the engine at once and the ones typed after it, while
//! it has not been written into a request, wait here; when it is written
//! (its `BoundaryAppendApplied`), they go as one steer, which rides the
//! request after it together. A held message has not reached the engine,
//! so taking it back is free; the one in flight can only be recalled from
//! the engine, which costs its request (`docs/evidence.md`, "A recalled
//! steer's slot").
//!
//! # States, per agent
//!
//! - [`UserSteers::NoneInFlight`]: no message of the user's waits in the
//!   engine for this run's next request.
//! - [`UserSteers::InFlight`]: one does ([`InFlight::owner`]: the client's
//!   send, or the daemon's send of held messages), under the input the
//!   engine admitted it as once its answer came ([`InFlight::input`]), and
//!   the messages typed since are held ([`InFlight::held`], oldest
//!   first).
//!
//! # Transitions
//!
//! - `NoneInFlight` → `InFlight` (nothing held): a client's steer is
//!   forwarded into the open run ([`Steers::admit`] answers
//!   [`Admit::Forward`]).
//! - `InFlight` → `InFlight` (one more held): a client's steer while one
//!   is in flight ([`Admit::Held`]: answered at once with an id of the
//!   daemon's, [`Held::id`]).
//! - `InFlight` with no input → the same with its input: the engine
//!   answered the send ([`Steers::answered`]); an input the run had
//!   already written ([`InFlight::seen`]) counts as applied at once.
//! - `InFlight` → `NoneInFlight` or a new `InFlight` of the held ones: the
//!   input in flight was written into a request ([`Steers::applied`]), a
//!   run started with it in its input ([`Steers::run_started`]), the
//!   engine refused its send, or it was taken back
//!   ([`Steers::settled`]). With messages held they go now as one
//!   ([`Release`]), the new one in flight.
//! - any → `NoneInFlight`: the run ended (no run of the agent open,
//!   [`Steers::run_ended`]): what is held goes as one input that starts
//!   the next run.
//! - a held message leaves: taken back by its client
//!   ([`Steers::take_back`], a recall or an interrupt), or dropped when
//!   the agent is seated again or retires ([`Steers::drop_all`]).
//!
//! "The run is open" is the daemon's reading: a run whose start it read
//! and whose end it did not, or an input of the user's it forwarded to
//! the idle agent since (that input starts a run whose start may not be
//! read yet: [`super::Activity::user_steer`]). A steer forwarded to an
//! agent the daemon reads as idle is not in flight: it starts a run, and
//! what is sent before that run begins joins its first request.
//!
//! The daemon's own notes ([`super::HeldNote`]) never take the slot of a
//! user's message: they go with a message of the user's (in front of its
//! words), and are steered on their own at a tool request only while no
//! message of the user's is in flight or held ([`Steers::is_quiet`]). A
//! peer's message is the engine's: it can take one engine slot between
//! two of the user's, no more (the held ones go as one).
//!
//! Each held message keeps its own id to the client until it is sent;
//! then the client follows it by the input it went as
//! ([`crate::daemon::protocol::HeldNow::Released`]), and the daemon still
//! knows it by its own id ([`Steers::alias`]): a recall, a question of its
//! fate and an interrupt may name it either way, and its delivered mark
//! comes when that input is written.

use meerkat_core::lifecycle::InputId;

/// How many sent held messages are remembered by their own id.
const ALIASES_KEPT: usize = 128;
/// How many inputs written before the in-flight send's answer are kept.
const SEEN_KEPT: usize = 16;

/// Who sent the message in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A client's send, by its key ([`crate::daemon::protocol::SEND_KEY`]).
    Send(String),
    /// The daemon's send of held messages, by its number.
    Release(u64),
    /// Nothing of it is followed any more (taken back by an interrupt):
    /// what is held goes at the run's end.
    Gone,
}

/// A message held for the agent's open run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The id its client knows it by (it is no engine input).
    pub id: InputId,
    /// What the user typed, as sent.
    pub text: String,
}

/// The message in flight and the ones held behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// Who sent it.
    pub owner: Owner,
    /// The input the engine admitted it as (`None`: not answered yet).
    pub input: Option<InputId>,
    /// Inputs the run wrote while the answer was not in (one may be it).
    pub seen: Vec<InputId>,
    /// What it says (a run whose input holds it carries it).
    pub text: String,
    /// The messages typed since, oldest first.
    pub held: Vec<Held>,
}

/// The user's messages into one agent's open run (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UserSteers {
    /// None waits in the engine for the run's next request.
    #[default]
    NoneInFlight,
    /// One does, and these wait behind it.
    InFlight(InFlight),
}

/// What to do with a client's steer ([`Steers::admit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Send it to the engine now.
    Forward,
    /// It is held: answer the client with this id.
    Held(InputId),
}

/// Held messages to send now as one steer ([`Release::text`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Its number: the send's answer names it ([`Steers::released`]).
    /// `None` when the run ended: it starts the next run, and nothing is
    /// in flight in a run.
    pub serial: Option<u64>,
    /// The messages, oldest first.
    pub held: Vec<Held>,
}

impl Release {
    /// The messages' texts as one, oldest first, a blank line between.
    #[must_use]
    pub fn text(&self) -> String {
        self.held
            .iter()
            .map(|held| held.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The messages' ids, oldest first.
    #[must_use]
    pub fn ids(&self) -> Vec<InputId> {
        self.held.iter().map(|held| held.id.clone()).collect()
    }
}

/// What a client's taking back of messages by id found
/// ([`Steers::take_back`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TakenBack {
    /// Held ones, taken back (the engine never had them).
    pub held: Vec<InputId>,
    /// Sent with others: the engine's input, with the ids named of it.
    pub sent: Vec<(InputId, Vec<InputId>)>,
    /// Ids that are no held message: the engine's inputs, as named.
    pub other: Vec<InputId>,
}

/// One agent's user steers and the held messages already sent.
#[derive(Debug, Default)]
pub struct Steers {
    /// The state.
    pub now: UserSteers,
    /// Sent held messages: their id and the input they went as, newest
    /// last.
    aliases: Vec<(InputId, InputId)>,
}

/// Numbers the daemon's sends of held messages.
static RELEASES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Steers {
    /// A client's steer `text`, sent with `key`, for an agent whose run is
    /// open (`running`): forwarded when nothing is in flight (it is in
    /// flight now), else held.
    pub fn admit(&mut self, running: bool, key: &str, text: &str) -> Admit {
        if !running {
            return Admit::Forward;
        }
        match &mut self.now {
            UserSteers::NoneInFlight => {
                self.now = UserSteers::InFlight(InFlight {
                    owner: Owner::Send(key.to_string()),
                    input: None,
                    seen: Vec::new(),
                    text: text.trim().to_string(),
                    held: Vec::new(),
                });
                Admit::Forward
            }
            UserSteers::InFlight(flight) => {
                let id = InputId::new();
                flight.held.push(Held {
                    id: id.clone(),
                    text: text.to_string(),
                });
                Admit::Held(id)
            }
        }
    }

    /// Measurement only (`docs/evidence.md`, "Holding the first message
    /// too"): hold `text` even with nothing in flight, to go at the run's
    /// next tool request ([`Self::tool_requested`]) or its end.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_first(&mut self, text: &str) -> InputId {
        let id = InputId::new();
        let held = Held {
            id: id.clone(),
            text: text.to_string(),
        };
        match &mut self.now {
            UserSteers::InFlight(flight) => flight.held.push(held),
            UserSteers::NoneInFlight => {
                self.now = UserSteers::InFlight(InFlight {
                    owner: Owner::Gone,
                    input: None,
                    seen: Vec::new(),
                    text: String::new(),
                    held: vec![held],
                });
            }
        }
        id
    }

    /// Measurement only: the run's model call asked for tools; what
    /// [`Self::hold_first`] held goes now.
    #[cfg(any(test, feature = "test-support"))]
    pub fn tool_requested(&mut self) -> Option<Release> {
        match &self.now {
            UserSteers::InFlight(flight)
                if flight.owner == Owner::Gone && !flight.held.is_empty() =>
            {
                self.next()
            }
            _ => None,
        }
    }

    /// The engine answered the send `owner` (`Ok`: the input it admitted
    /// it as; `Err`: refused): when it is the one in flight, it is
    /// followed by its input, or, refused, what is held goes now.
    pub fn answered(&mut self, owner: &Owner, input: Result<InputId, ()>) -> Option<Release> {
        self.answer(owner, input).1
    }

    /// [`Self::answered`], and whether the run had already written the
    /// input.
    fn answer(&mut self, owner: &Owner, input: Result<InputId, ()>) -> (bool, Option<Release>) {
        let UserSteers::InFlight(flight) = &mut self.now else {
            return (false, None);
        };
        if flight.owner != *owner || flight.input.is_some() {
            return (false, None);
        }
        match input {
            Ok(input) => {
                let written = flight.seen.contains(&input);
                flight.input = Some(input);
                flight.seen.clear();
                (written, if written { self.next() } else { None })
            }
            Err(()) => (false, self.next()),
        }
    }

    /// The daemon's send `serial` of `ids` went as `input` (`Ok`) or was
    /// refused: each id is that input from now on, and the send in flight
    /// is followed as [`Self::answered`] says. `true` when the run had
    /// already written it (its ids are delivered).
    pub fn released(
        &mut self,
        serial: Option<u64>,
        ids: &[InputId],
        input: Result<InputId, ()>,
    ) -> (bool, Option<Release>) {
        if let Ok(input) = &input {
            for id in ids {
                self.aliases.push((id.clone(), input.clone()));
            }
            let over = self.aliases.len().saturating_sub(ALIASES_KEPT);
            self.aliases.drain(..over);
        }
        let Some(serial) = serial else {
            return (false, None);
        };
        self.answer(&Owner::Release(serial), input)
    }

    /// The run wrote `input` into a request: the held ids sent as it (now
    /// delivered), and, when it was the one in flight, what is held goes
    /// now.
    pub fn applied(&mut self, input: &InputId) -> (Vec<InputId>, Option<Release>) {
        let delivered = self.alias(input);
        let in_flight = match &mut self.now {
            UserSteers::InFlight(flight) if flight.input.as_ref() == Some(input) => true,
            UserSteers::InFlight(flight) if flight.input.is_none() => {
                flight.seen.push(input.clone());
                let over = flight.seen.len().saturating_sub(SEEN_KEPT);
                flight.seen.drain(..over);
                false
            }
            _ => false,
        };
        (delivered, if in_flight { self.next() } else { None })
    }

    /// The inputs `inputs` ended or were taken back: when the one in
    /// flight is among them, what is held goes now (or, after an
    /// interrupt, `hold`: at the run's end).
    pub fn settled(&mut self, inputs: &[InputId], hold: bool) -> Option<Release> {
        let UserSteers::InFlight(flight) = &mut self.now else {
            return None;
        };
        if !flight
            .input
            .as_ref()
            .is_some_and(|input| inputs.contains(input))
        {
            return None;
        }
        if hold && !flight.held.is_empty() {
            flight.owner = Owner::Gone;
            flight.input = None;
            flight.text.clear();
            return None;
        }
        self.next()
    }

    /// The one in flight is done with: the held messages go now as one
    /// (in flight from now on), or nothing is in flight.
    fn next(&mut self) -> Option<Release> {
        let held = match std::mem::take(&mut self.now) {
            UserSteers::InFlight(flight) => flight.held,
            UserSteers::NoneInFlight => Vec::new(),
        };
        if held.is_empty() {
            return None;
        }
        let serial = RELEASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let release = Release {
            serial: Some(serial),
            held,
        };
        self.now = UserSteers::InFlight(InFlight {
            owner: Owner::Release(serial),
            input: None,
            seen: Vec::new(),
            text: release.text().trim().to_string(),
            held: Vec::new(),
        });
        Some(release)
    }

    /// A run of the agent started with `prompt` as its input: when it
    /// holds the message in flight (it started the run, or the run took
    /// it as it began), that one is done with and the held ones go now:
    /// they ride the run's next request.
    pub fn run_started(&mut self, prompt: &str) -> Option<Release> {
        let UserSteers::InFlight(flight) = &self.now else {
            return None;
        };
        if flight.text.is_empty() || !prompt.contains(&flight.text) {
            return None;
        }
        self.next()
    }

    /// No run of the agent is open any more: what is held goes as one
    /// input that starts the next run; nothing is in flight.
    pub fn run_ended(&mut self) -> Option<Release> {
        let held = match std::mem::take(&mut self.now) {
            UserSteers::InFlight(flight) => flight.held,
            UserSteers::NoneInFlight => Vec::new(),
        };
        (!held.is_empty()).then_some(Release { serial: None, held })
    }

    /// The daemon's send of held messages failed before it went (the
    /// engine refused it, or the daemon stopped): nothing of it is
    /// followed.
    pub fn release_failed(&mut self, serial: Option<u64>) -> Option<Release> {
        self.released(serial, &[], Err(())).1
    }

    /// A client takes back `ids`: the held ones leave the hold; one sent
    /// with others is named by the input it went as, but only when every
    /// message of that input is among `ids` (a recall of it takes all of
    /// them back, maybe another client's); else it counts as reached.
    pub fn take_back(&mut self, ids: &[InputId]) -> (TakenBack, Vec<InputId>) {
        let mut taken = TakenBack::default();
        let mut reached = Vec::new();
        if let UserSteers::InFlight(flight) = &mut self.now {
            flight.held.retain(|held| {
                let named = ids.contains(&held.id);
                if named {
                    taken.held.push(held.id.clone());
                }
                !named
            });
        }
        for id in ids {
            if taken.held.contains(id) {
                continue;
            }
            match self.aliases.iter().find(|(held, _)| held == id) {
                Some((_, input)) => {
                    let all_named = self
                        .aliases
                        .iter()
                        .filter(|(_, sent)| sent == input)
                        .all(|(held, _)| ids.contains(held));
                    if !all_named {
                        reached.push(id.clone());
                        continue;
                    }
                    match taken.sent.iter_mut().find(|(sent, _)| sent == input) {
                        Some((_, named)) => named.push(id.clone()),
                        None => taken.sent.push((input.clone(), vec![id.clone()])),
                    }
                }
                None => taken.other.push(id.clone()),
            }
        }
        (taken, reached)
    }

    /// Whether `id` is held now.
    #[must_use]
    pub fn holds(&self, id: &InputId) -> bool {
        matches!(&self.now, UserSteers::InFlight(flight) if flight.held.iter().any(|held| &held.id == id))
    }

    /// The input the held message `id` went as, once sent.
    #[must_use]
    pub fn sent_as(&self, id: &InputId) -> Option<InputId> {
        self.aliases
            .iter()
            .find(|(held, _)| held == id)
            .map(|(_, input)| input.clone())
    }

    /// The held ids that went as `input`.
    #[must_use]
    pub fn alias(&self, input: &InputId) -> Vec<InputId> {
        self.aliases
            .iter()
            .filter(|(_, sent)| sent == input)
            .map(|(held, _)| held.clone())
            .collect()
    }

    /// Whether no message of the user's is in flight or held: a note of
    /// the daemon's may then take a slot of its own.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.now == UserSteers::NoneInFlight
    }

    /// Every held message, taken (the agent was seated again or retired,
    /// or the daemon stops); nothing is in flight.
    pub fn drop_all(&mut self) -> Vec<Held> {
        match std::mem::take(&mut self.now) {
            UserSteers::InFlight(flight) => flight.held,
            UserSteers::NoneInFlight => Vec::new(),
        }
    }

    /// Tests: the held messages' texts, oldest first.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn held_texts(&self) -> Vec<String> {
        match &self.now {
            UserSteers::InFlight(flight) => {
                flight.held.iter().map(|held| held.text.clone()).collect()
            }
            UserSteers::NoneInFlight => Vec::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn send(key: &str) -> Owner {
        Owner::Send(key.to_string())
    }

    /// Three messages typed during one model call: the first goes, the
    /// next two are held and go as one when the first is written; that
    /// send's input written delivers both.
    #[test]
    fn the_first_goes_and_the_rest_go_as_one_when_it_is_written() {
        let mut steers = Steers::default();
        assert_eq!(steers.admit(true, "k1", "one"), Admit::Forward);
        let Admit::Held(two) = steers.admit(true, "k2", "two") else {
            panic!("held")
        };
        let Admit::Held(three) = steers.admit(true, "k3", "three") else {
            panic!("held")
        };
        assert!(!steers.is_quiet());
        let first = InputId::new();
        assert_eq!(steers.answered(&send("k1"), Ok(first.clone())), None);
        let (delivered, release) = steers.applied(&first);
        assert!(delivered.is_empty());
        let release = release.expect("the held ones go");
        assert_eq!(release.text(), "two\n\nthree");
        assert_eq!(release.ids(), vec![two.clone(), three.clone()]);
        let merged = InputId::new();
        assert_eq!(
            steers.released(release.serial, &release.ids(), Ok(merged.clone())),
            (false, None)
        );
        assert_eq!(steers.sent_as(&two), Some(merged.clone()));
        let (delivered, release) = steers.applied(&merged);
        assert_eq!(delivered, vec![two, three]);
        assert_eq!(release, None);
        assert!(steers.is_quiet());
    }

    /// Not running: never held. An input written before the send's answer
    /// counts once the answer names it.
    #[test]
    fn an_input_written_before_its_answer_releases_on_the_answer() {
        let mut steers = Steers::default();
        assert_eq!(steers.admit(false, "k0", "idle"), Admit::Forward);
        assert!(steers.is_quiet());
        steers.admit(true, "k1", "one");
        steers.admit(true, "k2", "two");
        let first = InputId::new();
        assert_eq!(steers.applied(&first).1, None, "not known yet");
        let release = steers.answered(&send("k1"), Ok(first)).expect("written");
        assert_eq!(release.text(), "two");
        // Another client's answer changes nothing.
        assert_eq!(steers.answered(&send("k9"), Ok(InputId::new())), None);
        // The daemon's own send written before its receipt: delivered on
        // the receipt.
        steers.admit(true, "k3", "three");
        let merged = InputId::new();
        assert_eq!(steers.applied(&merged), (Vec::new(), None));
        let release = match &steers.now {
            UserSteers::InFlight(flight) => match &flight.owner {
                Owner::Release(serial) => Some(*serial),
                other => panic!("{other:?}"),
            },
            UserSteers::NoneInFlight => panic!("in flight"),
        };
        let (written, next) = steers.released(release, &[InputId::new()], Ok(merged));
        assert!(written);
        assert_eq!(next.map(|r| r.text()), Some("three".to_string()));
    }

    /// The run's end sends what is held as a run start; a refused send
    /// lets the held ones go; the daemon's own failed send leaves nothing
    /// in flight.
    #[test]
    fn the_run_end_and_refusals_let_the_held_ones_go() {
        let mut steers = Steers::default();
        steers.admit(true, "k1", "one");
        steers.admit(true, "k2", "two");
        let ended = steers.run_ended().expect("held");
        assert_eq!((ended.serial, ended.text()), (None, "two".to_string()));
        assert!(steers.is_quiet());
        steers.admit(true, "k3", "three");
        steers.admit(true, "k4", "four");
        let release = steers.answered(&send("k3"), Err(())).expect("refused");
        assert_eq!(release.text(), "four");
        assert_eq!(steers.release_failed(release.serial), None);
        assert!(steers.is_quiet());
    }

    /// A client takes back a held one for free; a sent one only with every
    /// message of its input; else it counts as reached. An interrupt that
    /// took the one in flight leaves the rest for the run's end.
    #[test]
    fn taking_back_names_held_sent_and_other_inputs() {
        let mut steers = Steers::default();
        steers.admit(true, "k1", "one");
        let Admit::Held(two) = steers.admit(true, "k2", "two") else {
            panic!()
        };
        let Admit::Held(three) = steers.admit(true, "k3", "three") else {
            panic!()
        };
        let engine = InputId::new();
        let (taken, reached) = steers.take_back(&[three.clone(), engine.clone()]);
        assert_eq!(taken.held, vec![three.clone()]);
        assert_eq!(taken.other, vec![engine]);
        assert!(reached.is_empty());
        assert!(steers.holds(&two) && !steers.holds(&three));
        let first = InputId::new();
        steers.answered(&send("k1"), Ok(first.clone()));
        let release = steers.applied(&first).1.unwrap();
        let merged = InputId::new();
        steers.released(release.serial, &release.ids(), Ok(merged.clone()));
        let (taken, _) = steers.take_back(std::slice::from_ref(&two));
        assert_eq!(taken.sent, vec![(merged, vec![two])]);
        // Two sent as one: naming one alone reaches it.
        let mut steers = Steers::default();
        steers.admit(true, "k1", "one");
        let a = match steers.admit(true, "k2", "a") {
            Admit::Held(id) => id,
            Admit::Forward => panic!(),
        };
        let b = match steers.admit(true, "k3", "b") {
            Admit::Held(id) => id,
            Admit::Forward => panic!(),
        };
        let first = InputId::new();
        steers.answered(&send("k1"), Ok(first.clone()));
        let release = steers.applied(&first).1.unwrap();
        steers.released(release.serial, &release.ids(), Ok(InputId::new()));
        let (taken, reached) = steers.take_back(std::slice::from_ref(&a));
        assert!(taken.sent.is_empty());
        assert_eq!(reached, vec![a.clone()]);
        let (taken, _) = steers.take_back(&[a, b]);
        assert_eq!(taken.sent.len(), 1);
        // An interrupt took the one in flight: the held one waits for the
        // run's end.
        let mut steers = Steers::default();
        steers.admit(true, "k1", "one");
        steers.admit(true, "k2", "two");
        let first = InputId::new();
        steers.answered(&send("k1"), Ok(first.clone()));
        assert_eq!(steers.settled(std::slice::from_ref(&first), true), None);
        assert!(matches!(&steers.now, UserSteers::InFlight(f) if f.owner == Owner::Gone));
        assert!(matches!(steers.admit(true, "k3", "three"), Admit::Held(_)));
        assert_eq!(steers.run_ended().unwrap().text(), "two\n\nthree");
        // A recall of the one in flight lets the held ones go now.
        steers.admit(true, "k1", "one");
        steers.admit(true, "k2", "two");
        let first = InputId::new();
        steers.answered(&send("k1"), Ok(first.clone()));
        assert_eq!(
            steers.settled(&[first], false).map(|r| r.text()),
            Some("two".to_string())
        );
        assert_eq!(steers.drop_all().len(), 0);
    }
}
