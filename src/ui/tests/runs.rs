//! Runs of blocks folded into one line ([`crate::ui::transcript`]'s
//! runs): the messages of the agent's own helpers.

use super::*;

/// The `✉` rows of the screen.
fn mail_rows(app: &mut App) -> Vec<String> {
    screen(app, 40)
        .into_iter()
        .filter(|row| row.contains('✉'))
        .collect()
}

/// The screen row whose text starts (after blanks) with `start`.
fn row_starting(app: &mut App, start: &str) -> u16 {
    let rows = screen(app, 40);
    let at = rows
        .iter()
        .position(|row| row.trim_start().starts_with(start))
        .unwrap_or_else(|| panic!("no row starts with {start:?}: {rows:#?}"));
    u16::try_from(at).unwrap()
}

/// A click on the row [`row_starting`] finds, well after any earlier one
/// (never a double click).
fn click_on(app: &mut App, start: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LATER: AtomicU64 = AtomicU64::new(0);
    let row = row_starting(app, start);
    let at =
        Instant::now() + std::time::Duration::from_secs(LATER.fetch_add(10, Ordering::Relaxed));
    app.update_at(down(row, 3), at);
    app.update_at(up(row, 3), at);
}

/// An app whose agent has the helpers `helpers` seated, after the user's
/// prompt.
fn app_with(helpers: &[&str]) -> App {
    let mut app = App::new(StatusInfo::default());
    app.transcript.push(user("go"));
    app.transcript
        .seated_helpers(helpers.iter().map(|h| (*h).to_string()));
    app
}

fn message(app: &mut App, from: &str, text: &str) {
    app.update(Msg::Peer {
        from: from.into(),
        text: text.into(),
    });
}

/// A finished `read_file` call of the agent.
fn read_call(app: &mut App, id: &str) {
    app.update(agent(AgentEvent::ToolCallRequested {
        id: id.into(),
        name: "read_file".into(),
        args: test_args(json!({"path": id})),
    }));
    app.update(agent(AgentEvent::ToolExecutionCompleted {
        id: id.into(),
        name: "read_file".into(),
        is_error: false,
        duration_ms: 1,
    }));
}

const SIX: [&str; 5] = [
    "capture-api",
    "capture-source",
    "capture-camera",
    "capture-audio",
    "capture-ui",
];

/// One message stays its own line; two or more from the agent's helpers
/// in a row are one line: the count, the senders (three, then how many
/// more) and the latest message's first words.
#[test]
fn a_run_of_helper_messages_draws_as_one_line() {
    let mut app = app_with(&SIX);
    message(&mut app, "capture-api", "the api answers");
    assert_eq!(
        mail_rows(&mut app),
        ["✉ from agent capture-api: the api answers ▸"]
    );
    message(&mut app, "capture-api", "and the second endpoint too");
    assert_eq!(
        mail_rows(&mut app),
        ["✉ 2 messages from capture-api: and the second endpoint too ▸"]
    );
    for (i, from) in SIX.iter().enumerate().skip(1) {
        message(&mut app, from, &format!("part {i} done"));
    }
    message(&mut app, "capture-source", "all sources found");
    assert_eq!(
        mail_rows(&mut app),
        ["✉ 7 messages from capture-api, capture-source, capture-camera and 2 more ▸"]
    );
    // The blocks are one per message still.
    let peers = app
        .transcript
        .iter()
        .filter(|(_, block)| matches!(block, Block::Peer { .. }))
        .count();
    assert_eq!(peers, 7);
    assert!(app.transcript.export_text().contains("all sources found"));
    // With room on the line the latest message's first words follow.
    let mut app = app_with(&SIX);
    message(&mut app, "capture-api", "a");
    message(&mut app, "capture-ui", "b");
    message(&mut app, "capture-api", "the camera is found");
    assert_eq!(
        mail_rows(&mut app),
        ["✉ 3 messages from capture-api, capture-ui: the camera is found ▸"]
    );
}

