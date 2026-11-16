//! Clicks, drag selection and `/copy`.

use super::*;

#[test]
fn a_click_toggles_one_call_and_ctrl_o_all_of_them() {
    let mut app = App::new(StatusInfo::default());
    app.transcript.push(user("look"));
    for id in ["t1", "t2"] {
        app.update(agent(AgentEvent::ToolCallRequested {
            id: id.into(),
            name: "read_file".into(),
            args: test_args(json!({"path": "a.rs"})),
        }));
        app.update(agent(AgentEvent::ToolExecutionCompleted {
            id: id.into(),
            name: "read_file".into(),
            is_error: false,
            duration_ms: 1,
        }));
        app.update(agent(test_tool_result(id, "a\nb\nc")));
    }
    let (t1, t2) = (
        app.transcript.tool_block("t1").unwrap(),
        app.transcript.tool_block("t2").unwrap(),
    );
    assert!(tool_collapsed(&app, t1) && tool_collapsed(&app, t2));
    // Transcript drawn from screen row 2: user (2 rows), t1 (2), t2 (2).
    draw_rows(&mut app, 2, 60, 20);
    assert_eq!(app.block_at_row(1), None);
    assert_eq!(app.block_at_row(4), Some(t1));
    assert_eq!(app.block_at_row(30), None);

    app.take_dirty();
    click_at(&mut app, 4);
    assert!(!tool_collapsed(&app, t1));
    assert!(tool_collapsed(&app, t2));
    assert!(app.take_dirty());
    // Expanded t1 is 8 rows (line, 3 of arguments, 3 of output, spacer);
    // a click on its output collapses it again.
    draw_rows(&mut app, 2, 60, 20);
    assert_eq!(app.block_at_row(6), Some(t1));
    click_at(&mut app, 6);
    assert!(tool_collapsed(&app, t1));

    // A click on a non-tool block changes nothing and needs no redraw.
    draw_rows(&mut app, 2, 60, 20);
    app.take_dirty();
    click_at(&mut app, 2);
    assert!(!app.take_dirty());
    assert_eq!(app.take_clipboard_text(), None);

    // Ctrl-O shows every call in full, and the reasoning; Ctrl-T (the
    // task checklist) leaves them alone.
    app.update(agent(AgentEvent::ReasoningDelta {
        delta: "hmm".into(),
        assistant_message_id: None,
    }));
    app.update(ctrl('t'));
    assert!(tool_collapsed(&app, t2));
    app.update(ctrl('o'));
    assert!(!tool_collapsed(&app, t1) && !tool_collapsed(&app, t2));
    let reasoning = app
        .transcript
        .iter()
        .find_map(|(id, b)| matches!(b, Block::Reasoning { .. }).then_some(id))
        .unwrap();
    assert!(matches!(
        app.transcript.block(reasoning),
        Some(Block::Reasoning {
            collapsed: false,
            ..
        })
    ));
    // A call arriving now comes shown; a click still folds one.
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "t3".into(),
        name: "glob".into(),
        args: test_args(json!({"pattern": "*"})),
    }));
    let t3 = app.transcript.tool_block("t3").unwrap();
    assert!(!tool_collapsed(&app, t3));
    draw_rows(&mut app, 2, 60, 40);
    click_at(&mut app, 4);
    assert!(tool_collapsed(&app, t1));
    // Ctrl-O again folds them all, the clicked one included.
    app.update(ctrl('o'));
    assert!(tool_collapsed(&app, t1) && tool_collapsed(&app, t2) && tool_collapsed(&app, t3));
    assert!(matches!(
        app.transcript.block(reasoning),
        Some(Block::Reasoning {
            collapsed: true,
            ..
        })
    ));
    app.update(ctrl('o'));
    assert!(!tool_collapsed(&app, t1), "and all open again");
}

/// Thirty read calls, each with a two-line result.
fn thirty_reads() -> App {
    let mut app = App::new(StatusInfo::default());
    for i in 0..30 {
        let id = format!("t{i}");
        app.update(agent(AgentEvent::ToolCallRequested {
            id: id.clone(),
            name: "read_file".into(),
            args: test_args(json!({"path": format!("f{i}.rs")})),
        }));
        app.update(agent(AgentEvent::ToolExecutionCompleted {
            id: id.clone(),
            name: "read_file".into(),
            is_error: false,
            duration_ms: 1,
        }));
        app.update(agent(test_tool_result(&id, "     1\tx\n     2\ty\n")));
    }
    app
}

