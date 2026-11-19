//! No instructions about comms tools the members do not have.
//!
//! Every mob member's system prompt carries two texts of the engine about
//! its comms tools: the "Inter-agent Communication" section
//! (`meerkat-tools` `CommsToolSurface::usage_instructions`, appended to the
//! tool instructions by the facade's comms composition) and the preloaded
//! `mob-communication` skill (`meerkat-mob` `build.rs` preloads it for
//! every member; the factory renders it into the prompt as a `<skill>`
//! block). Both teach `reply_to_peer`, `send_request`, `send_response` and
//! `peers`, which rkat-tui does not offer
//! ([`crate::approval::policy::NOT_OFFERED`]); neither the spawn spec nor
//! the profile can leave them out (upstream lukacf/meerkat#2432). The
//! engine's `send_message` description also points at `send_request` and
//! `peers`.
//!
//! The daemon's client decorator (`super::schema_compat::SchemaCompat`)
//! sends every request of the agents and their helpers through
//! [`for_the_wire`] and [`description`], so what is stored stays as the
//! engine wrote it and only what is sent changes:
//!
//! - in every system message, the whole section and the whole skill block
//!   are left out, with the blank line that separated them. Both are
//!   obtained from the engine itself ([`engine_texts`]: the section from
//!   `CommsToolSurface::usage_instructions`, the block rendered by
//!   `meerkat_skills::renderer::render_injection` from the registered
//!   skill's body, as the factory renders a preloaded skill), so a changed
//!   wording is still removed. The prompt's skill inventory loses its
//!   entries for `mob-communication` and `multi-agent-comms` (rendered by
//!   `render_inventory`), which would teach the same when opened. What the members need of them is in their
//!   own instructions (`crate::bootstrap::persona`): `send_message` for
//!   every ask and answer, the peer_id from `contacts`, how a received
//!   message names its sender, that a join or leave note needs no answer;
//!   `send_message`'s own description says that it expects no response
//!   and what `handling_mode` does.
//! - `send_message`'s description loses its pointers at the tools it does
//!   not have ([`SEND_MESSAGE_EDITS`]: copied from the engine's text, so a
//!   test fails when the engine's description no longer holds one).
//!
//! The section names `send_message` too: nothing else in it is ours to
//! keep (it says only to pass `handling_mode`, which the tool's schema
//! requires and its description explains).

use std::borrow::Cow;
use std::sync::OnceLock;

use meerkat_core::{Message, SystemMessage, ToolDef, ToolSourceKind};

/// The engine's preloaded skill about the comms tools.
pub const SKILL: &str = "mob-communication";

/// The engine's skills that teach the comms tools the members do not have:
/// their entries in the prompt's `<available_skills>` inventory go too
/// (opened, each would teach them again).
pub const SKILLS: [&str; 2] = [SKILL, "multi-agent-comms"];

/// The edits `send_message`'s description gets: each engine phrase and
/// what it is sent as.
pub const SEND_MESSAGE_EDITS: [(&str, &str); 3] = [
    (
        " If you need a correlated reply, use send_request instead.",
        "",
    ),
    (
        "Call peers first to pick a peer_id.",
        "Call contacts first to pick a peer_id.",
    ),
    ("<peer-id-from-peers>", "<peer-id-from-contacts>"),
];

/// The texts of the engine every system message is sent without: the comms
/// section, the rendered [`SKILL`] block, and the inventory entry of each
/// of [`SKILLS`] (a skill's texts only when it is registered in this
/// build).
#[must_use]
pub fn engine_texts() -> &'static [String] {
    static TEXTS: OnceLock<Vec<String>> = OnceLock::new();
    TEXTS.get_or_init(|| {
        let mut texts = vec![meerkat_tools::CommsToolSurface::usage_instructions().to_string()];
        texts.extend(skill_block());
        texts.extend(SKILLS.iter().filter_map(|name| inventory_entry(name)));
        texts
    })
}

