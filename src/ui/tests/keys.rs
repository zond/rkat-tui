//! Keys: the editor, completion menus, history, chords, Esc/Ctrl-C/Ctrl-D and the loop's own requests.

use super::*;

#[test]
fn ctrl_t_toggles_the_checklist_and_records_the_view() {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(app.checklist_panel(), None);
    let list = Checklist::from_items(vec![ChecklistItem {
        text: "write the fix".into(),
        state: ItemState::InProgress,
    }]);
    app.update(Msg::Checklist(list.clone()));
    assert_eq!(app.checklist(), &list);
    // Hidden by default.
    assert_eq!(app.checklist_panel(), None);
    let fx = app.update(ctrl('t'));
    assert!(
        fx.contains(&Effect::SetChecklistView(ChecklistView::Shown)),
        "{fx:?}"
    );
    assert_eq!(app.checklist_panel(), Some(&list));
    let fx = app.update(ctrl('t'));
    assert!(fx.contains(&Effect::SetChecklistView(ChecklistView::Hidden)));
    assert_eq!(app.checklist_panel(), None);
    // `[ui] checklist = "shown"` starts it shown; an empty list never
    // shows.
    let mut shown = App::new(StatusInfo::default()).with_checklist_view(ChecklistView::Shown);
    assert_eq!(shown.checklist_panel(), None);
    shown.update(Msg::Checklist(list.clone()));
    assert_eq!(shown.checklist_panel(), Some(&list));
    // Ctrl-T types nothing.
    assert!(shown.input.is_empty());
}

#[test]
fn an_unreadable_task_store_is_named_once_at_the_next_ctrl_t() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::ChecklistUnavailable("database is locked".into()));
    let notices = |app: &App| {
        app.transcript
            .blocks()
            .iter()
            .filter(
                |b| matches!(b, Block::Notice { text, .. } if text.contains("database is locked")),
            )
            .count()
    };
    assert_eq!(notices(&app), 0);
    app.update(ctrl('t'));
    assert_eq!(notices(&app), 1);
    app.update(ctrl('t'));
    assert_eq!(notices(&app), 1);
}

#[test]
fn ctrl_underscore_undoes_input_edits() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "first words");
    app.update(ctrl('w'));
    assert_eq!(app.input.text(), "first ");
    app.update(ctrl('_'));
    assert_eq!(app.input.text(), "first words");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('7'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.input.text(), "first");
    // Ctrl-C clears the input; undo brings it back.
    app.update(ctrl('c'));
    assert!(app.input.is_empty());
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('-'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    )));
    assert_eq!(app.input.text(), "first");
}

#[test]
fn kill_and_yank_keys_edit_the_input() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "keep this text");
    app.update(ctrl('w'));
    assert_eq!(app.input.text(), "keep this ");
    app.update(ctrl('a'));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::ALT,
    )));
    assert_eq!(app.input.text(), " this ");
    app.update(ctrl('k'));
    assert_eq!(app.input.text(), "");
    // Alt-D then Ctrl-K was one run of kills: one entry.
    app.update(ctrl('y'));
    assert_eq!(app.input.text(), "keep this ");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::ALT,
    )));
    assert_eq!(app.input.text(), "text");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::ALT,
    )));
    assert_eq!(app.input.text(), "");
    app.update(ctrl('u'));
    app.update(ctrl('y'));
    assert_eq!(app.input.text(), "text");
}

#[test]
fn esc_esc_clears_the_draft_into_history() {
    let mut app = App::new(StatusInfo::default());
    let t0 = Instant::now();
    type_str(&mut app, "a draft");
    // One Esc does nothing but arm the clear.
    assert!(app.update_at(key(KeyCode::Esc), t0).is_empty());
    assert_eq!(app.input.text(), "a draft");
    assert!(app.esc_clear_armed(t0));
    // The second within the window clears and saves it to history.
    let t1 = t0 + Duration::from_millis(500);
    assert!(app.update_at(key(KeyCode::Esc), t1).is_empty());
    assert!(app.input.is_empty());
    assert!(!app.esc_clear_armed(t1));
    assert_eq!(app.take_submitted(), vec!["a draft".to_string()]);
    app.update_at(key(KeyCode::Up), t1);
    assert_eq!(app.input.text(), "a draft");
    // Too slow: the second Esc only arms again.
    let t2 = t1 + Duration::from_secs(10);
    app.update_at(key(KeyCode::Esc), t2);
    let late = t2 + DOUBLE_ESC_WINDOW + Duration::from_millis(1);
    app.update_at(key(KeyCode::Esc), late);
    assert_eq!(app.input.text(), "a draft");
    // Another key in between disarms it.
    app.update_at(key(KeyCode::Left), late);
    app.update_at(key(KeyCode::Esc), late);
    assert_eq!(app.input.text(), "a draft");
    assert!(app.take_submitted().is_empty());
    // An empty input: Esc Esc does nothing (there is no rewind), arms
    // nothing and saves nothing to history.
    let mut app = App::new(StatusInfo::default());
    assert!(app.update_at(key(KeyCode::Esc), t0).is_empty());
    assert!(!app.esc_clear_armed(t0));
    assert!(app.update_at(key(KeyCode::Esc), t0).is_empty());
    assert!(app.take_submitted().is_empty());
}

