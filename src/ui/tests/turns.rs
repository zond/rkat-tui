//! Turns, steering, the turn's end, the context gauge and the cost.

use super::*;

/// A compaction is one folded line with its figures; its summary, read
/// from the conversation's record (the event carries none), is drawn only
/// when the line is opened (a click, or Ctrl-O), and `/export` writes it
/// under a heading. A summary that could not be read says where it is.
#[test]
fn a_compaction_is_one_folded_line_that_opens_to_its_summary() {
    let completed = || {
        agent(AgentEvent::CompactionCompleted {
            summary_tokens: 3_249,
            messages_before: 379,
            messages_after: 68,
            summary_source: None,
            estimated_history_tokens_after: None,
        })
    };
    let mut app = App::new(StatusInfo::default());
    let fx = app.update(completed());
    assert!(fx.contains(&Effect::ReadCompactionSummary), "{fx:?}");
    let summary = "The user is refactoring the parser.\nNext: the tests.";
    app.update(Msg::CompactionSummary(Ok(summary.into())));
    let rows = screen(&mut app, 20);
    let text = rows.join("\n");
    assert!(
        rows.iter()
            .any(|r| r == "✓ compacted: 379 → 68 messages (3.2k summary tokens) ▸"),
        "{text}"
    );
    assert!(!text.contains("refactoring"), "{text}");
    let row = rows
        .iter()
        .position(|r| r.starts_with("✓ compacted"))
        .unwrap();
    click_at(&mut app, u16::try_from(row).unwrap());
    let open = screen(&mut app, 20).join("\n");
    assert!(
        open.contains("✓ compacted: 379 → 68 messages (3.2k summary tokens) ▾"),
        "{open}"
    );
    assert!(
        open.contains("  The user is refactoring the parser."),
        "{open}"
    );
    assert!(open.contains("  Next: the tests."), "{open}");
    let export = app.transcript.export_text();
    assert!(
        export.contains(
            "## compacted: 379 → 68 messages (3.2k summary tokens) (the summary the agent goes on from)"
        ) && export.contains("The user is refactoring the parser."),
        "{export}"
    );
    // Ctrl-O opens every one, a second compaction's too; one whose summary
    // could not be read says so and where it is.
    let mut app = App::new(StatusInfo::default());
    app.update(completed());
    app.update(Msg::CompactionSummary(Err("the daemon is gone".into())));
    app.update(ctrl('o'));
    let open = screen(&mut app, 20).join("\n");
    assert!(
        open.contains("the summary could not be read: the daemon is gone"),
        "{open}"
    );
    assert!(
        !app.transcript
            .blocks()
            .iter()
            .any(|b| matches!(b, Block::User { .. } | Block::Assistant { .. })),
        "{:?}",
        app.transcript.blocks()
    );
}

#[test]
fn a_remote_turn_shows_its_prompt_and_keeps_the_ui_busy_until_it_ends() {
    let mut app = App::new(StatusInfo::default());
    let fx = app.update(Msg::RemoteTurn {
        prompt: Some("from another client".into()),
    });
    assert!(fx.is_empty(), "nothing is sent for a turn the daemon runs");
    assert!(app.is_running());
    assert_eq!(
        app.transcript.blocks(),
        &[Block::User {
            text: "from another client".into()
        }]
    );
    // A second announcement while busy changes nothing.
    app.update(Msg::RemoteTurn { prompt: None });
    assert_eq!(app.transcript.blocks().len(), 1);
    // Esc interrupts it like a turn this UI started.
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    app.update(Msg::TurnFinished(Ok(summary())));
    assert!(!app.is_running());
    // `/daemon stop` is handed to the client runner.
    type_str(&mut app, "/daemon stop");
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::DaemonStop]);
    // `/agents` lists the agents, `/agents <name>` switches: both the
    // client runner's.
    type_str(&mut app, "/agents");
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::Agents(None)]);
    type_str(&mut app, "/agents main-ab12");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::Agents(Some("main-ab12".into()))]
    );
}

/// Another agent's message is shown labelled with its sender, folded to
/// one line that a click opens.
#[test]
fn a_peer_message_is_shown_labelled() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Peer {
        from: "main-ab12".into(),
        text: "I am changing src/lib.rs".into(),
    });
    assert_eq!(
        app.transcript.blocks(),
        &[Block::Peer {
            from: "main-ab12".into(),
            text: "I am changing src/lib.rs".into(),
            collapsed: true,
            to: None,
        }]
    );
    let id = app.transcript.iter().next().unwrap().0;
    assert_eq!(app.transcript.toggle_tool(id), Some(false));
}

/// Messages that reach a running turn are named by their sender at once;
/// the run that reads them carries their text, which fills those blocks
/// in order instead of adding the raw input as a prompt.
#[test]
fn a_message_that_arrived_mid_turn_gets_its_text_when_the_run_reads_it() {
    let mut app = App::new(StatusInfo::default());
    for from in ["core", "review", "core"] {
        app.update(Msg::Peer {
            from: from.into(),
            text: String::new(),
        });
    }
    for (from, text) in [
        ("core", "done"),
        ("review", "verified"),
        ("core", "no more edits"),
    ] {
        app.update(Msg::Peer {
            from: from.into(),
            text: text.into(),
        });
    }
    let shown: Vec<(String, String)> = app
        .transcript
        .blocks()
        .into_iter()
        .filter_map(|block| match block {
            Block::Peer { from, text, .. } => Some((from, text)),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("core".to_string(), "done".to_string()),
            ("review".to_string(), "verified".to_string()),
            ("core".to_string(), "no more edits".to_string()),
        ]
    );
    // One with no block waiting for it is a block of its own.
    app.update(Msg::Peer {
        from: "other".into(),
        text: "hello".into(),
    });
    assert_eq!(app.transcript.blocks().len(), 4);
}

#[test]
fn submit_starts_turn_and_streams_into_one_block() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "hi");
    let fx = app.update(key(KeyCode::Enter));
    assert_eq!(fx, vec![Effect::StartTurn("hi".into())]);
    assert!(app.is_running());
    assert!(app.status.running_since.is_some());
    app.update(agent(AgentEvent::TurnStarted {
        turn_number: 0,
        assistant_message_id: None,
    }));
    for d in ["Hel", "lo ", "there"] {
        app.update(agent(AgentEvent::TextDelta {
            delta: d.into(),
            assistant_message_id: None,
        }));
    }
    app.update(agent(AgentEvent::TurnCompleted {
        stop_reason: StopReason::EndTurn,
        usage: test_usage(Usage {
            input_tokens: 10,
            output_tokens: 4,
            ..Usage::default()
        }),
        assistant_message_id: None,
    }));
    assert_eq!(
        app.transcript.blocks(),
        &[
            Block::User { text: "hi".into() },
            Block::Assistant {
                md: "Hello there".into()
            }
        ]
    );
    assert_eq!((app.status.input_tokens, app.status.output_tokens), (10, 4));
    app.update(Msg::TurnFinished(Ok(summary())));
    assert!(!app.is_running());
    assert!(app.status.running_since.is_none());
}