/// Another visible block parts a run; a message of an agent that is no
/// helper of this one (the workspace's other agent, an agent wired with
/// `/wire`) is its own line and parts it too.
#[test]
fn a_run_is_parted_by_a_tool_line_and_by_a_message_of_no_helper() {
    let mut app = app_with(&SIX);
    message(&mut app, "capture-api", "one");
    message(&mut app, "capture-api", "two");
    read_call(&mut app, "a.rs");
    message(&mut app, "capture-api", "three");
    let rows = screen(&mut app, 40);
    let shown: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains('✉') || row.contains("read a.rs"))
        .collect();
    assert_eq!(
        shown,
        [
            "✉ 2 messages from capture-api: two ▸",
            "✓ read a.rs ▸",
            "✉ from agent capture-api: three ▸",
        ],
        "{rows:#?}"
    );

    let mut app = app_with(&SIX);
    message(&mut app, "capture-api", "one");
    message(&mut app, "other/main-ab12", "wired in");
    message(&mut app, "capture-api", "two");
    message(&mut app, "main-ab12", "same workspace");
    message(&mut app, "main-ab12", "again");
    assert_eq!(
        mail_rows(&mut app),
        [
            "✉ from agent capture-api: one ▸",
            "✉ from agent other/main-ab12: wired in ▸",
            "✉ from agent capture-api: two ▸",
            "✉ from agent main-ab12: same workspace ▸",
            "✉ from agent main-ab12: again ▸",
        ]
    );
}

/// A click on the run's line opens it: the line, then each message as it
/// draws alone, each opened by its own click; a click on the run's line
/// folds it again. Ctrl-O shows every message in full and folds them back
/// into the run.
#[test]
fn a_run_opens_and_folds_by_click_and_ctrl_o() {
    let mut app = app_with(&SIX);
    message(&mut app, "capture-api", "first words");
    message(&mut app, "capture-ui", "second words");
    let folded = mail_rows(&mut app);
    click_on(&mut app, "✉ 2 messages");
    assert_eq!(
        mail_rows(&mut app),
        [
            "✉ 2 messages from capture-api, capture-ui ▾",
            "✉ from agent capture-api: first words ▸",
            "✉ from agent capture-ui: second words ▸",
        ]
    );
    // A message of the opened run opens on its own.
    click_on(&mut app, "✉ from agent capture-ui");
    let rows = screen(&mut app, 40);
    assert!(
        rows.iter().any(|r| r == "✉ from agent capture-ui ▾"),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r == "  second words"), "{rows:#?}");
    click_on(&mut app, "✉ from agent capture-ui");
    // The run's line folds it.
    click_on(&mut app, "✉ 2 messages");
    assert_eq!(mail_rows(&mut app), folded);

    app.update(ctrl('o'));
    let rows = screen(&mut app, 40);
    assert!(
        rows.iter().any(|r| r == "✉ from agent capture-api ▾")
            && rows.iter().any(|r| r == "  first words"),
        "{rows:#?}"
    );
    app.update(ctrl('o'));
    assert_eq!(mail_rows(&mut app), folded);
}

/// A message that arrives while the run is opened joins it opened, and
/// a new message grows the folded line in place.
#[test]
fn a_run_grows_as_messages_arrive() {
    let mut app = app_with(&SIX);
    message(&mut app, "capture-api", "a");
    message(&mut app, "capture-api", "b");
    assert_eq!(mail_rows(&mut app), ["✉ 2 messages from capture-api: b ▸"]);
    message(&mut app, "capture-camera", "c");
    assert_eq!(
        mail_rows(&mut app),
        ["✉ 3 messages from capture-api, capture-camera: c ▸"]
    );
    // The row accounting holds: every line is one block's, and a drag
    // over the run's line copies it.
    let total = app.transcript.ensure_layout(80);
    assert_eq!(app.transcript.visible_lines(0, total + 5).len(), total);
    assert_eq!(app.transcript.visible_blocks(0, total + 5).len(), total);
    let row = row_starting(&mut app, "✉ 3 messages");
    let at = Instant::now();
    app.update_at(down(row, 0), at);
    app.update_at(drag(row, 11), at);
    app.update_at(up(row, 11), at);
    assert_eq!(app.take_clipboard_text().as_deref(), Some("✉ 3 messages"));
    click_on(&mut app, "✉ 3 messages");
    message(&mut app, "capture-api", "d");
    assert_eq!(
        mail_rows(&mut app),
        [
            "✉ 4 messages from capture-api, capture-camera ▾",
            "✉ from agent capture-api: a ▸",
            "✉ from agent capture-api: b ▸",
            "✉ from agent capture-camera: c ▸",
            "✉ from agent capture-api: d ▸",
        ]
    );
}