#[test]
fn ctrl_c_interrupts_running_turn_and_keeps_pending() {
    let mut app = running_app();
    submit(&mut app, "two");
    assert_eq!(app.update(ctrl('c')), vec![Effect::Interrupt]);
    assert_eq!(app.status.queued, 1);
    assert!(app.input.is_empty());
}

#[test]
fn ctrl_c_clears_input_then_double_press_quits() {
    let mut app = App::new(StatusInfo::default());
    let t0 = Instant::now();
    type_str(&mut app, "draft");
    assert!(app.update_at(ctrl('c'), t0).is_empty());
    assert!(app.input.is_empty());
    assert!(app.update_at(ctrl('c'), t0).is_empty());
    assert!(app.quit_armed(t0));
    // Too late: re-arms instead of quitting.
    let late = t0 + Duration::from_secs(2);
    assert!(app.update_at(ctrl('c'), late).is_empty());
    let soon = late + Duration::from_millis(500);
    assert_eq!(app.update_at(ctrl('c'), soon), vec![Effect::Quit]);
}

#[test]
fn the_slash_menu_filters_moves_completes_and_closes() {
    let mut app = App::new(StatusInfo::default()).with_history(vec!["older".into()]);
    type_str(&mut app, "/he");
    assert_eq!(menu_rows(&app), ["/help", "/helpers"]);
    assert_eq!(app.menu().unwrap().selected, 0);
    // ↓ moves the highlight (not history); Tab inserts it and a space,
    // which closes the menu.
    app.update(key(KeyCode::Down));
    assert_eq!(app.menu().unwrap().selected, 1);
    app.update(key(KeyCode::Up));
    app.update(key(KeyCode::Up));
    assert_eq!(app.menu().unwrap().selected, 1, "wraps");
    assert_eq!(app.input.text(), "/he");
    assert!(app.update(key(KeyCode::Tab)).is_empty());
    assert_eq!(app.input.text(), "/helpers ");
    assert!(app.menu().is_none());
    // Ctrl-_ undoes the completion; the menu comes back for the word.
    app.update(ctrl('_'));
    assert_eq!(app.input.text(), "/he");
    assert_eq!(menu_rows(&app), ["/help", "/helpers"]);
    // Enter on a partial name completes it instead of sending.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.input.text(), "/help ");
    assert!(app.take_submitted().is_empty());
    // Enter on a full name sends, args and all.
    app.input.clear();
    type_str(&mut app, "/stat");
    assert_eq!(menu_rows(&app), ["/status", "/stats"]);
    type_str(&mut app, "us");
    assert_eq!(menu_rows(&app), ["/status"]);
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::StatusReport]);
    assert!(app.input.is_empty());
    // An alias completes as itself; a single match completes on Tab.
    type_str(&mut app, "/tas");
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/tasks ");
    // Tab before existing args keeps them and steps over the space.
    app.input.clear();
    type_str(&mut app, "/mod gpt-5.5");
    for _ in 0.."gpt-5.5 ".len() {
        app.update(key(KeyCode::Left));
    }
    assert_eq!(menu_rows(&app), ["/mode", "/model"]);
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "/model gpt-5.5");
    assert_eq!(app.input.cursor(), (0, 7));
    assert!(app.menu().is_none());
}

/// The UI never walks the workspace itself: an `@` word asks the loop
/// for the listing once, the menu fills when it arrives, and an answer
/// to a request the menu no longer waits for is dropped.
#[test]
fn the_path_listing_is_asked_for_once_and_a_stale_answer_is_dropped() {
    let (_dir, mut app) = mention_app();
    type_str(&mut app, "look at @src");
    let requests: Vec<u64> = app
        .take_local()
        .into_iter()
        .filter_map(|l| match l {
            LocalEffect::IndexPaths { generation, .. } => Some(generation),
            _ => None,
        })
        .collect();
    assert_eq!(requests, [1]);
    assert!(app.menu().is_none());
    // The word ends before the answer arrives: the answer is stale.
    type_str(&mut app, " ");
    let root = app.workspace.clone().unwrap();
    app.update(Msg::PathIndex {
        generation: 1,
        paths: crate::mentions::workspace_paths(&root),
    });
    type_str(&mut app, "@src");
    assert!(app.menu().is_none());
    answer_index(&mut app);
    assert_eq!(
        menu_rows(&app),
        ["@src/", "@src/main.rs", "@src/ui/", "@src/ui/render.rs"]
    );
}