/// The registered builtin skill `name`, with its key.
fn registered(
    name: &str,
) -> Option<(
    &'static meerkat_skills::SkillRegistration,
    meerkat_core::skills::SkillKey,
)> {
    let registration = meerkat_skills::collect_registered_skills()
        .into_iter()
        .find(|registration| registration.id == name)?;
    let name = meerkat_core::skills::SkillName::parse(name).ok()?;
    Some((registration, meerkat_core::skills::SkillKey::builtin(name)))
}

/// [`SKILL`] as the factory renders a preloaded skill into the prompt.
fn skill_block() -> Option<String> {
    let (registration, key) = registered(SKILL)?;
    Some(meerkat_skills::renderer::render_injection(
        &key,
        registration.body,
    ))
}

/// The skill `name`'s entry in the prompt's skill inventory, as the
/// engine's renderer writes it (its own listing of that skill alone, less
/// the listing's opening and closing lines).
fn inventory_entry(name: &str) -> Option<String> {
    let (registration, key) = registered(name)?;
    let descriptor = meerkat_core::skills::SkillDescriptor::new(
        key,
        registration.name,
        registration.description,
    );
    let listing = meerkat_skills::renderer::render_inventory(&[descriptor], &[], usize::MAX);
    let entry = listing
        .strip_prefix("<available_skills>\n")?
        .strip_suffix("</available_skills>")?;
    (!entry.is_empty()).then(|| entry.to_string())
}

/// `content` without the engine's [`engine_texts`], each with the blank
/// line that joined it to its neighbour; borrowed when none is in it.
#[must_use]
pub fn system_text(content: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(content);
    for text in engine_texts() {
        if let Some(cut) = without(&out, text) {
            out = Cow::Owned(cut);
        }
    }
    out
}

/// `content` without `text` (every occurrence) and the blank line before
/// it (or, at the start, after it); `None` when `text` is not in it.
fn without(content: &str, text: &str) -> Option<String> {
    if text.is_empty() || !content.contains(text) {
        return None;
    }
    let mut out = content.to_string();
    while let Some(at) = out.find(text) {
        let mut from = at;
        let mut to = at + text.len();
        if out[..from].ends_with("\n\n") {
            from -= 2;
        } else if out[to..].starts_with("\n\n") {
            to += 2;
        }
        out.replace_range(from..to, "");
    }
    Some(out)
}

/// `messages` with every system message's text as [`system_text`] gives
/// it; `None` when no message changes.
#[must_use]
pub fn for_the_wire(messages: &[Message]) -> Option<Vec<Message>> {
    let changes = |message: &Message| {
        matches!(message, Message::System(system)
            if matches!(system_text(&system.content), Cow::Owned(_)))
    };
    if !messages.iter().any(changes) {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| match message {
                Message::System(system) => match system_text(&system.content) {
                    Cow::Owned(content) => Message::System(SystemMessage {
                        content,
                        ..system.clone()
                    }),
                    Cow::Borrowed(_) => message.clone(),
                },
                other => other.clone(),
            })
            .collect(),
    )
}