#[test]
fn turn_summary_text_used_when_nothing_streamed() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "q");
    app.update(key(KeyCode::Enter));
    app.update(Msg::TurnFinished(Ok(TurnSummary {
        text: "answer".into(),
        ..summary()
    })));
    assert_eq!(
        app.transcript.blocks().last(),
        Some(&Block::Assistant {
            md: "answer".into()
        })
    );
}

#[test]
fn tool_lifecycle_updates_block_states() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "t1".into(),
        name: "edit_file".into(),
        args: test_args(json!({"path": "src/x.rs"})),
    }));
    let id = app.transcript.tool_block("t1").unwrap();
    app.update(agent(AgentEvent::ToolExecutionStarted {
        id: "t1".into(),
        name: "edit_file".into(),
    }));
    assert!(matches!(
        app.transcript.block(id),
        Some(Block::Tool {
            state: ToolState::Running { .. },
            ..
        })
    ));
    app.update(Msg::Diff {
        tool_use_id: "t1".into(),
        path: "src/x.rs".into(),
        unified: "-a\n+b".into(),
        added: 1,
        removed: 1,
    });
    app.update(agent(AgentEvent::ToolExecutionCompleted {
        id: "t1".into(),
        name: "edit_file".into(),
        is_error: true,
        duration_ms: 12,
    }));
    app.update(agent(test_tool_result("t1", "ok")));
    match app.transcript.block(id) {
        Some(Block::Tool {
            state,
            result_preview,
            diff: Some(d),
            ..
        }) => {
            assert_eq!(
                *state,
                ToolState::Done {
                    is_error: true,
                    duration_ms: 12
                }
            );
            assert_eq!(result_preview.as_deref(), Some("ok"));
            assert_eq!(d.added, 1);
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// The client runner folds run ends with this.
#[test]
fn a_turn_end_keeps_the_first_failure_and_the_last_completion() {
    let done = |text: &str| {
        Ok(TurnSummary {
            text: text.into(),
            ..TurnSummary::default()
        })
    };
    let failed = |why: &str| Err(TurnFailure::Failed(why.into()));
    assert_eq!(merge_turn_end(Some(failed("a")), failed("b")), failed("a"));
    assert_eq!(merge_turn_end(Some(done("a")), done("b")), done("b"));
    assert_eq!(
        merge_turn_end(Some(failed("a")), Err(TurnFailure::Interrupted)),
        failed("a")
    );
    assert_eq!(
        merge_turn_end(Some(Err(TurnFailure::Interrupted)), done("b")),
        Err(TurnFailure::Interrupted)
    );
    assert_eq!(merge_turn_end(None, done("x")), done("x"));
}

#[test]
fn enter_while_running_queues_and_steers() {
    let mut app = running_app();
    assert_eq!(submit(&mut app, "two"), vec![steer(1, "two")]);
    assert_eq!(submit(&mut app, "three"), vec![steer(2, "three")]);
    assert_eq!(app.status.queued, 2);
    assert_eq!(
        app.transcript.blocks(),
        &[
            user("one"),
            queued_block(1, "two", QueuedLabel::Queued),
            queued_block(2, "three", QueuedLabel::Queued),
        ]
    );
    // The runtime holds them: nothing to recall locally.
    assert!(!app.has_recallable());
}

#[test]
fn admitted_then_delivered_in_place() {
    let mut app = running_app();
    app.update(agent_at(
        AgentEvent::TextDelta {
            delta: "a".into(),
            assistant_message_id: None,
        },
        4,
    ));
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    assert_eq!(
        app.pending.get(PendingId(1)).map(|p| p.state),
        Some(PendingState::Admitted)
    );
    assert_eq!(
        app.transcript.block(BlockId(2)),
        Some(&queued_block(1, "two", QueuedLabel::Delivering))
    );
    // A turn boundary alone does not count: only the runtime's ack.
    app.update(agent_at(
        AgentEvent::TurnStarted {
            turn_number: 1,
            assistant_message_id: None,
        },
        5,
    ));
    assert_eq!(app.status.queued, 1);
    assert!(app.update(Msg::SteerDelivered(PendingId(1))).is_empty());
    app.update(agent_at(
        AgentEvent::TextDelta {
            delta: "b".into(),
            assistant_message_id: None,
        },
        6,
    ));
    assert_eq!(app.status.queued, 0);
    // Replaced in place: transcript order is preserved.
    assert_eq!(
        app.transcript.blocks(),
        &[
            user("one"),
            Block::Assistant { md: "a".into() },
            user("two"),
            Block::Assistant { md: "b".into() },
        ]
    );
    let fx = app.update(finished(Ok(summary())));
    assert!(fx.is_empty());
    assert!(!app.is_running());
}

#[test]
fn ack_not_running_defers_or_starts_turn() {
    let mut app = running_app();
    submit(&mut app, "two");
    // The turn is ending: hold it for the turn's end.
    assert!(app.update(ack(1, Err(SteerFailure::NotRunning))).is_empty());
    assert_eq!(
        app.transcript.block(BlockId(1)),
        Some(&queued_block(1, "two", QueuedLabel::Deferred))
    );
    let fx = app.update(finished(Ok(summary())));
    assert_eq!(fx, vec![Effect::StartTurn("two".into())]);
    assert_eq!(app.transcript.blocks(), &[user("one"), user("two")]);
    app.update(finished(Ok(summary())));

    // Already idle when the refusal arrives: send it as a prompt,
    // converting the placeholder in place.
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(finished(Ok(summary())));
    assert!(!app.is_running());
    assert_eq!(app.status.queued, 1);
    assert_eq!(
        app.update(ack(1, Err(SteerFailure::NotRunning))),
        vec![Effect::StartTurn("two".into())]
    );
    assert!(app.is_running());
    assert_eq!(app.status.queued, 0);
    assert_eq!(app.transcript.blocks(), &[user("one"), user("two")]);
}

#[test]
fn rejected_steer_returns_to_the_editor_with_a_notice() {
    let mut app = running_app();
    submit(&mut app, "two");
    type_str(&mut app, "draft");
    assert!(
        app.update(ack(1, Err(SteerFailure::Rejected("closed".into()))))
            .is_empty()
    );
    assert_eq!(app.input.text(), "two\ndraft");
    assert!(app.pending.is_empty());
    assert!(matches!(
        last_notice(&app),
        Some((NoticeLevel::Warn, text)) if text.contains("closed") && text.contains("“two”")
    ));
}

/// A budget warning names its budget in words, not the
/// engine's type name.
#[test]
fn a_budget_warning_is_said_in_words() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::BudgetWarning {
        budget_type: meerkat_core::BudgetType::ToolCalls,
        used: 45,
        limit: 50,
        percent: 0.9,
    }));
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "⚠ the tool-call budget is 90% used (45 of 50)".to_string()
        ))
    );
}

/// A steer nobody can say reached the agent is not put back
/// in the editor (sent again it could reach the agent twice); the notice
/// says it may have reached it.
#[test]
fn a_steer_of_unknown_fate_stays_out_of_the_editor() {
    let mut app = running_app();
    submit(&mut app, "two");
    type_str(&mut app, "draft");
    assert!(
        app.update(ack(1, Err(SteerFailure::Unknown("timed out".into()))))
            .is_empty()
    );
    assert_eq!(app.input.text(), "draft");
    assert!(app.pending.is_empty());
    assert!(matches!(
        last_notice(&app),
        Some((NoticeLevel::Warn, text))
            if text.contains("may have reached the agent") && text.contains("“two”")
    ));
}