/// A `@word` that matches nothing is looked up once per word and index:
/// the messages that follow (ticks, stream events) do not scan the path
/// index again, a new word or the index arriving does.
#[test]
fn a_word_with_no_match_is_not_looked_up_again_on_every_message() {
    let (_dir, mut app) = mention_app();
    type_str(&mut app, "see @nosuchfile");
    answer_index(&mut app);
    assert!(app.menu().is_none());
    let after_index = app.menu_lookups;
    for _ in 0..50 {
        app.update(Msg::Tick);
    }
    assert_eq!(app.menu_lookups, after_index, "ticks scanned the index");
    // Another word is looked up.
    type_str(&mut app, "x");
    assert_eq!(app.menu_lookups, after_index + 1);
    app.update(Msg::Tick);
    assert_eq!(app.menu_lookups, after_index + 1);
    // A word that matches opens its menu again.
    for _ in 0.."nosuchfilex".len() {
        app.update(key(KeyCode::Backspace));
    }
    type_str(&mut app, "src");
    assert!(!menu_rows(&app).is_empty());
}

#[test]
fn the_at_menu_completes_paths_and_stays_open_for_directories() {
    let (_dir, mut app) = mention_app();
    type_str(&mut app, "look at @");
    answer_index(&mut app);
    assert_eq!(
        menu_rows(&app),
        [
            "@README.md",
            "@src/",
            "@src/main.rs",
            "@src/ui/",
            "@src/ui/render.rs"
        ]
    );
    type_str(&mut app, "s");
    assert_eq!(
        menu_rows(&app),
        ["@src/", "@src/main.rs", "@src/ui/", "@src/ui/render.rs"]
    );
    // A directory completes without a space and the menu lists its
    // contents.
    app.update(key(KeyCode::Tab));
    assert_eq!(app.input.text(), "look at @src/");
    assert_eq!(
        menu_rows(&app),
        ["@src/main.rs", "@src/ui/", "@src/ui/render.rs"]
    );
    // Enter completes the highlighted file and a space; the menu closes.
    app.update(key(KeyCode::Enter));
    assert_eq!(app.input.text(), "look at @src/main.rs ");
    assert!(app.menu().is_none());
    assert!(app.take_submitted().is_empty());
    // A name fragment matches by substring; Enter on a fully typed file
    // sends.
    app.input.clear();
    type_str(&mut app, "@rend");
    answer_index(&mut app);
    assert_eq!(menu_rows(&app), ["@src/ui/render.rs"]);
    app.input.clear();
    type_str(&mut app, "see @README.md");
    answer_index(&mut app);
    assert_eq!(menu_rows(&app), ["@README.md"]);
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::StartTurn("see @README.md".into())]
    );
    // Sealed paths are not offered; e-mail addresses open nothing.
    type_str(&mut app, "@.rk");
    assert!(app.menu().is_none());
    app.input.clear();
    type_str(&mut app, "mail a@READ");
    assert!(app.menu().is_none());
}

#[test]
fn esc_closes_the_slash_menu_until_the_word_changes() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "/co");
    assert_eq!(menu_rows(&app), ["/compact", "/context", "/copy", "/cost"]);
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.menu().is_none());
    assert_eq!(app.input.text(), "/co", "Esc only closes the menu");
    // Without the menu, ↑ is history again and Enter sends.
    app.update(agent(AgentEvent::TextDelta {
        delta: "x".into(),
        assistant_message_id: None,
    }));
    assert!(app.menu().is_none(), "other messages keep it closed");
    type_str(&mut app, "p");
    assert_eq!(menu_rows(&app), ["/copy"]);
    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.take_submitted(), ["/cop"]);
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Warn, "unknown command /cop (try /help)".into()))
    );
    // No menu: no match, `//`, a second line, the cursor past the
    // first word.
    for text in ["/zz", "//he", "hi /he"] {
        app.input.clear();
        type_str(&mut app, text);
        assert!(app.menu().is_none(), "{text}");
    }
    app.input.clear();
    type_str(&mut app, "/he");
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    assert!(app.menu().is_none(), "multi-line input");
    app.input.clear();
    type_str(&mut app, "/help x");
    assert!(app.menu().is_none());
}

#[test]
fn question_mark_on_an_empty_input_toggles_the_keys_panel() {
    let mut app = App::new(StatusInfo::default());
    assert!(app.update(key(KeyCode::Char('?'))).is_empty());
    assert_eq!(app.modal, Some(Modal::Keys));
    // A held `?` keeps it open; `?` again closes it and is not typed.
    let mut held = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    held.kind = KeyEventKind::Repeat;
    app.update(Msg::Key(held));
    assert_eq!(app.modal, Some(Modal::Keys));
    app.update(key(KeyCode::Char('?')));
    assert!(app.modal.is_none());
    assert!(app.input.is_empty());
    // Esc closes it, and so does any other key, which is not typed
    // (Enter does not send).
    for close in [KeyCode::Esc, KeyCode::Char('x'), KeyCode::Enter] {
        app.update(key(KeyCode::Char('?')));
        assert!(app.update(key(close)).is_empty(), "{close:?}");
        assert!(app.modal.is_none(), "{close:?}");
        assert!(app.input.is_empty(), "{close:?}");
    }
    // With text in the input `?` is just typed.
    type_str(&mut app, "why?");
    assert!(app.modal.is_none());
    assert_eq!(app.input.text(), "why?");
}

