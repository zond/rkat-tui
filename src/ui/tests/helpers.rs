//! The helper panel and a helper's full view.

use super::*;

#[test]
fn the_panel_lists_running_helpers_newest_first_and_drops_finished_ones_at_the_next_turn() {
    let (mut app, scout, fork) = helper_app();
    assert_eq!(app.helper_panel(), vec![fork, scout]);
    assert!(app.status.helper_panel);
    // scout finishes: still listed during this turn, as done.
    app.update(live(&["fork-1"]));
    assert_eq!(app.helper_panel(), vec![fork, scout]);
    assert!(matches!(
        app.transcript.block(scout),
        Some(Block::Helper(fold)) if fold.state == transcript::HelperState::Done
    ));
    // The next turn starts: the finished helper drops off, the running
    // one stays; the inline header stays in the transcript.
    type_str(&mut app, "next");
    app.update(key(KeyCode::Enter));
    assert!(app.is_running());
    assert_eq!(app.helper_panel(), vec![fork]);
    assert_eq!(app.transcript.helper_header("scout"), Some(scout));
    // Once nothing runs and nothing finished this turn, the panel is
    // empty and hidden.
    app.update(live(&[]));
    assert_eq!(app.helper_panel(), vec![fork]);
    app.update(Msg::TurnFinished(TurnOutcome {
        result: Ok(summary()),
    }));
    type_str(&mut app, "again");
    app.update(key(KeyCode::Enter));
    assert!(app.helper_panel().is_empty());
    assert!(!app.status.helper_panel);
}

#[test]
fn a_nested_helper_is_listed_under_its_parent() {
    let (mut app, scout, fork) = helper_app();
    app.update(Msg::Scoped(
        crate::mobs::HelperTag {
            owner: meerkat_core::SessionId::new(),
            path: vec!["scout".into(), "inner".into()],
            model: "m".into(),
        }
        .scope(AgentEvent::ToolCallRequested {
            id: "b".into(),
            name: "grep".into(),
            args: test_args(json!({})),
        }),
    ));
    let inner = app.transcript.helper_header("inner").unwrap();
    assert_eq!(app.helper_panel(), vec![fork, scout, inner]);
    let rows = screen(&mut app, 12);
    // Under the input (its border, one line), above the status bar.
    assert_eq!(
        rows[8],
        "  [fork-1 · m] ⠋ running · 1 tool call · glob \"*\""
    );
    assert_eq!(
        rows[9],
        "  [scout · m] ⠋ running · 1 tool call · glob \"*\""
    );
    assert_eq!(rows[10], "    [inner] ⠋ running · 1 tool call · grep");
}

#[test]
fn the_panel_shows_at_most_six_lines() {
    let mut app = App::new(StatusInfo::default());
    let labels: Vec<String> = (0..8).map(|i| format!("h{i}")).collect();
    let names: Vec<&str> = labels.iter().map(String::as_str).collect();
    app.update(live(&names));
    assert_eq!(app.helper_panel().len(), 8);
    let rows = screen(&mut app, 16);
    // Five helpers (newest first), then the rest counted.
    assert!(rows[9].starts_with("  [h7 · m]"), "{rows:?}");
    assert!(rows[13].starts_with("  [h3 · m]"), "{rows:?}");
    assert_eq!(rows[14], "  … and 3 more");
    // Selecting past the window moves it (the first press selects the
    // newest, seven more the oldest).
    for _ in 0..8 {
        app.update(ctrl_key(KeyCode::Down));
    }
    let rows = screen(&mut app, 16);
    assert!(rows[13].starts_with("  [h0 · m]"), "{rows:?}");
    assert_eq!(render::panel_window(8, Some(7)), (3, 5, 3));
    assert_eq!(render::panel_window(6, Some(5)), (0, 6, 0));
}

#[test]
fn ctrl_down_selects_the_newest_helper_and_ctrl_up_down_wrap() {
    let (mut app, scout, fork) = helper_app();
    assert_eq!(app.panel_selection(), None);
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), Some(fork));
    // Down past the last (oldest) wraps to the first, up past the first
    // to the last.
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), Some(scout));
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), Some(fork));
    app.update(ctrl_key(KeyCode::Up));
    assert_eq!(app.panel_selection(), Some(scout));
    // The first Ctrl+Up also starts at the newest.
    let (mut app, _, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Up));
    assert_eq!(app.panel_selection(), Some(fork));
    // Typing keeps the selection.
    type_str(&mut app, "x");
    assert_eq!(app.panel_selection(), Some(fork));
    // No helpers: nothing to select.
    let mut app = App::new(StatusInfo::default());
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), None);
    assert!(!app.status.helper_panel);
}

#[test]
fn enter_with_an_empty_input_opens_the_full_view_without_submitting() {
    let (mut app, scout, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    let fx = app.update(key(KeyCode::Enter));
    assert!(fx.is_empty(), "{fx:?}");
    assert!(!app.is_running());
    assert_eq!(app.helper_view(), Some(fork));
    // The inline header is not expanded by the view.
    assert!(!expanded(&app, fork));
    // Another selection switches the view; Enter on the open one
    // closes it.
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(scout));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), None);
}

#[test]
fn enter_with_text_submits_despite_a_selection() {
    let (mut app, _, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(fork));
    // The editor keeps working while the view is open.
    type_str(&mut app, "draft");
    let fx = app.update(key(KeyCode::Enter));
    assert_eq!(fx, vec![Effect::StartTurn("draft".into())]);
    assert!(app.input.is_empty());
    // The view and the selection stay.
    assert_eq!(app.helper_view(), Some(fork));
    assert_eq!(app.panel_selection(), Some(fork));
}