// Esc always interrupts a running turn (Claude Code), and ↑ takes the
// queued messages back.
#[test]
fn esc_interrupts_even_with_queued_messages() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Ok(())));
    app.update(ack(2, Err(SteerFailure::NotRunning)));
    type_str(&mut app, "draft");
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    // Nothing comes back to the editor; the queue stays in place.
    assert_eq!(app.input.text(), "draft");
    assert_eq!(app.status.queued, 2);
    // Ctrl-C does the same.
    assert_eq!(app.update(ctrl('c')), vec![Effect::Interrupt]);
    assert_eq!(app.status.queued, 2);
}

/// Several messages wait in the engine: one ↑ asks the runner to take
/// back all of them, and the ones it got back return to the editor in the
/// order typed; one the agent read meanwhile stays, shown as sent.
#[test]
fn up_takes_back_every_waiting_message_in_order() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    submit(&mut app, "four");
    for id in 1..=3 {
        app.update(ack(id, Ok(())));
    }
    assert_eq!(app.update(key(KeyCode::Up)), vec![Effect::RecallQueued]);
    // While it is on its way, ↑ waits.
    assert!(app.update(key(KeyCode::Up)).is_empty());
    app.update(Msg::Recalled {
        recalled: vec![PendingId(1), PendingId(3)],
        errors: Vec::new(),
    });
    assert_eq!(app.input.text(), "two\nfour");
    assert!(app.pending.get(PendingId(1)).is_none());
    assert!(app.pending.get(PendingId(3)).is_none());
    assert_eq!(
        app.pending.get(PendingId(2)).map(|p| p.state),
        Some(PendingState::Admitted)
    );
}

#[test]
fn up_with_an_admitted_message_recalls_it_without_interrupting() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Ok(())));
    app.update(ack(2, Err(SteerFailure::NotRunning)));
    assert!(app.can_take_back());
    assert_eq!(app.update(key(KeyCode::Up)), vec![Effect::RecallQueued]);
    // While the recall is on its way, ↑ waits for it.
    assert!(app.update(key(KeyCode::Up)).is_empty());
    let fx = app.update(Msg::Recalled {
        recalled: vec![PendingId(1)],
        errors: Vec::new(),
    });
    assert!(fx.is_empty());
    // The recalled and the held message, oldest first.
    assert_eq!(app.input.text(), "two\nthree");
    assert_eq!(app.transcript.blocks()[0], user("one"));
    assert!(
        !app.transcript
            .blocks()
            .iter()
            .any(|b| matches!(b, Block::Queued { .. }))
    );
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Info,
            "2 messages returned to the editor".into()
        ))
    );
    assert_eq!(app.status.queued, 0);
    assert!(!app.can_take_back());
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
}

#[test]
fn up_when_the_messages_already_reached_the_agent_notices_then_recalls_history() {
    let mut app = running_app().with_history(vec!["old".into()]);
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    assert_eq!(app.update(key(KeyCode::Up)), vec![Effect::RecallQueued]);
    let fx = app.update(Msg::Recalled {
        recalled: Vec::new(),
        errors: Vec::new(),
    });
    assert!(fx.is_empty());
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Info,
            "your messages already reached the agent; Esc interrupts".into()
        ))
    );
    // The message stays with the runtime; ↑ is history recall again.
    assert_eq!(
        app.transcript.blocks()[1],
        queued_block(1, "two", QueuedLabel::Delivering)
    );
    assert!(app.update(key(KeyCode::Up)).is_empty());
    assert_eq!(app.input.text(), "two");
    assert_eq!(app.status.queued, 1);
}

/// A steered message the runtime ended without a run returns to the
/// editor before the draft, its placeholder gone, with one notice saying
/// why; another admitted message stays with the agent.
#[test]
fn a_message_the_agent_did_not_take_returns_to_the_editor() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Ok(())));
    app.update(ack(2, Ok(())));
    app.update(Msg::TurnFinished(Err(TurnFailure::Interrupted)));
    app.input.set_text("draft");
    let fx = app.update(Msg::NotDelivered {
        id: PendingId(1),
        why: Some("it was cancelled".into()),
    });
    assert!(fx.is_empty());
    assert_eq!(app.input.text(), "two\ndraft");
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "the agent did not take “two” (it was cancelled); it is back in the editor".into()
        ))
    );
    assert!(app.pending.get(PendingId(1)).is_none());
    let queued: Vec<Block> = app
        .transcript
        .blocks()
        .iter()
        .filter(|b| matches!(b, Block::Queued { .. }))
        .cloned()
        .collect();
    assert_eq!(queued, [queued_block(2, "three", QueuedLabel::Delivering)]);
    // Without a reason, and a second time for the same message: nothing.
    app.input.set_text("");
    app.update(Msg::NotDelivered {
        id: PendingId(2),
        why: None,
    });
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "the agent did not take “three”; it is back in the editor".into()
        ))
    );
    let notices = app.transcript.blocks().len();
    app.update(Msg::NotDelivered {
        id: PendingId(2),
        why: None,
    });
    assert_eq!(app.transcript.blocks().len(), notices);
    assert_eq!(app.input.text(), "three");
}

/// A message shown delivered whose request the daemon's stop cut off
/// returns to the editor before the draft, with one notice saying why.
#[test]
fn a_message_the_engine_did_not_keep_returns_to_the_editor() {
    let mut app = running_app();
    app.input.set_text("draft");
    let fx = app.update(Msg::Unkept {
        text: "use nextest".into(),
        why: "the daemon stopped before the agent read it".into(),
    });
    assert!(fx.is_empty());
    assert_eq!(app.input.text(), "use nextest\ndraft");
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "the agent did not keep “use nextest” (the daemon stopped before the agent read it); it is back in the editor".into()
        ))
    );
}

#[test]
fn esc_with_nothing_pending_interrupts() {
    let mut app = running_app();
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    // Idle as the UI knows it: the client asks the daemon.
    let mut idle = App::new(StatusInfo::default());
    assert_eq!(idle.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
}

#[test]
fn up_recalls_the_newest_held_message() {
    let mut app = running_app().with_history(vec!["old".into()]);
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    app.update(ack(2, Err(SteerFailure::NotRunning)));
    assert!(app.update(key(KeyCode::Up)).is_empty());
    assert_eq!(app.input.text(), "three");
    assert_eq!(app.status.queued, 1);
    app.update(ctrl('u'));
    assert!(app.update(key(KeyCode::Up)).is_empty());
    assert_eq!(app.input.text(), "two");
    assert_eq!(app.status.queued, 0);
    // With nothing held, ↑ is history recall again.
    app.update(ctrl('u'));
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "three");
}

#[test]
fn successful_turn_sends_held_messages_as_the_next_turn() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    app.update(ack(2, Err(SteerFailure::NotRunning)));
    let fx = app.update(finished(Ok(summary())));
    assert_eq!(fx, vec![Effect::StartTurn("two\n\nthree".into())]);
    assert_eq!(
        app.transcript.blocks(),
        &[user("one"), user("two"), user("three")]
    );
}