#[test]
fn the_keys_panel_gives_way_to_escalations_and_pickers() {
    let mut app = App::new(StatusInfo::default());
    app.update(key(KeyCode::Char('?')));
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    assert!(matches!(app.modal, Some(Modal::Escalation { .. })));
    app.update(key(KeyCode::Char('y')));
    assert!(app.modal.is_none(), "the panel is not parked");
    app.update(key(KeyCode::Char('?')));
    app.update(Msg::ShowResumePicker(vec![("a".into(), "x".into())]));
    assert!(matches!(app.modal, Some(Modal::Resume(_))));
    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Char('?')));
    app.update(Msg::ShowModelPicker(picker_models()));
    assert!(matches!(app.modal, Some(Modal::Model(_))));
}

#[test]
fn ctrl_d_quits_only_on_empty_input() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "ab");
    app.update(key(KeyCode::Left));
    assert!(app.update(ctrl('d')).is_empty());
    assert_eq!(app.input.text(), "a");
    app.update(key(KeyCode::Backspace));
    // Empty and idle: the first press only arms the quit.
    let t0 = Instant::now();
    assert!(app.update_at(ctrl('d'), t0).is_empty());
    assert!(app.quit_confirm_armed(t0));
    assert!(app.transcript.blocks().is_empty(), "no notice, only a hint");
    assert_eq!(app.update_at(ctrl('d'), t0), vec![Effect::Quit]);
    // Too late: it arms again.
    let mut app = App::new(StatusInfo::default());
    app.update_at(ctrl('d'), t0);
    let late = t0 + QUIT_CONFIRM_WINDOW;
    assert!(app.update_at(ctrl('d'), late).is_empty());
    assert_eq!(app.update_at(ctrl('d'), late), vec![Effect::Quit]);
}

#[test]
fn ctrl_o_toggles_reasoning() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::ReasoningDelta {
        delta: "think".into(),
        assistant_message_id: None,
    }));
    app.update(ctrl('o'));
    assert!(matches!(
        app.transcript.blocks()[0],
        Block::Reasoning {
            collapsed: false,
            ..
        }
    ));
}

#[test]
fn ctrl_o_toggles_the_last_reasoning_and_tool_output_together() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::ReasoningDelta {
        delta: "think".into(),
        assistant_message_id: None,
    }));
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "t1".into(),
        name: "read_file".into(),
        args: test_args(json!({"path": "a.rs"})),
    }));
    app.update(agent(test_tool_result("t1", "a\nb")));
    let tool = app.transcript.tool_block("t1").unwrap();
    assert!(reasoning_collapsed(&app) && tool_collapsed(&app, tool));
    app.update(ctrl('o'));
    assert!(!reasoning_collapsed(&app) && !tool_collapsed(&app, tool));
    app.update(ctrl('o'));
    assert!(reasoning_collapsed(&app) && tool_collapsed(&app, tool));
    // One expanded, one collapsed (a click opened the tool): both expand.
    app.transcript.toggle_tool(tool);
    app.update(ctrl('o'));
    assert!(!reasoning_collapsed(&app) && !tool_collapsed(&app, tool));
}

#[test]
fn newline_variants_and_paste() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "a");
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    type_str(&mut app, "b");
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
    app.update(ctrl('j'));
    app.update(Msg::Paste("x\ny".into()));
    assert_eq!(app.input.text(), "a\nb\n\nx\ny");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::StartTurn("a\nb\n\nx\ny".into())]
    );
    // A backslash right before the cursor at the end of a line: Enter
    // replaces it with a newline instead of sending.
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "one\\");
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    type_str(&mut app, "two");
    assert_eq!(app.input.text(), "one\ntwo");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::StartTurn("one\ntwo".into())]
    );
}

#[test]
fn ctrl_enter_sends_now() {
    let ctrl_enter = || Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    let mut app = App::new(StatusInfo::default());
    // Idle: the same as Enter.
    type_str(&mut app, "first");
    assert_eq!(
        app.update(ctrl_enter()),
        vec![Effect::StartTurn("first".into())]
    );
    // Running: interrupt and send the input next.
    type_str(&mut app, "next");
    assert_eq!(app.update(ctrl_enter()), vec![Effect::Interrupt]);
    assert_eq!(app.send_next.as_deref(), Some("next"));
    // Running with an empty input: a plain interrupt.
    assert_eq!(app.update(ctrl_enter()), vec![Effect::Interrupt]);
    // A trailing backslash does not turn Ctrl+Enter into a newline.
    type_str(&mut app, "x\\");
    assert_eq!(app.update(ctrl_enter()), vec![Effect::Interrupt]);
}

#[test]
fn scroll_follow_semantics() {
    let mut s = ScrollState::default();
    assert_eq!(s.window(100, 10), 90);
    s.up(5);
    assert!(!s.follow);
    assert_eq!(s.window(100, 10), 85);
    // New output does not move the view while scrolled back.
    assert_eq!(s.window(110, 10), 85);
    s.down(100);
    assert!(s.follow);
    assert_eq!(s.window(120, 10), 110);
    s.top();
    assert_eq!(s.window(120, 10), 0);
    s.bottom();
    assert!(s.follow);
    assert_eq!(s.window(120, 10), 110);
    // Short transcripts never scroll.
    let mut short = ScrollState::default();
    short.up(3);
    assert_eq!(short.window(4, 10), 0);
    assert!(short.follow);
}