/// A stored conversation shows its helpers' messages folded the same way:
/// the calls that started them say they are the agent's helpers.
#[test]
fn a_restored_conversation_folds_its_helpers_messages() {
    use meerkat_core::{
        BlockAssistantMessage, CommsNoticeKind, ContentBlock, Message, StopReason,
        SystemNoticeBlock, SystemNoticeDirection, SystemNoticeKind, SystemNoticeMessage,
        SystemNoticePeer, types::AssistantBlock,
    };
    let assign = |id: &str, name: &str| AssistantBlock::ToolUse {
        id: id.into(),
        name: crate::tools::ASSIGN.into(),
        args: serde_json::value::RawValue::from_string(format!(
            r#"{{"name":"{name}","task":"look"}}"#
        ))
        .unwrap(),
        meta: None,
    };
    let notice = |from: &str, words: &str| {
        Message::SystemNotice(SystemNoticeMessage {
            kind: SystemNoticeKind::Comms,
            runtime_origin: None,
            body: Some("Peer message".into()),
            blocks: vec![SystemNoticeBlock::Comms {
                kind: CommsNoticeKind::Message,
                direction: SystemNoticeDirection::Incoming,
                peer: Some(SystemNoticePeer {
                    id: serde_json::from_value(json!("46d5fbeb-acb0-5a56-a10f-31dd0fb07f9e"))
                        .unwrap(),
                    display_name: Some(format!("tui/main/{from}")),
                }),
                sender_taint: None,
                request_id: None,
                intent: None,
                status: None,
                summary: Some("Peer message".into()),
                payload: None,
                content: vec![ContentBlock::Text {
                    text: format!("Peer message from tui/main/{from}:\n{words}"),
                }],
                delivery_id: None,
                in_reply_to: None,
            }],
            created_at: meerkat_core::types::message_timestamp_now(),
        })
    };
    let messages = [
        Message::BlockAssistant(BlockAssistantMessage::new(
            vec![assign("c1", "capture-api"), assign("c2", "capture-ui")],
            StopReason::ToolUse,
        )),
        notice("capture-api", "found it"),
        notice("capture-ui", "drawn"),
        notice("main-ab12", "not a helper"),
    ];
    let mut app = App::new(StatusInfo::default());
    app.transcript
        .reset(crate::ui::transcript::blocks_from_messages(&messages));
    assert_eq!(
        mail_rows(&mut app),
        [
            "✉ 2 messages from capture-api, capture-ui: drawn ▸",
            "✉ from agent main-ab12: not a helper ▸",
        ]
    );
    click_on(&mut app, "✉ 2 messages");
    assert_eq!(
        mail_rows(&mut app),
        [
            "✉ 2 messages from capture-api, capture-ui ▾",
            "✉ from agent capture-api: found it ▸",
            "✉ from agent capture-ui: drawn ▸",
            "✉ from agent main-ab12: not a helper ▸",
        ]
    );
}

/// In a helper's view the messages its own helpers sent it fold as the
/// helper's lines (its tag and indent); its parent's message stays its
/// own line. A click on the view's run line opens it there.
#[test]
fn a_helper_s_view_folds_the_messages_of_its_helpers() {
    let (mut app, scout, _) = helper_app();
    app.transcript
        .seated_helpers(["scout", "fork-1", "kid-1", "kid-2"].map(String::from));
    for (from, body) in [
        ("main-kgem", "fix the comments"),
        ("kid-1", "half done"),
        ("kid-2", "the rest done"),
    ] {
        app.update(helper_msg(
            "scout",
            super::helpers::run_on(&super::helpers::peer_input(from, body)),
        ));
    }
    // The panel's selection goes fork-1, then scout.
    app.update(ctrl_key(KeyCode::Down));
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(scout));
    let rows = mail_rows(&mut app);
    assert_eq!(
        rows,
        [
            "  ✉ [scout · m] from main-kgem: fix the comments ▸",
            "  ✉ [scout · m] 2 messages from kid-1, kid-2: the rest done ▸",
        ]
    );
    click_on(&mut app, "✉ [scout · m] 2 messages");
    assert_eq!(
        mail_rows(&mut app),
        [
            "  ✉ [scout · m] from main-kgem: fix the comments ▸",
            "  ✉ [scout · m] 2 messages from kid-1, kid-2 ▾",
            "  ✉ [scout · m] from kid-1: half done ▸",
            "  ✉ [scout · m] from kid-2: the rest done ▸",
        ]
    );
    click_on(&mut app, "✉ [scout · m] 2 messages");
    assert_eq!(mail_rows(&mut app).len(), 2);
}