/// The transcript's rows of a 30-row screen (the last three are the
/// input and the status bar).
fn transcript_rows(app: &mut App) -> Vec<String> {
    screen(app, 30)[..27].to_vec()
}

/// The last tool call's line on screen, as it reads folded.
fn last_call(rows: &[String]) -> String {
    rows.iter()
        .rev()
        .find(|row| row.contains("read f"))
        .cloned()
        .unwrap()
        .replace('▾', "▸")
}

/// Ctrl-O keeps the bottom of the screen where it is while everything
/// above it opens, scrolled back or not: the call at the bottom is still
/// the last one on screen, the bottom row is the same, and folding it all
/// back shows the screen as it was.
#[test]
fn ctrl_o_keeps_the_bottom_line_in_place() {
    for scrolled_back in [true, false] {
        let mut app = thirty_reads();
        let _ = transcript_rows(&mut app);
        if scrolled_back {
            app.update(key(KeyCode::PageUp));
        }
        let before = transcript_rows(&mut app);
        app.update(ctrl('o'));
        let open = transcript_rows(&mut app);
        assert_ne!(open, before, "{open:#?}");
        assert_eq!(last_call(&open), last_call(&before), "{open:#?}");
        app.update(ctrl('o'));
        assert_eq!(
            transcript_rows(&mut app),
            before,
            "scrolled back: {scrolled_back}"
        );
    }
}

/// A click that expands a call keeps the bottom line of the screen where
/// it is (the call opens upwards), and the click that folds it again
/// brings the screen back exactly.
#[test]
fn a_click_keeps_the_bottom_line_and_folding_restores_the_view() {
    let mut app = thirty_reads();
    let _ = transcript_rows(&mut app);
    app.update(key(KeyCode::PageUp));
    let before = transcript_rows(&mut app);
    let at = (18..27).find(|row| before[*row].contains(".rs")).unwrap();
    let clicked = before[at].clone();
    click_at(&mut app, u16::try_from(at).unwrap());
    let open = transcript_rows(&mut app);
    assert_ne!(open, before, "{open:#?}");
    assert_eq!(open.last(), before.last(), "{open:#?}");
    assert_eq!(
        open[at + 1..],
        before[at + 1..],
        "below the call nothing moved"
    );
    // The clicked call's line moved up by the lines it opened.
    let row = open
        .iter()
        .position(|row| row.replace('▾', "▸") == clicked)
        .expect("the clicked call is still on screen");
    assert!(row < at, "{open:#?}");
    click_at(&mut app, u16::try_from(row).unwrap());
    assert_eq!(transcript_rows(&mut app), before);
}

/// The call on the bottom row itself opens upwards: its last line is at
/// the bottom, so what it shows is on screen.
#[test]
fn the_bottom_call_opens_upwards() {
    let mut app = thirty_reads();
    let before = transcript_rows(&mut app);
    let at = (0..27)
        .rev()
        .find(|row| before[*row].contains(".rs"))
        .unwrap();
    click_at(&mut app, u16::try_from(at).unwrap());
    let open = transcript_rows(&mut app);
    let header = open
        .iter()
        .position(|row| row.replace('▾', "▸") == before[at])
        .expect("its line is on screen");
    assert!(header < at, "{open:#?}");
    assert_eq!(open.last(), before.last(), "{open:#?}");
    click_at(&mut app, u16::try_from(header).unwrap());
    assert_eq!(transcript_rows(&mut app), before);
}

