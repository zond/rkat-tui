//! Turns, steering, the turn's end, `/rewind`, the context gauge and the cost.

use super::*;

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
    app.update(Msg::TurnFinished(TurnOutcome {
        result: Ok(summary()),
    }));
    assert!(!app.is_running());
    // `/daemon stop` is handed to the integrator.
    type_str(&mut app, "/daemon stop");
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::DaemonStop]);
    // `/agents` lists the agents, `/agents <name>` switches: both the
    // integrator's.
    type_str(&mut app, "/agents");
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::Agents(None)]);
    type_str(&mut app, "/agents main-ab12");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::Agents(Some("main-ab12".into()))]
    );
}

/// Another agent's message is shown labelled with its sender.
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
            text: "I am changing src/lib.rs".into()
        }]
    );
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
    app.update(Msg::TurnFinished(TurnOutcome {
        result: Ok(summary()),
    }));
    assert!(!app.is_running());
    assert!(app.status.running_since.is_none());
}

#[test]
fn turn_summary_text_used_when_nothing_streamed() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "q");
    app.update(key(KeyCode::Enter));
    app.update(Msg::TurnFinished(TurnOutcome {
        result: Ok(TurnSummary {
            text: "answer".into(),
            ..summary()
        }),
    }));
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

/// Both runners fold run ends with this; the attach runner used to
/// keep the later of two failures.
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

// Esc used to recall queued messages before interrupting; it now
// always interrupts a running turn (Claude Code), and ↑ takes them back.
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

#[test]
fn esc_with_nothing_pending_interrupts() {
    let mut app = running_app();
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    let mut idle = App::new(StatusInfo::default());
    assert!(idle.update(key(KeyCode::Esc)).is_empty());
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

// An interrupted turn used to return undelivered messages to the
// editor; now they start the next turn at once, shown in place.
#[test]
fn interrupted_turn_sends_undelivered_messages_next() {
    let mut app = running_app();
    submit(&mut app, "two");
    submit(&mut app, "three");
    app.update(ack(1, Ok(())));
    app.update(ack(2, Ok(())));
    app.update(Msg::SteerDelivered(PendingId(1)));
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    assert_eq!(app.status.queued, 1);
    type_str(&mut app, "draft");
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert_eq!(fx, vec![Effect::StartTurn("three".into())]);
    assert!(app.is_running());
    // The draft stays in the editor.
    assert_eq!(app.input.text(), "draft");
    let users: Vec<&Block> = app
        .transcript
        .blocks()
        .iter()
        .filter(|b| matches!(b, Block::User { .. }))
        .collect();
    assert_eq!(users, vec![&user("one"), &user("two"), &user("three")]);
    assert!(
        !app.transcript
            .blocks()
            .iter()
            .any(|b| matches!(b, Block::Queued { .. }))
    );
    assert_eq!(app.status.queued, 0);
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

#[test]
fn failed_turn_returns_undelivered_with_a_notice() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
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
fn send_next_takes_priority_over_undelivered() {
    let mut app = running_app();
    let t0 = Instant::now();
    submit(&mut app, "two");
    app.update(ack(1, Ok(())));
    type_str(&mut app, "urgent");
    app.update_at(ctrl('x'), t0);
    assert_eq!(app.update_at(ctrl('s'), t0), vec![Effect::Interrupt]);
    assert_eq!(app.send_next.as_deref(), Some("urgent"));
    // The queued message stays in place until the turn ends.
    assert_eq!(app.status.queued, 1);
    let fx = app.update(finished(Err(TurnFailure::Interrupted)));
    assert_eq!(fx, vec![Effect::StartTurn("two\n\nurgent".into())]);
    assert!(app.send_next.is_none());
    assert!(app.is_running());
    assert!(app.input.is_empty());
    let users: Vec<&Block> = app
        .transcript
        .blocks()
        .iter()
        .filter(|b| matches!(b, Block::User { .. }))
        .collect();
    assert_eq!(users, vec![&user("one"), &user("two"), &user("urgent")]);
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
    app.update(Msg::TurnFinished(TurnOutcome {
        result: Ok(TurnSummary {
            text: "streamed".into(),
            ..summary()
        }),
    }));
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
    assert_eq!(app.status.session_short, "01234567");
}

#[test]
fn context_gauge_tracks_usage_compaction_and_clear() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Status(StatusUpdate::ContextWindow(Some(200_000))));
    app.update(agent(AgentEvent::TurnCompleted {
        stop_reason: StopReason::EndTurn,
        usage: test_usage(Usage {
            input_tokens: 1_000,
            output_tokens: 200,
            cache_creation_tokens: Some(30),
            cache_read_tokens: Some(80_000),
            ..Usage::default()
        }),
        assistant_message_id: None,
    }));
    assert_eq!(app.status.context_used, Some(81_230));
    assert!(!app.status.context_approx);
    app.update(agent(AgentEvent::CompactionCompleted {
        summary_tokens: 2_500,
        messages_before: 40,
        messages_after: 3,
        summary_source: None,
        estimated_history_tokens_after: None,
    }));
    assert_eq!(app.status.context_used, Some(2_500));
    assert!(app.status.context_approx);
    app.update(agent(AgentEvent::TurnCompleted {
        stop_reason: StopReason::EndTurn,
        usage: test_usage(Usage {
            input_tokens: 3_000,
            output_tokens: 100,
            ..Usage::default()
        }),
        assistant_message_id: None,
    }));
    assert_eq!(app.status.context_used, Some(3_100));
    assert!(!app.status.context_approx);
    assert_eq!(submit(&mut app, "/clear"), vec![Effect::NewSession]);
    // What the integrator sends once the new session is live.
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
    assert!(!app.compacting);
}

#[test]
fn prompts_during_the_request_are_queued_inputs_not_held_text() {
    let mut app = App::new(StatusInfo::default());
    app.status.context_used = Some(40_000);
    assert_eq!(submit(&mut app, "/compact now"), vec![Effect::CompactNow]);
    assert!(app.compacting);
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Info,
            "asking the engine to compact at the next model call…".into()
        ))
    );
    // The prompt goes to the runner at once: it admits it as a `Queue`
    // input after the request (effects are served in order).
    assert_eq!(
        submit(&mut app, "one"),
        vec![Effect::StartTurn("one".into())]
    );
    assert!(app.is_running());
    assert_eq!(app.status.queued, 0);
    // Lifecycle commands wait for the request; /remember does not.
    for cmd in ["/clear", "/resume last", "/compact now"] {
        assert!(submit(&mut app, cmd).is_empty(), "{cmd}");
        assert_eq!(
            last_notice(&app).map(|(l, _)| l),
            Some(NoticeLevel::Warn),
            "{cmd}"
        );
    }
    assert_eq!(submit(&mut app, "#x"), vec![Effect::Remember("x".into())]);
    assert!(app.update(Msg::CompactRequested(Ok(()))).is_empty());
    assert!(!app.compacting);
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
    assert!(!app.compacting);
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