// The rule: an interrupt returns every message the
// agent did not read to the editor, in the order typed (the ones the
// runner took back and the ones the UI held), before the draft, starts no
// turn, and says so in one line. One the agent read stays delivered.
#[test]
fn interrupted_turn_returns_the_unread_messages_to_the_editor() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    submit(&mut app, "four");
    app.update(ack(1, Ok(())));
    app.update(ack(2, Ok(())));
    app.update(ack(3, Err(SteerFailure::NotRunning)));
    app.update(Msg::SteerDelivered(PendingId(1)));
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    type_str(&mut app, "draft");
    // The runner took "three" back from the agent.
    assert!(app.update(Msg::Returned(vec![PendingId(2)])).is_empty());
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert!(fx.is_empty(), "{fx:?}");
    assert!(!app.is_running());
    assert_eq!(app.input.text(), "three\nfour\ndraft");
    let users: Vec<&Block> = app
        .transcript
        .iter()
        .map(|(_, b)| b)
        .filter(|b| matches!(b, Block::User { .. }))
        .collect();
    assert_eq!(users, vec![&user("one"), &user("two")]);
    assert!(
        !app.transcript
            .blocks()
            .iter()
            .any(|b| matches!(b, Block::Queued { .. }))
    );
    assert_eq!(app.status.queued, 0);
    let notices: Vec<String> = app
        .transcript
        .blocks()
        .iter()
        .filter_map(|b| match b {
            Block::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .filter(|text| text.contains("interrupt"))
        .collect();
    assert_eq!(
        notices,
        vec![
            "turn interrupted; the agent is idle, and your 2 typed messages are back in the input, in the order you typed them"
                .to_string()
        ]
    );
    // More presses right after it neither clear the editor nor arm its
    // clear: they go to the runner, which says once that the turn is
    // already stopped.
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    assert_eq!(app.input.text(), "three\nfour\ndraft");
}

#[test]
fn interrupted_turn_without_queued_messages_starts_nothing() {
    let mut app = running_app();
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    assert!(
        app.update(finished(Err(TurnFailure::Interrupted)))
            .is_empty()
    );
    assert!(!app.is_running());
}

/// A message the agent has (admitted) but has not read when its turn ends
/// is neither sent again nor returned: it stays shown as on its way, ↑
/// can still take it back, and its delivery marks it later.
#[test]
fn a_message_the_agent_has_not_read_stays_with_it_after_the_turn() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    assert!(app.update(finished(Ok(summary()))).is_empty());
    assert!(!app.is_running());
    assert_eq!(
        app.transcript.blocks()[1],
        queued_block(1, "two", QueuedLabel::Delivering)
    );
    assert_eq!(app.update(key(KeyCode::Up)), vec![Effect::RecallQueued]);
    app.update(Msg::Recalled {
        recalled: Vec::new(),
        errors: Vec::new(),
    });
    app.update(Msg::SteerDelivered(PendingId(1)));
    assert_eq!(app.transcript.blocks()[1], user("two"));
    assert!(app.pending.is_empty());
}

#[test]
fn failed_turn_returns_held_messages_with_a_notice() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    assert!(
        app.update(finished(Err(TurnFailure::Failed("boom".into()))))
            .is_empty()
    );
    assert_eq!(app.input.text(), "two");
    assert!(matches!(
        last_notice(&app),
        Some((NoticeLevel::Warn, text)) if text.contains("returned to the editor")
    ));
}

#[test]
fn send_next_takes_priority_over_held_messages() {
    let mut app = running_app();
    let t0 = Instant::now();
    submit(&mut app, "two");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    type_str(&mut app, "urgent");
    app.update_at(ctrl('x'), t0);
    assert_eq!(app.update_at(ctrl('s'), t0), vec![Effect::Interrupt]);
    assert_eq!(app.delivery.next(), Some("urgent"));
    // The queued message stays in place until the turn ends.
    assert_eq!(app.status.queued, 1);
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert_eq!(fx, vec![Effect::StartTurn("two\n\nurgent".into())]);
    assert!(!app.delivery.has_next());
    assert!(app.is_running());
    assert!(app.input.is_empty());
    let users: Vec<&Block> = app
        .transcript
        .iter()
        .map(|(_, b)| b)
        .filter(|b| matches!(b, Block::User { .. }))
        .collect();
    assert_eq!(users, vec![&user("one"), &user("two"), &user("urgent")]);
}

/// Ctrl-X Ctrl-S on the running turn.
fn send_now(app: &mut App) -> Vec<Effect> {
    let t0 = Instant::now();
    app.update_at(ctrl('x'), t0);
    app.update_at(ctrl('s'), t0)
}

/// The notices that speak of the interrupt.
fn interrupt_notices(app: &App) -> Vec<String> {
    notices(app)
        .into_iter()
        .map(|(_, text)| text)
        .filter(|text| text.contains("interrupt"))
        .collect()
}

/// Ctrl-X Ctrl-S on an empty editor while two of the user's messages wait
/// for the agent: the turn is interrupted and the two the interrupt hands
/// back go at once as the next run, in the order typed, not to the editor;
/// one notice says so.
#[test]
fn send_now_on_an_empty_editor_sends_the_waiting_messages_as_the_next_run() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Ok(())));
    app.update(ack(2, Ok(())));
    assert_eq!(send_now(&mut app), vec![Effect::Interrupt]);
    assert!(
        app.update(Msg::Returned(vec![PendingId(2), PendingId(1)]))
            .is_empty()
    );
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert_eq!(fx, vec![Effect::StartTurn("two\n\nthree".into())]);
    assert!(app.is_running());
    assert!(app.input.is_empty());
    assert!(app.pending.is_empty());
    let users: Vec<&Block> = app
        .transcript
        .iter()
        .map(|(_, b)| b)
        .filter(|b| matches!(b, Block::User { .. }))
        .collect();
    assert_eq!(users, vec![&user("one"), &user("two\n\nthree")]);
    assert_eq!(
        interrupt_notices(&app),
        ["interrupted; sending your 2 waiting messages now"]
    );
}

/// With text in the editor too, the waiting message goes first and the
/// text after it, in one run.
#[test]
fn send_now_with_text_sends_the_waiting_message_then_the_text() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    type_str(&mut app, "urgent");
    assert_eq!(send_now(&mut app), vec![Effect::Interrupt]);
    assert!(app.input.is_empty());
    app.update(Msg::Returned(vec![PendingId(1)]));
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert_eq!(fx, vec![Effect::StartTurn("two\n\nurgent".into())]);
    assert!(app.input.is_empty());
    assert_eq!(
        interrupt_notices(&app),
        ["interrupted; sending your waiting message and the new one now"]
    );
}

/// Nothing waiting and an empty editor: a plain interrupt, after which
/// nothing starts.
#[test]
fn send_now_with_nothing_waiting_is_a_plain_interrupt() {
    let mut app = running_app();
    assert_eq!(send_now(&mut app), vec![Effect::Interrupt]);
    assert!(
        app.update(finished(Err(TurnFailure::Interrupted)))
            .is_empty()
    );
    assert!(!app.is_running());
    assert_eq!(
        interrupt_notices(&app),
        ["turn interrupted; the agent is idle"]
    );
}