#[test]
fn page_and_end_keys_drive_scroll_state() {
    let mut app = App::new(StatusInfo::default());
    app.set_viewport_height(12);
    app.update(key(KeyCode::PageUp));
    assert_eq!(app.scroll.offset_from_bottom, 10);
    assert!(!app.scroll.follow);
    app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)));
    assert_eq!(app.scroll.offset_from_bottom, 7);
    app.update(key(KeyCode::End));
    assert!(app.scroll.follow);
    assert_eq!(app.scroll.offset_from_bottom, 0);
}

#[test]
fn quitting_a_busy_attached_ui_promises_only_what_holds_either_way() {
    let t0 = Instant::now();
    let mut app = running_app();
    app.update_at(ctrl('d'), t0);
    assert!(matches!(
        last_notice(&app),
        Some((NoticeLevel::Warn, text)) if text.contains("the turn is interrupted")
    ));
    let mut app = running_app();
    app.update(Msg::Status(StatusUpdate::Connection(
        crate::ui::Connection::Attached {
            address: "127.0.0.1:1".into(),
            daemon_build: None,
        },
    )));
    app.update_at(ctrl('d'), t0);
    assert!(matches!(
        last_notice(&app),
        Some((NoticeLevel::Warn, text))
            if text.contains("messages the daemon has not received are saved")
                && !text.contains("interrupted")
    ));
}

#[test]
fn quit_while_busy_needs_confirmation_and_hands_over_held_messages() {
    let mut app = running_app();
    let t0 = Instant::now();
    submit(&mut app, "two");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    // First Ctrl-D warns and arms.
    assert!(app.update_at(ctrl('d'), t0).is_empty());
    assert!(app.quit_confirm_armed(t0));
    assert!(matches!(
        last_notice(&app),
        Some((NoticeLevel::Warn, text))
            if text.contains("a turn is running") && text.contains("1 message queued")
    ));
    // The window expires on a tick; the next press only re-arms.
    app.update_at(Msg::Tick, t0 + QUIT_CONFIRM_WINDOW);
    assert!(app.quit_confirm.is_none());
    assert!(
        app.update_at(ctrl('d'), t0 + QUIT_CONFIRM_WINDOW)
            .is_empty()
    );
    assert!(app.quit_confirm_armed(t0 + QUIT_CONFIRM_WINDOW));
    // Ctrl-D then /quit (typing does not disarm) within the window quits,
    // handing over the text.
    let t1 = t0 + Duration::from_secs(10);
    assert!(app.update_at(ctrl('d'), t1).is_empty());
    for c in "/quit".chars() {
        app.update_at(key(KeyCode::Char(c)), t1);
    }
    assert_eq!(
        app.update_at(key(KeyCode::Enter), t1 + Duration::from_secs(1)),
        vec![
            Effect::SaveUnsent(vec![(Some(PendingId(1)), "two".into())]),
            Effect::Quit
        ]
    );
}

#[test]
fn idle_quit_is_immediate() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "/quit");
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::Quit]);
}

#[test]
fn alt_p_opens_the_model_picker_and_keeps_the_draft() {
    let mut app = app_on_sonnet();
    type_str(&mut app, "half a thought");
    let alt_p = Msg::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
    assert_eq!(
        app.update(alt_p.clone()),
        vec![Effect::RequestModelPicker { refresh: false }]
    );
    assert_eq!(app.input.text(), "half a thought");
    assert!(app.take_submitted().is_empty());
    app.update(Msg::ShowModelPicker(picker_models()));
    // Typing filters the picker, not the draft; Esc closes it.
    type_str(&mut app, "op");
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.modal.is_none());
    assert_eq!(app.input.text(), "half a thought");
    // Picking a model keeps it too.
    app.update(alt_p.clone());
    app.update(Msg::ShowModelPicker(picker_models()));
    app.update(key(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::SetModel("anthropic:claude-opus-4-6".into())]
    );
    assert_eq!(app.input.text(), "half a thought");
    // While a turn runs it is refused, as /model is.
    let mut running = running_app();
    assert!(running.update(alt_p).is_empty());
}

#[test]
fn repeated_enter_and_esc_do_not_act_in_modals() {
    let repeat = |code: KeyCode| {
        let mut k = KeyEvent::new(code, KeyModifiers::NONE);
        k.kind = KeyEventKind::Repeat;
        Msg::Key(k)
    };
    let mut app = app_on_sonnet();
    app.update(Msg::ShowModelPicker(picker_models()));
    assert!(app.update(repeat(KeyCode::Enter)).is_empty());
    assert!(app.update(repeat(KeyCode::Esc)).is_empty());
    assert!(matches!(app.modal, Some(Modal::Model(_))));
    // Held arrows still move.
    app.update(repeat(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::SetModel("anthropic:claude-opus-4-6".into())]
    );
    assert!(app.modal.is_none());
    // A held answer key does not answer an escalation.
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    assert!(app.update(repeat(KeyCode::Char('y'))).is_empty());
    assert!(app.update(repeat(KeyCode::Esc)).is_empty());
    assert!(matches!(app.modal, Some(Modal::Escalation { .. })));
}