/// The description the engine's `send_message` is sent with
/// ([`SEND_MESSAGE_EDITS`]); `None` for every other tool, and when nothing
/// changes.
#[must_use]
pub fn description(def: &ToolDef) -> Option<String> {
    let comms = def
        .provenance
        .as_ref()
        .is_some_and(|provenance| provenance.kind == ToolSourceKind::Comms);
    if !comms || def.name.as_str() != "send_message" {
        return None;
    }
    let mut out = def.description.clone();
    for (from, to) in SEND_MESSAGE_EDITS {
        out = out.replace(from, to);
    }
    (out != def.description).then_some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The four tools the members do not have, as the engine's texts name
    /// them.
    const ABSENT: [&str; 4] = ["reply_to_peer", "send_request", "send_response", "peers"];

    fn system(content: &str) -> Message {
        Message::System(SystemMessage::new(content.to_string()))
    }

    fn content(message: &Message) -> &str {
        match message {
            Message::System(system) => &system.content,
            other => panic!("{other:?}"),
        }
    }

    /// What is cut is the engine's own text, found: the section and the
    /// skill block both exist in this build and teach the four tools (when
    /// a pin move drops or rewords them, this says so).
    #[test]
    fn the_engine_texts_exist_and_name_the_tools_we_do_not_offer() {
        let texts = engine_texts();
        assert_eq!(
            texts.len(),
            4,
            "the section, the skill block, two inventory entries"
        );
        assert!(
            texts[0].starts_with("# Inter-agent Communication"),
            "{}",
            texts[0]
        );
        assert!(
            texts[1].starts_with("<skill ")
                && texts[1].contains("skill_name=\"mob-communication\""),
            "{}",
            texts[1]
        );
        for (entry, name) in texts[2..].iter().zip(SKILLS) {
            assert!(
                entry.starts_with("  <skill ")
                    && entry.contains(&format!("skill_name=\"{name}\""))
                    && entry.ends_with("</skill>\n"),
                "{entry}"
            );
            // Opened, the skill teaches the tools we do not offer.
            let (registration, _) = registered(name).expect("registered");
            assert!(registration.body.contains("send_request"), "{name}");
        }
        for text in &texts[..2] {
            for absent in ABSENT {
                assert!(
                    crate::approval::policy::NOT_OFFERED.contains(&absent),
                    "{absent}"
                );
                assert!(
                    text.contains(&format!("`{absent}`"))
                        || text.contains(&format!("**{absent}**")),
                    "{absent}: {text}"
                );
            }
        }
    }

    /// A prompt as the engine assembles it (persona, skill block, tool
    /// instructions, the comms section, each joined by a blank line) is
    /// sent with the persona and the other tool instructions only.
    #[test]
    fn the_section_and_the_skill_are_cut_with_their_blank_lines() {
        let [section, skill, mob, multi] = engine_texts() else {
            panic!("four texts")
        };
        let other = "  <skill source_uuid=\"x\" skill_name=\"other\">\n    <description>d</description>\n  </skill>\n";
        let prompt = format!(
            "PERSONA\n\n<available_skills>\n{multi}{other}{mob}</available_skills>\n\n{skill}\n\nOTHER TOOLS\n\n{section}"
        );
        let messages = vec![system(&prompt), system("NOTICE")];
        let sent = for_the_wire(&messages).expect("changed");
        assert_eq!(
            content(&sent[0]),
            format!("PERSONA\n\n<available_skills>\n{other}</available_skills>\n\nOTHER TOOLS")
        );
        assert_eq!(content(&sent[1]), "NOTICE");
        // At the very start, the blank line after it goes.
        assert_eq!(system_text(&format!("{section}\n\nREST")), "REST");
        assert_eq!(system_text(section.as_str()), "");
    }

    /// Nothing changes in a conversation without them; user text that
    /// quotes the section is never touched.
    #[test]
    fn other_messages_are_never_changed() {
        let section = &engine_texts()[0];
        let messages = vec![
            system("PERSONA"),
            Message::User(meerkat_core::UserMessage::text(section.clone())),
        ];
        assert!(for_the_wire(&messages).is_none());
        assert!(matches!(system_text("PERSONA"), Cow::Borrowed("PERSONA")));
    }

    /// `send_message`'s description as the engine has it holds every phrase
    /// edited (a pin move that rewords one fails here), and as sent names
    /// none of the four tools and points at `contacts`.
    #[test]
    fn send_message_s_description_loses_the_tools_we_do_not_offer() {
        let defs = meerkat_comms::comms_tool_defs();
        let def = defs
            .iter()
            .find(|def| def.name.as_str() == "send_message")
            .expect("the engine's send_message");
        for (from, _) in SEND_MESSAGE_EDITS {
            assert!(
                def.description.contains(from),
                "{from}: {}",
                def.description
            );
        }
        let sent = description(def).expect("changed");
        for absent in ABSENT {
            assert!(!sent.contains(absent), "{absent}: {sent}");
        }
        assert!(
            sent.contains("Call contacts first to pick a peer_id."),
            "{sent}"
        );
        assert!(sent.contains("fire-and-forget"), "{sent}");
        // Another tool, or a host tool of that name, is left as it is.
        let other = defs
            .iter()
            .find(|def| def.name.as_str() == "peers")
            .expect("peers");
        assert!(description(other).is_none());
        let mut host = ToolDef::clone(def);
        host.provenance = None;
        assert!(description(&host).is_none());
    }
}