/// The run ended on its own as the interrupt came: what the interrupt
/// handed back is sent all the same, and the notice says the run had
/// ended.
#[test]
fn send_now_after_a_run_that_ended_on_its_own_still_sends() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    assert_eq!(send_now(&mut app), vec![Effect::Interrupt]);
    app.update(Msg::Returned(vec![PendingId(1)]));
    let fx = app.update(finished(Ok(summary())));
    assert_eq!(fx, vec![Effect::StartTurn("two".into())]);
    assert_eq!(
        interrupt_notices(&app),
        ["the run had ended as you interrupted it; sending your waiting message now"]
    );
}

/// While one of the user's messages waits for the running turn, the
/// status bar's queued segment says how to push it, shortened or left out
/// as the width asks; a message the agent read already does not count.
#[test]
fn the_status_bar_says_how_to_push_a_waiting_message() {
    let mut app = running_app();
    let now = Instant::now();
    let line = |app: &App| crate::ui::render::status_line(&app.status, now);
    assert!(!line(&app).contains("Ctrl-X"), "{}", line(&app));
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    assert!(
        line(&app).contains("queued 1 · Ctrl-X Ctrl-S interrupts and sends"),
        "{}",
        line(&app)
    );
    let full = crate::ui::render::status_bar(&app.status, now).width();
    let fit = |width: usize| -> String {
        crate::ui::render::status_bar_fit(&app.status, now, width)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert!(fit(full).contains("Ctrl-X Ctrl-S interrupts and sends"));
    assert!(
        fit(full - 1).contains("queued 1 · C-x C-s sends"),
        "{}",
        fit(full - 1)
    );
    assert!(fit(10).ends_with("queued 1"), "{}", fit(10));
    // A recall found it with the agent: nothing left to push.
    app.update(key(KeyCode::Up));
    app.update(Msg::Recalled {
        recalled: Vec::new(),
        errors: Vec::new(),
    });
    assert!(!line(&app).contains("Ctrl-X"), "{}", line(&app));
}

#[test]
fn reset_transcript_keeps_queued_placeholders() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(Msg::ResetTranscript(vec![user("restored")]));
    assert_eq!(
        app.transcript.blocks(),
        &[
            user("restored"),
            queued_block(1, "two", QueuedLabel::Queued)
        ]
    );
    assert_eq!(
        app.pending.get(PendingId(1)).map(|p| p.block),
        Some(BlockId(1))
    );
}

#[test]
fn the_runners_mention_report_notes_what_was_attached_and_what_is_sealed() {
    let (_dir, mut app) = mention_app();
    let fx = submit(
        &mut app,
        "fix @src/main.rs and @src/ui, ignore @.rkat/config.toml and @bob",
    );
    // The session gets the text as typed; the runner expands it and
    // reports what it attached.
    assert_eq!(
        fx,
        vec![Effect::StartTurn(
            "fix @src/main.rs and @src/ui, ignore @.rkat/config.toml and @bob".into()
        )]
    );
    let before = app.transcript.len();
    app.update(Msg::Mentions {
        attached: vec!["src/main.rs".into(), "src/ui/".into()],
        sealed: vec![".rkat/config.toml".into()],
    });
    assert_eq!(
        &app.transcript.blocks()[before..],
        [
            Block::Notice {
                level: NoticeLevel::Info,
                text: "+ 2 files (src/main.rs, src/ui/)".into()
            },
            Block::Notice {
                level: NoticeLevel::Warn,
                text: "@.rkat/config.toml is sealed: not attached".into()
            },
        ]
    );
    // One file: singular. Nothing attached or sealed: no line.
    app.update(Msg::Mentions {
        attached: vec!["README.md".into()],
        sealed: Vec::new(),
    });
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Info, "+ 1 file (README.md)".into()))
    );
    let before = app.transcript.len();
    app.update(Msg::Mentions {
        attached: Vec::new(),
        sealed: Vec::new(),
    });
    assert_eq!(app.transcript.len(), before);
}

#[test]
fn last_seq_resets_with_the_session() {
    let mut app = running_app();
    app.update(agent_at(
        AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        },
        41,
    ));
    assert_eq!(app.last_seq, 41);
    app.update(Msg::Status(StatusUpdate::Session("next".into())));
    assert_eq!(app.last_seq, 0);
    app.update(agent_at(
        AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        },
        7,
    ));
    app.update(Msg::ResetTranscript(Vec::new()));
    assert_eq!(app.last_seq, 0);
}

#[test]
fn summary_text_is_not_repeated_after_streamed_text() {
    let mut app = running_app();
    app.update(agent(AgentEvent::TextDelta {
        delta: "streamed".into(),
        assistant_message_id: None,
    }));
    // A notice after the text must not make the summary look new.
    app.update(Msg::Notice {
        level: NoticeLevel::Info,
        text: "x".into(),
    });
    app.update(Msg::TurnFinished(Ok(TurnSummary {
        text: "streamed".into(),
        ..summary()
    })));
    let assistant = app
        .transcript
        .blocks()
        .iter()
        .filter(|b| matches!(b, Block::Assistant { .. }))
        .count();
    assert_eq!(assistant, 1);
}

#[test]
fn the_bell_rings_only_while_unfocused_and_enabled() {
    let mut app = running_app();
    // Focused: no bell.
    app.update(finished(Ok(summary())));
    assert!(!app.take_bell());
    // Unfocused: a finished turn and an escalation ring.
    app.update(Msg::Focus(false));
    submit(&mut app, "two");
    app.update(finished(Ok(summary())));
    assert!(app.take_bell());
    assert!(!app.take_bell(), "taken once");
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    assert!(app.take_bell());
    // A turn whose queued messages start the next one does not ring.
    let mut app = running_app();
    app.update(Msg::Focus(false));
    submit(&mut app, "two");
    app.update(ack(1, Err(SteerFailure::NotRunning)));
    app.update(finished(Ok(summary())));
    assert!(app.is_running());
    assert!(!app.take_bell());
    // Focus back: quiet again.
    app.update(Msg::Focus(true));
    app.update(finished(Ok(summary())));
    assert!(!app.take_bell());
    // Disabled.
    let mut app = App::new(StatusInfo::default()).with_bell(false);
    app.update(Msg::Focus(false));
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    assert!(!app.take_bell());
}

#[test]
fn status_updates_and_dirty_tracking() {
    let mut app = App::new(StatusInfo::default());
    assert!(app.take_dirty());
    app.update(Msg::Tick);
    assert!(!app.take_dirty());
    app.update(Msg::Status(StatusUpdate::Mode {
        mode: PermissionMode::Auto,
        paused: true,
        denials: crate::approval::desk::Denials {
            consecutive: 3,
            consecutive_limit: 3,
            total: 5,
            total_limit: 20,
        },
    }));
    assert!(app.take_dirty());
    assert_eq!(app.status.mode, Some(PermissionMode::Auto));
    assert!(app.status.auto_paused);
    assert_eq!(
        app.status.mode_text(),
        "auto, PAUSED (consecutive denials 3/3, total 5/20)"
    );
    app.update(Msg::Status(StatusUpdate::Session(
        "0123456789abcdef".into(),
    )));
    assert_eq!(app.status.session_short, "89abcdef");
}