#[test]
fn history_recall_with_up() {
    let mut app = App::new(StatusInfo::default()).with_history(vec!["old".into()]);
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "old");
}

#[test]
fn ctrl_p_and_ctrl_n_are_up_and_down() {
    let mut app = App::new(StatusInfo::default()).with_history(vec!["one".into(), "two".into()]);
    type_str(&mut app, "a");
    app.update(ctrl('j'));
    type_str(&mut app, "b");
    // A row up first, then history.
    app.update(ctrl('p'));
    assert_eq!(app.input.cursor(), (0, 1));
    app.update(ctrl('p'));
    assert_eq!(app.input.text(), "two");
    app.update(ctrl('p'));
    assert_eq!(app.input.text(), "one");
    app.update(ctrl('n'));
    assert_eq!(app.input.text(), "two");
    app.update(ctrl('n'));
    assert_eq!(app.input.text(), "a\nb");
    // On an empty input with a held message, Ctrl-P edits it like ↑.
    let mut app = running_app();
    submit(&mut app, "held");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    assert!(app.input.is_empty());
    app.update(ctrl('p'));
    assert_eq!(app.input.text(), "held");
}

#[test]
fn up_moves_a_visual_row_within_a_wrapped_line_keeping_the_column() {
    let mut app = App::new(StatusInfo::default()).with_history(vec!["old".into()]);
    app.input.set_wrap_width(10);
    type_str(&mut app, "abcdefghijklmnopqrstuvwxy");
    app.update(key(KeyCode::Left));
    app.update(key(KeyCode::Left));
    assert_eq!(app.input.cursor(), (0, 23));
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.cursor(), (0, 13));
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.cursor(), (0, 3));
    assert_eq!(app.input.text(), "abcdefghijklmnopqrstuvwxy");
    // On the first visual row Up recalls history as before.
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "old");
}

#[test]
fn down_moves_a_visual_row_within_a_wrapped_line_then_recalls_newer_history() {
    let mut app =
        App::new(StatusInfo::default()).with_history(vec!["abcdefghijklmnopqrstuvwxy".into()]);
    app.input.set_wrap_width(10);
    type_str(&mut app, "dra");
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "abcdefghijklmnopqrstuvwxy");
    assert_eq!(app.input.cursor(), (0, 25));
    app.update(key(KeyCode::Up));
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.cursor(), (0, 5));
    app.update(key(KeyCode::Down));
    assert_eq!(app.input.cursor(), (0, 15));
    app.update(key(KeyCode::Down));
    assert_eq!(app.input.cursor(), (0, 25));
    // On the last visual row Down recalls newer history (the draft).
    app.update(key(KeyCode::Down));
    assert_eq!(app.input.text(), "dra");
}

#[test]
fn take_submitted_drains_prompts_and_commands() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "hello");
    app.update(key(KeyCode::Enter));
    app.update(key(KeyCode::Enter)); // blank: ignored
    type_str(&mut app, "/help");
    app.update(key(KeyCode::Enter));
    assert_eq!(
        app.take_submitted(),
        vec!["hello".to_string(), "/help".to_string()]
    );
    assert!(app.take_submitted().is_empty());
}

#[test]
fn ctrl_r_enters_search_and_enter_accepts() {
    let mut app = App::new(StatusInfo::default())
        .with_history(vec!["cargo build".into(), "git status".into()])
        .with_global_history(vec!["cargo test --global".into(), "git status".into()]);
    type_str(&mut app, "draft");
    app.update(ctrl('r'));
    assert!(matches!(app.mode, InputMode::Search(_)));
    let (_, corpus) = app.search().unwrap();
    assert_eq!(corpus, ["git status", "cargo build", "cargo test --global"]);
    type_str(&mut app, "cargo");
    let (s, corpus) = app.search().unwrap();
    assert_eq!(s.current(corpus), Some("cargo build"));
    app.update(ctrl('r'));
    let (s, corpus) = app.search().unwrap();
    assert_eq!(s.current(corpus), Some("cargo test --global"));
    // Enter accepts into the editor without submitting.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.mode, InputMode::Normal);
    assert_eq!(app.input.text(), "cargo test --global");
    assert_eq!(app.input.cursor(), (0, 19));
    assert!(app.take_submitted().is_empty());
    // Searching while a turn runs is fine.
    app.update(key(KeyCode::Enter));
    assert!(app.is_running());
    app.update(ctrl('r'));
    type_str(&mut app, "git");
    app.update(key(KeyCode::End));
    assert_eq!(app.input.text(), "git status");
}

#[test]
fn esc_cancels_search_restores_input() {
    let mut app = App::new(StatusInfo::default()).with_history(vec!["hello".into()]);
    type_str(&mut app, "draft");
    app.update(ctrl('r'));
    type_str(&mut app, "hel");
    assert_eq!(app.input.text(), "draft");
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert_eq!(app.mode, InputMode::Normal);
    assert_eq!(app.input.text(), "draft");
    // Ctrl-C cancels too and does not arm quit / clear the input.
    app.update(ctrl('r'));
    assert!(app.update(ctrl('c')).is_empty());
    assert_eq!(app.input.text(), "draft");
    assert!(!app.quit_armed(Instant::now()));
}

