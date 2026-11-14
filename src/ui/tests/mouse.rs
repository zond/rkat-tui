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

/// Scrolled back, Ctrl-O keeps the block at the top of the view at the
/// top while everything above it opens.
#[test]
fn ctrl_o_keeps_the_top_line_in_place() {
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
    let rows = |app: &mut App| screen(app, 12);
    let _ = rows(&mut app);
    app.update(key(KeyCode::PageUp));
    let before = rows(&mut app);
    let top = before[0].clone();
    assert!(top.contains(".rs"), "{before:#?}");
    app.update(ctrl('o'));
    let after = rows(&mut app);
    assert_eq!(after[0].replace('▾', "▸"), top, "{after:#?}");
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