/// The gauge shows the daemon's reading (`rkat/cost`; how it is computed
/// from a model call and a compaction is the daemon ledger's,
/// `daemon::usage`'s tests): a model call's event changes nothing here, and
/// a new conversation clears it.
#[test]
fn context_gauge_shows_the_daemons_reading_and_clears() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Status(StatusUpdate::ContextWindow(Some(200_000))));
    app.update(agent(AgentEvent::TurnCompleted {
        stop_reason: StopReason::EndTurn,
        usage: test_usage(Usage {
            input_tokens: 1_000,
            ..Usage::default()
        }),
        assistant_message_id: None,
    }));
    assert_eq!(app.status.context_used, None, "not derived here");
    app.update(Msg::Status(StatusUpdate::Context {
        used: Some(2_500),
        approx: true,
    }));
    assert_eq!(app.status.context_used, Some(2_500));
    assert!(app.status.context_approx);
    assert_eq!(submit(&mut app, "/clear"), vec![Effect::NewSession]);
    // What the client runner sends once the new session is live.
    app.update(Msg::Status(StatusUpdate::Context {
        used: None,
        approx: false,
    }));
    app.update(Msg::Status(StatusUpdate::ContextWindow(Some(200_000))));
    assert_eq!(app.status.context_used, None);
    assert_eq!(app.status.context_window, Some(200_000));
}

#[test]
fn warn_mode_warns_once_on_crossing_then_every_turn_at_auto() {
    let mut app = gauge_app(CompactMode::Warn);
    assert!(turn_at(&mut app, 50_000).is_empty());
    assert_eq!(warn_count(&app), 0);
    assert!(turn_at(&mut app, 76_000).is_empty());
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "context 76% full (76k/100k tokens); run /compact to free space".into()
        ))
    );
    assert!(turn_at(&mut app, 80_000).is_empty());
    assert_eq!(warn_count(&app), 1, "no repeat below the auto threshold");
    assert!(
        turn_at(&mut app, 86_000).is_empty(),
        "warn mode never compacts"
    );
    assert!(turn_at(&mut app, 87_000).is_empty());
    assert_eq!(
        warn_count(&app),
        3,
        "repeats every turn at the auto threshold"
    );
    assert!(last_notice(&app).is_some_and(|(_, t)| t.contains("--auto-compact")));
    assert!(!app.compaction.busy());
}

/// A command held while a `/compact now` request is out waits for its
/// answer even when a turn started meanwhile ends first: [`App::run_held`]
/// waits out what [`App::defers`] held it for.
#[test]
fn a_held_command_waits_for_an_unanswered_compact_request() {
    let mut app = App::new(StatusInfo::default());
    app.status.context_used = Some(40_000);
    assert_eq!(submit(&mut app, "/compact now"), vec![Effect::CompactNow]);
    assert_eq!(
        submit(&mut app, "one"),
        vec![Effect::StartTurn("one".into())]
    );
    assert!(submit(&mut app, "/clear").is_empty());
    let held = |app: &App| {
        app.transcript.blocks().iter().any(|b| {
            matches!(
                b,
                Block::Queued {
                    label: QueuedLabel::Command,
                    ..
                }
            )
        })
    };
    app.update(finished(Ok(summary())));
    assert!(held(&app), "the request is not answered yet");
    app.update(Msg::CompactRequested(Ok(())));
    assert!(!held(&app), "the answer lets it run");
}

#[test]
fn prompts_during_the_request_are_queued_inputs_not_held_text() {
    let mut app = App::new(StatusInfo::default());
    app.status.context_used = Some(40_000);
    assert_eq!(submit(&mut app, "/compact now"), vec![Effect::CompactNow]);
    assert!(app.compaction.busy());
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Info, "asking the engine to compact…".into()))
    );
    // The prompt goes to the runner at once: it admits it as a `Queue`
    // input after the request (effects are served in order).
    assert_eq!(
        submit(&mut app, "one"),
        vec![Effect::StartTurn("one".into())]
    );
    assert!(app.is_running());
    assert_eq!(app.status.queued, 0);
    // Lifecycle commands wait for the turn's end; /compact asks again at
    // once; /remember does not wait.
    for cmd in ["/clear", "/resume last"] {
        assert!(submit(&mut app, cmd).is_empty(), "{cmd}");
        assert!(matches!(
            app.transcript.blocks().last(),
            Some(Block::Queued {
                label: QueuedLabel::Command,
                ..
            })
        ));
    }
    assert_eq!(submit(&mut app, "/compact now"), vec![Effect::CompactNow]);
    assert_eq!(submit(&mut app, "#x"), vec![Effect::Remember("x".into())]);
    assert!(app.update(Msg::CompactRequested(Ok(()))).is_empty());
    assert!(!app.compaction.busy());
    assert!(app.transcript.blocks().iter().any(
        |b| matches!(b, Block::Notice { level: NoticeLevel::Info, text } if text == COMPACT_REQUESTED)
    ));
    // The wording never claims the compaction itself happened.
    assert!(!COMPACT_REQUESTED.contains("armed"));
}

#[test]
fn auto_mode_neither_warns_nor_compacts_itself() {
    let mut app = gauge_app(CompactMode::Auto);
    for used in [80_000, 90_000, 99_000] {
        assert!(turn_at(&mut app, used).is_empty());
    }
    assert_eq!(warn_count(&app), 0);
    assert!(!app.compaction.busy());
    // Unknown window: no warning in auto mode either.
    let mut app = App::new(StatusInfo::default()).with_compaction_policy(CompactionPolicy {
        mode: CompactMode::Auto,
        ..CompactionPolicy::default()
    });
    assert!(turn_at(&mut app, 150_000).is_empty());
    assert_eq!(warn_count(&app), 0);
}

#[test]
fn a_native_compaction_rearms_the_warning() {
    let mut app = gauge_app(CompactMode::Warn);
    assert!(turn_at(&mut app, 76_000).is_empty());
    assert_eq!(warn_count(&app), 1);
    app.update(agent(AgentEvent::CompactionCompleted {
        summary_tokens: 500,
        messages_before: 40,
        messages_after: 3,
        summary_source: None,
        estimated_history_tokens_after: Some(4_000),
    }));
    app.update(Msg::Status(StatusUpdate::Context {
        used: Some(4_000),
        approx: true,
    }));
    assert_eq!(app.status.context_used, Some(4_000));
    assert!(app.status.context_approx);
    assert!(turn_at(&mut app, 77_000).is_empty());
    assert_eq!(warn_count(&app), 2);
}