/// The user's last message above the view is pinned on the transcript's
/// top row as a reversed bar, cut to one line; the transcript scrolls in
/// the rows under it, a click on the bar toggles nothing, and at the top
/// (the message itself on screen) there is no bar.
#[test]
fn the_last_message_above_the_view_is_pinned_on_the_top_row() {
    let mut app = App::new(StatusInfo::default());
    type_str(
        &mut app,
        "first question about the parser and its many edge cases",
    );
    app.update(key(KeyCode::Enter));
    for i in 0..30 {
        let id = format!("t{i}");
        app.update(agent(AgentEvent::ToolCallRequested {
            id: id.clone(),
            name: "read_file".into(),
            args: test_args(json!({"path": format!("f{i}.rs")})),
        }));
        app.update(agent(AgentEvent::ToolExecutionCompleted {
            id: id.clone(),
            name: "read_file".into(),
            is_error: false,
            duration_ms: 1,
        }));
        app.update(agent(test_tool_result(&id, "     1\tx\n")));
    }
    let rows = transcript_rows(&mut app);
    assert_eq!(
        rows[0].trim_end(),
        " ↑ you: first question about the parser and its many edge cases",
        "{rows:#?}"
    );
    assert!(
        rows[1..].iter().all(|row| !row.contains("↑ you")),
        "{rows:#?}"
    );
    // The bar is no transcript row: the rows under it are, and the bottom
    // of the view is still the transcript's end.
    assert!(app.block_at_row(0).is_none());
    assert!(app.block_at_row(1).is_some());
    let before = transcript_rows(&mut app);
    click_at(&mut app, 0);
    assert_eq!(transcript_rows(&mut app), before);
    // Its style is the bar's, not the transcript's.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
    terminal
        .draw(|f| render::draw_at(f, &mut app, Instant::now()))
        .unwrap();
    let cell = &terminal.backend().buffer()[(1, 0)];
    assert!(cell.modifier.contains(ratatui::style::Modifier::REVERSED));
    let below = &terminal.backend().buffer()[(1, 1)];
    assert!(!below.modifier.contains(ratatui::style::Modifier::REVERSED));
    // Scrolled to the top the message is on screen: no bar.
    app.update(key(KeyCode::Home));
    for _ in 0..40 {
        app.update(key(KeyCode::PageUp));
    }
    let top = transcript_rows(&mut app);
    assert!(top.iter().all(|row| !row.contains("↑ you")), "{top:#?}");
    assert!(top[0].contains("first question"), "{top:#?}");
}

#[test]
fn clicks_are_ignored_under_a_modal() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "t1".into(),
        name: "glob".into(),
        args: test_args(json!({})),
    }));
    app.update(agent(AgentEvent::ToolExecutionCompleted {
        id: "t1".into(),
        name: "glob".into(),
        is_error: false,
        duration_ms: 1,
    }));
    app.update(agent(test_tool_result("t1", "x")));
    let t1 = app.transcript.tool_block("t1").unwrap();
    draw_rows(&mut app, 0, 60, 10);
    app.update(Msg::ShowResumePicker(vec![("a".into(), "a".into())]));
    click_at(&mut app, 0);
    assert!(tool_collapsed(&app, t1));
    app.update(down(0, 0));
    app.update(drag(0, 5));
    app.update(up(0, 5));
    assert!(app.selection().is_none());
    assert_eq!(app.take_clipboard_text(), None);
}

#[test]
fn drag_selects_and_queues_clipboard_text() {
    let mut app = selection_app();
    app.update(down(1, 2));
    assert!(app.selection().is_none());
    app.update(drag(1, 4));
    app.update(drag(3, 7));
    let sel = *app.selection().unwrap();
    assert_eq!(sel.mode, SelectMode::Char);
    assert_eq!(sel.range(), (CellPos::new(0, 2), CellPos::new(2, 7)));
    assert_eq!(app.take_clipboard_text(), None);
    app.update(up(3, 7));
    assert_eq!(
        app.take_clipboard(),
        Some(QueuedCopy {
            text: "hello world\n\n› second".into(),
            truncated: false,
        })
    );
    // The runtime reports how the copy went; the app only queues it.
    assert_eq!(last_notice(&app), None);
    // The selection stays visible after copying.
    assert!(app.selection().is_some());
}

#[test]
fn double_and_triple_click_select_word_and_line() {
    let mut app = selection_app();
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    app.update_at(down(1, 9), at(0));
    app.update_at(up(1, 9), at(10));
    assert_eq!(app.take_clipboard_text(), None);
    app.update_at(down(1, 9), at(100));
    assert_eq!(app.selection().unwrap().mode, SelectMode::Word);
    app.update_at(up(1, 9), at(110));
    assert_eq!(app.take_clipboard_text().as_deref(), Some("world"));
    app.update_at(down(1, 9), at(200));
    assert_eq!(app.selection().unwrap().mode, SelectMode::Line);
    app.update_at(up(1, 9), at(210));
    assert_eq!(app.take_clipboard_text().as_deref(), Some("› hello world"));
    // Too slow, or elsewhere: a plain click again, which clears the selection.
    app.update_at(down(1, 9), at(1000));
    assert!(app.selection().is_none());
    app.update_at(up(1, 9), at(1010));
    app.update_at(down(1, 3), at(1100));
    app.update_at(up(1, 3), at(1110));
    assert_eq!(app.take_clipboard_text(), None);
    // Dragging a double-click selection extends it by words.
    app.update_at(down(3, 4), at(2000));
    app.update_at(up(3, 4), at(2010));
    app.update_at(down(3, 4), at(2100));
    app.update_at(drag(3, 10), at(2150));
    app.update_at(up(3, 10), at(2200));
    assert_eq!(app.take_clipboard_text().as_deref(), Some("second line"));
}