#[test]
fn chord_ctrl_x_ctrl_s_interrupt_and_send_when_running() {
    let mut app = App::new(StatusInfo::default());
    let t0 = Instant::now();
    // Idle: behaves like Enter.
    type_str(&mut app, "first");
    assert!(app.update_at(ctrl('x'), t0).is_empty());
    assert!(app.chord_pending(t0));
    assert_eq!(
        app.update_at(ctrl('s'), t0),
        vec![Effect::StartTurn("first".into())]
    );
    assert!(app.chord.is_none());
    // Running with input: interrupt and send.
    type_str(&mut app, "next");
    app.update_at(ctrl('x'), t0);
    assert_eq!(app.update_at(ctrl('s'), t0), vec![Effect::Interrupt]);
    assert!(app.input.is_empty());
    assert_eq!(
        app.take_submitted(),
        vec!["first".to_string(), "next".into()]
    );
    assert_eq!(app.input.history().last().map(String::as_str), Some("next"));
    // Running with an empty editor: plain interrupt.
    app.update_at(ctrl('x'), t0);
    assert_eq!(app.update_at(ctrl('s'), t0), vec![Effect::Interrupt]);
    // Esc cancels the chord without interrupting.
    app.update_at(ctrl('x'), t0);
    assert!(app.update_at(key(KeyCode::Esc), t0).is_empty());
    assert!(app.chord.is_none());
    // Any other key cancels the chord and is processed normally.
    app.update_at(ctrl('x'), t0);
    app.update_at(key(KeyCode::Char('a')), t0);
    assert!(app.chord.is_none());
    assert_eq!(app.input.text(), "a");
    assert!(app.update_at(ctrl('s'), t0).is_empty());
}

#[test]
fn chord_expires_after_window() {
    let mut app = App::new(StatusInfo::default());
    let t0 = Instant::now();
    type_str(&mut app, "x");
    app.update_at(ctrl('x'), t0);
    let late = t0 + CHORD_WINDOW + Duration::from_millis(1);
    assert!(!app.chord_pending(late));
    // Too late for the chord: a bare Ctrl-S, which stashes.
    assert!(app.update_at(ctrl('s'), late).is_empty());
    assert!(app.input.is_empty() && app.input.has_stash());
    assert!(!app.is_running());
    // A tick after the window clears the pending chord.
    app.update_at(ctrl('x'), t0);
    app.update_at(Msg::Tick, late);
    assert!(app.chord.is_none());
}

#[test]
fn shift_tab_cycles_the_permission_mode() {
    let back_tab = || Msg::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    let mut app = App::new(StatusInfo::default());
    // Before the runner reports a mode the engine is in its default, auto.
    assert_eq!(
        app.update(back_tab()),
        vec![Effect::SetMode(PermissionMode::Manual)]
    );
    // The runner answers with the new mode; each press moves one on.
    for (now, next) in [
        (PermissionMode::Manual, PermissionMode::AcceptEdits),
        (PermissionMode::AcceptEdits, PermissionMode::Plan),
        (PermissionMode::Plan, PermissionMode::Auto),
        (PermissionMode::Auto, PermissionMode::Manual),
    ] {
        app.status.apply(StatusUpdate::Mode {
            mode: now,
            paused: false,
            denials: crate::approval::desk::Denials {
                consecutive: 0,
                consecutive_limit: 3,
                total: 0,
                total_limit: 20,
            },
        });
        assert_eq!(
            app.update(back_tab()),
            vec![Effect::SetMode(next)],
            "{now:?}"
        );
    }
    // A paused auto mode is auto mode.
    app.status.apply(StatusUpdate::Mode {
        mode: PermissionMode::Auto,
        paused: true,
        denials: crate::approval::desk::Denials {
            consecutive: 3,
            consecutive_limit: 3,
            total: 3,
            total_limit: 20,
        },
    });
    assert_eq!(
        app.update(back_tab()),
        vec![Effect::SetMode(PermissionMode::Manual)]
    );
    // It works while a turn runs too.
    submit(&mut app, "work");
    assert!(app.is_running());
    app.status.apply(StatusUpdate::Mode {
        mode: PermissionMode::Plan,
        paused: false,
        denials: crate::approval::desk::Denials {
            consecutive: 0,
            consecutive_limit: 3,
            total: 0,
            total_limit: 20,
        },
    });
    assert_eq!(
        app.update(back_tab()),
        vec![Effect::SetMode(PermissionMode::Auto)]
    );
}

#[test]
fn ctrl_z_requests_a_suspend() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "draft");
    assert!(app.update(ctrl('z')).is_empty());
    assert_eq!(loop_requests(&mut app), vec![LocalEffect::Suspend]);
    assert_eq!(app.input.text(), "draft");
    let mut running = running_app();
    assert!(running.update(ctrl('z')).is_empty());
    assert_eq!(loop_requests(&mut running), vec![LocalEffect::Suspend]);
}