/// The UI counts no cost itself: a model call, a compaction or a helper's
/// call changes nothing until the daemon's totals arrive
/// ([`StatusUpdate::CostTotals`]), which the footer and `/cost` price.
#[test]
fn the_footer_and_cost_price_the_daemons_totals() {
    let mut app = App::new(StatusInfo::default());
    app.status.apply(StatusUpdate::Model {
        endpoint: "anthropic".into(),
        model: "main".into(),
    });
    assert!(app.update(completed_by("main", 1_000_000, 0)).is_empty());
    assert!(app.status.cost.is_empty(), "nothing counted by the UI");
    app.update(Msg::Status(StatusUpdate::Prices(priced(&[
        ("main", 3.0, 15.0),
        ("judge", 1.0, 5.0),
    ]))));
    // The daemon's totals: 1.5M in / 0.2M out of `main` (1.5×3 + 0.2×15 =
    // $7.50), a classifier's 1M in / 0.1M out of `judge` ($1.50).
    let mut totals = SessionCost::default();
    let usage = |input, output| Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    };
    totals.record("main", &usage(1_500_000, 200_000));
    totals.record("judge", &usage(1_000_000, 100_000));
    app.update(Msg::Status(StatusUpdate::CostTotals(totals)));
    // The gauge's reading comes with the totals (`rkat/cost`).
    app.update(Msg::Status(StatusUpdate::Context {
        used: Some(1_000_000),
        approx: false,
    }));
    let (usd, all) = app.status.cost.cost_usd(&app.status.prices);
    assert!(all);
    assert!((usd - 9.0).abs() < 1e-9, "{usd}");
    assert!(footer(&app).contains(" │ $9.00 │ "), "{}", footer(&app));
    let breakdown = app.status.cost.breakdown(&app.status.prices);
    assert!(
        breakdown.contains("main: in 1500000 · out 200000"),
        "{breakdown}"
    );
    assert!(
        breakdown.contains("judge: in 1000000 · out 100000"),
        "{breakdown}"
    );
    assert!(breakdown.contains("total: $9.00"), "{breakdown}");
    // A model nobody priced makes the total approximate.
    let mut unpriced = SessionCost::default();
    unpriced.record("main", &usage(1_000_000, 0));
    unpriced.record("mystery", &usage(1, 1));
    app.update(Msg::Status(StatusUpdate::CostTotals(unpriced)));
    assert!(footer(&app).contains(" │ ~$3.00 │ "), "{}", footer(&app));
}

#[test]
fn resume_replaces_the_cost_totals() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Status(StatusUpdate::Prices(priced(&[(
        "a", 1.0, 1.0,
    )]))));
    app.status.apply(StatusUpdate::Model {
        endpoint: "anthropic".into(),
        model: "a".into(),
    });
    // Resuming a session with recorded totals.
    let mut recorded = SessionCost::default();
    recorded.add(
        "a",
        &UsageTotals {
            output: 420_000,
            ..UsageTotals::default()
        },
    );
    app.update(Msg::Status(StatusUpdate::CostTotals(recorded)));
    assert!(footer(&app).contains(" │ $0.42"), "{}", footer(&app));
    // A new session starts from zero: no segment.
    app.update(Msg::Status(
        StatusUpdate::CostTotals(SessionCost::default()),
    ));
    assert!(!footer(&app).contains('$'), "{}", footer(&app));
}

/// A message queued while a model request runs long says what it waits
/// for, and for how long; once the tools run (the reply is in) it does not.
#[test]
fn a_queued_message_says_it_waits_for_the_model_s_reply() {
    let start = Instant::now();
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "go");
    app.update_at(
        agent(AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        }),
        start,
    );
    submit(&mut app, "more");
    let at = |secs| render::status_line(&app.status, start + Duration::from_secs(secs));
    assert!(at(2).contains("queued 1"), "{}", at(2));
    assert!(!at(2).contains("waiting"), "{}", at(2));
    assert!(
        at(10).contains("queued 1, waiting for the model's reply (10s)"),
        "{}",
        at(10)
    );
    // After the quiet period the activity segment says it, once.
    assert!(
        at(42).contains("waiting for the model's reply 42s"),
        "{}",
        at(42)
    );
    assert_eq!(at(42).matches("waiting for the model's reply").count(), 1);
    assert!(at(42).contains("│ queued 1"), "{}", at(42));
    app.update_at(
        agent(AgentEvent::ToolExecutionStarted {
            id: "t1".into(),
            name: "glob".into(),
        }),
        start + Duration::from_secs(43),
    );
    let after = render::status_line(&app.status, start + Duration::from_secs(50));
    assert!(!after.contains("waiting"), "{after}");
}

/// An interrupt the daemon never confirmed is not called an interrupted
/// turn: the notice says the run may still be going.
#[test]
fn an_unconfirmed_interrupt_says_the_run_may_still_be_going() {
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "go");
    app.update(finished(Err(TurnFailure::Unconfirmed)));
    assert!(!app.is_running());
    let notices: Vec<String> = app
        .transcript
        .blocks()
        .iter()
        .filter_map(|block| match block {
            Block::Notice { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notices
            .iter()
            .any(|t| t.contains("the run may still be going")),
        "{notices:?}"
    );
    assert!(
        !notices.iter().any(|t| t == "turn interrupted"),
        "{notices:?}"
    );
}

/// While the run `/compact` starts goes, a message typed is held and sent
/// when it ends; a compaction shows `compacting… Ns` in the status bar
/// until it ends.
#[test]
fn a_compaction_run_holds_what_is_typed_and_the_bar_says_compacting() {
    let start = Instant::now();
    let mut app = App::new(StatusInfo::default());
    app.status.context_used = Some(40_000);
    assert_eq!(submit(&mut app, "/compact"), vec![Effect::CompactNow]);
    app.update(Msg::CompactionRun(true));
    app.update(Msg::CompactRequested(Ok(())));
    assert!(
        app.transcript
            .blocks()
            .iter()
            .any(|b| matches!(b, Block::Notice { text, .. } if text == COMPACT_RUNNING))
    );
    app.update_at(
        agent(AgentEvent::CompactionStarted {
            input_tokens: 40_000,
            estimated_history_tokens: 40_000,
            message_count: 12,
        }),
        start,
    );
    let bar = render::status_line(&app.status, start + Duration::from_secs(7));
    assert!(bar.contains("compacting… 7s"), "{bar}");
    // Typed meanwhile: held, not started.
    assert!(submit(&mut app, "after it").is_empty());
    assert!(!app.is_running());
    // Session commands wait too.
    assert!(submit(&mut app, "/clear").is_empty());
    // The run ends: the held message goes as a turn, the bar is clear.
    let fx = app.update(Msg::CompactionRun(false));
    assert_eq!(fx, vec![Effect::StartTurn("after it".into())]);
    let bar = render::status_line(&app.status, start + Duration::from_secs(9));
    assert!(!bar.contains("compacting"), "{bar}");
}

/// The runner's side is
/// `client::runner::tests::a_run_after_the_compaction_s_ends_the_quiet_turn`.
/// A message typed while the compaction's run holds the UI is drawn at
/// once (its placeholder, "will send after this turn"), ↑ takes it back,
/// Ctrl-X Ctrl-S says what it does and stops the run; when the agent goes
/// on with a run of its own (the turn opens, then the compaction's run is
/// over) the held message is steered into it, and what is typed after is
/// steered too, never held.
#[test]
fn a_message_held_for_the_compaction_is_drawn_and_steered_into_the_next_run() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::CompactionRun(true));
    assert!(submit(&mut app, "held one").is_empty());
    let rows = screen(&mut app, 20).join("\n");
    assert!(
        rows.contains("› held one  · will send after this turn"),
        "drawn at once: {rows}"
    );
    // ↑ takes it back; Enter holds it again.
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "held one");
    assert!(app.pending.items().is_empty());
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    // Ctrl-X Ctrl-S: the run that lets the agent compact is stopped, and
    // it says so.
    let t0 = Instant::now();
    app.update_at(ctrl('x'), t0);
    assert_eq!(app.update_at(ctrl('s'), t0), vec![Effect::Interrupt]);
    assert!(
        last_notice(&app)
            .is_some_and(|(_, text)| text.contains("stopping the run that lets the agent compact"))
    );
    // The agent goes on with a helper's note: the turn opens, then the
    // compaction's run is over; the held message is steered into it.
    app.update(Msg::RemoteTurn { prompt: None });
    let fx = app.update(Msg::CompactionRun(false));
    let Some(Effect::Steer { id, text }) = fx.first().cloned() else {
        panic!("not steered: {fx:?}")
    };
    assert_eq!(text, "held one");
    assert_eq!(fx.len(), 1, "{fx:?}");
    let rows = screen(&mut app, 20).join("\n");
    assert!(rows.contains("› held one  · queued"), "{rows}");
    app.update(ack(id.0, Ok(())));
    // Typed now: steered, not held.
    let fx = submit(&mut app, "and this");
    assert!(
        matches!(fx.as_slice(), [Effect::Steer { text, .. }] if text == "and this"),
        "{fx:?}"
    );
}