#[test]
fn esc_closes_the_view_then_drops_the_selection() {
    let (mut app, _, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(fork));
    let fx = app.update(key(KeyCode::Esc));
    assert!(fx.is_empty());
    assert_eq!(app.helper_view(), None);
    assert_eq!(app.panel_selection(), Some(fork));
    let fx = app.update(key(KeyCode::Esc));
    assert!(fx.is_empty());
    assert_eq!(app.panel_selection(), None);
    // A finished helper's view stays open until Esc.
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(live(&[]));
    assert_eq!(app.helper_view(), Some(fork));
}

#[test]
fn the_helper_view_follows_new_output_while_at_the_bottom() {
    let (mut app, _, _) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    for i in 0..10 {
        app.update(fork_call(&format!("f{i}")));
    }
    // 12 rows: the view gets 7 (title + 6), then the input, the panel
    // (two helpers) and the status bar.
    let rows = screen(&mut app, 12);
    assert!(rows[0].starts_with("◂ [fork-1 · m]"), "{rows:?}");
    assert!(rows[5].contains("read_file({\"path\":\"f9\"})"), "{rows:?}");
    app.update(fork_call("f10"));
    let rows = screen(&mut app, 12);
    assert!(rows[5].contains("\"f10\""), "{rows:?}");
    // Scrolled back, new output does not move the view.
    app.update(key(KeyCode::PageUp));
    let before = screen(&mut app, 12);
    app.update(fork_call("f11"));
    let after = screen(&mut app, 12);
    assert_eq!(before[1..7], after[1..7]);
    // The transcript's own scroll is untouched.
    assert!(app.scroll.follow);
    // Back at the bottom it follows again.
    app.update(Msg::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)));
    let rows = screen(&mut app, 12);
    assert!(rows[5].contains("\"f11\""), "{rows:?}");
}

#[test]
fn a_helpers_prose_and_reasoning_are_in_its_view_in_order() {
    let (mut app, _, fork) = helper_app();
    for event in [
        AgentEvent::TurnStarted {
            turn_number: 1,
            assistant_message_id: None,
        },
        AgentEvent::ReasoningDelta {
            delta: "Where is it?".into(),
            assistant_message_id: None,
        },
        AgentEvent::TextDelta {
            delta: "It is in **a.rs**.".into(),
            assistant_message_id: None,
        },
        AgentEvent::TextComplete {
            content: "It is in **a.rs**.".into(),
            assistant_message_id: None,
        },
    ] {
        app.update(helper_msg("fork-1", event));
    }
    app.update(fork_call("f1"));
    let kinds: Vec<&str> = app
        .transcript
        .helper_view_blocks(fork)
        .into_iter()
        .map(|id| match app.transcript.block(id) {
            Some(Block::Tool { .. }) => "tool",
            Some(Block::Reasoning { .. }) => "reasoning",
            Some(Block::Assistant { .. }) => "prose",
            other => panic!("unexpected block {other:?}"),
        })
        .collect();
    assert_eq!(kinds, ["tool", "reasoning", "prose", "tool"]);
    // Ctrl-O in the view toggles the helper's latest reasoning (and
    // its latest tool output).
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(ctrl('o'));
    let reasoning = app.transcript.helper_view_blocks(fork)[1];
    assert!(matches!(
        app.transcript.block(reasoning),
        Some(Block::Reasoning {
            collapsed: false,
            ..
        })
    ));
    // Folded inline: nothing of it is on the transcript.
    assert!(app.transcript.is_hidden(reasoning));
}

#[test]
fn a_click_on_a_helper_header_toggles_it() {
    let (mut app, scout, _) = helper_app();
    draw_rows(&mut app, 0, 80, 20);
    // Rows: user (2 lines), then scout's header.
    assert_eq!(app.block_at_row(2), Some(scout));
    click_at(&mut app, 2);
    assert!(expanded(&app, scout));
    // Later than a double click.
    draw_rows(&mut app, 0, 80, 20);
    let later = Instant::now() + Duration::from_secs(2);
    app.update_at(down(2, 3), later);
    app.update_at(up(2, 3), later);
    assert!(!expanded(&app, scout));
}

#[test]
fn helpers_command_lists_expands_and_folds() {
    let (mut app, scout, fork) = helper_app();
    type_str(&mut app, "/helpers");
    app.update(key(KeyCode::Enter));
    let listing = match app.transcript.blocks().last() {
        Some(Block::Notice { text, .. }) => text.clone(),
        other => panic!("no listing: {other:?}"),
    };
    assert!(
        listing.contains("▸ [scout · m] running · 1 tool call"),
        "{listing}"
    );
    assert!(listing.contains("[fork-1 · m]"), "{listing}");
    type_str(&mut app, "/helpers expand scout");
    app.update(key(KeyCode::Enter));
    assert!(expanded(&app, scout));
    assert!(!expanded(&app, fork));
    type_str(&mut app, "/helpers expand all");
    app.update(key(KeyCode::Enter));
    assert!(expanded(&app, scout) && expanded(&app, fork));
    type_str(&mut app, "/helpers fold all");
    app.update(key(KeyCode::Enter));
    assert!(!expanded(&app, scout) && !expanded(&app, fork));
    type_str(&mut app, "/helpers fold nobody");
    app.update(key(KeyCode::Enter));
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { level: NoticeLevel::Warn, text }) if text.contains("nobody")
    ));
}

#[test]
fn a_reset_drops_the_selection_and_the_view() {
    let (mut app, _, _) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(Msg::ResetTranscript(Vec::new()));
    assert_eq!(app.panel_selection(), None);
    assert_eq!(app.helper_view(), None);
    assert!(!app.status.helper_panel);
}