/// One drain carries everything the loop does itself, in order, and
/// leaves nothing behind.
#[test]
fn take_local_drains_every_loop_effect_once() {
    let mut app = App::new(StatusInfo::default()).with_bell(true);
    assert_eq!(app.take_local(), vec![LocalEffect::Draw]);
    assert!(app.take_local().is_empty());
    submit(&mut app, "hi");
    app.update(Msg::Focus(false));
    app.update(finished(Ok(summary())));
    app.update(ctrl('l'));
    let local = app.take_local();
    assert_eq!(
        local,
        vec![
            LocalEffect::Submitted("hi".into()),
            LocalEffect::Redraw,
            LocalEffect::Bell,
            LocalEffect::Draw,
        ]
    );
    assert!(app.take_local().is_empty());
}

#[test]
fn ctrl_g_and_ctrl_x_ctrl_e_edit_the_draft_externally() {
    let mut app = App::new(StatusInfo::default());
    let t0 = Instant::now();
    type_str(&mut app, "draft");
    assert!(app.update_at(ctrl('g'), t0).is_empty());
    assert_eq!(
        loop_requests(&mut app),
        vec![LocalEffect::ExternalEditor(EditorTarget::Draft(
            "draft".into()
        ))]
    );
    assert_eq!(app.input.text(), "draft");
    assert!(app.update_at(ctrl('x'), t0).is_empty());
    assert!(app.update_at(ctrl('e'), t0).is_empty());
    assert_eq!(
        loop_requests(&mut app),
        vec![LocalEffect::ExternalEditor(EditorTarget::Draft(
            "draft".into()
        ))]
    );
    assert!(app.chord.is_none());
    // Bare Ctrl-E still goes to the line end.
    assert!(app.update_at(ctrl('e'), t0).is_empty());
    // While a turn runs too: it only edits the input.
    let mut running = running_app();
    type_str(&mut running, "steer");
    assert!(running.update(ctrl('g')).is_empty());
    assert_eq!(
        loop_requests(&mut running),
        vec![LocalEffect::ExternalEditor(EditorTarget::Draft(
            "steer".into()
        ))]
    );
}

#[test]
fn the_edited_draft_replaces_the_input_and_a_failure_keeps_it() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "old");
    assert!(
        app.update(Msg::EditorClosed(EditorClosed::Draft(Ok(
            "new\ntext".into()
        ))))
        .is_empty()
    );
    assert_eq!(app.input.text(), "new\ntext");
    // Ctrl-_ brings the old draft back.
    app.update(ctrl('_'));
    assert_eq!(app.input.text(), "old");
    app.update(Msg::EditorClosed(EditorClosed::Draft(Err(
        "vi exited with 1".into(),
    ))));
    assert_eq!(app.input.text(), "old");
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "the input is unchanged: vi exited with 1".into()
        ))
    );
}

#[test]
fn ctrl_l_requests_a_redraw() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "kept");
    assert!(app.update(ctrl('l')).is_empty());
    assert_eq!(loop_requests(&mut app), vec![LocalEffect::Redraw]);
    assert_eq!(app.input.text(), "kept");
}

#[test]
fn ctrl_s_alone_stashes_and_restores() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "text");
    assert!(app.update(ctrl('s')).is_empty());
    assert!(app.input.is_empty());
    assert!(app.input.has_stash());
    assert!(!app.is_running());
    // Send something else meanwhile, then restore.
    assert_eq!(
        submit(&mut app, "quick question"),
        vec![Effect::StartTurn("quick question".into())]
    );
    assert!(app.update(ctrl('s')).is_empty());
    assert_eq!(app.input.text(), "text");
    assert!(!app.input.has_stash());
    // Stashing over a stash keeps the older one in the history.
    app.update(ctrl('s'));
    type_str(&mut app, "newer");
    app.take_submitted();
    app.update(ctrl('s'));
    assert_eq!(app.take_submitted(), vec!["text".to_string()]);
    assert_eq!(app.input.history().last().map(String::as_str), Some("text"));
    app.update(ctrl('s'));
    assert_eq!(app.input.text(), "newer");
}

#[test]
fn hash_input_remembers_and_escapes_do_not() {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "#  prefer tabs "),
        vec![Effect::Remember("prefer tabs".into())]
    );
    assert!(!app.is_running());
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Info, "remembering: prefer tabs".into()))
    );
    assert_eq!(
        submit(&mut app, "/remember use nextest"),
        vec![Effect::Remember("use nextest".into())]
    );
    assert_eq!(
        submit(&mut app, "## heading"),
        vec![Effect::StartTurn("## heading".into())]
    );
    app.update(finished(Ok(summary())));
    assert_eq!(
        submit(&mut app, "\\#tag"),
        vec![Effect::StartTurn("#tag".into())]
    );
    app.update(finished(Ok(summary())));
    type_str(&mut app, "#a");
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    assert_eq!(
        submit(&mut app, "b"),
        vec![Effect::StartTurn("#a\nb".into())]
    );
}