/// Ctrl-X Ctrl-S counts a message the UI holds (one the runner could not
/// steer into the turn) as waiting: the turn is stopped and the message
/// is sent as the next run, not returned to the editor.
#[test]
fn send_now_sends_a_message_the_ui_holds() {
    let mut app = running_app();
    let fx = submit(&mut app, "two");
    let Some(Effect::Steer { id, .. }) = fx.first().cloned() else {
        panic!("{fx:?}")
    };
    app.update(ack(id.0, Err(SteerFailure::NotRunning)));
    assert!(app.holds_messages());
    let t0 = Instant::now();
    app.update_at(ctrl('x'), t0);
    assert_eq!(app.update_at(ctrl('s'), t0), vec![Effect::Interrupt]);
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert_eq!(fx, vec![Effect::StartTurn("two".into())]);
    assert!(app.input.is_empty());
}

/// The engine's retry of an empty answer, attempt `n` of 3.
fn empty_retry(n: u32) -> Msg {
    agent(AgentEvent::Retrying {
        retry: meerkat_core::LlmRetrySchedule {
            failure: meerkat_core::LlmRetryFailure {
                provider: "self_hosted".into(),
                kind: meerkat_core::LlmRetryFailureKind::RetryableProviderError,
                retry_after_ms: None,
                duration_ms: None,
                message: "LLM completed without user-visible text, images, or tool calls".into(),
            },
            plan: meerkat_core::LlmRetryPlan {
                attempt: n,
                max_retries: 3,
                computed_delay_ms: 10,
                selected_delay_ms: 10,
                retry_after_hint_ms: None,
                rate_limit_floor_applied: false,
                budget_capped: false,
            },
        },
        assistant_message_id: None,
    })
}

/// The run's end with `report`.
fn run_failed(report: meerkat_core::AgentErrorReport) -> Msg {
    agent(AgentEvent::RunFailed {
        session_id: meerkat_core::SessionId::new(),
        identity: meerkat_core::types::TranscriptMessageIdentity::default(),
        error_report: report,
        terminal_cause_kind: None,
    })
}

/// Every notice of the transcript.
fn notices(app: &App) -> Vec<(NoticeLevel, String)> {
    app.transcript
        .blocks()
        .iter()
        .filter_map(|b| match b {
            Block::Notice { level, text } => Some((*level, text.clone())),
            _ => None,
        })
        .collect()
}

/// An empty answer after the engine's retries (as when another
/// workspace's agent's acknowledgement woke this agent): a run nobody
/// here typed ends in one quiet line; a run the user started in one plain
/// error; neither draws the retries or says the failure twice.
#[test]
fn an_empty_answer_is_one_line_quiet_when_nobody_here_asked() {
    // As the engine reports it after its retries.
    let empty = || {
        meerkat_core::AgentErrorReport {
        class: meerkat_core::AgentErrorClass::Llm,
        reason: Some(meerkat_core::event::AgentErrorReason::TurnTerminalCause {
            outcome: meerkat_core::TurnTerminalOutcome::Failed,
            cause_kind: meerkat_core::TurnTerminalCauseKind::RetryExhausted,
        }),
        message: "Terminal failure: Failed (RetryExhausted): retry exhausted: LLM error (self_hosted): LLM completed without user-visible text, images, or tool calls".into(),
    }
    };
    let failed = || {
        Msg::TurnFinished(Err(TurnFailure::Failed(
            "Terminal failure: Failed (RetryExhausted): retry exhausted".into(),
        )))
    };
    // Woken by another agent's message.
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Peer {
        from: "main-ab12".into(),
        text: "Acknowledged. I will relay it.".into(),
    });
    app.update(Msg::RemoteTurn { prompt: None });
    for n in 1..=3 {
        app.update(empty_retry(n));
    }
    app.update(run_failed(empty()));
    app.update(failed());
    assert_eq!(
        notices(&app),
        [(
            NoticeLevel::Info,
            "the agent had nothing to add to main-ab12's message".to_string()
        )]
    );
    // Joined on the daemon's word first, the message just after.
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::RemoteTurn { prompt: None });
    app.update(Msg::Peer {
        from: "main-cd34".into(),
        text: "Thanks.".into(),
    });
    app.update(run_failed(empty()));
    app.update(failed());
    assert_eq!(
        notices(&app),
        [(
            NoticeLevel::Info,
            "the agent had nothing to add to main-cd34's message".to_string()
        )]
    );
    // The user's own message: one plain error, the calls counted.
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "hello");
    for n in 1..=3 {
        app.update(empty_retry(n));
    }
    app.update(run_failed(empty()));
    app.update(failed());
    assert_eq!(
        notices(&app),
        [(
            NoticeLevel::Error,
            "the model returned an empty answer 4 times: send the message again, or switch the model (/model)"
                .to_string()
        )]
    );
    // The next message is a turn as any.
    assert_eq!(
        submit(&mut app, "again"),
        vec![Effect::StartTurn("again".into())]
    );
}

/// Any other failure is said once: the run's `error: …`, not again as
/// the turn's `turn failed: …`; a turn that fails with no run failure
/// still says it.
#[test]
fn a_failure_is_said_once() {
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "hello");
    app.update(run_failed(meerkat_core::AgentErrorReport {
        class: meerkat_core::AgentErrorClass::Llm,
        reason: None,
        message: "the provider is down".into(),
    }));
    app.update(Msg::TurnFinished(Err(TurnFailure::Failed(
        "the provider is down".into(),
    ))));
    assert_eq!(
        notices(&app),
        [(
            NoticeLevel::Error,
            "error: the provider is down".to_string()
        )]
    );
    submit(&mut app, "again");
    app.update(Msg::TurnFinished(Err(TurnFailure::Failed(
        "the daemon went away".into(),
    ))));
    assert_eq!(
        notices(&app).last(),
        Some(&(
            NoticeLevel::Error,
            "turn failed: the daemon went away".to_string()
        ))
    );
}