#[test]
fn esc_and_other_keys_clear_the_selection() {
    let mut app = selection_app();
    app.update(down(1, 2));
    app.update(drag(1, 6));
    app.update(up(1, 6));
    assert!(app.selection().is_some());
    app.running = true;
    // Esc only dismisses the selection; it does not interrupt.
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.selection().is_none());
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);

    // So does the first Ctrl-C; the second interrupts.
    app.update(down(1, 2));
    app.update(drag(1, 6));
    app.update(up(1, 6));
    assert!(app.selection().is_some());
    assert!(app.update(ctrl('c')).is_empty());
    assert!(app.selection().is_none());
    assert_eq!(app.update(ctrl('c')), vec![Effect::Interrupt]);

    app.update(down(1, 2));
    app.update(drag(1, 6));
    app.update(up(1, 6));
    app.update(key(KeyCode::Char('x')));
    assert!(app.selection().is_none());
    assert_eq!(app.input.text(), "x");
}

#[test]
fn copy_command_copies_the_nth_latest_answer() {
    let mut app = selection_app();
    submit(&mut app, "/copy");
    assert_eq!(app.take_clipboard_text(), None);
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Info, "nothing to copy".into()))
    );
    app.transcript.push(Block::Assistant {
        md: "first **answer**".into(),
    });
    app.transcript.push(Block::Assistant {
        md: "the *last* answer".into(),
    });
    submit(&mut app, "/copy");
    assert_eq!(
        app.take_clipboard_text().as_deref(),
        Some("the *last* answer")
    );
    submit(&mut app, "/copy 2");
    assert_eq!(
        app.take_clipboard_text().as_deref(),
        Some("first **answer**")
    );
    submit(&mut app, "/copy 3");
    assert_eq!(app.take_clipboard_text(), None);
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Warn, "there are only 2 answers to copy".into()))
    );
    submit(&mut app, "/copy 0");
    assert!(
        matches!(last_notice(&app), Some((NoticeLevel::Warn, t)) if t.starts_with("usage: /copy"))
    );

    // A mouse selection is copied on release; Ctrl-Y no longer copies and,
    // like any other key, clears the selection.
    draw_rows(&mut app, 1, 60, 20);
    app.update(down(1, 2));
    app.update(drag(1, 6));
    app.update(up(1, 6));
    assert_eq!(app.take_clipboard_text().as_deref(), Some("hello"));
    app.update(ctrl('y'));
    assert_eq!(app.take_clipboard_text(), None);
    assert!(app.selection().is_none());
}

#[test]
fn dragging_past_the_edges_scrolls() {
    let mut app = App::new(StatusInfo::default());
    for i in 0..20 {
        app.transcript.push(user(&format!("line {i}")));
    }
    // 40 lines, 10 visible from screen row 2; following the bottom.
    draw_rows(&mut app, 2, 40, 10);
    assert_eq!(app.scroll.offset_from_bottom, 0);
    app.update(down(5, 2));
    app.update(drag(1, 2));
    assert_eq!(app.scroll.offset_from_bottom, 1);
    assert!(!app.scroll.follow);
    // The head is on the line above the first visible one (line 30).
    assert_eq!(app.selection().unwrap().range().0, CellPos::new(29, 2));
    draw_rows(&mut app, 2, 40, 10);
    app.update(drag(0, 2));
    assert_eq!(app.scroll.offset_from_bottom, 2);
    draw_rows(&mut app, 2, 40, 10);
    // Below the transcript (input box rows) scrolls back down.
    app.update(drag(13, 2));
    assert_eq!(app.scroll.offset_from_bottom, 1);
    app.update(up(13, 2));
    assert!(app.take_clipboard_text().is_some());
}
